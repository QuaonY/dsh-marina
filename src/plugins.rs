//! Discovery and safe editing of third-party plugins in a DSH desktop profile.
//!
//! The profile's `package.json` is the source of truth for installed packages.
//! `cordis.patch.yml` is a user-owned YAML patch layer; Marina only appends
//! id-targeted entries and never rewrites the existing file.  This preserves
//! comments, formatting and `!!js` expressions.

use chrono::Utc;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

const PROFILE_MANIFEST: &str = "profiles/desktop/package.json";
const PROFILE_PATCH: &str = "profiles/desktop/cordis.patch.yml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    /// Loader entry id targeted by the patch layer.  Defaults to the package
    /// name when the patch does not mention the plugin.
    pub entry_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchEntry {
    pub id: String,
    pub name: Option<String>,
    pub disabled: Option<bool>,
}

#[derive(Debug, Error)]
pub enum PluginReadError {
    #[error("读取插件清单失败（{path}）：{source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("插件清单格式不对（{path}）：{source}")]
    ParseManifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("读取插件配置补丁失败（{path}）：{source}")]
    ReadPatch {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("备份插件配置补丁失败（{path}）：{source}")]
    BackupPatch {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("写入插件配置补丁失败（{path}）：{source}")]
    WritePatch {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("实例的插件配置还没有初始化：{0}")]
    ProfileNotInitialized(PathBuf),
}

/// Return third-party dependencies from the initialized desktop profile.
pub fn list_plugins(home: &Path) -> Result<Vec<PluginInfo>, PluginReadError> {
    let manifest = home.join(PROFILE_MANIFEST);
    if !manifest.is_file() {
        return Ok(Vec::new());
    }
    let contents =
        fs::read_to_string(&manifest).map_err(|source| PluginReadError::ReadManifest {
            path: manifest.clone(),
            source,
        })?;
    let package: Value =
        serde_json::from_str(&contents).map_err(|source| PluginReadError::ParseManifest {
            path: manifest,
            source,
        })?;
    let patch = read_patch_entries(home)?;

    let mut plugins = Vec::new();
    for section in ["dependencies", "devDependencies"] {
        let Some(dependencies) = package.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (name, version) in dependencies {
            if name.starts_with("@deepseek-ai/") {
                continue;
            }
            let version = version.as_str().unwrap_or("未知版本");
            let entry = find_last_entry(&patch, name);
            let entry_id = entry
                .map(|entry| entry.id.clone())
                .unwrap_or_else(|| name.clone());
            let enabled = entry.and_then(|entry| entry.disabled) != Some(true);
            if let Some(existing) = plugins
                .iter_mut()
                .find(|plugin: &&mut PluginInfo| plugin.name == *name)
            {
                existing.version = version.to_owned();
            } else {
                plugins.push(PluginInfo {
                    name: name.clone(),
                    version: version.to_owned(),
                    enabled,
                    entry_id,
                });
            }
        }
    }
    plugins.sort_by_key(|plugin| plugin.name.to_lowercase());
    Ok(plugins)
}

/// Append `disabled: true` entries for the selected package names.
/// Returns the number of patch entries written.
pub fn disable_plugins(home: &Path, names: &[String]) -> Result<usize, PluginReadError> {
    append_disabled(home, names, true)
}

/// Append `disabled: false` entries for the selected package names.
pub fn enable_plugins(home: &Path, names: &[String]) -> Result<usize, PluginReadError> {
    append_disabled(home, names, false)
}

/// Disable every third-party package known to the profile.  This mirrors the
/// desktop app's `disableAllPlugins` behavior while keeping the original patch
/// recoverable as a timestamped backup.
pub fn safe_mode(home: &Path) -> Result<usize, PluginReadError> {
    let plugins = list_plugins(home)?;
    let names = plugins
        .into_iter()
        .map(|plugin| plugin.name)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Ok(0);
    }
    disable_plugins(home, &names)
}

