#[tokio::main]
async fn main() {
    match simple_downloader::Downloader::builder(
        "https://dldir1.qq.com/qqfile/qq/PCQQ9.7.17/QQ9.7.17.29225.exe", // 下载链接
        "QQ9.7.17.29225.exe",                                            // 保存路径
    )
    .workers(16)
    .download()
    .await
    {
        Ok(_) => println!("下载成功！"),
        Err(e) => eprintln!("下载失败: {}", e),
    }
}
