use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::Client;
use simple_downloader::{
    DownloadInfo, Downloader, MultiSourceConfig, SourceConfig, reqwest::ClientBuilder,
};
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// 这个示例演示本次 Phase 1 “repo-native test_server + 多源下载”路径的**手动观察版**：
///
/// - 自动生成更大的本地源文件
/// - 自动启动两个不同限速的 `test_server/server.py`
/// - 使用 `Downloader::new_multi(...)` 执行真实多源下载
/// - 在终端中实时刷新总进度、速度和源侧 stats 摘要
/// - 最终校验文件字节完全一致，并确认两个源都参与了 Range 请求
///
/// 运行方式：
///
/// ```bash
/// cargo run --features multi-source,progress --example manual_multi_source_test_server
/// ```
///
/// 这个示例验证的是：
/// 1. 两个 repo-native `test_server` 源可以共同完成同一个文件的下载。
/// 2. 下载结果在字节级别与源文件完全一致。
/// 3. fast / slow 两个源都真正参与了 Range 请求。
/// 4. 终端输出会在下载过程中持续刷新，方便人眼观察速度和进度变化。
///
/// 这个示例**不**验证：
/// - 吞吐率是否达到某个阈值；
/// - fast 源是否一定下载得比 slow 源更多；
/// - 更复杂的失败恢复 / 黑名单策略矩阵；
/// - 生产环境远程镜像站行为。

const SAMPLE_FILE_NAME: &str = "manual-multi-source.bin";
const SAMPLE_FILE_SIZE: usize = 500 * 1024 * 1024;
const FAST_TOTAL_MAX_SPEED: &str = "16m";
const FAST_PER_THREAD_MAX_SPEED: &str = "16m";
const SLOW_TOTAL_MAX_SPEED: &str = "2m";
const SLOW_PER_THREAD_MAX_SPEED: &str = "2m";
const WORKERS: u64 = 2;
const UPDATE_INTERVAL_SECS: f64 = 0.20;
const READINESS_TIMEOUT: Duration = Duration::from_secs(4);
const READINESS_POLL: Duration = Duration::from_millis(50);
const STATS_REFRESH_INTERVAL: Duration = Duration::from_millis(300);
const INTERACTIVE_REFRESH_HZ: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Interactive,
    Captured,
}

impl OutputMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interactive => "interactive-bars",
            Self::Captured => "captured-bars",
        }
    }
}

/// 当终端输出被重定向或不支持交互式刷新时，`indicatif` 默认体验会变差。
/// 这里沿用仓库里另一个 example 的思路，提供一个“可见终端”包装，让 captured
/// 场景下也能得到稳定、可读的刷新输出。
#[derive(Debug, Clone)]
struct VisibleTerm {
    width: u16,
}

impl VisibleTerm {
    fn new(width: u16) -> Self {
        Self { width }
    }

    fn write_ansi(&self, bytes: &[u8]) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(bytes)?;
        stderr.flush()
    }
}

impl indicatif::TermLike for VisibleTerm {
    fn width(&self) -> u16 {
        self.width
    }

    fn height(&self) -> u16 {
        20
    }

    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        match n {
            0 => Ok(()),
            _ => self.write_ansi(format!("\x1b[{n}A").as_bytes()),
        }
    }

    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        match n {
            0 => Ok(()),
            _ => self.write_ansi(format!("\x1b[{n}B").as_bytes()),
        }
    }

    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        match n {
            0 => Ok(()),
            _ => self.write_ansi(format!("\x1b[{n}C").as_bytes()),
        }
    }

    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        match n {
            0 => Ok(()),
            _ => self.write_ansi(format!("\x1b[{n}D").as_bytes()),
        }
    }

    fn write_line(&self, s: &str) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(s.as_bytes())?;
        stderr.write_all(b"\n")?;
        stderr.flush()
    }

    fn write_str(&self, s: &str) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(s.as_bytes())?;
        stderr.flush()
    }

    fn clear_line(&self) -> io::Result<()> {
        self.write_ansi(b"\r\x1b[2K")
    }

    fn flush(&self) -> io::Result<()> {
        io::stderr().lock().flush()
    }
}

