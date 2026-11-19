//! GitHub API client for authentication and operations.
//!
//! All calls are blocking (like the rest of Marina's networking) and must run
//! on the background executor.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tempfile::NamedTempFile;

const GITHUB_CLIENT_ID: &str = "178c6fc778ccc68e1d6a";

pub(crate) fn client() -> Result<reqwest::blocking::Client, String> {
    // 代理配置（环境变量与系统代理）全部交给 `net::client` 处理；
    // 这里不打印代理地址或凭据，避免敏感信息出现在控制台/日志里。
    dsh_marina::net::client(Duration::from_secs(60 * 10))
        .map_err(|e| format!("创建网络客户端失败：{e}"))
}

/// Device Flow 认证响应
#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[allow(dead_code)]
    pub expires_in: u64,
    pub interval: u64,
}

/// Token 响应
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
}

/// GitHub 用户信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    pub avatar_url: String,
    pub name: Option<String>,
}

/// 发起 Device Flow 认证
pub fn start_device_flow() -> Result<DeviceCodeResponse, String> {
    let client = client()?;

    let response = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", GITHUB_CLIENT_ID), ("scope", "public_repo")])
        .send()
        .map_err(|_| "无法连接到 GitHub 登录服务，请稍后重试".to_owned())?;

    if !response.status().is_success() {
        return Err("无法连接到 GitHub 登录服务，请稍后重试".to_owned());
    }

    let text = response
        .text()
        .map_err(|_| "无法连接到 GitHub 登录服务".to_owned())?;

    serde_json::from_str::<DeviceCodeResponse>(&text)
        .map_err(|_| "GitHub 登录服务暂时不可用，请稍后重试".to_owned())
}

fn error_code(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct ErrorResponse {
        error: String,
    }
    serde_json::from_str::<ErrorResponse>(text)
        .ok()
        .map(|e| e.error)
}

/// 轮询检查授权状态
pub fn poll_device_token(device_code: &str) -> Result<TokenResponse, PollError> {
    let response = client()
        .map_err(PollError::Network)?
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", GITHUB_CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .map_err(|e| PollError::Network(format!("网络请求失败：{e}")))?;

    let text = response
        .text()
        .map_err(|e| PollError::Network(format!("读取响应失败：{e}")))?;

    if let Some(code) = error_code(&text) {
        return Err(match code.as_str() {
            "authorization_pending" => PollError::Pending,
            "slow_down" => PollError::SlowDown,
            "expired_token" => PollError::Expired,
            "access_denied" => PollError::Denied,
            _ => PollError::Other(code),
        });
    }

    serde_json::from_str::<TokenResponse>(&text)
        .map_err(|e| PollError::Other(format!("解析响应失败：{e}")))
}

#[derive(Debug)]
pub enum PollError {
    Pending,
    SlowDown,
    Expired,
    Denied,
    Network(String),
    Other(String),
}

/// 获取 GitHub 用户信息
pub fn get_user_info(token: &str) -> Result<GitHubUser, String> {
    let response = client()?
        .get("https://api.github.com/user")
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|e| format!("网络请求失败：{e}"))?;

    if !response.status().is_success() {
        return Err(format!("GitHub API 返回错误：{}", response.status()));
    }

    response
        .json::<GitHubUser>()
        .map_err(|e| format!("解析响应失败：{e}"))
}

/// Marina 自身的最新发布
#[derive(Debug, Clone, Deserialize)]
pub struct LatestRelease {
    pub tag_name: String,
    pub html_url: String,
    /// Release notes in GitHub-flavoured Markdown.
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// One downloadable file attached to a release.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

impl LatestRelease {
    /// The portable executable attached to this release, if any.
    pub fn windows_exe(&self) -> Option<&ReleaseAsset> {
        self.assets
            .iter()
            .find(|asset| asset.name.ends_with("-windows-x64.exe"))
    }

