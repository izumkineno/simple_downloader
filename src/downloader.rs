//! 包含核心的 `Downloader` 结构体，是整个库的入口和总协调器。

use crate::chunk::chunk_run;
use crate::lane::{MultiRuntime, MultiSourceConfig};
use crate::monitor::DownloadMonitor;
#[cfg(feature = "resume")]
use crate::resume::ResumePlan;
use crate::types::DownloadError;
use crate::types::{DownloadCmd, DownloadInfo, Result};
#[cfg(not(feature = "resume"))]
use crate::util::file_writer_task;
#[cfg(feature = "resume")]
use crate::util::file_writer_task_with_resume;
use crate::util::get_file_info;
use faststr::FastStr;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use reqwest::{Client, ClientBuilder};
#[cfg(feature = "resume")]
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};
use tokio::time::interval;

const MIN_PARALLEL_FILE_SIZE: u64 = 1024 * 1024; // 1 MiB：小于此值自动单线程，避免切片开销
const CHANNEL_CAPACITY: usize = 4096; // 增大以容纳节流后 ~1.5k 进度 + 控制事件，避免 Lagged 丢 Failed/Complete
const DEFAULT_UPDATE_INTERVAL: f64 = 0.5;

type BoxProgressFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type ProgressHandler =
    Box<dyn FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> BoxProgressFuture + Send>;

fn default_client_builder() -> ClientBuilder {
    ClientBuilder::new()
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_keepalive(std::time::Duration::from_secs(60))
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

/// 下载器构建器，使用流畅的 Builder 模式配置下载参数。
///
/// # 示例
///
/// ```no_run
/// use simple_downloader::Downloader;
///
/// # #[tokio::main]
/// # async fn main() {
/// let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
///     .workers(8) // 设置 8 个并发线程
///     .update_interval(1.0) // 每秒更新一次进度
///     .resume(true) // 启用断点续传
///     .build();
/// # }
/// ```
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
    /// 创建一个新的下载构建器。
    ///
    /// # 参数
    ///
    /// - `url`: 要下载的文件的 URL
    /// - `output_path`: 下载后文件的保存路径
    ///
    /// # 默认配置
    ///
    /// - `workers`: 自动检测 CPU 核心数，默认值为核心数，最少为 1
    /// - `update_interval`: 0.5 秒（进度更新间隔）
    /// - `resume_enabled`: 根据 `resume` feature 是否启用自动决定
    /// - `client_builder`: 使用默认的 reqwest 客户端配置
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
    /// 设置并发下载的工作线程数。
    ///
    /// # 参数
    ///
    /// - `workers`: 工作线程数，最小值为 1
    ///
    /// # 注意
    ///
    /// 如果服务器不支持 Range 请求，或者文件大小小于 1MB，会自动降级为单线程下载。
    pub fn workers(mut self, workers: u64) -> Self {
        self.workers = workers.max(1);
        self
    }

    /// 设置进度更新的时间间隔（秒）。
    ///
    /// # 参数
    ///
    /// - `update_interval`: 进度更新间隔，必须大于 0
    ///
    /// 默认值为 0.5 秒。
    pub fn update_interval(mut self, update_interval: f64) -> Self {
        if update_interval > 0.0 {
            self.update_interval = update_interval;
        }
        self
    }

    /// 设置自定义的 reqwest 客户端构建器。
    ///
    /// 使用此方法可以自定义 HTTP 客户端的配置，例如超时、代理、证书等。
    ///
    /// # 示例
    ///
    /// ```no_run
    /// use simple_downloader::Downloader;
    /// use reqwest::ClientBuilder;
    /// use std::time::Duration;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
    ///     .client_builder(|| {
    ///         ClientBuilder::new()
    ///             .timeout(Duration::from_secs(30))
    ///             .connect_timeout(Duration::from_secs(10))
    ///     })
    ///     .build();
    /// # }
    /// ```
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

    /// 启用或禁用断点续传功能。
    ///
    /// 仅在 `resume` feature 启用时可用。
    ///
    /// # 参数
    ///
    /// - `enabled`: 是否启用断点续传
    ///
    /// 启用后，下载中断后再次启动时会自动从断点处恢复，无需重新下载已完成的部分。
    #[cfg(feature = "resume")]
    pub fn resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    /// 构建 Downloader 实例。
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

    /// 直接启动下载（便捷方法）。
    ///
    /// 相当于先调用 `build()` 再调用 `download().await`。
    pub async fn download(self) -> Result<()> {
        self.build().download().await
    }

    /// 启动下载并提供进度监控（便捷方法）。
    ///
    /// 相当于先调用 `build()` 再调用 `run(progress_handler).await`。
    /// 仅在 `progress` feature 启用时可用。
    ///
    /// # 参数
    ///
    /// - `progress_handler`: 进度处理闭包，接收文件总大小和进度信息接收器
    #[cfg(feature = "progress")]
    pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
    where
        ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.build().run(progress_handler).await
    }
}

