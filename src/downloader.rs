//! 包含核心的 `Downloader` 结构体，是整个库的入口和总协调器。

use crate::chunk::chunk_run;
use crate::lane::{MultiRuntime, MultiSourceConfig};
use crate::monitor::DownloadMonitor;
#[cfg(feature = "resume")]
use crate::resume::ResumePlan;
#[cfg(feature = "resume")]
use crate::types::DownloadError;
use crate::types::{DownloadCmd, DownloadInfo, Result};
#[cfg(not(feature = "resume"))]
use crate::util::file_writer_task;
#[cfg(feature = "resume")]
use crate::util::file_writer_task_with_resume;
use crate::util::get_file_info;
use faststr::FastStr;
use futures_util::stream::FuturesUnordered;
use reqwest::{Client, ClientBuilder};
use std::future::Future;
#[cfg(feature = "resume")]
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};

const MIN_CHUNK_SIZE: u64 = 1024 * 1024; // 1 MB
const CHANNEL_CAPACITY: usize = 1024;
const DEFAULT_UPDATE_INTERVAL: f64 = 0.5;

type BoxProgressFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type ProgressHandler =
    Box<dyn FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> BoxProgressFuture + Send>;

fn default_client_builder() -> ClientBuilder {
    ClientBuilder::new()
}

fn default_workers() -> u64 {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get() as u64)
        .unwrap_or(4)
        .max(1)
}

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

#[cfg_attr(not(any(test, feature = "multi-source")), allow(dead_code))]
#[derive(Clone)]
enum DownloadMode {
    Single(DownloaderConfig),
    Multi(MultiSourceConfig),
}

/// 面向调用方的简化构建器。
pub struct DownloadBuilder<F = fn() -> ClientBuilder>
where
    F: Fn() -> ClientBuilder,
{
    url: FastStr,
    output_path: FastStr,
    workers: u64,
    update_interval: f64,
    client_builder: F,
    resume_enabled: bool,
}

impl DownloadBuilder {
    pub fn new(url: impl Into<FastStr>, output_path: impl Into<FastStr>) -> Self {
        Self {
            url: url.into(),
            output_path: output_path.into(),
            workers: default_workers(),
            update_interval: DEFAULT_UPDATE_INTERVAL,
            client_builder: default_client_builder,
            resume_enabled: cfg!(feature = "resume"),
        }
    }
}

