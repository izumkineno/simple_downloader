# RALPLAN: Rate-Limit Feature (governor, global+per_source) — simple_downloader 0.5.0

> **Spec**: `.omc/specs/deep-interview-rate-limit.md` (7 rounds, 20% ambiguity, brownfield v0.4.0)
> **Mode**: consensus --direct (skip interview, interview already gated)
> **Threshold**: 20% (default) | **Status**: PENDING APPROVAL — Architect 5 must-fix + Critic 3CRITICAL/6MAJOR 已闭环（rev1 2026-08-31），待显式执行批准
> **Date**: 2026-08-31 | **Branch**: `feat/rate-limit` (from `fix/m4-reaudit-p0` v0.4.0)

## Requirements Summary

基于 Deep Interview 的 3 拓扑组件：
1. **限速引擎**：字节级限速，`global` + `per_source` 两级，`governor` 令牌桶，`burst` 可配（默认 0 硬限），限速与 `ConcurrencyManager` 自适应感知联动（monitor 排除等待）
2. **Feature与API**：新增 `rate-limit` Cargo feature (`default=[]` 零成本)，`DownloadBuilder::speed_limit(u64)` 全局 + `SourceConfig::with_speed_limit(u64)` 分源，`with_burst(u64)`，`0` → `InvalidArgument`
3. **主流对标与验证**：对标 `curl --limit-rate` + `aria2 --max-overall/--max-download-limit`，`test_server` 精度矩阵（时长±10% + 瞬时≤1.05×），全局为硬上限（和>global按剩余分配）

## RALPLAN-DR Summary (Short Mode)

### Principles (4)
1. **零成本可选（修正）**：未启用 `rate-limit` 时零新增依赖（`governor` optional）、零运行时开销（`#[cfg(feature="rate-limit")]` 隔离所有 `limiter.rs` + `chunk.rs` acquire 分支，`Cargo.toml:14-19` opt-in 体系）；启用后新增 1 依赖为可测精度付费，见 Option 权衡
2. **可测精度优先于灵活性**：时长+瞬时双断言是验收门禁，burst 默认 0 保证瞬时可测，复杂度为可测性让路
3. **不破坏自适应**：限速不误导 `ConcurrencyManager` 的增益判断，潜在速度与限速后速度分离
4. **全局硬上限语义**：和>global 不报错但按剩余分配，符合 aria2 行为，避免配置期校验带来的 breaking

### Decision Drivers (Top 3)
1. **验证可行性**：`test_server` 已支持 `--throttle`，能否在 `cargo test --features rate-limit` 中以 `10MiB @1MiB/s → 9-11s` 稳定复现
2. **生态成熟度**：`governor` 2M+ 下载、async `until_ready`、burst 支持 vs 自研桶的维护成本
3. **API 稳定性**：`DownloadBuilder` 链式与 `SourceConfig` 已有形状，新增 API 是否一次到位避免 0.6.0 breaking

### Viable Options

#### Option A: governor 令牌桶（推荐）
**Approach**: `Cargo.toml` 新增 `governor = { version = \"0.7\", optional = true }`，`src/limiter.rs` 封装 `RateLimiter { global: Option<Arc<RateLimiter>>, per_source: HashMap<SourceId, Arc<RateLimiter>> }`，`Quota::per_second(NonZeroU32)` + `burst: Option<NonZeroU32>`（None=0硬限，对应 `allow_burst(NonZeroU32::new(1).unwrap())` 且按 32KiB 批量消耗），`chunk_run` 中 `limiter.until_n_ready(NonZeroU32::new(n).unwrap()).await`（**禁止 `until_ready_with_jitter`**）按 `n = min(chunk.len() as u32, 65536)` 批量
**Pros**:
- 成熟生态，burst/ refill 精度已验证，async 友好，无需自测边界
- 与主流对标一致（aria2 同为令牌桶），文档可直接引用 governor 语义
- 支持分级桶串联（global + per_source 先后获取）
**Cons**:
- 新增外部依赖，`governor` 拉取 `smallvec/futures-timer/quanta` 等，编译时间 +~1s
- `NotKeyed` vs `Keyed` 选型需权衡：`InMemoryState` 单桶简单但多源需多实例

#### Option B: 自研固定间隔桶
**Approach**: `src/limiter.rs` 自研 `TokenBucket { capacity, tokens, refill_interval: Duration::from_secs_f64(bytes as f64 / limit as f64) }`，每次 `acquire(n)` 时 `sleep(refill_interval * n / 64KiB)`，burst 通过 `capacity` 控制
**Pros**:
- 零外部依赖，代码 <100 行，编译最快，逻辑完全可控
- 硬限速语义天然（无 jitter），瞬时断言最易通过
**Cons**:
- 需自证 burst/并发正确性，`tokio::time::sleep` 漂移在 `32 workers` 高并发下精度差，需大量单测
- 与 `governor` 相比无社区背书，对 `NonZeroU32` 溢出、`u64::MAX` 等边界需自处理
- 后续若需 `Keyed` 限速（per_lane）需重写

