//! 提供工具函数，如获取文件信息和处理文件写入。

#[cfg(feature = "resume")]
use crate::resume::ResumeRecorder;
use crate::types::DownloadCmd;
use crate::types::{DownloadError, Result};
use faststr::FastStr;
use reqwest::header::{HeaderValue, USER_AGENT};
use reqwest::{Client, RequestBuilder};
use std::io;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// 确保请求携带 `User-Agent`，若 `Client` 默认头与 `RequestBuilder` 均未设置则注入 crate 默认 UA。
/// 优先保留用户显式配置（client 默认头或已设置的 header），仅在缺失时回退。
pub(crate) fn ensure_user_agent(rb: RequestBuilder) -> RequestBuilder {
    // RequestBuilder 未暴露已设置 header 的直接查询，借助 try_clone + build 探测
    if let Some(cloned) = rb.try_clone() {
        if let Ok(req) = cloned.build() {
            if req.headers().contains_key(USER_AGENT) {
                return rb;
            }
        }
    }
    // try_clone 失败（如不可克隆 body）或探测到缺失则注入
    // HeaderValue::from_static 对 DEFAULT_USER_AGENT 是合法的（仅 ascii）
    match HeaderValue::from_str(crate::DEFAULT_USER_AGENT) {
        Ok(v) => rb.header(USER_AGENT, v),
        Err(_) => rb.header(USER_AGENT, crate::DEFAULT_USER_AGENT),
    }
}

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
    if let Ok(resp) = ensure_user_agent(client.head(url))
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
    let range_resp = match ensure_user_agent(client.get(url).header("Range", "bytes=0-0"))
        .send()
        .await
    {
        Ok(resp) => {
            ::tracing::debug!(status = %resp.status(), headers = ?resp.headers(), "Range GET probe");
            resp
        }
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
        // 416 特殊处理：尝试从 Content-Range: bytes */<total> 解析文件大小
        if status == StatusCode::RANGE_NOT_SATISFIABLE
            && let Some(cr) = headers.get(CONTENT_RANGE)
            && let Ok(crs) = cr.to_str()
            && let Some((_, _, total)) = crate::util::parse_content_range(crs)
            && total != 0
        {
            ::tracing::info!(total, "probe via 416 Content-Range");
            return Ok((total, true));
        }
        if let Some(size) = head_size {
            ::tracing::debug!(size, "Range GET non-success, fallback to HEAD size");
            return Ok((size, head_support));
        }
        // 4xx/5xx 且无 HEAD 回退时，不应把错误响应体的 Content-Length 当作文件大小（例如 400 "No userAgent" 27B）
        // 仅对 2xx 的兜底在后续分支处理，此处直接失败以触发 streaming 回退或上层错误
        ::tracing::error!(status = %status, "probe failed: non-success without HEAD fallback");
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
                    ::tracing::info!(
                        content_length,
                        support_ranges = true,
                        "probe via 206 Content-Range"
                    );
                    return Ok((content_length, true));
                }
            }
        }
        // 206 但无 Content-Range，仍判支持 Range，用 HEAD 或 Content-Length 回退
        if let Some(size) = head_size {
            ::tracing::info!(
                size,
                "206 without Content-Range, fallback to HEAD size with range support"
            );
            return Ok((size, true));
        }
        if let Some(len_val) = headers.get(CONTENT_LENGTH)
            && let Ok(len_str) = len_val.to_str()
            && let Ok(content_length) = len_str.parse::<u64>()
        {
            ::tracing::info!(
                content_length,
                "206 without Content-Range, fallback via Content-Length"
            );
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
                ::tracing::info!(
                    content_length,
                    support_ranges = true,
                    "probe via 200 Content-Range"
                );
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
        ::tracing::info!(
            content_length,
            "probe fallback via GET Content-Length (no range)"
        );
        return Ok((content_length, false));
    }

    ::tracing::error!("probe failed: MissingContentLength");
    Err(DownloadError::MissingContentLength)
}

