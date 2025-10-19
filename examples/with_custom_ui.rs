use simple_downloader::reqwest::ClientBuilder;
use simple_downloader::{DownloadInfo, Downloader};
use tokio::sync::broadcast;

async fn progress_bar_task(file_size: u64, mut info_rx: broadcast::Receiver<DownloadInfo>) {
    use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
    use std::collections::HashMap;

    // 创建一个多进度条容器
    let multi_progress = MultiProgress::new();
    // 创建总体进度条
    let total_pb = multi_progress.add(ProgressBar::new(file_size));
    total_pb.set_style(
        ProgressStyle::with_template(
            // {msg} 会显示 set_message 的内容, e.g., "总进度 (10.5 MB/s)"
            " {spinner:.green} [{msg}] [{wide_bar:.yellow/blue}] {bytes}/{total_bytes} ({eta})",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    total_pb.set_message("总进度 (0.00 MB/s)");

    // 为每个下载块（线程）定义进度条样式
    let chunk_style =
        ProgressStyle::with_template("  [{msg}] [{bar:40.cyan/blue}] {bytes}/{total_bytes}")
            .unwrap()
            .progress_chars("##-");

    // 使用 HashMap 存储每个下载块的进度条和状态信息
    let mut chunk_pbs: HashMap<u64, ProgressBar> = HashMap::new();
    let mut chunk_statuses: HashMap<u64, (u8, Option<String>)> = HashMap::new();

    // 获取状态对应的颜色和描述
    fn get_status_info(status: u8) -> (&'static str, &'static str) {
        match status {
            0 => ("cyan", "下载中"),
            1 => ("yellow", "重试中"),
            2 => ("blue", "等待重试"),
            3 => ("magenta", "延迟重试"),
            4 => ("green", "已完成"),
            5 => ("red", "失败"),
            _ => ("white", "未知状态"),
        }
    }

    // 循环接收来自下载器的状态信息
    while let Ok(info) = info_rx.recv().await {
        match info {
            // 处理合并后的 MonitorUpdate 事件
            DownloadInfo::MonitorUpdate {
                total_size,
                total_downloaded,
                total_speed,
                chunk_details,
            } => {
                // 1. 更新总体进度条
                total_pb.set_length(total_size);
                total_pb.set_position(total_downloaded);
                total_pb.set_message(format!(
                    "总进度 ({:.2} MB/s)",
                    total_speed / 1024.0 / 1024.0
                ));

                // 2. 遍历并更新所有下载块的进度条
                for (id, size, downloaded, speed, status) in chunk_details {
                    // 更新状态信息
                    chunk_statuses.insert(id, (status, None));

                    // 如果是新的块，则创建并添加到 HashMap 和 MultiProgress 中
                    let pb = chunk_pbs.entry(id).or_insert_with(|| {
                        let pb = multi_progress.add(ProgressBar::new(size));
                        pb.set_style(chunk_style.clone());
                        pb
                    });

                    // 获取当前状态信息
                    let status_info = chunk_statuses.get(&id).unwrap_or(&(0, None));
                    let (color, status_text) = get_status_info(status_info.0);
                    let status_msg = if let Some(msg) = &status_info.1 {
                        format!("{} - {}", status_text, msg)
                    } else {
                        status_text.to_string()
                    };

                    // 更新进度条的长度、位置和消息
                    pb.set_length(size);
                    pb.set_position(downloaded);
                    pb.set_message(format!(
                        "任务 {id} ({:.2} MB/s) [{}]",
                        speed / 1024.0 / 1024.0,
                        status_msg
                    ));

                    // 根据状态设置不同的样式
                    pb.set_style(
                        ProgressStyle::with_template(&format!(
                            "  [{{msg}}] [{{bar:40.{}/blue}}] {{bytes}}/{{total_bytes}}",
                            color
                        ))
                        .unwrap()
                        .progress_chars("##-"),
                    );
                }
            }
            // 当一个下载块完成时
            DownloadInfo::DownloadComplete(id) => {
                if let Some(pb) = chunk_pbs.get(&id) {
                    // 将其标记为完成
                    pb.finish_with_message(format!("任务 {id} 完成"));
                }
                // 更新状态
                chunk_statuses.insert(id, (4, None));
            }
            // 处理状态变化事件
            DownloadInfo::ChunkStatusChanged {
                id,
                status,
                message,
            } => {
                // 更新状态信息
                chunk_statuses.insert(id, (status, message.clone()));

                if let Some(pb) = chunk_pbs.get(&id) {
                    let (color, status_text) = get_status_info(status);
                    let status_msg = if let Some(msg) = &message {
                        format!("{} - {}", status_text, msg)
                    } else {
                        status_text.to_string()
                    };

                    // 更新进度条消息和样式
                    let current_msg = pb.message();
                    if let Some(speed_part) = current_msg.split(']').next() {
                        pb.set_message(format!("{}] [{}]", speed_part, status_msg));
                    }

                    pb.set_style(
                        ProgressStyle::with_template(&format!(
                            "  [{{msg}}] [{{bar:40.{}/blue}}] {{bytes}}/{{total_bytes}}",
                            color
                        ))
                        .unwrap()
                        .progress_chars("##-"),
                    );

                    // 如果是失败状态，显示错误信息
                    if status == 5 {
                        if let Some(error_msg) = message {
                            pb.println(format!("  ❌ 任务 {id} 失败: {}", error_msg));
                        }
                    }
                }
            }
            _ => {
                // 忽略其他事件
            }
        }
    }
    // 所有任务完成后，将总体进度条标记为完成
    total_pb.finish_with_message("下载完成");
}

#[tokio::main]
async fn main() {
    // 1. --- 配置 ---

    let url = "https://dlied4.myapp.com/myapp/1104466820/cos.release-40109/10040714_com.tencent.tmgp.sgame_a2480356_8.2.1.9_F0BvnI.apk";
    let output_path = "10040714_com.tencent.tmgp.sgame_a2480356_8.2.1.9_F0BvnI.apk";

    let workers = 16; // 最大并发数
    let update_interval = 0.2; // 更新间隔为 0.2 秒

    // 2. --- 执行 ---
    // 创建下载器实例
    let downloader = Downloader::new(url, output_path, workers, update_interval, || {
        ClientBuilder::new()
    });

    println!("开始下载...");
    let start_time = std::time::Instant::now();

    // 运行下载器，并将UI处理逻辑 (progress_bar_task) 作为回调函数传入
    let result = downloader.run(progress_bar_task).await;
    let elapsed = start_time.elapsed();

    // 3. --- 输出 ---
    // 处理最终的下载结果
    match result {
        Ok(_) => {
            // 等待一小段时间，确保所有进度条都渲染完毕
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            println!("✅ 所有任务成功完成! 耗时: {:.2}秒", elapsed.as_secs_f64());
        }
        Err(e) => eprintln!("❌ 下载过程中发生错误: {e}"),
    }
}
