//! 一个支持多线程、断点续传和动态并发控制的高性能下载器库。
//!
//! # 核心特性
//!
//! - **多线程下载**：将文件分割成多个块（Chunk），并行下载以提高速度。
//! - **断点续传**：下载进度会被保存，中断后可从上次的位置继续。
//! - **动态并发控制**：根据网络速度动态调整并发线程数，以寻找最佳下载效率。
//! - **异步 IO**：完全基于 `tokio` 构建，提供高并发、低开销的 IO 操作。
//! - **自定义客户端**：允许用户通过 `reqwest::ClientBuilder` 自定义 HTTP 客户端（例如，设置代理、超时、自定义头等）。
//!
//! # 架构
//!
//! 系统采用 CSP（Communicating Sequential Processes）模型的通信机制与 Actor 模型
//! 的独立执行单元设计相结合，构建出一种无锁、消息驱动、事件分发式并发结构。
//!
//! - **`Downloader`**: 作为系统的入口和监督者，负责初始化和启动所有组件。
//! - **`MonitorActor`**: 系统的事件中心（Event Hub），负责创建和管理所有的下载工作单元（`ChunkActor`），
//!   聚合进度信息，执行并发策略和重试逻辑。
//! - **`ChunkActor`**: 独立的下载工作单元，负责下载文件的特定范围（一个块）。
//! - **`WriterActor`**: 独立的文件写入单元，负责将所有下载的数据块按正确的顺序写入文件，避免了多线程写入的锁竞争。
//!
//! # 使用示例
//!
//! ```rust,no_run
//! use downloader_rs::{Downloader, DownloaderConfig};
//! use tokio::sync::broadcast;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let url = "https://example.com/largefile.zip";
//!     let output_path = "largefile.zip";
//!     let config = DownloaderConfig {
//!         workers: 16,
//!         ..Default::default()
//!     };
//!
//!     let downloader = Downloader::new(url, output_path, config, || reqwest::ClientBuilder::new());
//!
//!     let progress_handler = |total_size: u64, mut info_rx: broadcast::Receiver<downloader_rs::DownloadInfo>| async move {
//!         while let Ok(info) = info_rx.recv().await {
//!             if let downloader_rs::DownloadInfo::MonitorUpdate { total_downloaded, total_speed, .. } = info {
//!                 let progress = if total_size > 0 {
//!                     total_downloaded as f64 / total_size as f64 * 100.0
//!                 } else {
//!                     0.0
//!                 };
//!                 println!(
//!                     "下载进度: {:.2}%, 速度: {:.2} MB/s",
//!                     progress,
//!                     total_speed / 1024.0 / 1024.0
//!                 );
//!             }
//!         }
//!     };
//!
//!     if let Err(e) = downloader.run(progress_handler).await {
//!         eprintln!("下载失败: {}", e);
//!     } else {
//!         println!("下载成功！");
//!     }
//! }
//! ```

mod downloader;
mod monitor;
mod types;
mod util;

// --- 公共 API 导出 ---

// 导出核心的 `Downloader`，它是用户的主要入口点。
pub use downloader::Downloader;
// 重新导出 `reqwest`，允许用户提供自定义的 `ClientBuilder`。
pub use reqwest;
// 导出公共类型，方便用户在类型注解和模式匹配中使用。
pub use types::{ChunkId, DownloadError, DownloadInfo, DownloaderConfig, Result};
