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

    /// 根据新下载的字节数和经过的时间来更新速度。
    /// 使用指数移动平均法（EMA）进行平滑处理。
    pub fn update_speed(&mut self, elapsed_secs: f64, smoothing_factor: f64) {
        let newly_downloaded = self
            .downloaded_bytes
            .saturating_sub(self.last_sampled_bytes);
        let instantaneous_speed = newly_downloaded as f64 / elapsed_secs;
        if self.speed == 0.0 {
            // 第一次计算速度时，直接使用瞬时速度
            self.speed = instantaneous_speed;
        } else {
            // 使用平滑算法更新速度
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

    /// 将一个块标记为已完成。
    /// 这会将其从活跃块列表中移除，并将其下载的字节数累加到 `completed_bytes`。
    pub fn complete_chunk(&mut self, id: &ChunkId) {
        if let Some(chunk) = self.chunks.remove(id) {
            self.completed_bytes += chunk.downloaded_bytes;
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
