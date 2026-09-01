use simple_downloader::state::{ChunkState, DownloadState};

/// 卡100%回归：8并发 + throttle 64KiB 滞后，complete 必须按 size() 否则卡99%
#[test]
fn high_concurrency_does_not_stall_at_100() {
    let file_size: u64 = 2 * 1024 * 1024;
    let chunk_size = file_size / 8;
    let mut state = DownloadState::with_completed(file_size, 0);
    for i in 0..8 {
        let start = i * chunk_size;
        let mut end = start + chunk_size - 1;
        if i == 7 { end = file_size - 1; }
        state.chunks.insert(i, ChunkState::new(i, start, end));
    }
    assert_eq!(state.total_downloaded(), 0);
    for i in 0..8 {
        state.complete_chunk(&i);
    }
    assert!(state.is_download_finished(), "card 100% should be finished via size()");
    assert_eq!(state.total_downloaded(), file_size);
}

/// 速度突增回归：tiny elapsed 与巨大 delta 应被 guard/cap
#[test]
fn speed_does_not_spike_on_tiny_elapsed() {
    let mut state = DownloadState::with_completed(1_000_000, 0);
    let mut chunk = ChunkState::new(1, 0, 999_999);
    chunk.update_downloaded(500 * 1024);
    state.chunks.insert(1, chunk);

    let before = state.chunks.get(&1).unwrap().speed;
    state.chunks.get_mut(&1).unwrap().update_speed(0.001, 0.30);
    assert_eq!(state.chunks.get(&1).unwrap().speed, before, "tiny elapsed should not spike");

    let mut chunk2 = ChunkState::new(2, 0, 3_000_000_000);
    chunk2.update_downloaded(2 * 1024 * 1024 * 1024);
    chunk2.update_speed(0.5, 0.30);
    assert!(chunk2.speed <= 1024.0 * 1024.0 * 1024.0 + 1.0, "speed should be capped");
}

/// progress 回调中 speed 应稳定无恶性突增
#[test]
fn progress_speed_stable_under_high_throughput() {
    let mut chunk = ChunkState::new(1, 0, 10 * 1024 * 1024 - 1);
    for step in 1..=5 {
        chunk.update_downloaded(step * 2 * 1024 * 1024);
        chunk.update_speed(0.5, 0.30);
        assert!(chunk.speed < 500.0 * 1024.0 * 1024.0, "speed spike too high at step {}: {}", step, chunk.speed);
    }
    let before = chunk.speed;
    chunk.update_speed(0.5, 0.30);
    assert_eq!(chunk.speed, before);
}
