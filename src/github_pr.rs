//! GitHub PR 创建相关功能：把整合包提交到社区仓库。
//!
//! 所有函数都是阻塞调用，必须在后台线程执行。

use base64::Engine;
use reqwest::blocking::RequestBuilder;
use serde::Deserialize;
use std::time::Duration;

use crate::catalog::CatalogEntry;

pub const UPSTREAM_OWNER: &str = "QuaonY";
pub const UPSTREAM_REPO: &str = "dsh-marina-packs";
pub const UPSTREAM_BRANCH: &str = "master";
pub const SHARED_PACKS_REPO: &str = "dsh-marina-shared-packs";

fn api(builder: RequestBuilder, token: &str) -> RequestBuilder {
    builder
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
}

fn failure(action: &str, response: reqwest::blocking::Response) -> String {
    let status = response.status();
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    let detail = response
        .text()
        .ok()
        .and_then(|t| serde_json::from_str::<Message>(&t).ok())
        .map(|m| m.message)
        .unwrap_or_default();
    // GitHub 的 message 是英文；常见原因翻成中文，其余原样保留并写进日志。
    let reason = if detail.contains("Bad credentials") {
        "GitHub 授权已失效，请重新连接账号".to_owned()
    } else if detail.contains("Not Found") {
        "找不到对应的仓库或文件".to_owned()
    } else if detail.contains("rate limit") {
        "GitHub 接口调用过于频繁，请稍后再试".to_owned()
    } else if detail.contains("Validation Failed") {
        "GitHub 拒绝了这次提交".to_owned()
    } else if detail.contains("already exists") {
        "同名内容已经存在".to_owned()
    } else {
        // 其余 message 是 GitHub 的英文原文，不直接展示给用户：
        // 日志里已记录原文，界面上只保留中文说明。
        String::new()
    };
    let action = action.trim();
    if !detail.is_empty() {
        dsh_marina::applog::warn(&format!("[GitHub] {action}：HTTP {status}，{detail}"));
    }
    if reason.is_empty() {
        format!("{action}失败（HTTP {status}）")
    } else {
        format!("{action}失败（HTTP {status}）：{reason}")
    }
}

