//! Launcher-wide preferences stored in `marina.toml` (design §8.3).
//!
//! The file uses the sectioned layout from the design:
//!
//! ```toml
//! schema_version = 1
//! [download]
//! default_source = "official"
//! mirror_url = "https://ghproxy.com/"
//! custom_url = ""
//! max_runtimes = 3
//! stable_only = false
//! [lifecycle]
//! confirm_on_exit = true
//! auto_start_on_boot = false
//! close_to_tray = true
//! [ui]
//! minimize_to_tray_hint_shown = false
//! ```
//!
//! Older Marina builds wrote flat keys (`close_to_tray`, `auto_start`, ...).
//! Those are still read and are rewritten in the sectioned layout on save.

use crate::download::DownloadSource;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;
use thiserror::Error;

const SETTINGS_FILE: &str = "marina.toml";
const SCHEMA_VERSION: u32 = 1;

/// Default prefix for the GitHub download mirror.
pub const DEFAULT_MIRROR_URL: &str = "https://ghproxy.com/";
/// 社区整合包目录的默认地址。
pub const DEFAULT_PACK_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/QuaonY/dsh-marina-packs/master/index.json";
/// Default number of managed runtimes kept before Marina suggests cleanup.
pub const DEFAULT_MAX_RUNTIMES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarinaSettings {
    /// Whether the one-time "minimized to tray" hint has been shown.
    pub tray_hint_shown: bool,
    /// Register Marina in the current user's Run key.
    pub auto_start: bool,
    /// Ask before exiting while instances are running.  When disabled,
    /// exiting still stops every instance gracefully first.
    pub confirm_on_exit: bool,
    /// Warn "可能丢失未保存内容" before stopping or restarting an instance (§7.5).
    pub confirm_stop: bool,
    /// Preferred download source; other sources are tried when it fails.
    pub download_source: DownloadSource,
    /// Prefix placed in front of GitHub URLs for the mirror source.
    pub mirror_url: String,
    /// Base URL of a user-provided electron-updater generic feed.
    pub custom_url: String,
    /// Managed runtime count above which Marina suggests removing old ones.
    pub max_runtimes: u32,
    /// Use the stable feed (`latest.yml`) instead of nightly.
    pub stable_only: bool,
    /// Last time the download feed was checked, as an RFC 3339 timestamp.
    pub last_feed_check: Option<String>,
    /// Marina owns the `dsh://` protocol and asks which instance opens a link.
    pub handle_links: bool,
    /// The `dsh://` command registered before Marina took over, restored when
    /// the option is turned off.
    pub previous_link_command: Option<String>,
    /// Open links with this instance without asking.
    pub default_link_instance: Option<String>,
    /// Address of the shared 整合包 catalog (https:// or a local file).
    pub pack_catalog_url: String,
    /// Check GitHub for a new Marina release once per launch.
    pub auto_check_update: bool,
    /// Use the dark color theme.  Defaults to the light theme.
    pub dark_mode: bool,
}