**Invalidation Rationale (why A over B)**: 自研桶虽零依赖，但 `cargo test` 精度与 `governor` 相當且需额外 2-3 天验证 burst/并发，高并发下 `sleep` 叠加导致总时长方差 >10% 的风险高于新增依赖成本；`governor` 已在 `crates.io` 被 `reqwest` 生态间接验证，且 `simple_downloader` 已有 `tracing/tracing-subscriber` 等 6 依赖，1 个新增在 `0.4.x` 依赖表可接受。

#### Option C: leaky-bucket crate
**Approach**: `leaky-bucket = \"0.5\"`，API 类似 `Bucket::new(limit).acquire(n).await`
**Pros**: 更轻量，单文件实现
**Cons**: 生态小（<100k 下载），无 `until_n_ready` 批量接口，文档与 burst 语义模糊，`tokio` 集成需 `leaky-bucket` feature 额外配置；对比 `governor` 无优势，**已排除**。

## Acceptance Criteria (10, 100% testable)

- [ ] **AC1 Feature 门面**：`cargo tree --features rate-limit | grep governor` 存在；`cargo tree | grep governor` 默认不存在；`cargo clippy --all-features -- -D warnings` 0 告警
- [ ] **AC2 API 编译**：`examples/download.rs` 加 `.speed_limit(1024*1024)` 在 `--features rate-limit` 下编译通过，无 feature 时 `#[cfg]` 不暴露该方法（或编译错提示 feature）
- [ ] **AC3 burst=0 硬限**：`test_server` 20MiB 文件，`global 2MiB/s burst=0` → 采样 `MonitorUpdate total_speed`（0.5s ticker）在 5s 窗口内 `≤2.1MiB/s`，`cargo test --features rate-limit --test rate_limit hard_limit` 通过
- [ ] **AC4 burst=1MiB 允许瞬时**：同上 `burst=1MiB` → 允许瞬时 `≤2.4MiB/s`（1.2×），但 10s 平均 `1.9-2.1MiB/s`（±5%）
- [ ] **AC5 全局限速时长**：`test_server` 10MiB，`global 1MiB/s` → `elapsed 9-11s`（±10%），`cargo test --features rate-limit --test rate_limit global_duration` 通过
- [ ] **AC6 分源限速**：`MultiSourceConfig` 2 源各 `400KiB/s` + `global 1MiB/s`，`test_server` 双源各限速 2m/16m 模拟，实测总和 `≤1.05MiB/s` 且单源 `≤420KiB/s`，字节一致性通过
- [ ] **AC7 全局硬上限**：`global 500KiB/s` + `per_source 400+400`（和800>500）→ 总和 `≤525KiB/s`（1.05×），不 `Err`；`speed_limit(0)` → `Err(DownloadError::InvalidArgument)` 且错误信息含 `speed_limit`
- [ ] **AC8a 自适应冻结**：限速启用时 `ConcurrencyManager::phase` 在 10s 内保持 `Probing→Stable` 单次切换（`consecutive_probe_no_gain` 不递增），`cargo test --features rate-limit --test rate_limit adaptive_frozen` 断言 `phase` 不抖动
- [ ] **AC8b 吞吐隔离**：同上 `adaptive_bench` 对比，未限速 vs 限速（排除限速等待后）吞吐差 <10%，`monitor` 日志含 `is_rate_limited:true`
- [ ] **AC9 边界**：`limit 5KiB/s` 极小值下载 100KiB 文件不挂死（`elapsed < 25s`），`limit u64::MAX` 视为不限速（`elapsed < 1s` 对 10MiB 小文件）
- [ ] **AC10 回归**：`cargo test --all-features` 全绿（复用 0.4.0 矩阵：basic 8 + multi 8 + resume 5 + process_resume 2 + comprehensive 9 + rate_limit 新增），`cargo publish --dry-run --features rate-limit` 66+1 文件打包验证通过

## Implementation Steps

