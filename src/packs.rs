//! Marina Pack export/import.
//!
//! A pack is a zip container with a JSON manifest plus the instance's profile
//! configuration and local `.tgz` packages.  Sessions, credentials, Electron
//! state, and `node_modules` are never included.

use crate::instances::{InstanceConfig, InstanceRepository, OriginConfig, OriginKind};
use crate::plugins::list_plugins;
pub use crate::sensitive::{SensitiveFinding, SensitiveKind};
use crate::sensitive::{redact_text, scan_text, user_home};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const PACK_FORMAT: &str = "dsh-marina-pack";
pub const PACK_FORMAT_VERSION: u32 = 1;
const MANIFEST_NAME: &str = "marina-pack.json";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

const PROFILE_FILES: &[&str] = &[
    "package.json",
    "cordis.patch.yml",
    "cordis.yml",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "compatibility.json",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackPlugin {
    pub name: String,
    pub version: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    pub format: String,
    pub format_version: u32,
    pub name: String,
    #[serde(default)]
    pub dsh_version: String,
    pub created_at: String,
    #[serde(default)]
    pub plugins: Vec<PackPlugin>,
    #[serde(default)]
    pub files: Vec<PackFile>,
}

#[derive(Debug, Error)]
pub enum PackError {
    #[error("读写整合包失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("整合包文件无效：{0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("整合包清单无效：{0}")]
    Manifest(#[from] serde_json::Error),
    #[error("不支持的整合包格式：{0}")]
    UnsupportedFormat(String),
    #[error("整合包含有不安全的路径：{0}")]
    UnsafePath(String),
    #[error("整合包里的文件超过大小限制：{0}")]
    Oversized(String),
    #[error("整合包缺少 {0}")]
    MissingEntry(String),
    #[error("实例的插件配置还没有初始化：{0}")]
    ProfileNotInitialized(PathBuf),
    #[error("实例仓库出错：{0}")]
    Repository(String),
}

/// Scan the text files [`export_instance`] would put into a pack for
/// credential-looking values and local absolute paths (design §FR-7).
/// Package archives (`packages/*.tgz`) are binary and not scanned.
pub fn scan_export(
    repository: &InstanceRepository,
    instance_id: &str,
) -> Result<Vec<SensitiveFinding>, PackError> {
    let profile = export_profile(repository, instance_id)?;
    let home = user_home();
    let mut findings = Vec::new();
    for (relative, source, is_text) in export_sources(&profile)? {
        if !is_text {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&source) {
            findings.extend(scan_text(&relative, &text, home.as_deref()));
        }
    }
    Ok(findings)
}

pub fn export_instance(
    repository: &InstanceRepository,
    instance_id: &str,
    destination: &Path,
) -> Result<PackManifest, PackError> {
    export_with(repository, instance_id, destination, None)
}

/// [`export_instance`], but text files have detected secret values replaced
/// with `***` and the user's home directory prefix replaced with
/// `%USERPROFILE%` (design §FR-7).  The manifest describes the redacted files.
pub fn export_instance_redacted(
    repository: &InstanceRepository,
    instance_id: &str,
    destination: &Path,
) -> Result<PackManifest, PackError> {
    let home = user_home();
    let home_str = home.as_deref().and_then(|p| p.to_str());
    export_with(
        repository,
        instance_id,
        destination,
        Some(Redaction { home: home_str }),
    )
}

#[derive(Debug, Clone, Copy)]
struct Redaction<'a> {
    home: Option<&'a str>,
}

/// Files that go into a pack, as (path inside the pack, source path, is text).
fn export_sources(profile: &Path) -> Result<Vec<(String, PathBuf, bool)>, PackError> {
    let mut sources = Vec::new();
    for name in PROFILE_FILES {
        let source = profile.join(name);
        if source.is_file() {
            sources.push(((*name).to_owned(), source, true));
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
            {
                let path = entry.path();
                let relative = relative_path(profile, &path)?;
                sources.push((relative, path, false));
            }
        }
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(sources)
}

fn export_profile(
    repository: &InstanceRepository,
    instance_id: &str,
) -> Result<PathBuf, PackError> {
    let paths = repository
        .paths(instance_id)
        .map_err(|error| PackError::Repository(error.to_string()))?;
    let profile = paths.home.join("profiles/desktop");
    if !profile.is_dir() {
        return Err(PackError::ProfileNotInitialized(profile));
    }
    Ok(profile)
}

fn export_with(
    repository: &InstanceRepository,
    instance_id: &str,
    destination: &Path,
    redaction: Option<Redaction<'_>>,
) -> Result<PackManifest, PackError> {
    let config = repository
        .load(instance_id)
        .map_err(|error| PackError::Repository(error.to_string()))?;
    let paths = repository
        .paths(instance_id)
        .map_err(|error| PackError::Repository(error.to_string()))?;
    let profile = export_profile(repository, instance_id)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut files = Vec::new();
    let mut replaced = std::collections::HashMap::new();
    for (_, source, is_text) in export_sources(&profile)? {
        let mut file = relative_file(&profile, &source)?;
        if let Some(redaction) = redaction
            && is_text
            && let Ok(text) = fs::read_to_string(&source)
        {
            let redacted = redact_text(&text, redaction.home);
            if redacted != text {
                file.size = redacted.len() as u64;
                file.sha256 = hex_digest(redacted.as_bytes());
                replaced.insert(file.path.clone(), redacted.into_bytes());
            }
        }
        files.push(file);
    }

    let plugins = list_plugins(&paths.home)
        .unwrap_or_default()
        .into_iter()
        .map(|plugin| PackPlugin {
            name: plugin.name,
            version: plugin.version,
            enabled: plugin.enabled,
        })
        .collect();

    let manifest = PackManifest {
        format: PACK_FORMAT.to_owned(),
        format_version: PACK_FORMAT_VERSION,
        name: config.name.clone(),
        dsh_version: config.runtime.dsh_version.clone(),
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        plugins,
        files,
    };

    let mut temporary = NamedTempFile::new_in(
        destination
            .parent()
            .ok_or_else(|| PackError::UnsafePath(destination.display().to_string()))?,
    )?;
    {
        let mut writer = ZipWriter::new(temporary.as_file_mut());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file(MANIFEST_NAME, options)?;
        writer.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        for file in &manifest.files {
            writer.start_file(&file.path, options)?;
            if let Some(bytes) = replaced.get(&file.path) {
                writer.write_all(bytes)?;
            } else {
                let mut source = File::open(profile.join(&file.path))?;
                std::io::copy(&mut source, &mut writer)?;
            }
        }
        writer.finish()?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| PackError::Io(error.error))?;
    Ok(manifest)
}

fn relative_file(profile: &Path, path: &Path) -> Result<PackFile, PackError> {
    let relative = relative_path(profile, path)?;
    let bytes = fs::read(path)?;
    Ok(PackFile {
        path: relative,
        size: bytes.len() as u64,
        sha256: hex_digest(&bytes),
    })
}

fn relative_path(profile: &Path, path: &Path) -> Result<String, PackError> {
    let relative = path
        .strip_prefix(profile)
        .map_err(|_| PackError::UnsafePath(path.display().to_string()))?;
    Ok(relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_string_lossy().into_owned()),
            _ => Err(PackError::UnsafePath(path.display().to_string())),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/"))
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn preview_pack(path: &Path) -> Result<PackManifest, PackError> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    read_manifest(&mut archive)
}

/// Read an archive entry with a hard limit on the bytes actually produced.
///
/// `ZipFile::size()` comes from the central directory and is attacker
/// controlled: a tiny declared size with a huge deflate stream would otherwise
/// be decompressed in full before the checksum check runs.
fn read_entry_limited<R: Read>(
    entry: &mut R,
    limit: u64,
    name: &str,
) -> Result<Vec<u8>, PackError> {
    let mut bytes = Vec::new();
    entry
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(PackError::Oversized(name.to_owned()));
    }
    Ok(bytes)
}

fn read_manifest(archive: &mut ZipArchive<File>) -> Result<PackManifest, PackError> {
    let mut entry = archive
        .by_name(MANIFEST_NAME)
        .map_err(|_| PackError::MissingEntry(MANIFEST_NAME.to_owned()))?;
    let bytes = read_entry_limited(&mut entry, MAX_MANIFEST_BYTES, MANIFEST_NAME)?;
    let manifest: PackManifest = serde_json::from_slice(&bytes)?;
    if manifest.format != PACK_FORMAT || manifest.format_version != PACK_FORMAT_VERSION {
        return Err(PackError::UnsupportedFormat(format!(
            "{} v{}",
            manifest.format, manifest.format_version
        )));
    }
    Ok(manifest)
}

/// Import a pack into a new instance.  The caller must ensure the instance name
/// is non-empty; a sensible default is derived from the manifest.
pub fn import_pack(
    repository: &InstanceRepository,
    pack_path: &Path,
    name_override: Option<&str>,
) -> Result<InstanceConfig, PackError> {
    let mut archive = ZipArchive::new(File::open(pack_path)?)?;
    let manifest = read_manifest(&mut archive)?;
    validate_manifest_files(&manifest)?;
    let name = name_override
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&manifest.name)
        .to_owned();
    let mut config = InstanceConfig::new_blank(name);
    if !manifest.dsh_version.is_empty() {
        config.runtime.dsh_version = manifest.dsh_version.clone();
    }
    config.origin = OriginConfig {
        kind: OriginKind::Package,
        from: Some(
            pack_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    };
    let config = repository
        .create(config)
        .map_err(|error| PackError::Repository(error.to_string()))?;
    let paths = repository
        .paths(&config.id)
        .map_err(|error| PackError::Repository(error.to_string()))?;
    let profile = paths.home.join("profiles/desktop");
    let result = extract_pack(&mut archive, &manifest, &profile);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&paths.root);
        return Err(error);
    }
    Ok(config)
}

