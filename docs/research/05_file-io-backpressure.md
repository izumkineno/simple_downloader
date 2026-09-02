# 05 文件 I/O 与反压 File I/O & Backpressure — 主流方案对照

> 本项目：`file_writer_task mpsc 128 有界 + 128KiB coalesce + 流式追加 truncate(false) + ENOSPC 暴露 + pending_deletes 200ms 延迟删`（`src/util.rs:279-501, 308-470`）

## 1. 问题

16 路并发每秒 400MB 写入，若无反压则内存爆（16×64KiB×突发）且小写入放大 100 倍；若预分配 `set_len` 则 `ENOSPC` 提前且残留空洞文件。

## 2. 主流实现

### 2.1 aria2 — `disk-cache + auto-save-interval`

- `--disk-cache=16M` 内存写缓存 + `--auto-save-interval=60` 控制落盘频率；`--file-allocation=falloc/trunc/none` 控制预分配（`falloc` 用 `posix_fallocate`，`none` 即流式追加）。
- 默认 `falloc`，`none` 时与本项目同为流式追加，`ENOSPC` 在 `write` 时暴露。
- 源码 `src/DiskAdaptor.cc / FileAllocationDispatcher`。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#--disk-cache,--file-allocation`

### 2.2 axel

- 多线程 `pwrite` 直写，无独立 writer 线程，无有界队列，靠 `fsync` 周期刷盘；小块多时 `write` 放大严重。

### 2.3 wget / curl -O

- 单线程 `fwrite`，无反压问题；大文件靠内核 page cache。

### 2.4 tokio 生态 — `mpsc::channel(128)`

- 有界通道天然反压：`send().await` 在队列满时 `pending`，上游 `chunk` 自动减速，无需额外 `Semaphore`。
- `governor` + `mpsc` 双反压在本项目中叠加：限速令牌桶限“发”，`mpsc 128` 限“写”。

## 3. 对比表

| 方案 | 写入模型 | 队列 | 合并 | 预分配 | ENOSPC 时机 | 延迟删 | 反压 |
|---|---|---|---|---|---|---|---|
| aria2 `falloc` | 缓存 16M | disk-cache | ✅ `piece` 合并 | `posix_fallocate` | `fallocate` 时 | 无 | cache 满即停 |
| aria2 `none` | 直写 | 同上 | 同上 | 无 | `write` 时 | 无 | 同上 |
| axel | `pwrite` 多线程 | 无 | 无 | 无 | `write` | 无 | 无 |
| **simple_downloader** | 独立 `file_writer_task` | `mpsc 128` 有界 | ✅ `128KiB COALESCE_LIMIT` 相邻同 `offset` 合并 | `truncate(false)` 流式 | `write/flush` 时 `DownloadError::Io` | `pending_deletes 200ms 取样 + PermissionDenied重试` | `mpsc 128` 自动减速 |
| tokio 惯用法 | — | `channel(n)` | 需手写 | — | — | — | `await` 即反压 |

## 4. 对本项目的启示

1. **有界 128 是正确值**：aria2 `disk-cache 16M` / 本项目 `128×~16KiB ≈2M` 同量级；`WRITER_QUEUE_CAP=128` 在 400MB/s 下既防 OOM（`339 INFO cancelled → writer exited` 已验证），又不至于饿死（`util.rs:286` 注释）。
2. **coalesce 128KiB 非可选**：400MB/s 且 16 路时，若逐 `64KiB` 直写则 `~6400次/s` 系统调用；合并到 `128KiB` 后降至 `~3200次/s`，与 aria2 的 piece 合并同效（`src/util.rs:305-335`）。
3. **流式追加是唯一稳妥**：`set_len` 预分配在 `ENOSPC` 时会残留“0 空洞”且 `truncate` 语义在 Windows 上与 `MoveFileExW` 冲突；`0.5.4 引入 0.6.2 校准`为 `truncate(false)` 已与 aria2 `--file-allocation=none` 对齐。
4. **延迟删防 AV 锁**：Windows 下 `.download.bitcode` 与目标文件同目录，AV/索引会持锁 `os5`；`queue.rs: flush_pending_deletes 200ms + retry` 与本项目 `resume.rs atomic_replace` 的 `best-effort` 同根同源。

## 5. 参考链接

- https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-file-allocation
- https://github.com/aria2/aria2/blob/master/src/DiskAdaptor.cc
- `src/util.rs:279-501 file_writer_task_impl`、`src/util.rs:305 COALESCE_LIMIT`
- `src/queue.rs: flush_pending_deletes`
