# 最佳实践指南

本文档总结了使用 simple_downloader 的最佳实践，帮助你充分利用库的功能，避免常见陷阱。

## 基础使用最佳实践

### 1. 使用 Builder 模式配置下载器
**推荐**：
```rust
Downloader::builder("https://example.com/file.zip", "output.zip")
    .workers(16)
    .update_interval(1.0)
    .resume(true)
    .download()
    .await
    .unwrap();
```
**不推荐**：
```rust
// 直接使用 Downloader::new 配置不够直观
let downloader = Downloader::new(
    "https://example.com/file.zip",
    "output.zip",
    16,
    1.0,
    default_client_builder
);
downloader.download().await.unwrap();
```

### 2. 合理设置并发线程数
线程数不是越多越好，需要根据文件大小和网络情况调整：
- 小文件（<100MB）：4-8 线程
- 中等文件（100MB-1GB）：8-16 线程
- 大文件（>1GB）：16-32 线程
- 多源下载：可以适当增加到 32-64 线程

**注意**：过多的线程可能会导致服务器限流、连接超时或者网络拥塞，反而降低下载速度。

### 3. 始终处理错误
不要忽略错误返回，对不同的错误类型进行合适的处理：
```rust
match Downloader::builder(url, path).download().await {
    Ok(_) => println!("下载成功"),
    Err(DownloadError::Request(e)) if e.is_timeout() => {
        eprintln!("下载超时，正在重试...");
        // 可以在这里实现重试逻辑
    }
    Err(DownloadError::Io(e)) if e.kind() == std::io::ErrorKind::StorageFull => {
        eprintln!("磁盘空间不足，请清理后重试");
        // 不需要重试
    }
    Err(e) => {
        eprintln!("下载失败: {}", e);
        // 其他错误处理
    }
}
```

### 4. 为长时间运行的下载设置合理的超时
```rust
use std::time::Duration;

Downloader::builder(url, path)
    .client_builder(|| {
        ClientBuilder::new()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(3600)) // 总超时 1 小时，适合大文件
    })
    .download()
    .await
    .unwrap();
```

---

## 断点续传最佳实践

### 1. 大文件下载务必启用断点续传
```rust
Downloader::builder(url, path)
    .resume(true) // 默认就是 true，可以显式写出来更清晰
    .download()
    .await
    .unwrap();
```
下载中断后，再次运行相同的代码会自动从断点恢复，不需要额外处理。

### 2. 定期清理不需要的断点元数据
断点续传的元数据文件（`*.resume`）会占用少量磁盘空间，下载完成后如果不需要可以删除：
```rust
use std::fs;

let output_path = "output.zip";
let resume_path = format!("{}.resume", output_path);

Downloader::builder(url, output_path)
    .resume(true)
    .download()
    .await
    .unwrap();

// 下载成功后删除元数据文件（可选）
let _ = fs::remove_file(resume_path);
```

### 3. 敏感文件下载的安全处理
如果下载的是敏感文件，下载完成后建议：
1. 删除断点元数据文件（包含下载进度和块信息）
2. 验证文件的哈希值确保完整性
3. 设置合适的文件权限，避免其他用户访问

---

## 进度监控最佳实践

### 1. 合理设置进度更新间隔
- 桌面应用：0.5-1 秒更新一次，UI 更流畅
- 服务器端应用：1-5 秒更新一次，减少 CPU 占用
- 命令行工具：0.2-0.5 秒更新一次，进度条更顺滑

```rust
Downloader::builder(url, path)
    .update_interval(1.0) // 每秒更新一次
    .run(progress_callback)
    .await
    .unwrap();
```

### 2. 不要在进度回调中执行耗时操作
进度回调在 Tokio 任务中运行，执行耗时操作会阻塞进度更新：
**不推荐**：
```rust
.run(|_, mut info_rx| async move {
    while let Ok(info) = info_rx.recv().await {
        // ❌ 耗时的 IO 操作会阻塞进度更新
        std::fs::write("progress.log", format!("{}", info.progress_percent())).unwrap();
    }
})
```
**推荐**：
```rust
.run(|_, mut info_rx| async move {
    while let Ok(info) = info_rx.recv().await {
        // ✅ 只做轻量的 UI 更新或日志记录
        println!("进度: {:.1}%", info.progress_percent());
        // 耗时操作发送到其他任务处理
        // tx.send(info).unwrap();
    }
})
```

