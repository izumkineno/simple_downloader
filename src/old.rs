

pub use downloader::Downloader;
pub use reqwest;

pub use types_old::{ChunkId, DownloadCmd, DownloadError, DownloadInfo};

pub mod types_old {
    use bytes::Bytes;
    use std::io;
    use thiserror::Error;

    pub type ChunkId = u64;
    pub type Result<T> = std::result::Result<T, DownloadError>;

    #[derive(Debug, Error)]
    pub enum DownloadError {
        #[error("网络请求失败: {0}")]
        Request(#[from] reqwest::Error),
        #[error("文件 I/O 错误: {0}")]
        Io(#[from] io::Error),
        #[error("并发任务执行失败: {0}")]
        Join(#[from] tokio::task::JoinError),
        #[error("无法从服务器响应头中获取文件大小 (Content-Length)")]
        MissingContentLength,
    }

    #[derive(Debug, Clone)]
    pub enum DownloadCmd {
        WriteFile { offset: u64, data: Bytes },
        BisectDownload { id: ChunkId },
        TerminateAll,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Copy)]
    pub enum ChunkStatus {
        Downloading,
        Retrying { attempt: u32 },
        Delayed,
        PermanentlyFailed,
        Completed,
    }

    #[derive(Clone, Debug)]
    pub enum DownloadInfo {
        ChunkProgress {
            id: ChunkId,
            start_byte: u64,
            end_byte: u64,
            downloaded: u64,
        },
        MonitorUpdate {
            total_size: u64,
            total_downloaded: u64,
            total_speed: f64,
            chunk_details: Vec<(ChunkId, u64, u64, f64, ChunkStatus)>,
        },
        ChunkCompleted(ChunkId),
        ChunkFailed {
            id: ChunkId,
            start: u64,
            end: u64,
            error: String,
        },
        ChunkBisected {
            original_id: ChunkId,
            new_start: u64,
            new_end: u64,
        },
        ChunkStatusChanged {
            id: ChunkId,
            status: ChunkStatus,
            message: Option<String>,
        },
    }
}

pub mod state {
    use crate::old::types_old::{ChunkId, ChunkStatus};
    use std::collections::HashMap;
    use std::time::Instant;

    #[derive(Debug, Clone)]
    pub struct ChunkState {
        pub id: ChunkId,
        pub start_byte: u64,
        pub end_byte: u64,
        pub downloaded_bytes: u64,
        last_sampled_bytes: u64,
        pub speed: f64,
        pub status: ChunkStatus,
        pub status_message: Option<String>,
        pub status_changed_at: Instant,
    }

    impl ChunkState {
        pub fn new(id: ChunkId, start_byte: u64, end_byte: u64) -> Self {
            Self {
                id,
                start_byte,
                end_byte,
                downloaded_bytes: 0,
                last_sampled_bytes: 0,
                speed: 0.0,
                status: ChunkStatus::Downloading,
                status_message: None,
                status_changed_at: Instant::now(),
            }
        }

        pub fn update_status(&mut self, status: ChunkStatus, message: Option<String>) {
            self.status = status;
            self.status_message = message;
            self.status_changed_at = Instant::now();
        }

        pub fn update_downloaded(&mut self, downloaded_bytes: u64) {
            self.downloaded_bytes = downloaded_bytes;
        }

        pub fn update_end_byte(&mut self, end_byte: u64) {
            self.end_byte = end_byte;
        }

        pub fn size(&self) -> u64 {
            self.end_byte
                .saturating_sub(self.start_byte)
                .saturating_add(1)
        }

