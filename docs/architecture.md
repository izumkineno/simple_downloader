# simple_downloader 架构说明

> 本文基于当前源码的静态梳理（`src/*.rs`、`examples/*.rs`、`test_server/server.py`）。README 中的 Mermaid 图只作为概念总览；**本页是运行时流程、通道语义、动态分片与重试行为的权威说明**。

## 1. 项目概览

`simple_downloader` 是一个以 **Rust 异步并发下载** 为核心的下载器库/示例工程。整体设计是“入口编排 + 分片执行 + 监控控制环 + 消息通道”的模型：

- `Downloader` 负责构建 HTTP client、探测远端能力、启动文件写入任务、启动进度处理器、派发初始分片，并把运行期管理权交给 `DownloadMonitor`。
- `DownloadMonitor` 拥有并维护 `DownloadState`、`ConcurrencyManager`、`RetryHandler`，是运行时状态、动态分片与重试恢复的控制中心。
- `chunk_run` 是按 byte range 拉取数据的执行任务；worker 不是一次性全部预创建，而是由初始任务、分片事件或重试队列按需产生。
- `file_writer_task` 是独立写入任务，集中处理 `WriteFile`，避免多个下载任务直接竞争同一个文件句柄。
- `DownloadInfo` 和 `DownloadCmd` 是模块之间的协议边界，但 `DownloadCmd` 不是单一总线；它被用于控制广播和写入队列两类不同传输。

## 2. 目录结构

```text
src/
  chunk.rs         # 单分片下载、分片响应、写入回传
  concurrency.rs   # 动态并发控制、分片决策、阶段管理
  downloader.rs    # 顶层下载入口与启动编排
  monitor.rs       # 运行时控制环，拥有状态、并发管理器、重试处理器
  retry.rs         # 即时重试与延迟重试队列
  state.rs         # 分片状态与聚合下载状态
  types.rs         # 领域消息、错误、事件协议
  util.rs          # 远端文件探测与文件写入任务（P0-4 流式追加）
  limiter.rs       # rate-limit 令牌桶（0.5.x 全局/分源）
  config.rs        # 运行时热更新 RuntimeConfig（0.5.5）
  trace.rs         # tracing 初始化门面
  queue.rs         # 任务队列 FIFO/并发调度（0.6.x pending_deletes 延迟删）
  task.rs          # 任务句柄与快照
examples/
  download.rs
  with_custom_ui.rs
  with_rate_limit.rs
  with_queue.rs
test_server/
  server.py        # 可控测试 HTTP 服务
tests/
```

## 3. 核心模块与所有权

### 3.1 启动链路

```text
Downloader::builder(url, output).workers(n).download() / .run(handler)
  ├─ build reqwest::Client (pool 32/90s/keepalive 60s + UA simple_downloader/x.y.z)
  ├─ get_file_info(HEAD；必要时回退 Range: bytes=0-0 → Content-Range)
  ├─ ResumePlan::prepare_async (resume feature, 64KiB segment hash, version=1)
  ├─ file_writer_task[_with_resume](output_path, file_size) -> mpsc<DownloadCmd> 128  # 0.5.4+ 流式追加，无 set_len 预分配，.truncate(false)，create_dir_all
  ├─ spawn(progress_handler(total_size, info_rx)) // progress feature, broadcast + reliable mpsc
  ├─ spawn initial chunk(s) covering remaining_ranges (单源全量或 resume 剩余区间；workers 仅上限)
  ├─ DownloadMonitor::new[_with_completed](file_size, update_interval, workers).with_rate_limit(global_limiter)
  └─ monitor.run[with_reliable](info_rx, tasks, channels, next_chunk_id, client, writer_tx, cmd_tx, url, multi_runtime)
     └─ 限速时 is_rate_limited=true 跳过 ConcurrencyManager::decide_and_act (0.5.1+ 冻结，drain_pending 例外见 §6.4)
```

这条链路强调两点：

1. `Downloader` 是启动编排者，不在运行期持续直接管理 `DownloadState`、并发策略或重试队列。
2. `DownloadMonitor` 接管运行期控制后，分片扩容、失败重试、状态聚合都通过 monitor-owned helper 完成。

### 3.2 运行时协作角色

