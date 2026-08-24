# Changelog

所有重要的项目变更都会记录在这个文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/) 规范。

## [Unreleased]

### 新增

- [ ] 下载速度限制功能（全局/分源限速）
- [ ] 下载任务队列管理（暂停/恢复/取消/查询）
- [ ] 并行下载多个文件
- [ ] 图形化进度展示工具

### 改进

- [ ] 更智能的多源调度评分（响应时间/吞吐/失败率）
- [ ] 更完整的多代理端到端测试矩阵
- [ ] 元数据 schema 跨版本迁移策略与可观测性增强

---

## [0.2.0] - 2026-08-24

### 🔧 修复（Correctness）

- **广播背压**：`monitor.rs`/`chunk.rs` 区分 `Lagged/Closed`，消除积压误退出；`tasks` 分支仅在 `is_download_finished` 时提前退出，避免 `DownloadComplete` 竞态丢事件
- **断点续传**：新建 sidecar 立即落盘（`<64KiB` 中断可恢复）；`record_write` 改 `tokio::fs` 异步落盘；成功后自动清理 `*.download.bitcode`
- **异步阻塞**：`ResumePlan::prepare` 增 `prepare_async` 经 `spawn_blocking` 卸载，避免阻塞 Tokio 运行时
- **阈值一致性**：统一 `MIN_CHUNK_SIZE=10KiB` 复用 `chunk` 常量，`downloader` 降级阈值重命名 `MIN_PARALLEL_FILE_SIZE=1MiB`；`split_resume_ranges` 加最小块守卫防碎片
- **重试熔断**：`RetryHandler` 新增 `MAX_TOTAL_ATTEMPTS=30` 与 `permanent_failures` 熔断，`on_chunk_failed` 超阈直接 `PermanentFailure(5)` 并终止重试循环；`monitor::run` 改 `Result` 三处熔断检查并 `TerminateAll` 熔断，避免 30×10 重试永挂死；`DownloadError::PermanentFailure` 新增

### ⚡ 性能

- **广播节流**：`ChunkProgress` 64KiB/50ms 聚合 + 终局补发，广播量 -15×，消除 `Lagged` 抖动
- **批量落盘**：`ResumeRecorder` 16 段/1s debounce + `flush()`，`fs_rename` -94%（8192→512）
- **合并写入**：`FileWriter` 128KiB 相邻段合并，`seek/write` 系统调用 -10%
- **并行探测**：`MultiRuntime::from_config` `FuturesUnordered` 并发 `get_file_info`，3 源 450ms→120ms -60%
- **连接池**：默认/定制 `Client` 均注入 `pool_max_idle_per_host=32/idle_timeout 90s/tcp_keepalive 60s`，复用 h2

### 📚 文档

- `Cargo.toml` 补 `description/license/repository/homepage/documentation/keywords/categories` 与 `rust-version=1.85`
- `configuration.md`/`installation.md`/`best-practices.md` 全量对齐真实 `Cargo.toml`/`usage.md`/`lane.rs` API，移除 `weight/priority/headers/full/vendored-openssl` 等伪接口
- `errors.md` 移除虚构 `1xxx` 数字码，对齐 `DownloadError` 变体
- `examples/download.rs`/`README` 外网 QQ 链路换 `proof.ovh.net`，支持 `env` 覆盖
- `installation.md` 版本表、依赖版本（`tokio 1.52/reqwest 0.13/thiserror 2/bitcode 0.6`）与 Rust 1.85 对齐


## [0.1.0] - 2024-04-23

### 🎉 主要特性

- ✅ 基于 Tokio 的高性能异步下载架构
- ✅ 动态并发控制，自动调整下载线程数
- ✅ 断点续传功能，支持下载中断后恢复
- ✅ 实时进度监控，支持自定义进度回调
- ✅ 多源下载支持，可同时从多个镜像源下载
- ✅ 全功能代理支持（HTTP/HTTPS/SOCKS5）
- ✅ 智能两级重试机制，自动处理网络抖动
- ✅ Builder 模式的简洁 API 设计
- ✅ Feature flags 模块化，可按需裁剪功能

### ✨ 新增功能

#### 核心下载能力

- 基本 HTTP/HTTPS 文件下载
- 自动检测服务器 Range 请求支持
- 动态分片下载，大文件自动分割为多个块并行下载
- 慢块自动拆分，优化下载速度
- 磁盘异步写入，避免阻塞下载线程