/// 解析 `Content-Range` 头，支持 `bytes <start>-<end>/<total>` 及 `bytes */<total>` 形式。
///
/// - 正常范围：`bytes 0-44/45` -> `Some((0,44,45))`
/// - 通配总大小：`bytes 0-44/*` -> `Some((0,44,0))` (total 未知以 0 表示)
/// - 416 形态：`bytes */1234` -> `Some((0,0,1234))`
///
/// 大小写不敏感地匹配 `bytes ` 前缀，失败返回 `None`。
pub(crate) fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    let header = header.trim();
    // case-insensitive prefix "bytes "
    if header.len() < 6 || !header[..6].eq_ignore_ascii_case("bytes ") {
        return None;
    }
    let rest = header[6..].trim();
    let slash_pos = rest.rfind('/')?;
    let range_part = rest[..slash_pos].trim();
    let total_part = rest[slash_pos + 1..].trim();

    // 解析 total： "*" 表示未知，用 0 占位；否则解析数字
    let total: u64 = if total_part == "*" {
        0
    } else {
        total_part.parse::<u64>().ok()?
    };

    // 416 形态：range_part == "*"
    if range_part == "*" {
        // total 必须为有效数字（非 "*"）
        if total_part == "*" {
            return None;
        }
        return Some((0, 0, total));
    }

    // 正常形态：range_part = "start-end"
    let dash_pos = range_part.find('-')?;
    let start_str = range_part[..dash_pos].trim();
    let end_str = range_part[dash_pos + 1..].trim();
    let start: u64 = start_str.parse::<u64>().ok()?;
    let end: u64 = end_str.parse::<u64>().ok()?;
    if start > end {
        return None;
    }
    Some((start, end, total))
}
///
/// 这种模式将所有磁盘 I/O 操作集中在一个任务中，避免了多个下载线程同时写入文件
/// 导致的竞争和性能问题。
///
/// # 参数
/// - `filepath`: 文件的保存路径。
/// - `size`: 文件总大小，仅用于日志，不用于预分配文件空间。
///
/// # 返回
/// 一个 `mpsc::Sender<DownloadCmd>`，其他任务可以通过它发送 `WriteFile` 命令。
pub async fn file_writer_task(
    filepath: FastStr,
    size: u64,
) -> Result<(
    mpsc::Sender<DownloadCmd>,
    JoinHandle<std::result::Result<(), DownloadError>>,
)> {
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
) -> Result<(
    mpsc::Sender<DownloadCmd>,
    JoinHandle<std::result::Result<(), DownloadError>>,
)> {
    file_writer_task_impl(filepath, size, truncate, resume_recorder).await
}

