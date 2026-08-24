#[tokio::main]
async fn main() {
    // 使用公开稳定的测试文件；如需替换为私有源，请改此 URL 与保存路径
    let url = std::env::var("DOWNLOAD_URL")
        .unwrap_or_else(|_| "https://proof.ovh.net/files/10Mio.dat".to_string());
    let output = std::env::var("OUTPUT_PATH").unwrap_or_else(|_| "10Mio.dat".to_string());
    match simple_downloader::Downloader::builder(url, output)
        .workers(16)
        .download()
        .await
    {
        Ok(_) => println!("下载成功！"),
        Err(e) => eprintln!("下载失败: {}", e),
    }
}
