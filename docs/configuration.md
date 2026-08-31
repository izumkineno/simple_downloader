# 配置参考

本文档详细介绍 simple_downloader 的所有可配置选项和参数。以 `Cargo.toml:14-19` 与 `docs/usage.md` 为准。

## 核心配置选项

### 1. 基本配置（`Downloader::builder`）

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `url` | `impl Into<FastStr>` | 必填 | 要下载的文件 URL（`builder(url, output)` 第1参） |
| `output_path` | `impl Into<FastStr>` | 必填 | 保存路径（第2参） |
| `workers` | `u64` | CPU 核心数（`available_parallelism`） | 并发上限 `max(1,n)`，实际受 §11 自动降级约束：不支持 Range 或 `<1 MiB` 时降为 1 |
| `update_interval` | `f64` | `0.5` | `MonitorUpdate` 广播间隔（秒），`>0` 才生效 |

```rust
use simple_downloader::Downloader;
let dl = Downloader::builder("https://example.com/file.bin", "output.bin")
    .workers(16)
    .update_interval(0.5)
    .build();
```

### 2. 断点续传配置（`resume` feature）

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `resume(bool)` | `DownloadBuilder::resume` / `Downloader::with_resume` | `true`（feature 启用时） | 是否启用断点续传。关闭则忽略并删除同级 `*.download.bitcode` |
| `DEFAULT_SEGMENT_SIZE` | `u64` | `64 KiB` | `src/resume.rs` 固定 segment 大小，哈希校验粒度（非 chunk 大小） |

```rust
use simple_downloader::Downloader;
Downloader::builder("https://example.com/large.bin", "large.bin")
    .resume(false) // 强制全新下载
    .download().await?;
```

sidecar 路径：`metadata_path_for("a.bin") -> "a.bin.download.bitcode"`，`verify` 仅复用哈希通过的 segment。

### 3. HTTP 客户端配置

通过 `client_builder` 注入 `reqwest::ClientBuilder` 闭包，可定制超时、证书、代理等（见 `src/lib.rs:128-143` 示例）：

```rust
use reqwest::ClientBuilder;
use std::time::Duration;

let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
    .client_builder(|| {
        ClientBuilder::new()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(60))
    })
    .build();
```

常用项：`timeout` / `connect_timeout` / `tcp_keepalive` / `pool_max_idle_per_host` / `gzip/brotli/deflate` / `https_only` / `danger_accept_invalid_certs` / `proxy`。本库默认 `reqwest` 走 `rustls`（见 `cargo tree | grep rustls`），无需系统 `openssl`。

## Feature Flags 配置

以 `Cargo.toml:14-20` 为准，默认**不启用**任何可选 feature（`default = []`）：

| Feature | 默认 | 隐含依赖 | 说明 |
|---------|------|----------|------|
| `resume` | ❌ | `bitcode@0.6` | 断点续传、`*.download.bitcode`、`resume(bool)`/`with_resume` |
| `progress` | ❌ | — | `DownloadInfo`（`0.5.5+ #[non_exhaustive]` 稳定契约）与 `run(handler)` |
| `multi-source` | ❌ | — | `MultiSourceConfig`/`SourceConfig`/`LaneModel`、`Downloader::new_multi` |
| `proxy` | ❌ | `multi-source` | `ProxyConfig`、`SourceConfig::with_proxies`，支持 `http/https/socks5` |
| `rate-limit` | ❌ | `governor@0.7` | 全局/分源限速 `governor` 令牌桶 `1 token=1 byte` |
| `default` | ❌ | — | 仅基础单源多线程下载 `Downloader::builder().download()` |

```toml
# 最轻量
simple_downloader = { version = "0.5", default-features = false }
# 常用：基础 + 断点续传 + 进度
simple_downloader = { version = "0.5", default-features = false, features = ["resume","progress"] }
# 全功能（含限速）
simple_downloader = { version = "0.5", default-features = false, features = ["resume","progress","multi-source","proxy","rate-limit"] }
```
> 历史文档中 `default = [resume, progress, ...]` 与 `full` 已过时。

## 多源下载配置（`multi-source` feature）

### 构造示例（以 `docs/usage.md` §7 为准）

```rust
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig, LaneModel};

let cfg = MultiSourceConfig::new("output.bin", 32, 0.5)
    .with_sources(vec![
        SourceConfig::new("https://mirror1.example.com/file.bin").with_id("m1"),
        SourceConfig::new("https://mirror2.example.com/file.bin"), // id 自动为 url
    ])
    .with_lane_model(LaneModel::PerSource) // 或 PerSourceProxy（需 proxy feature）
    .with_max_chunks_per_lane(2)
    .with_max_chunks_per_source(Some(8));

Downloader::new_multi(cfg, Default::default).download().await?;
```

