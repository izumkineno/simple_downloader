//! 管理失败下载块的重试逻辑。

use crate::state::DownloadState;
use crate::types::{ChunkId, DownloadInfo};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 每个块的最大即时重试次数。
const MAX_RETRIES: u32 = 10;
/// 两次即时重试之间的最小延迟。
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// 当达到最大即时重试次数后，进入延迟重试队列的等待时间。
const DELAYED_RETRY_DURATION: Duration = Duration::from_secs(10);
/// 单块跨延迟周期的最大总尝试次数，超过则判永久失败避免永重试挂死。
const MAX_TOTAL_ATTEMPTS: u32 = 30;
/// 存储失败块的信息，用于重试。
#[derive(Debug)]
pub struct FailedChunkInfo {
    pub id: ChunkId,
    pub start: u64,
    pub end: u64,
    /// 失败发生的时间戳。
    failure_time: Instant,
    /// 当前的重试次数。
    pub attempts: u32,
}

/// 存储需要延迟重试的块的信息。
#[derive(Debug)]
struct DelayedChunkInfo {
    chunk: FailedChunkInfo,
    /// 应该在何时进行下一次重试。
    retry_at: Instant,
}
/// 管理所有失败块重试逻辑的结构体。
pub struct RetryHandler {
    /// 即时重试队列：失败的块会先进入这里。
    retry_queue: VecDeque<FailedChunkInfo>,
    /// 延迟重试队列：当一个块的即时重试次数用尽后，会进入这里进行更长时间的等待。
    delayed_retry_queue: VecDeque<DelayedChunkInfo>,
    /// 记录每个块的重试次数。
    retry_attempts: HashMap<ChunkId, u32>,
    /// 跨周期的总尝试次数，超过阈值判永久失败。
    total_attempts: HashMap<ChunkId, u32>,
    /// 永久失败的块，触发下载整体失败。
    permanent_failures: Vec<FailedChunkInfo>,
}

impl Default for RetryHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl RetryHandler {
    pub fn new() -> Self {
        ::tracing::trace!("RetryHandler created");
        Self {
            retry_queue: VecDeque::new(),
            delayed_retry_queue: VecDeque::new(),
            retry_attempts: HashMap::new(),
            total_attempts: HashMap::new(),
            permanent_failures: Vec::new(),
        }
    }

    pub fn retry_queue_len(&self) -> usize {
        self.retry_queue.len()
    }
    pub fn delayed_queue_len(&self) -> usize {
        self.delayed_retry_queue.len()
    }

