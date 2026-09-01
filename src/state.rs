//! 定义用于存储下载状态的数据结构。

use crate::types::ChunkId;
use std::collections::HashMap;
use std::time::Instant;

/// 表示单个下载块（chunk）的状态。
#[derive(Debug, Clone)]
pub struct ChunkState {
    /// 块的唯一标识符。
    pub id: ChunkId,
    /// 块在文件中的起始字节位置。
    pub start_byte: u64,
    /// 块在文件中的结束字节位置。
    pub end_byte: u64,
    /// 此块已下载的字节数。
    pub downloaded_bytes: u64,
    /// 上次采样时已下载的字节数，用于计算瞬时速度。
    last_sampled_bytes: u64,
    /// 当前块的下载速度（字节/秒），经过平滑处理。
    pub speed: f64,
    /// 块的当前状态码。
    /// 0=下载中, 1=重试中, 2=等待重试, 3=延迟重试, 4=已完成, 5=失败
    pub status: u8,
    /// 与当前状态相关的描述信息。
    pub status_message: Option<String>,
    /// 状态最后一次变更的时间戳。
    pub status_changed_at: Instant,
}

impl ChunkState {
    /// 创建一个新的 `ChunkState` 实例。
    pub fn new(id: ChunkId, start_byte: u64, end_byte: u64) -> Self {
        Self {
            id,
            start_byte,
            end_byte,
            downloaded_bytes: 0,
            last_sampled_bytes: 0,
            speed: 0.0,
            status: 0, // 初始状态为“下载中”
            status_message: None,
            status_changed_at: Instant::now(),
        }
    }

    /// 更新块的状态。
    pub fn update_status(&mut self, status: u8, message: Option<String>) {
        self.status = status;
        self.status_message = message;
        self.status_changed_at = Instant::now();
    }

    /// 更新已下载的字节数。
    pub fn update_downloaded(&mut self, downloaded_bytes: u64) {
        self.downloaded_bytes = downloaded_bytes;
    }

    /// 更新块的结束字节位置（当块被分割时会发生变化）。
    pub fn update_end_byte(&mut self, end_byte: u64) {
        self.end_byte = end_byte;
    }

    /// 计算块的总大小。
    pub fn size(&self) -> u64 {
        self.end_byte.saturating_sub(self.start_byte) + 1
    }

    /// 计算块当前剩余的未下载字节数。
    pub fn remaining_bytes(&self) -> u64 {
        self.size().saturating_sub(self.downloaded_bytes)
    }

    /// 判断当前块是否还能被安全地一分为二。
    pub fn is_splittable(&self, min_chunk_size: u64) -> bool {
        self.remaining_bytes() >= min_chunk_size * 2
    }

    /// 根据新下载的字节数和经过的时间来更新速度。
    /// 使用指数移动平均法（EMA）进行平滑处理，并防护 `elapsed` 过小导致的瞬时速度突增。
    pub fn update_speed(&mut self, elapsed_secs: f64, smoothing_factor: f64) {
        if !elapsed_secs.is_finite() || elapsed_secs < 0.05 {
            return;
        }
        let elapsed = elapsed_secs.max(0.05);
        let newly_downloaded = self
            .downloaded_bytes
            .saturating_sub(self.last_sampled_bytes);
        if newly_downloaded == 0 {
            self.last_sampled_bytes = self.downloaded_bytes;
            return;
        }
        let mut instantaneous_speed = newly_downloaded as f64 / elapsed;
        const MAX_SINGLE_CHUNK_BPS: f64 = 200.0 * 1024.0 * 1024.0;
        if instantaneous_speed > MAX_SINGLE_CHUNK_BPS {
            instantaneous_speed = MAX_SINGLE_CHUNK_BPS;
        }
        if self.speed == 0.0 {
            // 首采样同样平滑，避免首个 46M/0.2s 瞬时 230M 直接污染 total
            self.speed = instantaneous_speed * smoothing_factor;
        } else {
            self.speed =
                (instantaneous_speed * smoothing_factor) + (self.speed * (1.0 - smoothing_factor));
        }
        self.last_sampled_bytes = self.downloaded_bytes;
    }
}

/// 整个下载任务的中心状态存储。
pub struct DownloadState {
    /// 文件的总大小。
    pub total_file_size: u64,
    /// 存储所有当前活跃（未完成）的下载块的状态。
    pub chunks: HashMap<ChunkId, ChunkState>,
    /// 已完成并从 `chunks` 映射中移除的块所贡献的总字节数。
    completed_bytes: u64,
}

impl DownloadState {
    /// 创建一个新的 `DownloadState` 实例。
    pub fn new(total_file_size: u64) -> Self {
        Self {
            total_file_size,
            chunks: HashMap::new(),
            completed_bytes: 0,
        }
    }

    /// 创建带有已验证完成字节数的状态，用于断点续传启动。
    pub fn with_completed(total_file_size: u64, completed_bytes: u64) -> Self {
        Self {
            total_file_size,
            chunks: HashMap::new(),
            completed_bytes: completed_bytes.min(total_file_size),
        }
    }

