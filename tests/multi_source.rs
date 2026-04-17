use assert2::assert;
use mockito::Server;
use reqwest::ClientBuilder;
use simple_downloader::lane::{LaneCandidate, LaneHealth, LaneModel, LaneScheduler};
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig};
use tempfile::NamedTempFile;

mod test_server_harness;

use test_server_harness::{ServerSpec, TestServerCluster};

fn read_file(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read downloaded file")
}

fn deterministic_payload(size: usize) -> Vec<u8> {
    (0..size)
        .map(|index| ((index.wrapping_mul(31).wrapping_add(17)) % 251) as u8)
        .collect()
}

async fn download_from_sources(
    output_path: &std::path::Path,
    workers: u64,
    sources: Vec<SourceConfig>,
) {
    let config = MultiSourceConfig::new(output_path.to_string_lossy().to_string(), workers, 0.05)
        .with_max_chunks_per_lane(1)
        .with_sources(sources);

    let downloader = Downloader::new_multi(config, ClientBuilder::new);
    downloader
        .run(|_, _| async {})
        .await
        .expect("download succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[test]
fn per_source_lane_model_shares_capacity_across_proxies() {
    let scheduler = LaneScheduler::from_candidates(
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

    let mut scheduler = scheduler;
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
        LaneModel::PerSourceProxy,
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
    assert!(scheduler.best_lane().expect("fallback").as_str() == "lane-b");
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

    let config = MultiSourceConfig::new(path.to_string_lossy().to_string(), 1, 0.05)
        .with_lane_model(LaneModel::PerSourceProxy)
        .with_sources(vec![
            SourceConfig::new(format!("{}/file", bad.url())).with_id("bad"),
            SourceConfig::new(format!("{}/file", good.url())).with_id("good"),
        ]);

    let downloader = Downloader::new_multi(config, ClientBuilder::new);

    downloader
        .run(|_, _| async {})
        .await
        .expect("download succeeds");
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
        .with_body(body[..split].to_vec())
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
        .with_body(body[split..].to_vec())
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
    downloader
        .run(|_, _| async {})
        .await
        .expect("download succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(read_file(&path) == body);

    first_head.assert_async().await;
    second_head.assert_async().await;
    first_get.assert_async().await;
    second_get.assert_async().await;
}

#[tokio::test]
async fn test_server_multi_source_downloads_byte_correct_file_from_fast_and_slow_sources() {
    let file_name = "fast-slow.bin";
    let body = deterministic_payload(2 * 1024 * 1024);
    let cluster = TestServerCluster::start(
        file_name,
        &body,
        &[
            ServerSpec {
                id: "fast",
                total_max_speed: "16m",
                per_thread_max_speed: "16m",
            },
            ServerSpec {
                id: "slow",
                total_max_speed: "512k",
                per_thread_max_speed: "512k",
            },
        ],
    )
    .await
    .expect("test servers start");

    let temp = NamedTempFile::new().expect("temp file");
    let path = temp.path().to_path_buf();
    download_from_sources(
        &path,
        2,
        vec![
            SourceConfig::new(cluster.source_url(0, file_name)).with_id("fast"),
            SourceConfig::new(cluster.source_url(1, file_name)).with_id("slow"),
        ],
    )
    .await;

    assert!(read_file(&path) == body);
    assert!(cluster.get_count(0, file_name).await.expect("fast stats") > 0);
    assert!(cluster.get_count(1, file_name).await.expect("slow stats") > 0);
}

#[tokio::test]
async fn test_server_multi_source_downloads_byte_correct_file_from_three_heterogeneous_sources() {
    let file_name = "three-sources.bin";
    let body = deterministic_payload(3 * 1024 * 1024);
    let cluster = TestServerCluster::start(
        file_name,
        &body,
        &[
            ServerSpec {
                id: "fast",
                total_max_speed: "16m",
                per_thread_max_speed: "16m",
            },
            ServerSpec {
                id: "medium",
                total_max_speed: "4m",
                per_thread_max_speed: "4m",
            },
            ServerSpec {
                id: "slow",
                total_max_speed: "768k",
                per_thread_max_speed: "768k",
            },
        ],
    )
    .await
    .expect("test servers start");

    let temp = NamedTempFile::new().expect("temp file");
    let path = temp.path().to_path_buf();
    download_from_sources(
        &path,
        3,
        vec![
            SourceConfig::new(cluster.source_url(0, file_name)).with_id("fast"),
            SourceConfig::new(cluster.source_url(1, file_name)).with_id("medium"),
            SourceConfig::new(cluster.source_url(2, file_name)).with_id("slow"),
        ],
    )
    .await;

    assert!(read_file(&path) == body);
    assert!(cluster.get_count(0, file_name).await.expect("fast stats") > 0);
    assert!(cluster.get_count(1, file_name).await.expect("medium stats") > 0);
    assert!(cluster.get_count(2, file_name).await.expect("slow stats") > 0);
}

#[tokio::test]
async fn test_server_multi_source_skips_invalid_source_and_uses_remaining_throttled_sources() {
    let file_name = "valid-sources.bin";
    let body = deterministic_payload(2 * 1024 * 1024);
    let cluster = TestServerCluster::start(
        file_name,
        &body,
        &[
            ServerSpec {
                id: "valid-a",
                total_max_speed: "4m",
                per_thread_max_speed: "4m",
            },
            ServerSpec {
                id: "valid-b",
                total_max_speed: "1m",
                per_thread_max_speed: "1m",
            },
        ],
    )
    .await
    .expect("test servers start");

    let temp = NamedTempFile::new().expect("temp file");
    let path = temp.path().to_path_buf();
    download_from_sources(
        &path,
        2,
        vec![
            SourceConfig::new(cluster.missing_url(0)).with_id("invalid"),
            SourceConfig::new(cluster.source_url(0, file_name)).with_id("valid-a"),
            SourceConfig::new(cluster.source_url(1, file_name)).with_id("valid-b"),
        ],
    )
    .await;

    assert!(read_file(&path) == body);
    assert!(cluster.get_count(0, "missing.bin").await.expect("missing stats") > 0);
    assert!(cluster.get_count(0, file_name).await.expect("valid-a stats") > 0);
    assert!(cluster.get_count(1, file_name).await.expect("valid-b stats") > 0);
}
