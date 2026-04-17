//! 一个支持多线程、断点续传和动态并发控制的下载器库。
//!
//!
//! # 使用示例
//!
//! ```no_run
//!use simple_downloader::Downloader;
//!
//!#[tokio::main]
//!async fn main() {
//!    match Downloader::builder(
//!        "https://proof.ovh.net/files/100Mio.dat", // 下载链接
//!        "100Mio.dat",                             // 保存路径
//!    )
//!    .workers(16)
//!    .download()
//!    .await
//!    {
//!        Ok(_) => println!("下载成功！"),
//!        Err(e) => eprintln!("下载失败: {}", e),
//!    }
//!}
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
