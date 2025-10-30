// types.rs
use std::{fmt::Debug, sync::Arc};

use bytes::Bytes;
use faststr::FastStr;
use reqwest::RequestBuilder;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, DownloadError>;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("网络请求失败: {0}")]
    Request(#[from] reqwest::Error),
    #[error("文件 I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("并发任务执行失败: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("无法从服务器响应头中获取文件大小 (Content-Length)")]
    MissingContentLength,
}

pub type ChunkId = u64;

// 定义一个类型别名，方便使用
// 输入: (current_offset: u64, current_end: u64)
// 输出: Option<new_start_byte: u64> , 返回 None 表示不进行分裂
type FuncSplit = Box<dyn Fn(u64, u64) -> Option<u64> + Send + Sync>;

/// 状态信息广播事件 (StatusInfo)
///
/// 用于分片任务向外部（如下载管理器）广播其状态和生命周期事件。
#[derive(Debug, Clone)]
pub enum EventStatus {
    /// 分片任务已开始
    ChunkStarted {
        id: ChunkId,
        start_byte: u64,
        end_byte: u64,
    },
    /// 分片下载进度更新
    ChunkProgress {
        id: ChunkId,
        // 此分片原始的起始字节
        start_byte: u64,
        // 此分片当前（可能因分裂而改变）的结束字节
        end_byte: u64,
        // 从 start_byte 开始，已经下载的字节数
        downloaded_bytes: u64,
    },
    /// 分片被分裂
    ChunkSplited {
        original_id: ChunkId,
        // 新分片的起始字节
        new_start_byte: u64,
        // 新分片的结束字节
        new_end_byte: u64,
    },
    /// 分片下载完成
    ChunkCompleted { id: ChunkId },
    /// 分片被终止
    ChunkTerminated {
        id: ChunkId,
        start_byte: u64,
        end_byte: u64,
        downloaded_bytes: u64,
    },

    // 交给重试模块

    /// 分片下载失败
    ChunkFailed {
        id: ChunkId,
        start_byte: u64,
        end_byte: u64,
        downloaded_bytes: u64,
        error: FastStr,
    },
}

pub enum EventThread {
    SplitChunk {
        id: ChunkId,
        strategy: Arc<dyn Fn(u64, u64) -> Option<u64> + Send + Sync>,
    },
    Terminate(ChunkId),
    TerminateAll,
}

impl Debug for EventThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SplitChunk { id, strategy } => f
                .debug_struct("SplitChunk")
                .field("id", id)
                .field("strategy", &strategy(0, 0))
                .finish(),
            Self::Terminate(arg0) => f.debug_tuple("Terminate").field(arg0).finish(),
            Self::TerminateAll => write!(f, "TerminateAll"),
        }
    }
}

// 手动为 ThreadEvent 实现 Clone
impl Clone for EventThread {
    fn clone(&self) -> Self {
        match self {
            Self::SplitChunk { id, strategy } => Self::SplitChunk {
                id: *id,
                strategy: strategy.clone(),
            },
            Self::Terminate(id) => Self::Terminate(*id),
            Self::TerminateAll => Self::TerminateAll,
        }
    }
}

#[derive(Debug)]
pub enum EventResource {
    TestResources,
    ReqResource,
    ResResource(RequestBuilder),
    WriteFile { offset: u64, data: Bytes },
}
