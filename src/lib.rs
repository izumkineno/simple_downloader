//! 一个支持多线程、断点续传和动态并发控制的下载器库。
//!
//!
//! # 使用示例
//!
//! ```no_run
//!use simple_downloader::{Downloader, DownloadInfo, reqwest::ClientBuilder};
//!use tokio::sync::broadcast;
//!
//!#[tokio::main]
//!async fn main() {
//!    let downloader = Downloader::new(
//!        "https://proof.ovh.net/files/100Mio.dat", // 下载链接
//!        "100Mio.dat",                             // 保存路径
//!        16,                                       // 最大并发线程数
//!        1.0,                                      // 进度更新间隔(秒)
//!        || ClientBuilder::new(),                  // 提供网络客户端构建器
//!    );
//!
//!    // 定义一个处理下载进度的闭包
//!    let progress_handler = |total_size: u64, mut info_rx: broadcast::Receiver<DownloadInfo>| async move {
//!        println!("文件总大小: {:.2} MB", total_size as f64 / 1024.0 / 1024.0);
//!
//!        // 循环接收并打印进度信息
//!        while let Ok(info) = info_rx.recv().await {
//!            if let DownloadInfo::MonitorUpdate { total_downloaded, total_speed, .. } = info {
//!                let progress = (total_downloaded as f64 / total_size as f64) * 100.0;
//!                println!(
//!                    "进度: {:.2}% | 已下载: {:.2} MB | 速度: {:.2} MB/s",
//!                    progress,
//!                    total_downloaded as f64 / 1024.0 / 1024.0,
//!                    total_speed / 1024.0 / 1024.0
//!                );
//!            }
//!        }
//!    };
//!
//!    // 启动下载！
//!    match downloader.run(progress_handler).await {
//!        Ok(_) => println!("下载成功！"),
//!        Err(e) => eprintln!("下载失败: {}", e),
//!    }
//!}
//! ```

// 声明所有模块以构建库的结构。
pub mod chunk;
pub mod concurrency;
pub mod downloader;
pub mod monitor;
pub mod retry;
pub mod state;
pub mod types;
pub mod util;

// --- 公共 API 导出 ---

// 导出核心的 `Downloader`，它是用户的主要入口点。
pub use downloader::Downloader;

// 导出公共类型，方便用户在类型注解和模式匹配中使用。
pub use types::{ChunkId, DownloadCmd, DownloadError, DownloadInfo, Result};

// 重新导出 `reqwest`，允许用户提供自定义的 `ClientBuilder`。
pub use reqwest;