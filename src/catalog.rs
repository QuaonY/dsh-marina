//! Shared plugin packs (整合包), like the modpack browser of a Minecraft
//! launcher: a catalog lists packs other users shared, Marina downloads one,
//! checks its hash and imports it as a new instance.
//!
//! The catalog is a JSON document at a user-configured HTTPS address (or a
//! local file, for testing and offline sharing):
//!
//! ```json
//! { "packs": [ { "id": "writer-kit", "name": "写作套装", "author": "someone",
//!   "description": "…", "dsh_version": "1.4.2", "plugins": ["a", "b"],
//!   "url": "https://…/writer-kit.zip", "size": 12345, "sha256": "…" } ] }
//! ```
//!
//! Downloaded packs are kept in `<data_root>/packs/`.  Every pack is an
//! ordinary Marina instance pack, so import keeps the existing validation
//! (safe paths, per-file hashes, size limits).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;

use crate::packs::{self, PackManifest};

pub const PACKS_DIR: &str = "packs";
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
/// Shared packs hold plugin configuration and local packages, not sessions.
pub const MAX_PACK_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub dsh_version: String,
    #[serde(default)]
    pub plugins: Vec<String>,
    pub url: String,
    #[serde(default)]
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CatalogDocument {
    #[serde(default)]
    packs: Vec<CatalogEntry>,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("网络请求失败：{0}")]
    Request(#[from] reqwest::Error),
    #[error("读写文件失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("整合包目录格式不对：{0}")]
    Format(String),
    #[error("只支持 https:// 地址或本地文件")]
    InsecureUrl,
    #[error("文件过大")]
    TooLarge,
    #[error("校验失败：下载的文件和目录里登记的哈希不一致")]
    HashMismatch,
    #[error("已取消")]
    Cancelled,
    #[error("不是有效的整合包：{0}")]
    Pack(String),
}

/// A catalog location is `https://…`, `file:///…` or a plain local path.
enum Location {
    Remote(String),
    Local(PathBuf),
}