| # | File:Line | Change | Interface | Verification |
|---|-----------|--------|-----------|--------------|
| 1 | `Cargo.toml:14-20` | 新增 `rate-limit = [\"dep:governor\"]`，`[dependencies] governor = { version = \"0.7\", optional = true }` | `cargo metadata` 可见 | `cargo tree --features rate-limit` |
| 2 | `src/limiter.rs` 新 120行 | 封装 `pub struct RateLimiter { limiter: Arc<governor::RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>> }`；`fn new(bytes_per_sec: NonZeroU32, burst: Option<NonZeroU32>) -> Self`（`None`→`allow_burst(64*1024)` 内部分片硬限）；`async fn acquire(&self, n: NonZeroU32)` 内部 `until_n_ready(n).await`；`GlobalLimiter` + `PerSourceLimiterMap: HashMap<SourceId, Arc<RateLimiter>>`；`#[cfg(test)]` 注入 `governor::clock::FakeClock` 解耦 `QuantaClock`/`tokio::time` | `RateLimiter::acquire(n).await` | 单测 `limiter::tests::burst_zero_hard` |
| 3 | `src/types.rs:266` | 新增 `DownloadError::InvalidArgument(String)` 或复用 `InvalidArgument` 若已存在，`speed_limit==0` 时 `Err` | `DownloadError::InvalidArgument` | `cargo test --test rate_limit invalid_zero` |
| 4 | `src/downloader.rs:73` | `DownloadBuilder` 新增 `speed_limit: Option<u64>, burst: Option<u64>` 字段，`fn speed_limit(mut self, bps: u64) -> Self` + `fn with_burst`，`#[cfg(feature=\"rate-limit\")]` 条件编译，无 feature 时方法不存在（或 `compile_error!`） | `Builder::speed_limit` | `cargo check --features rate-limit` |
| 5 | `src/lane.rs:90,148` | `SourceConfig` 新增 `speed_limit: Option<NonZeroU64>`，`fn with_speed_limit(bps: u64) -> Result<Self, InvalidArgument>`（`0`→`Err`），`MultiSourceConfig` 新增 `global_speed_limit: Option<NonZeroU64>`（对应 `lane.rs:159` 附近，`with_global_speed_limit`），`MultiRuntime::from_config` 为每个有效源创建 `PerSourceLimiter` 并持有 `global_limiter: Option<Arc<RateLimiter>>` 串联 | `SourceConfig::with_speed_limit` | `multi_source` 集成测试 |
| 6 | `src/chunk.rs:69-96,148` | `chunk_run` 签名新增 `global_limiter: Option<Arc<RateLimiter>>, per_source_limiter: Option<Arc<RateLimiter>>`，在 `chunk.rs:234-252 bytes_stream.next()` 后 `let n = chunk.len() as u32; let nz = NonZeroU32::new(n.min(65536)).unwrap(); if let Some(p) = per_source { p.acquire(nz).await } if let Some(g) = global { g.acquire(nz).await }`（**32-64KiB 批量**，避免 1-4KiB 小片 16× acquire），`streaming_download` 同理在 `util.rs::streaming` 分支加同款限速；不记录 per-worker sum，改为 `monitor` 单调 `throttled_wall: Instant` 统计 | `chunk_run(..., limiter)` | `rate_limit` 精度测试 |
| 7 | `src/monitor.rs:32,98,165` | `DownloadMonitor` 新增 `is_rate_limited: bool`（`global_limiter.is_some()`），`monitor.rs:356 decide_and_act` 入口 `if self.is_rate_limited { return; }` **冻结自适应**（替代 `throttled_duration` sum 模型，避免 32×墙钟重叠高估 `src/concurrency.rs:268 BANDWIDTH_PROBE_FACTOR`）；`DownloadState::total_speed` 仍按墙钟 EMA，但并发决策跳过，满足 AC8 可测性 | `monitor.throttled_duration` | `adaptive_bench` 对比 |
| 8 | `src/lib.rs:14` | `#[cfg(feature=\"rate-limit\")] pub mod limiter;`，重导出 `limiter::RateLimiter` 可选 | `simple_downloader::limiter` | `cargo doc --features rate-limit` |
| 9 | `docs/usage.md:25` | 新增限速小节，示例 `simple_downloader = { version=\"0.4\", features=[\"rate-limit\"] }` + `builder.speed_limit` | 文档 | `cargo test --doc` |
| 10 | `tests/rate_limit.rs` 新 250行 | 4用例：`global_duration`, `hard_limit`, `per_source`, `global_hard_limit`，复用 `test_server_harness::{TestServerFile,RunningTestServer}`，`#[cfg(feature=\"rate-limit\")]` | `cargo test --features rate-limit --test rate_limit` | 本地 `test_server` 精度 |
| 11 | `examples/with_rate_limit.rs` 新 | 演示 `global 1MiB/s` 下载 `proof.ovh.net` 或本地 `test_server`，打印 `MonitorUpdate` 速度 | `cargo run --features rate-limit,progress --example with_rate_limit` | 手工观察 |

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| governor `InMemoryState` Mutex 32 workers 串行 + 小片高频 acquire | 高：`src/chunk.rs:235` 每 `bytes_stream.next()` 1次 `until_n_ready`，1-4KiB 小片 64/s→256/s，`NonZeroU32` 创建开销 | 批量 32-64KiB：`let n = min(chunk.len(), 65536) as u32; limiter.until_n_ready(NonZeroU32::new(n).unwrap()).await`，**禁 jitter**，`Arc<RateLimiter>` 单例全局共享 |
| governor 等待阻塞 `tasks` 空转误判 Lagged | 高：`src/monitor.rs:168 Lagged` 对账 `tasks.empty && chunks non-empty` | `is_rate_limited` 分支：`if self.is_rate_limited { tracing::debug!("rate-limited, skip Lagged reconcile"); return; }` 不触发 `TerminateAll` |
| burst 与瞬时断言冲突（AC3 vs AC4） | 中 | burst 默认 0 硬限，AC3 用 0，AC4 显式 `with_burst(1MiB)` 隔离 |
| per_source 和>global 时并发分配不均 | 中 | `acquire` 串联：先 per_source 后 global，global 剩余不足时 per_source 排队，保证全局硬上限 |
| 极小值 `5KiB/s` 导致 `Quota::per_second` 的 `NonZeroU32` 精度与 `governor` 最小 1 byte | 低 | `limit<1024` 时按 `1KiB` 兜底或文档说明最小 1KiB/s，`InvalidArgument` 提示 |
| `u64` bps 转 `NonZeroU32` 溢出（>4GiB/s） | 低 | `bps as u32` 前 `clamp(1, u32::MAX)` 并文档说明上限 4GiB/s，超限视为不限速 |
| `test_server` 本地限速与 governor 叠加导致总时长方差>10% | 中 | 测试用例中 `test_server` 限速设为 `16m` 远高于 governor limit，确保瓶颈在 governor |
| `streaming_download`（`downloader.rs:590` 未知大小分支）遗漏限速 | 中：`streaming_download` 单流 `bytes_stream` 不经 `chunk_run` | 同 `chunk.rs` 同款 `global.acquire` 植入 `src/downloader.rs:streaming_download` 循环内 |

