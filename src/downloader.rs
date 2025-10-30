// // downloader.rs

// use crate::chunk::ChunkContext;
// use crate::file::file_writer_task;
// use crate::monitor::{MonitorScheduler, SchedulingStrategy, SimpleSplitStrategy};
// use crate::resource::Resource;
// use crate::state::DownloadState;
// use crate::types::{DownloadError, EventResource, EventStatus, EventThread, Result};
// use faststr::FastStr;
// use futures_util::StreamExt;
// use futures_util::stream::FuturesUnordered;
// use reqwest::ClientBuilder;
// use std::sync::Arc;
// use std::time::Duration;
// use tokio::spawn;
// use tokio::sync::{broadcast, mpsc};
// use tokio::task::JoinHandle;

// /// 下载器配置构建器
// ///
// /// 提供了一种链式调用的方式来配置和创建一个 `Downloader` 实例。
// pub struct DownloaderBuilder<F: Fn() -> ClientBuilder> {
//     url: FastStr,
//     output_path: FastStr,
//     client_builder: F,
//     initial_chunks: usize,
//     proxy: Option<FastStr>,
//     scheduling_strategy: Box<dyn SchedulingStrategy>,
// }

// impl<F: Fn() -> ClientBuilder + Send + Sync + 'static> DownloaderBuilder<F> {
//     /// 创建一个新的 DownloaderBuilder
//     ///
//     /// # Arguments
//     /// * `url` - 下载链接
//     /// * `output_path` - 文件保存路径
//     /// * `client_builder` - 一个返回 `reqwest::ClientBuilder` 的闭包，用于自定义客户端
//     pub fn new(
//         url: impl Into<FastStr>,
//         output_path: impl Into<FastStr>,
//         client_builder: F,
//     ) -> Self {
//         Self {
//             url: url.into(),
//             output_path: output_path.into(),
//             client_builder,
//             initial_chunks: 8, // 默认初始分片数为 8
//             proxy: None,
//             // 默认使用一个简单的分裂策略
//             scheduling_strategy: Box::new(SimpleSplitStrategy::new(16, 0.5, 1024 * 1024, 5)),
//         }
//     }

//     /// 设置初始并发下载的分片数量
//     pub fn initial_chunks(mut self, count: usize) -> Self {
//         self.initial_chunks = count.max(1); // 至少为 1
//         self
//     }

//     /// 设置代理服务器地址
//     pub fn proxy(mut self, proxy: Option<impl Into<FastStr>>) -> Self {
//         self.proxy = proxy.map(Into::into);
//         self
//     }

//     /// 自定义调度策略
//     pub fn strategy(mut self, strategy: Box<dyn SchedulingStrategy>) -> Self {
//         self.scheduling_strategy = strategy;
//         self
//     }

//     /// 构建 Downloader 实例
//     ///
//     /// 在构建时会进行网络探测，以确认资源可用性和服务器特性。
//     ///
//     /// # Returns
//     /// * `Result<Downloader>` - 如果资源探测成功，则返回 Downloader 实例
//     pub async fn build(self) -> Result<Downloader> {
//         // 1. 创建资源对象并测试其可用性
//         let mut resource = Resource::new(
//             self.client_builder,
//             true, // 初始假设支持连接池
//             self.url.clone(),
//             self.proxy,
//         );
//         let file_size = resource.test_availability().await?;

//         // 2. 创建通信通道
//         let (command_tx, _) = broadcast::channel(128);
//         let (status_tx, status_rx) = mpsc::channel(128);

//         // 3. 创建监控调度器
//         let monitor = MonitorScheduler::new(
//             file_size,
//             status_rx,
//             command_tx.clone(),
//             Duration::from_secs(1), // 每秒检查一次调度
//             self.scheduling_strategy,
//         );

//         Ok(Downloader {
//             url: self.url,
//             output_path: self.output_path,
//             file_size,
//             initial_chunks: self.initial_chunks,
//             resource: Arc::new(resource),
//             command_tx,
//             status_tx,
//             monitor: Some(monitor),
//         })
//     }
// }

// /// 多线程分片下载器
// ///
// /// 负责编排整个下载流程，包括初始化、任务分发、状态监控和最终完成。
// pub struct Downloader {
//     url: FastStr,
//     output_path: FastStr,
//     file_size: u64,
//     initial_chunks: usize,
//     resource: Arc<Resource<Box<dyn Fn() -> ClientBuilder + Send + Sync>>>,
//     command_tx: broadcast::Sender<EventThread>,
//     status_tx: mpsc::Sender<EventStatus>,
//     monitor: Option<MonitorScheduler>,
// }

