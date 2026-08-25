//! 提供工具函数，如获取文件信息和处理文件写入。

#[cfg(feature = "resume")]
use crate::resume::ResumeRecorder;
use crate::types::DownloadCmd;
use crate::types::{DownloadError, Result};
use faststr::FastStr;
use reqwest::Client;
use std::io;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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
#[::tracing::instrument(skip(client), fields(url = %url))]
pub async fn get_file_info(client: &Client, url: &str) -> Result<(u64, bool)> {
    use reqwest::StatusCode;
    use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};

    // 记录 HEAD 探测结果，但不直接作为 Range 判定依据；Accept-Ranges 缺失时仍可能支持 Range
    let mut head_size: Option<u64> = None;
    let mut head_support = false;
    if let Ok(resp) = client
        .head(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        let headers = resp.headers();
        if let Some(len_val) = headers.get(CONTENT_LENGTH)
            && let Ok(len_str) = len_val.to_str()
            && let Ok(content_length) = len_str.parse::<u64>()
        {
            head_size = Some(content_length);
            head_support = headers
                .get(ACCEPT_RANGES)
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"bytes"));
        }
    }

    ::tracing::debug!(head_size = ?head_size, head_support, "HEAD probe result");
    // 2. 范围 GET 探测：以 206/ Content-Range 为金标准，失败则回退 HEAD 避免 501 误判
    let range_resp = match client.get(url).header("Range", "bytes=0-0").send().await {
        Ok(resp) => {
            ::tracing::debug!(status = %resp.status(), headers = ?resp.headers(), "Range GET probe");
            resp
        },
        Err(e) => {
            ::tracing::warn!(error = %e, head_size = ?head_size, "Range GET failed, fallback to HEAD");
            if let Some(size) = head_size {
                return Ok((size, head_support));
            }
            return Err(DownloadError::MissingContentLength);
        }
    };

    let status = range_resp.status();
    let headers = range_resp.headers();
    if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
        if let Some(size) = head_size {
            ::tracing::debug!(size, "Range GET non-success, fallback to HEAD size");
            return Ok((size, head_support));
        }
        // 无 HEAD 回退则尝试从 Range 响应的 Content-Length 兜底
        if let Some(len_val) = headers.get(CONTENT_LENGTH)
            && let Ok(len_str) = len_val.to_str()
            && let Ok(content_length) = len_str.parse::<u64>()
        {
            ::tracing::info!(content_length, "Range probe fallback via Content-Length (no range support)");
            return Ok((content_length, false));
        }
        ::tracing::error!(status = %status, "probe failed: no Content-Length");
        return Err(DownloadError::MissingContentLength);
    }
    if status == StatusCode::PARTIAL_CONTENT {
        if let Some(cr) = headers.get(CONTENT_RANGE)
            && let Ok(crs) = cr.to_str()
        {
            if let Some(pos) = crs.rfind('/') {
                let total = &crs[pos + 1..].trim();
                if *total != "*"
                    && let Ok(content_length) = total.parse::<u64>()
                {
                    ::tracing::info!(content_length, support_ranges = true, "probe via 206 Content-Range");
                    return Ok((content_length, true));
                }
            }
        }
        // 206 但无 Content-Range，仍判支持 Range，用 HEAD 或 Content-Length 回退
        if let Some(size) = head_size {
            ::tracing::info!(size, "206 without Content-Range, fallback to HEAD size with range support");
            return Ok((size, true));
        }
        if let Some(len_val) = headers.get(CONTENT_LENGTH)
            && let Ok(len_str) = len_val.to_str()
            && let Ok(content_length) = len_str.parse::<u64>()
        {
            ::tracing::info!(content_length, "206 without Content-Range, fallback via Content-Length");
            return Ok((content_length, true));
        }
    }

    if let Some(cr) = headers.get(CONTENT_RANGE)
        && let Ok(crs) = cr.to_str()
    {
        // 某些服务返回 200 但仍带 Content-Range，同样判支持
        if let Some(pos) = crs.rfind('/') {
            let total = &crs[pos + 1..].trim();
            if *total != "*"
                && let Ok(content_length) = total.parse::<u64>()
            {
                ::tracing::info!(content_length, support_ranges = true, "probe via 200 Content-Range");
                return Ok((content_length, true));
            }
        }
    }

    // 3. 最终回退：优先 HEAD 的 size，否则 Range 响应的 Content-Length，保守判不支持
    if let Some(size) = head_size {
        ::tracing::info!(size, head_support, "probe fallback to HEAD");
        return Ok((size, head_support));
    }
    if let Some(len_val) = headers.get(CONTENT_LENGTH)
        && let Ok(len_str) = len_val.to_str()
        && let Ok(content_length) = len_str.parse::<u64>()
    {
        ::tracing::info!(content_length, "probe fallback via GET Content-Length (no range)");
        return Ok((content_length, false));
    }

    ::tracing::error!("probe failed: MissingContentLength");
    Err(DownloadError::MissingContentLength)
}
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
pub async fn file_writer_task(
    filepath: FastStr,
    size: u64,
) -> Result<(mpsc::Sender<DownloadCmd>, JoinHandle<()>)> {
    file_writer_task_impl(
        filepath,
        size,
        true,
        #[cfg(feature = "resume")]
        None,
    )
    .await
}

