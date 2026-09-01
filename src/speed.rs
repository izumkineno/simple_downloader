//! 滑动窗口速度估算器，对齐主流下载器口径（aria2 / curl）。
//!
//! 主流方案调研（2026-09-01）：
//! - **aria2 `SpeedCalc.cc`**: `WINDOW_TIME=10s`，`1s`粒度桶聚合，`bytesWindow`为窗口内总字节，
//!   `speed = bytesWindow * 1000 / elapsed_ms`（`elapsed=now - oldestSlot`），`avg = accumulated / totalElapsed`。
//!   `removeStaleTimeSlot`逐出>10s旧槽，`update`同秒内合并到尾桶，否则新建桶。
//! - **curl `progress.c`**: 环形 `CURL_SPEED_RECORDS`（默认~10），每`>=1s`新建`speed_amount/speed_time`记录，
//!   `current_speed = (latest_amount-oldest_amount)*1e6 / (latest_time-oldest_time)`，
//!   `avg = cur_size / total_spent_us`。与aria2同为“窗口增量/窗口时长”，非EMA。
//! - **Chrome/Chromium**：`DownloadItem`同样用最近2-3s滑动窗口计算`CurrentSpeed`，与系统任务管理器一致。
//!
//! 本估算器复刻 aria2 逻辑，但窗口可配：默认全局`5s`（比aria2的10s更贴近系统任务管理器的3s响应，
//!   又避免0.5s抖动），桶粒度由调用方`update_interval`决定（`monitor.tick` 0.2-0.5s即天然桶）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 滑动窗口速度估算器（curl 环形缓冲语义 + aria2 窗口语义的简化融合）。
/// 存储 (时间, 累计总量)，窗口内速度 = (最新总量-最旧总量)/窗口时长。
/// 首个窗口未填满时退化为瞬时 delta/interval，避免 0 时长突增。
#[derive(Debug)]
pub struct SpeedEstimator {
    window: Duration,
    slots: VecDeque<(Instant, u64)>,
    start: Option<Instant>,
    max_speed: f64,
}

impl SpeedEstimator {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            slots: VecDeque::new(),
            start: None,
            max_speed: 0.0,
        }
    }

    pub fn reset(&mut self, now: Instant) {
        self.slots.clear();
        self.start = Some(now);
        self.max_speed = 0.0;
    }

    fn ensure_started(&mut self, now: Instant) {
        if self.start.is_none() {
            self.start = Some(now);
        }
    }

    fn remove_stale(&mut self, now: Instant) {
        while let Some((t, _)) = self.slots.front() {
            if now.duration_since(*t) <= self.window {
                break;
            }
            self.slots.pop_front();
        }
    }

    /// 观测累计总量（与 curl `speed_amount` 同义）。每 tick 调用一次。
    pub fn observe(&mut self, total: u64, now: Instant) {
        self.ensure_started(now);
        self.remove_stale(now);
        // 去重：同瞬间的重复观测合并
        if let Some((last_t, last_total)) = self.slots.back_mut() {
            if *last_t == now {
                *last_total = total;
                return;
            }
        }
        self.slots.push_back((now, total));
        // 保持窗口外最旧点用于时长计算，超窗后上面 remove_stale 会逐出
    }

    /// 兼容旧接口：增量更新（内部转为累计）。保留以兼容已有调用方。
    pub fn update(&mut self, bytes: u64, now: Instant) {
        let total = self.slots.back().map(|(_, v)| *v).unwrap_or(0) + bytes;
        self.observe(total, now);
    }

    /// 窗口速度 bytes/s（主流： (最新-最旧)/时长 ）。窗口未满时退化避免 0 除。
    pub fn speed(&mut self, now: Instant) -> f64 {
        self.remove_stale(now);
        if self.slots.len() < 2 {
            return 0.0;
        }
        let (oldest_t, oldest_v) = self.slots.front().unwrap();
        let (latest_t, latest_v) = self.slots.back().unwrap();
        let amount = latest_v.saturating_sub(*oldest_v) as f64;
        let mut elapsed_ms = latest_t.duration_since(*oldest_t).as_millis() as f64;
        if elapsed_ms < 1.0 {
            elapsed_ms = 1.0;
        }
        let s = amount * 1000.0 / elapsed_ms;
        if s > self.max_speed {
            self.max_speed = s;
        }
        s
    }

    /// 平均速度 bytes/s（自 start 起，curl `trspeed` 语义）。
    pub fn avg_speed(&self, now: Instant) -> f64 {
        let Some(start) = self.start else { return 0.0 };
        let Some((_, latest_v)) = self.slots.back() else { return 0.0 };
        let elapsed_ms = now.duration_since(start).as_millis() as f64;
        if elapsed_ms <= 4.0 {
            return 0.0;
        }
        *latest_v as f64 * 1000.0 / elapsed_ms
    }

    pub fn max_speed(&self) -> f64 { self.max_speed }
    pub fn window_bytes(&self) -> u64 {
        if self.slots.len() < 2 { return 0; }
        let (_, oldest) = self.slots.front().unwrap();
        let (_, latest) = self.slots.back().unwrap();
        latest.saturating_sub(*oldest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_and_avg() {
        let mut est = SpeedEstimator::new(Duration::from_secs(10));
        let t0 = Instant::now();
        est.update(1000, t0);
        let s0 = est.speed(t0);
        assert!(s0 >= 0.0);
        // 1s后追加
        let t1 = t0 + Duration::from_secs(1);
        est.update(1000, t1);
        let s1 = est.speed(t1);
        assert!(s1 > 0.0);
        let avg = est.avg_speed(t1);
        assert!(avg > 0.0);
    }
}
