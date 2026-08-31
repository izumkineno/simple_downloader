//! 限速示例（需 `rate-limit` + `progress`）
//!
//! ```bash
//! cargo run --features rate-limit,progress --example with_rate_limit
//! ```

use simple_downloader::{DownloadInfo, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 使用本地 test_server 演示更稳定，若无 test_server 则回退到 proof.ovh.net
    let url = std::env::var("TEST_URL")
        .unwrap_or_else(|_| "https://proof.ovh.net/files/10Mio.dat".to_string());
    let out = std::env::var("TEST_OUT").unwrap_or_else(|_| "10Mio_rate_limited.dat".to_string());

    println!("下载 {} -> {} (限速 512 KiB/s, burst 64 KiB)", url, out);

    #[cfg(feature = "rate-limit")]
    {
        Downloader::builder(url, out)
            .workers(8)
            .speed_limit(512 * 1024)
            .with_burst(64 * 1024)
            .run(|total, mut rx| async move {
                println!("总大小: {} bytes", total);
                while let Ok(info) = rx.recv().await {
                    if let DownloadInfo::MonitorUpdate {
                        total_downloaded,
                        total_speed,
                        ..
                    } = info
                    {
                        println!(
                            "已下载 {}/{}  ({:.1}%) 速度 {:.2} MiB/s",
                            total_downloaded,
                            total,
                            info.progress_percent(),
                            total_speed / 1024.0 / 1024.0
                        );
                    }
                }
            })
            .await?;
    }

    #[cfg(not(feature = "rate-limit"))]
    {
        eprintln!("请启用 rate-limit feature: cargo run --features rate-limit,progress --example with_rate_limit");
        Downloader::builder(url, out).workers(8).download().await?;
    }

    println!("下载完成");
    Ok(())
}
