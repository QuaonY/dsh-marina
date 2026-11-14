//! 插件市场: the community list at awesome-dsh-plugin.com, read as a DSH
//! plugin market index (format 1, the same document DSH 0.2.1+ reads under
//! 插件 → 发现).  Every listed plugin is an npm package; installing one runs
//! `dsh plugin --profile desktop add <name>@<version>` for a stopped
//! instance (see `lifecycle::install_market_plugin`).
//!
//! The index is ~3 MB, so it is kept on disk and refreshed at most every few
//! hours; when every source fails the cached copy is shown instead.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// Where the index is read from, in order.  The site is GitHub Pages; the npm
/// package of the same list is mirrored by jsDelivr, which is often faster
/// from mainland China.
pub const INDEX_SOURCES: [&str; 2] = [
    "https://awesome-dsh-plugin.com/index.json",
    "https://cdn.jsdelivr.net/npm/dsh-plugin-catalog/index.json",
];
pub const SITE: &str = "https://awesome-dsh-plugin.com";
pub const REPOSITORY: &str = "https://github.com/awesome-dsh-plugin/awesome-dsh-plugin";

const CACHE_FILE: &str = "market-index.json";
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
/// A cached index younger than this is used without asking the network.
pub const FRESH_FOR: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Error)]
pub enum MarketError {
    #[error("{0}")]
    Request(#[from] reqwest::Error),
    #[error("服务器返回 {0}")]
    Status(u16),
    #[error("插件列表超过 {} MB", MAX_INDEX_BYTES / 1024 / 1024)]
    TooLarge,
    #[error("插件列表格式不对：{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("所有来源都读取失败：{0}")]
    AllFailed(String),
}

/// One installable plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketPlugin {
    /// npm package name, what `dsh plugin add` takes.
    pub name: String,
    /// Display title, usually `owner/repo`.
    pub title: String,
    pub description_zh: String,
    pub description_en: String,
    pub homepage: String,
    pub repository: String,
    pub category: String,
    /// Newest listed version.
    pub version: String,
    /// Unix seconds of that version.
    pub published: i64,
    /// `engines.dsh` semver range, empty when the plugin did not declare one.
    pub dsh_range: String,
    /// Community statistics from `plugins.json`; `None` until read.
    pub stats: Option<PluginStats>,
}

/// What the list measured about a plugin (stars, npm downloads, and the
/// capabilities its code uses, from the list's static scan).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginStats {
    pub stars: Option<u64>,
    /// npm downloads over the last ~30 days.
    pub downloads: Option<u64>,
    pub capabilities: Vec<String>,
    /// Combinations the scan flags, e.g. reading credentials with network access.
    pub red_lines: Vec<String>,
    pub added: String,
}

/// Chinese label of a capability id from the list's scan.
pub fn capability_label(id: &str) -> &str {
    match id {
        "network" => "联网",
        "shell" => "运行命令",
        "fs-write" => "写文件",
        "fs-read" => "读文件",
        "env" => "读环境变量",
        "credentials" => "读凭据",
        "llm" => "调用模型",
        "host-runtime" => "改宿主",
        "dynamic-code" => "动态代码",
        "subagent" => "子代理",
        other => other,
    }
}

/// Chinese wording of a flagged combination.
pub fn red_line_label(text: &str) -> String {
    if text.starts_with("reads credentials") && text.contains("network") {
        "读取凭据并且会联网".to_owned()
    } else if let Some(rest) = text.strip_prefix("runs code at install time") {
        format!("安装时会执行代码{}", rest.trim())
    } else if let Some(host) = text.strip_prefix("uses plaintext http:// to ") {
        format!("用明文 http 访问 {host}")
    } else if let Some(rest) = text.strip_prefix("uses literal IP ") {
        format!(
            "直接访问 IP {}",
            rest.trim_end_matches(" for network access")
        )
    } else {
        text.to_owned()
    }
}

/// Whether a capability is worth pointing out before installing.
pub fn is_sensitive_capability(id: &str) -> bool {
    matches!(
        id,
        "shell" | "credentials" | "dynamic-code" | "host-runtime"
    )
}

impl MarketPlugin {
    /// The description in the user's language, falling back to the other.
    pub fn description(&self) -> &str {
        if self.description_zh.is_empty() {
            &self.description_en
        } else {
            &self.description_zh
        }
    }

