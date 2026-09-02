# 错误码说明

本文档详细介绍 simple_downloader 中可能出现的所有错误类型，以及对应的原因和处理方案。

## 错误类型总览（以 `src/types.rs:DownloadError` 为准，当前无数字错误码）

| 错误类型 | 变体 | 说明 | 可重试 |
|----------|------|------|--------|
| 网络请求错误 | `DownloadError::Request(reqwest::Error)` | HTTP 请求过程中发生的错误 | ✅ 大部分情况可重试 |
| 文件 I/O 错误 | `DownloadError::Io(io::Error)` | 本地文件读写操作发生的错误 | ❌ 一般不可重试 |
| 并发任务错误 | `DownloadError::Join(JoinError)` | 异步任务执行过程中发生的错误 | ✅ 可重试 |
| 缺少长度错误 | `DownloadError::MissingContentLength` | 服务器未返回 Content-Length（0.3.1+ 自动回退单流流式下载） | ✅ 自动回退，无需重试 |
| 多源下载错误 | `DownloadError::NoAvailableSources` | 多源模式下无可用源 | ✅ 可重试 |
| 断点续传错误 | `DownloadError::ResumeTargetMissing(PathBuf)` / `ResumeMetadata(String)` | 断点续传相关：sidecar 存在但文件缺失 / 元数据损坏 | ❌ 需清理后重试 |
| 永久失败 | `DownloadError::PermanentFailure(String)` | 达 `MAX_TOTAL_ATTEMPTS=30` 或 lane 连续失败熔断 | ❌ 需修源/网络后重试 |
| 无效参数 | `DownloadError::InvalidArgument(String)` | `rate-limit` 等参数校验（`0`/`>u32::MAX`/`burst` 无限速等） | ❌ 改参后重试 |

### 1. 网络请求错误 (Request)

**错误信息**：`网络请求失败: {具体错误}`

**可能原因**：
- 网络连接中断或不稳定
- DNS 解析失败，无法找到服务器地址
- 服务器响应超时
- 服务器返回 4xx 客户端错误（如 404 未找到、403 禁止访问）
- 服务器返回 5xx 服务端错误（如 500 内部错误、503 服务不可用）
- SSL/TLS 握手失败，证书无效或不被信任
- 代理服务器连接失败或认证失败

**处理建议**：
```rust
use simple_downloader::{Downloader, DownloadError};

#[tokio::main]
async fn main() {
    let result = Downloader::builder("https://example.com/file.bin", "output.bin")
        .download()
        .await;
    
    match result {
        Ok(_) => println!("下载成功"),
        Err(DownloadError::Request(e)) => {
            if e.is_timeout() {
                eprintln!("请求超时，请检查网络连接或增加超时时间");
                // 可以重试下载
            } else if e.is_connect() {
                eprintln!("连接服务器失败，请检查网络或代理配置");
            } else if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                eprintln!("文件不存在，请检查下载链接是否正确");
                // 404 错误不需要重试
            } else if e.status() == Some(reqwest::StatusCode::FORBIDDEN) {
                eprintln!("访问被禁止，请检查是否需要认证或代理");
            } else if e.is_ssl() {
                eprintln!("SSL 证书验证失败，请检查证书配置");
            } else {
                eprintln!("网络请求失败: {}", e);
            }
        }
        Err(e) => eprintln!("其他错误: {}", e),
    }
}
```

**重试策略**：
- 超时、连接失败、5xx 错误：建议重试 3-5 次，每次间隔递增
- 4xx 错误（除了 429 限流）：不需要重试，需要修正请求参数
- SSL 错误：不需要重试，需要检查证书或网络环境

---

### 2. 文件 I/O 错误 (Io)

**错误信息**：`文件 I/O 错误: {具体错误}`

**可能原因**：
- 磁盘空间不足，无法写入文件
- 没有目标路径的写入权限
- 目标文件被其他进程占用或锁定
- 磁盘损坏或文件系统错误
- 目标路径是一个目录而不是文件
- 路径包含非法字符或过长

**处理建议**：
```rust
match result {
    Err(DownloadError::Io(e)) => {
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => {
                eprintln!("没有写入权限，请检查路径是否正确或使用管理员权限运行");
            }
            std::io::ErrorKind::StorageFull => {
                eprintln!("磁盘空间不足，请清理磁盘后重试");
            }
            std::io::ErrorKind::NotFound => {
                eprintln!("保存路径不存在，请检查路径是否正确");
            }
            std::io::ErrorKind::AlreadyExists => {
                eprintln!("文件已存在且被占用，请关闭占用该文件的程序后重试");
            }
            _ => {
                eprintln!("文件读写失败: {}", e);
            }
        }
    }
}
```

**重试策略**：
- ❌ 一般不可重试，需要先解决对应的 I/O 问题
- 临时占用的情况：可以等待几秒后重试

---

### 3. 并发任务错误 (Join)

**错误信息**：`并发任务执行失败: {具体错误}`

**可能原因**：
- 下载任务内部发生 panic
- Tokio 运行时资源不足
- 任务被外部强制取消
- 内存不足导致任务终止

