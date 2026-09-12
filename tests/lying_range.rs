use mockito::{Matcher, Server};
use simple_downloader::Downloader;
use tempfile::NamedTempFile;

#[tokio::test]
async fn lying_range_probe_is_distrusted() {
    let mut server = Server::new_async().await;
    let _head = server
        .mock("HEAD", "/f")
        .with_status(200)
        .with_header("Content-Length", "5")
        .create_async()
        .await;
    // 206 配空体：Content-Range 照写总量，体缺失
    let _probe = server
        .mock("GET", "/f")
        .match_header("Range", "bytes=0-0")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-0/5")
        .create_async()
        .await;
    let client = simple_downloader::reqwest::Client::new();
    let err = simple_downloader::util::get_file_info(&client, &format!("{}/f", server.url()))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        simple_downloader::DownloadError::MissingContentLength
    ));
}

#[tokio::test]
async fn lying_range_download_falls_back_to_plain_get() {
    let mut server = Server::new_async().await;
    let full = b"0123456789abcdef".to_vec();
    let _head = server
        .mock("HEAD", "/f")
        .with_status(200)
        .with_header("Content-Length", "5")
        .create_async()
        .await;
    let _probe = server
        .mock("GET", "/f")
        .match_header("Range", "bytes=0-0")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-0/5")
        .create_async()
        .await;
    // 纯 GET 只匹配无 Range 请求，避免盖住上面的探针 mock
    let full_clone = full.clone();
    let _get = server
        .mock("GET", "/f")
        .match_header("Range", Matcher::Missing)
        .with_status(200)
        .with_body(full_clone)
        .create_async()
        .await;
    let out = NamedTempFile::new().unwrap();
    let path = out.path().to_path_buf();
    Downloader::builder(format!("{}/f", server.url()), path.to_string_lossy().to_string())
        .workers(4)
        .download()
        .await
        .expect("lying range must fall back to plain GET");
    assert_eq!(std::fs::read(&path).unwrap(), full);
}
#[tokio::test]
async fn incoherent_range_cache_is_distrusted() {
    // 单字节探针能过（1 字节体），但与整包首字节错位：flingtrainer 实测 Range 回 0x1f，真文件 0x4D
    let mut server = Server::new_async().await;
    let _head = server
        .mock("HEAD", "/f")
        .with_status(200)
        .with_header("Content-Length", "5")
        .create_async()
        .await;
    let _probe = server
        .mock("GET", "/f")
        .match_header("Range", "bytes=0-0")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-0/5")
        .with_body("X")
        .create_async()
        .await;
    let _get = server
        .mock("GET", "/f")
        .match_header("Range", Matcher::Missing)
        .with_status(200)
        .with_body("YBCDE")
        .create_async()
        .await;
    let client = simple_downloader::reqwest::Client::new();
    let err = simple_downloader::util::get_file_info(&client, &format!("{}/f", server.url()))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        simple_downloader::DownloadError::MissingContentLength
    ));
}

#[tokio::test]
async fn coherent_range_cache_stays_ranged() {
    // 首字节一致：诚实服务端不受交叉验证影响，仍走分片
    let mut server = Server::new_async().await;
    let _head = server
        .mock("HEAD", "/f")
        .with_status(200)
        .with_header("Content-Length", "5")
        .create_async()
        .await;
    let _probe = server
        .mock("GET", "/f")
        .match_header("Range", "bytes=0-0")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-0/5")
        .with_body("H")
        .create_async()
        .await;
    let _get = server
        .mock("GET", "/f")
        .match_header("Range", Matcher::Missing)
        .with_status(200)
        .with_body("HELLO")
        .create_async()
        .await;
    let client = simple_downloader::reqwest::Client::new();
    let (size, support) =
        simple_downloader::util::get_file_info(&client, &format!("{}/f", server.url()))
            .await
            .unwrap();
    assert_eq!((size, support), (5, true));
}

#[tokio::test]
async fn incoherent_range_download_falls_back_to_plain_get() {
    let mut server = Server::new_async().await;
    let full = b"YBCDEFGHIJKLMNOP".to_vec();
    let _head = server
        .mock("HEAD", "/f")
        .with_status(200)
        .with_header("Content-Length", "5")
        .create_async()
        .await;
    let _probe = server
        .mock("GET", "/f")
        .match_header("Range", "bytes=0-0")
        .with_status(206)
        .with_header("Content-Range", "bytes 0-0/5")
        .with_body("X")
        .create_async()
        .await;
    let full_clone = full.clone();
    let _get = server
        .mock("GET", "/f")
        .match_header("Range", Matcher::Missing)
        .with_status(200)
        .with_body(full_clone)
        .create_async()
        .await;
    let out = NamedTempFile::new().unwrap();
    let path = out.path().to_path_buf();
    Downloader::builder(format!("{}/f", server.url()), path.to_string_lossy().to_string())
        .workers(4)
        .download()
        .await
        .expect("incoherent range must fall back to plain GET");
    assert_eq!(std::fs::read(&path).unwrap(), full);
}
