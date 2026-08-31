//! 任务队列：多任务入队、FIFO 调度、生命周期控制、重命名
//! 按 RALPLAN feat/queue 极简 7 方法实现。
//! 约束：独立 TaskQueue 不改 Downloader 签名；两层并发独立不变式；重命名双触发无限递增；Mutex 不跨 await.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use faststr::FastStr;
use tokio::sync::{mpsc, Notify};
use tokio::task::{AbortHandle, JoinSet};

use crate::downloader::Downloader;
use crate::task::{Task, TaskId, TaskSnapshot, TaskState};

/// 队列错误
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("invalid state for {id}: expected {expected}, got {got}")]
    InvalidState {
        id: String,
        expected: &'static str,
        got: String,
    },
    #[error("io error: {0}")]
    Io(String),
}

type QueueResult<T> = Result<T, QueueError>;

/// 内部状态（仅 Mutex 守卫）
struct QueueState {
    queue: VecDeque<Task>,
    active: HashMap<TaskId, ActiveEntry>,
    all: HashMap<TaskId, Task>,
    max: u8,
    occupied: HashSet<String>,
}

struct ActiveEntry {
    task: Task,
    abort: AbortHandle,
}

#[allow(dead_code)]
#[derive(Debug)]
enum QueueCmd {
    Pump,
    Shutdown,
}

