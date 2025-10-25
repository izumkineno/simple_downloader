//! 管理失败下载块的重试逻辑。

use crate::state::DownloadState;
use crate::types::{ChunkId, DownloadInfo};
use log::{debug, info, trace, warn};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// 每个块的最大即时重试次数。
const MAX_RETRIES: u32 = 10;
/// 两次即时重试之间的最小延迟。
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// 当达到最大即时重试次数后，进入延迟重试队列的等待时间。
const DELAYED_RETRY_DURATION: Duration = Duration::from_secs(10);

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
}

impl RetryHandler {
    pub fn new() -> Self {
        debug!("RetryHandler 已创建。");
        Self {
            retry_queue: VecDeque::new(),
            delayed_retry_queue: VecDeque::new(),
            retry_attempts: HashMap::new(),
        }
    }

    /// 处理一个失败的块，将其添加到适当的重试队列中。
    pub fn on_chunk_failed(
        &mut self,
        id: ChunkId,
        start: u64,
        end: u64,
        error: String,
        state: &mut DownloadState,
        info_tx: &broadcast::Sender<DownloadInfo>,
    ) {
        warn!("[RetryHandler] 收到块 {id} 的失败报告: {error}");

        // 从活跃的块列表中移除该块
        state.chunks.remove(&id);

        // 增加该块的重试次数
        let attempts = self.retry_attempts.entry(id).or_insert(0);
        *attempts += 1;

        if *attempts <= MAX_RETRIES {
            // 如果未达到最大重试次数，放入即时重试队列
            info!(
                "[RetryHandler] 块 {id} 将进行第 {} 次重试 (共 {} 次).",
                *attempts, MAX_RETRIES
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
            warn!(
                "[RetryHandler] 块 {id} 已达到最大即时重试次数，将延迟 {:?} 后再次尝试。",
                DELAYED_RETRY_DURATION
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
                retry_at: Instant::now() + DELAYED_RETRY_DURATION,
            });

            // 从即时重试计数器中移除，以便它在长时间等待后能重新开始计数
            self.retry_attempts.remove(&id);
        }
    }

    /// 处理队列，将延迟队列中到期的块移回主重试队列。
    pub fn process_queues(&mut self) {
        debug!("[RetryHandler] 正在处理延迟重试队列。");
        let now = Instant::now();
        while let Some(delayed_info) = self.delayed_retry_queue.front() {
            if now >= delayed_info.retry_at {
                // 时间到了，将它从延迟队列中取出
                let mut info_to_retry = self.delayed_retry_queue.pop_front().unwrap().chunk;

                // 重置失败时间戳和尝试次数，让它能进入主重试队列并被立即处理
                info_to_retry.failure_time = Instant::now();
                info_to_retry.attempts = 0; // 重置尝试次数

                info!(
                    "[RetryHandler] 块 {} 长时间等待结束，重新加入下载队列。",
                    info_to_retry.id
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
        debug!("[RetryHandler] 检查是否有块准备好进行即时重试。");
        if let Some(failed_chunk) = self.retry_queue.front() {
            if failed_chunk.failure_time.elapsed() >= RETRY_DELAY {
                info!("[RetryHandler] 从重试队列中弹出块 {}。", failed_chunk.id);
                return self.retry_queue.pop_front();
            }
        }
        trace!("[RetryHandler] 没有准备好进行即时重试的块。");
        None
    }

    /// 当一个块最终下载成功时，清除其重试记录。
    pub fn on_download_complete(&mut self, id: &ChunkId) {
        debug!(
            "[RetryHandler] 块 {id} 下载成功，从重试跟踪中移除。",
            id = id
        );
        self.retry_attempts.remove(id);
    }

    /// 检查所有重试队列是否都为空。
    pub fn are_all_tasks_done(&self) -> bool {
        self.retry_queue.is_empty() && self.delayed_retry_queue.is_empty()
    }
}