        pub fn update_speed(&mut self, elapsed_secs: f64, smoothing_factor: f64) {
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

    pub struct DownloadState {
        pub total_file_size: u64,
        pub chunks: HashMap<ChunkId, ChunkState>,
        completed_bytes: u64,
    }

    impl DownloadState {
        pub fn new(total_file_size: u64) -> Self {
            Self {
                total_file_size,
                chunks: HashMap::new(),
                completed_bytes: 0,
            }
        }

        pub fn complete_chunk(&mut self, id: &ChunkId) {
            if let Some(chunk) = self.chunks.remove(id) {
                self.completed_bytes += chunk.size();
            }
        }

        pub fn total_downloaded(&self) -> u64 {
            self.completed_bytes
                + self
                    .chunks
                    .values()
                    .map(|c| c.downloaded_bytes)
                    .sum::<u64>()
        }

        pub fn total_speed(&self) -> f64 {
            self.chunks.values().map(|c| c.speed).sum()
        }

        pub fn is_download_finished(&self) -> bool {
            self.total_downloaded() >= self.total_file_size
        }
    }
}

pub mod chunk_old {
    use crate::types_old::{ChunkId, DownloadCmd, DownloadInfo};
    use bytes::Bytes;
    use futures_util::StreamExt;
    use reqwest::RequestBuilder;
    use tokio::sync::{broadcast, mpsc};

    pub(crate) const MIN_CHUNK_SIZE: u64 = 1024 * 10;

    pub(crate) struct ChunkContext {
        pub id: ChunkId,
        pub cmd_tx: mpsc::Sender<DownloadCmd>,
        pub bd_rx: broadcast::Receiver<DownloadCmd>,
        pub info_tx: broadcast::Sender<DownloadInfo>,
        pub req_builder: RequestBuilder,
        pub start_byte: u64,
        pub end_byte: u64,
    }

    pub(crate) async fn chunk_run(mut ctx: ChunkContext) {
        let mut end = ctx.end_byte;
        let mut offset = ctx.start_byte;

        let range_header = format!("bytes={}-{}", ctx.start_byte, ctx.end_byte);
        let response = match ctx
            .req_builder
            .header("Range", range_header)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => resp,
            Err(e) => {
                let _ = ctx.info_tx.send(DownloadInfo::ChunkFailed {
                    id: ctx.id,
                    start: ctx.start_byte,
                    end,
                    error: e.to_string(),
                });
                return;
            }
        };

        let mut stream = response.bytes_stream();
        let mut failed = false;

        loop {
            tokio::select! {
                biased;

                Ok(cmd) = ctx.bd_rx.recv() => match cmd {
                    DownloadCmd::BisectDownload { id } if id == ctx.id => {
                        let remaining = end.saturating_sub(offset);
                        if remaining < MIN_CHUNK_SIZE * 2 { continue; }

                        let midpoint = offset + remaining / 2;
                        let new_chunk_start = midpoint + 1;

                        if ctx.info_tx.send(DownloadInfo::ChunkBisected {
                            original_id: ctx.id,
                            new_start: new_chunk_start,
                            new_end: end,
                        }).is_ok() {
                            end = midpoint;
                        }
                    }
                    DownloadCmd::TerminateAll => break,
                    _ => {}
                },

                chunk_result = stream.next() => {
                    let chunk = match chunk_result {
                        Some(Ok(c)) => c,
                        Some(Err(e)) => {
                            let _ = ctx.info_tx.send(DownloadInfo::ChunkFailed { id: ctx.id, start: offset, end, error: e.to_string() });
                            failed = true;
                            break;
                        },
                        None => break,
                    };

                    if offset > end { break; }

                    let allowed = end.saturating_sub(offset).saturating_add(1);
                    if allowed == 0 { break; }

                    let chunk_len = chunk.len() as u64;
                    let write_len = std::cmp::min(allowed, chunk_len);
                    let to_write: Bytes = if write_len as usize == chunk.len() {
                        chunk
                    } else {
                        chunk.slice(..write_len as usize)
                    };

                    if ctx.cmd_tx.send(DownloadCmd::WriteFile { offset, data: to_write }).await.is_err() {
                        let error_msg = format!("[Chunk {}] 文件写入通道已关闭", ctx.id);
                        let _ = ctx.info_tx.send(DownloadInfo::ChunkFailed { id: ctx.id, start: offset, end, error: error_msg });
                        failed = true;
                        break;
                    }

                    offset = offset.saturating_add(write_len);

                    let _ = ctx.info_tx.send(DownloadInfo::ChunkProgress {
                        id: ctx.id,
                        start_byte: ctx.start_byte,
                        end_byte: end,
                        downloaded: offset.saturating_sub(ctx.start_byte),
                    });

                    if write_len < chunk_len { break; }
                }
            }
        }

