# M2 一致性修复详细设计（P1 8项）

> 承接 Phase-0 总纲（`docs/fix-plan-outline.md` 待补链），对应审计 `simple-downloader-audit t1/t2/t3` 的 P1 一致性类缺陷。M2 在 M1 热修复（P0 6项）完成后实施，目标：消除静默不一致、流式旁路丢失、队列空转与状态重叠，恢复进度/文件/重试的强一致语义。
>
> 约定：涉及行号以 `v0.3.1`（`git HEAD` commit 见 `Cargo.toml` 0.3.1）为准，改动后需同步更新 `docs/architecture.md` §5 时序与 `docs/errors.md`。

---

## 0. 依赖与执行顺序

```
M1（P0） ———→  M2（本文件 8项） ———→  M3（P2 体验）
          \
           └→  M2-07 依赖 M1-02（EOF 完整性）已修正，否则 preserve_partial 仍基于错误 Complete 语义
           └→  M2-11 依赖 M1-05（sidecar 自愈）策略一致
           └→  M2-09 可与 M1 并行开发，但联调需 M1-03（Lagged 可靠化）先合入，避免同一 chunk 同时触发 Pending 与 Lagged
```

分支策略：`fix/m2-consistency` 从 `fix/m1-hotfix` 合后切出；每项独立 commit，CI 需过 `cargo test --all-features` + `resume` / `multi-source` 集成。

---

## M2-07 preserve_partial throttling 滞后导致重叠/进度虚低

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/state.rs:137-141` `preserve_partial()`; `src/chunk.rs:54-193` throttling ( `PROGRESS_THROTTLE_BYTES=64KiB`, `PROGRESS_THROTTLE_INTERVAL=50ms`); `src/retry.rs:99-102` `on_chunk_failed` 调用 `preserve_partial` 后 `chunks.remove` |
| **现象** | 失败瞬间最多 64 KiB 未上报，`chunk.downloaded_bytes` 为上次节流值；`preserve_partial` 将其累加到 `completed_bytes`，导致 `total_downloaded()` 长期虚低（最多每失败一次丢 64 KiB），重试区间仍按原 `start-end` 重下，落盘时与已写入的前缀重叠；并发下 `is_download_finished()` 误判需多一次 tick。 |
| **根因** | 进度是“采样”而非“确认”；`preserve_partial` 信任采样值，而 `file_writer_task` 的 `WriteFile {offset,data}` 已在 mpsc 中排队或已落盘。`monitor.rs:245` 的 `ChunkProgress` 也用 `or_insert_with` 重建状态，无法追溯真实 `offset`。 |
| **改动设计** | 1) **chunk 侧最终冲刷**：`chunk_run` 在发送 `ChunkFailed` 前强制补发一次 `ChunkProgress { downloaded = offset-start_byte }`（与成功分支的“最终补发”对称，见 `chunk.rs:219-230`），确保失败前至少有一个精确快照。<br>2) **state 侧双轨**：`preserve_partial` 改为 `preserve_partial_exact(id, downloaded)` 由调用方传入真实偏移；`retry.rs:100` 传入 `offset-start` 而非读 `state.chunks[id].downloaded_bytes`。为兼容，保留旧签名但标记 `#[deprecated]`。<br>3) **writer 确认（可选增强）**：`DownloadState` 新增 `acknowledged_bytes: HashMap<ChunkId,u64>` 由 writer 回执更新（经 `DownloadCmd::AckWrite` 或复用 `DownloadInfo::ChunkProgress` 的最终值），`total_downloaded()` 优先取 ack 值。M2 最小闭环只做 1)+2)，3) 列为 M2-07b 扩展。 |
| **接口变更** | `RetryHandler::on_chunk_failed` 增加 `failed_offset: u64` 参数（或从 `ChunkFailed.error` 携带）；`ChunkFailed` 已含 `start/end`，补充 `downloaded` 字段需评估兼容性——建议仅改内部调用，不改公开 `DownloadInfo::ChunkFailed`，而是在 `monitor.rs:293` 解包后计算 `actual = state.chunks.get(&id).map_or(0,|c| c.downloaded_bytes)` 并在补发后立即取新值。 |
| **依赖** | 依赖 M1-02（EOF 判完整）——否则 EOF 误 Complete 会绕过本修复。 |
| **风险** | 补发可能与 `broadcast Lagged` 叠加；需与 M1-03 的 `Lagged` 对账逻辑一起联调，补发后若仍 Lagged 则 monitor 按 `size()` 回退（保留现有容错）。 |
| **验收标准** | mockito：单 chunk 10 MiB，限速 5 MiB/s，注入中途 `stream Err`，断言 `state.completed_bytes + Σdownloaded == 写入文件实际大小` 且 `total_downloaded` 误差 < 4 KiB；失败 100 次累计误差归零。集成：`tests/chunk.rs` 新增 `preserve_partial_throttle_gap` 用例。 |
| **估时** | 0.5d |
| **回滚点** | revert `chunk.rs:204` 补发 + `retry.rs:100` 签名改动 |

