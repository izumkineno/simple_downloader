# Deep Interview Spec: 限速功能 (Rate Limit) — simple_downloader

## Metadata
- Interview ID: 022ae480-d96d-4aed-bfc1-6aa3d85d2594
- Rounds: 7
- Final Ambiguity Score: 20%
- Type: brownfield
- Generated: 2026-08-31T02:35:00Z
- Threshold: 0.2
- Threshold Source: default
- Initial Context Summarized: no
- Status: PASSED

## Clarity Breakdown
| Dimension | Score | Weight | Weighted |
|-----------|-------|--------|----------|
| Goal Clarity | 0.80 | 0.35 | 0.28 |
| Constraint Clarity | 0.80 | 0.25 | 0.20 |
| Success Criteria | 0.80 | 0.25 | 0.20 |
| Context Clarity | 0.80 | 0.15 | 0.12 |
| **Total Clarity** | | | **0.80** |
| **Ambiguity** | | | **20%** |

Per-component breakdown:

| Component | Goal | Constraints | Criteria | Context | Weakest |
|-----------|------|-------------|----------|---------|---------|
| 限速引擎 | 0.85 | 0.85 | 0.85 | 0.85 | - |
| Feature与API集成 | 0.90 | 0.85 | 0.80 | 0.85 | Criteria |
| 主流对标与验证 | 0.80 | 0.80 | 0.80 | 0.80 | - |

## Topology
| Component | Status | Description | Coverage / Deferral Note |
|-----------|--------|-------------|--------------------------|
| 限速引擎 | active | 令牌桶/漏桶实现，全局/分源两级限速，governor + 可配置 burst，自适应感知限速 | 已覆盖：全局+分源粒度、governor选型、burst可配置、monitor排除等待时间 |
| Feature与API集成 | active | 新增可选 Cargo feature `rate-limit`，DownloadBuilder::speed_limit + SourceConfig::with_speed_limit | 已覆盖：feature命名、零成本 opt-in、Builder与SourceConfig API形状 |
| 主流对标与验证 | active | 对标 aria2/curl --limit-rate，主流 Rust crate 选型，test_server 验证矩阵 | 已覆盖：双断言(时长+瞬时)、全局优先约束、0值校验 |

## Goal
为 `simple_downloader v0.4.0` 新增**可选的、可配置的字节级限速能力**，支持**全局限速**与**分源限速**两级（`global + per_source`），使用 `governor` 令牌桶，通过 `rate-limit` Cargo feature 零成本可选接入；在全局与分源同时配置时**全局为硬上限**，分源实际生效为 `min(配置, global剩余)`，限速与自适应并发**感知联动**（monitor排除限速等待，不误判无增益）。

## Constraints
- **Feature 约束**：新增 `rate-limit` feature，`default=[]` 保持不变；未启用时零依赖、零成本（`#[cfg(feature=\"rate-limit\")]` 隔离所有限速代码与 `governor` 依赖）；启用后 `Cargo.toml` 新增 `governor = \"0.7\"`（或兼容版本）
- **API 约束**：`DownloadBuilder::speed_limit(bytes_per_sec: u64) -> Self` 全局；`SourceConfig::with_speed_limit(bytes_per_sec: u64) -> Self` 分源；可选 `with_burst(bytes: u64)`（默认 0 硬限速，可配）；`0` 值返回 `DownloadError::InvalidArgument`（复用或新增）；单位统一 `bytes/s`，文档标注 `1 MiB/s = 1_048_576`
- **算法约束**：`governor` `Quota::per_second` + `NonZeroU32`，burst 可配（默认 0），限速点在 `chunk_run` 的 `bytes_stream` 每次 `write` 前 `limiter.until_ready().await`；分源与全局为两级令牌桶串联（先取 per_source 再取 global，或按剩余配额计算）
- **自适应约束**：`ConcurrencyManager` 不因限速误判；`monitor.rs` 的 `total_speed` 计算排除 `governor` 等待时长，或 `monitor` 维护 `throttled_duration` 并在 `decide_and_act` 时基于 `potential_speed`（未限速前）判断，避免限速场景下频繁 Probing→Stable 抖动
- **兼容约束**：与现有 `resume` / `multi-source` / `progress` / `proxy` 正交；`streaming_download`（未知大小）同样受限速；`workers` 与 `update_interval` 语义不变
- **主流对标约束**：语义对标 `curl --limit-rate`（全局）+ `aria2 --max-overall-download-limit` / `--max-download-limit`（全局+单任务），行为与 `pypdl` 的 per-source 限速一致

