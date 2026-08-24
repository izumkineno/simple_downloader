#![cfg(feature = "multi-source")]

use assert2::assert;
use mockito::Server;
use reqwest::ClientBuilder;
use simple_downloader::{
    Downloader, LaneCandidate, LaneHealth, LaneModel, LaneScheduler, MultiSourceConfig,
    SourceConfig,
};
use tempfile::NamedTempFile;

mod test_server_harness;

fn read_file(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read downloaded file")
}

fn temp_output_path() -> std::path::PathBuf {
    NamedTempFile::new()
        .expect("temp output file")
        .into_temp_path()
        .to_path_buf()
}

async fn run_multi_source_download(
    sources: Vec<SourceConfig>,
    workers: u64,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let path = temp_output_path();
    let config = MultiSourceConfig::new(path.to_string_lossy().to_string(), workers, 0.05)
        .with_sources(sources);
    let downloader = Downloader::new_multi(config, ClientBuilder::new);
    downloader.download().await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    Ok(path)
}

async fn assert_all_servers_served_ranges(
    servers: &[test_server_harness::RunningTestServer],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for server in servers {
        let stats = server.stats().await?;
        assert!(
            stats.get("range_requests").copied().unwrap_or_default() > 0,
            "expected server {} to serve at least one range request; stats={stats:?}",
            server.base_url()
        );
    }
    Ok(())
}

#[test]
fn per_source_lane_model_shares_capacity_across_proxies() {
    let mut scheduler = LaneScheduler::from_candidates(
        vec![
            LaneCandidate::new("lane-a", "source-a", None::<&str>, 120.0),
            LaneCandidate::new("lane-b", "source-a", Some("proxy-b"), 90.0),
            LaneCandidate::new("lane-c", "source-b", None::<&str>, 80.0),
        ],
        LaneModel::PerSource,
        4,
        2,
        Some(2),
    );

    assert!(scheduler.available_capacity() == 4);
    assert!(scheduler.lane_ids().len() == 2);
    let first = scheduler.best_lane().expect("first lane");
    let second = scheduler.best_lane().expect("second lane");

    assert!(first.as_str() == "source-a");
    assert!(second.as_str() == "source-a");

    scheduler.assign_chunk(first.clone());
    scheduler.assign_chunk(second.clone());

    let third = scheduler.best_lane().expect("third lane");
    assert!(third.as_str() == "source-b");
}

#[test]
fn failing_lane_is_blacklisted_until_released() {
    let mut scheduler = LaneScheduler::from_candidates(
        vec![
            LaneCandidate::new("lane-a", "source-a", None::<&str>, 100.0),
            LaneCandidate::new("lane-b", "source-b", None::<&str>, 90.0),
        ],
        LaneModel::PerSource,
        2,
        1,
        None,
    );

    let lane = scheduler.best_lane().expect("lane");
    scheduler.assign_chunk(lane.clone());
    scheduler.record_failure(&lane);
    scheduler.record_failure(&lane);
    scheduler.record_failure(&lane);

    assert!(scheduler.lane_health(&lane) == Some(LaneHealth::Blacklisted));
    assert!(scheduler.best_lane().expect("fallback").as_str() == "source-b");
}

#[cfg(feature = "proxy")]
#[test]
fn per_source_proxy_lane_model_keeps_distinct_proxy_lanes() {
    let scheduler = LaneScheduler::from_candidates(
        vec![
            LaneCandidate::new("lane-a", "source-a", None::<&str>, 120.0),
            LaneCandidate::new("lane-b", "source-a", Some("proxy-b"), 90.0),
            LaneCandidate::new("lane-c", "source-b", None::<&str>, 80.0),
        ],
        LaneModel::PerSourceProxy,
        4,
        2,
        Some(2),
    );

    assert!(scheduler.lane_ids().len() == 3);
}

#[tokio::test]
async fn multi_source_downloader_skips_invalid_probe_source_and_downloads() {
    let mut bad = Server::new_async().await;
    let mut good = Server::new_async().await;
    let body = b"multi-source-content-for-test";

    let bad_head = bad
        .mock("HEAD", "/file")
        .with_status(503)
        .create_async()
        .await;

    let good_head = good
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;

    let good_get = good
        .mock("GET", "/file")
        .match_header(
            "Range",
            format!("bytes=0-{}", body.len().saturating_sub(1)).as_str(),
        )
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes 0-{}/{}", body.len().saturating_sub(1), body.len()).as_str(),
        )
        .with_body(body.as_slice())
        .create_async()
        .await;

    let temp = NamedTempFile::new().expect("temp file");
    let path = temp.path().to_path_buf();

    let config =
        MultiSourceConfig::new(path.to_string_lossy().to_string(), 1, 0.05).with_sources(vec![
            SourceConfig::new(format!("{}/file", bad.url())).with_id("bad"),
            SourceConfig::new(format!("{}/file", good.url())).with_id("good"),
        ]);

    let downloader = Downloader::new_multi(config, ClientBuilder::new);

    downloader.download().await.expect("download succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(read_file(&path) == body);

    bad_head.assert_async().await;
    good_head.assert_async().await;
    good_get.assert_async().await;
}

