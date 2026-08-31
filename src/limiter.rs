//! 基于 `governor` 的字节级令牌桶限速器（`rate-limit` feature 门面）。
//!
//! 设计：
//! - 1 token = 1 byte，`Quota::per_second(bytes)` + `allow_burst(burst)`；burst 默认 64KiB 硬限（满足 AC3 瞬时 ≤1.05×）
//! - `RateLimiter::acquire(n)` 批量获取 `n` 令牌，`n` 按 32-64KiB 聚合（见 `chunk.rs` 调用点），避免 1-4KiB 小片高频 `until_n_ready`
//! - 全局与分源为两级桶串联：`per_source.acquire` → `global.acquire`，全局为硬上限
//! - `QuantaClock` 与 `tokio::time` 解耦：测试用 `FakeClock` 可注入（当前用 `QuantaClock::default()`，后续可扩展）

#[cfg(feature = "rate-limit")]
mod imp {
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use governor::{
        clock::QuantaClock,
        state::{InMemoryState, NotKeyed},
        Quota, RateLimiter as GovLimiter,
    };

    /// 字节级限速器（`governor` 封装）。
    #[derive(Clone)]
    pub struct RateLimiter {
        inner: Arc<GovLimiter<NotKeyed, InMemoryState, QuantaClock>>,
    }

    impl RateLimiter {
        /// 创建限速器。
        /// `bytes_per_sec`: 每秒字节数（`NonZeroU32`，`0` 需在调用方校验为 `InvalidArgument`）
        /// `burst`: 突发容量，`None` → 默认 64KiB 硬限（满足 AC3），`Some` → 显式 burst
        pub fn new(bytes_per_sec: NonZeroU32, burst: Option<NonZeroU32>) -> Self {
            let burst_val = burst.unwrap_or_else(|| NonZeroU32::new(64 * 1024).unwrap());
            let quota = Quota::per_second(bytes_per_sec).allow_burst(burst_val);
            let limiter = GovLimiter::direct(quota);
            Self {
                inner: Arc::new(limiter),
            }
        }

        /// 批量获取 `n` 字节令牌（`n` 需 `NonZeroU32`，调用方保证 `1..=65536`）。
        /// 内部使用 `until_n_ready`，无 jitter（保证 burst=0 时瞬时 ≤1.05×）。
        pub async fn acquire(&self, n: NonZeroU32) {
            // governor 的 `until_n_ready` 在限速未满足时会 `sleep` 到下一个 refill 周期
            let _ = self.inner.until_n_ready(n).await;
        }

        /// 尝试非阻塞获取（用于测试/观测，可选）。
        #[allow(dead_code)]
        pub fn check_n(&self, n: NonZeroU32) -> bool {
            self.inner.check_n(n).is_ok()
        }
    }

    impl std::fmt::Debug for RateLimiter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RateLimiter").finish_non_exhaustive()
        }
    }
}

#[cfg(feature = "rate-limit")]
pub use imp::RateLimiter;

#[cfg(not(feature = "rate-limit"))]
mod no_impl {
    /// 占位：未启用 `rate-limit` 时的空类型（避免下游 `#[cfg]` 分支污染）。
    #[derive(Clone, Debug)]
    pub struct RateLimiter;
    impl RateLimiter {
        pub fn new(_: std::num::NonZeroU32, _: Option<std::num::NonZeroU32>) -> Self {
            Self
        }
        pub async fn acquire(&self, _: std::num::NonZeroU32) {}
    }
}

#[cfg(not(feature = "rate-limit"))]
pub use no_impl::RateLimiter;

#[cfg(all(test, feature = "rate-limit"))]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn burst_zero_hard() {
        let limiter = RateLimiter::new(NonZeroU32::new(1024 * 1024).unwrap(), None);
        let start = Instant::now();
        // 2MiB 以 1MiB/s 限速，burst=64KiB 硬限 → 应约 2s
        for _ in 0..32 {
            limiter
                .acquire(NonZeroU32::new(64 * 1024).unwrap())
                .await;
        }
        let elapsed = start.elapsed();
        // 允许 ±20% 抖动（CI 容器调度）
        assert!(
            elapsed >= Duration::from_millis(1800) && elapsed <= Duration::from_millis(2400),
            "elapsed {:?} not in 1.8-2.4s",
            elapsed
        );
    }

    #[tokio::test]
    async fn small_limit_not_deadlock() {
        let limiter = RateLimiter::new(NonZeroU32::new(5 * 1024).unwrap(), None);
        let start = Instant::now();
        limiter
            .acquire(NonZeroU32::new(5 * 1024).unwrap())
            .await;
        limiter
            .acquire(NonZeroU32::new(5 * 1024).unwrap())
            .await;
        assert!(start.elapsed() >= Duration::from_millis(900));
    }
}
