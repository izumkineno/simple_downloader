# Changelog

所有重要的项目变更都会记录在这个文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/) 规范。

## [Unreleased]

### 新增

- [ ] 下载速度限制功能（全局/分源限速）
- [ ] 下载任务队列管理（暂停/恢复/取消/查询）
- [ ] 并行下载多个文件
- [ ] 图形化进度展示工具

### 改进

- [ ] 更智能的多源调度评分（响应时间/吞吐/失败率）
- [ ] 更完整的多代理端到端测试矩阵
- [ ] 元数据 schema 跨版本迁移策略与可观测性增强

---

## [0.4.0] - 2026-08-31

### ✨ 新增

- **可观测性 `trace` 模块** 新增 `src/trace.rs`：库零侵入全局 `tracing`/`tracing-subscriber` 初始化门面 `init_tracing`/`try_init_tracing`/`init_tracing_with_filter`/`init_tracing_json_for_env`，支持 `RUST_LOG`/`SIMPLE_DOWNLOADER_LOG` > `Env::Development(debug)`/`Production(info)` 分级与 `::tracing::instrument` 全链路埋点；`src/lib.rs` 暴露 `pub mod trace` + `pub use trace as tracing` 兼容别名，不自动安装订阅者（`663211b` 前已引入 `tracing 0.1`/`tracing-subscriber 0.3`）
- **自适应并发分级调优** `concurrency.rs` 新增 `new_with_interval`、分级阈值/指数退避/动态观察期与 `AdaptiveBenchmark` 压测 harness `examples/adaptive_bench.rs`，`Probing→Stable` 收敛参数由 `2→1` 等调优，`test_server` 三源 32 workers 验证无回归（`ba0d919`/`797740b`/`85255e9`）
- **默认 User-Agent** `src/lib.rs` 新增 `pub(crate) const DEFAULT_USER_AGENT = concat!("simple_downloader/", env!("CARGO_PKG_VERSION"))`，`util::make_request`/`downloader::default_client_builder` 自动注入，兼容 ToDesktop 等严格 UA 校验网关（`663211b`）

### 🔧 修复（Correctness - P0 阻断 8 项）

- **P0-01 Range 206 强校验** `chunk.rs:69-96,148-164` + `util.rs:68-98`：请求 `Range` 时严格校验 `206 Partial Content` + `Content-Range`，缺失/不一致时 `ChunkFailed` 并在单段场景可降级单流；补充 `util::parse_content_range` 与 `3` 项 mockito 用例（`d810d8a`）
- **P0-02 Early-EOF 完整性 + 真实字节累加** `chunk.rs:209-234` `state.rs:130-134`：流 `None` 时校验 `offset==end+1` 否则 `Incomplete` 失败，`state::complete_chunk` 按 `size()` 而非 `downloaded` 累加，修复 64KiB 节流下少算，`tests/chunk.rs` 新增截断用例（`1f6888d`）
- **P0-03 Broadcast Lagged 对账** `monitor.rs:165-171` + `state.rs`：`Lagged` 时通过 `chunk.range` 对账 `DownloadState`，避免 32 workers 打满 4096 时误判完成；后续补 `tasks.next()` 对账分支（`94cb896`/`7042f04`）
- **P0-04 预分配原子化 + mkdir -p** `util.rs:191-199` `downloader.rs:535-544`：`file_writer` 先 `set_len` 再 `truncate` 防 ENOSPC 清零，`download` 前 `create_dir_all(parent)`，`total_size==0` 时零分配（`2effc8f`）
- **P0-05 Resume 形状自愈** `resume.rs:120-139,206-225`：`validate_shape` 对 `total_size`/`segment_size`/`version` 不一致时 `warn` 并删除损坏 sidecar 重建，损坏 hash 仅失效该 segment（`8205c42`/`b8d2787`）
- **P0-06 Retry 计时 + FIFO** `retry.rs:102-235` `monitor.rs:417-457`：延迟计时统一 `10s` 不叠加 `2s` 探测期，`push_front→push_back` 改 FIFO，`DELAYED_RETRY_DURATION 10s`（`6107c93`/`18a8754`）
- **P0-C1/C2 控制面补发** `monitor.rs` + `chunk.rs:retry` 失败路径补 `MonitorUpdate` 终态与 `tasks.next()` lane `release` 对账（`aba81a7`/`7042f04`）
- **多源回退探测限速** `lane.rs` fallback 探针 `stream` 加 `64KiB` 限额，`multi-source` 长流不再无限缓冲（`3b90de5`）；`downloader.rs` `JoinHandle<Result>` 透传 `writer` 错误（`64d43e1`）

