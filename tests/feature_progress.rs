#![cfg(feature = "progress")]

use std::time::Duration;

use mockito::Server;
use simple_downloader::{DownloadInfo, Downloader};
use tempfile::NamedTempFile;
use tokio::sync::oneshot;

fn read_file(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read downloaded file")
}

#[tokio::test]
async fn progress_feature_emits_download_events_via_run_callback() {
    let mut server = Server::new_async().await;
    let body = vec![42_u8; 2 * 1024 * 1024];

    let _head = server
        .mock("HEAD", "/file")
        .with_status(200)
        .with_header("Content-Length", body.len().to_string().as_str())
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;

    let get = server
        .mock("GET", "/file")
        .match_header("Range", format!("bytes=0-{}", body.len() - 1).as_str())
        .with_status(206)
        .with_header(
            "Content-Range",
            format!("bytes 0-{}/{}", body.len() - 1, body.len()).as_str(),
        )
        .with_body(body.as_slice())
        .create_async()
        .await;

    let output = NamedTempFile::new().expect("temp output file");
    let path = output.path().to_path_buf();
    let expected_size = body.len() as u64;
    let (event_tx, event_rx) = oneshot::channel();

    Downloader::builder(
        format!("{}/file", server.url()),
        path.to_string_lossy().to_string(),
    )
    .workers(1)
    .update_interval(0.01)
    .run(move |total_size, mut info_rx| async move {
        let mut event_tx = Some(event_tx);
        let mut saw_chunk_progress = false;
        let mut saw_completion = false;

        while let Ok(info) = info_rx.recv().await {
            match info {
                DownloadInfo::ChunkProgress { downloaded, .. } if downloaded > 0 => {
                    saw_chunk_progress = true;
                }
                DownloadInfo::DownloadComplete(_) => {
                    saw_completion = true;
                }
                _ => {}
            }

            if saw_chunk_progress && saw_completion {
                if let Some(event_tx) = event_tx.take() {
                    let _ = event_tx.send((total_size, saw_chunk_progress, saw_completion));
                }
                break;
            }
        }

        if let Some(event_tx) = event_tx.take() {
            let _ = event_tx.send((total_size, saw_chunk_progress, saw_completion));
        }
    })
    .await
    .expect("download succeeds");

    let (reported_total_size, saw_chunk_progress, saw_completion) =
        tokio::time::timeout(Duration::from_secs(2), event_rx)
            .await
            .expect("progress callback completed in time")
            .expect("progress callback reported its observations");

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(reported_total_size, expected_size);
    assert!(
        saw_chunk_progress,
        "expected to observe ChunkProgress events"
    );
    assert!(
        saw_completion,
        "expected to observe DownloadComplete events"
    );
    assert_eq!(read_file(&path), body);
    get.assert_async().await;
}