### 3. 处理进度接收器的错误
当下载完成或发生错误时，进度通道会被关闭，`recv()` 会返回错误，需要处理这种情况：
```rust
.run(|total_size, mut info_rx| async move {
    println!("开始下载，总大小: {} bytes", total_size);
    loop {
        match info_rx.recv().await {
            Ok(info) => {
                // 处理进度更新
                println!("进度: {:.1}%", info.progress_percent());
                if info.is_complete() {
                    println!("下载完成！");
                    break;
                }
            }
            Err(_) => {
                // 通道关闭，下载结束
                break;
            }
        }
    }
})
.await
.unwrap();
```

---

## 多源下载最佳实践

### 1. 选择优质的下载源
- 优先选择延迟低、带宽高的镜像源
- 避免使用不可靠或速度慢的源
- 确保所有源的文件是同一个版本，避免下载到不完整或损坏的文件

### 2. 合理设置源的权重和优先级
```rust
let config = MultiSourceConfig::builder(
    vec![
        // 本地镜像，速度最快，权重最高
        SourceConfig::new("https://local-mirror.example.com/file.zip")
            .weight(3)
            .priority(1),
        // 官方源，速度中等
        SourceConfig::new("https://official.example.com/file.zip")
            .weight(2)
            .priority(2),
        // 第三方镜像，速度较慢
        SourceConfig::new("https://third-party.example.com/file.zip")
            .weight(1)
            .priority(3),
    ],
    "output.zip",
)
.workers(24)
.build();
```

### 3. 配置足够的备用源
建议配置至少 2-3 个备用源，当主源故障时可以自动切换到备用源，提高下载成功率。

### 4. 监控源的健康状态
在生产环境使用多源下载时，建议定期检查各个源的可用性，及时移除不可用的源，添加新的可用源。

---

## 代理使用最佳实践

### 1. 优先使用环境变量配置代理
避免在代码中硬编码代理地址：
```bash
# Linux/macOS
export HTTP_PROXY=http://proxy.example.com:8080
export HTTPS_PROXY=http://proxy.example.com:8080
export NO_PROXY=localhost,127.0.0.1,.example.com

# Windows
set HTTP_PROXY=http://proxy.example.com:8080
set HTTPS_PROXY=http://proxy.example.com:8080
```
simple_downloader 会自动识别这些环境变量，不需要额外配置。

### 2. 不要在代码中硬编码代理密码
**不推荐**：
```rust
ClientBuilder::new()
    .proxy(Proxy::http("http://user:password@proxy.example.com:8080").unwrap())
```
**推荐**：
```rust
// 从环境变量或配置文件读取代理信息
let proxy_url = std::env::var("HTTP_PROXY")?;
ClientBuilder::new()
    .proxy(Proxy::http(&proxy_url).unwrap())
```

### 3. 内网下载配置 NO_PROXY
如果有部分地址不需要走代理，配置 `NO_PROXY` 环境变量：
```bash
export NO_PROXY=localhost,127.0.0.1,.internal.example.com
```

---

## 性能优化最佳实践

### 1. 复用 ClientBuilder 提高性能
如果需要创建多个下载器，复用同一个 ClientBuilder 可以复用连接池，提高性能：
```rust
use std::sync::Arc;

let client_builder = Arc::new(|| {
    ClientBuilder::new()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(32)
});

// 多个下载任务复用同一个 client_builder
for url in urls {
    let builder = client_builder.clone();
    tokio::spawn(async move {
        Downloader::builder(url, output_path)
            .client_builder(move || builder())
            .download()
            .await
    });
}
```

### 2. 小文件下载优化
对于大量小文件下载：
1. 禁用断点续传（`.resume(false)`），减少磁盘 IO
2. 降低并发线程数到 1-4
3. 复用 HTTP 连接，减少连接建立开销
4. 考虑使用批量下载，减少开销

### 3. 大文件下载优化
对于大文件下载：
1. 启用断点续传，避免下载失败从头开始
2. 适当增加并发线程数（16-64）
3. 增加请求超时时间
4. 确保磁盘有足够的空间
5. 下载完成后校验文件完整性

---

## 生产环境最佳实践

