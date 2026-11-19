//! 自更新：把新版的 Marina exe 换成当前正在运行的这个。
//!
//! Windows 不允许覆盖正在运行的 exe，但允许把它**改名**。所以流程是：
//!
//! 1. 新 exe 下载到 `<数据目录>/updates/`，用发布出来的 `.sha256` 校验；
//! 2. 现在这个 exe 改名为 `dsh-marina.exe.old`；
//! 3. 新 exe 写到原来的位置；
//! 4. 用 `--wait-for <pid>` 启动新进程，它等旧进程退出后接手；
//! 5. 新进程启动时删掉 `.old`。
//!
//! 第 2 步之后任何一步失败，都会把 `.old` 改回来，所以最坏情况是更新没生效，
//! 而不是 Marina 启动不了。

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::download::{self, DownloadError};

/// Files staged under `<data_root>/updates`.
pub fn staging_dir(data_root: &Path) -> PathBuf {
    data_root.join("updates")
}

/// Where a downloaded executable is kept until it is applied.
pub fn staged_exe(data_root: &Path, version: &str) -> PathBuf {
    staging_dir(data_root).join(format!("dsh-marina-{version}-windows-x64.exe"))
}

/// Download a release asset to `destination` and verify its SHA-256.
///
/// `cancel` is polled through `download_with_resume`, so a cancel leaves a
/// `.part` file that a later attempt resumes.
pub fn download_verified(
    url: &str,
    destination: &Path,
    expected_sha256: Option<&str>,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(download::DownloadProgress),
) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建更新目录失败：{e}"))?;
    }
    download::download_with_resume(url, destination, cancel, progress).map_err(|e| match e {
        DownloadError::HttpStatus { status, .. } => format!("下载失败：服务器返回 {status}"),
        other => format!("下载失败：{other}"),
    })?;
    if let Some(expected) = expected_sha256.filter(|value| !value.trim().is_empty()) {
        let actual = sha256_file(destination)?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = fs::remove_file(destination);
            return Err(format!(
                "下载的文件校验不通过（期望 {expected}，实际 {actual}），已删除"
            ));
        }
    }
    Ok(())
}

/// Read the `sha256  filename` line published next to the executable.
pub fn fetch_sha256(url: &str) -> Result<String, String> {
    let response = crate::net::client(std::time::Duration::from_secs(60))
        .map_err(|e| format!("读取校验文件失败：{e}"))?
        .get(url)
        .header("User-Agent", "dsh-marina")
        .send()
        .map_err(|e| format!("读取校验文件失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "读取校验文件失败：服务器返回 {}",
            response.status()
        ));
    }
    let text = response
        .text()
        .map_err(|e| format!("读取校验文件失败：{e}"))?;
    // Accept both "<hash>  <name>" and a bare hash.
    text.split_whitespace()
        .next()
        .map(|hash| hash.to_owned())
        .ok_or_else(|| "校验文件是空的".to_owned())
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("读取文件失败：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("读取文件失败：{e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Path of the file the running executable is renamed to during a swap.
fn previous_exe(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Delete the `.old` copy left behind by a previous update.
///
/// Called at startup: the renamed file is no longer in use by then, so
/// removing it is safe.  Failures are ignored on purpose.
pub fn cleanup_stale_backup() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let backup = previous_exe(&exe);
    if backup.is_file() {
        let _ = fs::remove_file(&backup);
    }
}

/// Swap in a downloaded executable and start the new process.
///
/// The caller is expected to quit right after this returns; the new process
/// waits for this one to exit (`--wait-for`).
pub fn apply_and_restart(staged: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("找不到当前程序位置：{e}"))?;
    if !staged.is_file() {
        return Err("下载的更新文件不见了，请重新下载".to_owned());
    }
    let backup = previous_exe(&exe);

    // A leftover `.old` from an earlier attempt would block the rename.
    if backup.is_file() {
        let _ = fs::remove_file(&backup);
    }

    fs::rename(&exe, &backup).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            "没有权限替换 Marina 的程序文件。请用管理员身份运行，或改用安装包更新".to_owned()
        } else {
            format!("无法把当前程序移开（可能被杀毒软件占用）：{e}")
        }
    })?;

    if let Err(error) = fs::rename(staged, &exe) {
        // Put the working executable back before reporting the failure.
        let _ = fs::rename(&backup, &exe);
        return Err(format!("写入新版本失败，已恢复原版本：{error}"));
    }

    match Command::new(&exe)
        .arg("--wait-for")
        .arg(std::process::id().to_string())
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(error) => {
            // The new file is in place but cannot start: restore the old one.
            let _ = fs::remove_file(&exe);
            let _ = fs::rename(&backup, &exe);
            Err(format!("重启新版本失败，已恢复原版本：{error}"))
        }
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
    use std::io::Write;

    #[test]
    fn hashes_a_file_the_same_way_as_the_release_asset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"hello marina").unwrap();
        drop(file);

        // sha256("hello marina")
        assert_eq!(
            sha256_file(&path).unwrap(),
            "fa21f77de35ea4147c5d31b80d91e111c9e676915d8cd0c3cfa45525224f49ce"
        );
    }

    #[test]
    fn first_line_skips_headings_and_markers() {
        let notes = "## 变化\n\n- 新增自动检查更新\n- 修复一个错别字\n";
        assert_eq!(first_line(notes), "新增自动检查更新");
    }
}
