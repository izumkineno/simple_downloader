/// 包含核心的 `MonitorActor` 及其所有权下的状态和逻辑。
///
/// 这个模块是下载引擎的心脏，它作为一个事件中心，
/// 管理着所有下载块（`ChunkActor`）、下载状态、并发策略和重试逻辑。
use crate::resource::{Resource, ResourceHandle};
use crate::types::{ChunkId, DownloadInfo, DownloaderConfig, Result, SystemCommand, SystemEvent};
use bytes::Bytes;
use futures_util::stream::{FuturesUnordered, StreamExt};
use log::{debug, error, info, warn};
use reqwest::RequestBuilder;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::spawn;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::interval;

//==============================================================================================
// 1. Monitor Actor - 系统事件中心
//==============================================================================================

/// `MonitorActor` 是下载系统的核心协调器。
///
/// 它负责：
/// - 维护整个下载任务的状态 (`DownloadState`)。
/// - 根据系统事件更新状态。
/// - 定期计算并广播总下载进度和速度。
/// - 执行并发控制策略，决定何时增加下载线程 (`ConcurrencyManager`)。
/// - 管理失败块的重试逻辑 (`RetryHandler`)。
/// - 派生和监督所有的 `ChunkActor` 任务。
pub(crate) struct MonitorActor {
    /// 维护下载的全局状态，如文件总大小、各块的进度等。
    state: DownloadState,
    /// 处理失败块的重试逻辑。
    retry_handler: RetryHandler,
    /// 动态并发控制器，用于决定何时分裂下载块。
    concurrency_manager: ConcurrencyManager,
    /// 下载器配置。
    config: DownloaderConfig,
    /// 用于为新块分配唯一的 ID。
    next_chunk_id: u64,
    /// 用于获取资源和构建客户端的句柄。
    handle: ResourceHandle,
    /// 追踪每个活动 ChunkActor 正在使用的资源。
    active_resources: HashMap<ChunkId, Resource>,
    // --- 通信 Channel ---
    /// 接收来自 `ChunkActor` 的事件。
    event_rx: mpsc::Receiver<SystemEvent>,
    /// 将事件发送给自己或其他 Actor（例如，用于分裂块）。
    event_tx: mpsc::Sender<SystemEvent>,
    /// 广播系统级命令（如 `BisectDownload`, `TerminateAll`）。
    cmd_tx: broadcast::Sender<SystemCommand>,
    /// 向用户端广播聚合的进度信息。
    info_tx: broadcast::Sender<DownloadInfo>,
    /// 向 `WriterActor` 发送写入命令。
    writer_tx: mpsc::Sender<SystemCommand>,
}

