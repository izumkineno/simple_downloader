//! 包含核心的 MonitorActor 及其所有权下的状态和逻辑。
//!
//! 这个模块是下载引擎的心脏，它作为一个事件中心，
//! 管理着所有下载块（ChunkActor）、下载状态、并发策略和重试逻辑。

use crate::types::{ChunkId, DownloadInfo, DownloaderConfig, Result, SystemCommand, SystemEvent};
use bytes::Bytes;
use faststr::FastStr;
use futures_util::stream::{FuturesUnordered, StreamExt};
use log::{debug, error, info, warn};
use reqwest::{Client, ClientBuilder, RequestBuilder};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::interval;

//==============================================================================================
// 1. Monitor Actor - 系统事件中心
//==============================================================================================

pub(crate) struct MonitorActor<F>
where
    F: Fn() -> ClientBuilder,
{
    state: DownloadState,
    retry_handler: RetryHandler,
    concurrency_manager: ConcurrencyManager,
    config: DownloaderConfig,
    next_chunk_id: u64,
    client_builder: F,
    url: FastStr,
    // 通信 Channel
    event_rx: mpsc::Receiver<SystemEvent>,
    event_tx: mpsc::Sender<SystemEvent>,
    cmd_tx: broadcast::Sender<SystemCommand>,
    info_tx: broadcast::Sender<DownloadInfo>,
    writer_tx: mpsc::Sender<SystemCommand>,
}

