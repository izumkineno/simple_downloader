//! 队列调度示例（需 `queue` feature）
//!
//! ```bash
//! cargo run --features queue --example with_queue
//! # 限速 + 队列组合（如需）：
//! cargo run --features queue,progress --example with_queue
//! ```

use std::path::PathBuf;
use std::time::Duration;

use simple_downloader::{QueueError, TaskQueue, TaskState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // simple_downloader::trace::init_tracing();

    let out_dir = std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "queue_demo".to_string());
    std::fs::create_dir_all(&out_dir)?;

    // 1. 创建队列：全局最多 2 任务并发（与单任务 workers 隔离）
    let queue = TaskQueue::with_max_concurrent(2);
    println!("队列创建: max_concurrent=2 (单任务 workers 默认 = CPU 核心数)");

    // 2. 入队 2 个同名任务演示两阶段重命名（队列争用 + 磁盘）
    let url_a = std::env::var("DEMO_URL")
        .unwrap_or_else(|_| "https://proof.ovh.net/files/10Mb.dat".to_string());
    let url_b = url_a.clone();
    let same_path = PathBuf::from(format!("{}/same.bin", out_dir));

    // 预置磁盘文件触发磁盘重命名分支（可选演示）
    // std::fs::write(&same_path, b"pre-existing")?;

    println!("\n[2] 入队同路径任务 -> 触发重命名 a.bin / a(1).bin");
    let id1 = queue.enqueue(url_a, same_path.clone()).await;
    let id2 = queue.enqueue(url_b, same_path.clone()).await;
    println!("  id1={} -> {}", id1, same_path.display());
    println!("  id2={} -> {}", id2, same_path.display());

    // 3. 入队并指定 per-task workers（与队列 max 独立）
    let url_c = std::env::var("DEMO_URL_2")
        .unwrap_or_else(|_| "https://proof.ovh.net/files/10Mb.dat".to_string());
    let out_c = PathBuf::from(format!("{}/workers8.bin", out_dir));
    println!("\n[3] per-task workers 隔离: queue max=2, task workers=8");
    let id3 = queue.enqueue_with_workers(url_c, out_c.clone(), 8).await;
    println!("  id3={} workers=8 -> {}", id3, out_c.display());

    // 4. 演示 pause / resume（Active -> Paused -> Queued(front)）
    // 等待 id1 进入 Active 后 pause
    for _ in 0..30 {
        if let Some(s) = queue.query(id1.clone()).await {
            if s.state == TaskState::Active {
                break;
            }
        }
        if let Some(s) = queue.query(id1.clone()).await {
            if matches!(s.state, TaskState::Completed | TaskState::Failed(_) | TaskState::Removed) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if let Some(s) = queue.query(id1.clone()).await {
        if s.state == TaskState::Active {
            println!("\n[4] pause {} (Active -> Paused)", id1);
            queue.pause(id1.clone()).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let snap = queue.query(id1.clone()).await.unwrap();
            println!("  pause 后状态: {:?}", snap.state);
            if snap.state == TaskState::Paused {
                println!("  resume {} (Paused -> Queued front)", id1);
                queue.resume(id1.clone()).await?;
            }
        } else {
            println!("\n[4] 跳过 pause 演示，id1 已处于 {:?}", s.state);
        }
    }

    // 5. 演示 cancel（Queued / Paused 场景）
    let cancel_path = PathBuf::from(format!("{}/to_cancel.bin", out_dir));
    let cancel_url = std::env::var("DEMO_URL").unwrap_or_else(|_| "https://proof.ovh.net/files/10Mb.dat".to_string());
    let id_cancel = queue.enqueue(cancel_url, cancel_path.clone()).await;
    println!("\n[5] cancel 演示: id={} -> {}", id_cancel, cancel_path.display());
    tokio::time::sleep(Duration::from_millis(150)).await;
    match queue.cancel(id_cancel.clone()).await {
        Ok(()) => println!("  cancel 成功 -> Removed"),
        Err(QueueError::NotFound(_)) => println!("  cancel: NotFound (可能已完成)"),
        Err(e) => println!("  cancel 错误: {}", e),
    }
    if let Some(s) = queue.query(id_cancel.clone()).await {
        println!("  cancel 后状态: {:?}", s.state);
    }
    // 幂等
    let _ = queue.cancel(id_cancel.clone()).await;
    println!("  幂等 cancel 再次调用 OK");

    // 6. 队列状态快照与 wait_all
    println!("\n[6] 队列状态:");
    println!("  queued_len={}, active_count={}", queue.queued_len().await, queue.active_count().await);
    for id in [&id1, &id2, &id3] {
        if let Some(s) = queue.query(id.clone()).await {
            println!("  {} -> {:?} @ {}", s.id, s.state, s.output_path.display());
        }
    }

    println!("\n等待剩余任务完成 (wait_all) ...");
    // 超时保护：最多 120s
    if tokio::time::timeout(Duration::from_secs(120), queue.wait_all())
        .await
        .is_err()
    {
        eprintln!("wait_all 超时，当前 active={}, queued={}", queue.active_count().await, queue.queued_len().await);
    }

    println!("\n完成快照:");
    for id in [id1, id2, id3] {
        if let Some(s) = queue.query(id.clone()).await {
            match s.state {
                TaskState::Completed => println!("  {} Completed -> {} ({} bytes)", s.id, s.output_path.display(), s.output_path.metadata().map(|m| m.len()).unwrap_or(0)),
                TaskState::Failed(e) => println!("  {} Failed: {}", s.id, e),
                TaskState::Removed => println!("  {} Removed", s.id),
                other => println!("  {} {:?}", s.id, other),
            }
        }
    }

    // 展示重命名结果
    println!("\n重命名结果检查:");
    for name in ["same.bin", "same(1).bin", "workers8.bin"] {
        let p = PathBuf::from(format!("{}/{}", out_dir, name));
        if p.exists() {
            println!("  {} 存在 ({} bytes)", p.display(), p.metadata().map(|m| m.len()).unwrap_or(0));
        } else {
            println!("  {} 不存在", p.display());
        }
    }

    println!("\n提示:");
    println!("  - 重命名无限递增 a(N).ext 已覆盖 .tar.gz / .gitignore / 无扩展");
    println!("  - 两层并发独立：queue max=2 控制任务并发，per-task workers 控制片并发");
    println!("  - 队列仅进程内并发，跨进程同路径需外部文件锁（见 TaskQueue 文档）");
    Ok(())
}
