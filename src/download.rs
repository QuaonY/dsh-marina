//! Downloading and installing DSH desktop runtimes.
//!
//! The official DSH feed is an `electron-updater` generic provider.  Nightly
//! builds are described by `nightly.yml`; stable builds by `latest.yml`.

use crate::runtimes::{DSH_EXECUTABLE_NAME, RuntimeInfo, RuntimeRegistry, RuntimeSource};
use base64::Engine;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;

pub const OFFICIAL_FEED_URL: &str =
    "https://download.deepseek.com/dsh-desk/feeds/win-x64/nightly.yml";
pub const OFFICIAL_STABLE_FEED_URL: &str =
    "https://download.deepseek.com/dsh-desk/feeds/win-x64/latest.yml";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadSource {
    #[default]
    Official,
    GitHub,
    Mirror,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedInfo {
    pub version: String,
    pub url: String,
    pub sha512: String,
    pub size: u64,
    pub release_date: String,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("网络请求失败：{0}")]
    Request(#[from] reqwest::Error),
    #[error("下载源里没有 {0}")]
    MissingField(&'static str),
    #[error("下载已取消")]
    Cancelled,
    #[error("下载不完整：收到 {downloaded} / {total} 字节")]
    Incomplete { downloaded: u64, total: u64 },
    #[error("下载失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("校验失败：下载的文件和校验值不一致")]
    ChecksumMismatch,
    #[error("下载源里的校验值格式不对")]
    InvalidChecksum,
    #[error("解包 DSH 安装包失败：{0}")]
    InstallerFailed(String),
    #[error("DSH 安装包没有生成 {0}")]
    MissingInstalledExecutable(PathBuf),
    #[error("运行时信息有误：{0}")]
    Runtime(#[from] crate::runtimes::RuntimeError),
    #[error("下载 {url} 时服务器返回 HTTP {status}")]
    HttpStatus { status: u16, url: String },
    #[error("下载源不是合法的 YAML：{0}")]
    InvalidFeed(String),
    #[error("{0} 还没有配置")]
    SourceNotConfigured(&'static str),
    #[error("所有下载源都没有响应：{0}")]
    AllSourcesFailed(String),
    #[error("DSH {0} 已经安装过了")]
    AlreadyInstalled(String),
    #[error("下载源给出的版本号不合法：{0}")]
    InvalidVersion(String),
    #[error("下载地址不是 https，已拒绝：{0}")]
    InsecureSource(String),
}

/// Interrupted downloads left in `download/` that can be resumed (§7.10).
pub fn pending_partial_downloads(registry: &RuntimeRegistry) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = fs::read_dir(registry.download_dir()) else {
        return Vec::new();
    };
    let mut partial = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "part")
        })
        .filter_map(|entry| {
            let size = entry.metadata().ok()?.len();
            Some((entry.path(), size))
        })
        .collect::<Vec<_>>();
    partial.sort();
    partial
}

/// GitHub releases mirror of the desktop app (design §FR-9, V16).  The
/// `latest/download/<file>` form redirects to the newest release asset.
pub const GITHUB_RELEASES_PAGE: &str = "https://github.com/deepseek-ai/DeepSeek-Harness/releases";
pub const GITHUB_RELEASES_BASE: &str =
    "https://github.com/deepseek-ai/DeepSeek-Harness/releases/latest/download/";

impl DownloadSource {
    pub const ALL: [DownloadSource; 4] = [
        DownloadSource::Official,
        DownloadSource::GitHub,
        DownloadSource::Mirror,
        DownloadSource::Custom,
    ];

    /// Stable identifier used by `marina.toml` and the UI.
    pub fn key(self) -> &'static str {
        match self {
            DownloadSource::Official => "official",
            DownloadSource::GitHub => "github",
            DownloadSource::Mirror => "mirror",
            DownloadSource::Custom => "custom",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.key() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            DownloadSource::Official => "官方源",
            DownloadSource::GitHub => "GitHub Releases",
            DownloadSource::Mirror => "GitHub 镜像",
            DownloadSource::Custom => "自定义源",
        }
    }
}

/// Where to look for runtime metadata.  The preferred source is tried first;
/// on failure the remaining configured sources are tried in design order
/// (official → GitHub → mirror → custom).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedOptions {
    pub preferred: DownloadSource,
    pub stable: bool,
    pub mirror_url: String,
    pub custom_url: String,
}

impl Default for FeedOptions {
    fn default() -> Self {
        Self {
            preferred: DownloadSource::Official,
            stable: false,
            mirror_url: String::new(),
            custom_url: String::new(),
        }
    }
}

