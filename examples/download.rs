//! 示例 1: 简单的命令行进度打印

use simple_downloader::{DownloadInfo, Downloader, DownloaderConfig, reqwest::ClientBuilder};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    // 初始化日志记录器
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .init();

    // --- 1. 配置下载器 ---
    // 使用 DownloaderConfig 结构体来配置下载参数
    let config = DownloaderConfig {
        workers: 16,          // 最大并发线程数
        update_interval: 1.0, // 进度更新间隔(秒)
        ..Default::default()  // 其他选项使用默认值
    };

    // --- 2. 创建 Downloader 实例 ---
    // 注意：现在 new 方法的第三个参数是 config 对象
    // let downloader = Downloader::new(
    //     vec!["https://dldir1.qq.com/qqfile/qq/PCQQ9.7.17/QQ9.7.17.29225.exe"],
    //     None::<Vec<&str>>,
    //     "QQ9.7.17.29225.exe",
    //     config,
    //     || ClientBuilder::new(),
    // );

    let downloader = Downloader::new(
        vec![
            "https://github.com/ModOrganizer2/modorganizer/releases/download/v2.5.2/Mod.Organizer-2.5.2.7z",
            "https://gh-proxy.com/https://github.com/ModOrganizer2/modorganizer/releases/download/v2.5.2/Mod.Organizer-2.5.2.7z",
            "https://hk.gh-proxy.com/https://github.com/ModOrganizer2/modorganizer/releases/download/v2.5.2/Mod.Organizer-2.5.2.7z",
            "https://edgeone.gh-proxy.com/https://github.com/ModOrganizer2/modorganizer/releases/download/v2.5.2/Mod.Organizer-2.5.2.7z",
        ],
        vec!["http://127.0.0.1:7897"],
        true,
        "Mod.Organizer-2.5.2.7z",
        config,
        || ClientBuilder::new(),
    );

    // --- 3. 定义进度处理逻辑 ---
    // 定义一个处理下载进度的异步闭包
    let progress_handler = |total_size: u64, mut info_rx: broadcast::Receiver<DownloadInfo>| async move {
        println!("文件总大小: {:.2} MB", total_size as f64 / 1024.0 / 1024.0);

        // 循环接收并打印进度信息
        while let Ok(info) = info_rx.recv().await {
            if let DownloadInfo::MonitorUpdate {
                total_downloaded,
                total_speed,
                ..
            } = info
            {
                let progress = (total_downloaded as f64 / total_size as f64) * 100.0;
                println!(
                    "进度: {:.2}% | 已下载: {:.2} MB / {:.2} MB | 速度: {:.2} MB/s",
                    progress,
                    total_downloaded as f64 / 1024.0 / 1024.0,
                    total_size as f64 / 1024.0 / 1024.0,
                    total_speed / 1024.0 / 1024.0
                );
            }
        }
    };

    // --- 4. 启动下载 ---
    match downloader.run(progress_handler).await {
        Ok(_) => println!("下载成功！"),
        Err(e) => eprintln!("下载失败: {}", e),
    }
}
