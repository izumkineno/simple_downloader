// state.rs
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::types::*;

/// 分片状态
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkStatus {
    /// 正在下载
    Downloading,
    /// 已完成
    Completed,
    /// 下载失败
    Failed,
    /// 被终止
    Terminated,
}

/// 缓存的分片信息
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    /// 分片 ID
    pub id: ChunkId,
    /// 分片在文件中的起始字节位置
    pub start_byte: u64,
    /// 分片在文件中的结束字节位置
    pub end_byte: u64,
    /// 已下载的字节数
    pub downloaded_bytes: u64,
    /// 分片状态
    pub status: ChunkStatus,
    /// 分片开始下载时间
    pub start_time: Instant,
}

/// 下载管理器 (职责简化)
///
/// 负责维护所有分片的状态和整个文件的下载进度。
/// 速度计算的职责已移交给 SpeedMonitor。
#[derive(Debug)]
pub struct DownloadState {
    /// 下载文件的总大小
    pub file_size: u64,
    /// 使用原子类型，允许多线程安全地更新总下载量
    pub total_downloaded: Arc<AtomicU64>,
    /// 缓存每个分片的详细信息
    pub chunks: HashMap<ChunkId, ChunkInfo>,
    /// 下载开始时间
    start_time: Instant,
}

impl DownloadState {
    /// 创建一个新的下载管理器
    pub fn new(file_size: u64) -> Self {
        Self {
            file_size,
            total_downloaded: Arc::new(AtomicU64::new(0)),
            chunks: HashMap::new(),
            start_time: Instant::now(),
        }
    }

    /// 返回 total_downloaded 的原子引用句柄，供 SpeedMonitor 使用
    pub fn get_total_downloaded_handle(&self) -> Arc<AtomicU64> {
        self.total_downloaded.clone()
    }

    /// 处理来自下载线程的事件
    pub fn handle_event(&mut self, event: EventStatus) {
        match event {
            EventStatus::ChunkStarted {
                id,
                start_byte,
                end_byte,
            } => {
                let chunk_info = ChunkInfo {
                    id,
                    start_byte,
                    end_byte,
                    downloaded_bytes: 0,
                    status: ChunkStatus::Downloading,
                    start_time: Instant::now(),
                };
                self.chunks.insert(id, chunk_info);
            }
            EventStatus::ChunkProgress {
                id,
                start_byte,
                end_byte,
                downloaded_bytes,
            } => {
                if let Some(chunk) = self.chunks.get_mut(&id) {
                    chunk.start_byte = start_byte;
                    chunk.end_byte = end_byte;
                    let delta = downloaded_bytes.saturating_sub(chunk.downloaded_bytes);
                    if delta > 0 {
                        self.total_downloaded.fetch_add(delta, Ordering::Relaxed);
                        chunk.downloaded_bytes = downloaded_bytes;
                    }
                }
            }
            EventStatus::ChunkSplited {
                original_id,
                new_start_byte,
                new_end_byte: _,
            } => {
                if let Some(original_chunk) = self.chunks.get_mut(&original_id) {
                    original_chunk.end_byte = new_start_byte.saturating_sub(1);
                }
            }
            EventStatus::ChunkCompleted { id } => {
                if let Some(chunk) = self.chunks.get_mut(&id) {
                    chunk.status = ChunkStatus::Completed;
                }
            }
            EventStatus::ChunkTerminated {
                id,
                start_byte,
                end_byte,
                downloaded_bytes,
            } => {
                if let Some(chunk) = self.chunks.get_mut(&id) {
                    chunk.status = ChunkStatus::Terminated;
                    chunk.start_byte = start_byte;
                    chunk.end_byte = end_byte;
                    chunk.downloaded_bytes = downloaded_bytes;
                }
            }
            _ => {}
        }
    }

    /// 获取整个任务的平均下载速度 (bytes/sec)
    pub fn get_average_speed(&self) -> f64 {
        let duration = self.start_time.elapsed().as_secs_f64();
        if duration > 0.0 {
            self.total_downloaded.load(Ordering::Relaxed) as f64 / duration
        } else {
            0.0
        }
    }

    /// 获取下载进度 (0.0 to 1.0)
    pub fn get_progress(&self) -> f64 {
        if self.file_size == 0 {
            return 0.0;
        }
        self.total_downloaded.load(Ordering::Relaxed) as f64 / self.file_size as f64
    }
}
