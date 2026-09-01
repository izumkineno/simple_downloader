//! 回归示例：速度 guard 与 cap 演示
//! 运行：cargo run --example speed_guard_demo
//! 无需网络，直接演示 ChunkState::update_speed 对 tiny elapsed 与巨大 delta 的防护

use simple_downloader::state::ChunkState;

fn main() {
    println!("=== speed guard demo (regression for恶性突增) ===");

    // 1. tiny elapsed 来自 tokio interval 补偿突发
    let mut chunk = ChunkState::new(1, 0, 10_000);
    chunk.update_downloaded(100 * 1024);
    println!("before tiny elapsed speed = {:.2} KiB/s", chunk.speed / 1024.0);
    chunk.update_speed(0.001, 0.30);
    println!("after tiny elapsed 0.001s (should be ignored) speed = {:.2} KiB/s", chunk.speed / 1024.0);
    assert_eq!(chunk.speed, 0.0, "tiny elapsed should be ignored");

    // 2. 正常 0.5s
    chunk.update_speed(0.5, 0.30);
    println!("after normal 0.5s speed = {:.2} MiB/s", chunk.speed / (1024.0 * 1024.0));

    // 3. Lagged 补发巨大 delta：2GiB/0.5s 应被 cap 到 1GiB/s
    let mut chunk2 = ChunkState::new(2, 0, 3_000_000_000);
    chunk2.update_downloaded(2 * 1024 * 1024 * 1024);
    chunk2.update_speed(0.5, 0.30);
    println!("huge delta 2GiB/0.5s capped speed = {:.2} MiB/s (cap 1024 MiB/s)", chunk2.speed / (1024.0 * 1024.0));
    assert!(chunk2.speed <= 1024.0 * 1024.0 * 1024.0 + 1.0);

    // 4. zero delta 不应突变
    let before = chunk2.speed;
    chunk2.update_speed(0.5, 0.30);
    println!("zero delta speed unchanged = {:.2} MiB/s", chunk2.speed / (1024.0 * 1024.0));
    assert_eq!(chunk2.speed, before);

    println!("✓ all speed guards passed, no spike");
}