    /// `owner` of `owner/repo` titles.
    pub fn author(&self) -> &str {
        self.title
            .split_once('/')
            .map(|(owner, _)| owner)
            .unwrap_or("")
    }

    /// The short name shown on cards: the repository part of the title.
    pub fn short_title(&self) -> &str {
        self.title
            .split_once('/')
            .map(|(_, repo)| repo)
            .unwrap_or(&self.title)
    }

    /// `name@version` for `dsh plugin add`.
    pub fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Whether DSH `version` satisfies the declared range.  Unknown ranges and
    /// versions count as compatible: the plugin said nothing that rules it out.
    pub fn supports(&self, dsh_version: &str) -> bool {
        if self.dsh_range.trim().is_empty() {
            return true;
        }
        let (Ok(range), Ok(version)) = (
            semver::VersionReq::parse(&self.dsh_range),
            semver::Version::parse(dsh_version.trim_start_matches('v')),
        ) else {
            return true;
        };
        // Prereleases (0.2.2-rc.1) would never match `>=0.2.0` under strict
        // semver; compare their release part, which is what authors mean.
        let release = semver::Version::new(version.major, version.minor, version.patch);
        range.matches(&version) || range.matches(&release)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarketIndex {
    pub plugins: Vec<MarketPlugin>,
    /// Category id → Chinese title (English when no Chinese one exists),
    /// in the list's own order.
    pub categories: Vec<(String, String)>,
    /// Where the index came from, and when it was fetched.
    pub source: String,
    pub fetched_at: Option<SystemTime>,
    /// Shown from the cache because the network failed.
    pub stale: bool,
}

impl MarketIndex {
    pub fn category_title<'a>(&'a self, id: &'a str) -> &'a str {
        self.categories
            .iter()
            .find(|(c, _)| c == id)
            .map(|(_, t)| t.as_str())
            .unwrap_or(id)
    }

    /// Plugins per category id.
    pub fn category_counts(&self) -> BTreeMap<&str, usize> {
        let mut counts = BTreeMap::new();
        for plugin in &self.plugins {
            *counts.entry(plugin.category.as_str()).or_insert(0) += 1;
        }
        counts
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarketSort {
    /// Most npm downloads in the last month first.
    #[default]
    Downloads,
    Stars,
    /// Newest release first.
    Updated,
    Name,
}

/// Indices into `index.plugins` matching `query` (name, title, author,
/// description, category and scanned capabilities; case-insensitive, every
/// word must match) and `category`.
pub fn search(
    index: &MarketIndex,
    query: &str,
    category: Option<&str>,
    sort: MarketSort,
) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut hits: Vec<usize> = index
        .plugins
        .iter()
        .enumerate()
        .filter(|(_, p)| category.is_none_or(|c| p.category == c))
        .filter(|(_, p)| {
            if words.is_empty() {
                return true;
            }
            let capabilities = p
                .stats
                .as_ref()
                .map(|s| {
                    s.capabilities
                        .iter()
                        .map(|c| capability_label(c))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let haystack = format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n{}",
                p.name,
                p.title,
                p.author(),
                p.description_zh,
                p.description_en,
                p.category,
                capabilities
            )
            .to_lowercase();
            words.iter().all(|w| haystack.contains(w.as_str()))
        })
        .map(|(i, _)| i)
        .collect();
    let stat = |i: &usize, f: fn(&PluginStats) -> Option<u64>| {
        index.plugins[*i].stats.as_ref().and_then(f).unwrap_or(0)
    };
    match sort {
        MarketSort::Downloads => hits.sort_by(|a, b| {
            stat(b, |s| s.downloads)
                .cmp(&stat(a, |s| s.downloads))
                .then(
                    index.plugins[*b]
                        .published
                        .cmp(&index.plugins[*a].published),
                )
        }),
        MarketSort::Stars => hits.sort_by(|a, b| {
            stat(b, |s| s.stars).cmp(&stat(a, |s| s.stars)).then(
                index.plugins[*b]
                    .published
                    .cmp(&index.plugins[*a].published),
            )
        }),
        MarketSort::Updated => hits.sort_by(|a, b| {
            index.plugins[*b]
                .published
                .cmp(&index.plugins[*a].published)
        }),
        MarketSort::Name => hits.sort_by(|a, b| {
            index.plugins[*a]
                .short_title()
                .to_lowercase()
                .cmp(&index.plugins[*b].short_title().to_lowercase())
        }),
    }
    hits
}

// ---------------------------------------------------------------- parsing

#[derive(Deserialize)]
struct RawIndex {
    format: u32,
    #[serde(default)]
    metadata: RawMetadata,
    #[serde(default)]
    plugins: Vec<RawPlugin>,
}

#[derive(Deserialize, Default)]
struct RawMetadata {
    #[serde(default)]
    categories: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawPlugin {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<serde_json::Value>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<RawRepository>,
    #[serde(default)]
    metadata: Option<RawPluginMetadata>,
    #[serde(default)]
    versions: Vec<RawVersion>,
}

#[derive(Deserialize)]
struct RawRepository {
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
struct RawPluginMetadata {
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
struct RawVersion {
    version: String,
    #[serde(default, rename = "publishTimestamp")]
    publish_timestamp: i64,
    #[serde(default)]
    source: Option<RawSource>,
    #[serde(default)]
    engines: Option<RawEngines>,
}

#[derive(Deserialize)]
struct RawSource {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct RawEngines {
    #[serde(default)]
    dsh: Option<String>,
}

/// A localized text: a string, or `{ "en": …, "zh": … }`.
fn localized(value: Option<&serde_json::Value>, language: &str) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Object(map)) => map
            .get(language)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// Parse a format-1 index.  Only npm versions are kept: that is what the
/// bundled CLI installs, and what this list publishes.
pub fn parse_index(text: &str) -> Result<MarketIndex, MarketError> {
    let raw: RawIndex =
        serde_json::from_str(text).map_err(|e| MarketError::Invalid(e.to_string()))?;
    if raw.format != 1 {
        return Err(MarketError::Invalid(format!(
            "不支持的格式版本 {}",
            raw.format
        )));
    }
    let categories = raw
        .metadata
        .categories
        .iter()
        .map(|(id, value)| {
            let title = value.get("title");
            let zh = localized(title, "zh");
            let text = if zh.is_empty() {
                localized(title, "en")
            } else {
                zh
            };
            (id.clone(), if text.is_empty() { id.clone() } else { text })
        })
        .collect();
    let plugins = raw
        .plugins
        .into_iter()
        .filter(|p| crate::dsh_cli::validate_package_name(&p.name).is_ok())
        .filter_map(|p| {
            let newest = p
                .versions
                .iter()
                .filter(|v| v.source.as_ref().is_none_or(|s| s.kind == "npm"))
                .filter(|v| {
                    crate::dsh_cli::add_arguments(&[&format!("{}@{}", p.name, v.version)]).is_ok()
                })
                .max_by_key(|v| v.publish_timestamp)?;
            Some(MarketPlugin {
                title: p
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| p.name.clone()),
                description_zh: localized(p.description.as_ref(), "zh"),
                description_en: match p.description.as_ref() {
                    Some(serde_json::Value::String(text)) => text.clone(),
                    other => localized(other, "en"),
                },
                homepage: p.homepage.clone().unwrap_or_default(),
                repository: p
                    .repository
                    .as_ref()
                    .map(|r| r.url.clone())
                    .unwrap_or_default(),
                category: p
                    .metadata
                    .as_ref()
                    .and_then(|m| m.category.clone())
                    .unwrap_or_default(),
                version: newest.version.clone(),
                published: newest.publish_timestamp,
                dsh_range: newest
                    .engines
                    .as_ref()
                    .and_then(|e| e.dsh.clone())
                    .unwrap_or_default(),
                stats: None,
                name: p.name,
            })
        })
        .collect();
    Ok(MarketIndex {
        plugins,
        categories,
        ..MarketIndex::default()
    })
}

// ---------------------------------------------------------------- fetching

pub fn cache_path(data_root: &Path) -> PathBuf {
    data_root.join("cache").join(CACHE_FILE)
}

fn read_cache(data_root: &Path) -> Option<(MarketIndex, SystemTime)> {
    let path = cache_path(data_root);
    let modified = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    let text = fs::read_to_string(&path).ok()?;
    let mut index = parse_index(&text).ok()?;
    index.source = "本机缓存".to_owned();
    index.fetched_at = Some(modified);
    Some((index, modified))
}

fn fetch_text(url: &str) -> Result<String, MarketError> {
    fetch_limited(url, MAX_INDEX_BYTES)
}

fn fetch_limited(url: &str, limit: u64) -> Result<String, MarketError> {
    let response = crate::net::client(Duration::from_secs(120))?
        .get(url)
        .send()?;
    if !response.status().is_success() {
        return Err(MarketError::Status(response.status().as_u16()));
    }
    // Content-Length is the compressed size when gzip is used; the decoded
    // body is what the limit is for.
    let mut body = Vec::new();
    response.take(limit + 1).read_to_end(&mut body)?;
    if body.len() as u64 > limit {
        return Err(MarketError::TooLarge);
    }
    String::from_utf8(body).map_err(|e| MarketError::Invalid(e.to_string()))
}

fn short(error: &MarketError) -> String {
    match error {
        MarketError::Request(e) if e.is_timeout() || e.is_connect() => "连不上服务器".to_owned(),
        MarketError::Request(_) => "网络请求失败".to_owned(),
        other => other.to_string(),
    }
}

/// The index: from the cache when it is fresh (and `force` is off), else
/// from the first source that answers, else the stale cache.
pub fn load_index(data_root: &Path, force: bool) -> Result<MarketIndex, MarketError> {
    let cached = read_cache(data_root);
    if !force
        && let Some((index, modified)) = &cached
        && modified.elapsed().is_ok_and(|age| age < FRESH_FOR)
    {
        return Ok(index.clone());
    }
    let mut failures = Vec::new();
    for url in INDEX_SOURCES {
        let result = fetch_text(url).and_then(|text| parse_index(&text).map(|index| (index, text)));
        match result {
            Ok((mut index, text)) if !index.plugins.is_empty() => {
                let path = cache_path(data_root);
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&path, text);
                index.source = url.to_owned();
                index.fetched_at = Some(SystemTime::now());
                return Ok(index);
            }
            Ok(_) => failures.push(format!("{}：列表是空的", host(url))),
            Err(error) => failures.push(format!("{}：{}", host(url), short(&error))),
        }
    }
    match cached {
        Some((mut index, _)) => {
            index.stale = true;
            Ok(index)
        }
        None => Err(MarketError::AllFailed(failures.join("；"))),
    }
}

// ------------------------------------------------------------- statistics

/// The full list with statistics, same project.  ~12 MB of JSON, ~3 MB on
/// the wire gzipped, so it is cached like the index and read separately.
pub const STATS_SOURCES: [&str; 2] = [
    "https://awesome-dsh-plugin.com/plugins.json",
    "https://cdn.jsdelivr.net/npm/dsh-plugin-catalog/plugins.json",
];
const STATS_CACHE_FILE: &str = "market-stats.json";
const MAX_STATS_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct RawStatsList {
    #[serde(default)]
    plugins: Vec<RawStats>,
}

#[derive(Deserialize)]
struct RawStats {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    stars: Option<u64>,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default, rename = "capabilityRedLines")]
    red_lines: Option<Vec<String>>,
    #[serde(default)]
    added: Option<String>,
}

/// Statistics by npm package name, and by repository URL for entries the
/// index lists under a different name.
#[derive(Debug, Clone, Default)]
pub struct StatsTable {
    by_package: std::collections::HashMap<String, PluginStats>,
    by_repository: std::collections::HashMap<String, PluginStats>,
}

impl StatsTable {
    pub fn len(&self) -> usize {
        self.by_package.len().max(self.by_repository.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lookup(&self, plugin: &MarketPlugin) -> Option<&PluginStats> {
        self.by_package.get(&plugin.name).or_else(|| {
            self.by_repository
                .get(&normalize_repository(&plugin.repository))
        })
    }
}

fn normalize_repository(url: &str) -> String {
    let url = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    // `https://github.com/o/r/tree/main/dir` → `https://github.com/o/r`
    let parts: Vec<&str> = url.splitn(6, '/').collect();
    if parts.len() >= 5 {
        parts[..5].join("/")
    } else {
        url
    }
}

pub fn parse_stats(text: &str) -> Result<StatsTable, MarketError> {
    let raw: RawStatsList =
        serde_json::from_str(text).map_err(|e| MarketError::Invalid(e.to_string()))?;
    let mut table = StatsTable::default();
    for entry in raw.plugins {
        let stats = PluginStats {
            stars: entry.stars,
            downloads: entry.downloads,
            capabilities: entry.capabilities.unwrap_or_default(),
            red_lines: entry.red_lines.unwrap_or_default(),
            added: entry.added.unwrap_or_default(),
        };
        if let Some(url) = entry.url.as_deref().filter(|u| !u.is_empty()) {
            table
                .by_repository
                .entry(normalize_repository(url))
                .or_insert_with(|| stats.clone());
        }
        if let Some(npm) = entry.npm.filter(|n| !n.is_empty()) {
            table.by_package.insert(npm, stats);
        }
    }
    Ok(table)
}

/// Attach statistics to every plugin of `index` that has them.
pub fn apply_stats(index: &mut MarketIndex, table: &StatsTable) {
    for plugin in &mut index.plugins {
        plugin.stats = table.lookup(plugin).cloned();
    }
}

/// Statistics, cached like the index.  `None` when no source answered and
/// nothing is cached: the market works without them.
pub fn load_stats(data_root: &Path, force: bool) -> Option<StatsTable> {
    let path = data_root.join("cache").join(STATS_CACHE_FILE);
    let cached_age = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok());
    if !force
        && cached_age.is_some_and(|age| age < FRESH_FOR)
        && let Some(table) = fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse_stats(&t).ok())
    {
        return Some(table);
    }
    for url in STATS_SOURCES {
        let Ok(text) = fetch_limited(url, MAX_STATS_BYTES) else {
            continue;
        };
        if let Ok(table) = parse_stats(&text)
            && !table.is_empty()
        {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&path, &text);
            return Some(table);
        }
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|t| parse_stats(&t).ok())
}

fn host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "format": 1,
        "title": {"en": "Awesome DSH Plugin"},
        "metadata": {"categories": {
            "memory": {"title": {"en": "Memory", "zh": "记忆"}},
            "ui": {"title": {"en": "UI Enhancements"}}
        }},
        "plugins": [
            {"name": "@yolk/dsh-project-memory", "title": "00080000/dsh-project-memory",
             "description": {"en": "Project memory", "zh": "项目记忆"},
             "homepage": "https://awesome-dsh-plugin.com/p/x/", "repository": {"url": "https://github.com/00080000/dsh-project-memory"},
             "metadata": {"category": "memory"},
             "versions": [
                {"version": "0.6.9", "publishTimestamp": 100, "source": {"type": "npm"}},
                {"version": "0.6.10", "publishTimestamp": 200, "source": {"type": "npm"}, "engines": {"dsh": ">=0.2.1"}}
             ]},
            {"name": "dsh-status-rotator", "title": "01Virex/dsh-status-rotator", "description": "Rotates labels",
             "metadata": {"category": "ui"},
             "versions": [{"version": "0.38.0", "publishTimestamp": 300, "source": {"type": "npm"}}]},
            {"name": "git-only", "versions": [{"version": "1.0.0", "publishTimestamp": 1, "source": {"type": "git"}}]},
            {"name": "--evil", "versions": [{"version": "1.0.0", "publishTimestamp": 1}]},
            {"name": "bad-version", "versions": [{"version": "1 && calc", "publishTimestamp": 1}]}
        ]
    }"#;