- **Chunk worker**：只处理一个当前 byte range 的网络拉取、写入请求和事件上报。它不会直接修改全局状态。
- **FileWriter**：通过有界 `mpsc<DownloadCmd>` 接收 `WriteFile`，执行 seek + write，并在结束前 flush。
- **DownloadMonitor**：消费 `DownloadInfo`，更新 `DownloadState`，定期计算速度/总进度，驱动并发分片和重试队列。
- **ConcurrencyManager**：作为 monitor 的内部策略组件，基于速度、剩余时间、稳定性采样和最大并发限制决定是否广播 `BisectDownload`；当前策略会避免在吞吐证据不足、接近完成或只是“恰好有空闲 worker 槽位”时盲目继续分片。
- **RetryHandler**：作为 monitor 的内部恢复组件，维护即时重试队列和延迟重试队列，决定失败 range 何时重新生成 chunk。
## 4. 协议与通道图例

| 协议/通道 | 传输 | 发送者 | 消费者 | 语义 |
| --- | --- | --- | --- | --- |
| `DownloadInfo` | `broadcast 4096` + `mpsc reliable 1` 兜底 | chunk、monitor | monitor、progress handler | 状态/进度事件：`ChunkProgress`/`DownloadComplete`/`ChunkFailed`/`ChunkBisected`/`ChunkStatusChanged`/`MonitorUpdate`；终局 `Failed/Complete/Bisected` 经 `mpsc reliable` 兜底防 `Lagged` 丢失（0.6.2+ `send_terminal_event`） |
| `DownloadCmd::BisectDownload` | `broadcast<DownloadCmd>` | `ConcurrencyManager` 经由 monitor tick | chunk worker | 控制命令：要求指定 chunk 自切一半，并上报新 range |
| `DownloadCmd::TerminateAll` | `broadcast<DownloadCmd>` | `Downloader` 在 monitor 返回后 | chunk worker | 控制命令：下载结束后的清理信号 |
| `DownloadCmd::WriteFile` | `mpsc<DownloadCmd> 128` | chunk worker | file writer task | 写入命令：有界队列提供背压；`file_writer_task_impl` 流式 `seek+write+flush`，`ENOSPC` 在 `write/flush` 以 `Io` 暴露 |

`DownloadCmd` 是同一个 enum，但有两条传输路径：控制命令走 `broadcast`，写入命令走 `mpsc`。因此文档和图示中不把它描述成一个统一命令总线。

`DownloadInfo` 亦有两条传输路径：`broadcast 4096` 主通道 + `mpsc reliable` 终局兜底（`ChunkFailed/DownloadComplete/ChunkBisected`，`src/chunk.rs:send_terminal_event`）。

状态码来自 `DownloadInfo::MonitorUpdate.chunk_details` 和 `ChunkStatusChanged`：`0=下载中`、`1=重试中`、`2=等待重试`、`3=延迟重试`、`4=已完成`、`5=失败`。

