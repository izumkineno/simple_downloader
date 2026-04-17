use reqwest::Client;
use std::error::Error;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub id: &'static str,
    pub total_max_speed: &'static str,
    pub per_thread_max_speed: &'static str,
}

pub struct TestServerCluster {
    _root: TempDir,
    servers: Vec<TestServer>,
}

pub struct TestServer {
    id: &'static str,
    base_url: String,
    child: Child,
}

impl TestServerCluster {
    pub async fn start(
        file_name: &str,
        file_bytes: &[u8],
        specs: &[ServerSpec],
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join(file_name), file_bytes)?;

        let mut servers = Vec::with_capacity(specs.len());
        for spec in specs {
            let server = TestServer::start(spec, root.path()).await?;
            servers.push(server);
        }

        Ok(Self {
            _root: root,
            servers,
        })
    }

    pub fn source_url(&self, index: usize, file_name: &str) -> String {
        format!(
            "{}/{}",
            self.servers[index].base_url.trim_end_matches('/'),
            file_name.trim_start_matches('/')
        )
    }

    pub fn missing_url(&self, index: usize) -> String {
        format!("{}/missing.bin", self.servers[index].base_url)
    }

    pub async fn get_count(
        &self,
        index: usize,
        path: &str,
    ) -> Result<usize, Box<dyn Error + Send + Sync>> {
        self.servers[index].request_count("GET", path).await
    }
}

impl TestServer {
    async fn start(
        spec: &ServerSpec,
        directory: &Path,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let port = free_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let script = test_server_script();

        let child = Command::new(python_bin())
            .arg(&script)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--directory")
            .arg(directory)
            .arg("--total-max-speed")
            .arg(spec.total_max_speed)
            .arg("--per-thread-max-speed")
            .arg(spec.per_thread_max_speed)
            .arg("--no-watch-config")
            .arg("--no-console")
            .arg("--no-speed-monitor")
            .arg("--quiet")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let mut server = Self {
            id: spec.id,
            base_url,
            child,
        };
        server.wait_ready().await?;
        Ok(server)
    }

    async fn wait_ready(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let client = Client::new();
        let url = format!("{}/__files__", self.base_url);
        let mut last_error = String::from("server did not respond");

        for _ in 0..50 {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("test_server {} exited early: {status}", self.id).into());
            }

            match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => last_error = format!("unexpected readiness status {}", response.status()),
                Err(error) => last_error = error.to_string(),
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Err(format!("test_server {} not ready: {last_error}", self.id).into())
    }

    async fn request_count(
        &self,
        method: &str,
        path: &str,
    ) -> Result<usize, Box<dyn Error + Send + Sync>> {
        let body = Client::new()
            .get(format!("{}/__stats__", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        let expected_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };

        for line in body.lines() {
            let mut fields = line.split('\t');
            let Some(found_method) = fields.next() else {
                continue;
            };
            let Some(found_path) = fields.next() else {
                continue;
            };
            let Some(count) = fields.next() else {
                continue;
            };
            if found_method == method && found_path == expected_path {
                return Ok(count.parse()?);
            }
        }

        Ok(0)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn python_bin() -> &'static str {
    "python"
}

fn test_server_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test_server/server.py")
}

fn free_port() -> Result<u16, Box<dyn Error + Send + Sync>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
