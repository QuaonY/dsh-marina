//! Instance deletion, directory accounting, trash retention, and uninstall support.

use crate::instances::{InstanceConfig, InstancePaths, InstanceRepository};
use crate::packs;
use crate::runtimes::RuntimeRegistry;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

const TRASH_RETENTION_DAYS: i64 = 30;
const TRASH_METADATA: &str = "trash.json";
/// Snapshot of the instance config kept alongside `home/` so a restore can
/// rebuild the original name, badge and runtime binding.
const TRASH_CONFIG: &str = "instance.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Export a Marina Pack first, then recycle the instance directory.
    Export,
    /// Move `home/` to `trash/` for 30 days and recycle the rest.
    KeepData,
    /// Recycle the whole instance directory.
    Direct,
}

impl DeleteMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "export" => Some(Self::Export),
            "keep" => Some(Self::KeepData),
            "direct" | "trash" => Some(Self::Direct),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub exported_to: Option<PathBuf>,
    pub kept_at: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrashMetadata {
    instance_id: String,
    /// Display name captured at deletion; older entries omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    deleted_at: String,
    expires_at: String,
}

/// One restorable entry in Marina's own 30-day trash directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// Directory name under `trash/`; the stable key for restore/purge.
    pub key: String,
    pub instance_id: String,
    pub name: String,
    pub color: String,
    pub deleted_at: String,
    pub expires_at: String,
    pub size_bytes: u64,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("实例还在运行：{0}")]
    Running(String),
    #[error("不支持的删除方式")]
    InvalidMode,
    #[error("导出整合包失败：{0}")]
    Pack(#[from] packs::PackError),
    #[error("移到回收站失败：{0}")]
    Recycle(#[from] trash::Error),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("找不到卸载程序：{0}")]
    MissingUninstaller(PathBuf),
    #[error("恢复实例失败：{0}")]
    Repository(#[from] crate::instances::RepositoryError),
    #[error("找不到回收站条目：{0}")]
    TrashNotFound(String),
    #[error("无效的回收站条目")]
    InvalidTrashKey,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UninstallStats {
    pub instances: usize,
    pub runtimes: usize,
    pub bytes: u64,
}

pub fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            total = total.saturating_add(directory_size(&entry.path()));
        } else if file_type.is_file() {
            total =
                total.saturating_add(entry.metadata().map(|metadata| metadata.len()).unwrap_or(0));
        }
    }
    total
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 时 {} 分", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub fn desktop_directory() -> PathBuf {
    let profile = crate::platform::home_dir();
    let desktop = profile.join("Desktop");
    if desktop.is_dir() {
        return desktop;
    }
    let one_drive = profile.join("OneDrive").join("Desktop");
    if one_drive.is_dir() {
        return one_drive;
    }
    desktop
}

pub fn backup_directory(data_root: &Path) -> PathBuf {
    let desktop = desktop_directory();
    let candidate = desktop.join(format!(
        "DSH-Marina-Backup-{}",
        Utc::now().format("%Y%m%d-%H%M%S")
    ));
    if fs::create_dir_all(&candidate).is_ok() {
        candidate
    } else {
        let fallback = data_root.join("exports");
        let _ = fs::create_dir_all(&fallback);
        fallback
    }
}

/// Delete a stopped instance using one of the three documented strategies.
pub fn delete_instance(
    repository: &InstanceRepository,
    paths: &InstancePaths,
    mode: DeleteMode,
) -> Result<DeleteOutcome, StorageError> {
    match mode {
        DeleteMode::Export => {
            let directory = backup_directory(repository.data_root());
            let id = paths.root.file_name().unwrap_or_default().to_string_lossy();
            let name = repository
                .load(&id)
                .map(|config| config.name)
                .unwrap_or_else(|_| id.clone().into_owned());
            let destination =
                directory.join(format!("{}.marina-pack.zip", sanitize_file_name(&name)));
            packs::export_instance(repository, &id, &destination)?;
            remove_instance_directory(paths)?;
            Ok(DeleteOutcome {
                exported_to: Some(destination),
                kept_at: None,
            })
        }
        DeleteMode::KeepData => {
            let trash_dir = move_to_trash(repository, paths)?;
            remove_instance_directory(paths)?;
            Ok(DeleteOutcome {
                exported_to: None,
                kept_at: Some(trash_dir),
            })
        }
        DeleteMode::Direct => {
            remove_instance_directory(paths)?;
            Ok(DeleteOutcome {
                exported_to: None,
                kept_at: None,
            })
        }
    }
}