    #[test]
    fn parses_npm_plugins_with_newest_version() {
        let index = parse_index(SAMPLE).unwrap();
        assert_eq!(index.plugins.len(), 2);
        let memory = &index.plugins[0];
        assert_eq!(memory.version, "0.6.10");
        assert_eq!(memory.spec(), "@yolk/dsh-project-memory@0.6.10");
        assert_eq!(memory.description(), "项目记忆");
        assert_eq!(memory.author(), "00080000");
        assert_eq!(memory.short_title(), "dsh-project-memory");
        assert_eq!(memory.dsh_range, ">=0.2.1");
        assert_eq!(index.plugins[1].description(), "Rotates labels");
        assert_eq!(index.category_title("memory"), "记忆");
        assert_eq!(index.category_title("ui"), "UI Enhancements");
    }

    #[test]
    fn joins_statistics_by_package_and_repository() {
        let mut index = parse_index(SAMPLE).unwrap();
        let stats = parse_stats(
            r#"{"plugins": [
                {"npm": "dsh-status-rotator", "url": "https://github.com/01Virex/dsh-status-rotator", "stars": 35, "downloads": 313,
                 "capabilities": ["network", "shell"], "capabilityRedLines": ["reads credentials/secrets AND has network access"]},
                {"npm": null, "url": "https://github.com/00080000/dsh-project-memory/", "stars": 7, "downloads": null},
                {"npm": null, "url": "https://github.com/x/not-listed", "stars": 1}
            ]}"#,
        )
        .unwrap();
        apply_stats(&mut index, &stats);
        let rotator = index.plugins[1].stats.as_ref().unwrap();
        assert_eq!((rotator.stars, rotator.downloads), (Some(35), Some(313)));
        assert_eq!(red_line_label(&rotator.red_lines[0]), "读取凭据并且会联网");
        assert_eq!(capability_label("shell"), "运行命令");
        assert_eq!(index.plugins[0].stats.as_ref().unwrap().stars, Some(7));
        assert_eq!(search(&index, "", None, MarketSort::Downloads), vec![1, 0]);
        assert_eq!(search(&index, "", None, MarketSort::Stars), vec![1, 0]);
        assert_eq!(
            normalize_repository("https://GitHub.com/a/b/tree/main/pkg"),
            "https://github.com/a/b"
        );
    }

    /// `cargo test -- --ignored live_market_stats`
    #[test]
    #[ignore]
    fn live_market_stats() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = load_index(dir.path(), true).unwrap();
        let table = load_stats(dir.path(), true).unwrap();
        apply_stats(&mut index, &table);
        let with = index.plugins.iter().filter(|p| p.stats.is_some()).count();
        println!("stats for {with} of {} plugins", index.plugins.len());
        for i in search(&index, "", None, MarketSort::Downloads)
            .into_iter()
            .take(3)
        {
            let p = &index.plugins[i];
            println!("{} {:?}", p.name, p.stats);
        }
        assert!(with * 2 > index.plugins.len());
    }

    #[test]
    fn rejects_other_formats() {
        assert!(parse_index(r#"{"format": 2, "plugins": []}"#).is_err());
        assert!(parse_index("not json").is_err());
    }

    #[test]
    fn searches_and_sorts() {
        let index = parse_index(SAMPLE).unwrap();
        assert_eq!(search(&index, "", None, MarketSort::Updated), vec![1, 0]);
        assert_eq!(search(&index, "", None, MarketSort::Name), vec![0, 1]);
        assert_eq!(search(&index, "项目", None, MarketSort::Updated), vec![0]);
        assert_eq!(
            search(&index, "ROTATES labels", None, MarketSort::Updated),
            vec![1]
        );
        assert_eq!(
            search(&index, "", Some("memory"), MarketSort::Updated),
            vec![0]
        );
        assert!(search(&index, "rotates", Some("memory"), MarketSort::Updated).is_empty());
    }

    #[test]
    fn checks_dsh_compatibility() {
        let mut plugin = parse_index(SAMPLE).unwrap().plugins.remove(0);
        assert!(plugin.supports("0.2.1"));
        assert!(plugin.supports("0.2.2-rc.1"));
        assert!(!plugin.supports("0.2.0"));
        assert!(plugin.supports("not-a-version"));
        plugin.dsh_range.clear();
        assert!(plugin.supports("0.1.0"));
    }

    #[test]
    fn uses_fresh_cache_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, SAMPLE).unwrap();
        let index = load_index(dir.path(), false).unwrap();
        assert_eq!(index.plugins.len(), 2);
        assert_eq!(index.source, "本机缓存");
        assert!(!index.stale);
    }

    /// `cargo test -- --ignored live_market`
    #[test]
    #[ignore]
    fn live_market_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = load_index(dir.path(), true).unwrap();
        println!(
            "{} plugins, {} categories from {}",
            index.plugins.len(),
            index.categories.len(),
            index.source
        );
        assert!(index.plugins.len() > 100);
    }
}
