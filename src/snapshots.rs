//! Profile configuration snapshots and rollback support.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

/// Profile files that snapshots, packs and plugin copies track.
pub const PROFILE_FILES: &[&str] = &[
    "package.json",
    "cordis.patch.yml",
    "cordis.yml",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "compatibility.json",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPlugin {
    pub name: String,
    pub version: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub entry_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub created_at: String,
    pub reason: String,
    pub files: Vec<String>,
    #[serde(default)]
    pub plugins: Vec<SnapshotPlugin>,
    #[serde(default)]
    pub last_good: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("实例的插件配置还没有初始化：{0}")]
    ProfileNotInitialized(PathBuf),
    #[error("找不到快照：{0}")]
    NotFound(String),
    #[error("快照 ID 不合法：{0}")]
    InvalidId(String),
    #[error("快照清单格式不对（{path}）：{source}")]
    InvalidManifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("快照路径不安全：{0}")]
    UnsafePath(String),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("写入快照清单失败：{0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    instance_root: PathBuf,
}

impl SnapshotStore {
    pub fn new(instance_root: impl Into<PathBuf>) -> Self {
        Self {
            instance_root: instance_root.into(),
        }
    }

    pub fn list(&self) -> Result<Vec<SnapshotInfo>, SnapshotError> {
        let directory = self.instance_root.join("snapshots");
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut snapshots = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if id.starts_with('.') || !is_valid_id(&id) {
                continue;
            }
            // 一个损坏的清单不应让整张快照列表报错：跳过它，其余照常可用。
            match self.read_manifest(&id) {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(error) => {
                    crate::applog::warn(&format!("快照 {id} 的清单无法读取，已跳过：{error}"))
                }
            }
        }
        snapshots.sort_by(|left, right| right.id.cmp(&left.id));
        Ok(snapshots)
    }

    pub fn get(&self, id: &str) -> Result<SnapshotInfo, SnapshotError> {
        self.read_manifest(id)
    }

    pub fn last_good(&self) -> Result<Option<SnapshotInfo>, SnapshotError> {
        Ok(self.list()?.into_iter().find(|snapshot| snapshot.last_good))
    }

    /// Mark one snapshot as the recovery target and clear the previous mark.
    pub fn mark_last_good(&self, snapshot_id: &str) -> Result<SnapshotInfo, SnapshotError> {
        let mut selected = self.read_manifest(snapshot_id)?;
        for mut snapshot in self.list()? {
            if snapshot.id == snapshot_id {
                selected = snapshot;
            } else if snapshot.last_good {
                snapshot.last_good = false;
                self.write_manifest(&snapshot)?;
            }
        }
        selected.last_good = true;
        self.write_manifest(&selected)?;
        Ok(selected)
    }

    /// Snapshot taken right before a launch: reuses the newest snapshot when the
    /// tracked profile files are unchanged, otherwise creates a new one.
    /// Returns `Ok(None)` when the profile does not exist yet (first launch).
    pub fn pre_launch(&self) -> Result<Option<SnapshotInfo>, SnapshotError> {
        if !self.profile_dir().is_dir() {
            return Ok(None);
        }
        if let Some(latest) = self.list()?.into_iter().next()
            && self.matches_profile(&latest)?
        {
            return Ok(Some(latest));
        }
        self.create("启动前自动快照").map(Some)
    }

    fn matches_profile(&self, snapshot: &SnapshotInfo) -> Result<bool, SnapshotError> {
        let profile = self.profile_dir();
        let mut current = Vec::new();
        for name in PROFILE_FILES {
            if profile.join(name).is_file() {
                current.push((*name).to_owned());
            }
        }
        let packages = profile.join("packages");
        if packages.is_dir() {
            for entry in fs::read_dir(packages)? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "tgz")
                {
                    current.push(format!("packages/{}", entry.file_name().to_string_lossy()));
                }
            }
        }
        current.sort();
        if current != snapshot.files {
            return Ok(false);
        }
        let snapshot_dir = self.instance_root.join("snapshots").join(&snapshot.id);
        for file in &current {
            let relative = validate_relative_path(file)?;
            if fs::read(profile.join(&relative))? != fs::read(snapshot_dir.join(&relative))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn create(&self, reason: impl Into<String>) -> Result<SnapshotInfo, SnapshotError> {
        let profile = self.profile_dir();
        if !profile.is_dir() {
            return Err(SnapshotError::ProfileNotInitialized(profile));
        }
        let snapshots_dir = self.instance_root.join("snapshots");
        fs::create_dir_all(&snapshots_dir)?;
        let timestamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string();
        let mut id = timestamp.clone();
        let mut suffix = 1;
        while snapshots_dir.join(&id).exists() {
            id = format!("{timestamp}-{suffix}");
            suffix += 1;
        }
        let staging = snapshots_dir.join(format!(".{id}.staging"));
        fs::create_dir(&staging)?;
        let result = (|| {
            let mut files = Vec::new();
            for name in PROFILE_FILES {
                let source = profile.join(name);
                if source.is_file() {
                    fs::copy(&source, staging.join(name))?;
                    files.push((*name).to_owned());
                }
            }
            let packages = profile.join("packages");
            if packages.is_dir() {
                let package_backup = staging.join("packages");
                for entry in fs::read_dir(packages)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file()
                        && entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "tgz")
                    {
                        fs::create_dir_all(&package_backup)?;
                        let name = entry.file_name();
                        fs::copy(entry.path(), package_backup.join(&name))?;
                        files.push(format!("packages/{}", name.to_string_lossy()));
                    }
                }
            }
            files.sort();
            let plugins = crate::plugins::list_plugins(&self.instance_root.join("home"))
                .unwrap_or_default()
                .into_iter()
                .map(|plugin| SnapshotPlugin {
                    name: plugin.name,
                    version: plugin.version,
                    enabled: plugin.enabled,
                    entry_id: plugin.entry_id,
                })
                .collect();
            let info = SnapshotInfo {
                id: id.clone(),
                created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                reason: reason.into(),
                files,
                plugins,
                last_good: false,
            };
            let manifest = serde_json::to_vec_pretty(&info)?;
            fs::write(staging.join("snapshot.json"), manifest)?;
            fs::rename(&staging, snapshots_dir.join(&id))?;
            Ok::<SnapshotInfo, SnapshotError>(info)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    /// Restore tracked profile files after first preserving the current state.
    /// Returns the ID of the automatic pre-rollback safety snapshot.
    pub fn restore(&self, snapshot_id: &str) -> Result<String, SnapshotError> {
        let selected = self.read_manifest(snapshot_id)?;
        let snapshot_dir = self.instance_root.join("snapshots").join(snapshot_id);
        for file in &selected.files {
            validate_relative_path(file)?;
        }
        for file in &selected.files {
            let relative = validate_relative_path(file)?;
            if !snapshot_dir.join(relative).is_file() {
                return Err(SnapshotError::UnsafePath(file.clone()));
            }
        }
        let safety_copy = self.create(format!("回滚前自动备份（目标 {}）", selected.id))?;
        let profile = self.profile_dir();
        fs::create_dir_all(&profile)?;

        let selected_files = selected
            .files
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        for name in PROFILE_FILES {
            if !selected_files.contains(*name) {
                let path = profile.join(name);
                if path.is_file() {
                    fs::remove_file(path)?;
                }
            }
        }
        let packages = profile.join("packages");
        if packages.is_dir() {
            for entry in fs::read_dir(&packages)? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "tgz")
                    && !selected_files
                        .contains(&format!("packages/{}", entry.file_name().to_string_lossy()))
                {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        for file in &selected.files {
            let relative = validate_relative_path(file)?;
            let source = snapshot_dir.join(&relative);
            let target = profile.join(relative);
            if !source.is_file() {
                return Err(SnapshotError::UnsafePath(file.clone()));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, target)?;
        }
        Ok(safety_copy.id)
    }

    fn read_manifest(&self, id: &str) -> Result<SnapshotInfo, SnapshotError> {
        if !is_valid_id(id) {
            return Err(SnapshotError::InvalidId(id.to_owned()));
        }
        let path = self
            .instance_root
            .join("snapshots")
            .join(id)
            .join("snapshot.json");
        let contents = fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SnapshotError::NotFound(id.to_owned())
            } else {
                SnapshotError::Io(error)
            }
        })?;
        let info: SnapshotInfo =
            serde_json::from_slice(&contents).map_err(|source| SnapshotError::InvalidManifest {
                path: path.clone(),
                source,
            })?;
        if info.id != id {
            return Err(SnapshotError::InvalidId(info.id));
        }
        Ok(info)
    }

    fn write_manifest(&self, info: &SnapshotInfo) -> Result<(), SnapshotError> {
        let directory = self.instance_root.join("snapshots").join(&info.id);
        let manifest = serde_json::to_vec_pretty(info)?;
        let mut temporary = NamedTempFile::new_in(&directory)?;
        std::io::Write::write_all(&mut temporary, &manifest)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(directory.join("snapshot.json"))
            .map(|_| ())
            .map_err(|error| SnapshotError::Io(error.error))
    }

    fn profile_dir(&self) -> PathBuf {
        self.instance_root.join("home/profiles/desktop")
    }
}

