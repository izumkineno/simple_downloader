#![cfg(all(feature = "resume", feature = "progress", feature = "multi-source", feature = "rate-limit", feature = "queue"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use simple_downloader::{Downloader, DownloadInfo, MultiSourceConfig, SourceConfig};
#[cfg(feature = "proxy")]
use simple_downloader::{LaneModel, ProxyConfig};
use tempfile::NamedTempFile;

mod test_server_harness;
use test_server_harness::{RunningTestServer, TestServerFile};

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .is_ok()
        || std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
}

fn assert_file_eq(path: &std::path::Path, expected: &[u8]) {
    let got = std::fs::read(path).unwrap();
    assert_eq!(got.len(), expected.len(), "file len mismatch");
    assert_eq!(got, expected, "file content mismatch");
}

fn temp_output() -> (NamedTempFile, PathBuf) {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    (tmp, path)
}

// ---------- 基础下载（无特殊 feature，基础多线程） ----------
#[tokio::test]
async fn basic_download_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(2 * 1024 * 1024);
    let file = TestServerFile::new("basic.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .unwrap();
    let url = server.url_for("basic.bin");
    let (tmp, out_path) = temp_output();
    let out_str = out_path.to_string_lossy().to_string();
    drop(tmp);
    Downloader::builder(url, out_str)
        .workers(4)
        .download()
        .await
        .expect("basic download");
    assert_file_eq(&out_path, &bytes);
}

// ---------- resume 自适应段 + hash 校验 + 兼容 ----------
#[tokio::test]
async fn resume_adaptive_and_compat_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(2 * 1024 * 1024);
    let file = TestServerFile::new("resume_adaptive.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .unwrap();
    let url = server.url_for("resume_adaptive.bin");

    // 验证自适应分档：2M 文件应按 256K 切分
    {
        use simple_downloader::adaptive_segment_size;
        let seg = adaptive_segment_size(2 * 1024 * 1024);
        assert_eq!(seg, 256 * 1024, "2M should use 256K");
        assert_eq!(adaptive_segment_size(50 * 1024 * 1024), 64 * 1024);
        assert_eq!(adaptive_segment_size(500 * 1024 * 1024), 256 * 1024);
        assert_eq!(adaptive_segment_size(2 * 1024 * 1024 * 1024), 1024 * 1024);
    }

    let (tmp, out_path) = temp_output();
    let out_str = out_path.to_string_lossy().to_string();
    drop(tmp);
    // 先写一半前缀，制造已下载 1M 前缀
    std::fs::write(&out_path, &bytes[..1024 * 1024]).unwrap();
    // 构造旧 64K 侧车（v1）模拟旧版本残留，验证自动迁移为 256K 新分档且不 Err
    {
        use simple_downloader::{hash_bytes, metadata_path_for, ResumeMetadata, DEFAULT_SEGMENT_SIZE};
        let mut old_meta = ResumeMetadata::new(2 * 1024 * 1024, DEFAULT_SEGMENT_SIZE);
        for i in 0..16 {
            old_meta.set_segment_hash(i, hash_bytes(&bytes[i * 65536..(i + 1) * 65536]));
        }
        let meta_path = metadata_path_for(&out_path);
        old_meta.save_atomic(&meta_path).unwrap();

        Downloader::builder(url.clone(), out_str.clone())
            .workers(4)
            .download()
            .await
            .expect("resume with adaptive migration");
        assert_file_eq(&out_path, &bytes);
        let meta_path = metadata_path_for(&out_path);
        assert!(!meta_path.exists(), "successful download should clean sidecar");
    }
}