/// 任务队列（Send+Sync，仅进程内）
///
/// **WARNING**: 仅进程内并发，跨进程同路径需外部文件锁
pub struct TaskQueue {
    state: Arc<tokio::sync::Mutex<QueueState>>,
    tx: mpsc::Sender<QueueCmd>,
    notify: Arc<Notify>,
    _driver: tokio::task::JoinHandle<()>,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self::with_max_concurrent(3)
    }
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskQueue {
    pub fn with_max_concurrent(max_concurrent_tasks: u8) -> Self {
        let max = max_concurrent_tasks.clamp(1, 64);
        let state = Arc::new(tokio::sync::Mutex::new(QueueState {
            queue: VecDeque::new(),
            active: HashMap::new(),
            all: HashMap::new(),
            max,
            occupied: HashSet::new(),
        }));
        let (tx, rx) = mpsc::channel::<QueueCmd>(128);
        let notify = Arc::new(Notify::new());
        let driver_state = state.clone();
        let driver_notify = notify.clone();
        let driver_handle = tokio::spawn(driver_loop(rx, driver_state, driver_notify));
        Self {
            state,
            tx,
            notify,
            _driver: driver_handle,
        }
    }

    pub async fn enqueue(
        &self,
        url: impl Into<FastStr>,
        output: impl Into<PathBuf>,
    ) -> TaskId {
        self.enqueue_with_workers(url, output, default_workers()).await
    }

    pub async fn enqueue_with_workers(
        &self,
        url: impl Into<FastStr>,
        output: impl Into<PathBuf>,
        workers: u64,
    ) -> TaskId {
        let url: FastStr = url.into();
        let desired: PathBuf = output.into();
        let (id, final_path) = loop {
            let final_path = self.resolve_rename(&desired).await;
            let mut task = Task::new(url.clone(), final_path.clone(), workers.max(1));
            task.output_path = final_path.clone();
            let id = task.id.clone();
            let mut s = self.state.lock().await;
            let key = path_key(&final_path);
            let disk_exists = final_path.exists();
            let sidecar_exists = sidecar_path(&final_path)
                .as_ref()
                .map(|path| path.exists())
                .unwrap_or(false);
            if s.occupied.contains(&key) || disk_exists || sidecar_exists {
                ::tracing::debug!(candidate=%final_path.display(), "rename enqueue CAS conflict, retry");
                continue;
            }
            s.occupied.insert(key);
            s.queue.push_back(task.clone());
            s.all.insert(id.clone(), task);
            break (id, final_path);
        };
        let _ = self.tx.send(QueueCmd::Pump).await;
        self.notify.notify_waiters();
        ::tracing::info!(task_id=%id, path=%final_path.display(), "queue enqueue");
        id
    }

    pub async fn pause(&self, id: TaskId) -> QueueResult<()> {
        let mut s = self.state.lock().await;
        if let Some(entry) = s.active.remove(&id) {
            entry.abort.abort();
            let mut task = entry.task;
            task.state = TaskState::Paused;
            s.all.insert(id.clone(), task);
            drop(s);
            self.notify.notify_waiters();
            let _ = self.tx.send(QueueCmd::Pump).await;
            ::tracing::info!(task_id=%id, "queue pause active -> Paused");
            return Ok(());
        }
        if let Some(pos) = s.queue.iter().position(|t| t.id == id) {
            let mut task = s.queue.remove(pos).unwrap();
            task.state = TaskState::Paused;
            s.all.insert(id.clone(), task);
            drop(s);
            self.notify.notify_waiters();
            ::tracing::info!(task_id=%id, "queue pause queued -> Paused");
            return Ok(());
        }
        if let Some(t) = s.all.get(&id) {
            if t.state == TaskState::Paused {
                return Ok(());
            }
            return Err(QueueError::InvalidState {
                id: id.to_string(),
                expected: "Queued/Active",
                got: t.state.as_str().to_string(),
            });
        }
        Err(QueueError::NotFound(id.to_string()))
    }

    pub async fn resume(&self, id: TaskId) -> QueueResult<()> {
        let mut s = self.state.lock().await;
        let task_opt = s.all.get(&id).cloned();
        match task_opt {
            Some(mut t) if t.state == TaskState::Paused => {
                t.state = TaskState::Queued;
                s.all.insert(id.clone(), t.clone());
                s.queue.push_front(t);
                drop(s);
                let _ = self.tx.send(QueueCmd::Pump).await;
                self.notify.notify_waiters();
                ::tracing::info!(task_id=%id, "queue resume Paused -> Queued (front)");
                Ok(())
            }
            Some(t) => Err(QueueError::InvalidState {
                id: id.to_string(),
                expected: "Paused",
                got: t.state.as_str().to_string(),
            }),
            None => Err(QueueError::NotFound(id.to_string())),
        }
    }

    pub async fn cancel(&self, id: TaskId) -> QueueResult<()> {
        let mut s = self.state.lock().await;
        if let Some(entry) = s.active.remove(&id) {
            entry.abort.abort();
            let path = entry.task.output_path.clone();
            let mut task = entry.task;
            task.state = TaskState::Removed;
            s.all.insert(id.clone(), task);
            s.occupied.remove(&path_key(&path));
            drop(s);
            let _ = tokio::fs::remove_file(&path).await;
            if let Some(meta) = sidecar_path(&path) {
                let _ = tokio::fs::remove_file(&meta).await;
            }
            self.notify.notify_waiters();
            let _ = self.tx.send(QueueCmd::Pump).await;
            ::tracing::info!(task_id=%id, "queue cancel active -> Removed");
            return Ok(());
        }
        if let Some(pos) = s.queue.iter().position(|t| t.id == id) {
            let task = s.queue.remove(pos).unwrap();
            let path = task.output_path.clone();
            let mut removed = task;
            removed.state = TaskState::Removed;
            s.all.insert(id.clone(), removed);
            s.occupied.remove(&path_key(&path));
            drop(s);
            let _ = tokio::fs::remove_file(&path).await;
            if let Some(meta) = sidecar_path(&path) {
                let _ = tokio::fs::remove_file(&meta).await;
            }
            self.notify.notify_waiters();
            ::tracing::info!(task_id=%id, "queue cancel queued -> Removed");
            return Ok(());
        }
        if let Some(t) = s.all.get(&id).cloned() {
            if t.state == TaskState::Paused {
                let path = t.output_path.clone();
                let mut removed = t;
                removed.state = TaskState::Removed;
                s.all.insert(id.clone(), removed);
                s.occupied.remove(&path_key(&path));
                drop(s);
                let _ = tokio::fs::remove_file(&path).await;
                if let Some(meta) = sidecar_path(&path) {
                    let _ = tokio::fs::remove_file(&meta).await;
                }
                self.notify.notify_waiters();
                ::tracing::info!(task_id=%id, "queue cancel paused -> Removed");
                return Ok(());
            }
            if t.state == TaskState::Removed {
                return Ok(());
            }
            if matches!(t.state, TaskState::Completed | TaskState::Failed(_)) {
                let path = t.output_path.clone();
                let mut removed = t;
                removed.state = TaskState::Removed;
                s.all.insert(id.clone(), removed);
                s.occupied.remove(&path_key(&path));
                drop(s);
                let _ = tokio::fs::remove_file(&path).await;
                if let Some(meta) = sidecar_path(&path) {
                    let _ = tokio::fs::remove_file(&meta).await;
                }
                self.notify.notify_waiters();
                ::tracing::info!(task_id=%id, "queue cancel completed/failed -> Removed");
                return Ok(());
            }
            return Err(QueueError::InvalidState {
                id: id.to_string(),
                expected: "Queued/Active/Paused/Completed/Failed",
                got: t.state.as_str().to_string(),
            });
        }
        Err(QueueError::NotFound(id.to_string()))
    }
    pub async fn query(&self, id: TaskId) -> Option<TaskSnapshot> {
        let s = self.state.lock().await;
        s.all.get(&id).map(TaskSnapshot::from)
    }

    pub async fn wait_all(&self) {
        loop {
            {
                let s = self.state.lock().await;
                if s.queue.is_empty() && s.active.is_empty() {
                    return;
                }
            }
            self.notify.notified().await;
        }
    }

    pub async fn queued_len(&self) -> usize {
        self.state.lock().await.queue.len()
    }

    pub async fn active_count(&self) -> usize {
        self.state.lock().await.active.len()
    }

    pub async fn snapshot_all(&self) -> Vec<TaskSnapshot> {
        let s = self.state.lock().await;
        s.all.values().map(TaskSnapshot::from).collect()
    }

    async fn resolve_rename(&self, desired: &Path) -> PathBuf {
        let mut occupied_snapshot: HashSet<String> = {
            let s = self.state.lock().await;
            s.occupied.clone()
        };
        let mut n: u64 = 0;
        let mut candidate = desired.to_path_buf();
        loop {
            let key = path_key(&candidate);
            let in_occupied = occupied_snapshot.contains(&key);
            let disk_exists = tokio::fs::try_exists(&candidate).await.unwrap_or(false);
            let sidecar_opt = sidecar_path(&candidate);
            let sidecar_exists = if let Some(ref sc) = sidecar_opt {
                tokio::fs::try_exists(sc).await.unwrap_or(false)
            } else {
                false
            };
            if !in_occupied && !disk_exists && !sidecar_exists {
                let s = self.state.lock().await;
                let key = path_key(&candidate);
                let disk_sync = candidate.exists();
                let sidecar_sync = sidecar_opt
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false);
                if !s.occupied.contains(&key) && !disk_sync && !sidecar_sync {
                    return candidate;
                }
                occupied_snapshot = s.occupied.clone();
                ::tracing::debug!(candidate=%candidate.display(), "rename CAS conflict, retry");
            }
            n += 1;
            if n > 10000 {
                ::tracing::warn!(desired=%desired.display(), "rename loop >10000, break");
                break candidate;
            }
            candidate = with_suffix(desired, n);
        }
    }
}

