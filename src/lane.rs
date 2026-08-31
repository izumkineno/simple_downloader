use crate::limiter::RateLimiter;
use crate::types::{DownloadError, Result};
use crate::util::{ensure_user_agent, get_file_info};
use faststr::FastStr;
use futures_util::stream::{FuturesUnordered, StreamExt};
#[cfg(feature = "proxy")]
use reqwest::Proxy;
use reqwest::{Client, ClientBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
const BLACKLIST_THRESHOLD: u32 = 3;
const BLACKLIST_DURATION: Duration = Duration::from_secs(30);
/// 多源调度中 lane 的建模维度。
///
/// - `PerSource`: 每个源一个 lane（默认），同一源的所有 chunk 共享该源的 `Client`。
/// - `PerSourceProxy`: 每个 `源×代理` 组合一个 lane，需启用 `proxy` feature。
///
/// 配合 `MultiSourceConfig::with_lane_model()` 使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneModel {
    /// 按源维度调度（默认）。
    PerSource,
    /// 按源×代理维度调度，需 `proxy` feature。
    #[cfg(feature = "proxy")]
    PerSourceProxy,
}

/// lane 健康状态（内部调度使用，导出供调试/观测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneHealth {
    /// 健康，可继续分配 chunk。
    Healthy,
    /// 已黑名单（连续失败 `>= BLACKLIST_THRESHOLD=3`），调度器将跳过。
    Blacklisted,
}

/// 代理配置（需 `proxy` feature）。
///
/// # 示例
///
/// ```no_run
/// # #[cfg(feature = "proxy")]
/// # {
/// use simple_downloader::ProxyConfig;
/// let p = ProxyConfig::http("http://proxy.example.com:8080").unwrap()
///     .with_id("proxy-1");
/// # }
/// ```
#[cfg(feature = "proxy")]
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// 代理唯一标识
    pub id: FastStr,
    /// 代理 URL，如 `http://host:port` / `socks5://host:port`
    pub url: FastStr,
}

#[cfg(feature = "proxy")]
impl ProxyConfig {
    /// 创建代理配置，`id` 默认为 `proxy-{url}`。
    pub fn new(url: impl Into<FastStr>) -> Self {
        let url = url.into();
        let id = FastStr::from_string(format!("proxy-{}", url));
        Self { id, url }
    }

    /// 覆盖代理 `id`。
    pub fn with_id(mut self, id: impl Into<FastStr>) -> Self {
        self.id = id.into();
        self
    }

    /// 快捷构造 HTTP 代理（等价 `new`）。
    pub fn http(url: impl Into<FastStr>) -> Result<Self> {
        Ok(Self::new(url))
    }
    /// 快捷构造 HTTPS 代理。
    pub fn https(url: impl Into<FastStr>) -> Result<Self> {
        Ok(Self::new(url))
    }
    /// 快捷构造 SOCKS5 代理。
    pub fn socks5(url: impl Into<FastStr>) -> Result<Self> {
        Ok(Self::new(url))
    }
}

/// 单个下载源配置。
///
/// 最小调用：
///
/// ```no_run
/// use simple_downloader::SourceConfig;
/// let s = SourceConfig::new("https://mirror.example.com/file.bin")
///     .with_id("mirror-1");
/// ```
///
/// 代理（需 `proxy` feature）：
///
/// ```no_run
/// # #[cfg(feature = "proxy")]
/// # {
/// use simple_downloader::{SourceConfig, ProxyConfig};
/// let s = SourceConfig::new("https://example.com/file.bin")
///     .with_proxies(vec![ProxyConfig::http("http://proxy:8080").unwrap()]);
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SourceConfig {
    /// 源唯一标识，默认 `source-{url}`
    pub id: FastStr,
    /// 源 URL
    pub url: FastStr,
    /// 该源绑定的代理列表（需 `proxy` feature）
    #[cfg(feature = "proxy")]
    pub proxies: Vec<ProxyConfig>,
    /// 该源限速（bytes/s），`rate-limit` feature 生效
    pub speed_limit: Option<u64>,
    /// 该源突发（bytes），`rate-limit` feature 生效
    #[cfg(feature = "rate-limit")]
    pub burst: Option<u64>,
}