impl<F> MonitorActor<F>
where
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        total_file_size: u64,
        config: DownloaderConfig,
        client_builder: F,
        url: FastStr,
        event_rx: mpsc::Receiver<SystemEvent>,
        cmd_tx: broadcast::Sender<SystemCommand>,
        info_tx: broadcast::Sender<DownloadInfo>,
        writer_tx: mpsc::Sender<SystemCommand>,
        event_tx: mpsc::Sender<SystemEvent>,
    ) -> Result<Self> {
        let retry_handler = RetryHandler::new(
            config.max_immediate_retries,
            config.initial_retry_delay,
            config.long_retry_delay,
        );
        let concurrency_manager =
            ConcurrencyManager::new(config.workers, config.concurrency_split_delay);

        Ok(Self {
            state: DownloadState::new(total_file_size),
            retry_handler,
            concurrency_manager,
            config,
            next_chunk_id: 0,
            client_builder,
            url,
            event_rx,
            cmd_tx,
            info_tx,
            writer_tx,
            event_tx,
        })
    }

    /// 启动 MonitorActor 的主事件循环。
    pub(crate) async fn run(mut self) {
        info!("[MonitorActor] 正在运行。");
        let mut tasks = FuturesUnordered::<JoinHandle<()>>::new();
        let mut ticker = interval(Duration::from_secs_f64(self.config.update_interval));
        let mut last_tick_time = Instant::now();
        // 在 Actor 内部构建 Client，确保它与 Actor 的生命周期绑定
        let client = (self.client_builder)()
            .build()
            .expect("在 MonitorActor 中构建 Client 失败");

        // 派生第一个下载块，覆盖整个文件范围
        self.spawn_chunk_actor(
            0,
            self.state.total_file_size.saturating_sub(1),
            &client,
            &mut tasks,
        );

        'main_loop: loop {
            tokio::select! {
                biased; // 优先处理已完成的任务和事件，而不是定时器
                Some(result) = tasks.next() => {
                    if let Err(e) = result { error!("[MonitorActor] 一个下载任务 panicked: {e}"); }
                    if tasks.is_empty() && self.are_all_tasks_done() {
                        break 'main_loop;
                    }
                },
                Some(event) = self.event_rx.recv() => {
                    self.handle_system_event(event, &client, &mut tasks);
                },
                _ = ticker.tick() => {
                    let elapsed_secs = (Instant::now() - last_tick_time).as_secs_f64();
                    last_tick_time = Instant::now();
                    if self.handle_tick(elapsed_secs, &client, &mut tasks) {
                        break 'main_loop;
                    }
                },
                else => break,
            }
        }
        info!("[MonitorActor] 所有下载任务已完成，正在关闭。");
    }

    /// 处理来自其他 Actor 的系统事件。
    fn handle_system_event(
        &mut self,
        event: SystemEvent,
        client: &Client,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
    ) {
        match event {
            SystemEvent::ChunkProgress { id, downloaded } => {
                if let Some(chunk) = self.state.chunks.get_mut(&id) {
                    chunk.update_downloaded(downloaded);
                }
            }
            SystemEvent::ChunkCompleted { id } => {
                self.state.complete_chunk(&id);
                self.retry_handler.on_download_complete(&id);
            }
            SystemEvent::ChunkFailed {
                id,
                start,
                end,
                error,
            } => {
                self.retry_handler.on_chunk_failed(
                    id,
                    start,
                    end,
                    error,
                    &mut self.state,
                    &self.info_tx,
                );
            }
            SystemEvent::ChunkBisected {
                original_id,
                new_start,
                new_end,
            } => {
                // 更新原始块的结束字节位置，使其只负责前半部分
                if let Some(chunk) = self.state.chunks.get_mut(&original_id) {
                    chunk.update_end_byte(new_start.saturating_sub(1));
                }
                // 派生新的 ChunkActor 负责后半部分的下载
                self.spawn_chunk_actor(new_start, new_end, client, tasks);
                debug!(
                    "[MonitorActor] 分片块 {} 成为 {}-{}",
                    original_id, new_start, new_end
                );
            }
        }
    }

    /// 处理定时器事件，用于更新进度、执行并发控制和重试逻辑。
    fn handle_tick(
        &mut self,
        elapsed_secs: f64,
        client: &Client,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
    ) -> bool {
        if elapsed_secs <= 0.0 {
            return false;
        }

        for chunk in self.state.chunks.values_mut() {
            chunk.update_speed(elapsed_secs, self.config.speed_smoothing_factor);
        }
        self.send_monitor_update();
        self.concurrency_manager
            .decide_and_act(&self.state, &self.cmd_tx);
        self.retry_handler.process_queues();
        while let Some(chunk_to_retry) = self.retry_handler.pop_ready_chunk() {
            self.spawn_chunk_actor(chunk_to_retry.start, chunk_to_retry.end, client, tasks);
        }

        // 如果所有任务都已完成且下载已结束，则返回 true 以退出主循环
        self.are_all_tasks_done() && self.state.is_download_finished()
    }

    /// 派生一个新的 ChunkActor 任务。
    fn spawn_chunk_actor(
        &mut self,
        start: u64,
        end: u64,
        client: &Client,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
    ) {
        let id = self.next_chunk_id;
        self.next_chunk_id += 1;

        info!(
            "[MonitorActor] 派生新 ChunkActor (ID: {}), 范围: {}-{}",
            id, start, end
        );
        self.state
            .chunks
            .insert(id, ChunkState::new(id, start, end));
        let task = chunk_actor_task(
            id,
            self.event_tx.clone(),
            self.writer_tx.clone(),
            self.cmd_tx.subscribe(),
            client.get(self.url.as_str()),
            start,
            end,
            self.config.min_chunk_size_for_split,
        );
        tasks.push(spawn(task));
    }

    /// 向用户发送聚合的进度更新。
    fn send_monitor_update(&self) {
        let info = DownloadInfo::MonitorUpdate {
            total_size: self.state.total_file_size,
            total_downloaded: self.state.total_downloaded(),
            total_speed: self.state.total_speed(),
            chunk_details: self
                .state
                .chunks
                .values()
                .map(|c| (c.id, c.size(), c.downloaded_bytes, c.speed, c.status))
                .collect(),
        };
        // 如果发送失败，说明用户端的接收者已关闭，这不是一个致命错误。
        if self.info_tx.send(info).is_err() {
            debug!("[MonitorActor] 进度信息接收者已关闭，无法发送更新。");
        }
    }

    /// 检查是否所有任务（包括等待重试的）都已处理完毕。
    fn are_all_tasks_done(&self) -> bool {
        self.state.chunks.is_empty() && self.retry_handler.are_all_tasks_done()
    }
}

