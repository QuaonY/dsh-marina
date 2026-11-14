//! Persistent instance configuration and directory management.

use crate::copy::{self, CopyProgress};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::NamedTempFile;
use thiserror::Error;

pub const INSTANCE_SCHEMA_VERSION: u32 = 1;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceStatus {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
    AdoptedRunning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default = "default_color")]
    pub color: String,
    /// One emoji or character shown on the badge instead of the name's initial.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    /// Free-form notes shown in the overview (FR-1).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub isolation: IsolationConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub origin: OriginConfig,
}

impl InstanceConfig {
    pub fn new_blank(name: impl Into<String>) -> Self {
        Self {
            schema_version: INSTANCE_SCHEMA_VERSION,
            id: generate_instance_id(),
            name: name.into(),
            color: default_color(),
            icon: String::new(),
            notes: String::new(),
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            runtime: RuntimeConfig::default(),
            isolation: IsolationConfig::default(),
            safety: SafetyConfig::default(),
            origin: OriginConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        if !is_valid_id(&self.id) {
            return Err(RepositoryError::InvalidId(self.id.clone()));
        }
        if self.name.trim().is_empty() {
            return Err(RepositoryError::EmptyName);
        }
        if !is_valid_color(&self.color) {
            return Err(RepositoryError::InvalidColor(self.color.clone()));
        }
        if self.icon.chars().count() > MAX_ICON_CHARS || self.icon.chars().any(char::is_control) {
            return Err(RepositoryError::InvalidIcon(self.icon.clone()));
        }
        if self.notes.chars().count() > MAX_NOTES_CHARS {
            return Err(RepositoryError::NotesTooLong(MAX_NOTES_CHARS));
        }
        if self.schema_version != INSTANCE_SCHEMA_VERSION {
            return Err(RepositoryError::UnsupportedSchema(self.schema_version));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_runtime_version")]
    pub dsh_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_version: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            dsh_version: default_runtime_version(),
            last_used_version: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IsolationConfig {
    #[serde(default)]
    pub agents_home: AgentsHomeMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentsHomeMode {
    #[default]
    Shared,
    Isolated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub stable_seconds: u64,
    pub auto_rollback: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            stable_seconds: 180,
            auto_rollback: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OriginConfig {
    pub kind: OriginKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OriginKind {
    #[default]
    Blank,
    /// 早期版本支持「从原版 DSH 导入」；现在只用于读取旧实例配置。
    ImportOriginal,
    Clone,
    Package,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePaths {
    pub root: PathBuf,
    pub home: PathBuf,
    pub electron: PathBuf,
    pub agents: PathBuf,
    pub marina: PathBuf,
    pub config: PathBuf,
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("实例 ID 只能包含 ASCII 字母、数字、'-' 或 '_'：{0}")]
    InvalidId(String),
    #[error("实例名称不能为空")]
    EmptyName,
    #[error("实例颜色必须是 #RRGGBB 格式：{0}")]
    InvalidColor(String),
    #[error("图标最多 2 个字符：{0}")]
    InvalidIcon(String),
    #[error("备注不能超过 {0} 个字符")]
    NotesTooLong(usize),
    #[error("不支持的实例配置版本：{0}")]
    UnsupportedSchema(u32),
    #[error("实例已经存在：{0}")]
    AlreadyExists(String),
    #[error("找不到实例：{0}")]
    NotFound(String),
    #[error("读取实例配置失败（{path}）：{source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("实例配置不是合法的 TOML（{path}）：{source}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("写入实例配置失败：{0}")]
    SerializeConfig(#[from] toml::ser::Error),
    #[error("保存实例配置失败：{0}")]
    PersistConfig(#[source] std::io::Error),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
}

impl RepositoryError {
    /// True when a progress-aware copy stopped because the user cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Io(error) if copy::is_cancelled(error))
    }
}

#[derive(Debug, Clone)]
pub struct InstanceRepository {
    data_root: PathBuf,
}

impl InstanceRepository {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn list(&self) -> Result<Vec<InstanceConfig>, RepositoryError> {
        let instances_dir = self.instances_dir();
        if !instances_dir.exists() {
            return Ok(Vec::new());
        }

        let mut instances = Vec::new();
        for entry in fs::read_dir(instances_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(id) = file_name.to_str() else {
                continue;
            };
            // Ignore an interrupted creation staging directory, never mistake it for a real instance.
            if id.starts_with('.') || !is_valid_id(id) {
                continue;
            }
            instances.push(self.load(id)?);
        }
        instances.sort_by_key(|instance| instance.name.to_lowercase());
        Ok(instances)
    }

    pub fn load(&self, id: &str) -> Result<InstanceConfig, RepositoryError> {
        if !is_valid_id(id) {
            return Err(RepositoryError::InvalidId(id.to_owned()));
        }
        let paths = self.paths_for(id);
        if !paths.root.is_dir() {
            return Err(RepositoryError::NotFound(id.to_owned()));
        }
        let text =
            fs::read_to_string(&paths.config).map_err(|source| RepositoryError::ReadConfig {
                path: paths.config.clone(),
                source,
            })?;
        let config: InstanceConfig =
            toml::from_str(&text).map_err(|source| RepositoryError::ParseConfig {
                path: paths.config,
                source,
            })?;
        config.validate()?;
        if config.id != id {
            return Err(RepositoryError::InvalidId(config.id));
        }
        Ok(config)
    }

    pub fn create_blank(&self, name: impl Into<String>) -> Result<InstanceConfig, RepositoryError> {
        let config = InstanceConfig::new_blank(name);
        self.create(config)
    }

    pub fn create(&self, config: InstanceConfig) -> Result<InstanceConfig, RepositoryError> {
        config.validate()?;
        fs::create_dir_all(self.instances_dir())?;

        let paths = self.paths_for(&config.id);
        if paths.root.exists() {
            return Err(RepositoryError::AlreadyExists(config.id));
        }
        let staging = self.instances_dir().join(format!(".{}.staging", config.id));
        fs::create_dir(&staging)?;

        let result = (|| {
            fs::create_dir(staging.join("home"))?;
            fs::create_dir(staging.join("electron"))?;
            fs::create_dir_all(staging.join("marina"))?;
            write_config_atomically(&staging.join("instance.toml"), &config)?;
            fs::rename(&staging, &paths.root)?;
            Ok(config.clone())
        })();

        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    /// Rename an instance without changing its stable id or data directories.
    pub fn rename(
        &self,
        id: &str,
        name: impl Into<String>,
    ) -> Result<InstanceConfig, RepositoryError> {
        let mut config = self.load(id)?;
        config.name = name.into();
        config.validate()?;
        self.save(&config)?;
        Ok(config)
    }

    /// Clone plugin configuration and data into a new instance.
    /// Personal sessions, credentials, Electron state, and anonymous identity are
    /// excluded unless the caller explicitly opts into copying personal data.
    pub fn clone_instance(
        &self,
        source_id: &str,
        name: impl Into<String>,
        include_personal_data: bool,
    ) -> Result<InstanceConfig, RepositoryError> {
        self.clone_instance_with_progress(
            source_id,
            name,
            include_personal_data,
            &CopyProgress::default(),
        )
    }

    /// Like [`clone_instance`](Self::clone_instance), reporting copied bytes to
    /// `progress`.  A cancel or failure removes the half-copied instance.
    pub fn clone_instance_with_progress(
        &self,
        source_id: &str,
        name: impl Into<String>,
        include_personal_data: bool,
        progress: &CopyProgress,
    ) -> Result<InstanceConfig, RepositoryError> {
        let source_config = self.load(source_id)?;
        let source_paths = self.paths(source_id)?;
        let mut target_config = InstanceConfig::new_blank(name);
        target_config.color = source_config.color;
        target_config.runtime = source_config.runtime;
        target_config.runtime.last_used_version = None;
        target_config.isolation = source_config.isolation;
        target_config.safety = source_config.safety;
        target_config.origin = OriginConfig {
            kind: OriginKind::Clone,
            from: Some(source_id.to_owned()),
        };

        let target_config = self.create(target_config)?;
        let target_paths = self.paths(&target_config.id)?;
        let mut items = vec![CopyItem::new(
            source_paths.home,
            target_paths.home.clone(),
            true,
            include_personal_data,
            true,
        )];
        if include_personal_data {
            items.push(CopyItem::new(
                source_paths.electron,
                target_paths.electron.clone(),
                true,
                true,
                false,
            ));
        }
        if let Err(error) = run_copy(&items, progress) {
            let _ = fs::remove_dir_all(&target_paths.root);
            return Err(error.into());
        }
        Ok(target_config)
    }

    pub fn set_runtime_binding(
        &self,
        id: &str,
        dsh_version: impl Into<String>,
    ) -> Result<InstanceConfig, RepositoryError> {
        let mut config = self.load(id)?;
        config.runtime.dsh_version = dsh_version.into();
        self.save(&config)?;
        Ok(config)
    }

    pub fn set_last_used_version(
        &self,
        id: &str,
        version: impl Into<String>,
    ) -> Result<InstanceConfig, RepositoryError> {
        let mut config = self.load(id)?;
        config.runtime.last_used_version = Some(version.into());
        self.save(&config)?;
        Ok(config)
    }

    pub fn set_safety(
        &self,
        id: &str,
        auto_rollback: bool,
        stable_seconds: u64,
    ) -> Result<InstanceConfig, RepositoryError> {
        let mut config = self.load(id)?;
        config.safety.auto_rollback = auto_rollback;
        config.safety.stable_seconds = stable_seconds.max(30);
        self.save(&config)?;
        Ok(config)
    }

    /// Badge color (`#rrggbb`), icon (≤ 2 characters, empty = initial) and notes.
    pub fn set_appearance(
        &self,
        id: &str,
        color: &str,
        icon: &str,
        notes: &str,
    ) -> Result<InstanceConfig, RepositoryError> {
        let mut config = self.load(id)?;
        config.color = color.to_owned();
        config.icon = icon.trim().to_owned();
        config.notes = notes.trim().to_owned();
        config.validate()?;
        self.save(&config)?;
        Ok(config)
    }

    pub fn set_agents_isolated(
        &self,
        id: &str,
        isolated: bool,
    ) -> Result<InstanceConfig, RepositoryError> {
        let mut config = self.load(id)?;
        config.isolation.agents_home = if isolated {
            AgentsHomeMode::Isolated
        } else {
            AgentsHomeMode::Shared
        };
        self.save(&config)?;
        Ok(config)
    }
    pub fn save(&self, config: &InstanceConfig) -> Result<(), RepositoryError> {
        config.validate()?;
        let paths = self.paths_for(&config.id);
        if !paths.root.is_dir() {
            return Err(RepositoryError::NotFound(config.id.clone()));
        }
        write_config_atomically(&paths.config, config)
    }

    pub fn paths(&self, id: &str) -> Result<InstancePaths, RepositoryError> {
        if !is_valid_id(id) {
            return Err(RepositoryError::InvalidId(id.to_owned()));
        }
        Ok(self.paths_for(id))
    }

    fn instances_dir(&self) -> PathBuf {
        self.data_root.join("instances")
    }

    fn paths_for(&self, id: &str) -> InstancePaths {
        let root = self.instances_dir().join(id);
        InstancePaths {
            home: root.join("home"),
            electron: root.join("electron"),
            agents: root.join("agents"),
            marina: root.join("marina"),
            config: root.join("instance.toml"),
            root,
        }
    }
}

/// One source to copy: a single file or a directory tree with its filters.
struct CopyItem {
    source: PathBuf,
    target: PathBuf,
    is_dir: bool,
    include_personal_data: bool,
    filter_dsh_identity: bool,
}

impl CopyItem {
    fn new(
        source: PathBuf,
        target: PathBuf,
        is_dir: bool,
        include_personal_data: bool,
        filter_dsh_identity: bool,
    ) -> Self {
        Self {
            source,
            target,
            is_dir,
            include_personal_data,
            filter_dsh_identity,
        }
    }
}

/// Measure every item first so the progress bar has a real total, then copy.
fn run_copy(items: &[CopyItem], progress: &CopyProgress) -> std::io::Result<()> {
    for item in items {
        if item.is_dir {
            walk_tree(&item.source, None, item, progress)?;
        } else {
            progress.add_total(fs::metadata(&item.source)?.len());
        }
    }
    progress.finish_measuring();
    for item in items {
        if item.is_dir {
            walk_tree(&item.source, Some(&item.target), item, progress)?;
        } else {
            copy::copy_file(&item.source, &item.target, progress)?;
        }
    }
    Ok(())
}

/// Walk `source` with the item's filters.  Without a target it only adds
/// file sizes to the progress total.
fn walk_tree(
    source: &Path,
    target: Option<&Path>,
    item: &CopyItem,
    progress: &CopyProgress,
) -> std::io::Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    if let Some(target) = target {
        fs::create_dir_all(target)?;
    }
    for entry in fs::read_dir(source)? {
        progress.check()?;
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if item.filter_dsh_identity && !item.include_personal_data {
            let lower = name.to_string_lossy().to_ascii_lowercase();
            if lower == "sessions" || lower == ".credentials.yaml" || lower == ".anonymous-user-id"
            {
                continue;
            }
        }
        let destination = target.map(|target| target.join(&name));
        if file_type.is_dir() {
            walk_tree(&entry.path(), destination.as_deref(), item, progress)?;
        } else if file_type.is_file() {
            match destination {
                Some(destination) => copy::copy_file(&entry.path(), &destination, progress)?,
                None => progress.add_total(entry.metadata()?.len()),
            }
        }
    }
    Ok(())
}
fn write_config_atomically(path: &Path, config: &InstanceConfig) -> Result<(), RepositoryError> {
    let text = toml::to_string_pretty(config)?;
    let parent = path.parent().ok_or_else(|| {
        RepositoryError::PersistConfig(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "实例配置路径没有上级目录",
        ))
    })?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .write_all(text.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(RepositoryError::PersistConfig)?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| RepositoryError::PersistConfig(error.error))
}

fn generate_instance_id() -> String {
    let timestamp = Utc::now().timestamp_millis();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("instance-{timestamp}-{sequence}")
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn is_valid_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

fn default_schema_version() -> u32 {
    INSTANCE_SCHEMA_VERSION
}

/// An emoji with a variation selector or skin tone takes two chars.
const MAX_ICON_CHARS: usize = 2;
const MAX_NOTES_CHARS: usize = 2000;

fn default_color() -> String {
    "#0d6efd".into()
}

fn default_runtime_version() -> String {
    "latest-stable".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn creates_instance_layout_and_round_trips_design_config() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let config = repository.create_blank("工作区").unwrap();
        let paths = repository.paths(&config.id).unwrap();

        assert!(paths.home.is_dir());
        assert!(paths.electron.is_dir());
        assert!(paths.marina.is_dir());
        assert!(paths.config.is_file());
        assert_eq!(repository.load(&config.id).unwrap(), config);
        assert_eq!(repository.list().unwrap(), vec![config]);
    }

    #[test]
    fn save_replaces_existing_config_atomically() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let mut config = repository.create_blank("工作区").unwrap();
        config.name = "开发环境".into();
        config.safety.auto_rollback = true;

        repository.save(&config).unwrap();
        assert_eq!(repository.load(&config.id).unwrap(), config);
    }

    #[test]
    fn renames_instance_without_changing_its_id_or_paths() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let created = repository.create_blank("原名称").unwrap();
        let paths_before = repository.paths(&created.id).unwrap();

        let renamed = repository.rename(&created.id, "新名称").unwrap();

        assert_eq!(renamed.id, created.id);
        assert_eq!(renamed.name, "新名称");
        assert_eq!(repository.paths(&created.id).unwrap(), paths_before);
        assert_eq!(repository.load(&created.id).unwrap(), renamed);
    }

    #[test]
    fn clone_excludes_personal_data_by_default_and_records_origin() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        let source = repository.create_blank("源实例").unwrap();
        let source_paths = repository.paths(&source.id).unwrap();
        fs::create_dir_all(source_paths.home.join("profiles/desktop/packages")).unwrap();
        fs::create_dir_all(source_paths.home.join("sessions")).unwrap();
        fs::write(
            source_paths.home.join("profiles/desktop/package.json"),
            b"{}",
        )
        .unwrap();
        fs::write(source_paths.home.join(".credentials.yaml"), b"secret").unwrap();
        fs::write(source_paths.home.join("sessions/chat.json"), b"private").unwrap();
        fs::write(source_paths.electron.join("login.db"), b"private").unwrap();

        let cloned = repository
            .clone_instance(&source.id, "副本", false)
            .unwrap();
        let cloned_paths = repository.paths(&cloned.id).unwrap();

        assert_eq!(cloned.origin.kind, OriginKind::Clone);
        assert_eq!(cloned.origin.from.as_deref(), Some(source.id.as_str()));
        assert!(
            cloned_paths
                .home
                .join("profiles/desktop/package.json")
                .is_file()
        );
        assert!(!cloned_paths.home.join(".credentials.yaml").exists());
        assert!(!cloned_paths.home.join("sessions").exists());
        assert!(!cloned_paths.electron.join("login.db").exists());
    }
    #[test]
    fn rejects_ids_that_could_escape_the_instances_directory() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());

        assert!(matches!(
            repository.paths("../outside"),
            Err(RepositoryError::InvalidId(_))
        ));
    }

    #[test]
    fn empty_repository_is_a_valid_first_run_state() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path());
        assert!(repository.list().unwrap().is_empty());
    }
}