impl Default for MarinaSettings {
    fn default() -> Self {
        Self {
            tray_hint_shown: false,
            auto_start: false,
            confirm_on_exit: true,
            confirm_stop: true,
            download_source: DownloadSource::Official,
            mirror_url: DEFAULT_MIRROR_URL.to_owned(),
            custom_url: String::new(),
            max_runtimes: DEFAULT_MAX_RUNTIMES,
            stable_only: false,
            last_feed_check: None,
            handle_links: false,
            previous_link_command: None,
            default_link_instance: None,
            pack_catalog_url: DEFAULT_PACK_CATALOG_URL.to_owned(),
            auto_check_update: true,
            dark_mode: false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct PacksSection {
    #[serde(skip_serializing_if = "String::is_empty")]
    catalog_url: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct LinksSection {
    handle_links: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_instance: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct DownloadSection {
    default_source: DownloadSource,
    mirror_url: String,
    custom_url: String,
    max_runtimes: u32,
    stable_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_feed_check: Option<String>,
}

impl Default for DownloadSection {
    fn default() -> Self {
        let defaults = MarinaSettings::default();
        Self {
            default_source: defaults.download_source,
            mirror_url: defaults.mirror_url,
            custom_url: defaults.custom_url,
            max_runtimes: defaults.max_runtimes,
            stable_only: false,
            last_feed_check: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct LifecycleSection {
    confirm_on_exit: bool,
    confirm_stop: bool,
    auto_start_on_boot: bool,
}

impl Default for LifecycleSection {
    fn default() -> Self {
        Self {
            confirm_on_exit: true,
            confirm_stop: true,
            auto_start_on_boot: false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct UiSection {
    minimize_to_tray_hint_shown: bool,
    /// Off only when the user turns the launch-time check off.
    #[serde(skip_serializing_if = "is_true")]
    auto_check_update: Option<bool>,
    /// On only when the user picks the dark theme; light is the default.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dark_mode: bool,
}

/// `skip_serializing_if` helper for options whose default is `true`.
fn is_true(value: &Option<bool>) -> bool {
    value.unwrap_or(true)
}

/// On-disk layout.  The flat `legacy_*` keys are only read, never written.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SettingsFile {
    schema_version: u32,
    download: DownloadSection,
    lifecycle: LifecycleSection,
    ui: UiSection,
    links: LinksSection,
    packs: PacksSection,
    #[serde(rename = "close_to_tray", skip_serializing)]
    legacy_close_to_tray: Option<bool>,
    #[serde(rename = "tray_hint_shown", skip_serializing)]
    legacy_tray_hint_shown: Option<bool>,
    #[serde(rename = "auto_start", skip_serializing)]
    legacy_auto_start: Option<bool>,
    #[serde(rename = "download_source", skip_serializing)]
    legacy_download_source: Option<DownloadSource>,
    #[serde(rename = "last_feed_check", skip_serializing)]
    legacy_last_feed_check: Option<String>,
}

impl SettingsFile {
    fn into_settings(self) -> MarinaSettings {
        let mirror_url = if self.download.mirror_url.trim().is_empty() {
            DEFAULT_MIRROR_URL.to_owned()
        } else {
            self.download.mirror_url
        };
        MarinaSettings {
            tray_hint_shown: self
                .legacy_tray_hint_shown
                .unwrap_or(self.ui.minimize_to_tray_hint_shown),
            auto_start: self
                .legacy_auto_start
                .unwrap_or(self.lifecycle.auto_start_on_boot),
            confirm_on_exit: self.lifecycle.confirm_on_exit,
            confirm_stop: self.lifecycle.confirm_stop,
            download_source: self
                .legacy_download_source
                .unwrap_or(self.download.default_source),
            mirror_url,
            custom_url: self.download.custom_url,
            max_runtimes: self.download.max_runtimes.max(1),
            stable_only: self.download.stable_only,
            last_feed_check: self
                .legacy_last_feed_check
                .or(self.download.last_feed_check),
            handle_links: self.links.handle_links,
            previous_link_command: self.links.previous_command,
            default_link_instance: self.links.default_instance,
            pack_catalog_url: if self.packs.catalog_url.trim().is_empty() {
                DEFAULT_PACK_CATALOG_URL.to_owned()
            } else {
                self.packs.catalog_url
            },
            auto_check_update: self.ui.auto_check_update.unwrap_or(true),
            dark_mode: self.ui.dark_mode,
        }
    }

    fn from_settings(settings: &MarinaSettings) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            download: DownloadSection {
                default_source: settings.download_source,
                mirror_url: settings.mirror_url.clone(),
                custom_url: settings.custom_url.clone(),
                max_runtimes: settings.max_runtimes,
                stable_only: settings.stable_only,
                last_feed_check: settings.last_feed_check.clone(),
            },
            lifecycle: LifecycleSection {
                confirm_on_exit: settings.confirm_on_exit,
                confirm_stop: settings.confirm_stop,
                auto_start_on_boot: settings.auto_start,
            },
            ui: UiSection {
                minimize_to_tray_hint_shown: settings.tray_hint_shown,
                auto_check_update: (!settings.auto_check_update).then_some(false),
                dark_mode: settings.dark_mode,
            },
            links: LinksSection {
                handle_links: settings.handle_links,
                previous_command: settings.previous_link_command.clone(),
                default_instance: settings.default_link_instance.clone(),
            },
            packs: PacksSection {
                catalog_url: settings.pack_catalog_url.clone(),
            },
            ..Self::default()
        }
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("读取启动器设置失败（{path}）：{source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("启动器设置不是合法的 TOML（{path}）：{source}")]
    Parse {
        path: PathBuf,
        #[source]
        // Boxed: toml::de::Error is large and would bloat every Result using this error.
        source: Box<toml::de::Error>,
    },
    #[error("写入启动器设置失败：{0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("保存启动器设置失败：{0}")]
    Persist(#[source] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            path: data_root.into().join(SETTINGS_FILE),
        }
    }

    pub fn load(&self) -> Result<MarinaSettings, SettingsError> {
        if !self.path.exists() {
            return Ok(MarinaSettings::default());
        }
        let text = fs::read_to_string(&self.path).map_err(|source| SettingsError::Read {
            path: self.path.clone(),
            source,
        })?;
        let file: SettingsFile = toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: self.path.clone(),
            source: Box::new(source),
        })?;
        Ok(file.into_settings())
    }

    pub fn save(&self, settings: &MarinaSettings) -> Result<(), SettingsError> {
        let text = toml::to_string_pretty(&SettingsFile::from_settings(settings))?;
        let parent = self.path.parent().ok_or_else(|| {
            SettingsError::Persist(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "设置文件路径没有上级目录",
            ))
        })?;
        fs::create_dir_all(parent).map_err(SettingsError::Persist)?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(SettingsError::Persist)?;
        temporary
            .write_all(text.as_bytes())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(SettingsError::Persist)?;
        temporary
            .persist(&self.path)
            .map(|_| ())
            .map_err(|error| SettingsError::Persist(error.error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_settings_use_documented_defaults() {
        let directory = tempdir().unwrap();
        let store = SettingsStore::new(directory.path().join("data"));
        let settings = store.load().unwrap();
        assert!(!settings.auto_start);
        assert!(settings.confirm_on_exit);
        assert_eq!(settings.download_source, DownloadSource::Official);
        assert_eq!(settings.mirror_url, DEFAULT_MIRROR_URL);
        assert_eq!(settings.max_runtimes, DEFAULT_MAX_RUNTIMES);
    }

    #[test]
    fn settings_round_trip_atomically() {
        let directory = tempdir().unwrap();
        let store = SettingsStore::new(directory.path().join("data"));
        let settings = MarinaSettings {
            tray_hint_shown: true,
            auto_start: true,
            confirm_on_exit: false,
            download_source: DownloadSource::Custom,
            custom_url: "https://example.invalid/feed/".to_owned(),
            last_feed_check: Some("2026-11-01T00:00:00Z".to_owned()),
            handle_links: true,
            previous_link_command: Some("\"C:\\DSH\\DeepSeek Harness.exe\" \"%1\"".to_owned()),
            default_link_instance: Some("work".to_owned()),
            pack_catalog_url: "https://example.com/packs/catalog.json".to_owned(),
            dark_mode: true,
            ..MarinaSettings::default()
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), settings);
    }

    #[test]
    fn writes_the_sectioned_design_layout() {
        let directory = tempdir().unwrap();
        let store = SettingsStore::new(directory.path());
        store.save(&MarinaSettings::default()).unwrap();
        let text = fs::read_to_string(directory.path().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("schema_version = 1"));
        assert!(text.contains("[download]"));
        assert!(text.contains("default_source = \"official\""));
        assert!(text.contains("[lifecycle]"));
        assert!(text.contains("confirm_on_exit = true"));
        assert!(text.contains("[ui]"));
        assert!(!text.contains("\nclose_to_tray = true\n[download]"));
    }

    #[test]
    fn reads_flat_settings_written_by_older_builds() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join(SETTINGS_FILE),
            "tray_hint_shown = true\nauto_start = true\n\
             download_source = \"official\"\nlast_feed_check = \"2026-10-30T00:00:00Z\"\n",
        )
        .unwrap();
        let settings = SettingsStore::new(directory.path()).load().unwrap();
        assert!(settings.tray_hint_shown);
        assert!(settings.auto_start);
        assert!(settings.confirm_on_exit);
        assert_eq!(
            settings.last_feed_check.as_deref(),
            Some("2026-10-30T00:00:00Z")
        );
    }
}