    /// 处理一个失败的块，将其添加到适当的重试队列中。
    #[::tracing::instrument(skip(self, state, info_tx), fields(chunk_id = id, range = format!("{start}-{end}")))]
    pub fn on_chunk_failed(
        &mut self,
        id: ChunkId,
        start: u64,
        end: u64,
        error: String,
        state: &mut DownloadState,
        info_tx: &broadcast::Sender<DownloadInfo>,
    ) {
        ::tracing::warn!(
            chunk_id = id,
            start,
            end,
            error = %error,
            downloaded = state.total_downloaded(),
            total = state.total_file_size,
            active_chunks = state.chunks.len(),
            retry_q = self.retry_queue.len(),
            delayed = self.delayed_retry_queue.len(),
            "chunk failed"
        );

        // 保留已下载前缀，避免 total_downloaded 瞬时回落
        state.preserve_partial(&id);
        // 从活跃的块列表中移除该块
        state.chunks.remove(&id);

        // 跨周期总计数，超过阈值判永久失败
        let total = self.total_attempts.entry(id).or_insert(0);
        *total += 1;
        if *total > MAX_TOTAL_ATTEMPTS {
            ::tracing::error!(chunk_id = id, total = *total, max = MAX_TOTAL_ATTEMPTS, "chunk permanent failure: total attempts exceeded");
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id,
                status: 5,
                message: Some(format!("永久失败，已重试 {MAX_TOTAL_ATTEMPTS} 次: {error}")),
            });
            self.permanent_failures.push(FailedChunkInfo {
                id,
                start,
                end,
                failure_time: Instant::now(),
                attempts: *total,
            });
            self.retry_attempts.remove(&id);
            return;
        }

        // 增加该块的周期内重试次数
        let attempts = self.retry_attempts.entry(id).or_insert(0);
        *attempts += 1;

        if *attempts <= MAX_RETRIES {
            // 如果未达到最大重试次数，放入即时重试队列
            ::tracing::info!(
                chunk_id = id,
                attempt = *attempts,
                max = MAX_RETRIES,
                total = *total,
                "chunk will retry"
            );

            // 发送状态变更为“等待重试”
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id,
                status: 2, // 等待重试
                message: Some(format!(
                    "将进行第 {} 次重试 (共 {} 次)",
                    *attempts, MAX_RETRIES
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
            // 如果已达到最大重试次数，放入延迟队列，等待一段时间后再次尝试。
            let retry_at = Instant::now() + DELAYED_RETRY_DURATION;
            ::tracing::warn!(
                chunk_id = id,
                total = *total,
                delay = ?DELAYED_RETRY_DURATION,
                "chunk max retries reached, move to delayed queue"
            );

            // 发送状态变更为“延迟重试中”
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id,
                status: 3, // 延迟重试中
                message: Some(format!("将在 {:?} 后重试", DELAYED_RETRY_DURATION)),
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

            // 从即时重试计数器中移除，以便它在长时间等待后能重新开始计数
            self.retry_attempts.remove(&id);
        }
    }

    /// 处理队列，将延迟队列中到期的块移回主重试队列。
    pub fn process_queues(&mut self) {
        let now = Instant::now();
        while let Some(delayed_info) = self.delayed_retry_queue.front() {
            if now >= delayed_info.retry_at {
                let delayed = self
                    .delayed_retry_queue
                    .pop_front()
                    .expect("front is Some, pop_front must succeed");
                let mut info_to_retry = delayed.chunk;

                // 重置失败时间戳和尝试次数，让它能进入主重试队列并被立即处理
                info_to_retry.failure_time = Instant::now();
                info_to_retry.attempts = 0; // 重置尝试次数

                ::tracing::info!(
                    chunk_id = info_to_retry.id,
                    "delayed retry queue: chunk re-queued"
                );

                // 将其放回主重试队列
                self.retry_queue.push_back(info_to_retry);
            } else {
                // 队列是按时间排序的，如果队首的都没到时间，后面的肯定也没到
                break;
            }
        }
    }

    /// 从即时重试队列中弹出一个已达到重试延迟时间的块。
    pub fn pop_ready_chunk(&mut self) -> Option<FailedChunkInfo> {
        if let Some(failed_chunk) = self.retry_queue.front()
            && failed_chunk.failure_time.elapsed() >= RETRY_DELAY
        {
            let chunk = self.retry_queue.pop_front();
            if let Some(ref c) = chunk {
                ::tracing::debug!(chunk_id = c.id, attempts = c.attempts, "pop ready retry chunk");
            }
            return chunk;
        }
        None
    }

    pub(crate) fn push_front_retry(&mut self, chunk: FailedChunkInfo) {
        ::tracing::debug!(chunk_id = chunk.id, "push front deferred retry");
        self.retry_queue.push_front(chunk);
    }

    /// 当一个块最终下载成功时，清除其重试记录。
    pub fn on_download_complete(&mut self, id: &ChunkId) {
        if self.retry_attempts.remove(id).is_some() || self.total_attempts.remove(id).is_some() {
            ::tracing::debug!(chunk_id = id, "retry records cleared on complete");
        } else {
            self.total_attempts.remove(id);
        }
    }

    /// 检查所有重试队列是否都为空。
    pub fn are_all_tasks_done(&self) -> bool {
        self.retry_queue.is_empty() && self.delayed_retry_queue.is_empty()
    }

    pub fn has_permanent_failure(&self) -> bool {
        !self.permanent_failures.is_empty()
    }

    pub fn permanent_failure_message(&self) -> Option<String> {
        self.permanent_failures.first().map(|f| {
            format!(
                "块 {} 区间 {}-{} 永久失败，已重试 {} 次",
                f.id, f.start, f.end, f.attempts
            )
        })
    }

    #[cfg(test)]
    pub fn total_attempts_for_test(&self, id: ChunkId) -> u32 {
        *self.total_attempts.get(&id).unwrap_or(&0)
    }
}