### 1. 实现重试机制
对于重要的下载任务，实现指数退避重试：
```rust
use std::time::Duration;

async fn download_with_retry(url: &str, path: &str, max_retries: u32) -> Result<(), DownloadError> {
    let mut retries = 0;
    loop {
        match Downloader::builder(url, path)
            .resume(true)
            .download()
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                retries += 1;
                if retries >= max_retries {
                    return Err(e);
                }
                
                // 只对可重试的错误进行重试
                match e {
                    DownloadError::Request(_) | DownloadError::Join(_) => {
                        let wait_time = Duration::from_secs(2u64.pow(retries));
                        eprintln!("下载失败，{} 秒后重试 ({}/{}): {}", 
                                 wait_time.as_secs(), retries, max_retries, e);
                        tokio::time::sleep(wait_time).await;
                    }
                    _ => return Err(e),
                }
            }
        }
    }
}
```

### 2. 监控下载指标
在生产环境中建议监控以下指标：
- 下载成功率
- 平均下载速度
- 平均下载时间
- 错误类型分布
- 断点续传恢复率

### 3. 记录日志
为下载任务记录详细的日志，便于问题排查：
```rust
use tracing::{info, error, warn};

info!("开始下载: {} -> {}", url, path);
match Downloader::builder(url, path).download().await {
    Ok(_) => info!("下载成功: {} -> {}", url, path),
    Err(e) => {
        error!("下载失败: {} -> {}, 错误: {}", url, path, e);
        return Err(e);
    }
}
```

### 4. 安全配置
- 永远不要禁用 SSL 证书验证，除非在可控的测试环境
- 验证下载文件的哈希值，确保文件未被篡改
- 不要下载或执行来源不明的文件
- 限制下载目录的权限，避免写入到系统敏感目录

### 5. 资源限制
- 限制同时运行的下载任务数量，避免耗尽系统资源
- 限制下载速度，避免占满带宽影响其他业务
- 监控磁盘使用情况，避免占满磁盘空间

---

## 测试最佳实践

### 1. 单元测试
对于依赖下载功能的代码，可以使用 mock 服务器进行测试：
```rust
use mockito::{mock, server_url};

#[tokio::test]
async fn test_download() {
    let mock_body = "test content";
    let _m = mock("GET", "/test.txt")
        .with_status(200)
        .with_header("Content-Length", mock_body.len().to_string().as_str())
        .with_body(mock_body)
        .create();

    let url = format!("{}/test.txt", server_url());
    let path = "test_output.txt";
    
    Downloader::builder(&url, path)
        .download()
        .await
        .unwrap();
    
    // 验证文件内容
    let content = std::fs::read_to_string(path).unwrap();
    assert_eq!(content, mock_body);
    
    // 清理测试文件
    let _ = std::fs::remove_file(path);
}
```

### 2. 集成测试
在集成测试中测试真实的下载场景：
- 测试不同大小的文件下载
- 测试断点续传功能
- 测试错误处理逻辑
- 测试多源下载功能

### 3. 性能测试
定期进行性能测试，确保版本更新不会导致性能下降：
- 测试不同并发数下的下载速度
- 测试内存占用情况
- 测试大文件下载的稳定性

---

## 常见反模式

### ❌ 反模式 1: 忽略错误返回
```rust
// 错误: 完全忽略错误，下载失败了也不知道
let _ = Downloader::builder(url, path).download().await;
```

### ❌ 反模式 2: 设置过高的并发数
```rust
// 错误: 1000 线程会导致大量连接失败，反而降低速度
Downloader::builder(url, path).workers(1000).download().await;
```

### ❌ 反模式 3: 频繁创建新的 ClientBuilder
```rust
// 错误: 每次都创建新的 ClientBuilder，无法复用连接池
for url in urls {
    Downloader::builder(url, path)
        .client_builder(|| ClientBuilder::new()) // 每次都新建
        .download()
        .await
        .unwrap();
}
```

### ❌ 反模式 4: 在进度回调中执行大量计算
```rust
.run(|_, mut info_rx| async move {
    while let Ok(info) = info_rx.recv().await {
        // 错误: 大量计算会阻塞进度更新
        heavy_computation();
    }
})
```

### ❌ 反模式 5: 禁用证书验证
```rust
// 错误: 生产环境禁用证书验证存在严重安全隐患
ClientBuilder::new().danger_accept_invalid_certs(true)
```

遵循这些最佳实践可以帮助你更高效、更安全地使用 simple_downloader，避免常见问题。