impl FeedOptions {
    /// Metadata URL for one source, or `None` when the source is not configured.
    pub fn feed_url(&self, source: DownloadSource) -> Option<String> {
        let file = if self.stable {
            "latest.yml"
        } else {
            "nightly.yml"
        };
        match source {
            DownloadSource::Official => Some(
                if self.stable {
                    OFFICIAL_STABLE_FEED_URL
                } else {
                    OFFICIAL_FEED_URL
                }
                .to_owned(),
            ),
            // GitHub releases publish electron-builder's `latest.yml`.
            // Version list from GitHub; installers come from the official CDN.
            DownloadSource::GitHub => Some(GITHUB_RELEASES_PAGE.to_owned()),
            DownloadSource::Mirror => {
                let prefix = self.mirror_url.trim();
                (!prefix.is_empty())
                    .then(|| format!("{}{GITHUB_RELEASES_BASE}latest.yml", with_slash(prefix)))
            }
            DownloadSource::Custom => {
                let base = self.custom_url.trim();
                if base.is_empty() {
                    None
                } else if base.ends_with(".yml") || base.ends_with(".yaml") {
                    Some(base.to_owned())
                } else {
                    Some(format!("{}{file}", with_slash(base)))
                }
            }
        }
    }

    /// Sources to try, preferred first, without duplicates or unconfigured entries.
    pub fn attempt_order(&self) -> Vec<(DownloadSource, String)> {
        let mut order = vec![self.preferred];
        order.extend(
            DownloadSource::ALL
                .into_iter()
                .filter(|source| *source != self.preferred),
        );
        order
            .into_iter()
            .filter_map(|source| self.feed_url(source).map(|url| (source, url)))
            .collect()
    }
}

fn with_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_owned()
    } else {
        format!("{value}/")
    }
}

/// Result of a feed lookup, including which source answered.
#[derive(Debug, Clone)]
pub struct FeedLookup {
    pub feed: FeedInfo,
    pub source: DownloadSource,
    /// Human-readable failures of the sources tried before `source`.
    pub failures: Vec<String>,
}

/// Fetch metadata from one source.
pub fn fetch_feed(source: DownloadSource, stable: bool) -> Result<FeedInfo, DownloadError> {
    let options = FeedOptions {
        preferred: source,
        stable,
        ..FeedOptions::default()
    };
    let url = options
        .feed_url(source)
        .ok_or(DownloadError::SourceNotConfigured(source.label()))?;
    fetch_feed_url(&url, source, &options)
}

/// Try every configured source in order until one returns usable metadata.
pub fn fetch_feed_with_fallback(options: &FeedOptions) -> Result<FeedLookup, DownloadError> {
    let mut failures = Vec::new();
    for (source, url) in options.attempt_order() {
        // The stable channel may not be published yet; the newest build is.
        let result = fetch_feed_url(&url, source, options).or_else(|error| {
            if url == OFFICIAL_STABLE_FEED_URL {
                fetch_feed_url(OFFICIAL_FEED_URL, source, options)
            } else {
                Err(error)
            }
        });
        match result {
            Ok(feed) => {
                return Ok(FeedLookup {
                    feed,
                    source,
                    failures,
                });
            }
            Err(error) => failures.push(format!("{}：{}", source.label(), short_error(&error))),
        }
    }
    Err(DownloadError::AllSourcesFailed(failures.join("；")))
}

fn fetch_feed_url(
    url: &str,
    source: DownloadSource,
    options: &FeedOptions,
) -> Result<FeedInfo, DownloadError> {
    let client = crate::net::builder(Duration::from_secs(20)).build()?;
    let text = client.get(url).send()?.error_for_status()?.text()?;
    let mut feed = parse_feed(&text)?;
    feed.url = resolve_installer_url(url, &feed.url, source, options);
    Ok(feed)
}

/// Make the installer URL absolute.  electron-builder feeds usually list
/// file names relative to the feed directory; mirrored GitHub URLs keep the
/// mirror prefix.
pub fn resolve_installer_url(
    feed_url: &str,
    installer: &str,
    source: DownloadSource,
    options: &FeedOptions,
) -> String {
    let absolute = installer.starts_with("https://") || installer.starts_with("http://");
    if absolute {
        if source == DownloadSource::Mirror
            && installer.starts_with("https://github.com/")
            && !options.mirror_url.trim().is_empty()
        {
            return format!("{}{installer}", with_slash(options.mirror_url.trim()));
        }
        return installer.to_owned();
    }
    let base = feed_url
        .rfind('/')
        .map(|index| &feed_url[..=index])
        .unwrap_or(feed_url);
    format!("{base}{}", installer.trim_start_matches("./"))
}