## Non-Goals
- 不做 `per_lane (PerSourceProxy)` 细粒度限速（留 0.6.0）
- 不做运行时动态调整 `set_speed_limit()`（留 0.6.0，当前仅 Builder 静态配置）
- 不做图形化限速展示或 CLI 包装
- 不做磁盘写入限速（仅网络字节流限速）
- 不引入 `leaky-bucket` 或自研桶之外的第二种算法

## Acceptance Criteria
- [ ] **Feature 门面**：`cargo build` 默认不拉 `governor`；`cargo build --features rate-limit` 拉取成功；`cargo clippy --all-features -- -D warnings` 零告警
- [ ] **API 可用**：`DownloadBuilder::new(url,path).speed_limit(1024*1024).download().await` 编译通过（需 `rate-limit`）；`SourceConfig::new(url).with_speed_limit(512*1024)` 在 `MultiSourceConfig` 中生效
- [ ] **burst 可配**：`with_burst(0)` 时瞬时 `total_speed ≤ 1.05×limit`；`with_burst(1 MiB)` 时允许瞬时 `≤1.2×limit` 但 10s 平均仍 `±10%` 内
- [ ] **全局限速精度（时长）**：`test_server` 10MiB 文件，`global 1MiB/s` → 总时长 `9-11s`（±10%），`cargo test --features rate-limit --test rate_limit` 通过
- [ ] **全局限速精度（瞬时）**：同上场景，`MonitorUpdate total_speed` 采样（0.5s ticker）持续 `≤1.05×limit`（burst=0 时）
- [ ] **分源限速**：`MultiSourceConfig` 2 源各 `400KiB/s`，`global 1MiB/s` 时，`test_server` 双源限速场景下总和 `≤1.05MiB/s` 且单源各自 `≤420KiB/s`，字节级一致性校验通过
- [ ] **全局优先约束**：`global 500KiB/s` + `per_source 400KiB+400KiB (=800>500)` 时，实际总和仍 `≤525KiB/s`，不报错但按剩余配额分配；`speed_limit(0)` 返回 `Err(InvalidArgument)` 
- [ ] **自适应感知**：限速 1MiB/s 时，`ConcurrencyManager` 不因限速触发 `Probing无增益→Stable` 误判，`32 workers` 场景下 `adaptive_bench` 吞吐不劣化>10%（排除限速后，逻辑同 0.4.0 基线）
- [ ] **边界**：`limit < 10KiB/s` 极小值仍可完成（不挂死），`limit = u64::MAX` 视为不限速（或文档说明上限）
- [ ] **回归**：`cargo test --all-features` 全绿（复用 0.4.0 的 9+8+5+2+...），`cargo publish --dry-run --features rate-limit` 打包包含 `governor` 且验证编译通过

## Assumptions Exposed & Resolved
| Assumption | Challenge | Resolution |
|------------|-----------|------------|
| 仅需全局限速 | Round1 问分源粒度 | 决议：全局+分源两级，per_lane 暂缓 |
| 瞬时超限可接受 | Round2 问验收标准 | 决议：两者都要（时长+瞬时），per_source和≤global |
| Feature 无需新依赖，自研即可 | Round3 问依赖 | 决议：新增 `rate-limit` + `governor 0.7`，对标主流 |
| Burst 可忽略 | Round4 Contrarian 挑战 burst 与瞬时断言冲突 | 决议：burst 可配置，默认 0 硬限速，API 暴露 with_burst |
| 限速与自适应无关 | Round6 Simplifier 挑战联动复杂度 | 决议：自适应感知限速，monitor排除等待，基于潜在速度判断 |
| 和>global 无需处理 | Round7 问边界 | 决议：全局为硬上限，per_source 受剩余约束，0→InvalidArgument |

## Technical Context
**Brownfield 现状 (v0.4.0)**：
- `src/chunk.rs:139-145` 已有 64KiB/50ms 节流广播，`src/monitor.rs:98 0.5s ticker` + `state.rs EMA 0.30` 平滑速度
- `src/downloader.rs:73 DownloadBuilder` 链式 `workers/update_interval/client_builder/resume`；`src/lane.rs:837` 已有 `max_chunks_per_lane/per_source` 容量限制
- `Cargo.toml:14-19` `default=[]` opt-in：`resume/bitcode 0.6, multi-source, proxy, progress`，新增 `rate-limit` 延续此模式
- `test_server/server.py` 支持 `--throttle 2m/16m` 限速模拟，已用于 `tests/multi_source.rs:fast/slow` 验证
- `trace.rs` 已接入 `tracing`，可观测限速等待时长

