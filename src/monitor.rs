// monitor.rs

use crate::state::DownloadState;
use crate::types::{ChunkId, EventStatus, EventThread};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Interval;

/// 调度策略接口
///
/// 任何实现此 trait 的结构体都可以作为调度器的一个策略。
/// `decide` 方法会定期被调用，并根据当前的下载状态返回需要执行的命令列表。
pub trait SchedulingStrategy: Send {
    /// 根据当前下载状态，决定要执行的调度命令
    ///
    /// # Arguments
    /// * `state` - 对当前下载状态的只读引用
    ///
    /// # Returns
    /// * 一个包含零个或多个 `EventThread` 命令的向量
    fn decide(&mut self, state: &DownloadState) -> Vec<EventThread>;
}

/// 一个简单的调度策略：分裂慢速分片
///
/// 此策略会计算所有正在下载分片的平均速度。如果某个分片的速度
/// 明显低于平均值（例如，低于平均值的 50%），并且该分片剩余
/// 的下载量足够大（例如，大于 1MB），则会创建一个命令来
/// 将该分片从中间分裂成两个。
pub struct SimpleSplitStrategy {
    /// 触发分裂的速度阈值因子（例如 0.5 表示低于平均速度 50%）
    speed_factor_threshold: f64,
    /// 分片可被分裂的最小剩余字节数
    min_remaining_bytes_for_split: u64,
    /// 用于防止在短时间内重复分裂同一个分片
    recently_split: HashMap<ChunkId, Instant>,
    /// 冷却时间，防止重复分裂
    cooldown: Duration,
    /// 最大的额外线程数
    max_threads: usize,
}

impl SimpleSplitStrategy {
    pub fn new(
        max_threads: usize,
        speed_factor_threshold: f64,
        min_remaining_bytes_for_split: u64,
        cooldown_secs: u64,
    ) -> Self {
        Self {
            max_threads,
            speed_factor_threshold,
            min_remaining_bytes_for_split,
            recently_split: HashMap::new(),
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }
}

impl SchedulingStrategy for SimpleSplitStrategy {
    fn decide(&mut self, state: &DownloadState) -> Vec<EventThread> {
        let mut commands = Vec::new();
        let now = Instant::now();

        // 清理过期的冷却记录
        self.recently_split
            .retain(|_, &mut v| now.duration_since(v) < self.cooldown);

        let avg_speed = state.get_current_speed();
        let speed_threshold = avg_speed * self.speed_factor_threshold;

        for chunk in state.get_downloading_chunks() {
            let chunk_duration = chunk.start_time.elapsed().as_secs_f64();
            if chunk_duration < 2.0 {
                // 给予新分片一些启动时间
                continue;
            }

            let chunk_speed = chunk.get_current_speed();
            let remaining_bytes =
                (chunk.end_byte + 1).saturating_sub(chunk.start_byte + chunk.downloaded_bytes);

            // 检查是否满足分裂条件
            if chunk_speed < speed_threshold
                && remaining_bytes >= self.min_remaining_bytes_for_split
                && !self.recently_split.contains_key(&chunk.id)
            {
                // 定义分裂策略：从剩余部分的中间进行分裂
                let strategy = Arc::new(
                    move |current_offset: u64, current_end: u64| -> Option<u64> {
                        let remaining = current_end.saturating_sub(current_offset);
                        if remaining > 0 {
                            Some(current_offset + remaining / 2)
                        } else {
                            None
                        }
                    },
                );

                println!(
                    "Scheduling: Splitting slow chunk {} (speed: {:.2} KB/s, avg: {:.2} KB/s)",
                    chunk.id,
                    chunk_speed / 1024.0,
                    avg_speed / 1024.0
                );

                commands.push(EventThread::SplitChunk {
                    id: chunk.id,
                    strategy,
                });

                // 记录下来，进入冷却
                self.recently_split.insert(chunk.id, now);
            }
        }

        commands
    }
}

/// 监控与调度器
///
/// 运行在一个独立的异步任务中，负责接收状态更新并根据策略发出调度指令。
pub struct MonitorScheduler {
    /// 下载状态管理器
    state: DownloadState,
    /// 接收来自各分片的状态事件
    status_rx: mpsc::Receiver<EventStatus>,
    /// 广播命令给所有分片
    command_tx: broadcast::Sender<EventThread>,
    /// 定时器，用于触发周期性调度检查
    interval: Interval,
    /// 具体的调度策略实现
    strategy: Box<dyn SchedulingStrategy>,
}

impl MonitorScheduler {
    /// 创建一个新的监控调度器
    pub fn new(
        file_size: u64,
        status_rx: mpsc::Receiver<EventStatus>,
        command_tx: broadcast::Sender<EventThread>,
        interval_duration: Duration,
        strategy: Box<dyn SchedulingStrategy>,
    ) -> Self {
        Self {
            state: DownloadState::new(file_size),
            status_rx,
            command_tx,
            interval: tokio::time::interval(interval_duration),
            strategy,
        }
    }

    /// 运行监控调度循环
    ///
    /// 这个方法会一直运行，直到状态事件的发送端全部关闭。
    pub async fn run(mut self) {
        println!("MonitorScheduler is running.");
        loop {
            tokio::select! {
                // 偏向于优先处理状态更新，以保证数据最新
                biased;

                // 1. 处理来自下载分片的状态事件
                Some(event) = self.status_rx.recv() => {
                    // println!("Monitor received status event: {:?}", event);
                    self.state.handle_event(event);
                },

                // 2. 定时器触发，执行调度逻辑
                _ = self.interval.tick() => {
                    let commands = self.strategy.decide(&self.state);
                    for command in commands {
                        // 广播命令，如果失败（没有监听者），也无伤大雅
                        let _ = self.command_tx.send(command);
                    }

                    // 打印进度信息
                    println!(
                        "Progress: {:.2}% | Speed: {:.2} MB/s",
                        self.state.get_progress() * 100.0,
                        self.state.get_average_speed() / 1024.0 / 1024.0
                    );
                },

                // 如果 status_rx 关闭，则退出循环
                else => {
                    break;
                }
            }
        }
        println!("MonitorScheduler has shut down.");
    }
}
