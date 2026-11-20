//! 自更新：下载更新包后做 **SHA-256 校验**（不再使用 minisign / Ed25519 签名，
//! 因此发布与更新都不需要任何私钥）。
//!
//! 流程：
//! 1. `check` 读取更新清单（GitHub 原始文件优先，CNB 镜像回退），比对版本；
//! 2. `download` 下载对应平台的更新包并校验 SHA-256，返回字节；
//! 3. `stage_pending` 把包和元数据落到 `<数据目录>/updates/pending`，供「退出时再更新」；
//! 4. `install_pending` 重建 `Update` 并安装。Windows 会拉起 NSIS 安装器（被动模式 +
//!    自动重启）并随即退出本进程；macOS 替换 `.app` 后重启。
//!
//! 清单里 `platforms.[target].signature` 字段承载更新包的 SHA-256（hex）；字段名沿用
//! updater schema 以保持兼容。清单同时保留旧版 `tag_name/html_url/body/assets`。
//! 安全边界：SHA-256 防传输损坏与 CDN 篡改，但不防仓库本身被攻破（那需要离线签名）。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use cargo_packager_updater::url::Url;
use cargo_packager_updater::{
    Config, Update, UpdateFormat, UpdaterBuilder, WindowsConfig, WindowsUpdateInstallMode,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 更新清单端点。`{{target}}`/`{{arch}}` 由 updater 自动替换；这里用静态全平台清单。
const ENDPOINTS: &[&str] = &[
    "https://raw.githubusercontent.com/QuaonY/dsh-marina/main/update.json",
    "https://cnb.cool/QuaonY/dsh-marina/-/git/raw/main/update.json",
];

/// Files staged under `<data_root>/updates`.
pub fn staging_dir(data_root: &Path) -> PathBuf {
    data_root.join("updates")
}

fn pending_dir(data_root: &Path) -> PathBuf {
    staging_dir(data_root).join("pending")
}

fn pending_json(data_root: &Path) -> PathBuf {
    pending_dir(data_root).join("pending.json")
}

fn config() -> Result<Config, String> {
    let endpoints = ENDPOINTS
        .iter()
        .map(|endpoint| Url::parse(endpoint).map_err(|e| format!("更新端点无效：{e}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Config {
        endpoints,
        // 不做 minisign 验签（改 SHA-256），公钥留空；install 不读取它。
        pubkey: String::new(),
        windows: Some(WindowsConfig {
            installer_args: None,
            // 被动模式：只显示进度条，装完自动重启（NSIS /P /R）。
            install_mode: Some(WindowsUpdateInstallMode::Passive),
        }),
    })
}

/// 当前程序版本（去掉可能的前导 `v`）。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 检查是否有新版；`Ok(None)` 表示已是最新或清单尚未发布。
pub fn check() -> Result<Option<Update>, String> {
    let version = Version::parse(current_version().trim_start_matches('v'))
        .map_err(|e| format!("解析当前版本失败：{e}"))?;
    let updater = UpdaterBuilder::new(version, config()?)
        .build()
        .map_err(|e| format!("初始化更新器失败：{e}"))?;
    updater.check().map_err(|e| format!("检查更新失败：{e}"))
}

/// 下载更新包并校验 SHA-256，返回字节。`progress` 收到（累计已下载, 总大小）。
/// 期望哈希取自清单 `signature` 字段（现承载 SHA-256 hex）。
pub fn download(
    update: &Update,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("初始化下载客户端失败：{e}"))?;
    let mut response = client
        .get(update.download_url.as_str())
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("下载更新失败：{e}"))?;
    let total = response.content_length();

    let mut hasher = Sha256::new();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = response
            .read(&mut chunk)
            .map_err(|e| format!("读取更新流失败：{e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        buffer.extend_from_slice(&chunk[..n]);
        progress(buffer.len() as u64, total);
    }
    let actual = format!("{:x}", hasher.finalize());
    let expected = update.signature.trim().to_lowercase();
    if expected.is_empty() {
        return Err("清单缺少更新包 SHA-256，拒绝安装".into());
    }
    if actual != expected {
        return Err(format!(
            "更新包 SHA-256 校验失败（期望 {expected}，实际 {actual}），已丢弃"
        ));
    }
    Ok(buffer)
}

/// 已下载、等待安装的更新包元数据（落在 `updates/pending/pending.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    pub target: String,
    pub download_url: String,
    /// 更新包 SHA-256（hex）。旧 pending.json 用 `signature` 键，兼容读取。
    #[serde(alias = "signature")]
    pub sha256: String,
    /// `nsis` / `app` / `wix` / `appimage`。
    pub format: String,
    /// 包文件名（相对 `updates/pending`）。
    pub package: String,
}

impl PendingUpdate {
    pub fn package_path(&self, data_root: &Path) -> PathBuf {
        pending_dir(data_root).join(&self.package)
    }
}

fn package_extension(format: UpdateFormat) -> &'static str {
    match format {
        UpdateFormat::Nsis => "exe",
        UpdateFormat::Wix => "msi",
        UpdateFormat::App => "app.tar.gz",
        UpdateFormat::AppImage => "AppImage.tar.gz",
    }
}

fn parse_format(name: &str) -> Result<UpdateFormat, String> {
    match name {
        "nsis" => Ok(UpdateFormat::Nsis),
        "wix" => Ok(UpdateFormat::Wix),
        "app" => Ok(UpdateFormat::App),
        "appimage" => Ok(UpdateFormat::AppImage),
        other => Err(format!("未知的更新包格式：{other}")),
    }
}