#[derive(Debug, Default, Clone)]
struct SourceStatsSnapshot {
    fast: HashMap<String, u64>,
    slow: HashMap<String, u64>,
}

#[derive(Clone)]
struct ExampleContext {
    fast_server: Arc<RunningTestServer>,
    slow_server: Arc<RunningTestServer>,
    stats: Arc<Mutex<SourceStatsSnapshot>>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("[manual-multi-source] {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let workspace = ExampleWorkspace::new()?;
    let source_bytes = deterministic_bytes(SAMPLE_FILE_SIZE);
    workspace.write_source_file(SAMPLE_FILE_NAME, &source_bytes)?;

    let fast_server = Arc::new(
        RunningTestServer::spawn(
            workspace.serve_dir(),
            FAST_TOTAL_MAX_SPEED,
            FAST_PER_THREAD_MAX_SPEED,
        )
        .await?,
    );
    let slow_server = Arc::new(
        RunningTestServer::spawn(
            workspace.serve_dir(),
            SLOW_TOTAL_MAX_SPEED,
            SLOW_PER_THREAD_MAX_SPEED,
        )
        .await?,
    );

    let output_mode = detect_output_mode();

    println!(
        "[manual-multi-source] fast source: {}",
        fast_server.base_url()
    );
    println!(
        "[manual-multi-source] slow source: {}",
        slow_server.base_url()
    );
    println!(
        "[manual-multi-source] source file: {} ({:.2} MiB)",
        workspace.source_file().display(),
        to_mib(source_bytes.len() as u64)
    );
    println!(
        "[manual-multi-source] output file: {}",
        workspace.output_file().display()
    );
    println!("[manual-multi-source] workers: {WORKERS}, update interval: {UPDATE_INTERVAL_SECS}s");
    println!("[manual-multi-source] output mode: {}", output_mode.label());
    println!(
        "[manual-multi-source] speed profile: fast={FAST_TOTAL_MAX_SPEED}, slow={SLOW_TOTAL_MAX_SPEED}"
    );

    let config = MultiSourceConfig::new(
        workspace.output_file_string(),
        WORKERS,
        UPDATE_INTERVAL_SECS,
    )
    .with_sources(vec![
        SourceConfig::new(fast_server.url_for(SAMPLE_FILE_NAME)).with_id("fast"),
        SourceConfig::new(slow_server.url_for(SAMPLE_FILE_NAME)).with_id("slow"),
    ]);

    let downloader = Downloader::new_multi(config, ClientBuilder::new);

    let context = ExampleContext {
        fast_server: fast_server.clone(),
        slow_server: slow_server.clone(),
        stats: Arc::new(Mutex::new(SourceStatsSnapshot::default())),
    };

    let progress_handler_context = context.clone();
    let started_at = Instant::now();

    println!("[manual-multi-source] starting multi-source download...");
    downloader
        .run(move |total_size, info_rx| {
            render_progress(total_size, info_rx, output_mode, progress_handler_context)
        })
        .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let downloaded = fs::read(workspace.output_file())?;
    if downloaded != source_bytes {
        return Err("downloaded bytes do not match the original source bytes".into());
    }

    println!("[manual-multi-source] byte-for-byte verification passed.");

    let fast_stats = fast_server.stats().await?;
    let slow_stats = slow_server.stats().await?;

    println!("[manual-multi-source] final fast stats: {fast_stats:?}");
    println!("[manual-multi-source] final slow stats: {slow_stats:?}");

    let fast_ranges = fast_stats
        .get("range_requests")
        .copied()
        .unwrap_or_default();
    let slow_ranges = slow_stats
        .get("range_requests")
        .copied()
        .unwrap_or_default();

    if fast_ranges == 0 || slow_ranges == 0 {
        return Err(format!(
            "expected both sources to serve at least one range request, got fast={fast_ranges}, slow={slow_ranges}"
        )
        .into());
    }

    println!(
        "[manual-multi-source] success: both sources participated and the downloaded file is byte-correct."
    );
    println!(
        "[manual-multi-source] elapsed: {}",
        format_duration(started_at.elapsed())
    );

    Ok(())
}

/// 这个异步渲染器负责：
/// 1. 从 `DownloadInfo` 中读取总进度和总速度；
/// 2. 使用 `indicatif` 在终端中实时刷新总进度条；
/// 3. 周期性读取两个源的 `/__stats__`，展示参与度摘要；
/// 4. 同时兼容 TTY 和 captured 输出模式。
///
/// 注意：这里展示的 `/__stats__` 只是参与度摘要，不是精确的 per-source 吞吐率。
async fn render_progress(
    total_size: u64,
    mut info_rx: broadcast::Receiver<DownloadInfo>,
    output_mode: OutputMode,
    context: ExampleContext,
) {
    let draw_target = match output_mode {
        OutputMode::Interactive => ProgressDrawTarget::stderr_with_hz(INTERACTIVE_REFRESH_HZ),
        OutputMode::Captured => ProgressDrawTarget::term_like_with_hz(
            Box::new(VisibleTerm::new(120)),
            INTERACTIVE_REFRESH_HZ,
        ),
    };

    let multi_progress = MultiProgress::with_draw_target(draw_target);

    let total_bar = multi_progress.add(ProgressBar::new(total_size));
    total_bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{msg}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})",
        )
        .expect("valid total progress template")
        .progress_chars("=> "),
    );
    total_bar.set_message("total 0.00 MiB/s");

    let fast_bar = multi_progress.add(ProgressBar::new_spinner());
    fast_bar.set_style(
        ProgressStyle::with_template("  [fast ] {spinner:.green} {msg}")
            .expect("valid fast summary template"),
    );
    fast_bar.enable_steady_tick(Duration::from_millis(120));
    fast_bar.set_message("range=0 get=0 head=0");

    let slow_bar = multi_progress.add(ProgressBar::new_spinner());
    slow_bar.set_style(
        ProgressStyle::with_template("  [slow ] {spinner:.yellow} {msg}")
            .expect("valid slow summary template"),
    );
    slow_bar.enable_steady_tick(Duration::from_millis(120));
    slow_bar.set_message("range=0 get=0 head=0");

    let stats_state = context.stats.clone();
    let fast_server = context.fast_server.clone();
    let slow_server = context.slow_server.clone();

    let stats_task = tokio::spawn(async move {
        loop {
            let fast = fast_server.stats().await.unwrap_or_default();
            let slow = slow_server.stats().await.unwrap_or_default();

            if let Ok(mut guard) = stats_state.lock() {
                guard.fast = fast;
                guard.slow = slow;
            }

            tokio::time::sleep(STATS_REFRESH_INTERVAL).await;
        }
    });

    let mut last_rendered_fast = String::from("range=0 get=0 head=0");
    let mut last_rendered_slow = String::from("range=0 get=0 head=0");

    while let Ok(info) = info_rx.recv().await {
        match info {
            DownloadInfo::MonitorUpdate {
                total_downloaded,
                total_speed,
                ..
            } => {
                total_bar.set_length(total_size);
                total_bar.set_position(total_downloaded);
                total_bar.set_message(format!("total {}", format_speed(total_speed)));

                if let Ok(guard) = context.stats.lock() {
                    let fast_msg = format_stats_summary(&guard.fast);
                    let slow_msg = format_stats_summary(&guard.slow);

                    if fast_msg != last_rendered_fast {
                        fast_bar.set_message(fast_msg.clone());
                        last_rendered_fast = fast_msg;
                    }
                    if slow_msg != last_rendered_slow {
                        slow_bar.set_message(slow_msg.clone());
                        last_rendered_slow = slow_msg;
                    }
                }
            }
            DownloadInfo::ChunkBisected {
                original_id,
                new_start,
                new_end,
            } => {
                total_bar.println(format!(
                    "[manual-multi-source] split chunk {original_id} -> {new_start}..={new_end}"
                ));
            }
            DownloadInfo::ChunkFailed {
                id,
                start,
                end,
                error,
            } => {
                total_bar.println(format!(
                    "[manual-multi-source] chunk {id} failed at {start}..={end}: {error}"
                ));
            }
            DownloadInfo::DownloadComplete(id) => {
                total_bar.println(format!("[manual-multi-source] chunk {id} completed"));
            }
            DownloadInfo::ChunkStatusChanged {
                id,
                status,
                message,
            } => match message {
                Some(message) => total_bar.println(format!(
                    "[manual-multi-source] chunk {id} status={status} ({message})"
                )),
                None => {
                    total_bar.println(format!("[manual-multi-source] chunk {id} status={status}"))
                }
            },
            DownloadInfo::ChunkProgress { .. } => {}
            _ => {}
        }
    }

    stats_task.abort();
    let _ = stats_task.await;

    // 在结束前再读一次共享状态，保证 captured 模式下最终摘要也落地。
    let (final_fast_summary, final_slow_summary) = if let Ok(guard) = context.stats.lock() {
        (
            format_stats_summary(&guard.fast),
            format_stats_summary(&guard.slow),
        )
    } else {
        (last_rendered_fast.clone(), last_rendered_slow.clone())
    };

    if final_fast_summary != last_rendered_fast {
        fast_bar.set_message(final_fast_summary.clone());
    }
    if final_slow_summary != last_rendered_slow {
        slow_bar.set_message(final_slow_summary.clone());
    }

    fast_bar.finish_with_message(format!("final {final_fast_summary}"));
    slow_bar.finish_with_message(format!("final {final_slow_summary}"));
    total_bar.finish_with_message("download complete");
}

