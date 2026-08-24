//! 管理动态并发控制逻辑。

use crate::state::{ChunkState, DownloadState};
use crate::types::{ChunkId, DownloadCmd};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 带宽探测因子：当速度超过历史最大速度的这个倍数时，认为带宽有提升空间。
const BANDWIDTH_PROBE_FACTOR: f64 = 1.2;
/// 稳定阶段分割阈值：当速度低于历史最大速度的这个比例时，可能需要分割以提升速度。
const STABLE_SPLIT_THRESHOLD: f64 = 0.8;
/// 最小分割间隔，防止过于频繁地分割任务。
const MIN_SPLIT_INTERVAL: Duration = Duration::from_millis(300);
/// 触发分割所需的最小预估剩余时间，避免在下载即将完成时进行不必要的分割。
const MIN_REMAINING_TIME_FOR_SPLIT: f64 = 5.0;
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

/// 管理动态并发控制逻辑的结构体。
pub struct ConcurrencyManager {
    /// 用户设置的最大并发工作线程数。
    max_workers: u64,
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
}

impl ConcurrencyManager {
    /// 创建一个新的 `ConcurrencyManager` 实例。
    pub fn new(max_workers: u64) -> Self {
        Self {
            max_workers,
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
        }
    }

    /// 分析当前下载状态，并决定是否需要调整并发度（即分割块）。
    pub fn decide_and_act(
        &mut self,
        state: &DownloadState,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        // 如果距离上次分割时间太短，则不做任何操作
        if self.last_split_time.elapsed() < MIN_SPLIT_INTERVAL {
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

        // 更新最近最佳速度
        if avg_speed > self.recent_best_speed {
            self.recent_best_speed = avg_speed;
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
            self.transition_to_stable();
            return;
        }

        if !self.split_is_useful(avg_speed, estimated_time) {
            return;
        }

        // 如果当前速度显著高于历史最大速度，说明增加并发带来了好处
        if avg_speed > previous_max_speed * BANDWIDTH_PROBE_FACTOR || previous_max_speed == 0.0 {
            // 分割当前最大的块，以期进一步提升速度
            if let Some(largest_chunk) = self.find_largest_splittable_chunk(&state.chunks) {
                self.request_split(largest_chunk.id, cmd_tx);
                self.consecutive_probe_no_gain = 0; // 重置连续无增益计数
            } else {
                self.transition_to_stable();
            }
        } else {
            // 没有获得显著增益，增加计数
            self.consecutive_probe_no_gain += 1;

            // 连续2次没有增益且有足够样本，转换到稳定阶段
            if active_chunks > 1
                && self.stable_speed_samples.len() >= 3
                && self.consecutive_probe_no_gain >= 2
            {
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
        // 3. 分割是有用的
        let should_consider_split = avg_speed < self.recent_best_speed * STABLE_SPLIT_THRESHOLD
            && active_chunks < self.max_workers
            && self.split_is_useful(avg_speed, estimated_time);

        if should_consider_split {
            // 尝试分割最慢的块，因为它可能是瓶颈
            if let Some(slowest_chunk) = self.find_slowest_splittable_chunk(&state.chunks) {
                self.request_split_with_observation(slowest_chunk.id, avg_speed, cmd_tx);
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

            if gain_vs_pre_split && no_regression_vs_recent_best {
                // 分割成功，更新最近最佳速度
                self.recent_best_speed = observation.best_speed_seen.max(self.recent_best_speed);

                // 如果还有可用并发槽位且分割仍然有用，可以考虑继续分割
                let active_chunks = state.chunks.len() as u64;
                if active_chunks < self.max_workers
                    && self.split_is_useful(avg_speed, estimated_time)
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
        self.request_split(id, cmd_tx);

        // 启动观察期，需要收集2个样本（1-2个采样周期）
        self.observation_state = ObservationState::Observing;
        self.current_observation = Some(SplitObservation {
            pre_split_speed: current_speed,
            pre_split_recent_best: self.recent_best_speed,
            required_samples: 2,
            collected_samples: 0,
            best_speed_seen: current_speed,
        });
    }

    /// 发送一个分割请求。
    fn request_split(&mut self, id: ChunkId, cmd_tx: &broadcast::Sender<DownloadCmd>) {
        let _ = cmd_tx.send(DownloadCmd::BisectDownload { id });
        self.last_split_time = Instant::now();
    }

    /// 转换到稳定下载阶段。
    fn transition_to_stable(&mut self) {
        self.phase = DownloadPhase::Stable;
        println!("[ConcurrencyManager] 转换到稳定下载阶段。");
        self.stable_speed_samples.clear();
        self.observation_state = ObservationState::Ready;
        self.current_observation = None;
        // 将探测阶段的最大速度作为稳定阶段的初始最近最佳速度
        self.recent_best_speed = self.max_speed;
    }

    fn split_is_useful(&self, avg_speed: f64, estimated_time: f64) -> bool {
        avg_speed > 0.0 && estimated_time > MIN_REMAINING_TIME_FOR_SPLIT
    }

    /// 在所有块中找到剩余工作量最大的、且可继续分割的块。
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
                chunk(1, 0, 19_999, 5_000, 75.0),
                chunk(2, 20_000, 39_999, 5_000, 75.0),
            ],
        );
        let (cmd_tx, mut cmd_rx) = broadcast::channel(4);

        manager.decide_and_act(&state, &cmd_tx);

        assert_eq!(manager.max_speed, 112.5);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn stable_phase_no_mechanical_refill_when_concurrency_not_full() {
        // 验证稳定阶段不会仅仅因为并发数未满就进行分割
        let mut manager = ConcurrencyManager::new(3);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 1_000.0;
        manager.recent_best_speed = 1_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.stable_speed_samples = VecDeque::from(vec![1_000.0, 1_000.0, 1_000.0]);

        let state = state_with_chunks(
            200_000,
            [
                chunk(1, 0, 60_000, 55_000, 500.0),
                chunk(2, 60_001, 120_000, 15_000, 500.0),
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
        manager.max_speed = 1_000.0;
        manager.recent_best_speed = 1_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;
        manager.stable_speed_samples = VecDeque::from(vec![700.0, 700.0, 700.0]); // 速度低于阈值

        let state = state_with_chunks(
            200_000,
            [
                chunk(1, 0, 60_000, 55_000, 400.0),
                chunk(2, 60_001, 120_000, 15_000, 300.0), // 慢块，总和700
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
        manager.max_speed = 1_000.0;
        manager.recent_best_speed = 1_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 第一次调用：速度低，触发分割
        manager.stable_speed_samples = VecDeque::from(vec![700.0, 700.0, 700.0]);
        let state1 = state_with_chunks(
            200_000,
            [
                chunk(1, 0, 60_000, 55_000, 400.0),
                chunk(2, 60_001, 120_000, 15_000, 300.0), // 总和700
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
            200_000,
            [
                chunk(1, 0, 60_000, 58_000, 450.0),
                chunk(2, 60_001, 90_000, 10_000, 225.0),
                chunk(3, 90_001, 120_000, 10_000, 225.0),
            ],
        ); // 总速度900
        manager.stable_speed_samples = VecDeque::from(vec![700.0, 700.0, 700.0]);
        manager.decide_and_act(&state2, &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));

        // 重置分割间隔
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 观察期第二次样本：速度进一步提升，进入评估状态
        let state3 = state_with_chunks(
            200_000,
            [
                chunk(1, 0, 60_000, 60_000, 0.0),
                chunk(2, 60_001, 90_000, 20_000, 350.0),
                chunk(3, 90_001, 120_000, 20_000, 350.0),
                chunk(4, 120_001, 199_999, 10_000, 400.0),
            ],
        ); // 总速度1100（包括已完成块的0速度）
        manager.stable_speed_samples = VecDeque::from(vec![700.0, 900.0, 1100.0]);
        manager.decide_and_act(&state3, &cmd_tx);
        // 评估会立即执行，分割成功，更新最近最佳速度（说明增益门通过）
        assert_eq!(manager.recent_best_speed, 1100.0);
        // 观察状态会根据是否继续分割而变化，此处只需验证增益门逻辑正确
    }

    #[test]
    fn stable_phase_combined_gain_gate_fails() {
        // 验证组合增益门失败时不允许继续分割
        let mut manager = ConcurrencyManager::new(3);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 1_000.0;
        manager.recent_best_speed = 1_000.0;
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL;

        // 第一次调用：速度低，触发分割
        manager.stable_speed_samples = VecDeque::from(vec![700.0, 700.0, 700.0]);
        let state1 = state_with_chunks(
            200_000,
            [
                chunk(1, 0, 60_000, 55_000, 400.0),
                chunk(2, 60_001, 120_000, 15_000, 300.0), // 总和700
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
            200_000,
            [
                chunk(1, 0, 60_000, 58_000, 400.0),
                chunk(2, 60_001, 90_000, 10_000, 175.0),
                chunk(3, 90_001, 120_000, 10_000, 175.0),
            ],
        ); // 总速度750
        manager.stable_speed_samples = VecDeque::from(vec![700.0, 700.0, 700.0]);
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
            100_000,
            [
                chunk(1, 0, 49_999, 25_000, 500.0),
                chunk(2, 50_000, 99_999, 25_000, 500.0),
            ],
        ); // 总速度1000
        let (cmd_tx, _) = broadcast::channel(4);

        // 第一次探测：没有增益（速度还是1000，没有超过1000*1.2=1200）
        manager.decide_and_act(&state1, &cmd_tx);
        assert_eq!(manager.phase, DownloadPhase::Probing);
        assert_eq!(manager.consecutive_probe_no_gain, 1);

        // 第二次探测：仍然没有增益
        manager.last_split_time = Instant::now() - MIN_SPLIT_INTERVAL; // 重置分割间隔
        manager.stable_speed_samples = VecDeque::from(vec![1000.0, 1000.0, 1000.0]);
        manager.decide_and_act(&state1, &cmd_tx);
        assert_eq!(manager.phase, DownloadPhase::Stable); // 转换到稳定阶段
    }
}
