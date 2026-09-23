//! 管理动态并发控制逻辑。

use crate::state::{ChunkState, DownloadState};
use crate::types::{ChunkId, DownloadCmd};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 带宽探测因子：当速度超过历史最大速度的这个倍数时，认为带宽有提升空间。
const BANDWIDTH_PROBE_FACTOR: f64 = 1.1;
/// 稳定阶段分割阈值：当速度低于历史最大速度的这个比例时，可能需要分割以提升速度。
const STABLE_SPLIT_THRESHOLD: f64 = 0.8;
/// 最小分割间隔，防止过于频繁地分割任务。
const MIN_SPLIT_INTERVAL: Duration = Duration::from_millis(150);
/// 零速块阈值：chunk 速度低于此即判慢连接 stall，偏心切分 + 豁免观察期连续切。
const STALL_SKEW_SPEED_BPS: f64 = 1024.0;
/// 触发分割所需的最小预估剩余时间（兜底值），实际使用按文件大小自适应的阈值，避免在下载即将完成时进行不必要的分割。
const MIN_REMAINING_TIME_FOR_SPLIT: f64 = 3.0;
/// 块的最小尺寸，统一复用 `chunk::MIN_CHUNK_SIZE` 的 10 KiB 阈值（可安全二分的最小剩余量 ×2）。
pub(crate) use crate::chunk::MIN_CHUNK_SIZE;
/// 下载过程所处的阶段。
#[derive(Debug, PartialEq, Clone, Copy)]
enum DownloadPhase {
    /// 探测阶段：刚开始下载时，逐步增加并发数以探测最大可用带宽。
    Probing,
    /// 稳定阶段：已找到一个较优的并发数，主要任务是维持下载速度，并在速度下降时进行调整。
    Stable,
}

/// 稳定阶段的观察状态
#[derive(Debug, PartialEq, Clone, Copy)]
enum ObservationState {
    /// 准备状态：可以决定是否尝试分割
    Ready,
    /// 观察状态：分割后等待样本收集
    Observing,
    /// 评估状态：收集足够样本后评估分割效果
    Evaluating,
}

/// 分割后的观察记录
#[derive(Debug, Clone)]
struct SplitObservation {
    /// 分割前的基准速度
    pre_split_speed: f64,
    /// 分割前的最近最佳速度
    pre_split_recent_best: f64,
    /// 观察期需要收集的样本数
    required_samples: usize,
    /// 已收集的样本数
    collected_samples: usize,
    /// 观察期内看到的最佳速度
    best_speed_seen: f64,
}

/// 某块距上次有进展的秒数；找不到块按 0 处理（不抢未知块）。
fn chunk_idle_secs(state: &DownloadState, id: ChunkId) -> u64 {
    state.chunks.get(&id).map(|c| c.last_progress_at.elapsed().as_secs()).unwrap_or(0)
}
/// 管理动态并发控制逻辑的结构体。
pub struct ConcurrencyManager {
    /// 用户设置的最大并发工作线程数。
    max_workers: u64,
    /// 状态更新间隔，用于自适应观察期与阈值。
    update_interval: f64,
    /// 当前所处的下载阶段。
    phase: DownloadPhase,
    /// 记录到的历史最大下载速度。
    max_speed: f64,
    /// 最近的最佳速度（用于评估分割效果，区别于长期历史最大值）
    recent_best_speed: f64,
    /// 上次执行分割操作的时间戳。
    last_split_time: Instant,
    /// 用于计算平均速度的样本队列。
    stable_speed_samples: VecDeque<f64>,
    /// 稳定阶段的观察状态
    observation_state: ObservationState,
    /// 当前的分割观察记录（如果处于观察或评估状态）
    current_observation: Option<SplitObservation>,
    /// 探测阶段连续没有获得速度增益的次数
    consecutive_probe_no_gain: usize,
    /// 同块偏心切分计数：trickle 连接靠切分救不回来（切几次还是零速），
    /// 达阈值后改发 TerminateChunk 杀坏连接走重试换新连接。
    skewed_split_counts: HashMap<ChunkId, u32>,
    /// 同块抢段时间隔节流：aria2 式只抢零进展 + 空闲超阈的段，有进展的继续滴完。
    last_steal_time: HashMap<ChunkId, Instant>,
}

impl ConcurrencyManager {
    /// 创建一个新的 `ConcurrencyManager` 实例（默认 update_interval 0.5s，兼容旧单元测试）。
    pub fn new(max_workers: u64) -> Self {
        Self::new_with_interval(max_workers, 0.5)
    }