/// 这个工作区把示例运行时的临时文件和输出文件都约束在系统临时目录下，
/// 避免污染仓库工作区。
struct ExampleWorkspace {
    root: PathBuf,
    serve_dir: PathBuf,
    source_file: PathBuf,
    output_file: PathBuf,
}

impl ExampleWorkspace {
    fn new() -> Result<Self, Box<dyn Error + Send + Sync>> {
        let unique = unique_suffix();
        let root =
            std::env::temp_dir().join(format!("simple-downloader-manual-multi-source-{unique}"));
        let serve_dir = root.join("files");
        let downloads_dir = root.join("downloads");
        let source_file = serve_dir.join(SAMPLE_FILE_NAME);
        let output_file = downloads_dir.join("output.bin");

        fs::create_dir_all(&serve_dir)?;
        fs::create_dir_all(&downloads_dir)?;

        Ok(Self {
            root,
            serve_dir,
            source_file,
            output_file,
        })
    }

    fn write_source_file(
        &self,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let path = self.serve_dir.join(file_name);
        fs::write(path, bytes)?;
        Ok(())
    }

    fn serve_dir(&self) -> &Path {
        &self.serve_dir
    }

    fn source_file(&self) -> &Path {
        &self.source_file
    }

    fn output_file(&self) -> &Path {
        &self.output_file
    }