#[derive(Debug, Default, Deserialize)]
struct FeedFile {
    url: Option<String>,
    sha512: Option<String>,
    size: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeedDocument {
    version: Option<String>,
    files: Vec<FeedFile>,
    path: Option<String>,
    sha512: Option<String>,
    #[serde(rename = "releaseDate")]
    release_date: Option<String>,
}

/// Parse the subset of `electron-updater` metadata Marina needs.  The first
/// `.exe` entry in `files` wins; top-level `path`/`sha512` are the fallback.
pub fn parse_feed(text: &str) -> Result<FeedInfo, DownloadError> {
    let document: FeedDocument = serde_yaml_ng::from_str(text)
        .map_err(|error| DownloadError::InvalidFeed(error.to_string()))?;
    let version = document
        .version
        .filter(|version| !version.trim().is_empty())
        .ok_or(DownloadError::MissingField("version"))?;
    if !crate::runtimes::is_safe_version(version.trim()) {
        return Err(DownloadError::InvalidVersion(version.trim().to_owned()));
    }
    let file = document.files.iter().find(|file| {
        file.url
            .as_deref()
            .is_some_and(|url| url.to_ascii_lowercase().ends_with(".exe"))
    });
    let url = file
        .and_then(|file| file.url.clone())
        .or_else(|| document.path.clone())
        .filter(|url| !url.trim().is_empty())
        .ok_or(DownloadError::MissingField("url"))?;
    let sha512 = file
        .and_then(|file| file.sha512.clone())
        .or(document.sha512)
        .map(|value| value.split_whitespace().collect::<String>())
        .filter(|value| !value.is_empty())
        .ok_or(DownloadError::MissingField("sha512"))?;
    let size = file
        .and_then(|file| file.size)
        .ok_or(DownloadError::MissingField("size"))?;
    Ok(FeedInfo {
        version: version.trim().to_owned(),
        url: url.trim().to_owned(),
        sha512,
        size,
        release_date: document.release_date.unwrap_or_default(),
    })
}

pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: u64,
}

/// Download a file, resuming from an existing `.part` file when possible.
pub fn download_with_resume(
    url: &str,
    destination: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(DownloadProgress),
) -> Result<(), DownloadError> {
    let partial = destination.with_extension(format!(
        "{}.part",
        destination
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("download")
    ));
    if let Some(parent) = partial.parent() {
        fs::create_dir_all(parent)?;
    }
    // A `.part` larger than the remote file makes the server answer 416; drop
    // it and start over once instead of failing every later attempt.
    let mut retried = false;
    loop {
        let existing = fs::metadata(&partial)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let client = crate::net::builder(Duration::from_secs(600)).build()?;
        let mut request = client.get(url);
        if existing > 0 {
            request = request.header(RANGE, format!("bytes={existing}-"));
        }
        let response = request.send()?;
        let status = response.status();
        if status.as_u16() == 416 && existing > 0 && !retried {
            drop(response);
            fs::remove_file(&partial)?;
            retried = true;
            continue;
        }
        if !status.is_success() {
            return Err(DownloadError::HttpStatus {
                status: status.as_u16(),
                url: url.to_owned(),
            });
        }
        let resuming = existing > 0 && status.as_u16() == 206;
        let downloaded_before = if resuming { existing } else { 0 };
        let total = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|length| length + downloaded_before)
            .unwrap_or(0);

        let mut file = if resuming {
            let mut file = OpenOptions::new().append(true).open(&partial)?;
            file.seek(SeekFrom::End(0))?;
            file
        } else {
            File::create(&partial)?
        };
        let mut reader = response;
        let mut buffer = [0u8; 64 * 1024];
        let mut downloaded = downloaded_before;
        progress(DownloadProgress { downloaded, total });
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DownloadError::Cancelled);
            }
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            downloaded = downloaded.saturating_add(read as u64);
            progress(DownloadProgress { downloaded, total });
        }
        file.flush()?;
        file.sync_all()?;
        if total > 0 && downloaded < total {
            return Err(DownloadError::Incomplete { downloaded, total });
        }
        fs::rename(&partial, destination)?;
        return Ok(());
    }
}

pub fn verify_sha512(path: &Path, expected_base64: &str) -> Result<(), DownloadError> {
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected_base64.trim())
        .map_err(|_| DownloadError::InvalidChecksum)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if hasher.finalize().as_slice() == expected {
        Ok(())
    } else {
        Err(DownloadError::ChecksumMismatch)
    }
}

