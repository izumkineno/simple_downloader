//! 回归：Range 响应被服务端截断（trickle/CDN 掐连接）时，失败块前缀落账 + 续传剩余区间
//! 必须恰好铺满原区间一次。修前 ledger 少记前缀字节 → is_download_finished 永假 →
//! monitor 死胡同逃生注入假永久失败 → 任务失败重来（实测差 32768B）。

#![cfg(feature = "resume")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use simple_downloader::{DownloadInfo, Downloader};
use tokio::sync::broadcast;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const FILE_SIZE: usize = 128 * 1024;
const CUT_AT: usize = 32 * 1024;

fn deterministic_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
}

/// 最小 HTTP/1.1 服务器：支持 HEAD 与 bytes=a-b 的 206，并「只截断第一次」大区间响应，
/// 制造一次 early EOF 迫使 chunk 失败→重试续传。
async fn spawn_cut_once_server(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let shared = Arc::new(body);
    let cut_served = Arc::new(AtomicBool::new(false));

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let body = Arc::clone(&shared);
            let cut_served = Arc::clone(&cut_served);
            tokio::spawn(async move {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let method = head.split_whitespace().next().unwrap_or("").to_owned();
                let range = head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("range")
                        .then(|| parse_range(value.trim()))
                        .flatten()
                });
                let size = body.len();

                if method == "HEAD" {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                    return;
                }

                let (start, end) = range.unwrap_or((0, size as u64 - 1));
                let end = end.min(size as u64 - 1);
                let start = start.min(end);
                let range_len = (end - start + 1) as usize;
                // 只截断第一次「带 Range 的真实大区间」响应：探测用的无 Range 整包 GET
                // 与 bytes=0-0 探测都不受影响，否则标志位会被探测吃掉。
                let truncate =
                    range.is_some() && range_len > CUT_AT && !cut_served.swap(true, Ordering::SeqCst);
                let send_len = if truncate { CUT_AT } else { range_len };

                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {range_len}\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock
                    .write_all(&body[start as usize..start as usize + send_len])
                    .await;
                let _ = sock.flush().await;
                // 提前关闭：声明完整 Content-Length 却少发字节 → 客户端报截断，chunk 走 Failed+重试。
            });
        }
    });

    format!("http://{addr}/file")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncated_range_must_not_stall_after_retry() {
    let body = deterministic_bytes(FILE_SIZE);
    let url = spawn_cut_once_server(body.clone()).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let output = dir.path().join("cut.bin");

    let failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let failure_start_sum = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let max_downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seen_total = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let f = failures.clone();
    let fs = failure_start_sum.clone();
    let md = max_downloaded.clone();
    let st = seen_total.clone();

    let result = Downloader::builder(url, output.to_string_lossy().to_string())
        .workers(1)
        .update_interval(0.05)
        .resume(true)
        .run(move |total_size, mut info_rx| async move {
            st.store(total_size, Ordering::SeqCst);
            loop {
                match info_rx.recv().await {
                    Ok(DownloadInfo::ChunkFailed { start, .. }) => {
                        f.fetch_add(1, Ordering::SeqCst);
                        fs.fetch_add(start, Ordering::SeqCst);
                    }
                    Ok(DownloadInfo::MonitorUpdate {
                        total_downloaded, ..
                    }) => {
                        md.fetch_max(total_downloaded, Ordering::SeqCst);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        })
        .await;

    let downloaded = std::fs::read(&output).unwrap_or_default();
    println!(
        "result={result:?} chunk_failed={} failed_start_sum={} max_reported={} total={} on_disk={}",
        failures.load(Ordering::SeqCst),
        failure_start_sum.load(Ordering::SeqCst),
        max_downloaded.load(Ordering::SeqCst),
        seen_total.load(Ordering::SeqCst),
        downloaded.len()
    );
    assert!(
        failures.load(Ordering::SeqCst) >= 1,
        "scenario must exercise a truncated range failure"
    );
    assert!(
        result.is_ok(),
        "一次截断+续传后任务必须完成（修前会因 ledger 少记前缀走 stall 逃生失败）: {result:?}"
    );
    assert_eq!(downloaded.len(), FILE_SIZE, "落盘长度必须等于声明大小");
    assert_eq!(downloaded, body, "续传拼接后的字节必须与服务端一致");
}
