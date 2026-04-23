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
//! ## 功能模块
//!
//! 本库通过 feature flags 来启用可选功能，默认启用所有功能：
//!
//! | Feature | 默认启用 | 描述 |
//! |---------|---------|------|
//! | `resume` | ✅ | 断点续传功能 |
//! | `progress` | ✅ | 下载进度监控功能 |
//! | `proxy` | ✅ | 代理支持功能 |
//! | `multi-source` | ✅ | 多源下载功能 |
//!
//! # 快速开始
//!
//! ## 基础下载
//!
//! ```no_run
//! use simple_downloader::Downloader;
//!
//! #[tokio::main]
//! async fn main() {
//!     match Downloader::builder(
//!         "https://proof.ovh.net/files/100Mio.dat", // 下载链接
//!         "100Mio.dat",                             // 保存路径
//!     )
//!     .workers(16) // 设置并发线程数
//!     .download()
//!     .await
//!     {
//!         Ok(_) => println!("下载成功！"),
//!         Err(e) => eprintln!("下载失败: {}", e),
//!     }
//! }
//! ```
//!
//! ## 带进度监控的下载
//!
//! ```no_run
//! use simple_downloader::{Downloader, DownloadInfo};
//! use tokio::runtime::Runtime;
//!
//! #[tokio::main]
//! async fn main() {
//!     Downloader::builder("https://proof.ovh.net/files/100Mio.dat", "100Mio.dat")
//!         .workers(16)
//!         .run(|total_size, mut info_rx| async move {
//!             println!("文件总大小: {} bytes", total_size);
//!             while let Ok(info) = info_rx.recv().await {
//!                 println!(
//!                     "已下载: {}/{} bytes, 速度: {:.2} MB/s, 进度: {:.1}%",
//!                     info.downloaded_bytes(),
//!                     total_size,
//!                     info.speed_mbps(),
//!                     info.progress_percent()
//!                 );
//!             }
//!         })
//!         .await
//!         .unwrap();
//! }
//! ```
//!
//! ## 多源下载
//!
//! ```no_run
//! use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = MultiSourceConfig::new("output.bin", 32, 0.5)
//!         .with_sources(vec![
//!             SourceConfig::new("https://mirror1.example.com/file.bin"),
//!             SourceConfig::new("https://mirror2.example.com/file.bin"),
//!             SourceConfig::new("https://mirror3.example.com/file.bin"),
//!         ]);
//!
//!     Downloader::new_multi(config, Default::default)
//!         .download()
//!         .await
//!         .unwrap();
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

// 重新导出 `reqwest`，允许用户提供自定义的 `ClientBuilder`。
pub use reqwest;
