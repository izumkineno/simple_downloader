# simple_downloader 文档导航

本文档页用于快速定位当前代码库中的**运行时说明、测试覆盖面和推荐验证入口**。如果只想理解当前实现，请先读本页，再按需跳转到更细的设计文档。

## 一、文档入口

- [`../README.md`](../README.md)：项目功能概览、公开用法、概念级 Mermaid 图。
- [`usage.md`](./usage.md)：**调用指南（权威）**，覆盖所有公开 API 的最小可运行调用形态、Feature 选型、6 大场景与错误模板。
- [`architecture.md`](./architecture.md)：当前实现的**权威运行时说明**，覆盖启动链路、控制面、消息协议、动态分片与重试行为。
- [`configuration.md`](./configuration.md)：可配置项全表与调优建议（参数含义，`usage.md` 提供调用形态）。
- [`errors.md`](./errors.md)：错误变体全表与重试策略。
- [`best-practices.md`](./best-practices.md)：生产级最佳实践与反模式。
- `tests/concurrency.rs`：并发策略回归测试，覆盖“无吞吐证据不分片”“空闲槽位不盲目补位”“接近完成不继续切分”“按剩余工作量而非原始尺寸选目标”等行为。
- `tests/chunk.rs`：分片下载成功/失败路径，以及保留中的 bisect 行为测试骨架。
- `tests/util.rs`：文件信息探测回退链路、写入任务和基础工具行为。
- `test_server/server.py`：本地可控 Range/限速测试服务，适合集成验证与手工观察并发行为。

## 二、项目概览（按 0.6.6 源码校准）

simple_downloader 是一个基于 Rust 与 Tokio 的异步下载库，当前实现重点在于：

- `Downloader`：`DownloadBuilder` 流式编排入口，负责 `Client`（`pool 32/90s/60s + UA`）、`get_file_info` 三级回退探测、`ResumePlan` + 写入任务和初始 `remaining_ranges` 分片。
- `DownloadMonitor`：运行期控制循环，持有 `DownloadState`、`ConcurrencyManager`、`RetryHandler`，`is_rate_limited` 时冻结 `decide_and_act`（`drain_pending` 容量补位除外），`reliable mpsc` 兜底终局事件。
- `chunk_run_with_reliable`：单 `byte-range` 拉取、`206/Content-Range` 强校验、`Early-EOF` 门限、`send_terminal_event` 经 `broadcast 4096` + `reliable mpsc 1` 上报，`PROGRESS_THROTTLE 64KiB/50ms` 限频，`governor` 双桶 `join max` 32-64KiB 批量。
- `file_writer_task_impl`：独立写入任务，有界 `mpsc 128` 背压，`0.5.4+` 流式追加（无 `set_len` 预分配，`create_dir_all` + `truncate(false)`，`ENOSPC` 在 `write/flush` 以 `DownloadError::Io` 透传；`truncate(true)` 仅 resume 覆盖场景）。
- `limiter`：`rate-limit` feature 下 `governor` 令牌桶 `1 token=1 byte`，全局/分源双桶 `tokio::join` 取 `max`，`burst` 默认 `64KiB`，`RateLimiter::reconfigure/disable` 支持 `apply_config` 热更全局限速。
- `queue`：`queue` feature 下 `TaskQueue` FIFO 调度，`JoinSet+AbortHandle` 驱动，`with_max_concurrent 1..64`，`pending_deletes 200ms` 延迟删（`PermissionDenied/未知Io重试`），`with_suffix` `a.tar(1).gz` 与三重 CAS 重命名。

## 三、当前验证与回归面

### 1. 自动化测试

`cargo test --all-features` 31 passed（`cargo clippy --all-features -D warnings` 绿）：

- `tests/concurrency.rs`
  - 无正向吞吐证据时不继续探测分片
  - 仅因 worker 槽位空闲不会触发补位分片
  - 接近完成时不会为了补位继续切分
  - 补位目标按“剩余可分片工作量”而不是原始区间尺寸选择
- `tests/util.rs`
  - `HEAD` 成功获取文件信息
  - `HEAD` 失败后回退 `GET Range: bytes=0-0` 解析 `Content-Range`
  - 无 `Content-Range` 时回退到 `Content-Length`，`416` 与 `bytes */total` 兼容
  - `file_writer_task_impl` 流式追加（`0.5.4+` 无 `set_len`，`ENOSPC` 在 `flush` 暴露）与 `truncate(true/false)` 尾部语义
- `tests/chunk.rs`
  - `206 Content-Range` 强校验、`416 Range Not Satisfiable`、`200 仅单段降级`（P0-1）及 `parse_content_range` 大小写兼容
  - `Early-EOF` 门限与 `final_downloaded==size` 才 `DownloadComplete` + `terminal_event` 经 `reliable` 兜底不丢（`b4fcadf`）
  - `split_range` `MIN_CHUNK_SIZE 10KiB` 边界与 `PROGRESS_THROTTLE 64KiB/50ms` 限频
  - `test_chunk_bisect` 仍 `#[ignore]`，动态分片由 `concurrency` 集成验证
- `tests/rate_limit.rs`（`--features rate-limit,multi-source` 5 用例）
  - `invalid_zero/burst_zero_hard` 校验、`5MiB@1MiB/s 4-6.5s` 全局、`per_source 5-8.5s`、`global+per_source 3-5.5s`