        if !failed {
            let _ = ctx.info_tx.send(DownloadInfo::ChunkCompleted(ctx.id));
        }
    }
}

pub mod concurrency {
    use crate::state::{ChunkState, DownloadState};
    use crate::types_old::{ChunkId, DownloadCmd};
    use std::collections::{HashMap, VecDeque};
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    const BANDWIDTH_PROBE_FACTOR: f64 = 1.2;
    const STABLE_SPLIT_THRESHOLD: f64 = 0.8;
    const MIN_SPLIT_INTERVAL: Duration = Duration::from_millis(100);
    const MIN_REMAINING_TIME_FOR_SPLIT: f64 = 5.0;
    pub(crate) const MIN_CHUNK_SIZE: u64 = 1024 * 10;

    #[derive(Debug, PartialEq, Clone, Copy)]
    enum DownloadPhase {
        Probing,
        Stable,
    }

    pub struct ConcurrencyManager {
        max_workers: u64,
        phase: DownloadPhase,
        max_speed: f64,
        last_split_time: Instant,
        stable_speed_samples: VecDeque<f64>,
    }

    impl ConcurrencyManager {
        pub fn new(max_workers: u64) -> Self {
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
            }
        }

        pub fn decide_and_act(
            &mut self,
            state: &DownloadState,
            cmd_tx: &broadcast::Sender<DownloadCmd>,
        ) {
            if self.last_split_time.elapsed() < MIN_SPLIT_INTERVAL {
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

            let avg_speed = self.stable_speed_samples.iter().sum::<f64>()
                / self.stable_speed_samples.len() as f64;
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
            _avg_speed: f64,
            cmd_tx: &broadcast::Sender<DownloadCmd>,
        ) {
            let active_chunks = state.chunks.len() as u64;
            let next_target = (active_chunks * 2).min(self.max_workers);
            if next_target > active_chunks {
                let mut splits_needed = next_target - active_chunks;
                while splits_needed > 0 {
                    if let Some(largest_chunk) = self.find_largest_chunk(&state.chunks) {
                        if largest_chunk.size() >= MIN_CHUNK_SIZE * 2 {
                            self.request_split(largest_chunk.id, cmd_tx);
                            splits_needed -= 1;
                        } else {
                            self.transition_to_stable();
                            return;
                        }
                    } else {
                        break;
                    }
                }
            } else {
                self.transition_to_stable();
            }
        }

        fn handle_stable_phase(
            &mut self,
            state: &DownloadState,
            avg_speed: f64,
            estimated_time: f64,
            cmd_tx: &broadcast::Sender<DownloadCmd>,
        ) {
            if self.stable_speed_samples.len() > 3
                && (avg_speed > self.max_speed * BANDWIDTH_PROBE_FACTOR
                    || avg_speed < self.max_speed * STABLE_SPLIT_THRESHOLD)
                && estimated_time > MIN_REMAINING_TIME_FOR_SPLIT
                && (state.chunks.len() as u64) < self.max_workers
            {
                if let Some(slowest_chunk) = self.find_slowest_splittable_chunk(&state.chunks) {
                    self.request_split(slowest_chunk.id, cmd_tx);
                }
                if avg_speed < self.max_speed * STABLE_SPLIT_THRESHOLD {
                    self.stable_speed_samples.clear();
                }
            }
        }

        fn reactive_split(
            &mut self,
            state: &DownloadState,
            cmd_tx: &broadcast::Sender<DownloadCmd>,
        ) {
            if self.phase == DownloadPhase::Stable
                && self.last_split_time.elapsed() > MIN_SPLIT_INTERVAL
            {
                let active_chunks = state.chunks.len() as u64;
                if active_chunks > 0 && active_chunks < self.max_workers {
                    if let Some(largest_chunk) = self.find_largest_chunk(&state.chunks) {
                        self.request_split(largest_chunk.id, cmd_tx);
                    }
                }
            }
        }

        fn request_split(&mut self, id: ChunkId, cmd_tx: &broadcast::Sender<DownloadCmd>) {
            let _ = cmd_tx.send(DownloadCmd::BisectDownload { id });
            self.last_split_time = Instant::now();
        }

        fn transition_to_stable(&mut self) {
            self.phase = DownloadPhase::Stable;
            self.stable_speed_samples.clear();
        }

        fn find_largest_chunk<'a>(
            &self,
            chunks: &'a HashMap<ChunkId, ChunkState>,
        ) -> Option<&'a ChunkState> {
            chunks.values().max_by_key(|c| c.size())
        }

