# 常见问题解答

## 基础使用问题

### Q: 最简单的下载怎么实现？
A: 使用 builder 模式只需要几行代码：
```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() {
    Downloader::builder("https://example.com/file.zip", "save_path.zip")
        .download()
        .await
        .unwrap();
}
```

### Q: 如何显示下载进度？
A: 使用 `run()` 方法并提供进度回调函数：
```rust
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .run(|total_size, mut info_rx| async move {
        println!("总大小: {} bytes", total_size);
        while let Ok(info) = info_rx.recv().await {
            println!(
                "进度: {:.1}% | 速度: {:.2} MB/s | 已下载: {} bytes",
                info.progress_percent(),
                info.speed_mbps(),
                info.downloaded_bytes()
            );
        }
    })
    .await
    .unwrap();
```

### Q: 断点续传功能如何使用？
A: 默认已经启用（需要 `resume` feature，默认已开启），下载中断后再次运行相同的代码会自动从断点恢复：
```rust
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .resume(true) // 显式启用，默认是 true
    .download()
    .await
    .unwrap();
```
断点续传的元数据保存在与输出文件同目录下，文件名是 `输出文件名.resume`。

### Q: 如何设置并发下载线程数？
A: 使用 `workers()` 方法配置：
```rust
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .workers(16) // 使用 16 个线程并行下载
    .download()
    .await
    .unwrap();
```
**注意**：如果服务器不支持 Range 请求，或者文件大小小于 1MB，会自动降级为单线程下载。

### Q: 如何配置代理？
A: 通过自定义 reqwest 客户端配置代理：
```rust
use reqwest::{ClientBuilder, Proxy};

Downloader::builder("https://example.com/file.zip", "output.zip")
    .client_builder(|| {
        ClientBuilder::new()
            .proxy(Proxy::http("http://proxy.example.com:8080").unwrap())
            // SOCKS5 代理: .proxy(Proxy::socks5("socks5://127.0.0.1:1080").unwrap())
    })
    .download()
    .await
    .unwrap();
```
程序也会自动识别 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 等环境变量。

---

## 功能相关问题

### Q: 多源下载怎么配置？
A: 需要启用 `multi-source` feature（默认已开启），使用 `MultiSourceConfig` 配置多个下载源：
```rust
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig};

let config = MultiSourceConfig::builder(
    vec![
        SourceConfig::new("https://mirror1.example.com/file.zip").weight(2),
        SourceConfig::new("https://mirror2.example.com/file.zip").weight(1),
        SourceConfig::new("https://mirror3.example.com/file.zip").weight(1),
    ],
    "output.zip",
)
.workers(32)
.build();

Downloader::new_multi(config, Default::default)
    .download()
    .await
    .unwrap();
```
权重越高的源会被分配更多的下载任务。

### Q: 如何自定义 HTTP 请求头？
A: 在 client_builder 中配置默认 headers：
```rust
use reqwest::header::{HeaderMap, USER_AGENT};

let mut headers = HeaderMap::new();
headers.insert(USER_AGENT, "MyDownloader/1.0".parse().unwrap());
headers.insert("Authorization", "Bearer token123".parse().unwrap());

Downloader::builder("https://example.com/file.zip", "output.zip")
    .client_builder(move || {
        ClientBuilder::new()
            .default_headers(headers.clone())
    })
    .download()
    .await
    .unwrap();
```

### Q: 如何设置请求超时时间？
A: 在 client_builder 中配置超时：
```rust
use std::time::Duration;

Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .client_builder(|| {
        ClientBuilder::new()
            .connect_timeout(Duration::from_secs(10)) // 连接超时 10 秒
            .timeout(Duration::from_secs(120)) // 整个请求超时 120 秒
    })
    .download()
    .await
    .unwrap();
```

### Q: 下载完成后会校验文件完整性吗？
A: 当前版本会校验下载的总字节数是否与服务器返回的 Content-Length 一致。如果需要哈希校验，可以在下载完成后自行计算文件的 MD5/SHA256 等哈希值与预期值比较。

### Q: 可以暂停和恢复下载吗？
A: 目前的断点续传功能支持程序退出后再次启动时恢复下载。运行时的暂停/恢复功能正在开发中，预计在 0.2.0 版本提供。

---

## 性能优化问题

### Q: 下载速度慢怎么办？
A: 可以尝试以下优化：
1. **增加并发线程数**：
   ```rust
   Downloader::builder(url, path)
       .workers(32) // 对于大文件可以增加到 16-64 线程
       .download()
       .await
   ```
   
2. **使用多源下载**：配置多个镜像源，充分利用带宽。

3. **调整客户端配置**：
   ```rust
   ClientBuilder::new()
       .tcp_keepalive(Duration::from_secs(60))
       .pool_max_idle_per_host(32) // 增加连接池大小
       .build()
   ```

4. **检查网络环境**：确保网络带宽足够，没有被限流，服务器没有限速。

5. **选择离自己近的镜像源**：延迟越低，下载速度越快。

