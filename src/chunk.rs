//! 定义和管理单个下载块（chunk）的执行逻辑。

use crate::types::{ChunkId, DownloadCmd, DownloadInfo};
use crate::limiter::RateLimiter;
#[cfg(feature = "rate-limit")]
use std::num::NonZeroU32;
use std::sync::Arc;
use crate::util::ensure_user_agent;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::RequestBuilder;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// 定义一个块（chunk）的最小尺寸。
/// 当一个块被分割时，分割后的每个块的大小不能小于此值。
pub(crate) const MIN_CHUNK_SIZE: u64 = 1024 * 10; // 10 KB

fn split_range(offset: u64, end: u64) -> Option<(u64, u64)> {
    let remaining_bytes = end.saturating_sub(offset).saturating_add(1);
    if remaining_bytes < MIN_CHUNK_SIZE * 2 {
        return None;
    }

    let left_chunk_size = remaining_bytes / 2;
    let midpoint = offset + left_chunk_size - 1;
    Some((midpoint, midpoint + 1))
}

/// 单个下载块（worker）的执行任务。
///
/// 这个异步函数负责下载文件的一个特定范围（从 `start_byte` 到 `end_byte`）。
/// 它会监听控制命令（如分割任务），并将下载的数据块和状态更新发送出去。
///
/// # 参数
/// - `id`: 此块的唯一标识符。
/// - `cmd_tx`: 用于向文件写入任务发送 `WriteFile` 命令的发送端。
/// - `bd_rx`: 用于接收来自监控器或其他任务的广播命令（如 `BisectDownload` 或 `TerminateAll`）的接收端。
/// - `bd_tx`: 用于广播此块的状态更新（如进度、失败、完成）的发送端。
/// - `rb`: 一个 `reqwest::RequestBuilder`，用于创建下载请求。
/// - `start_byte`: 此块下载的起始字节位置。
/// - `end_byte`: 此块下载的结束字节位置。
#[allow(clippy::too_many_arguments, unused_variables)]
#[::tracing::instrument(
    skip(cmd_tx, bd_rx, bd_tx, rb, global_limiter, per_source_limiter),
    fields(chunk_id = id, start_byte = start_byte, end_byte = end_byte, size = end_byte.saturating_sub(start_byte).saturating_add(1))
)]
pub async fn chunk_run(
    id: ChunkId,
    cmd_tx: mpsc::Sender<DownloadCmd>,
    mut bd_rx: broadcast::Receiver<DownloadCmd>,
    bd_tx: broadcast::Sender<DownloadInfo>,
    rb: RequestBuilder,
    start_byte: u64,
    end_byte: u64,
    global_limiter: Option<Arc<RateLimiter>>,
    per_source_limiter: Option<Arc<RateLimiter>>,
) {
    let mut end = end_byte;
    let mut offset = start_byte;
    let mut failed = false;
    // 节流：64KiB 或 50ms 聚合一次，避免高频 broadcast 导致 Lagged
    const PROGRESS_THROTTLE_BYTES: u64 = 64 * 1024;
    const PROGRESS_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);
    let mut last_progress = Instant::now();
    let mut last_reported = 0u64; // 已上报的 downloaded

    ::tracing::debug!(
        chunk_id = id,
        start_byte,
        end_byte,
        size = end_byte.saturating_sub(start_byte).saturating_add(1),
        "chunk start"
    );
    // 构建 Range 请求头，确保携带 User-Agent（保留用户自定义，仅缺失时注入默认值）
    let range_header = format!("bytes={start_byte}-{end_byte}");
    let response = match ensure_user_agent(rb.header("Range", range_header.clone()))
        .send()
        .await
    {
        Ok(resp) => {
            ::tracing::debug!(
                chunk_id = id,
                status = %resp.status(),
                range = %range_header,
                headers = ?resp.headers(),
                "chunk response"
            );
            // P0-01: 校验 HTTP 状态与 Content-Range 一致性
            let status = resp.status();
            if status == reqwest::StatusCode::PARTIAL_CONTENT {
                // 必须携带 Content-Range 且与请求范围一致
                let cr_opt = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok());
                match cr_opt {
                    Some(cr_str) => {
                        if let Some((cr_start, cr_end, _cr_total)) =
                            crate::util::parse_content_range(cr_str)
                        {
                            if cr_start != start_byte || cr_end != end_byte {
                                let error_msg = format!(
                                    "Content-Range mismatch: expected bytes {start_byte}-{end_byte}/*, got {cr_str}"
                                );
                                ::tracing::error!(chunk_id = id, range = %range_header, content_range = %cr_str, error = %error_msg, "range mismatch");
                                let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                                    id,
                                    start: start_byte,
                                    end,
                                    error: error_msg,
                                });
                                return;
                            }
                        } else {
                            let error_msg =
                                format!("invalid Content-Range header for 206: {cr_str}");
                            ::tracing::error!(chunk_id = id, range = %range_header, error = %error_msg, "invalid Content-Range");
                            let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                                id,
                                start: start_byte,
                                end,
                                error: error_msg,
                            });
                            return;
                        }
                    }
                    None => {
                        let error_msg =
                            "missing Content-Range header for 206 Partial Content".to_string();
                        ::tracing::error!(chunk_id = id, range = %range_header, error = %error_msg, "missing Content-Range");
                        let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                            id,
                            start: start_byte,
                            end,
                            error: error_msg,
                        });
                        return;
                    }
                }
            } else if status == reqwest::StatusCode::OK {
                // 仅允许单段全量降级：start_byte 必须为 0
                if start_byte != 0 {
                    let error_msg = format!(
                        "server returned 200 OK but Range requested {start_byte}-{end_byte}, only single-segment full download (0-*) is allowed to downgrade"
                    );
                    ::tracing::error!(chunk_id = id, range = %range_header, status = %status, error = %error_msg, "range ignored");
                    let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                        id,
                        start: start_byte,
                        end,
                        error: error_msg,
                    });
                    return;
                }
                ::tracing::warn!(chunk_id = id, range = %range_header, status = %status, "server ignored Range, returned 200 OK, downgrading to single-stream full download");
            } else {
                let cr_info = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let error_msg = format!(
                    "unexpected status {} for Range request bytes={start_byte}-{end_byte}, Content-Range: {cr_info}",
                    status
                );
                ::tracing::error!(chunk_id = id, range = %range_header, status = %status, error = %error_msg, "unexpected status");
                let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                    id,
                    start: start_byte,
                    end,
                    error: error_msg,
                });
                return;
            }
            resp
        }
        Err(e) => {
            let error_msg = format!("{e}");
            ::tracing::error!(chunk_id = id, range = %range_header, error = %error_msg, "chunk request failed");
            // 发送块失败信息
            let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                id,
                start: start_byte,
                end,
                error: error_msg,
            });
            return;
        }
    };

    // 获取响应的字节流
    let mut stream = response.bytes_stream();

    loop {
        tokio::select! {
            // `biased` 确保优先处理控制命令，使得系统能快速响应如“分割”或“终止”等操作。
            biased;

            // 接收广播命令；单独处理 Lagged，避免因背压误终止下载
            result = bd_rx.recv() => match result {
                Ok(cmd) => match cmd {
                    // 如果收到分割命令且目标是当前块
                    DownloadCmd::BisectDownload { id: id_ } if id == id_ => {
                        ::tracing::debug!(chunk_id = id, offset, end, remaining = end.saturating_sub(offset).saturating_add(1), "recv BisectDownload");
                        let Some((midpoint, new_chunk_start)) = split_range(offset, end) else {
                            ::tracing::debug!(chunk_id = id, remaining = end.saturating_sub(offset).saturating_add(1), "bisect rejected: remaining < 2*MIN_CHUNK");
                            continue;
                        };

                        // 广播“块已分割”事件，通知监控器创建新任务
                        if bd_tx.send(DownloadInfo::ChunkBisected {
                            original_id: id,
                            new_start: new_chunk_start,
                            new_end: end,
                        }).is_ok() {
                            ::tracing::info!(chunk_id = id, kept_range = format!("{offset}-{midpoint}"), new_range = format!("{new_chunk_start}-{end}"), "chunk bisected");
                            ::tracing::debug!(chunk_id = id, offset, midpoint, new_start = new_chunk_start, new_end = end, "bisected detail");
                            // 更新当前块的结束位置
                            end = midpoint;
                        } else {
                            ::tracing::warn!(chunk_id = id, "bisected send failed (no monitor)");
                        }
                    }
                    // 收到终止命令，退出循环
                    DownloadCmd::TerminateAll => {
                        ::tracing::debug!(chunk_id = id, "recv TerminateAll, exiting");
                        break;
                    },
                    _ => {}
                },
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    ::tracing::warn!(chunk_id = id, skipped, "broadcast lagged, skip control commands");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    ::tracing::debug!(chunk_id = id, "broadcast closed, exiting");
                    break;
                },
            },
            // 从网络流中获取下一个数据块
            chunk_result = stream.next() => match chunk_result {
                Some(Ok(mut chunk)) => {
                    if offset > end { break; }

                    let remaining_chunk_len = chunk.len() as u64;
                    // 计算当前块允许写入的最大长度
                    let allowed = end.saturating_sub(offset).saturating_add(1);
                    if allowed == 0 { break; }

                    // 确定实际要写入的长度
                    let write_len = std::cmp::min(allowed, remaining_chunk_len);
                    let to_write: Bytes = if write_len as usize == chunk.len() {
                        chunk
                    } else {
                        // 如果网络数据块超出了范围，则进行切分
                        chunk.split_to(write_len as usize)
                    };

                    // 限速：全局+分源两级串联，32-64KiB 批量，禁止 jitter — 双 limiter 并发 join 取 max
                    #[cfg(feature = "rate-limit")]
                    {
                        if write_len > 0 {
                            // 将 write_len 按 65536 批量切分，避免 1-4KiB 小片高频 acquire
                            let mut remaining = write_len as u32;
                            while remaining > 0 {
                                let batch = std::cmp::min(remaining, 64*1024) as u32;
                                let nz = NonZeroU32::new(batch).unwrap();
                                match (per_source_limiter.as_ref(), global_limiter.as_ref()) {
                                    (Some(per), Some(glob)) => {
                                        tokio::join!(per.acquire(nz), glob.acquire(nz));
                                    }
                                    (Some(per), None) => per.acquire(nz).await,
                                    (None, Some(glob)) => glob.acquire(nz).await,
                                    (None, None) => {}
                                }
                                remaining -= batch;
                            }
                        }
                    }
                    // 将数据发送给文件写入任务
                    if cmd_tx.send(DownloadCmd::WriteFile { offset, data: to_write }).await.is_err() {
                        let error_msg = format!("[Chunk {id}] 文件写入通道已关闭");
                        ::tracing::error!(chunk_id = id, "file writer channel closed");
                        let actual = offset.saturating_sub(start_byte);
                        if actual != last_reported {
                            let _ = bd_tx.send(DownloadInfo::ChunkProgress { id, start_byte, end_byte: end, downloaded: actual });
                        }
                        let _ = bd_tx.send(DownloadInfo::ChunkFailed { id, start: offset, end, error: error_msg });
                        failed = true;
                        break;
                    }
                    // 更新当前下载偏移量
                    offset = offset.saturating_add(write_len);

                    // 节流广播：仅当累积足够或超时或到达边界时发送，避免高频 Lagged
                    let downloaded = offset.saturating_sub(start_byte);
                    let should_send = downloaded.saturating_sub(last_reported)
                        >= PROGRESS_THROTTLE_BYTES
                        || last_progress.elapsed() >= PROGRESS_THROTTLE_INTERVAL
                        || offset >= end
                        || write_len < remaining_chunk_len;
                    if should_send {
                        let _ = bd_tx.send(DownloadInfo::ChunkProgress {
                            id,
                            start_byte,
                            end_byte: end,
                            downloaded,
                        });
                        last_reported = downloaded;
                        last_progress = Instant::now();
                    }

                    // 如果写入的数据小于接收到的数据块，说明已到达当前块的边界，终止下载
                    if write_len < remaining_chunk_len {
                        ::tracing::debug!(chunk_id = id, offset, end, "reached range boundary, break");
                        break;
                    }
                }
                Some(Err(e)) => {
                    let error_msg = format!("{e}");
                    ::tracing::error!(chunk_id = id, error = %error_msg, "download stream error");
                    let actual = offset.saturating_sub(start_byte);
                    if actual != last_reported {
                        let _ = bd_tx.send(DownloadInfo::ChunkProgress { id, start_byte, end_byte: end, downloaded: actual });
                    }
                    let _ = bd_tx.send(DownloadInfo::ChunkFailed { id, start: offset, end, error: error_msg });
                    failed = true;
                    break;
                },
                // 流结束：P0-02 完整性门，offset 必须到达 end+1 否则判 Early-EOF
                None => {
                    ::tracing::debug!(chunk_id = id, offset, end, "stream exhausted");
                    if offset != end.saturating_add(1) {
                        // bisect 后 end 已缩小，此处按当前 end 判定；offset>end 视为已完成不判失败
                        if offset <= end {
                            let expected = end.saturating_sub(start_byte).saturating_add(1);
                            let got = offset.saturating_sub(start_byte);
                            let error_msg = format!(
                                "early EOF: expected {} bytes ({}-{}), got {}",
                                expected, start_byte, end, got
                            );
                            ::tracing::error!(chunk_id = id, offset, end, error = %error_msg, "early EOF");
                            let actual = offset.saturating_sub(start_byte);
                            if actual != last_reported {
                                let _ = bd_tx.send(DownloadInfo::ChunkProgress { id, start_byte, end_byte: end, downloaded: actual });
                            }
                            let _ = bd_tx.send(DownloadInfo::ChunkFailed {
                                id,
                                start: offset,
                                end,
                                error: error_msg,
                            });
                            failed = true;
                        }
                    }
                    break;
                },
            },
            // 所有分支都无法进行时退出
            else => break,
        }
    }
    ::tracing::debug!(
        chunk_id = id,
        offset,
        end,
        failed,
        downloaded = offset.saturating_sub(start_byte),
        "chunk exit"
    );
    // 收尾：若最后一段被节流未发送，补一次最终进度，避免 Monitor 统计低估
    if !failed {
        let final_downloaded = offset.saturating_sub(start_byte);
        if final_downloaded != last_reported {
            ::tracing::trace!(
                chunk_id = id,
                final_downloaded,
                total = end.saturating_sub(start_byte).saturating_add(1),
                "final progress補發"
            );
            let _ = bd_tx.send(DownloadInfo::ChunkProgress {
                id,
                start_byte,
                end_byte: end,
                downloaded: final_downloaded,
            });
        }
        // 如果没有发生失败，则广播下载完成消息
        ::tracing::debug!(chunk_id = id, "DownloadComplete");
        let _ = bd_tx.send(DownloadInfo::DownloadComplete(id));
    } else {
        ::tracing::warn!(chunk_id = id, "chunk exit with failure");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_range_allows_exactly_two_minimum_chunks() {
        let end = (MIN_CHUNK_SIZE * 2) - 1;

        assert_eq!(
            split_range(0, end),
            Some((MIN_CHUNK_SIZE - 1, MIN_CHUNK_SIZE))
        );
    }

    #[test]
    fn split_range_rejects_ranges_smaller_than_two_minimum_chunks() {
        let end = (MIN_CHUNK_SIZE * 2) - 2;

        assert_eq!(split_range(0, end), None);
    }
}
