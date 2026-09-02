# 安装指南

本文档介绍如何在项目中安装和使用 simple_downloader 库。以 `Cargo.toml` 与 `docs/usage.md` 为准。

## 前置要求

### 1. Rust 版本

`Cargo.toml:5` 要求 **1.85 或更高**（`edition = 2024`）。

```bash
rustc --version
rustup update
```

### 2. 系统要求

- Windows 10 / 11、macOS 10.15+、Linux 4.15+
- 本库 `reqwest` 默认走 `rustls`，**无需** 系统 `openssl`/`libssl-dev`。`installation.md` 旧文提及的 `libssl-dev` 仅适用于 `native-tls` 场景，本库无需。

## 基本安装

`Cargo.toml` 中 `default = []`，裸依赖即最轻量单源下载：

```toml
[dependencies]
simple_downloader = "0.5.4"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

此时可用 `Downloader::builder(url, path).download().await`，未启用 `resume/progress/multi-source/proxy/rate-limit`。

> `simple_downloader = "0.5.4"` **不会** 默认启用任何可选功能，旧文“默认启用所有”已过时。

## 自定义 Feature 安装

以 `Cargo.toml:14-19` 与 `docs/usage.md:25` 为准：
| Feature | 默认 | 说明 | 额外依赖 |
|---------|------|------|----------|
| `resume` | ❌ | 断点续传 `*.download.bitcode`，依赖 `bitcode@0.6` | `bitcode@0.6` |
| `progress` | ❌ | 进度事件 `DownloadInfo` 与 `run(handler)` | — |
| `multi-source` | ❌ | `MultiSourceConfig`/`SourceConfig`/`LaneModel`、`new_multi` | — |
| `proxy` | ❌ | 隐含 `multi-source`，`ProxyConfig` 等 | `multi-source` |
| `rate-limit` | ❌ | 全局/分源限速 `governor` 令牌桶（`1 token=1 byte`）| `governor@0.7` |

```toml
# 最小（仅基础）
simple_downloader = { version = "0.5", default-features = false }

# 常用：基础 + 断点续传 + 进度
simple_downloader = { version = "0.5", default-features = false, features = ["resume","progress"] }

# 全功能（含限速）
simple_downloader = { version = "0.5", default-features = false, features = ["resume","progress","multi-source","proxy","rate-limit"] }
```

不存在 `full`/`vendored-openssl` feature，勿使用。

## 安装最新开发版本

```toml
[dependencies]
simple_downloader = { git = "https://github.com/izumkineno/simple_downloader", branch = "master" }
# 指定 commit
simple_downloader = { git = "https://github.com/izumkineno/simple_downloader", rev = "abc1234" }
```

## 从源代码安装

```bash
git clone https://github.com/izumkineno/simple_downloader
cd simple_downloader
cargo build --release
cargo test --all-features
cargo doc --open
```

本地路径依赖：

```toml
simple_downloader = { path = "../simple_downloader" }
```

## 验证安装

```bash
cargo new test_download && cd test_download
```

`Cargo.toml`:

```toml
[dependencies]
simple_downloader = { version = "0.5", default-features = false }
tokio = { version = "1", features = ["rt-multi-thread","macros"] }
```

`src/main.rs`:

```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() {
    println!("开始测试下载...");
    match Downloader::builder("https://proof.ovh.net/files/10Mio.dat","10Mio.dat")
        .download().await {
        Ok(_) => println!("✅ 下载成功！"),
        Err(e) => eprintln!("❌ 下载失败: {}", e),
    }
}
```

```bash
cargo run
```

| simple_downloader | 最低 Rust | 关键依赖 |
|-------------------|-----------|----------|
| 0.5.x | 1.85 | `tokio 1.52` `reqwest 0.13` `thiserror 2` `bytes 1` `faststr 0.2` `futures-util 0.3` `bitcode 0.6` (resume) `tracing 0.1` `tracing-subscriber 0.3` `governor 0.7` (rate-limit) |
| 0.4.x | 1.85 | `tokio 1.52` `reqwest 0.13` `thiserror 2` `bytes 1` `faststr 0.2` `futures-util 0.3` `bitcode 0.6` (resume) `tracing 0.1` `tracing-subscriber 0.3` |
| 0.3.x | 1.85 | `tokio 1.52` `reqwest 0.13` `thiserror 2` `bytes 1` `faststr 0.2` `futures-util 0.3` `bitcode 0.6` (resume) |
| 0.2.x | 1.85 | 同 0.3.x，逻辑加固与测试补齐 |
| 0.1.x | 1.85 | 同 0.2.x，API 兼容，仅性能与文档差异 |

## Cargo 配置优化（可选）

`~/.cargo/config.toml`:

```toml
[build]
incremental = true
jobs = 8
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 16
```

## 常见问题

### Q: 代理不生效 / 断点续传不可用
检查 `Cargo.toml` 是否按需启用了 `proxy`/`resume`，并重建 `cargo clean && cargo build --all-features`。

### Q: 提示无法找到包
`cargo update` 刷新索引，检查 `Cargo.toml` `version` 与 `git` 地址拼写（应为 `izumkineno/simple_downloader`）。

## 升级/卸载

```bash
cargo update -p simple_downloader
cargo tree | grep simple_downloader
# 卸载：删除 Cargo.toml 对应行即可
```
