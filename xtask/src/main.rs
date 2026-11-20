//! DSH Marina 发布工具。
//!
//! 子命令：
//! - `cargo xtask package [--skip-build]`
//!   在当前平台构建并打包（Windows → NSIS；macOS → .app + .dmg），产物按稳定
//!   文件名放进 `dist/`，为更新包计算 SHA-256（不做签名、无需私钥），
//!   并写出 `dist/fragment-<target>.json`。
//! - `cargo xtask manifest --tag v<版本> --notes CHANGELOG.md`
//!   从累计式 CHANGELOG 抽取该版本章节，合并各平台片段生成 `update.json`（含旧客户端
//!   读取的 `tag_name/html_url/body/assets` 与 updater 需要的 `version/notes/pub_date/platforms`），
//!   并写出 `release-body.md` 供 CI 作为 Release 说明。发布由 CI 全自动完成。

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use cargo_packager::{Config, PackageFormat, PackageOutput, package};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const REPO: &str = "QuaonY/dsh-marina";

fn main() -> Result<()> {
    let root = env::current_dir().context("无法读取当前目录")?;
    if !root.join("Cargo.toml").is_file() {
        bail!("请在仓库根目录运行 cargo xtask");
    }
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("package") => {
            let skip_build = args.iter().any(|a| a == "--skip-build");
            cmd_package(&root, skip_build)
        }
        Some("manifest") => {
            let tag = flag_value(&args, "--tag").context("manifest 需要 --tag v<版本>")?;
            let notes = flag_value(&args, "--notes").context("manifest 需要 --notes <文件>")?;
            cmd_manifest(&root, &tag, Path::new(&notes))
        }
        Some("cnb-release") => {
            let tag = flag_value(&args, "--tag").context("cnb-release 需要 --tag v<版本>")?;
            cmd_cnb_release(&root, &tag)
        }
        _ => {
            println!(
                "用法：\n  cargo xtask package [--skip-build]\n  cargo xtask manifest --tag v<版本> --notes CHANGELOG.md\n  cargo xtask cnb-release --tag v<版本>   # 需 CNB_TOKEN；把 Release+附件同步到 CNB"
            );
            Ok(())
        }
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn read_version(root: &Path) -> Result<String> {
    let manifest = fs::read_to_string(root.join("Cargo.toml")).context("读取 Cargo.toml 失败")?;
    let doc: toml::Value = toml::from_str(&manifest).context("解析 Cargo.toml 失败")?;
    doc["package"]["version"]
        .as_str()
        .map(str::to_owned)
        .context("Cargo.toml 缺少 package.version")
}

fn host_target() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macos-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else {
        "unknown"
    }
}

fn exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "dsh-marina.exe"
    } else {
        "dsh-marina"
    }
}

/// 公开只读下载基址（GitHub Releases；CNB 仅作代码/清单镜像）。
fn release_url(version: &str, file: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/v{version}/{file}")
}

fn load_config(root: &Path, version: &str, exe: &Path) -> Result<Config> {
    let manifest = fs::read_to_string(root.join("Cargo.toml"))?;
    let doc: toml::Value = toml::from_str(&manifest)?;
    let mut pkg = doc["package"]["metadata"]["packager"].clone();
    let table = pkg
        .as_table_mut()
        .context("Cargo.toml 缺少 [package.metadata.packager]")?;
    table.insert("version".into(), toml::Value::String(version.to_owned()));
    let mut bin = toml::map::Map::new();
    bin.insert(
        "path".into(),
        toml::Value::String(exe.display().to_string()),
    );
    bin.insert("main".into(), toml::Value::Boolean(true));
    table.insert(
        "binaries".into(),
        toml::Value::Array(vec![toml::Value::Table(bin)]),
    );
    let mut config = Config::deserialize(pkg).context("解析 packager 配置失败")?;
    config.out_dir = root.join("target").join("packager");
    Ok(config)
}