impl SourceConfig {
    /// 创建源配置。
    pub fn new(url: impl Into<FastStr>) -> Self {
        let url = url.into();
        let id = FastStr::from_string(format!("source-{}", url));
        Self {
            id,
            url,
            #[cfg(feature = "proxy")]
            proxies: Vec::new(),
            speed_limit: None,
            #[cfg(feature = "rate-limit")]
            burst: None,
        }
    }

    /// 覆盖源 `id`（用于日志/统计辨识）。
    pub fn with_id(mut self, id: impl Into<FastStr>) -> Self {
        self.id = id.into();
        self
    }

    /// 设置该源限速（bytes/s），`rate-limit` 生效。`0` 将在下载时返回 `InvalidArgument`。
    #[cfg(feature = "rate-limit")]
    pub fn with_speed_limit(mut self, bytes_per_sec: u64) -> Self {
        self.speed_limit = Some(bytes_per_sec);
        self
    }

    /// 设置该源突发容量（bytes），`rate-limit` 生效。默认 64KiB。
    #[cfg(feature = "rate-limit")]
    pub fn with_burst(mut self, burst_bytes: u64) -> Self {
        self.burst = Some(burst_bytes);
        self
    }

    /// 为该源绑定代理列表（需 `proxy` feature）。
    #[cfg(feature = "proxy")]
    pub fn with_proxies(mut self, proxies: Vec<ProxyConfig>) -> Self {
        self.proxies = proxies;
        self
    }
}

/// 多源下载配置聚合。
///
/// # 示例
///
/// ```no_run
/// use simple_downloader::{MultiSourceConfig, SourceConfig, LaneModel};
/// let cfg = MultiSourceConfig::new("output.bin", 32, 0.5)
///     .with_sources(vec![
///         SourceConfig::new("https://mirror1.example.com/file.bin").with_id("m1"),
///         SourceConfig::new("https://mirror2.example.com/file.bin"),
///     ])
///     .with_lane_model(LaneModel::PerSource)
///     .with_max_chunks_per_lane(2)
///     .with_max_chunks_per_source(Some(8));
/// ```
#[derive(Debug, Clone)]
pub struct MultiSourceConfig {
    /// 输出文件路径
    pub output_path: FastStr,
    /// 总并发上限，受 §11 自动降级约束
    pub workers: u64,
    /// `MonitorUpdate` 广播间隔（秒）
    pub update_interval: f64,
    /// 源列表
    pub sources: Vec<SourceConfig>,
    /// lane 建模维度
    pub lane_model: LaneModel,
    /// 每个 lane 最大并发 chunk 数
    pub max_chunks_per_lane: usize,
    /// 每个源最大并发 chunk 数（`None` 不限）
    pub max_chunks_per_source: Option<usize>,
    /// 全局限速（bytes/s），`rate-limit` 生效
    pub global_speed_limit: Option<u64>,
    /// 全局突发（bytes），`rate-limit` 生效
    pub global_burst: Option<u64>,
}

impl MultiSourceConfig {
    /// 创建多源配置，`workers` 会在内部 `max(1, workers)`，`lane_model` 默认 `PerSource`。
    pub fn new(output_path: impl Into<FastStr>, workers: u64, update_interval: f64) -> Self {
        Self {
            output_path: output_path.into(),
            workers,
            update_interval,
            sources: Vec::new(),
            lane_model: LaneModel::PerSource,
            max_chunks_per_lane: 1,
            max_chunks_per_source: None,
            global_speed_limit: None,
            global_burst: None,
        }
    }

    /// 注入源列表。
    pub fn with_sources(mut self, sources: Vec<SourceConfig>) -> Self {
        self.sources = sources;
        self
    }

    /// 设置 lane 建模维度。
    pub fn with_lane_model(mut self, lane_model: LaneModel) -> Self {
        self.lane_model = lane_model;
        self
    }

    /// 设置每个 lane 最大并发 chunk 数（`max(1, n)`）。
    pub fn with_max_chunks_per_lane(mut self, max_chunks_per_lane: usize) -> Self {
        self.max_chunks_per_lane = max_chunks_per_lane.max(1);
        self
    }

    /// 设置每个源最大并发 chunk 数，`None` 表示不限。
    pub fn with_max_chunks_per_source(mut self, max_chunks_per_source: Option<usize>) -> Self {
        self.max_chunks_per_source = max_chunks_per_source;
        self
    }