#[tokio::test]
async fn multi_source_downloader_uses_multiple_sources_for_initial_chunks() {
    let mut first = Server::new_async().await;
    let mut second = Server::new_async().await;
    let body = vec![7_u8; 2 * 1024 * 1024];
    let split = body.len() / 2;

    let first_head = first
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;
    let second_head = second
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;
    let first_get = first
        .mock("GET", "/file")
        .match_header("Range", format!("bytes=0-{}", split - 1).as_str())
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes 0-{}/{}", split - 1, body.len()).as_str(),
        )
        .with_body(body[..split].as_ref())
        .create_async()
        .await;
    let second_get = second
        .mock("GET", "/file")
        .match_header(
            "Range",
            format!("bytes={}-{}", split, body.len() - 1).as_str(),
        )
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes {}-{}/{}", split, body.len() - 1, body.len()).as_str(),
        )
        .with_body(body[split..].as_ref())
        .create_async()
        .await;

    let temp = NamedTempFile::new().expect("temp file");
    let path = temp.path().to_path_buf();
    let config =
        MultiSourceConfig::new(path.to_string_lossy().to_string(), 2, 0.05).with_sources(vec![
            SourceConfig::new(format!("{}/file", first.url())).with_id("first"),
            SourceConfig::new(format!("{}/file", second.url())).with_id("second"),
        ]);

    let downloader = Downloader::new_multi(config, ClientBuilder::new);
    downloader.download().await.expect("download succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(read_file(&path) == body);

    first_head.assert_async().await;
    second_head.assert_async().await;
    first_get.assert_async().await;
    second_get.assert_async().await;
}

#[tokio::test]
async fn test_server_fast_and_slow_sources_download_byte_correct_output() {
    let file = test_server_harness::TestServerFile::new(
        "phase1-fast-slow.bin",
        test_server_harness::deterministic_bytes(2 * 1024 * 1024),
    )
    .expect("test file");
    let fast = test_server_harness::RunningTestServer::spawn(file.directory(), "64m", "64m")
        .await
        .expect("fast test_server");
    let slow = test_server_harness::RunningTestServer::spawn(file.directory(), "16m", "16m")
        .await
        .expect("slow test_server");

    let path = run_multi_source_download(
        vec![
            SourceConfig::new(fast.url_for(&file.name)).with_id("fast"),
            SourceConfig::new(slow.url_for(&file.name)).with_id("slow"),
        ],
        2,
    )
    .await
    .expect("download succeeds");

    assert!(read_file(&path) == file.bytes);
    assert_all_servers_served_ranges(&[fast, slow])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_server_three_heterogeneous_sources_download_byte_correct_output() {
    let file = test_server_harness::TestServerFile::new(
        "phase1-three-source.bin",
        test_server_harness::deterministic_bytes(3 * 1024 * 1024),
    )
    .expect("test file");
    let fastest = test_server_harness::RunningTestServer::spawn(file.directory(), "96m", "96m")
        .await
        .expect("fastest test_server");
    let middle = test_server_harness::RunningTestServer::spawn(file.directory(), "48m", "48m")
        .await
        .expect("middle test_server");
    let slowest = test_server_harness::RunningTestServer::spawn(file.directory(), "24m", "24m")
        .await
        .expect("slowest test_server");

    let path = run_multi_source_download(
        vec![
            SourceConfig::new(fastest.url_for(&file.name)).with_id("fastest"),
            SourceConfig::new(middle.url_for(&file.name)).with_id("middle"),
            SourceConfig::new(slowest.url_for(&file.name)).with_id("slowest"),
        ],
        3,
    )
    .await
    .expect("download succeeds");

    assert!(read_file(&path) == file.bytes);
    assert_all_servers_served_ranges(&[fastest, middle, slowest])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_server_invalid_source_is_skipped_while_valid_throttled_sources_complete() {
    let file = test_server_harness::TestServerFile::new(
        "phase1-invalid-source.bin",
        test_server_harness::deterministic_bytes(2 * 1024 * 1024),
    )
    .expect("test file");
    let valid_a = test_server_harness::RunningTestServer::spawn(file.directory(), "40m", "40m")
        .await
        .expect("valid test_server A");
    let valid_b = test_server_harness::RunningTestServer::spawn(file.directory(), "20m", "20m")
        .await
        .expect("valid test_server B");
    let invalid_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("unused port")
        .local_addr()
        .expect("unused port addr")
        .port();
    let invalid_url = format!("http://127.0.0.1:{invalid_port}/{}", file.name);

    let path = run_multi_source_download(
        vec![
            SourceConfig::new(invalid_url).with_id("invalid"),
            SourceConfig::new(valid_a.url_for(&file.name)).with_id("valid-a"),
            SourceConfig::new(valid_b.url_for(&file.name)).with_id("valid-b"),
        ],
        2,
    )
    .await
    .expect("download succeeds through valid sources");

    assert!(read_file(&path) == file.bytes);
    assert_all_servers_served_ranges(&[valid_a, valid_b])
        .await
        .unwrap();
}
