//! Discovery and selection of DSH desktop runtimes.
//!
//! Marina keeps downloaded runtimes in `runtimes/<version>/`.  A user may also
//! point Marina at an already installed DSH executable; that installation is
//! exposed as the single `external` runtime and is never modified by Marina.

use crate::runtime::RuntimeSettings;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const LATEST_STABLE: &str = "latest-stable";
pub const EXTERNAL_RUNTIME_ID: &str = "external";
pub const DSH_EXECUTABLE_NAME: &str = "DeepSeek Harness.exe";
const RUNTIME_METADATA_FILE: &str = "marina-runtime.json";
const MAX_ASAR_HEADER_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeSource {
    /// Installed below Marina's own `runtimes/` directory.
    Managed,
    /// An existing DSH installation outside Marina's data root.
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub id: String,
    pub version: String,
    pub executable: PathBuf,
    pub source: RuntimeSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl RuntimeInfo {
    pub fn is_external(&self) -> bool {
        self.source == RuntimeSource::External
    }

    pub fn display_label(&self) -> String {
        match self.source {
            RuntimeSource::Managed => format!("DSH {}", self.version),
            RuntimeSource::External => format!("原版 DSH {}", self.version),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RuntimeMetadata {
    #[serde(default)]
    version: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    installed_at: Option<String>,
    #[serde(default)]
    sha512: Option<String>,
    /// See [`fingerprint`]; recorded at install time (design §14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("还没有可用的 DSH 运行时，请先下载一个，或选择一个已有的可执行文件")]
    NoRuntime,
    #[error("找不到这个版本的 DSH 运行时：{0}")]
    VersionNotFound(String),
    #[error("运行时目录里缺少 {0}")]
    MissingExecutable(PathBuf),
    #[error("读取运行时信息失败（{path}）：{source}")]
    ReadMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("运行时信息格式不对（{path}）：{source}")]
    ParseMetadata {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("写入运行时信息失败：{0}")]
    EncodeMetadata(#[from] serde_json::Error),
    #[error("运行时版本号不合法：{0}")]
    InvalidVersion(String),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
}

/// Whether `version` may be used as a directory name under `runtimes/`.
///
/// Version strings come from remote feeds and from an installer's
/// `app.asar`, and they are joined onto the data root — so anything that can
/// climb out of it (separators, `.`/`..`, absolute paths) is rejected.
pub fn is_safe_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version.starts_with(|c: char| c.is_ascii_alphanumeric())
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

#[derive(Debug, Clone)]
pub struct RuntimeRegistry {
    data_root: PathBuf,
}

impl RuntimeRegistry {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn runtimes_dir(&self) -> PathBuf {
        self.data_root.join("runtimes")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.data_root.join("download")
    }

    /// Managed runtimes, newest version first.
    pub fn scan(&self) -> Result<Vec<RuntimeInfo>, RuntimeError> {
        let directory = self.runtimes_dir();
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut runtimes = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(directory_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if directory_name.starts_with('.') {
                continue;
            }
            let executable = entry.path().join(DSH_EXECUTABLE_NAME);
            if !executable.is_file() {
                continue;
            }
            let metadata = read_metadata(&entry.path()).ok().flatten();
            let version = metadata
                .as_ref()
                .map(|metadata| metadata.version.clone())
                .filter(|version| !version.is_empty())
                .or_else(|| read_dsh_version(&executable))
                .unwrap_or_else(|| directory_name.clone());
            runtimes.push(RuntimeInfo {
                id: directory_name.clone(),
                version,
                executable,
                source: RuntimeSource::Managed,
                installed_at: metadata.as_ref().and_then(|m| m.installed_at.clone()),
                size_bytes: None,
            });
        }
        runtimes.sort_by(|left, right| compare_versions(&right.version, &left.version));
        Ok(runtimes)
    }

    /// The DSH installation the user already had before installing Marina.
    /// `configured` wins over the conventional installation directory.
    pub fn external(&self, configured: Option<&Path>) -> Option<RuntimeInfo> {
        let mut candidates = Vec::new();
        if let Some(configured) = configured {
            candidates.push(configured.to_path_buf());
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local_app_data)
                    .join("Programs")
                    .join("DeepSeek Harness")
                    .join(DSH_EXECUTABLE_NAME),
            );
        }
        for executable in candidates {
            if !executable.is_file() {
                continue;
            }
            let version = read_dsh_version(&executable).unwrap_or_else(|| "外部安装".to_owned());
            let runtime = RuntimeInfo {
                id: EXTERNAL_RUNTIME_ID.to_owned(),
                version,
                executable,
                source: RuntimeSource::External,
                installed_at: None,
                size_bytes: None,
            };
            self.refresh_external_record(&runtime);
            return Some(runtime);
        }
        None
    }

    /// Keep `external.toml` in step with detection, writing only on change.
    fn refresh_external_record(&self, runtime: &RuntimeInfo) {
        if !self.data_root.is_dir() {
            return;
        }
        let unchanged = crate::external::load_external(&self.data_root).is_some_and(|record| {
            record.version == runtime.version && same_file(&record.executable, &runtime.executable)
        });
        if !unchanged {
            let _ = self.save_external_record(runtime);
        }
    }

    /// All runtimes visible to the user, including the external installation.
    pub fn all(&self, settings: &RuntimeSettings) -> Result<Vec<RuntimeInfo>, RuntimeError> {
        let mut runtimes = self.scan()?;
        if let Some(external) = self.external(settings.dsh_executable.as_deref())
            && !runtimes
                .iter()
                .any(|runtime| same_file(&runtime.executable, &external.executable))
        {
            runtimes.push(external);
        }
        Ok(runtimes)
    }

    /// Resolve the runtime requested by an instance.  `latest-stable` follows
    /// the newest managed runtime and falls back to the external installation.
    pub fn resolve(
        &self,
        requested: &str,
        settings: &RuntimeSettings,
    ) -> Result<RuntimeInfo, RuntimeError> {
        let runtimes = self.all(settings)?;
        let managed = runtimes
            .iter()
            .filter(|runtime| runtime.source == RuntimeSource::Managed)
            .collect::<Vec<_>>();
        let external = runtimes
            .iter()
            .find(|runtime| runtime.source == RuntimeSource::External);

        if requested == EXTERNAL_RUNTIME_ID {
            return external.cloned().ok_or(RuntimeError::NoRuntime);
        }
        if requested.is_empty() || requested == LATEST_STABLE {
            if let Some(latest) = managed.first() {
                return Ok((*latest).clone());
            }
            return external.cloned().ok_or(RuntimeError::NoRuntime);
        }
        if let Some(runtime) = managed.iter().find(|runtime| runtime.version == requested) {
            return Ok((*runtime).clone());
        }
        if let Some(runtime) = external
            && (runtime.version == requested || runtime.id == requested)
        {
            return Ok(runtime.clone());
        }
        Err(RuntimeError::VersionNotFound(requested.to_owned()))
    }

    /// Register an already installed DSH executable as the external runtime.
    /// This is deliberately a path registration, not a copy: the installation
    /// remains owned by the user.
    pub fn register_external(&self, executable: &Path) -> Result<RuntimeInfo, RuntimeError> {
        if !executable.is_file() {
            return Err(RuntimeError::MissingExecutable(executable.to_path_buf()));
        }
        let version = read_dsh_version(executable).unwrap_or_else(|| "外部安装".to_owned());
        let runtime = RuntimeInfo {
            id: EXTERNAL_RUNTIME_ID.to_owned(),
            version,
            executable: executable.to_path_buf(),
            source: RuntimeSource::External,
            installed_at: None,
            size_bytes: None,
        };
        // Best effort: external.toml is a record for the user, not a source of truth.
        let _ = self.save_external_record(&runtime);
        Ok(runtime)
    }

    pub fn mark_installed(&self, version: &str) -> Result<(), RuntimeError> {
        self.mark_installed_from(version, "official")
    }

    pub fn mark_installed_from(&self, version: &str, source: &str) -> Result<(), RuntimeError> {
        if !is_safe_version(version) {
            return Err(RuntimeError::InvalidVersion(version.to_owned()));
        }
        let directory = self.runtimes_dir().join(version);
        let metadata = RuntimeMetadata {
            version: version.to_owned(),
            source: source.to_owned(),
            installed_at: Some(chrono::Utc::now().to_rfc3339()),
            sha512: None,
            fingerprint: fingerprint(&directory).ok(),
        };
        write_metadata(&directory, &metadata)
    }

    /// Store the current [`fingerprint`] of a managed runtime in its
    /// `marina-runtime.json`.  External runtimes are never written to.
    pub fn record_fingerprint(&self, runtime: &RuntimeInfo) -> Result<(), RuntimeError> {
        let Some(directory) = managed_directory(runtime) else {
            return Ok(());
        };
        let mut metadata = read_metadata(directory)?.unwrap_or_else(|| RuntimeMetadata {
            version: runtime.version.clone(),
            source: "unknown".to_owned(),
            installed_at: runtime.installed_at.clone(),
            ..RuntimeMetadata::default()
        });
        metadata.fingerprint = Some(fingerprint(directory)?);
        write_metadata(directory, &metadata)
    }

    /// Compare a managed runtime against its recorded fingerprint (design §14:
    /// warn when runtime files change, e.g. after DSH updated itself in place).
    /// `None` for external runtimes or when nothing was recorded;
    /// `Some(false)` when files changed or can no longer be read.
    pub fn verify_fingerprint(&self, runtime: &RuntimeInfo) -> Option<bool> {
        let directory = managed_directory(runtime)?;
        let recorded = read_metadata(directory).ok().flatten()?.fingerprint?;
        Some(fingerprint(directory).is_ok_and(|current| current == recorded))
    }

    /// Persist the external DSH found by detection or chosen by the user to
    /// `external.toml` (design §8.1).
    pub fn save_external_record(&self, runtime: &RuntimeInfo) -> std::io::Result<()> {
        crate::external::save_external(
            &self.data_root,
            &crate::external::ExternalRecord::for_runtime(runtime),
        )
    }
}

fn managed_directory(runtime: &RuntimeInfo) -> Option<&Path> {
    if runtime.is_external() {
        return None;
    }
    runtime.executable.parent()
}

fn write_metadata(directory: &Path, metadata: &RuntimeMetadata) -> Result<(), RuntimeError> {
    let bytes = serde_json::to_vec_pretty(metadata)?;
    fs::write(directory.join(RUNTIME_METADATA_FILE), bytes)?;
    Ok(())
}

/// Cheap tamper fingerprint of a runtime directory: SHA-256 over the relative
/// path, size and modification time of the main executable and every file
/// below `resources/`.  File contents are not hashed.
pub fn fingerprint(runtime_dir: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::time::UNIX_EPOCH;

    let mut files = Vec::new();
    let executable = runtime_dir.join(DSH_EXECUTABLE_NAME);
    files.push((DSH_EXECUTABLE_NAME.to_owned(), fs::metadata(&executable)?));
    let mut pending = vec![(runtime_dir.join("resources"), "resources".to_owned())];
    while let Some((directory, relative)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = format!("{relative}/{}", entry.file_name().to_string_lossy());
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push((entry.path(), name));
            } else {
                files.push((name, entry.metadata()?));
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (name, metadata) in &files {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(metadata.len().to_le_bytes());
        hasher.update(modified.to_le_bytes());
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Ids of runtimes used by at least one instance, per [`usage_count`].
pub fn used_runtime_ids(
    instances: &[crate::instances::InstanceConfig],
    runtimes: &[RuntimeInfo],
) -> HashSet<String> {
    let newest_managed = runtimes
        .iter()
        .filter(|runtime| !runtime.is_external())
        .max_by(|left, right| compare_versions(&left.version, &right.version))
        .map(|runtime| runtime.version.clone());
    runtimes
        .iter()
        .filter(|runtime| usage_count(instances, runtime, newest_managed.as_deref()) > 0)
        .map(|runtime| runtime.id.clone())
        .collect()
}

/// Design §FR-9: at most `max` managed runtimes.  When there are more, return
/// the oldest unused managed runtimes whose removal gets back to `max` (fewer
/// if not enough are unused).  External runtimes never count or appear.
pub fn cleanup_candidates(
    managed: &[RuntimeInfo],
    used_ids: &HashSet<String>,
    max: u32,
) -> Vec<RuntimeInfo> {
    let managed = managed
        .iter()
        .filter(|runtime| !runtime.is_external())
        .collect::<Vec<_>>();
    let excess = managed.len().saturating_sub(max as usize);
    if excess == 0 {
        return Vec::new();
    }
    let mut unused = managed
        .into_iter()
        .filter(|runtime| !used_ids.contains(&runtime.id))
        .collect::<Vec<_>>();
    unused.sort_by(|left, right| compare_age(left, right));
    unused.into_iter().take(excess).cloned().collect()
}

/// Older first: by semver when both parse, otherwise by install time.
fn compare_age(left: &RuntimeInfo, right: &RuntimeInfo) -> std::cmp::Ordering {
    match (
        Version::parse(&left.version),
        Version::parse(&right.version),
    ) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left
            .installed_at
            .cmp(&right.installed_at)
            .then_with(|| left.version.cmp(&right.version)),
    }
}

fn read_metadata(directory: &Path) -> Result<Option<RuntimeMetadata>, RuntimeError> {
    let path = directory.join(RUNTIME_METADATA_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| RuntimeError::ParseMetadata { path, source }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RuntimeError::ReadMetadata { path, source }),
    }
}

/// Read `version` from `resources/app.asar` without extracting the archive.
///
/// An asar file is a small pickle header followed by a JSON directory index and
/// the concatenated file payloads.  We only need the `package.json` entry.
pub fn read_dsh_version(executable: &Path) -> Option<String> {
    let asar = executable.parent()?.join("resources").join("app.asar");
    read_asar_package_version(&asar)
}

pub fn read_asar_package_version(asar: &Path) -> Option<String> {
    let mut file = fs::File::open(asar).ok()?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header).ok()?;
    let json_length = u32::from_le_bytes(header[12..16].try_into().ok()?) as u64;
    if json_length == 0 || json_length > MAX_ASAR_HEADER_BYTES {
        return None;
    }
    file.seek(SeekFrom::Start(16)).ok()?;
    let mut json = vec![0u8; json_length as usize];
    file.read_exact(&mut json).ok()?;
    let directory: Value = serde_json::from_slice(&json).ok()?;
    let entry = directory.get("files")?.get("package.json")?;
    let offset = entry.get("offset")?.as_str()?.parse::<u64>().ok()?;
    let size = entry.get("size")?.as_u64()?;
    if size == 0 || size > 4 * 1024 * 1024 {
        return None;
    }
    file.seek(SeekFrom::Start(16 + json_length + offset)).ok()?;
    let mut package = vec![0u8; size as usize];
    file.read_exact(&mut package).ok()?;
    let package: Value = serde_json::from_slice(&package).ok()?;
    package
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

/// Whether moving from `previous` to `next` crosses a major version (design
/// §FR-12).  For 0.x releases the minor number acts as the major one, so
/// 0.2.1 → 0.3.0 counts as a major change.  Unparsable versions never warn.
pub fn is_major_change(previous: &str, next: &str) -> bool {
    match (Version::parse(previous), Version::parse(next)) {
        (Ok(previous), Ok(next)) => {
            if previous.major == 0 && next.major == 0 {
                previous.minor != next.minor
            } else {
                previous.major != next.major
            }
        }
        _ => false,
    }
}

/// Number of instances bound to each runtime binding value.
pub fn usage_count(
    instances: &[crate::instances::InstanceConfig],
    runtime: &RuntimeInfo,
    newest_managed: Option<&str>,
) -> usize {
    instances
        .iter()
        .filter(|instance| {
            let binding = instance.runtime.dsh_version.as_str();
            if runtime.is_external() {
                binding == EXTERNAL_RUNTIME_ID
                    || ((binding.is_empty() || binding == LATEST_STABLE)
                        && newest_managed.is_none())
            } else {
                binding == runtime.version
                    || ((binding.is_empty() || binding == LATEST_STABLE)
                        && newest_managed == Some(runtime.version.as_str()))
            }
        })
        .count()
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy()),
        _ => left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn major_change_uses_minor_for_zero_versions() {
        assert!(is_major_change("0.2.1", "0.3.0"));
        assert!(!is_major_change("0.2.1", "0.2.9"));
        assert!(is_major_change("1.4.0", "2.0.0"));
        assert!(!is_major_change("1.4.0", "1.9.0"));
        assert!(!is_major_change("外部安装", "0.3.0"));
    }

    fn write_fake_runtime(root: &Path, name: &str) -> PathBuf {
        let directory = root.join("runtimes").join(name);
        fs::create_dir_all(&directory).unwrap();
        let executable = directory.join(DSH_EXECUTABLE_NAME);
        fs::write(&executable, b"placeholder").unwrap();
        executable
    }

    #[test]
    fn scans_managed_runtimes_newest_first() {
        let root = tempdir().unwrap();
        write_fake_runtime(root.path(), "0.2.0");
        write_fake_runtime(root.path(), "0.3.0");
        let registry = RuntimeRegistry::new(root.path());

        let runtimes = registry.scan().unwrap();
        assert_eq!(runtimes.len(), 2);
        assert_eq!(runtimes[0].version, "0.3.0");
        assert_eq!(runtimes[1].version, "0.2.0");
        assert_eq!(runtimes[0].source, RuntimeSource::Managed);
    }

    #[test]
    fn resolves_latest_stable_to_newest_managed_runtime() {
        let root = tempdir().unwrap();
        write_fake_runtime(root.path(), "0.2.0");
        let expected = write_fake_runtime(root.path(), "0.3.0");
        let registry = RuntimeRegistry::new(root.path());

        let runtime = registry
            .resolve(LATEST_STABLE, &RuntimeSettings::default())
            .unwrap();
        assert_eq!(runtime.version, "0.3.0");
        assert_eq!(runtime.executable, expected);
    }

    #[test]
    fn resolves_external_executable_when_managed_runtimes_are_absent() {
        let root = tempdir().unwrap();
        let executable = root.path().join("external").join(DSH_EXECUTABLE_NAME);
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"placeholder").unwrap();
        let settings = RuntimeSettings {
            dsh_executable: Some(executable.clone()),
        };
        let registry = RuntimeRegistry::new(root.path().join("data"));

        let runtime = registry.resolve(LATEST_STABLE, &settings).unwrap();
        assert_eq!(runtime.source, RuntimeSource::External);
        assert_eq!(runtime.executable, executable);
    }

    #[test]
    fn missing_requested_version_is_reported() {
        let root = tempdir().unwrap();
        write_fake_runtime(root.path(), "0.2.0");
        let registry = RuntimeRegistry::new(root.path());
        assert!(matches!(
            registry.resolve("9.9.9", &RuntimeSettings::default()),
            Err(RuntimeError::VersionNotFound(version)) if version == "9.9.9"
        ));
    }

    #[test]
    fn reads_version_from_an_asar_directory_index() {
        let root = tempdir().unwrap();
        let asar = root.path().join("app.asar");
        let package = br#"{"version":"0.2.1-rc.5"}"#;
        let index = format!(
            r#"{{"files":{{"package.json":{{"size":{},"offset":"0"}}}}}}"#,
            package.len()
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&((index.len() + 8) as u32).to_le_bytes());
        bytes.extend_from_slice(&((index.len() + 4) as u32).to_le_bytes());
        bytes.extend_from_slice(&(index.len() as u32).to_le_bytes());
        bytes.extend_from_slice(index.as_bytes());
        bytes.extend_from_slice(package);
        fs::write(&asar, bytes).unwrap();

        assert_eq!(
            read_asar_package_version(&asar).as_deref(),
            Some("0.2.1-rc.5")
        );
    }

    fn managed(version: &str, installed_at: Option<&str>) -> RuntimeInfo {
        RuntimeInfo {
            id: version.into(),
            version: version.into(),
            executable: PathBuf::from(version).join(DSH_EXECUTABLE_NAME),
            source: RuntimeSource::Managed,
            installed_at: installed_at.map(str::to_owned),
            size_bytes: None,
        }
    }

    #[test]
    fn cleanup_candidates_pick_the_oldest_unused_runtimes() {
        let runtimes = vec![
            managed("0.4.0", None),
            managed("0.10.0", None),
            managed("0.2.0", None),
            managed("0.3.0", None),
            managed("0.1.0", None),
        ];
        let used = HashSet::from(["0.1.0".to_owned()]);
        let candidates = cleanup_candidates(&runtimes, &used, 3);
        let ids = candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["0.2.0", "0.3.0"]);
        assert!(cleanup_candidates(&runtimes, &used, 5).is_empty());
        let all_used = runtimes.iter().map(|r| r.id.clone()).collect();
        assert!(cleanup_candidates(&runtimes, &all_used, 3).is_empty());
    }

    #[test]
    fn cleanup_candidates_fall_back_to_install_time() {
        let runtimes = vec![
            managed("custom-b", Some("2026-02-01T00:00:00Z")),
            managed("custom-a", Some("2026-03-01T00:00:00Z")),
        ];
        let candidates = cleanup_candidates(&runtimes, &HashSet::new(), 1);
        assert_eq!(candidates[0].id, "custom-b");
    }

    #[test]
    fn used_ids_follow_instance_bindings() {
        let runtimes = vec![managed("0.3.0", None), managed("0.2.0", None)];
        let mut latest = crate::instances::InstanceConfig::new_blank("a".to_owned());
        latest.runtime.dsh_version = LATEST_STABLE.into();
        let used = used_runtime_ids(&[latest], &runtimes);
        assert_eq!(used, HashSet::from(["0.3.0".to_owned()]));
    }

    #[test]
    fn fingerprint_is_recorded_on_install_and_detects_changes() {
        let root = tempdir().unwrap();
        let executable = write_fake_runtime(root.path(), "0.2.0");
        let resources = executable.parent().unwrap().join("resources");
        fs::create_dir_all(resources.join("app.asar.unpacked")).unwrap();
        fs::write(resources.join("app.asar"), b"asar").unwrap();
        fs::write(resources.join("app.asar.unpacked/x.node"), b"x").unwrap();
        let registry = RuntimeRegistry::new(root.path());
        registry.mark_installed("0.2.0").unwrap();
        let runtime = registry.scan().unwrap().remove(0);
        assert_eq!(registry.verify_fingerprint(&runtime), Some(true));

        fs::write(resources.join("app.asar"), b"asar, but updated").unwrap();
        assert_eq!(registry.verify_fingerprint(&runtime), Some(false));
        registry.record_fingerprint(&runtime).unwrap();
        assert_eq!(registry.verify_fingerprint(&runtime), Some(true));

        fs::write(resources.join("new.dll"), b"y").unwrap();
        assert_eq!(registry.verify_fingerprint(&runtime), Some(false));
    }

    #[test]
    fn fingerprint_is_unknown_without_metadata_or_for_external_runtimes() {
        let root = tempdir().unwrap();
        let executable = write_fake_runtime(root.path(), "0.2.0");
        let registry = RuntimeRegistry::new(root.path());
        let runtime = registry.scan().unwrap().remove(0);
        assert_eq!(registry.verify_fingerprint(&runtime), None);
        let external = registry.register_external(&executable).unwrap();
        assert_eq!(registry.verify_fingerprint(&external), None);
        registry.record_fingerprint(&external).unwrap();
        assert!(
            !executable
                .parent()
                .unwrap()
                .join(RUNTIME_METADATA_FILE)
                .exists()
        );
    }
}
