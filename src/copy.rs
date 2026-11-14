//! Cancellable file copying with byte progress.  Copying a large instance or
//! the whole data directory can take minutes, so the UI shows progress and
//! offers cancel (design §FR-1 边界).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Files below this size use `fs::copy` (fast, keeps timestamps); larger
/// ones are copied in chunks so cancel takes effect mid-file.
const CHUNKED_THRESHOLD: u64 = 16 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;

/// Shared between the copying thread and the UI.
#[derive(Debug, Default)]
pub struct CopyProgress {
    copied: AtomicU64,
    total: AtomicU64,
    measured: AtomicBool,
    cancelled: AtomicBool,
}

/// Marker carried inside the `io::Error` returned after a cancel.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("已取消")
    }
}

impl std::error::Error for Cancelled {}

impl CopyProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_total(&self, bytes: u64) {
        self.total.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Called once the size of everything to copy is known.
    pub fn finish_measuring(&self) {
        self.measured.store(true, Ordering::Relaxed);
    }

    pub fn is_measured(&self) -> bool {
        self.measured.load(Ordering::Relaxed)
    }

    /// `(copied, total)` in bytes.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.copied.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            Err(io::Error::other(Cancelled))
        } else {
            Ok(())
        }
    }
}

pub fn is_cancelled(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Cancelled>())
}

/// Copy one file, reporting bytes as they are written.
pub fn copy_file(source: &Path, target: &Path, progress: &CopyProgress) -> io::Result<()> {
    progress.check()?;
    let metadata = fs::metadata(source)?;
    let len = metadata.len();
    if len < CHUNKED_THRESHOLD {
        fs::copy(source, target)?;
        progress.copied.fetch_add(len, Ordering::Relaxed);
        return Ok(());
    }
    let mut input = File::open(source)?;
    let mut output = File::create(target)?;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        if let Err(error) = progress.check() {
            drop(output);
            let _ = fs::remove_file(target);
            return Err(error);
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        progress.copied.fetch_add(read as u64, Ordering::Relaxed);
    }
    output.flush()?;
    if let Ok(modified) = metadata.modified() {
        let _ = output.set_modified(modified);
    }
    Ok(())
}

/// Total size of the regular files under `path`; symlinks and junctions are
/// skipped.
pub fn measure_tree(path: &Path, progress: &CopyProgress) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        progress.check()?;
        let entry = entry?;
        // A junction looks like a directory but points elsewhere: recursing
        // into one can leave the tree or loop forever.
        if is_reparse_point(&entry) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            measure_tree(&entry.path(), progress)?;
        } else if file_type.is_file() {
            progress.add_total(entry.metadata()?.len());
        }
    }
    Ok(())
}

/// Windows marks junctions, symlinks and other links with this attribute.
#[cfg(windows)]
fn is_reparse_point(entry: &fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    entry
        .metadata()
        .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_reparse_point(entry: &fs::DirEntry) -> bool {
    entry
        .file_type()
        .map(|file_type| file_type.is_symlink())
        .unwrap_or(false)
}

/// Copy everything under `source` into `target` (created as needed).
/// Symlinks and junctions are skipped, matching `measure_tree`.
pub fn copy_tree(source: &Path, target: &Path, progress: &CopyProgress) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let source_real = fs::canonicalize(source)?;
    let target_real = fs::canonicalize(target)?;
    if target_real.starts_with(&source_real) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "目标目录不能放在源目录里面",
        ));
    }
    copy_dir(source, target, progress)
}

fn copy_dir(source: &Path, target: &Path, progress: &CopyProgress) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        progress.check()?;
        let entry = entry?;
        if is_reparse_point(&entry) {
            continue;
        }
        let file_type = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &destination, progress)?;
        } else if file_type.is_file() {
            copy_file(&entry.path(), &destination, progress)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn copies_tree_and_counts_bytes() {
        let root = tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir_all(source.join("a/b")).unwrap();
        fs::write(source.join("one.txt"), vec![1u8; 10]).unwrap();
        fs::write(source.join("a/b/two.bin"), vec![2u8; 25]).unwrap();

        let progress = CopyProgress::new();
        measure_tree(&source, &progress).unwrap();
        assert_eq!(progress.snapshot(), (0, 35));
        copy_tree(&source, &root.path().join("dst"), &progress).unwrap();
        assert_eq!(progress.snapshot(), (35, 35));
        assert_eq!(
            fs::read(root.path().join("dst/a/b/two.bin")).unwrap(),
            vec![2u8; 25]
        );
    }

    #[test]
    fn cancel_stops_with_a_recognisable_error() {
        let root = tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("f"), b"x").unwrap();
        let progress = CopyProgress::new();
        progress.cancel();
        let error = copy_tree(&source, &root.path().join("out"), &progress).unwrap_err();
        assert!(is_cancelled(&error));
        assert!(!is_cancelled(&io::Error::other("other")));
    }

    #[test]
    fn refuses_a_target_inside_the_source() {
        let root = tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("f"), b"x").unwrap();
        let progress = CopyProgress::new();
        let error = copy_tree(&source, &source.join("inner"), &progress).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