fn path_key(p: &Path) -> String {
 let s = p.to_string_lossy().to_string();
 #[cfg(windows)]
 { s.to_lowercase() }
 #[cfg(not(windows))]
 { s }
}

fn sidecar_path(p: &Path) -> Option<PathBuf> {
 #[cfg(feature = "resume")]
 {
 Some(crate::resume::metadata_path_for(p))
 }
 #[cfg(not(feature = "resume"))]
 {
 let _ = p;
 None
 }
}
fn with_suffix(path: &Path, n: u64) -> PathBuf {
    let parent = path.parent();
    let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or("file");
    let (stem, ext) = if let Some(dot) = file_name.rfind('.') {
        if dot == 0 {
            (file_name, "")
        } else {
            (&file_name[..dot], &file_name[dot + 1..])
        }
    } else {
        (file_name, "")
    };
    let new_name = if ext.is_empty() {
        format!("{}({})", stem, n)
    } else {
        format!("{}({}).{}", stem, n, ext)
    };
    if let Some(p) = parent {
        if p.as_os_str().is_empty() {
            PathBuf::from(new_name)
        } else {
            p.join(new_name)
        }
    } else {
        PathBuf::from(new_name)
    }
}

fn default_workers() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(4)
}

async fn driver_loop(
    mut rx: mpsc::Receiver<QueueCmd>,
    state: Arc<tokio::sync::Mutex<QueueState>>,
    notify: Arc<Notify>,
) {
    let mut join_set: JoinSet<(TaskId, Result<(), crate::types::DownloadError>)> = JoinSet::new();
    loop {
        tokio::select! {
            cmd = rx.recv() => {
                match cmd {
                    Some(QueueCmd::Pump) => pump(&state, &mut join_set).await,
                    Some(QueueCmd::Shutdown) | None => {
                        ::tracing::info!("queue driver shutdown");
                        break;
                    }
                }
            }
            res = join_set.join_next(), if !join_set.is_empty() => {
                match res {
                    Some(Ok((id, result))) => {
                        on_complete(&state, id, result, &notify).await;
                        pump(&state, &mut join_set).await;
                    }
                    Some(Err(e)) if e.is_cancelled() => {
                        pump(&state, &mut join_set).await;
                    }
                    Some(Err(e)) => {
                        ::tracing::warn!(error=%e, "queue join error");
                        pump(&state, &mut join_set).await;
                    }
                    None => {}
                }
            }
        }
    }
}