**处理建议**：
```rust
match result {
    Err(DownloadError::Join(e)) => {
        if e.is_panic() {
            eprintln!("下载任务崩溃: {}", e);
            // 可以尝试重新下载
        } else if e.is_cancelled() {
            eprintln!("下载任务被取消");
        } else {
            eprintln!("任务执行失败: {}", e);
        }
    }
}
```

**重试策略**：
- ✅ 可以重试，除非是系统资源严重不足的情况
- 建议重试前检查系统资源使用情况

---

### 4. 缺少 Content-Length 错误 (MissingContentLength)

**错误信息**：`无法从服务器响应头中获取文件大小 (Content-Length)`

**行为变更（0.3.1+）**：`Downloader` 在 `HEAD` 与 `Range 0-0` 均无法获取大小时，不再直接返回 `Err(MissingContentLength)`，而是自动回退为 **单流流式下载**（`Transfer-Encoding: chunked` 场景），顺序写入、不预分配、不支持 Range/多源/断点续传，下载成功即视为完成。仅当流式 `GET` 亦失败时才透出该错误。

**可能原因**：
- 服务器不返回 Content-Length 响应头
- 文件是动态生成的流，没有固定大小
- 服务器使用分块传输编码（Transfer-Encoding: chunked）但没有 Content-Length
- 服务器配置错误或使用了反向代理丢失了头信息

**处理建议**：
自 0.3.1 起无需特殊处理，`Downloader::builder(url, path).download().await` 会自动流式回退；如需禁用回退或自定义，可捕获该错误后自行处理：
```rust
match Downloader::builder("https://example.com/stream", "out.bin").download().await {
    Err(simple_downloader::DownloadError::MissingContentLength) => {
        eprintln!("服务器未返回大小且流式下载失败");
    }
    other => other.unwrap(),
}
```

**重试策略**：
- ✅ 0.3.1+ 自动流式回退，无需重试；若仍返回该错误，说明 HEAD、Range 探测与流式 GET 均失败，需检查网络/链接

---

### 5. 无可用下载源错误 (NoAvailableSources)

**错误信息**：`没有可用的下载源`

**仅在多源下载模式下出现**

**可能原因**：
- 所有配置的下载源都无法访问
- 所有下载源都返回 4xx/5xx 错误
- 所有下载源的文件校验失败
- 所有下载源的连续失败次数超过阈值

**处理建议**：
```rust
match result {
    Err(DownloadError::NoAvailableSources) => {
        eprintln!("所有下载源都不可用，请检查：");
        eprintln!("1. 网络连接是否正常");
        eprintln!("2. 下载源地址是否正确");
        eprintln!("3. 是否需要配置代理");
        eprintln!("4. 下载源是否已经失效");
        // 可以尝试增加更多下载源后重试
    }
}
```

**重试策略**：
- ✅ 可以重试，但需要先确认至少有一个可用的下载源
- 建议增加更多备用下载源

---

### 6. 断点续传目标文件缺失错误 (ResumeTargetMissing)

**错误信息**：`断点续传元数据存在，但目标文件不存在: {文件路径}`

**仅在启用断点续传功能时出现**

**可能原因**：
- 上次下载后，目标文件被手动删除或移动
- 文件路径被重命名
- 保存文件的磁盘分区被卸载
- 程序运行权限变化导致无法访问文件

**处理建议**：
```rust
match result {
    Err(DownloadError::ResumeTargetMissing(path)) => {
        eprintln!("断点续传失败，目标文件不存在: {}", path.display());
        eprintln!("可以选择：");
        eprintln!("1. 恢复目标文件到原路径");
        eprintln!("2. 删除对应的 *.download.bitcode 元数据文件后重新下载");
        eprintln!("3. 禁用断点续传功能重新下载（.resume(false)/with_resume(false)）");
    }
}
```

**重试策略**：
- ❌ 直接重试会失败，需要先解决文件缺失问题或禁用断点续传

---

### 7. 断点续传元数据无效错误 (ResumeMetadata)

**错误信息**：`断点续传元数据无效: {具体原因}`

**仅在启用断点续传功能时出现**

**可能原因**：
- 元数据文件损坏或被手动修改
- 元数据文件版本与当前程序版本不兼容
- 目标文件与元数据记录的文件不匹配（大小、修改时间不一致）
- 元数据文件被其他程序占用

**处理建议**：
```rust
match result {
    Err(DownloadError::ResumeMetadata(reason)) => {
        eprintln!("断点续传元数据无效: {}", reason);
        eprintln!("建议：");
        eprintln!("1. 删除对应的 *.download.bitcode 元数据文件后重新下载");
        eprintln!("2. 禁用断点续传功能重新下载");
    }
}
```

**重试策略**：
- ❌ 直接重试会失败，需要删除无效的元数据文件或禁用断点续传

---

### 8. 永久失败（PermanentFailure）

**错误信息**：`下载失败，已达重试上限: {原因}`