    fn output_file_string(&self) -> String {
        self.output_file.to_string_lossy().to_string()
    }
}

impl Drop for ExampleWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// 轻量封装一个运行中的 repo-native `test_server` 进程。
struct RunningTestServer {
    child: Mutex<Child>,
    port: u16,
}

impl RunningTestServer {
    async fn spawn(
        file_directory: &Path,
        total_max_speed: &str,
        per_thread_max_speed: &str,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let port = free_port()?;
        let script = test_server_script();
        let python =
            std::env::var("SIMPLE_DOWNLOADER_TEST_PYTHON").unwrap_or_else(|_| "python".to_owned());

        let child = Command::new(python)
            .arg(script)
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_HOST", "127.0.0.1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_PORT", port.to_string())
            .env(
                "SIMPLE_DOWNLOADER_TEST_SERVER_DIRECTORY",
                file_directory.as_os_str(),
            )
            .env(
                "SIMPLE_DOWNLOADER_TEST_SERVER_TOTAL_MAX_SPEED",
                total_max_speed,
            )
            .env(
                "SIMPLE_DOWNLOADER_TEST_SERVER_PER_THREAD_MAX_SPEED",
                per_thread_max_speed,
            )
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_CONFIG_WATCH", "1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_CONSOLE", "1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_STATUS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let mut server = Self {
            child: Mutex::new(child),
            port,
        };
        server.wait_until_ready().await?;
        Ok(server)
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn url_for(&self, file_name: &str) -> String {
        format!("{}/{}", self.base_url(), file_name)
    }

    async fn stats(&self) -> Result<HashMap<String, u64>, Box<dyn Error + Send + Sync>> {
        let body = Client::new()
            .get(format!("{}/__stats__", self.base_url()))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        let mut stats = HashMap::new();
        for line in body.lines() {
            if let Some((key, value)) = line.split_once('\t') {
                stats.insert(key.to_owned(), value.parse::<u64>()?);
            }
        }

        Ok(stats)
    }

    async fn wait_until_ready(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let client = Client::new();
        let url = format!("{}/__files__", self.base_url());
        let deadline = Instant::now() + READINESS_TIMEOUT;
        let mut last_error = String::new();

        while Instant::now() < deadline {
            if let Some(status) = self.child.lock().expect("lock child").try_wait()? {
                return Err(format!("test_server exited before readiness: {status}").into());
            }

            match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => last_error = format!("HTTP {}", response.status()),
                Err(error) => last_error = error.to_string(),
            }

            tokio::time::sleep(READINESS_POLL).await;
        }

        Err(format!(
            "test_server did not become ready at {url} within {:?}: {last_error}",
            READINESS_TIMEOUT
        )
        .into())
    }
}