impl MonitorActor {
    /// 创建一个新的 `MonitorActor` 实例。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        total_file_size: u64,
        config: DownloaderConfig,
        handle: ResourceHandle,
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
            handle,
            active_resources: HashMap::new(),
            event_rx,
            cmd_tx,
            info_tx,
            writer_tx,
            event_tx,
        })
    }

    /// 启动 `MonitorActor` 的主事件循环。
    ///
    /// 这个循环会一直运行，直到所有下载任务（包括重试）都完成。
    pub(crate) async fn run(mut self) {
        info!("[MonitorActor] 正在运行。");
        // `FuturesUnordered` 用于管理所有 `ChunkActor` 的 `JoinHandle`。
        let mut tasks = FuturesUnordered::<JoinHandle<()>>::new();
        let mut ticker = interval(Duration::from_secs_f64(self.config.update_interval));
        let mut last_tick_time = Instant::now();

        // 初始时，派生一个覆盖整个文件范围的下载块。
        self.spawn_chunk_actor(0, self.state.total_file_size.saturating_sub(1), &mut tasks)
            .await;

        'main_loop: loop {
            tokio::select! {
                // `biased` 优先处理已完成的任务和事件，而不是定时器，这能更快地响应系统变化。
                biased;

                // 监听是否有 `ChunkActor` 任务完成。
                Some(result) = tasks.next() => {
                    if let Err(e) = result { error!("[MonitorActor] 一个下载任务 panicked: {e}"); }
                    // 如果所有派生的任务都已结束，并且没有待处理的任务（如重试），则退出主循环。
                    if tasks.is_empty() && self.are_all_tasks_done() {
                        break 'main_loop;
                    }
                },
                // 监听来自 `ChunkActor` 的系统事件。
                Some(event) = self.event_rx.recv() => {
                    self.handle_system_event(event, &mut tasks).await;
                },
                // 定时器事件，用于周期性地更新进度、执行并发控制和重试逻辑。
                _ = ticker.tick() => {
                    let elapsed_secs = (Instant::now() - last_tick_time).as_secs_f64();
                    last_tick_time = Instant::now();
                    // 如果 `handle_tick` 返回 true，表示下载已完成。
                    if self.handle_tick(elapsed_secs, &mut tasks).await {
                        break 'main_loop;
                    }
                },
                // 如果所有信道都关闭，则退出。
                else => break,
            }
        }
        info!("[MonitorActor] 所有下载任务已完成，正在关闭。");
    }

    /// 处理来自其他 Actor 的系统事件。
    async fn handle_system_event(
        &mut self,
        event: SystemEvent,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
    ) {
        match event {
            // 更新一个块的下载进度。
            SystemEvent::ChunkProgress { id, downloaded } => {
                if let Some(chunk) = self.state.chunks.get_mut(&id) {
                    chunk.update_downloaded(downloaded);
                }
            }
            // 一个块已成功下载完成。
            SystemEvent::ChunkCompleted { id } => {
                self.state.complete_chunk(&id);
                self.retry_handler.on_download_complete(&id);
                // 移除对已完成块资源的追踪。
                self.active_resources.remove(&id);
            }
            // 一个块下载失败。
            SystemEvent::ChunkFailed {
                id,
                start,
                end,
                error,
            } => {
                // 报告资源失败，以便降低其权重。
                if let Some(resource) = self.active_resources.remove(&id) {
                    self.handle.report_failure(resource).await;
                }
                self.retry_handler.on_chunk_failed(
                    id,
                    start,
                    end,
                    error,
                    &mut self.state,
                    &self.info_tx,
                );
            }
            // 一个块被分裂成两部分。
            SystemEvent::ChunkBisected {
                original_id,
                new_start,
                new_end,
            } => {
                // 更新原始块的结束字节位置，使其只负责前半部分。
                if let Some(chunk) = self.state.chunks.get_mut(&original_id) {
                    chunk.update_end_byte(new_start.saturating_sub(1));
                }
                // 派生新的 `ChunkActor` 负责后半部分的下载。
                self.spawn_chunk_actor(new_start, new_end, tasks).await;
                debug!(
                    "[MonitorActor] 分片块 {} 成为 {}-{}",
                    original_id, new_start, new_end
                );
            }
        }
    }

    /// 处理定时器事件，用于更新进度、执行并发控制和重试逻辑。
    /// 如果下载完成，则返回 `true`。
    async fn handle_tick(
        &mut self,
        elapsed_secs: f64,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
    ) -> bool {
        if elapsed_secs <= 0.0 {
            return false;
        }

        // 更新每个块的速度并向用户发送聚合的进度更新。
        for chunk in self.state.chunks.values_mut() {
            chunk.update_speed(elapsed_secs, self.config.speed_smoothing_factor);
        }
        self.send_monitor_update();

        // 执行并发控制和重试逻辑。
        self.concurrency_manager
            .decide_and_act(&self.state, &self.cmd_tx);
        self.retry_handler.process_queues();

        // 派生准备好重试的块。
        while let Some(chunk_to_retry) = self.retry_handler.pop_ready_chunk() {
            self.spawn_chunk_actor(chunk_to_retry.start, chunk_to_retry.end, tasks)
                .await;
        }

        // 如果所有任务都已完成且下载已结束，则返回 true 以退出主循环。
        self.are_all_tasks_done() && self.state.is_download_finished()
    }

    /// 派生一个新的 `ChunkActor` 任务来下载文件的指定范围。
    async fn spawn_chunk_actor(
        &mut self,
        start: u64,
        end: u64,
        tasks: &mut FuturesUnordered<JoinHandle<()>>,
    ) {
        // 从资源管理器获取一个配置好的 RequestBuilder 和其使用的资源。
        if let Some((rb, resource)) = self.handle.get_request_builder().await {
            let id = self.next_chunk_id;
            self.next_chunk_id += 1;

            info!(
                "[MonitorActor] 派生新 ChunkActor (ID: {}), 资源: {:?}, 范围: {}-{}",
                id, resource, start, end
            );
            // 在状态中注册这个新块。
            self.state
                .chunks
                .insert(id, ChunkState::new(id, start, end));
            // 追踪此块使用的资源。
            self.active_resources.insert(id, resource);

            // 创建并派生任务。
            let task = chunk_actor_task(
                id,
                self.event_tx.clone(),
                self.writer_tx.clone(),
                self.cmd_tx.subscribe(), // 每个 `ChunkActor` 都订阅命令广播。
                rb,
                start,
                end,
                self.config.min_chunk_size_for_split,
            );
            tasks.push(spawn(task));
        } else {
            error!(
                "[MonitorActor] 无法从资源池获取资源来下载范围 {}-{}。下载可能会暂停。",
                start, end
            );
            // 如果无法获取资源，将块信息放回重试队列，以便稍后再次尝试。
            self.retry_handler.on_chunk_failed(
                self.next_chunk_id, // 使用一个临时的ID
                start,
                end,
                "无法获取可用资源".to_string(),
                &mut self.state, // 传递状态以保持一致性
                &self.info_tx,
            );
            // 注意：因为没有实际创建块，所以这个失败的块不会存在于 state.chunks 中，
            // on_chunk_failed 中的 state.chunks.remove(&id) 会无操作，这是安全的。
        }
    }

    /// 向用户端发送聚合的进度更新。
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

    /// 检查是否所有任务（包括正在运行的和等待重试的）都已处理完毕。
    fn are_all_tasks_done(&self) -> bool {
        self.state.chunks.is_empty() && self.retry_handler.are_all_tasks_done()
    }
}

