# simple_downloader 项目文档

## 一、项目概述
simple_downloader 是一个基于 Rust 开发的异步多线程下载库，支持动态并发控制、智能重试、实时监控等高级特性。

- **版本**: 0.1.0
- **依赖**: tokio (异步运行时)、reqwest (HTTP客户端)、bytes (字节处理)、thiserror (错误处理) 等
- **许可证**: MIT

## 二、核心功能特性
1. **消息驱动的异步架构**:
   - 四大核心组件：Downloader (协调器)、DownloadMonitorState (状态机)、chunk_run (下载工蜂)、file_writer_task (文件写入器)
   - 组件间通过 tokio 的 broadcast 和有界 mpsc 通道通信，完全解耦

2. **自适应下载引擎**:
   - 动态并发控制：从单线程启动，经过带宽探测阶段，自动调整到最佳并发数
   - 智能任务调度：平稳阶段动态分裂最慢/最大的任务块，实现负载均衡

3. **两级重试机制**:
   - 即时重试：应对瞬时网络抖动，有最大重试次数限制
   - 长延迟重试：长时间网络中断或服务器故障时，延迟后再次重试，极大提高下载成功率

4. **精准实时监控**:
   - 中心化状态管理，统一追踪所有下载块状态
   - 指数移动平均（EMA）算法计算速度，避免瞬时波动
   - 丰富的状态事件广播，方便UI对接

5. **高效安全的文件IO**:
   - 独立写入任务，避免多线程写入锁竞争
   - 有界通道实现反压机制，防止内存溢出
   - 下载前预分配磁盘空间，减少文件碎片

6. **高兼容性文件探测**:
   - 优先使用HEAD请求，失败自动回退到Range请求，兼容各类服务器

## 三、项目结构
```
simple_downloader/
├── src/                     # 核心源码
│   ├── lib.rs              # 库入口，公共API导出
│   ├── downloader.rs       # 下载器核心实现
│   ├── monitor.rs          # 下载监控器，状态管理
│   ├── chunk.rs            # 下载块任务实现
│   ├── concurrency.rs      # 并发控制逻辑
│   ├── retry.rs            # 重试机制实现
│   ├── state.rs            # 下载状态定义
│   ├── types.rs            # 公共类型定义
│   └── util.rs             # 工具函数
├── examples/               # 使用示例
│   ├── download.rs         # 基础下载示例
│   └── with_custom_ui.rs   # 自定义进度条UI示例
├── test_server/            # 测试服务
├── Cargo.toml              # 项目配置
└── README.md               # 项目文档
```

## 四、API 使用说明
### 核心入口
```rust
pub use downloader::Downloader;
pub use types::{ChunkId, DownloadCmd, DownloadError, DownloadInfo, Result};
pub use reqwest; // 允许用户自定义客户端
```

### 基础使用示例
```rust
use simple_downloader::{Downloader, DownloadInfo, reqwest::ClientBuilder};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    let downloader = Downloader::new(
        "下载链接",
        "保存路径",
        16, // 最大并发数
        1.0, // 进度更新间隔（秒）
        || ClientBuilder::new(), // 自定义客户端构建器
    );

    // 进度处理逻辑
    let progress_handler = |total_size: u64, mut info_rx: broadcast::Receiver<DownloadInfo>| async move {
        // 处理进度更新事件
    };

    // 启动下载
    match downloader.run(progress_handler).await {
        Ok(_) => println!("下载成功！"),
        Err(e) => eprintln!("下载失败: {}", e),
    }
}
```

### 高级功能：自定义UI
参考 `examples/with_custom_ui.rs`，可以利用 DownloadInfo 事件实现丰富的进度展示：
- 总进度条 + 各分块独立进度条
- 分块状态显示（下载中/重试中/已完成/失败等）
- 实时速度、剩余时间显示
- 错误信息提示

## 五、系统架构
### 核心流程
1. 用户调用 Downloader.run()，传入进度处理器
2. 下载器启动文件写入任务、监控器和初始下载块任务
3. 下载块从服务器获取数据，通过数据通道发送给文件写入任务
4. 监控器统一管理下载状态，动态调整并发数和处理重试
5. 所有状态变化通过广播通道通知进度处理器

### 通信通道
- **InfoChannel (广播)**: 下载块进度、状态变化、聚合监控更新
- **CmdChannel (广播)**: 控制命令（分割任务、终止下载等）
- **DataChannel (MPSC)**: 下载数据传输到文件写入任务

## 六、待实现功能
1. **断点续传**: 进度持久化到元数据文件，支持任务恢复
2. **多源多代理下载**: 支持多个镜像URL，智能源调度和切换
3. **速度限制**: 可配置全局下载速度上限
4. **动态配置调整**: 运行时可修改并发数、重试策略等

## 七、运行说明
### 运行基础示例
```bash
cargo run --example download
```

### 运行带自定义UI示例
```bash
cargo run --example with_custom_ui