// impl Downloader {
//     /// 运行下载任务直到完成
//     ///
//     /// 此方法会阻塞直到所有分片下载完成或出现不可恢复的错误。
//     pub async fn run(mut self) -> Result<()> {
//         if self.file_size == 0 {
//             // 对于大小为 0 的文件，直接创建一个空文件并返回成功
//             tokio::fs::File::create(&*self.output_path).await?;
//             return Ok(());
//         }

//         // 1. 启动文件写入任务
//         let file_writer_tx = file_writer_task(
//             self.command_tx.subscribe(),
//             self.output_path,
//             self.file_size,
//         )
//         .await?;

//         // 2. 启动监控调度器
//         let monitor_handle = if let Some(monitor) = self.monitor.take() {
//             spawn(monitor.run())
//         } else {
//             // 理论上不会发生，因为 build 总是会创建 monitor
//             return Err(DownloadError::Io(std::io::Error::new(
//                 std::io::ErrorKind::Other,
//                 "MonitorScheduler not found",
//             )));
//         };

//         // 3. 创建并运行初始的分片任务
//         let mut tasks = self.create_initial_chunks(file_writer_tx);

//         // 4. 主事件循环：监听新分片请求和任务完成
//         loop {
//             tokio::select! {
//                 // 监听是否有任务完成
//                 Some(result) = tasks.next() => {
//                     if let Err(e) = result {
//                         // 一个任务的 panic 是严重错误，终止所有任务
//                         eprintln!("A download task panicked: {:?}", e);
//                         let _ = self.command_tx.send(EventThread::TerminateAll);
//                         return Err(e.into());
//                     }
//                 },

//                 // 监听监控器是否发出了分裂分片的指令
//                 Ok(event) = self.command_tx.subscribe().recv() => {
//                     if let EventStatus::ChunkSplited { new_start_byte, new_end_byte, .. } = event {
//                         println!("Downloader: Creating new chunk from split: {}-{}", new_start_byte, new_end_byte);
//                         let new_chunk = self.create_chunk(
//                             file_writer_tx.clone(),
//                             new_start_byte,
//                             new_end_byte
//                         );
//                         tasks.push(spawn(new_chunk));
//                     }
//                 }
//             }

//             // 如果所有任务都已完成，则退出循环
//             if tasks.is_empty() {
//                 break;
//             }
//         }

//         // 5. 等待监控器任务结束（可选，确保所有状态消息都被处理）
//         drop(self.status_tx); // 关闭 status_tx，让监控器知道不会再有新消息
//         let _ = monitor_handle.await;

//         println!("Download completed successfully.");
//         Ok(())
//     }

//     /// 创建初始的一组分片任务
//     fn create_initial_chunks(
//         &self,
//         file_writer_tx: std::sync::mpsc::Sender<EventResource>,
//     ) -> FuturesUnordered<JoinHandle<()>> {
//         let mut tasks = FuturesUnordered::new();
//         let chunk_size = (self.file_size as f64 / self.initial_chunks as f64).ceil() as u64;

//         if chunk_size == 0 {
//             return tasks;
//         }

//         for i in 0..self.initial_chunks {
//             let start_byte = i as u64 * chunk_size;
//             if start_byte >= self.file_size {
//                 break;
//             }
//             let end_byte = (start_byte + chunk_size - 1).min(self.file_size - 1);

//             let chunk_task = self.create_chunk(file_writer_tx.clone(), start_byte, end_byte);
//             tasks.push(spawn(chunk_task));
//         }
//         tasks
//     }

//     /// 创建一个分片下载的异步任务
//     fn create_chunk(
//         &self,
//         file_writer_tx: std::sync::mpsc::Sender<EventResource>,
//         start_byte: u64,
//         end_byte: u64,
//     ) -> impl std::future::Future<Output = ()> {
//         let context = ChunkContext::new(
//             file_writer_tx,
//             self.command_tx.subscribe(),
//             self.status_tx.clone(),
//             start_byte,
//             end_byte,
//         );

//         let resource = self.resource.clone();

//         async move {
//             match resource.get_request() {
//                 Ok(req_builder) => {
//                     context.run(req_builder).await;
//                 }
//                 Err(e) => {
//                     // 如果连请求都无法创建，直接报告失败
//                     let _ = context.state_tx.send(EventStatus::ChunkFailed {
//                         id: context.id,
//                         start_byte: context.start_byte,
//                         end_byte: context.end_byte,
//                         downloaded_bytes: 0,
//                         error: e.to_string().into(),
//                     });
//                 }
//             }
//         }
//     }
// }