    /// 使用指定的 update_interval 创建实例，Monitor 应传入真实间隔以自适应观察期。
    pub fn new_with_interval(max_workers: u64, update_interval: f64) -> Self {
        let interval = if update_interval > 0.0 {
            update_interval
        } else {
            0.5
        };
        ::tracing::debug!(max_workers, interval, "ConcurrencyManager created");
        Self {
            max_workers,
            update_interval: interval,
            // 如果最大并发数只有1，则直接进入稳定阶段
            phase: if max_workers == 1 {
                DownloadPhase::Stable
            } else {
                DownloadPhase::Probing
            },
            max_speed: 0.0,
            recent_best_speed: 0.0,
            last_split_time: Instant::now() - MIN_SPLIT_INTERVAL,
            stable_speed_samples: VecDeque::with_capacity(10),
            observation_state: ObservationState::Ready,
            current_observation: None,
            consecutive_probe_no_gain: 0,
            skewed_split_counts: HashMap::new(),
            last_steal_time: HashMap::new(),
        }
    }
    /// 0.5.5 热更新：运行时调整最大并发（配置灵活性）
    pub fn set_max_workers(&mut self, workers: u64) {
        let w = workers.max(1);
        if w != self.max_workers {
            ::tracing::info!(
                old = self.max_workers,
                new = w,
                "concurrency max_workers hot-update"
            );
            self.max_workers = w;
        }
    }

    pub fn max_workers(&self) -> u64 {
        self.max_workers
    }


    /// 按文件大小自适应的最小剩余时间阈值（大文件更激进，小文件保持保守）。
    /// 修复前 1.5/2.0/2.5/3.5/5.0 仍导致 S2/S5 零分裂（est 0.31s/1.05s <阈值），修复后更激进以允许大中文件及早探测。
    fn adaptive_remaining_threshold(total_file_size: u64) -> f64 {
        if total_file_size >= 20 * 1024 * 1024 {
            0.8
        } else if total_file_size >= 10 * 1024 * 1024 {
            1.0
        } else if total_file_size >= 5 * 1024 * 1024 {
            1.2
        } else if total_file_size >= 1024 * 1024 {
            1.8
        } else {
            MIN_REMAINING_TIME_FOR_SPLIT
        }
    }

    /// 根据 update_interval 计算观察期所需样本数（覆盖约 1s 采样）。
    fn required_samples_for_interval(update_interval: f64) -> usize {
        let n = (1.0 / update_interval.max(0.05)).ceil() as usize;
        n.clamp(2, 5)
    }