    /// 将一个块标记为已完成。
    /// P0-02 完整性门已保证仅当 `offset == end+1` 才发送 `DownloadComplete`，此时 `downloaded == size`。
    /// 为容忍 `broadcast Lagged` 导致最终 `ChunkProgress` 丢失，使用 `size()` 精确累加，避免 `total_downloaded` 低估而卡 100%。
    /// 截断流已在 chunk 侧判为 `ChunkFailed` 不会走到此分支，故不会误算零填充。
    pub fn complete_chunk(&mut self, id: &ChunkId) {
        if let Some(chunk) = self.chunks.remove(id) {
            self.completed_bytes += chunk.size();
        }
    }

    /// 失败时保留已下载的前缀，避免 total_downloaded 回落导致剩余时间误判。
    /// 调用方需保证在发送 `ChunkFailed` 前已通过 `ChunkProgress` 将 `downloaded` 精确落入 `state`，
    /// 若存在 Lagged 丢失，调用方可传入 `exact_downloaded` 兜底。
    pub(crate) fn preserve_partial(&mut self, id: &ChunkId) {
        if let Some(chunk) = self.chunks.get(id) {
            self.completed_bytes += chunk.downloaded_bytes;
        }
    }

    /// 失败时带精确进度兜底的保留，避免节流 64KiB 窗口丢失导致 progress 回退 0.5s 卡顿。
    pub(crate) fn preserve_partial_exact(&mut self, id: &ChunkId, exact_downloaded: u64) {
        if let Some(chunk) = self.chunks.get(id) {
            let best = chunk.downloaded_bytes.max(exact_downloaded.min(chunk.size()));
            self.completed_bytes += best;
        } else {
            self.completed_bytes = self.completed_bytes.saturating_add(exact_downloaded);
        }
    }

    /// 计算当前已下载的总字节数。
    /// 这是已完成块的字节数和所有活跃块当前已下载字节数的总和。
    pub fn total_downloaded(&self) -> u64 {
        self.completed_bytes
            + self
                .chunks
                .values()
                .map(|c| c.downloaded_bytes)
                .sum::<u64>()
    }

    /// 计算当前的总下载速度。
    /// 这是所有活跃块速度的总和。
    pub fn total_speed(&self) -> f64 {
        self.chunks.values().map(|c| c.speed).sum::<f64>()
    }

    /// 检查下载是否已完成。
    pub fn is_download_finished(&self) -> bool {
        self.total_downloaded() >= self.total_file_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regression_card_complete_uses_size_not_stale_downloaded() {
        // 卡100%回归：broadcast Lagged 导致 ChunkProgress 丢失，state 中 downloaded 仍为 0，
        // complete_chunk 必须按 size() 累加而非 stale downloaded，否则 is_finished 永假
        let mut state = DownloadState::with_completed(1000, 0);
        let chunk_id = 1;
        // 模拟已插入但未更新进度的块
        state.chunks.insert(chunk_id, ChunkState::new(chunk_id, 0, 999));
        // 不更新 downloaded（保持 0 模拟 Lagged 丢失 final Progress）
        state.complete_chunk(&chunk_id);
        assert_eq!(state.completed_bytes, 1000, "should use size() not stale 0");
        assert!(state.is_download_finished(), "should be finished after size add");
        assert_eq!(state.total_downloaded(), 1000);
    }

    #[test]
    fn regression_preserve_exact_avoids_throttle_gap() {
        // 节流 64KiB 窗口：失败时 state.downloaded 滞后，preserve 需用精确 offset
        let mut state = DownloadState::with_completed(1000, 0);
        let chunk_id = 2;
        let mut chunk = ChunkState::new(chunk_id, 0, 999);
        chunk.update_downloaded(400); // stale，实际已下载 600
        state.chunks.insert(chunk_id, chunk);
        state.preserve_partial_exact(&chunk_id, 600);
        // 应取 max(stale 400, exact 600) =600
        assert_eq!(state.completed_bytes, 600);
        // 模拟 retry handler 移除 chunk 后，total 应为精确值
        state.chunks.remove(&chunk_id);
        assert_eq!(state.total_downloaded(), 600);
    }
    #[test]
    fn regression_speed_guard_tiny_elapsed() {
        let mut chunk = ChunkState::new(1, 0, 9999);
        chunk.update_downloaded(100 * 1024);
        // 极小 elapsed 0.001s 来自 interval 补偿突发，应被 guard 忽略，speed 保持 0
        chunk.update_speed(0.001, 0.30);
        assert_eq!(chunk.speed, 0.0, "tiny elapsed should be ignored");
        // 正常 0.5s 应计算
        chunk.update_speed(0.5, 0.30);
        assert!(chunk.speed > 0.0 && chunk.speed < 1_000_000_000.0);
    }

    #[test]
    fn regression_speed_cap() {
        let mut chunk = ChunkState::new(1, 0, 10_000_000);
        // 模拟 Lagged 补发巨大 delta：2GiB 在 0.5s 内
        chunk.update_downloaded(2 * 1024 * 1024 * 1024);
        chunk.update_speed(0.5, 0.30);
        assert!(chunk.speed <= 1024.0 * 1024.0 * 1024.0 + 1.0, "should be capped at 1GiB/s");
    }

    #[test]
    fn regression_speed_zero_delta_no_spike() {
        let mut chunk = ChunkState::new(1, 0, 1000);
        chunk.update_downloaded(100);
        chunk.update_speed(0.5, 0.30);
        let speed1 = chunk.speed;
        // 无新数据时不应突变，last_sampled 更新后 speed 保持
        chunk.update_speed(0.5, 0.30);
        assert_eq!(chunk.speed, speed1, "zero delta should not change speed");
    }
}
