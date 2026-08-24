use crate::types::{DownloadError, Result};
use crate::util::get_file_info;
use faststr::FastStr;
use futures_util::stream::{FuturesUnordered, StreamExt};
#[cfg(feature = "proxy")]
use reqwest::Proxy;
use reqwest::{Client, ClientBuilder};
use std::collections::HashMap;
const BLACKLIST_THRESHOLD: u32 = 3;

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
        }
    }

    /// 覆盖源 `id`（用于日志/统计辨识）。
    pub fn with_id(mut self, id: impl Into<FastStr>) -> Self {
        self.id = id.into();
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
            })
            .collect();
        lanes.sort_by(|a, b| b.candidate.probe_speed.total_cmp(&a.candidate.probe_speed));
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

    pub fn best_lane(&self) -> Option<FastStr> {
        self.select_lane(false)
            .or_else(|| self.select_lane(true))
            .map(|entry| entry.candidate.lane_id.clone())
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
        }
    }

    pub fn release_chunk(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            entry.active_chunks = entry.active_chunks.saturating_sub(1);
        }
    }

    pub fn record_failure(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            entry.consecutive_failures += 1;
            if entry.consecutive_failures >= BLACKLIST_THRESHOLD {
                entry.health = LaneHealth::Blacklisted;
            }
        }
    }

    pub fn record_success(&mut self, lane_id: impl AsRef<str>) {
        if let Some(entry) = self
            .lanes
            .iter_mut()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
        {
            entry.consecutive_failures = 0;
            entry.health = LaneHealth::Healthy;
        }
    }

    #[cfg_attr(not(any(test, feature = "multi-source")), allow(dead_code))]
    pub fn lane_health(&self, lane_id: impl AsRef<str>) -> Option<LaneHealth> {
        self.lanes
            .iter()
            .find(|entry| entry.candidate.lane_id.as_str() == lane_id.as_ref())
            .map(|entry| entry.health)
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
}

impl MultiRuntime {
    pub async fn from_config<F>(
        config: &MultiSourceConfig,
        client_builder: &F,
    ) -> Result<(u64, Self)>
    where
        F: Fn() -> ClientBuilder,
    {
        let expanded = expand_lanes(config, client_builder)?;
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
                    if support_ranges {
                        if let Some(expected) = range_file_size {
                            if expected != file_size {
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
                        runtime.probe_speed = 1.0;
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
                            continue;
                        }
                        if let Some(expected) = fallback_file_size {
                            if expected != file_size {
                                continue;
                            }
                        } else {
                            fallback_file_size = Some(file_size);
                        }
                        runtime.probe_speed = 1.0;
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
                _ => {}
            }
        }
        let (file_size, supports_ranges, runtimes, candidates) =
            if !range_candidates.is_empty() {
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
                return Err(DownloadError::NoAvailableSources);
            };

        let scheduler = LaneScheduler::from_candidates(
            candidates,
            config.lane_model,
            config.workers as usize,
            config.max_chunks_per_lane,
            config.max_chunks_per_source,
        );

        Ok((
            file_size,
            Self {
                scheduler,
                runtimes,
                next_runtime_index: HashMap::new(),
                supports_ranges,
            },
        ))
    }

    pub fn claim_request_builder(&mut self) -> Option<(FastStr, reqwest::RequestBuilder)> {
        let lane_id = self.scheduler.best_lane()?;
        self.scheduler.assign_chunk(lane_id.as_str());
        let runtime = self.next_runtime(&lane_id)?;
        Some((lane_id, runtime.client.get(runtime.url.as_str())))
    }

    pub fn primary_lane(&self) -> Option<(Client, FastStr)> {
        let lane_id = self.scheduler.best_lane()?;
        let runtime = self.runtimes.get(&lane_id)?.first()?;
        Some((runtime.client.clone(), runtime.url.clone()))
    }

    pub fn lane_runtime(&self, lane_id: &FastStr) -> Option<&LaneRuntime> {
        self.runtimes.get(lane_id)?.first()
    }

    pub fn best_lane_runtime(&self) -> Option<&LaneRuntime> {
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
                    eprintln!("[Lane] 代理 {} 解析失败: {error}, 跳过该 lane", proxy.url);
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
                    eprintln!("[Lane] 代理 {} 构建 Client 失败: {error}, 跳过", proxy.url);
                    continue;
                }
            };
            let lane_id = match config.lane_model {
                LaneModel::PerSource => source.id.clone(),
                LaneModel::PerSourceProxy => {
                    FastStr::from_string(format!("{}::{}", source.id, proxy.id))
                }
            };
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