    /// 设置全局限速（bytes/s），`rate-limit` 生效。
    #[cfg(feature = "rate-limit")]
    pub fn with_global_speed_limit(mut self, bytes_per_sec: u64) -> Self {
        self.global_speed_limit = Some(bytes_per_sec);
        self
    }

    /// 设置全局突发（bytes），`rate-limit` 生效。
    #[cfg(feature = "rate-limit")]
    pub fn with_global_burst(mut self, burst_bytes: u64) -> Self {
        self.global_burst = Some(burst_bytes);
        self
    }
}

#[derive(Debug, Clone)]
pub struct LaneCandidate {
    pub lane_id: FastStr,
    pub source_id: FastStr,
    pub proxy_id: Option<FastStr>,
    pub probe_speed: f64,
}

impl LaneCandidate {
    #[cfg_attr(not(any(test, feature = "multi-source")), allow(dead_code))]
    pub fn new(
        lane_id: impl Into<FastStr>,
        source_id: impl Into<FastStr>,
        proxy_id: Option<impl Into<FastStr>>,
        probe_speed: f64,
    ) -> Self {
        Self {
            lane_id: lane_id.into(),
            source_id: source_id.into(),
            proxy_id: proxy_id.map(Into::into),
            probe_speed,
        }
    }
}
#[derive(Debug, Clone)]
struct LaneEntry {
    candidate: LaneCandidate,
    active_chunks: usize,
    consecutive_failures: u32,
    health: LaneHealth,
    blacklisted_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct LaneScheduler {
    lane_model: LaneModel,
    max_workers: usize,
    max_chunks_per_lane: usize,
    max_chunks_per_source: Option<usize>,
    lanes: Vec<LaneEntry>,
}

fn dedupe_candidates(candidates: Vec<LaneCandidate>) -> Vec<LaneCandidate> {
    let mut deduped = Vec::new();
    for candidate in candidates {
        if deduped
            .iter()
            .any(|existing: &LaneCandidate| existing.lane_id == candidate.lane_id)
        {
            continue;
        }
        deduped.push(candidate);
    }
    deduped
}

fn normalize_candidates(
    candidates: Vec<LaneCandidate>,
    lane_model: LaneModel,
) -> Vec<LaneCandidate> {
    match lane_model {
        LaneModel::PerSource => candidates
            .into_iter()
            .map(|mut candidate| {
                candidate.lane_id = candidate.source_id.clone();
                candidate.proxy_id = None;
                candidate
            })
            .collect(),
        #[cfg(feature = "proxy")]
        LaneModel::PerSourceProxy => candidates,
    }
}

impl LaneScheduler {
    pub fn from_candidates(
        candidates: Vec<LaneCandidate>,
        lane_model: LaneModel,
        max_workers: usize,
        max_chunks_per_lane: usize,
        max_chunks_per_source: Option<usize>,
    ) -> Self {
        let mut lanes: Vec<_> = dedupe_candidates(normalize_candidates(candidates, lane_model))
            .into_iter()
            .map(|candidate| LaneEntry {
                candidate,
                active_chunks: 0,
                consecutive_failures: 0,
                health: LaneHealth::Healthy,
                blacklisted_at: None,
            })
            .collect();
        lanes.sort_by(|a, b| b.candidate.probe_speed.total_cmp(&a.candidate.probe_speed));
        ::tracing::debug!(lanes = lanes.len(), model = ?lane_model, max_workers, "LaneScheduler created");
        Self {
            lane_model,
            max_workers: max_workers.max(1),
            max_chunks_per_lane: max_chunks_per_lane.max(1),
            max_chunks_per_source,
            lanes,
        }
    }

    pub fn available_capacity(&self) -> usize {
        self.max_workers.saturating_sub(self.total_active_chunks())
    }

    pub fn best_lane(&mut self) -> Option<FastStr> {
        self.decay_expired_blacklists();
        // 先尝试健康 lane，容量不足或全黑时再退化到黑名单
        if let Some(entry) = self.select_lane(false) {
            return Some(entry.candidate.lane_id.clone());
        }
        let fallback = self
            .select_lane(true)
            .map(|entry| entry.candidate.lane_id.clone());
        if fallback.is_some() {
            ::tracing::debug!("best_lane fallback to blacklisted lane");
        }
        fallback
    }

