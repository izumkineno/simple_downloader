//! 包含核心的 `Downloader` 结构体，是整个库的入口和总协调器。

use crate::chunk::chunk_run;
use crate::monitor::DownloadMonitor;
use crate::types::{DownloadCmd, DownloadInfo, Result};
use crate::util::{file_writer_task, get_file_info};
use faststr::FastStr;
use futures_util::stream::FuturesUnordered;
use reqwest::{Client, ClientBuilder};
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

/// 主要的下载管理器。
///
/// 泛型 `F` 允许用户传入一个闭包，用于创建 `reqwest::ClientBuilder`，
/// 从而可以自定义客户端配置（如代理、超时等）。
pub struct Downloader<F>
where
    F: Fn() -> ClientBuilder,
{
    /// 下载配置。
    config: DownloaderConfig,
    /// 用于创建 reqwest 客户端的构建器闭包。
    client_builder: F,
    /// 用于广播控制命令（如 `BisectDownload`, `TerminateAll`）的发送端。
    cmd_tx: broadcast::Sender<DownloadCmd>,
    /// 用于广播下载信息（如进度、状态）的发送端。
    info_tx: broadcast::Sender<DownloadInfo>,
    /// 进度更新的间隔时间（秒）。
    update_interval: f64,
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
            config: DownloaderConfig {
                url: url.into(),
                output_path: output_path.into(),
                workers,
            },
            client_builder,
            cmd_tx,
            info_tx,
            update_interval,
        }
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
        // 构建 reqwest 客户端
        let client = (self.client_builder)().build()?;
        // 获取文件信息（大小、是否支持范围请求）
        let (file_size, support_ranges) = get_file_info(&client, &self.config.url).await?;
        // 为进度处理器订阅信息通道
        let info_rx_for_progress = self.info_tx.subscribe();
        // 启动文件写入任务，并获取其命令发送端
        let writer_tx = file_writer_task(self.config.output_path.clone(), file_size).await?;

        // 异步执行用户提供的进度处理逻辑
        spawn(progress_handler(file_size, info_rx_for_progress));

        // 协调和管理所有下载任务
        self.orchestrate_downloads(file_size, support_ranges, writer_tx, client)
            .await?;

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
    ) -> Result<()> {
        // 使用 FuturesUnordered 来管理所有并发的下载任务
        let tasks = FuturesUnordered::new();
        // 用于生成唯一的块 ID
        let next_chunk_id = AtomicU64::new(0);

        // 决定实际的并发数
        let workers = if !support_ranges || self.config.workers == 1 || file_size < MIN_CHUNK_SIZE {
            // 如果服务器不支持范围请求，或用户只设置了1个worker，或文件太小，则强制使用单线程
            1
        } else {
            self.config.workers
        };

        // 创建并启动第一个下载任务，它将下载整个文件
        let initial_id = next_chunk_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let rb = client.get(self.config.url.as_str());
        let task = chunk_run(
            initial_id,
            writer_tx.clone(),
            self.cmd_tx.subscribe(),
            self.info_tx.clone(),
            rb,
            0,
            file_size.saturating_sub(1),
        );
        tasks.push(spawn(task));
        println!(
            "[Main] 启动初始下载任务 (ID: {}), 最大并发数设置为: {}",
            initial_id, workers
        );

        // 创建下载监控器
        let monitor = DownloadMonitor::new(file_size, self.update_interval, workers);

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
                &self.config.url,
            )
            .await;
        Ok(())
    }
}
