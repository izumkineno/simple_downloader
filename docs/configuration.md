# 配置参考

本文档详细介绍 simple_downloader 的所有可配置选项和参数。

## 核心配置选项

### 1. 基本配置

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `url` | `String` | 必填 | 要下载的文件的 URL |
| `output_path` | `String` | 必填 | 下载后文件的保存路径 |
| `workers` | `u64` | CPU 核心数 | 并发下载的工作线程数，最小值为 1 |
| `update_interval` | `f64` | 0.5 | 进度更新间隔时间（秒），必须大于 0 |

### 2. 断点续传配置（`resume` feature）

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `resume_enabled` | `bool` | `true` | 是否启用断点续传功能 |
| `DEFAULT_SEGMENT_SIZE` | `u64` | 1MB | 断点续传的元数据段大小（内部常量） |

### 3. HTTP 客户端配置

可以通过 `client_builder` 方法自定义 reqwest 客户端的所有配置：

```rust
use reqwest::ClientBuilder;
use std::time::Duration;

let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
    .client_builder(|| {
        ClientBuilder::new()
            .timeout(Duration::from_secs(30)) // 请求超时时间
            .connect_timeout(Duration::from_secs(10)) // 连接超时时间
            .tcp_keepalive(Duration::from_secs(60)) // TCP 保持连接时间
            .gzip(true) // 启用 Gzip 压缩
            .brotli(true) // 启用 Brotli 压缩
            .deflate(true) // 启用 Deflate 压缩
            .https_only(true) // 仅允许 HTTPS 请求
            .danger_accept_invalid_certs(false) // 不接受无效证书
    })
    .build();
```

#### 常用客户端配置说明

| 配置项 | 说明 | 默认值 |
|--------|------|--------|
| `timeout` | 请求总超时时间 | 无超时 |
| `connect_timeout` | 连接建立超时时间 | 30 秒 |
| `tcp_keepalive` | TCP 连接保持时间 | 无 |
| `gzip`/`brotli`/`deflate` | 启用对应的压缩算法 | 全部启用 |
| `https_only` | 是否只允许 HTTPS 请求 | `false` |
| `danger_accept_invalid_certs` | 是否接受无效的 SSL 证书 | `false` |
| `proxy` | 代理服务器配置 | 无 |

## Feature Flags 配置

simple_downloader 使用 feature flags 来启用或禁用可选功能，默认启用所有功能。

### 可用 Feature 列表

| Feature | 默认启用 | 依赖 | 说明 |
|---------|---------|------|------|
| `default` | ✅ | - | 默认启用所有功能 |
| `resume` | ✅ | `serde`, `bincode` | 断点续传功能 |
| `progress` | ✅ | - | 下载进度监控功能 |
| `proxy` | ✅ | `reqwest/proxy` | 代理支持功能 |
| `multi-source` | ✅ | - | 多源下载功能 |

### 自定义 Feature 组合

如果只需要基础下载功能，可以在 Cargo.toml 中禁用默认功能并按需启用：

```toml
[dependencies]
simple_downloader = { version = "0.1", default-features = false, features = ["resume"] }
```

## 多源下载配置（`multi-source` feature）

### MultiSourceConfig 配置

```rust
use simple_downloader::{MultiSourceConfig, SourceConfig};

let config = MultiSourceConfig::builder(
    vec![
        SourceConfig::new("https://mirror1.example.com/file.bin")
            .weight(2) // 设置源的权重，权重越高分配的任务越多
            .priority(1), // 设置源的优先级，数字越小优先级越高
        SourceConfig::new("https://mirror2.example.com/file.bin")
            .weight(1)
            .priority(2),
    ],
    "output.bin",
)
.workers(32) // 总并发线程数
.update_interval(1.0)
.health_check_interval(30) // 源健康检查间隔（秒）
.max_retries_per_source(5) // 每个源的最大重试次数
.build();
```

### SourceConfig 配置项

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `url` | `String` | 必填 | 下载源的 URL |
| `weight` | `u32` | 1 | 源的权重，权重越高分配的下载任务越多 |
| `priority` | `u32` | 1 | 源的优先级，数字越小优先级越高，优先使用高优先级的源 |
| `headers` | `HeaderMap` | 空 | 该源的自定义 HTTP 请求头 |

