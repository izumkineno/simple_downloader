#![cfg(all(feature = "resume", feature = "multi-source"))]

use simple_downloader::{DEFAULT_SEGMENT_SIZE, ResumeMetadata, hash_bytes, metadata_path_for};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod test_server_harness;

const FILE_SIZE: usize = 8 * 1024 * 1024;
const WORKERS: &str = "2";
const UPDATE_INTERVAL: &str = "0.05";
const TOTAL_MAX_SPEED: &str = "1m";
const PER_THREAD_MAX_SPEED: &str = "1m";
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_AFTER_INTERRUPT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn deterministic_bytes(len: usize) -> Vec<u8> {
    test_server_harness::deterministic_bytes(len)
}

fn workspace_file(root: &TempDir, name: &str) -> PathBuf {
    root.path().join(name)
}

fn read_file(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read file")
}

fn assert_file_eq(path: &Path, expected: &[u8]) {
    let actual = read_file(path);
    assert_eq!(
        actual.len(),
        expected.len(),
        "downloaded file length mismatch"
    );
    assert_eq!(
        hash_bytes(&actual),
        hash_bytes(expected),
        "downloaded file hash mismatch"
    );
}

#[tokio::test]
async fn single_source_console_interrupt_resumes_after_restart() {
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "single-process.bin");
    let body = deterministic_bytes(FILE_SIZE);
    let file = test_server_harness::TestServerFile::new("single-process.bin", body.clone())
        .expect("test file");
    let server = test_server_harness::RunningTestServer::spawn(
        file.directory(),
        TOTAL_MAX_SPEED,
        PER_THREAD_MAX_SPEED,
    )
    .await
    .expect("test server");

    let mut child = spawn_single_source(&server.url_for(&file.name), &output).expect("spawn child");
    wait_for_resume_progress(&output).await;
    interrupt_child(&mut child).expect("interrupt child");
    wait_for_exit(&mut child, EXIT_AFTER_INTERRUPT_TIMEOUT).await;

    run_single_source(&server.url_for(&file.name), &output).await;
    assert_file_eq(&output, &body);
}

#[tokio::test]
async fn multi_source_kill_resumes_after_restart() {
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "multi-process.bin");
    let body = deterministic_bytes(FILE_SIZE);
    let file = test_server_harness::TestServerFile::new("multi-process.bin", body.clone())
        .expect("test file");
    let first = test_server_harness::RunningTestServer::spawn(file.directory(), "1m", "1m")
        .await
        .expect("first server");
    let second = test_server_harness::RunningTestServer::spawn(file.directory(), "768k", "768k")
        .await
        .expect("second server");

    let sources = vec![first.url_for(&file.name), second.url_for(&file.name)];
    let mut child = spawn_multi_source(&output, &sources).expect("spawn multi child");
    wait_for_resume_progress_bytes(&output, 1024 * 1024).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    child.kill().expect("kill child");
    wait_for_exit(&mut child, EXIT_AFTER_INTERRUPT_TIMEOUT).await;

    run_multi_source(&output, &sources).await;
    assert_file_eq(&output, &body);
}

async fn run_single_source(url: &str, output: &Path) {
    let output = Command::new(harness_path())
        .args([
            "single",
            url,
            output.to_string_lossy().as_ref(),
            WORKERS,
            UPDATE_INTERVAL,
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run harness");
    assert!(
        output.status.success(),
        "resume_harness failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
}

async fn run_multi_source(output: &Path, sources: &[String]) {
    let mut command = Command::new(harness_path());
    command
        .args([
            "multi",
            output.to_string_lossy().as_ref(),
            WORKERS,
            UPDATE_INTERVAL,
        ])
        .args(sources);
    let output = command.stdin(Stdio::null()).output().expect("run harness");
    assert!(
        output.status.success(),
        "resume_harness failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn spawn_single_source(url: &str, output: &Path) -> io::Result<Child> {
    let mut command = base_command();
    command.args([
        "single",
        url,
        output.to_string_lossy().as_ref(),
        WORKERS,
        UPDATE_INTERVAL,
    ]);
    command.spawn()
}

fn spawn_multi_source(output: &Path, sources: &[String]) -> io::Result<Child> {
    let mut command = base_command();
    command.args([
        "multi",
        output.to_string_lossy().as_ref(),
        WORKERS,
        UPDATE_INTERVAL,
    ]);
    command.args(sources);
    command.spawn()
}

fn base_command() -> Command {
    let mut command = Command::new(harness_path());
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    command
}

fn harness_path() -> PathBuf {
    static HARNESS_PATH: OnceLock<PathBuf> = OnceLock::new();
    HARNESS_PATH
        .get_or_init(|| {
            build_resume_harness();
            example_binary_path("resume_harness")
        })
        .clone()
}

fn build_resume_harness() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args([
            "build",
            "--example",
            "resume_harness",
            "--features",
            "resume,multi-source",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("build resume_harness example");

    assert!(
        output.status.success(),
        "failed to build resume_harness example: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn example_binary_path(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("current test binary path");
    path.pop();
    path.pop();
    path.push("examples");
    path.push(name);

    #[cfg(windows)]
    path.set_extension("exe");

    assert!(
        path.exists(),
        "example binary does not exist: {}",
        path.display()
    );
    path
}

async fn wait_for_resume_progress(output: &Path) {
    wait_for_resume_progress_bytes(output, DEFAULT_SEGMENT_SIZE).await;
}

async fn wait_for_resume_progress_bytes(output: &Path, minimum_completed_bytes: u64) {
    let metadata_path = metadata_path_for(output);
    let deadline = Instant::now() + WAIT_TIMEOUT;

    while Instant::now() < deadline {
        if let Ok(metadata) = ResumeMetadata::load(&metadata_path) {
            if metadata.completed_bytes() >= minimum_completed_bytes {
                return;
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    panic!(
        "timed out waiting for resumable progress in {}",
        metadata_path.display()
    );
}

async fn wait_for_exit(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("query child").is_some() {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let _ = child.kill();
    let _ = child.wait();
    panic!("child did not exit within {:?}", timeout);
}

#[cfg(windows)]
fn interrupt_child(child: &mut Child) -> io::Result<()> {
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
    }

    const CTRL_BREAK_EVENT: u32 = 1;
    let result = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn interrupt_child(child: &mut Child) -> io::Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const SIGINT: i32 = 2;
    let result = unsafe { kill(child.id() as i32, SIGINT) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