//==============================================================================================
// 2. Chunk Actor - 下载工作单元
//==============================================================================================

async fn chunk_actor_task(
    id: ChunkId,
    event_tx: mpsc::Sender<SystemEvent>,
    writer_tx: mpsc::Sender<SystemCommand>,
    mut cmd_rx: broadcast::Receiver<SystemCommand>,
    rb: RequestBuilder,
    start_byte: u64,
    end_byte: u64,
    min_chunk_size_for_split: u64,
) {
    let mut end = end_byte;
    let mut offset = start_byte;
    let mut failed = false;

    let range_header = format!("bytes={start_byte}-{end_byte}");
    let response = match rb
        .header("Range", range_header)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(resp) => resp,
        Err(e) => {
            let event = SystemEvent::ChunkFailed {
                id,
                start: start_byte,
                end,
                error: e.to_string(),
            };
            if event_tx.send(event).await.is_err() {
                error!(
                    "[ChunkActor] 事件信道已关闭，无法为块 {} 报告初始失败。",
                    id
                );
            }
            return;
        }
    };

    let mut stream = response.bytes_stream();

    loop {
        tokio::select! {
            biased;
            Ok(cmd) = cmd_rx.recv() => {
                match cmd {
                    SystemCommand::BisectDownload { id: target_id } if id == target_id => {
                        debug!("[ChunkActor] 收到块 {} 的分片命令。", id);
                        let remaining = end.saturating_sub(offset);
                        if remaining >= min_chunk_size_for_split * 2 {
                            let midpoint = offset + remaining / 2;
                            let event = SystemEvent::ChunkBisected { original_id: id, new_start: midpoint + 1, new_end: end };
                            if event_tx.send(event).await.is_ok() {
                                end = midpoint; // 更新当前块的结束边界
                            } else {
                                error!("[ChunkActor] 事件信道已关闭，无法为块 {} 进行分片。", id);
                            }
                        }
                    }
                    SystemCommand::TerminateAll => {
                        info!("[ChunkActor] 收到块 {} 的 TerminateAll 命令，正在关闭。", id);
                        break;
                    }
                    _ => {}
                }
            },
            chunk_result = stream.next() => {
                match chunk_result {
                    Some(Ok(mut chunk)) => {
                        if offset > end { break; }
                        let allowed = end.saturating_sub(offset).saturating_add(1);
                        if allowed == 0 { break; }
                        let len = chunk.len();
                        let write_len = std::cmp::min(allowed, len as u64);
                        let to_write: Bytes = if write_len as usize == len { chunk } else { chunk.split_to(write_len as usize) };

                        let write_cmd = SystemCommand::WriteFile { offset, data: to_write };
                        if writer_tx.send(write_cmd).await.is_err() {
                            let event = SystemEvent::ChunkFailed { id, start: offset, end, error: "文件写入信道已关闭".to_string() };
                            if event_tx.send(event).await.is_err() {
                                error!("[ChunkActor] 事件信道已关闭，无法为块 {} 报告写入失败。", id);
                            }
                            failed = true;
                            break;
                        }
                        offset = offset.saturating_add(write_len);
                        let progress_event = SystemEvent::ChunkProgress { id, downloaded: offset.saturating_sub(start_byte) };
                        if event_tx.send(progress_event).await.is_err() {
                            error!("[ChunkActor] 事件信道已关闭，无法为块 {} 报告进度。任务终止。", id);
                            failed = true; // 无法更新状态，视为失败
                            break;
                        }
                        if write_len < len as u64 { break; }
                    }
                    Some(Err(e)) => {
                        let event = SystemEvent::ChunkFailed { id, start: offset, end, error: e.to_string() };
                        if event_tx.send(event).await.is_err() {
                            error!("[ChunkActor] 事件信道已关闭，无法为块 {} 报告流错误。", id);
                        }
                        failed = true;
                        break;
                    },
                    None => break, // 流结束
                }
            },
            else => break,
        }
    }

    if !failed {
        if event_tx
            .send(SystemEvent::ChunkCompleted { id })
            .await
            .is_err()
        {
            error!(
                "[ChunkActor] 事件信道已关闭，无法为块 {} 报告完成状态。",
                id
            );
        }
    }
}

