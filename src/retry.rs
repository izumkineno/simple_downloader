//! 管理失败下载块的重试逻辑。
//! P0-6: retry_queue 10×2s + delayed 10s (总量30), push_back FIFO, pop_ready扫描首个就绪防队头阻塞

use crate::state::DownloadState;
use crate::types::{ChunkId, DownloadInfo};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 每个块的最大即时重试次数。
const MAX_RETRIES: u32 = 10;
/// 两次即时重试之间的最小延迟。
const RETRY_DELAY: Duration = Duration::from_secs(1);
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
    /// 批量失败熔断：上次失败时间，用于检测 500ms 内多路并败
    last_failure_time: Option<Instant>,
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
            last_failure_time: None,
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
        if error.contains("decoding") {
            ::tracing::debug!(
                chunk_id = id,
                start,
                end,
                error = %error,
                downloaded = state.total_downloaded(),
                total = state.total_file_size,
                active_chunks = state.chunks.len(),
                retry_q = self.retry_queue.len(),
                delayed = self.delayed_retry_queue.len(),
                "chunk failed transient (decoding)"
            );
        } else {
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
        }
        // 保留已下载前缀，避免 total_downloaded 瞬时回落
        // 使用 ChunkFailed 携带的 start(offset) 精确计算已下载，避免依赖 broadcast 节流后的 stale downloaded_bytes
        if let Some(chunk) = state.chunks.get(&id) {
            let exact = start.saturating_sub(chunk.start_byte);
            state.preserve_partial_exact(&id, exact);
        } else {
            state.preserve_partial(&id);
        }
        // 从活跃的块列表中移除该块
        state.chunks.remove(&id);
        // 本地 I/O 击穿（writer 死亡）不该走 30 次漫长重试：一次即永久失败，避免卡死 100%
        let is_local_io_failure = error.contains("写入通道已关闭")
            || error.contains("writer closed")
            || error.contains("BrokenPipe")
            || error.contains("os error 5");
        if is_local_io_failure {
            ::tracing::error!(chunk_id = id, error = %error, "local writer I/O failure, permanent without retry");
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id,
                status: 5,
                message: Some(format!("本地写入失败，终止重试: {error}")),
            });
            self.permanent_failures.push(FailedChunkInfo {
                id,
                start,
                end,
                failure_time: Instant::now(),
                attempts: *self.total_attempts.get(&id).unwrap_or(&0) + 1,
            });
            self.retry_attempts.remove(&id);
            return;
        }
        // 跨周期总计数，超过阈值判永久失败
        let total = self.total_attempts.entry(id).or_insert(0);
        *total += 1;
        if *total > MAX_TOTAL_ATTEMPTS {
            ::tracing::error!(
                chunk_id = id,
                total = *total,
                max = MAX_TOTAL_ATTEMPTS,
                "chunk permanent failure: total attempts exceeded"
            );
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
        // 批量失败熔断：500ms 内多路并败时抖动退避，避免 16路并发重试惊群
        let now = Instant::now();
        let is_batch = self
            .last_failure_time
            .map(|t| now.duration_since(t) < Duration::from_millis(500) && self.retry_queue.len() >= 3)
            .unwrap_or(false);
        self.last_failure_time = Some(now);

        if *attempts <= MAX_RETRIES {
            // 瞬时 decoding 降为 debug，避免 7路并败时 info 风暴
            if error.contains("decoding") {
                ::tracing::debug!(
                    chunk_id = id,
                    attempt = *attempts,
                    max = MAX_RETRIES,
                    total = *total,
                    batch = is_batch,
                    "chunk will retry transient"
                );
            } else {
                ::tracing::info!(
                    chunk_id = id,
                    attempt = *attempts,
                    max = MAX_RETRIES,
                    total = *total,
                    "chunk will retry"
                );
            }

            // 发送状态变更为“等待重试”
            let _ = info_tx.send(DownloadInfo::ChunkStatusChanged {
                id,
                status: 2, // 等待重试
                message: Some(format!(
                    "将进行第 {} 次重试 (共 {} 次)",
                    *attempts, MAX_RETRIES
                )),
            });

            let failure_time = if is_batch {
                now + Duration::from_millis(200 * (self.retry_queue.len() as u64 + 1))
            } else {
                now
            };
            self.retry_queue.push_back(FailedChunkInfo {
                id,
                start,
                end,
                failure_time,
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

                // P0-06 修复：延迟队列已等待 10s，无需再等 2s，设为可立即 pop
                info_to_retry.failure_time = Instant::now() - RETRY_DELAY;
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

    /// 饥饿时强制将延迟队列全部移回即时队列并清 1s/10s 等待（IDM 尾部急补），避免空闲 10s 慢补
    pub fn force_drain_delayed(&mut self) {
        let now = Instant::now();
        let mut drained = 0usize;
        while let Some(mut delayed) = self.delayed_retry_queue.pop_front() {
            delayed.chunk.failure_time = now - RETRY_DELAY;
            delayed.chunk.attempts = 0;
            self.retry_queue.push_back(delayed.chunk);
            drained += 1;
        }
        if drained > 0 {
            ::tracing::info!(drained, "starved force_drain_delayed");
        }
    }

    /// 碎片合并：类似内存碎片整理，将重试队列中相邻且均小的非连续孔洞合并为大块，减少微任务风暴（IDM 碎片整理）
    pub fn coalesce_small_fragments(&mut self) {
        if self.retry_queue.len() < 2 {
            return;
        }
        let mut vec: Vec<FailedChunkInfo> = self.retry_queue.drain(..).collect();
        vec.sort_by_key(|c| c.start);
        let mut merged: VecDeque<FailedChunkInfo> = VecDeque::new();
        for chunk in vec {
            if let Some(last) = merged.back_mut() {
                let last_size = last.end.saturating_sub(last.start) + 1;
                let cur_size = chunk.end.saturating_sub(chunk.start) + 1;
                let adjacent = last.end.checked_add(1) == Some(chunk.start);
                let both_small = last_size < 256 * 1024 || cur_size < 256 * 1024;
                let combined_ok = last_size + cur_size <= 1024 * 1024;
                if adjacent && both_small && combined_ok {
                    last.end = chunk.end;
                    last.attempts = last.attempts.max(chunk.attempts);
                    if chunk.failure_time < last.failure_time {
                        last.failure_time = chunk.failure_time;
                    }
                    ::tracing::debug!(merged_start = last.start, merged_end = last.end, "coalesced fragmented retry");
                    continue;
                }
            }
            merged.push_back(chunk);
        }
        ::tracing::trace!(merged = merged.len(), "fragment coalesce done");
        self.retry_queue = merged;
    }

    /// 饥饿时允许立即弹出未到 1s 的队头（绕过 RETRY_DELAY），避免单线程慢尾空转
    pub fn pop_ready_chunk_starved(&mut self) -> Option<FailedChunkInfo> {
        if self.retry_queue.is_empty() {
            return None;
        }
        let chunk = self.retry_queue.pop_front();
        if let Some(c) = &chunk {
            ::tracing::info!(chunk_id = c.id, "starved immediate pop retry (bypass 1s)");
        }
        chunk
    }

    /// 从即时重试队列中弹出一个已达到重试延迟时间的块（扫描首个就绪项，避免队头阻塞）。
    pub fn pop_ready_chunk(&mut self) -> Option<FailedChunkInfo> {
        let pos = self
            .retry_queue
            .iter()
            .position(|c| c.failure_time.elapsed() >= RETRY_DELAY)?;
        let chunk = self.retry_queue.remove(pos);
        if let Some(c) = &chunk {
            ::tracing::debug!(
                chunk_id = c.id,
                attempts = c.attempts,
                pos,
                "pop ready retry chunk (scan)"
            );
        }
        chunk
    }

    #[allow(dead_code)]
    pub(crate) fn push_front_retry(&mut self, chunk: FailedChunkInfo) {
        self.retry_queue.push_front(chunk);
    }

    #[allow(dead_code)]
    pub(crate) fn push_back_retry(&mut self, chunk: FailedChunkInfo) {
        ::tracing::debug!(chunk_id = chunk.id, "push back deferred retry");
        self.retry_queue.push_back(chunk);
    }

    pub(crate) fn push_back_retry_with_backoff(&mut self, mut chunk: FailedChunkInfo) {
        chunk.failure_time = Instant::now();
        ::tracing::debug!(
            chunk_id = chunk.id,
            "push back deferred retry with 2s backoff"
        );
        self.retry_queue.push_back(chunk);
    }

    /// 当一个块最终下载成功时，清除其重试记录。
    pub fn on_download_complete(&mut self, id: &ChunkId) {
        let a = self.retry_attempts.remove(id);
        let b = self.total_attempts.remove(id);
        if a.is_some() || b.is_some() {
            ::tracing::debug!(chunk_id = id, "retry records cleared on complete");
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