    /// The `...exe.sha256` sibling published next to the executable.
    pub fn windows_exe_sha256(&self) -> Option<&ReleaseAsset> {
        self.assets
            .iter()
            .find(|asset| asset.name.ends_with("-windows-x64.exe.sha256"))
    }
}

/// 查询 DSH Marina 的最新正式版发布；仓库还没有正式版时返回 `Ok(None)`。
pub fn latest_release(token: Option<&str>) -> Result<Option<LatestRelease>, String> {
    let send = |token: Option<&str>| -> Result<reqwest::blocking::Response, String> {
        let mut request = client()?
            .get("https://api.github.com/repos/QuaonY/dsh-marina/releases/latest")
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "dsh-marina");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        request.send().map_err(|e| format!("网络请求失败：{e}"))
    };

    let mut response = send(token)?;
    // 保存的 token 过期或失效时退回匿名请求，不因此让检查失败。
    if response.status().as_u16() == 401 && token.is_some() {
        response = send(None)?;
    }

    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(match status.as_u16() {
            403 | 429 => "GitHub 请求过于频繁，请稍后再试".to_owned(),
            401 => "GitHub 账号已失效，请重新连接".to_owned(),
            _ => format!("GitHub 返回 {status}"),
        });
    }

    response
        .json::<LatestRelease>()
        .map(Some)
        .map_err(|e| format!("解析响应失败：{e}"))
}

/// CNB 的 release 镜像清单。CNB 的 release API 需要登录令牌，所以发布时
/// 同一个小 JSON 也会写进仓库，由它的公开只读路径提供。
const CNB_UPDATE_MANIFEST: &str = "https://cnb.cool/QuaonY/dsh-marina/-/git/raw/main/update.json";

/// 查询 CNB 镜像里的最新版本。返回 `None` 表示还没有发布过清单。
pub fn latest_release_cnb() -> Result<Option<LatestRelease>, String> {
    let response = client()?
        .get(CNB_UPDATE_MANIFEST)
        .header("User-Agent", "dsh-marina")
        .send()
        .map_err(|e| format!("网络请求失败：{e}"))?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("CNB 返回 {status}"));
    }
    let text = response.text().map_err(|e| format!("读取响应失败：{e}"))?;
    let release: LatestRelease =
        serde_json::from_str(&text).map_err(|e| format!("解析响应失败：{e}"))?;
    Ok(Some(release))
}

/// 按「GitHub 优先，失败回退 CNB」的顺序查最新版本。
///
/// 两边都失败时把两个错误都带回去，用户才知道到底卡在哪。
pub fn latest_release_any(token: Option<&str>) -> Result<Option<LatestRelease>, String> {
    match latest_release(token) {
        Ok(release) => Ok(release),
        Err(github_error) => match latest_release_cnb() {
            Ok(release) => Ok(release),
            Err(cnb_error) => Err(format!("GitHub：{github_error}；CNB：{cnb_error}")),
        },
    }
}

fn token_file(data_root: &Path) -> std::path::PathBuf {
    data_root.join("github_token.json")
}

/// 用当前 Windows 账号加密 / 解密 token（DPAPI）。
///
/// 数据目录被复制到别的机器、别的账号或云同步里时，token 无法解密，
/// Marina 会回到「未登录」状态，而不会把凭据泄露出去。
#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).ok()?,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    let result = unsafe {
        if encrypt {
            CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, 0, &mut out_blob)
        } else {
            CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out_blob)
        }
    };
    if result.is_err() || out_blob.pbData.is_null() {
        return None;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(Some(HLOCAL(out_blob.pbData.cast())));
    }
    Some(bytes)
}

#[cfg(not(windows))]
fn dpapi(_input: &[u8], _encrypt: bool) -> Option<Vec<u8>> {
    None
}

/// 保存 token 到文件；加密不可用时退回明文（旧文件也继续可读）。
pub fn save_token(data_root: &Path, token: &str, user: &GitHubUser) -> Result<(), String> {
    #[derive(Serialize)]
    struct TokenData<'a> {
        token: String,
        encrypted: bool,
        user: &'a GitHubUser,
    }

    let (token, encrypted) = match dpapi(token.as_bytes(), true) {
        Some(blob) => (base64::engine::general_purpose::STANDARD.encode(blob), true),
        None => (token.to_owned(), false),
    };

    let json = serde_json::to_string_pretty(&TokenData {
        token: token.clone(),
        encrypted,
        user,
    })
    .map_err(|e| format!("序列化失败：{e}"))?;

    std::fs::create_dir_all(data_root).map_err(|e| format!("创建数据目录失败：{e}"))?;
    let mut temporary =
        NamedTempFile::new_in(data_root).map_err(|e| format!("保存文件失败：{e}"))?;
    temporary
        .write_all(json.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|e| format!("保存文件失败：{e}"))?;
    temporary
        .persist(token_file(data_root))
        .map(|_| ())
        .map_err(|e| format!("保存文件失败：{}", e.error))
}