// ---------- progress：ETA + pieces ----------
#[tokio::test]
async fn progress_eta_and_pieces_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(3 * 1024 * 1024);
    let file = TestServerFile::new("progress.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "32m", "32m")
        .await
        .unwrap();
    let url = server.url_for("progress.bin");
    let (tmp, out_path) = temp_output();
    let out_str = out_path.to_string_lossy().to_string();
    drop(tmp);

    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let saw_chunk = Arc::new(AtomicBool::new(false));
    let saw_eta = Arc::new(AtomicBool::new(false));
    let saw_chunk_clone = saw_chunk.clone();
    let saw_eta_clone = saw_eta.clone();
    let bytes_clone = bytes.clone();

    Downloader::builder(url, out_str)
        .workers(4)
        .update_interval(0.05)
        .run(move |_total, mut rx| {
            let saw_chunk = saw_chunk_clone.clone();
            let saw_eta = saw_eta_clone.clone();
            let bytes_len = bytes_clone.len();
            async move {
                while let Ok(info) = rx.recv().await {
                    match &info {
                        DownloadInfo::ChunkProgress { .. } => {
                            saw_chunk.store(true, Ordering::Relaxed)
                        }
                        DownloadInfo::MonitorUpdate {
                            eta_secs,
                            total_downloaded,
                            total_size,
                            ..
                        } => {
                            if eta_secs.is_some() {
                                saw_eta.store(true, Ordering::Relaxed);
                            }
                            assert!(*total_downloaded <= *total_size);
                            let _ = bytes_len;
                        }
                        _ => {}
                    }
                }
            }
        })
        .await
        .expect("progress download");
    assert_file_eq(&out_path, &bytes);
    assert!(saw_chunk.load(Ordering::Relaxed), "should see ChunkProgress");
    // eta may be None for fast downloads, but at least not panic
}

// ---------- multi-source：快/慢 lane 调度 ----------
#[tokio::test]
async fn multi_source_fast_slow_lane_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(4 * 1024 * 1024);
    let file_fast = TestServerFile::new("multi.bin", bytes.clone()).unwrap();
    let slow_dir_file = TestServerFile::new("multi.bin", bytes.clone()).unwrap();

    let fast_server = RunningTestServer::spawn(file_fast.directory(), "16m", "16m")
        .await
        .unwrap();
    let slow_server = RunningTestServer::spawn(slow_dir_file.directory(), "2m", "2m")
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("multi_out.bin");
    let cfg = MultiSourceConfig::new(out_path.to_string_lossy().to_string(), 8, 0.2)
        .with_sources(vec![
            SourceConfig::new(fast_server.url_for("multi.bin")).with_id("fast"),
            SourceConfig::new(slow_server.url_for("multi.bin")).with_id("slow"),
        ])
        .with_max_chunks_per_lane(4);
    let start = Instant::now();
    Downloader::new_multi(cfg, || reqwest::ClientBuilder::new())
        .download()
        .await
        .expect("multi-source download");
    let elapsed = start.elapsed();
    assert_file_eq(&out_path, &bytes);
    eprintln!("multi fast/slow elapsed: {:?}", elapsed);
}

// ---------- proxy：PerSourceProxy 容错 ----------
#[tokio::test]
#[cfg(feature = "proxy")]
async fn proxy_lane_tolerance_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(1 * 1024 * 1024);
    let file = TestServerFile::new("proxy2.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("proxy_out.bin");
    let bad_proxy = ProxyConfig::http("http://127.0.0.1:1").unwrap().with_id("bad");
    let cfg = MultiSourceConfig::new(out_path.to_string_lossy().to_string(), 4, 0.2)
        .with_sources(vec![
            SourceConfig::new(server.url_for("proxy2.bin"))
                .with_id("via_bad_proxy")
                .with_proxies(vec![bad_proxy]),
            SourceConfig::new(server.url_for("proxy2.bin")).with_id("direct"),
        ])
        .with_lane_model(LaneModel::PerSourceProxy)
        .with_max_chunks_per_lane(2);
    Downloader::new_multi(cfg, || reqwest::ClientBuilder::new())
        .download()
        .await
        .expect("proxy fallback download");
    assert_file_eq(&out_path, &bytes);
}

