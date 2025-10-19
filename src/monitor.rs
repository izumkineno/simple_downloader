//! 下载监控器，作为状态、重试和并发管理的协调中心。

use crate::chunk::chunk_run;
use crate::concurrency::ConcurrencyManager;
use crate::retry::RetryHandler;
use crate::state::{ChunkState, DownloadState};
use crate::types::{DownloadCmd, DownloadInfo};
use faststr::FastStr;
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::interval;

/// 用于速度计算的平滑因子，防止速度因瞬时网络波动而剧烈变化。
const SMOOTHING_FACTOR: f64 = 0.15;

/// 下载监控器，充当状态、重试和并发管理的协调器。
pub struct DownloadMonitor {
    /// 整个下载任务的中心状态存储。
    state: DownloadState,
    /// 失败块的重试逻辑处理器。
    retry_handler: RetryHandler,
    /// 动态并发控制管理器。
    concurrency_manager: ConcurrencyManager,
    /// 状态更新的间隔时间（秒）。
    update_interval: f64,
}

impl DownloadMonitor {
    /// 创建一个新的 `DownloadMonitor` 实例。
    pub fn new(total_file_size: u64, update_interval: f64, max_workers: u64) -> Self {
        Self {
            state: DownloadState::new(total_file_size),
            retry_handler: RetryHandler::new(),
            concurrency_manager: ConcurrencyManager::new(max_workers),
            update_interval,
        }
    }