### 🛠️ 修复（体验 - M3 P2 5 项）

- **M3-01 probe_speed 实测** `lane.rs:549,575,221,491,308`：`MultiRuntime::from_config` 各源 `64KiB` 真实吞吐 `probe_speed` 测量后按速度降序建 `LaneScheduler`，`LaneEntry` 黑名单 30s 语义保留（`e33546f`）
- **M3-02 碎片门槛 256KiB** `concurrency.rs:16,109,496`：`MIN_SPLITTABLE_REMAINING 256KiB`，remaining<阈值时 `splits 0`，避免小文件过度切分（`311c377`）
- **M3-03 单源分裂统一** `downloader.rs:798,718`：`split_resume_ranges` 单源/多源统一分裂逻辑，`1MiB 8 workers` 验证 `len 8`（`9a0170a`）
- **M3-04 Sidecar 清理重试** `downloader.rs:577` `resume.rs:142`：成功后 `*.download.bitcode` 删除带 3 次重试与 `PermissionDenied` 指数退避（`254bd2e`）
- **M3-05 连接池保留** `downloader.rs:37,404`：`with_client_builder` 时保留用户 `pool_max_idle_per_host/idle_timeout/tcp_keepalive`，不再被 `32/90s/60s` 默认覆盖（`c1e52c4`）

### 📚 文档

- `docs/installation.md` 版本表 `0.3.x→0.4.x`，示例 `simple_downloader = "0.4.0"` 同步 `Cargo.toml:3`
- `src/lib.rs` crate 文档安装示例 `version = "0.4"` 对齐
- `docs/fix-plan.md` 等计划文档锚点仍为 `v0.3.1`（历史基线），新锚点为 `v0.4.0`

## [0.3.1] - 2026-08-24

### 🔧 修复

- **MissingContentLength 流式回退** `downloader::run_internal` 单源/多源在 `get_file_info` 返回 `MissingContentLength`/`NoAvailableSources` 时自动回退为 **单流流式下载**（`streaming_download`）：`file_writer_task(0)` 零预分配，`bytes_stream` 顺序 `offset+=len`，`ChunkProgress` + `ticker 0.5s MonitorUpdate(total_size 0)`，`writer flush` 前哈希，`DownloadComplete` 收尾；`progress_handler(0, rx)` 兼容
- **is_complete 未知大小误判** `types::DownloadInfo::is_complete` 对 `total_size==0` 改为 `downloaded==0`（`0/0 完成，0/N 未完成`），修复流式 `0/10` 误判完成而 `0/0` 仍完成
- **docs** `docs/errors.md` `MissingContentLength` 表格与详情更新为 `0.3.1+ 自动回退` 行为
- **tests** 新增 `tests/missing_content_length.rs` 2 用例：`chunked-body-no-length` 流式回退（`HEAD 200 无长度` + `Range 0-0 200 chunked` + `GET chunked`）与 `0 字节 is_complete` 语义，`cargo test --test missing_content_length --all-features 2 passed`
## [0.3.0] - 2026-08-24

### 🔧 修复（Correctness - P0 挂死/丢数据）

- **永重试挂死** `retry.rs` `MAX_TOTAL_ATTEMPTS=30` + `permanent_failures` 熔断，`monitor::run→Result` 三处检查 `TerminateAll+PermanentFailure`，`downloader` 保活 `writer_handle`
- **丢范围空洞** `monitor.rs` 新增 `pending_bisects:VecDeque`，`ChunkBisected` 容量不足入队；`handle_tick` 按 `&mut Option<MultiRuntime>` 增量调度 pending；`are_all_tasks_done` 纳入 pending；`downloader` 初始 `claim` 失败入队；`retry` 容量不足 `push_front_retry`
- **完成计数少算 64KiB** `state.rs` `complete_chunk += size()` 而非 `downloaded`，容忍节流/`Lagged` 丢失
- **广播 Lagged 丢控制** `downloader.rs` `CHANNEL_CAPACITY 1024→4096`，`64KiB/50ms` 节流后 `1.5k≪4096`