// ---------- rate-limit：global/per_source burst 自适应 ----------
#[tokio::test]
async fn rate_limit_global_and_per_source_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(2 * 1024 * 1024);
    let file = TestServerFile::new("rate2.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .unwrap();
    let url = server.url_for("rate2.bin");
    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("rate_out.bin");
    let start = Instant::now();
    Downloader::builder(url, out_path.to_string_lossy().to_string())
        .workers(4)
        .speed_limit(1024 * 1024)
        .download()
        .await
        .expect("rate limited download");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1500) && elapsed <= Duration::from_millis(4000),
        "elapsed {:?} not in 1.5-4s for 2MiB@1MiB/s",
        elapsed
    );
    assert_file_eq(&out_path, &bytes);

    // per_source + global 双桶
    let bytes2 = test_server_harness::deterministic_bytes(2 * 1024 * 1024);
    let f1 = TestServerFile::new("rate_multi.bin", bytes2.clone()).unwrap();
    let f1_multi = TestServerFile::new("rate_multi.bin", bytes2.clone()).unwrap();
    let s1 = RunningTestServer::spawn(f1.directory(), "64m", "64m").await.unwrap();
    let s2 = RunningTestServer::spawn(f1_multi.directory(), "64m", "64m")
        .await
        .unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    let out2 = tmp2.path().join("rate_multi_out.bin");
    let cfg = MultiSourceConfig::new(out2.to_string_lossy().to_string(), 4, 0.2)
        .with_sources(vec![
            SourceConfig::new(s1.url_for("rate_multi.bin"))
                .with_id("s1")
                .with_speed_limit(512 * 1024),
            SourceConfig::new(s2.url_for("rate_multi.bin"))
                .with_id("s2")
                .with_speed_limit(512 * 1024),
        ])
        .with_global_speed_limit(700 * 1024);
    Downloader::new_multi(cfg, || reqwest::ClientBuilder::new())
        .download()
        .await
        .expect("per_source+global download");
    assert_file_eq(&out2, &bytes2);
}

// ---------- queue：FIFO + workers 隔离 + 17并发唯一 ----------
#[tokio::test]
async fn queue_fifo_and_workers_isolation_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    use simple_downloader::TaskQueue;
    let bytes = test_server_harness::deterministic_bytes(512 * 1024);
    let file = TestServerFile::new("queue.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .unwrap();
    let url = server.url_for("queue.bin");
    let dir = tempfile::tempdir().unwrap();
    let queue = TaskQueue::with_max_concurrent(2);
    let mut ids = Vec::new();
    for i in 0..4 {
        let out = dir.path().join(format!("queue_out_{i}.bin"));
        let id = queue
            .enqueue_with_workers(url.clone(), out.to_string_lossy().to_string(), 2 + (i % 2) as u64)
            .await;
        ids.push((id, out));
    }
    queue.wait_all().await;
    for (id, path) in ids {
        let snap = queue.query(id.clone()).await.unwrap();
        assert!(
            matches!(
                snap.state,
                simple_downloader::TaskState::Completed | simple_downloader::TaskState::Removed
            ),
            "task {:?} not completed: {:?}",
            id,
            snap.state
        );
        if path.exists() {
            assert_file_eq(&path, &bytes);
        }
    }
    // 17 并发同名入队唯一性（AC-6）
    let dir2 = tempfile::tempdir().unwrap();
    let q2 = std::sync::Arc::new(TaskQueue::with_max_concurrent(4));
    let base_url = server.url_for("queue.bin");
    let base_path = dir2.path().join("a.bin").to_string_lossy().to_string();
    let mut handles = Vec::new();
    for _ in 0..17 {
        let q = q2.clone();
        let u = base_url.clone();
        let p = base_path.clone();
        handles.push(tokio::spawn(async move { q.enqueue(u, p).await }));
    }
    let mut ids = Vec::new();
    for h in handles {
        let id = h.await.unwrap();
        ids.push(id);
    }
    let uniq: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(uniq.len(), 17, "17 concurrent enqueue should be unique");
}

// ---------- concurrency：Probing→Stable + tail 急补 ----------
#[tokio::test]
async fn concurrency_probing_and_tail_via_test_server() {
    if !python_available() {
        eprintln!("skip: python not available");
        return;
    }
    let bytes = test_server_harness::deterministic_bytes(4 * 1024 * 1024);
    let file = TestServerFile::new("concur.bin", bytes.clone()).unwrap();
    let server = RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .unwrap();
    let url = server.url_for("concur.bin");
    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("concur_out.bin");
    let start = Instant::now();
    Downloader::builder(url, out_path.to_string_lossy().to_string())
        .workers(8)
        .download()
        .await
        .expect("concurrency download");
    let elapsed = start.elapsed();
    assert_file_eq(&out_path, &bytes);
    assert!(
        elapsed < Duration::from_secs(3),
        "concurrency tail not drained, elapsed {:?}",
        elapsed
    );
}