### SourceConfig

| 方法 | 说明 |
|------|------|
| `SourceConfig::new(url)` | 新建源，`id` 默认即 `url` |
| `.with_id("m1")` | 显式 lane/source 标识 |
| `.with_proxies(vec![ProxyConfig::http("http://proxy:8080").unwrap()])` | 需 `proxy` feature |

### MultiSourceConfig

| 方法 | 说明 |
|------|------|
| `MultiSourceConfig::new(output, workers, update_interval)` | `workers = max(1,n)`，`update_interval>0` |
| `.with_sources(vec![...])` | 设置镜像列表 |
| `.with_lane_model(LaneModel::PerSource\|PerSourceProxy)` | `PerSource` 同源共享 lane，`PerSourceProxy` 按 源×代理 独立 lane |
| `.with_max_chunks_per_lane(n)` | 单 lane 并发上限 |
| `.with_max_chunks_per_source(Some(n))` | 单源并发上限（仅 PerSource 生效） |

调度：启动时 `get_file_info` 探测各源，跳过不可用/不支持 Range/大小不一致的源，全不可用则 `Err(NoAvailableSources)`；`BLACKLIST_THRESHOLD=3` 连续失败进黑名单。

## 代理配置（`proxy` feature，隐含 `multi-source`）

`SourceConfig::with_proxies` 以 lane 维度建模，`ClientBuilder` 侧亦可直接 `proxy(Proxy::all(..))` 注入：

```rust
use simple_downloader::{ProxyConfig, SourceConfig, MultiSourceConfig, LaneModel, Downloader};

let src = SourceConfig::new("https://example.com/file.bin")
    .with_proxies(vec![
        ProxyConfig::http("http://proxy.example.com:8080").unwrap(),
        ProxyConfig::socks5("socks5://proxy.example.com:1080").unwrap(),
    ]);

let cfg = MultiSourceConfig::new("output.bin", 16, 0.5)
    .with_sources(vec![src])
    .with_lane_model(LaneModel::PerSourceProxy);

Downloader::new_multi(cfg, Default::default).download().await?;
```

短链：`ProxyConfig::http/https/socks5(url) -> Result<Self>`，`with_id` 可显式命名。

## 环境变量配置

底层 `reqwest` 会自动识别（无需本库额外代码）：

| 环境变量 | 说明 |
|----------|------|
| `HTTP_PROXY` / `http_proxy` | HTTP 代理 |
| `HTTPS_PROXY` / `https_proxy` | HTTPS 代理 |
| `ALL_PROXY` / `all_proxy` | 默认代理 |
| `NO_PROXY` / `no_proxy` | 直连域名列表（逗号分隔） |

## 性能优化建议

### 1. 并发线程数

- 小文件（<100 MiB）：4–8
- 大文件（>1 GiB）：16–32
- 多源：可至 32，但受 `max_chunks_per_lane/source` 与服务器限流约束

### 2. 进度间隔

- 桌面 UI：0.5–1s；服务端：1–5s 降 CPU

### 3. 断点续传

- 大文件强烈建议启用；临时小文件可 `resume(false)` 减 I/O
- 本库下载成功后已自动删除 `*.download.bitcode`，无需手动清理；失败/中断残留则下次自动复用已校验 segment

### 4. 客户端优化

```rust
use reqwest::ClientBuilder;
use std::time::Duration;
Downloader::builder("https://example.com/large.bin", "output.bin")
    .workers(32)
    .client_builder(|| {
        ClientBuilder::new()
            .timeout(Duration::from_secs(120))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_max_idle_per_host(32)
    })
    .build();
```

## 运行时热更新（0.5.5 配置灵活性）

`src/config.rs::RuntimeConfig` + `DownloadMonitor::apply_config` 支持下载进行中调整：

```rust
use simple_downloader::{config::{RuntimeConfig, new_shared, apply_config}, DownloadMonitor};
let shared = new_shared(RuntimeConfig::default().with_workers(8).with_update_interval(0.5));
// ... monitor 创建后 ...
apply_config(&shared, RuntimeConfig::default().with_workers(16));
// monitor.apply_config(&shared.read());
```

可热更字段：`workers`（`ConcurrencyManager::set_max_workers`）、`update_interval`、`speed_limit/burst`（`limiter::RateLimiter::apply` 待接）。为后续 `queue pause/resume` 与 `智能评分 burst` 打前站。

> `DownloadInfo` 自 `0.5.5` 标记 `#[non_exhaustive]`，新增变体/字段为兼容变更，`match` 需 `_` 分支。

## 配置最佳实践
3. 多源仅对同文件多镜像生效，需保证各源 `Content-Length` 一致
4. 代理优先走 `NO_PROXY` 环境变量，避免硬编码密码
