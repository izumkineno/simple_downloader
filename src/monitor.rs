//! 下载监控器，作为状态、重试和并发管理的协调中心。

use crate::chunk::chunk_run;
use crate::concurrency::ConcurrencyManager;
use crate::lane::MultiRuntime;
use crate::retry::RetryHandler;
use crate::state::{ChunkState, DownloadState};
use crate::types::{ChunkId, DownloadCmd, DownloadInfo};
use faststr::FastStr;
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use std::collections::HashMap;
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
    /// 多源模式下的 chunk -> lane 绑定。
    lane_bindings: HashMap<ChunkId, FastStr>,
    /// 因 lane 容量不足暂未调度的分割新区间的缓冲，避免丢范围空洞。
    pub(crate) pending_bisects: std::collections::VecDeque<(u64, u64)>,
    /// 状态更新的间隔时间（秒）。
    update_interval: f64,
}

impl DownloadMonitor {
    /// 创建一个新的 `DownloadMonitor` 实例。
    pub fn new(total_file_size: u64, update_interval: f64, max_workers: u64) -> Self {
        Self::new_with_completed(total_file_size, 0, update_interval, max_workers)
    }

    pub fn new_with_completed(
        total_file_size: u64,
        completed_bytes: u64,
        update_interval: f64,
        max_workers: u64,
    ) -> Self {
        Self {
            state: DownloadState::with_completed(total_file_size, completed_bytes),
            retry_handler: RetryHandler::new(),
            concurrency_manager: ConcurrencyManager::new(max_workers),
            lane_bindings: HashMap::new(),
            pending_bisects: std::collections::VecDeque::new(),
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
        url: Option<&FastStr>,
        initial_lanes: Vec<(ChunkId, FastStr)>,
        mut multi_runtime: Option<MultiRuntime>,
    ) -> Result<(), crate::types::DownloadError> {
        for (chunk_id, lane_id) in initial_lanes {
            self.lane_bindings.insert(chunk_id, lane_id);
        }

        let mut ticker = interval(Duration::from_secs_f64(self.update_interval));
        let mut last_tick_time = Instant::now();

        'main_loop: loop {
            // 永久失败快速退出，避免无限挂死
            if self.retry_handler.has_permanent_failure() {
                let msg = self
                    .retry_handler
                    .permanent_failure_message()
                    .unwrap_or_else(|| "unknown permanent failure".to_owned());
                let _ = cmd_tx.send(DownloadCmd::TerminateAll);
                eprintln!("[Monitor] 永久失败，终止下载: {msg}");
                return Err(crate::types::DownloadError::PermanentFailure(msg));
            }
            tokio::select! {
                // `biased` 确保优先处理已完成的任务和信息，而不是等待定时器。
                biased;

                // 一个下载任务已完成（或 panic）
                Some(result) = tasks.next() => {
                    if let Err(e) = result { eprintln!("[Monitor] 一个下载任务 panicked: {e}"); }
                    // 任务结束不直接判定下载完成，避免与 DownloadComplete 竞态丢事件；
                    // 真正的完成判定由定时 tick 的 handle_tick 统一处理。
                    // 仅在空任务且已完成时可提前退出，避免 0.5s tick 延迟。
                    if tasks.is_empty()
                        && self.are_all_tasks_done()
                        && self.state.is_download_finished()
                    {
                        break 'main_loop;
                    }
                },

                // 收到来自下载块的信息；区分 Lagged/Closed，避免因广播积压误退出
                result = info_rx.recv() => match result {
                    Ok(info) => {
                        self.handle_download_info(
                            info,
                            &mut tasks,
                            next_chunk_id,
                            client,
                            &writer_tx,
                            cmd_tx,
                            &info_tx,
                            url,
                            multi_runtime.as_mut(),
                        );
                        if self.retry_handler.has_permanent_failure() {
                            let msg = self
                                .retry_handler
                                .permanent_failure_message()
                                .unwrap_or_else(|| "unknown permanent failure".to_owned());
                            let _ = cmd_tx.send(DownloadCmd::TerminateAll);
                            return Err(crate::types::DownloadError::PermanentFailure(msg));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        eprintln!("[Monitor] 广播滞后，跳过 {skipped} 条事件，继续运行");
                    }
                    Err(broadcast::error::RecvError::Closed) => break 'main_loop,
                },

                // 定时器触发
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let elapsed_secs = (now - last_tick_time).as_secs_f64();
                    last_tick_time = now;

                    // 执行定期的处理逻辑
                    if self.handle_tick(
                        elapsed_secs,
                        &mut tasks,
                        &info_tx,
                        cmd_tx,
                        client,
                        &writer_tx,
                        url,
                        &mut multi_runtime,
                        next_chunk_id,
                    ) {
                        // 如果 tick 处理器返回 true，表示下载已完成
                        break 'main_loop;
                    }
                    if self.retry_handler.has_permanent_failure() {
                        let msg = self
                            .retry_handler
                            .permanent_failure_message()
                            .unwrap_or_else(|| "unknown permanent failure".to_owned());
                        let _ = cmd_tx.send(DownloadCmd::TerminateAll);
                        return Err(crate::types::DownloadError::PermanentFailure(msg));
                    }
                },
            }
        }
        println!("[Monitor] 所有下载任务已完成。监控器正在关闭。");
        Ok(())
    }
    /// 处理从下载块接收到的各种 `DownloadInfo` 消息。
    #[allow(clippy::too_many_arguments)]
    fn handle_download_info(
        &mut self,
        info: DownloadInfo,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
        next_chunk_id: &AtomicU64,
        client: &Client,
        writer_tx: &mpsc::Sender<DownloadCmd>,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
        info_tx: &broadcast::Sender<DownloadInfo>,
        url: Option<&FastStr>,
        multi_runtime: Option<&mut MultiRuntime>,
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
                if let Some(lane_id) = self.lane_bindings.remove(&id)
                    && let Some(runtime) = multi_runtime
                {
                    runtime.record_success(&lane_id);
                    runtime.release_chunk(&lane_id);
                }
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
                if let Some(lane_id) = self.lane_bindings.remove(&id)
                    && let Some(runtime) = multi_runtime
                {
                    runtime.record_failure(&lane_id);
                    runtime.release_chunk(&lane_id);
                }
                // 将失败的块交给重试处理器
                self.retry_handler
                    .on_chunk_failed(id, start, end, error, &mut self.state, info_tx);
            }
            DownloadInfo::ChunkBisected {
                new_start, new_end, ..
            } => {
                // 尝试为新区间分配 lane；若容量不足则缓冲至 pending_bisects，避免丢范围
                let Some((lane_id, rb)) = build_request(client, url, multi_runtime) else {
                    eprintln!(
                        "[Monitor] lane 容量不足，缓冲分割区间 {new_start}-{new_end} 待下次 tick 调度"
                    );
                    self.pending_bisects.push_back((new_start, new_end));
                    return;
                };
                let new_id = next_chunk_id.fetch_add(1, Ordering::SeqCst);
                if let Some(lane_id) = lane_id {
                    self.lane_bindings.insert(new_id, lane_id);
                }
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
    #[allow(clippy::too_many_arguments)]
    fn handle_tick(
        &mut self,
        elapsed_secs: f64,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
        info_tx: &broadcast::Sender<DownloadInfo>,
        cmd_tx: &broadcast::Sender<DownloadCmd>,
        client: &Client,
        writer_tx: &mpsc::Sender<DownloadCmd>,
        url: Option<&FastStr>,
        multi_runtime: &mut Option<MultiRuntime>,
        next_chunk_id: &AtomicU64,
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

        // 调度之前因 lane 容量不足而缓冲的分割区间
        while let Some((start, end)) = self.pending_bisects.front().copied() {
            let Some((lane_id, rb)) = build_request(client, url, multi_runtime.as_mut()) else {
                break;
            };
            self.pending_bisects.pop_front();
            let new_id = next_chunk_id.fetch_add(1, Ordering::SeqCst);
            if let Some(lane_id) = lane_id {
                self.lane_bindings.insert(new_id, lane_id);
            }
            let task = chunk_run(
                new_id,
                writer_tx.clone(),
                cmd_tx.subscribe(),
                info_tx.clone(),
                rb,
                start,
                end,
            );
            tasks.push(tokio::spawn(task));
        }

        // 委托重试处理：处理重试队列
        self.retry_handler.process_queues();
        let mut deferred_retries = Vec::new();
        while let Some(chunk_to_retry) = self.retry_handler.pop_ready_chunk() {
            let Some((lane_id, rb)) = build_request(client, url, multi_runtime.as_mut()) else {
                deferred_retries.push(chunk_to_retry);
                continue;
            };
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id: chunk_to_retry.id,
                status: 1, // 状态：重试中
                message: Some(format!("正在进行第 {} 次重试", chunk_to_retry.attempts)),
            });
            if let Some(lane_id) = lane_id {
                self.lane_bindings.insert(chunk_to_retry.id, lane_id);
            }
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
        for chunk in deferred_retries.into_iter().rev() {
            self.retry_handler.push_front_retry(chunk);
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
        self.pending_bisects.is_empty()
            && self.state.chunks.is_empty()
            && self.retry_handler.are_all_tasks_done()
    }
}

fn build_request(
    client: &Client,
    url: Option<&FastStr>,
    multi_runtime: Option<&mut MultiRuntime>,
) -> Option<(Option<FastStr>, reqwest::RequestBuilder)> {
    if let Some(runtime) = multi_runtime {
        let (lane_id, rb) = runtime.claim_request_builder()?;
        return Some((Some(lane_id), rb));
    }

    let url = url?;
    Some((None, client.get(url.as_str())))
}
