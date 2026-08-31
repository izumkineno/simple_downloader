//! 限速示例（需 `rate-limit` + `progress`）
//!
//! ```bash
//! cargo run --features rate-limit,progress --example with_rate_limit
//! # 多源分源限速 + 全局：
//! cargo run --features rate-limit,progress,multi-source --example with_rate_limit -- --multi
//! ```

use simple_downloader::{DownloadInfo, Downloader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let is_multi = std::env::args().any(|a| a == "--multi");
    if is_multi {
        #[cfg(all(feature = "rate-limit", feature = "multi-source"))]
        {
            use simple_downloader::{MultiSourceConfig, SourceConfig};
            let url = std::env::var("TEST_URL")
                .unwrap_or_else(|_| "https://proof.ovh.net/files/10Mb.dat".to_string());
            let out = std::env::var("TEST_OUT").unwrap_or_else(|_| "10Mb_multi_rate.dat".to_string());
            println!("多源限速: s1 300KiB/s burst 64KiB + s2 300KiB/s burst 32KiB, 全局 512KiB/s burst 64KiB -> {}", out);
            let cfg = MultiSourceConfig::new(out.clone(), 8, 0.5)
                .with_sources(vec![
                    SourceConfig::new(url.clone()).with_id("s1").with_speed_limit(300 * 1024).with_burst(64 * 1024),
                    SourceConfig::new(url).with_id("s2").with_speed_limit(300 * 1024).with_burst(32 * 1024),
                ])
                .with_global_speed_limit(512 * 1024)
                .with_global_burst(64 * 1024);
            Downloader::new_multi(cfg, || reqwest::ClientBuilder::new())
                .run(|total, mut rx| async move {
                    println!("总大小: {} bytes", total);
                    while let Ok(info) = rx.recv().await {
                        if let DownloadInfo::MonitorUpdate { total_downloaded, total_speed, .. } = info {
                            println!("已下载 {}/{} ({:.1}%) 速度 {:.2} MiB/s", total_downloaded, total, info.progress_percent(), total_speed/1024.0/1024.0);
                        }
                    }
                })
                .await?;
            println!("多源下载完成: {}", out);
            return Ok(());
        }
        #[cfg(not(all(feature = "rate-limit", feature = "multi-source")))]
        {
            eprintln!("多源示例需 --features rate-limit,multi-source,progress");
            return Ok(());
        }
    }
    // 单源限速（默认）
    let url = std::env::var("TEST_URL")
        .unwrap_or_else(|_| "https://proof.ovh.net/files/10Mb.dat".to_string());
    let out = std::env::var("TEST_OUT").unwrap_or_else(|_| "10Mb_rate_limited.dat".to_string());

    println!("单源限速 512 KiB/s burst 64 KiB: {} -> {}", url, out);

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
