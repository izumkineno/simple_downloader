# simple_downloader — Project Profile（T1 全源码盘点）

> 产出：project-analyst | 锚点 v0.3.1 / Cargo.toml:1-60 / edition 2024 / MSRV 1.85 | 全量读取：src/*.rs(13) + Cargo.toml + README.md + docs/*.md(10) + examples/*.rs(5) + tests/*.rs(4) + test_server/server.py | 复用于 t4 差距矩阵

---

## 1. 目录结构

```
E:\Code\simple_downloader\
├─ Cargo.toml                 # 包元信息、features、deps
├─ README.md                 # 8大已落地能力 + TODO 4类 + Mermaid 架构图
├─ docs/
│  ├─ architecture.md        # 运行时权威架构、通道表、时序图
│  ├─ usage.md               # 6 场景调用指南、feature 选型、Builder 全景
│  ├─ configuration.md       # workers/update_interval/client_builder/resume
│  ├─ errors.md              # 7 变体错误表
│  ├─ best-practices.md / faq.md / installation.md
│  └─ fix-plan*.md(4)        # M1-M3 修复计划
├─ src/
│  ├─ lib.rs (198)           # Feature 门面、公共导出、文档
│  ├─ downloader.rs (882)    # Downloader/DownloadBuilder、run_internal、orchestrate、streaming_download
│  ├─ monitor.rs (517)       # DownloadMonitor 控制环
│  ├─ state.rs (163)         # ChunkState/DownloadState EMA
│  ├─ concurrency.rs (809)   # ConcurrencyManager 两阶段三态
│  ├─ retry.rs (272)         # RetryHandler 两级队列
│  ├─ chunk.rs (370)         # chunk_run 单 Range 执行
│  ├─ util.rs (406)          # get_file_info、parse_content_range、file_writer_task
│  ├─ resume.rs (552)        # ResumeMetadata/Plan/Recorder、64KiB ledger
│  ├─ lane.rs (837)          # MultiSourceConfig/LaneScheduler/MultiRuntime
│  ├─ types.rs (266)         # DownloadError/DownloadCmd/DownloadInfo
│  └─ trace.rs (155)         # tracing 门面 Env/Filter
├─ examples/
│  ├─ download.rs            # 基础单源
│  ├─ with_custom_ui.rs      # indicatif MultiProgress
│  ├─ resume_harness.rs      # 单/多源子进程恢复
│  ├─ test_server_smart_schedule.rs
│  └─ manual_multi_source_test_server.rs # 500MiB 双源限速观察
├─ tests/
│  ├─ resume.rs              # 元数据/损坏/缺文件/单多源恢复/禁用
│  ├─ process_resume.rs      # 真实子进程 kill/中断恢复
│  ├─ multi_source.rs        # fast/slow 三源/无效源跳过
│  └─ concurrency.rs         # 并发决策单测
└─ test_server/
   └─ server.py              # 可控限速 HTTP Range 服务
```
源码合计 ~5.5k 行，MSRV 1.85 较新。

---

## 2. Cargo Feature

`Cargo.toml:14-20` 真实定义（以此为准，非旧文档 default 全开）：

| Feature | 默认 | 作用 | 依赖 | 新增 API |
|---|:---:|---|---|---|
| _(none)_ | ✅ | 基础单源多线程下载 | 无 | `Downloader::builder(url,path).download()` |
| `resume` | ❌ | 断点续传 sidecar `*.download.bitcode` | `bitcode@0.6` | `DownloadBuilder::resume()/Downloader::with_resume()/ResumeMetadata/metadata_path_for/hash_bytes/DEFAULT_SEGMENT_SIZE` |
| `progress` | ❌ | 进度事件与回调 | 无 | `DownloadInfo/Downloader::run()/DownloadBuilder::run()` |
| `multi-source` | ❌ | 多源 lane 调度 | 无 | `MultiSourceConfig/SourceConfig/LaneModel/LaneHealth/LaneScheduler/Downloader::new_multi()` |
| `proxy` | ❌ | 代理 lane 建模 | 隐含 multi-source | `ProxyConfig/SourceConfig::with_proxies()/LaneModel::PerSourceProxy` |

推荐组合：`default-features=false` 最轻量；常用 `resume+progress`；全功能四开。`tokio 1.52(rt-multi-thread/macros/fs/io-util/signal) + reqwest 0.13(stream) + bytes + futures-util + faststr + tracing/tracing-subscriber`。

---

## 3. 8大已落地能力（对齐 README Features 1-8）

### 能力1 — 消息驱动的异步架构
`Downloader(启动编排)→DownloadMonitor(控制循环持有DownloadState)→chunk_run(工蜂)→file_writer_task(独立写入)` 经 `broadcast(4096) DownloadInfo/broadcast DownloadCmd/mpsc(128) WriteFile` 解耦，`biased select!` 驱动。

### 能力2 — 自适应下载引擎（核心差异化）
单流冷启动→`Probing` 带宽探测(`avg>prev_max*1.1` 扩容最大剩余块，1 次无增益切 Stable)→`Stable`(仅 `avg<recent*0.8 && active<max && split_is_useful` 切最慢块)→`Observing(2-5样本~1s)→Evaluating(双重增益门 best>pre*1.05 && best>recent*0.95)`，`recent_best` 0.97/0.98 衰减，`split_is_useful: remaining>=256KiB && estimated>adaptive(0.8-3s,单块×0.4) && avg>0`，`MIN_CHUNK 10KiB→remaining>=20KiB` 可分。

### 能力3 — 两级重试
`RetryHandler: retry_queue 10次×2s → delayed 10s → MAX_TOTAL 30 永久失败(TerminateAll)`，到期项重置 `failure_time-=2s, attempts=0`，`pop_ready_chunk` 扫描首个就绪防队头阻塞，`preserve_partial` 防进度回落，`Lagged/Closed` 分别计数/退出。

### 能力4 — 精准实时监控（progress）
中心化 `DownloadState` 聚合，`DownloadMonitor` 每 `0.5s` 广播 `MonitorUpdate{total_size,total_downloaded,total_speed,chunk_details:(id,size,downloaded,speed,status:u8 0-5)}`，速度 EMA 0.30 平滑，`ChunkProgress/Bisected/Complete/Failed/StatusChanged` 事件全量，快捷 `progress_percent/speed_mbps/is_complete`。

### 能力5 — 高效安全文件 I/O
独立 `file_writer_task` 有界 `mpsc128` 背压，预分配 `set_len`(原子化 `create_dir_all`→set_len，resume 仅长度不一致才 truncate，空文件显式清零)，`128KiB` 相邻段合并减少 seek，错误全链路回传 (`seek/write/flush/record` 均 `DownloadError::Io`)。

### 能力6 — 高兼容文件信息探测
`get_file_info`: `HEAD(Content-Length+Accept-Ranges:bytes)`→失败回退 `GET Range:bytes=0-0` 解析 `Content-Range` 总大小→最后 `Content-Length`，`206+CR` 为金标准，`206无CR` 仍判支持 Range，大小写兼容 `bytes */total`，返回 `(file_size, support_ranges)` 驱动 `workers<1MiB||!Range →1` 降级。

### 能力7 — 断点续传（resume）
固定 `64KiB` segment ledger + `bitcode` 持久化 `output.download.bitcode (version=1)`，启动立即落盘保证 <64KiB 中断可恢复，FNV-1a 段哈希，`verify_against_file` 逐段 seek+hash 比对失效丢弃，按覆盖恢复(verified/remaining 合并)非旧拓扑，单/多源统一 `ResumePlan::prepare_async(spawn_blocking)`，`sidecar存在但文件缺失→ResumeTargetMissing fail-stop`，损坏/版本/大小自愈重建，`record_write` 增量 `covered_ranges` 段全覆盖后读盘哈希，`16段/1s` 批量 flush，成功后 3 次重试清理 sidecar。

### 能力8 — 多源/多代理基础
`MultiSourceConfig/SourceConfig/ProxyConfig/LaneModel(PerSource/PerSourceProxy)`，`MultiRuntime::from_config` 并行探测各源，跳过不可用/非Range/大小不一致源，全不可用 `NoAvailableSources`，每源 `64KiB` 真实 `probe_speed` 测量后按速度降序建 `LaneScheduler(max_workers/max_per_lane/max_per_source)`，`LaneEntry{active, consecutive 3→Blacklisted 30s decay}`，`best_lane` 健康优先黑名单 fallback，`claim_request_builder/assign/release/record_*` 维护 `lane_bindings`。

---

## 4. TODO（待实现/待完善，对齐 README 待实现清单）

| 域 | 剩余项 | 证据 |
|---|---|---|
| 断点续传 | 元数据 schema 演进策略、可观测性(复用/失效 segment 事件) | README 1.x 两项未勾 |
| 多源调度 | 运行时动态评分(响应时间/吞吐/失败率择优)、真实代理端到端矩阵 | README 2.x 两项 |
| 限速 | 全局/分源/分 lane 限速器、节流逻辑 | README 3.x 两项 |
| 配置/队列/UI | 运行时动态调并发/重试策略、任务队列(暂停/恢复/取消/查询)、稳定 UI 契约(DownloadInfo 语义与兼容) | README 4.x 三项 |
| 隐含债 | 端到端校验(ETag/全文件 hash)、Feature 可重入配置化、裸 `status:u8→enum`、FNV 强度、流式 `total=0` 退化 | 代码审计 |

---

## 5. 架构（运行时权威见 docs/architecture.md）

### 5.1 启动链路（downloader.rs:396-830）
```
Downloader::run_internal
 ├─ 单源：client.build→get_file_info→ResumePlan::prepare_async→file_writer_task[*]→spawn(progress_handler)→orchestrate_downloads
 └─ 多源：MultiRuntime::from_config(并行探测)→同上共用 ResumePlan/writer
