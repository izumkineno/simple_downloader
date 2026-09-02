# 常见问题解答

> 以 `Cargo.toml:14-19` 与 `docs/usage.md` 为准；`default = []` 默认不启用任何可选 feature。

## 基础使用问题

### Q: 最简单的下载怎么实现？
A: 默认无需任何 feature，使用 builder 即可：
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
A: 启用 `progress` feature，使用 `run()`：
```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Downloader::builder("https://example.com/large_file.zip", "output.zip")
        .workers(16)
        .run(|total_size, mut info_rx| async move {
            println!("总大小: {} bytes", total_size);
            while let Ok(info) = info_rx.recv().await {
                // 仅 MonitorUpdate 携带可聚合进度（见 DownloadInfo 文档）
                println!(
                    "进度: {:.1}% | 速度: {:.2} MB/s | 已下载: {} bytes",
                    info.progress_percent(),
                    info.speed_mbps(),
                    info.downloaded_bytes()
                );
                if info.is_complete() { break; }
            }
        })
        .await?;
    Ok(())
}
```
完整 UI 见 `examples/with_custom_ui.rs`（`indicatif::MultiProgress`）与 `docs/usage.md:6`。

### Q: 断点续传功能如何使用？
A: 启用 `resume` feature（`default = []`，需显式开启），下载中断后再次运行相同 `url + output_path` 会自动从校验通过的 segment 恢复：
```toml
[dependencies]
simple_downloader = { version = "0.5", features = ["resume"] }
```
```rust
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .download() // resume 启用时默认 true，自动恢复
    .await
    .unwrap();

// 显式禁用
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .resume(false)
    .download()
    .await
    .unwrap();
```
元数据为 `output_path.download.bitcode`（`src/resume.rs:RESUME_EXTENSION`，`DEFAULT_SEGMENT_SIZE 64 KiB` 固定 ledger + 哈希校验），成功后自动删除。`ResumeTargetMissing`（sidecar 存在但文件缺失）会 fail-stop，详见 `docs/errors.md:6/7`。

### Q: 如何设置并发下载线程数？
A: `workers()`：
```rust
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .workers(16)
    .download()
    .await
    .unwrap();
```
**注意**：`src/downloader.rs:502` 实际并发 `if !support_ranges || file_size < 1 MiB {1} else {workers}`；`state.rs:is_splittable` 要求 `remaining >= 20 KiB`（`MIN_CHUNK_SIZE 10 KiB ×2`）；限速启用时冻结自适应分裂。

### Q: 如何配置代理？
A: 两种方式，二选一：

**单源**（任意 feature）通过 `client_builder`：
```rust
use reqwest::{ClientBuilder, Proxy};
Downloader::builder("https://example.com/file.zip", "output.zip")
    .client_builder(|| {
        ClientBuilder::new()
            .proxy(Proxy::http("http://proxy.example.com:8080").unwrap())
    })
    .download()
    .await
    .unwrap();
```

**多源多代理**（需 `proxy` feature，隐含 `multi-source`）以 lane 建模：
```rust
use simple_downloader::{MultiSourceConfig, SourceConfig, ProxyConfig, LaneModel, Downloader};
let src = SourceConfig::new("https://example.com/file.bin")
    .with_proxies(vec![
        ProxyConfig::http("http://proxy:8080").unwrap(),
        ProxyConfig::socks5("socks5://proxy:1080").unwrap(),
    ]);
let cfg = MultiSourceConfig::new("output.bin", 16, 0.5)
    .with_sources(vec![src])
    .with_lane_model(LaneModel::PerSourceProxy);
Downloader::new_multi(cfg, Default::default).download().await.unwrap();
```
程序亦自动识别 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`（`reqwest` 底层）。

---

## 功能相关问题

### Q: 多源下载怎么配置？
A: 启用 `multi-source`（`proxy` 隐含它），以 `MultiSourceConfig`/`SourceConfig`/`LaneModel` 为准（无 `weight/priority/headers` 伪接口）：
```rust
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig, LaneModel};

let config = MultiSourceConfig::new("output.zip", 32, 0.5)
    .with_sources(vec![
        SourceConfig::new("https://mirror1.example.com/file.zip").with_id("m1"),
        SourceConfig::new("https://mirror2.example.com/file.zip").with_id("m2"),
    ])
    .with_lane_model(LaneModel::PerSource)
    .with_max_chunks_per_lane(2)
    .with_max_chunks_per_source(Some(8));

