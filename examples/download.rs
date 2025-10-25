use simple_downloader::{DownloadInfo, Downloader, reqwest::ClientBuilder};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();
    let downloader = Downloader::new(
        "https://dldir1.qq.com/qqfile/qq/PCQQ9.7.17/QQ9.7.17.29225.exe", // 下载链接
        "QQ9.7.17.29225.exe",                                            // 保存路径
        16,                                                              // 最大并发线程数
        1.0,                                                             // 进度更新间隔(秒)
        || ClientBuilder::new(),                                         // 提供网络客户端构建器
    );

    // 定义一个处理下载进度的闭包
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

    // 启动下载！
    match downloader.run(progress_handler).await {
        Ok(_) => println!("下载成功！"),
        Err(e) => eprintln!("下载失败: {}", e),
    }
}
