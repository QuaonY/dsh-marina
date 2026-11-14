//! `dsh://` links (design §3 共享资源表, §14 协议关联).
//!
//! Every DSH instance registers `dsh://` for itself on launch, so the last one
//! started wins.  Marina can optionally take the protocol over: links then go
//! to Marina, which asks which instance should open them.  A second Marina
//! process started by Windows only queues the link in `pending-links.txt`;
//! the running Marina picks it up on its next tick.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const SCHEME: &str = "dsh://";
const MAX_LENGTH: usize = 2048;
const PENDING_FILE: &str = "pending-links.txt";

/// Check that `url` is a `dsh://` link Marina may pass on as one argument.
pub fn validate(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url
        .get(..SCHEME.len())
        .is_none_or(|prefix| !prefix.eq_ignore_ascii_case(SCHEME))
    {
        return Err("不是 dsh:// 链接".to_owned());
    }
    if url.len() > MAX_LENGTH {
        return Err(format!("链接过长（超过 {MAX_LENGTH} 个字符）"));
    }
    if url.chars().any(|c| c.is_control() || c == '"') {
        return Err("链接里有不允许的字符".to_owned());
    }
    Ok(url.to_owned())
}

pub fn pending_file(data_root: &Path) -> PathBuf {
    data_root.join(PENDING_FILE)
}

/// Queue a link for the running Marina.
pub fn enqueue(data_root: &Path, url: &str) -> io::Result<()> {
    let url = validate(url).map_err(io::Error::other)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending_file(data_root))?;
    writeln!(file, "{url}")
}

/// Take every queued link; invalid lines are dropped.
pub fn drain(data_root: &Path) -> Vec<String> {
    let file = pending_file(data_root);
    let Ok(text) = fs::read_to_string(&file) else {
        return Vec::new();
    };
    let _ = fs::remove_file(&file);
    text.lines()
        .filter_map(|line| validate(line).ok())
        .collect()
}

/// `"<exe>" --open-url "%1"`, the command Marina registers.
pub fn marina_command(executable: &Path) -> String {
    format!("\"{}\" --open-url \"%1\"", executable.display())
}

pub fn is_marina_command(command: &str, executable: &Path) -> bool {
    command
        .trim()
        .eq_ignore_ascii_case(&marina_command(executable))
}

#[cfg(windows)]
pub mod registry {
    //! `HKCU\Software\Classes\dsh`, the per-user protocol handler.
    use super::*;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::PCWSTR;

    const CLASS_KEY: &str = r"Software\Classes\dsh";
    const COMMAND_KEY: &str = r"Software\Classes\dsh\shell\open\command";

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A `REG_DWORD` under HKCU.
    pub fn read_dword(key: &str, value: &str) -> Option<u32> {
        let key = wide(key);
        let name = wide(value);
        let mut data = 0u32;
        let mut size = 4u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                windows::Win32::System::Registry::RRF_RT_REG_DWORD,
                None,
                Some((&mut data as *mut u32).cast()),
                Some(&mut size),
            )
        };
        (status == ERROR_SUCCESS).then_some(data)
    }

    /// A `REG_SZ` under HKCU.
    pub fn read(key: &str, value: Option<&str>) -> Option<String> {
        let key = wide(key);
        let name = value.map(wide);
        let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        let mut size = 0u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                name_ptr,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS || size == 0 {
            return None;
        }
        let mut buffer = vec![0u16; size as usize / 2 + 1];
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                name_ptr,
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..end]))
    }

    fn write(key: &str, value: Option<&str>, data: &str) -> Result<(), String> {
        let key = wide(key);
        let name = value.map(wide);
        let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        let data = wide(data);
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                name_ptr,
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("写入注册表失败（错误码 {}）", status.0))
        }
    }

    /// The command currently registered for `dsh://`, if any.
    pub fn current_command() -> Option<String> {
        read(COMMAND_KEY, None)
    }

    /// Point `dsh://` at Marina.
    pub fn register(executable: &Path) -> Result<(), String> {
        write(CLASS_KEY, None, "URL:DSH Protocol")?;
        write(CLASS_KEY, Some("URL Protocol"), "")?;
        write(COMMAND_KEY, None, &marina_command(executable))
    }

    /// Put back the command saved before Marina took over, or remove the
    /// class when there was none.
    pub fn restore(previous: Option<&str>) -> Result<(), String> {
        match previous {
            Some(command) if !command.trim().is_empty() => write(COMMAND_KEY, None, command),
            _ => {
                let key = wide(CLASS_KEY);
                let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) };
                if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                    Ok(())
                } else {
                    Err(format!("删除注册表项失败（错误码 {}）", status.0))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validates_links() {
        assert_eq!(
            validate("  dsh://session/abc?x=1 ").unwrap(),
            "dsh://session/abc?x=1"
        );
        assert!(validate("DSH://a").is_ok());
        assert!(validate("http://a").is_err());
        assert!(validate("dsh:").is_err());
        assert!(validate("dsh://a\" --inject").is_err());
        assert!(validate("dsh://a\nb").is_err());
        assert!(validate(&format!("dsh://{}", "a".repeat(MAX_LENGTH))).is_err());
    }

    #[test]
    fn queues_and_drains_links() {
        let dir = tempdir().unwrap();
        enqueue(dir.path(), "dsh://one").unwrap();
        enqueue(dir.path(), "dsh://two").unwrap();
        assert!(enqueue(dir.path(), "https://bad").is_err());
        fs::OpenOptions::new()
            .append(true)
            .open(pending_file(dir.path()))
            .unwrap()
            .write_all(b"junk\n")
            .unwrap();
        assert_eq!(drain(dir.path()), vec!["dsh://one", "dsh://two"]);
        assert!(drain(dir.path()).is_empty());
    }

    #[test]
    fn recognises_its_own_command() {
        let exe = Path::new(r"C:\Program Files\Marina\dsh-marina.exe");
        assert_eq!(
            marina_command(exe),
            "\"C:\\Program Files\\Marina\\dsh-marina.exe\" --open-url \"%1\""
        );
        assert!(is_marina_command(&marina_command(exe).to_uppercase(), exe));
        assert!(!is_marina_command(
            "\"C:\\DSH\\DeepSeek Harness.exe\" \"%1\"",
            exe
        ));
    }
}