//==============================================================================================
// 2. Chunk Actor - 下载工作单元
//==============================================================================================

/// `ChunkActor` 的异步任务函数，负责下载文件的特定范围。
///
/// # 参数
/// * `id`: 此块的唯一标识符。
/// * `event_tx`: 用于向 `MonitorActor` 发送事件（如进度、完成、失败）的信道。
/// * `writer_tx`: 用于向 `WriterActor` 发送写入命令的信道。
/// * `cmd_rx`: 用于接收来自 `MonitorActor` 的命令（如分裂、终止）的广播接收器。
/// * `rb`: `reqwest::RequestBuilder`，预配置了 URL 和客户端。
/// * `start_byte`, `end_byte`: 此块负责下载的字节范围（包含两端）。
/// * `min_chunk_size_for_split`: 块可以被分裂的最小尺寸。
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
    let mut end = end_byte; // 结束字节可能会因分裂而改变。
    let mut offset = start_byte; // 当前下载的偏移量。
    let mut failed = false;

    // 发送带 Range 头的 HTTP 请求。
    let range_header = format!("bytes={start_byte}-{end_byte}");
    let response = match rb
        .header("Range", range_header)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(resp) => resp,
        Err(e) => {
            // 如果请求初始失败，则向 Monitor 报告失败。
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

    // 获取响应的字节流。
    let mut stream = response.bytes_stream();

    loop {
        tokio::select! {
            // 优先处理命令，以快速响应分裂或终止请求。
            biased;
            // 接收命令。
            Ok(cmd) = cmd_rx.recv() => {
                match cmd {
                    // 如果收到针对此块的分裂命令。
                    SystemCommand::BisectDownload { id: target_id } if id == target_id => {
                        debug!("[ChunkActor] 收到块 {} 的分片命令。", id);
                        let remaining = end.saturating_sub(offset);
                        // 只有当剩余部分足够大时才执行分裂。
                        if remaining >= min_chunk_size_for_split * 2 {
                            let midpoint = offset + remaining / 2;
                            let event = SystemEvent::ChunkBisected { original_id: id, new_start: midpoint + 1, new_end: end };
                            // 通知 Monitor 分裂事件，让它创建新任务。
                            if event_tx.send(event).await.is_ok() {
                                end = midpoint; // 更新当前块的结束边界。
                            } else {
                                error!("[ChunkActor] 事件信道已关闭，无法为块 {} 进行分片。", id);
                            }
                        }
                    }
                    // 收到全局终止命令。
                    SystemCommand::TerminateAll => {
                        info!("[ChunkActor] 收到块 {} 的 TerminateAll 命令，正在关闭。", id);
                        break;
                    }
                    _ => {}
                }
            },
            // 从 TCP 流中接收数据块。
            chunk_result = stream.next() => {
                match chunk_result {
                    Some(Ok(mut chunk)) => {
                        if offset > end { break; }
                        let allowed = end.saturating_sub(offset).saturating_add(1);
                        if allowed == 0 { break; }
                        let len = chunk.len();
                        let write_len = std::cmp::min(allowed, len as u64);
                        let to_write: Bytes = if write_len as usize == len { chunk } else { chunk.split_to(write_len as usize) };

                        // 将数据块发送给 WriterActor 进行写入。
                        let write_cmd = SystemCommand::WriteFile { offset, data: to_write };
                        if writer_tx.send(write_cmd).await.is_err() {
                            let event = SystemEvent::ChunkFailed { id, start: offset, end, error: "文件写入信道已关闭".to_string() };
                            if event_tx.send(event).await.is_err() {
                                error!("[ChunkActor] 事件信道已关闭，无法为块 {} 报告写入失败。", id);
                            }
                            failed = true;
                            break;
                        }
                        // 更新偏移量并报告进度。
                        offset = offset.saturating_add(write_len);
                        let progress_event = SystemEvent::ChunkProgress { id, downloaded: offset.saturating_sub(start_byte) };
                        if event_tx.send(progress_event).await.is_err() {
                            error!("[ChunkActor] 事件信道已关闭，无法为块 {} 报告进度。任务终止。", id);
                            failed = true; // 无法更新状态，视为失败。
                            break;
                        }
                        // 如果服务器发送了超出范围的数据，则停止。
                        if write_len < len as u64 { break; }
                    }
                    // 流发生错误。
                    Some(Err(e)) => {
                        let event = SystemEvent::ChunkFailed { id, start: offset, end, error: e.to_string() };
                        if event_tx.send(event).await.is_err() {
                            error!("[ChunkActor] 事件信道已关闭，无法为块 {} 报告流错误。", id);
                        }
                        failed = true;
                        break;
                    },
                    // 流结束。
                    None => break,
                }
            },
            else => break,
        }
    }

    // 如果没有发生失败，则报告完成。
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

/// 维护单个下载块的状态。
#[derive(Debug, Clone)]
struct ChunkState {
    id: ChunkId,
    start_byte: u64,
    end_byte: u64,
    downloaded_bytes: u64,
    /// 上一次采样时的已下载字节数，用于计算瞬时速度。
    last_sampled_bytes: u64,
    /// 当前块的下载速度（字节/秒），使用指数移动平均平滑。
    speed: f64,
    /// 状态码 (0=下载中, 2=等待重试, 3=延迟重试)。
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
            status: 0, // 初始状态为下载中
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
    fn size_remaining(&self) -> u64 {
        self.end_byte
            .saturating_sub(self.start_byte)
            .saturating_add(1)
            .saturating_sub(self.downloaded_bytes)
    }
    /// 使用指数移动平均 (EMA) 更新速度。
    fn update_speed(&mut self, elapsed_secs: f64, smoothing_factor: f64) {
        let newly_downloaded = self
            .downloaded_bytes
            .saturating_sub(self.last_sampled_bytes);
        let instantaneous_speed = newly_downloaded as f64 / elapsed_secs;
        // 如果速度为 0，直接使用瞬时速度；否则，应用平滑算法。
        self.speed = if self.speed == 0.0 {
            instantaneous_speed
        } else {
            (instantaneous_speed * smoothing_factor) + (self.speed * (1.0 - smoothing_factor))
        };
        self.last_sampled_bytes = self.downloaded_bytes;
    }
}

/// 维护整个下载任务的全局状态。
struct DownloadState {
    total_file_size: u64,
    /// 所有当前正在活动的下载块的状态。
    chunks: HashMap<ChunkId, ChunkState>,
    /// 已完成并从 `chunks` map 中移除的块的总字节数。
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
    /// 标记一个块为完成，并将其字节数累加到 `completed_bytes`。
    fn complete_chunk(&mut self, id: &ChunkId) {
        if let Some(chunk) = self.chunks.remove(id) {
            self.completed_bytes += chunk.downloaded_bytes;
        }
    }
    /// 计算当前已下载的总字节数。
    fn total_downloaded(&self) -> u64 {
        self.completed_bytes
            + self
                .chunks
                .values()
                .map(|c| c.downloaded_bytes)
                .sum::<u64>()
    }
    /// 计算所有活动块的总下载速度。
    fn total_speed(&self) -> f64 {
        self.chunks.values().map(|c| c.speed).sum::<f64>()
    }
    /// 检查下载是否已完成。
    fn is_download_finished(&self) -> bool {
        self.total_downloaded() >= self.total_file_size
    }
}

//==============================================================================================
// 4. Concurrency Management - 动态并发控制器
//==============================================================================================

/// 动态并发控制器，用于智能地调整并发下载的线程数。
struct ConcurrencyManager {
    max_workers: u64,
    phase: DownloadPhase,
    /// 记录的历史最高速度。
    max_speed: f64,
    last_split_time: Instant,
    /// 用于计算平均速度的样本队列。
    stable_speed_samples: VecDeque<f64>,
    /// 两次分裂决策之间的最小延迟。
    split_delay: Duration,
}

/// 下载过程的阶段。
#[derive(Debug, PartialEq, Clone, Copy)]
enum DownloadPhase {
    /// 探测阶段：快速增加并发数，以找到速度峰值。
    Probing,
    /// 稳定阶段：在达到并发上限或速度不再显著增加后，进行保守的优化。
    Stable,
}

impl ConcurrencyManager {
    fn new(max_workers: u64, split_delay: Duration) -> Self {
        Self {
            max_workers,
            // 如果最大并发为 1，则无需探测。
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

    /// 根据当前状态决定是否要分裂块以增加并发。
    fn decide_and_act(&mut self, state: &DownloadState, cmd_tx: &broadcast::Sender<SystemCommand>) {
        // 避免过于频繁地分裂。
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

    /// 处理探测阶段的逻辑（使用指数分裂策略）。
    fn handle_probing_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        cmd_tx: &broadcast::Sender<SystemCommand>,
    ) {
        let active_chunks = state.chunks.len() as u64;
        // 如果已达到最大并发，则切换到稳定阶段。
        if active_chunks >= self.max_workers {
            self.transition_to_stable();
            return;
        }
        // 如果速度显著提升，则对所有块进行指数分裂以快速增加并发。
        if avg_speed > self.max_speed * 1.2 || self.max_speed == 0.0 {
            self.max_speed = avg_speed;
            // 收集所有可分裂的块ID
            let chunks_to_split: Vec<ChunkId> = state.chunks.values().map(|c| c.id).collect();
            // 对每个块发送分裂命令，实现指数增长
            for chunk_id in chunks_to_split {
                if active_chunks * 2 > self.max_workers {
                    break; // 避免超过最大并发数
                }
                self.request_split(chunk_id, cmd_tx);
            }
        } else if active_chunks > 1 && self.stable_speed_samples.len() >= 3 {
            // 如果速度不再提升，则切换到稳定阶段。
            self.transition_to_stable();
        }
    }

    /// 处理稳定阶段的逻辑。
    fn handle_stable_phase(
        &mut self,
        state: &DownloadState,
        avg_speed: f64,
        estimated_time: f64,
        cmd_tx: &broadcast::Sender<SystemCommand>,
    ) {
        // 如果速度波动较大且剩余下载时间较长，尝试分裂最慢的块。
        if self.stable_speed_samples.len() > 3
            && (avg_speed > self.max_speed * 1.2 || avg_speed < self.max_speed * 0.8)
            && estimated_time > 5.0
            && (state.chunks.len() as u64) < self.max_workers
        {
            if let Some(slowest_chunk) = state
                .chunks
                .values()
                // 确保块足够大以便分裂
                .filter(|c| c.size_remaining() > 1024 * 20)
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

    /// 反应式分裂：如果并发数低于最大值，主动分裂最大的块。
    fn reactive_split(&mut self, state: &DownloadState, cmd_tx: &broadcast::Sender<SystemCommand>) {
        if self.phase == DownloadPhase::Stable && self.last_split_time.elapsed() > self.split_delay
        {
            let active_chunks = state.chunks.len() as u64;
            if active_chunks > 0 && active_chunks < self.max_workers {
                if let Some(largest_chunk) =
                    state.chunks.values().max_by_key(|c| c.size_remaining())
                {
                    self.request_split(largest_chunk.id, cmd_tx);
                }
            }
        }
    }

    /// 发送分裂命令。
    fn request_split(&mut self, id: ChunkId, cmd_tx: &broadcast::Sender<SystemCommand>) {
        let _ = cmd_tx.send(SystemCommand::BisectDownload { id });
        self.last_split_time = Instant::now();
    }

    /// 切换到稳定阶段。
    fn transition_to_stable(&mut self) {
        self.phase = DownloadPhase::Stable;
        self.stable_speed_samples.clear();
    }
}

//==============================================================================================
// 5. Retry Management - 重试处理器
//==============================================================================================

/// 负责管理失败块的重试逻辑。
struct RetryHandler {
    /// 等待立即重试的队列。
    retry_queue: VecDeque<FailedChunkInfo>,
    /// 等待较长延迟后重试的队列。
    delayed_retry_queue: VecDeque<DelayedChunkInfo>,
    /// 记录每个块的重试次数。
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

    /// 当一个块下载失败时调用此方法。
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
        // 从活动块列表中移除。
        state.chunks.remove(&id);
        let attempts = self.retry_attempts.entry(id).or_insert(0);
        *attempts += 1;

        if *attempts <= self.max_retries {
            // 如果未超过最大立即重试次数，则加入立即重试队列。
            let msg = DownloadInfo::ChunkStatusChanged {
                id,
                status: 2, // 等待重试
                message: Some(format!(
                    "将进行第 {}/{} 次重试",
                    *attempts, self.max_retries
                )),
            };
            let _ = info_tx.send(msg);
            self.retry_queue.push_back(FailedChunkInfo {
                start,
                end,
                failure_time: Instant::now(),
                attempts: *attempts,
            });
        } else {
            // 否则，加入延迟重试队列。
            let msg = DownloadInfo::ChunkStatusChanged {
                id,
                status: 3, // 延迟重试
                message: Some(format!(
                    "超过最大重试次数，将在 {:?} 后再次尝试",
                    self.long_delay
                )),
            };
            let _ = info_tx.send(msg);
            self.delayed_retry_queue.push_back(DelayedChunkInfo {
                chunk: FailedChunkInfo {
                    start,
                    end,
                    failure_time: Instant::now(),
                    attempts: *attempts,
                },
                retry_at: Instant::now() + self.long_delay,
            });
            // 重置计数器，以便从延迟队列出来后可以再次进行立即重试。
            self.retry_attempts.remove(&id);
        }
    }

    /// 处理延迟队列，将到期的任务移至立即重试队列。
    fn process_queues(&mut self) {
        let now = Instant::now();
        while let Some(delayed_info) = self.delayed_retry_queue.front() {
            if now >= delayed_info.retry_at {
                let mut info_to_retry = self.delayed_retry_queue.pop_front().unwrap().chunk;
                info_to_retry.failure_time = Instant::now();
                info_to_retry.attempts = 0; // 重置尝试次数。
                self.retry_queue.push_back(info_to_retry);
            } else {
                break;
            }
        }
    }

    /// 从立即重试队列中取出一个已达到重试延迟的块。
    fn pop_ready_chunk(&mut self) -> Option<FailedChunkInfo> {
        if let Some(failed_chunk) = self.retry_queue.front() {
            if failed_chunk.failure_time.elapsed() >= self.initial_delay {
                return self.retry_queue.pop_front();
            }
        }
        None
    }

    /// 当一个块最终成功完成时，清除其重试记录。
    fn on_download_complete(&mut self, id: &ChunkId) {
        self.retry_attempts.remove(id);
    }

    /// 检查是否所有重试任务都已处理完毕。
    fn are_all_tasks_done(&self) -> bool {
        self.retry_queue.is_empty() && self.delayed_retry_queue.is_empty()
    }
}