fn validate_manifest_files(manifest: &PackManifest) -> Result<(), PackError> {
    let mut total = 0u64;
    for file in &manifest.files {
        if !is_allowed_path(&file.path) {
            return Err(PackError::UnsafePath(file.path.clone()));
        }
        if file.size > MAX_FILE_BYTES {
            return Err(PackError::Oversized(file.path.clone()));
        }
        total = total.saturating_add(file.size);
        if total > MAX_TOTAL_BYTES {
            return Err(PackError::Oversized("整合包总大小".to_owned()));
        }
    }
    Ok(())
}

fn extract_pack(
    archive: &mut ZipArchive<File>,
    manifest: &PackManifest,
    profile: &Path,
) -> Result<(), PackError> {
    fs::create_dir_all(profile)?;
    let expected = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<std::collections::HashMap<_, _>>();
    let mut extracted = std::collections::HashSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(path) = entry.enclosed_name() else {
            return Err(PackError::UnsafePath(entry.name().to_owned()));
        };
        let path_text = path.to_string_lossy().replace('\\', "/");
        if path_text == MANIFEST_NAME {
            continue;
        }
        let Some(expected_file) = expected.get(path_text.as_str()) else {
            // Unknown entries are ignored rather than trusted.
            continue;
        };
        if !is_allowed_path(&path_text) {
            return Err(PackError::UnsafePath(path_text));
        }
        if entry.size() > MAX_FILE_BYTES {
            return Err(PackError::Oversized(path_text));
        }
        let bytes = read_entry_limited(&mut entry, MAX_FILE_BYTES, &path_text)?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_TOTAL_BYTES {
            return Err(PackError::Oversized("整合包总大小".to_owned()));
        }
        if bytes.len() as u64 != expected_file.size || hex_digest(&bytes) != expected_file.sha256 {
            return Err(PackError::UnsafePath(format!(
                "{path_text} 的哈希和清单不一致"
            )));
        }
        let destination = profile.join(&path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(destination, bytes)?;
        extracted.insert(path_text);
    }
    for file in &manifest.files {
        if !extracted.contains(file.path.as_str()) {
            return Err(PackError::MissingEntry(file.path.clone()));
        }
    }
    Ok(())
}