/// Move `home/` into `trash/<id>-<timestamp>/` together with a config snapshot
/// and retention metadata.  The rest of the instance directory is left for the
/// caller to dispose of, so this step is testable without the recycle bin.
fn move_to_trash(
    repository: &InstanceRepository,
    paths: &InstancePaths,
) -> Result<PathBuf, StorageError> {
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S");
    let id = paths.root.file_name().unwrap_or_default().to_string_lossy();
    let trash_dir = repository
        .data_root()
        .join("trash")
        .join(format!("{id}-{timestamp}"));
    fs::create_dir_all(&trash_dir)?;
    let config = repository.load(&id).ok();
    if let Some(config) = &config
        && let Ok(text) = toml::to_string_pretty(config)
    {
        let _ = fs::write(trash_dir.join(TRASH_CONFIG), text);
    }
    let home_target = trash_dir.join("home");
    if paths.home.is_dir() {
        fs::rename(&paths.home, &home_target)?;
    }
    let metadata = TrashMetadata {
        instance_id: id.to_string(),
        name: config.as_ref().map(|config| config.name.clone()),
        deleted_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + Duration::days(TRASH_RETENTION_DAYS)).to_rfc3339(),
    };
    let text = serde_json::to_vec_pretty(&metadata)
        .map_err(|error| StorageError::Io(std::io::Error::other(error.to_string())))?;
    fs::write(trash_dir.join(TRASH_METADATA), text)?;
    Ok(trash_dir)
}

fn remove_instance_directory(paths: &InstancePaths) -> Result<(), StorageError> {
    if !paths.root.exists() {
        return Ok(());
    }
    match trash::delete(&paths.root) {
        Ok(()) => Ok(()),
        Err(_) => {
            // The recycle bin can be unavailable on removable or network drives.
            // Removing a stopped instance directory is still the requested action.
            fs::remove_dir_all(&paths.root)?;
            Ok(())
        }
    }
}

/// Remove trash entries whose 30-day retention has expired.
pub fn cleanup_trash(data_root: &Path) -> Result<usize, StorageError> {
    let root = data_root.join("trash");
    if !root.is_dir() {
        return Ok(0);
    }
    let now = Utc::now();
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let metadata_path = entry.path().join(TRASH_METADATA);
        let expired = fs::read(&metadata_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TrashMetadata>(&bytes).ok())
            .and_then(|metadata| DateTime::parse_from_rfc3339(&metadata.expires_at).ok())
            .is_some_and(|expires| expires.with_timezone(&Utc) <= now);
        if expired {
            let _ = fs::remove_dir_all(entry.path());
            removed += 1;
        }
    }
    Ok(removed)
}

/// Reject keys that could escape the `trash/` directory.
fn validate_trash_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty() || key.contains("..") || key.contains(['/', '\\']) {
        return Err(StorageError::InvalidTrashKey);
    }
    Ok(())
}

/// List the restorable entries in Marina's own trash, newest first.  Entries
/// without readable metadata are skipped.  Walks `home/` for sizes, so call it
/// off the UI thread.
pub fn list_trash(data_root: &Path) -> Result<Vec<TrashEntry>, StorageError> {
    let root = data_root.join("trash");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let Some(metadata) = fs::read(dir.join(TRASH_METADATA))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TrashMetadata>(&bytes).ok())
        else {
            continue;
        };
        let config = fs::read_to_string(dir.join(TRASH_CONFIG))
            .ok()
            .and_then(|text| toml::from_str::<InstanceConfig>(&text).ok());
        let name = config
            .as_ref()
            .map(|config| config.name.clone())
            .or(metadata.name.clone())
            .unwrap_or_else(|| metadata.instance_id.clone());
        let color = config
            .as_ref()
            .map(|config| config.color.clone())
            .unwrap_or_else(|| "#0d6efd".to_owned());
        entries.push(TrashEntry {
            key: entry.file_name().to_string_lossy().into_owned(),
            instance_id: metadata.instance_id,
            name,
            color,
            deleted_at: metadata.deleted_at,
            expires_at: metadata.expires_at,
            size_bytes: directory_size(&dir.join("home")),
        });
    }
    entries.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    Ok(entries)
}