---

## M2-08 MissingContentLength / NoAvailableSources streaming 旁路丢失 ResumePlan 与多源回退

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/downloader.rs:414-425` `MissingContentLength -> streaming_download`；`src/downloader.rs:446-471` `NoAvailableSources/MissingContentLength` 多源回退到首源流式；`src/downloader.rs:590-691` `streaming_download`（单流、`size=0`、`truncate=true`、无 `ResumeRecorder`）；`src/downloader.rs:495-524` `ResumePlan::prepare_async` 原应在旁路前执行 |
| **现象** | 单源 `HEAD/Range` 均无长度时直接进流式写 `0` 大小编译路径，残留的 `.download.bitcode` 未清理也未参与校验，下次恢复仍走 `prepare` 误判 `completed_bytes`；多源全失败回退首源流式同样丢失多源 `lane` 信息与 resume，导致断点后无法续传，且错误被 `warn` 吞没（`NoAvailableSources` 应透传）。 |
| **根因** | `run_internal` 的探测分支在 `resume_plan` 构造前就 `return streaming_download`，旁路成为“无状态”路径；多源分支把两类错误合并处理，未区分“可降级流式”与“应失败”。 |
| **改动设计** | 1) **前移 ResumePlan**：在探测前先构造 `ResumePlan::prepare` 的 `metadata_path` 句柄，旁路内根据 `resume_enabled` 执行 `remove_file(sidecar)` 或 `save_atomic(empty)`，并在 `streaming_download` 签名增加 `resume_enabled:bool` 与 `output_path:PathBuf` 参数，使流式也能走 `file_writer_task` 的同一截断/清理语义。<br>2) **错误分级**：`MissingContentLength` 仅单源可回退流式；多源 `NoAvailableSources` 必须 `return Err` 不回退，仅 `MissingContentLength` 且 `config.sources.len()>=1` 时回退首源，且回退前 `tracing::warn!(fallback=true)` 并保留 `info_tx` 的 `MonitorUpdate {total_size=0}` 语义。<br>3) **文档**：`docs/errors.md` 明确 `MissingContentLength` 的流式语义与 resume 互斥。 |
| **接口变更** | `streaming_download` 新增 `resume_enabled` 参数；`DownloadError::NoAvailableSources` 保持不变，但调用处不再吞错。 |
| **依赖** | 依赖 M1-05 的 sidecar 自愈策略（validate_shape 失败删重建）保持一致；与 M2-11 的 `file_len` 归一联动。 |
| **风险** | 流式文件为 `0` 长度预分配，`set_len(0)` 后 `truncate(true)` 语义需与 `file_writer_task_impl:199` 对齐，避免 `ENOSPC` 分支误触发。 |
| **验收标准** | 单测：`get_file_info` mock `HEAD 0` + `Range 200 no length` → `MissingContentLength` → `streaming_download` 成功且 `metadata_path` 不存在；多源 2 失效源 → `NoAvailableSources` 不回退。集成：`tests/missing_content_length.rs` 覆盖两种旁路。 |
| **估时** | 0.5d |
| **回滚点** | revert `downloader.rs:398-493` 分支顺序 |

---

## M2-09 pending_bisects / deferred_retries 无界与空转

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/monitor.rs:32-33` `pending_bisects: VecDeque<(u64,u64)>`；`src/monitor.rs:298-304` `ChunkBisected` 入队；`src/monitor.rs:378-457` `handle_tick` 的 `drained_pending` + `deferred_retries: Vec<FailedChunkInfo>`（`push_front` 回队） |
| **现象** | lane 容量 `max_chunks_per_lane=1` + `workers=32` 时 `concurrency` 仍按 workers 决策分裂，`pending_bisects` 短时堆积数百区间（审计 t2-3）；`deferred_retries` 每 tick `pop_ready→defer→push_front` 空转，`retry_queue` FIFO 被 `push_front` 破坏。 |
| **根因** | 分裂决策未感知 lane 容量；队列无背压上限与排水优先级；重试 `push_front` 导致新到期块插队。 |
| **改动设计** | 1) **容量感知分裂**：`ConcurrencyManager::decide_and_act` 增加 `available_capacity: usize` 参数（由 `monitor` 传入 `multi_runtime.scheduler.available_capacity()`），`split_is_useful` 中若 `available==0` 直接 `false`。<br>2) **有界 + 优先级**：`pending_bisects` 设上限 `max_pending = max_workers*2`，超限时 `warn + drop_oldest` 或直接 `return`（不丢范围空洞，需记录）；`handle_tick` 中先 `drain_pending` 再 `process_retries`，且 `deferred_retries` 改 `push_back` 保序，超限时 `tracing::warn!(pending=len)`。<br>3) **重试背压**：`RetryHandler::process_queues` 到期后按 `retry_at` 排序，`pop_ready_chunk` 仅弹到期首个，避免每 tick 扫描全队。 |
| **接口变更** | `ConcurrencyManager::decide_and_act(&mut self, state:&DownloadState, available:usize, cmd_tx:…)`；`DownloadMonitor::handle_tick` 计算 `available` 后传入。 |
| **依赖** | 无 M1 强依赖，但与 M1-03 的 Lagged 修复同属 monitor 控制环，需同版本联调。 |
| **风险** | `available_capacity` 计算需在 `handle_tick` 持有 `&mut MultiRuntime` 时可重入；避免与 `build_request` 的 `best_lane` 竞争。 |
| **验收标准** | 多源用例：`max_chunks_per_lane=1` + 2 源 + workers=32 下 50 MiB 下载，`pending_bisects.len() < 64` 且下载时间与限速理论值误差 <15%；`retry` 单测：`push_front` 改 `push_back` 后 FIFO 顺序断言。 |
| **估时** | 1d |
| **回滚点** | revert `monitor.rs:378-457` + `concurrency.rs:491-506` |