### 🛠️ 修复（Correctness - P1 正确性）

- **单代理非法拖垮全源** `lane::expand_lanes` `Proxy::all`/`Client::build` 改 per-lane `match+continue` 仅跳过该 lane
- **多源非 Range 一刀切** `lane::MultiRuntime` 双桶 `range/fallback`，`from_config` 优先 Range，无 Range 回退非 Range 并 `supports_ranges` 标记；`downloader` 据此 `workers=1` 降级
- **哈希前未 flush** `util::file_writer` 3 处 `write_all→flush→record_write`，`通道关闭` 同理
- **HEAD 误判 Range** `util::get_file_info` `HEAD` 仅记录 `head_size/head_support`，`Range 0-0` 以 `206/Content-Range` 金标准，`501` 回退 `HEAD`
- **失败时 total 回落** `state::preserve_partial` + `retry::on_chunk_failed` 先保留 `downloaded` 再移除

### 🧹 修复（契约 - P2）

- **黑名单永久** `lane::LaneScheduler` `blacklisted_at:Option<Instant>` + `BLACKLIST_DURATION 30s`，`best_lane &mut decay`，`lane_health` 有效判定，`primary/best_lane_runtime &mut`
- **0 字节永不完成** `types::DownloadInfo::is_complete` 改 `downloaded>=total_size`（`0>=0 true`）
- **测试适配** `tests/multi_source` `best_lane &mut` 适配

### ✅ 测试（test_server 集成）

- 新增 `tests/test_server_comprehensive.rs` 9 用例（`--all-features 1.95s`）复用 `test_server_harness::{TestServerFile,RunningTestServer}`：`zero_byte`/`large 3MiB 8workers`/`per_lane pending`/`proxy invalid`/`resume preserve`/`head fallback`/`retry/blacklist/state` 单元
- 暴露 `LaneScheduler::set_blacklisted_at_for_test/integration_test` 与 `RetryHandler::total_attempts_for_test` 供集成测试
- 修复 `mockito` `Range 0-0` 探测与 `501` 降级 mock，`cargo test --all-features 11 套全绿 30s`
## [0.2.0] - 2026-08-24

### 🔧 修复（Correctness）

- **广播背压**：`monitor.rs`/`chunk.rs` 区分 `Lagged/Closed`，消除积压误退出；`tasks` 分支仅在 `is_download_finished` 时提前退出，避免 `DownloadComplete` 竞态丢事件
- **断点续传**：新建 sidecar 立即落盘（`<64KiB` 中断可恢复）；`record_write` 改 `tokio::fs` 异步落盘；成功后自动清理 `*.download.bitcode`
- **异步阻塞**：`ResumePlan::prepare` 增 `prepare_async` 经 `spawn_blocking` 卸载，避免阻塞 Tokio 运行时
- **阈值一致性**：统一 `MIN_CHUNK_SIZE=10KiB` 复用 `chunk` 常量，`downloader` 降级阈值重命名 `MIN_PARALLEL_FILE_SIZE=1MiB`；`split_resume_ranges` 加最小块守卫防碎片
- **重试熔断**：`RetryHandler` 新增 `MAX_TOTAL_ATTEMPTS=30` 与 `permanent_failures` 熔断，`on_chunk_failed` 超阈直接 `PermanentFailure(5)` 并终止重试循环；`monitor::run` 改 `Result` 三处熔断检查并 `TerminateAll` 熔断，避免 30×10 重试永挂死；`DownloadError::PermanentFailure` 新增

### ⚡ 性能

- **广播节流**：`ChunkProgress` 64KiB/50ms 聚合 + 终局补发，广播量 -15×，消除 `Lagged` 抖动
- **批量落盘**：`ResumeRecorder` 16 段/1s debounce + `flush()`，`fs_rename` -94%（8192→512）
- **合并写入**：`FileWriter` 128KiB 相邻段合并，`seek/write` 系统调用 -10%
- **并行探测**：`MultiRuntime::from_config` `FuturesUnordered` 并发 `get_file_info`，3 源 450ms→120ms -60%
- **连接池**：默认/定制 `Client` 均注入 `pool_max_idle_per_host=32/idle_timeout 90s/tcp_keepalive 60s`，复用 h2

