//! 下载器监督者与系统入口。

use crate::monitor::MonitorActor;
use crate::types::{DownloadInfo, DownloaderConfig, Result, SystemCommand, SystemEvent};
use crate::util::{get_file_info, writer_actor_task};
use faststr::FastStr;
use log::{debug, info};
use reqwest::ClientBuilder;
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};

/// 下载器，作为 Actor 系统的监督者和入口。
pub struct Downloader<F>
where
    F: Fn() -> ClientBuilder,
{
    url: FastStr,
    output_path: FastStr,
    config: DownloaderConfig,
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
    /// * `config`: 下载过程的配置。
    /// * `client_builder`: 一个闭包，用于返回 `reqwest::ClientBuilder` 来自定义 HTTP 客户端。
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
    /// # 参数
    ///
    /// * `progress_handler`: 一个异步闭包，它接收文件总大小和一个用于接收 `DownloadInfo` 更新的广播接收器。
    ///   这个闭包将被派生为一个独立的任务来处理进度更新。
    pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
    where
        ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        info!(
            "启动下载 Actor 系统: '{}' -> '{}'",
            self.url, self.output_path
        );

        // 1. 创建通信信道
        let (info_tx, info_rx_for_progress) = broadcast::channel(self.config.channel_capacity);
        let (cmd_tx, _) = broadcast::channel::<SystemCommand>(self.config.channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<SystemEvent>(self.config.channel_capacity);

        // 2. 获取文件信息
        let client = (self.client_builder)().build()?;
        let (file_size, _) = get_file_info(&client, &self.url).await?;
        info!("文件大小: {} 字节。", file_size);

        // 3. 启动文件写入 Actor
        let writer_tx = writer_actor_task(
            self.output_path.clone(),
            file_size,
            self.config.writer_queue_capacity,
        )
        .await?;

        // 4. 派生用户进度处理任务
        spawn(progress_handler(file_size, info_rx_for_progress));

        // 5. 创建并启动核心 MonitorActor
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

        // 6. 等待 MonitorActor 完成
        monitor_handle.await?;

        // 7. 下载结束，发送终止命令以清理所有任务
        info!("[Downloader] 下载协调完成，发送 TerminateAll 命令。");
        // 如果发送失败，说明所有接收者都已消失，这是正常情况，因此忽略错误。
        let _ = cmd_tx.send(SystemCommand::TerminateAll);

        Ok(())
    }
}
