//! 提供工具函数，如获取文件信息和处理文件写入。

use crate::types::DownloadCmd;
use crate::types::{DownloadError, Result};
use faststr::FastStr;
use log::{debug, error, info, trace, warn};
use reqwest::Client;
use std::io;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::spawn;
use tokio::sync::mpsc;

/// 从 URL 检索文件元数据（大小和是否支持范围请求）。
///
/// 它会按顺序尝试以下方法：
/// 1. 发送 `HEAD` 请求，检查 `Content-Length` 和 `Accept-Ranges` 头。
/// 2. 如果 `HEAD` 失败或信息不全，则发送一个 `GET` 请求，请求范围为 `bytes=0-0`，
///    并解析 `Content-Range` 头来获取总大小。
/// 3. 如果 `Content-Range` 也不可用，则回退到检查 `GET` 响应的 `Content-Length` 头。
///
/// # 返回
/// 一个元组 `(u64, bool)`，分别代表文件总大小和服务器是否支持范围请求。
pub(crate) async fn get_file_info(client: &Client, url: &str) -> Result<(u64, bool)> {
    use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};

    info!("尝试为 URL 获取文件信息: {}", url);

    // 1. 尝试 HEAD 请求
    debug!("步骤 1: 尝试 HEAD 请求。");
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
                    info!(
                        "HEAD 请求成功。长度: {}, Range 支持: {}.",
                        content_length, accept_ranges
                    );
                    return Ok((content_length, accept_ranges));
                }
            }
        }
    }

    // 2. 回退到范围 GET 请求
    debug!("步骤 2: HEAD 失败或信息不全，尝试使用 Range: bytes=0-0 的 GET 请求。");
    let range_resp = client
        .get(url)
        .header("Range", "bytes=0-0")
        .send()
        .await?
        .error_for_status()?;

    let headers = range_resp.headers();
    if let Some(cr) = headers.get(CONTENT_RANGE) {
        if let Ok(crs) = cr.to_str() {
            // Content-Range 格式通常是 "bytes 0-0/12345"
            if let Some(pos) = crs.rfind('/') {
                let total = &crs[pos + 1..].trim();
                if *total != "*" {
                    if let Ok(content_length) = total.parse::<u64>() {
                        info!("GET 与 Content-Range 成功。总大小: {}.", content_length);
                        return Ok((content_length, true)); // 如果有 Content-Range，说明支持范围请求
                    }
                }
            }
        }
    }

    // 3. 最终回退到 GET 响应的 Content-Length
    debug!("步骤 3: 未找到 Content-Range，回退到 GET 响应的 Content-Length。");
    if let Some(len_val) = headers.get(CONTENT_LENGTH) {
        if let Ok(len_str) = len_val.to_str() {
            if let Ok(content_length) = len_str.parse::<u64>() {
                info!(
                    "GET 与 Content-Length 成功。总大小: {}。假设不支持 range。",
                    content_length
                );
                // 此时无法确定是否支持范围请求，保守地返回 false
                return Ok((content_length, false));
            }
        }
    }

    error!("无法从 HEAD 或 GET 请求中确定文件大小。所有方法均已失败。");
    Err(DownloadError::MissingContentLength)
}

/// 创建并异步运行一个专门处理所有文件写入操作的任务。
///
/// 这种模式将所有磁盘 I/O 操作集中在一个任务中，避免了多个下载线程同时写入文件
/// 导致的竞争和性能问题。
///
/// # 参数
/// - `filepath`: 文件的保存路径。
/// - `size`: 文件的总大小，用于预分配文件空间。
///
/// # 返回
/// 一个 `mpsc::Sender<DownloadCmd>`，其他任务可以通过它发送 `WriteFile` 命令。
pub(crate) async fn file_writer_task(
    filepath: FastStr,
    size: u64,
) -> Result<mpsc::Sender<DownloadCmd>> {
    const WRITER_QUEUE_CAP: usize = 128;
    let (tx, mut rx) = mpsc::channel::<DownloadCmd>(WRITER_QUEUE_CAP);

    info!(
        "文件写入任务启动，目标路径: '{}'，预分配大小: {} 字节。",
        filepath, size
    );

    // 打开（或创建）文件
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true) // 如果文件已存在，则清空
        .open(&*filepath)
        .await?;
    // 预分配文件大小，防止磁盘空间不足，并可能提高写入性能
    file.set_len(size).await?;
    debug!("[FileWriter] 文件已打开并预分配大小。");

    // 异步执行文件写入循环
    spawn(async move {
        info!("[FileWriter] 进入主写入循环。");
        while let Some(command) = rx.recv().await {
            trace!("[FileWriter] 等待下一条命令。");
            match command {
                DownloadCmd::WriteFile { offset, data } => {
                    trace!(
                        "[FileWriter] 收到 WriteFile 命令，offset: {}, 大小: {}。",
                        offset,
                        data.len()
                    );
                    // 移动到指定偏移量并写入数据
                    if file.seek(io::SeekFrom::Start(offset)).await.is_err()
                        || file.write_all(&data).await.is_err()
                    {
                        error!("[FileWriter] 写入文件失败！循环终止。");
                        break; // 发生错误时退出
                    }
                }
                DownloadCmd::TerminateAll => {
                    info!("[FileWriter] 收到 TerminateAll 命令，正在关闭。");
                    break;
                }
                _ => {}
            }
        }
        info!("[FileWriter] 正在刷新文件并退出任务。");
        // 任务结束前确保所有缓冲数据都已写入磁盘
        let _ = file.flush().await;
    });

    Ok(tx)
}
