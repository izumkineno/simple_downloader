# 07 多源与多代理调度 Multi-Source & Proxy — 主流方案对照

> 本项目：`MultiSourceConfig + SourceConfig::with_proxies + LaneModel(PerSource/PerSourceProxy) + max_chunks_per_lane/per_source + 黑名单 BLACKLIST_THRESHOLD=3/30s`（`src/lane.rs / concurrency.rs / downloader.rs`）

## 1. 问题

同一文件多镜像/多代理时：选谁、限几路、失败是否传染、全局限速如何分配。

## 2. 主流实现

### 2.1 aria2 — 多 URI + Metalink + URI Selector

- 输入文件/命令行可传多个 URI 同一文件：`aria2c "http://mirror1/file" "http://mirror2/file"`；`--split` 个连接按 `URI Selector: default/feedback/adaptive` 分配到各 URI。
- **Metalink**：`*.metalink` 带多 `<url>` + `piece hash` + `maxconnections`，天然多源 + 校验；`--metalink-enable-unique-protocol=false` 可同文件 HTTP+BT 混下。
- **Server Performance Profile**：`--server-stat-of/if` 持久化各服务器的下载速度/失败率，`--uri-selector=feedback` 优先快源。
- **代理**：`--all-proxy/--http-proxy/--https-proxy` 全局代理，无 per-source 代理维度。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html#-i/--input-file,--uri-selector,--server-stat-*`（本次已拉取）、`https://github.com/aria2/aria2/blob/master/src/URISelector*.cc`

### 2.2 lftp — `mirror --parallel + pget`

- `mirror --parallel=3` 多文件并行，单文件 `pget -n 5` 静态切分；`mirror` 支持多源镜像列表，但无 lane 级限速/黑名单。
- 代理靠 `http:proxy` 全局。

### 2.3 curl / wget

- 单 URI 模型，多源需上层脚本轮询；curl 8.x 新增 `Parallel with curl --parallel` 但为多文件，不为单文件多镜像分片。

### 2.4 Chrome / IDM

- Chrome：单源单连；IDM：多镜像“智能分段”但无公开 lane 抽象。

## 3. 对比表

| 维度 | aria2 | lftp | curl/wget | **simple_downloader** |
|---|---|---|---|---|
| 多源输入 | 多 URI / Metalink | mirror 列表 | 单 URI | `MultiSourceConfig::with_sources(vec![SourceConfig])` |
| 调度维度 | URI Selector + ServerStat | 固定 | 无 | `LaneModel::PerSource` vs `PerSourceProxy`（源×代理笛卡尔） |
| 并发上限 | `--split/--max-connection-per-server` 全局 | `pget -n` | 无 | `max_chunks_per_lane + max_chunks_per_source` 双级 |
| 失败隔离 | `feedback` 降权 | 无 | 无 | `≥3 次连续失败 → 30s 黑名单` + `MultiRuntime::from_config 64KiB probe_speed 排序` |
| 限速 | 全局 | 无 | `curl --limit-rate` 单桶 | `governor` `global + per_source` 双桶 `join max` + 热更 |
| 校验 | Metalink piece hash | 无 | 无 | `resume` 固定 segment hash 跨源通用 |
| 本地验证 | 无 | 无 | 无 | `tests/multi_source.rs` 多限速源 + `manual_multi_source_test_server 500M fast16m/slow2m` |

## 4. 对本项目的启示

1. **lane 抽象是多源的必要抽象**：aria2 的 URI Selector 仍停留在“挑 URL”，本项目 `LaneModel` 把“源×代理”拍扁为 `lane_id`，使 `max_chunks_per_lane` 可对 `http+proxyA` 与 `http+proxyB` 独立限流，与 aria2 未来的 `per-proxy` 需求等价。
2. **双级限速 + 黑名单缺一不可**：aria2 的 `ServerStat` 是离线持久化，本项目 `BLACKLIST_THRESHOLD + 30s 解封 + probe_speed 排序` 是在线版；在 `test_server fast16m/slow2m` 实测下，快源吞吐是慢源 8 倍，若无黑名单则慢源的 `decoding` 会传染快源（已在 `01:54 7并败` 观测）。
3. **热更全局限速是 0.6.2 后的优势**：aria2 限速需重启，`DownloadMonitor::apply_config` 已支持 `RuntimeConfig{speed_limit,burst}` 热更，代理维度的 `per_source` 热更待接（`TODO 0.7 EWMA 动态评分`）。
4. **Metalink 已被对象存储取代**：Metalink 的 piece hash 在 HF/ModelScope 上无提供，本项目 `resume` 的 `hash_bytes` 自校验是更普适的替代。

## 5. 参考链接

- https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-s
- https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-uri-selector
- `src/lane.rs`、`src/concurrency.rs: MultiRuntime`、`src/downloader.rs: new_multi`
- `tests/multi_source.rs`、`examples/manual_multi_source_test_server.rs`