Downloader::new_multi(config, Default::default).download().await.unwrap();
```
启动时 `get_file_info` 探测各源，跳过不可用/不支持 Range/大小不一致的源，全不可用 → `NoAvailableSources`；lane 连续失败 `≥3` 次进 `BLACKLIST 30s`。详见 `docs/usage.md:7` 与 `docs/architecture.md:6.2`。

### Q: 限速怎么用？
A: 启用 `rate-limit` feature（`governor 0.7`，`1 token=1 byte`，`burst` 默认 64 KiB）：
```rust
// 全局
Downloader::builder("https://example.com/file.bin", "output.bin")
    .speed_limit(1024*1024) // 1 MiB/s
    .with_burst(64*1024)
    .download().await?;

// 分源 + 全局硬上限
use simple_downloader::{MultiSourceConfig, SourceConfig};
let cfg = MultiSourceConfig::new("output.bin", 32, 0.5)
    .with_sources(vec![
        SourceConfig::new("https://m1.example.com/file.bin").with_speed_limit(300*1024),
        SourceConfig::new("https://m2.example.com/file.bin").with_speed_limit(300*1024),
    ])
    .with_global_speed_limit(512*1024);
```
校验：`0`/`>u32::MAX`/`burst` 无 `speed_limit` 均为 `InvalidArgument`；限速期自适应冻结，详见 `docs/configuration.md:限速` 与 `examples/with_rate_limit.rs`。

### Q: 如何自定义 HTTP 请求头？
A: `client_builder` 中配置 `default_headers`：
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
默认 UA 为 `simple_downloader/<version>`（`src/lib.rs:DEFAULT_USER_AGENT`，`util::ensure_user_agent` 注入）。

### Q: 如何设置请求超时时间？
A: `client_builder`：
```rust
use std::time::Duration;
Downloader::builder("https://example.com/large_file.zip", "output.zip")
    .client_builder(|| {
        ClientBuilder::new()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
    })
    .download()
    .await
    .unwrap();
```
建议大文件 `timeout 60-120s`，`tcp_keepalive 60s`，`pool_max_idle_per_host 32`（见 `docs/best-practices.md:性能`）。

### Q: 下载完成后会校验文件完整性吗？
A: 会校验 `total_downloaded >= total_size` 与 `Range 206/Content-Range` 一致性及 `Early-EOF` 门限（`chunk.rs:P0-1/P0-2`）；`resume` 场景按 `64 KiB` segment 哈希复用；如需业务级 `MD5/SHA256`，下载完成后自行计算比对。

### Q: 可以暂停和恢复下载吗？
A: `resume` feature 支持进程级中断后恢复（`*.download.bitcode` ledger，`tests/process_resume.rs` 已覆盖控制台中断与 kill 恢复）；运行时暂停/恢复的任务队列 API 仍在 TODO（见 `README:任务队列 API`）。

---

## 性能优化问题

### Q: 下载速度慢怎么办？
A: 排查清单：
1. **并发**：`workers 16-32`（大文件），小文件 `4-8`，`<1 MiB` 自动单线程。
2. **多源**：配置多镜像充分利用带宽（`MultiSourceConfig`）。
3. **限速**：确认未误启用 `rate-limit` 的全局/分源限速。
4. **客户端**：`tcp_keepalive 60s`、`pool_max_idle_per_host 32`。
5. **网络/源**：选近源，检查源侧限流与 `RetryHandler` 熔断（`MAX_TOTAL_ATTEMPTS 30`）。

### Q: 大文件下载时内存占用高怎么办？
A: 本库已通过有界 `mpsc 128` 背压与 `ChunkProgress 64KiB/50ms` 节流控制内存（`CHANNEL_CAPACITY 4096`）；若仍高：降 `workers`、检查是否同时启动大量 `Downloader` 实例、升级到 `0.5.4` 流式追加（无 `set_len` 预分配）。

### Q: 小文件下载速度慢怎么办？
A: 小文件（`<10MB`）建议：`workers 1-4`、`resume(false)`、复用连接、批量下载时复用 `ClientBuilder`（见 `docs/best-practices.md:小文件`），`<1 MiB` 已自动单线程。

### Q: 进度更新太频繁导致 UI 卡顿怎么办？
A: `update_interval(secs)`：
```rust
Downloader::builder(url, path)
    .update_interval(1.0) // 默认 0.5s，桌面 0.5-1s，服务端 1-5s
    .run(progress_callback)
    .await