/// Recreate an instance from a kept trash entry: restore its config (or a blank
/// fallback for legacy entries) and move `home/` back.  The original id is
/// reused when free, otherwise a fresh one is minted.  The trash entry is
/// removed on success.  Only `home/` survives deletion — login state and other
/// directories went to the system recycle bin and are not recovered here.
pub fn restore_trash(
    repository: &InstanceRepository,
    key: &str,
) -> Result<InstanceConfig, StorageError> {
    validate_trash_key(key)?;
    let trash_dir = repository.data_root().join("trash").join(key);
    if !trash_dir.is_dir() {
        return Err(StorageError::TrashNotFound(key.to_owned()));
    }
    let saved = fs::read_to_string(trash_dir.join(TRASH_CONFIG))
        .ok()
        .and_then(|text| toml::from_str::<InstanceConfig>(&text).ok());
    let metadata = fs::read(trash_dir.join(TRASH_METADATA))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<TrashMetadata>(&bytes).ok());
    let mut config = match saved {
        Some(config) => config,
        None => {
            let name = metadata
                .as_ref()
                .and_then(|metadata| metadata.name.clone())
                .or_else(|| {
                    metadata
                        .as_ref()
                        .map(|metadata| metadata.instance_id.clone())
                })
                .unwrap_or_else(|| "恢复的实例".to_owned());
            InstanceConfig::new_blank(name)
        }
    };
    let id_taken = repository
        .paths(&config.id)
        .map(|paths| paths.root.exists())
        .unwrap_or(false);
    if id_taken {
        config.id = InstanceConfig::new_blank(config.name.clone()).id;
    }
    config.validate()?;
    let created = repository.create(config)?;
    let paths = repository.paths(&created.id)?;
    let home_source = trash_dir.join("home");
    if home_source.is_dir() {
        fs::remove_dir_all(&paths.home)?;
        if fs::rename(&home_source, &paths.home).is_err() {
            copy_directory(&home_source, &paths.home)?;
            fs::remove_dir_all(&home_source)?;
        }
    }
    let _ = fs::remove_dir_all(&trash_dir);
    Ok(created)
}

/// Permanently remove one trash entry (the user confirmed this is irreversible).
pub fn purge_trash_entry(data_root: &Path, key: &str) -> Result<(), StorageError> {
    validate_trash_key(key)?;
    let dir = data_root.join("trash").join(key);
    if !dir.is_dir() {
        return Err(StorageError::TrashNotFound(key.to_owned()));
    }
    fs::remove_dir_all(&dir)?;
    Ok(())
}

pub fn uninstall_stats(
    repository: &InstanceRepository,
    registry: &RuntimeRegistry,
) -> Result<UninstallStats, StorageError> {
    let instances = repository
        .list()
        .map_err(|error| StorageError::Io(std::io::Error::other(error.to_string())))?;
    let mut bytes = 0u64;
    for instance in &instances {
        if let Ok(paths) = repository.paths(&instance.id) {
            bytes = bytes.saturating_add(directory_size(&paths.root));
        }
    }
    let runtimes = registry
        .scan()
        .map_err(|error| StorageError::Io(std::io::Error::other(error.to_string())))?;
    for runtime in &runtimes {
        if let Some(parent) = runtime.executable.parent() {
            bytes = bytes.saturating_add(directory_size(parent));
        }
    }
    Ok(UninstallStats {
        instances: instances.len(),
        runtimes: runtimes.len(),
        bytes,
    })
}

pub fn move_data_to_desktop(data_root: &Path) -> Result<PathBuf, StorageError> {
    let desktop = desktop_directory();
    let destination = desktop.join(format!(
        "DSH-Marina-data-{}",
        Utc::now().format("%Y%m%d-%H%M%S")
    ));
    if fs::rename(data_root, &destination).is_ok() {
        return Ok(destination);
    }
    copy_directory(data_root, &destination)?;
    fs::remove_dir_all(data_root)?;
    Ok(destination)
}

fn copy_directory(source: &Path, target: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_directory(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

/// Run the NSIS uninstaller once Marina has exited.
///
/// The uninstaller removes the program files, shortcuts and the entry under
/// 「应用和功能」; waiting here keeps it from tripping over the running exe.
#[cfg(windows)]
pub fn spawn_installer_uninstall(uninstaller: &Path) -> Result<PathBuf, StorageError> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let script = std::env::temp_dir().join(format!(
        "dsh-marina-uninstall-{}-{stamp}.cmd",
        std::process::id()
    ));
    let contents = format!(
        "@echo off\r\nping 127.0.0.1 -n 3 >nul\r\nstart \"\" /wait \"{}\" /S\r\ndel /f /q \"%~f0\"\r\n",
        batch_escape(uninstaller)
    );
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&script)?;
        file.write_all(contents.as_bytes())?;
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd.exe")
            .arg("/C")
            .arg(&script)
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()?;
    }

    Ok(script)
}