    /// 分析当前下载状态，并决定是否需要调整并发度（即分割块）。
    #[::tracing::instrument(skip(self, state, cmd_tx), fields(phase = ?self.phase, obs = ?self.observation_state, max_workers = self.max_workers))]
    pub fn decide_and_act(
        &mut self,
        state: &DownloadState,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        // 干净分支语义：一律走节流+探测/稳定有用性门限，不做饥饿/尾部绕行，避免小尾碎片风暴与 429
        // 如果距离上次分割时间太短，则不做任何操作
        if self.last_split_time.elapsed() < MIN_SPLIT_INTERVAL {
            ::tracing::trace!(
                phase = ?self.phase,
                obs = ?self.observation_state,
                elapsed = ?self.last_split_time.elapsed(),
                interval = ?MIN_SPLIT_INTERVAL,
                "concurrency skip: throttled"
            );
            return;
        }

        // 收集速度样本
        let current_speed = state.total_speed();
        self.stable_speed_samples.push_back(current_speed);
        if self.stable_speed_samples.len() > 5 {
            self.stable_speed_samples.pop_front();
        }
        if self.stable_speed_samples.is_empty() {
            return;
        }
        // 计算平均速度和预估剩余时间
        let avg_speed =
            self.stable_speed_samples.iter().sum::<f64>() / self.stable_speed_samples.len() as f64;
        let remaining_bytes = state
            .total_file_size
            .saturating_sub(state.total_downloaded());
        let estimated_time = if avg_speed > 0.0 {
            remaining_bytes as f64 / avg_speed
        } else {
            f64::MAX
        };
        let previous_max_speed = self.max_speed;
        if avg_speed > 0.0 {
            self.max_speed = self.max_speed.max(avg_speed);
        }

        // 更新最近最佳速度，并对陈旧的 recent_best 做温和衰减，避免阈值永久锁死
        if avg_speed > self.recent_best_speed {
            self.recent_best_speed = avg_speed;
        } else if self.phase == DownloadPhase::Stable
            && self.observation_state == ObservationState::Ready
        {
            // 仅在稳定期且空闲时缓慢下漂，避免后台长期阈值过高导致无法再触发
            // 修复前 0.992 / tick 半衰 ~43s 过慢，抖动恢复慢；改为 0.97 半衰 ~11s，更快贴近现实
            let decayed = self.recent_best_speed * 0.97;
            // 不可低于当前均值，避免因瞬时抖动误降
            if decayed > avg_speed {
                self.recent_best_speed = decayed;
            } else if avg_speed > 0.0 {
                // 若已低于均值但均值仍 < 旧峰值，逐步让阈值贴近现实
                self.recent_best_speed = self.recent_best_speed * 0.98 + avg_speed * 0.02;
            }
        }

        // 如果处于观察状态，处理样本收集
        if self.phase == DownloadPhase::Stable
            && self.observation_state == ObservationState::Observing
            && let Some(ref mut observation) = self.current_observation
        {
            observation.collected_samples += 1;
            observation.best_speed_seen = observation.best_speed_seen.max(current_speed);

            // 如果收集到足够样本，进入评估状态
            if observation.collected_samples >= observation.required_samples {
                self.observation_state = ObservationState::Evaluating;
            }
        }

        ::tracing::trace!(
            phase = ?self.phase,
            obs = ?self.observation_state,
            active = state.chunks.len(),
            avg_kbs = avg_speed / 1024.0,
            cur_kbs = current_speed / 1024.0,
            max_kbs = self.max_speed / 1024.0,
            recent_best_kbs = self.recent_best_speed / 1024.0,
            remaining = remaining_bytes,
            est_s = if estimated_time.is_finite() { estimated_time } else { -1.0 },
            samples = ?self.stable_speed_samples,
            "concurrency tick"
        );

        // 根据当前阶段执行不同的逻辑
        match self.phase {
            DownloadPhase::Probing => self.handle_probing_phase(
                state,
                avg_speed,
                estimated_time,
                previous_max_speed,
                cmd_tx,
            ),
            DownloadPhase::Stable => {
                self.handle_stable_phase(state, avg_speed, estimated_time, cmd_tx)
            }
        }
    }

    /// 处理探测阶段的逻辑。
    fn handle_probing_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        previous_max_speed: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        let active_chunks = state.chunks.len() as u64;
        // 如果已达到最大并发数，则转换到稳定阶段
        if active_chunks >= self.max_workers {
            ::tracing::info!(
                active = active_chunks,
                max = self.max_workers,
                "probing: workers saturated -> Stable"
            );
            self.transition_to_stable();
            return;
        }

        if !self.split_is_useful(state, avg_speed, estimated_time) {
            ::tracing::trace!(
                avg_kbs = avg_speed / 1024.0,
                est_s = estimated_time,
                threshold = Self::adaptive_remaining_threshold(state.total_file_size),
                remaining = state
                    .total_file_size
                    .saturating_sub(state.total_downloaded()),
                "probing: split not useful"
            );
            return;
        }