fn validate_relative_path(path: &str) -> Result<PathBuf, SnapshotError> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !(PROFILE_FILES.contains(&path)
            || path.starts_with("packages/") && path.ends_with(".tgz"))
    {
        return Err(SnapshotError::UnsafePath(path.to_owned()));
    }
    Ok(relative.to_path_buf())
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pre_launch_reuses_unchanged_snapshot_and_detects_changes() {
        let root = tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        assert!(store.pre_launch().unwrap().is_none());

        let profile = root.path().join("home/profiles/desktop");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("package.json"), b"one").unwrap();
        let first = store.pre_launch().unwrap().unwrap();
        assert_eq!(store.pre_launch().unwrap().unwrap().id, first.id);

        fs::write(profile.join("package.json"), b"two").unwrap();
        let second = store.pre_launch().unwrap().unwrap();
        assert_ne!(second.id, first.id);

        fs::write(profile.join("cordis.patch.yml"), b"patch").unwrap();
        assert_ne!(store.pre_launch().unwrap().unwrap().id, second.id);
    }

    #[test]
    fn snapshots_known_profile_files_and_package_archives() {
        let root = tempdir().unwrap();
        let profile = root.path().join("home/profiles/desktop");
        fs::create_dir_all(profile.join("packages")).unwrap();
        fs::write(profile.join("package.json"), b"before").unwrap();
        fs::write(profile.join("cordis.patch.yml"), b"patch").unwrap();
        fs::write(profile.join("packages/test.tgz"), b"archive").unwrap();
        fs::write(profile.join("sessions.txt"), b"excluded").unwrap();

        let store = SnapshotStore::new(root.path());
        let snapshot = store.create("pre-launch").unwrap();

        assert_eq!(
            snapshot.files,
            vec!["cordis.patch.yml", "package.json", "packages/test.tgz"]
        );
        assert!(
            root.path()
                .join("snapshots")
                .join(&snapshot.id)
                .join("package.json")
                .is_file()
        );
        assert!(
            !root
                .path()
                .join("snapshots")
                .join(&snapshot.id)
                .join("sessions.txt")
                .exists()
        );
        assert_eq!(store.list().unwrap(), vec![snapshot]);
    }

    #[test]
    fn restore_preserves_current_state_and_restores_snapshot() {
        let root = tempdir().unwrap();
        let profile = root.path().join("home/profiles/desktop");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("package.json"), b"old").unwrap();
        let store = SnapshotStore::new(root.path());
        let snapshot = store.create("last-good candidate").unwrap();
        fs::write(profile.join("package.json"), b"broken").unwrap();

        let backup_id = store.restore(&snapshot.id).unwrap();

        assert_eq!(fs::read(profile.join("package.json")).unwrap(), b"old");
        assert_eq!(
            fs::read(
                root.path()
                    .join("snapshots")
                    .join(backup_id)
                    .join("package.json")
            )
            .unwrap(),
            b"broken"
        );
    }

    #[test]
    fn rejects_unsafe_snapshot_paths() {
        assert!(matches!(
            validate_relative_path("../outside"),
            Err(SnapshotError::UnsafePath(_))
        ));
        assert!(validate_relative_path("package.json").is_ok());
    }

    /// 一个损坏的 snapshot.json 只跳过它自己，不应让整张列表报错。
    #[test]
    fn a_broken_manifest_does_not_break_the_list() {
        let root = tempdir().unwrap();
        let profile = root.path().join("home/profiles/desktop");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("package.json"), b"data").unwrap();
        let store = SnapshotStore::new(root.path());
        let good = store.create("pre-launch").unwrap();

        let broken = root.path().join("snapshots/20000101T000000000Z");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("snapshot.json"), b"{ not json").unwrap();

        assert_eq!(store.list().unwrap(), vec![good]);
    }
}