### MultiSourceConfig 配置项

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `sources` | `Vec<SourceConfig>` | 必填 | 下载源列表 |
| `output_path` | `String` | 必填 | 文件保存路径 |
| `workers` | `u64` | CPU 核心数 | 总并发下载线程数 |
| `update_interval` | `f64` | 0.5 | 进度更新间隔（秒） |
| `health_check_interval` | `u64` | 30 | 源健康检查间隔时间（秒） |
| `max_retries_per_source` | `u32` | 5 | 每个源的最大连续失败次数，超过后会被标记为不可用 |
| `failover_threshold` | `f64` | 0.5 | 源故障转移阈值，当源的成功率低于此值时会被降级 |

## 代理配置（`proxy` feature）

### HTTP/HTTPS 代理

```rust
use reqwest::Proxy;

let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
    .client_builder(|| {
        ClientBuilder::new()
            .proxy(Proxy::http("http://proxy.example.com:8080").unwrap())
            .proxy(Proxy::https("http://proxy.example.com:8080").unwrap())
    })
    .build();
```

### SOCKS5 代理

```rust
use reqwest::Proxy;

let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
    .client_builder(|| {
        ClientBuilder::new()
            .proxy(Proxy::socks5("socks5://proxy.example.com:1080").unwrap())
    })
    .build();
```

### 带认证的代理

```rust
use reqwest::Proxy;

let downloader = Downloader::builder("https://example.com/file.bin", "output.bin")
    .client_builder(|| {
        ClientBuilder::new()
            .proxy(Proxy::http("http://user:password@proxy.example.com:8080").unwrap())
    })
    .build();
```

## 环境变量配置

simple_downloader 会自动识别以下环境变量：

| 环境变量 | 说明 |
|----------|------|
| `HTTP_PROXY` / `http_proxy` | HTTP 代理服务器地址 |
| `HTTPS_PROXY` / `https_proxy` | HTTPS 代理服务器地址 |
| `ALL_PROXY` / `all_proxy` | 所有协议的默认代理服务器地址 |
| `NO_PROXY` / `no_proxy` | 不使用代理的域名列表，用逗号分隔 |

## 性能优化建议

### 1. 并发线程数配置

- 对于小文件（< 100MB）：建议使用 4-8 个线程
- 对于大文件（> 1GB）：建议使用 16-32 个线程
- 对于多源下载：可以适当增加线程数，但不要超过源的数量 × 4
- 注意：过多的线程可能会导致服务器限流或连接超时

### 2. 进度更新间隔

- 如果不需要实时进度，可以将 `update_interval` 设置为 1-5 秒，减少 CPU 占用
- 如果需要精确的进度统计，可以保持默认的 0.5 秒

### 3. 断点续传配置

- 对于大文件下载，强烈建议启用断点续传功能
- 如果下载的是临时文件或者小文件，可以禁用断点续传以减少磁盘 I/O

### 4. 客户端配置优化

```rust
use reqwest::ClientBuilder;
use std::time::Duration;

let downloader = Downloader::builder("https://example.com/large_file.bin", "output.bin")
    .workers(32)
    .client_builder(|| {
        ClientBuilder::new()
            .timeout(Duration::from_secs(120)) // 大文件下载需要更长的超时时间
            .tcp_keepalive(Duration::from_secs(60))
            .pool_max_idle_per_host(32) // 增加连接池大小，避免频繁创建连接
    })
    .build();
```

## 配置验证

可以通过以下方法验证配置是否正确：

```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() {
    let builder = Downloader::builder("https://example.com/file.bin", "output.bin")
        .workers(16)
        .update_interval(1.0);
    
    // 构建下载器实例
    let downloader = builder.build();
    
    // 检查配置是否符合预期
    assert_eq!(downloader.workers(), 16);
    assert_eq!(downloader.update_interval(), 1.0);
}
```

## 配置最佳实践

1. **使用 Builder 模式**：推荐使用 `Downloader::builder()` 来配置下载器，而不是直接调用 `Downloader::new()`
2. **合理设置超时**：根据文件大小和网络状况设置合理的超时时间，避免长时间等待
3. **启用压缩**：默认启用的压缩算法可以显著减少下载时间，除非有特殊需求否则不要禁用
4. **配置连接池**：对于多线程下载，适当增加连接池大小可以提高性能
5. **使用环境变量配置代理**：优先使用环境变量配置代理，而不是硬编码在代码中