/// ASCII 前缀比较（忽略大小写）。
///
/// `str::get` 在索引不是字符边界时返回 `None`：地址里第 8 个字节正好落在
/// 一个多字节字符中间时也不会 panic，而是当作「不匹配」。
fn has_ascii_prefix(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn location(address: &str) -> Result<Location, CatalogError> {
    let address = address.trim();
    if has_ascii_prefix(address, "https://") {
        return Ok(Location::Remote(address.to_owned()));
    }
    if has_ascii_prefix(address, "file:///") {
        let rest = address.get("file:///".len()..).unwrap_or_default();
        return Ok(Location::Local(PathBuf::from(rest.replace('/', "\\"))));
    }
    let path = Path::new(address);
    if path.is_absolute() {
        return Ok(Location::Local(path.to_path_buf()));
    }
    Err(CatalogError::InsecureUrl)
}

/// Resolve a pack URL from the catalog against the catalog's own address, so a
/// catalog may use relative names such as `writer-kit.zip`.
///
/// A remote catalog may only point at remote packs: an entry naming a local
/// file must not make the app read and install something from this machine.
fn resolve_pack_url(catalog: &str, url: &str) -> Result<Location, CatalogError> {
    match location(catalog)? {
        Location::Remote(base) => {
            let base = &base[..base.rfind('/').map_or(base.len(), |i| i + 1)];
            if has_ascii_prefix(url, "https://") {
                return Ok(Location::Remote(url.to_owned()));
            }
            if Path::new(url).is_absolute()
                || url.contains("://")
                || url.contains("..")
                || url.starts_with(['/', '\\'])
            {
                return Err(CatalogError::InsecureUrl);
            }
            Ok(Location::Remote(format!("{base}{url}")))
        }
        Location::Local(path) => {
            if let Ok(location) = location(url) {
                return Ok(location);
            }
            if url.contains("://") || url.contains("..") || url.starts_with(['/', '\\']) {
                return Err(CatalogError::InsecureUrl);
            }
            Ok(Location::Local(
                path.parent().unwrap_or(Path::new(".")).join(url),
            ))
        }
    }
}

fn client() -> Result<reqwest::blocking::Client, CatalogError> {
    Ok(crate::net::client(Duration::from_secs(60 * 30))?)
}

/// Parse and validate a catalog document.  Invalid entries are dropped so one
/// bad entry does not hide the rest.
pub fn parse_catalog(text: &str) -> Result<Vec<CatalogEntry>, CatalogError> {
    let document: CatalogDocument =
        serde_json::from_str(text).map_err(|e| CatalogError::Format(e.to_string()))?;
    let mut seen = std::collections::HashSet::new();
    Ok(document
        .packs
        .into_iter()
        .filter(|entry| valid_entry(entry) && seen.insert(entry.id.clone()))
        .collect())
}

fn valid_entry(entry: &CatalogEntry) -> bool {
    let id_ok = !entry.id.is_empty()
        && entry.id.len() <= 64
        && entry
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !entry.id.starts_with('.');
    let hash_ok = entry.sha256.len() == 64 && entry.sha256.chars().all(|c| c.is_ascii_hexdigit());
    id_ok
        && hash_ok
        && !entry.name.trim().is_empty()
        && !entry.url.trim().is_empty()
        && entry.size <= MAX_PACK_BYTES
}

/// Fetch the catalog at `address`.
pub fn fetch_catalog(address: &str) -> Result<Vec<CatalogEntry>, CatalogError> {
    let text = match location(address)? {
        Location::Local(path) => {
            if fs::metadata(&path)?.len() > MAX_CATALOG_BYTES {
                return Err(CatalogError::TooLarge);
            }
            fs::read_to_string(path)?
        }
        Location::Remote(url) => {
            let response = client()?.get(url).send()?.error_for_status()?;
            let mut text = String::new();
            response
                .take(MAX_CATALOG_BYTES + 1)
                .read_to_string(&mut text)?;
            if text.len() as u64 > MAX_CATALOG_BYTES {
                return Err(CatalogError::TooLarge);
            }
            text
        }
    };
    parse_catalog(&text)
}

pub fn packs_dir(data_root: &Path) -> PathBuf {
    data_root.join(PACKS_DIR)
}

/// Where `entry` is stored once downloaded.
pub fn local_path(data_root: &Path, entry: &CatalogEntry) -> PathBuf {
    packs_dir(data_root).join(format!("{}.zip", entry.id))
}

/// Already downloaded and still matching the catalog hash.
pub fn is_downloaded(data_root: &Path, entry: &CatalogEntry) -> bool {
    let path = local_path(data_root, entry);
    path.is_file() && file_sha256(&path).is_ok_and(|hash| hash.eq_ignore_ascii_case(&entry.sha256))
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Download `entry` into `packs/`, verify its hash and manifest, and return
/// the stored file with its manifest.  `progress(done, total)` is called as
/// bytes arrive.  A failed or cancelled download leaves nothing behind.
pub fn download_pack(
    data_root: &Path,
    catalog: &str,
    entry: &CatalogEntry,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(PathBuf, PackManifest), CatalogError> {
    let target = local_path(data_root, entry);
    if is_downloaded(data_root, entry) {
        let manifest =
            packs::preview_pack(&target).map_err(|e| CatalogError::Pack(e.to_string()))?;
        return Ok((target, manifest));
    }
    fs::create_dir_all(packs_dir(data_root))?;
    let partial = target.with_extension("zip.part");
    let result = (|| {
        let mut reader: Box<dyn Read> = match resolve_pack_url(catalog, &entry.url)? {
            Location::Local(path) => Box::new(File::open(path)?),
            Location::Remote(url) => Box::new(client()?.get(url).send()?.error_for_status()?),
        };
        let mut output = File::create(&partial)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CatalogError::Cancelled);
            }
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            done += read as u64;
            if done > MAX_PACK_BYTES {
                return Err(CatalogError::TooLarge);
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
            progress(done, entry.size.max(done));
        }
        output.sync_all()?;
        drop(output);
        let hash: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if !hash.eq_ignore_ascii_case(&entry.sha256) {
            return Err(CatalogError::HashMismatch);
        }
        let manifest =
            packs::preview_pack(&partial).map_err(|e| CatalogError::Pack(e.to_string()))?;
        fs::rename(&partial, &target)?;
        Ok((target.clone(), manifest))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// A pack file in `packs/` with its manifest (unreadable files are skipped).
#[derive(Debug, Clone)]
pub struct LocalPack {
    pub path: PathBuf,
    pub manifest: PackManifest,
    pub size: u64,
}

pub fn local_packs(data_root: &Path) -> Vec<LocalPack> {
    let Ok(entries) = fs::read_dir(packs_dir(data_root)) else {
        return Vec::new();
    };
    let mut packs: Vec<LocalPack> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")))
        .filter_map(|path| {
            let manifest = packs::preview_pack(&path).ok()?;
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            Some(LocalPack {
                path,
                manifest,
                size,
            })
        })
        .collect();
    packs.sort_by(|a, b| b.manifest.created_at.cmp(&a.manifest.created_at));
    packs
}

/// The catalog entry to paste into a shared catalog for a pack you exported.
pub fn entry_for_file(
    path: &Path,
    id: &str,
    author: &str,
    description: &str,
) -> Result<CatalogEntry, CatalogError> {
    let manifest = packs::preview_pack(path).map_err(|e| CatalogError::Pack(e.to_string()))?;
    let size = fs::metadata(path)?.len();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(CatalogEntry {
        id: id.to_owned(),
        name: manifest.name.clone(),
        author: author.to_owned(),
        description: description.to_owned(),
        dsh_version: manifest.dsh_version.clone(),
        plugins: manifest
            .plugins
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.name.clone())
            .collect(),
        url: file_name,
        size,
        sha256: file_sha256(path)?,
    })
}

pub fn entry_json(entry: &CatalogEntry) -> String {
    serde_json::to_string_pretty(entry).unwrap_or_default()
}

/// A catalog id derived from a display name.
pub fn slug(name: &str) -> String {
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        slug = format!("pack-{}", chrono::Utc::now().format("%Y%m%d%H%M%S"));
    }
    slug.truncate(48);
    slug
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::InstanceRepository;
    use tempfile::tempdir;

    fn entry(id: &str, sha: &str) -> String {
        format!(r#"{{"id":"{id}","name":"N {id}","url":"{id}.zip","sha256":"{sha}"}}"#)
    }

    #[test]
    fn parses_and_filters_catalog_entries() {
        let good = "a".repeat(64);
        let text = format!(
            r#"{{"packs":[{},{},{},{}]}}"#,
            entry("writer-kit", &good),
            entry("../evil", &good),
            entry("short-hash", "abc"),
            entry("writer-kit", &good)
        );
        let packs = parse_catalog(&text).unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].id, "writer-kit");
        assert!(parse_catalog("not json").is_err());
        assert!(parse_catalog("{}").unwrap().is_empty());
    }

    #[test]
    fn rejects_plain_http_and_escaping_relative_urls() {
        assert!(matches!(
            location("http://example.com/c.json"),
            Err(CatalogError::InsecureUrl)
        ));
        assert!(matches!(
            location("relative.json"),
            Err(CatalogError::InsecureUrl)
        ));
        assert!(matches!(
            location("https://example.com/c.json"),
            Ok(Location::Remote(_))
        ));
        match resolve_pack_url("https://example.com/packs/catalog.json", "kit.zip").unwrap() {
            Location::Remote(url) => assert_eq!(url, "https://example.com/packs/kit.zip"),
            Location::Local(_) => panic!("expected remote"),
        }
        assert!(resolve_pack_url("https://example.com/c.json", "../x.zip").is_err());
        assert!(resolve_pack_url("https://example.com/c.json", "http://x/y.zip").is_err());
        // A remote catalog may not point the app at a file on this machine.
        assert!(resolve_pack_url("https://example.com/c.json", r"C:\evil\pack.zip").is_err());
        assert!(
            resolve_pack_url("https://example.com/c.json", "file:///C:/evil/pack.zip").is_err()
        );
    }

    #[test]
    fn shares_downloads_and_imports_a_pack_from_a_local_catalog() {
        let dir = tempdir().unwrap();
        let source_root = dir.path().join("source");
        let repository = InstanceRepository::new(&source_root);
        let instance = repository.create_blank("写作套装").unwrap();
        let profile = repository
            .paths(&instance.id)
            .unwrap()
            .home
            .join("profiles/desktop");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("package.json"), r#"{"dependencies":{}}"#).unwrap();
        let shared = dir.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        let pack_file = shared.join("writer-kit.zip");
        packs::export_instance(&repository, &instance.id, &pack_file).unwrap();

        let entry = entry_for_file(&pack_file, "writer-kit", "me", "test").unwrap();
        assert_eq!(entry.url, "writer-kit.zip");
        let catalog = shared.join("catalog.json");
        fs::write(&catalog, format!(r#"{{"packs":[{}]}}"#, entry_json(&entry))).unwrap();

        let address = catalog.to_string_lossy().into_owned();
        let listed = fetch_catalog(&address).unwrap();
        assert_eq!(listed, vec![entry.clone()]);

        let data_root = dir.path().join("marina");
        let mut last = (0, 0);
        let (path, manifest) = download_pack(
            &data_root,
            &address,
            &listed[0],
            &AtomicBool::new(false),
            |d, t| last = (d, t),
        )
        .unwrap();
        assert_eq!(manifest.name, "写作套装");
        assert_eq!(last.0, entry.size);
        assert!(is_downloaded(&data_root, &entry));
        assert_eq!(local_packs(&data_root).len(), 1);

        let target = InstanceRepository::new(&data_root);
        let imported = packs::import_pack(&target, &path, None).unwrap();
        assert_eq!(imported.name, "写作套装");
    }

    #[test]
    fn a_hash_mismatch_or_cancel_leaves_nothing() {
        let dir = tempdir().unwrap();
        let shared = dir.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("x.zip"), b"not really a zip").unwrap();
        let catalog = shared.join("catalog.json").to_string_lossy().into_owned();
        let entry = CatalogEntry {
            id: "x".into(),
            name: "X".into(),
            author: String::new(),
            description: String::new(),
            dsh_version: String::new(),
            plugins: Vec::new(),
            url: "x.zip".into(),
            size: 0,
            sha256: "0".repeat(64),
        };
        let root = dir.path().join("marina");
        let error =
            download_pack(&root, &catalog, &entry, &AtomicBool::new(false), |_, _| {}).unwrap_err();
        assert!(matches!(error, CatalogError::HashMismatch));
        let error =
            download_pack(&root, &catalog, &entry, &AtomicBool::new(true), |_, _| {}).unwrap_err();
        assert!(matches!(error, CatalogError::Cancelled));
        assert_eq!(fs::read_dir(packs_dir(&root)).unwrap().count(), 0);
    }

    #[test]
    fn slugs_names() {
        assert_eq!(slug("Writer Kit 2"), "writer-kit-2");
        assert!(slug("写作套装").starts_with("pack-"));
    }

    /// 第 8 个字节落在多字节字符中间时不能 panic，只能是「地址不合法」。
    #[test]
    fn non_ascii_address_does_not_panic() {
        assert!(matches!(
            location("abcdef你"),
            Err(CatalogError::InsecureUrl)
        ));
        assert!(matches!(
            location("https:/你"),
            Err(CatalogError::InsecureUrl)
        ));
        assert!(matches!(location("file:///你"), Ok(Location::Local(_))));
        assert!(matches!(
            resolve_pack_url("abcdef你", "x.zip"),
            Err(CatalogError::InsecureUrl)
        ));
        assert!(matches!(
            resolve_pack_url("https://example.com/catalog.json", "你你你你"),
            Ok(Location::Remote(_))
        ));
    }
}