//==============================================================================================
// 3. State Management - 状态机定义
//==============================================================================================

#[derive(Debug, Clone)]
struct ChunkState {
    id: ChunkId,
    start_byte: u64,
    end_byte: u64,
    downloaded_bytes: u64,
    last_sampled_bytes: u64,
    speed: f64,
    status: u8,
}

impl ChunkState {
    fn new(id: ChunkId, start_byte: u64, end_byte: u64) -> Self {
        Self {
            id,
            start_byte,
            end_byte,
            downloaded_bytes: 0,
            last_sampled_bytes: 0,
            speed: 0.0,
            status: 0,
        }
    }
    fn update_downloaded(&mut self, downloaded_bytes: u64) {
        self.downloaded_bytes = downloaded_bytes;
    }
    fn update_end_byte(&mut self, end_byte: u64) {
        self.end_byte = end_byte;
    }
    fn size(&self) -> u64 {
        self.end_byte.saturating_sub(self.start_byte) + 1
    }
    fn update_speed(&mut self, elapsed_secs: f64, smoothing_factor: f64) {
        let newly_downloaded = self
            .downloaded_bytes
            .saturating_sub(self.last_sampled_bytes);
        let instantaneous_speed = newly_downloaded as f64 / elapsed_secs;
        self.speed = if self.speed == 0.0 {
            instantaneous_speed
        } else {
            (instantaneous_speed * smoothing_factor) + (self.speed * (1.0 - smoothing_factor))
        };
        self.last_sampled_bytes = self.downloaded_bytes;
    }
}

struct DownloadState {
    total_file_size: u64,
    chunks: HashMap<ChunkId, ChunkState>,
    completed_bytes: u64,
}

impl DownloadState {
    fn new(total_file_size: u64) -> Self {
        Self {
            total_file_size,
            chunks: HashMap::new(),
            completed_bytes: 0,
        }
    }
    fn complete_chunk(&mut self, id: &ChunkId) {
        if let Some(chunk) = self.chunks.remove(id) {
            self.completed_bytes += chunk.downloaded_bytes;
        }
    }
    fn total_downloaded(&self) -> u64 {
        self.completed_bytes
            + self
                .chunks
                .values()
                .map(|c| c.downloaded_bytes)
                .sum::<u64>()
    }
    fn total_speed(&self) -> f64 {
        self.chunks.values().map(|c| c.speed).sum::<f64>()
    }
    fn is_download_finished(&self) -> bool {
        self.total_downloaded() >= self.total_file_size
    }
}

//==============================================================================================
// 4. Concurrency Management - 动态并发控制器
//==============================================================================================

struct ConcurrencyManager {
    max_workers: u64,
    phase: DownloadPhase,
    max_speed: f64,
    last_split_time: Instant,
    stable_speed_samples: VecDeque<f64>,
    split_delay: Duration,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum DownloadPhase {
    Probing,
    Stable,
}

impl ConcurrencyManager {
    fn new(max_workers: u64, split_delay: Duration) -> Self {
        Self {
            max_workers,
            phase: if max_workers == 1 {
                DownloadPhase::Stable
            } else {
                DownloadPhase::Probing
            },
            max_speed: 0.0,
            last_split_time: Instant::now(),
            stable_speed_samples: VecDeque::with_capacity(10),
            split_delay,
        }
    }