fn is_allowed_path(path: &str) -> bool {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return false;
    }
    PROFILE_FILES.contains(&path) || path.starts_with("packages/") && path.ends_with(".tgz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn exports_and_imports_profile_configuration() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path().join("data"));
        let source = repository.create_blank("工作区").unwrap();
        let paths = repository.paths(&source.id).unwrap();
        let profile = paths.home.join("profiles/desktop");
        fs::create_dir_all(profile.join("packages")).unwrap();
        fs::write(
            profile.join("package.json"),
            br#"{"dependencies":{"p":"1.0.0"}}"#,
        )
        .unwrap();
        fs::write(profile.join("cordis.patch.yml"), b"- id: p\n").unwrap();
        fs::write(profile.join("packages/p.tgz"), b"package").unwrap();
        fs::write(paths.home.join(".credentials.yaml"), b"secret").unwrap();

        let pack = root.path().join("工作区.marina-pack.zip");
        let manifest = export_instance(&repository, &source.id, &pack).unwrap();
        assert_eq!(manifest.name, "工作区");
        assert_eq!(manifest.files.len(), 3);
        assert_eq!(manifest.plugins.len(), 1);
        assert_eq!(manifest.plugins[0].name, "p");

        let imported = import_pack(&repository, &pack, None).unwrap();
        let imported_paths = repository.paths(&imported.id).unwrap();
        assert_eq!(
            fs::read(imported_paths.home.join("profiles/desktop/package.json")).unwrap(),
            br#"{"dependencies":{"p":"1.0.0"}}"#
        );
        assert!(!imported_paths.home.join(".credentials.yaml").exists());
        assert_eq!(imported.origin.kind, OriginKind::Package);
    }

    #[test]
    fn scans_and_redacts_exported_text_files() {
        let root = tempdir().unwrap();
        let repository = InstanceRepository::new(root.path().join("data"));
        let source = repository.create_blank("secret").unwrap();
        let paths = repository.paths(&source.id).unwrap();
        let profile = paths.home.join("profiles/desktop");
        fs::create_dir_all(profile.join("packages")).unwrap();
        let home = user_home().map_or_else(
            || r"C:\Users\example".to_owned(),
            |p| p.display().to_string(),
        );
        fs::write(profile.join("package.json"), br#"{"dependencies":{}}"#).unwrap();
        fs::write(
            profile.join("cordis.yml"),
            format!("plugins:\n  p:\n    apiKey: exampleSecret0001\n    dir: {home}\\bots\n    other: D:\\data\n"),
        )
        .unwrap();
        fs::write(profile.join("packages/p.tgz"), b"apiKey: exampleSecret0001").unwrap();

        let findings = scan_export(&repository, &source.id).unwrap();
        let summary = findings
            .iter()
            .map(|finding| (finding.file.as_str(), finding.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("cordis.yml", SensitiveKind::Token),
                ("cordis.yml", SensitiveKind::AbsolutePath),
                ("cordis.yml", SensitiveKind::AbsolutePath),
            ]
        );
        assert!(
            findings
                .iter()
                .all(|finding| !finding.excerpt.contains("abcdefgh"))
        );

        let pack = root.path().join("secret.marina-pack.zip");
        export_instance_redacted(&repository, &source.id, &pack).unwrap();
        let imported = import_pack(&repository, &pack, None).unwrap();
        let imported_profile = repository
            .paths(&imported.id)
            .unwrap()
            .home
            .join("profiles/desktop");
        assert_eq!(
            fs::read_to_string(imported_profile.join("cordis.yml")).unwrap(),
            "plugins:\n  p:\n    apiKey: \"***\"\n    dir: %USERPROFILE%\\bots\n    other: D:\\data\n"
        );
        assert_eq!(
            fs::read(imported_profile.join("packages/p.tgz")).unwrap(),
            b"apiKey: exampleSecret0001"
        );

        let plain = root.path().join("plain.marina-pack.zip");
        export_instance(&repository, &source.id, &plain).unwrap();
        let imported = import_pack(&repository, &plain, None).unwrap();
        let text = fs::read_to_string(
            repository
                .paths(&imported.id)
                .unwrap()
                .home
                .join("profiles/desktop/cordis.yml"),
        )
        .unwrap();
        assert!(text.contains("exampleSecret0001"));
    }

    #[test]
    fn rejects_packs_with_unsafe_entries() {
        let root = tempdir().unwrap();
        let pack = root.path().join("bad.zip");
        {
            let file = File::create(&pack).unwrap();
            let mut writer = ZipWriter::new(file);
            let manifest = PackManifest {
                format: PACK_FORMAT.into(),
                format_version: PACK_FORMAT_VERSION,
                name: "bad".into(),
                dsh_version: "latest-stable".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                plugins: Vec::new(),
                files: vec![PackFile {
                    path: "../escape".into(),
                    size: 1,
                    sha256: "00".into(),
                }],
            };
            writer
                .start_file(MANIFEST_NAME, SimpleFileOptions::default())
                .unwrap();
            writer
                .write_all(&serde_json::to_vec(&manifest).unwrap())
                .unwrap();
            writer.finish().unwrap();
        }
        let repository = InstanceRepository::new(root.path().join("data"));
        assert!(matches!(
            import_pack(&repository, &pack, None),
            Err(PackError::UnsafePath(_))
        ));
    }

    /// 声明的大小不可信：实际数据超过上限时必须拒绝，而不是先全部读进内存。
    #[test]
    fn limits_the_bytes_actually_read_from_an_entry() {
        let mut exact = std::io::Cursor::new(vec![7u8; 16]);
        assert_eq!(read_entry_limited(&mut exact, 16, "x").unwrap().len(), 16);

        let mut over = std::io::Cursor::new(vec![7u8; 16]);
        assert!(matches!(
            read_entry_limited(&mut over, 15, "x"),
            Err(PackError::Oversized(_))
        ));

        let mut huge = std::io::Cursor::new(vec![0u8; 1 << 20]);
        assert!(matches!(
            read_entry_limited(&mut huge, 1024, "x"),
            Err(PackError::Oversized(_))
        ));
    }
}
