/// 提供工具函数，如获取文件信息和处理文件写入。
use crate::types::{DownloadError, Result, SystemCommand};
use faststr::FastStr;
use log::{debug, error, info, trace};
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};
use reqwest::{Client, RequestBuilder};
use std::io;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::spawn;
use tokio::sync::mpsc;

/// 从 URL 检索文件元数据（大小和是否支持范围请求）。
///
/// 它会按顺序尝试以下方法：
/// 1. 发送 `HEAD` 请求，这是最高效的方式。
/// 2. 如果 `HEAD` 失败或未提供 `Content-Length`，则尝试发送一个 `Range: bytes=0-0` 的 `GET` 请求，
///    从 `Content-Range` 头中解析总大小。
/// 3. 如果以上都失败，则作为最后的备用方案，检查普通 `GET` 请求的 `Content-Length`。
pub(crate) async fn get_file_info(req_builder: RequestBuilder) -> Result<(u64, bool)> {
    // 尝试使用 HEAD 请求。
    let rb = req_builder.try_clone().unwrap();
    if let Ok(resp) = rb.send().await.and_then(|r| r.error_for_status()) {
        let headers = resp.headers();
        if let Some(len_val) = headers.get(CONTENT_LENGTH) {
            if let Ok(len_str) = len_val.to_str() {
                if let Ok(content_length) = len_str.parse::<u64>() {
                    let accept_ranges = headers
                        .get(ACCEPT_RANGES)
                        .map_or(false, |v| v.as_bytes().eq_ignore_ascii_case(b"bytes"));
                    if content_length > 0 {
                        return Ok((content_length, accept_ranges));
                    }
                }
            }
        }
    }

    // 如果 HEAD 失败，尝试发送一个 0 字节的 Range 请求。
    let range_resp = req_builder
        .header("Range", "bytes=0-0")
        .send()
        .await?
        .error_for_status()?;
    let headers = range_resp.headers();
    if let Some(cr) = headers.get(CONTENT_RANGE) {
        if let Ok(crs) = cr.to_str() {
            if let Some(pos) = crs.rfind('/') {
                if let Ok(content_length) = &crs[pos + 1..].trim().parse::<u64>() {
                    if *content_length > 0 {
                        return Ok((*content_length, true));
                    }
                }
            }
        }
    }

    // 作为最后的备用方案，检查普通 GET 请求的 Content-Length。
    if let Some(len_val) = headers.get(CONTENT_LENGTH) {
        if let Ok(len_str) = len_val.to_str() {
            if let Ok(content_length) = len_str.parse::<u64>() {
                if content_length > 0 {
                    return Ok((content_length, false));
                }
            }
        }
    }

    Err(DownloadError::MissingContentLength)
}

/// 创建并运行一个文件写入 Actor。
///
/// 这个 Actor 在一个独立的 `tokio` 任务中运行，通过一个 mpsc 信道接收 `WriteFile` 命令，
/// 并将接收到的数据异步写入到指定文件的正确偏移位置。
///
/// # 参数
/// * `filepath`: 要写入的文件的路径。
/// * `size`: 文件的总大小，用于预分配磁盘空间以提高性能。
/// * `queue_capacity`: 此 Actor 的消息信道容量。
///
/// # 返回
///
/// 返回一个 `Result`，成功时包含用于与该 Actor 通信的信道发送端 `mpsc::Sender`。
pub(crate) async fn writer_actor_task(
    filepath: FastStr,
    size: u64,
    queue_capacity: usize,
) -> Result<mpsc::Sender<SystemCommand>> {
    let (tx, mut rx) = mpsc::channel::<SystemCommand>(queue_capacity);

    // 打开或创建文件。
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true) // 如果文件已存在，则清空。
        .open(&*filepath)
        .await?;
    // 预分配文件大小，可以减少文件系统碎片并可能提高写入性能。
    file.set_len(size).await?;
    debug!("[WriterActor] 文件已打开并预分配大小。");

    // 派生一个独立的任务来处理写入循环。
    spawn(async move {
        info!("[WriterActor] 进入主写入循环。");
        while let Some(command) = rx.recv().await {
            match command {
                SystemCommand::WriteFile { offset, data } => {
                    trace!(
                        "[WriterActor] 收到 WriteFile，偏移: {}, 大小: {}。",
                        offset,
                        data.len()
                    );
                    // 寻道到正确的偏移位置并写入数据。
                    if file.seek(io::SeekFrom::Start(offset)).await.is_err()
                        || file.write_all(&data).await.is_err()
                    {
                        error!("[WriterActor] 写入文件失败！循环终止。");
                        break;
                    }
                }
                SystemCommand::TerminateAll => {
                    info!("[WriterActor] 收到 TerminateAll，正在关闭。");
                    break;
                }
                _ => {} // 忽略其他命令。
            }
        }
        info!("[WriterActor] 正在刷新文件并退出任务。");
        // 任务结束前确保所有缓冲数据都已写入磁盘。
        if let Err(e) = file.flush().await {
            error!("[WriterActor] 刷新文件失败: {}", e);
        }
    });

    Ok(tx)
}
