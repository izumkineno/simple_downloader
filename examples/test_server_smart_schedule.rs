use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use simple_downloader::reqwest::ClientBuilder;
use simple_downloader::{DownloadInfo, Downloader};
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8000";
const MANIFEST_PATH: &str = "/__files__";
const OUTPUT_DIR: &str = "target/test_server_demo";
const WORKERS: u64 = 16;
const UPDATE_INTERVAL_SECS: f64 = 0.5;
const INTERACTIVE_REFRESH_HZ: u8 = 10;

#[derive(Debug, Clone)]
struct RemoteFile {
    path: String,
    size: u64,
}

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

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("[test-server-demo] {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let base_url = DEFAULT_BASE_URL;
    let manifest_url = format!("{base_url}{MANIFEST_PATH}");
    println!("[test-server-demo] manifest: {manifest_url}");

    let files = fetch_manifest(&manifest_url).await?;
    if files.is_empty() {
        return Err(
            std::io::Error::new(std::io::ErrorKind::InvalidData, "manifest is empty").into(),
        );
    }

    let selected = files
        .iter()
        .max_by_key(|file| file.size)
        .cloned()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "failed to select largest file",
            )
        })?;

    println!(
        "[test-server-demo] selected largest file: {} ({:.2} MiB)",
        selected.path,
        to_mib(selected.size)
    );

    let download_url = join_url(base_url, &selected.path);
    let output_path = build_output_path(&selected.path)?;
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output_mode = detect_output_mode();

    println!("[test-server-demo] download url: {download_url}");
    println!("[test-server-demo] output path: {}", output_path.display());
    println!("[test-server-demo] workers: {WORKERS}, update interval: {UPDATE_INTERVAL_SECS}s");
    println!("[test-server-demo] progress mode: {}", output_mode.label());
    println!("[test-server-demo] top manifest candidates:");
    for file in files.iter().take(3) {
        println!("  - {} ({:.2} MiB)", file.path, to_mib(file.size));
    }

    let downloader = Downloader::new(
        download_url,
        output_path.to_string_lossy().to_string(),
        WORKERS,
        UPDATE_INTERVAL_SECS,
        || ClientBuilder::new(),
    );

    let started_at = std::time::Instant::now();
    let progress_handler = move |total_size: u64, info_rx: broadcast::Receiver<DownloadInfo>| async move {
        render_progress(total_size, info_rx, output_mode).await;
    };

    downloader.run(progress_handler).await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let elapsed = started_at.elapsed();
    let downloaded_bytes = tokio::fs::metadata(&output_path).await?.len();
    let avg_speed = if elapsed.is_zero() {
        0.0
    } else {
        downloaded_bytes as f64 / elapsed.as_secs_f64()
    };
    println!(
        "[test-server-demo] download completed: used {:.2} MiB in {} (avg {})",
        to_mib(downloaded_bytes),
        format_duration(elapsed),
        format_speed(avg_speed),
    );
    Ok(())
}

async fn render_progress(
    total_size: u64,
    mut info_rx: broadcast::Receiver<DownloadInfo>,
    output_mode: OutputMode,
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
    total_bar.set_message(format!("total {}", format_speed(0.0)));

    let mut chunk_bars: HashMap<u64, ProgressBar> = HashMap::new();

    while let Ok(info) = info_rx.recv().await {
        match info {
            DownloadInfo::MonitorUpdate {
                total_downloaded,
                total_speed,
                chunk_details,
                ..
            } => {
                total_bar.set_length(total_size);
                total_bar.set_position(total_downloaded);
                total_bar.set_message(format!("total {}", format_speed(total_speed)));

                for (id, size, downloaded, speed, status) in chunk_details {
                    let (color, status_text) = status_info(status);
                    let pb = chunk_bars.entry(id).or_insert_with(|| {
                        let pb = multi_progress.add(ProgressBar::new(size));
                        pb.set_prefix(format!("chunk {id:>2}"));
                        pb
                    });

                    pb.set_style(chunk_style(color));
                    pb.set_length(size);
                    pb.set_position(downloaded);
                    pb.set_message(format!("{status_text} | {}", format_speed(speed)));
                }
            }
            DownloadInfo::ChunkBisected {
                original_id,
                new_start,
                new_end,
            } => {
                total_bar.println(format!(
                    "[split] chunk {original_id} -> range {new_start}..={new_end}"
                ));
            }
            DownloadInfo::ChunkFailed {
                id,
                start,
                end,
                error,
            } => {
                total_bar.println(format!("[failed] chunk {id} {start}..={end}: {error}"));
            }
            DownloadInfo::DownloadComplete(id) => {
                if let Some(pb) = chunk_bars.get(&id) {
                    pb.finish_with_message("complete");
                }
                total_bar.println(format!("[complete] chunk {id} finished"));
            }
            DownloadInfo::ChunkStatusChanged {
                id,
                status,
                message,
            } => {
                let (color, status_text) = status_info(status);
                if let Some(pb) = chunk_bars.get(&id) {
                    let msg = message
                        .as_deref()
                        .map(|item| format!("{status_text} | {item}"))
                        .unwrap_or_else(|| status_text.to_string());
                    pb.set_style(chunk_style(color));
                    pb.set_message(msg.clone());
                    if status == 5 {
                        pb.abandon_with_message(msg);
                    }
                }

                match message {
                    Some(message) => {
                        total_bar.println(format!("[status] chunk {id} => {status} ({message})"));
                    }
                    None => {
                        total_bar.println(format!("[status] chunk {id} => {status}"));
                    }
                }
            }
            DownloadInfo::ChunkProgress { .. } => {}
        }
    }

    total_bar.finish_with_message("download complete");
}

async fn fetch_manifest(url: &str) -> Result<Vec<RemoteFile>, Box<dyn Error + Send + Sync>> {
    let body = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let mut files = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((path, size)) = line.split_once('\t') else {
            continue;
        };

        let size = size.trim().parse::<u64>()?;
        files.push(RemoteFile {
            path: path.trim().to_string(),
            size,
        });
    }

    files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    Ok(files)
}

fn detect_output_mode() -> OutputMode {
    if std::io::stdout().is_terminal() || std::io::stderr().is_terminal() {
        OutputMode::Interactive
    } else {
        OutputMode::Captured
    }
}

fn status_info(status: u8) -> (&'static str, &'static str) {
    match status {
        0 => ("cyan", "downloading"),
        1 => ("yellow", "retrying"),
        2 => ("blue", "waiting-retry"),
        3 => ("magenta", "backoff"),
        4 => ("green", "complete"),
        5 => ("red", "failed"),
        _ => ("white", "unknown"),
    }
}

fn chunk_style(color: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "  [{{prefix:.{color}}}] [{{bar:32.{color}/blue}}] {{bytes}}/{{total_bytes}} {{msg}}"
    ))
    .expect("valid chunk progress template")
    .progress_chars("##-")
}

fn format_speed(bytes_per_sec: f64) -> String {
    format!("{:.2} MiB/s", bytes_per_sec / 1024.0 / 1024.0)
}

fn format_duration(duration: std::time::Duration) -> String {
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

fn join_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn build_output_path(remote_path: &str) -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let file_name = Path::new(remote_path).file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "remote path has no file name",
        )
    })?;
    let mut path = PathBuf::from(OUTPUT_DIR);
    path.push(file_name);
    Ok(path)
}