        fn find_slowest_splittable_chunk<'a>(
            &self,
            chunks: &'a HashMap<ChunkId, ChunkState>,
        ) -> Option<&'a ChunkState> {
            chunks
                .values()
                .filter(|c| c.size().saturating_sub(c.downloaded_bytes) > MIN_CHUNK_SIZE * 2)
                .min_by(|a, b| {
                    a.speed
                        .partial_cmp(&b.speed)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        }
    }
}

pub mod downloader {
    use crate::chunk_old::{ChunkContext, chunk_run};
    use crate::monitor::{DownloadMonitor, MonitorContext};
    use crate::types_old::{DownloadCmd, DownloadInfo, Result};
    use crate::util_old::{file_writer_task, get_file_info};
    use faststr::FastStr;
    use futures_util::stream::FuturesUnordered;
    use reqwest::{Client, ClientBuilder};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::spawn;
    use tokio::sync::{broadcast, mpsc};

    const MIN_CHUNK_SIZE_FOR_CONCURRENCY: u64 = 1024 * 1024;
    const CHANNEL_CAPACITY: usize = 1024;

    #[derive(Clone)]
    struct DownloaderConfig {
        url: FastStr,
        output_path: FastStr,
        workers: u64,
    }

    pub struct Downloader<F>
    where
        F: Fn() -> ClientBuilder,
    {
        config: DownloaderConfig,
        client_builder: F,
        cmd_tx: broadcast::Sender<DownloadCmd>,
        info_tx: broadcast::Sender<DownloadInfo>,
        update_interval: f64,
    }

    impl<F> Downloader<F>
    where
        F: Fn() -> ClientBuilder + Send + Sync + 'static,
    {
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

        pub async fn run<ProgF, Fut>(self, progress_handler: ProgF) -> Result<()>
        where
            ProgF: FnOnce(u64, broadcast::Receiver<DownloadInfo>) -> Fut,
            Fut: std::future::Future<Output = ()> + Send + 'static,
        {
            let client = (self.client_builder)().build()?;
            let (file_size, support_ranges) = get_file_info(&client, &self.config.url).await?;

            let info_rx_for_progress = self.info_tx.subscribe();
            spawn(progress_handler(file_size, info_rx_for_progress));

            let writer_tx = file_writer_task(self.config.output_path.clone(), file_size).await?;

            self.start_and_monitor(file_size, support_ranges, writer_tx, client)
                .await?;

            let _ = self.cmd_tx.send(DownloadCmd::TerminateAll);
            Ok(())
        }

        async fn start_and_monitor(
            &self,
            file_size: u64,
            support_ranges: bool,
            writer_tx: mpsc::Sender<DownloadCmd>,
            client: Client,
        ) -> Result<()> {
            let tasks = FuturesUnordered::new();
            let next_chunk_id = AtomicU64::new(0);

            let workers = if !support_ranges
                || self.config.workers <= 1
                || file_size < MIN_CHUNK_SIZE_FOR_CONCURRENCY
            {
                1
            } else {
                self.config.workers
            };

            let initial_id = next_chunk_id.fetch_add(1, Ordering::SeqCst);
            let initial_chunk_ctx = ChunkContext {
                id: initial_id,
                cmd_tx: writer_tx.clone(),
                bd_rx: self.cmd_tx.subscribe(),
                info_tx: self.info_tx.clone(),
                req_builder: client.get(self.config.url.as_str()),
                start_byte: 0,
                end_byte: file_size.saturating_sub(1),
            };
            tasks.push(spawn(chunk_run(initial_chunk_ctx)));

            let monitor = DownloadMonitor::new(file_size, self.update_interval, workers);

            let monitor_ctx = MonitorContext {
                tasks,
                next_chunk_id: &next_chunk_id,
                client: &client,
                writer_tx: &writer_tx,
                cmd_tx: &self.cmd_tx,
                info_tx: &self.info_tx,
                url: &self.config.url,
            };

            monitor.run(self.info_tx.subscribe(), monitor_ctx).await;
            Ok(())
        }
    }
}

pub mod monitor {
    use crate::chunk_old::{ChunkContext, chunk_run};
    use crate::concurrency::ConcurrencyManager;
    use crate::retry::RetryHandler;
    use crate::state::{ChunkState, DownloadState};
    use crate::types_old::{ChunkId, ChunkStatus, DownloadCmd, DownloadInfo};
    use faststr::FastStr;
    use futures_util::stream::{FuturesUnordered, StreamExt};
    use reqwest::Client;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::{broadcast, mpsc};
    use tokio::task::JoinHandle;
    use tokio::time::interval;