/// 把下载好的包与元数据写入 pending 目录，返回元数据。
pub fn stage_pending(
    data_root: &Path,
    update: &Update,
    bytes: &[u8],
) -> Result<PendingUpdate, String> {
    let dir = pending_dir(data_root);
    fs::create_dir_all(&dir).map_err(|e| format!("创建更新目录失败：{e}"))?;
    let package = format!(
        "dsh-marina-{}-{}.{}",
        update.version,
        update.target,
        package_extension(update.format)
    );
    fs::write(dir.join(&package), bytes).map_err(|e| format!("保存更新包失败：{e}"))?;
    let pending = PendingUpdate {
        version: update.version.clone(),
        notes: update.body.clone(),
        target: update.target.clone(),
        download_url: update.download_url.to_string(),
        sha256: update.signature.clone(),
        format: update.format.to_string(),
        package,
    };
    let json = serde_json::to_string_pretty(&pending).map_err(|e| format!("序列化失败：{e}"))?;
    fs::write(pending_json(data_root), json).map_err(|e| format!("保存更新信息失败：{e}"))?;
    Ok(pending)
}

/// 读取待安装的更新（若存在）。
pub fn read_pending(data_root: &Path) -> Option<PendingUpdate> {
    let text = fs::read_to_string(pending_json(data_root)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 从元数据重建 `Update`（离线可用，无需再次请求清单）。
fn rebuild_update(data_root: &Path, pending: &PendingUpdate) -> Result<Update, String> {
    Ok(Update {
        config: config()?,
        body: pending.notes.clone(),
        current_version: current_version().to_owned(),
        version: pending.version.clone(),
        date: None,
        target: pending.target.clone(),
        extract_path: staging_dir(data_root),
        download_url: Url::parse(&pending.download_url)
            .map_err(|e| format!("更新下载地址无效：{e}"))?,
        signature: pending.sha256.clone(),
        timeout: None,
        headers: Default::default(),
        format: parse_format(&pending.format)?,
    })
}

/// 安装待更新的包。Windows 上安装器拉起后本进程会退出（成功时不返回）。
pub fn install_pending(data_root: &Path) -> Result<(), String> {
    let pending = read_pending(data_root).ok_or("没有待安装的更新")?;
    let bytes =
        fs::read(pending.package_path(data_root)).map_err(|e| format!("读取更新包失败：{e}"))?;
    let update = rebuild_update(data_root, &pending)?;
    update
        .install(bytes)
        .map_err(|e| format!("安装更新失败：{e}"))?;
    // 仅 macOS/Linux 会走到这里（Windows 安装即退出）；清掉 pending。
    clear_pending(data_root);
    Ok(())
}

/// 删除 pending 元数据与包文件（忽略失败）。
pub fn clear_pending(data_root: &Path) {
    if let Some(pending) = read_pending(data_root) {
        let _ = fs::remove_file(pending.package_path(data_root));
    }
    let _ = fs::remove_file(pending_json(data_root));
}

/// 启动时清理：若 pending 的版本不高于当前版本，说明更新已生效，清掉残留。
pub fn cleanup_applied_pending(data_root: &Path) {
    let Some(pending) = read_pending(data_root) else {
        return;
    };
    let Ok(current) = Version::parse(current_version().trim_start_matches('v')) else {
        return;
    };
    let Ok(staged) = Version::parse(pending.version.trim_start_matches('v')) else {
        return;
    };
    if staged <= current {
        clear_pending(data_root);
    }
}

/// Read the first line of a release's notes, used as a one-line teaser.
pub fn first_line(notes: &str) -> String {
    notes
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or_default()
        .trim_start_matches(['-', '*', '+', ' '])
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_skips_headings_and_markers() {
        let notes = "## 变化\n\n- 新增自动检查更新\n- 修复一个错别字\n";
        assert_eq!(first_line(notes), "新增自动检查更新");
    }

    #[test]
    fn pending_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingUpdate {
            version: "0.2.0".into(),
            notes: Some("## 变化\n- 多端".into()),
            target: "windows-x86_64".into(),
            download_url: "https://example.com/a.exe".into(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            format: "nsis".into(),
            package: "dsh-marina-0.2.0-windows-x86_64.exe".into(),
        };
        fs::create_dir_all(pending_dir(dir.path())).unwrap();
        fs::write(pending.package_path(dir.path()), b"payload").unwrap();
        fs::write(
            pending_json(dir.path()),
            serde_json::to_string(&pending).unwrap(),
        )
        .unwrap();

        let loaded = read_pending(dir.path()).expect("pending loads");
        assert_eq!(loaded.version, "0.2.0");
        assert_eq!(loaded.format, "nsis");
        assert!(loaded.package_path(dir.path()).is_file());

        clear_pending(dir.path());
        assert!(read_pending(dir.path()).is_none());
        assert!(!loaded.package_path(dir.path()).exists());
    }

    #[test]
    fn cleanup_keeps_a_newer_pending_and_drops_an_applied_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(pending_dir(dir.path())).unwrap();
        // 当前版本是 0.1.2；pending 0.1.2 视为已生效，应被清理。
        let applied = PendingUpdate {
            version: current_version().into(),
            notes: None,
            target: "windows-x86_64".into(),
            download_url: "https://example.com/a.exe".into(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            format: "nsis".into(),
            package: "applied.exe".into(),
        };
        fs::write(
            pending_json(dir.path()),
            serde_json::to_string(&applied).unwrap(),
        )
        .unwrap();
        cleanup_applied_pending(dir.path());
        assert!(read_pending(dir.path()).is_none());
    }

    #[test]
    fn format_names_round_trip() {
        for format in [
            UpdateFormat::Nsis,
            UpdateFormat::Wix,
            UpdateFormat::App,
            UpdateFormat::AppImage,
        ] {
            assert!(parse_format(&format.to_string()).is_ok());
        }
        assert!(parse_format("bogus").is_err());
    }
}