    fn decide_and_act(&mut self, state: &DownloadState, cmd_tx: &broadcast::Sender<SystemCommand>) {
        if self.last_split_time.elapsed() < self.split_delay {
            return;
        }
        let current_speed = state.total_speed();
        self.stable_speed_samples.push_back(current_speed);
        if self.stable_speed_samples.len() > 5 {
            self.stable_speed_samples.pop_front();
        }
        if self.stable_speed_samples.is_empty() {
            return;
        }
        let avg_speed =
            self.stable_speed_samples.iter().sum::<f64>() / self.stable_speed_samples.len() as f64;
        let remaining_bytes = state
            .total_file_size
            .saturating_sub(state.total_downloaded());
        let estimated_time = if avg_speed > 0.0 {
            remaining_bytes as f64 / avg_speed
        } else {
            f64::MAX
        };
        match self.phase {
            DownloadPhase::Probing => self.handle_probing_phase(state, avg_speed, cmd_tx),
            DownloadPhase::Stable => {
                self.handle_stable_phase(state, avg_speed, estimated_time, cmd_tx)
            }
        }
        self.reactive_split(state, cmd_tx);
    }

    fn handle_probing_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        cmd_tx: &broadcast::Sender<SystemCommand>,
    ) {
        let active_chunks = state.chunks.len() as u64;
        if active_chunks >= self.max_workers {
            self.transition_to_stable();
            return;
        }
        if avg_speed > self.max_speed * 1.2 || self.max_speed == 0.0 {
            self.max_speed = avg_speed;
            if let Some(largest_chunk) = state.chunks.values().max_by_key(|c| c.size()) {
                self.request_split(largest_chunk.id, cmd_tx);
            }
        } else if active_chunks > 1 && self.stable_speed_samples.len() >= 3 {
            self.transition_to_stable();
        }
    }

    fn handle_stable_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        cmd_tx: &broadcast::Sender<SystemCommand>,
    ) {
        if self.stable_speed_samples.len() > 3
            && (avg_speed > self.max_speed * 1.2 || avg_speed < self.max_speed * 0.8)
            && estimated_time > 5.0
            && (state.chunks.len() as u64) < self.max_workers
        {
            if let Some(slowest_chunk) = state
                .chunks
                .values()
                .filter(|c| c.size().saturating_sub(c.downloaded_bytes) > 1024 * 20) // MIN_CHUNK_SIZE * 2
                .min_by(|a, b| {
                    a.speed
                        .partial_cmp(&b.speed)
                        .unwrap_or(core::cmp::Ordering::Equal)
                })
            {
                self.request_split(slowest_chunk.id, cmd_tx);
            }
            if avg_speed < self.max_speed * 0.8 {
                self.stable_speed_samples.clear();
            }
        }
    }

    fn reactive_split(&mut self, state: &DownloadState, cmd_tx: &broadcast::Sender<SystemCommand>) {
        if self.phase == DownloadPhase::Stable && self.last_split_time.elapsed() > self.split_delay
        {
            let active_chunks = state.chunks.len() as u64;
            if active_chunks > 0 && active_chunks < self.max_workers {
                if let Some(largest_chunk) = state.chunks.values().max_by_key(|c| c.size()) {
                    self.request_split(largest_chunk.id, cmd_tx);
                }
            }
        }
    }

    fn request_split(&mut self, id: ChunkId, cmd_tx: &broadcast::Sender<SystemCommand>) {
        let _ = cmd_tx.send(SystemCommand::BisectDownload { id });
        self.last_split_time = Instant::now();
    }

    fn transition_to_stable(&mut self) {
        self.phase = DownloadPhase::Stable;
        self.stable_speed_samples.clear();
    }
}

