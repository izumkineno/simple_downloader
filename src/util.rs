//! 提供工具函数，如获取文件信息和处理文件写入。

use crate::types::{DownloadError, Result, SystemCommand};
use faststr::FastStr;
use log::{debug, error, info, trace};
use reqwest::Client;
use std::io;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::spawn;
use tokio::sync::mpsc;

/// 从 URL 检索文件元数据（大小和是否支持范围请求）。
pub(crate) async fn get_file_info(client: &Client, url: &str) -> Result<(u64, bool)> {
    use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};

    info!("尝试为 URL 获取文件信息: {}", url);

    // 尝试使用 HEAD 请求获取信息，效率最高
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

    // 如果 HEAD 失败或信息不全，尝试发送一个 0 字节的 Range 请求
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
                if let Ok(content_length) = &crs[pos + 1..].trim().parse::<u64>() {
                    return Ok((*content_length, true));
                }
            }
        }
    }

    // 作为最后的备用方案，检查普通 GET 请求的 Content-Length
    if let Some(len_val) = headers.get(CONTENT_LENGTH) {
        if let Ok(len_str) = len_val.to_str() {
            if let Ok(content_length) = len_str.parse::<u64>() {
                return Ok((content_length, false));
            }
        }
    }

    Err(DownloadError::MissingContentLength)
}

/// 创建并运行一个文件写入 Actor。
///
/// 这个 Actor 在一个独立的任务中运行，监听 `WriteFile` 命令，
/// 并将接收到的数据写入到指定文件的正确偏移位置。
///
/// # 参数
/// * `filepath`: 要写入的文件的路径。
/// * `size`: 文件的总大小，用于预分配空间。
/// * `queue_capacity`: 此 Actor 的消息信道容量。
///
/// # 返回
/// 一个 `Result`，其中包含用于与该 Actor 通信的信道发送端。
pub(crate) async fn writer_actor_task(
    filepath: FastStr,
    size: u64,
    queue_capacity: usize,
) -> Result<mpsc::Sender<SystemCommand>> {
    let (tx, mut rx) = mpsc::channel::<SystemCommand>(queue_capacity);

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&*filepath)
        .await?;
    // 预分配文件大小，可以减少碎片并提高性能
    file.set_len(size).await?;
    debug!("[WriterActor] 文件已打开并预分配大小。");

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
                _ => {} // 忽略其他命令
            }
        }
        info!("[WriterActor] 正在刷新文件并退出任务。");
        if let Err(e) = file.flush().await {
            error!("[WriterActor] 刷新文件失败: {}", e);
        }
    });

    Ok(tx)
}