---

## M2-10 `or_insert_with` 隐式新建 State 导致重叠

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/monitor.rs:243-258` `ChunkProgress { entry(id).or_insert_with(|| ChunkState::new(id,start,end)) }`；`src/monitor.rs:281-294` `ChunkFailed` 的 `preserve_partial` 后 `remove`；`src/state.rs:31-33` `ChunkState::new` |
| **现象** | 若同一 `ChunkId` 的 `ChunkFailed` 与延迟的 `ChunkProgress` 乱序到达，或重试重用同 `id`（`retry.rs:149` 重用原 `id`），`or_insert_with` 会以旧 `start/end` 重建一个“僵尸” `ChunkState`，与新重试任务的区间重叠，`total_downloaded` 双计数 `completed_bytes + chunks[].downloaded`。 |
| **根因** | `ChunkProgress` 信任 `start_byte/end_byte` 来自 chunk 自身，而 monitor 未校验 `id` 是否属于当前活跃集合；`chunks` 的生命周期由 `DownloadComplete/ChunkFailed/remove` 驱动，`or_insert` 破坏不变量。 |
| **改动设计** | 1) **严格模式**：`ChunkProgress` 仅 `if let Some(chunk)=self.state.chunks.get_mut(&id)` 时更新，否则 `warn!(id, start,end, downloaded, "stale progress dropped")` 并忽略；初始化仅在 `orchestrate_downloads` 与 `ChunkBisected`/`pop_ready` 处 `insert`。<br>2) **重试隔离**：`RetryHandler` 重试时若 `id` 仍在 `state.chunks`（未 `remove`）则分配新 `id`（`next_chunk_id.fetch_add`），避免复用。<br>3) **双计数防护**：`state.rs:137` `preserve_partial` 后立即 `debug_assert!(chunks.contains_key(id)==false)`。 |
| **接口变更** | 无公开 API 变更；内部 `handle_download_info` 行为变更，需更新注释。 |
| **依赖** | 依赖 M1-02 的 EOF 完整性（否则 Complete 语义不可靠）。 |
| **风险** | 丢弃 stale progress 可能使 `total_downloaded` 瞬时回落；但因 M2-07 已补发精确值，回落窗口 <50ms 可接受。 |
| **验收标准** | 单测：构造 `state.chunks.remove(id)` 后再发 `ChunkProgress{id}` 应被丢弃且 `total_downloaded` 不增加；并发测试：32 workers 注入 5% 随机重试，文件校验和通过且 `state.chunks.len()==0` 时 `completed_bytes==file_size`。 |
| **估时** | 0.5d |
| **回滚点** | revert `monitor.rs:243-248` |

---

## M2-11 `record_write` flush 未 sync 与 `file_len` 归一

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/util.rs:222-241` `file.flush()` 未 `sync_all`；`src/resume.rs:329` `read_segment` 同 fd 读 page cache；`src/resume.rs:141-170` `verify_against_file` 对 `file_len <= seg.end` 仅 `hash=None` 不截断；`src/resume.rs:152-154` `file_len > file_size` 未处理 |
| **现象** | `flush` 仅到 OS cache，若未来改为 `sync_all` 或换 fd 读则哈希读旧零；`file_len` 超长尾部被信任，`collect_contiguous` 误判已验证；截断文件未归一导致下次恢复多读一遍。 |
| **根因** | 写入与校验共用同一 `File` 句柄且依赖 page cache 可见性；`verify_against_file` 只做段级丢弃，未做文件级归一。 |
| **改动设计** | 1) **增量哈希**：`ResumeRecorder` 维护每段的 FNV 累加器 `segment_hasher: Vec<Option<u64>>`，`record_write` 按 `Bytes` 直接 `fold` 进累加器，达到 `seg_len` 时 `finalize` 为 `hash`，彻底消除 `read_segment` 回读。<br>2) **文件归一**：`verify_against_file` 入口增加 `if file_len != file_size { warn!(); file.set_len(file_size)?; file_len=file_size; }`；对 `file_len==0` 且 `segment.hash.is_some()` 的异常提前 `truncate_output=true`。<br>3) **sync 策略**：`file_writer_task_impl` 的 `flush` 后仍 `flush`，但哈希不再依赖读，故无需 `sync_all`；仅 `recorder.flush()` 时 `sync_all` 一次。 |
| **接口变更** | `resume.rs:307 record_write(file:&mut File,offset,len)` 改为 `record_write(offset, data:&[u8])` 或保留签名但忽略 `file` 回读；为兼容先加新方法 `record_write_bytes` 并灰度。 |
| **依赖** | 依赖 M1-05 的 sidecar 自愈（validate_shape 失败重建）保持文件级一致。 |
| **风险** | 增量哈希需处理跨多次 `WriteFile` 覆盖同一段的幂等 `add_covered_range` 去重，复用现有 `covered_ranges` 逻辑即可。 |
| **验收标准** | 单测：`ResumeRecorder` 写入 1 KiB ×64 次跨段，断言每段一次 finalize；`verify_against_file` 对手工 `truncate(半长)` 与 `extend(超长)` 均能归一到 `file_size`。集成：`tests/resume.rs` 损坏段仍能归一。 |
| **估时** | 1d |
| **回滚点** | revert `resume.rs:283-360` + `util.rs:224` |

