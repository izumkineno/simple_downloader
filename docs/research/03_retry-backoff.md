# 03 重试与退避 Retry & Backoff — 主流方案对照

> 本项目：两级队列 `retry_queue(即时, RETRY_DELAY=1s) + delayed_retry_queue(DELAYED=10s)` + `MAX_RETRIES=10 / MAX_TOTAL=50` + `batch 500ms/200ms 抖动` + `decoding 降级 DEBUG`（`src/retry.rs:1-200+`）

## 1. 问题

HTTP 常见三类失败：A) 瞬时抖动（TCP 重传/decoding）应在 1s 内重试；B) 服务端限流/503 应退避数秒；C) 长时间断网应 10s 级延迟重试，避免“尾部一块失败全链卡死”。

## 2. 主流实现

### 2.1 aria2 — `--max-tries + --retry-wait + --timeout`

- `--max-tries=5`（0=无限）+ `--retry-wait=0`（>0 时对 503 自动重试，指数等待）+ `--timeout=60` 连接超时 + `--max-file-not-found=0`。
- 单级重试：失败即等 `retry-wait` 后重连，无“即时 vs 延迟”两级；未区分 `decoding` 瞬时失败与 503。
- 源码 `src/HttpResponse.cc` 对 5xx 直接进 `retry`，无 `per-chunk` 总次数上限。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#-m/--max-tries,--retry-wait,--timeout`

### 2.2 curl — `--retry / --retry-delay / --retry-max-time / --retry-all-errors`

- `curl --retry 5 --retry-delay 2 --retry-max-time 60`：仅对 `5xx/网络失败` 重试，默认 `GET` 才 retry，`--retry-all-errors` 扩到所有。
- 指数退避：`retry-delay * (2^n)`，无二级队列。
- `--continue-at -` 与重试正交，重试时自动带 `Range`。

> 来源：`https://curl.se/docs/manpage.html#--retry`

### 2.3 wget — `--tries / --waitretry / --timeout`

- `wget --tries=20 --waitretry=10 --timeout=60 -c`：失败即等 10s，重试次数含 `file-not-found`（类似 aria2）。
- 无 per-chunk 概念，整文件重试。

### 2.4 axel / lftp

- axel：连接断开即重连该线程的剩余区间，`max-redirects` 限跳，无退避；lftp：`net:reconnect-interval-base/multiplier` 指数退避。

## 3. 对比表

| 维度 | aria2 | curl | wget | **simple_downloader** |
|---|---|---|---|---|
| 分级 | 单级 `retry-wait` | 单级指数 | 单级 | **两级：即时(1s) / 延迟(10s)** |
| 单块上限 | `max-tries` 全局 | `retry` 全局 | `tries` 全局 | `MAX_RETRIES=10` per-chunk + `MAX_TOTAL=50` per-download |
| 尾部急补 | 无 | 无 | 无 | `active=0` 时 `force_drain_delayed + pop_starved` 绕过 1s |
| 惊群抑制 | 无 | 指数退避 | 固定等待 | `500ms 内≥3并败 → 200ms*queue_len 抖动` |
| 瞬时降噪 | 无 | 无 | 无 | `error decoding → DEBUG` 防 16路风暴 |
| 延迟重置 | 无 | 无 | 无 | `process_queues` 设 `failure_time=now-RETRY_DELAY` 立即可 pop |

## 4. 对本项目的启示

1. **两级是尾部问题的根解**：aria2/curl 在 `7/16 并败` 后，单级 `retry-wait=0` 会“1s 内 7次即时重试→又失败→进 10s 延迟→tail 空转 9s”。本项目 `MAX_RETRIES=10 后转 delayed(10s)` + `active=0/tail碎片 时 force_drain` 保证“小尾巴也能即时补”。
2. **batch 抖动必不可少**：16 路同时 `error decoding`（常见于 HTTP/2 RSS + 服务器限流）若同在 `t+0` 推 `retry_queue`，会同在 `t+1s` 再并发 16 路重连→再被限流。`500ms 窗 + 200ms*len` 抖动将 7 路错开到 `1.0/1.2/1.4...s`，与 curl 指数退避异曲同工但更轻。
3. **per-chunk + per-download 双上限**：aria2 的 `max-tries=5` 是全局下载的 try，非单 piece 的 try；本项目 `10/50` 双上限既防单块无限重试，又防整文件因 1 块卡死（`permanent_failures`）。
4. **日志分级**：aria2/curl 对 `decoding` 与 `503` 同级 `WARN`，16路并败即刷屏。本项目 `decoding→DEBUG` 已在 `01:54 7路并败` 实测中证明可将 `14 WARN+7 ERROR` 压到 0。
