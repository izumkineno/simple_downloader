# 安装指南

本文档介绍如何在项目中安装和使用 simple_downloader 库。

## 前置要求

### 1. Rust 版本
simple_downloader 要求 Rust 版本 **1.65 或更高**。

检查当前 Rust 版本：
```bash
rustc --version
```

如果 Rust 版本过低，可以通过 rustup 更新：
```bash
rustup update
```

### 2. 系统要求
simple_downloader 支持以下操作系统：
- ✅ Windows 10 / Windows 11
- ✅ macOS 10.15 (Catalina) 或更高版本
- ✅ Linux (内核版本 4.15 或更高)

### 3. 依赖库
大部分依赖会被 Cargo 自动下载和编译，部分系统可能需要额外安装：

#### Linux 系统
```bash
# Debian/Ubuntu 系列
sudo apt install build-essential libssl-dev pkg-config

# CentOS/RHEL 系列
sudo dnf install gcc openssl-devel pkg-config
```

#### macOS 系统
```bash
# 安装 Xcode 命令行工具
xcode-select --install
```

#### Windows 系统
不需要额外依赖，Rust 安装时会自动配置所需的构建工具。

## 基本安装

在 Cargo.toml 中添加 simple_downloader 依赖：

```toml
[dependencies]
simple_downloader = "0.1.0"
```

默认会启用所有功能（断点续传、进度监控、代理支持、多源下载）。

## 自定义 Feature 安装

如果只需要部分功能，可以禁用默认功能并按需启用：

### 最小功能安装（仅基础下载）
```toml
[dependencies]
simple_downloader = { version = "0.1.0", default-features = false }
```

### 常用组合
```toml
# 基础下载 + 断点续传 + 进度监控
simple_downloader = { version = "0.1.0", default-features = false, features = ["resume", "progress"] }

# 基础下载 + 多源支持
simple_downloader = { version = "0.1.0", default-features = false, features = ["multi-source"] }

# 全功能（与默认相同）
simple_downloader = { version = "0.1.0", features = ["full"] }
```

### 可用 Feature 列表
| Feature | 说明 | 额外依赖 |
|---------|------|----------|
| `resume` | 断点续传功能 | `serde`, `bincode` |
| `progress` | 下载进度监控功能 | - |
| `proxy` | 代理支持功能 | `reqwest/proxy` |
| `multi-source` | 多源下载功能 | - |
| `full` | 启用所有功能 | - |

## 安装最新开发版本

如果想使用最新的开发版本，可以直接从 GitHub 安装：

```toml
[dependencies]
simple_downloader = { git = "https://github.com/your-username/simple_downloader.git", branch = "main" }
```

或者安装指定的 commit：
```toml
simple_downloader = { git = "https://github.com/your-username/simple_downloader.git", rev = "abc1234" }
```

## 从源代码安装

如果需要本地修改或调试，可以克隆源代码并安装：

```bash
# 克隆仓库
git clone https://github.com/your-username/simple_downloader.git
cd simple_downloader

# 构建库
cargo build --release

# 运行测试
cargo test --all-features

# 生成文档
cargo doc --open
```

在本地项目中使用这个版本：
```toml
[dependencies]
simple_downloader = { path = "../path/to/simple_downloader" }
```

## 验证安装

### 1. 创建测试项目
```bash
cargo new test_download
cd test_download
```

### 2. 添加依赖
在 Cargo.toml 中添加：
```toml
[dependencies]
simple_downloader = "0.1.0"
tokio = { version = "1.0", features = ["full"] }
```

### 3. 编写测试代码
将 src/main.rs 替换为：
```rust
use simple_downloader::Downloader;

#[tokio::main]
async fn main() {
    println!("开始测试下载...");
    
    // 下载一个小的测试文件
    match Downloader::builder(
        "https://httpbin.org/image/jpeg",
        "test_image.jpg",
    )
    .download()
    .await
    {
        Ok(_) => println!("✅ 下载成功！安装验证通过。"),
        Err(e) => eprintln!("❌ 下载失败: {}", e),
    }
}
```

### 4. 运行测试
```bash
cargo run
```

如果看到 "✅ 下载成功！安装验证通过。" 的输出，说明安装成功。

## 版本兼容性

### Rust 版本兼容性
| simple_downloader 版本 | 最低 Rust 版本 |
|------------------------|---------------|
| 0.1.x                  | 1.65          |

### 依赖库兼容性
| 依赖库 | 版本要求 |
|--------|---------|
| tokio | 1.0 ~ 1.35 |
| reqwest | 0.11 ~ 0.12 |
| thiserror | 1.0 |
| bytes | 1.0 |
| faststr | 0.2 |
| futures-util | 0.3 |
| serde（resume feature）| 1.0 |
| bincode（resume feature）| 1.3 |

##  Cargo 配置优化

为了加快编译速度，可以在 `~/.cargo/config.toml` 中添加以下配置：

```toml
[build]
# 使用增量编译
incremental = true
# 并行编译任务数
jobs = 8

# 优化依赖编译
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 16
panic = "abort"
```

## 常见安装问题

### Q: 编译时出现 "openssl-sys 编译失败"
**A**: 这是因为系统缺少 OpenSSL 开发库：

**Linux**:
```bash
# Debian/Ubuntu
sudo apt install libssl-dev pkg-config

# CentOS/RHEL
sudo dnf install openssl-devel pkg-config
```

**macOS**:
```bash
brew install openssl
```

**Windows**:
可以使用 `vendored-openssl` feature 来静态编译 OpenSSL：
```toml
simple_downloader = { version = "0.1.0", features = ["vendored-openssl"] }
```

### Q: 编译速度太慢
**A**: 
1. 首次编译需要编译所有依赖，属于正常情况
2. 可以启用 `sccache` 来缓存编译结果：
```bash
# 安装 sccache
cargo install sccache

# 在 ~/.cargo/config.toml 中添加
[build]
rustc-wrapper = "sccache"
```

### Q: 提示 "无法找到包 simple_downloader"
**A**: 
1. 检查 Cargo.toml 中的版本号是否正确
2. 运行 `cargo update` 更新包索引
3. 如果是从 GitHub 安装，检查仓库地址是否正确，网络是否能访问 GitHub

### Q: 下载功能正常，但断点续传功能不可用
**A**:
1. 检查是否启用了 `resume` feature
2. 确认 Cargo.toml 中 `resume` feature 已经添加
3. 运行 `cargo clean && cargo build` 重新编译

### Q: 代理功能不生效
**A**:
1. 检查是否启用了 `proxy` feature
2. 确认代理地址和端口配置正确
3. 检查是否设置了 `NO_PROXY` 环境变量排除了目标域名

## 升级版本

升级到最新版本：
```bash
cargo update -p simple_downloader
```

查看当前安装的版本：
```bash
cargo tree | grep simple_downloader
```

## 卸载

从项目中移除依赖，只需要删除 Cargo.toml 中的 simple_downloader 相关配置即可。

如果是全局安装的工具（如果有的话）：
```bash
cargo uninstall simple_downloader
```