    fn decay_expired_blacklists(&mut self) {
        for entry in &mut self.lanes {
            if entry.health == LaneHealth::Blacklisted {
                if let Some(at) = entry.blacklisted_at {
                    if at.elapsed() >= BLACKLIST_DURATION {
                        ::tracing::info!(lane_id = %entry.candidate.lane_id, "lane blacklist expired -> Healthy");
                        entry.health = LaneHealth::Healthy;
                        entry.consecutive_failures = 0;
                        entry.blacklisted_at = None;
                    }
                }
            }
        }
    }

    #[cfg_attr(not(any(test, feature = "multi-source")), allow(dead_code))]
    pub fn lane_ids(&self) -> Vec<FastStr> {
        self.lanes
            .iter()
            .map(|entry| entry.candidate.lane_id.clone())
            .collect()
    }
    pub fn assign_chunk(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            entry.active_chunks += 1;
            ::tracing::trace!(lane_id = %entry.candidate.lane_id, active = entry.active_chunks, "assign_chunk");
        }
    }

    pub fn release_chunk(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            entry.active_chunks = entry.active_chunks.saturating_sub(1);
            ::tracing::trace!(lane_id = %entry.candidate.lane_id, active = entry.active_chunks, "release_chunk");
        }
    }

    pub fn record_failure(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            entry.consecutive_failures += 1;
            ::tracing::warn!(lane_id = %entry.candidate.lane_id, consecutive = entry.consecutive_failures, threshold = BLACKLIST_THRESHOLD, "lane failure");
            if entry.consecutive_failures >= BLACKLIST_THRESHOLD {
                ::tracing::warn!(lane_id = %entry.candidate.lane_id, "lane blacklisted");
                entry.health = LaneHealth::Blacklisted;
                entry.blacklisted_at = Some(Instant::now());
            }
        }
    }

    pub fn record_success(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            if entry.consecutive_failures > 0 || entry.health != LaneHealth::Healthy {
                ::tracing::debug!(lane_id = %entry.candidate.lane_id, "lane success -> reset health");
            }
            entry.consecutive_failures = 0;
            entry.health = LaneHealth::Healthy;
            entry.blacklisted_at = None;
        }
    }

    #[cfg_attr(not(any(test, feature = "multi-source")), allow(dead_code))]
    pub fn lane_health(&self, lane_id: impl AsRef<str>) -> Option<LaneHealth> {
        self.lanes
            .iter()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
            .map(|entry| {
                if entry.health == LaneHealth::Blacklisted {
                    if let Some(at) = entry.blacklisted_at {
                        if at.elapsed() >= BLACKLIST_DURATION {
                            return LaneHealth::Healthy;
                        }
                    }
                }
                entry.health
            })
    }

    #[allow(dead_code)]
    pub fn set_blacklisted_at_for_test(&mut self, lane_id: &str, at: Instant) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id)
        {
            entry.health = LaneHealth::Blacklisted;
            entry.consecutive_failures = BLACKLIST_THRESHOLD;
            entry.blacklisted_at = Some(at);
        }
    }

    // 供集成测试使用：与 #[cfg(test)] 的单元测试不同，integration test 编译时 library 的 cfg(test) 为 false，需额外暴露
    #[allow(dead_code)]
    pub fn set_blacklisted_at_for_integration_test(&mut self, lane_id: &str, at: Instant) {
        self.set_blacklisted_at_for_test(lane_id, at);
    }
    fn total_active_chunks(&self) -> usize {
        self.lanes.iter().map(|entry| entry.active_chunks).sum()
    }

    fn source_active_chunks(&self, source_id: &str) -> usize {
        self.lanes
            .iter()
            .filter(|entry| entry.candidate.source_id.as_str() == source_id)
            .map(|entry| entry.active_chunks)
            .sum()
    }

    fn lane_has_capacity(&self, entry: &LaneEntry) -> bool {
        if self.available_capacity() == 0 {
            return false;
        }
        if entry.active_chunks >= self.max_chunks_per_lane {
            return false;
        }
        if matches!(self.lane_model, LaneModel::PerSource)
            && let Some(max_chunks_per_source) = self.max_chunks_per_source
            && self.source_active_chunks(entry.candidate.source_id.as_str())
                >= max_chunks_per_source
        {
            return false;
        }
        true
    }

    fn select_lane(&self, allow_blacklisted: bool) -> Option<&LaneEntry> {
        self.lanes.iter().find(|entry| {
            (allow_blacklisted || entry.health == LaneHealth::Healthy)
                && self.lane_has_capacity(entry)
        })
    }
}