### Q: 大文件下载时内存占用高怎么办？
A: simple_downloader 对内存使用已经做了优化，正常情况下内存占用应该是稳定的。如果还是很高：
1. 适当降低并发线程数，减少同时下载的块数
2. 检查是否开启了过多的其他功能
3. 确保使用的是最新版本，我们持续在优化内存使用

### Q: 小文件下载速度慢怎么办？
A: 对于小文件（<10MB），建议：
1. 降低并发线程数到 1-4
2. 禁用断点续传功能（`.resume(false)`）
3. 如果批量下载多个小文件，建议使用连接池复用连接

### Q: 进度更新太频繁导致 UI 卡顿怎么办？
A: 调整进度更新间隔：
```rust
Downloader::builder(url, path)
    .update_interval(1.0) // 每秒更新一次进度，默认是 0.5 秒
    .run(progress_callback)
    .await
```

---

## 错误处理问题

### Q: 提示 "无法从服务器响应头中获取文件大小" 怎么办？
A: 这个错误说明服务器没有返回 Content-Length 头，或者使用了分块传输编码。可以尝试：
1. 强制使用单线程下载：
   ```rust
   Downloader::builder(url, path)
       .workers(1)
       .download()
       .await
   ```
2. 检查下载链接是否正确，有些动态生成的文件确实没有固定大小
3. 如果知道文件大小，可以手动设置（需要修改源码，未来版本会提供 API）

### Q: 断点续传失败怎么办？
A: 常见原因和解决方案：
1. **目标文件不存在**：检查文件是否被删除或移动，可以删除对应的 `.resume` 文件重新下载
2. **元数据损坏**：删除对应的 `.resume` 文件，重新下载
3. **服务器文件已变更**：删除 `.resume` 文件和不完整的目标文件，重新下载

### Q: 提示 "没有可用的下载源"（多源下载时）怎么办？
A: 
1. 检查每个下载源的 URL 是否正确
2. 单独访问每个下载源，确认是否可以正常下载
3. 检查网络连接和代理配置
4. 增加更多可用的下载源
5. 调整 `max_retries_per_source` 配置，允许更多的重试次数

### Q: SSL 证书验证失败怎么办？
A: 优先建议修复证书问题，如果是测试环境或者信任的源，可以临时禁用证书验证（**不安全，生产环境不建议使用**）：
```rust
ClientBuilder::new()
    .danger_accept_invalid_certs(true) // 仅用于测试环境！
    .build()
```

---

## 开发相关问题

### Q: 如何在生产环境使用 simple_downloader？
A: 生产环境建议：
1. 使用最新的稳定版本
2. 配置合理的重试机制
3. 对错误进行全面处理
4. 避免使用不安全的配置（如禁用证书验证）
5. 监控下载进度和错误情况
6. 对于关键文件下载，下载完成后校验文件哈希值

### Q: 支持同步 API 吗？
A: simple_downloader 是完全基于异步 Tokio 运行时设计的，不提供同步 API。如果需要在同步代码中使用，可以使用 `tokio::runtime::Runtime` 来运行异步代码：
```rust
use tokio::runtime::Runtime;

fn sync_download(url: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::new()?;
    rt.block_on(async {
        Downloader::builder(url, path)
            .download()
            .await
    })?;
    Ok(())
}
```

### Q: 支持 WASM 吗？
A: 目前还不支持 WASM 环境，因为依赖的 reqwest 和 Tokio 在 WASM 环境下有较多限制。我们计划在未来的版本中考虑 WASM 支持。

### Q: 支持下载到内存中而不是文件吗？
A: 目前版本只支持下载到本地文件。下载到内存的功能在规划中，预计在 0.2.0 版本提供。如果现在需要，可以自定义文件写入逻辑（需要修改源码）。

### Q: 如何贡献代码？
A: 请参考 [CONTRIBUTING.md](../CONTRIBUTING.md) 文档，欢迎提交 PR 和 Issue！

### Q: 功能请求或 Bug 报告在哪里提交？
A: 请在 GitHub Issues 中提交，描述清楚问题和复现步骤。

---

## 其他问题

### Q: simple_downloader 和其他下载库有什么区别？
A: 
- **高性能异步架构**：基于 Tokio 异步运行时，充分利用多核性能
- **内置断点续传**：不需要额外配置即可使用
- **多源下载支持**：可以同时从多个源下载，提升速度和可靠性
- **智能调度**：动态调整并发数，自动重试失败的块
- **模块化设计**：通过 Feature flags 可以按需裁剪功能，减小二进制体积
- **简洁 API**：Builder 模式设计，上手简单，同时支持深度定制

### Q: 支持哪些操作系统？
A: 支持 Windows、macOS、Linux，所有 Rust 支持的平台基本都可以使用。

### Q: 商业项目可以免费使用吗？
A: 是的，simple_downloader 使用 Apache 2.0 许可证，可以免费用于商业项目，不需要付费。具体请参考 [LICENSE](../LICENSE) 文件。

### Q: 有计划提供其他语言的绑定吗？
A: 目前没有计划，但我们欢迎社区贡献其他语言的绑定（如 Python、Node.js、Go 等）。

如果你的问题没有在这里找到答案，可以在 GitHub Discussions 中提问，或者提交 Issue。
