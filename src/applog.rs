//! `marina.log`: Marina's own log (design §8.1), flushed on exit (§7.6).
//!
//! Plain functions over a process-wide `Mutex<Option<AppLog>>`.  Nothing here
//! panics or returns errors: a log that cannot be written is silently skipped.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub const LOG_FILE: &str = "marina.log";
pub const ROTATED_LOG_FILE: &str = "marina.log.1";
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

static LOG: Mutex<Option<AppLog>> = Mutex::new(None);

/// One log file with size-based rotation to `<name>.1`.
#[derive(Debug)]
pub struct AppLog {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    max_bytes: u64,
}

impl AppLog {
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64) -> Self {
        let path = path.into();
        let mut log = Self {
            path,
            file: None,
            written: 0,
            max_bytes,
        };
        log.reopen();
        log
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn reopen(&mut self) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.written = self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }

    fn rotate(&mut self) {
        self.file = None;
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        let rotated = PathBuf::from(rotated);
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&self.path, &rotated);
        self.reopen();
    }

    pub fn write(&mut self, level: &str, message: &str) {
        let line = format_line(level, message);
        if self.written > 0 && self.written + line.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if self.file.is_none() {
            self.reopen();
        }
        if let Some(file) = self.file.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                self.written += line.len() as u64;
            } else {
                self.file = None;
            }
        }
    }

    pub fn flush(&mut self) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
            let _ = file.sync_data();
        }
    }
}

/// `2026-11-06T12:00:00.123+08:00 INFO message`, one physical line, with
/// `token=` values redacted.
fn format_line(level: &str, message: &str) -> String {
    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
    let message = crate::launch::redact_tokens(message.trim_end())
        .replace("\r\n", " ⏎ ")
        .replace(['\r', '\n'], " ⏎ ");
    format!("{timestamp} {:<5} {message}\n", level.to_ascii_uppercase())
}

fn lock() -> MutexGuard<'static, Option<AppLog>> {
    LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Open (or switch to) `<data_root>/marina.log`.
pub fn init(data_root: &Path) {
    *lock() = Some(AppLog::open(data_root.join(LOG_FILE), MAX_LOG_BYTES));
}

/// Append one line; a no-op before [`init`].
pub fn write(level: &str, message: &str) {
    if let Some(log) = lock().as_mut() {
        log.write(level, message);
    }
}

pub fn info(message: &str) {
    write("INFO", message);
}

pub fn warn(message: &str) {
    write("WARN", message);
}

pub fn error(message: &str) {
    write("ERROR", message);
}

/// Flush to disk; call before the process exits.
pub fn flush() {
    if let Some(log) = lock().as_mut() {
        log.flush();
    }
}

/// Current log path, if initialized.
pub fn path() -> Option<PathBuf> {
    lock().as_ref().map(|log| log.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_timestamped_redacted_lines() {
        let root = tempdir().unwrap();
        let mut log = AppLog::open(root.path().join(LOG_FILE), MAX_LOG_BYTES);
        log.write(
            "info",
            "opened http://127.0.0.1:1/?token=secret&x=1\nsecond",
        );
        log.flush();
        let text = fs::read_to_string(root.path().join(LOG_FILE)).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(" INFO  opened http://127.0.0.1:1/?token=***&x=1 ⏎ second"));
        assert!(!text.contains("secret"));
        assert!(text.as_bytes()[0].is_ascii_digit());
    }

    #[test]
    fn rotates_when_the_file_exceeds_the_limit() {
        let root = tempdir().unwrap();
        let path = root.path().join(LOG_FILE);
        let mut log = AppLog::open(&path, 200);
        for index in 0..10 {
            log.write("INFO", &format!("message number {index}"));
        }
        log.flush();
        let rotated = root.path().join(ROTATED_LOG_FILE);
        assert!(rotated.is_file());
        assert!(fs::metadata(&path).unwrap().len() <= 200);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("message number 9")
        );
    }

    #[test]
    fn global_logger_is_a_noop_before_init_and_writes_after() {
        write("INFO", "dropped");
        let root = tempdir().unwrap();
        init(root.path());
        info("hello");
        flush();
        assert_eq!(path(), Some(root.path().join(LOG_FILE)));
        let text = fs::read_to_string(root.path().join(LOG_FILE)).unwrap();
        assert!(text.contains("hello"));
        *lock() = None;
    }
}