/// 主要的下载管理器，是整个库的核心协调者。
///
/// Downloader 负责协调整个下载过程，包括：
/// - 初始化下载配置和组件
/// - 获取文件信息和检测服务器 Range 支持
/// - 处理断点续传逻辑
/// - 分配下载任务给工作线程
/// - 监控下载进度和处理错误
/// - 合并下载的文件块并完成下载
///
/// 泛型 `F` 允许用户传入一个闭包，用于创建 `reqwest::ClientBuilder`，
/// 从而可以自定义客户端配置（如代理、超时、证书等）。
pub struct Downloader<F>
where
    F: Fn() -> ClientBuilder,
{
    /// 下载模式（单源或多源）。
    mode: DownloadMode,
    /// 用于创建 reqwest 客户端的构建器闭包。
    client_builder: F,
    /// 用于广播控制命令（如暂停、终止）的发送端。
    cmd_tx: broadcast::Sender<DownloadCmd>,
    /// 用于广播下载信息（如进度、速度）的发送端。
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
    /// 创建一个新的单源下载器实例。
    ///
    /// 推荐使用 `Downloader::builder()` 来创建下载器，它提供了更友好的配置接口。
    ///
    /// # 参数
    ///
    /// - `url`: 要下载的文件 URL
    /// - `output_path`: 文件保存路径
    /// - `workers`: 并发工作线程数（最小值为 1）
    /// - `update_interval`: 进度更新间隔（秒，必须大于 0）
    /// - `client_builder`: reqwest 客户端构建器闭包
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

    /// 创建一个新的多源下载器实例。
    ///
    /// 仅在 `multi-source` feature 启用时可用。
    ///
    /// # 参数
    ///
    /// - `config`: 多源下载配置
    /// - `client_builder`: reqwest 客户端构建器闭包
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

    /// 显式启用或关闭自动断点续传。
    ///
    /// 仅在 `resume` feature 开启时可用。
    ///
    /// # 参数
    ///
    /// - `enabled`: 是否启用断点续传
    #[cfg(feature = "resume")]
    pub fn with_resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    /// 启动下载，不返回进度信息。
    ///
    /// 这是最简单的下载方法，适合不需要监控进度的场景。
    ///
    /// # 返回值
    ///
    /// - `Ok(())`: 下载成功完成
    /// - `Err(DownloadError)`: 下载过程中发生错误
    pub async fn download(self) -> Result<()> {
        self.run_internal(None).await
    }

    /// 启动下载，并提供进度监控。
    ///
    /// 仅在 `progress` feature 启用时可用。
    ///
    /// # 参数
    ///
    /// - `progress_handler`: 进度处理闭包，接收两个参数：
    ///   - `total_size`: 文件总大小（字节）
    ///   - `info_rx`: 进度信息接收器，可以接收 `DownloadInfo` 结构体获取实时进度
    ///
    /// # 示例
    ///
    /// ```no_run
    /// # use simple_downloader::{Downloader, DownloadInfo};
    /// # #[tokio::main]
    /// # async fn main() {
    /// Downloader::builder("https://example.com/file.bin", "output.bin")
    ///     .workers(8)
    ///     .run(|total_size, mut info_rx| async move {
    ///         while let Ok(info) = info_rx.recv().await {
    ///             println!("进度: {:.1}%", info.progress_percent());
    ///         }
    ///     })
    ///     .await
    ///     .unwrap();
    /// # }
    /// ```
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

    #[::tracing::instrument(skip(self, progress_handler), fields(mode = ?match &self.mode { DownloadMode::Single(c) => format!("single:{}", c.url), DownloadMode::Multi(c) => format!("multi:{}", c.output_path) }))]
    async fn run_internal(self, progress_handler: Option<ProgressHandler>) -> Result<()> {
        ::tracing::info!("download run_internal start");
        let (file_size, support_ranges, writer_path, client, download_url, workers, multi_runtime) =
            match &self.mode {
                DownloadMode::Single(config) => {
                    ::tracing::debug!(url = %config.url, workers = config.workers, interval = self.update_interval, "probing single source");
                    let client = (self.client_builder)()
                        .pool_max_idle_per_host(32)
                        .pool_idle_timeout(Duration::from_secs(90))
                        .tcp_keepalive(Duration::from_secs(60))
                        .build()?;
                    let (file_size, support_ranges) =
                        match get_file_info(&client, &config.url).await {
                            Ok(v) => {
                                ::tracing::info!(size = v.0, support_ranges = v.1, url = %config.url, "probe ok");
                                v
                            },
                            Err(DownloadError::MissingContentLength) => {
                                ::tracing::warn!(url = %config.url, "missing Content-Length -> streaming fallback");
                                let writer_path = config.output_path.clone();
                                let download_url = config.url.clone();
                                return self
                                    .streaming_download(
                                        client,
                                        download_url,
                                        writer_path,
                                        progress_handler,
                                    )
                                    .await;
                            }
                            Err(e) => {
                                ::tracing::error!(error = %e, url = %config.url, "probe failed");
                                return Err(e)
                            },
                        };
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
                    ::tracing::debug!(output = %config.output_path, workers = config.workers, sources = config.sources.len(), "probing multi sources");
                    let runtime_res = MultiRuntime::from_config(config, &self.client_builder).await;
                    let (file_size, mut runtime) = match runtime_res {
                        Ok(v) => v,
                        Err(DownloadError::NoAvailableSources)
                        | Err(DownloadError::MissingContentLength) => {
                            // 多源探测失败，回退为单流流式下载（首源）
                            ::tracing::warn!("multi-source probe failed, fallback to streaming with first source");
                            if let Some(first) = config.sources.first() {
                                let client = (self.client_builder)()
                                    .pool_max_idle_per_host(32)
                                    .pool_idle_timeout(Duration::from_secs(90))
                                    .tcp_keepalive(Duration::from_secs(60))
                                    .build()?;
                                let writer_path = config.output_path.clone();
                                let download_url = first.url.clone();
                                return self
                                    .streaming_download(
                                        client,
                                        download_url,
                                        writer_path,
                                        progress_handler,
                                    )
                                    .await;
                            } else {
                                ::tracing::error!("multi-source no sources available");
                                return Err(DownloadError::NoAvailableSources);
                            }
                        }
                        Err(e) => {
                            ::tracing::error!(error = %e, "multi-source probe failed");
                            return Err(e);
                        },
                    };
                    let support_ranges = runtime.supports_ranges;
                    ::tracing::info!(file_size, support_ranges, "multi-source probe ok");
                    let (client, download_url) = runtime
                        .best_lane_runtime()
                        .map(|lane| (lane.client.clone(), lane.url.clone()))
                        .expect("validated multi-source runtime must contain a lane");
                    (
                        file_size,
                        support_ranges,
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
        let resume_plan = ResumePlan::prepare_async(
            Path::new(&writer_path_string).to_path_buf(),
            file_size,
            self.resume_enabled,
        )
        .await?;
        #[cfg(feature = "resume")]
        if resume_plan.completed_bytes > 0 && !support_ranges {
            ::tracing::error!(completed = resume_plan.completed_bytes, "partial resume requires HTTP Range support but server does not support it");
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

        ::tracing::info!(
            file_size,
            support_ranges,
            writer_path = %writer_path,
            completed_bytes,
            remaining_ranges = initial_ranges.len(),
            "resume plan ready, initializing writer"
        );

        #[cfg(feature = "resume")]
        let (writer_tx, writer_handle) = file_writer_task_with_resume(
            writer_path.clone(),
            file_size,
            truncate_output,
            resume_plan.into_recorder(),
        )
        .await?;
        #[cfg(not(feature = "resume"))]
        let (writer_tx, writer_handle) = file_writer_task(writer_path.clone(), file_size).await?;
        let writer_shutdown_tx = writer_tx.clone();

        ::tracing::debug!(writer_path = %writer_path, file_size, "writer task ready");

        if let Some(progress_handler) = progress_handler {
            spawn(progress_handler(file_size, self.info_tx.subscribe()));
            ::tracing::debug!("progress handler spawned");
        }

        let orchestrate_result = self
            .orchestrate_downloads(
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
            .await;
        let _ = writer_shutdown_tx.send(DownloadCmd::TerminateAll).await;
        let _ = writer_handle.await;
        let _ = self.cmd_tx.send(DownloadCmd::TerminateAll);
        if let Err(ref e) = orchestrate_result {
            ::tracing::error!(error = %e, "orchestrate_downloads failed");
        } else {
            ::tracing::info!(writer_path = %writer_path, file_size, "orchestrate_downloads done");
        }
        orchestrate_result?;
        #[cfg(feature = "resume")]
        {
            // 下载成功后清理 sidecar，避免下次启动做全量哈希校验
            let meta_path = crate::resume::metadata_path_for(Path::new(&writer_path_string));
            match tokio::fs::remove_file(&meta_path).await {
                Ok(_) => ::tracing::info!(path = %meta_path.display(), "resume sidecar cleaned after success"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                Err(e) => ::tracing::warn!(error = %e, path = %meta_path.display(), "failed to clean resume sidecar"),
            }
        }
        ::tracing::info!(writer_path = %writer_path, "download complete");
        Ok(())
    }
    #[::tracing::instrument(skip(self, client, writer_path, progress_handler), fields(url = %url, path = %writer_path))]
    async fn streaming_download(
        self,
        client: Client,
        url: FastStr,
        writer_path: FastStr,
        progress_handler: Option<ProgressHandler>,
    ) -> Result<()> {
        ::tracing::info!(url = %url, path = %writer_path, "streaming_download start (unknown Content-Length, single stream)");
        // 未知 Content-Length 时的流式回退：单流顺序写入，不预分配，不支持 Range/多源
        let (writer_tx, writer_handle) = {
            #[cfg(feature = "resume")]
            {
                crate::util::file_writer_task(writer_path.clone(), 0).await?
            }
            #[cfg(not(feature = "resume"))]
            {
                crate::util::file_writer_task(writer_path.clone(), 0).await?
            }
        };
        let writer_shutdown_tx = writer_tx.clone();
        if let Some(handler) = progress_handler {
            spawn(handler(0, self.info_tx.subscribe()));
            ::tracing::debug!("streaming progress handler spawned");
        }
        let resp = client.get(url.as_str()).send().await?.error_for_status()?;
        ::tracing::debug!(status = %resp.status(), url = %url, "streaming GET response");
        let mut stream = resp.bytes_stream();
        let mut offset = 0u64;
        let mut total_downloaded = 0u64;
        let mut ticker = interval(Duration::from_secs_f64(self.update_interval));
        let mut last_tick = Instant::now();
        let mut last_downloaded = 0u64;
        loop {
            tokio::select! {
                biased;
                chunk = stream.next() => match chunk {
                    Some(Ok(bytes)) => {
                        let len = bytes.len() as u64;
                        if len == 0 {
                            continue;
                        }
                        writer_tx
                            .send(DownloadCmd::WriteFile { offset, data: bytes })
                            .await
                            .map_err(|_| {
                                ::tracing::error!("streaming writer channel closed");
                                DownloadError::Io(std::io::Error::new(
                                    std::io::ErrorKind::BrokenPipe,
                                    "writer closed",
                                ))
                            })?;
                        offset += len;
                        total_downloaded += len;
                        ::tracing::trace!(offset, len, total_downloaded, "streaming chunk written");
                        let _ = self.info_tx.send(DownloadInfo::ChunkProgress {
                            id: 0,
                            start_byte: 0,
                            end_byte: 0,
                            downloaded: total_downloaded,
                        });
                    }
                    Some(Err(e)) => {
                        ::tracing::error!(error = %e, url = %url, "streaming request error");
                        return Err(DownloadError::Request(e));
                    },
                    None => {
                        ::tracing::info!(total_downloaded, "streaming completed (EOF)");
                        break;
                    },
                },
                _ = ticker.tick() => {
                    let elapsed = last_tick.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 {
                        (total_downloaded - last_downloaded) as f64 / elapsed
                    } else {
                        0.0
                    };
                    last_tick = Instant::now();
                    last_downloaded = total_downloaded;
                    ::tracing::trace!(total_downloaded, speed_kbs = speed/1024.0, "streaming tick");
                    let _ = self.info_tx.send(DownloadInfo::MonitorUpdate {
                        total_size: 0,
                        total_downloaded,
                        total_speed: speed,
                        chunk_details: vec![(0, 0, total_downloaded, speed, 0)],
                    });
                }
            }
        }
        let _ = writer_shutdown_tx.send(DownloadCmd::TerminateAll).await;
        let _ = writer_handle.await;
        let _ = self.cmd_tx.send(DownloadCmd::TerminateAll);
        let _ = self.info_tx.send(DownloadInfo::MonitorUpdate {
            total_size: total_downloaded,
            total_downloaded,
            total_speed: 0.0,
            chunk_details: vec![],
        });
        let _ = self.info_tx.send(DownloadInfo::DownloadComplete(0));
        ::tracing::info!(total_downloaded, path = %writer_path, "streaming_download complete");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[::tracing::instrument(skip(self, writer_tx, client, multi_runtime), fields(file_size = file_size, support_ranges = support_ranges, workers = workers, completed_bytes = completed_bytes))]
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

        let workers = if !support_ranges || workers == 1 || file_size < MIN_PARALLEL_FILE_SIZE {
            ::tracing::info!(support_ranges, file_size, workers, "downgrade to single worker");
            1
        } else {
            workers
        };
        ::tracing::debug!(effective_workers = workers, file_size, support_ranges, mode = if multi_runtime.is_some() { "multi" } else { "single" }, "effective workers");

        let initial_ranges = split_resume_ranges(resume_ranges, workers, multi_runtime.is_some());
        ::tracing::debug!(initial_ranges = ?initial_ranges, "initial ranges after split");
        let mut pending_initial = Vec::new();
        let mut initial_lanes = Vec::new();
        // 预先创建 monitor 以便缓冲因 lane 容量不足而暂缓的初始区间
        let mut monitor = DownloadMonitor::new_with_completed(
            file_size,
            completed_bytes,
            self.update_interval,
            workers,
        );
        for (start_byte, end_byte) in initial_ranges {
            let (lane_id, rb) = if let Some(runtime) = multi_runtime.as_mut() {
                match runtime.claim_request_builder() {
                    Some((lane_id, rb)) => (Some(lane_id), rb),
                    None => {
                        ::tracing::warn!(
                            start_byte,
                            end_byte,
                            "lane capacity insufficient, buffering initial range"
                        );
                        pending_initial.push((start_byte, end_byte));
                        continue;
                    }
                }
            } else {
                (None, client.get(download_url.as_str()))
            };
            let id = next_chunk_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(lane_id) = lane_id {
                initial_lanes.push((id, lane_id));
            }
            ::tracing::debug!(chunk_id = id, start_byte, end_byte, "spawn initial chunk");
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
        ::tracing::debug!(
            initial_tasks = tasks.len(),
            pending_initial = ?pending_initial,
            next_id = next_chunk_id.load(std::sync::atomic::Ordering::SeqCst),
            "spawned initial tasks"
        );
        // 将缓冲的初始区间移入 monitor 的 pending 队列，待 tick 时按容量逐步调度
        monitor.pending_bisects.extend(pending_initial);

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
            .await?;
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
        // 避免产生过小碎片：小于 2×最小块阈值则不再分裂
        if len < crate::chunk::MIN_CHUNK_SIZE * 2 {
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
