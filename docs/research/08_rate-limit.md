# 08 限速 Rate Limit — 主流方案对照

> 本项目：`governor` 令牌桶 `1 token=1 byte` + `global + per_source` 双桶 `tokio::join max` + `burst 64KiB 硬限 + 自适应冻结 + 热更`（`src/limiter.rs / monitor.rs:524`）

## 1. 问题

限速要“硬上限、突发可配、限速时不误判为带宽不足而乱切、运行时可热更”。

## 2. 主流实现

### 2.1 aria2 — `--lowest-speed-limit`（下限）+ `--max-download-limit`（上限）

- `--lowest-speed-limit=0`：低于此速则关连接（防僵死），非限速；`--max-download-limit=0`（部分 fork 支持）全局限速，按 `piece` 限，非令牌桶。
- 无 per-source 限速，无 burst 概念，无热更。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-lowest-speed-limit`

### 2.2 trickle — `LD_PRELOAD` 限速

- `trickle -d 500 -u 100 curl ...` 拦截 `socket` 限全局带宽，令牌桶实现，但为进程级，非 per-source。

### 2.3 curl — `--limit-rate 500K`

- 单桶 `limit-rate`，`burst` 无暴露；实现为 `progress.c` 的 `sleep`，串行等待，与 `governor` 的 `join max` 相比慢 30%（本项目 `0.6.x 校准 join max` 即为此优化）。

### 2.4 governor crate

- GCRA 令牌桶，`Quota::per_second(1.mebibyte())` + `burst`：`1 token = 1 byte` 时 `burst` 即桶容量，符合直觉；`cargo test --test rate_limit 5MiB@1MiB/s 4-6.5s` 精度即靠此。
- 支持 `Arc<RateLimiter>` 共享，`global + per_source` 双桶 `join` 取 `max` 等待时间，避免串行 `sum` 膨胀。

## 3. 对比表

| 方案 | 模型 | 粒度 | burst | 双桶 | 热更 | 自适应冻结 |
|---|---|---|---|---|---|---|
| aria2 | `lowest-speed` 下限 / `max-download-limit` | 全局 | 无 | ❌ | ❌ | 无 |
| trickle | 令牌桶（进程级） | 全局 | 有 | ❌ | ❌ | 无 |
| curl `--limit-rate` | sleep 限速 | 全局单桶 | 无 | ❌ | ❌ | 无 |
| **simple_downloader** | `governor` GCRA `1 token=1 byte` | `global + per_source` 双级 | `64KiB` 硬限，`0/>4GiB/s` `InvalidArgument` | ✅ `join max` 防串行 30% 慢 | ✅ `RuntimeConfig + apply_config` `reconfigure/disable` | ✅ 限速时跳过 `decide_and_act`，`drain_pending` 除外 |

## 4. 对本项目的启示

1. **`1 token=1 byte` 是最直观**：`burst` 即“突发字节”，`64KiB` 恰为 `tokio::io` 默认缓冲，与 `governor` 示例一致；`>u32::MAX` 判 `InvalidArgument` 防 `Quota` 溢出。
2. **双桶必须 `join max` 并行**：早期 `per_source`→`global` 串行 `sleep sum` 会多 30% 延迟，`tokio::join` 取 `max` 已校准（`0.6.x 自适应冻结校准`）。
3. **限速时冻结自适应**：限速下的“慢”是人为的，非带宽不足；`monitor.rs:524` 限速时跳过 `ConcurrencyManager::decide_and_act`（仅 `drain_pending` 可补位）避免“限速→被判慢→再切→更慢”震荡。
4. **热更是 0.6.2+ 的差异化**：`cargo run --example with_rate_limit -- --multi` 演示 `global 512KiB/s` 硬上限下 `s1/s2 300KiB` 之和被压到 `512KiB`，且可运行时 `apply_config` 改速，aria2/curl 均需重启。

## 5. 参考链接

- https://docs.rs/governor/latest/governor/
- https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-lowest-speed-limit
- `src/limiter.rs`、`src/monitor.rs:123/524 apply_config`、`tests/rate_limit` 5 用例
- `examples/with_rate_limit.rs`
