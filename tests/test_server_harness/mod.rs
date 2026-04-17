use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const READINESS_TIMEOUT: Duration = Duration::from_secs(4);
const READINESS_POLL: Duration = Duration::from_millis(50);

pub struct TestServerFile {
    _root: TempDir,
    pub name: String,
    pub bytes: Vec<u8>,
    directory: PathBuf,
}

impl TestServerFile {
    pub fn new(name: &str, bytes: Vec<u8>) -> io::Result<Self> {
        let root = TempDir::new()?;
        let directory = root.path().join("files");
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join(name), &bytes)?;
        Ok(Self {
            _root: root,
            name: name.to_owned(),
            bytes,
            directory,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

pub struct RunningTestServer {
    child: Child,
    port: u16,
}

impl RunningTestServer {
    pub async fn spawn(
        file_directory: &Path,
        total_max_speed: &str,
        per_thread_max_speed: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let port = free_port()?;
        let script = test_server_script();
        let python = std::env::var("SIMPLE_DOWNLOADER_TEST_PYTHON")
            .unwrap_or_else(|_| "python".to_owned());

        let child = Command::new(python)
            .arg(script)
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_HOST", "127.0.0.1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_PORT", port.to_string())
            .env(
                "SIMPLE_DOWNLOADER_TEST_SERVER_DIRECTORY",
                file_directory.as_os_str(),
            )
            .env(
                "SIMPLE_DOWNLOADER_TEST_SERVER_TOTAL_MAX_SPEED",
                total_max_speed,
            )
            .env(
                "SIMPLE_DOWNLOADER_TEST_SERVER_PER_THREAD_MAX_SPEED",
                per_thread_max_speed,
            )
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_CONSOLE", "1")
            .env("SIMPLE_DOWNLOADER_TEST_SERVER_DISABLE_STATUS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let mut server = Self { child, port };
        server.wait_until_ready().await?;
        Ok(server)
    }

    pub fn url_for(&self, file_name: &str) -> String {
        format!("{}/{}", self.base_url(), file_name)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub async fn stats(
        &self,
    ) -> Result<HashMap<String, u64>, Box<dyn std::error::Error + Send + Sync>> {
        let body = reqwest::Client::new()
            .get(format!("{}/__stats__", self.base_url()))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        let mut stats = HashMap::new();
        for line in body.lines() {
            if let Some((key, value)) = line.split_once('\t') {
                stats.insert(key.to_owned(), value.parse::<u64>()?);
            }
        }
        Ok(stats)
    }

    async fn wait_until_ready(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let url = format!("{}/__files__", self.base_url());
        let deadline = Instant::now() + READINESS_TIMEOUT;
        let mut last_error = String::new();

        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("test_server exited before readiness: {status}").into());
            }

            match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => last_error = format!("HTTP {}", response.status()),
                Err(error) => last_error = error.to_string(),
            }

            tokio::time::sleep(READINESS_POLL).await;
        }

        Err(format!(
            "test_server did not become ready at {url} within {:?}: {last_error}",
            READINESS_TIMEOUT
        )
        .into())
    }
}

impl Drop for RunningTestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn deterministic_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| ((index.wrapping_mul(31).wrapping_add(7)) % 251) as u8)
        .collect()
}

fn free_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn test_server_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_server")
        .join("server.py")
}
