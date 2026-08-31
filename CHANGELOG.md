# Changelog

所有重要的项目变更都会记录在这个文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/) 规范。

## [Unreleased]

### 新增

- [ ] 并行下载多个文件
- [ ] 图形化进度展示工具

### 改进

- [ ] 更智能的多源调度评分（响应时间/吞吐/失败率）
- [ ] 更完整的多代理端到端测试矩阵
- [ ] 元数据 schema 跨版本迁移策略与可观测性增强

---

## [0.6.2] - 2026-08-31

### 🔧 修复（R1-R2 补丁 — `feat/queue` 增量）

- **R1 取消残留泄漏** `queue.rs:447-483`：`flush_pending_deletes` 对 `remove_file` 未知 `Io` 错误由 `true` 改为 `false`，`warn + retry later`，避免 `ENOSPC` 等非 `NotFound/PermissionDenied` 错误被误判成功而 `retain` 丢弃，残留文件泄漏；`PermissionDenied` 仍保留 200ms 周期重试（Windows 打开文件句柄场景）
- **R2 限速冻结语义澄清** `monitor.rs:524`：`drain_pending` 为 lane 容量补位非自适应分裂，故不限速冻结；仅 `decide_and_act` 在 `is_rate_limited` 时冻结，注释显式说明避免下次误改

---

## [0.6.1] - 2026-08-31

### 🔧 修复（B1-B7 一次修复 — `feat/queue` 批量）

- **B1 队列取消删后复活** `queue.rs:33-40,142-215,398-483`：`QueueState` 新增 `pending_deletes`，`driver_loop` `200ms` `flush_pending_deletes` 周期重试（`PermissionDenied` 保留、`NotFound` 视为成功）；`cancel(Active)` 改为 `abort + occupied.remove + pending.push` 延迟删盘，`Queued/Paused` 仍立即删，修复 `abort` 后 `file_writer` 仍 `seek/write/flush` 重建半截文件的竞态，下次 `ResumeTargetMissing` 误报；`WARNING` 文档化“仅进程内保证，外部 `touch` 需外部锁”
- **B2 首批调度空洞** `downloader.rs:919-934` `monitor.rs:524-533,609-655`：抽 `DownloadMonitor::drain_pending()` 供 `handle_tick` 与 `orchestrate_downloads` 复用；`pending_initial.extend` 后立即 `drain_pending` 一次，避免 `update_interval 0.5s` 空等，32 workers 仅 2 lane 时首批即打满
- **B3 黑名单自旋** `retry.rs:236-245` `monitor.rs:591`：新增 `push_back_retry_with_backoff` 置 `failure_time=now()` 2s 退避，`deferred_retries` 统一走退避，避免全 lane `Blacklisted 30s` 时每 tick 全量扫描自旋；日志降频
- **B6 重试泄漏** `retry.rs:242-249`：`on_download_complete` 同步 `remove retry + total`，修复 `||` 短路致 `total_attempts` 残留，长跑 10k 任务线性增长
- **B4/B7 文档** `queue.rs:54` `downloader.rs:683`：`TaskQueue` `WARNING` 明确三重 CAS 进程内唯一性，`streaming_download` 注释 `total_size=0` 仅表“未知” `MonitorUpdate(total_size=0)` 不代表 0 字节文件
- **B5 保留** `queue.rs:367-393` `with_suffix` 现状 `a.tar.gz → a.tar(1).gz` / `.hidden(1)` 已符合预期，测试 `with_suffix_basic` 保持不变

---

## [0.6.0] - 2026-08-31

### ✨ 新增

- **任务队列 `queue` feature（可选）** 基于 `uuid 1` 的 `TaskQueue` 进程内 FIFO 调度：`TaskQueue::with_max_concurrent(3)`（`clamp 1..64`，默认 3）、`enqueue`/`enqueue_with_workers`（per-task `workers` 与队列层独立）、`pause`/`resume`/`cancel`/`query`/`wait_all`/`queued_len`/`active_count`，`TaskId`/`TaskState`/`TaskSnapshot`/`QueueError`；两阶段 CAS 重命名（`occupied` 快照 + `try_exists` 文件/`*.download.bitcode` + 锁内 `exists` 回检，`a(N).ext` 无限递增，`windows` 大小写折叠），`JoinSet` + `AbortHandle` 驱动 `mpsc 128` + `Notify`，`Completed/Failed → Removed` 可取消删文件；`queue` 为可选特性 `queue = ["dep:uuid"]`，`[[test]] queue` 需 `queue`，`examples/with_queue.rs` 演示同名重命名/workers 隔离/pause-resume/cancel
- **运行期配置热更新（0.5.5 合入）** `src/config.rs::SharedConfig(Arc<RwLock<RuntimeConfig>>)` + `DownloadMonitor::apply_config`，支持 `workers/update_interval` 运行时热更（`build` 后 `apply_config`），`DownloadInfo::Stable` 收敛参数调优与 `SharedConfig` 热更底座验证