/// 确保用户有一个可写的仓库副本，返回 PR 的源仓库 owner。
/// 仓库所有者直接使用上游仓库，其他人使用（必要时新建的）fork。
pub fn prepare_fork(token: &str, username: &str) -> Result<String, String> {
    if username.eq_ignore_ascii_case(UPSTREAM_OWNER) {
        return Ok(UPSTREAM_OWNER.to_owned());
    }
    let client = crate::github::client()?;

    let response = api(
        client.post(format!(
            "https://api.github.com/repos/{UPSTREAM_OWNER}/{UPSTREAM_REPO}/forks"
        )),
        token,
    )
    .json(&serde_json::json!({ "default_branch_only": true }))
    .send()
    .map_err(|e| format!("Fork 请求失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure("Fork 仓库", response));
    }

    // Fork 是异步创建的，等它可用
    let fork_url = format!("https://api.github.com/repos/{username}/{UPSTREAM_REPO}");
    let mut ready = false;
    for _ in 0..30 {
        if api(client.get(&fork_url), token)
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if !ready {
        return Err("等待 Fork 仓库创建超时，请稍后重试".to_owned());
    }

    // 已有的 fork 可能落后于上游，尽量同步（失败不影响后续流程）
    let _ = api(client.post(format!("{fork_url}/merge-upstream")), token)
        .json(&serde_json::json!({ "branch": UPSTREAM_BRANCH }))
        .send();

    Ok(username.to_owned())
}

/// 基于上游主分支的最新提交，在 `owner` 的仓库中新建分支。
pub fn create_branch(token: &str, owner: &str, branch: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Object {
        sha: String,
    }
    #[derive(Deserialize)]
    struct Ref {
        object: Object,
    }
    let client = crate::github::client()?;

    let response = api(
        client.get(format!(
            "https://api.github.com/repos/{UPSTREAM_OWNER}/{UPSTREAM_REPO}/git/ref/heads/{UPSTREAM_BRANCH}"
        )),
        token,
    )
    .send()
    .map_err(|e| format!("获取分支信息失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure("获取上游分支", response));
    }
    let sha = response
        .json::<Ref>()
        .map_err(|e| format!("解析分支信息失败：{e}"))?
        .object
        .sha;

    let response = api(
        client.post(format!(
            "https://api.github.com/repos/{owner}/{UPSTREAM_REPO}/git/refs"
        )),
        token,
    )
    .json(&serde_json::json!({ "ref": format!("refs/heads/{branch}"), "sha": sha }))
    .send()
    .map_err(|e| format!("创建分支请求失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure("创建分支", response));
    }
    Ok(())
}

/// 在 `owner` 仓库的 `branch` 上创建或覆盖文件。
pub fn upload_file(
    token: &str,
    owner: &str,
    branch: &str,
    path: &str,
    content: &[u8],
    message: &str,
) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Existing {
        sha: String,
    }
    let client = crate::github::client()?;
    let url = format!("https://api.github.com/repos/{owner}/{UPSTREAM_REPO}/contents/{path}");

    // 覆盖已有文件时 GitHub 要求提供原文件的 sha
    let existing = api(client.get(&url).query(&[("ref", branch)]), token)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<Existing>().ok())
        .map(|e| e.sha);

    let mut body = serde_json::json!({
        "message": message,
        "content": base64::engine::general_purpose::STANDARD.encode(content),
        "branch": branch,
    });
    if let Some(sha) = existing {
        body["sha"] = serde_json::Value::String(sha);
    }

    let response = api(client.put(&url), token)
        .json(&body)
        .send()
        .map_err(|e| format!("上传文件请求失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure(&format!("上传 {path}"), response));
    }
    Ok(())
}

/// 向上游仓库发起 Pull Request，返回 PR 页面地址。
pub fn create_pull_request(
    token: &str,
    owner: &str,
    branch: &str,
    title: &str,
    body: &str,
) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Pull {
        html_url: String,
    }
    let head = if owner.eq_ignore_ascii_case(UPSTREAM_OWNER) {
        branch.to_owned()
    } else {
        format!("{owner}:{branch}")
    };

    let response = api(
        crate::github::client()?.post(format!(
            "https://api.github.com/repos/{UPSTREAM_OWNER}/{UPSTREAM_REPO}/pulls"
        )),
        token,
    )
    .json(
        &serde_json::json!({ "title": title, "body": body, "head": head, "base": UPSTREAM_BRANCH }),
    )
    .send()
    .map_err(|e| format!("创建 PR 请求失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure("创建 Pull Request", response));
    }
    response
        .json::<Pull>()
        .map(|p| p.html_url)
        .map_err(|e| format!("解析响应失败：{e}"))
}

/// 检查用户是否有指定的仓库
pub fn check_repo_exists(token: &str, owner: &str, repo: &str) -> Result<bool, String> {
    dsh_marina::applog::info(&format!("[GitHub] 检查仓库 {}/{}", owner, repo));
    let client = crate::github::client()?;

    let response = api(
        client.get(format!("https://api.github.com/repos/{owner}/{repo}")),
        token,
    )
    .send()
    .map_err(|e| format!("检查仓库失败：{e}"))?;

    Ok(response.status().is_success())
}

/// 创建一个新的公开仓库
pub fn create_repo(token: &str, repo_name: &str, description: &str) -> Result<(), String> {
    dsh_marina::applog::info(&format!("[GitHub] 创建仓库 {}", repo_name));
    let client = crate::github::client()?;

    let response = api(client.post("https://api.github.com/user/repos"), token)
        .json(&serde_json::json!({
            "name": repo_name,
            "description": description,
            "private": false,
            "auto_init": true,
        }))
        .send()
        .map_err(|e| format!("创建仓库失败：{e}"))?;

    if !response.status().is_success() {
        return Err(failure("创建仓库", response));
    }

    Ok(())
}

/// 按 tag 查已存在的 Release，没有则返回 `None`。
fn release_id_by_tag(
    client: &reqwest::blocking::Client,
    token: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) -> Result<Option<u64>, String> {
    #[derive(Deserialize)]
    struct Release {
        id: u64,
    }

    let response = api(
        client.get(format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}"
        )),
        token,
    )
    .send()
    .map_err(|e| format!("查询 Release 失败：{e}"))?;

    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(failure("查询 Release", response));
    }
    let release = response
        .json::<Release>()
        .map_err(|e| format!("解析响应失败：{e}"))?;
    Ok(Some(release.id))
}