        // 如果当前速度显著高于历史最大速度，说明增加并发带来了好处
        if avg_speed > previous_max_speed * BANDWIDTH_PROBE_FACTOR || previous_max_speed == 0.0 {
            ::tracing::debug!(
                avg_kbs = avg_speed / 1024.0,
                prev_max_kbs = previous_max_speed / 1024.0,
                factor = BANDWIDTH_PROBE_FACTOR,
                "probing: gain detected -> split largest"
            );
            // 分割当前最大的块，以期进一步提升速度
            if let Some(largest_chunk) = self.find_largest_splittable_chunk(&state.chunks) {
                ::tracing::info!(
                    chunk_id = largest_chunk.id,
                    remaining = largest_chunk.remaining_bytes(),
                    speed_kbs = largest_chunk.speed / 1024.0,
                    "probing: splitting largest"
                );
                self.request_split(largest_chunk.id, cmd_tx);
                self.consecutive_probe_no_gain = 0; // 重置连续无增益计数
            } else {
                ::tracing::debug!("probing: no splittable largest -> Stable");
                self.transition_to_stable();
            }
        } else {
            // 没有获得显著增益，增加计数
            self.consecutive_probe_no_gain += 1;
            ::tracing::trace!(
                avg_kbs = avg_speed / 1024.0,
                prev_max_kbs = previous_max_speed / 1024.0,
                factor = BANDWIDTH_PROBE_FACTOR,
                consecutive = self.consecutive_probe_no_gain,
                active = active_chunks,
                "probing: no gain"
            );

            // 连续1次没有增益且有足够样本，转换到稳定阶段（修复前需2次导致 total瓶颈下多余分裂）
            if active_chunks > 1
                && self.stable_speed_samples.len() >= 3
                && self.consecutive_probe_no_gain >= 1
            {
                ::tracing::info!(
                    consecutive = self.consecutive_probe_no_gain,
                    samples = self.stable_speed_samples.len(),
                    "probing: consecutive no-gain -> Stable"
                );
                self.transition_to_stable();
            }
        }
    }

    /// 处理稳定阶段的逻辑。
    fn handle_stable_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        match self.observation_state {
            ObservationState::Ready => {
                self.handle_stable_ready(state, avg_speed, estimated_time, cmd_tx);
            }
            ObservationState::Observing => {
                // 观察期不做任何决策，等待样本收集完成
                ::tracing::trace!("stable: observing, skip decision");
            }
            ObservationState::Evaluating => {
                self.handle_stable_evaluate(state, avg_speed, estimated_time, cmd_tx);
            }
        }
    }

    /// 处理稳定阶段准备状态的逻辑
    fn handle_stable_ready(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        let active_chunks = state.chunks.len() as u64;

        // 只有在有证据表明分割可能带来收益时才考虑分割
        // 1. 当前速度显著低于最近最佳速度（可能有慢块瓶颈）
        // 2. 还有可用的并发槽位
        // 3. 分割是有用的（按文件大小自适应）
        let threshold = self.recent_best_speed * STABLE_SPLIT_THRESHOLD;
        let useful = self.split_is_useful(state, avg_speed, estimated_time);
        let should_consider_split =
            avg_speed < threshold && active_chunks < self.max_workers && useful;

        ::tracing::trace!(
            avg_kbs = avg_speed / 1024.0,
            recent_best_kbs = self.recent_best_speed / 1024.0,
            threshold_kbs = threshold / 1024.0,
            active = active_chunks,
            max = self.max_workers,
            est_s = estimated_time,
            remaining = state
                .total_file_size
                .saturating_sub(state.total_downloaded()),
            useful = useful,
            should_split = should_consider_split,
            "stable::ready check"
        );

        // stall 救援优先：零速 + 空闲超阈的块才抢（aria2 SegmentMan 只抢 idle 段——
        // 刚切出的块速度恒为 0，必须给它滴数据的时间，否则每个新块都被秒杀）。
        // 绕过 useful/should_split 门控：3KB 尾块 useful=false + 不满足可切分尺寸，
        // 双重拦截下只能等 15s 空闲超时。
        const STALL_RESCUE_IDLE_SECS: u64 = 10;
        if let Some(stalled) = state.chunks.values().filter(|c| c.speed < STALL_SKEW_SPEED_BPS && c.remaining_bytes() > 0 && chunk_idle_secs(state, c.id) >= STALL_RESCUE_IDLE_SECS).min_by(|a, b| a.speed.partial_cmp(&b.speed).unwrap_or(std::cmp::Ordering::Equal)) {
            let stalled_id = stalled.id;
            let stalled_speed = stalled.speed;
            let stalled_remaining = stalled.remaining_bytes();
            ::tracing::info!(
                chunk_id = stalled_id,
                speed_kbs = stalled_speed / 1024.0,
                remaining = stalled_remaining,
                "stable::ready skewed-splitting stalled chunk"
            );
            self.request_skewed_split(state, stalled_id, stalled_remaining, cmd_tx);
            return;
        }
        if should_consider_split {
            // 尝试分割最慢的块，因为它可能是瓶颈；正常切分要求单块剩余 ≥512KiB，
            // 尾部小块建连+首字节开销大于并行收益（106KB→20KB碎片越切越慢）。
            // 但零速 stall 豁免：偏心切分只留 1/8 本地、新连接取 7/8，正是救尾手段。
            if let Some(slowest_chunk) = self.find_slowest_splittable_chunk(&state.chunks) {
                let slowest_id = slowest_chunk.id;
                let slowest_speed = slowest_chunk.speed;
                let slowest_remaining = slowest_chunk.remaining_bytes();
                let is_stall = slowest_speed < STALL_SKEW_SPEED_BPS;
                if !is_stall && slowest_remaining < 512 * 1024 {
                    ::tracing::debug!(
                        chunk_id = slowest_id,
                        remaining = slowest_remaining,
                        "stable::ready skip: slowest remaining <512KiB tail"
                    );
                    return;
                }
                if is_stall {
                    ::tracing::info!(
                        chunk_id = slowest_id,
                        speed_kbs = slowest_speed / 1024.0,
                        remaining = slowest_remaining,
                        "stable::ready skewed-splitting stalled chunk"
                    );
                    self.request_skewed_split(state, slowest_id, slowest_remaining, cmd_tx);
                } else {
                    ::tracing::info!(
                        chunk_id = slowest_id,
                        speed_kbs = slowest_speed / 1024.0,
                        remaining = slowest_remaining,
                        "stable::ready splitting slowest"
                    );
                    self.request_split_with_observation(slowest_id, avg_speed, cmd_tx);
                }
            } else {
                ::tracing::debug!("stable::ready no splittable slowest chunk");
            }
        }
    }

    /// 处理稳定阶段评估状态的逻辑
    fn handle_stable_evaluate(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        if let Some(observation) = self.current_observation.take() {
            // 组合增益门：
            // 1. 观察期最佳速度 > 分割前基准速度（分割带来了提升）
            // 2. 观察期最佳速度不显著低于分割前的最近最佳速度（没有倒退）
            let gain_vs_pre_split =
                observation.best_speed_seen > observation.pre_split_speed * 1.05;
            let no_regression_vs_recent_best =
                observation.best_speed_seen > observation.pre_split_recent_best * 0.95;
            ::tracing::debug!(
                pre_kbs = observation.pre_split_speed / 1024.0,
                recent_best_kbs = observation.pre_split_recent_best / 1024.0,
                best_seen_kbs = observation.best_speed_seen / 1024.0,
                avg_kbs = avg_speed / 1024.0,
                gain = gain_vs_pre_split,
                no_regression = no_regression_vs_recent_best,
                active = state.chunks.len(),
                "stable::evaluating"
            );

            if gain_vs_pre_split && no_regression_vs_recent_best {
                // 分割成功，更新最近最佳速度
                self.recent_best_speed = observation.best_speed_seen.max(self.recent_best_speed);

                // 如果还有可用并发槽位且分割仍然有用，可以考虑继续分割
                let active_chunks = state.chunks.len() as u64;
                if active_chunks < self.max_workers
                    && self.split_is_useful(state, avg_speed, estimated_time)
                {
                    // 分割当前最大的块以进一步提升
                    if let Some(largest_chunk) = self.find_largest_splittable_chunk(&state.chunks) {
                        self.request_split_with_observation(largest_chunk.id, avg_speed, cmd_tx);
                        return; // 保持观察状态，不需要重置为Ready
                    }
                }
            }

            // 无论分割是否成功，都重置为准备状态
            self.observation_state = ObservationState::Ready;
        } else {
            // 没有观察记录，重置为准备状态
            self.observation_state = ObservationState::Ready;
        }
    }

    /// 发送分割请求并启动观察期
    fn request_split_with_observation(
        &mut self,
        id: ChunkId,
        current_speed: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        ::tracing::debug!(
            chunk_id = id,
            pre_kbs = current_speed / 1024.0,
            recent_best_kbs = self.recent_best_speed / 1024.0,
            "split request with observation"
        );
        self.request_split(id, cmd_tx);

        // 启动观察期，样本数按 update_interval 自适应（覆盖约 1s），保证不同 tick 间隔下鲁棒
        let required = Self::required_samples_for_interval(self.update_interval);
        self.observation_state = ObservationState::Observing;
        self.current_observation = Some(SplitObservation {
            pre_split_speed: current_speed,
            pre_split_recent_best: self.recent_best_speed,
            required_samples: required,
            collected_samples: 0,
            best_speed_seen: current_speed,
        });
        ::tracing::debug!(
            chunk_id = id,
            pre_kbs = current_speed / 1024.0,
            required_samples = required,
            "stable -> Observing"
        );
    }
    /// 偏心分割请求：豁免观察期（保持 Ready 可连续切），仅受 MIN_SPLIT_INTERVAL 节流。
    /// 同块累计 4 次后仍 stall（trickle 连接：有数据但极慢，空闲超时永不触发，
    /// 切分只救走 7/8、坏连接留守的 1/8 永远跑不动），第 5 次改发 TerminateChunk
    /// 杀坏连接走重试换新连接（IDM 式 reassign 优先，kill 只作最后手段——aria2
    /// #686/#897/#2111 证明杀后不补会 hang，我方 kill 走重试补新连接故无此坑，
    /// 但建连 0.1~0.5s + CDN 可能限流，能 reassign 抢完就不杀）。
    /// 小剩余（<128KiB）切分救回的 7/8 也不值得一次建连，
    /// 且 12KB 尾块证明连 trickle 都停了时直接杀、不浪费一次切分。
    fn request_skewed_split(&mut self, state: &DownloadState, id: ChunkId, remaining: u64, cmd_tx: &broadcast::Sender<DownloadCmd>) {
        // aria2 式抢段（SegmentMan::getCleanSegmentIfOwnerIsIdle）：小尾巴（<128KiB）不切分，
        // 但零进展 + 空闲超阈则直接 TerminateChunk 走重试换新连接——重试带 offset 续传
        // （chunk.rs TerminateChunk 分支上报 actual + ChunkFailed 剩余区间），取消不丢进度。
        const IDLE_STEAL_SECS: u64 = 10;
        const SMALL_TAIL_BYTES: u64 = 128 * 1024;
        if remaining < SMALL_TAIL_BYTES {
            let idle = chunk_idle_secs(state, id);
            let throttled = self.last_steal_time.get(&id).is_some_and(|t| t.elapsed().as_secs() < IDLE_STEAL_SECS);
            if idle >= IDLE_STEAL_SECS && !throttled {
                ::tracing::info!(chunk_id = id, remaining, idle_secs = idle, "small-tail idle steal: TerminateChunk, retry resumes from offset");
                let _ = cmd_tx.send(DownloadCmd::TerminateChunk { id });
                self.last_steal_time.insert(id, Instant::now());
                self.last_split_time = Instant::now();
            } else {
                ::tracing::trace!(chunk_id = id, remaining, idle_secs = idle, "small-tail stall: let it drain, no split no kill");
            }
            return;
        }
        let count = self.skewed_split_counts.entry(id).or_insert(0);
        *count += 1;
        if *count > 4 {
            ::tracing::debug!(chunk_id = id, skewed_count = *count, phase = ?self.phase, "TerminateChunk: trickle connection kill");
            let _ = cmd_tx.send(DownloadCmd::TerminateChunk { id });
            self.skewed_split_counts.remove(&id);
        } else {
            ::tracing::debug!(chunk_id = id, skewed_count = *count, phase = ?self.phase, "BisectDownloadSkewed");
            let _ = cmd_tx.send(DownloadCmd::BisectDownloadSkewed { id });
        }
        self.last_split_time = Instant::now();
    }

    /// 发送一个分割请求。
    fn request_split(&mut self, id: ChunkId, cmd_tx: &broadcast::Sender<DownloadCmd>) {
        ::tracing::debug!(chunk_id = id, phase = ?self.phase, "BisectDownload");
        let _ = cmd_tx.send(DownloadCmd::BisectDownload { id });
        self.last_split_time = Instant::now();
    }

    /// 转换到稳定下载阶段。
    fn transition_to_stable(&mut self) {
        ::tracing::info!(
            max_kbs = self.max_speed / 1024.0,
            samples = ?self.stable_speed_samples,
            "Probing -> Stable"
        );
        self.phase = DownloadPhase::Stable;
        self.stable_speed_samples.clear();
        self.observation_state = ObservationState::Ready;
        self.current_observation = None;
        // 将探测阶段的最大速度作为稳定阶段的初始最近最佳速度
        self.recent_best_speed = self.max_speed;
    }

    fn split_is_useful(&self, state: &DownloadState, avg_speed: f64, estimated_time: f64) -> bool {
        if avg_speed <= 0.0 {
            return false;
        }
        // 干净分支语义：极小剩余不分片，避免碎片；门槛 256KiB
        let remaining = state
            .total_file_size
            .saturating_sub(state.total_downloaded());
        if remaining < 256 * 1024 {
            return false;
        }
        let mut threshold = Self::adaptive_remaining_threshold(state.total_file_size);
        // 首分激进：单块时阈值打 4 折，允许 S2/S5 等大中文件在 est 仅 0.3-1s 时仍探测，避免零分裂
        if state.chunks.len() == 1 {
            threshold *= 0.4;
        }
        estimated_time > threshold
    }
    fn find_largest_splittable_chunk<'a>(
        &self,
        chunks: &'a HashMap<ChunkId, ChunkState>,
    ) -> Option<&'a ChunkState> {
        chunks
            .values()
            .filter(|c| c.is_splittable(MIN_CHUNK_SIZE))
            .max_by_key(|c| c.remaining_bytes())
    }

    /// 找到最慢且可以被分割的块。
    /// “可以被分割”意味着其剩余大小至少是最小块尺寸的两倍。
    fn find_slowest_splittable_chunk<'a>(
        &self,
        chunks: &'a HashMap<ChunkId, ChunkState>,
    ) -> Option<&'a ChunkState> {
        chunks
            .values()
            .filter(|c| c.is_splittable(MIN_CHUNK_SIZE))
            .min_by(|a, b| {
                a.speed
                    .partial_cmp(&b.speed)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    fn chunk(
        id: ChunkId,
        start_byte: u64,
        end_byte: u64,
        downloaded_bytes: u64,
        speed: f64,
    ) -> ChunkState {
        let mut chunk = ChunkState::new(id, start_byte, end_byte);
        chunk.downloaded_bytes = downloaded_bytes;
        chunk.speed = speed;
        chunk
    }

    fn state_with_chunks(
        total_file_size: u64,
        chunks: impl IntoIterator<Item = ChunkState>,
    ) -> DownloadState {
        let mut state = DownloadState::new(total_file_size);
        for chunk in chunks {
            state.chunks.insert(chunk.id, chunk);
        }
        state
    }

    #[test]
    fn stalled_chunk_gets_skewed_split_without_observation() {
        // 零速 stall 块：偏心切分且豁免观察期（保持 Ready，可连续切）
        let mut manager = ConcurrencyManager::new(8);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 10_000.0;
        manager.recent_best_speed = 10_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.stable_speed_samples = VecDeque::from(vec![5000.0, 5000.0, 5000.0]);
        let state = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 60_000, 55_000, 9000.0),
                chunk(2, 60_001, 5_000_000 - 1, 1000, 6.0), // stall：6B/s
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);
        manager.decide_and_act(&state, &cmd_tx);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(DownloadCmd::BisectDownloadSkewed { id: 2 })
        ));
        assert_eq!(manager.observation_state, ObservationState::Ready);
    }
    #[test]
    fn probing_phase_does_not_split_without_positive_speed_evidence() {
        let mut manager = ConcurrencyManager::new(4);
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        let state = state_with_chunks(100_000, [chunk(1, 0, 99_999, 0, 0.0)]);
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);

        manager.decide_and_act(&state, &cmd_tx);

        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn stable_phase_refreshes_max_speed_without_forcing_split() {
        let mut manager = ConcurrencyManager::new(2);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 100.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.stable_speed_samples = VecDeque::from(vec![100.0, 100.0, 100.0]);

        let state = state_with_chunks(
            50_000,
            [
                chunk(1, 0, 19_999, 5_000, 7500.0),
                chunk(2, 20_000, 39_999, 5_000, 7500.0),
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);

        manager.decide_and_act(&state, &cmd_tx);

        assert_eq!(manager.max_speed, 3825.0);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn stable_phase_no_mechanical_refill_when_concurrency_not_full() {
        // 验证稳定阶段不会仅仅因为并发数未满就进行分割
        let mut manager = ConcurrencyManager::new(3);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 10_000.0;
        manager.recent_best_speed = 10_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.stable_speed_samples = VecDeque::from(vec![1_000.0, 1_000.0, 1_000.0]);

        let state = state_with_chunks(
            200_000,
            [
                chunk(1, 0, 60_000, 55_000, 5000.0),
                chunk(2, 60_001, 120_000, 15_000, 5000.0),
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);

        manager.decide_and_act(&state, &cmd_tx);

        // 速度等于最近最佳速度，不应该触发分割
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn stable_phase_starts_observation_window_after_split() {
        // 验证稳定阶段分割后会进入观察期
        let mut manager = ConcurrencyManager::new(3);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 10_000.0;
        manager.recent_best_speed = 10_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.stable_speed_samples = VecDeque::from(vec![7000.0, 7000.0, 7000.0]); // 速度低于阈值

        let state = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 1_060_000, 55_000, 4000.0),
                chunk(2, 1_060_001, 2_120_000, 15_000, 3000.0), // 慢块
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);

        // 第一次调用应该触发分割并进入观察状态
        manager.decide_and_act(&state, &cmd_tx);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(DownloadCmd::BisectDownload { id }) if id == 2
        ));
        assert_eq!(manager.observation_state, ObservationState::Observing);

        // 重置分割间隔，允许下一次调用
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 观察期内不应该再次分割，但会收集样本
        manager.decide_and_act(&state, &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(manager.observation_state, ObservationState::Observing);

        // 重置分割间隔
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 第二次调用后收集到足够样本，评估完成后回到Ready状态（因为分割没有带来增益）
        manager.decide_and_act(&state, &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(manager.observation_state, ObservationState::Ready);
    }

    #[test]
    fn stable_phase_combined_gain_gate_passes() {
        // 验证组合增益门通过时允许继续分割
        let mut manager = ConcurrencyManager::new(4);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 10_000.0;
        manager.recent_best_speed = 10_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 第一次调用：速度低，触发分割
        manager.stable_speed_samples = VecDeque::from(vec![7000.0, 7000.0, 7000.0]);
        let state1 = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 1_060_000, 55_000, 4000.0),
                chunk(2, 1_060_001, 2_120_000, 15_000, 3000.0),
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);
        manager.decide_and_act(&state1, &cmd_tx);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(DownloadCmd::BisectDownload { id: 2 })
        ));

        // 重置分割间隔，允许下一次调用
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 观察期第一次样本：速度提升
        let state2 = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 60_000, 58_000, 4500.0),
                chunk(2, 60_001, 90_000, 10_000, 2250.0),
                chunk(3, 90_001, 120_000, 10_000, 2250.0),
            ],
        ); // 总速度9000
        manager.stable_speed_samples = VecDeque::from(vec![7000.0, 7000.0, 7000.0]);
        manager.decide_and_act(&state2, &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));

        // 重置分割间隔
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 观察期第二次样本：速度进一步提升，进入评估状态
        let state3 = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 60_000, 60_000, 0.0),
                chunk(2, 60_001, 90_000, 20_000, 3500.0),
                chunk(3, 90_001, 120_000, 20_000, 3500.0),
                chunk(4, 120_001, 199_999, 10_000, 4000.0),
            ],
        ); // 总速度11000（包括已完成块的0速度）
        manager.stable_speed_samples = VecDeque::from(vec![7000.0, 9000.0, 11000.0]);
        manager.decide_and_act(&state3, &cmd_tx);
        // 评估会立即执行，分割成功，更新最近最佳速度（说明增益门通过）
        assert_eq!(manager.recent_best_speed, 11000.0);
        // 观察状态会根据是否继续分割而变化，此处只需验证增益门逻辑正确
    }

    #[test]
    fn stable_phase_combined_gain_gate_fails() {
        // 验证组合增益门失败时不允许继续分割
        let mut manager = ConcurrencyManager::new(3);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 10_000.0;
        manager.recent_best_speed = 10_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 第一次调用：速度低，触发分割
        manager.stable_speed_samples = VecDeque::from(vec![7000.0, 7000.0, 7000.0]);
        let state1 = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 1_060_000, 55_000, 4000.0),
                chunk(2, 1_060_001, 2_120_000, 15_000, 3000.0),
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);
        manager.decide_and_act(&state1, &cmd_tx);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(DownloadCmd::BisectDownload { id: 2 })
        ));

        // 重置分割间隔
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 观察期样本：速度没有提升
        let state2 = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 60_000, 58_000, 4000.0),
                chunk(2, 60_001, 90_000, 10_000, 1750.0),
                chunk(3, 90_001, 120_000, 10_000, 1750.0),
            ],
        ); // 总速度7500
        manager.stable_speed_samples = VecDeque::from(vec![7000.0, 7000.0, 7000.0]);
        manager.decide_and_act(&state2, &cmd_tx); // 收集第一个样本
        assert_eq!(manager.observation_state, ObservationState::Observing);

        // 重置分割间隔
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.decide_and_act(&state2, &cmd_tx); // 收集第二个样本，评估完成后回到Ready状态
        assert_eq!(manager.observation_state, ObservationState::Ready);

        // 重置分割间隔
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 评估阶段：分割失败，不允许继续分割
        manager.decide_and_act(&state2, &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(manager.observation_state, ObservationState::Ready);
    }

    #[test]
    fn probing_phase_stops_after_consecutive_no_gain() {
        // 验证探测阶段连续无增益后会转换到稳定阶段
        let mut manager = ConcurrencyManager::new(4);
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.max_speed = 1000.0;

        // 初始状态：已经分割过一次，有2个活跃块
        manager.stable_speed_samples = VecDeque::from(vec![1000.0, 1000.0, 1000.0]);
        let state1 = state_with_chunks(
            5_000_000,
            [
                chunk(1, 0, 49_999, 25_000, 500.0),
                chunk(2, 50_000, 99_999, 25_000, 500.0),
            ],
        ); // 总速度1000
        let (cmd_tx, _) = broadcast::channel(4);

        // 第一次探测：没有增益（速度还是1000，没有超过1000*1.2=1200），阈值1次即切 Stable (0.3.1后 2->1)
        manager.decide_and_act(&state1, &cmd_tx);
        assert_eq!(manager.phase, DownloadPhase::Stable);
        assert_eq!(manager.consecutive_probe_no_gain, 1);
    }

}