- `queue` 集成（`--features queue` `cargo test --test queue`）
  - `TaskQueue::with_max_concurrent` FIFO/pause/resume/cancel/重命名 三重 CAS + `pending_deletes 200ms` 延迟删
  - `concurrent_enqueue_assigns_unique_paths` 17 并发同名全唯一，无覆盖
- `resume` 单元（`src/resume.rs`）
  - `hash_is_stable`/`prepare_self_heals_on_version/size/malformed`/`atomic_save`/`truncate_tail`
- `monitor` 热更（`src/monitor.rs:apply_config_updates_and_disables_global_limiter`）
  - `workers/interval` 切换与 `global limiter reconfigure/disable` 热更已验证

### 2. 手工 / 集成验证

`test_server/server.py` 支持：

- Range 请求与 `Content-Range` 校验
- 全局 / 单连接限速（供 `rate-limit` 精度矩阵）
- 500 MiB 多源手工观察（`examples/manual_multi_source_test_server.rs` fast 16m/slow 2m）
- 断点续传进程级 kill 恢复（`tests/process_resume.rs` + `examples/resume_harness.rs`）
- 队列隔离示例（`examples/with_queue.rs` 同名 `a(N).ext` 演进）
- 智能调度观察（`examples/test_server_smart_schedule.rs` 与 `adaptive_bench.rs`）

## 四、推荐验证命令

在仓库根目录运行（以 `Cargo.toml:3 0.6.6` 为准）：

```bash
cargo fmt --check
cargo clippy --all-features -D warnings
cargo test --all-features
# 细分
cargo test --all-features -- --nocapture --test-threads=1
cargo test --features rate-limit,multi-source --test rate_limit -- --nocapture
cargo test --features queue --test queue -- --nocapture
cargo test --features resume,multi-source --test resume -- --nocapture --test-threads=1
cargo test --features resume,multi-source --test process_resume -- --nocapture --test-threads=1
```

如果要观察本地服务端行为，可额外启动：

```bash
python test_server/server.py
cargo run --example download
cargo run --features progress --example with_custom_ui
cargo run --features rate-limit,progress --example with_rate_limit
cargo run --features rate-limit,progress --example with_rate_limit -- --multi
cargo run --features multi-source,progress --example manual_multi_source_test_server
cargo run --features queue --example with_queue
cargo run --features progress --example test_server_smart_schedule
```

若需要理解这些验证对应到哪些运行时路径，请对照 [`architecture.md`](./architecture.md) 中的“源码映射”和“运行时时序图”章节。

## 五、项目结构速览

```text
├── src/
│   ├── downloader.rs    # 顶层启动编排（DownloadBuilder/orchestrate_downloads/streaming_download）
│   ├── monitor.rs       # 运行时控制循环（run_with_reliable/apply_config/drain_pending）
│   ├── concurrency.rs   # 动态分片决策（Probing/Stable, 256KiB 阈值）
│   ├── retry.rs         # 即时/延迟重试队列（1s/10s/30总量 FIFO）
│   ├── chunk.rs         # 单分片下载执行（206/Early-EOF/reliable terminal）
│   ├── state.rs         # 聚合下载状态（EMA 0.30）
│   ├── util.rs          # 文件信息探测、流式写入任务（P0-4, truncate 分支）
│   ├── limiter.rs       # rate-limit 令牌桶（governor 双桶 join max）
│   ├── trace.rs         # tracing 初始化门面（RUST_LOG/SIMPLE_DOWNLOADER_LOG）
│   ├── config.rs        # 运行时热更新 RuntimeConfig（workers/interval/global limiter）
│   ├── queue.rs         # 任务队列 FIFO/并发调度（pending_deletes 200ms）
│   ├── task.rs          # 任务句柄与快照（TaskId/TaskState/TaskSnapshot）
│   └── types.rs         # 公共协议类型（DownloadInfo #[non_exhaustive] 稳定契约）
├── tests/
│   ├── chunk.rs
│   ├── concurrency.rs
│   ├── rate_limit.rs
│   ├── util.rs
│   ├── queue.rs
│   ├── resume.rs
│   ├── process_resume.rs
│   └── multi_source.rs
├── test_server/
│   ├── config.ini
│   └── server.py
├── examples/
│   ├── download.rs
│   ├── with_custom_ui.rs
│   ├── with_rate_limit.rs
│   ├── with_queue.rs
│   ├── test_server_smart_schedule.rs
│   ├── manual_multi_source_test_server.rs
│   ├── adaptive_bench.rs
│   └── resume_harness.rs
└── README.md
```
## 六、待实现功能（文档层摘要）

以 `README:79-112 TODO` 为准，`0.6.2+2` 已交付 `rate-limit 双桶/自适应冻结/可靠终局/队列延迟删/DownloadInfo 稳定契约`；剩余 `断点续传 schema 跨版本迁移（version=1 自愈已落地，迁移表待 0.7）`、`多源智能评分（probe_speed 排序+黑名单 3/30s 已落地，EWMA 动态评分待 0.7）`、`多代理端到端矩阵`、`可观测性（tracing info 已落地，复用/失效 segment 事件待 0.7）`。