/// Install a DSH runtime from the electron-builder NSIS installer into
/// `target`.
///
/// The installer is not run: it shares its app id with the user's own DSH,
/// so silently it would uninstall that copy first, register itself under
/// Apps & features and create shortcuts — and it exits with code 2 when the
/// user's DSH is running.  The application files are a 7z archive appended
/// to the installer; Windows' own `tar.exe` (libarchive, Windows 10 1803+)
/// unpacks it.  The installer is never run, so the user's DSH stays as it is.
pub fn install_nsis(installer: &Path, target: &Path) -> Result<(), DownloadError> {
    // `extract_nsis_payload` removes an existing `target`: never let a
    // version string that climbed out of `runtimes/` get this far.
    if target.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        return Err(DownloadError::InstallerFailed(
            "安装目标路径不合法".to_owned(),
        ));
    }
    extract_nsis_payload(installer, target).map_err(DownloadError::InstallerFailed)
}

const SEVEN_ZIP_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// Offset of the first 7z archive inside `file`.
pub fn find_7z_payload(file: &Path) -> std::io::Result<Option<u64>> {
    let mut reader = File::open(file)?;
    let mut buffer = vec![0u8; 4 * 1024 * 1024];
    let mut base = 0u64;
    let mut carry = 0usize;
    loop {
        let read = reader.read(&mut buffer[carry..])?;
        if read == 0 {
            return Ok(None);
        }
        let filled = carry + read;
        if let Some(at) = buffer[..filled]
            .windows(SEVEN_ZIP_SIGNATURE.len())
            .position(|w| w == SEVEN_ZIP_SIGNATURE)
        {
            return Ok(Some(base + at as u64));
        }
        // Keep the last bytes so a signature across two reads is found.
        carry = (SEVEN_ZIP_SIGNATURE.len() - 1).min(filled);
        buffer.copy_within(filled - carry..filled, 0);
        base += (filled - carry) as u64;
    }
}

fn extract_nsis_payload(installer: &Path, target: &Path) -> Result<(), String> {
    let offset = find_7z_payload(installer)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "安装包里没有找到程序文件".to_owned())?;
    let parent = target.parent().ok_or_else(|| "目标目录无效".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let work = tempfile::Builder::new()
        .prefix(".extract-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    let archive = work.path().join("payload.7z");
    {
        let mut source = File::open(installer).map_err(|e| e.to_string())?;
        source
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut out = File::create(&archive).map_err(|e| e.to_string())?;
        std::io::copy(&mut source, &mut out).map_err(|e| e.to_string())?;
    }
    let unpacked = work.path().join("app");
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let tar = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("tar.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("tar.exe"));
    let mut command = Command::new(tar);
    command.arg("-xf").arg(&archive).arg("-C").arg(&unpacked);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command
        .output()
        .map_err(|e| format!("无法运行 tar.exe：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "tar.exe 解包失败：{}",
            stderr.lines().last().unwrap_or_default()
        ));
    }
    if !unpacked.join(DSH_EXECUTABLE_NAME).is_file()
        || !unpacked.join("resources").join("app.asar").is_file()
    {
        return Err("解出来的文件不完整".to_owned());
    }
    let _ = fs::remove_file(&archive);
    if target.exists() {
        fs::remove_dir_all(target).map_err(|e| e.to_string())?;
    }
    fs::rename(&unpacked, target).map_err(|e| e.to_string())?;
    Ok(())
}

/// Installer files placed directly in `runtimes/` for offline installation
/// (design §FR-9).  Directories that already contain the executable are
/// picked up by [`RuntimeRegistry::scan`] without any work.
pub fn offline_installers(registry: &RuntimeRegistry) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(registry.runtimes_dir()) else {
        return Vec::new();
    };
    let mut installers = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        })
        .collect::<Vec<_>>();
    installers.sort();
    installers
}

