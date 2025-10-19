//! 定义库中使用的各种公共类型、枚举和错误。

use bytes::Bytes;
use std::io;
use thiserror::Error;

/// 下载块（线程）ID 的类型别名。
pub type ChunkId = u64;

/// 库中通用的 `Result` 类型别名。
pub type Result<T> = std::result::Result<T, DownloadError>;

/// 定义了库中可能发生的所有错误类型。
#[derive(Debug, Error)]
pub enum DownloadError {
    /// 网络请求失败。
    #[error("网络请求失败: {0}")]
    Request(#[from] reqwest::Error),
    /// 文件 I/O 错误。
    #[error("文件 I/O 错误: {0}")]
    Io(#[from] io::Error),
    /// 并发任务执行失败（例如，任务 panic）。
    #[error("并发任务执行失败: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// 无法从服务器响应头中获取文件大小（Content-Length）。
    #[error("无法从服务器响应头中获取文件大小 (Content-Length)")]
    MissingContentLength,
}

/// 发送给下载器组件的控制命令。
#[derive(Debug, Clone)]
pub enum DownloadCmd {
    /// 写入文件数据的命令（发送给文件写入任务）。
    WriteFile { offset: u64, data: Bytes },
    /// 分割一个下载任务的命令（广播给所有块任务）。
    BisectDownload { id: ChunkId },
    /// 终止所有下载任务的命令（广播给所有任务）。
    TerminateAll,
}

/// 来自下载器组件的状态和进度信息消息。
#[derive(Clone, Debug)]
pub enum DownloadInfo {
    /// 来自单个下载块的原始进度更新。
    ChunkProgress {
        id: ChunkId,
        start_byte: u64,
        end_byte: u64,
        downloaded: u64,
    },
    /// 来自监控器的聚合进度更新。
    MonitorUpdate {
        total_size: u64,
        total_downloaded: u64,
        total_speed: f64,
        /// 每个块的详细信息：(id, size, downloaded, speed, status)
        /// status: 0=下载中, 1=重试中, 2=等待重试, 3=延迟重试, 4=已完成, 5=失败
        chunk_details: Vec<(ChunkId, u64, u64, f64, u8)>,
    },
    /// 一个下载块已成功完成。
    DownloadComplete(ChunkId),
    /// 一个下载块失败并请求重试。
    ChunkFailed {
        id: ChunkId,
        start: u64,
        end: u64,
        error: String,
    },
    /// 一个块已成功被分割。
    ChunkBisected {
        original_id: ChunkId,
        new_start: u64,
        new_end: u64,
    },
    /// 一个块的状态已改变。
    ChunkStatusChanged {
        id: ChunkId,
        /// status: 0=下载中, 1=重试中, 2=等待重试, 3=延迟重试, 4=已完成, 5=失败
        status: u8,
        message: Option<String>,
    },
}
