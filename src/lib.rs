//! 一个高性能、可配置的异步下载器库，支持多线程、断点续传、多源下载和动态并发控制。
//!
//! ## 主要特性
//!
//! - ✅ **异步架构**：基于 Tokio 运行时，充分利用多核 CPU 性能
//! - ✅ **动态并发控制**：自动调整并发下载线程数，优化下载速度
//! - ✅ **断点续传**：支持下载中断后自动恢复，无需重新下载已完成部分
//! - ✅ **进度监控**：实时获取下载进度、速度、剩余时间等信息
//! - ✅ **多源下载**：支持同时从多个源下载同一个文件，提高下载速度和稳定性
//! - ✅ **代理支持**：支持 HTTP/HTTPS/SOCKS5 代理配置
//! - ✅ **两级重试机制**：针对网络错误和 chunk 下载失败自动重试
//! - ✅ **灵活配置**：通过 Builder 模式自定义各种下载参数
//!
//! ## 功能模块（以 `Cargo.toml:6-11` 为准）
//!
//! | Feature | 默认 | 描述 | 启用后新增 API |
//! |---|:---:|---|---|
//! | _(none)_ | ✅ | 基础单源多线程下载 | `Downloader::builder().download()` |
//! | `resume` | ❌ | 断点续传、sidecar `*.download.bitcode` | `DownloadBuilder::resume()`, `Downloader::with_resume()`, `ResumeMetadata` 等 |
//! | `progress` | ❌ | 进度事件与回调 | `DownloadInfo`, `Downloader::run()`, `DownloadBuilder::run()` |
//! | `multi-source` | ❌ | 多源调度建模 | `MultiSourceConfig`, `SourceConfig`, `LaneModel`, `Downloader::new_multi()` |
//! | `proxy` | ❌ | 代理 lane 建模（隐含 `multi-source`） | `ProxyConfig`, `SourceConfig::with_proxies()` |
//!
//! 完整调用形态见 [`docs/usage.md`](https://github.com/simple_downloader/docs/usage.md)（仓库内 `docs/usage.md`），
//! 配置项见 `docs/configuration.md`，错误全表见 `docs/errors.md`。
//!
//! ## 安装
//!
//! ```toml
//! # 最轻量：仅基础下载
//! simple_downloader = { version = "0.3", default-features = false }
//! # 常用：基础 + 断点续传 + 进度
//! simple_downloader = { version = "0.3", default-features = false, features = ["resume", "progress"] }
//! # 全功能
//! simple_downloader = { version = "0.3", default-features = false, features = ["resume", "progress", "multi-source", "proxy"] }
//! ```
//!
//! ## 快速开始
//!
//! ### 基础下载（无需任何 feature）
//!
//! ```no_run
//! use simple_downloader::Downloader;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     Downloader::builder(
//!         "https://proof.ovh.net/files/100Mio.dat", // 下载链接
//!         "100Mio.dat",                             // 保存路径
//!     )
//!     .workers(16) // 并发上限，受服务器 Range/文件大小自动降级约束
//!     .download()
//!     .await?;
//!     Ok(())
//! }
//! ```
//!
//! ### 带进度监控的下载（需 `progress`）
//!
//! ```no_run
//! use simple_downloader::{Downloader, DownloadInfo};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     Downloader::builder("https://proof.ovh.net/files/100Mio.dat", "100Mio.dat")
//!         .workers(16)
//!         .run(|total_size, mut info_rx| async move {
//!             println!("文件总大小: {} bytes", total_size);
//!             while let Ok(info) = info_rx.recv().await {
//!                 // 仅 MonitorUpdate 携带可聚合进度，见 DownloadInfo 文档
//!                 if let DownloadInfo::MonitorUpdate { .. } = &info {
//!                     println!(
//!                         "已下载: {}/{} bytes, 速度: {:.2} MB/s, 进度: {:.1}%",
//!                         info.downloaded_bytes(),
//!                         total_size,
//!                         info.speed_mbps(),
//!                         info.progress_percent()
//!                     );
//!                 }
//!             }
//!         })
//!         .await?;
//!     Ok(())
//! }
//! ```
//!
//! ### 多源下载（需 `multi-source`）
//!
//! ```no_run
//! use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = MultiSourceConfig::new("output.bin", 32, 0.5)
//!         .with_sources(vec![
//!             SourceConfig::new("https://mirror1.example.com/file.bin").with_id("m1"),
//!             SourceConfig::new("https://mirror2.example.com/file.bin").with_id("m2"),
//!         ]);
//!     Downloader::new_multi(config, Default::default).download().await?;
//!     Ok(())
//! }
//! ```
//!
//! ### 断点续传（需 `resume`）
//!
//! ```no_run
//! use simple_downloader::Downloader;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 默认启用：同目录下 output.bin + output.bin.download.bitcode 自动恢复
//!     Downloader::builder("https://example.com/large.bin", "large.bin")
//!         .workers(16)
//!         .download()
//!         .await?;
//!     // 显式禁用
//!     Downloader::builder("https://example.com/large.bin", "large.bin")
//!         .resume(false)
//!         .download()
//!         .await?;
//!     Ok(())
//! }
//! ```
//!
//! ### 自定义 HTTP 客户端
//!
//! ```no_run
//! use simple_downloader::Downloader;
//! use reqwest::ClientBuilder;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     Downloader::builder("https://example.com/file.bin", "output.bin")
//!         .client_builder(|| {
//!             ClientBuilder::new()
//!                 .timeout(Duration::from_secs(120))
//!                 .connect_timeout(Duration::from_secs(10))
//!         })
//!         .download()
//!         .await?;
//!     Ok(())
//! }
//! ```

// 声明所有模块以构建库的结构。
#[doc(hidden)]
pub mod chunk;
#[doc(hidden)]
pub mod concurrency;
#[doc(hidden)]
pub mod downloader;
mod lane;
#[doc(hidden)]
pub mod monitor;
#[cfg(feature = "resume")]
mod resume;
#[doc(hidden)]
pub mod retry;
#[doc(hidden)]
pub mod state;
pub mod trace;
mod types;
#[doc(hidden)]
pub mod util;

// --- 公共 API 导出 ---

// 导出核心的 `Downloader`，它是用户的主要入口点。
pub use downloader::{DownloadBuilder, Downloader};
#[cfg(feature = "proxy")]
pub use lane::ProxyConfig;
#[cfg(feature = "multi-source")]
pub use lane::{
    LaneCandidate, LaneHealth, LaneModel, LaneScheduler, MultiSourceConfig, SourceConfig,
};
#[cfg(feature = "resume")]
pub use resume::{DEFAULT_SEGMENT_SIZE, ResumeMetadata, hash_bytes, metadata_path_for};

// 导出公共类型，方便用户在类型注解和模式匹配中使用。
#[cfg(feature = "progress")]
pub use types::DownloadInfo;
pub use types::{ChunkId, DownloadError, Result};

#[doc(hidden)]
pub mod internal {
    pub use crate::types::{DownloadCmd, DownloadInfo};
}

// 日志门面：基于 tracing 的调试/生产分级初始化，库本身不自动安装全局订阅者。
// 二进制按需调用 `simple_downloader::trace::init_tracing()` 即可通过
// RUST_LOG / SIMPLE_DOWNLOADER_LOG 控制级别；见 `crate::trace` 模块文档。
// `simple_downloader::tracing` 为兼容别名，指向同一模块。
#[doc(hidden)]
pub use crate::trace as tracing;

// 重新导出 `reqwest`，允许用户提供自定义的 `ClientBuilder`。
pub use reqwest;