---

## M2-12 `Content-Range: bytes */N` / `416` 处理不全

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/util.rs:26-142` `get_file_info`；`src/util.rs:69-83` `status.is_success() && !=206` 分支；`src/util.rs:85-111` `206` 分支的 `total=="*"` 丢弃 |
| **现象** | 服务端对 `Range: bytes=0-0` 回 `416 Range Not Satisfiable` 且 `Content-Range: bytes */1234` 时，`is_success()` 为 `false` 且非 `206`，当前代码回退 `HEAD` 或 `Content-Length`，若两者缺失则 `MissingContentLength` 误判；对 `bytes 0-0/*` 也直接 `head_support` 回退，丢失总大小。 |
| **根因** | 仅处理 `rfind('/')` 后 `total!="*"`，未处理 `*/N` 的语义；未单独处理 `416`。 |
| **改动设计** | 1) **416 分支**：在 `status==416` 时优先解析 `Content-Range: bytes */N` 的 `N` 为 `file_size`，`support_ranges=false`（或 `true` 视 `Accept-Ranges`），直接 `Ok((N,false))`。<br>2) **通配分支**：在所有 `Content-Range` 解析处增加 `if crs.starts_with("bytes */") { parse N; return Ok((N, head_support||support)) }`。<br>3) **fallback 顺序**：`416` → `206/Content-Range` → `HEAD` → `Content-Length` → `MissingContentLength`，并增加 `tracing::info!(probe="416-target")`。 |
| **接口变更** | 无；仅 `get_file_info` 内部。 |
| **依赖** | 与 M2-08 的 `MissingContentLength` 分级联动，明确 416 属于“可恢复的探测成功”。 |
| **风险** | `416` 的 `support_ranges` 语义需与 `downloader.rs:505` 的 `partial resume requires Range` 联动，避免对不支持 Range 的流式误判。 |
| **验收标准** | mockito：`GET Range:0-0` → `416` + `Content-Range:bytes */1234` + `HEAD 1234` → `Ok((1234,false))`；`GET 200` + `Content-Range:bytes */1234` 同上；覆盖 `tests/missing_content_length.rs` 新增 416 用例。 |
| **估时** | 0.25d |
| **回滚点** | revert `util.rs:66-110` |

---

## M2-13 `update_interval=0` busy-loop 与 `workers=0` 校验

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/downloader.rs:147-152` `update_interval(mut self, v: f64)` 仅 `if v>0` 才赋值，否则静默保留旧值；`src/downloader.rs:135` `workers(mut self, v: u64)` `max(1)`；`src/monitor.rs:98` `interval(Duration::from_secs_f64(update_interval))` 若 `0.0` 会 panic 或 busy-loop；`src/concurrency.rs:86` `new_with_interval` 对 `<=0` 回退 `0.5` |
| **现象** | `builder().update_interval(0.0)` 看似接受但实际无效果，调用方无法感知错误；若未来绕过 builder 直接构造 `Downloader{update_interval:0}`，`monitor::interval(0)` 会以极高频 tick 导致 CPU 100% 与 `broadcast Lagged` 风暴；`workers=0` 已 `max(1)` 但未 `warn`。 |
| **根因** | 参数校验“静默纠正”而非“显式拒绝”，且校验分散在三处未统一。 |
| **改动设计** | 1) **统一校验**：`DownloadBuilder::update_interval` 保持 `>0` 才赋值，额外 `tracing::warn!(input, kept)`；`Downloader::new`/`new_multi` 对 `update_interval <=0` 直接 `default=0.5` 并 `warn`；`Monitor::new_with_completed` 对 `<=0.0` `clamp(0.05, 10.0)`。<br>2) **显式 API**：新增 `validate()` 或在 `build()` 阶段对 `workers` 归一后 `tracing::info!(effective_workers)`，保持现有 `max(1)` 语义但增加可观测性。<br>3) **文档**：`docs/configuration.md` 明确 `update_interval` 范围 `0.05..10.0`，`workers` 最小 1。 |
| **接口变更** | 无破坏性；仅增加 `warn` 日志。 |
| **依赖** | 无 M1 强依赖。 |
| **风险** | 低。 |
| **验收标准** | 单测：`builder().update_interval(0.0).build()` 断言 `update_interval==0.5` 且触发 `warn`；`monitor` 构造 `0.0` 不 panic。 |
| **估时** | 0.25d |
| **回滚点** | revert `downloader.rs:147` + `monitor.rs:58` |