/// Read the patch layer into a small, tolerant model.  A full YAML parser is
/// intentionally not used: the file can contain `!!js` tags, and Marina only
/// needs ids, package names and the last disabled value.
pub fn read_patch_entries(home: &Path) -> Result<Vec<PatchEntry>, PluginReadError> {
    let path = home.join(PROFILE_PATCH);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(PluginReadError::ReadPatch { path, source }),
    };
    Ok(parse_patch_entries(&text))
}

fn parse_patch_entries(text: &str) -> Vec<PatchEntry> {
    let mut entries = Vec::new();
    let mut current: Option<PatchEntry> = None;
    let mut current_indent = 0usize;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix("- ")
            && let Some(id) = rest.strip_prefix("id:")
        {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(PatchEntry {
                id: unquote(id.trim()),
                name: None,
                disabled: None,
            });
            current_indent = indent;
            continue;
        }
        if current.is_none() {
            continue;
        }
        if !trimmed.is_empty() && indent <= current_indent && !trimmed.starts_with('#') {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        if let Some(entry) = current.as_mut() {
            if let Some(value) = trimmed.strip_prefix("name:") {
                entry.name = Some(unquote(value.trim()));
            } else if let Some(value) = trimmed.strip_prefix("disabled:") {
                entry.disabled = match value.trim() {
                    "true" | "True" | "TRUE" => Some(true),
                    "false" | "False" | "FALSE" => Some(false),
                    _ => entry.disabled,
                };
            }
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

fn find_last_entry<'a>(entries: &'a [PatchEntry], plugin_name: &str) -> Option<&'a PatchEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.name.as_deref() == Some(plugin_name) || entry.id == plugin_name)
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Quote a value for YAML so characters like `:` `#` `[` cannot start a new
/// structure.  A single-quoted scalar only needs `'` doubled.
fn yaml_scalar(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn append_disabled(
    home: &Path,
    names: &[String],
    disabled: bool,
) -> Result<usize, PluginReadError> {
    let profile = home.join("profiles/desktop");
    if !profile.is_dir() {
        return Err(PluginReadError::ProfileNotInitialized(profile));
    }
    let patch_path = home.join(PROFILE_PATCH);
    if patch_path.is_file() {
        let timestamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ");
        let backup = patch_path.with_file_name(format!("cordis.patch.yml.bak-{timestamp}"));
        fs::copy(&patch_path, &backup).map_err(|source| PluginReadError::BackupPatch {
            path: backup,
            source,
        })?;
    }
    let entries = read_patch_entries(home)?;
    let mut text = String::new();
    let existing = fs::read_to_string(&patch_path).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.push('\n');
    }
    let mut written = 0;
    for name in names {
        let entry_id = find_last_entry(&entries, name)
            .map(|entry| entry.id.clone())
            .unwrap_or_else(|| name.clone());
        // 包名与补丁里的 id 都可能来自外部文件：去掉换行等控制字符，并把
        // id 按 YAML 单引号标量写出，否则一行字符串能拼出额外的 YAML 条目。
        let name = single_line(name);
        let entry_id = yaml_scalar(&single_line(&entry_id));
        text.push_str(&format!(
            "# DSH Marina: {} {}\n- id: {}\n  disabled: {}\n",
            if disabled { "disable" } else { "enable" },
            name,
            entry_id,
            disabled
        ));
        written += 1;
    }
    if written == 0 {
        return Ok(0);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&patch_path)
        .map_err(|source| PluginReadError::WritePatch {
            path: patch_path.clone(),
            source,
        })?;
    file.write_all(text.as_bytes())
        .map_err(|source| PluginReadError::WritePatch {
            path: patch_path,
            source,
        })?;
    Ok(written)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_profile(home: &Path, manifest: &str, patch: &str) {
        let profile = home.join("profiles/desktop");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("package.json"), manifest).unwrap();
        if !patch.is_empty() {
            fs::write(profile.join("cordis.patch.yml"), patch).unwrap();
        }
    }

    #[test]
    fn lists_third_party_dependencies_and_excludes_official_plugins() {
        let root = tempdir().unwrap();
        write_profile(
            root.path(),
            r#"{
                "dependencies": {
                    "z-plugin": "^1.2.0",
                    "@deepseek-ai/dsh-core": "0.2.1",
                    "@scope/a-plugin": "~2.0.0"
                },
                "devDependencies": { "a-dev-plugin": "0.1.0" }
            }"#,
            "",
        );

        let plugins = list_plugins(root.path()).unwrap();
        assert_eq!(
            plugins,
            vec![
                PluginInfo {
                    name: "@scope/a-plugin".into(),
                    version: "~2.0.0".into(),
                    enabled: true,
                    entry_id: "@scope/a-plugin".into(),
                },
                PluginInfo {
                    name: "a-dev-plugin".into(),
                    version: "0.1.0".into(),
                    enabled: true,
                    entry_id: "a-dev-plugin".into(),
                },
                PluginInfo {
                    name: "z-plugin".into(),
                    version: "^1.2.0".into(),
                    enabled: true,
                    entry_id: "z-plugin".into(),
                },
            ]
        );
    }

    #[test]
    fn missing_profile_manifest_is_a_valid_uninitialized_state() {
        let root = tempdir().unwrap();
        assert!(list_plugins(root.path()).unwrap().is_empty());
    }

    #[test]
    fn reads_inserted_entries_and_last_disabled_value() {
        let root = tempdir().unwrap();
        write_profile(
            root.path(),
            r#"{"dependencies":{"@zaofan/dsh-qqbot":"1.0.0","dsh-meme":"0.1.0"}}"#,
            r#"
- insert:
    - id: qqbot1
      name: '@zaofan/dsh-qqbot'
- id: dsh-meme
  name: "dsh-meme"
  disabled: true
- id: dsh-meme
  disabled: false
"#,
        );

        let plugins = list_plugins(root.path()).unwrap();
        let qqbot = plugins
            .iter()
            .find(|plugin| plugin.name == "@zaofan/dsh-qqbot")
            .unwrap();
        assert_eq!(qqbot.entry_id, "qqbot1");
        assert!(qqbot.enabled);
        let meme = plugins
            .iter()
            .find(|plugin| plugin.name == "dsh-meme")
            .unwrap();
        assert!(meme.enabled);
    }

    #[test]
    fn disabling_appends_an_entry_and_backs_up_the_original_patch() {
        let root = tempdir().unwrap();
        write_profile(
            root.path(),
            r#"{"dependencies":{"dsh-meme":"0.1.0"}}"#,
            "- id: dsh-meme\n  name: dsh-meme\n",
        );

        let written = disable_plugins(root.path(), &["dsh-meme".to_owned()]).unwrap();
        assert_eq!(written, 1);
        let patch =
            fs::read_to_string(root.path().join("profiles/desktop/cordis.patch.yml")).unwrap();
        assert!(patch.contains("- id: 'dsh-meme'\n  disabled: true"));
        assert!(
            fs::read_dir(root.path().join("profiles/desktop"))
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        );
        assert!(!list_plugins(root.path()).unwrap()[0].enabled);
    }

    /// 包名里的换行不能拼出额外的 YAML 条目。
    #[test]
    fn a_newline_in_a_plugin_name_cannot_inject_patch_entries() {
        let root = tempdir().unwrap();
        write_profile(root.path(), r#"{"dependencies":{}}"#, "");

        disable_plugins(root.path(), &["evil\ndisabled: false".to_owned()]).unwrap();
        let patch =
            fs::read_to_string(root.path().join("profiles/desktop/cordis.patch.yml")).unwrap();
        assert_eq!(
            patch
                .lines()
                .filter(|line| line.starts_with("- id:"))
                .count(),
            1
        );
        assert!(!patch.contains("\ndisabled: false"));
    }

    #[test]
    fn safe_mode_disables_all_third_party_plugins() {
        let root = tempdir().unwrap();
        write_profile(
            root.path(),
            r#"{"dependencies":{"a-plugin":"1","@deepseek-ai/core":"2","b-plugin":"3"}}"#,
            "",
        );

        assert_eq!(safe_mode(root.path()).unwrap(), 2);
        let plugins = list_plugins(root.path()).unwrap();
        assert!(plugins.iter().all(|plugin| !plugin.enabled));
    }
}
