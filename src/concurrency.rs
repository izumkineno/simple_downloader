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
/// 块的最小尺寸，与 `chunk.rs` 中定义的值保持一致。
pub(crate) const MIN_CHUNK_SIZE: u64 = 1024 * 10;

/// 下载过程所处的阶段。
#[derive(Debug, PartialEq, Clone, Copy)]
enum DownloadPhase {
    /// 探测阶段：刚开始下载时，逐步增加并发数以探测最大可用带宽。
    Probing,
    /// 稳定阶段：已找到一个较优的并发数，主要任务是维持下载速度，并在速度下降时进行调整。
    Stable,
}

/// 管理动态并发控制逻辑的结构体。
pub struct ConcurrencyManager {
    /// 用户设置的最大并发工作线程数。
    max_workers: u64,
    /// 当前所处的下载阶段。
    phase: DownloadPhase,
    /// 记录到的历史最大下载速度。
    max_speed: f64,
    /// 上次执行分割操作的时间戳。
    last_split_time: Instant,
    /// 用于计算平均速度的样本队列。
    stable_speed_samples: VecDeque<f64>,
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
            last_split_time: Instant::now() - MIN_SPLIT_INTERVAL,
            stable_speed_samples: VecDeque::with_capacity(10),
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

        // 根据当前阶段执行不同的逻辑
        match self.phase {
            DownloadPhase::Probing => self.handle_probing_phase(
                state,
                avg_speed,
                estimated_time,
                previous_max_speed,
                cmd_tx,
            ),
            DownloadPhase::Stable => self.handle_stable_phase(
                state,
                avg_speed,
                estimated_time,
                previous_max_speed,
                cmd_tx,
            ),
        }

        // 执行被动分割逻辑
        self.reactive_split(state, avg_speed, estimated_time, cmd_tx);
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
            } else {
                self.transition_to_stable();
            }
        } else if active_chunks > 1 && self.stable_speed_samples.len() >= 3 {
            // 如果增加并发后速度没有显著提升，则认为已达到带宽瓶颈，转换到稳定阶段
            self.transition_to_stable();
        }
    }

    /// 处理稳定阶段的逻辑。
    fn handle_stable_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        previous_max_speed: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        // 上升的速度样本只需要刷新基准，不需要因此立刻继续分片。
        if avg_speed >= previous_max_speed || previous_max_speed == 0.0 {
            return;
        }

        // 当速度显著下降时，尝试通过重新分片最慢的块来恢复吞吐。
        if self.stable_speed_samples.len() > 3
            && avg_speed < previous_max_speed * STABLE_SPLIT_THRESHOLD
            && self.split_is_useful(avg_speed, estimated_time)
            && (state.chunks.len() as u64) < self.max_workers
        {
            // 尝试分割最慢的块，因为它可能是瓶颈
            if let Some(slowest_chunk) = self.find_slowest_splittable_chunk(&state.chunks) {
                self.request_split(slowest_chunk.id, cmd_tx);
            }
            // 如果速度下降，则清空速度样本，以便重新评估
            if avg_speed < self.max_speed * STABLE_SPLIT_THRESHOLD {
                self.stable_speed_samples.clear();
            }
        }
    }

    /// 被动分割逻辑：在稳定阶段，如果并发数未满，则周期性地分割最大的块。
    /// 这有助于处理某些块提前完成导致并发数下降的情况。
    fn reactive_split(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
    ) {
        if self.phase == DownloadPhase::Stable && self.split_is_useful(avg_speed, estimated_time) {
            let active_chunks = state.chunks.len() as u64;
            if active_chunks == 0 || active_chunks >= self.max_workers {
                return;
            }
            if let Some(largest_chunk) = self.find_largest_splittable_chunk(&state.chunks) {
                self.request_split(largest_chunk.id, cmd_tx);
            }
        }
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

    fn chunk(id: ChunkId, start_byte: u64, end_byte: u64, downloaded_bytes: u64, speed: f64) -> ChunkState {
        let mut chunk = ChunkState::new(id, start_byte, end_byte);
        chunk.downloaded_bytes = downloaded_bytes;
        chunk.speed = speed;
        chunk
    }

    fn state_with_chunks(total_file_size: u64, chunks: impl IntoIterator<Item = ChunkState>) -> DownloadState {
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

        assert_eq!(manager.max_speed, 150.0);
        assert!(matches!(cmd_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn reactive_split_prefers_largest_remaining_splittable_chunk() {
        let mut manager = ConcurrencyManager::new(3);
        manager.phase = DownloadPhase::Stable;
        manager.max_speed = 1_000.0;
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

        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(DownloadCmd::BisectDownload { id }) if id == 2
        ));
    }
}