## 5. 运行时时序图（单张分区块）

    Note over User,Downloader: 1. 启动与探测
    User->>Downloader: builder(url, output).workers(n).download() / run(handler)
    Downloader->>Probe: build client and probe file info
    Probe->>Server: HEAD
    alt HEAD 返回完整元数据
        Server-->>Probe: Content-Length / Accept-Ranges
    else HEAD 失败或响应头不完整
        Probe->>Server: GET Range bytes=0-0
        Server-->>Probe: Content-Range / Content-Length
    end
    Probe-->>Downloader: file_size + support_ranges
    Downloader->>Writer: file_writer_task[_with_resume](streaming append, no preallocate, truncate(false))
    Downloader->>Progress: spawn progress_handler(info_rx) // broadcast + reliable mpsc

    Note over Downloader,Monitor: 2. 初始分片与 Monitor 接管
    Downloader->>Chunk: spawn initial full-range chunk
    Downloader->>Monitor: run(info_rx, tasks, channels, next_chunk_id)
    Note over Monitor: Monitor owns DownloadState, ConcurrencyManager, RetryHandler

    Note over Chunk,Progress: 3. 常规进度循环
    loop streaming bytes and monitor ticks
        Chunk->>Server: GET Range current bytes
        Server-->>Chunk: byte stream
        Chunk->>Writer: mpsc WriteFile(offset, data)
        Writer-->>Writer: seek + write
        Chunk-->>Monitor: DownloadInfo::ChunkProgress
        Monitor-->>Monitor: update DownloadState and EMA speed
        Monitor-->>Progress: MonitorUpdate / StatusChanged
    end

    Note over Monitor,Chunk: 4. 动态分片
    opt speed and remaining-time conditions allow split
        Monitor-->>Monitor: ConcurrencyManager decides split
        Monitor-->>Chunk: broadcast DownloadCmd::BisectDownload(id)
        Chunk-->>Monitor: DownloadInfo::ChunkBisected(new range)
        Monitor->>Chunk: spawn new chunk for new range
    end

    Note over Chunk,Monitor: 5. 即时重试
    alt request stream or writer path fails
        Chunk-->>Monitor: DownloadInfo::ChunkFailed(start,end,error)
        Monitor-->>Monitor: RetryHandler queues immediate retry
        Monitor-->>Progress: ChunkStatusChanged waiting retry
        loop retry delay elapsed
            Monitor->>Chunk: respawn chunk for failed range
            Monitor-->>Progress: ChunkStatusChanged retrying
        end
    end

    Note over Monitor,Chunk: 6. 延迟恢复
    opt attempts exceed MAX_RETRIES
        Monitor-->>Monitor: move failed range to delayed retry queue
        Monitor-->>Progress: ChunkStatusChanged delayed retry
        Monitor-->>Monitor: after delay move back to ready retry
        Monitor->>Chunk: respawn chunk after delayed recovery
    end

    Note over Chunk,Downloader: 7. 完成与关闭
    Chunk-->>Monitor: DownloadComplete(id)
    Monitor-->>Monitor: complete chunk and clear retry record
    opt all active chunks and retry queues done
        Monitor-->>Downloader: monitor loop returns
        Downloader-->>Chunk: broadcast DownloadCmd::TerminateAll
    end
