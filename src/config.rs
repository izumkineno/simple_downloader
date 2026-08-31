//! 运行时可更新配置（0.5.5 配置灵活性）
//!
//! 将 `DownloadBuilder` 冻结的 `workers/update_interval/limiter` 抽为
//! `SharedConfig(Arc<RwLock<RuntimeConfig>>)`，供 `DownloadMonitor::apply_config` 热更新，
//! 为后续 `queue pause/resume` 与 `99 智能评分 hot-update burst` 打前站。

use std::sync::Arc;

use parking_lot::RwLock;
/// 可在下载进行中热更新的运行时配置
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// 最大并发 workers（>=1）
    pub workers: u64,
    /// Monitor 聚合间隔秒（>0）
    pub update_interval: f64,
    /// 全局限速 bps（None=不限）
    pub speed_limit: Option<u64>,
    /// 桶 burst bytes
    pub burst: Option<u64>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            workers: std::thread::available_parallelism()
                .map(|p| p.get() as u64)
                .unwrap_or(4)
                .max(1),
            update_interval: 0.5,
            speed_limit: None,
            burst: None,
        }
    }
}

impl RuntimeConfig {
    pub fn with_workers(mut self, workers: u64) -> Self {
        self.workers = workers.max(1);
        self
    }
    pub fn with_update_interval(mut self, secs: f64) -> Self {
        if secs > 0.0 && secs.is_finite() {
            self.update_interval = secs;
        }
        self
    }
}

/// 线程安全的共享配置句柄
pub type SharedConfig = Arc<RwLock<RuntimeConfig>>;

pub fn new_shared(cfg: RuntimeConfig) -> SharedConfig {
    Arc::new(RwLock::new(cfg))
}

/// 热更新：返回旧值，调用方负责通知 monitor/limiter
pub fn apply_config(shared: &SharedConfig, new: RuntimeConfig) -> RuntimeConfig {
    let mut w = shared.write();
    let old = w.clone();
    *w = new;
    old
}
