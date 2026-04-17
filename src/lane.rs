use crate::types::{DownloadError, Result};
use crate::util::get_file_info;
use faststr::FastStr;
use reqwest::{Client, ClientBuilder, Proxy};
use std::collections::HashMap;

const BLACKLIST_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneModel {
    PerSource,
    PerSourceProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneHealth {
    Healthy,
    Blacklisted,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub id: FastStr,
    pub url: FastStr,
}

impl ProxyConfig {
    pub fn new(url: impl Into<FastStr>) -> Self {
        let url = url.into();
        let id = FastStr::from_string(format!("proxy-{}", url));
        Self { id, url }
    }

    pub fn with_id(mut self, id: impl Into<FastStr>) -> Self {
        self.id = id.into();
        self
    }
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub id: FastStr,
    pub url: FastStr,
    pub proxies: Vec<ProxyConfig>,
}

impl SourceConfig {
    pub fn new(url: impl Into<FastStr>) -> Self {
        let url = url.into();
        let id = FastStr::from_string(format!("source-{}", url));
        Self {
            id,
            url,
            proxies: Vec::new(),
        }
    }

    pub fn with_id(mut self, id: impl Into<FastStr>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_proxies(mut self, proxies: Vec<ProxyConfig>) -> Self {
        self.proxies = proxies;
        self
    }
}

#[derive(Debug, Clone)]
pub struct MultiSourceConfig {
    pub output_path: FastStr,
    pub workers: u64,
    pub update_interval: f64,
    pub sources: Vec<SourceConfig>,
    pub lane_model: LaneModel,
    pub max_chunks_per_lane: usize,
    pub max_chunks_per_source: Option<usize>,
}

impl MultiSourceConfig {
    pub fn new(output_path: impl Into<FastStr>, workers: u64, update_interval: f64) -> Self {
        Self {
            output_path: output_path.into(),
            workers,
            update_interval,
            sources: Vec::new(),
            lane_model: LaneModel::PerSourceProxy,
            max_chunks_per_lane: 1,
            max_chunks_per_source: None,
        }
    }

    pub fn with_sources(mut self, sources: Vec<SourceConfig>) -> Self {
        self.sources = sources;
        self
    }

    pub fn with_lane_model(mut self, lane_model: LaneModel) -> Self {
        self.lane_model = lane_model;
        self
    }

    pub fn with_max_chunks_per_lane(mut self, max_chunks_per_lane: usize) -> Self {
        self.max_chunks_per_lane = max_chunks_per_lane.max(1);
        self
    }

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
        LaneModel::PerSourceProxy => candidates,
        LaneModel::PerSource => candidates
            .into_iter()
            .map(|mut candidate| {
                candidate.lane_id = candidate.source_id.clone();
                candidate.proxy_id = None;
                candidate
            })
            .collect(),
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
        if matches!(self.lane_model, LaneModel::PerSource) {
            if let Some(max_chunks_per_source) = self.max_chunks_per_source {
                if self.source_active_chunks(entry.candidate.source_id.as_str())
                    >= max_chunks_per_source
                {
                    return false;
                }
            }
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
        let mut detected_file_size = None;
        let mut runtimes: HashMap<FastStr, Vec<LaneRuntime>> = HashMap::new();
        let mut candidates = Vec::new();

        for mut runtime in expanded {
            match get_file_info(&runtime.client, runtime.url.as_str()).await {
                Ok((file_size, support_ranges)) if support_ranges => {
                    if let Some(expected) = detected_file_size {
                        if expected != file_size {
                            continue;
                        }
                    } else {
                        detected_file_size = Some(file_size);
                    }
                    runtime.probe_speed = 1.0;
                    candidates.push(LaneCandidate {
                        lane_id: runtime.lane_id.clone(),
                        source_id: runtime.source_id.clone(),
                        proxy_id: runtime.proxy_id.clone(),
                        probe_speed: runtime.probe_speed,
                    });
                    runtimes
                        .entry(runtime.lane_id.clone())
                        .or_default()
                        .push(runtime);
                }
                _ => {}
            }
        }

        let file_size = detected_file_size.ok_or(DownloadError::NoAvailableSources)?;
        if candidates.is_empty() {
            return Err(DownloadError::NoAvailableSources);
        }

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
        if source.proxies.is_empty() {
            let client = client_builder().build()?;
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

        for proxy in &source.proxies {
            let client = client_builder()
                .proxy(Proxy::all(proxy.url.as_str())?)
                .build()?;
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
