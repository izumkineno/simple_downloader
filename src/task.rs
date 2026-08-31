//! 任务模型：TaskId / TaskState / Task / TaskSnapshot
//! 独立于 Downloader 单任务，供 TaskQueue 调度。
//! 变更隔离：仅本文件 + queue.rs，不改 downloader 零破坏。

use std::path::PathBuf;

use faststr::FastStr;
use uuid::Uuid;

/// 全局唯一任务标识，v4 UUID，跨进程/重启不重用
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for TaskId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// 队列侧状态机（与 DownloadInfo 解耦，仅队列真相）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Active,
    Paused,
    Failed(String),
    Completed,
    Removed,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::Completed | Self::Removed)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Active => "Active",
            Self::Paused => "Paused",
            Self::Failed(_) => "Failed",
            Self::Completed => "Completed",
            Self::Removed => "Removed",
        }
    }
}

/// 队列持有的任务实体
#[derive(Debug, Clone)]
pub struct Task {
    pub id: TaskId,
    pub url: FastStr,
    pub output_path: PathBuf,
    pub state: TaskState,
    pub downloaded: u64,
    pub total_size: u64,
    pub workers: u64,
    pub created_at: std::time::Instant,
}

impl Task {
    pub fn new(url: impl Into<FastStr>, output_path: PathBuf, workers: u64) -> Self {
        Self {
            id: TaskId::new(),
            url: url.into(),
            output_path,
            state: TaskState::Queued,
            downloaded: 0,
            total_size: 0,
            workers,
            created_at: std::time::Instant::now(),
        }
    }
}

/// query 返回的快照（避免暴露内部可变）
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub url: FastStr,
    pub output_path: PathBuf,
    pub state: TaskState,
    pub downloaded: u64,
    pub total_size: u64,
    pub workers: u64,
}

impl From<&Task> for TaskSnapshot {
    fn from(t: &Task) -> Self {
        Self {
            id: t.id.clone(),
            url: t.url.clone(),
            output_path: t.output_path.clone(),
            state: t.state.clone(),
            downloaded: t.downloaded,
            total_size: t.total_size,
            workers: t.workers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_terminal() {
        assert!(!TaskState::Queued.is_terminal());
        assert!(!TaskState::Active.is_terminal());
        assert!(!TaskState::Paused.is_terminal());
        assert!(TaskState::Failed("x".into()).is_terminal());
        assert!(TaskState::Completed.is_terminal());
        assert!(TaskState::Removed.is_terminal());
    }

    #[test]
    fn task_id_unique() {
        let a = TaskId::new();
        let b = TaskId::new();
        assert_ne!(a, b);
    }
}