/// Launch a detached cmd script that removes Marina after the process exits.
/// `remove_data` is false when the data directory has already been moved.
#[cfg(windows)]
pub fn spawn_uninstall_script(
    executable: &Path,
    data_root: &Path,
    remove_data: bool,
) -> Result<PathBuf, StorageError> {
    // A unique, unpredictable name keeps a stale or planted file in %TEMP%
    // from being executed, and `create_new` refuses to reuse an existing one.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let script = std::env::temp_dir().join(format!(
        "dsh-marina-uninstall-{}-{stamp}.cmd",
        std::process::id()
    ));
    let mut contents = String::from("@echo off\r\nping 127.0.0.1 -n 3 >nul\r\n");
    if remove_data {
        contents.push_str(&format!("rmdir /s /q \"{}\"\r\n", batch_escape(data_root)));
    }
    contents.push_str(&format!("del /f /q \"{}\"\r\n", batch_escape(executable)));
    contents.push_str("del /f /q \"%~f0\"\r\n");
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&script)?;
        file.write_all(contents.as_bytes())?;
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd.exe")
            .arg("/C")
            .arg(&script)
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("cmd.exe")
            .arg("/C")
            .arg(&script)
            .spawn()?;
    }
    Ok(script)
}

/// Quote a path for a batch file.  `%` would be expanded by cmd and a trailing
/// separator would escape the closing quote, so both are handled here.
#[cfg(windows)]
fn batch_escape(path: &Path) -> String {
    let text = path.display().to_string().replace('%', "%%");
    let text = text.trim_end_matches(['\\', '/']);
    if text.is_empty() {
        return String::from("\\");
    }
    text.chars()
        .filter(|c| *c != '"' && !c.is_control())
        .collect()
}

