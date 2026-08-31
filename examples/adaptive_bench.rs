use simple_downloader::{DownloadInfo, Downloader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::net::TcpListener;
use tempfile::TempDir;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}
fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_server").join("server.py")
}
fn deterministic_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i.wrapping_mul(31).wrapping_add(7)) % 251) as u8).collect()
}

struct Running {
    child: Child,
    port: u16,
    _root: TempDir,
}

impl Running {
    async fn spawn(bytes: Vec<u8>, total: &str, per_thread: &str, name: &str) -> Self {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("files");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), &bytes).unwrap();
        let port = free_port();
        let script = script_path();
        let mut child = Command::new("python")
            .arg(script)
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_HOST", "127.0.0.1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_PORT", port.to_string())
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DIRECTORY", dir.as_os_str())
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_TOTAL_MAX_SPEED", total)
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_PER_THREAD_MAX_SPEED", per_thread)
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_CONSOLE", "1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_STATUS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // wait ready
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/__files__", port);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                panic!("server exited early");
            }
            if Instant::now() > deadline {
                panic!("server not ready");
            }
            if let Ok(r) = client.get(&url).send().await {
                if r.status().is_success() { break; }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self { child, port, _root: root }
    }
    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, name)
    }
}
impl Drop for Running {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

async fn run_one(label: &str, file_size: usize, total: &str, per_thread: &str, workers: u64) {
    eprintln!("\n========== SCENARIO: {} ==========", label);
    eprintln!("[Bench] file_size={} ({:.2} MiB) total={} per_thread={} workers={}", file_size, file_size as f64/1024.0/1024.0, total, per_thread, workers);
    let bytes = deterministic_bytes(file_size);
    let srv = Running::spawn(bytes.clone(), total, per_thread, "bench.bin").await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("out.bin");

    let url = srv.url("bench.bin");
    let out_str = out.to_string_lossy().to_string();

    let start = Instant::now();
    let bisect_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let complete_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dl2 = Downloader::builder(url.clone(), out_str.clone())
        .workers(workers)
        .update_interval(0.2)
        .build();
    let bc2 = bisect_counter.clone();
    let cc2 = complete_counter.clone();
    let fc2 = failed_counter.clone();
    dl2.run(move |total_size, mut info_rx| async move {
        while let Ok(info) = info_rx.recv().await {
            match &info {
                DownloadInfo::ChunkBisected { original_id, new_start, new_end } => {
                    eprintln!("[Bench][Event] Bisected orig={} new={}..{}", original_id, new_start, new_end);
                    bc2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                DownloadInfo::DownloadComplete(id) => {
                    eprintln!("[Bench][Event] Complete id={}", id);
                    cc2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                DownloadInfo::ChunkFailed { id, start, end, error } => {
                    eprintln!("[Bench][Event] Failed id={} {}..{} err={}", id, start, end, error);
                    fc2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                DownloadInfo::MonitorUpdate { total_downloaded, total_speed, chunk_details, .. } => {
                    eprintln!("[Bench][Monitor] {}/{} {:.2} MiB/s chunks={} {:?}", total_downloaded, total_size, total_speed/1024.0/1024.0, chunk_details.len(), chunk_details.iter().map(|(id,_,_,sp,st)| format!("{}:{:.1}KB/s st{}", id, sp/1024.0, st)).collect::<Vec<_>>());
                }
                _ => {}
            }
        }
    }).await.expect("download failed");
    let elapsed = start.elapsed();
    let got = std::fs::read(&out).unwrap();
    assert_eq!(got.len(), file_size, "size mismatch");
    assert_eq!(got, bytes, "content mismatch");
    let avg = file_size as f64 / elapsed.as_secs_f64() / 1024.0/1024.0;
    eprintln!("[Bench][Result] {} done size={:.2} MiB time={:.2}s avg={:.2} MiB/s bisects={} completes={} fails={}", label, file_size as f64/1024.0/1024.0, elapsed.as_secs_f64(), avg, bisect_counter.load(std::sync::atomic::Ordering::SeqCst), complete_counter.load(std::sync::atomic::Ordering::SeqCst), failed_counter.load(std::sync::atomic::Ordering::SeqCst));
    // brief sleep to let server flush
    tokio::time::sleep(Duration::from_millis(200)).await;
}



#[tokio::main]
async fn main() {
    // cargo run --example adaptive_bench --features progress,multi-source,resume
    // 初始化 tracing 以显示自适应引擎的 debug/trace 日志（输出到 stderr）
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    eprintln!("[Bench] starting adaptive bench with verbose adaptive logs");
    // Scenario matrix: adaptivity should shine on large file + moderate throttle where splitting helps
    run_one("S1_small_fast_3MiB_w8_total128m_per64m", 3*1024*1024, "128m", "64m", 8).await;
    run_one("S2_large_fast_20MiB_w16_total128m_per64m", 20*1024*1024, "128m", "64m", 16).await;
    run_one("S3_large_slow_per_thread_20MiB_w16_total96m_per1m", 20*1024*1024, "96m", "1m", 16).await;
    run_one("S4_large_total_bottleneck_20MiB_w16_total5m_per10m", 20*1024*1024, "5m", "10m", 16).await;
    run_one("S5_medium_w4_total32m_per8m", 8*1024*1024, "32m", "8m", 4).await;
    eprintln!("\n========== ALL BENCH DONE ==========");
}