#[cfg(feature = "resume")]
pub async fn file_writer_task_with_resume(
    filepath: FastStr,
    size: u64,
    truncate: bool,
    resume_recorder: Option<ResumeRecorder>,
) -> Result<(mpsc::Sender<DownloadCmd>, JoinHandle<()>)> {
    file_writer_task_impl(filepath, size, truncate, resume_recorder).await
}

async fn file_writer_task_impl(
    filepath: FastStr,
    size: u64,
    truncate: bool,
    #[cfg(feature = "resume")] resume_recorder: Option<ResumeRecorder>,
) -> Result<(mpsc::Sender<DownloadCmd>, JoinHandle<()>)> {
    const WRITER_QUEUE_CAP: usize = 128;
    let (tx, mut rx) = mpsc::channel::<DownloadCmd>(WRITER_QUEUE_CAP);
    #[cfg(feature = "resume")]
    let mut resume_recorder = resume_recorder;

    // 打开（或创建）文件
    ::tracing::debug!(path = %filepath, size, truncate, "opening output file");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(&*filepath)
        .await?;
    // 预分配文件大小，防止磁盘空间不足，并可能提高写入性能
    file.set_len(size).await?;
    ::tracing::info!(path = %filepath, size, truncate, "output file ready (preallocated)");

    // 异步执行文件写入循环（带相邻段合并，128KiB 限）
    let writer_handle = tokio::spawn(async move {
        let mut pending: Option<(u64, Vec<u8>)> = None;
        const COALESCE_LIMIT: usize = 128 * 1024;

        while let Some(command) = rx.recv().await {
            match command {
                DownloadCmd::WriteFile { offset, data } => {
                    let data_len = data.len();
                    let can_coalesce = if let Some((p_off, p_buf)) = pending.as_ref() {
                        p_off + p_buf.len() as u64 == offset
                            && p_buf.len() + data_len <= COALESCE_LIMIT
                    } else {
                        false
                    };
                    if can_coalesce {
                        if let Some((_, p_buf)) = pending.as_mut() {
                            p_buf.extend_from_slice(&data);
                        }
                        continue;
                    }
                    // 先落盘之前的 pending
                    if let Some((p_off, p_buf)) = pending.take() {
                        if file.seek(io::SeekFrom::Start(p_off)).await.is_err()
                            || file.write_all(&p_buf).await.is_err()
                        {
                            ::tracing::error!(offset = p_off, len = p_buf.len(), "file writer write failed");
                            break;
                        }
                        // 确保数据落盘可见后再做哈希校验，避免同一 fd 读到旧数据
                        let _ = file.flush().await;
                        #[cfg(feature = "resume")]
                        if let Some(recorder) = resume_recorder.as_mut()
                            && let Err(e) = recorder
                                .record_write(&mut file, p_off, p_buf.len() as u64)
                                .await
                        {
                            ::tracing::error!(error = %e, offset = p_off, "resume metadata update failed");
                            break;
                        }
                        ::tracing::trace!(offset = p_off, len = p_buf.len(), "flushed coalesced write");
                    }
                    pending = Some((offset, data.to_vec()));
                }
                DownloadCmd::TerminateAll => {
                    ::tracing::debug!("file writer recv TerminateAll");
                    if let Some((p_off, p_buf)) = pending.take() {
                        if file.seek(io::SeekFrom::Start(p_off)).await.is_err()
                            || file.write_all(&p_buf).await.is_err()
                        {
                            ::tracing::error!(offset = p_off, len = p_buf.len(), "file writer final write failed");
                        } else {
                            let _ = file.flush().await;
                            #[cfg(feature = "resume")]
                            if let Some(recorder) = resume_recorder.as_mut() {
                                let _ = recorder
                                    .record_write(&mut file, p_off, p_buf.len() as u64)
                                    .await;
                            }
                            ::tracing::trace!(offset = p_off, len = p_buf.len(), "final pending flushed on TerminateAll");
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        // 通道关闭：落盘剩余 pending
        if let Some((p_off, p_buf)) = pending.take() {
            ::tracing::debug!(offset = p_off, len = p_buf.len(), "channel closed, flushing remaining pending");
            if file.seek(io::SeekFrom::Start(p_off)).await.is_ok()
                && file.write_all(&p_buf).await.is_ok()
            {
                let _ = file.flush().await;
                #[cfg(feature = "resume")]
                if let Some(recorder) = resume_recorder.as_mut() {
                    let _ = recorder
                        .record_write(&mut file, p_off, p_buf.len() as u64)
                        .await;
                }
            } else {
                ::tracing::error!(offset = p_off, len = p_buf.len(), "flush remaining pending failed");
            }
        }
        #[cfg(feature = "resume")]
        if let Some(recorder) = resume_recorder.as_mut() {
            let _ = recorder.flush().await;
            ::tracing::debug!("resume recorder flushed on writer exit");
        }
        let _ = file.flush().await;
        ::tracing::info!("file writer task exited");
    });

    Ok((tx, writer_handle))
}
