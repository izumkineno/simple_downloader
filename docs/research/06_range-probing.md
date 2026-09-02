# 06 Range 探测与兼容 Probing — 主流方案对照

> 本项目：`get_file_info HEAD → Range 0-0 GET → Content-Length` 三级回退 + `Accept-Ranges/206/Content-Range` 三重判定（`src/util.rs:36-191`）

## 1. 问题

部分 CDN/对象存储对 `HEAD` 返回 `403/405` 或无 `Content-Length`，对 `Range` 返回 `200` 而非 `206`；单靠 `HEAD` 兼容性不足 60%。

## 2. 主流实现

### 2.1 aria2 — `download_helper.cc: probe`

1. `HEAD` 取 `Content-Length` + `Accept-Ranges: bytes`
2. 失败则 `GET Range: bytes=0-0` 解析 `Content-Range: bytes 0-0/12345`
3. 再失败则取 `GET` 的 `Content-Length`（整文件长度）
- 同时校验 `200 vs 206`：`200` 视为不支持 Range，不切分。

> 来源：`https://github.com/aria2/aria2/blob/master/src/download_helper.cc`、`https://aria2.github.io/manual/en/html/aria2c.html#http-ftp-options`

### 2.2 curl — `-I + --range`

- `curl -I https://...` 取 `HEAD`，失败则 `curl -r 0-0 -i` 取 `Content-Range`；`curl --head` 对 `405` 自动回退到 `GET -r 0-0`（与本项目一致）。
- 源码 `tool_operate.c: getinfo`。

### 2.3 wget

- `wget --spider --server-response` 先 `HEAD`，`406` 则 `GET`；对 `Content-Range: bytes */123`（`*` 形式）同样支持解析为总长。

### 2.4 本项目特有细节

- `Content-Range` 解析同时处理 `bytes 0-0/123` 与 `bytes */123` 两种形式（`src/util.rs:192` 注释）。
- `User-Agent` 注入 `ensure_user_agent` 防 `ToDesktop/CND` 严格 UA 校验（`fix: add default User-Agent to handle ToDesktop strict UA check`）。
- 未显式传 `workers` 时 `probe` 结果直接决定 `initial chunk` 数量，`support_ranges=false` 时退化为单连接整文件下载（`monitor.rs: handle_tick` 判 `is_finished`）。

## 3. 对比表

| 方案 | 一级 | 二级 | 三级 | Content-Range 双格式 | 206 判定 | UA 注入 |
|---|---|---|---|---|---|---|
| aria2 | HEAD | GET 0-0 | GET Content-Length | ✅ | ✅ `200→不支持` | 无 |
| curl | HEAD | GET 0-0 | GET | ✅ | ✅ | 有 |
| wget | HEAD | GET | — | ✅ | ✅ | 有 |
| **simple_downloader** | HEAD | GET 0-0 | GET Content-Length | ✅ `192: bytes 0-0/123 + bytes */123` | ✅ `support_ranges` 严格 | ✅ `ensure_user_agent` |

## 4. 对本项目的启示

1. **三级回退是刚需**：实测 ModelScope / HF / 阿里云对 `HEAD` 行为不一致（ModelScope `HEAD 403` 需 `GET 0-0`），单级 HEAD 在本项目 `manual_multi_source_test_server 500MiB` 场景下会误判为 `size=0`。
2. **`bytes */total` 必须支持**：部分 CDN 对 `Range 0-0` 返回 `Content-Range: bytes */12345`（416 语义的友好形式），不解析即拿不到 `total` 而 `0.6.2` 前 `416 from 0,0` 曾触发 ` UnexpectedEof`。
3. **不支持 Range 必须 fail-fast 不切分**：aria2 对 `200` 不切分，本项目 `support_ranges=false` 时 `monitor` 跳过 `bisect`，避免“切了但服务器返回 200 全文”导致 `offset` 越界。
4. **UA 注入非多余**：`ToDesktop` 等严格 UA 校验会使 `HEAD` 被 WAF 拦截，`ensure_user_agent` 是探测链可达性的前置，与 curl 的默认 UA 同理。

## 5. 参考链接

- https://github.com/aria2/aria2/blob/master/src/download_helper.cc
- https://aria2.github.io/manual/en/html/aria2c.html
- `src/util.rs:36-191 get_file_info`、`src/util.rs:16-33 ensure_user_agent`