fn sha256_hex(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_sha256_file(target: &Path, hash: &str, name: &str) -> Result<()> {
    let sidecar = target.with_file_name(format!("{name}.sha256"));
    fs::write(&sidecar, format!("{hash}  {name}"))
        .with_context(|| format!("写入 {} 失败", sidecar.display()))?;
    Ok(())
}

fn find_output(outputs: &[PackageOutput], format: PackageFormat, ext: &str) -> Result<PathBuf> {
    outputs
        .iter()
        .filter(|o| o.format == format)
        .flat_map(|o| o.paths.iter())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
        .cloned()
        .with_context(|| format!("打包产物里找不到 {ext}（{format:?}）"))
}

fn find_app_dir(outputs: &[PackageOutput]) -> Result<PathBuf> {
    outputs
        .iter()
        .filter(|o| o.format == PackageFormat::App)
        .flat_map(|o| o.paths.iter())
        .find(|p| p.is_dir())
        .cloned()
        .context("打包产物里找不到 .app 目录")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// 单个平台的清单片段，由 `package` 写出、`manifest` 合并。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fragment {
    target: String,
    version: String,
    /// 更新包下载地址。
    url: String,
    /// 更新包的 SHA-256（hex）。字段名沿用 updater schema 的 `signature`，
    /// 但自更新只做哈希校验、不再做 minisign 验签，故无需任何私钥。
    signature: String,
    /// `nsis` / `app`。
    format: String,
    /// 旧客户端兼容资产（Windows 安装器 exe + sha256）。
    #[serde(default)]
    legacy_assets: Vec<LegacyAsset>,
}

fn cmd_package(root: &Path, skip_build: bool) -> Result<()> {
    let version = read_version(root)?;
    let target = host_target();
    let exe = root.join("target").join("release").join(exe_name());
    if !skip_build {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release"])
            .current_dir(root)
            .status()
            .context("无法启动 cargo build")?;
        if !status.success() {
            bail!("cargo build --release 失败");
        }
    }
    if !exe.is_file() {
        bail!(
            "找不到 {}，先构建发布版（或去掉 --skip-build）",
            exe.display()
        );
    }

    let dist = root.join("dist");
    fs::create_dir_all(&dist)?;
    let mut config = load_config(root, &version, &exe)?;

    let mut fragment = Fragment {
        target: target.to_owned(),
        version: version.clone(),
        url: String::new(),
        signature: String::new(),
        format: String::new(),
        legacy_assets: Vec::new(),
    };

    if cfg!(target_os = "windows") {
        config.formats = Some(vec![PackageFormat::Nsis]);
        let outputs = package(&config).context("NSIS 打包失败")?;
        let setup_src = find_output(&outputs, PackageFormat::Nsis, "exe")?;
        let setup_name = format!("dsh-marina-setup-v{version}-windows-x64.exe");
        let setup_dst = dist.join(&setup_name);
        fs::copy(&setup_src, &setup_dst)?;
        let setup_sha = sha256_hex(&setup_dst)?;
        write_sha256_file(&setup_dst, &setup_sha, &setup_name)?;
        fragment.signature = setup_sha;
        fragment.format = "nsis".into();
        fragment.url = release_url(&version, &setup_name);
        // Legacy (v0.1.2) clients read `assets`; point them at the installer now
        // that the portable edition is no longer published.
        let setup_size = fs::metadata(&setup_dst)?.len();
        let sha_name = format!("{setup_name}.sha256");
        let sha_size = fs::metadata(dist.join(&sha_name))?.len();
        fragment.legacy_assets = vec![
            LegacyAsset {
                name: setup_name.clone(),
                browser_download_url: release_url(&version, &setup_name),
                size: setup_size,
            },
            LegacyAsset {
                name: sha_name.clone(),
                browser_download_url: release_url(&version, &sha_name),
                size: sha_size,
            },
        ];
        println!("Windows 产物：{setup_name}（安装器/更新包）");
    } else if cfg!(target_os = "macos") {
        config.formats = Some(vec![PackageFormat::App, PackageFormat::Dmg]);
        let outputs = package(&config).context("macOS 打包失败")?;
        let app_src = find_app_dir(&outputs)?;
        let dmg_src = find_output(&outputs, PackageFormat::Dmg, "dmg")?;
        let dmg_name = format!("dsh-marina-v{version}-{target}.dmg");
        let dmg_dst = dist.join(&dmg_name);
        fs::copy(&dmg_src, &dmg_dst)?;
        let dmg_sha = sha256_hex(&dmg_dst)?;
        write_sha256_file(&dmg_dst, &dmg_sha, &dmg_name)?;

        let tgz_name = format!("dsh-marina-v{version}-{target}.app.tar.gz");
        let tgz_dst = dist.join(&tgz_name);
        // 不签名：直接用 tar 把 .app 打成 .app.tar.gz（tar 默认保留符号链接）。
        let app_parent = app_src.parent().context("无法定位 .app 父目录")?;
        let app_dir_name = app_src
            .file_name()
            .context("无法定位 .app 目录名")?
            .to_string_lossy()
            .into_owned();
        let tar_status = Command::new("tar")
            .args(["-czf"])
            .arg(&tgz_dst)
            .arg("-C")
            .arg(app_parent)
            .arg(&app_dir_name)
            .status()
            .context("无法启动 tar 打包 .app")?;
        if !tar_status.success() {
            bail!("tar 打包 .app.tar.gz 失败");
        }
        let tgz_sha = sha256_hex(&tgz_dst)?;
        write_sha256_file(&tgz_dst, &tgz_sha, &tgz_name)?;
        fragment.signature = tgz_sha;
        fragment.url = release_url(&version, &tgz_name);
        fragment.format = "app".into();
        println!("macOS 产物：{tgz_name}（更新包）+ {dmg_name}（全新安装）");
    } else {
        bail!("暂不支持的发布平台：{target}（Linux 推迟）");
    }

    let frag_path = dist.join(format!("fragment-{target}.json"));
    fs::write(&frag_path, serde_json::to_string_pretty(&fragment)?)?;
    println!("已写出平台片段 {}", frag_path.display());
    Ok(())
}

/// 从累计式 CHANGELOG 中抽取 `## <version>` 到下一个 `## ` 之间的章节（双语全文）。
fn changelog_section(path: &Path, version: &str) -> Result<String> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取更新日志 {} 失败", path.display()))?;
    if text.contains('\u{FFFD}') || text.contains(['鍙', '銆', '鈥', '锛']) {
        bail!(
            "更新日志疑似乱码（编码读取错误？），请确认 {} 是 UTF-8",
            path.display()
        );
    }
    let mut section = String::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if inside {
                break;
            }
            let head = rest.trim().trim_start_matches('v');
            if head == version || head.starts_with(&format!("{version} ")) {
                inside = true;
            }
            continue;
        }
        if inside {
            section.push_str(line);
            section.push('\n');
        }
    }
    if !inside {
        bail!(
            "{} 中找不到 ## {version} 章节；发版前请先在 CHANGELOG 追加该版本",
            path.display()
        );
    }
    Ok(section.trim().to_owned() + "\n")
}

