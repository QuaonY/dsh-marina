//! Launcher-wide settings for locating a DSH executable.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RuntimeSettings {
    /// The selected DSH desktop executable, if one has been configured.
    pub dsh_executable: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RuntimeSettingsStore {
    settings_file: PathBuf,
}

impl RuntimeSettingsStore {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            settings_file: data_root.into().join("runtime.toml"),
        }
    }

    pub fn load(&self) -> Result<RuntimeSettings, RuntimeSettingsError> {
        if !self.settings_file.exists() {
            return Ok(RuntimeSettings::default());
        }
        let content = fs::read_to_string(&self.settings_file).map_err(|source| {
            RuntimeSettingsError::ReadSettings {
                path: self.settings_file.clone(),
                source,
            }
        })?;
        toml::from_str(&content).map_err(|source| RuntimeSettingsError::ParseSettings {
            path: self.settings_file.clone(),
            source: Box::new(source),
        })
    }

    pub fn set_dsh_executable(
        &self,
        executable: impl Into<PathBuf>,
    ) -> Result<RuntimeSettings, RuntimeSettingsError> {
        let executable = executable.into();
        validate_dsh_executable(&executable)?;
        let settings = RuntimeSettings {
            dsh_executable: Some(executable),
        };
        self.save(&settings)?;
        Ok(settings)
    }

    fn save(&self, settings: &RuntimeSettings) -> Result<(), RuntimeSettingsError> {
        let content = toml::to_string_pretty(settings)?;
        let parent = self.settings_file.parent().ok_or_else(|| {
            RuntimeSettingsError::PersistSettings(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "运行时设置文件路径没有上级目录",
            ))
        })?;
        fs::create_dir_all(parent).map_err(RuntimeSettingsError::PersistSettings)?;
        let mut temporary =
            NamedTempFile::new_in(parent).map_err(RuntimeSettingsError::PersistSettings)?;
        temporary
            .write_all(content.as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(RuntimeSettingsError::PersistSettings)?;
        temporary
            .persist(&self.settings_file)
            .map(|_| ())
            .map_err(|error| RuntimeSettingsError::PersistSettings(error.error))
    }
}

pub fn validate_dsh_executable(path: &Path) -> Result<(), RuntimeSettingsError> {
    let is_exe = path
        .extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("exe"));
    // A relative path (or one with `..`) still works right now, from Marina's
    // working directory, but breaks as soon as it is launched from elsewhere —
    // so store only a plain absolute path.
    let plain_absolute = path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        });
    if plain_absolute && path.is_file() && is_exe {
        Ok(())
    } else {
        Err(RuntimeSettingsError::InvalidExecutable(path.to_path_buf()))
    }
}

#[derive(Debug, Error)]
pub enum RuntimeSettingsError {
    #[error("DSH 可执行文件必须是已存在的 .exe 文件：{0}")]
    InvalidExecutable(PathBuf),
    #[error("读取运行时设置失败（{path}）：{source}")]
    ReadSettings {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("运行时设置不是合法的 TOML（{path}）：{source}")]
    ParseSettings {
        path: PathBuf,
        #[source]
        // Boxed: toml::de::Error is large and would bloat every Result using this error.
        source: Box<toml::de::Error>,
    },
    #[error("写入运行时设置失败：{0}")]
    SerializeSettings(#[from] toml::ser::Error),
    #[error("保存运行时设置失败：{0}")]
    PersistSettings(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_runtime_settings_are_a_valid_unconfigured_state() {
        let directory = tempdir().unwrap();
        let store = RuntimeSettingsStore::new(directory.path().join("data"));
        assert_eq!(store.load().unwrap(), RuntimeSettings::default());
    }

    #[test]
    fn selected_executable_is_validated_and_persisted() {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("DeepSeek Harness.exe");
        fs::write(&executable, b"test executable placeholder").unwrap();
        let store = RuntimeSettingsStore::new(directory.path().join("data"));

        let settings = store.set_dsh_executable(&executable).unwrap();
        assert_eq!(settings.dsh_executable, Some(executable.clone()));
        assert_eq!(store.load().unwrap(), settings);
    }

    #[test]
    fn rejects_missing_files_and_non_executable_paths() {
        let directory = tempdir().unwrap();
        let store = RuntimeSettingsStore::new(directory.path().join("data"));
        let text_file = directory.path().join("not-an-executable.txt");
        fs::write(&text_file, b"not an executable").unwrap();

        assert!(matches!(
            store.set_dsh_executable(directory.path().join("missing.exe")),
            Err(RuntimeSettingsError::InvalidExecutable(_))
        ));
        assert!(matches!(
            store.set_dsh_executable(text_file),
            Err(RuntimeSettingsError::InvalidExecutable(_))
        ));
    }
}
