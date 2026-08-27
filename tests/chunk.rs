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
    let len = test_data.len() as u64;
    // 模拟范围请求 — Range 与 Content-Range 需与 body 长度一致，避免触发 P0-02 early EOF
    let range_hdr = format!("bytes=0-{}", len - 1);
    let cr_hdr = format!("bytes 0-{}/{}", len - 1, len);
    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", range_hdr.as_str())
        .with_status(206)
        .with_header("Content-Range", cr_hdr.as_str())
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
    let end = len - 1;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 0, end).await;
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
        if offset >= len {
            break;
        }
    }

    // 检查接收到的数据是否正确 — 若不匹配先打印 info 以定位 P0-01/02 校验失败
    if received_data != test_data {
        // 尝试收集 info 以诊断
        let mut diag = Vec::new();
        while let Ok(info) = info_bd_rx.try_recv() {
            diag.push(format!("{:?}", info));
        }
        eprintln!("diag infos before complete check: {:?}", diag);
        eprintln!("received len {}, expected len {}", received_data.len(), test_data.len());
    }
    assert_eq!(received_data, test_data);

    // 检查是否收到完成消息
    let mut complete_received = false;
    let mut all_infos = Vec::new();
    // 使用 try_recv 轮询避免无限阻塞，等待 2s
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(2) {
        match info_bd_rx.try_recv() {
            Ok(info) => {
                eprintln!("info recv: {:?}", info);
                all_infos.push(format!("{:?}", info));
                match info {
                    DownloadInfo::DownloadComplete(id) if id == chunk_id => {
                        complete_received = true;
                        break;
                    }
                    DownloadInfo::ChunkProgress { .. } => continue,
                    DownloadInfo::ChunkFailed { id, error, .. } if id == chunk_id => {
                        eprintln!("ChunkFailed: {}", error);
                        break;
                    }
                    _ => {}
                }
            }
            Err(broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => {
                eprintln!("recv err: {:?}", e);
                break;
            }
        }
    }
    if !complete_received {
        eprintln!("all_infos: {:?}", all_infos);
    }
    assert!(complete_received, "all_infos: {:?}", all_infos);

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
async fn test_chunk_200_single_segment_downgrade_allowed() {
    // P0-01: 200 单段全量允许降级 (start==0)
    let mut server = Server::new_async().await;
    let test_data = b"0123456789"; // 10 bytes
    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=0-9")
        .with_status(200)
        .with_body(test_data)
        .create_async()
        .await;

    let (cmd_tx, mut cmd_rx) = mpsc::channel(10);
    let (_cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    let chunk_id: ChunkId = 10;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 0, 9).await;
    });

    let mut received = Vec::new();
    while let Some(cmd) = cmd_rx.recv().await {
        if let DownloadCmd::WriteFile { data, .. } = cmd {
            received.extend_from_slice(&data);
            break;
        }
    }
    assert_eq!(received, test_data);

    let mut ok = false;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::DownloadComplete(id) if id == chunk_id => {
                ok = true;
                break;
            }
            DownloadInfo::ChunkFailed { id, .. } if id == chunk_id => panic!("expected downgrade success but got ChunkFailed"),
            _ => continue,
        }
    }
    assert!(ok);
    mock.assert_async().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn test_chunk_200_multi_segment_rejected() {
    // P0-01: 200 多段必须失败 (start !=0)
    let mut server = Server::new_async().await;
    let test_data = b"0123456789";
    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=10-19")
        .with_status(200)
        .with_body(test_data)
        .create_async()
        .await;

    let (cmd_tx, _cmd_rx) = mpsc::channel(10);
    let (_cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    let chunk_id: ChunkId = 11;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 10, 19).await;
    });

    let mut failed = false;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::ChunkFailed { id, error, .. } if id == chunk_id => {
                assert!(error.contains("200"), "error should mention 200: {error}");
                failed = true;
                break;
            }
            DownloadInfo::DownloadComplete(id) if id == chunk_id => panic!("multi-segment 200 should not succeed"),
            _ => continue,
        }
    }
    assert!(failed);
    mock.assert_async().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn test_chunk_206_wrong_content_range_rejected() {
    // P0-01: 206 但 Content-Range 与请求不一致 -> ChunkFailed
    let mut server = Server::new_async().await;
    let test_data = b"0123456789";
    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=10-19")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-9/20")
        .with_body(test_data)
        .create_async()
        .await;

    let (cmd_tx, _cmd_rx) = mpsc::channel(10);
    let (_cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    let chunk_id: ChunkId = 12;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 10, 19).await;
    });

    let mut failed = false;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::ChunkFailed { id, error, .. } if id == chunk_id => {
                assert!(error.contains("Content-Range mismatch") || error.contains("mismatch"), "error should mention mismatch: {error}");
                failed = true;
                break;
            }
            DownloadInfo::DownloadComplete(id) if id == chunk_id => panic!("wrong Content-Range should not succeed"),
            _ => continue,
        }
    }
    assert!(failed);
    mock.assert_async().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn test_chunk_early_eof_is_failed() {
    // P0-02: 流提前结束 (None) 且 offset != end+1 -> ChunkFailed, 不发送 DownloadComplete
    let mut server = Server::new_async().await;
    let test_data = vec![0u8; 512]; // 仅 512B，但 Range 请求 0-1023 (1KiB)
    let mock = server
        .mock("GET", "/testfile")
        .match_header("Range", "bytes=0-1023")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-1023/1024")
        .with_body(test_data.clone())
        .create_async()
        .await;

    let (cmd_tx, mut cmd_rx) = mpsc::channel(10);
    let (_cmd_bd_tx, cmd_bd_rx) = broadcast::channel(10);
    let (info_bd_tx, mut info_bd_rx) = broadcast::channel(10);

    let client = Client::new();
    let url = format!("{}/testfile", server.url());
    let rb = client.get(&url);

    let chunk_id: ChunkId = 99;
    let handle = tokio::spawn(async move {
        chunk_run(chunk_id, cmd_tx, cmd_bd_rx, info_bd_tx.clone(), rb, 0, 1023).await;
    });

    let mut received: Vec<u8> = Vec::new();
    while let Some(cmd) = cmd_rx.recv().await {
        if let DownloadCmd::WriteFile { data, .. } = cmd {
            received.extend_from_slice(&data);
        }
    }
    // 仅收到 512B
    assert_eq!(received.len(), 512);

    let mut failed = false;
    let mut completed = false;
    while let Ok(info) = info_bd_rx.recv().await {
        match info {
            DownloadInfo::ChunkFailed { id, error, .. } if id == chunk_id => {
                assert!(error.contains("early EOF"), "error should mention early EOF: {error}");
                failed = true;
                break;
            }
            DownloadInfo::DownloadComplete(id) if id == chunk_id => {
                completed = true;
                break;
            }
            DownloadInfo::ChunkProgress { .. } => continue,
            _ => {}
        }
    }
    assert!(failed, "early EOF should be ChunkFailed, not Complete");
    assert!(!completed);
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