/// 创建一个 Release；如果同 tag 的 Release 已经存在，复用它。
pub fn create_release(
    token: &str,
    owner: &str,
    repo: &str,
    tag: &str,
    name: &str,
    body: &str,
) -> Result<u64, String> {
    dsh_marina::applog::info(&format!(
        "[GitHub] 创建 Release {}/{} tag: {}",
        owner, repo, tag
    ));
    let client = crate::github::client()?;

    // 上一次分享可能在创建 Release 之后才失败；重试时复用同一个 Release。
    if let Some(id) = release_id_by_tag(&client, token, owner, repo, tag)? {
        dsh_marina::applog::info(&format!("[GitHub] 复用已存在的 Release #{id}"));
        return Ok(id);
    }

    #[derive(Deserialize)]
    struct Release {
        id: u64,
    }

    let response = api(
        client.post(format!(
            "https://api.github.com/repos/{owner}/{repo}/releases"
        )),
        token,
    )
    .json(&serde_json::json!({
        "tag_name": tag,
        "name": name,
        "body": body,
        "draft": false,
        "prerelease": false,
    }))
    .send()
    .map_err(|e| format!("创建 Release 失败：{e}"))?;

    if !response.status().is_success() {
        // 两边同时创建时 GitHub 返回 422：如果现在能查到，就用它。
        if response.status().as_u16() == 422
            && let Some(id) = release_id_by_tag(&client, token, owner, repo, tag)?
        {
            return Ok(id);
        }
        return Err(failure("创建 Release", response));
    }

    let release = response
        .json::<Release>()
        .map_err(|e| format!("解析响应失败：{e}"))?;
    Ok(release.id)
}

/// 删掉同名资产，避免重新上传时返回 422 `already_exists`。
fn delete_release_asset(
    client: &reqwest::blocking::Client,
    token: &str,
    owner: &str,
    repo: &str,
    release_id: u64,
    filename: &str,
) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Asset {
        id: u64,
        name: String,
    }

    let response = api(
        client.get(format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/{release_id}/assets"
        )),
        token,
    )
    .send()
    .map_err(|e| format!("查询 Release 文件失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure("查询 Release 文件", response));
    }
    let assets = response
        .json::<Vec<Asset>>()
        .map_err(|e| format!("解析响应失败：{e}"))?;
    for asset in assets.into_iter().filter(|asset| asset.name == filename) {
        let response = api(
            client.delete(format!(
                "https://api.github.com/repos/{owner}/{repo}/releases/assets/{}",
                asset.id
            )),
            token,
        )
        .send()
        .map_err(|e| format!("删除旧的 Release 文件失败：{e}"))?;
        if !response.status().is_success() {
            return Err(failure("删除旧的 Release 文件", response));
        }
    }
    Ok(())
}

