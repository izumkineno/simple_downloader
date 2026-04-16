# simple_downloader 文档导航

本文档页用于快速定位当前代码库中的**运行时说明、测试覆盖面和推荐验证入口**。如果只想理解当前实现，请先读本页，再按需跳转到更细的设计文档。

## 一、文档入口

- [`../README.md`](../README.md)：项目功能概览、公开用法、概念级 Mermaid 图。
- [`architecture.md`](./architecture.md)：当前实现的**权威运行时说明**，覆盖启动链路、控制面、消息协议、动态分片与重试行为。
- `tests/chunk.rs`：分片下载成功/失败路径，以及保留中的 bisect 行为测试骨架。
- `tests/util.rs`：文件信息探测回退链路、写入任务和基础工具行为。
- `test_server/server.py`：本地可控 Range/限速测试服务，适合集成验证与手工观察并发行为。

## 二、项目概览（按当前源码校准）

simple_downloader 是一个基于 Rust 与 Tokio 的异步下载库，当前实现重点在于：

- `Downloader`：启动编排入口，负责 client、文件信息探测、写入任务和初始 chunk。
- `DownloadMonitor`：运行期控制循环，持有 `DownloadState`、`ConcurrencyManager`、`RetryHandler`。
- `chunk_run`：执行单个 byte-range 拉取、写入请求发送与事件上报。
- `file_writer_task`：独立文件写入任务，通过有界 `mpsc` 提供背压。

项目当前许可证文件为仓库根目录中的 Apache License 2.0（见 `LICENSE`）。

## 三、当前验证与回归面

### 1. 自动化测试

- `tests/util.rs`
  - `HEAD` 成功获取文件信息
  - `HEAD` 失败后回退 `GET Range: bytes=0-0`
  - 无 `Content-Range` 时回退到 `Content-Length`
  - `file_writer_task` 的偏移写入与零填充行为
- `tests/chunk.rs`
  - chunk 正常下载并发送 `WriteFile` / `DownloadComplete`
  - 请求失败后发送 `ChunkFailed`
  - `test_chunk_bisect` 目前保留为 `#[ignore]`，说明动态分片仍主要依赖更复杂的延迟响应场景做集成验证

### 2. 手工 / 集成验证

`test_server/server.py` 支持：

- Range 请求
- 全局 / 单连接限速
- 配置热更新
- 下载进度与连接状态观察

这使它适合验证 `ConcurrencyManager` 的动态分片决策是否符合预期，而不仅是验证单个单元测试断言。

## 四、推荐验证命令

在仓库根目录运行：

```bash
cargo fmt --check
cargo check
cargo test
```

如果要观察本地服务端行为，可额外启动：

```bash
python test_server/server.py
cargo run --example download
```

若需要理解这些验证对应到哪些运行时路径，请对照 [`architecture.md`](./architecture.md) 中的“源码映射”和“运行时时序图”章节。

## 五、项目结构速览

```text
simple_downloader/
├── src/
│   ├── downloader.rs    # 顶层启动编排
│   ├── monitor.rs       # 运行时控制循环
│   ├── concurrency.rs   # 动态分片决策
│   ├── retry.rs         # 即时/延迟重试队列
│   ├── chunk.rs         # 单分片下载执行
│   ├── state.rs         # 聚合下载状态
│   ├── util.rs          # 文件信息探测、写入任务
│   └── types.rs         # 公共协议类型
├── tests/
│   ├── chunk.rs
│   └── util.rs
├── test_server/
│   ├── config.ini
│   └── server.py
├── examples/
└── README.md
```

## 六、待实现功能（文档层摘要）

当前 README 中列出的断点续传、多源多代理、速度限制和运行时动态配置仍属于未来工作；在这些能力落地前，应继续把 `DownloadMonitor` / `DownloadState` 的职责边界当作扩展基线，而不是回退到旧版状态机命名或“预创建固定 worker”这类过时实现假设。