impl Drop for RunningTestServer {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn detect_output_mode() -> OutputMode {
    if std::io::stdout().is_terminal() || std::io::stderr().is_terminal() {
        OutputMode::Interactive
    } else {
        OutputMode::Captured
    }
}

fn format_stats_summary(stats: &HashMap<String, u64>) -> String {
    let range = stats.get("range_requests").copied().unwrap_or_default();
    let get = stats.get("get_requests").copied().unwrap_or_default();
    let head = stats.get("head_requests").copied().unwrap_or_default();
    format!("range={range} get={get} head={head}")
}

fn format_speed(bytes_per_sec: f64) -> String {
    let normalized = if bytes_per_sec <= 0.0 {
        0.0
    } else {
        bytes_per_sec
    };
    format!("{:.2} MiB/s", normalized / 1024.0 / 1024.0)
}

fn format_duration(duration: Duration) -> String {
    let total_millis = duration.as_millis();
    let hours = total_millis / 3_600_000;
    let minutes = (total_millis / 60_000) % 60;
    let seconds = (total_millis / 1_000) % 60;
    let millis = total_millis % 1_000;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
    } else if minutes > 0 {
        format!("{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{seconds}.{millis:03}s")
    }
}

fn to_mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

fn deterministic_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| ((index.wrapping_mul(31).wrapping_add(7)) % 251) as u8)
        .collect()
}

fn free_port() -> Result<u16, Box<dyn Error + Send + Sync>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn test_server_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_server")
        .join("server.py")
}

fn unique_suffix() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{timestamp}-{}", std::process::id())
}
