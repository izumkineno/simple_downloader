//! 一个支持多线程、断点续传和动态并发控制的下载器库。
//!
//! # 架构
//!
//! 系统采用 CSP（Communicating Sequential Processes）模型的通信机制与 Actor 模型
//! 的独立执行单元设计相结合，构建出一种无锁、消息驱动、事件分发式并发结构。
//! `Monitor` 模块作为系统的事件分发器（Event Hub），负责集中监听和分发事件。

// 声明所有模块以构建库的结构。
pub mod downloader;
pub mod monitor;
pub mod types;
pub mod util;

// --- 公共 API 导出 ---

// 导出核心的 `Downloader`，它是用户的主要入口点。
pub use downloader::Downloader;

// 导出公共类型，方便用户在类型注解和模式匹配中使用。
pub use types::{ChunkId, DownloadError, DownloadInfo, Result};

// 重新导出 `reqwest`，允许用户提供自定义的 `ClientBuilder`。
pub use reqwest;
