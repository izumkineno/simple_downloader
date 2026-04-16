use simple_downloader::concurrency::ConcurrencyManager;
use simple_downloader::state::{ChunkState, DownloadState};
use simple_downloader::types::{ChunkId, DownloadCmd};
use std::time::Duration;
use tokio::sync::broadcast;

const POLICY_COOLDOWN: Duration = Duration::from_millis(350);

fn active_chunk(id: ChunkId, start: u64, end: u64, downloaded: u64, speed: f64) -> ChunkState {
    let mut chunk = ChunkState::new(id, start, end);
    chunk.downloaded_bytes = downloaded;
    chunk.speed = speed;
    chunk
}

fn state_with(total_file_size: u64, chunks: impl IntoIterator<Item = ChunkState>) -> DownloadState {
    let mut state = DownloadState::new(total_file_size);
    for chunk in chunks {
        state.chunks.insert(chunk.id, chunk);
    }
    state
}

fn command_channel() -> (
    broadcast::Sender<DownloadCmd>,
    broadcast::Receiver<DownloadCmd>,
) {
    let (cmd_tx, cmd_rx) = broadcast::channel(16);
    (cmd_tx, cmd_rx)
}

fn wait_for_policy_cooldown() {
    std::thread::sleep(POLICY_COOLDOWN);
}

fn assert_no_split(rx: &mut broadcast::Receiver<DownloadCmd>) {
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "unexpected split command was emitted"
    );
}

fn assert_split_for(rx: &mut broadcast::Receiver<DownloadCmd>, expected_id: ChunkId) {
    match rx.try_recv() {
        Ok(DownloadCmd::BisectDownload { id }) => assert_eq!(id, expected_id),
        Ok(other) => panic!("unexpected command emitted: {other:?}"),
        Err(err) => panic!("expected split command for chunk {expected_id}, got {err:?}"),
    }
}

#[test]
fn probing_does_not_split_without_throughput_evidence() {
    let mut manager = ConcurrencyManager::new(4);
    let state = state_with(200_000, [active_chunk(1, 0, 199_999, 0, 0.0)]);
    let (cmd_tx, mut cmd_rx) = command_channel();

    wait_for_policy_cooldown();
    manager.decide_and_act(&state, &cmd_tx);

    assert_no_split(&mut cmd_rx);
}

#[test]
fn reactive_refill_does_not_split_only_because_worker_slot_is_free() {
    let mut manager = ConcurrencyManager::new(2);
    let full_state = state_with(
        400_000,
        [
            active_chunk(1, 0, 199_999, 50_000, 100_000.0),
            active_chunk(2, 200_000, 399_999, 50_000, 100_000.0),
        ],
    );
    let (cmd_tx, mut cmd_rx) = command_channel();

    wait_for_policy_cooldown();
    manager.decide_and_act(&full_state, &cmd_tx);
    assert_no_split(&mut cmd_rx);

    let one_idle_slot_without_benefit =
        state_with(400_000, [active_chunk(1, 0, 199_999, 50_000, 0.0)]);

    wait_for_policy_cooldown();
    manager.decide_and_act(&one_idle_slot_without_benefit, &cmd_tx);

    assert_no_split(&mut cmd_rx);
}

#[test]
fn stable_policy_does_not_split_near_completion() {
    let mut manager = ConcurrencyManager::new(2);
    let full_state = state_with(
        200_000,
        [
            active_chunk(1, 0, 99_999, 90_000, 50_000.0),
            active_chunk(2, 100_000, 199_999, 90_000, 50_000.0),
        ],
    );
    let (cmd_tx, mut cmd_rx) = command_channel();

    wait_for_policy_cooldown();
    manager.decide_and_act(&full_state, &cmd_tx);
    assert_no_split(&mut cmd_rx);

    let nearly_complete = state_with(200_000, [active_chunk(1, 0, 199_999, 196_000, 50_000.0)]);

    wait_for_policy_cooldown();
    manager.decide_and_act(&nearly_complete, &cmd_tx);

    assert_no_split(&mut cmd_rx);
}

#[test]
fn steady_stable_speed_does_not_reprobe_against_stale_baseline() {
    let mut manager = ConcurrencyManager::new(3);
    let saturated_state = state_with(
        900_000,
        [
            active_chunk(1, 0, 299_999, 50_000, 150_000.0),
            active_chunk(2, 300_000, 599_999, 50_000, 150_000.0),
            active_chunk(3, 600_000, 899_999, 50_000, 150_000.0),
        ],
    );
    let (cmd_tx, mut cmd_rx) = command_channel();

    wait_for_policy_cooldown();
    manager.decide_and_act(&saturated_state, &cmd_tx);
    assert_no_split(&mut cmd_rx);

    let steady_state_with_idle_slot = state_with(
        900_000,
        [
            active_chunk(1, 0, 299_999, 50_000, 150_000.0),
            active_chunk(2, 300_000, 599_999, 50_000, 150_000.0),
        ],
    );

    for _ in 0..4 {
        wait_for_policy_cooldown();
        manager.decide_and_act(&steady_state_with_idle_slot, &cmd_tx);
        assert_no_split(&mut cmd_rx);
    }
}

#[test]
fn split_target_prefers_remaining_splittable_work_over_original_size() {
    let mut manager = ConcurrencyManager::new(3);
    let saturated_state = state_with(
        1_600_000,
        [
            active_chunk(1, 0, 999_999, 900_000, 25_000.0),
            active_chunk(2, 1_000_000, 1_499_999, 0, 25_000.0),
            active_chunk(3, 1_500_000, 1_599_999, 0, 25_000.0),
        ],
    );
    let (cmd_tx, mut cmd_rx) = command_channel();

    wait_for_policy_cooldown();
    manager.decide_and_act(&saturated_state, &cmd_tx);
    assert_no_split(&mut cmd_rx);

    let refill_state = state_with(
        1_600_000,
        [
            // Largest original range, but only 10 KiB remains.
            active_chunk(1, 0, 999_999, 989_760, 25_000.0),
            // Smaller original range, but much more remaining splittable work.
            active_chunk(2, 1_000_000, 1_499_999, 0, 25_000.0),
        ],
    );

    wait_for_policy_cooldown();
    manager.decide_and_act(&refill_state, &cmd_tx);

    assert_split_for(&mut cmd_rx, 2);
}
