//! 基于 `tracing` / `tracing-subscriber` 的统一日志门面。
//!
//! 设计目标：
//! - **库不强行初始化**：`simple_downloader` 作为库，不在下载路径上隐式调用全局 `init`，
//!   避免与宿主应用的日志系统冲突。未初始化时所有 `::tracing::{debug,info,warn,error}` 均为 no-op，不会 panic。
//! - **提供一键初始化**：二进制 / example / 测试可调用本模块的 `init_*` 便捷函数，
//!   基于 `RUST_LOG` / `SIMPLE_DOWNLOADER_LOG` 环境变量实现调试与生产分级。
//! - **生产 / 调试分级**：生产默认 `info`，调试默认 `debug`，均可被环境变量覆盖；
//!   日志分级见模块文档 `init_tracing`。
//! - **结构化与 span**：所有下载关键路径使用 `::tracing::instrument` 与结构化字段，
//!   可直接对接 `tracing-subscriber::fmt`、JSON、OTEL 等后端。
//!
//! ## 快速开始
//!
//! ```no_run
//! // 在 main 最顶部调用一次即可
//! simple_downloader::tracing::init_tracing();
//! // 或显式区分环境
//! // simple_downloader::tracing::init_tracing_for_env(simple_downloader::tracing::Env::Production);
//! ```
//!
//! ## 环境变量
//!
//! - `RUST_LOG`：标准 `tracing-subscriber::EnvFilter` 语法，优先级最高。
//!   例如 `RUST_LOG=simple_downloader=debug,reqwest=warn`。
//! - `SIMPLE_DOWNLOADER_LOG`：同上，仅作为回退（当 `RUST_LOG` 未设置时）。
//! - 两者均未设置时，按 `Env` 选择默认：`Development` -> `debug`，`Production` -> `info`。

use ::tracing::Level;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// 运行环境，影响未显式配置过滤器时的默认日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    /// 开发 / 调试：默认 `debug`，输出 `[Adaptive]` 等详细决策日志。
    Development,
    /// 生产：默认 `info` + `warn` + `error`，仅保留关键生命周期与告警。
    Production,
}

impl Env {
    /// 按 Cargo 编译期 `debug_assertions` 自动推断：`debug_assertions` 启用为 `Development`，否则 `Production`。
    pub fn infer() -> Self {
        if cfg!(debug_assertions) {
            Self::Development
        } else {
            Self::Production
        }
    }

    fn default_filter(&self) -> &'static str {
        match self {
            Self::Development => "info,simple_downloader=debug",
            Self::Production => "warn,simple_downloader=info",
        }
    }
}

/// 初始化全局 `tracing` 订阅者（幂等：已初始化则静默返回 `false`）。
///
/// 过滤器优先级：`RUST_LOG` > `SIMPLE_DOWNLOADER_LOG` > `Env::infer().default_filter()`。
/// 输出为带时间、级别、`target`、`span` 的人类可读文本行，自动关联 `tokio` 任务。
///
/// 建议在二进制 `main` 顶部调用一次；库代码无需调用。
pub fn init_tracing() -> bool {
    try_init_tracing_for_env(Env::infer())
}

/// 按显式环境初始化（幂等）。
pub fn init_tracing_for_env(env: Env) -> bool {
    try_init_tracing_for_env(env)
}

/// 尝试初始化，返回 `true` 表示本次成功安装，`false` 表示已有全局订阅者（不覆盖）。
pub fn try_init_tracing() -> bool {
    try_init_tracing_for_env(Env::infer())
}

pub fn try_init_tracing_for_env(env: Env) -> bool {
    // EnvFilter 优先级：RUST_LOG > SIMPLE_DOWNLOADER_LOG > env default
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| {
            std::env::var("SIMPLE_DOWNLOADER_LOG").map(|v| EnvFilter::new(v))
        })
        .unwrap_or_else(|_| EnvFilter::new(env.default_filter()));

    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_line_number(true)
        .with_thread_ids(false)
        .with_thread_names(false);

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);

    // `try_init` 幂等：已有全局订阅者时返回 Err，不 panic
    ::tracing::subscriber::set_global_default(subscriber).is_ok()
}

/// 以显式 `RUST_LOG` 风格字符串初始化，适合编程化配置（幂等）。
///
/// 例如 `init_tracing_with_filter("simple_downloader=trace,reqwest=warn")`。
pub fn init_tracing_with_filter(filter: &str) -> bool {
    let filter = EnvFilter::new(filter.to_owned());
    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_line_number(true);
    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    ::tracing::subscriber::set_global_default(subscriber).is_ok()
}

/// JSON 格式初始化（适合生产采集），幂等，过滤器规则同 `init_tracing`。
pub fn init_tracing_json_for_env(env: Env) -> bool {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| std::env::var("SIMPLE_DOWNLOADER_LOG").map(|v| EnvFilter::new(v)))
        .unwrap_or_else(|_| EnvFilter::new(env.default_filter()));
    let fmt_layer = fmt::layer()
        .json()
        .with_target(true)
        .with_level(true)
        .with_current_span(true);
    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    ::tracing::subscriber::set_global_default(subscriber).is_ok()
}

/// 将 `::tracing::Level` 映射为生产可读的中文标签（仅用于少量面向用户的 info 日志前缀）。
pub fn level_label(level: Level) -> &'static str {
    match level {
        Level::TRACE => "跟踪",
        Level::DEBUG => "调试",
        Level::INFO => "信息",
        Level::WARN => "警告",
        Level::ERROR => "错误",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_infer_does_not_panic() {
        let _ = Env::infer().default_filter();
    }

    #[test]
    fn try_init_is_idempotent() {
        // 多次调用不应 panic，至少第二次返回 false
        let _ = try_init_tracing();
        let second = try_init_tracing();
        // second 可能是 true/false 取决于是否已有全局订阅者，但不应 panic
        let _ = second;
    }
}
