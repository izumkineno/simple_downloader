use faststr::FastStr;
use log::{error, info, warn};
use rand::distr::weighted::WeightedIndex;
use rand::prelude::Distribution;
// 导入 Client 和 RequestBuilder
use reqwest::{Client, ClientBuilder, Proxy, RequestBuilder};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinSet;

const DEFAULT_WEIGHT: u32 = 100;
const FAILURE_WEIGHT_DECREASE: u32 = 33;
const TOO_SLOW_WEIGHT_DECREASE: u32 = 20;

#[derive(Debug, Clone)]
pub struct Resource {
    pub link: FastStr,
    pub proxy: Option<FastStr>,
    weight: Arc<AtomicU32>,
    is_available: Arc<AtomicBool>,
    /// 追踪资源被使用的次数
    times_used: Arc<AtomicU32>,
}

impl Resource {
    pub fn new(
        link: impl Into<FastStr>,
        proxy: impl Into<Option<FastStr>>,
        initial_weight: u32,
    ) -> Self {
        Self {
            link: link.into(),
            proxy: proxy.into(),
            weight: Arc::new(AtomicU32::new(initial_weight)),
            is_available: Arc::new(AtomicBool::new(true)),
            times_used: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn get_weight(&self) -> u32 {
        self.weight.load(Ordering::Acquire)
    }

    pub fn is_available(&self) -> bool {
        self.is_available.load(Ordering::Relaxed)
    }

    pub fn set_availability(&self, available: bool) {
        self.is_available.store(available, Ordering::Relaxed);
    }

    pub fn decrease_weight(&self, amount: u32) {
        // 使用 fetch_update 执行原子性的“读-改-写”操作，确保权重不会降到 1 以下。
        if self
            .weight
            // 使用 AcqRel 确保此原子操作的内存效果对其他线程可见
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |w| {
                if w > amount {
                    Some(w - amount)
                } else {
                    Some(1)
                }
            })
            .is_ok()
        {
            info!(
                "资源权重已降低: link={}, proxy={:?}, new_weight={}",
                self.link,
                self.proxy,
                self.get_weight()
            );
        }
    }
}

// --- Actor 消息定义 ---

#[derive(Debug)]
pub enum ActorMessage {
    GetResource {
        respond_to: oneshot::Sender<Option<Resource>>,
    },
    /// 新增消息：请求一个由 Actor 内部构建好的 Client 和对应的 Resource
    GetClientWithResource {
        respond_to: oneshot::Sender<Option<(Client, Resource)>>,
    },
    ReportFailure {
        resource: Resource,
    },
    ReportTooSlow {
        resource: Resource,
    },
    TestResources {
        respond_to: oneshot::Sender<()>,
        timeout: Duration,
    },
}

// --- Actor 定义 ---

#[derive(Debug)]
pub struct WeightedSourceActor<F>
where
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    pool: Arc<Mutex<Vec<Resource>>>,
    receiver: mpsc::Receiver<ActorMessage>,
    client_builder: Arc<F>,
}