#[derive(Debug, Clone)]
pub struct LaneRuntime {
    pub lane_id: FastStr,
    pub source_id: FastStr,
    pub proxy_id: Option<FastStr>,
    pub url: FastStr,
    pub client: Client,
    pub probe_speed: f64,
}

#[derive(Debug, Clone)]
pub struct MultiRuntime {
    scheduler: LaneScheduler,
    runtimes: HashMap<FastStr, Vec<LaneRuntime>>,
    next_runtime_index: HashMap<FastStr, usize>,
    pub supports_ranges: bool,
    per_source_limiters: HashMap<FastStr, Arc<RateLimiter>>,
    global_limiter: Option<Arc<RateLimiter>>,
}

impl MultiRuntime {
    #[::tracing::instrument(skip(config, client_builder), fields(sources = config.sources.len(), workers = config.workers, path = %config.output_path))]
    pub async fn from_config<F>(
        config: &MultiSourceConfig,
        client_builder: &F,
    ) -> Result<(u64, Self)>
    where
        F: Fn() -> ClientBuilder,
    {
        let expanded = expand_lanes(config, client_builder)?;
        ::tracing::debug!(expanded = expanded.len(), "expanded lanes");
        let mut probe_futs = FuturesUnordered::new();
        for runtime in expanded {
            let url = runtime.url.clone();
            let client = runtime.client.clone();
            probe_futs.push(async move {
                let res = get_file_info(&client, url.as_str()).await;
                (runtime, res)
            });
        }
        let mut range_file_size = None;
        let mut fallback_file_size = None;
        let mut range_runtimes: HashMap<FastStr, Vec<LaneRuntime>> = HashMap::new();
        let mut fallback_runtimes: HashMap<FastStr, Vec<LaneRuntime>> = HashMap::new();
        let mut range_candidates = Vec::new();
        let mut fallback_candidates = Vec::new();

        while let Some((mut runtime, res)) = probe_futs.next().await {
            match res {
                Ok((file_size, support_ranges)) => {
                    ::tracing::debug!(lane_id = %runtime.lane_id, url = %runtime.url, file_size, support_ranges, "probe success");
                    if support_ranges {
                        if let Some(expected) = range_file_size {
                            if expected != file_size {
                                ::tracing::warn!(lane_id = %runtime.lane_id, expected, got = file_size, "probe file size mismatch, skipping lane");
                                continue;
                            }
                        } else {
                            range_file_size = Some(file_size);
                            // 若此前已收集 fallback 但文件大小不一致，清空 fallback 以避免混用
                            if let Some(fb) = fallback_file_size {
                                if fb != file_size {
                                    fallback_candidates.clear();
                                    fallback_runtimes.clear();
                                    fallback_file_size = None;
                                }
                            }
                        }
                        // M3-01 实测 probe_speed：64KiB 采样替代硬编码 1.0
                        let measured = {
                            let start = Instant::now();
                            let resp_res = ensure_user_agent(
                                runtime
                                    .client
                                    .get(runtime.url.as_str())
                                    .header("Range", "bytes=0-65535"),
                            )
                            .send()
                            .await;
                            match resp_res {
                                Ok(r) => match r.bytes().await {
                                    Ok(b) => {
                                        let elapsed = start.elapsed().as_secs_f64().max(0.001);
                                        let s = b.len() as f64 / elapsed;
                                        ::tracing::info!(lane_id = %runtime.lane_id, bytes = b.len(), elapsed, speed = s, "probe_speed measured (range)");
                                        if s > 0.0 { s } else { 1.0 }
                                    }
                                    Err(e) => {
                                        ::tracing::warn!(lane_id = %runtime.lane_id, error = %e, "probe_speed bytes read failed, fallback 1.0");
                                        1.0
                                    }
                                },
                                Err(e) => {
                                    ::tracing::warn!(lane_id = %runtime.lane_id, error = %e, "probe_speed request failed, fallback 1.0");
                                    1.0
                                }
                            }
                        };
                        runtime.probe_speed = measured;
                        range_candidates.push(LaneCandidate {
                            lane_id: runtime.lane_id.clone(),
                            source_id: runtime.source_id.clone(),
                            proxy_id: runtime.proxy_id.clone(),
                            probe_speed: runtime.probe_speed,
                        });
                        range_runtimes
                            .entry(runtime.lane_id.clone())
                            .or_default()
                            .push(runtime);
                    } else {
                        // 非 Range 仅作为 fallback：仅在无 Range 可用时启用
                        if range_file_size.is_some() {
                            ::tracing::debug!(lane_id = %runtime.lane_id, "non-range lane skipped because range lanes exist");
                            continue;
                        }
                        if let Some(expected) = fallback_file_size {
                            if expected != file_size {
                                ::tracing::warn!(lane_id = %runtime.lane_id, expected, got = file_size, "fallback size mismatch, skipping");
                                continue;
                            }
                        } else {
                            fallback_file_size = Some(file_size);
                        }
                        // M3-01 fallback 同样实测（无 Range 则全量采样首 64KiB，流式限长避免 OOM）
                        let measured = {
                            let start = Instant::now();
                            let resp_res = ensure_user_agent(
                                runtime
                                    .client
                                    .get(runtime.url.as_str())
                                    .header("Range", "bytes=0-65535"),
                            )
                            .send()
                            .await;
                            match resp_res {
                                Ok(r) => {
                                    let mut stream = r.bytes_stream();
                                    let mut total: usize = 0;
                                    let mut stream_err: Option<String> = None;
                                    while let Some(chunk) = stream.next().await {
                                        match chunk {
                                            Ok(bytes) => {
                                                total = total.saturating_add(bytes.len());
                                                if total >= 65535 {
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                stream_err = Some(e.to_string());
                                                break;
                                            }
                                        }
                                    }
                                    drop(stream);
                                    if let Some(err) = stream_err {
                                        ::tracing::warn!(lane_id = %runtime.lane_id, error = %err, "probe_speed fallback bytes failed, 1.0");
                                        1.0
                                    } else {
                                        let elapsed = start.elapsed().as_secs_f64().max(0.001);
                                        let s = total as f64 / elapsed;
                                        ::tracing::info!(lane_id = %runtime.lane_id, bytes = total, elapsed, speed = s, "probe_speed measured (fallback)");
                                        if s > 0.0 { s } else { 1.0 }
                                    }
                                }
                                Err(e) => {
                                    ::tracing::warn!(lane_id = %runtime.lane_id, error = %e, "probe_speed fallback request failed, 1.0");
                                    1.0
                                }
                            }
                        };
                        runtime.probe_speed = measured;
                        fallback_candidates.push(LaneCandidate {
                            lane_id: runtime.lane_id.clone(),
                            source_id: runtime.source_id.clone(),
                            proxy_id: runtime.proxy_id.clone(),
                            probe_speed: runtime.probe_speed,
                        });
                        fallback_runtimes
                            .entry(runtime.lane_id.clone())
                            .or_default()
                            .push(runtime);
                    }
                }
                Err(e) => {
                    ::tracing::warn!(lane_id = %runtime.lane_id, url = %runtime.url, error = %e, "probe failed, lane skipped");
                    continue;
                }
            }
        }
        let (file_size, supports_ranges, runtimes, candidates) = if !range_candidates.is_empty() {
            (
                range_file_size.ok_or(DownloadError::NoAvailableSources)?,
                true,
                range_runtimes,
                range_candidates,
            )
        } else if !fallback_candidates.is_empty() {
            (
                fallback_file_size.ok_or(DownloadError::NoAvailableSources)?,
                false,
                fallback_runtimes,
                fallback_candidates,
            )
        } else {
            ::tracing::error!("no available sources after probe");
            return Err(DownloadError::NoAvailableSources);
        };

        ::tracing::info!(
            file_size,
            supports_ranges,
            lanes = candidates.len(),
            "multi-source probe done"
        );

        let scheduler = LaneScheduler::from_candidates(
            candidates,
            config.lane_model,
            config.workers as usize,
            config.max_chunks_per_lane,
            config.max_chunks_per_source,
        );

        // 限速：per_source + global（仅 rate-limit 生效，否则空）
        #[cfg(feature = "rate-limit")]
        let per_source_limiters: HashMap<FastStr, Arc<RateLimiter>> = {
            let mut map: HashMap<FastStr, Arc<RateLimiter>> = HashMap::new();
            for src in &config.sources {
                if let Some(limit) = src.speed_limit {
                    if limit == 0 {
                        return Err(DownloadError::InvalidArgument(format!("source {} speed_limit 0 无效", src.id)));
                    }
                    if limit > u32::MAX as u64 {
                        return Err(DownloadError::InvalidArgument(format!("source {} speed_limit {} 超过 {} 需 ≤4GiB/s", src.id, limit, u32::MAX)));
                    }
                    if let Some(b) = src.burst {
                        if b == 0 {
                            return Err(DownloadError::InvalidArgument(format!("source {} burst 0 无效", src.id)));
                        }
                        if b > u32::MAX as u64 {
                            return Err(DownloadError::InvalidArgument(format!("source {} burst {} 超过 {}", src.id, b, u32::MAX)));
                        }
                    }
                    let burst = src.burst.and_then(|b| std::num::NonZeroU32::new(b as u32)).or_else(|| std::num::NonZeroU32::new(64*1024));
                    let nz = std::num::NonZeroU32::new(limit as u32).unwrap();
                    map.insert(src.id.clone(), Arc::new(RateLimiter::new(nz, burst)));
                } else if src.burst.is_some() {
                    return Err(DownloadError::InvalidArgument(format!("source {} burst 需配合 speed_limit", src.id)));
                }
            }
            map
        };
        #[cfg(not(feature = "rate-limit"))]
        let per_source_limiters: HashMap<FastStr, Arc<RateLimiter>> = HashMap::new();
        #[cfg(feature = "rate-limit")]
        let global_limiter: Option<Arc<RateLimiter>> = if let Some(limit) = config.global_speed_limit {
            if limit == 0 {
                return Err(DownloadError::InvalidArgument("global_speed_limit 0 无效".to_string()));
            }
            if limit > u32::MAX as u64 {
                return Err(DownloadError::InvalidArgument(format!("global_speed_limit {} 超过 {} 需 ≤4GiB/s", limit, u32::MAX)));
            }
            if let Some(b) = config.global_burst {
                if b == 0 {
                    return Err(DownloadError::InvalidArgument("global_burst 0 无效".to_string()));
                }
                if b > u32::MAX as u64 {
                    return Err(DownloadError::InvalidArgument(format!("global_burst {} 超过 {}", b, u32::MAX)));
                }
            }
            let burst = config.global_burst.and_then(|b| std::num::NonZeroU32::new(b as u32)).or_else(|| std::num::NonZeroU32::new(64*1024));
            let nz = std::num::NonZeroU32::new(limit as u32).unwrap();
            Some(Arc::new(RateLimiter::new(nz, burst)))
        } else {
            if config.global_burst.is_some() {
                return Err(DownloadError::InvalidArgument("global_burst 需配合 global_speed_limit".to_string()));
            }
            None
        };
        #[cfg(not(feature = "rate-limit"))]
        let global_limiter: Option<Arc<RateLimiter>> = None;
        Ok((
            file_size,
            Self {
                scheduler,
                runtimes,
                next_runtime_index: HashMap::new(),
                supports_ranges,
                per_source_limiters,
                global_limiter,
            },
        ))
    }
    pub fn limiter_for_lane(&self, lane_id: &str) -> Option<Arc<RateLimiter>> {
        let source_id = self.scheduler.lanes.iter().find(|e| e.candidate.lane_id.as_str() == lane_id).map(|e| e.candidate.source_id.clone())?;
        self.per_source_limiters.get(&source_id).cloned()
    }

    pub fn global_limiter(&self) -> Option<Arc<RateLimiter>> {
        self.global_limiter.clone()
    }

    pub fn has_rate_limit(&self) -> bool {
        !self.per_source_limiters.is_empty() || self.global_limiter.is_some()
    }

    pub fn claim_request_builder(&mut self) -> Option<(FastStr, reqwest::RequestBuilder)> {
        let lane_id = self.scheduler.best_lane()?;
        self.scheduler.assign_chunk(lane_id.as_str());
        let runtime = self.next_runtime(&lane_id)?;
        ::tracing::trace!(lane_id = %lane_id, url = %runtime.url, "claim lane");
        Some((lane_id, runtime.client.get(runtime.url.as_str())))
    }

    pub fn primary_lane(&mut self) -> Option<(Client, FastStr)> {
        let lane_id = self.scheduler.best_lane()?;
        let runtime = self.runtimes.get(&lane_id)?.first()?;
        Some((runtime.client.clone(), runtime.url.clone()))
    }

    pub fn lane_runtime(&self, lane_id: &FastStr) -> Option<&LaneRuntime> {
        self.runtimes.get(lane_id)?.first()
    }

    pub fn best_lane_runtime(&mut self) -> Option<&LaneRuntime> {
        let lane_id = self.scheduler.best_lane()?;
        self.lane_runtime(&lane_id)
    }

    fn next_runtime(&mut self, lane_id: &FastStr) -> Option<LaneRuntime> {
        let runtimes = self.runtimes.get(lane_id)?;
        let index = self.next_runtime_index.entry(lane_id.clone()).or_insert(0);
        let runtime = runtimes.get(*index % runtimes.len())?.clone();
        *index += 1;
        Some(runtime)
    }

    pub fn release_chunk(&mut self, lane_id: &FastStr) {
        self.scheduler.release_chunk(lane_id.as_str());
    }

    pub fn record_success(&mut self, lane_id: &FastStr) {
        self.scheduler.record_success(lane_id.as_str());
    }

    pub fn record_failure(&mut self, lane_id: &FastStr) {
        self.scheduler.record_failure(lane_id.as_str());
    }
}

fn expand_lanes<F>(config: &MultiSourceConfig, client_builder: &F) -> Result<Vec<LaneRuntime>>
where
    F: Fn() -> ClientBuilder,
{
    let mut runtimes = Vec::new();

    for source in &config.sources {
        #[cfg(not(feature = "proxy"))]
        {
            let client = (client_builder)()
                .pool_max_idle_per_host(32)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .tcp_keepalive(std::time::Duration::from_secs(60))
                .build()?;
            ::tracing::debug!(source_id = %source.id, url = %source.url, "expand lane (no proxy)");
            runtimes.push(LaneRuntime {
                lane_id: source.id.clone(),
                source_id: source.id.clone(),
                proxy_id: None,
                url: source.url.clone(),
                client,
                probe_speed: 0.0,
            });
            continue;
        }

        #[cfg(feature = "proxy")]
        if source.proxies.is_empty() {
            let client = (client_builder)()
                .pool_max_idle_per_host(32)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .tcp_keepalive(std::time::Duration::from_secs(60))
                .build()?;
            ::tracing::debug!(source_id = %source.id, url = %source.url, "expand lane (source has no proxies)");
            runtimes.push(LaneRuntime {
                lane_id: source.id.clone(),
                source_id: source.id.clone(),
                proxy_id: None,
                url: source.url.clone(),
                client,
                probe_speed: 0.0,
            });
            continue;
        }

        #[cfg(feature = "proxy")]
        for proxy in &source.proxies {
            let proxy_obj = match Proxy::all(proxy.url.as_str()) {
                Ok(p) => p,
                Err(error) => {
                    ::tracing::warn!(proxy_url = %proxy.url, error = %error, "proxy parse failed, skip lane");
                    continue;
                }
            };
            let client = match (client_builder)()
                .proxy(proxy_obj)
                .pool_max_idle_per_host(32)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .tcp_keepalive(std::time::Duration::from_secs(60))
                .build()
            {
                Ok(c) => c,
                Err(error) => {
                    ::tracing::warn!(proxy_url = %proxy.url, error = %error, "proxy client build failed, skip lane");
                    continue;
                }
            };
            let lane_id = match config.lane_model {
                LaneModel::PerSource => source.id.clone(),
                LaneModel::PerSourceProxy => {
                    FastStr::from_string(format!("{}::{}", source.id, proxy.id))
                }
            };
            ::tracing::debug!(lane_id = %lane_id, source_id = %source.id, proxy_id = %proxy.id, "expand lane (proxy)");
            runtimes.push(LaneRuntime {
                lane_id,
                source_id: source.id.clone(),
                proxy_id: Some(proxy.id.clone()),
                url: source.url.clone(),
                client,
                probe_speed: 0.0,
            });
        }
    }

    Ok(runtimes)
}