**可能原因**：
-  `RetryHandler` 达 `MAX_TOTAL_ATTEMPTS=30` 或 `MAX_RETRIES` + `DELAYED_RETRY_DURATION 10s` 仍未成功
-  多源 `lane` 连续失败 `≥ BLACKLIST_THRESHOLD=3` 隔离 + 无健康 lane 可用
-  网络/源端长期不可用

**处理建议**：
```rust
match result {
    Err(DownloadError::PermanentFailure(msg)) => {
        eprintln!("永久失败: {}，请检查网络/源可用性后重试", msg);
    }
}
```

---

### 9. 无效参数（InvalidArgument）

**错误信息**：`无效参数: {原因}`

**常见场景（`rate-limit` feature）**：
- `speed_limit == 0` 或 `burst == 0`
- `burst` 单独设置而无 `speed_limit`
- `speed_limit` / `burst`  `> u32::MAX`（`~4 GiB/s` 上限，`governor::Quota` 限制）
- `MultiSourceConfig::with_global_speed_limit(0)` 等

**处理建议**：
```rust
match Downloader::builder(url, path)
    .speed_limit(0) // ❌ 将返回 InvalidArgument
    .download().await {
    Err(DownloadError::InvalidArgument(msg)) => eprintln!("参数错误: {}", msg),
    other => other.unwrap(),
```

## 全局错误处理最佳实践

### 1. 自动重试策略

```rust
use simple_downloader::{Downloader, DownloadError};
use std::time::Duration;

async fn download_with_retry(url: &str, output_path: &str, max_retries: u32) -> Result<(), DownloadError> {
    let mut retries = 0;
    loop {
        match Downloader::builder(url, output_path)
            .resume(true) // 启用断点续传，重试时可以继续之前的进度
            .download()
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                retries += 1;
                if retries >= max_retries {
                    return Err(e);
                }
                
                // 判断错误是否可重试
                match e {
                    DownloadError::Request(_) | DownloadError::Join(_) => {
                        // 等待一段时间后重试，使用指数退避
                        let wait_time = Duration::from_secs(2u64.pow(retries));
                        eprintln!("下载失败，{} 秒后重试 ({}/{}): {}", wait_time.as_secs(), retries, max_retries, e);
                        tokio::time::sleep(wait_time).await;
                    }
                    _ => {
                        // 不可重试的错误，直接返回
                        return Err(e);
                    }
                }
            }
        }
    }
}
```

### 2. 用户友好的错误提示

```rust
fn format_error(e: &DownloadError) -> String {
    match e {
        DownloadError::Request(req_e) => {
            if req_e.is_timeout() {
                "下载超时，请检查网络连接".to_string()
            } else if req_e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                "文件不存在，请检查下载链接".to_string()
            } else if req_e.is_connect() {
                "无法连接到服务器，请检查网络或代理设置".to_string()
            } else {
                format!("网络错误: {}", req_e)
            }
        }
        DownloadError::Io(io_e) => {
            match io_e.kind() {
                std::io::ErrorKind::PermissionDenied => "没有文件写入权限".to_string(),
                std::io::ErrorKind::StorageFull => "磁盘空间不足".to_string(),
                _ => format!("文件读写错误: {}", io_e),
            }
        }
        DownloadError::MissingContentLength => "服务器不支持获取文件大小，建议使用单线程下载".to_string(),
        DownloadError::NoAvailableSources => "所有下载源都不可用".to_string(),
        DownloadError::ResumeTargetMissing(_) => "断点续传的目标文件不存在，需要重新下载".to_string(),
        DownloadError::ResumeMetadata(_) => "断点续传数据损坏，需要重新下载".to_string(),
        _ => format!("下载失败: {}", e),
    }
}
```

## 错误调试技巧

1. **启用详细日志**：
   ```rust
   // 推荐：本库 tracing 门面（见 `src/trace.rs`）
   simple_downloader::trace::init_tracing();
   // 或：RUST_LOG=simple_downloader=debug cargo run ...
   ```

2. **捕获错误上下文**：
   使用 `thiserror` 和 `anyhow` 库可以保留更多错误上下文信息，便于调试。

3. **检查临时文件**：
   断点续传的元数据文件默认保存在与输出文件相同的目录下，后缀为 `.download.bitcode`（`RESUME_EXTENSION`），可以检查该文件是否存在；损坏时会被自动删除重建（`src/resume.rs:validate_shape`）。

## 常见问题排查
### Q: 总是出现 "网络请求失败: 连接超时"
A: 
- 检查网络连接是否正常
- 尝试使用浏览器访问下载链接是否能正常下载
- 检查是否需要配置代理
- 尝试增加客户端超时时间

### Q: 断点续传总是失败
A:
- 检查输出文件是否被其他程序占用
- 检查磁盘空间是否足够
- 尝试删除对应的 `*.download.bitcode` 元数据文件后重新下载
- 确认下载的文件是否支持 Range 请求

### Q: 多源下载时提示 "没有可用的下载源"
- 检查每个下载源是否能单独访问
- 检查是否需要配置代理
- 尝试降低 `max_retries_per_source` 配置值