    const SMOOTHING_FACTOR: f64 = 0.15;

    pub struct MonitorContext<'a> {
        pub tasks: FuturesUnordered<JoinHandle<()>>,
        pub next_chunk_id: &'a AtomicU64,
        pub client: &'a Client,
        pub writer_tx: &'a mpsc::Sender<DownloadCmd>,
        pub cmd_tx: &'a broadcast::Sender<DownloadCmd>,
        pub info_tx: &'a broadcast::Sender<DownloadInfo>,
        pub url: &'a FastStr,
    }

    impl<'a> MonitorContext<'a> {
        fn spawn_chunk(&mut self, start: u64, end: u64) -> ChunkId {
            let id = self.next_chunk_id.fetch_add(1, Ordering::SeqCst);
            let chunk_ctx = ChunkContext {
                id,
                cmd_tx: self.writer_tx.clone(),
                bd_rx: self.cmd_tx.subscribe(),
                info_tx: self.info_tx.clone(),
                req_builder: self.client.get(self.url.as_str()),
                start_byte: start,
                end_byte: end,
            };
            self.tasks.push(tokio::spawn(chunk_run(chunk_ctx)));
            id
        }
    }

    pub struct DownloadMonitor {
        state: DownloadState,
        retry_handler: RetryHandler,
        concurrency_manager: ConcurrencyManager,
        update_interval: Duration,
    }

    impl DownloadMonitor {
        pub fn new(total_file_size: u64, update_interval_secs: f64, max_workers: u64) -> Self {
            Self {
                state: DownloadState::new(total_file_size),
                retry_handler: RetryHandler::new(),
                concurrency_manager: ConcurrencyManager::new(max_workers),
                update_interval: Duration::from_secs_f64(update_interval_secs),
            }
        }

        pub async fn run(
            mut self,
            mut info_rx: broadcast::Receiver<DownloadInfo>,
            mut ctx: MonitorContext<'_>,
        ) {
            let mut ticker = interval(self.update_interval);
            let mut last_tick_time = Instant::now();

            loop {
                tokio::select! {
                    biased;

                    Some(_result) = ctx.tasks.next() => {
                        if ctx.tasks.is_empty() && self.is_finished() { break; }
                    },

                    Ok(info) = info_rx.recv() => {
                        self.handle_download_info(info, &mut ctx);
                    },

                    _ = ticker.tick() => {
                        let now = Instant::now();
                        let elapsed = now.duration_since(last_tick_time);
                        last_tick_time = now;

                        self.handle_tick(elapsed.as_secs_f64(), &mut ctx);
                        if self.is_finished() { break; }
                    },

                    else => break,
                }
            }
        }

        fn handle_download_info(&mut self, info: DownloadInfo, ctx: &mut MonitorContext<'_>) {
            match info {
                DownloadInfo::ChunkProgress {
                    id,
                    start_byte,
                    end_byte,
                    downloaded,
                } => {
                    let chunk = self
                        .state
                        .chunks
                        .entry(id)
                        .or_insert_with(|| ChunkState::new(id, start_byte, end_byte));
                    chunk.update_downloaded(downloaded);
                    chunk.update_end_byte(end_byte);
                    if chunk.status != ChunkStatus::Downloading {
                        chunk.update_status(ChunkStatus::Downloading, None);
                        let _ = ctx.info_tx.send(DownloadInfo::ChunkStatusChanged {
                            id,
                            status: ChunkStatus::Downloading,
                            message: None,
                        });
                    }
                }
                DownloadInfo::ChunkCompleted(id) => {
                    self.state.complete_chunk(&id);
                    self.retry_handler.on_download_complete(&id);
                    let _ = ctx.info_tx.send(DownloadInfo::ChunkStatusChanged {
                        id,
                        status: ChunkStatus::Completed,
                        message: None,
                    });
                }
                DownloadInfo::ChunkFailed {
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
                        ctx.info_tx,
                    );
                }
                DownloadInfo::ChunkBisected {
                    new_start, new_end, ..
                } => {
                    ctx.spawn_chunk(new_start, new_end);
                }
                _ => {}
            }
        }

