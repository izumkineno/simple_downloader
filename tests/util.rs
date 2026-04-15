use simple_downloader::util::*;
use simple_downloader::types::DownloadCmd;
use faststr::FastStr;
use mockito::Server;
use reqwest::Client;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tempfile::NamedTempFile;
use bytes::Bytes;

#[tokio::test]
async fn test_get_file_info_head_success() {
    let mut server = Server::new_async().await;
    let mock = server.mock("HEAD", "/testfile")
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

#[tokio::test]
async fn test_get_file_info_range_get_success() {
    let mut server = Server::new_async().await;
    // 模拟HEAD请求失败
    let mock_head = server.mock("HEAD", "/testfile")
        .with_status(405)
        .create_async()
        .await;
    
    let mock_get = server.mock("GET", "/testfile")
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
    let mock_head = server.mock("HEAD", "/testfile")
        .with_status(405)
        .create_async()
        .await;
    
    // 模拟范围请求不返回Content-Range
    let mock_get = server.mock("GET", "/testfile")
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

    // 创建写入任务
    let tx = file_writer_task(FastStr::new(path), file_size).await.unwrap();

    // 写入多个分片数据
    tx.send(DownloadCmd::WriteFile { offset: 0, data: Bytes::from_static(b"Hello") }).await.unwrap();
    tx.send(DownloadCmd::WriteFile { offset: 10, data: Bytes::from_static(b"World") }).await.unwrap();
    tx.send(DownloadCmd::TerminateAll).await.unwrap();

    // 等待写入完成
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // 读取文件内容验证
    let mut file = fs::File::open(path).await.unwrap();
    let metadata = file.metadata().await.unwrap();
    assert_eq!(metadata.len(), file_size);

    let mut content = Vec::new();
    file.read_to_end(&mut content).await.unwrap();
    
    assert_eq!(&content[0..5], b"Hello");
    assert_eq!(&content[10..15], b"World");
    // 中间未写入部分应该是0填充
    for i in 5..10 {
        assert_eq!(content[i], 0);
    }
}