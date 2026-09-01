//! 回归示例：高并发卡100%修复演示（无网络，仅 state 逻辑）
//! 运行：cargo run --example regression_high_concurrency
//! 演示 broadcast Lagged 丢最终 Progress 时，complete_chunk 按 size() 仍可完成

use simple_downloader::state::{ChunkState, DownloadState};

fn main() {
    println!("=== regression: high-concurrency card 100% fix ===");
    // 模拟 600M 文件 8×75M 并发，throttle 导致最终 Progress 丢失
    let file_size = 600 * 1024 * 1024u64;
    let chunk_size = file_size / 8;
    let mut state = DownloadState::with_completed(file_size, 0);

    // 插入 8 块，均未更新 downloaded（模拟 Lagged 丢失）
    for i in 0..8 {
        let start = i * chunk_size;
        let end = if i == 7 { file_size - 1 } else { start + chunk_size - 1 };
        state.chunks.insert(i, ChunkState::new(i, start, end));
        // 注意：不调用 update_downloaded，保持 0 模拟丢失
    }
    println!("initial total_downloaded = {}", state.total_downloaded());
    assert_eq!(state.total_downloaded(), 0);

    // 逐个完成：即使 downloaded 仍为 0，complete_chunk 按 size() 累加应完成
    for i in 0..8 {
        state.complete_chunk(&i);
        println!("after complete {}: total_downloaded={}, finished={}", i, state.total_downloaded(), state.is_download_finished());
    }
    assert!(state.is_download_finished(), "should be finished even with stale downloaded");
    assert_eq!(state.total_downloaded(), file_size);
    println!("✓ 8 chunks completed via size(), not stalled at 99%");

    // 对比旧逻辑：若按 downloaded(0) 累加则会卡
    let mut state_old = DownloadState::with_completed(file_size, 0);
    for i in 0..8 {
        let start = i * chunk_size;
        let end = if i == 7 { file_size - 1 } else { start + chunk_size - 1 };
        state_old.chunks.insert(i, ChunkState::new(i, start, end));
    }
    // 模拟旧 preserve 逻辑：用 stale 0 累加 8 次仍为 0，is_finished 永假
    // 此处仅演示概念，实际旧代码会卡
    println!("(old logic with stale downloaded would remain 0 and stall)");
    println!("✓ card 100% regression fixed");
}