    /// 运行监控器的主事件循环。
    ///
    /// 这个循环会监听来自各个下载块的信息，并定期触发状态更新、并发决策和重试处理。
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        mut self,
        mut info_rx: broadcast::Receiver<DownloadInfo>,
        info_tx: broadcast::Sender<DownloadInfo>,
        mut tasks: FuturesUnordered<JoinHandle<()>>,
        next_chunk_id: &AtomicU64,
        client: &Client,
        writer_tx: mpsc::Sender<DownloadCmd>,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
        url: &FastStr,
    ) {
        let mut ticker = interval(Duration::from_secs_f64(self.update_interval));
        let mut last_tick_time = Instant::now();

        'main_loop: loop {
            tokio::select! {
                // `biased` 确保优先处理已完成的任务和信息，而不是等待定时器。
                biased;

                // 一个下载任务已完成（或 panic）
                Some(result) = tasks.next() => {
                    if let Err(e) = result { eprintln!("[Monitor] 一个下载任务 panicked: {e}"); }
                    // 如果所有任务都已结束，则退出循环
                    if tasks.is_empty() && self.are_all_tasks_done() { break 'main_loop; }
                },

                // 收到来自下载块的信息
                Ok(info) = info_rx.recv() => {
                    self.handle_download_info(info, &mut tasks, next_chunk_id, client, &writer_tx, cmd_tx, &info_tx, url);
                },

                // 定时器触发
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let elapsed_secs = (now - last_tick_time).as_secs_f64();
                    last_tick_time = now;

                    // 执行定期的处理逻辑
                    if self.handle_tick(elapsed_secs, &mut tasks, &info_tx, cmd_tx, client, &writer_tx, url) {
                        // 如果 tick 处理器返回 true，表示下载已完成
                        break 'main_loop;
                    }
                },

                // 通道关闭或发生其他错误，退出循环
                else => break,
            }
        }
        println!("[Monitor] 所有下载任务已完成。监控器正在关闭。");
    }

    /// 处理从下载块接收到的各种 `DownloadInfo` 消息。
    fn handle_download_info(
        &mut self,
        info: DownloadInfo,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
        next_chunk_id: &AtomicU64,
        client: &Client,
        writer_tx: &mpsc::Sender<DownloadCmd>,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
        info_tx: &broadcast::Sender<DownloadInfo>,
        url: &FastStr,
    ) {
        match info {
            DownloadInfo::ChunkProgress {
                id,
                start_byte,
                end_byte,
                downloaded,
            } => {
                // 更新块的进度信息
                let chunk = self
                    .state
                    .chunks
                    .entry(id)
                    .or_insert_with(|| ChunkState::new(id, start_byte, end_byte));
                chunk.update_downloaded(downloaded);
                chunk.update_end_byte(end_byte);
                // 如果块的状态不是“下载中”，则更新为“下载中”
                if chunk.status != 0 {
                    chunk.update_status(0, None);
                    let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                        id,
                        status: 0,
                        message: None,
                    });
                }
            }
            DownloadInfo::DownloadComplete(id) => {
                // 标记一个块为已完成
                self.state.complete_chunk(&id);
                self.retry_handler.on_download_complete(&id);
                let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                    id,
                    status: 4,
                    message: None,
                });
            }
            DownloadInfo::ChunkFailed {
                id,
                start,
                end,
                error,
            } => {
                // 将失败的块交给重试处理器
                self.retry_handler
                    .on_chunk_failed(id, start, end, error, &mut self.state, info_tx);
            }
            DownloadInfo::ChunkBisected {
                new_start, new_end, ..
            } => {
                // 当一个块被分割时，为新的部分创建一个新的下载任务
                let new_id = next_chunk_id.fetch_add(1, Ordering::SeqCst);
                let rb = client.get(url.as_str());
                let task = chunk_run(
                    new_id,
                    writer_tx.clone(),
                    cmd_tx.subscribe(),
                    info_tx.clone(),
                    rb,
                    new_start,
                    new_end,
                );
                tasks.push(tokio::spawn(task));
            }
            _ => {}
        }
    }

    /// 处理定时器触发的事件。
    /// 返回 `true` 表示下载已完成。
    fn handle_tick(
        &mut self,
        elapsed_secs: f64,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
        info_tx: &broadcast::Sender<DownloadInfo>,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
        client: &Client,
        writer_tx: &mpsc::Sender<DownloadCmd>,
        url: &FastStr,
    ) -> bool {
        if elapsed_secs <= 0.0 {
            return false;
        }

        // 委托状态更新：计算每个块的速度
        for chunk in self.state.chunks.values_mut() {
            chunk.update_speed(elapsed_secs, SMOOTHING_FACTOR);
        }
        // 发送聚合后的监控更新
        self.send_monitor_update(info_tx);

        // 委托并发控制：让并发管理器决定是否需要分割块
        self.concurrency_manager.decide_and_act(&self.state, cmd_tx);

        // 委托重试处理：处理重试队列
        self.retry_handler.process_queues();
        while let Some(chunk_to_retry) = self.retry_handler.pop_ready_chunk() {
            // 如果有块准备好重试，则为其创建新任务
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id: chunk_to_retry.id,
                status: 1, // 状态：重试中
                message: Some(format!("正在进行第 {} 次重试", chunk_to_retry.attempts)),
            });
            let rb = client.get(url.as_str());
            let task = chunk_run(
                chunk_to_retry.id,
                writer_tx.clone(),
                cmd_tx.subscribe(),
                info_tx.clone(),
                rb,
                chunk_to_retry.start,
                chunk_to_retry.end,
            );
            tasks.push(tokio::spawn(task));
        }

        // 检查下载是否已全部完成
        self.are_all_tasks_done() && self.state.is_download_finished()
    }

    /// 发送聚合的监控更新信息。
    fn send_monitor_update(&self, info_tx: &broadcast::Sender<DownloadInfo>) {
        let chunk_details = self
            .state
            .chunks
            .values()
            .map(|c| (c.id, c.size(), c.downloaded_bytes, c.speed, c.status))
            .collect();
        let _ = info_tx.send(DownloadInfo::MonitorUpdate {
            total_size: self.state.total_file_size,
            total_downloaded: self.state.total_downloaded(),
            total_speed: self.state.total_speed(),
            chunk_details,
        });
    }

    /// 检查是否所有任务（包括活跃的下载和重试队列中的）都已处理完毕。
    fn are_all_tasks_done(&self) -> bool {
        self.state.chunks.is_empty() && self.retry_handler.are_all_tasks_done()
    }
}