---

## M2-14 writer 128 KiB 合并 + 同步 hash 阻塞

| 维度 | 内容 |
|---|---|
| **文件:行号** | `src/util.rs:183-293` `file_writer_task_impl`；`src/util.rs:204-244` `COALESCE_LIMIT=128KiB` + `pending` 合并；`src/resume.rs:329` `record_write` 同步 `seek+read` 哈希 |
| **现象** | writer 单线程串行 32 workers 的 `mpsc(128)`，合并缓冲 128 KiB 未满不落盘，`record_write` 同步读整段 64 KiB 做 FNV，导致 `writer_tx.send().await` 阻塞 50-100ms，`chunk` 的 `select! biased` 下 `BisectDownload/Terminate` 响应延迟，`concurrency` 观察期样本滞后。 |
| **根因** | 合并与哈希在同一 `tokio::spawn` 串行，且哈希依赖回读文件；`WRITER_QUEUE_CAP=128` 在高并发下易满背压。 |
| **改动设计** | 1) **M2-14a 轻量**：保持合并，但 `record_write` 改增量哈希（同 M2-11），消除 `seek+read`，writer 仅 `seek+write + flush`。<br>2) **M2-14b 容量**：`WRITER_QUEUE_CAP` 从 `128` 提升至 `512` 或按 `workers` 自适应 `max(128, workers*16)`，减少背压。<br>3) **M2-14c 异步化（可选）**：`record_write` 的 `save_atomic_async` 已异步，但 `pending` 刷新频率由 `16段或1s` 控制，保持不变；若仍瓶颈则将哈希累加器移至 `ResumeRecorder` 内存态，writer 线程仅做 `memcpy+hash`，不觸 I/O。<br>本 M2 先交付 14a+14b，14c 视压测再定。 |
| **接口变更** | 无公开 API；内部 `file_writer_task_impl` 签名不变。 |
| **依赖** | 依赖 M2-11 的增量哈希已落地，否则本项无法消除阻塞。 |
| **风险** | 队列加大增加内存（512×64 KiB 峰值 32 MiB）但仍可控；合并阈值不变保证落盘粒度。 |
| **验收标准** | 压测：32 workers + 100 MiB 文件，`writer_tx.send` 的 `await` p95 < 10ms，`BisectDownload` 端到端延迟 < 150ms；`cargo bench --example adaptive_bench` 对比前后。 |
| **估时** | 0.5d（14a 与 M2-11 同步）+ 0.25d（14b） |
| **回滚点** | revert `util.rs:184` 队列大小 + `resume.rs:329` |

