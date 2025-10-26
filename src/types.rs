/// 定义库中使用的公共类型、错误、配置和内部消息。
use bytes::Bytes;
use std::io;
use std::time::Duration;
use thiserror::Error;

// --- 公共类型 ---

/// 下载块（线程）ID 的类型别名。
pub type ChunkId = u64;

/// 库中通用的 `Result` 类型别名，错误类型为 `DownloadError`。
pub type Result<T> = std::result::Result<T, DownloadError>;

/// 定义了库中可能发生的所有公共错误类型。
#[derive(Debug, Error)]
pub enum DownloadError {
    /// 网络请求失败，包装了来自 `reqwest` 的错误。
    #[error("网络请求失败: {0}")]
    Request(#[from] reqwest::Error),
    /// 文件 I/O 错误，例如创建或写入文件失败。
    #[error("文件 I/O 错误: {0}")]
    Io(#[from] io::Error),
    /// 并发任务执行失败（例如，任务 panic）。
    #[error("并发任务执行失败: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// 无法从服务器响应头中获取文件大小（Content-Length）。
    /// 这是启动下载的必要条件。
    #[error("无法从服务器响应头中获取文件大小 (Content-Length)")]
    MissingContentLength,
}

/// 面向用户的状态和进度信息消息。
///
/// 用户通过订阅 `Downloader::run` 中的广播信道来接收这些消息。
#[derive(Clone, Debug)]
pub enum DownloadInfo {
    /// 来自监控器的聚合进度更新。
    MonitorUpdate {
        /// 文件总大小（字节）。
        total_size: u64,
        /// 已下载的总字节数。
        total_downloaded: u64,
        /// 所有活动块的聚合下载速度（字节/秒）。
        total_speed: f64,
        /// 每个活动块的详细信息。
        /// 元组内容: `(id, 块总大小, 已下载字节, 速度, 状态码)`。
        /// 状态码:
        /// - `0`: 下载中
        /// - `2`: 等待重试
        /// - `3`: 延迟重试
        chunk_details: Vec<(ChunkId, u64, u64, f64, u8)>,
    },
    /// 一个块的状态已改变（例如，开始重试）。
    ChunkStatusChanged {
        /// 状态发生改变的块 ID。
        id: ChunkId,
        /// 新的状态码。
        /// - `2`: 等待重试
        /// - `3`: 延迟重试
        status: u8,
        /// 描述状态变更原因的可选消息。
        message: Option<String>,
    },
}

/// 下载器的配置选项。
#[derive(Debug, Clone)]
pub struct DownloaderConfig {
    /// 并发下载的工作线程（块）的最大数量。
    pub workers: u64,
    /// 进度更新事件的发送间隔（秒）。
    pub update_interval: f64,
    /// 内部 Actor 之间通信信道的容量。
    pub channel_capacity: usize,
    /// 文件写入 Actor 的任务队列容量。
    pub writer_queue_capacity: usize,
    /// 一个块可被进一步分裂的最小尺寸（字节）。
    pub min_chunk_size_for_split: u64,
    /// 计算下载速度时使用的平滑因子（用于指数移动平均）。
    /// 值越接近 1.0，近期速度的权重越高，速度曲线波动越大。
    /// 值越接近 0.0，历史速度的权重越高，速度曲线越平滑。
    pub speed_smoothing_factor: f64,
    /// 一个失败的块可以被立即重试的最大次数。
    pub max_immediate_retries: u32,
    /// 失败后首次重试前的等待延迟。
    pub initial_retry_delay: Duration,
    /// 超过最大立即重试次数后，进入更长的延迟重试周期。
    pub long_retry_delay: Duration,
    /// 并发管理器两次执行分片决策之间的最小时间间隔，以防止过于频繁地调整并发。
    pub concurrency_split_delay: Duration,
}

impl Default for DownloaderConfig {
    /// 提供一组合理的默认配置。
    fn default() -> Self {
        Self {
            workers: 8,
            update_interval: 1.0,
            channel_capacity: 1024,
            writer_queue_capacity: 128,
            min_chunk_size_for_split: 1024 * 10, // 10 KB
            speed_smoothing_factor: 0.15,
            max_immediate_retries: 10,
            initial_retry_delay: Duration::from_secs(2),
            long_retry_delay: Duration::from_secs(10),
            concurrency_split_delay: Duration::from_millis(200),
        }
    }
}

// --- 内部 Actor 系统消息 ---

/// 在 Actor 系统内部，从 `ChunkActor` 发送给 `MonitorActor` 的事件。
#[derive(Debug)]
pub(crate) enum SystemEvent {
    /// 报告一个块已成功下载完成。
    ChunkCompleted { id: ChunkId },
    /// 报告一个块下载失败。
    ChunkFailed {
        id: ChunkId,
        start: u64,
        end: u64,
        error: String,
    },
    /// 报告一个块已被分裂，请求 `MonitorActor` 创建新任务。
    ChunkBisected {
        original_id: ChunkId,
        new_start: u64,
        new_end: u64,
    },
    /// 报告一个块的下载进度。
    ChunkProgress { id: ChunkId, downloaded: u64 },
}

/// 在 Actor 系统内部，由 `MonitorActor` 或 `Downloader` 发出的指令。
#[derive(Debug, Clone)]
pub(crate) enum SystemCommand {
    /// 指示 `WriterActor` 将数据写入文件。
    WriteFile { offset: u64, data: Bytes },
    /// 指示一个 `ChunkActor` 将自身分裂。
    BisectDownload { id: ChunkId },
    /// 指示所有 Actor 任务终止。
    TerminateAll,
}
