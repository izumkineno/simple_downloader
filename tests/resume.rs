use mockito::Server;
use reqwest::ClientBuilder;
use simple_downloader::resume::{ResumeMetadata, metadata_path_for};
use simple_downloader::{DownloadError, Downloader, MultiSourceConfig, SourceConfig};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod test_server_harness;

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
        simple_downloader::resume::hash_bytes(&actual),
        simple_downloader::resume::hash_bytes(expected),
        "downloaded file hash mismatch"
    );
}

fn write_partial(path: &Path, bytes: &[u8], len: usize) {
    std::fs::write(path, &bytes[..len]).expect("write partial");
}

async fn run_single_source_download(url: String, output_path: &Path) -> Result<(), DownloadError> {
    Downloader::new(
        url,
        output_path.to_string_lossy().to_string(),
        2,
        0.05,
        ClientBuilder::new,
    )
    .run(|_, _| async {})
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    Ok(())
}

async fn run_multi_source_download(
    sources: Vec<SourceConfig>,
    output_path: &Path,
) -> Result<(), DownloadError> {
    let config = MultiSourceConfig::new(output_path.to_string_lossy().to_string(), 2, 0.05)
        .with_sources(sources);
    Downloader::new_multi(config, ClientBuilder::new)
        .run(|_, _| async {})
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    Ok(())
}

fn seed_metadata_for_prefix(output_path: &Path, file_size: u64, verified_len: usize) {
    let mut metadata = ResumeMetadata::new(file_size, verified_len as u64);
    let bytes = read_file(output_path);
    metadata.set_segment_hash(
        0,
        simple_downloader::resume::hash_bytes(&bytes[..verified_len]),
    );
    metadata
        .save_atomic(&metadata_path_for(output_path))
        .expect("save metadata");
}

#[tokio::test]
async fn single_source_resumes_verified_prefix_without_restarting_from_zero() {
    let mut server = Server::new_async().await;
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "single.bin");
    let body = deterministic_bytes(128 * 1024);
    let verified_len = 64 * 1024;

    write_partial(&output, &body, verified_len);
    seed_metadata_for_prefix(&output, body.len() as u64, verified_len);

    let head = server
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;
    let remaining = server
        .mock("GET", "/file")
        .match_header(
            "Range",
            format!("bytes={}-{}", verified_len, body.len() - 1).as_str(),
        )
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes {}-{}/{}", verified_len, body.len() - 1, body.len()).as_str(),
        )
        .with_body(body[verified_len..].to_vec())
        .create_async()
        .await;

    run_single_source_download(format!("{}/file", server.url()), &output)
        .await
        .expect("resume succeeds");

    assert_file_eq(&output, &body);
    head.assert_async().await;
    remaining.assert_async().await;
}

#[tokio::test]
async fn metadata_without_target_file_is_fail_stop() {
    let mut server = Server::new_async().await;
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "missing.bin");
    let body = deterministic_bytes(16 * 1024);
    let metadata = ResumeMetadata::new(body.len() as u64, 1024);
    metadata
        .save_atomic(&metadata_path_for(&output))
        .expect("save metadata");

    let _head = server
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;

    let error = run_single_source_download(format!("{}/file", server.url()), &output)
        .await
        .expect_err("missing file must fail-stop");

    assert!(matches!(error, DownloadError::ResumeTargetMissing(_)));
    assert!(!output.exists());
}

#[tokio::test]
async fn corrupted_verified_segment_is_invalidated_and_redownloaded() {
    let mut server = Server::new_async().await;
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "corrupt.bin");
    let body = deterministic_bytes(96 * 1024);
    let segment_len = 32 * 1024;

    write_partial(&output, &body, segment_len);
    seed_metadata_for_prefix(&output, body.len() as u64, segment_len);

    let mut corrupt = read_file(&output);
    corrupt[0] ^= 0xff;
    std::fs::write(&output, corrupt).expect("write corrupted output");

    let _head = server
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;
    let full = server
        .mock("GET", "/file")
        .match_header("Range", format!("bytes=0-{}", body.len() - 1).as_str())
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes 0-{}/{}", body.len() - 1, body.len()).as_str(),
        )
        .with_body(body.clone())
        .create_async()
        .await;

    run_single_source_download(format!("{}/file", server.url()), &output)
        .await
        .expect("corrupted segment is redownloaded");

    assert_file_eq(&output, &body);
    full.assert_async().await;
}

#[tokio::test]
async fn disabling_resume_uses_fresh_download_path() {
    let mut server = Server::new_async().await;
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "opt-out.bin");
    let body = deterministic_bytes(64 * 1024);
    let verified_len = 32 * 1024;

    write_partial(&output, &body, verified_len);
    seed_metadata_for_prefix(&output, body.len() as u64, verified_len);

    let _head = server
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;
    let full = server
        .mock("GET", "/file")
        .match_header("Range", format!("bytes=0-{}", body.len() - 1).as_str())
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes 0-{}/{}", body.len() - 1, body.len()).as_str(),
        )
        .with_body(body.clone())
        .create_async()
        .await;

    Downloader::new(
        format!("{}/file", server.url()),
        output.to_string_lossy().to_string(),
        2,
        0.05,
        ClientBuilder::new,
    )
    .with_resume(false)
    .run(|_, _| async {})
    .await
    .expect("fresh download succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_file_eq(&output, &body);
    assert!(!metadata_path_for(&output).exists());
    full.assert_async().await;
}

#[tokio::test]
async fn multi_source_resumes_verified_prefix_through_real_runtime_path() {
    let root = TempDir::new().expect("temp dir");
    let output = workspace_file(&root, "multi.bin");
    let body = deterministic_bytes(128 * 1024);
    let verified_len = 64 * 1024;

    let file =
        test_server_harness::TestServerFile::new("multi.bin", body.clone()).expect("test file");
    let first = test_server_harness::RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .expect("first server");
    let second = test_server_harness::RunningTestServer::spawn(file.directory(), "32m", "32m")
        .await
        .expect("second server");

    write_partial(&output, &body, verified_len);
    seed_metadata_for_prefix(&output, body.len() as u64, verified_len);

    run_multi_source_download(
        vec![
            SourceConfig::new(first.url_for(&file.name)).with_id("first"),
            SourceConfig::new(second.url_for(&file.name)).with_id("second"),
        ],
        &output,
    )
    .await
    .expect("multi-source resume succeeds");

    assert_file_eq(&output, &body);
}
