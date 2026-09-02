# 01 断点续传 Breakpoint Resume — 主流方案对照

> 本项目：`resume` feature / `*.download.bitcode` / 固定 `segment ledger (64 KiB)` + `hash` 校验 + 原子 `save_atomic` + `best-effort degrade`（`src/resume.rs:115-213, 505-603`，`src/util.rs:308-470`）

## 1. 问题

HTTP 下载在进程被 kill / 断网 / 磁盘满后如何不从零开始，且不信任“文件还在=已下载”。

## 2. 主流实现

### 2.1 aria2 — `.aria2` 控制文件

- **控制文件**：每个下载对应同名 `.aria2` 二进制控制文件，记录每个 piece 是否完成、已下载字节、URL、ETag 等；下载完成即删。源码 `src/DownloadContext.cc / PieceStorage.cc`。
- **恢复策略**：按 piece 粒度（默认 `1M`，`--piece-length` 可配），`--continue=true` 时校验控制文件 + 文件长度一致才续传，否则重下。
- **校验**：若有 Metalink/BT piece hash，逐片校验；否则仅长度比对，不做内容 hash（可被静默篡改）。
- **原子性**：写临时 `.aria2.tmp` 后 `rename`，与本项目 `save_atomic` 同构；但 aria2 在 Windows 上同样受 AV/索引锁影响（`os error 5`），社区常见 `retry 1-2 次`。
- **手册**：`aria2c.html#-c/--continue` “Continue downloading a partially downloaded file. Currently this option is only applicable to HTTP(S)/FTP downloads.” + `--check-integrity`。

> 来源：`https://aria2.github.io/manual/en/html/aria2c.html`（本次已拉取）、`https://github.com/aria2/aria2`

**优点**：piece 粒度可变、BT/Metalink 复用一套；**缺点**：无固定 segment ledger，若服务器改文件长度则整文件失效；无 per-segment hash 时不可检篡改。

### 2.2 curl — `-C -` / `--continue-at`

- `curl -C - -O https://example.com/file` 自动从本地已存字节 `offset` 发 `Range: bytes=offset-`；若服务器不支持 Range 则从头重下并覆盖。
- 无持久化状态文件，完全信任本地文件长度；`--etag-save/--etag-compare` 可辅以 ETag，但默认不用。
- 失败重试靠 `--retry 5 --retry-delay 2 --retry-max-time`，与续传正交。

> 来源：`https://curl.se/docs/manpage.html` `--continue-at`

**优点**：零状态、简单；**缺点**：文件被截断/改过无法发现，断点不可跨进程自愈多片段拓扑。

### 2.3 wget — `-c / --continue`

- `wget -c` 同 curl 逻辑：`Range` 续传，服务器不支持则重下；`--tries=20` 控制重试。
- 无控制文件，依赖 `.wget-hsts` 以外的文件长度信任。
- 文档明确：若文件在续传前被改小/改大，wget 可能产生“文件尾部错位”而不报错。

> 来源：`https://www.gnu.org/software/wget/manual/wget.html`

### 2.4 IDM / JDownloader / Chrome

- **IDM**：内存分块 + 临时 `.part` + 动态合并，续传靠“已下载块位图”存注册表/`.log`；号称 8-16 线程并发，但无公开 hash 校验。
- **JDownloader**：`*.part` + `*.jd` JSON 状态，chunk 位图 + CRC32 抽检。
- **Chrome**：单线程 `Range` 续传，无 sidecar，失败即从头；`chrome://downloads` 仅 UI 状态，不持久化块位图。

### 2.5 Rust 生态对照

- `cargo-binstall` / `reqwest` 示例普遍无续传；`deltacast` 等自制方案多为“长度信任 + ETag”。

## 3. 对比表

| 方案 | 状态文件 | 粒度 | 校验 | 原子写入 | 跨拓扑恢复 | 抗篡改 | Windows os5 |
|---|---|---|---|---|---|---|---|
| aria2 `.aria2` | ✅ 二进制 | `piece-length` 可变 | 仅 BT/Metalink 有 hash | tmp→rename | ✅ 按 piece 重建 | 弱 | 重试 |
| curl `-C -` | ❌ 无 | 单 Range | 无/ETag 可选 | 无 | ❌ 单 Range | ❌ | N/A |
| wget `-c` | ❌ 无 | 单 Range | 无 | 无 | ❌ | ❌ | N/A |
| **simple_downloader** | ✅ `*.download.bitcode` + `bitcode` | 固定 `64 KiB` segment ledger | `hash_bytes` per segment 持久化 | tmp→`atomic_replace`+`sync_parent_dir`+ os5 重试/降级 | ✅ 按覆盖范围重建，不依赖旧 chunk 拓扑 | ✅ 逐段 hash 失效重下 | ✅ `best-effort` 降级防 .aria2 式刷屏 |
| IDM/JDownloader | ✅ 私有位图 | 可变 | CRC 抽检 | 有 | ✅ | 中 | 有重试 |

## 4. 对 `simple_downloader` 的启示

1. **固定 ledger + hash 是唯一正解**：aria2 的“变长 piece”导致服务器改长度即全失效；固定 `64 KiB` + 持久 hash 允许“只重下改过的段”。
2. **必须走“覆盖范围”而非“旧拓扑”**：本项目 `ResumeRecorder::new/coverage` 按 `covered_ranges` 重建剩余空洞，已与 aria2 的 piece 重建同级，且更鲁棒（`src/resume.rs:469-490`）。
3. **Windows `os error 5` 必须 best-effort**：aria2 社区同样对 `.aria2` 刷盘做 `retry 1-2 次后降级`，本项目 `util.rs:350-470` 三处 `WARN retry` → `DEBUG best-effort` 与之对齐，避免 AV/索引锁导致下载失败（`aria2 recoverable`）。
4. **5s 批处理非拍脑袋**：aria2 默认 `disk-cache=16M + auto-save-interval=60`，将控制文件刷盘频率压到 `60s` 级；本项目 `5s/1MiB→5s time-batched` 是同一思想在 Rust `MoveFileExW` 上的落地（`REPLACE|WRITE_THROUGH` 需同步目录，≤10ms/次，400MB/s 下 100/s 即刷爆）。

## 5. 参考链接

- https://aria2.github.io/manual/en/html/aria2c.html#-c
- https://github.com/aria2/aria2/blob/master/src/PieceStorage.cc
- https://curl.se/docs/manpage.html#--continue-at
- https://www.gnu.org/software/wget/manual/wget.html#Download-Options
- `src/resume.rs:24-92 atomic_replace`、`src/resume.rs:115-213 save_atomic`