**植入点决策**：
- 主限速：`src/chunk.rs:chunk_run` 中 `bytes_stream.next().await` 后 `global_limiter + per_source_limiter.until_ready().await`，等待时长计入 `throttled_duration` 供 monitor 排除
- 备用：`src/util.rs::file_writer_task` 前亦可限，但网络层限速更贴近主流（aria2在socket层）
- 新增：`src/limiter.rs` 封装 `GlobalLimiter/PerSourceLimiter`（`governor::RateLimiter<NotKeyed, InMemoryState, QuantaClock>`），提供 `acquire(n_bytes)` 接口，`#[cfg(feature=\"rate-limit\")]`

**依赖选型依据**：
- `governor 0.7`：令牌桶成熟，支持 burst、per-second quota、async `until_ready`，crates.io 下载>2M，`tokio` 兼容；`leaky-bucket` 更轻但不支持复杂分级
- 自研桶：约80行可实现固定间隔，但需自测 burst 与 refill 精度，维护成本高，暂不选

## Ontology (Key Entities)
| Entity | Type | Fields | Relationships |
|--------|------|--------|---------------|
| RateLimiter | core domain | global_limit:u64, per_source_limit:u64, burst:u64, quota | RateLimiter throttles ChunkWorkers |
| GovernorLimiter | external system | quota:Quota, burst:NonZeroU32, clock | GovernorLimiter implements RateLimiter |
| DownloadBuilder | core domain | url, path, workers, speed_limit:Option<u64>, burst:Option<u64> | Builder creates Downloader with Limiter |
| SourceConfig | supporting | url, id, speed_limit:Option<u64> | SourceConfig belongs to MultiSourceConfig, has PerSourceLimiter |
| DownloadMonitor | supporting | ticker:0.5s, total_speed, throttled_duration | Monitor excludes throttled time, drives ConcurrencyManager |
| ConcurrencyManager | supporting | phase:Probing/Stable, potential_speed | Manager decides split based on potential_speed |
| BurstConfig | supporting | burst_bytes:u64 | BurstConfig tunes RateLimiter |

## Ontology Convergence
| Round | Entity Count | New | Changed | Stable | Stability Ratio |
|-------|-------------|-----|---------|--------|----------------|
| 1 | 3 | 3 | - | - | - |
| 2 | 2 | 1 | 0 | 1 | 50% |
| 3 | 2 | 1 | 0 | 1 | 60% |
| 4 | 1 | 1 | 0 | 2 | 70% |
| 5 | - | - | - | - | - |
| 6 | - | - | - | - | - |
| 7 | 2 | 0 | 0 | 3 | 85% |

最终 3 实体稳定（RateLimiter/DownloadBuilder/SourceConfig），新增收敛。

## Interview Transcript
<details>
<summary>Full Q&A (7 rounds)</summary>

### Round 0 — Topology confirmation — not scored
**Q:** I'm reading this as 3 components: 限速引擎, Feature与API, 主流对标与验证. Is that topology right?
**A:** Looks right, proceed
**Ambiguity:** not scored

### Round 1 — limiter-engine / Goal
**Q:** 限速粒度：仅全局 / 全局+分源 / 全局+分lane / aria2三级？
**A:** 全局 + 分源限速
**Ambiguity:** 66% (Goal 0.4, Constraints 0.3, Criteria 0.2, Context 0.5)

### Round 2 — limiter-engine / Criteria
**Q:** 全局1MiB/s下10MiB文件的可观测行为？总时长 vs 瞬时 vs 两者？
**A:** 两者都要，且 per_source叠加不超过global
**Ambiguity:** 57% (Goal 0.5, Constraints 0.3, Criteria 0.4, Context 0.6)

### Round 3 — feature-api / Constraints
**Q:** Feature命名与依赖：limit/rate-limit/throttle + governor/leaky/self？
**A:** 新增 rate-limit feature + governor
**Ambiguity:** 53%

### Round 4 — limiter-engine / Constraints (Contrarian)
**Q:** burst允许瞬时超速 vs 硬限速 vs 可配置？
**A:** 可配置 burst
**Ambiguity:** 42%

### Round 5 — feature-api / Goal
**Q:** Builder API形状：全局+分源 vs 仅全局 vs 闭包？
**A:** Builder全局 + SourceConfig分源
**Ambiguity:** 33%

### Round 6 — benchmark-validation / Constraints (Simplifier)
**Q:** 限速与自适应联动：冻结 vs 感知 vs 仅writer？
**A:** 自适应感知限速（Monitor排除等待，基于潜在速度）
**Ambiguity:** 28% → 20% after correction

### Round 7 — benchmark-validation / Criteria
**Q:** 边界：global vs per_source和超限，0值语义？
**A:** 全局优先，per_source受全局约束，0→InvalidArgument
**Ambiguity:** 20% PASSED

</details>
