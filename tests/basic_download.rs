use mockito::Server;
use simple_downloader::Downloader;
use tempfile::NamedTempFile;

fn read_file(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read downloaded file")
}

#[tokio::test]
async fn builder_downloads_file_with_default_path() {
    let mut server = Server::new_async().await;
    let body = b"builder-default-download";

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

    Downloader::builder(
        format!("{}/file", server.url()),
        path.to_string_lossy().to_string(),
    )
    .workers(2)
    .download()
    .await
    .expect("download succeeds");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(read_file(&path), body);
    get.assert_async().await;
}