/// 上传文件到 Release
pub fn upload_release_asset(
    token: &str,
    owner: &str,
    repo: &str,
    release_id: u64,
    filename: &str,
    content: Vec<u8>,
) -> Result<String, String> {
    dsh_marina::applog::info(&format!(
        "[GitHub] 上传 {} 到 Release #{}",
        filename, release_id
    ));
    let client = crate::github::client()?;

    #[derive(Deserialize)]
    struct Asset {
        browser_download_url: String,
    }

    let response = client
        .post(format!(
            "https://uploads.github.com/repos/{owner}/{repo}/releases/{release_id}/assets?name={filename}"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Content-Type", "application/zip")
        .body(content.clone())
        .send()
        .map_err(|e| format!("上传文件失败：{e}"))?;

    let response = if response.status().as_u16() == 422 {
        // 复用旧 Release 时同名文件还在：删掉再传一次。
        delete_release_asset(&client, token, owner, repo, release_id, filename)?;
        client
            .post(format!(
                "https://uploads.github.com/repos/{owner}/{repo}/releases/{release_id}/assets?name={filename}"
            ))
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", format!("Bearer {token}"))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Content-Type", "application/zip")
            .body(content)
            .send()
            .map_err(|e| format!("上传文件失败：{e}"))?
    } else {
        response
    };

    if !response.status().is_success() {
        return Err(failure("上传 Release 文件", response));
    }

    let asset = response
        .json::<Asset>()
        .map_err(|e| format!("解析响应失败：{e}"))?;
    Ok(asset.browser_download_url)
}

/// 读取社区仓库的 index.json，加入或替换本整合包的条目，再写回 `branch`。
///
/// 社区仓库的目录格式是：
/// `{ "version": 1, "packs": [ { id, name, author, description, dsh_version,
///   plugins, url, size, sha256 } ] }`
pub fn update_catalog_index(
    token: &str,
    owner: &str,
    branch: &str,
    entry: &CatalogEntry,
) -> Result<(), String> {
    dsh_marina::applog::info(&format!(
        "[GitHub] 更新社区目录 index.json（条目 {}）",
        entry.id
    ));
    let client = crate::github::client()?;
    let url = format!("https://api.github.com/repos/{owner}/{UPSTREAM_REPO}/contents/index.json");

    #[derive(Deserialize)]
    struct Existing {
        sha: String,
        content: String,
    }

    let existing = api(client.get(&url).query(&[("ref", branch)]), token)
        .send()
        .map_err(|e| format!("读取社区目录失败：{e}"))?;

    let (mut document, sha) = if existing.status().is_success() {
        let file = existing
            .json::<Existing>()
            .map_err(|e| format!("解析社区目录响应失败：{e}"))?;
        let cleaned: String = file
            .content
            .chars()
            .filter(|c| *c != '\n' && *c != '\r')
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .map_err(|e| format!("解码社区目录失败：{e}"))?;
        let text = String::from_utf8(bytes).map_err(|e| format!("社区目录不是 UTF-8：{e}"))?;
        let value: serde_json::Value = if text.trim().is_empty() {
            serde_json::json!({ "version": 1, "packs": [] })
        } else {
            serde_json::from_str(&text)
                .map_err(|e| format!("社区目录 JSON 已损坏，为免覆盖现有整合包已停止：{e}"))?
        };
        (value, Some(file.sha))
    } else if existing.status().as_u16() == 404 {
        (serde_json::json!({ "version": 1, "packs": [] }), None)
    } else {
        return Err(failure("读取社区目录", existing));
    };

    let packs = document
        .get_mut("packs")
        .and_then(|value| value.as_array_mut())
        .ok_or_else(|| "社区目录格式不对：缺少 packs 数组".to_owned())?;
    packs.retain(|item| item.get("id").and_then(|value| value.as_str()) != Some(entry.id.as_str()));
    packs.push(serde_json::to_value(entry).map_err(|e| format!("序列化目录条目失败：{e}"))?);

    let text =
        serde_json::to_string_pretty(&document).map_err(|e| format!("生成社区目录失败：{e}"))?;
    let content = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut body = serde_json::json!({
        "message": format!("Add pack entry: {}", entry.name),
        "content": content,
        "branch": branch,
    });
    if let Some(sha) = sha {
        body["sha"] = serde_json::Value::String(sha);
    }

    let response = api(client.put(&url), token)
        .json(&body)
        .send()
        .map_err(|e| format!("写入社区目录失败：{e}"))?;
    if !response.status().is_success() {
        return Err(failure("更新社区目录", response));
    }
    Ok(())
}