/// 组装更新清单（纯函数，便于单测）。
fn assemble_manifest(
    fragments: &[Fragment],
    tag: &str,
    notes: &str,
    version: &str,
    pub_date: &str,
) -> Value {
    let mut platforms = Map::new();
    let mut assets: Vec<Value> = Vec::new();
    for f in fragments {
        platforms.insert(
            f.target.clone(),
            json!({ "url": f.url, "signature": f.signature, "format": f.format }),
        );
        for a in &f.legacy_assets {
            assets.push(json!({
                "name": a.name,
                "browser_download_url": a.browser_download_url,
                "size": a.size,
            }));
        }
    }
    json!({
        // —— 旧客户端（v0.1.2）兼容字段 ——
        "tag_name": tag,
        "html_url": format!("https://github.com/{REPO}/releases/tag/{tag}"),
        "body": notes,
        "assets": assets,
        // —— cargo-packager-updater 新字段 ——
        "version": version,
        "notes": notes,
        "pub_date": pub_date,
        "platforms": platforms,
    })
}

fn cmd_manifest(root: &Path, tag: &str, notes_file: &Path) -> Result<()> {
    let version = tag.trim_start_matches('v').to_owned();
    semver::Version::parse(&version).context("--tag 版本号不是合法 semver")?;
    let dist = root.join("dist");
    let mut fragments = Vec::new();
    if dist.is_dir() {
        for entry in fs::read_dir(&dist)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("fragment-") && name.ends_with(".json") {
                let text = fs::read_to_string(&path)?;
                let frag: Fragment = serde_json::from_str(&text)
                    .with_context(|| format!("解析片段 {} 失败", path.display()))?;
                fragments.push(frag);
            }
        }
    }
    if fragments.is_empty() {
        bail!("dist/ 下没有任何 fragment-*.json，先在各平台运行 cargo xtask package");
    }
    fragments.sort_by(|a, b| a.target.cmp(&b.target));
    let notes = changelog_section(notes_file, &version)?;
    let pub_date = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let manifest = assemble_manifest(&fragments, tag, &notes, &version, &pub_date);
    let out = root.join("update.json");
    fs::write(&out, serde_json::to_string_pretty(&manifest)? + "\n")?;
    // 供 CI 的 Release body 使用（只含当前版本章节）。
    fs::write(root.join("release-body.md"), &notes)?;
    println!(
        "已写入 {}（{tag}，{} 个平台）",
        out.display(),
        fragments.len()
    );
    println!(
        "update.json 已生成；推送到 main 后由 CI 自动发布并提交回 main（本工具不做 git 操作）。"
    );
    Ok(())
}