/// Install one offline installer: run it into a staging directory, read the
/// version from the installed `app.asar`, then move it to `runtimes/<version>`.
/// The installer is moved to `download/` afterwards so it is not run twice.
pub fn install_offline(
    installer: &Path,
    registry: &RuntimeRegistry,
) -> Result<RuntimeInfo, DownloadError> {
    let runtimes = registry.runtimes_dir();
    let staging = runtimes.join(format!(
        ".offline-{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S%3f")
    ));
    let result = (|| {
        install_nsis(installer, &staging)?;
        let executable = staging.join(DSH_EXECUTABLE_NAME);
        let version = crate::runtimes::read_dsh_version(&executable)
            .ok_or(DownloadError::MissingField("version"))?;
        if !crate::runtimes::is_safe_version(&version) {
            return Err(DownloadError::InvalidVersion(version));
        }
        let target = runtimes.join(&version);
        if target.exists() {
            return Err(DownloadError::AlreadyInstalled(version));
        }
        fs::rename(&staging, &target)?;
        registry.mark_installed_from(&version, "offline")?;
        Ok(version)
    })();
    match result {
        Ok(version) => {
            let archive_dir = registry.download_dir();
            fs::create_dir_all(&archive_dir)?;
            if let Some(name) = installer.file_name() {
                let _ = fs::rename(installer, archive_dir.join(name));
            }
            registry
                .scan()?
                .into_iter()
                .find(|runtime| runtime.version == version)
                .ok_or(DownloadError::MissingField("runtime"))
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

/// One version a source offers.  `feed` is known when the listing already
/// read the version's metadata; otherwise `feed_url` is read on download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVersion {
    pub version: String,
    pub prerelease: bool,
    pub release_date: String,
    pub feed_url: String,
    pub feed: Option<FeedInfo>,
}

/// Every version offered by `options.preferred` (falling back to the other
/// configured sources), newest first, and whether only prereleases exist
/// although stable ones were asked for.
///
/// The official server only publishes the newest build (`nightly.yml`; the
/// stable `latest.yml` may not exist yet).  GitHub lists every release but
/// attaches no installers: those live on the official CDN under a fixed
/// name, so both sources combine the GitHub list with the CDN download.
pub fn list_versions(
    options: &FeedOptions,
) -> Result<(DownloadSource, Vec<RemoteVersion>, bool), DownloadError> {
    let _ = take_list_note();
    let mut failures = Vec::new();
    for (source, _) in options.attempt_order() {
        match list_source(source, options) {
            Ok(mut versions) if !versions.is_empty() => {
                versions.sort_by(|a, b| crate::runtimes::compare_versions(&b.version, &a.version));
                versions.dedup_by(|a, b| a.version == b.version);
                let only_prerelease = options.stable && versions.iter().all(|v| v.prerelease);
                if options.stable && !only_prerelease {
                    versions.retain(|v| !v.prerelease);
                }
                return Ok((source, versions, only_prerelease));
            }
            Ok(_) => failures.push(format!("{}：没有可用版本", source.label())),
            Err(error) => failures.push(format!("{}：{}", source.label(), short_error(&error))),
        }
    }
    Err(DownloadError::AllSourcesFailed(failures.join("；")))
}

fn short_error(error: &DownloadError) -> String {
    match error {
        DownloadError::Request(e) if e.is_timeout() || e.is_connect() => "连不上服务器".to_owned(),
        DownloadError::Request(e) => match e.status() {
            Some(status) => format!("服务器返回 {}", status.as_u16()),
            None => "网络请求失败".to_owned(),
        },
        DownloadError::HttpStatus { status, .. } => format!("服务器返回 {status}"),
        other => other.to_string(),
    }
}

/// Installer of `version` on the official CDN.
pub fn official_installer_url(version: &str) -> String {
    format!("{OFFICIAL_BIN_BASE}deepseek-harness-{version}-win-x64.exe")
}

pub const OFFICIAL_BIN_BASE: &str = "https://download.deepseek.com/dsh-desk/bin/win-x64/";

static LAST_LIST_NOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Why the last [`list_versions`] is incomplete, if it is.
pub fn take_list_note() -> Option<String> {
    LAST_LIST_NOTE.lock().ok().and_then(|mut note| note.take())
}

fn list_source(
    source: DownloadSource,
    options: &FeedOptions,
) -> Result<Vec<RemoteVersion>, DownloadError> {
    let from_feed = |url: String| -> Result<RemoteVersion, DownloadError> {
        let feed = fetch_feed_url(&url, source, options)?;
        Ok(RemoteVersion {
            version: feed.version.clone(),
            prerelease: feed.version.contains('-'),
            release_date: feed.release_date.clone(),
            feed_url: url,
            feed: Some(feed),
        })
    };
    match source {
        DownloadSource::Official | DownloadSource::GitHub => {
            // The channel feeds carry checksums; GitHub adds older versions.
            let mut versions: Vec<RemoteVersion> = [OFFICIAL_STABLE_FEED_URL, OFFICIAL_FEED_URL]
                .into_iter()
                .filter_map(|url| from_feed(url.to_owned()).ok())
                .collect();
            let github = fetch_github_releases();
            match github {
                Ok(releases) => {
                    for release in releases {
                        if !versions.iter().any(|v| v.version == release.version) {
                            versions.push(release);
                        }
                    }
                }
                Err(error) if versions.is_empty() => return Err(error),
                Err(error) => {
                    if let Ok(mut note) = LAST_LIST_NOTE.lock() {
                        *note = Some(format!(
                            "GitHub 版本列表读取失败（{}），只列出了官方最新版",
                            short_error(&error)
                        ));
                    }
                }
            }
            Ok(versions)
        }
        DownloadSource::Mirror | DownloadSource::Custom => {
            let url = options
                .feed_url(source)
                .ok_or(DownloadError::SourceNotConfigured(source.label()))?;
            Ok(vec![from_feed(url)?])
        }
    }
}

fn fetch_github_releases() -> Result<Vec<RemoteVersion>, DownloadError> {
    let text = crate::net::builder(Duration::from_secs(40))
        .build()?
        .get(GITHUB_RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?
        .text()?;
    parse_github_releases(&text)
}

pub const GITHUB_RELEASES_API: &str =
    "https://api.github.com/repos/deepseek-ai/DeepSeek-Harness/releases?per_page=30";

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// DSH releases from the GitHub API.  Tags look like `dsh-v0.2.2-rc.1`; a
/// release with its own `latest.yml` uses it, otherwise the installer is
/// read from the official CDN.
pub fn parse_github_releases(text: &str) -> Result<Vec<RemoteVersion>, DownloadError> {
    let releases: Vec<GitHubRelease> = serde_json::from_str(text)
        .map_err(|error| DownloadError::InvalidFeed(error.to_string()))?;
    Ok(releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let tag = release.tag_name.trim();
            let version = tag
                .strip_prefix("dsh-")
                .unwrap_or(tag)
                .trim_start_matches('v')
                .to_owned();
            if !version.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            let feed_url = release
                .assets
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case("latest.yml"))
                .map(|a| a.browser_download_url.clone())
                .unwrap_or_default();
            Some(RemoteVersion {
                prerelease: release.prerelease || version.contains('-'),
                release_date: release.published_at.unwrap_or_default(),
                feed_url,
                feed: None,
                version,
            })
        })
        .collect())
}

