//! 定义库中使用的各种公共类型、枚举和错误。

use bytes::Bytes;
use std::io;
use thiserror::Error;

// --- 公共类型 ---

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

/// 来自下载器组件的状态和进度信息消息（面向用户）。
#[derive(Clone, Debug)]
pub enum DownloadInfo {
    /// 来自监控器的聚合进度更新。
    MonitorUpdate {
        total_size: u64,
        total_downloaded: u64,
        total_speed: f64,
        /// 每个块的详细信息：(id, size, downloaded, speed, status)
        /// status: 0=下载中, 1=重试中, 2=等待重试, 3=延迟重试, 4=已完成, 5=失败
        chunk_details: Vec<(ChunkId, u64, u64, f64, u8)>,
    },
    /// 一个块的状态已改变。
    ChunkStatusChanged {
        id: ChunkId,
        /// status: 0=下载中, 1=重试中, 2=等待重试, 3=延迟重试, 4=已完成, 5=失败
        status: u8,
        message: Option<String>,
    },
}

// --- 内部 Actor 系统消息 ---

/// 发送给核心 `MonitorActor` 的事件。
#[derive(Debug)]
pub(crate) enum SystemEvent {
    ChunkCompleted {
        id: ChunkId,
    },
    ChunkFailed {
        id: ChunkId,
        start: u64,
        end: u64,
        error: String,
    },
    ChunkBisected {
        original_id: ChunkId,
        new_start: u64,
        new_end: u64,
    },
    ChunkProgress {
        id: ChunkId,
        downloaded: u64,
    },
}

/// 从 `MonitorActor` 发出的指令。
#[derive(Debug, Clone)]
pub(crate) enum SystemCommand {
    WriteFile { offset: u64, data: Bytes },
    BisectDownload { id: ChunkId },
    TerminateAll,
}
