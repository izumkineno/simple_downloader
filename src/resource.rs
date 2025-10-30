// resource.rs
use crate::types::{DownloadError, Result};
use faststr::FastStr;
use reqwest::{Client, ClientBuilder, Proxy, RequestBuilder};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub struct Resource<T: Fn() -> ClientBuilder> {
    /// 资源id
    id: u64,
    /// 客户端构建器
    client_builder: Arc<T>,
    /// 链接缓存池
    client: Option<Client>,
    /// 是否支持链接池
    support_pool: bool,
    /// 是否支持范围请求
    accept_ranges: bool,
    /// 链接地址
    url: FastStr,
    /// 代理地址
    proxy: Option<FastStr>,
}

impl<T: Fn() -> ClientBuilder> Resource<T> {
    /// 资源id计数器
    const ID: AtomicU64 = AtomicU64::new(0);

    pub fn new(
        client_builder: T,
        support_pool: bool,
        url: FastStr,
        proxy: Option<FastStr>,
    ) -> Self {
        let client_builder = Arc::new(client_builder);
        let id = Self::ID.fetch_add(1, Ordering::Relaxed);
        Self {
            id,
            client_builder,
            accept_ranges: false,
            client: None,
            support_pool,
            url,
            proxy,
        }
    }

    pub fn get_id(&self) -> u64 {
        self.id
    }

    pub fn get_client(&self) -> Result<Client> {
        if let Some(cached_client) = &self.client {
            Ok(cached_client.clone())
        } else {
            // 否则，创建一个新的 client
            let builder = (self.client_builder)();
            let client = if let Some(proxy) = &self.proxy {
                builder.proxy(Proxy::all(proxy.as_str())?)
            } else {
                builder
            }
            .build()?;
            Ok(client)
        }
    }

    pub fn get_request(&self) -> Result<RequestBuilder> {
        let client = self.get_client()?;
        Ok(client.get(self.url.as_str()))
    }

    /// 测试资源可用性，并检查是否支持连接池和范围请求
    /// 如果成功，会缓存 Client 并更新 support_pool 状态
    pub async fn test_availability(&mut self) -> Result<u64> {
        use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE};

        // 如果已经有缓存的 client，则直接使用
        let client = self.get_client()?;

        // 第一次尝试：发送一个普通的 GET 请求
        match client.get(self.url.as_str()).send().await {
            Ok(resp) => {
                if let Ok(resp) = resp.error_for_status() {
                    let headers = resp.headers();
                    if let Some(len_val) = headers.get(CONTENT_LENGTH) {
                        if let Ok(len_str) = len_val.to_str() {
                            if let Ok(content_length) = len_str.parse::<u64>() {
                                // 连接成功，意味着连接池是可用的
                                self.support_pool = true;

                                // 检查是否支持范围请求
                                let accept_ranges = headers
                                    .get(ACCEPT_RANGES)
                                    .map_or(false, |v| v.as_bytes().eq_ignore_ascii_case(b"bytes"));
                                self.accept_ranges = accept_ranges;

                                // 缓存 client 以供复用
                                self.client = Some(client);
                                return Ok(content_length);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // 如果第一次请求就失败了，直接返回错误
                return Err(e.into());
            }
        }

        // 第二次尝试：如果第一次请求没有获取到 Content-Length，则尝试范围请求
        let range_resp = client
            .get(self.url.as_str())
            .header("Range", "bytes=0-0")
            .send()
            .await?
            .error_for_status()?;

        let headers = range_resp.headers();
        if let Some(cr) = headers.get(CONTENT_RANGE) {
            if let Ok(crs) = cr.to_str() {
                if let Some(pos) = crs.rfind('/') {
                    let total_str = crs[pos + 1..].trim();
                    if total_str != "*" {
                        if let Ok(content_length) = total_str.parse::<u64>() {
                            // 连接成功，意味着连接池是可用的
                            self.support_pool = true;
                            self.accept_ranges = true;

                            // 缓存 client 以供复用
                            self.client = Some(client);
                            return Ok(content_length);
                        }
                    }
                }
            }
        }

        // 如果范围请求的响应头里也没有总长度，但有 Content-Length，也接受
        if let Some(len_val) = headers.get(CONTENT_LENGTH) {
            if let Ok(len_str) = len_val.to_str() {
                if let Ok(content_length) = len_str.parse::<u64>() {
                    // 连接成功，意味着连接池是可用的
                    self.support_pool = true;
                    // 此时 accept_ranges 可能仍为 false，但这取决于服务器的行为

                    // 缓存 client 以供复用
                    self.client = Some(client);
                    return Ok(content_length);
                }
            }
        }

        Err(DownloadError::MissingContentLength)
    }
}