impl<F> DownloadBuilder<F>
where
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    pub fn workers(mut self, workers: u64) -> Self {
        self.workers = workers.max(1);
        self
    }

    pub fn update_interval(mut self, update_interval: f64) -> Self {
        if update_interval > 0.0 {
            self.update_interval = update_interval;
        }
        self
    }

    pub fn client_builder<G>(self, client_builder: G) -> DownloadBuilder<G>
    where
        G: Fn() -> ClientBuilder + Send + Sync + 'static,
    {
        DownloadBuilder {
            url: self.url,
            output_path: self.output_path,
            workers: self.workers,
            update_interval: self.update_interval,
            client_builder,
            resume_enabled: self.resume_enabled,
        }
    }

    #[cfg(feature = "resume")]
    pub fn resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    pub fn build(self) -> Downloader<F> {
        let mut downloader = Downloader::new(
            self.url,
            self.output_path,
            self.workers,
            self.update_interval,
            self.client_builder,
        );
        downloader.resume_enabled = self.resume_enabled;
        downloader
    }

    pub async fn download(self) -> Result<()> {
        self.build().download().await
    }

    #[cfg(feature = "progress")]
    pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
    where
        ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.build().run(progress_handler).await
    }
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
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    /// 创建一个新的 `Downloader` 实例。
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
                workers: workers.max(1),
            }),
            client_builder,
            cmd_tx,
            info_tx,
            update_interval,
            resume_enabled: cfg!(feature = "resume"),
        }
    }

    #[cfg(feature = "multi-source")]
    pub fn new_multi(config: MultiSourceConfig, client_builder: F) -> Self {
        let (cmd_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (info_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            update_interval: config.update_interval,
            mode: DownloadMode::Multi(config),
            client_builder,
            cmd_tx,
            info_tx,
            resume_enabled: cfg!(feature = "resume"),
        }
    }

    /// 显式启用或关闭自动断点续传。仅在 `resume` feature 开启时可用。
    #[cfg(feature = "resume")]
    pub fn with_resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    /// 以最简单的默认路径启动下载，不暴露进度通道。
    pub async fn download(self) -> Result<()> {
        self.run_internal(None).await
    }

    /// 启动下载过程，并将进度流暴露给调用方。
    #[cfg(feature = "progress")]
    pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
    where
        ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let progress_handler: ProgressHandler =
            Box::new(move |total_size, info_rx| Box::pin(progress_handler(total_size, info_rx)));
        self.run_internal(Some(progress_handler)).await
    }

    async fn run_internal(self, progress_handler: Option<ProgressHandler>) -> Result<()> {
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
                        config.url.clone(),
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
                        download_url,
                        config.workers,
                        Some(runtime),
                    )
                }
            };

        #[cfg(feature = "resume")]
        let writer_path_string = writer_path.to_string();
        #[cfg(feature = "resume")]
        let resume_plan = ResumePlan::prepare(
            Path::new(&writer_path_string),
            file_size,
            self.resume_enabled,
        )?;
        #[cfg(feature = "resume")]
        if resume_plan.completed_bytes > 0 && !support_ranges {
            return Err(DownloadError::ResumeMetadata(
                "partial resume requires HTTP Range support".to_owned(),
            ));
        }
        #[cfg(feature = "resume")]
        let truncate_output = resume_plan.truncate_output;
        #[cfg(feature = "resume")]
        let initial_ranges = resume_plan.remaining_ranges.clone();
        #[cfg(not(feature = "resume"))]
        let initial_ranges = if file_size == 0 {
            Vec::new()
        } else {
            vec![(0, file_size - 1)]
        };
        #[cfg(feature = "resume")]
        let completed_bytes = resume_plan.completed_bytes;
        #[cfg(not(feature = "resume"))]
        let completed_bytes = 0;

        #[cfg(feature = "resume")]
        let (writer_tx, writer_handle) = file_writer_task_with_resume(
            writer_path,
            file_size,
            truncate_output,
            resume_plan.into_recorder(),
        )
        .await?;
        #[cfg(not(feature = "resume"))]
        let (writer_tx, writer_handle) = file_writer_task(writer_path, file_size).await?;
        let writer_shutdown_tx = writer_tx.clone();

        if let Some(progress_handler) = progress_handler {
            spawn(progress_handler(file_size, self.info_tx.subscribe()));
        }

        self.orchestrate_downloads(
            file_size,
            support_ranges,
            writer_tx,
            client,
            &download_url,
            workers,
            multi_runtime,
            initial_ranges,
            completed_bytes,
        )
        .await?;

        let _ = writer_shutdown_tx.send(DownloadCmd::TerminateAll).await;
        let _ = writer_handle.await;
        let _ = self.cmd_tx.send(DownloadCmd::TerminateAll);
        Ok(())
    }

    async fn orchestrate_downloads(
        &self,
        file_size: u64,
        support_ranges: bool,
        writer_tx: mpsc::Sender<DownloadCmd>,
        client: Client,
        download_url: &FastStr,
        workers: u64,
        mut multi_runtime: Option<MultiRuntime>,
        resume_ranges: Vec<(u64, u64)>,
        completed_bytes: u64,
    ) -> Result<()> {
        let tasks = FuturesUnordered::new();
        let next_chunk_id = AtomicU64::new(0);

        let workers = if !support_ranges || workers == 1 || file_size < MIN_CHUNK_SIZE {
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
                (None, client.get(download_url.as_str()))
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

        let monitor = DownloadMonitor::new_with_completed(
            file_size,
            completed_bytes,
            self.update_interval,
            workers,
        );
        monitor
            .run(
                self.info_tx.subscribe(),
                self.info_tx.clone(),
                tasks,
                &next_chunk_id,
                &client,
                writer_tx,
                &self.cmd_tx,
                Some(download_url),
                initial_lanes,
                multi_runtime,
            )
            .await;
        Ok(())
    }
}

impl Downloader<fn() -> ClientBuilder> {
    pub fn builder(
        url: impl Into<FastStr>,
        output_path: impl Into<FastStr>,
    ) -> DownloadBuilder<fn() -> ClientBuilder> {
        DownloadBuilder::new(url, output_path)
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
    use super::{DownloadBuilder, split_resume_ranges};

    #[test]
    fn split_resume_ranges_caps_initial_ranges_to_worker_count() {
        let ranges = vec![(0, 9), (20, 29)];

        let split = split_resume_ranges(ranges, 2, true);

        assert_eq!(split.len(), 2);
        assert_eq!(split, vec![(0, 9), (20, 29)]);
    }

    #[test]
    fn builder_defaults_are_sensible() {
        let builder = DownloadBuilder::new("https://example.com/file.bin", "file.bin");

        assert!(builder.workers >= 1);
        assert_eq!(builder.update_interval, super::DEFAULT_UPDATE_INTERVAL);
    }
}
