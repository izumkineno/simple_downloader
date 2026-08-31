# 调用指南（Usage）

本文是 `simple_downloader` 的**调用侧权威文档**，覆盖所有公开 API 的最小可运行调用形态、Feature 选型、错误处理与完整示例索引。设计细节见 [`architecture.md`](./architecture.md)，配置项详解见 [`configuration.md`](./configuration.md)，错误全表见 [`errors.md`](./errors.md)。

## 目录

- [1. 安装与 Feature 选型](#1-安装与-feature-选型)
- [2. 快速开始](#2-快速开始)
- [3. Builder 全景](#3-builder-全景)
- [4. 场景一：基础单源下载](#4-场景一基础单源下载)
- [5. 场景二：断点续传](#5-场景二断点续传resume)
- [6. 场景三：进度监控](#6-场景三进度监控progress)
- [7. 场景四：多源下载](#7-场景四多源下载multi-source)
- [8. 场景五：代理](#8-场景五代理proxy)
- [9. 场景六：自定义 HTTP 客户端](#9-场景六自定义-http-客户端)
- [10. 错误处理模板](#10-错误处理模板)
- [11. 并发与自动降级](#11-并发与自动降级)
- [12. 完整示例索引](#12-完整示例索引)
- [13. 调用检查清单](#13-调用检查清单)

---

| Feature | 默认 | 作用 | 额外依赖 |
|---|---|---|---|
| `resume` | ❌ | 断点续传、sidecar 元数据 `*.download.bitcode` | `bitcode@0.6` |
| `progress` | ❌ | 进度事件 `DownloadInfo` 与 `run(handler)` | — |
| `multi-source` | ❌ | 多源下载建模 `MultiSourceConfig/SourceConfig/LaneModel` 与 `new_multi` | — |
| `proxy` | ❌ | 代理能力，隐含 `multi-source` | — |
| `rate-limit` | ❌ | 全局/分源限速 `governor` 令牌桶 `1 token=1 byte` `speed_limit/with_burst` | `governor@0.7` |
| `queue` | ❌ | 任务队列 FIFO/并发/pause/resume/cancel/重命名 `TaskQueue` | `uuid@1` |

```toml
# 仅基础能力（最轻量，1.5 MiB rlib）
[dependencies]
simple_downloader = { version = "0.6", default-features = false }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }

# 常用组合：基础 + 断点续传 + 进度
simple_downloader = { version = "0.6", default-features = false, features = ["resume", "progress"] }

# 队列调度
simple_downloader = { version = "0.6", default-features = false, features = ["queue"] }

# 全功能（含限速与队列）
simple_downloader = { version = "0.6", default-features = false, features = ["resume", "progress", "multi-source", "proxy", "rate-limit", "queue"] }
```

> 历史文档中 `default = [resume, progress, ...]` 已过时，以 `Cargo.toml` 为准。

---

## 2. 快速开始

```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Downloader::builder("https://proof.ovh.net/files/100Mio.dat", "100Mio.dat")
        .workers(16)          // 并发数，见 §11 自动降级规则
        .download()           // 不需要 progress feature
        .await?;
    println!("下载完成");
    Ok(())
}
```

对应示例：`examples/download.rs`。

---

## 3. Builder 全景

唯一推荐入口为 `Downloader::builder(url, output_path)`，返回 `DownloadBuilder`。低层 `Downloader::new()` / `Downloader::new_multi()` 仅在需要完全控制 `ClientBuilder` 闭包类型时使用。

| 方法 | Feature | 说明 | 示例 |
|---|---|---|---|
| `workers(n)` | — | 并发上限，`max(1, n)`，实际生效受 §11 约束 | `.workers(16)` |
| `update_interval(secs)` | — | `MonitorUpdate` 广播间隔，`>0` 生效，默认 `0.5` | `.update_interval(1.0)` |
| `client_builder(\|\| ClientBuilder::new()...)` | — | 注入 `reqwest::ClientBuilder` 闭包，见 §9 | `.client_builder(\|\| ClientBuilder::new().timeout(...))` |
| `resume(bool)` | `resume` | 显式开关断点续传，默认 `true`（当 feature 启用时） | `.resume(false)` |
| `build() -> Downloader` | — | 产出下载器，复用或进一步 `with_resume()` | `let dl = builder.build();` |
| `download().await` | — | 消费 `self` 直接下载 | `builder.download().await?;` |
| `run(handler).await` | `progress` | 消费 `self` 并注入进度回调 | `builder.run(\|total, rx\| async move {...}).await?;` |

`Downloader` 侧同名方法：`with_resume(bool)`（`resume`）、`download().await`、`run(handler).await`（`progress`）、`new_multi(config, client_builder)`（`multi-source`）。

---

## 4. 场景一：基础单源下载

不启用任何 feature 即可用，自动处理 `HEAD -> Range: bytes=0-0 -> Content-Length` 回退（`src/util.rs:get_file_info`）。

```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 方式 A：Builder 便捷调用（推荐）
    Downloader::builder("https://example.com/file.bin", "output.bin")
        .workers(8)
        .update_interval(0.5)
        .download()
        .await?;

    // 方式 B：先 build 再 download（需复用或条件分支时）
    let dl = Downloader::builder("https://example.com/file.bin", "output.bin")
        .workers(8)
        .build();
    dl.download().await?;
    Ok(())
}
```

降级行为：服务器不支持 `Range` 或文件 `<1 MiB` 时自动单线程（见 §11）。

---

## 5. 场景二：断点续传（`resume`）

启用 `resume` 后，`Downloader` 自动在 `output_path` 同级维护 `output_path.download.bitcode` sidecar（`src/resume.rs:RESUME_EXTENSION`），基于固定 `64 KiB` segment 哈希校验恢复，非简单追文件长度。

```toml
[dependencies]
simple_downloader = { version = "0.3", features = ["resume"] }
```

```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 默认启用：文件 + sidecar 同时存在时自动恢复
    Downloader::builder("https://example.com/large.bin", "large.bin")
        .workers(16)
        .download()
        .await?;

    // 显式禁用：强制全新下载（忽略已有 sidecar）
    Downloader::builder("https://example.com/large.bin", "large.bin")
        .resume(false)
        .download()
        .await?;

    // 或在 Downloader 层控制
    let dl = Downloader::builder("https://example.com/large.bin", "large.bin")
        .build()
        .with_resume(false);
    dl.download().await?;
    Ok(())
}
```

恢复语义（`src/resume.rs` / `src/downloader.rs:427-454`）：

- 仅复用哈希校验通过的 segment，其余重建为待下载区间。
- `sidecar` 存在但目标文件缺失 → `Err(ResumeTargetMissing)` fail-stop，不静默重下。
- 已验证 segment 被外部篡改 → 该 segment 失效重下。
- 单源/多源统一走 `ResumePlan`。

工具 API（`resume` feature 导出）：

```rust
use simple_downloader::{metadata_path_for, hash_bytes, ResumeMetadata, DEFAULT_SEGMENT_SIZE};

let meta_path = metadata_path_for("large.bin"); // -> "large.bin.download.bitcode"
let h = hash_bytes(&bytes);
let meta = ResumeMetadata::new(file_size, DEFAULT_SEGMENT_SIZE);
```

示例与测试：`examples/resume_harness.rs`、`tests/resume.rs`、`tests/process_resume.rs`。

---

## 6. 场景三：进度监控（`progress`）

```toml
[dependencies]
simple_downloader = { version = "0.3", features = ["progress"] }
```

`run(handler)` 将 `total_size: u64` 与 `broadcast::Receiver<DownloadInfo>` 交给调用方，调用方在独立 task 中消费事件。`DownloadInfo` 变体见 `src/types.rs:99-187`。

```rust
use simple_downloader::{Downloader, DownloadInfo};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Downloader::builder("https://proof.ovh.net/files/100Mio.dat", "100Mio.dat")
        .workers(16)
        .run(|total_size, mut info_rx| async move {
            println!("总大小: {} bytes", total_size);
            while let Ok(info) = info_rx.recv().await {
                match info {
                    DownloadInfo::MonitorUpdate { total_downloaded, total_speed, chunk_details, .. } => {
                        // 常用快捷方法
                        println!(
                            "进度 {:.1}% 速度 {:.2} MB/s {}/{}",
                            info.progress_percent(),
                            info.speed_mbps(),
                            total_downloaded, total_size
                        );
                        if info.is_complete() { break; }
                        // chunk_details: Vec<(ChunkId, size, downloaded, speed, status)>
                        // status: 0=下载中 1=重试中 2=等待重试 3=延迟重试 4=已完成 5=失败
                    }
                    DownloadInfo::ChunkProgress { id, downloaded, .. } => {}
                    DownloadInfo::ChunkFailed { id, error, .. } => eprintln!("块 {} 失败: {}", id, error),
                    DownloadInfo::ChunkBisected { original_id, new_start, new_end } => {}
                    DownloadInfo::ChunkStatusChanged { id, status, message } => {}
                    DownloadInfo::DownloadComplete(id) => {}
                }
            }
        })
        .await?;
    Ok(())
}
```

完整 UI 示例：`examples/with_custom_ui.rs`（`indicatif::MultiProgress` 多进度条）、`examples/test_server_smart_schedule.rs`。

`DownloadInfo` 快捷方法：

| 方法 | 适用变体 | 说明 |
|---|---|---|
| `progress_percent() -> f64` | `MonitorUpdate` | `0.0~100.0` |
| `speed_mbps() -> f64` | `MonitorUpdate` | `total_speed / 1MiB` |
| `downloaded_bytes() -> u64` | `MonitorUpdate` | `total_downloaded` |
| `total_bytes() -> u64` | `MonitorUpdate` | `total_size` |
| `is_complete() -> bool` | `MonitorUpdate` | `total_downloaded >= total_size` |

> 回调内避免阻塞/耗时 IO，必要时通过 `mpsc` 转发到其他任务（见 `docs/best-practices.md:132-151`）。

---

## 7. 场景四：多源下载（`multi-source`）

```toml
[dependencies]
simple_downloader = { version = "0.3", features = ["multi-source", "progress"] }
```

```rust
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig, LaneModel};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 构造多源配置
    let config = MultiSourceConfig::new("output.bin", 32, 0.5)
        .with_sources(vec![
            SourceConfig::new("https://mirror1.example.com/file.bin").with_id("mirror1"),
            SourceConfig::new("https://mirror2.example.com/file.bin").with_id("mirror2"),
            SourceConfig::new("https://mirror3.example.com/file.bin"),
        ])
        // 可选：lane 建模
        .with_lane_model(LaneModel::PerSource) // 或 PerSourceProxy（配合 proxy）
        .with_max_chunks_per_lane(1)
        .with_max_chunks_per_source(Some(8));

    // 方式 A：直接多源下载
    Downloader::new_multi(config, Default::default).download().await?;

    // 方式 B：带进度
    let config = MultiSourceConfig::new("output.bin", 32, 0.5)
        .with_sources(vec![
            SourceConfig::new("https://mirror1.example.com/file.bin"),
            SourceConfig::new("https://mirror2.example.com/file.bin"),
        ]);
    Downloader::new_multi(config, Default::default)
        .run(|total, mut rx| async move {
            while let Ok(info) = rx.recv().await {
                println!("{:.1}%", info.progress_percent());
            }
        })
        .await?;
    Ok(())
}
```

行为（`src/lane.rs` / `src/downloader.rs:408-424`）：

- 启动时并发探测各源 `get_file_info`，跳过不可用/不支持 `Range`/文件大小不一致的源，全部不可用 → `Err(NoAvailableSources)`。
- `LaneModel::PerSource` 按源维度调度，`PerSourceProxy` 按源×代理组合调度。
- lane 连续失败 `>=3` 次（`BLACKLIST_THRESHOLD`）进入黑名单并切换。
- 断点续传与多源正交，`resume` 启用时同样基于 segment 哈希恢复，与 `new_multi` 共享 `ResumePlan` 路径。

`SourceConfig` API：`new(url)`、`with_id(id)`、`with_proxies(vec)`（需 `proxy` feature）。

`MultiSourceConfig` API：`new(output, workers, update_interval)`、`with_sources(vec)`、`with_lane_model()`、`with_max_chunks_per_lane(n)`、`with_max_chunks_per_source(Option<usize>)`。

测试与示例：`tests/multi_source.rs`、`examples/manual_multi_source_test_server.rs`（500 MiB 真实多源观察）。

---

## 8. 场景五：代理（`proxy`）

`proxy` 隐含 `multi-source`，代理以 lane 维度建模，支持 `http`/`https`/`socks5`。

```toml
[dependencies]
simple_downloader = { version = "0.3", features = ["proxy", "progress"] }
```

```rust
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig, ProxyConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proxies = vec![
        ProxyConfig::http("http://proxy1.example.com:8080")?,
        ProxyConfig::socks5("socks5://proxy2.example.com:1080")?,
    ];

    let source = SourceConfig::new("https://example.com/file.bin")
        .with_proxies(proxies);

    let config = MultiSourceConfig::new("output.bin", 16, 0.5)
        .with_sources(vec![source])
        .with_lane_model(simple_downloader::LaneModel::PerSourceProxy);

    Downloader::new_multi(config, Default::default).download().await?;
    Ok(())
}
```

`ProxyConfig` 构造（`src/lane.rs:24-43`）：`ProxyConfig::http(url)`、`ProxyConfig::https(url)`、`ProxyConfig::socks5(url)`，亦可通过 `client_builder` 直接注入 `reqwest::Proxy`（见 §9）。

环境变量自动识别：`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`（由 `reqwest` 底层处理）。

---

## 9. 场景六：自定义 HTTP 客户端

所有场景均可通过 `client_builder` 注入 `reqwest::ClientBuilder`，用于超时、TLS、Header、连接池等。

```rust
use simple_downloader::Downloader;
use reqwest::ClientBuilder;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Downloader::builder("https://example.com/file.bin", "output.bin")
        .workers(16)
        .client_builder(|| {
            ClientBuilder::new()
                .timeout(Duration::from_secs(120))
                .connect_timeout(Duration::from_secs(10))
                .tcp_keepalive(Duration::from_secs(60))
                .pool_max_idle_per_host(32)
                // 自定义 Header
                .default_headers({
                    let mut h = reqwest::header::HeaderMap::new();
                    h.insert("User-Agent", "simple_downloader/0.3".parse().unwrap());
                    h
                })
                // 危险：仅测试环境
                // .danger_accept_invalid_certs(true)
        })
        .download()
        .await?;
    Ok(())
}
```

代理亦可在此层注入（单源场景）：

```rust
use reqwest::{ClientBuilder, Proxy};
Downloader::builder("https://example.com/file.bin", "output.bin")
    .client_builder(|| {
        ClientBuilder::new()
            .proxy(Proxy::http("http://proxy.example.com:8080").unwrap())
    })
    .download()
    .await?;
```

> `client_builder` 为 `Fn() -> ClientBuilder`，每次探测/重试会重新 `build()`，确保多源场景下每个 lane 独立 `Client`。

---

## 10. 错误处理模板

完整错误变体与重试建议见 [`errors.md`](./errors.md)。`src/types.rs:14-82` 定义：

| 变体 | 场景 | 可重试 |
|---|---|---|
| `Request(reqwest::Error)` | 网络/DNS/超时/4xx/5xx/TLS | 部分（超时/5xx） |
| `Io(io::Error)` | 磁盘满/权限/占用 | 否 |
| `Join(JoinError)` | task panic/取消 | 是 |
| `MissingContentLength` | 无 `Content-Length` | 否（需降级单线程） |
| `NoAvailableSources` | 多源全不可用 | 是（增源后） |
| `ResumeTargetMissing(PathBuf)` | sidecar 存在但文件缺失 | 否 |
| `ResumeMetadata(String)` | 元数据损坏/版本不兼容 | 否 |

```rust
use simple_downloader::{Downloader, DownloadError};

async fn download_with_retry(url: &str, path: &str) -> Result<(), DownloadError> {
    let mut attempts = 0;
    loop {
        match Downloader::builder(url, path).download().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts += 1;
                if attempts >= 3 { return Err(e); }
                match &e {
                    DownloadError::Request(err) if err.is_timeout() || err.is_connect() => {
                        tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempts))).await;
                        continue;
                    }
                    DownloadError::Request(err) if err.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    DownloadError::Join(_) => continue,
                    _ => return Err(e),
                }
            }
        }
    }
}
```

---

## 11. 并发与自动降级

`src/downloader.rs:502-510` 实际生效并发：

```text
if !support_ranges || workers == 1 || file_size < 1 MiB { 1 } else { workers }
```

- 服务器不支持 `Range` → 强制单线程，忽略 `workers`。
- 文件 `< 1 MiB` → 单线程，避免分片开销。
- 运行时 `DownloadState`/`ConcurrencyManager` 在 `Probing -> Stable` 两阶段动态探测带宽，自动分裂最慢可分片块（`state.rs:is_splittable` 要求 `remaining >= 20 KiB`），无需调用方干预。详见 `architecture.md:动态分片` 与 `tests/concurrency.rs`。

建议：小文件 `<100 MiB` 用 `4-8` workers，大文件 `>1 GiB` 用 `16-32`，多源可 `32-64` 但不超过 `源数×4`。

---

## 12. 完整示例索引

| 示例 | 路径 | Feature | 说明 |
|---|---|---|---|
| 基础下载 | `examples/download.rs` | — | 单源最小调用 |
| 自定义 UI | `examples/with_custom_ui.rs` | `progress` | `indicatif::MultiProgress` 总进度+分块进度条 |
| 断点续传子进程 | `examples/resume_harness.rs` | `resume,multi-source` | `single`/`multi` 双模式，供 `process_resume` 集成测试 |
| 智能调度观察 | `examples/test_server_smart_schedule.rs` | `progress` | 本地 `test_server` 限速观察并发决策 |
| 手工多源 500MiB | `examples/manual_multi_source_test_server.rs` | `multi-source,progress` | 双源 `16m`/`2m` 限速、实时 stats 与字节校验 |

运行：

```bash
cargo run --example download
cargo run --features progress --example with_custom_ui
cargo run --features multi-source,progress --example manual_multi_source_test_server
cargo run --features resume,multi-source --example resume_harness -- single https://example.com/file.bin out.bin 8 0.5
```

测试入口：

```bash
cargo test --features resume,multi-source --test resume -- --nocapture --test-threads=1
cargo test --features resume,multi-source --test process_resume -- --nocapture --test-threads=1
cargo test --test multi_source -- --nocapture
```

---

## 13. 调用检查清单

- [ ] `Cargo.toml` feature 按需开启，未全量 `default-features = false` 是否满足需求？
- [ ] `workers` / `update_interval` 是否符合文件大小与 UI 刷新需求？
- [ ] 需要断点续传时 `resume` 是否启用，sidecar 清理策略是否明确？
- [ ] 需要进度时 `progress` + `run(handler)` 回调内是否无阻塞操作？
- [ ] 多源场景源是否同文件同大小且支持 `Range`，`LaneModel` 是否匹配代理维度？
- [ ] 自定义 `ClientBuilder` 超时/连接池是否按文件大小配置？
- [ ] 错误分支是否区分可重试/不可重试（`Request::is_timeout` / `ResumeTargetMissing` 等）？