```
回调内避免阻塞，必要时 `mpsc` 转发。

---

## 错误处理问题

### Q: 提示 "无法从服务器响应头中获取文件大小" 怎么办？
A: `0.3.1+` 已自动回退为**单流流式下载**（`Transfer-Encoding: chunked`，`util::get_file_info` HEAD→`Range 0-0`→`Content-Length`→流式），无需手动 `workers(1)`；若仍 `MissingContentLength`，说明三阶段探测与流式 `GET` 均失败，需检查链接/网络；详见 `docs/errors.md:4`。

### Q: 断点续传失败怎么办？
A:
1. `ResumeTargetMissing`：sidecar 存在但目标文件缺失 → 恢复文件或删 `*.download.bitcode` 或 `resume(false)`。
2. `ResumeMetadata`：损坏/版本不一致 → 删 sidecar 重下（`validate_shape` 会自愈，但显式删除更可控）。
3. 服务器文件已变更（大小/内容变）→ 删 sidecar 与未完成文件重下。

### Q: 提示 "没有可用的下载源"（多源下载时）怎么办？
A: 检查：URL 是否正确、是否单独可访问、是否支持 `Range`、大小是否一致、是否需代理；全部探测失败即 `NoAvailableSources`，可增源后重试。

### Q: `InvalidArgument` 限速参数错误？
A: `speed_limit 0` / `burst 0` / `burst` 无 `speed_limit` / `>u32::MAX` 均 `InvalidArgument`（`src/downloader.rs:run_internal` 即时校验，无残留文件）。

### Q: SSL 证书验证失败怎么办？
A: 优先修复证书；测试环境临时禁用（**生产不建议**）：
```rust
ClientBuilder::new()
    .danger_accept_invalid_certs(true)
    .build()
```

---

## 开发相关问题

### Q: 如何在生产环境使用 simple_downloader？
A: 建议：固定 `version = "0.5"`、按需 feature、`cargo tree` 检查 `governor/tracing`、区分可重试错误（`Request::is_timeout/is_connect/5xx`、`Join`）与不可重试（`Io/ResumeTargetMissing/InvalidArgument/PermanentFailure`）、落地指数退避与 `trace::init_tracing()` 日志、关键文件下载后哈希校验。

### Q: 支持同步 API 吗？
A: 基于 `Tokio` 异步，不提供同步 API；可在同步代码中 `Runtime::block_on`：
```rust
use tokio::runtime::Runtime;
fn sync_download(url: &str, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::new()?;
    rt.block_on(async { Downloader::builder(url, path).download().await })?;
    Ok(())
}
```

### Q: 支持 WASM 吗？
A: 暂不支持（`reqwest` + `Tokio` 多线程 runtime 限制）。

### Q: 支持下载到内存中而不是文件吗？
A: 暂仅支持文件落地（`file_writer_task` 有界 `mpsc` + `seek/write`）；内存目标可通过替换 `file_writer` 存储后端扩展（保持背压协议，见 `architecture.md:8`）。

### Q: 如何贡献代码？
A: 见 `CONTRIBUTING.md`；提交前 `cargo fmt --check`、`cargo check --all-features`、`cargo test --all-features`、`cargo clippy --all-features -D warnings`。

### Q: 功能请求或 Bug 报告在哪里提交？
A: GitHub Issues（附 `RUST_LOG=simple_downloader=debug` 日志与 `src/trace.rs` 相关 `span`）。

---

## 其他问题

### Q: simple_downloader 和其他下载库有什么区别？
A: 亮点：`Tokio` 异步消息驱动、动态 `Probing→Stable` 带宽探测分裂、两级 `Retry 2s/10s` + `30` 熔断、`broadcast 4096` + `64KiB/50ms` 节流、`mpsc 128` 背压、`governor` 双桶限速、`bitcode` 哈希断点续传、`LaneScheduler` 多源/代理 lane。

### Q: 支持哪些操作系统？
A: Windows/macOS/Linux（Rust `1.85+`、`edition 2024`）。

### Q: 商业项目可以免费使用吗？
A: `Apache-2.0`，可免费商用。

### Q: 有计划提供其他语言的绑定吗？
A: 暂无计划，欢迎社区贡献。

如果你的问题没有在这里找到答案，可在 GitHub Discussions 提问或提交 Issue。