## Verification Steps

1. `cargo check --features rate-limit` + `cargo tree --features rate-limit | grep governor` (AC1) + `cargo clippy --all-features -- -D warnings` (AC1)
2. `cargo test --features rate-limit --test rate_limit -- --nocapture` (AC3 burst=0, AC4 burst=1MiB, AC5 全局时长, AC6 分源, AC7 硬上限, AC9 极小值) — 6用例 `30s` 内全绿
3. `cargo test --features rate-limit --test rate_limit adaptive_frozen` (AC8a) + `adaptive_bench --features rate-limit` 对比 (AC8b)
4. `cargo test --all-features` 回归 (AC10) — 复用 0.4.0 矩阵 13+ 套
4. `cargo run --features rate-limit,progress --example with_rate_limit` 手工：10MiB @1MiB/s 观察 `MonitorUpdate` 速度平稳
5. `cargo publish --dry-run --registry crates-io --features rate-limit --allow-dirty` 打包验证 (67 files)
6. `adaptive_bench` 对比：限速启用 vs 未启用（同 `workers 32`），排除 throttled 后吞吐差 <10% (AC8)

## ADR (Draft — pending Architect/Critic)

- **Decision**: 选用 `governor 0.7` 令牌桶，新增 `rate-limit` feature，全局+分源两级，burst 可配默认 0，植入 `chunk_run`，monitor 感知限速
- **Drivers**: 验证可行性、生态成熟度、API 稳定性（见 RALPLAN-DR）
- **Alternatives**: 自研固定间隔、leaky-bucket（均已 invalidation）
- **Why chosen**: 见 Option A Pros + invalidation rationale
- **Consequences**: + 成熟精度/文档可追溯，- 新增 1 依赖编译+1s；+ 自适应冻结 1 行实现（`monitor.rs:356`）规避 32×墙钟求和的 30×高估风险
- **Follow-ups**: 0.6.0 考虑 `per_lane`、运行时 `set_speed_limit`、动态 burst 调整

## Changelog
- 2026-08-31 draft by Planner (from deep-interview-rate-limit spec)
- 2026-08-31 rev1 by Planner: 闭环 Architect 5 must-fix (burst Option<NonZeroU32>、批量32KiB、is_rate_limited冻结、MultiSourceConfig global、FakeClock) + Critic 6 MAJOR (streaming遗漏、AC8拆分、Verification逐AC映射、file:line校准)
