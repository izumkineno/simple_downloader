use mockito::Server;
use simple_downloader::Downloader;
use tempfile::NamedTempFile;

#[tokio::test]
async fn missing_content_length_streaming_fallback() {
    let mut server = Server::new_async().await;
    let body = b"chunked-body-no-length-12345".to_vec();

    // HEAD 返回 200 但无 Content-Length
    let _head = server
        .mock("HEAD", "/file")
        .with_status(200)
        .create_async()
        .await;
    // Range 探测 GET bytes=0-0 返回 200 无 Content-Length（无 Range 支持）chunked
    let body_clone = body.clone();
    let _probe = server
        .mock("GET", "/file")
        .match_header("Range", "bytes=0-0")
        .with_status(200)
        .with_chunked_body(move |w| w.write_all(&body_clone))
        .create_async()
        .await;
    // 流式下载的真实 GET（无 Range）返回完整 body，chunked
    let body_clone2 = body.clone();
    let _get = server
        .mock("GET", "/file")
        .with_status(200)
        .with_chunked_body(move |w| w.write_all(&body_clone2))
        .create_async()
        .await;

    let out = NamedTempFile::new().unwrap();
    let path = out.path().to_path_buf();

    Downloader::builder(
        format!("{}/file", server.url()),
        path.to_string_lossy().to_string(),
    )
    .workers(4)
    .download()
    .await
    .expect("streaming fallback should succeed");

    let got = std::fs::read(&path).unwrap();
    assert_eq!(got, body);
}

#[tokio::test]
async fn zero_byte_still_works() {
    // 0 字节的 is_complete 语义：MonitorUpdate(0,0) 应完成
    use simple_downloader::DownloadInfo;
    let info = DownloadInfo::MonitorUpdate {
        total_size: 0,
        total_downloaded: 0,
        total_speed: 0.0,
        chunk_details: vec![],
        eta_secs: None,
        pieces: Vec::new(),
    };
    assert!(info.is_complete(), "0 字节应完成");
    let not_yet = DownloadInfo::MonitorUpdate {
        total_size: 0,
        total_downloaded: 10,
        total_speed: 0.0,
        chunk_details: vec![],
        eta_secs: None,
        pieces: Vec::new(),
    };
    assert!(!not_yet.is_complete(), "未知大小 0/10 不应完成");
}