```

## 6. 分阶段说明

### 6.1 启动与探测

`get_file_info` 先尝试 `HEAD`，读取 `Content-Length` 与 `Accept-Ranges`。如果服务端不支持或响应头不完整，则回退到 `GET Range: bytes=0-0`，优先解析 `Content-Range`，最后才尝试普通 `Content-Length`。返回值是 `(file_size, support_ranges)`。

### 6.2 初始分片与接管

即使配置允许多个 worker，当前实现也先创建一个覆盖完整文件范围的初始 chunk。后续是否增加 worker，由 `DownloadMonitor` 定期调用 `ConcurrencyManager` 决定。这样可以先用真实吞吐采样判断是否值得分片，而不是在启动时预创建所有分片。

### 6.3 常规进度循环

chunk 拉取远端字节流后，把数据通过 `mpsc<DownloadCmd>` 发送给 `FileWriter`，同时通过 `broadcast<DownloadInfo>` 上报 `ChunkProgress`。monitor 消费这些事件，更新 `DownloadState` 中对应 chunk 的进度和结束位置；tick 到来时，monitor 更新 EMA 速度并广播聚合 `MonitorUpdate`。

### 6.4 动态分片

`ConcurrencyManager` 是 monitor 内部的决策器。它根据当前总速度、稳定性样本、剩余时间、最小分片间隔、最大并发数等因素决定是否发送 `BisectDownload`。当前实现的关键策略是：

- **探测阶段**：只有在正向吞吐样本表明确有带宽增益时才继续扩容；如果没有可安全切分的 range，则直接转入稳定阶段。
- **稳定阶段**：速度上升只会刷新历史基线，不会因为“速度又涨了”就立刻重新探测；只有当吞吐相对历史基线显著下降、且仍有足够剩余时间时，才尝试切分最慢的可分片 chunk。
- **补位分片**：并发槽位空出来后，也只有在当前平均速度为正、剩余完成时间仍值得分片、并且存在足够大的剩余 range 时，才会补充分片。
- **目标选择**：无论主动恢复还是补位，优先选择“剩余未下载字节数最多且仍可安全二分”的 chunk，而不是按原始 chunk 总尺寸粗暴排序。

收到命令的 chunk 自己调整当前 range 并上报 `ChunkBisected`，monitor 再为新增 range 生成新 chunk。

### 6.5 即时重试与延迟恢复

当网络流错误或写入队列关闭导致 chunk 失败时，chunk 上报 `ChunkFailed`。monitor 将失败范围交给 `RetryHandler`：未超过 `MAX_RETRIES` 时进入即时重试队列并等待 `RETRY_DELAY`；超过后进入延迟重试队列并等待 `DELAYED_RETRY_DURATION`。队列到期后，monitor 重新 spawn 对应 range 的 chunk。

### 6.6 完成与关闭

chunk 完成后上报 `DownloadComplete`。monitor 标记该 chunk 完成并清理 retry 记录；当活跃任务和重试队列都为空且 `DownloadState` 判断文件已完成时，monitor loop 返回。随后 `Downloader` 广播 `TerminateAll` 做最终清理。

## 7. 源码映射（模块级，不绑定行号，见 `src/*.rs` 顶层文档）

- 启动与编排：`src/downloader.rs` (`DownloadBuilder`/`Downloader`/`orchestrate_downloads`/`streaming_download` 含 `MissingContentLength` 单流回退)
- 文件信息探测与写入任务：`src/util.rs` (`get_file_info`→`Head→Range 0-0→Content-Length` 三级回退/`parse_content_range`/`file_writer_task_impl` 流式追加 `mpsc 128` + `ENOSPC` 透传)
- monitor 主循环与运行时接管：`src/monitor.rs` (`DownloadMonitor::run`/`run_with_reliable`/`apply_config` 热更 `workers/interval/global limiter`/`drain_pending` 容量补位不限速冻结/`reliable_rx` 终局兜底)
- 动态分片策略：`src/concurrency.rs` (`ConcurrencyManager` `Probing→Stable` + `MIN_SPLITTABLE_REMAINING 256KiB`/`MIN_CHUNK_SIZE 10KiB`/`ObservationState`)
- 重试队列与延迟恢复：`src/retry.rs` (`RetryHandler` `MAX_RETRIES=10`/`RETRY_DELAY=1s`/`DELAYED=10s`/`MAX_TOTAL=30` FIFO `VecDeque`)
- chunk 下载、切分、写入、事件上报：`src/chunk.rs` (`chunk_run_with_reliable` `206/Content-Range`强校验/`Early-EOF`门限/`send_terminal_event`/`PROGRESS_THROTTLE 64KiB/50ms`)
- 状态与 EMA 速度：`src/state.rs` (`DownloadState` `SMOOTHING_FACTOR 0.30` EMA + `is_splittable`/`complete_chunk`)
- 消息协议：`src/types.rs` (`DownloadCmd`/`DownloadInfo #[non_exhaustive]` + `MonitorUpdate` 稳定契约 `0.6.2+` + `is_complete` 对 `0/0` 分支)
- 限速：`src/limiter.rs` (`RateLimiter` `governor` `1token=1byte` `burst 64KiB` `reconfigure`/`disable`/`acquire` 32-64KiB 批量 `join max`)
- 配置热更新：`src/config.rs` (`RuntimeConfig{workers,update_interval,speed_limit,burst}`/`SharedConfig` `apply_config` 热更全局限速已落地，`per_source` 待接)
- 任务队列：`src/queue.rs`/`src/task.rs` (`TaskQueue` FIFO `pending_deletes 200ms` `PermissionDenied/未知Io重试`/`with_suffix a.tar(1).gz`/`occupied` 三重 CAS)
- 可观测：`src/trace.rs` (`init_tracing`/`try_init_tracing`/`RUST_LOG`/`SIMPLE_DOWNLOADER_LOG` + `#[instrument]` 全链路)

## 8. 扩展边界

- 新调度策略优先扩展 `concurrency.rs`，并保持由 `DownloadMonitor` 调用。
- 新重试策略优先扩展 `retry.rs`，不要让 chunk 直接持有全局恢复状态。
- 新 UI 或 CLI 进度展示应消费 `DownloadInfo`，不要读取 monitor 内部状态。
- 新存储后端可以替换 `file_writer_task` 背后的实现，但仍应保留有界写入队列提供背压。
- 新测试场景可以扩展 `test_server/`，模拟限速、断连、慢流、Range 兼容性等场景。

## 9. 一句话总结

`simple_downloader` 的运行时核心不是“预先启动固定数量 worker”，而是：`Downloader` 完成启动编排，`DownloadMonitor` 拥有状态/并发/重试控制面，chunk worker 按需执行 byte range，所有边界通过明确的 `DownloadInfo` 和双传输 `DownloadCmd` 协议连接。
