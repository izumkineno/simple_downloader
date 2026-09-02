# 09 任务队列 Task Queue — 主流方案对照

> 本项目：`queue` feature / `TaskQueue FIFO 1..64 clamp + with_max_concurrent + enqueue/enqueue_with_workers (两层并发独立) + pause/resume/cancel/query/wait_all + pending_deletes 200ms + reliable 终局`（`src/queue.rs + lane.rs`）

## 1. 问题

多文件下载要“FIFO 公平、并发隔离、重名不碰撞、取消不残留、UI 事件不丢”。

## 2. 主流实现

### 2.1 aria2 — `--max-concurrent-downloads` + 队列

- `--max-concurrent-downloads=5`（默认 5）限同时下载的“队列项”数（`--input-file` 的每行一个项）；`--split/-k` 限项内连接数，两层并发独立，与本项目 `workers` vs `max_concurrent` 同构。
- 无 `pause/resume/cancel` 的单项控制（靠 RPC `aria2.pause/pauseAll/unpause/remove`），重名靠 `--auto-file-renaming/--allow-overwrite`。
- 文档：`aria2c.html#-j/--max-concurrent-downloads` “Set the maximum number of parallel downloads for every queue item. See also the --split option.”（本次已拉取）

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-j`、`https://github.com/aria2/aria2/blob/master/src/RequestGroupMan.cc`

### 2.2 JDownloader — LinkGrabber + Package 队列

- 抓链→去重→`package` 队列→`maxChunks/maxDownloads` 两层并发；重名 `filename_2.ext` 递增；支持 `pause/resume` 单项与全局。

### 2.3 IDM — 队列调度

- `Queue → Schedule → max simultaneous downloads`，`3..8` 并发，重名 `file(1).ext`，`pause/resume` 靠临时 `.part` 保留位图。

### 2.4 wget — 无队列

- 单文件模型，多文件需 `xargs -P` 或脚本循环。

## 3. 对比表

| 维度 | aria2 | JDownloader/IDM | **simple_downloader** |
|---|---|---|---|
| 队列 | `--max-concurrent-downloads` FIFO | Package FIFO | `TaskQueue::with_max_concurrent(3)` `1..64` FIFO |
| 两层并发 | `max_concurrent`（项） vs `split`（连接） | `maxDownloads` vs `maxChunks` | `max_concurrent`（队列） vs `workers`（项内） 独立 clamp |
| 重命名 | `--auto-file-renaming` | `file_2.ext` | `occupied + try_exists + *.download.bitcode` 三重 CAS，`a.tar.gz→a.tar(1).gz` 无限递增，`wins` 大小写折叠，`17并发唯一` 已测 |
| 取消 | RPC `remove` | 队列移除 + 删 part | `cancel` + `pending_deletes 200ms` 周期 `PermissionDenied/未知Io` 重试，防 Windows 锁残留 |
| 可靠终局 | 轮询 `tellStatus` | 事件 | `ChunkFailed/Complete/Bisected` 经 `reliable mpsc` 兜底防 `broadcast Lagged 4096` 丢失（`b4fcadf`） |
| 并发唯一性测试 | 无公开 | 无 | `concurrent_enqueue_assigns_unique_paths 17 并发` 全唯一 |

## 4. 对本项目的启示

1. **三重 CAS 是并发唯一的必要条件**：仅 `occupied` 内存集合在 `17 并发 enqueue` 下会撞名；仅 `try_exists` 磁盘检查在 `400MB/s` 高并发下 `open` 与 `create_new` 竞争仍撞；`*download.bitcode` 的存在性是第三重。
2. **两层并发必须独立**：`queue 3` 与 `workers 16` 解耦，正如 aria2 `--max-concurrent-downloads 5` 与 `--split 5` 解耦；`enqueue_with_workers` 允许单项 `workers` 覆盖全局默认。
3. **延迟删是 Windows 刚需**：`queue cancel` 后文件仍被 `file_writer_task` 持有 `File` 句柄，AV/索引持锁 `os5`；`pending_deletes 200ms + retry` 与 `resume` 的 `best-effort` 同源，已在 `0.6.1/0.6.2` 校准。
4. **reliable 兜底是 UI 不丢帧的关键**：`broadcast 4096` 在 `16路+progress` 高频 `ChunkProgress 4096/50ms` 下会 `Lagged` 丢终局 `Complete`，导致 `active=0` 但 `is_finished=false` 卡死；`b4fcadf` 的 `mpsc reliable` 已根治。

## 5. 参考链接

- https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-j
- `src/queue.rs:136 enqueue, 328 CAS, pending_deletes`、`tests/queue.rs`
- `examples/with_queue.rs`
