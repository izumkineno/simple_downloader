//! 下载监控器，作为状态、重试和并发管理的协调中心。

use crate::chunk::chunk_run;
use crate::limiter::RateLimiter;
use std::sync::Arc;
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

/// 用于速度计算的平滑因子，0.30 更快响应新建连接的带宽变化，利于探测增益
const SMOOTHING_FACTOR: f64 = 0.30;

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
    /// Lagged 事件计数，用于 P0-03 对账
    lagged_count: u64,
    is_rate_limited: bool,
    global_limiter: Option<Arc<RateLimiter>>,
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
        ::tracing::debug!(
            total_file_size,
            completed_bytes,
            update_interval,
            max_workers,
            "DownloadMonitor created"
        );
        Self {
            state: DownloadState::with_completed(total_file_size, completed_bytes),
            retry_handler: RetryHandler::new(),
            concurrency_manager: ConcurrencyManager::new_with_interval(max_workers, update_interval),
            lane_bindings: HashMap::new(),
            pending_bisects: std::collections::VecDeque::new(),
            update_interval,
            lagged_count: 0,
            is_rate_limited: false,
            global_limiter: None,
        }
    }

    pub fn with_rate_limit(mut self, limiter: Option<Arc<RateLimiter>>) -> Self {
        self.is_rate_limited = limiter.is_some();
        self.global_limiter = limiter;
        self
    }

    pub fn set_rate_limited(&mut self, limited: bool) {
        self.is_rate_limited = limited;
    }

    /// 运行监控器的主事件循环。
    ///
    /// 这个循环会监听来自各个下载块的信息，并定期触发状态更新、并发决策和重试处理。
    #[allow(clippy::too_many_arguments)]
    #[::tracing::instrument(skip(self, info_rx, info_tx, tasks, next_chunk_id, client, writer_tx, cmd_tx, url, initial_lanes, multi_runtime), fields(total_size = self.state.total_file_size, completed = self.state.total_downloaded()))]
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
        ::tracing::info!(
            total_size = self.state.total_file_size,
            completed = self.state.total_downloaded(),
            update_interval = self.update_interval,
            pending_bisects = self.pending_bisects.len(),
            tasks = tasks.len(),
            lanes = ?initial_lanes.iter().map(|(id,lane)|(id,lane.as_str())).collect::<Vec<_>>(),
            "monitor start"
        );
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
                ::tracing::error!(msg = %msg, "monitor permanent failure, terminating");
                return Err(crate::types::DownloadError::PermanentFailure(msg));
            }
            tokio::select! {
                // `biased` 确保优先处理已完成的任务和信息，而不是等待定时器。
                biased;

                // 一个下载任务已完成（或 panic）
                Some(result) = tasks.next() => {
                    if let Err(e) = result { ::tracing::error!(error = %e, "download task panicked"); }
                    ::tracing::debug!(
                        remaining_tasks = tasks.len(),
                        pending_bisects = self.pending_bisects.len(),
                        active_chunks = self.state.chunks.len(),
                        downloaded = self.state.total_downloaded(),
                        total = self.state.total_file_size,
                        finished = self.state.is_download_finished(),
                        "task joined"
                    );
                    // 任务结束不直接判定下载完成，避免与 DownloadComplete 竞态丢事件；
                    // 真正的完成判定由定时 tick 的 handle_tick 统一处理。
                    // 仅在空任务且已完成时可提前退出，避免 0.5s tick 延迟。
                    if tasks.is_empty()
                        && self.are_all_tasks_done()
                        && self.state.is_download_finished()
                    {
                        ::tracing::info!("monitor all done fast-path exit");
                        break 'main_loop;
                    }
                },

                // 收到来自下载块的信息；区分 Lagged/Closed，避免因广播积压误退出
                result = info_rx.recv() => match result {
                    Ok(info) => {
                        ::tracing::trace!(info = ?info, "monitor recv info");
                        self.handle_download_info(
                            info,
                            &mut tasks,
                            next_chunk_id,
                            client,
                            &writer_tx,
                            cmd_tx,
                            &info_tx,
                            url,
                            &mut multi_runtime,
                        );
                        if self.retry_handler.has_permanent_failure() {
                            let msg = self
                                .retry_handler
                                .permanent_failure_message()
                                .unwrap_or_else(|| "unknown permanent failure".to_owned());
                            let _ = cmd_tx.send(DownloadCmd::TerminateAll);
                            ::tracing::error!(msg = %msg, "permanent failure after handling info");
                            return Err(crate::types::DownloadError::PermanentFailure(msg));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        self.lagged_count += skipped;
                        ::tracing::warn!(skipped, total_lagged = self.lagged_count, pending_bisects = self.pending_bisects.len(), active_chunks = self.state.chunks.len(), tasks = tasks.len(), "broadcast lagged, skip events");
                        // P0-C1 轻量对账：若 tasks 已空但 state 仍有残留，说明 Complete/Failed 丢失，可能挂死，下次 tick 将尝试重试
                        if tasks.is_empty() && !self.state.chunks.is_empty() {
                            ::tracing::error!(active_chunks = self.state.chunks.len(), total_lagged = self.lagged_count, "lagged reconciliation: tasks empty but state has chunks, will reconcile on next tick");
                        }
                        // 暴露 lagged_count 到监控流，便于上层观测
                        self.send_monitor_update(&info_tx);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        ::tracing::info!("broadcast closed, monitor exit");
                        break 'main_loop;
                    },
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
                        ::tracing::error!(msg = %msg, "permanent failure after tick");
                        return Err(crate::types::DownloadError::PermanentFailure(msg));
                    }
                },
            }
        }
        ::tracing::info!("monitor all download tasks complete, shutting down");
        Ok(())
    }
    /// 处理从下载块接收到的各种 `DownloadInfo` 消息。
    #[allow(clippy::too_many_arguments)]
    #[::tracing::instrument(skip(self, tasks, next_chunk_id, client, writer_tx, cmd_tx, info_tx, url, multi_runtime), fields(info = ?info))]
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
        multi_runtime: &mut Option<MultiRuntime>,
    ) {
        ::tracing::trace!(
            chunks = self.state.chunks.len(),
            downloaded = self.state.total_downloaded(),
            total = self.state.total_file_size,
            speed_kbs = self.state.total_speed() / 1024.0,
            pending_bisects = self.pending_bisects.len(),
            retry_q = self.retry_handler.retry_queue_len(),
            delayed = self.retry_handler.delayed_queue_len(),
            "handle_download_info before"
        );
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
                ::tracing::info!(chunk_id = id, "chunk DownloadComplete");
                // 标记一个块为已完成
                self.state.complete_chunk(&id);
                if let Some(lane_id) = self.lane_bindings.remove(&id)
                    && let Some(runtime) = multi_runtime
                {
                    runtime.record_success(&lane_id);
                    runtime.release_chunk(&lane_id);
                    ::tracing::debug!(chunk_id = id, lane_id = %lane_id, "lane success+release on complete");
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
                ::tracing::warn!(chunk_id = id, start, end, error = %error, "ChunkFailed");
                if let Some(lane_id) = self.lane_bindings.remove(&id)
                    && let Some(runtime) = multi_runtime
                {
                    runtime.record_failure(&lane_id);
                    runtime.release_chunk(&lane_id);
                    ::tracing::debug!(chunk_id = id, lane_id = %lane_id, "lane failure+release on ChunkFailed");
                }
                // 将失败的块交给重试处理器
                self.retry_handler
                    .on_chunk_failed(id, start, end, error, &mut self.state, info_tx);
            }
            DownloadInfo::ChunkBisected {
                new_start, new_end, original_id
            } => {
                ::tracing::debug!(original_id, new_start, new_end, tasks = tasks.len(), pending = self.pending_bisects.len(), "ChunkBisected");
                // 尝试为新区间分配 lane；若容量不足则缓冲至 pending_bisects，避免丢范围
                let Some((lane_id, rb)) = build_request(client, url, multi_runtime.as_mut()) else {
                    ::tracing::warn!(new_start, new_end, "lane capacity insufficient, buffer bisected range");
                    self.pending_bisects.push_back((new_start, new_end));
                    return;
                };
                let new_id = next_chunk_id.fetch_add(1, Ordering::SeqCst);
                ::tracing::info!(new_id, new_start, new_end, lane_id = ?lane_id.as_ref().map(|s| s.as_str()), "spawn bisected chunk");
                // 限速：解析分源与全局（需在 move 前）
                let per_source = lane_id.as_ref().and_then(|id| multi_runtime.as_ref().and_then(|r| r.limiter_for_lane(id.as_str())));
                let global = multi_runtime.as_ref().and_then(|r| r.global_limiter()).or_else(|| self.global_limiter.clone());
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
                    global,
                    per_source,
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
            ::tracing::trace!("tick skip elapsed<=0");
            return false;
        }

        // 委托状态更新：计算每个块的速度
        for chunk in self.state.chunks.values_mut() {
            let before = chunk.speed;
            chunk.update_speed(elapsed_secs, SMOOTHING_FACTOR);
            ::tracing::trace!(
                chunk_id = chunk.id,
                downloaded = chunk.downloaded_bytes,
                size = chunk.size(),
                before_kbs = before / 1024.0,
                after_kbs = chunk.speed / 1024.0,
                elapsed = elapsed_secs,
                "chunk speed update"
            );
        }
        ::tracing::trace!(
            elapsed_secs,
            downloaded = self.state.total_downloaded(),
            total = self.state.total_file_size,
            speed_kbs = self.state.total_speed() / 1024.0,
            active = self.state.chunks.len(),
            pending_bisects = self.pending_bisects.len(),
            retry_q = self.retry_handler.retry_queue_len(),
            delayed = self.retry_handler.delayed_queue_len(),
            tasks = tasks.len(),
            "monitor tick"
        );
        // 发送聚合后的监控更新
        self.send_monitor_update(info_tx);

        // 委托并发控制：让并发管理器决定是否需要分割块（限速时冻结，避免误判）
        if self.is_rate_limited {
            ::tracing::debug!("rate-limited: skip decide_and_act (freeze adaptive)");
        } else {
            self.concurrency_manager.decide_and_act(&self.state, cmd_tx);
        }

        // 调度之前因 lane 容量不足而缓冲的分割区间
        let mut drained_pending = 0usize;
        while let Some((start, end)) = self.pending_bisects.front().copied() {
            let Some((lane_id, rb)) = build_request(client, url, multi_runtime.as_mut()) else {
                ::tracing::debug!(start, end, remaining = self.pending_bisects.len(), "pending_bisects still blocked");
                break;
            };
            self.pending_bisects.pop_front();
            let new_id = next_chunk_id.fetch_add(1, Ordering::SeqCst);
            ::tracing::info!(new_id, start, end, lane_id = ?lane_id.as_ref().map(|s| s.as_str()), "drain pending_bisect");
            // 限速：解析分源与全局
            let per_source = lane_id.as_ref().and_then(|id| multi_runtime.as_ref().and_then(|r| r.limiter_for_lane(id.as_str())));
            let global = multi_runtime.as_ref().and_then(|r| r.global_limiter()).or_else(|| self.global_limiter.clone());
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
                global,
                per_source,
            );
            tasks.push(tokio::spawn(task));
            drained_pending += 1;
        }
        if drained_pending > 0 {
            ::tracing::debug!(drained = drained_pending, tasks = tasks.len(), "drained pending_bisects");
        }

        // 委托重试处理：处理重试队列
        let before_retry = self.retry_handler.retry_queue_len();
        let before_delayed = self.retry_handler.delayed_queue_len();
        self.retry_handler.process_queues();
        ::tracing::trace!(
            before_retry,
            before_delayed,
            after_retry = self.retry_handler.retry_queue_len(),
            after_delayed = self.retry_handler.delayed_queue_len(),
            "retry process_queues"
        );
        let mut deferred_retries = Vec::new();
        let mut retried = 0usize;
        while let Some(chunk_to_retry) = self.retry_handler.pop_ready_chunk() {
            ::tracing::debug!(
                chunk_id = chunk_to_retry.id,
                start = chunk_to_retry.start,
                end = chunk_to_retry.end,
                attempts = chunk_to_retry.attempts,
                "pop ready retry chunk"
            );
            let Some((lane_id, rb)) = build_request(client, url, multi_runtime.as_mut()) else {
                ::tracing::debug!(chunk_id = chunk_to_retry.id, "lane capacity blocked for retry, deferred");
                deferred_retries.push(chunk_to_retry);
                continue;
            };
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id: chunk_to_retry.id,
                status: 1, // 状态：重试中
                message: Some(format!("正在进行第 {} 次重试", chunk_to_retry.attempts)),
            });
            // 限速：解析分源与全局（需在 move 之前）
            let per_source = lane_id.as_ref().and_then(|id| multi_runtime.as_ref().and_then(|r| r.limiter_for_lane(id.as_str())));
            let global = multi_runtime.as_ref().and_then(|r| r.global_limiter()).or_else(|| self.global_limiter.clone());
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
                global,
                per_source,
            );
            tasks.push(tokio::spawn(task));
            retried += 1;
        }
        if retried > 0 || !deferred_retries.is_empty() {
            ::tracing::debug!(retried, deferred = deferred_retries.len(), remaining_retry = self.retry_handler.retry_queue_len(), "tick retry result");
        }
        for chunk in deferred_retries {
            self.retry_handler.push_back_retry(chunk);
        }

        // 检查下载是否已全部完成
        let done = self.are_all_tasks_done() && self.state.is_download_finished();
        if done {
            ::tracing::info!(
                downloaded = self.state.total_downloaded(),
                total = self.state.total_file_size,
                "monitor tick: all done"
            );
        }
        done
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