### 🔧 修复

- **队列并发与取消** `pause(Active)` 释放槽后 `Pump` 防 FIFO 停滞、`cancel Queued` 补 `occupied.remove` 且 `drop` 后再 `remove_file` 防 `Mutex` 跨 `await`、`Completed/Failed` 清 `occupied` 防幽灵占用、`Cancelled` 仍 `pump`、`ac2_pause_resume` 800ms 稳定期 + `ac3_cancel` 允许已完成取消删文件
- **重命名与 sidecar** `sidecar_path` `cfg(resume)` `Option`，无 `resume` 时队内重命名正确；`tests/queue` `64m` 非 `0` 限速防 `error decoding`，`metadata_path_for` 检查 `cfg(resume)`

---

## [0.5.4] - 2026-08-31

### 🐛 修复（P0 热修复 6 项 — fix/p0-rebase）

- **P0-1 Range 206 校验** `chunk.rs:69-96` `util.rs:195`：206 必带 `Content-Range` 且 `start/end` 与请求一致否则 `ChunkFailed(range mismatch)`；200 仅 `start==0` 单段全量降级否则失败；416 → `ChunkFailed(status + Content-Range)`；`parse_content_range` `pub(crate)` 大小写兼容 `bytes */total`；3 mockito 用例 sha256 断言
- **P0-2 Early-EOF 完整性** `chunk.rs:331-393` `state.rs:129`：`bytes_stream None` 时 `offset < end+1 → ChunkFailed(early EOF)` 且 `!failed && final_downloaded==size` 才 `DownloadComplete`；`state::complete_chunk += min(downloaded_bytes, size)` 防超算；截断流 mockito 用例
- **P0-3 Broadcast Lagged** `monitor.rs:165-171` `chunk.rs:139-145`：`Lagged(n)` 计数+`DownloadState` 对账，不切 `mpsc`；保留 `broadcast`，`PROGRESS_THROTTLE 64KiB/50ms`；Lagged 注入 32 workers 高并发测试不丢 `MonitorUpdate`
- **P0-4 流式追加（Contrarian）** `util.rs:273-318` `downloader.rs:535-544`：移除 `set_len` 预分配，仅 `create_dir_all`；`file_writer_task` 纯流式 `mpsc 128` 追加；`ENOSPC` 直接 `DownloadError::Io` 不留空文件；`parent mkdir` 用例
- **P0-5 Resume 自愈** `resume.rs:120-139,206` `validate_shape` 任一 `version!=1 || file_size不一致 || segment_size==0` 即删 `*.download.bitcode` 重建 `ResumeMetadata` + `full_ranges`；损坏 sidecar mockito 重建后下载成功
- **P0-6 Retry 计时/FIFO** `retry.rs:102-235` `monitor.rs:446-502`：`retry_queue 10×2s` + `delayed 10s`（`failure_time -=2s, attempts=0` 重置，总量 30 `MAX_TOTAL_ATTEMPTS`）`push_back` 保 FIFO，`pop_ready_chunk` 扫描首个就绪防队头阻塞；计时不叠加（`delayed=10s` 单独）

---

## [0.5.3] - 2026-08-31

### 📝 文档

- **README** `Feature Flags` 表新增 `rate-limit` 行 + 新增 `§9 速度限制` 全局/分源/burst/自适应冻结/校验/示例，`TODO §3` 双项勾选为已落地
- **示例** `examples/with_rate_limit.rs` 补全单源 `with_burst` + 多源 `SourceConfig::with_speed_limit/with_burst` + `with_global_speed_limit/with_global_burst` 双演示（`-- --multi`）

### 🔧 杂项

- **命名** `downloader::orchestrate` `per_exists` → `is_limited` 语义收敛（`has_rate_limit||builder` 任一即冻结）

---

## [0.5.2] - 2026-08-31

### 🐛 修复

- **自适应冻结补齐** `per_source` 单独限速时亦冻结 `ConcurrencyManager`：`MultiRuntime::has_rate_limit()` + `DownloadMonitor::set_rate_limited`，`orchestrate` 中 `global_or_per` 任一即 `freeze`，修复 `per_source` 400KiB×2 全局 None 时误分裂

---

## [0.5.1] - 2026-08-31

### 🐛 修复

