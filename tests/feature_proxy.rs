#![cfg(all(feature = "multi-source", feature = "proxy"))]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use reqwest::ClientBuilder;
use simple_downloader::{Downloader, LaneModel, MultiSourceConfig, ProxyConfig, SourceConfig};
use tempfile::NamedTempFile;

fn read_file(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).expect("read downloaded file")
}

#[derive(Debug, Default, Clone, Copy)]
struct ProxyStats {
    total_requests: usize,
    head_requests: usize,
    get_requests: usize,
    range_get_requests: usize,
    absolute_form_requests: usize,
}

struct TestProxyServer {
    base_url: String,
    stats: Arc<Mutex<ProxyStats>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TestProxyServer {
    fn spawn(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind proxy listener");
        listener
            .set_nonblocking(true)
            .expect("mark proxy listener nonblocking");

        let port = listener.local_addr().expect("proxy addr").port();
        let base_url = format!("http://127.0.0.1:{port}");
        let stats = Arc::new(Mutex::new(ProxyStats::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let body = Arc::new(body);

        let worker = {
            let stats = Arc::clone(&stats);
            let shutdown = Arc::clone(&shutdown);
            let body = Arc::clone(&body);

            thread::spawn(move || {
                while !shutdown.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let stats = Arc::clone(&stats);
                            let body = Arc::clone(&body);
                            thread::spawn(move || {
                                let _ = handle_proxy_connection(stream, body, stats);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            base_url,
            stats,
            shutdown,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn stats(&self) -> ProxyStats {
        *self.stats.lock().expect("proxy stats lock")
    }
}

impl Drop for TestProxyServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn handle_proxy_connection(
    mut stream: TcpStream,
    body: Arc<Vec<u8>>,
    stats: Arc<Mutex<ProxyStats>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = String::from_utf8_lossy(&request);
    let mut lines = request.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    let mut range = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range = parse_range_header(value.trim(), body.len());
            }
        }
    }

    {
        let mut stats = stats.lock().expect("proxy stats lock");
        stats.total_requests += 1;
        if target.starts_with("http://") || target.starts_with("https://") {
            stats.absolute_form_requests += 1;
        }
        match method {
            "HEAD" => stats.head_requests += 1,
            "GET" => {
                stats.get_requests += 1;
                if range.is_some() {
                    stats.range_get_requests += 1;
                }
            }
            _ => {}
        }
    }

    match method {
        "HEAD" => {
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes())?;
        }
        "GET" => {
            let (status_line, extra_headers, payload) = if let Some((start, end)) = range {
                let payload = body[start..=end].to_vec();
                (
                    "HTTP/1.1 206 Partial Content\r\n".to_owned(),
                    format!("Content-Range: bytes {}-{}/{}\r\n", start, end, body.len()),
                    payload,
                )
            } else {
                (
                    "HTTP/1.1 200 OK\r\n".to_owned(),
                    String::new(),
                    body.as_ref().clone(),
                )
            };

            let response = format!(
                "{status_line}Content-Length: {}\r\nAccept-Ranges: bytes\r\n{extra_headers}Connection: close\r\n\r\n",
                payload.len()
            );
            stream.write_all(response.as_bytes())?;
            stream.write_all(&payload)?;
        }
        _ => {
            stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")?;
        }
    }

    stream.flush()?;
    Ok(())
}

fn parse_range_header(header: &str, body_len: usize) -> Option<(usize, usize)> {
    let raw = header.strip_prefix("bytes=")?;
    let (start, end) = raw.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        body_len.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?
    };
    if start > end || end >= body_len {
        return None;
    }
    Some((start, end))
}

#[tokio::test]
async fn proxy_feature_downloads_through_configured_proxy_lane() {
    let proxy = TestProxyServer::spawn((0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect());
    let output = NamedTempFile::new().expect("temp output file");
    let path = output.path().to_path_buf();

    let config = MultiSourceConfig::new(path.to_string_lossy().to_string(), 1, 0.05)
        .with_lane_model(LaneModel::PerSourceProxy)
        .with_sources(vec![
            SourceConfig::new("http://does-not-resolve.invalid/proxied.bin")
                .with_id("proxied-source")
                .with_proxies(vec![ProxyConfig::new(proxy.base_url().to_owned())]),
        ]);

    let downloader = Downloader::new_multi(config, ClientBuilder::new);
    downloader
        .download()
        .await
        .expect("proxied download succeeds");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let stats = proxy.stats();
    let expected = (0..(2 * 1024 * 1024))
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();

    assert_eq!(read_file(&path), expected);
    assert!(
        stats.head_requests > 0,
        "expected proxied HEAD probe; stats={stats:?}"
    );
    assert!(
        stats.get_requests > 0,
        "expected proxied GET download; stats={stats:?}"
    );
    assert!(
        stats.range_get_requests > 0,
        "expected ranged GET requests through proxy; stats={stats:?}"
    );
    assert!(
        stats.absolute_form_requests > 0,
        "expected HTTP proxy absolute-form requests; stats={stats:?}"
    );
}
