//! Platform helpers: single-instance guard, tray icon art, auto-start,
//! shell integration and original-DSH uninstall.
//!
//! The shared surface lives here; each OS backend implements the same
//! function signatures in its own module.  Linux is not supported yet, but
//! the split keeps a future `linux.rs` backend a drop-in addition.

use std::path::PathBuf;

pub const APP_TITLE: &str = "DSH Marina";

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

/// The current user's home directory (`%USERPROFILE%` / `$HOME`).
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Total physical RAM in bytes; unsupported platforms report 0.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn total_physical_memory() -> u64 {
    0
}

/// Available physical RAM in bytes; unsupported platforms report 0.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn available_physical_memory() -> u64 {
    0
}

/// Extract the native window handle GPUI exposes, as a raw pointer value.
/// Windows: `HWND`.  macOS: the `NSView *` (its `window` gives the `NSWindow`).
pub fn native_window_handle(handle: raw_window_handle::RawWindowHandle) -> Option<isize> {
    match handle {
        #[cfg(windows)]
        raw_window_handle::RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        #[cfg(target_os = "macos")]
        raw_window_handle::RawWindowHandle::AppKit(handle) => {
            Some(handle.ns_view.as_ptr() as isize)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconState {
    Idle,
    Running,
    Crashed,
}

/// Draw the Marina hexagon tray icon. The badge dot mirrors the tray contract:
/// green while an instance runs, red after a startup crash.
pub fn build_anchor_icon(size: u32, state: TrayIconState) -> Vec<u8> {
    // Load the logo PNG and resize if needed
    let logo_bytes = include_bytes!("../../assets/logo.png");
    let img = image::load_from_memory(logo_bytes)
        .map_err(|e| crate::applog::warn(&format!("读取 logo.png 失败：{e}")))
        .ok()
        .map(|i| i.resize_exact(size, size, image::imageops::FilterType::Lanczos3))
        .map(|i| i.to_rgba8());

    let mut rgba = if let Some(buf) = img {
        buf.into_raw()
    } else {
        // Fallback: Create a simple colored square if logo loading fails
        crate::applog::warn("logo.png 读取失败，改用备用图标");
        let mut buf = vec![0u8; (size * size * 4) as usize];
        // Fill with a recognizable color (blue)
        for i in 0..(size * size) as usize {
            buf[i * 4] = 70; // R
            buf[i * 4 + 1] = 130; // G
            buf[i * 4 + 2] = 180; // B
            buf[i * 4 + 3] = 255; // A
        }
        buf
    };

    let s = size as f32;
    let put = |rgba: &mut [u8], x: i32, y: i32, color: (u8, u8, u8)| {
        if x >= 0 && y >= 0 && (x as u32) < size && (y as u32) < size {
            let index = ((y as u32 * size + x as u32) * 4) as usize;
            if index + 3 < rgba.len() {
                rgba[index] = color.0;
                rgba[index + 1] = color.1;
                rgba[index + 2] = color.2;
                rgba[index + 3] = 255;
            }
        }
    };

    let badge = match state {
        TrayIconState::Idle => None,
        TrayIconState::Running => Some((16, 185, 129)),
        TrayIconState::Crashed => Some((239, 68, 68)),
    };
    if let Some(color) = badge {
        let dot_cx = s * 0.76;
        let dot_cy = s * 0.78;
        let dot_r = (s * 0.13).max(2.0);
        for y in (dot_cy - dot_r - 2.0) as i32..=(dot_cy + dot_r + 2.0) as i32 {
            for x in (dot_cx - dot_r - 2.0) as i32..=(dot_cx + dot_r + 2.0) as i32 {
                let distance = ((x as f32 - dot_cx).powi(2) + (y as f32 - dot_cy).powi(2)).sqrt();
                if distance <= dot_r + 1.5 {
                    put(&mut rgba, x, y, (255, 255, 255));
                }
            }
        }
        for y in (dot_cy - dot_r) as i32..=(dot_cy + dot_r) as i32 {
            for x in (dot_cx - dot_r) as i32..=(dot_cx + dot_r) as i32 {
                let distance = ((x as f32 - dot_cx).powi(2) + (y as f32 - dot_cy).powi(2)).sqrt();
                if distance <= dot_r {
                    put(&mut rgba, x, y, color);
                }
            }
        }
    }
    rgba
}

pub fn current_executable() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| error.to_string())
}

/// Start a fresh Marina that waits for this process to exit (after a data move).
pub fn spawn_restart() -> Result<(), String> {
    let executable = current_executable()?;
    std::process::Command::new(executable)
        .arg("--wait-for")
        .arg(std::process::id().to_string())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法重新启动 Marina：{e}"))
}