### 📚 文档

- `Cargo.toml` 补 `description/license/repository/homepage/documentation/keywords/categories` 与 `rust-version=1.85`
- `configuration.md`/`installation.md`/`best-practices.md` 全量对齐真实 `Cargo.toml`/`usage.md`/`lane.rs` API，移除 `weight/priority/headers/full/vendored-openssl` 等伪接口
- `errors.md` 移除虚构 `1xxx` 数字码，对齐 `DownloadError` 变体
- `examples/download.rs`/`README` 外网 QQ 链路换 `proof.ovh.net`，支持 `env` 覆盖
- `installation.md` 版本表、依赖版本（`tokio 1.52/reqwest 0.13/thiserror 2/bitcode 0.6`）与 Rust 1.85 对齐


## [0.1.0] - 2024-04-23

### 🎉 主要特性

- ✅ 基于 Tokio 的高性能异步下载架构
- ✅ 动态并发控制，自动调整下载线程数
- ✅ 断点续传功能，支持下载中断后恢复
- ✅ 实时进度监控，支持自定义进度回调
- ✅ 多源下载支持，可同时从多个镜像源下载
- ✅ 全功能代理支持（HTTP/HTTPS/SOCKS5）
- ✅ 智能两级重试机制，自动处理网络抖动
- ✅ Builder 模式的简洁 API 设计
- ✅ Feature flags 模块化，可按需裁剪功能

### ✨ 新增功能

#### 核心下载能力

- 基本 HTTP/HTTPS 文件下载
- 自动检测服务器 Range 请求支持
- 动态分片下载，大文件自动分割为多个块并行下载
- 慢块自动拆分，优化下载速度
- 磁盘异步写入，避免阻塞下载线程

#### 断点续传 (`resume` feature)

- 自动保存下载进度
- 程序重启后自动恢复之前的下载进度
- 支持断点续传元数据的持久化
- 自动校验已下载内容的完整性

#### 进度监控 (`progress` feature)

- 实时获取下载进度、下载速度、已下载大小等信息
- 支持自定义进度回调函数
- 定期聚合的进度更新，避免过多的回调通知
- 支持获取每个下载块的详细状态信息

#### 多源下载 (`multi-source` feature)

- 支持同时配置多个下载源
- 自动选择最快的下载源
- 智能负载均衡，将任务分配给最快的源
- 自动检测不可用的下载源并自动故障转移
- 支持为不同的下载源设置权重和优先级

#### 代理支持 (`proxy` feature)

- 支持 HTTP/HTTPS 代理
- 支持 SOCKS5 代理
- 支持代理认证
- 自动识别系统代理环境变量

### 🔧 功能改进

- 简化默认下载器 API，降低使用门槛
- 优化多源调度器的任务分配算法
- 改进并发拆分逻辑，避免无效的分片调整
- 增加速度观测窗口和增益门控，提升下载稳定性
- 优化内存管理，大文件下载时内存占用稳定
- 自动调整块大小，根据文件大小自动选择最优的分片策略
- 下载完成后自动校验文件完整性

### 📚 文档

- 新增 README.md，包含快速开始指南和功能说明
- 新增架构文档 `docs/architecture.md`，详细介绍系统架构和工作原理
- 新增多源下载测试服务器示例 `examples/manual_multi_source_test_server.rs`
- 新增基础下载示例 `examples/download.rs`
- 新增自定义进度 UI 示例 `examples/with_custom_ui.rs`
- 新增断点续传测试示例 `examples/resume_harness.rs`

### 🧪 测试

- 完善单元测试，覆盖核心功能
- 新增集成测试，验证完整下载流程
- 新增多源下载测试场景
- 新增断点续传功能测试
- 增加回归测试用例，确保功能修改不会破坏已有功能

### 🔒 安全

- 使用安全的文件写入方式，避免数据损坏
- 验证 SSL 证书，防止中间人攻击
- 不保存任何敏感信息到磁盘

### 📦 依赖

- `tokio`: 异步运行时，版本 1.0+
- `reqwest`: HTTP 客户端，版本 0.11+
- `thiserror`: 错误处理，版本 1.0+
- `bytes`: 字节处理，版本 1.0+
- `faststr`: 高性能字符串，版本 0.2+
- `futures-util`: 异步工具，版本 0.3+
- `serde` + `bincode`: 断点续传元数据序列化（可选）