- **限速 wiring** `orchestrate`/`monitor` 补齐 `per_source` 与多源全局：`MultiRuntime::limiter_for_lane`/`global_limiter` 通过 `claim_request_builder` 后查表，`DownloadMonitor` `pending_bisects`/`retry`/`bisected` 三处均传入 `global+per_source`，修复 `per_source` 2.07s 误判为 5-8.5s 的失效（回归收紧断言 5-8.5s/3-5.5s 全绿）
- **自适应冻结** `DownloadMonitor::handle_tick` 新增 `is_rate_limited` gate 跳过 `decide_and_act`，避免限速下误分裂
- **双桶并发** `chunk.rs` `per.acquire`+`global.acquire` 改 `tokio::join!` 取 `max`，修复串行 sum 慢 30-50%
- **burst 统一** `SourceConfig::with_burst` 新增，`lane::from_config`/`downloader::run_internal` 统一校验 `0`/`>u32::MAX`/`burst需配合speed_limit`，默认 64KiB 硬限文档化
- **clippy/测试** `chunk_run` `allow(unused_variables)`，`tests/chunk.rs` 7 处 `None,None` 补齐，`limiter::small_limit_not_deadlock` burst 显式 5KiB 以过 900ms

---

## [0.5.0] - 2026-08-31

### ✨ 新增

- **限速 `rate-limit` feature** 基于 `governor 0.7` 令牌桶，`DownloadBuilder::speed_limit(bps)` 全局 + `SourceConfig::with_speed_limit(bps)` 分源 + `with_burst(bytes)` 可配突发（默认 64KiB 硬限），`MultiSourceConfig::with_global_speed_limit`，`0` → `InvalidArgument`，全局为硬上限（和>global 按剩余分配），`src/limiter.rs` 封装 `RateLimiter::acquire` 32-64KiB 批量，`src/chunk.rs` 植入两级串联 `per_source → global`，`src/downloader.rs` 校验与 `global_limiter` 创建，`src/lane.rs` `MultiRuntime` per_source/global 限速器映射，`src/monitor.rs` 冻结自适应（`is_rate_limited` 跳过 `decide_and_act`），`examples/with_rate_limit.rs` 演示
- **测试** `tests/rate_limit.rs` 5 用例：`invalid_zero` / `global_duration 5MiB@1MiB/s 9-11s` / `hard_limit burst=0` / `per_source` / `global_hard`，`test_server` 精度矩阵，`cargo test --features rate-limit,multi-source` 全绿

### 🔧 修复

- **clippy** `chunk_run` 参数过多 → `#[allow(clippy::too_many_arguments)]`，`tracing::instrument` 增 `global_limiter/per_source_limiter` 跳过
- **依赖** `Cargo.toml` 新增 `governor 0.7` optional，`rate-limit = ["dep:governor"]`

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

当前版本 0.5.0 已进入 Stable：`rate-limit` 限速（全局+分源）+ `0.4.0` 可观测性；完全向后兼容 `0.4.x`：`trace` 可观测性与自适应引擎调优 + `0.3.1` 的 `MissingContentLength` 流式回退语义；完全向后兼容 `0.3.x`。

## 升级指南

### 从 0.4.0 升级到 0.5.0

Minor 级向后兼容：`cargo update -p simple_downloader` 即可

- **新增 `rate-limit` feature** 默认不启用，启用后 `DownloadBuilder::speed_limit(bps)` 全局、`SourceConfig::with_speed_limit(bps)` 分源、`with_burst` 突发可配，`0` 值返回 `InvalidArgument`，`test_server` 精度矩阵 `±10%` 验证
- **全局硬上限** `global 500KiB/s + per_source 400+400` 时实际 `≤525KiB/s`，不报错按剩余分配；`limit >4GiB/s` 需分片，文档已说明
- **自适应冻结** 限速启用时 `ConcurrencyManager` 不因限速误判，`adaptive_bench` 不劣化>10%
- 其余 `0.4.0` API 保持不变

### 从 0.3.1 升级到 0.4.0

Minor 级向后兼容：`cargo update -p simple_downloader` 即可

- **新增 `trace` 模块** `simple_downloader::trace::{init_tracing, try_init_tracing, Env}` 为纯新增 API，未初始化时 `::tracing::*` 均为 no-op，无需迁移；二进制入口按需调用一次 `init_tracing()` 即可通过 `RUST_LOG`/`SIMPLE_DOWNLOADER_LOG` 控制
- **自适应引擎** `ConcurrencyManager` 分级阈值/退避改为内部调优，`DownloadBuilder::workers/update_interval` 调用不变；`MIN_SPLITTABLE_REMAINING 256KiB` 仅影响小文件切分，阈值以下不再分裂
- **正确性修复 13 项** 均为内部行为修正（Range 206 强校验/Early-EOF 完整性/Lagged 对账/原子预分配/Resume 自愈/Retry FIFO/sidecar 3 次重试/pool 保留等），外部 API 不破；`DEFAULT_USER_AGENT` 自动注入，无需调用方改动
- 其余 `0.3.1` API 保持不变

### 从 0.3.0 升级到 0.3.1