        fn handle_tick(&mut self, elapsed_secs: f64, ctx: &mut MonitorContext<'_>) {
            if elapsed_secs <= 0.0 {
                return;
            }

            for chunk in self.state.chunks.values_mut() {
                chunk.update_speed(elapsed_secs, SMOOTHING_FACTOR);
            }
            self.send_monitor_update(ctx.info_tx);

            self.concurrency_manager
                .decide_and_act(&self.state, ctx.cmd_tx);

            self.retry_handler.process_queues();
            while let Some(chunk_to_retry) = self.retry_handler.pop_ready_chunk() {
                let _ = ctx.info_tx.send(DownloadInfo::ChunkStatusChanged {
                    id: chunk_to_retry.id,
                    status: ChunkStatus::Retrying {
                        attempt: chunk_to_retry.attempts,
                    },
                    message: Some(format!("正在进行第 {} 次重试", chunk_to_retry.attempts)),
                });
                ctx.spawn_chunk(chunk_to_retry.start, chunk_to_retry.end);
            }
        }

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

        fn is_finished(&self) -> bool {
            self.state.is_download_finished() && self.retry_handler.is_idle()
        }
    }
}

pub mod retry {
    use crate::state::DownloadState;
    use crate::types_old::{ChunkId, ChunkStatus, DownloadInfo};
    use std::collections::{HashMap, VecDeque};
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    const MAX_RETRIES: u32 = 10;
    const RETRY_DELAY: Duration = Duration::from_secs(2);
    const DELAYED_RETRY_DURATION: Duration = Duration::from_secs(10);

    #[derive(Debug)]
    pub struct FailedChunkInfo {
        pub id: ChunkId,
        pub start: u64,
        pub end: u64,
        failure_time: Instant,
        pub attempts: u32,
    }

    #[derive(Debug)]
    struct DelayedChunkInfo {
        chunk: FailedChunkInfo,
        retry_at: Instant,
    }

    pub struct RetryHandler {
        retry_queue: VecDeque<FailedChunkInfo>,
        delayed_retry_queue: VecDeque<DelayedChunkInfo>,
        retry_attempts: HashMap<ChunkId, u32>,
    }

    impl RetryHandler {
        pub fn new() -> Self {
            Self {
                retry_queue: VecDeque::new(),
                delayed_retry_queue: VecDeque::new(),
                retry_attempts: HashMap::new(),
            }
        }

