# simple_downloader 文档导航

本文档页用于快速定位当前代码库中的**运行时说明、测试覆盖面和推荐验证入口**。如果只想理解当前实现，请先读本页，再按需跳转到更细的设计文档。

## 一、文档入口

- [`../README.md`](../README.md)：项目功能概览、公开用法、概念级 Mermaid 图。
- [`usage.md`](./usage.md)：**调用指南（权威）**，覆盖所有公开 API 的最小可运行调用形态、Feature 选型、7 大场景（含 `rate-limit`）与错误模板。
- [`architecture.md`](./architecture.md)：当前实现的**权威运行时说明**，覆盖启动链路、控制面、消息协议、动态分片、重试与限速冻结。
- [`configuration.md`](./configuration.md)：可配置项全表与调优建议（`rate-limit` 全局/分源双桶，`burst` 校检）。
- [`errors.md`](./errors.md)：错误变体全表（9 变体，含 `PermanentFailure`/`InvalidArgument`）与重试策略。
- [`best-practices.md`](./best-practices.md)：生产级最佳实践与反模式（含 `rate-limit`）。
- [`faq.md`](./faq.md)：高频问题（断点续传 `*.download.bitcode`、并发降级、`rate-limit` 等）。
- `tests/concurrency.rs`：并发策略回归测试，覆盖“无吞吐证据不分片”“空闲槽位不盲目补位”“接近完成不继续切分”“按剩余工作量而非原始尺寸选目标”等行为。
- `tests/chunk.rs`：分片下载成功/失败路径，以及保留中的 bisect 行为测试骨架。
- `tests/util.rs`：文件信息探测回退链路、写入任务和基础工具行为。
- `tests/rate_limit.rs`：限速校验与多源硬上限。
- `test_server/server.py`：本地可控 Range/限速测试服务，适合集成验证与手工观察并发行为。
## 二、项目概览（按 0.5.4 源码校准）

simple_downloader 是一个基于 Rust 与 Tokio 的异步下载库，当前实现重点在于：

- `Downloader`：启动编排入口，负责 client、文件信息探测、写入任务和初始 chunk。
- `DownloadMonitor`：运行期控制循环，持有 `DownloadState`、`ConcurrencyManager`、`RetryHandler`，限速启用时冻结并发探测。
- `chunk_run`：执行单个 byte-range 拉取、Range 206/Content-Range 校验、Early-EOF 门限与事件上报。
- `file_writer_task`：独立文件写入任务，通过有界 `mpsc 128` 提供背压，0.5.4+ 流式追加（无 `set_len` 预分配，`truncate(false)`）。
- `limiter`：`rate-limit` feature 下 `governor` 令牌桶 `1 token=1 byte`，全局/分源双桶 `tokio::join` 取 `max`，`burst` 默认 64KiB。

项目当前许可证文件为仓库根目录中的 Apache License 2.0（见 `LICENSE`）。
**版本**：`Cargo.toml 0.5.4`（`rust-version 1.85`，`edition 2024`）。
## 三、当前验证与回归面

### 1. 自动化测试


- `tests/concurrency.rs`
  - 无正向吞吐证据时不继续探测分片
  - 仅因 worker 槽位空闲不会触发补位分片
  - 接近完成时不会为了补位继续切分
  - 补位目标按“剩余可分片工作量”而不是原始区间尺寸选择
- `tests/util.rs`
  - `HEAD` 成功获取文件信息
  - `HEAD` 失败后回退 `GET Range: bytes=0-0`
  - 无 `Content-Range` 时回退到 `Content-Length`
  - `file_writer_task` 流式追加（0.5.4+ 无 `set_len`，`ENOSPC` 在 `flush` 暴露）与零填充行为已对齐
- `tests/chunk.rs`
  - `206 Content-Range` 校验、`416 Range Not Satisfiable`、`200 仅单段降级`（P0-1）
  - `Early-EOF` 门限与 `final_downloaded==size` 才 `DownloadComplete`（P0-2）
  - `test_chunk_bisect` 目前保留为 `#[ignore]`，说明动态分片仍主要依赖更复杂的延迟响应场景做集成验证
- `tests/rate_limit.rs`（`--features rate-limit,multi-source`）
  - `invalid_zero` / `global 5MiB@1MiB/s 4-6.5s` / `per_source` / `global_hard` / `burst=0` 校验，`tokio::join` 双桶 `max`
- `tests/multi_source.rs` / `tests/resume.rs` / `tests/process_resume.rs`：多源 lane 黑名单、断点续传 hash 恢复与子进程级恢复

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
cargo check --all-features
cargo test --all-features
cargo clippy --all-features -- -D warnings
```

细分：
```bash
cargo test --features rate-limit,multi-source --test rate_limit -- --nocapture
cargo test --features resume,multi-source --test resume -- --nocapture --test-threads=1
python test_server/server.py
cargo run --example download
cargo run --features rate-limit,progress --example with_rate_limit -- --multi
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
│   ├── chunk.rs         # 单分片下载执行（P0-1 Range/Early-EOF，MIN_CHUNK_SIZE 10 KiB）
│   ├── state.rs         # 聚合下载状态（EMA 0.30）
│   ├── util.rs          # 文件信息探测、流式写入任务（P0-4）
│   ├── limiter.rs       # rate-limit 令牌桶（0.5.x，governor 双桶）
│   ├── trace.rs         # tracing 初始化门面
│   ├── lane.rs          # 多源/代理调度（PerSource/PerSourceProxy）
│   ├── resume.rs        # 断点续传 ledger（64 KiB segment）
│   └── types.rs         # 公共协议类型（9 错误变体）
├── tests/
│   ├── chunk.rs
│   ├── concurrency.rs
│   ├── rate_limit.rs
│   ├── multi_source.rs
│   ├── resume.rs
│   ├── process_resume.rs
│   └── util.rs
├── test_server/
│   ├── config.ini
│   └── server.py
├── examples/
│   ├── download.rs
│   ├── with_custom_ui.rs
│   ├── with_rate_limit.rs
│   ├── test_server_smart_schedule.rs
│   ├── manual_multi_source_test_server.rs
│   └── resume_harness.rs
└── README.md
```

## 六、待实现功能（文档层摘要）

`0.5.4` 已落地 `rate-limit` 流式追加 + P0 6 项；`README TODO` 剩余 `断点续传 schema 演进/可观测性`、`多源智能调度/代理矩阵`、`配置灵活性/任务队列/稳定 UI 契约` 仍按 `README:79-112` 为准，本文仅做测试与结构导航，不重复总纲。