//==============================================================================================
// 5. Retry Management - 重试处理器
//==============================================================================================

struct RetryHandler {
    retry_queue: VecDeque<FailedChunkInfo>,
    delayed_retry_queue: VecDeque<DelayedChunkInfo>,
    retry_attempts: HashMap<ChunkId, u32>,
    max_retries: u32,
    initial_delay: Duration,
    long_delay: Duration,
}

#[derive(Debug)]
struct FailedChunkInfo {
    start: u64,
    end: u64,
    failure_time: Instant,
    attempts: u32,
}
#[derive(Debug)]
struct DelayedChunkInfo {
    chunk: FailedChunkInfo,
    retry_at: Instant,
}

impl RetryHandler {
    fn new(max_retries: u32, initial_delay: Duration, long_delay: Duration) -> Self {
        Self {
            retry_queue: VecDeque::new(),
            delayed_retry_queue: VecDeque::new(),
            retry_attempts: HashMap::new(),
            max_retries,
            initial_delay,
            long_delay,
        }
    }

    fn on_chunk_failed(
        &mut self,
        id: ChunkId,
        start: u64,
        end: u64,
        error: String,
        state: &mut DownloadState,
        info_tx: &broadcast::Sender<DownloadInfo>,
    ) {
        warn!("[RetryHandler] 收到块 {id} 的失败报告: {error}");
        state.chunks.remove(&id);
        let attempts = self.retry_attempts.entry(id).or_insert(0);
        *attempts += 1;

        if *attempts <= self.max_retries {
            let msg = DownloadInfo::ChunkStatusChanged {
                id,
                status: 2,
                message: Some(format!(
                    "将进行第 {}/{} 次重试",
                    *attempts, self.max_retries
                )),
            };
            if info_tx.send(msg).is_err() {
                debug!("[RetryHandler] 进度信息接收者已关闭，无法发送状态变更。");
            }
            self.retry_queue.push_back(FailedChunkInfo {
                start,
                end,
                failure_time: Instant::now(),
                attempts: *attempts,
            });
        } else {
            let msg = DownloadInfo::ChunkStatusChanged {
                id,
                status: 3,
                message: Some(format!(
                    "超过最大重试次数，将在 {:?} 后再次尝试",
                    self.long_delay
                )),
            };
            if info_tx.send(msg).is_err() {
                debug!("[RetryHandler] 进度信息接收者已关闭，无法发送状态变更。");
            }
            self.delayed_retry_queue.push_back(DelayedChunkInfo {
                chunk: FailedChunkInfo {
                    start,
                    end,
                    failure_time: Instant::now(),
                    attempts: *attempts,
                },
                retry_at: Instant::now() + self.long_delay,
            });
            self.retry_attempts.remove(&id); // 重置计数器，进入延迟队列
        }
    }

    fn process_queues(&mut self) {
        let now = Instant::now();
        while let Some(delayed_info) = self.delayed_retry_queue.front() {
            if now >= delayed_info.retry_at {
                let mut info_to_retry = self.delayed_retry_queue.pop_front().unwrap().chunk;
                info_to_retry.failure_time = Instant::now();
                info_to_retry.attempts = 0; // 重置尝试次数
                self.retry_queue.push_back(info_to_retry);
            } else {
                break;
            }
        }
    }

    fn pop_ready_chunk(&mut self) -> Option<FailedChunkInfo> {
        if let Some(failed_chunk) = self.retry_queue.front() {
            if failed_chunk.failure_time.elapsed() >= self.initial_delay {
                return self.retry_queue.pop_front();
            }
        }
        None
    }

    fn on_download_complete(&mut self, id: &ChunkId) {
        self.retry_attempts.remove(id);
    }
    fn are_all_tasks_done(&self) -> bool {
        self.retry_queue.is_empty() && self.delayed_retry_queue.is_empty()
    }
}
