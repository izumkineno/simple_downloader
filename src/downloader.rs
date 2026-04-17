//! 包含核心的 `Downloader` 结构体，是整个库的入口和总协调器。

use crate::chunk::chunk_run;
use crate::lane::MultiRuntime;
use crate::lane::MultiSourceConfig;
use crate::monitor::DownloadMonitor;
use crate::resume::ResumePlan;
use crate::types::{DownloadCmd, DownloadInfo, Result};
use crate::util::{file_writer_task_with_resume, get_file_info};
use faststr::FastStr;
use futures_util::stream::FuturesUnordered;
use reqwest::{Client, ClientBuilder};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};

const MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1 MB
const CHANNEL_CAPACITY: usize = 1024;

/// 下载器的配置信息。
#[derive(Clone)]
struct DownloaderConfig {
    /// 下载目标的 URL。
    url: FastStr,
    /// 文件保存路径。
    output_path: FastStr,
    /// 最大并发工作线程数。
    workers: u64,
}

#[derive(Clone)]
enum DownloadMode {
    Single(DownloaderConfig),
    Multi(MultiSourceConfig),
}

/// 主要的下载管理器。
///
/// 泛型 `F` 允许用户传入一个闭包，用于创建 `reqwest::ClientBuilder`，
/// 从而可以自定义客户端配置（如代理、超时等）。
pub struct Downloader<F>
where
    F: Fn() -> ClientBuilder,
{
    /// 下载配置。
    mode: DownloadMode,
    /// 用于创建 reqwest 客户端的构建器闭包。
    client_builder: F,
    /// 用于广播控制命令（如 `BisectDownload`, `TerminateAll`）的发送端。
    cmd_tx: broadcast::Sender<DownloadCmd>,
    /// 用于广播下载信息（如进度、状态）的发送端。
    info_tx: broadcast::Sender<DownloadInfo>,
    /// 进度更新的间隔时间（秒）。
    update_interval: f64,
    /// 是否启用自动断点续传。
    resume_enabled: bool,
}