---

## 验收矩阵与回归

| 项 | 用例 | 预期 |
|---|---|---|
| M2-07 | `chunk` 注入失败的 `throttle_gap` 单测 | `total_downloaded` 误差 <4 KiB |
| M2-08 | `MissingContentLength` 单源流式 + `NoAvailableSources` 多源不回退 | 前者 sidecar 清理，后者 `Err` |
| M2-09 | `max_chunks_per_lane=1` 32 workers 多源 50 MiB | `pending<64` 且耗时回归 |
| M2-10 | `or_insert` 僵尸 state | stale `ChunkProgress` 丢弃 |
| M2-11 | `verify_against_file` 截断/超长归一 + 增量哈希 | 文件长度归一，哈希一致 |
| M2-12 | `416 + bytes */N` mock | `Ok((N, _))` |
| M2-13 | `update_interval=0` / `workers=0` | `warn` + `0.5` / `1` 归一 |
| M2-14 | 32 workers  writer 背压 | p95<10ms, bisect<150ms |

回归范围：`tests/resume.rs`、`tests/multi_source.rs`（`--features multi-source,resume,progress`）、`tests/missing_content_length.rs`、`tests/basic_download.rs`（mockito）、`tests/chunk.rs`、`tests/concurrency.rs`。

---

## 发布检查

- [ ] `cargo test --features resume,multi-source,progress` 全过
- [ ] `cargo test --features resume,multi-source --test process_resume -- --nocapture --test-threads=1`（进程级恢复）
- [ ] `docs/architecture.md` §5 时序更新（`preserve_exact`、`capacity-aware split`、`incremental hash`）
- [ ] `CHANGELOG.md` M2 条目与 M1 分章
- [ ] 回滚预案：`git revert fix/m2-consistency` 可单独回退，不影响 M1 标签

