# 04 速度计量 Speed Metering — 主流方案对照

> 本项目：早期 `EMA(α=0.1/速度)` → 修复为 `SpeedCalc 10s 滑动窗口 1s 桶`（`src/speed.rs + monitor.rs + docs/CHANGELOG 0.6.6`），`b4e8e8b` 前 EMA 错把瞬时波动平滑成“恶性突增 300→600MB/s”

## 1. 问题

下载速度要“准、稳、抗尾部膨胀”。EMA 对“块完成瞬间 `total_downloaded` 跃迁 + 并发回收”敏感，会算出翻倍假速度并触发错误 `bisect`。

## 2. 主流实现

### 2.1 aria2 — `src/SpeedCalc.cc:20 WINDOW_TIME=10s`

```cpp
// 伪
const int WINDOW_TIME = 10; // 秒
void update(bytes) { if (same_sec) merge_tail; else push_bucket(now, bytes); }
double calculateSpeed() { return bytesWindow*1000 / elapsed; } // elapsed = now - oldest
double calculateAvgSpeed() { return accumulated*1000 / totalElapsed; }
```

- 10s 滑动窗口，1s 一桶，同秒合并尾桶；`elapsed = now - oldest`，窗口未满 1s 则不更新速度，避免“1块完成即×N”膨胀。
- 代码 `SpeedCalc.cc:20-60` 可直接核验。

> 来源：`https://github.com/aria2/aria2/blob/master/src/SpeedCalc.cc`、`src/DownloadContext.cc` 速度采样

### 2.2 curl — `lib/progress.c:488`

- `CURL_SPEED_RECORDS` 环形缓冲，`>=1s` 新建 `speed_amount/speed_time`；`current_speed = (latest-oldest)*1e6 / (latestTime-oldestTime)`，同 aria2 10s 思想但窗口 5-10s 可配。
- `avg_speed = total / total_time`，与 `calculateAvgSpeed` 一致。

> 来源：`https://github.com/curl/curl/blob/master/lib/progress.c`

### 2.3 wget — `src/progress.c`

- `1s` 定时采样，`speed = bytes_since_last_tick / 1s`，无滑动窗口，抖动大。

### 2.4 Chrome / IDM

- Chrome：`1s` 采样 + `α=0.3` EMA 展示；IDM：`0.5s` 采样 + 平滑。

## 3. 对比表

| 方案 | 窗口 | 桶粒度 | 同秒合并 | 未满 1s | 尾部膨胀 | 公式 |
|---|---|---|---|---|---|---|
| aria2 | 10s 滑动 | 1s | ✅ 合并尾桶 | 不更新 | ✅ 按窗口 elapsed 消除 | `bytesWindow/elapsed` |
| curl | 5-10s 环形 | 1s | ✅ | 不更新 | ✅ | `(latest-oldest)/time` |
| wget | 无 | 1s 瞬时 | ❌ | 直接除 1s | ❌ 完成瞬间翻倍 | `bytes/tick` |
| **simple_downloader 现** | 10s 滑动 | 1s | ✅ | 不更新 | ✅ 完成即删旧桶 | 同 aria2 |
| simple_downloader 旧 EMA | 无 | per-tick | — | — | ❌ `total_downloaded/min(total,completed)` 膨胀 | `EMAα=0.1` |

## 4. 对本项目的启示

1. **EMA 必须废**：`0.3→0.6.3` 回归中 `state.rs:187 total_downloaded>=total` + EMA 导致 `100% 卡死 + 速度翻倍`，根因即 EMA 把“块完成瞬间”当持续吞吐。已在 `6783018` 修复为窗口，与 aria2/curl 对齐。
2. **窗口 10s 是事实标准**：aria2 `WINDOW_TIME=10s` + curl `CURL_SPEED_RECORDS` 均选 10s；过短则抖，过长则滞后。本项目 `speed.rs:5.1KB` 选同值，无需再调。
3. **同秒合并消尾桶分裂**：400MB/s 下 16 路并发，单秒内 7 块完成会产生 7 桶；aria2 的 `same_sec 合并尾桶` 保证 `elapsed` 不被碎片化，本项目同理。
4. **限速下速度=令牌消耗**：`governor` 限速时速度计应采 `actual_write_bytes` 而非 `downloaded_bytes`，否则限速被算成“带宽不足”误触发 `bisect`（本项目 `monitor.rs:524` 自适应冻结即为此）。

## 5. 参考链接

- https://github.com/aria2/aria2/blob/master/src/SpeedCalc.cc#L20
- https://github.com/curl/curl/blob/master/lib/progress.c#L488
- `src/speed.rs`、`src/monitor.rs:250` `task joined remaining_tasks/p pending_bisects`
- `docs/CHANGELOG 0.6.6` “window+status+stall decay”
