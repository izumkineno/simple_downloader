//! 定义和管理单个下载块（chunk）的执行逻辑。

use crate::types::{ChunkId, DownloadCmd, DownloadInfo};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::RequestBuilder;
use tokio::sync::{broadcast, mpsc};

/// 定义一个块（chunk）的最小尺寸。
/// 当一个块被分割时，分割后的每个块的大小不能小于此值。
pub(crate) const MIN_CHUNK_SIZE: u64 = 1024 * 10; // 10 KB

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
pub(crate) async fn chunk_run(
    id: ChunkId,
    cmd_tx: mpsc::Sender<DownloadCmd>,
    mut bd_rx: broadcast::Receiver<DownloadCmd>,
    bd_tx: broadcast::Sender<DownloadInfo>,
    rb: RequestBuilder,
    start_byte: u64,
    end_byte: u64,
) {
    let mut end = end_byte;
    let mut offset = start_byte;
    let mut failed = false;

    // 构建 Range 请求头
    let range_header = format!("bytes={start_byte}-{end_byte}");
    let response = match rb
        .header("Range", range_header)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(resp) => resp,
        Err(e) => {
            let error_msg = format!("{e}");
            eprintln!("[Chunk {id}] 请求失败: {error_msg}");
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

            // 接收广播命令
            Ok(cmd) = bd_rx.recv() => match cmd {
                // 如果收到分割命令且目标是当前块
                DownloadCmd::BisectDownload { id: id_ } if id == id_ => {
                    let remaining = end.saturating_sub(offset);
                    // 如果剩余大小不足以分割成两个最小块，则忽略
                    if remaining < MIN_CHUNK_SIZE * 2 { continue; }

                    let midpoint = offset + remaining / 2;
                    let new_chunk_start = midpoint + 1;

                    // 广播“块已分割”事件，通知监控器创建新任务
                    if bd_tx.send(DownloadInfo::ChunkBisected {
                        original_id: id,
                        new_start: new_chunk_start,
                        new_end: end,
                    }).is_ok() {
                        println!("[Chunk {id}] 已分割。新范围: {offset}-{midpoint}");
                        // 更新当前块的结束位置
                        end = midpoint;
                    }
                }
                // 收到终止命令，退出循环
                DownloadCmd::TerminateAll => break,
                _ => {}
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

                    // 将数据发送给文件写入任务
                    if cmd_tx.send(DownloadCmd::WriteFile { offset, data: to_write }).await.is_err() {
                        let error_msg = format!("[Chunk {id}] 文件写入通道已关闭");
                        eprintln!("{error_msg}");
                        let _ = bd_tx.send(DownloadInfo::ChunkFailed { id, start: offset, end, error: error_msg });
                        failed = true;
                        break;
                    }

                    // 更新当前下载偏移量
                    offset = offset.saturating_add(write_len);

                    // 广播进度更新
                    let _ = bd_tx.send(DownloadInfo::ChunkProgress {
                        id,
                        start_byte,
                        end_byte: end,
                        downloaded: offset.saturating_sub(start_byte),
                    });

                    // 如果写入的数据小于接收到的数据块，说明已到达当前块的边界，终止下载
                    if write_len < remaining_chunk_len {
                        break;
                    }
                }
                Some(Err(e)) => {
                    let error_msg = format!("{e}");
                    eprintln!("[Chunk {id}] 下载流错误: {error_msg}");
                    let _ = bd_tx.send(DownloadInfo::ChunkFailed { id, start: offset, end, error: error_msg });
                    failed = true;
                    break;
                },
                // 流结束
                None => break,
            },
            // 所有分支都无法进行时退出
            else => break,
        }
    }

    if !failed {
        // 如果没有发生失败，则广播下载完成消息
        let _ = bd_tx.send(DownloadInfo::DownloadComplete(id));
    }
}
