//! Persistent, per-instance launch history for diagnostics and recovery UI.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

const HISTORY_FILE: &str = "launch-history.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchOutcome {
    Running,
    Exited,
    Crashed,
    Stopped,
    ForcedStop,
}

impl LaunchOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "运行中",
            Self::Exited => "正常退出",
            Self::Crashed => "异常退出",
            Self::Stopped => "已停止",
            Self::ForcedStop => "强制结束",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchRecord {
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub process_id: u32,
    pub outcome: LaunchOutcome,
    pub exit_code: Option<u32>,
    pub runtime_seconds: Option<u64>,
    pub diagnostic_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspects: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchHistory {
    records: Vec<LaunchRecord>,
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("读取启动历史失败：{0}")]
    Read(#[from] std::io::Error),
    #[error("启动历史格式不对：{0}")]
    Parse(#[from] serde_json::Error),
    #[error("写入启动历史失败：{0}")]
    Write(std::io::Error),
}

#[derive(Debug, Clone)]
pub struct LaunchHistoryStore {
    path: PathBuf,
}

impl LaunchHistoryStore {
    pub fn new(instance_root: impl AsRef<Path>) -> Self {
        Self {
            path: instance_root.as_ref().join("marina").join(HISTORY_FILE),
        }
    }

    pub fn list(&self) -> Result<Vec<LaunchRecord>, HistoryError> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice::<LaunchHistory>(&bytes)?.records),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn record_started(
        &self,
        process_id: u32,
        diagnostic_path: Option<PathBuf>,
        snapshot_id: Option<String>,
    ) -> Result<(), HistoryError> {
        let mut history = self.load()?;
        history.records.push(LaunchRecord {
            started_at: Utc::now(),
            finished_at: None,
            process_id,
            outcome: LaunchOutcome::Running,
            exit_code: None,
            runtime_seconds: None,
            diagnostic_path,
            snapshot_id,
            suspects: Vec::new(),
        });
        self.save(&history)
    }

    pub fn latest(&self) -> Result<Option<LaunchRecord>, HistoryError> {
        Ok(self.load()?.records.pop())
    }

    pub fn set_suspects(&self, process_id: u32, suspects: Vec<String>) -> Result<(), HistoryError> {
        let mut history = self.load()?;
        if let Some(record) = history
            .records
            .iter_mut()
            .rev()
            .find(|record| record.process_id == process_id)
        {
            record.suspects = suspects;
        }
        self.save(&history)
    }

    pub fn record_finished(
        &self,
        process_id: u32,
        outcome: LaunchOutcome,
        exit_code: Option<u32>,
        runtime_seconds: Option<u64>,
    ) -> Result<(), HistoryError> {
        let mut history = self.load()?;
        if let Some(record) = history.records.iter_mut().rev().find(|record| {
            record.process_id == process_id && record.outcome == LaunchOutcome::Running
        }) {
            record.finished_at = Some(Utc::now());
            record.outcome = outcome;
            record.exit_code = exit_code;
            record.runtime_seconds = runtime_seconds;
        }
        self.save(&history)
    }

    /// Close records left in `Running` by a previous Marina process.
    ///
    /// Marina writes the outcome when an instance exits; if it was killed or
    /// the machine shut down first, the record would stay “运行中” forever.
    pub fn finalize_stale(&self) -> Result<usize, HistoryError> {
        let mut history = self.load()?;
        let mut closed = 0;
        for record in &mut history.records {
            if record.outcome == LaunchOutcome::Running {
                record.finished_at = Some(Utc::now());
                record.outcome = LaunchOutcome::ForcedStop;
                record.exit_code = None;
                closed += 1;
            }
        }
        if closed > 0 {
            self.save(&history)?;
        }
        Ok(closed)
    }

    fn load(&self) -> Result<LaunchHistory, HistoryError> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(LaunchHistory::default())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, history: &LaunchHistory) -> Result<(), HistoryError> {
        let directory = self
            .path
            .parent()
            .expect("history file has a parent directory");
        fs::create_dir_all(directory).map_err(HistoryError::Write)?;
        let mut temporary = NamedTempFile::new_in(directory).map_err(HistoryError::Write)?;
        serde_json::to_writer_pretty(&mut temporary, history)?;
        temporary.write_all(b"\n").map_err(HistoryError::Write)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(HistoryError::Write)?;
        temporary
            .persist(&self.path)
            .map_err(|error| HistoryError::Write(error.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_finishes_a_launch_atomically() {
        let root = tempfile::tempdir().unwrap();
        let store = LaunchHistoryStore::new(root.path());
        store
            .record_started(42, Some(PathBuf::from("diagnostic.txt")), None)
            .unwrap();
        store
            .record_finished(42, LaunchOutcome::Crashed, Some(1), Some(4))
            .unwrap();

        let records = store.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, LaunchOutcome::Crashed);
        assert_eq!(records[0].exit_code, Some(1));
        assert_eq!(records[0].runtime_seconds, Some(4));
        assert!(records[0].finished_at.is_some());
    }

    #[test]
    fn absent_history_is_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            LaunchHistoryStore::new(root.path())
                .list()
                .unwrap()
                .is_empty()
        );
    }
}
