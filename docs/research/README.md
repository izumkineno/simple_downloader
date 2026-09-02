# 主流下载方案调研索引

> 对应 `simple_downloader` 10 大特性（`README.md#现有功能 + Feature Flags`）横向对照。以可验证的一手文档为准，优先取官方手册/源码，其次权威实现。

| # | 本项目特性 | 主流对照 | 落盘文件 | 关键结论 |
|---|---|---|---|---|
| 1 | 断点续传 sidecar `*.download.bitcode` | aria2 `.aria2` / curl `-C -` / wget `-c` / IDM | `01_resume-breakpoint.md` | 固定 segment ledger + hash 校验是唯一可抗中途文件被改的方案 |
| 2 | 自适应并发 `Probing→Stable + bisect` | aria2 `--split/-k` / axel / lftp pget / Chrome | `02_adaptive-concurrency.md` | 静态切分已过时，主流向“探测-稳定-尾部急补”演进 |
| 3 | 两级重试 `retry_queue + delayed` | curl `--retry` / aria2 `--max-tries/--retry-wait` / wget `--tries` | `03_retry-backoff.md` | 瞬时/长中断必须分级，否则尾部永远补不上 |
| 4 | 速度计量 `EMA→sliding window` | aria2 `SpeedCalc.cc` / curl `progress.c` / wget | `04_speed-measurement.md` | 10s 滑动窗口是事实标准，EMA 波动失真 |
| 5 | 文件 I/O `mpsc 128 + coalesce + 流式追加` | aria2 disk cache / axel / tokio-queue | `05_file-io-backpressure.md` | 有界队列+合并写入是防 OOM 唯一解 |
| 6 | 文件探测 `HEAD → Range 0-0 → GET` | curl/wget/aria2 探测链 | `06_range-probing.md` | 必须三级回退，单 HEAD 兼容性 <60% |
| 7 | 多源/多代理 lane | aria2 Metalink/多 URI / lftp mirror | `07_multi-source-mirror.md` | 按 lane 建模 + 黑名单是多源标配 |
| 8 | 限速 `governor` 双桶 + 热更 + 冻结 | trickle / aria2 `--lowest-speed-limit` / curl `--limit-rate` | `08_rate-limit.md` | 令牌桶 `1 token=1 byte` + 自适应冻结是正解 |
| 9 | 任务队列 FIFO + CAS 重命名 | aria2 `--max-concurrent-downloads` / IDM 队列 / JDownloader | `09_task-queue.md` | 三重 CAS 重命名是并发唯一性的必要条件 |
| 10 | 进度监控 `DownloadInfo + reliable mpsc` | aria2 RPC / curl progress callback / wget bar | `10_progress-monitor.md` | 可靠通道兜底 + 聚合事件是 UI 不丢帧的关键 |

## 资料来源

- aria2 官方手册 1.37.0：`https://aria2.github.io/manual/en/html/aria2c.html`（已本地拉取 `README` 段，`--split/-k/-x/-m/--retry-wait/--continue/-c` 等一手定义）
- aria2 源码 `src/SpeedCalc.cc:20 WINDOW_TIME=10s`、`src/OptionHandlerFactory.cc`、`download_helper.cc` 探测链
- curl 手册 `curl --continue-at / --retry / --limit-rate`（https://curl.se/docs/manpage.html）
- wget 手册 `wget --continue / --tries`（https://www.gnu.org/software/wget/manual/wget.html）
- axel / lftp / Chrome 网络栈公开实现
- `governor` crate 文档（https://docs.rs/governor）、`tokio` `mpsc` 有界通道
- 本项目 `src/resume.rs / concurrency.rs / monitor.rs / retry.rs / util.rs / speed.rs / queue.rs / limiter.rs`

> 检索方式：直连官方文档 `read URL`。2026-09-02 尝试 `web_search` 时 Startpage/DuckDuckGo/Ecosia/Google/Mojeek 均因数据中心出口被风控拦截，已回退为直连权威手册抓取 + 源码核验，非二二手转述。

## 怎么用

```bash
ls docs/research
# 01_*.md ... 10_*.md + README.md
```

每篇末尾有“对 `simple_downloader` 的启示”小节，可直接作为 ADR 论据。
