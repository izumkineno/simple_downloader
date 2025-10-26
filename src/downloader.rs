/// 包含下载器 `Downloader` 的定义，作为系统的入口和监督者。
use crate::monitor::MonitorActor;
use crate::types::{DownloadInfo, DownloaderConfig, Result, SystemCommand, SystemEvent};
use crate::util::{get_file_info, writer_actor_task};
use faststr::FastStr;
use log::{debug, info};
use reqwest::ClientBuilder;
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};

/// 下载器，作为 Actor 系统的监督者和用户交互的入口。
///
/// 它负责初始化整个下载流程，包括创建通信信道、启动核心的 `MonitorActor`
/// 和 `WriterActor`，并管理它们的生命周期。
pub struct Downloader<F>
where
    F: Fn() -> ClientBuilder,
{
    /// 要下载的文件的 URL。
    url: FastStr,
    /// 下载文件保存的本地路径。
    output_path: FastStr,
    /// 下载过程的详细配置。
    config: DownloaderConfig,
    /// 用于创建 `reqwest::Client` 的闭包，允许用户自定义 HTTP 客户端。
    client_builder: F,
}

impl<F> Downloader<F>
where
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    /// 创建一个新的 `Downloader` 实例。
    ///
    /// # 参数
    ///
    /// * `url`: 要下载的文件的 URL。
    /// * `output_path`: 保存文件的本地路径。
    /// * `config`: 下载过程的配置，定义了并发数、重试策略等。
    /// * `client_builder`: 一个闭包，用于返回 `reqwest::ClientBuilder` 来自定义 HTTP 客户端。
    ///   这允许用户配置代理、超时、自定义头等高级选项。
    ///
    /// # 返回
    ///
    /// 返回一个新的 `Downloader` 实例。
    pub fn new(
        url: impl Into<FastStr>,
        output_path: impl Into<FastStr>,
        config: DownloaderConfig,
        client_builder: F,
    ) -> Self {
        Self {
            url: url.into(),
            output_path: output_path.into(),
            config,
            client_builder,
        }
    }

    /// 启动下载过程，初始化并运行整个 Actor 系统。
    ///
    /// 这个方法会消费 `Downloader` 实例，并在后台启动所有必要的并发任务。
    /// 它会阻塞直到下载完成或发生不可恢复的错误。
    ///
    /// # 参数
    ///
    /// * `progress_handler`: 一个异步闭包，它接收文件总大小和一个用于接收 `DownloadInfo` 更新的广播接收器。
    ///   这个闭包将被派生为一个独立的任务来处理进度更新，允许用户实现自己的进度条、日志记录等。
    pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
    where
        ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!(
            "启动下载 Actor 系统: '{}' -> '{}'",
            self.url, self.output_path
        );

        // 步骤 1: 创建整个系统所需的通信信道。
        // `info_tx/rx`: 用于向用户端广播进度更新。
        // `cmd_tx/rx`: 用于广播系统级命令，如分片、终止。
        // `event_tx/rx`: 用于将事件从 `ChunkActor` 发送到 `MonitorActor`。
        let (info_tx, info_rx_for_progress) = broadcast::channel(self.config.channel_capacity);
        let (cmd_tx, _) = broadcast::channel::<SystemCommand>(self.config.channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<SystemEvent>(self.config.channel_capacity);

        // 步骤 2: 获取文件信息（大小和是否支持范围请求）。
        let client = (self.client_builder)().build()?;
        let (file_size, _) = get_file_info(&client, &self.url).await?;
        info!("文件大小: {} 字节。", file_size);

        // 步骤 3: 启动文件写入 Actor，它将在一个独立的任务中处理所有文件写入请求。
        let writer_tx = writer_actor_task(
            self.output_path.clone(),
            file_size,
            self.config.writer_queue_capacity,
        )
        .await?;

        // 步骤 4: 派生用户提供的进度处理任务。
        spawn(progress_handler(file_size, info_rx_for_progress));

        // 步骤 5: 创建并启动核心的 MonitorActor，它是下载逻辑的中心。
        let monitor_actor = MonitorActor::new(
            file_size,
            self.config,
            self.client_builder,
            self.url,
            event_rx,
            cmd_tx.clone(),
            info_tx,
            writer_tx,
            event_tx,
        )?;
        let monitor_handle = spawn(monitor_actor.run());
        debug!("[Downloader] MonitorActor 已启动。");

        // 步骤 6: 等待 MonitorActor 完成其所有工作。
        // `monitor_handle.await` 会在 `MonitorActor` 的 `run` 方法结束后返回。
        monitor_handle.await?;

        // 步骤 7: 下载协调完成，发送终止命令以清理所有剩余的任务（如 ChunkActor）。
        info!("[Downloader] 下载协调完成，发送 TerminateAll 命令。");
        // 如果发送失败，说明所有接收者都已消失（这是任务结束时的正常情况），因此忽略错误。
        let _ = cmd_tx.send(SystemCommand::TerminateAll);

        Ok(())
    }
}
