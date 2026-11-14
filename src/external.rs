//! `external.toml`: the record of the user's own (original) DSH installation
//! that Marina detected or was pointed at (design §8.1, §8.2).  Marina never
//! modifies that installation; this file only remembers its path and version.

use crate::runtimes::RuntimeInfo;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const EXTERNAL_FILE: &str = "external.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRecord {
    pub executable: PathBuf,
    #[serde(default)]
    pub version: String,
    /// The original DSH data directory (`~/.dsh`).
    #[serde(default)]
    pub data_home: PathBuf,
    /// RFC 3339 timestamp.
    #[serde(default)]
    pub detected_at: String,
}

impl ExternalRecord {
    pub fn for_runtime(runtime: &RuntimeInfo) -> Self {
        Self {
            executable: runtime.executable.clone(),
            version: runtime.version.clone(),
            data_home: default_data_home(),
            detected_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// `%USERPROFILE%\.dsh`, the data directory of an original DSH install.
pub fn default_data_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".dsh")
}

pub fn external_path(data_root: &Path) -> PathBuf {
    data_root.join(EXTERNAL_FILE)
}

/// Atomically write `<data_root>/external.toml`.
pub fn save_external(data_root: &Path, record: &ExternalRecord) -> std::io::Result<()> {
    fs::create_dir_all(data_root)?;
    let text = toml::to_string_pretty(record)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut temporary = tempfile::NamedTempFile::new_in(data_root)?;
    std::io::Write::write_all(&mut temporary, text.as_bytes())?;
    temporary
        .persist(external_path(data_root))
        .map_err(|error| error.error)?;
    Ok(())
}

/// `None` when the file is missing or unreadable.
pub fn load_external(data_root: &Path) -> Option<ExternalRecord> {
    let text = fs::read_to_string(external_path(data_root)).ok()?;
    toml::from_str(&text).ok()
}

/// Remove the record (design §7.7, after the original install was deleted).
/// A missing file is not an error.
pub fn remove_external(data_root: &Path) -> std::io::Result<()> {
    match fs::remove_file(external_path(data_root)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtimes::{DSH_EXECUTABLE_NAME, RuntimeRegistry};
    use tempfile::tempdir;

    #[test]
    fn saves_loads_and_removes_the_record() {
        let root = tempdir().unwrap();
        assert!(load_external(root.path()).is_none());
        let record = ExternalRecord {
            executable: PathBuf::from(r"C:\Apps\DeepSeek Harness\DeepSeek Harness.exe"),
            version: "0.2.1".into(),
            data_home: PathBuf::from(r"C:\Users\someone\.dsh"),
            detected_at: "2026-01-01T00:00:00Z".into(),
        };
        save_external(root.path(), &record).unwrap();
        assert_eq!(load_external(root.path()), Some(record));
        remove_external(root.path()).unwrap();
        assert!(load_external(root.path()).is_none());
        remove_external(root.path()).unwrap();
    }

    #[test]
    fn registering_an_external_runtime_writes_the_record() {
        let root = tempdir().unwrap();
        let executable = root.path().join("dsh").join(DSH_EXECUTABLE_NAME);
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"placeholder").unwrap();
        let data = root.path().join("data");
        RuntimeRegistry::new(&data)
            .register_external(&executable)
            .unwrap();
        let record = load_external(&data).unwrap();
        assert_eq!(record.executable, executable);
        assert_eq!(record.version, "外部安装");
        assert!(record.data_home.ends_with(".dsh"));
    }
}
