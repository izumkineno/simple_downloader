use bytes::Bytes;
use faststr::FastStr;
use mockito::Server;
use reqwest::Client;
use simple_downloader::internal::DownloadCmd;
use simple_downloader::util::*;
use std::io::Write;
use tempfile::NamedTempFile;
use tokio::fs;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn test_get_file_info_head_success() {
    let mut server = Server::new_async().await;
    let mock = server
        .mock("HEAD", "/testfile")
        .with_status(200)
        .with_header("Content-Length", "102400")
        .with_header("Accept-Ranges", "bytes")
        .create_async()
        .await;

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let (size, accept_ranges) = get_file_info(&client, &url).await.unwrap();

    assert_eq!(size, 102400);
    assert!(accept_ranges);
    mock.assert_async().await;
}

#[cfg(feature = "resume")]
#[tokio::test]
async fn file_writer_task_with_resume_does_not_truncate_existing_file() {
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(b"existing-data").unwrap();
    let path = temp_file.path().to_str().unwrap();

    let (tx, handle) = file_writer_task_with_resume(FastStr::new(path), 32, false, None)
        .await
        .unwrap();
    tx.send(DownloadCmd::WriteFile {
        offset: 16,
        data: Bytes::from_static(b"tail"),
    })
    .await
    .unwrap();
    tx.send(DownloadCmd::TerminateAll).await.unwrap();
    handle.await.unwrap();

    let content = std::fs::read(path).unwrap();
    assert_eq!(&content[..13], b"existing-data");
    assert_eq!(&content[16..20], b"tail");
}

#[tokio::test]
async fn test_get_file_info_range_get_success() {
    let mut server = Server::new_async().await;
    // 模拟HEAD请求失败
    let mock_head = server
        .mock("HEAD", "/testfile")
        .with_status(405)
        .create_async()
        .await;

    let mock_get = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=0-0")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-0/102400")
        .with_body("a")
        .create_async()
        .await;

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let (size, accept_ranges) = get_file_info(&client, &url).await.unwrap();

    assert_eq!(size, 102400);
    assert!(accept_ranges);
    mock_head.assert_async().await;
    mock_get.assert_async().await;
}

#[tokio::test]
async fn test_get_file_info_fallback_to_content_length() {
    let mut server = Server::new_async().await;
    // 模拟HEAD请求失败
    let mock_head = server
        .mock("HEAD", "/testfile")
        .with_status(405)
        .create_async()
        .await;

    // 模拟范围请求不返回Content-Range
    let mock_get = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=0-0")
        .with_status(200)
        .with_header("Content-Length", "102400")
        .with_body(vec![0u8; 102400])
        .create_async()
        .await;

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let (size, accept_ranges) = get_file_info(&client, &url).await.unwrap();

    assert_eq!(size, 102400);
    assert!(!accept_ranges);
    mock_head.assert_async().await;
    mock_get.assert_async().await;
}

#[tokio::test]
async fn test_file_writer_task() {
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_str().unwrap();
    let file_size = 100u64;

    // 创建写入任务 — P0-4 streaming: no preallocation, file grows on demand
    let (tx, handle) = file_writer_task(FastStr::new(path), file_size)
        .await
        .unwrap();

    // 写入多个分片数据
    tx.send(DownloadCmd::WriteFile {
        offset: 0,
        data: Bytes::from_static(b"Hello"),
    })
    .await
    .unwrap();
    tx.send(DownloadCmd::WriteFile {
        offset: 10,
        data: Bytes::from_static(b"World"),
    })
    .await
    .unwrap();
    tx.send(DownloadCmd::TerminateAll).await.unwrap();

    // 等待写入完成
    handle.await.unwrap().unwrap();

    // 读取文件内容验证 — streaming: file length is max written offset + len (15), not preallocated 100
    let mut file = fs::File::open(path).await.unwrap();
    let metadata = file.metadata().await.unwrap();
    // streaming: file should be at least 15 bytes (highest write), not preallocated to 100
    assert!(metadata.len() >= 15, "streaming file should be at least 15 bytes, got {}", metadata.len());
    assert!(metadata.len() <= file_size, "streaming file should not exceed file_size");

    let mut content = Vec::new();
    file.read_to_end(&mut content).await.unwrap();

    assert_eq!(&content[0..5], b"Hello");
    assert_eq!(&content[10..15], b"World");
    // 中间未写入部分应该是0填充 (sparse hole)
    for i in 5..10 {
        assert_eq!(content[i], 0);
    }
}