impl<F> WeightedSourceActor<F>
where
    F: Fn() -> ClientBuilder + Send + Sync + 'static,
{
    pub fn new(
        links: Vec<impl Into<FastStr>>,
        proxies: Vec<impl Into<FastStr>>,
        client_builder: Arc<F>,
        add_no_proxy: bool,
        init_weight: Option<u32>,
    ) -> (Self, ResourceHandle) {
        let proxies: Vec<_> = if add_no_proxy {
            proxies
                .into_iter()
                .map(|v| Some(v.into()))
                .chain(std::iter::once(None))
                .collect()
        } else {
            proxies.into_iter().map(|v| Some(v.into())).collect()
        };

        let initial_pool: Vec<Resource> = if proxies.is_empty() {
            links
                .into_iter()
                .map(|l| Resource::new(l.into(), None, init_weight.unwrap_or(DEFAULT_WEIGHT)))
                .collect()
        } else {
            links
                .into_iter()
                .flat_map(|link| {
                    let link = link.into();
                    proxies.iter().map(move |proxy| {
                        Resource::new(
                            link.clone(),
                            proxy.clone(),
                            init_weight.unwrap_or(DEFAULT_WEIGHT),
                        )
                    })
                })
                .collect()
        };

        info!(
            "创建了包含 {} 个资源的 WeightedSourceActor",
            initial_pool.len()
        );

        let (sender, receiver) = mpsc::channel(100);

        let actor = Self {
            pool: Arc::new(Mutex::new(initial_pool)),
            receiver,
            client_builder,
        };
        let handle = ResourceHandle { sender };

        (actor, handle)
    }

    pub async fn run(mut self) {
        info!("WeightedSourceActor 事件循环已启动");
        while let Some(msg) = self.receiver.recv().await {
            self.handle_message(msg).await;
        }
        info!("WeightedSourceActor 事件循环已停止");
    }

    async fn handle_message(&mut self, msg: ActorMessage) {
        match msg {
            ActorMessage::GetResource { respond_to } => {
                let pool_guard = self.pool.lock().await;

                let available_resources: Vec<&Resource> =
                    pool_guard.iter().filter(|r| r.is_available()).collect();

                if available_resources.is_empty() {
                    warn!("资源池中没有可用的资源");
                    let _ = respond_to.send(None);
                    return;
                }

                let (unused, used): (Vec<&Resource>, Vec<&Resource>) = available_resources
                    .into_iter()
                    .partition(|r| r.times_used.load(Ordering::Relaxed) == 0);

                let selection_pool = if !unused.is_empty() {
                    info!("发现 {} 个未使用资源，将优先选择", unused.len());
                    unused
                } else {
                    info!("所有可用资源均已使用，从已使用池中选择");
                    used
                };

                let weights: Vec<u32> = selection_pool.iter().map(|r| r.get_weight()).collect();

                let dist = match WeightedIndex::new(&weights) {
                    Ok(d) => d,
                    Err(e) => {
                        error!("创建权重分布失败: {:?}，权重列表: {:?}", e, weights);
                        let _ = respond_to.send(None);
                        return;
                    }
                };

                let mut rng = rand::rng();
                let chosen_index = dist.sample(&mut rng);
                let chosen_resource = selection_pool[chosen_index].clone();

                chosen_resource.times_used.fetch_add(1, Ordering::Relaxed);

                info!(
                    "提供了一个资源: link={}, proxy={:?}, weight={}, times_used={}",
                    chosen_resource.link,
                    chosen_resource.proxy,
                    chosen_resource.get_weight(),
                    chosen_resource.times_used.load(Ordering::Relaxed)
                );
                let _ = respond_to.send(Some(chosen_resource));
            }
            ActorMessage::GetClientWithResource { respond_to } => {
                let pool_guard = self.pool.lock().await;

                // 1. 选择资源 (逻辑与 GetResource 完全相同)
                let available_resources: Vec<&Resource> =
                    pool_guard.iter().filter(|r| r.is_available()).collect();

                if available_resources.is_empty() {
                    warn!("资源池中没有可用的资源");
                    let _ = respond_to.send(None);
                    return;
                }

                let (unused, used): (Vec<&Resource>, Vec<&Resource>) = available_resources
                    .into_iter()
                    .partition(|r| r.times_used.load(Ordering::Relaxed) == 0);

                let selection_pool = if !unused.is_empty() { unused } else { used };

                let weights: Vec<u32> = selection_pool.iter().map(|r| r.get_weight()).collect();
                let dist = match WeightedIndex::new(&weights) {
                    Ok(d) => d,
                    Err(e) => {
                        error!("创建权重分布失败: {:?}，权重列表: {:?}", e, weights);
                        let _ = respond_to.send(None);
                        return;
                    }
                };
                let mut rng = rand::rng();
                let chosen_index = dist.sample(&mut rng);
                let resource = selection_pool[chosen_index].clone();
                resource.times_used.fetch_add(1, Ordering::Relaxed);

                info!(
                    "已选择资源用于构建 Client: link={}, proxy={:?}",
                    resource.link, resource.proxy
                );

                // 2. 使用 Actor 内部的 client_builder 构建 Client
                let mut builder = (self.client_builder)();
                if let Some(proxy_str) = &resource.proxy {
                    match Proxy::all(proxy_str.as_str()) {
                        Ok(proxy) => {
                            builder = builder.proxy(proxy);
                        }
                        Err(e) => {
                            error!("为 {:?} 创建代理失败: {}. 无法构建客户端。", proxy_str, e);
                            let _ = respond_to.send(None);
                            return;
                        }
                    }
                }

                let client = match builder.build() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("构建 reqwest::Client 失败: {}", e);
                        let _ = respond_to.send(None);
                        return;
                    }
                };

                // 3. 将构建好的 Client 和 Resource 发送回去 (Client 是 Send+Sync 的)
                let _ = respond_to.send(Some((client, resource)));
            }
            ActorMessage::ReportFailure { resource } => {
                resource.decrease_weight(FAILURE_WEIGHT_DECREASE);
            }
            ActorMessage::ReportTooSlow { resource } => {
                resource.decrease_weight(TOO_SLOW_WEIGHT_DECREASE);
            }
            ActorMessage::TestResources {
                respond_to,
                timeout,
            } => {
                let pool_clone = self.pool.clone();
                let client_builder = self.client_builder.clone();

                tokio::spawn(async move {
                    info!("开始并发资源测试...");
                    let resources_to_test = pool_clone.lock().await.clone();
                    let total_resources = resources_to_test.len();
                    let mut set = JoinSet::new();
                    info!("测试资源数量: {}", total_resources);

                    for resource in resources_to_test {
                        let client_builder = client_builder.clone();
                        set.spawn(async move {
                            let client_builder = if let Some(proxy_str) = &resource.proxy {
                                let proxy = match Proxy::all(proxy_str.as_str()) {
                                    Ok(p) => p,
                                    Err(_) => return (resource, false),
                                };
                                client_builder().proxy(proxy)
                            } else {
                                client_builder()
                            };

                            let client = match client_builder.timeout(timeout).build() {
                                Ok(c) => c,
                                Err(_) => return (resource, false),
                            };

                            let is_ok = client
                                .head(resource.link.as_str())
                                .send()
                                .await
                                .map_or(false, |r| r.status().is_success());

                            (resource, is_ok)
                        });
                    }

                    let mut available_count = 0;
                    while let Some(res) = set.join_next().await {
                        if let Ok((resource, is_ok)) = res {
                            resource.set_availability(is_ok);
                            if is_ok {
                                available_count += 1;
                            }
                            info!(
                                "测试结果: link={}, proxy={:?}, 可用性={}",
                                resource.link, resource.proxy, is_ok
                            );
                        }
                    }

                    info!(
                        "资源测试完成，可用资源 {} / {}",
                        available_count, total_resources
                    );
                    let _ = respond_to.send(());
                });
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResourceHandle {
    sender: mpsc::Sender<ActorMessage>,
}

impl ResourceHandle {
    pub async fn get_resource(&self) -> Option<Resource> {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::GetResource { respond_to: send };
        if self.sender.send(msg).await.is_err() {
            error!("WeightedSourceActor 已停止");
            return None;
        }
        recv.await.unwrap_or(None)
    }

    pub async fn report_failure(&self, resource: Resource) {
        let msg = ActorMessage::ReportFailure { resource };
        if self.sender.send(msg).await.is_err() {
            error!("WeightedSourceActor 已停止");
        }
    }

    pub async fn report_too_slow(&self, resource: Resource) {
        let msg = ActorMessage::ReportTooSlow { resource };
        if self.sender.send(msg).await.is_err() {
            error!("WeightedSourceActor 已停止");
        }
    }

    pub async fn test_resources(&self, timeout: Duration) {
        let (send, recv) = oneshot::channel();
        let msg = ActorMessage::TestResources {
            respond_to: send,
            timeout,
        };
        if self.sender.send(msg).await.is_err() {
            error!("WeightedSourceActor 已停止");
            return;
        }
        if recv.await.is_err() {
            error!("等待资源测试完成时发生错误");
        }
    }

    /// 从 Actor 获取一个经过加权选择的资源和为其配置好的 `reqwest::Client`，
    /// 然后在当前上下文中构建一个 `RequestBuilder`。
    ///
    /// 这个函数不再需要外部传入 ClientBuilder，因为它会使用 Actor 初始化时
    /// 配置的 `client_builder`。
    ///
    /// # 返回
    /// - `Some((RequestBuilder, Resource))`: 如果成功，返回构建好的请求构建器和其所使用的资源。
    /// - `None`: 如果资源池为空，或者在构建客户端/代理时出错，则返回 `None`。
    pub async fn get_request_builder(&self) -> Option<(RequestBuilder, Resource)> {
        // 步骤 1: 创建一个 channel 来接收 Actor 的响应
        let (send, recv) = oneshot::channel();

        // 步骤 2: 发送新消息，请求一个 Client 和 Resource
        let msg = ActorMessage::GetClientWithResource { respond_to: send };
        if self.sender.send(msg).await.is_err() {
            error!("WeightedSourceActor 已停止");
            return None;
        }

        // 步骤 3: 等待 Actor 的响应
        match recv.await {
            Ok(Some((client, resource))) => {
                // 步骤 4: 在当前任务中创建 RequestBuilder (这是安全的，因为没有跨越 .await 点)
                info!(
                    "已从 Actor 接收到 Client，正在构建 RequestBuilder for {}",
                    resource.link
                );
                let request_builder = client.get(resource.link.as_str());
                Some((request_builder, resource))
            }
            Ok(None) => {
                warn!("Actor 未能提供 Client 和 Resource (可能无可用资源或构建失败)");
                None
            }
            Err(_) => {
                error!("与 WeightedSourceActor 通信时发生错误");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;

    // 辅助函数，用于初始化测试日志。
    fn init_logger() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    async fn test_resource_actor() {
        init_logger();

        info!("开始 WeightedSourceActor 测试");
        let links = vec![
            "https://gh-proxy.com/",
            "https://hk.gh-proxy.com/",
            "https://cdn.gh-proxy.com/",
            "https://edgeone.gh-proxy.com/",
        ];
        let proxies = vec!["http://127.0.0.1:7897"];

        let (actor, handle) = WeightedSourceActor::new(
            links,
            proxies,
            Arc::new(|| reqwest::ClientBuilder::new()),
            true,
            None,
        );
        tokio::spawn(actor.run());

        info!("--- 第一次资源测试 ---");
        handle.test_resources(Duration::from_secs(5)).await;

        info!("\n--- 模拟 16 个下载任务 ---");
        for i in 1..=16 {
            if let Some(resource) = handle.get_resource().await {
                info!(
                    "下载任务 {} 获得了资源: link={}, proxy={:?}, weight={}",
                    i,
                    resource.link,
                    resource.proxy,
                    resource.get_weight()
                );
                if i == 3 {
                    info!("报告前权重: {}", resource.get_weight());
                    warn!("下载任务 {} 模拟失败，报告资源", i);
                    handle.report_failure(resource.clone()).await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    info!("报告后权重: {}", resource.get_weight());
                }
            } else {
                error!("下载任务 {} 无法获取资源，资源池可能已空", i);
            }
        }

        info!("\n--- 再次获取资源 ---");
        if let Some(resource) = handle.get_resource().await {
            info!(
                "再次获取的资源: link={}, proxy={:?}, weight={}",
                resource.link,
                resource.proxy,
                resource.get_weight()
            );
        }
    }

    #[tokio::test]
    async fn test_get_resource_success() {
        init_logger();
        let links = vec!["http://localhost:1234/success"];
        let (actor, handle) = WeightedSourceActor::new(
            links,
            Vec::<&str>::new(),
            Arc::new(|| reqwest::ClientBuilder::new()),
            true,
            None,
        );
        tokio::spawn(actor.run());

        let resource = handle.get_resource().await;
        assert!(resource.is_some(), "应该能成功获取一个资源");
        let resource = resource.unwrap();
        assert_eq!(resource.get_weight(), 100, "初始权重应为 100");
        assert_eq!(
            resource.times_used.load(Ordering::Relaxed),
            1,
            "资源应该被标记为已使用一次"
        );
    }

    #[tokio::test]
    async fn test_report_failure_decreases_weight() {
        init_logger();
        let links = vec!["http://localhost:1234/failure"];
        let (actor, handle) = WeightedSourceActor::new(
            links,
            Vec::<&str>::new(),
            Arc::new(|| reqwest::ClientBuilder::new()),
            true,
            None,
        );
        tokio::spawn(actor.run());

        let resource = handle.get_resource().await.unwrap();
        assert_eq!(resource.get_weight(), 100);

        handle.report_failure(resource.clone()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            resource.get_weight(),
            100 - FAILURE_WEIGHT_DECREASE,
            "失败后权重应减少 {}",
            FAILURE_WEIGHT_DECREASE
        );
    }

    #[tokio::test]
    async fn test_unused_resource_priority() {
        init_logger();
        let links = vec!["link1", "link2"];
        let (actor, handle) = WeightedSourceActor::new(
            links,
            Vec::<&str>::new(),
            Arc::new(|| reqwest::ClientBuilder::new()),
            true,
            None,
        );
        tokio::spawn(actor.run());

        let mut acquired_links = HashSet::new();

        let r1 = handle.get_resource().await.unwrap();
        acquired_links.insert(r1.link.clone());
        assert_eq!(r1.times_used.load(Ordering::Relaxed), 1);

        let r2 = handle.get_resource().await.unwrap();
        acquired_links.insert(r2.link.clone());
        assert_eq!(r2.times_used.load(Ordering::Relaxed), 1);

        assert_eq!(acquired_links.len(), 2, "应该获取了两个不同的资源");

        let r3 = handle.get_resource().await.unwrap();
        assert_eq!(
            r3.times_used.load(Ordering::Relaxed),
            2,
            "其中一个资源现在应该被使用了两次"
        );
    }

    #[tokio::test]
    async fn test_get_resource_unavailable() {
        init_logger();
        let links = vec!["link1"];
        let (actor, handle) = WeightedSourceActor::new(
            links,
            Vec::<&str>::new(),
            Arc::new(|| reqwest::ClientBuilder::new()),
            true,
            None,
        );
        let actor_pool = actor.pool.clone();
        tokio::spawn(actor.run());

        {
            let mut pool = actor_pool.lock().await;
            pool[0].set_availability(false);
        }

        let resource = handle.get_resource().await;
        assert!(resource.is_none(), "当所有资源都不可用时不应获取到任何资源");
    }

    #[tokio::test]
    async fn test_get_request_builder() {
        init_logger();
        let links = vec!["https://api.github.com/"];
        let proxies = vec!["http://127.0.0.1:7890"];

        // 定义基础的 ClientBuilder 工厂函数
        let client_builder_factory =
            Arc::new(|| ClientBuilder::new().user_agent("test-agent-from-actor"));

        let (actor, handle) = WeightedSourceActor::new(
            links,
            proxies,
            client_builder_factory, // 将工厂函数传递给 Actor
            false,
            None,
        );
        tokio::spawn(actor.run());

        // 调用新函数，不再需要传递闭包
        let result = handle.get_request_builder().await;

        assert!(result.is_some(), "应该成功获取 RequestBuilder 和 Resource");

        let (builder, resource) = result.unwrap();

        assert_eq!(resource.link, "https://api.github.com/");
        assert_eq!(resource.proxy, Some("http://127.0.0.1:7890".into()));

        let request = builder.build().unwrap();
        assert_eq!(request.method(), &reqwest::Method::GET);
        assert_eq!(request.url().as_str(), "https://api.github.com/");

        info!("成功构建了由 Actor 配置的请求: {:?}", request);
    }
}
