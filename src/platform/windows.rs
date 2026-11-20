//! Windows backend: named-mutex single instance, Win32 window management,
//! registry auto-start and the original DSH NSIS uninstaller.

use super::{APP_TITLE, home_dir};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IDYES, IsIconic, IsWindowVisible, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK,
    MB_YESNO, MessageBoxW, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
};
use windows::core::PCWSTR;

pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "DSH Marina";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keeps the named mutex alive for the process lifetime.
pub struct SingleInstance {
    handle: HANDLE,
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Wait until `pid` has exited, or `timeout` passes.
///
/// Used by `--wait-for` after an in-place update: the new build must not try to
/// take the single-instance mutex until the build it replaces is really gone.
/// Returns `true` when the process is no longer running.
pub fn wait_for_process_exit(pid: u32, timeout: std::time::Duration) -> bool {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        // The process is already gone, which is what we were waiting for.
        Err(_) => return true,
        Ok(handle) => handle,
    };
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    let waited = unsafe { WaitForSingleObject(handle, millis) };
    unsafe {
        let _ = CloseHandle(handle);
    }
    waited == WAIT_OBJECT_0
}

/// `None` means another Marina process already owns the mutex.
/// `data_root` is unused on Windows (the mutex name is global).
pub fn acquire_single_instance(_data_root: &Path) -> Option<SingleInstance> {
    // Debug builds may use a separate lock so a development copy can run
    // next to the installed Marina (used for UI screenshots).
    let name = if cfg!(debug_assertions)
        && let Some(suffix) = std::env::var_os("DSH_MARINA_DEV_INSTANCE")
    {
        let mut name = std::ffi::OsString::from("Local\\dsh-marina-dev-");
        name.push(suffix);
        name
    } else {
        std::ffi::OsString::from("Local\\dsh-marina")
    };
    let wide: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return None;
    }
    Some(SingleInstance { handle })
}

/// Show and focus an existing Marina window.
pub fn activate_window(title: &str, _data_root: &Path) -> bool {
    let wide = wide_null(title);
    let Ok(window) = (unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) }) else {
        return false;
    };
    reveal(window);
    true
}

/// Bring a window to the front without changing its size.
///
/// `SW_RESTORE` also un-maximizes a maximized window, so only use it when the
/// window is minimized. A visible maximized window is left as it is.
fn reveal(window: HWND) {
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        } else if !IsWindowVisible(window).as_bool() {
            let _ = ShowWindow(window, SW_SHOW);
        }
        let _ = SetForegroundWindow(window);
    }
}

/// Hide a window (by raw HWND value) to the tray without destroying it.
pub fn hide_window(hwnd: isize) {
    let window = HWND(hwnd as *mut core::ffi::c_void);
    unsafe {
        let _ = ShowWindow(window, SW_HIDE);
    }
}

/// Show, restore and focus a window by raw HWND value.
pub fn show_window(hwnd: isize) {
    reveal(HWND(hwnd as *mut core::ffi::c_void));
}

/// Ask whether Marina may stop `running` instances and exit. Blocks the
/// calling thread, so call it off the UI thread.
pub fn confirm_exit(running: usize) -> bool {
    let text = wide_null(&format!(
        "当前有 {running} 个实例正在运行。退出 DSH Marina 会先停止这些实例，是否继续？"
    ));
    let caption = wide_null(APP_TITLE);
    let result = unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        )
    };
    result == IDYES
}

pub fn show_tray_hint() {
    let text = wide_null("DSH Marina 已最小化到系统托盘。单击托盘图标即可恢复窗口。");
    let caption = wide_null(APP_TITLE);
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Show a confirmation dialog with Yes/No buttons.
pub fn confirm(message: &str, title: &str) -> bool {
    let text = wide_null(message);
    let caption = wide_null(title);
    let result = unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        )
    };
    result == IDYES
}

pub fn set_auto_start(enabled: bool) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new("reg.exe");
    if enabled {
        command
            .arg("add")
            .arg(RUN_KEY)
            .arg("/v")
            .arg(RUN_VALUE)
            .arg("/t")
            .arg("REG_SZ")
            .arg("/d")
            .arg(format!("\"{}\" --background", executable.display()))
            .arg("/f");
    } else {
        command
            .arg("delete")
            .arg(RUN_KEY)
            .arg("/v")
            .arg(RUN_VALUE)
            .arg("/f");
    }
    let status = command
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() || !enabled {
        Ok(())
    } else {
        Err(format!(
            "注册表操作失败（退出码 {}）",
            status.code().unwrap_or(-1)
        ))
    }
}

pub fn is_auto_start_enabled() -> bool {
    let status = Command::new("reg.exe")
        .arg("query")
        .arg(RUN_KEY)
        .arg("/v")
        .arg(RUN_VALUE)
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    matches!(status, Ok(status) if status.success())
}

pub fn open_path(path: &Path) {
    let mut command = Command::new("explorer.exe");
    if path.is_file() {
        command.arg(format!("/select,{}", path.display()));
    } else {
        command.arg(path);
    }
    let _ = command.spawn();
}

pub fn external_dsh_home() -> PathBuf {
    home_dir().join(".dsh")
}

pub fn external_data_directory() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|app_data| app_data.join("@deepseek-ai").join("dsh-desktop"))
}

pub fn external_uninstaller(executable: &Path) -> Option<PathBuf> {
    let parent = executable.parent()?;
    for name in [
        "Uninstall DeepSeek Harness.exe",
        "Uninstall DeepSeek Harness Desktop.exe",
    ] {
        let candidate = parent.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Per-user NSIS installs live under `%LOCALAPPDATA%\Programs`.
fn is_per_user_install(path: &Path) -> bool {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .is_some_and(|local| path.starts_with(local.join("Programs")))
}

pub fn uninstall_external(executable: &Path, delete_data: bool) -> Result<(), String> {
    let uninstaller =
        external_uninstaller(executable).ok_or_else(|| "未找到原版 DSH 卸载程序。".to_owned())?;
    let mut command = Command::new(&uninstaller);
    // electron-builder NSIS per-user installs register
    // `QuietUninstallString = "...Uninstall DeepSeek Harness.exe" /currentuser /S`.
    if is_per_user_install(&uninstaller) {
        command.arg("/currentuser");
    }
    let status = command
        .arg("/S")
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| format!("启动卸载程序失败：{error}"))?;
    if !status.success() {
        return Err(format!("卸载程序退出码 {}。", status.code().unwrap_or(-1)));
    }
    if delete_data {
        let home = external_dsh_home();
        if home.is_dir() {
            let _ = std::fs::remove_dir_all(home);
        }
        if let Some(data) = external_data_directory()
            && data.is_dir()
        {
            let _ = std::fs::remove_dir_all(data);
        }
    }
    Ok(())
}

/// Bytes the current user may still write on the drive holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }.ok()?;
    Some(available)
}

/// Total physical RAM in bytes (0 if the query fails).
pub fn total_physical_memory() -> u64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) }.is_ok() {
        status.ullTotalPhys
    } else {
        0
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
