use mockito::Server;
use reqwest::Client;
use simple_downloader::ChunkId;
use simple_downloader::chunk::*;
use simple_downloader::internal::{DownloadCmd, DownloadInfo};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_chunk_download_success() {
    // 创建模拟服务器
    let mut server = Server::new_async().await;
    let test_data = b"Hello World! This is a test file content.";

    // 模拟范围请求
    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=0-44")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-44/45")
        .with_body(test_data)
        .create_async()
        .await;

    // 创建通道
    let (cmd_tx, mut cmd_rx) = mpsc::channel(10);
    // 命令广播通道：发送命令给chunk
    let (_cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    // 状态广播通道：chunk发送状态更新
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    // 创建请求构建器
    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    // 启动chunk任务
    let chunk_id: ChunkId = 1;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 0, 44).await;
    });

    // 收集写入命令
    let mut received_data = Vec::new();
    let mut offset = 0;
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            DownloadCmd::WriteFile { offset: off, data } => {
                assert_eq!(off, offset);
                received_data.extend_from_slice(&data);
                offset += data.len() as u64;
            }
            DownloadCmd::TerminateAll => break,
            _ => {}
        }
        if offset >= 45 {
            break;
        }
    }

    // 检查接收到的数据是否正确
    assert_eq!(received_data, test_data);

    // 检查是否收到完成消息
    let mut complete_received = false;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::DownloadComplete(id) if id == chunk_id => {
                complete_received = true;
                break;
            }
            DownloadInfo::ChunkProgress { .. } => continue,
            _ => {}
        }
    }
    assert!(complete_received);

    mock.assert_async().await;
    handle.await.unwrap();
}

#[tokio::test]
#[ignore = "测试需要更复杂的延迟响应模拟，暂时跳过"]
async fn test_chunk_bisect() {
    // 创建模拟服务器，返回一个较大的数据流
    let mut server = Server::new_async().await;
    let test_data = vec![0u8; 100 * 1024]; // 100KB数据

    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=0-99999")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-99999/100000")
        .with_body(test_data)
        .create_async()
        .await;

    // 创建通道
    let (cmd_tx, _cmd_rx) = mpsc::channel(100);
    // 命令广播通道：发送命令给chunk
    let (cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    // 状态广播通道：chunk发送状态更新
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    // 创建请求构建器
    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    // 启动chunk任务
    let chunk_id: ChunkId = 1;
    let handle = tokio::spawn(async move {
        chunk_run(
            chunk_id,
            cmd_tx,
            cmd_bd_rx,
            info_bd_tx.clone(),
            rb,
            0,
            99999,
        )
        .await;
    });

    // 等待下载开始
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // 发送分割命令
    let _ = cmd_bd_tx.send(DownloadCmd::BisectDownload { id: chunk_id });

    // 检查是否收到分割事件
    let mut bisected_received = false;
    let mut new_start = 0;
    let mut new_end = 0;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::ChunkBisected {
                original_id,
                new_start: ns,
                new_end: ne,
            } if original_id == chunk_id => {
                bisected_received = true;
                new_start = ns;
                new_end = ne;
                break;
            }
            DownloadInfo::ChunkProgress { .. } => continue,
            _ => {}
        }
    }
    assert!(bisected_received);
    assert_eq!(new_start, 50001); // 中间位置+1
    assert_eq!(new_end, 99999);

    // 发送终止命令
    let _ = cmd_bd_tx.send(DownloadCmd::TerminateAll);

    mock.assert_async().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn test_chunk_request_failure() {
    // 创建模拟服务器，返回错误
    let mut server = Server::new_async().await;

    let mock = server
        .mock("GET", "/testfile")
        .with_status(500)
        .create_async()
        .await;

    // 创建通道
    let (cmd_tx, _cmd_rx) = mpsc::channel(10);
    // 命令广播通道：发送命令给chunk
    let (_cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    // 状态广播通道：chunk发送状态更新
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    // 创建请求构建器
    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    // 启动chunk任务
    let chunk_id: ChunkId = 1;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 0, 100).await;
    });

    // 检查是否收到失败消息
    let mut failed_received = false;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::ChunkFailed { id, .. } if id == chunk_id => {
                failed_received = true;
                break;
            }
            _ => {}
        }
    }
    assert!(failed_received);

    mock.assert_async().await;
    handle.await.unwrap();
}