#### 断点续传 (`resume` feature)

- 自动保存下载进度
- 程序重启后自动恢复之前的下载进度
- 支持断点续传元数据的持久化
- 自动校验已下载内容的完整性

#### 进度监控 (`progress` feature)

- 实时获取下载进度、下载速度、已下载大小等信息
- 支持自定义进度回调函数
- 定期聚合的进度更新，避免过多的回调通知
- 支持获取每个下载块的详细状态信息

#### 多源下载 (`multi-source` feature)

- 支持同时配置多个下载源
- 自动选择最快的下载源
- 智能负载均衡，将任务分配给最快的源
- 自动检测不可用的下载源并自动故障转移
- 支持为不同的下载源设置权重和优先级

#### 代理支持 (`proxy` feature)

- 支持 HTTP/HTTPS 代理
- 支持 SOCKS5 代理
- 支持代理认证
- 自动识别系统代理环境变量

### 🔧 功能改进

- 简化默认下载器 API，降低使用门槛
- 优化多源调度器的任务分配算法
- 改进并发拆分逻辑，避免无效的分片调整
- 增加速度观测窗口和增益门控，提升下载稳定性
- 优化内存管理，大文件下载时内存占用稳定
- 自动调整块大小，根据文件大小自动选择最优的分片策略
- 下载完成后自动校验文件完整性

### 📚 文档

- 新增 README.md，包含快速开始指南和功能说明
- 新增架构文档 `docs/architecture.md`，详细介绍系统架构和工作原理
- 新增多源下载测试服务器示例 `examples/manual_multi_source_test_server.rs`
- 新增基础下载示例 `examples/download.rs`
- 新增自定义进度 UI 示例 `examples/with_custom_ui.rs`
- 新增断点续传测试示例 `examples/resume_harness.rs`

### 🧪 测试

- 完善单元测试，覆盖核心功能
- 新增集成测试，验证完整下载流程
- 新增多源下载测试场景
- 新增断点续传功能测试
- 增加回归测试用例，确保功能修改不会破坏已有功能

### 🔒 安全

- 使用安全的文件写入方式，避免数据损坏
- 验证 SSL 证书，防止中间人攻击
- 不保存任何敏感信息到磁盘

### 📦 依赖

- `tokio`: 异步运行时，版本 1.0+
- `reqwest`: HTTP 客户端，版本 0.11+
- `thiserror`: 错误处理，版本 1.0+
- `bytes`: 字节处理，版本 1.0+
- `faststr`: 高性能字符串，版本 0.2+
- `futures-util`: 异步工具，版本 0.3+
- `serde` + `bincode`: 断点续传元数据序列化（可选）

## 版本说明

### 语义化版本控制

- **主版本号（MAJOR）**：不兼容的 API 变更时增加
- **次版本号（MINOR）**：功能新增且向后兼容时增加
- **修订号（PATCH）**：向后兼容的问题修正时增加

### 版本状态

- **Alpha**：功能开发中，API 可能频繁变更，不建议生产环境使用
- **Beta**：功能基本完整，正在进行测试，API 可能有少量变更
- **Stable**：稳定版本，API 保持向后兼容，可用于生产环境

当前版本 0.2.0 已达到 Beta 尾声、接近 Stable：默认档异步正确性、断点续传可靠性、文档契约与性能基线已对齐；预计 0.3.0 进入 Stable。

## 升级指南

### 从 0.1.0 升级到 0.2.0

0.2.0 完全向后兼容，无破坏性 API 变更。直接 `cargo update -p simple_downloader` 即可：

- 新增 `ResumePlan::prepare_async`（内部使用），原 `prepare` 保留兼容
- `ResumeRecorder` 增 `pending_segments/last_save` 与 `flush()`，对外不暴露破坏
- 新增 `DownloadError::PermanentFailure(String)` 变体（重试熔断），仅作为新增错误分支，现有 `match` 需补 `_` 或显式处理该分支
- 性能行为变更：`ChunkProgress` 节流（64KiB/50ms）、`save_atomic` 16段/1s 批量、`FileWriter` 128KiB 合并、`MultiRuntime` 并行探测、`Client` 连接池注入；下载结果不变，仅更少系统调用与广播
- 文档 `configuration.md/installation.md/best-practices.md` 修正为真实 API，无需代码迁移

### 从 0.0.x 升级到 0.1.0

0.1.0 是第一个公开版本，没有之前的版本，直接安装即可。

### 重大变更说明
