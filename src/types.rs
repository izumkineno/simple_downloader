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

    /// 无效参数。
    #[error("无效参数: {0}")]
    InvalidArgument(String),
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

/// 下载进度和状态信息 — 面向 UI 的稳定契约（0.6.2+）。
///
/// ### 稳定性保证（SemVer Minor 兼容）
/// - `#[non_exhaustive]` 自 `0.5.5` 起：新增变体为 **minor** 兼容变更，`match` 必须含 `_` 分支。
/// - `MonitorUpdate` 新增字段亦为 **minor**：旧代码忽略即可，读取请用 `..`。
/// - 现有变体与现有字段的语义 **patch 内不变**（见下表），删除/重命名/改类型仅在 **major**。
/// - 辅助方法 `progress_percent/speed_mbps/downloaded_bytes/total_bytes/is_complete` 的
///   “非 MonitorUpdate 返回 0/false” 亦为稳定行为。
///
/// ### UI 应依赖的最小稳定集
/// | 能力 | 稳定来源 | 说明 |
/// |---|---|---|
/// | 总进度/速度 | `MonitorUpdate { total_size, total_downloaded, total_speed }` + `progress_percent()` | `total_size==0` 表示“未知大小流式”或“0 字节文件”，后者 `is_complete()==true`，前者 `false`（由 `DownloadComplete` 终局判定） |
/// | 已下载/总量 | `downloaded_bytes()/total_bytes()` | 非 `MonitorUpdate` 返回 0 |
/// | 是否完成 | `is_complete()` | 仅 `MonitorUpdate` 有效；流式下载完成以 `DownloadComplete` 为准 |
/// | 细粒度块 | `chunk_details: Vec<(id, size, downloaded, speed, status_u8)>` | `status_u8` 见下，新增状态码为 minor |
///
/// ### 字段与状态码契约
/// - `MonitorUpdate.total_size`：`0` 仅两种含义，`is_complete()` 已区分；UI 展示时 `0` 建议显示 `--` 而非 `0%`。
/// - `total_speed`：`bytes/s` 的 EMA 平滑值，`speed_mbps()` 已除 `1MiB`，限速下为限速后观测值。
/// - `chunk_details[].4 status_u8`：`0 下载中/1 重试中/2 等待重试/3 延迟重试/4 已完成/5 失败`，与 `ChunkStatusChanged.status` 一致，新增码为 minor。
/// - `ChunkProgress { id, start_byte, end_byte, downloaded }`：`downloaded` 为该块已落盘字节，非增量。
/// - `ChunkFailed { id, start, end, error }`：`error` 为人类可读，UI 透传即可，不作 `match` 分支依赖。
/// - `ChunkBisected/DownloadComplete/ChunkStatusChanged`：通知类，UI 可忽略，仅 `MonitorUpdate` 为聚合权威。
///
/// 当使用 `run()` 方法启动下载时，可以通过接收器获取此类型的消息，
/// 实时监控下载进度和状态变化。
/// `0.5.5+` 标记 `#[non_exhaustive]`，新增变体/字段为兼容变更，调用方需 `_` 分支。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DownloadInfo {
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
    /// 获取下载进度百分比（0.0 ~ 100.0）— 稳定契约。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 `0.0`（稳定行为，UI 可对任意 `DownloadInfo` 安全调用）。
    /// `total_size==0` 时返回 `0.0`，0 字节文件请用 `is_complete()` 判定。
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

    /// 获取下载速度（MB/秒）— 稳定契约。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 `0.0`。
    pub fn speed_mbps(&self) -> f64 {
        match self {
            DownloadInfo::MonitorUpdate { total_speed, .. } => *total_speed / (1024.0 * 1024.0),
            _ => 0.0,
        }
    }

    /// 获取已下载的字节数 — 稳定契约。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 `0`。
    pub fn downloaded_bytes(&self) -> u64 {
        match self {
            DownloadInfo::MonitorUpdate {
                total_downloaded, ..
            } => *total_downloaded,
            _ => 0,
        }
    }

    /// 获取文件总大小（字节）— 稳定契约。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 `0`。`0` 的语义见顶层契约。
    pub fn total_bytes(&self) -> u64 {
        match self {
            DownloadInfo::MonitorUpdate { total_size, .. } => *total_size,
            _ => 0,
        }
    }

    /// 检查下载是否已完成 — 稳定契约。
    ///
    /// 仅对 `MonitorUpdate` 变体有效，其他变体返回 `false`。`0/0` 完成、`0/N` 未完成（流式由 `DownloadComplete` 终局）。
    pub fn is_complete(&self) -> bool {
        match self {
            DownloadInfo::MonitorUpdate {
                total_size,
                total_downloaded,
                ..
            } => {
                if *total_size == 0 {
                    // 0 字节文件：0/0 完成；未知大小流式：0/N 未完成（由 DownloadComplete 判定）
                    *total_downloaded == 0
                } else {
                    *total_downloaded >= *total_size
                }
            }
            _ => false,
        }
    }
}