async fn pump(
    state: &Arc<tokio::sync::Mutex<QueueState>>,
    join_set: &mut JoinSet<(TaskId, Result<(), crate::types::DownloadError>)>,
) {
    loop {
        let task_opt = {
            let mut s = state.lock().await;
            // formal invariant: active.len() <= max
            if s.active.len() >= s.max as usize {
                break;
            }
            let Some(mut task) = s.queue.pop_front() else {
                break;
            };
            task.state = TaskState::Active;
            s.all.insert(task.id.clone(), task.clone());
            Some(task)
        };
        let Some(task) = task_opt else {
            break;
        };
        let id = task.id.clone();
        let url = task.url.clone();
        let path = task.output_path.clone();
        let workers = task.workers;
        let id_clone = id.clone();
        let abort = join_set.spawn(async move {
            let fast_path = FastStr::from(path.to_string_lossy().to_string());
            let res = Downloader::builder(url, fast_path)
                .workers(workers)
                .build()
                .download()
                .await;
            (id_clone, res.map(|_| ()))
        });
        {
            let mut s = state.lock().await;
            // 若在 spawn 间隙被 pause/cancel，需检查并 abort
            if let Some(t) = s.all.get(&id) {
                if matches!(t.state, TaskState::Removed | TaskState::Paused) {
                    abort.abort();
                    continue;
                }
            }
            s.active.insert(id.clone(), ActiveEntry { task, abort });
        }
        ::tracing::debug!(task_id=%id, "queue pump spawn");
    }
}

async fn on_complete(
    state: &Arc<tokio::sync::Mutex<QueueState>>,
    id: TaskId,
    result: Result<(), crate::types::DownloadError>,
    notify: &Arc<Notify>,
) {
    let mut s = state.lock().await;
    s.active.remove(&id);
    let mut occupied_key: Option<String> = None;
    if let Some(task) = s.all.get_mut(&id) {
        match result {
            Ok(()) => {
                if task.state == TaskState::Active {
                    task.state = TaskState::Completed;
                    ::tracing::info!(task_id=%id, "queue task Completed");
                }
            }
            Err(e) => {
                if task.state != TaskState::Removed && task.state != TaskState::Paused {
                    task.state = TaskState::Failed(e.to_string());
                    ::tracing::warn!(task_id=%id, error=%e, "queue task Failed");
                }
            }
        }
        if matches!(task.state, TaskState::Completed | TaskState::Failed(_)) {
            occupied_key = Some(path_key(&task.output_path));
        }
    }
    if let Some(k) = occupied_key {
        s.occupied.remove(&k);
    }
    drop(s);
    notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn with_suffix_basic() {
        assert_eq!(with_suffix(Path::new("a.bin"), 1), PathBuf::from("a(1).bin"));
        assert_eq!(with_suffix(Path::new("a"), 1), PathBuf::from("a(1)"));
        assert_eq!(with_suffix(Path::new("a.tar.gz"), 1), PathBuf::from("a.tar(1).gz"));
        assert_eq!(with_suffix(Path::new(".gitignore"), 1), PathBuf::from(".gitignore(1)"));
        assert_eq!(with_suffix(Path::new("dir/a.bin"), 1), PathBuf::from("dir/a(1).bin"));
    }
    #[tokio::test]
    async fn concurrent_enqueue_assigns_unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let desired = dir.path().join("a.bin");
        let queue = TaskQueue::new();

        futures_util::future::join_all((0..20).map(|_| {
            queue.enqueue("http://127.0.0.1:1/unreachable", desired.clone())
        }))
        .await;
        let paths: std::collections::HashSet<_> = queue
            .snapshot_all()
            .await
            .into_iter()
            .map(|task| task.output_path)
            .collect();

        assert_eq!(paths.len(), 20);
    }
}
