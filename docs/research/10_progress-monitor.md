# 10 进度监控 Progress & Monitor — 主流方案对照

> 本项目：`progress` feature / `DownloadInfo` 枚举 + `MonitorUpdate{progress_percent,speed_mbps,downloaded_bytes,total_bytes,is_complete}` 聚合 + `DownloadState + SpeedCalc 10s 窗 + reliable mpsc + broadcast 4096`（`src/types.rs / monitor.rs / state.rs / speed.rs`）

## 1. 问题

进度要“不丢帧、不回退、不刷屏、跨线程聚合一致”。

## 2. 主流实现

### 2.1 aria2 — RPC `aria2.tellStatus / tellActive / tellWaiting`

- `aria2c --enable-rpc --rpc-listen-port=6800` 后轮询 `tellStatus(gid)` 得 `completedLength/totalLength/downloadSpeed/connections/pieces`。
- `pieces` 位图 + `bitfield` 展示分块进度，`downloadSpeed` 来自 `SpeedCalc 10s 窗`（`src/SpeedCalc.cc`）。
- 事件靠 `onDownloadComplete/onDownloadError` hook，非推送。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface`、`src/SpeedCalc.cc:20`

### 2.2 curl — `CURLOPT_PROGRESSFUNCTION / --progress-bar`

- `curl_easy_setopt(curl, CURLOPT_PROGRESSFUNCTION, cb)` 每 `progress_interval` 回调 `dltotal/dlnow/ultotal/ulnow`；`--progress-bar` 终端条。
- 速度 `progress.c:488` 环形缓冲，与 aria2 同级。

### 2.3 wget — `dot + bar + percent`

- `wget --progress=bar:force` 每 `1s` 刷 `percent/speed/ETA`，单线程无需聚合。

### 2.4 indicatif / Rust 生态

- `indicatif::ProgressBar` 按 `tick` 更新，需上层自行 `inc(n)`；无“可靠终局”概念。

## 3. 对比表

| 维度 | aria2 RPC | curl callback | wget bar | **simple_downloader** |
|---|---|---|---|---|
| 推送 | 轮询 `tellStatus` | 回调 | 终端直刷 | `broadcast 4096 + reliable mpsc` 双通道推送 `DownloadInfo` |
| 聚合 | `completed/total + speed` | `dltotal/dlnow` | 同上 | `MonitorUpdate{progress_percent,speed_mbps,downloaded/total,is_complete}` 稳定契约 `#[non_exhaustive]` |
| 丢帧 | 无（轮询） | 无 | 无 | `broadcast Lagged` 丢终局已由 `reliable mpsc` 兜底（`b4fcadf`） |
| 回退 | 无 | 无 | 无 | `completed_bytes = min(sum, total)` 防 `bisect` 时 `4→4+16` 虚算 `1.7G>1.47G` 回退 |
| 刷屏 | `auto-save-interval` 限频 | `progress_interval` | 1s | `ChunkProgress throttle 64KiB/50ms + last_reported` 限频，`decoding→DEBUG` 防 7路风暴 |
| 契约稳定 | RPC JSON | C 回调 | 终端 | `DownloadInfo #[non_exhaustive]` + `MonitorUpdate` 字段/状态码稳定，新增变体为 minor（`src/types.rs` + `docs/usage.md#6`） |

## 4. 对本项目的启示

1. **双通道是丢帧的根解**：`broadcast 4096` 高频 `ChunkProgress` 在 `16路 400MB/s` 下 `50ms/64KiB` 限频仍可能 `Lagged` 丢 `Complete`；`reliable mpsc` 对 `Complete/Failed/Bisected` 三终局可靠送达，已在 `monitor.rs: handle_download_info` 与 `chunk.rs: send_terminal_event` 双端验证。
2. **聚合事件优于原始事件**：UI 不应 `sum ChunkProgress` 自算 `progress_percent`（会受 stale 影响），而应只依赖 `MonitorUpdate` 聚合；`state.rs: completed_bytes = min(sum,size)` 已防 bisect 时虚算。
3. **限频必须在源头**：`last_reported + 64KiB/50ms` 节流在 `chunk_run` 源头做，比 aria2 的 `auto-save-interval` 更细；`monitor` 侧 `50ms` 再聚合同窗口，两级限频保证 `with_custom_ui 16路` 不刷屏。
4. **稳定契约是库的 API 承诺**：`#[non_exhaustive]` + `match _` 强制 UI 兼容未来新增变体，与 aria2 的 `gid + JSON` 稳定契约同理；`0.6.2+` 已将 `progress_percent/speed_mbps/is_complete` 行为文档化为“非 MonitorUpdate 返回 0/false”。

## 5. 参考链接

- https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface
- https://github.com/aria2/aria2/blob/master/src/SpeedCalc.cc
- `src/types.rs: DownloadInfo #[non_exhaustive]`、`src/monitor.rs: handle_download_info`、`src/state.rs: completed_bytes`
- `examples/with_custom_ui.rs`、`src/speed.rs`
