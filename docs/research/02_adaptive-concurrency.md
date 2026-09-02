# 02 自适应并发 Adaptive Concurrency — 主流方案对照

> 本项目：`Probing→Stable` 两阶段 + 最慢块优先 `bisect` + `tail/starved 急补` + `coalesce_small_fragments`（`src/concurrency.rs / monitor.rs:250-524`）

## 1. 问题

固定 `N` 线程要么跑不满带宽，要么在小文件/尾部空转；动态找“吞吐拐点”并只切值得切的块。

## 2. 主流实现

### 2.1 aria2 — 静态 `split + min-split-size + max-connection-per-server`

- `--split=N`（默认 5）+ `--min-split-size=20M` + `--max-connection-per-server=1` 决定最大切分数：`effective = min(split, file_size / min_split_size)`.
- 连接数在**启动时即确定**，中途不自适应；若设 `split=16` 且文件 `1.4G`，`20M` 阈值下实际 `~35` 片但受 ` split` 硬限，无法随带宽探测扩张。
- 尾部由 `PieceStorage` 按 piece 轮询补位，无“最慢块优先 bisect”。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#-s/--split,-k/--min-split-size,-x/--max-connection-per-server`（本次已拉取）

**结论**：配置驱动，非自适应；文件小或带宽抖时要么切不动，要么切太碎。

### 2.2 axel — 动态多连接 + 尾部抢占

- `axel -n 10 https://...` 启动即 `10` 连接，各自均分 `file_size / n`，快线程完成后**抢最慢线程的剩余区间后半段**（类似 `bisect`）。
- 无 `Probing` 探测期，无速度增益阈值判断，纯靠“谁快抢谁”。

> 来源：`https://github.com/axel-download-accelerator/axel` (`axel.c: search_speed()`)

### 2.3 lftp `pget -n`

- `pget -n 5 file` 预切 `5` 段，支持 `-c` 续传；并发数固定，无自适应扩张。
- 依赖 `mirror --parallel` 在多文件维度并发，非单文件内自适应。

### 2.4 Chrome / Firefox 单文件

- 单连接 `Range`，无并发；多文件靠并行下载项实现全局并发（对应本项目 `queue` 的 `max_concurrent`）。

### 2.5 IDM / JDownloader

- IDM 宣称“动态 8/16 线程 + 智能调度”，但无公开算法；JDownloader 按 `chunksPerFile` 固定切分，可配 `maxChunks` 上限，未公开探测逻辑。

## 3. 对比表

| 方案 | 启动并发 | 自适应 | 探测期 | 分裂策略 | 尾部处理 | 碎片合并 |
|---|---|---|---|---|---|---|
| aria2 | `--split` 静态 | ❌ | 无 | 启动即定，不中途扩张 | 轮询补位 | 无 |
| axel | `-n` 固定 | 半自适应 | 无 | 快者抢慢者后半段 | 抢占式 | 无 |
| lftp pget | `-n` 固定 | ❌ | 无 | 均分 | 无 | 无 |
| **simple_downloader** | 1 探测 | ✅ `Probing→Stable` | 带宽增益阈值 + 用满 `workers 0.8/0.6` | 最慢可分块优先 `bisect mid` + `incremental_id` 保序 | `starved(active=0)` / `tail 50%空闲 + 碎片` 绕过 `1s` 限流，`coalesce_small_fragments 256K→1M` | ✅ 相邻小孔洞合并 |

## 4. 对本项目的启示

1. **静态切分已过时**：aria2/axel 的“启动即定”在 `16并发 400MB/s` 下要么 `20M` 阈值卡死（`35片` 但 `split=16` 限死），要么 `split` 过大导致 `HEAD` 阶段即占满；本项目 `workers=1 起探 + 增益阈值` 更贴合“带宽未知”现实。
2. **分裂必须“值得才分”**：`monitor.rs: decide_and_act` 的“剩余工作量/时间”门限，避免 `active=1 + 极小尾部` 仍二分的无效抖动（axel 无此门限，尾部常切出 10KiB 碎片）。
3. **尾部急补是必选题**：`active=0` 时 `1000ms throttle` 会活锁；本项目 `starved` 旁路 + `coalesce` 合并 `<256K` 碎片，正是 axel “快抢慢”在高碎片场景下的工程化（`concurrency.rs: split_is_useful` 对 `active==0` 放宽为“有洞>split 即可分”）。
4. **增量 ID 保序**：`chunk_id = next_id +=1` 保证 bisect 产生的 `new_start` 单调递增，避免 aria2 式“piece 乱序”导致进度显示回退。
