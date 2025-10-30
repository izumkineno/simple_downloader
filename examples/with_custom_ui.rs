// use simple_downloader::Downloader;
// use simple_downloader::reqwest::ClientBuilder;
// use simple_downloader::types_old::{ChunkStatus, DownloadInfo};
// use tokio::sync::broadcast;

// async fn progress_bar_task(file_size: u64, mut info_rx: broadcast::Receiver<DownloadInfo>) {
//     use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
//     use std::collections::HashMap;

//     let multi_progress = MultiProgress::new();
//     let total_pb = multi_progress.add(ProgressBar::new(file_size));
//     total_pb.set_style(
//         ProgressStyle::with_template(
//             " {spinner:.green} [{msg}] [{wide_bar:.yellow/blue}] {bytes}/{total_bytes} ({eta})",
//         )
//         .unwrap()
//         .progress_chars("=> "),
//     );
//     total_pb.set_message("总进度 (0.00 MB/s)");

//     let chunk_style =
//         ProgressStyle::with_template("  [{msg}] [{bar:40.cyan/blue}] {bytes}/{total_bytes}")
//             .unwrap()
//             .progress_chars("##-");

//     let mut chunk_pbs: HashMap<u64, ProgressBar> = HashMap::new();

//     // 修改：get_status_info 现在接收强类型的 ChunkStatus
//     fn get_status_info(status: ChunkStatus) -> (&'static str, String) {
//         match status {
//             ChunkStatus::Downloading => ("cyan", "下载中".to_string()),
//             ChunkStatus::Retrying { attempt } => ("yellow", format!("重试中 ({})", attempt)),
//             ChunkStatus::Delayed => ("magenta", "延迟重试".to_string()),
//             ChunkStatus::Completed => ("green", "已完成".to_string()),
//             ChunkStatus::PermanentlyFailed => ("red", "失败".to_string()),
//         }
//     }

//     while let Ok(info) = info_rx.recv().await {
//         match info {
//             DownloadInfo::MonitorUpdate {
//                 total_downloaded,
//                 total_speed,
//                 chunk_details,
//                 ..
//             } => {
//                 total_pb.set_position(total_downloaded);
//                 total_pb.set_message(format!(
//                     "总进度 ({:.2} MB/s)",
//                     total_speed / 1024.0 / 1024.0
//                 ));

//                 // 修改：元组中的 status 现在是 ChunkStatus 类型
//                 for (id, size, downloaded, speed, status) in chunk_details {
//                     let pb = chunk_pbs.entry(id).or_insert_with(|| {
//                         let pb = multi_progress.add(ProgressBar::new(size));
//                         pb.set_style(chunk_style.clone());
//                         pb
//                     });

//                     let (color, status_text) = get_status_info(status);

//                     pb.set_length(size);
//                     pb.set_position(downloaded);
//                     pb.set_message(format!(
//                         "任务 {id} ({:.2} MB/s) [{status_text}]",
//                         speed / 1024.0 / 1024.0,
//                     ));

//                     pb.set_style(
//                         ProgressStyle::with_template(&format!(
//                             "  [{{msg}}] [{{bar:40.{}/blue}}] {{bytes}}/{{total_bytes}}",
//                             color
//                         ))
//                         .unwrap()
//                         .progress_chars("##-"),
//                     );
//                 }
//             }
//             // 修改：枚举成员从 DownloadComplete 变为 ChunkCompleted
//             DownloadInfo::ChunkCompleted(id) => {
//                 if let Some(pb) = chunk_pbs.get(&id) {
//                     pb.finish_with_message(format!("任务 {id} 完成"));
//                 }
//             }
//             DownloadInfo::ChunkStatusChanged {
//                 id,
//                 status,
//                 message,
//             } => {
//                 if let Some(pb) = chunk_pbs.get(&id) {
//                     let (color, mut status_text) = get_status_info(status);

//                     // 如果有额外消息，附加到状态文本后面
//                     if let Some(msg) = &message {
//                         status_text = format!("{} - {}", status_text, msg);
//                     }

//                     // 更新进度条消息和样式
//                     let current_msg = pb.message();
//                     if let Some(speed_part) = current_msg.split(']').next() {
//                         pb.set_message(format!("{}] [{}]", speed_part, status_text));
//                     }

//                     pb.set_style(
//                         ProgressStyle::with_template(&format!(
//                             "  [{{msg}}] [{{bar:40.{}/blue}}] {{bytes}}/{{total_bytes}}",
//                             color
//                         ))
//                         .unwrap()
//                         .progress_chars("##-"),
//                     );

//                     // 修改：检查失败状态
//                     if status == ChunkStatus::PermanentlyFailed {
//                         if let Some(error_msg) = message {
//                             pb.println(format!("  ❌ 任务 {id} 失败: {}", error_msg));
//                         }
//                     }
//                 }
//             }
//             _ => {}
//         }
//     }
//     total_pb.finish_with_message("下载完成");
// }

#[tokio::main]
async fn main() {
    // let url = "https://dlied4.myapp.com/myapp/1104466820/cos.release-40109/10040714_com.tencent.tmgp.sgame_a2480356_8.2.1.9_F0BvnI.apk";
    // let output_path = "king_of_glory.apk";
    // let workers = 16;
    // let update_interval = 0.2;

    // let downloader = Downloader::new(url, output_path, workers, update_interval, || {
    //     ClientBuilder::new()
    // });

    // println!("开始下载...");
    // let start_time = std::time::Instant::now();

    // let result = downloader.run(progress_bar_task).await;
    // let elapsed = start_time.elapsed();

    // match result {
    //     Ok(_) => {
    //         tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    //         println!("✅ 所有任务成功完成! 耗时: {:.2}秒", elapsed.as_secs_f64());
    //     }
    //     Err(e) => eprintln!("❌ 下载过程中发生错误: {e}"),
    // }
}