async fn file_writer_task_impl(
    filepath: FastStr,
    size: u64,
    truncate: bool,
    #[cfg(feature = "resume")] resume_recorder: Option<ResumeRecorder>,
) -> Result<(
    mpsc::Sender<DownloadCmd>,
    JoinHandle<std::result::Result<(), DownloadError>>,
)> {
    const WRITER_QUEUE_CAP: usize = 128;
    let (tx, mut rx) = mpsc::channel::<DownloadCmd>(WRITER_QUEUE_CAP);
    #[cfg(feature = "resume")]
    let mut resume_recorder = resume_recorder;

    // P0-4 streaming: only ensure parent dir exists, no preallocation (Contrarian)
    if let Some(parent) = std::path::Path::new(&*filepath).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    ::tracing::debug!(path = %filepath, size, truncate, "opening output file (streaming)");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(&*filepath)
        .await?;
    ::tracing::info!(path = %filepath, size, truncate, "output file ready (streaming, no preallocation)");

    // 异步执行文件写入循环（带相邻段合并，128KiB 限）—— P0-1 修复：所有 seek/write/flush/record 错误回传
    let writer_handle = tokio::spawn(async move {
        let mut pending: Option<(u64, Vec<u8>)> = None;
        const COALESCE_LIMIT: usize = 128 * 1024;
        let mut writer_err: Option<DownloadError> = None;

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
                        if let Err(e) = file.seek(io::SeekFrom::Start(p_off)).await {
                            ::tracing::error!(offset = p_off, len = p_buf.len(), error = %e, "file writer seek failed");
                            writer_err = Some(DownloadError::Io(e));
                            break;
                        }
                        if let Err(e) = file.write_all(&p_buf).await {
                            ::tracing::error!(offset = p_off, len = p_buf.len(), error = %e, "file writer write failed");
                            writer_err = Some(DownloadError::Io(e));
                            break;
                        }
                        if let Err(e) = file.flush().await {
                            ::tracing::error!(offset = p_off, error = %e, "file writer flush failed");
                            writer_err = Some(DownloadError::Io(e));
                            break;
                        }
                        #[cfg(feature = "resume")]
                        if let Some(recorder) = resume_recorder.as_mut()
                            && let Err(e) = recorder
                                .record_write(&mut file, p_off, p_buf.len() as u64)
                                .await
                        {
                            ::tracing::error!(error = %e, offset = p_off, "resume metadata update failed");
                            writer_err = Some(e);
                            break;
                        }
                        ::tracing::trace!(
                            offset = p_off,
                            len = p_buf.len(),
                            "flushed coalesced write"
                        );
                    }
                    pending = Some((offset, data.to_vec()));
                }
                DownloadCmd::TerminateAll => {
                    ::tracing::debug!("file writer recv TerminateAll");
                    if let Some((p_off, p_buf)) = pending.take() {
                        if let Err(e) = file.seek(io::SeekFrom::Start(p_off)).await {
                            ::tracing::error!(offset = p_off, len = p_buf.len(), error = %e, "file writer final seek failed");
                            writer_err = Some(DownloadError::Io(e));
                        } else if let Err(e) = file.write_all(&p_buf).await {
                            ::tracing::error!(offset = p_off, len = p_buf.len(), error = %e, "file writer final write failed");
                            writer_err = Some(DownloadError::Io(e));
                        } else if let Err(e) = file.flush().await {
                            ::tracing::error!(offset = p_off, error = %e, "file writer final flush failed");
                            writer_err = Some(DownloadError::Io(e));
                        } else {
                            #[cfg(feature = "resume")]
                            if let Some(recorder) = resume_recorder.as_mut()
                                && let Err(e) = recorder
                                    .record_write(&mut file, p_off, p_buf.len() as u64)
                                    .await
                            {
                                ::tracing::error!(error = %e, offset = p_off, "resume metadata update failed on TerminateAll");
                                writer_err = Some(e);
                            }
                            if writer_err.is_none() {
                                ::tracing::trace!(
                                    offset = p_off,
                                    len = p_buf.len(),
                                    "final pending flushed on TerminateAll"
                                );
                            }
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        // 通道关闭：落盘剩余 pending（仅当之前未出错）
        if writer_err.is_none() {
            if let Some((p_off, p_buf)) = pending.take() {
                ::tracing::debug!(
                    offset = p_off,
                    len = p_buf.len(),
                    "channel closed, flushing remaining pending"
                );
                if let Err(e) = file.seek(io::SeekFrom::Start(p_off)).await {
                    ::tracing::error!(offset = p_off, len = p_buf.len(), error = %e, "flush remaining pending seek failed");
                    writer_err = Some(DownloadError::Io(e));
                } else if let Err(e) = file.write_all(&p_buf).await {
                    ::tracing::error!(offset = p_off, len = p_buf.len(), error = %e, "flush remaining pending write failed");
                    writer_err = Some(DownloadError::Io(e));
                } else if let Err(e) = file.flush().await {
                    ::tracing::error!(offset = p_off, error = %e, "flush remaining pending flush failed");
                    writer_err = Some(DownloadError::Io(e));
                } else {
                    #[cfg(feature = "resume")]
                    if let Some(recorder) = resume_recorder.as_mut()
                        && let Err(e) = recorder
                            .record_write(&mut file, p_off, p_buf.len() as u64)
                            .await
                    {
                        ::tracing::error!(error = %e, offset = p_off, "resume metadata update failed on channel close");
                        writer_err = Some(e);
                    }
                }
            }
        } else if let Some((p_off, p_buf)) = pending.take() {
            ::tracing::warn!(
                offset = p_off,
                len = p_buf.len(),
                "discarding pending due to prior writer error"
            );
        }
        #[cfg(feature = "resume")]
        if let Some(recorder) = resume_recorder.as_mut() {
            if let Err(e) = recorder.flush().await {
                ::tracing::error!(error = %e, "resume recorder final flush failed");
                if writer_err.is_none() {
                    writer_err = Some(e);
                }
            } else {
                ::tracing::debug!("resume recorder flushed on writer exit");
            }
        }
        if writer_err.is_none() {
            if let Err(e) = file.flush().await {
                ::tracing::error!(error = %e, "final file flush failed");
                writer_err = Some(DownloadError::Io(e));
            }
        }
        ::tracing::info!("file writer task exited");
        match writer_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    });

    Ok((tx, writer_handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn writer_truncate_true_removes_stale_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        tokio::fs::write(&path, b"stale tail").await.unwrap();

        let (tx, handle) = file_writer_task(FastStr::from(path.to_string_lossy().to_string()), 3)
            .await
            .unwrap();
        tx.send(DownloadCmd::WriteFile {
            offset: 0,
            data: Bytes::from_static(b"new"),
        })
        .await
        .unwrap();
        tx.send(DownloadCmd::TerminateAll).await.unwrap();
        handle.await.unwrap().unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new");
    }
    #[cfg(feature = "resume")]
    #[tokio::test]
    async fn writer_truncate_false_preserves_existing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.bin");
        tokio::fs::write(&path, b"old tail").await.unwrap();

        let (tx, handle) = file_writer_task_with_resume(
            FastStr::from(path.to_string_lossy().to_string()),
            3,
            false,
            None,
        )
        .await
        .unwrap();
        tx.send(DownloadCmd::WriteFile {
            offset: 0,
            data: Bytes::from_static(b"new"),
        })
        .await
        .unwrap();
        tx.send(DownloadCmd::TerminateAll).await.unwrap();
        handle.await.unwrap().unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new tail");
    }
}
