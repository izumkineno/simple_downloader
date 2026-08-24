//! 定义库中使用的各种公共类型、枚举和错误。

use bytes::Bytes;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// 下载块（线程）ID 的类型别名。
pub type ChunkId = u64;

/// 库中通用的 `Result` 类型别名。
pub type Result<T> = std::result::Result<T, DownloadError>;

/// 定义了库中可能发生的所有错误类型。
#[derive(Debug, Error)]
pub enum DownloadError {
    /// 网络请求失败。
    ///
    /// 可能的原因：
    /// - 网络连接中断
    /// - 服务器无响应
    /// - DNS 解析失败
    /// - 请求超时
    #[error("网络请求失败: {0}")]
    Request(#[from] reqwest::Error),

    /// 文件 I/O 错误。
    ///
    /// 可能的原因：
    /// - 磁盘空间不足
    /// - 没有写入权限
    /// - 文件被其他进程占用
    /// - 磁盘损坏
    #[error("文件 I/O 错误: {0}")]
    Io(#[from] io::Error),

    /// 并发任务执行失败。
    ///
    /// 可能的原因：
    /// - 下载任务 panic
    /// - 运行时资源不足
    /// - 任务被强制终止
    #[error("并发任务执行失败: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// 无法从服务器响应头中获取文件大小（Content-Length）。
    ///
    /// 可能的原因：
    /// - 服务器不返回 Content-Length 头
    /// - 响应是动态生成的流
    /// - 服务器配置错误
    #[error("无法从服务器响应头中获取文件大小 (Content-Length)")]
    MissingContentLength,

    /// 多源模式下没有可用的下载源。
    ///
    /// 可能的原因：
    /// - 所有配置的下载源都无法访问
    /// - 所有下载源的文件校验失败
    /// - 下载源返回 4xx/5xx 错误
    #[error("没有可用的下载源")]
    NoAvailableSources,

    /// 断点续传元数据存在，但目标文件不存在。
    ///
    /// 可能的原因：
    /// - 上次下载后目标文件被手动删除
    /// - 文件路径被移动或重命名
    /// - 磁盘分区被卸载
    #[error("断点续传元数据存在，但目标文件不存在: {0}")]
    ResumeTargetMissing(PathBuf),

    /// 断点续传元数据无效。
    ///
    /// 可能的原因：
    /// - 元数据文件损坏
    /// - 元数据版本不兼容
    /// - 目标文件与元数据不匹配
    /// - 元数据被手动修改
    #[error("断点续传元数据无效: {0}")]
    ResumeMetadata(String),

    /// 块下载永久失败，已达重试上限。
    #[error("下载失败，已达重试上限: {0}")]
    PermanentFailure(String),
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

/// 下载进度和状态信息。
///
/// 当使用 `run()` 方法启动下载时，可以通过接收器获取此类型的消息，
/// 实时监控下载进度和状态变化。
#[derive(Clone, Debug)]
pub enum DownloadInfo {
    /// 单个下载块的进度更新。
    ///
    /// 当某个下载块接收到新的数据时，会发送此消息。
    ChunkProgress {
        /// 下载块 ID
        id: ChunkId,
        /// 块的起始字节位置
        start_byte: u64,
        /// 块的结束字节位置
        end_byte: u64,
        /// 该块已下载的字节数
        downloaded: u64,
    },

    /// 全局进度更新（由监控器定期聚合发送）。
    ///
    /// 这是最常用的进度信息，包含了整体下载进度、速度和所有块的状态。
    /// 默认每 0.5 秒发送一次，可以通过 `update_interval()` 方法配置。
    MonitorUpdate {
        /// 文件总大小（字节）
        total_size: u64,
        /// 已下载的总字节数
        total_downloaded: u64,
        /// 当前下载速度（字节/秒）
        total_speed: f64,
        /// 每个下载块的详细信息：
        /// (块ID, 块总大小, 已下载大小, 块下载速度, 块状态)
        ///
        /// 状态说明：
        /// - 0: 下载中
        /// - 1: 重试中
        /// - 2: 等待重试
        /// - 3: 延迟重试
        /// - 4: 已完成
        /// - 5: 失败
        chunk_details: Vec<(ChunkId, u64, u64, f64, u8)>,
    },

    /// 下载块完成通知。
    ///
    /// 当某个下载块成功下载完成时发送此消息。
    DownloadComplete(ChunkId),

    /// 下载块失败通知。
    ///
    /// 当某个下载块下载失败，即将进行重试时发送此消息。
    ChunkFailed {
        /// 下载块 ID
        id: ChunkId,
        /// 块的起始字节位置
        start: u64,
        /// 块的结束字节位置
        end: u64,
        /// 错误信息
        error: String,
    },

    /// 下载块分割通知。
    ///
    /// 当某个下载速度过慢的块被自动分割成两个小块时发送此消息。
    ChunkBisected {
        /// 原始块 ID
        original_id: ChunkId,
        /// 新块的起始字节位置
        new_start: u64,
        /// 新块的结束字节位置
        new_end: u64,
    },

    /// 下载块状态变化通知。
    ///
    /// 当某个下载块的状态发生变化时发送此消息。
    ChunkStatusChanged {
        /// 下载块 ID
        id: ChunkId,
        /// 新的状态：
        /// - 0: 下载中
        /// - 1: 重试中
        /// - 2: 等待重试
        /// - 3: 延迟重试
        /// - 4: 已完成
        /// - 5: 失败
        status: u8,
        /// 可选的状态说明信息
        message: Option<String>,
    },
}

impl DownloadInfo {
    /// 获取下载进度百分比（0.0 ~ 100.0）。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 0.0。
    pub fn progress_percent(&self) -> f64 {
        match self {
            DownloadInfo::MonitorUpdate {
                total_size,
                total_downloaded,
                ..
            } => {
                if *total_size == 0 {
                    0.0
                } else {
                    (*total_downloaded as f64 / *total_size as f64) * 100.0
                }
            }
            _ => 0.0,
        }
    }

    /// 获取下载速度（MB/秒）。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 0.0。
    pub fn speed_mbps(&self) -> f64 {
        match self {
            DownloadInfo::MonitorUpdate { total_speed, .. } => *total_speed / (1024.0 * 1024.0),
            _ => 0.0,
        }
    }

    /// 获取已下载的字节数。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 0。
    pub fn downloaded_bytes(&self) -> u64 {
        match self {
            DownloadInfo::MonitorUpdate {
                total_downloaded, ..
            } => *total_downloaded,
            _ => 0,
        }
    }

    /// 获取文件总大小（字节）。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 0。
    pub fn total_bytes(&self) -> u64 {
        match self {
            DownloadInfo::MonitorUpdate { total_size, .. } => *total_size,
            _ => 0,
        }
    }

    /// 检查下载是否已完成。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 false。
    pub fn is_complete(&self) -> bool {
        match self {
            DownloadInfo::MonitorUpdate {
                total_size,
                total_downloaded,
                ..
            } => *total_size > 0 && *total_downloaded >= *total_size,
            _ => false,
        }
    }
}