orchestrate_downloads: workers 生效(!Range||1||<1MiB→1) → split_resume_ranges→Monitor::new_with_completed→monitor.run
streaming_download(未知 CL): 单流 mpsc 顺序写，无预分配/Range
```

### 5.2 协作角色（见第3章 8 能力分解）

### 5.3 通道与协议（types.rs:88-191）
`DownloadInfo broadcast4096`(6 变体) / `BisectDownload broadcast` / `TerminateAll broadcast` / `WriteFile mpsc128`；`DownloadInfo` 快捷方法 `progress_percent/speed_mbps/downloaded_bytes/total_bytes/is_complete`；`trace.rs` 库不自动 init，`Env::infer` 分级，`RUST_LOG>S_SIMPLE_DOWNLOADER_LOG>env default`，4 入口幂等 `set_global_default`。

---

## 6. API 形态（以 docs/usage.md 6 场景为准）

| 场景 | 最小调用 |
|---|---|
| 基础单源 | `Downloader::builder(url,path).workers(16).download().await` |
| 断点续传 | `features=["resume"]` 默认自动恢复；`builder.resume(false)/.with_resume(false)` 禁用；`metadata_path_for/hash_bytes` 工具 |
| 进度监控 | `features=["progress"]` `builder.workers(16).run(\|total, mut rx\| async move { while Ok(info)=rx.recv().await { match info { MonitorUpdate{..}=>info.progress_percent() } } }).await` |
| 多源 | `features=["multi-source"]` `MultiSourceConfig::new(path,32,0.5).with_sources(vec![SourceConfig::new(url).with_id("m1")]).with_lane_model(PerSource).with_max_chunks_per_lane(1)` + `Downloader::new_multi(cfg, Default::default).download().await` |
| 代理 | `features=["proxy"]` `SourceConfig::new(url).with_proxies(vec![ProxyConfig::http("http://proxy:8080")?])` + `LaneModel::PerSourceProxy` |
| 自定义 Client | `.client_builder(\|\| ClientBuilder::new().timeout(120s).connect_timeout(10s))` 每次探测 `build()` 产生独立 `Client` |

`DownloadBuilder` 流畅：`workers/update_interval/client_builder/resume/build/download/run`；`Downloader::new(url,path,workers,interval,client_builder)` 低层保留。

---

## 7. 已验证能力（repo-native 真实验证矩阵）

| 验证 | 命令/入口 | 覆盖 |
|---|---|---|
| 断点元数据/自愈 | `cargo test --features resume,multi-source --test resume` | 哈希恢复、损坏 segment、缺文件 fail-stop、单/多源显式禁用、版本/大小/损坏自愈重建 |
| 进程级中断恢复 | `cargo test --features resume,multi-source --test process_resume -- --nocapture --test-threads=1` | 单源控制台中断、多源 kill/崩溃子进程恢复（`examples/resume_harness` 双模式） |
| 多源多限速 | `cargo test --test multi_source -- --nocapture` + `tests/multi_source.rs` | 本地 `test_server` fast=16m/slow=2m、三源异构、invalid+valid 跳过 |
| 并发决策 | `tests/concurrency` + `concurrency.rs`7单测 | probing 无增益不分、stable 无机械补位、观察窗、双重增益门通过/失败、连续无增益切 Stable |
| 手工观察 | `cargo run --features multi-source,progress --example manual_multi_source_test_server` | 500MiB 临时文件、双源限速、实时总进度/速度/stats、字节级一致性校验、两源均参与 Range |
| 智能调度观察 | `cargo run --features progress --example test_server_smart_schedule` | 本地限速下的并发决策路径 |
| Range 严格性 | `src/chunk.rs` 校验门 | 206 必带 CR 且一致、200 仅允 0-*、其他状态 Failed、Early-EOF 门 |
| 通道健壮性 | `monitor.rs/ chunk.rs` | Lagged 计数与对账、pending_bisects/lane 容量阻塞、4096/128 背压 |

未覆盖缺口：性能 bench、模糊测试、长稳、真实代理链路矩阵、全文件 ETag/hash 校验。

---
*end — project-profile.md for t4 gap matrix*