/// 从文件加载 token
pub fn load_token(data_root: &Path) -> Option<(String, GitHubUser)> {
    #[derive(Deserialize)]
    struct TokenData {
        token: String,
        #[serde(default)]
        encrypted: bool,
        user: GitHubUser,
    }

    let content = std::fs::read_to_string(token_file(data_root)).ok()?;
    let data: TokenData = serde_json::from_str(&content).ok()?;
    let token = if data.encrypted {
        let blob = base64::engine::general_purpose::STANDARD
            .decode(data.token.as_bytes())
            .ok()?;
        String::from_utf8(dpapi(&blob, false)?).ok()?
    } else {
        data.token
    };
    Some((token, data.user))
}

/// 删除保存的 token
pub fn clear_token(data_root: &Path) {
    let _ = std::fs::remove_file(token_file(data_root));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> GitHubUser {
        GitHubUser {
            login: "octocat".to_owned(),
            avatar_url: "https://example.com/a.png".to_owned(),
            name: None,
        }
    }

    #[test]
    fn round_trips_a_saved_token() {
        let dir = tempfile::tempdir().unwrap();
        save_token(dir.path(), "ghp_exampleTokenValue0123456789", &user()).unwrap();
        let (token, loaded) = load_token(dir.path()).expect("token loads");
        assert_eq!(token, "ghp_exampleTokenValue0123456789");
        assert_eq!(loaded.login, "octocat");
    }

    #[test]
    fn token_is_not_stored_in_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        save_token(dir.path(), "ghp_exampleTokenValue0123456789", &user()).unwrap();
        let content = std::fs::read_to_string(token_file(dir.path())).unwrap();
        assert!(
            !content.contains("ghp_exampleTokenValue0123456789"),
            "{content}"
        );
    }

    #[test]
    fn reads_a_legacy_plain_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let json =
            r#"{"token":"old-token","user":{"login":"octocat","avatar_url":"","name":null}}"#;
        std::fs::write(token_file(dir.path()), json).unwrap();
        let (token, loaded) = load_token(dir.path()).expect("legacy token loads");
        assert_eq!(token, "old-token");
        assert_eq!(loaded.login, "octocat");
    }

    #[test]
    fn clear_token_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        save_token(dir.path(), "t", &user()).unwrap();
        clear_token(dir.path());
        assert!(load_token(dir.path()).is_none());
    }

    #[test]
    fn reads_the_mirror_manifest_and_finds_the_windows_assets() {
        // Same shape as update.json and the GitHub release response.
        // `r###` because the notes start a heading, i.e. contain `"##`.
        let json = r###"{
            "tag_name": "v0.1.2",
            "html_url": "https://github.com/QuaonY/dsh-marina/releases/tag/v0.1.2",
            "body": "## 变化\n\n- 新增自动更新\n",
            "assets": [
                {"name":"dsh-marina-v0.1.2-windows-x64.exe","browser_download_url":"https://example.com/a.exe","size":123},
                {"name":"dsh-marina-v0.1.2-windows-x64.exe.sha256","browser_download_url":"https://example.com/a.exe.sha256","size":99}
            ]
        }"###;
        let release: LatestRelease = serde_json::from_str(json).unwrap();
        assert_eq!(release.tag_name, "v0.1.2");
        assert!(release.body.contains("自动更新"));
        assert_eq!(release.windows_exe().map(|asset| asset.size), Some(123));
        assert!(
            release
                .windows_exe_sha256()
                .is_some_and(|asset| asset.name.ends_with(".sha256"))
        );
    }

    #[test]
    fn a_release_without_assets_is_read_as_empty() {
        let json = r#"{"tag_name":"v0.1.0","html_url":"https://example.com"}"#;
        let release: LatestRelease = serde_json::from_str(json).unwrap();
        assert!(release.windows_exe().is_none());
        assert!(release.body.is_empty());
    }
}