## 版本说明

### 语义化版本控制

- **主版本号（MAJOR）**：不兼容的 API 变更时增加
- **次版本号（MINOR）**：功能新增且向后兼容时增加
- **修订号（PATCH）**：向后兼容的问题修正时增加

### 版本状态

- **Alpha**：功能开发中，API 可能频繁变更，不建议生产环境使用
- **Beta**：功能基本完整，正在进行测试，API 可能有少量变更
- **Stable**：稳定版本，API 保持向后兼容，可用于生产环境

当前版本 0.4.0 已进入 Stable：`trace` 可观测性与自适应引擎调优 + `0.3.1` 的 `MissingContentLength` 流式回退语义；完全向后兼容 `0.3.x`。

## 升级指南

### 从 0.3.1 升级到 0.4.0

Minor 级向后兼容：`cargo update -p simple_downloader` 即可

- **新增 `trace` 模块** `simple_downloader::trace::{init_tracing, try_init_tracing, Env}` 为纯新增 API，未初始化时 `::tracing::*` 均为 no-op，无需迁移；二进制入口按需调用一次 `init_tracing()` 即可通过 `RUST_LOG`/`SIMPLE_DOWNLOADER_LOG` 控制
- **自适应引擎** `ConcurrencyManager` 分级阈值/退避改为内部调优，`DownloadBuilder::workers/update_interval` 调用不变；`MIN_SPLITTABLE_REMAINING 256KiB` 仅影响小文件切分，阈值以下不再分裂
- **正确性修复 13 项** 均为内部行为修正（Range 206 强校验/Early-EOF 完整性/Lagged 对账/原子预分配/Resume 自愈/Retry FIFO/sidecar 3 次重试/pool 保留等），外部 API 不破；`DEFAULT_USER_AGENT` 自动注入，无需调用方改动
- 其余 `0.3.1` API 保持不变

### 从 0.3.0 升级到 0.3.1

Patch 级向后兼容：`cargo update -p simple_downloader` 即可

- `MissingContentLength` 不再直接 `Err`，`Downloader` 自动 `streaming_download` 单流回退（`Transfer-Encoding: chunked`），`DownloadInfo::is_complete` 对 `0/N` 改为 `downloaded==0` 语义
- 其余 `0.3.0` API 保持不变

### 从 0.2.0 升级到 0.3.0

向后兼容，无破坏 API（新增 `supports_ranges`/`PermanentFailure`/`pending_bisects` 均为内部或新增分支）：`cargo update -p simple_downloader` 即可

- `LaneScheduler::best_lane` 改 `&mut self`（`MultiRuntime` 已同步），`lane_health` 语义不变；外部直接调用需 `mut`
- `MultiRuntime` 新增 `supports_ranges:bool` 公开字段，`from_config` 仍 `(u64,Self)` 不破
- `DownloadInfo::is_complete` 修正 `0>=0 true`，`0 字节` 进度回调首次即完成
- `get_file_info` 对 `501`/`4xx` 探测自动回退 `HEAD`，无需变更调用方
- 其它：`pending_bisects`/`preserve_partial`/`blacklist 30s`/`CHANNEL 4096`/`flush` 均为内部行为优化

### 从 0.1.0 升级到 0.2.0

0.2.0 完全向后兼容，无破坏性 API 变更。直接 `cargo update -p simple_downloader` 即可：

- 新增 `ResumePlan::prepare_async`（内部使用），原 `prepare` 保留兼容
- `ResumeRecorder` 增 `pending_segments/last_save` 与 `flush()`，对外不暴露破坏
- 新增 `DownloadError::PermanentFailure(String)` 变体（重试熔断），仅作为新增错误分支，现有 `match` 需补 `_` 或显式处理该分支
- 性能行为变更：`ChunkProgress` 节流（64KiB/50ms）、`save_atomic` 16段/1s 批量、`FileWriter` 128KiB 合并、`MultiRuntime` 并行探测、`Client` 连接池注入；下载结果不变，仅更少系统调用与广播
- 文档 `configuration.md/installation.md/best-practices.md` 修正为真实 API，无需代码迁移

### 从 0.0.x 升级到 0.1.0

0.1.0 是第一个公开版本，没有之前的版本，直接安装即可。

### 重大变更说明