/// Metadata of `version`, reading its feed when the listing did not.  A
/// release without a feed downloads the official installer; its size comes
/// from the server and there is no checksum to verify.
pub fn resolve_version(
    version: &RemoteVersion,
    options: &FeedOptions,
) -> Result<FeedInfo, DownloadError> {
    if let Some(feed) = &version.feed {
        return Ok(feed.clone());
    }
    if !version.feed_url.is_empty() {
        let source = if version.feed_url.starts_with("https://github.com/") {
            DownloadSource::GitHub
        } else {
            DownloadSource::Custom
        };
        return fetch_feed_url(&version.feed_url, source, options);
    }
    let url = official_installer_url(&version.version);
    let response = crate::net::builder(Duration::from_secs(20))
        .build()?
        .head(&url)
        .send()?;
    if !response.status().is_success() {
        return Err(DownloadError::HttpStatus {
            status: response.status().as_u16(),
            url,
        });
    }
    let size = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Ok(FeedInfo {
        version: version.version.clone(),
        url,
        sha512: String::new(),
        size,
        release_date: version.release_date.clone(),
    })
}
pub fn download_and_install(
    feed: &FeedInfo,
    registry: &RuntimeRegistry,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(DownloadProgress),
) -> Result<RuntimeInfo, DownloadError> {
    if !crate::runtimes::is_safe_version(&feed.version) {
        return Err(DownloadError::InvalidVersion(feed.version.clone()));
    }
    if !feed.url.starts_with("https://") {
        return Err(DownloadError::InsecureSource(feed.url.clone()));
    }
    let download_dir = registry.download_dir();
    fs::create_dir_all(&download_dir)?;
    let installer = download_dir.join(format!("dsh-{}.exe", feed.version));
    if !installer.is_file() {
        download_with_resume(&feed.url, &installer, cancel, progress)?;
    }
    // Versions listed from GitHub have no published checksum; the complete
    // HTTPS download from the official server is all there is to check.
    if !feed.sha512.is_empty() {
        verify_sha512(&installer, &feed.sha512)?;
    }
    let target = registry.runtimes_dir().join(&feed.version);
    install_nsis(&installer, &target)?;
    registry.mark_installed(&feed.version)?;
    let executable = target.join(DSH_EXECUTABLE_NAME);
    Ok(RuntimeInfo {
        id: feed.version.clone(),
        version: feed.version.clone(),
        executable,
        source: RuntimeSource::Managed,
        installed_at: Some(chrono::Utc::now().to_rfc3339()),
        size_bytes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_7z_payload_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("setup.exe");
        let mut bytes = vec![0u8; 4 * 1024 * 1024 - 3];
        bytes.extend_from_slice(&SEVEN_ZIP_SIGNATURE);
        bytes.extend_from_slice(b"rest");
        fs::write(&path, &bytes).unwrap();
        assert_eq!(find_7z_payload(&path).unwrap(), Some(4 * 1024 * 1024 - 3));
        fs::write(&path, b"MZ no archive here").unwrap();
        assert_eq!(find_7z_payload(&path).unwrap(), None);
    }

    /// A real installer named by `DSH_TEST_INSTALLER`:
    /// `cargo test -- --ignored live_extract`.
    #[test]
    #[ignore]
    fn live_extracts_a_real_installer() {
        let installer = PathBuf::from(std::env::var("DSH_TEST_INSTALLER").unwrap());
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("runtime");
        install_nsis(&installer, &target).unwrap();
        let version = crate::runtimes::read_dsh_version(&target.join(DSH_EXECUTABLE_NAME)).unwrap();
        println!("extracted DSH {version}");
    }

    /// Network check against the real servers: `cargo test -- --ignored live`.
    #[test]
    #[ignore]
    fn live_lists_official_and_github_versions() {
        for (preferred, stable) in [
            (DownloadSource::Official, true),
            (DownloadSource::GitHub, false),
        ] {
            let options = FeedOptions {
                preferred,
                stable,
                ..FeedOptions::default()
            };
            let (source, versions, only_pre) = list_versions(&options).unwrap();
            println!(
                "{preferred:?} stable={stable} -> {source:?} {} versions, only_pre={only_pre}",
                versions.len()
            );
            for v in versions.iter().take(4) {
                println!(
                    "  {} pre={} feed={}",
                    v.version,
                    v.prerelease,
                    v.feed.is_some()
                );
            }
            assert!(!versions.is_empty());
            let older = versions.iter().find(|v| v.feed.is_none()).unwrap();
            let feed = resolve_version(older, &options).unwrap();
            println!(
                "  resolved {} -> {} ({} bytes)",
                feed.version, feed.url, feed.size
            );
            assert!(feed.size > 1_000_000);
        }
    }

    #[test]
    fn lists_github_releases_with_a_feed() {
        let text = r#"[
            {"tag_name":"v0.3.0","prerelease":false,"published_at":"2026-10-01T00:00:00Z",
             "assets":[{"name":"latest.yml","browser_download_url":"https://github.com/o/r/releases/download/v0.3.0/latest.yml"}]},
            {"tag_name":"v0.3.1-rc.1","prerelease":true,
             "assets":[{"name":"latest.yml","browser_download_url":"https://github.com/o/r/releases/download/v0.3.1-rc.1/latest.yml"}]},
            {"tag_name":"v0.2.9","draft":true,"assets":[{"name":"latest.yml","browser_download_url":"x"}]},
            {"tag_name":"dsh-v0.2.8","prerelease":true,"assets":[]},
            {"tag_name":"nightly","assets":[]}
        ]"#;
        let versions = parse_github_releases(text).unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[2].version, "0.2.8");
        assert!(versions[2].feed_url.is_empty());
        assert_eq!(
            official_installer_url("0.2.8"),
            "https://download.deepseek.com/dsh-desk/bin/win-x64/deepseek-harness-0.2.8-win-x64.exe"
        );
        assert_eq!(versions[0].version, "0.3.0");
        assert!(!versions[0].prerelease);
        assert!(versions[0].feed_url.ends_with("v0.3.0/latest.yml"));
        assert!(versions[1].prerelease);
        assert!(versions.iter().all(|v| v.feed.is_none()));
        assert!(parse_github_releases("{}").is_err());
    }

    #[test]
    fn finds_offline_installers_but_not_directories() {
        let root = tempfile::tempdir().unwrap();
        let registry = RuntimeRegistry::new(root.path());
        assert!(offline_installers(&registry).is_empty());
        let runtimes = registry.runtimes_dir();
        fs::create_dir_all(runtimes.join("0.2.0")).unwrap();
        fs::write(runtimes.join("deepseek-harness-0.2.1-win-x64.EXE"), b"x").unwrap();
        fs::write(runtimes.join("notes.txt"), b"x").unwrap();
        let found = offline_installers(&registry);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("deepseek-harness-0.2.1-win-x64.EXE"));
    }

    #[test]
    fn parses_official_nightly_feed() {
        let feed = parse_feed(
            r#"
version: 0.2.1-rc.5
files:
  - url: >-
      https://download.deepseek.com/dsh-desk/bin/win-x64/deepseek-harness-0.2.1-rc.5-win-x64.exe
    sha512: >-
      gu2dH+aTr678c0P8DGAH/h4B1GaWfQIjpsPrLjl/ukLrQIh6DLVq/MhX4FZvJ8o1GCMocOhi x6XbLa6KmtjXXg==
    size: 289408544
path: >-
  https://download.deepseek.com/dsh-desk/bin/win-x64/deepseek-harness-0.2.1-rc.5-win-x64.exe
sha512: >-
  gu2dH+aTr678c0P8DGAH/h4B1GaWfQIjpsPrLjl/ukLrQIh6DLVq/MhX4FZvJ8o1GCMocOhi x6XbLa6KmtjXXg==
releaseDate: '2026-10-29T14:04:01.198Z'
"#,
        )
        .unwrap();
        assert_eq!(feed.version, "0.2.1-rc.5");
        assert_eq!(feed.size, 289408544);
        assert!(feed.url.ends_with(".exe"));
        assert!(feed.sha512.ends_with("=="));
        assert_eq!(feed.release_date, "2026-10-29T14:04:01.198Z");
    }

    #[test]
    fn reports_missing_feed_fields() {
        assert!(matches!(
            parse_feed("version: 1.0.0\n"),
            Err(DownloadError::MissingField("url"))
        ));
    }

    #[test]
    fn parses_relative_electron_builder_feed_and_resolves_installer_url() {
        let feed = parse_feed(
            "version: 0.3.0\nfiles:\n  - url: DeepSeek-Harness-Setup-0.3.0.exe\n    sha512: abc==\n    size: 42\n\
             path: DeepSeek-Harness-Setup-0.3.0.exe\nsha512: abc==\nreleaseDate: '2026-11-01T00:00:00.000Z'\n",
        )
        .unwrap();
        assert_eq!(feed.url, "DeepSeek-Harness-Setup-0.3.0.exe");
        let options = FeedOptions::default();
        assert_eq!(
            resolve_installer_url(
                "https://example.invalid/feeds/win-x64/latest.yml",
                &feed.url,
                DownloadSource::Custom,
                &options
            ),
            "https://example.invalid/feeds/win-x64/DeepSeek-Harness-Setup-0.3.0.exe"
        );
    }

    #[test]
    fn mirror_prefixes_github_urls_only() {
        let options = FeedOptions {
            preferred: DownloadSource::Mirror,
            mirror_url: "https://mirror.invalid".to_owned(),
            ..FeedOptions::default()
        };
        assert_eq!(
            options.feed_url(DownloadSource::Mirror).unwrap(),
            format!("https://mirror.invalid/{GITHUB_RELEASES_BASE}latest.yml")
        );
        assert_eq!(
            resolve_installer_url(
                "https://mirror.invalid/x/latest.yml",
                "https://github.com/a/b.exe",
                DownloadSource::Mirror,
                &options
            ),
            "https://mirror.invalid/https://github.com/a/b.exe"
        );
        assert_eq!(
            resolve_installer_url(
                "https://x/latest.yml",
                "https://download.deepseek.com/a.exe",
                DownloadSource::Mirror,
                &options
            ),
            "https://download.deepseek.com/a.exe"
        );
    }

    #[test]
    fn attempt_order_starts_with_preferred_and_skips_unconfigured_sources() {
        let options = FeedOptions {
            preferred: DownloadSource::GitHub,
            ..FeedOptions::default()
        };
        let order = options
            .attempt_order()
            .into_iter()
            .map(|(source, _)| source)
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![DownloadSource::GitHub, DownloadSource::Official]
        );

        let options = FeedOptions {
            preferred: DownloadSource::Custom,
            stable: true,
            custom_url: "https://feed.invalid/dsh".to_owned(),
            mirror_url: "https://mirror.invalid/".to_owned(),
        };
        let order = options.attempt_order();
        assert_eq!(order[0].0, DownloadSource::Custom);
        assert_eq!(order[0].1, "https://feed.invalid/dsh/latest.yml");
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn source_keys_round_trip() {
        for source in DownloadSource::ALL {
            assert_eq!(DownloadSource::parse(source.key()), Some(source));
        }
        assert_eq!(DownloadSource::parse("ftp"), None);
    }
}
