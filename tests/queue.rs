mod test_server_harness;

use std::time::Duration;

use mockito::Server;
use tempfile::TempDir;
use test_server_harness::{RunningTestServer, TestServerFile};

use simple_downloader::{TaskQueue, TaskState};

fn deterministic_bytes(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

async fn create_mock(server: &mut Server, path: &str, bytes: Vec<u8>) {
    let len = bytes.len().to_string();
    server
        .mock("HEAD", path)
        .with_status(200)
        .with_header("content-length", &len)
        .with_header("accept-ranges", "bytes")
        .create_async()
        .await;
    let bytes_clone2 = bytes.clone();
    let cr = format!("bytes 0-0/{}", bytes.len());
    server
        .mock("GET", path)
        .match_header("range", "bytes=0-0")
        .with_status(206)
        .with_header("content-range", &cr)
        .with_header("content-length", "1")
        .with_header("accept-ranges", "bytes")
        .with_body(bytes_clone2[0..1].to_vec())
        .create_async()
        .await;
    let bytes_clone = bytes;
    server
        .mock("GET", path)
        .with_status(200)
        .with_header("content-length", &len)
        .with_header("accept-ranges", "bytes")
        .with_chunked_body(move |w| w.write_all(&bytes_clone))
        .create_async()
        .await;
}

#[tokio::test]
async fn ac1_concurrency_fifo() {
    let mut server = Server::new_async().await;
    let temp = TempDir::new().unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let mut ids = Vec::new();
    for i in 0..10 {
        let path = format!("/file{}", i);
        let bytes = deterministic_bytes(256 * 1024, i as u8);
        create_mock(&mut server, &path, bytes).await;
        let url = format!("{}{}", server.url(), path);
        let out = temp.path().join(format!("out{}.bin", i));
        let id = queue.enqueue(url, out).await;
        ids.push(id);
    }
    let mut peak = 0usize;
    for _ in 0..60 {
        let c = queue.active_count().await;
        if c > peak {
            peak = c;
        }
        assert!(c <= 3, "active {} > max 3", c);
        if queue.queued_len().await == 0 && c == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    queue.wait_all().await;
    assert!(
        peak <= 3 && peak > 0,
        "peak active should be 1..3, got {}",
        peak
    );
    for (i, id) in ids.iter().enumerate() {
        let snap = queue.query(id.clone()).await.expect("query");
        assert_eq!(
            snap.state,
            TaskState::Completed,
            "task {} not completed: {:?}",
            i,
            snap.state
        );
        let out = temp.path().join(format!("out{}.bin", i));
        assert!(out.exists(), "file {} missing", out.display());
        let data = std::fs::read(&out).unwrap();
        let expected = deterministic_bytes(256 * 1024, i as u8);
        assert_eq!(data, expected, "file {} content mismatch", i);
    }
}

#[tokio::test]
async fn ac1_workers_isolation() {
    let server_file =
        TestServerFile::new("big.bin", deterministic_bytes(2 * 1024 * 1024, 42)).unwrap();
    let server = RunningTestServer::spawn(server_file.directory(), "64m", "64m")
        .await
        .unwrap();
    let temp = TempDir::new().unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let url = server.url_for("big.bin");
    let out = temp.path().join("big.bin");
    let id = queue.enqueue_with_workers(url, out.clone(), 8).await;
    queue.wait_all().await;
    let snap = queue.query(id).await.unwrap();
    assert_eq!(
        snap.state,
        TaskState::Completed,
        "expected Completed got {:?}",
        snap.state
    );
    assert!(out.exists());
    let data = std::fs::read(&out).unwrap();
    let expected = deterministic_bytes(2 * 1024 * 1024, 42);
    assert_eq!(data, expected);
}

#[tokio::test]
async fn ac2_pause_resume() {
    let server_file =
        TestServerFile::new("pause.bin", deterministic_bytes(2 * 1024 * 1024, 7)).unwrap();
    let server = RunningTestServer::spawn(server_file.directory(), "64m", "64m")
        .await
        .unwrap();
    let temp = TempDir::new().unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let url = server.url_for("pause.bin");
    let out = temp.path().join("pause.bin");
    let id = queue.enqueue(url, out.clone()).await;
    for _ in 0..40 {
        if let Some(s) = queue.query(id.clone()).await {
            if s.state == TaskState::Active && out.exists() {
                if let Ok(m) = std::fs::metadata(&out) {
                    if m.len() > 1024 {
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(out.exists(), "file should exist before pause");
    queue.pause(id.clone()).await.unwrap();
    let snap = queue.query(id.clone()).await.unwrap();
    assert_eq!(snap.state, TaskState::Paused);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let len1 = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    assert!(len1 > 0, "file should have data after pause, got {}", len1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let len2 = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    assert_eq!(len1, len2, "file should not grow after pause");
    #[cfg(feature = "resume")]
    {
        let sidecar = simple_downloader::metadata_path_for(&out);
        assert!(sidecar.exists(), "sidecar should exist after pause");
    }
    queue.resume(id.clone()).await.unwrap();
    queue.wait_all().await;
    let snap2 = queue.query(id).await.unwrap();
    assert_eq!(
        snap2.state,
        TaskState::Completed,
        "resume should complete, got {:?}",
        snap2.state
    );
    assert!(out.exists());
    let data = std::fs::read(&out).unwrap();
    let expected = deterministic_bytes(2 * 1024 * 1024, 7);
    assert_eq!(data, expected, "resumed file mismatch");
    drop(server);
}

#[tokio::test]
async fn ac3_cancel() {
    let mut server = Server::new_async().await;
    let temp = TempDir::new().unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let bytes = deterministic_bytes(512 * 1024, 9);
    create_mock(&mut server, "/cancel", bytes).await;
    let url = format!("{}/cancel", server.url());
    let out = temp.path().join("cancel.bin");
    let id = queue.enqueue(url, out.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    queue.cancel(id.clone()).await.unwrap();
    let snap = queue.query(id.clone()).await.unwrap();
    assert_eq!(snap.state, TaskState::Removed);
    assert!(!out.exists(), "file should be deleted after cancel");
    #[cfg(feature = "resume")]
    {
        let sidecar = simple_downloader::metadata_path_for(&out);
        assert!(!sidecar.exists(), "sidecar should be deleted after cancel");
    }
    tokio::time::timeout(Duration::from_secs(2), queue.wait_all())
        .await
        .expect("wait_all should not hang after cancel");
    queue.cancel(id).await.unwrap();
}

#[tokio::test]
async fn ac4_rename_three() {
    let mut server = Server::new_async().await;
    let temp = TempDir::new().unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let bytes = deterministic_bytes(128 * 1024, 1);
    for i in 0..3 {
        let path = format!("/rename{}", i);
        create_mock(&mut server, &path, bytes.clone()).await;
    }
    let out = temp.path().join("a.bin");
    let mut ids = Vec::new();
    for i in 0..3 {
        let url = format!("{}/rename{}", server.url(), i);
        let id = queue.enqueue(url, out.clone()).await;
        ids.push(id);
    }
    queue.wait_all().await;
    for (idx, expected_name) in ["a.bin", "a(1).bin", "a(2).bin"].iter().enumerate() {
        let p = temp.path().join(expected_name);
        assert!(p.exists(), "expected {} to exist", p.display());
        let data = std::fs::read(&p).unwrap();
        assert_eq!(data, bytes, "content mismatch for {}", expected_name);
        let snap = queue.query(ids[idx].clone()).await.unwrap();
        assert_eq!(snap.state, TaskState::Completed);
        assert_eq!(snap.output_path, p);
    }
}

#[tokio::test]
async fn ac4_rename_disk_trigger() {
    let mut server = Server::new_async().await;
    let temp = TempDir::new().unwrap();
    let pre = temp.path().join("a.bin");
    std::fs::write(&pre, b"pre").unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let bytes = deterministic_bytes(64 * 1024, 5);
    create_mock(&mut server, "/disk", bytes.clone()).await;
    let url = format!("{}/disk", server.url());
    let id = queue.enqueue(url, pre.clone()).await;
    queue.wait_all().await;
    let snap = queue.query(id).await.unwrap();
    assert_eq!(snap.state, TaskState::Completed);
    let expected = temp.path().join("a(1).bin");
    assert_eq!(snap.output_path, expected);
    assert!(expected.exists());
    let data = std::fs::read(&expected).unwrap();
    assert_eq!(data, bytes);
    assert_eq!(std::fs::read(&pre).unwrap(), b"pre");
}

#[tokio::test]
async fn ac5_isolation() {
    let mut server = Server::new_async().await;
    let temp = TempDir::new().unwrap();
    let queue = TaskQueue::with_max_concurrent(3);
    let bytes1 = deterministic_bytes(128 * 1024, 11);
    let bytes2 = deterministic_bytes(128 * 1024, 12);
    create_mock(&mut server, "/ok1", bytes1.clone()).await;
    create_mock(&mut server, "/ok2", bytes2.clone()).await;
    server
        .mock("HEAD", "/fail")
        .with_status(500)
        .create_async()
        .await;
    server
        .mock("GET", "/fail")
        .with_status(500)
        .with_body("error")
        .create_async()
        .await;
    let id1 = queue
        .enqueue(format!("{}/ok1", server.url()), temp.path().join("ok1.bin"))
        .await;
    let id_fail = queue
        .enqueue(
            format!("{}/fail", server.url()),
            temp.path().join("fail.bin"),
        )
        .await;
    let id2 = queue
        .enqueue(format!("{}/ok2", server.url()), temp.path().join("ok2.bin"))
        .await;
    queue.wait_all().await;
    let s1 = queue.query(id1).await.unwrap();
    let s2 = queue.query(id2).await.unwrap();
    let sf = queue.query(id_fail).await.unwrap();
    assert_eq!(s1.state, TaskState::Completed);
    assert_eq!(s2.state, TaskState::Completed);
    match sf.state {
        TaskState::Failed(_) => {}
        _ => panic!("fail task should be Failed, got {:?}", sf.state),
    }
    assert!(temp.path().join("ok1.bin").exists());
    assert!(temp.path().join("ok2.bin").exists());
}