impl<F> Downloader<F>
where
    // `F` 必须是 `Send + Sync + 'static` 的，因为它可能被移动到其他线程。
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    /// 创建一个新的 `Downloader` 实例。
    ///
    /// # 参数
    /// - `url`: 下载文件的 URL。
    /// - `output_path`: 文件保存的路径。
    /// - `workers`: 最大并发下载线程数。
    /// - `update_interval`: 进度信息更新的频率（秒）。
    /// - `client_builder`: 一个返回 `reqwest::ClientBuilder` 的闭包。
    pub fn new(
        url: impl Into<FastStr>,
        output_path: impl Into<FastStr>,
        workers: u64,
        update_interval: f64,
        client_builder: F,
    ) -> Self {
        let (cmd_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (info_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            mode: DownloadMode::Single(DownloaderConfig {
                url: url.into(),
                output_path: output_path.into(),
                workers,
            }),
            client_builder,
            cmd_tx,
            info_tx,
            update_interval,
            resume_enabled: true,
        }
    }

    pub fn new_multi(config: MultiSourceConfig, client_builder: F) -> Self {
        let (cmd_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (info_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            update_interval: config.update_interval,
            mode: DownloadMode::Multi(config),
            client_builder,
            cmd_tx,
            info_tx,
            resume_enabled: true,
        }
    }

    /// 显式启用或关闭自动断点续传。默认启用。
    pub fn with_resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    /// 启动下载过程。
    ///
    /// # 参数
    /// - `progress_handler`: 一个异步闭包，接收总文件大小和 `DownloadInfo` 的接收端，
    ///   用于处理和显示下载进度。
    pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
    where
        ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (file_size, support_ranges, writer_path, client, download_url, workers, multi_runtime) =
            match &self.mode {
                DownloadMode::Single(config) => {
                    let client = (self.client_builder)().build()?;
                    let (file_size, support_ranges) = get_file_info(&client, &config.url).await?;
                    (
                        file_size,
                        support_ranges,
                        config.output_path.clone(),
                        client,
                        Some(config.url.clone()),
                        config.workers,
                        None,
                    )
                }
                DownloadMode::Multi(config) => {
                    let (file_size, runtime) =
                        MultiRuntime::from_config(config, &self.client_builder).await?;
                    let (client, download_url) = runtime
                        .best_lane_runtime()
                        .map(|lane| (lane.client.clone(), lane.url.clone()))
                        .expect("validated multi-source runtime must contain a lane");
                    (
                        file_size,
                        true,
                        config.output_path.clone(),
                        client,
                        Some(download_url),
                        config.workers,
                        Some(runtime),
                    )
                }
            };
        // 为进度处理器订阅信息通道
        let info_rx_for_progress = self.info_tx.subscribe();
        let writer_path_string = writer_path.to_string();
        let resume_plan = ResumePlan::prepare(
            Path::new(&writer_path_string),
            file_size,
            self.resume_enabled,
        )?;
        if resume_plan.completed_bytes > 0 && !support_ranges {
            return Err(crate::types::DownloadError::ResumeMetadata(
                "partial resume requires HTTP Range support".to_owned(),
            ));
        }
        let truncate_output = resume_plan.truncate_output;
        let initial_ranges = resume_plan.remaining_ranges.clone();
        let completed_bytes = resume_plan.completed_bytes;
        let resume_recorder = resume_plan.into_recorder();
        // 启动文件写入任务，并获取其命令发送端
        let (writer_tx, writer_handle) =
            file_writer_task_with_resume(writer_path, file_size, truncate_output, resume_recorder)
                .await?;
        let writer_shutdown_tx = writer_tx.clone();

        // 异步执行用户提供的进度处理逻辑
        spawn(progress_handler(file_size, info_rx_for_progress));

        // 协调和管理所有下载任务
        self.orchestrate_downloads(
            file_size,
            support_ranges,
            writer_tx,
            client,
            download_url.as_ref(),
            workers,
            multi_runtime,
            initial_ranges,
            completed_bytes,
        )
        .await?;

        let _ = writer_shutdown_tx.send(DownloadCmd::TerminateAll).await;
        let _ = writer_handle.await;
        // 下载结束后，发送终止命令以清理所有任务
        let _ = self.cmd_tx.send(DownloadCmd::TerminateAll);
        Ok(())
    }

    /// 内部函数，用于创建和管理所有下载任务。
    async fn orchestrate_downloads(
        &self,
        file_size: u64,
        support_ranges: bool,
        writer_tx: mpsc::Sender<DownloadCmd>,
        client: Client,
        url: Option<&FastStr>,
        workers: u64,
        mut multi_runtime: Option<MultiRuntime>,
        resume_ranges: Vec<(u64, u64)>,
        completed_bytes: u64,
    ) -> Result<()> {
        // 使用 FuturesUnordered 来管理所有并发的下载任务
        let tasks = FuturesUnordered::new();
        // 用于生成唯一的块 ID
        let next_chunk_id = AtomicU64::new(0);

        // 决定实际的并发数
        let workers = if !support_ranges || workers == 1 || file_size < MIN_CHUNK_SIZE {
            // 如果服务器不支持范围请求，或用户只设置了1个worker，或文件太小，则强制使用单线程
            1
        } else {
            workers
        };

        let mut initial_lanes = Vec::new();
        let initial_ranges = split_resume_ranges(resume_ranges, workers, multi_runtime.is_some());
        for (start_byte, end_byte) in initial_ranges {
            let id = next_chunk_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (lane_id, rb) = if let Some(runtime) = multi_runtime.as_mut() {
                runtime
                    .claim_request_builder()
                    .map(|(lane_id, rb)| (Some(lane_id), rb))
                    .expect("validated runtime must provide an initial lane")
            } else {
                let url = url.expect("single-source mode requires a URL");
                (None, client.get(url.as_str()))
            };
            if let Some(lane_id) = lane_id {
                initial_lanes.push((id, lane_id));
            }
            let task = chunk_run(
                id,
                writer_tx.clone(),
                self.cmd_tx.subscribe(),
                self.info_tx.clone(),
                rb,
                start_byte,
                end_byte,
            );
            tasks.push(spawn(task));
        }
        println!(
            "[Main] 启动初始下载任务数量: {}, 最大并发数设置为: {}",
            tasks.len(),
            workers
        );

        // 创建下载监控器
        let monitor = DownloadMonitor::new_with_completed(
            file_size,
            completed_bytes,
            self.update_interval,
            workers,
        );

        // 运行监控器，它将接管下载过程的管理
        monitor
            .run(
                self.info_tx.subscribe(),
                self.info_tx.clone(),
                tasks,
                &next_chunk_id,
                &client,
                writer_tx,
                &self.cmd_tx,
                url,
                initial_lanes,
                multi_runtime,
            )
            .await;
        Ok(())
    }
}

fn split_resume_ranges(
    ranges: Vec<(u64, u64)>,
    workers: u64,
    split_for_multi_source: bool,
) -> Vec<(u64, u64)> {
    if !split_for_multi_source {
        return ranges;
    }
    let target = workers.max(1) as usize;
    let mut ranges = ranges;

    while ranges.len() < target {
        let Some((index, (start, end))) = ranges
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|(_, (start, end))| end.saturating_sub(*start).saturating_add(1))
        else {
            break;
        };

        let len = end.saturating_sub(start).saturating_add(1);
        if len <= 1 {
            break;
        }

        let left_len = len / 2;
        let mid = start + left_len - 1;
        ranges.splice(index..=index, [(start, mid), (mid + 1, end)]);
    }

    ranges
}

#[cfg(test)]
mod tests {
    use super::split_resume_ranges;

    #[test]
    fn split_resume_ranges_caps_initial_ranges_to_worker_count() {
        let ranges = vec![(0, 9), (20, 29)];

        let split = split_resume_ranges(ranges, 2, true);

        assert_eq!(split.len(), 2);
        assert_eq!(split, vec![(0, 9), (20, 29)]);
    }
}
