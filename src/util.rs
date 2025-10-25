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
                if let Ok(content_length) = &crs[pos + 1..].trim().parse::<u64>() {
                    return Ok((*content_length, true));
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

/// 创建并运行一个文件写入 Actor。
pub(crate) async fn writer_actor_task(
    filepath: FastStr,
    size: u64,
) -> Result<mpsc::Sender<SystemCommand>> {
    const WRITER_QUEUE_CAP: usize = 128;
    let (tx, mut rx) = mpsc::channel::<SystemCommand>(WRITER_QUEUE_CAP);

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&*filepath)
        .await?;
    file.set_len(size).await?;
    debug!("[WriterActor] 文件已打开并预分配大小。");

    spawn(async move {
        info!("[WriterActor] 进入主写入循环。");
        while let Some(command) = rx.recv().await {
            match command {
                SystemCommand::WriteFile { offset, data } => {
                    trace!(
                        "[WriterActor] 收到 WriteFile，offset: {}, 大小: {}。",
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
                _ => {}
            }
        }
        info!("[WriterActor] 正在刷新文件并退出任务。");
        let _ = file.flush().await;
    });

    Ok(tx)
}