/// CNB OpenAPI base（与 web 路径同构）。
const CNB_API: &str = "https://api.cnb.cool";

fn cnb_agent() -> Result<ureq::Agent> {
    let mut builder = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(600));
    if let Ok(proxy) = env::var("HTTPS_PROXY").or_else(|_| env::var("https_proxy")) {
        builder = builder.proxy(ureq::Proxy::new(&proxy).context("HTTPS_PROXY 地址无效")?);
    }
    Ok(builder.build())
}

/// 把当前版本的 Release 与全部附件同步到 CNB（CNB 仅作镜像，下载基址仍是 GitHub）。
/// 需要 `CNB_TOKEN`（CNB 个人令牌，OpenAPI 权限）。本地缺的附件先从 GitHub Release 下载。
fn cmd_cnb_release(root: &Path, tag: &str) -> Result<()> {
    let token = env::var("CNB_TOKEN").context("需要 CNB_TOKEN（CNB 个人令牌）")?;
    let auth = format!("Bearer {token}");
    let version = tag.trim_start_matches('v');
    let agent = cnb_agent()?;
    let manifest: Value = serde_json::from_str(&fs::read_to_string(root.join("update.json"))?)
        .context("解析 update.json 失败")?;
    let notes = manifest["body"].as_str().unwrap_or_default().to_owned();

    let win_exe = format!("dsh-marina-setup-v{version}-windows-x64.exe");
    let mac_tgz = format!("dsh-marina-v{version}-macos-aarch64.app.tar.gz");
    let mac_dmg = format!("dsh-marina-v{version}-macos-aarch64.dmg");
    let names = vec![
        win_exe.clone(),
        format!("{win_exe}.sha256"),
        mac_tgz.clone(),
        format!("{mac_tgz}.sha256"),
        mac_dmg.clone(),
        format!("{mac_dmg}.sha256"),
        "update.json".to_owned(),
    ];
    let dist = root.join("dist");
    fs::create_dir_all(&dist)?;
    let local_path = |name: &str| -> PathBuf {
        if name == "update.json" {
            root.join("update.json")
        } else {
            dist.join(name)
        }
    };
    for name in &names {
        if name == "update.json" {
            continue; // 用仓库根（CI 回写版）的 update.json
        }
        // 始终从 GitHub Release 拉取权威产物，保证 CNB 镜像与 GitHub 字节一致。
        let local = local_path(name);
        println!("拉取 GitHub 资产 {name} …");
        let resp = agent
            .get(&release_url(version, name))
            .call()
            .with_context(|| format!("下载 {name} 失败"))?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .read_to_end(&mut bytes)
            .with_context(|| format!("读取 {name} 流失败"))?;
        fs::write(&local, &bytes)?;
    }

    let release_id = match agent
        .get(&format!("{CNB_API}/{REPO}/-/releases/tags/{tag}"))
        .set("Authorization", &auth)
        .set("Accept", "application/json")
        .call()
    {
        Ok(resp) => resp
            .into_json::<Value>()?
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Err(ureq::Error::Status(404, _)) => {
            let resp = agent
                .post(&format!("{CNB_API}/{REPO}/-/releases"))
                .set("Authorization", &auth)
                .set("Accept", "application/json")
                .send_json(json!({
                    "tag_name": tag,
                    "name": format!("DSH Marina {tag}"),
                    "body": notes,
                    "target_commitish": "main",
                    "make_latest": "true",
                }))
                .context("创建 CNB Release 失败")?;
            resp.into_json::<Value>()?
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        }
        Err(e) => return Err(anyhow::anyhow!("查询 CNB Release 失败：{e}")),
    };
    ensure!(!release_id.is_empty(), "CNB Release id 为空");

    for name in &names {
        let local = local_path(name);
        let size = fs::metadata(&local)?.len();
        let resp = agent
            .post(&format!(
                "{CNB_API}/{REPO}/-/releases/{release_id}/asset-upload-url"
            ))
            .set("Authorization", &auth)
            .set("Accept", "application/json")
            .send_json(json!({ "asset_name": name, "size": size, "overwrite": true }))
            .with_context(|| format!("申请 {name} 上传地址失败"))?;
        let urls: Value = resp.into_json()?;
        let upload_url = urls["upload_url"].as_str().context("缺少 upload_url")?;
        let verify_url = urls["verify_url"].as_str().context("缺少 verify_url")?;
        let bytes = fs::read(&local)?;
        agent
            .put(upload_url)
            .send_bytes(&bytes)
            .with_context(|| format!("上传 {name} 失败"))?;
        agent
            .post(verify_url)
            .set("Authorization", &auth)
            .set("Accept", "application/json")
            .call()
            .with_context(|| format!("确认 {name} 上传失败"))?;
        println!("已上传 {name}（{size}B）");
    }
    println!("CNB Release {tag} 同步完成（{} 个附件）。", names.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changelog_section_extracts_only_the_requested_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CHANGELOG.md");
        fs::write(
            &path,
            "# Changelog\n\n## 0.2.0\n\nnew stuff\n\n## 0.1.3\n\nold stuff\n中文\n\n## 0.1.2\n\nolder\n",
        )
        .unwrap();
        let section = changelog_section(&path, "0.1.3").unwrap();
        assert!(section.contains("old stuff"));
        assert!(section.contains("中文"));
        assert!(!section.contains("new stuff"));
        assert!(!section.contains("older"));
        assert!(changelog_section(&path, "9.9.9").is_err());
    }

    fn win_fragment() -> Fragment {
        Fragment {
            target: "windows-x86_64".into(),
            version: "0.2.0".into(),
            url: "https://github.com/QuaonY/dsh-marina/releases/download/v0.2.0/setup.exe".into(),
            signature: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            format: "nsis".into(),
            legacy_assets: vec![LegacyAsset {
                name: "dsh-marina-setup-v0.2.0-windows-x64.exe".into(),
                browser_download_url: "https://example.com/p.exe".into(),
                size: 123,
            }],
        }
    }

    fn mac_fragment() -> Fragment {
        Fragment {
            target: "macos-aarch64".into(),
            version: "0.2.0".into(),
            url: "https://github.com/QuaonY/dsh-marina/releases/download/v0.2.0/a.app.tar.gz"
                .into(),
            signature: "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".into(),
            format: "app".into(),
            legacy_assets: vec![],
        }
    }

    #[test]
    fn manifest_keeps_old_and_new_fields() {
        let frags = vec![win_fragment(), mac_fragment()];
        let m = assemble_manifest(
            &frags,
            "v0.2.0",
            "## 变化\n- 多端\n",
            "0.2.0",
            "2026-11-20T00:00:00Z",
        );
        // 旧字段
        assert_eq!(m["tag_name"], "v0.2.0");
        assert!(
            m["html_url"]
                .as_str()
                .unwrap()
                .contains("releases/tag/v0.2.0")
        );
        assert_eq!(m["assets"].as_array().unwrap().len(), 1);
        // 新字段
        assert_eq!(m["version"], "0.2.0");
        assert_eq!(m["pub_date"], "2026-11-20T00:00:00Z");
        let platforms = m["platforms"].as_object().unwrap();
        assert_eq!(platforms.len(), 2);
        assert_eq!(platforms["windows-x86_64"]["format"], "nsis");
        assert_eq!(platforms["macos-aarch64"]["format"], "app");
        assert_eq!(
            platforms["macos-aarch64"]["signature"],
            "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"
        );
    }

    #[test]
    fn sha256_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.bin");
        fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_hex(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn notes_guard_rejects_mojibake() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("CHANGELOG.md");
        fs::write(&f, "## 0.1.3\n- 正常\n").unwrap();
        assert!(changelog_section(&f, "0.1.3").is_ok());
        fs::write(&f, "## 0.1.3\n鍙銆锛").unwrap();
        assert!(changelog_section(&f, "0.1.3").is_err());
    }

    #[test]
    fn version_is_read_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(read_version(dir.path()).unwrap(), "1.2.3");
    }
}