pub fn sanitize_file_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|character| match character {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect::<String>()
        .trim()
        .to_owned();
    if cleaned.is_empty() {
        "instance".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn directory_size_skips_missing_paths_and_sums_files() {
        let root = tempdir().unwrap();
        assert_eq!(directory_size(&root.path().join("missing")), 0);
        fs::write(root.path().join("a.bin"), vec![0u8; 10]).unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/b.bin"), vec![0u8; 5]).unwrap();
        assert_eq!(directory_size(root.path()), 15);
    }

    #[test]
    fn formats_bytes_and_durations_for_the_ui() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_duration(42), "42 秒");
        assert_eq!(format_duration(125), "2 分 5 秒");
        assert_eq!(format_duration(3661), "1 时 1 分");
    }

    #[test]
    fn sanitizes_instance_names_for_file_destinations() {
        assert_eq!(sanitize_file_name("工作区 / 测试"), "工作区 - 测试");
        assert_eq!(sanitize_file_name(""), "instance");
    }

    #[test]
    fn cleanup_trash_removes_expired_entries_only() {
        let root = tempdir().unwrap();
        let trash_root = root.path().join("trash");
        let expired = trash_root.join("old");
        let current = trash_root.join("new");
        fs::create_dir_all(&expired).unwrap();
        fs::create_dir_all(&current).unwrap();
        let old_metadata = TrashMetadata {
            instance_id: "old".into(),
            name: None,
            deleted_at: "2020-01-01T00:00:00Z".into(),
            expires_at: "2020-01-31T00:00:00Z".into(),
        };
        fs::write(
            expired.join(TRASH_METADATA),
            serde_json::to_vec(&old_metadata).unwrap(),
        )
        .unwrap();
        let new_metadata = TrashMetadata {
            instance_id: "new".into(),
            name: None,
            deleted_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + Duration::days(30)).to_rfc3339(),
        };
        fs::write(
            current.join(TRASH_METADATA),
            serde_json::to_vec(&new_metadata).unwrap(),
        )
        .unwrap();

        assert_eq!(cleanup_trash(root.path()).unwrap(), 1);
        assert!(!expired.exists());
        assert!(current.exists());
    }

    /// Move an instance's home into the trash and recycle the rest, without
    /// touching the real system recycle bin; returns the trash key.
    fn keep_instance(repository: &InstanceRepository, name: &str) -> (InstanceConfig, String) {
        let config = repository.create_blank(name).unwrap();
        let paths = repository.paths(&config.id).unwrap();
        fs::write(paths.home.join("marker.txt"), b"data").unwrap();
        let trash_dir = move_to_trash(repository, &paths).unwrap();
        fs::remove_dir_all(&paths.root).unwrap();
        let key = trash_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        (config, key)
    }

    #[test]
    fn keep_data_saves_a_config_snapshot_and_lists_the_entry() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let mut config = repository.create_blank("工作区").unwrap();
        config.color = "#1f9d6b".into();
        repository.save(&config).unwrap();
        let paths = repository.paths(&config.id).unwrap();
        fs::write(paths.home.join("marker.txt"), b"data").unwrap();

        let trash_dir = move_to_trash(&repository, &paths).unwrap();

        assert!(trash_dir.join(TRASH_CONFIG).is_file());
        assert!(trash_dir.join("home/marker.txt").is_file());
        let entries = list_trash(root.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "工作区");
        assert_eq!(entries[0].color, "#1f9d6b");
        assert_eq!(entries[0].instance_id, config.id);
        assert!(entries[0].size_bytes > 0);
    }

    #[test]
    fn restore_rebuilds_the_instance_with_its_home_data_and_original_id() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let (config, key) = keep_instance(&repository, "工作区");

        let restored = restore_trash(&repository, &key).unwrap();

        assert_eq!(restored.id, config.id);
        assert_eq!(restored.name, "工作区");
        let paths = repository.paths(&restored.id).unwrap();
        assert_eq!(fs::read(paths.home.join("marker.txt")).unwrap(), b"data");
        assert!(paths.electron.is_dir());
        assert!(!repository.data_root().join("trash").join(&key).exists());
        assert!(list_trash(root.path()).unwrap().is_empty());
        assert_eq!(repository.list().unwrap().len(), 1);
    }

    #[test]
    fn restore_mints_a_new_id_when_the_original_is_taken() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let (config, key) = keep_instance(&repository, "工作区");
        let mut squat = InstanceConfig::new_blank("占用者");
        squat.id = config.id.clone();
        repository.create(squat).unwrap();

        let restored = restore_trash(&repository, &key).unwrap();

        assert_ne!(restored.id, config.id);
        assert_eq!(restored.name, "工作区");
        assert!(
            repository
                .paths(&restored.id)
                .unwrap()
                .home
                .join("marker.txt")
                .is_file()
        );
        assert_eq!(repository.list().unwrap().len(), 2);
    }

    #[test]
    fn restore_falls_back_to_a_blank_instance_for_legacy_entries() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let key = "legacy-20200101T000000";
        let trash_dir = root.path().join("trash").join(key);
        fs::create_dir_all(trash_dir.join("home")).unwrap();
        fs::write(trash_dir.join("home/marker.txt"), b"old").unwrap();
        let metadata = TrashMetadata {
            instance_id: "legacy".into(),
            name: None,
            deleted_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + Duration::days(30)).to_rfc3339(),
        };
        fs::write(
            trash_dir.join(TRASH_METADATA),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();

        let restored = restore_trash(&repository, key).unwrap();

        assert_eq!(restored.name, "legacy");
        assert!(
            repository
                .paths(&restored.id)
                .unwrap()
                .home
                .join("marker.txt")
                .is_file()
        );
    }

    #[test]
    fn purge_removes_only_the_selected_entry() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let (_, first) = keep_instance(&repository, "甲");
        let (_, second) = keep_instance(&repository, "乙");
        assert_eq!(list_trash(root.path()).unwrap().len(), 2);

        purge_trash_entry(root.path(), &first).unwrap();

        let remaining = list_trash(root.path()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, second);
    }

    #[test]
    fn rejects_unsafe_or_missing_trash_keys() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        assert!(matches!(
            restore_trash(&repository, "../escape"),
            Err(StorageError::InvalidTrashKey)
        ));
        assert!(matches!(
            purge_trash_entry(root.path(), "a/b"),
            Err(StorageError::InvalidTrashKey)
        ));
        assert!(matches!(
            restore_trash(&repository, "missing"),
            Err(StorageError::TrashNotFound(_))
        ));
    }
}