        pub fn on_chunk_failed(
            &mut self,
            id: ChunkId,
            start: u64,
            end: u64,
            _error: String,
            state: &mut DownloadState,
            info_tx: &broadcast::Sender<DownloadInfo>,
        ) {
            state.chunks.remove(&id);

            let attempts = self
                .retry_attempts
                .entry(id)
                .and_modify(|e| *e += 1)
                .or_insert(1);

            if *attempts <= MAX_RETRIES {
                let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                    id,
                    status: ChunkStatus::Retrying { attempt: *attempts },
                    message: Some(format!(
                        "将在 {:?} 后进行第 {}/{} 次重试",
                        RETRY_DELAY, *attempts, MAX_RETRIES
                    )),
                });
                self.retry_queue.push_back(FailedChunkInfo {
                    id,
                    start,
                    end,
                    failure_time: Instant::now(),
                    attempts: *attempts,
                });
            } else {
                let retry_at = Instant::now() + DELAYED_RETRY_DURATION;
                let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                    id,
                    status: ChunkStatus::Delayed,
                    message: Some(format!(
                        "重试次数过多，将在 {:?} 后再次尝试",
                        DELAYED_RETRY_DURATION
                    )),
                });
                self.delayed_retry_queue.push_back(DelayedChunkInfo {
                    chunk: FailedChunkInfo {
                        id,
                        start,
                        end,
                        failure_time: Instant::now(),
                        attempts: *attempts,
                    },
                    retry_at,
                });
                self.retry_attempts.remove(&id);
            }
        }

        pub fn process_queues(&mut self) {
            let now = Instant::now();
            while let Some(delayed_info) = self.delayed_retry_queue.front() {
                if now >= delayed_info.retry_at {
                    if let Some(delayed_info) = self.delayed_retry_queue.pop_front() {
                        let mut info_to_retry = delayed_info.chunk;
                        info_to_retry.failure_time = Instant::now();
                        info_to_retry.attempts = 0;
                        self.retry_queue.push_back(info_to_retry);
                    }
                } else {
                    break;
                }
            }
        }

        pub fn pop_ready_chunk(&mut self) -> Option<FailedChunkInfo> {
            if let Some(true) = self
                .retry_queue
                .front()
                .map(|c| c.failure_time.elapsed() >= RETRY_DELAY)
            {
                self.retry_queue.pop_front()
            } else {
                None
            }
        }

        pub fn on_download_complete(&mut self, id: &ChunkId) {
            self.retry_attempts.remove(id);
        }

        pub fn is_idle(&self) -> bool {
            self.retry_queue.is_empty() && self.delayed_retry_queue.is_empty()
        }
    }
}

pub mod util_old {
    use crate::types_old::DownloadCmd;
    use crate::types_old::{DownloadError, Result};
    use faststr::FastStr;
    use reqwest::Client;
    use std::io;
    use tokio::fs::OpenOptions;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    use tokio::spawn;
    use tokio::sync::mpsc;

    pub(crate) async fn get_file_info(client: &Client, url: &str) -> Result<(u64, bool)> {
        use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};

        if let Ok(resp) = client
            .head(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            let headers = resp.headers();
            if let Some(len_val) = headers.get(CONTENT_LENGTH) {
                if let Ok(len_str) = len_val.to_str() {
                    if let Ok(content_length) = len_str.parse::<u64>() {
                        let accept_ranges = headers
                            .get(ACCEPT_RANGES)
                            .map_or(false, |v| v.as_bytes().eq_ignore_ascii_case(b"bytes"));
                        return Ok((content_length, accept_ranges));
                    }
                }
            }
        }

        let range_resp = client
            .get(url)
            .header("Range", "bytes=0-0")
            .send()
            .await?
            .error_for_status()?;

        let headers = range_resp.headers();
        if let Some(cr) = headers.get(CONTENT_RANGE) {
            if let Ok(crs) = cr.to_str() {
                if let Some(pos) = crs.rfind('/') {
                    let total = &crs[pos + 1..].trim();
                    if *total != "*" {
                        if let Ok(content_length) = total.parse::<u64>() {
                            return Ok((content_length, true));
                        }
                    }
                }
            }
        }

        if let Some(len_val) = headers.get(CONTENT_LENGTH) {
            if let Ok(len_str) = len_val.to_str() {
                if let Ok(content_length) = len_str.parse::<u64>() {
                    return Ok((content_length, false));
                }
            }
        }

        Err(DownloadError::MissingContentLength)
    }

    pub(crate) async fn file_writer_task(
        filepath: FastStr,
        size: u64,
    ) -> Result<mpsc::Sender<DownloadCmd>> {
        const WRITER_QUEUE_CAP: usize = 128;
        let (tx, mut rx) = mpsc::channel::<DownloadCmd>(WRITER_QUEUE_CAP);

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&*filepath)
            .await?;
        file.set_len(size).await?;

        spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    DownloadCmd::WriteFile { offset, data } => {
                        if file.seek(io::SeekFrom::Start(offset)).await.is_err()
                            || file.write_all(&data).await.is_err()
                        {
                            break;
                        }
                    }
                    DownloadCmd::TerminateAll => break,
                    _ => {}
                }
            }
            let _ = file.flush().await;
        });

        Ok(tx)
    }
}
