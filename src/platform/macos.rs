//! macOS backend: flock single instance, AppKit window management,
//! LaunchAgent auto-start, Keychain-free shell integration and trashing the
//! original DSH `.app` bundle.
//!
//! Runtime behaviour is verified on CI's macOS runners; this file is
//! compile-checked from other hosts via `cargo check --target *-apple-darwin`.

use super::{APP_TITLE, home_dir};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const LAUNCH_AGENT_LABEL: &str = "com.quaon.dsh-marina";
const SHOW_REQUEST_FILE: &str = "show-request";

/// Keeps an exclusive `flock` on a lock file for the process lifetime.
pub struct SingleInstance {
    _file: std::fs::File,
}

/// Wait until `pid` has exited, or `timeout` passes (`kill(pid, 0)` polling).
pub fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `None` means another Marina process already holds the lock.
pub fn acquire_single_instance(data_root: &Path) -> Option<SingleInstance> {
    use std::os::unix::io::AsRawFd;

    let name = if cfg!(debug_assertions)
        && let Some(suffix) = std::env::var_os("DSH_MARINA_DEV_INSTANCE")
    {
        format!("marina-dev-{}.lock", suffix.to_string_lossy())
    } else {
        "marina.lock".to_owned()
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(data_root.join(name))
        .ok()?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return None;
    }
    Some(SingleInstance { _file: file })
}

/// Ask the running Marina to show its window: queue a request file it polls
/// on its tick, and try to front the process via System Events as well.
pub fn activate_window(_title: &str, data_root: &Path) -> bool {
    let queued = std::fs::write(data_root.join(SHOW_REQUEST_FILE), b"show").is_ok();
    // 打包成 .app 后 System Events 里的进程名可能是 bundle 名「DSH Marina」，
    // 裸二进制则是「dsh-marina」；两个都试。即便都不匹配，上面的请求文件仍会
    // 让运行中的 Marina 在下一个 tick 自行唤回窗口（主路径）。
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(
            "tell application \"System Events\" to set frontmost of (first process whose name is \"dsh-marina\" or name is \"DSH Marina\") to true",
        )
        .output();
    queued
}

/// Consume a queued show request (see [`activate_window`]).
pub fn take_show_request(data_root: &Path) -> bool {
    let file = data_root.join(SHOW_REQUEST_FILE);
    if file.exists() {
        let _ = std::fs::remove_file(&file);
        true
    } else {
        false
    }
}

/// Hide the window owning a raw `NSView *` to the tray without destroying it.
pub fn hide_window(raw: isize) {
    // SAFETY: GPUI invokes window callbacks on the main thread, and `raw` is
    // the live NSView pointer extracted from its raw window handle.
    unsafe {
        let view = &*(raw as *const objc2_app_kit::NSView);
        if let Some(window) = view.window() {
            window.orderOut(None);
        }
    }
}

/// Show, restore and focus the window owning a raw `NSView *`.
pub fn show_window(raw: isize) {
    // SAFETY: same main-thread contract as `hide_window`.
    unsafe {
        let view = &*(raw as *const objc2_app_kit::NSView);
        if let Some(window) = view.window() {
            if window.isMiniaturized() {
                window.deminiaturize(None);
            }
            window.makeKeyAndOrderFront(None);
        }
        #[allow(deprecated)]
        objc2_app_kit::NSRunningApplication::currentApplication().activateWithOptions(
            objc2_app_kit::NSApplicationActivationOptions::ActivateIgnoringOtherApps,
        );
    }
}

/// Ask whether Marina may stop `running` instances and exit. Blocks the
/// calling thread, so call it off the UI thread.
pub fn confirm_exit(running: usize) -> bool {
    confirm(
        &format!("当前有 {running} 个实例正在运行。退出 DSH Marina 会先停止这些实例，是否继续？"),
        APP_TITLE,
    )
}

pub fn show_tray_hint() {
    let _ = rfd::MessageDialog::new()
        .set_title(APP_TITLE)
        .set_description("DSH Marina 已最小化到系统托盘。单击托盘图标即可恢复窗口。")
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Show a confirmation dialog with Yes/No buttons.
pub fn confirm(message: &str, title: &str) -> bool {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(message)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

fn launch_agent_path() -> PathBuf {
    home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

pub fn set_auto_start(enabled: bool) -> Result<(), String> {
    let plist = launch_agent_path();
    let uid = unsafe { libc::getuid() };
    if enabled {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\
             \t<string>{LAUNCH_AGENT_LABEL}</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>{}</string>\n\
             \t\t<string>--background</string>\n\
             \t</array>\n\
             \t<key>RunAtLoad</key>\n\
             \t<true/>\n\
             </dict>\n\
             </plist>\n",
            executable.display()
        );
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&plist, contents).map_err(|e| e.to_string())?;
        // bootstrap is the modern API; fall back to load for older systems.
        let status = Command::new("launchctl")
            .args([
                "bootstrap",
                &format!("gui/{uid}"),
                &plist.display().to_string(),
            ])
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            let _ = Command::new("launchctl")
                .args(["load", "-w", &plist.display().to_string()])
                .status();
        }
        Ok(())
    } else {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{LAUNCH_AGENT_LABEL}")])
            .status();
        let _ = Command::new("launchctl")
            .args(["unload", "-w", &plist.display().to_string()])
            .status();
        match std::fs::remove_file(&plist) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub fn is_auto_start_enabled() -> bool {
    launch_agent_path().is_file()
}

pub fn open_path(path: &Path) {
    let mut command = Command::new("open");
    if path.is_file() {
        command.arg("-R");
    }
    command.arg(path);
    let _ = command.spawn();
}

pub fn external_dsh_home() -> PathBuf {
    home_dir().join(".dsh")
}

pub fn external_data_directory() -> Option<PathBuf> {
    Some(
        home_dir()
            .join("Library")
            .join("Application Support")
            .join("@deepseek-ai")
            .join("dsh-desktop"),
    )
}

/// macOS DSH ships as a `.app` bundle without a separate uninstaller.
pub fn external_uninstaller(_executable: &Path) -> Option<PathBuf> {
    None
}

/// Trash the original DSH `.app` bundle (or the bare executable), optionally
/// removing its data directories as well.
pub fn uninstall_external(executable: &Path, delete_data: bool) -> Result<(), String> {
    let bundle = executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| executable.to_path_buf());
    trash::delete(&bundle).map_err(|e| format!("移到废纸篓失败：{e}"))?;
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

/// Bytes the current user may still write on the volume holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize))
}

/// Total physical RAM in bytes (0 if the query fails).
pub fn total_physical_memory() -> u64 {
    let mut mem = 0u64;
    let mut len = std::mem::size_of::<u64>();
    let name = b"hw.memsize\0";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            (&mut mem as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 { mem } else { 0 }
}
