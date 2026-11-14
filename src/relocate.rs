//! Moving Marina's data root to another folder (design §8.1 "可在设置里改到其他盘").
//!
//! The chosen location is kept in a small pointer file next to the default
//! data root, so it survives the old root being deleted.  The move copies
//! everything with progress, verifies the byte count, then writes the pointer
//! with `remove_previous` set; the restarted Marina deletes the old root.

use crate::copy::{self, CopyProgress};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Name of the folder created inside the folder the user picks.
pub const DATA_FOLDER: &str = "dsh-marina";
/// Free space kept in reserve on the target drive.
const SPACE_MARGIN: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pointer {
    pub data_root: Option<PathBuf>,
    /// Old root left behind by a move; removed on the next start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove_previous: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum RelocateError {
    #[error("目标文件夹不能是当前数据目录或它的子目录")]
    InsideCurrent,
    #[error("目标位置已有内容：{0}")]
    NotEmpty(PathBuf),
    #[error("目标磁盘空间不足：需要 {needed} 字节，可用 {available} 字节")]
    NoSpace { needed: u64, available: u64 },
    #[error("校验失败：复制了 {copied} 字节，目标只有 {found} 字节")]
    Verify { copied: u64, found: u64 },
    #[error("已取消")]
    Cancelled,
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("无法写入位置文件：{0}")]
    Pointer(String),
}

/// `%USERPROFILE%\.dsh-marina-location.toml`, beside the default root.
pub fn default_pointer_file(default_root: &Path) -> PathBuf {
    let name = default_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| DATA_FOLDER.to_owned());
    default_root.with_file_name(format!("{name}-location.toml"))
}

pub fn read_pointer(file: &Path) -> Pointer {
    fs::read_to_string(file)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn write_pointer(file: &Path, pointer: &Pointer) -> Result<(), RelocateError> {
    let text =
        toml::to_string_pretty(pointer).map_err(|e| RelocateError::Pointer(e.to_string()))?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| RelocateError::Pointer(e.to_string()))?;
    }
    let temporary = file.with_extension("toml.tmp");
    fs::write(&temporary, text)
        .and_then(|()| fs::rename(&temporary, file))
        .map_err(|e| RelocateError::Pointer(e.to_string()))
}

/// The data root to use: the pointer's target when it still exists, else the default.
pub fn resolve(default_root: &Path, pointer: &Pointer) -> PathBuf {
    match &pointer.data_root {
        Some(root) if root.is_dir() => root.clone(),
        _ => default_root.to_path_buf(),
    }
}

fn normalize(path: &Path) -> PathBuf {
    let absolute = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(
        absolute
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase(),
    )
}

/// The new root for a folder the user picked: `<picked>\dsh-marina`, or the
/// picked folder itself when it is already an empty `dsh-marina` folder.
pub fn plan_target(current: &Path, picked: &Path) -> Result<PathBuf, RelocateError> {
    let is_data_folder = picked
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(DATA_FOLDER));
    let target = if is_data_folder && is_empty_or_missing(picked) {
        picked.to_path_buf()
    } else {
        picked.join(DATA_FOLDER)
    };
    let (current_n, target_n) = (normalize(current), normalize(&target));
    if target_n == current_n || target_n.starts_with(&current_n) {
        return Err(RelocateError::InsideCurrent);
    }
    if !is_empty_or_missing(&target) {
        return Err(RelocateError::NotEmpty(target));
    }
    Ok(target)
}

fn is_empty_or_missing(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => !path.exists(),
    }
}

/// Copy `current` into `target` and verify it.  `free_space` reports the
/// bytes available at a path (None when unknown).  On any failure or cancel
/// the partial copy is removed.
pub fn copy_data(
    current: &Path,
    target: &Path,
    progress: &CopyProgress,
    free_space: impl Fn(&Path) -> Option<u64>,
) -> Result<u64, RelocateError> {
    let result = copy_inner(current, target, progress, free_space);
    if result.is_err() {
        let _ = fs::remove_dir_all(target);
    }
    result
}

fn copy_inner(
    current: &Path,
    target: &Path,
    progress: &CopyProgress,
    free_space: impl Fn(&Path) -> Option<u64>,
) -> Result<u64, RelocateError> {
    let map = |e: io::Error| {
        if copy::is_cancelled(&e) {
            RelocateError::Cancelled
        } else {
            RelocateError::Io(e)
        }
    };
    copy::measure_tree(current, progress).map_err(map)?;
    progress.finish_measuring();
    let (_, total) = progress.snapshot();
    let probe = target.ancestors().find(|p| p.exists()).unwrap_or(target);
    if let Some(available) = free_space(probe)
        && available < total + SPACE_MARGIN
    {
        return Err(RelocateError::NoSpace {
            needed: total + SPACE_MARGIN,
            available,
        });
    }
    copy::copy_tree(current, target, progress).map_err(map)?;
    let check = CopyProgress::default();
    copy::measure_tree(target, &check)?;
    let (_, found) = check.snapshot();
    if found != total {
        return Err(RelocateError::Verify {
            copied: total,
            found,
        });
    }
    Ok(total)
}

/// Remove the old root recorded by a previous move and clear the field.
/// Returns an error message when removal failed (the field is then kept
/// cleared anyway so a locked folder does not block every start).
pub fn finish_pending_removal(
    pointer_file: &Path,
    current: &Path,
) -> Option<Result<PathBuf, String>> {
    let mut pointer = read_pointer(pointer_file);
    let previous = pointer.remove_previous.take()?;
    let _ = write_pointer(pointer_file, &pointer);
    if normalize(&previous) == normalize(current) || !previous.join("instances").is_dir() {
        return None;
    }
    Some(
        fs::remove_dir_all(&previous)
            .map(|()| previous.clone())
            .map_err(|e| format!("{}：{e}", previous.display())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn seed(root: &Path) {
        fs::create_dir_all(root.join("instances/a/home")).unwrap();
        fs::write(root.join("instances/a/home/x.txt"), vec![7u8; 40]).unwrap();
        fs::write(root.join("marina.toml"), "schema_version = 1\n").unwrap();
    }

    #[test]
    fn rejects_targets_inside_the_current_root() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("dsh-marina");
        seed(&current);
        assert!(matches!(
            plan_target(&current, &current),
            Err(RelocateError::InsideCurrent)
        ));
        assert!(matches!(
            plan_target(&current, &current.join("instances")),
            Err(RelocateError::InsideCurrent)
        ));
        assert!(matches!(
            plan_target(&current, dir.path()),
            Err(RelocateError::InsideCurrent)
        ));
    }

    #[test]
    fn plans_a_subfolder_and_refuses_non_empty_targets() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("old");
        seed(&current);
        let picked = dir.path().join("D");
        assert_eq!(
            plan_target(&current, &picked).unwrap(),
            picked.join(DATA_FOLDER)
        );
        fs::create_dir_all(picked.join(DATA_FOLDER)).unwrap();
        fs::write(picked.join(DATA_FOLDER).join("f"), b"x").unwrap();
        assert!(matches!(
            plan_target(&current, &picked),
            Err(RelocateError::NotEmpty(_))
        ));
    }

    #[test]
    fn copies_verifies_and_resolves_through_the_pointer() {
        let dir = tempdir().unwrap();
        let default_root = dir.path().join("dsh-marina");
        seed(&default_root);
        let target = dir.path().join("E").join(DATA_FOLDER);
        let progress = CopyProgress::default();
        let bytes = copy_data(&default_root, &target, &progress, |_| None).unwrap();
        assert_eq!(bytes, 40 + 19);
        assert_eq!(
            fs::read(target.join("instances/a/home/x.txt")).unwrap(),
            vec![7u8; 40]
        );

        let pointer_file = default_pointer_file(&default_root);
        assert_eq!(pointer_file, dir.path().join("dsh-marina-location.toml"));
        let pointer = Pointer {
            data_root: Some(target.clone()),
            remove_previous: Some(default_root.clone()),
        };
        write_pointer(&pointer_file, &pointer).unwrap();
        assert_eq!(read_pointer(&pointer_file), pointer);
        assert_eq!(resolve(&default_root, &read_pointer(&pointer_file)), target);

        let removed = finish_pending_removal(&pointer_file, &target)
            .unwrap()
            .unwrap();
        assert_eq!(removed, default_root);
        assert!(!default_root.exists());
        assert!(read_pointer(&pointer_file).remove_previous.is_none());
        assert!(finish_pending_removal(&pointer_file, &target).is_none());
    }

    #[test]
    fn missing_pointer_target_falls_back_to_the_default() {
        let dir = tempdir().unwrap();
        let pointer = Pointer {
            data_root: Some(dir.path().join("gone")),
            remove_previous: None,
        };
        assert_eq!(resolve(dir.path(), &pointer), dir.path());
    }

    #[test]
    fn not_enough_space_and_cancel_leave_no_partial_copy() {
        let dir = tempdir().unwrap();
        let current = dir.path().join("old");
        seed(&current);
        let target = dir.path().join("new");
        let error =
            copy_data(&current, &target, &CopyProgress::default(), |_| Some(10)).unwrap_err();
        assert!(matches!(error, RelocateError::NoSpace { .. }));
        assert!(!target.exists());

        let progress = CopyProgress::default();
        progress.cancel();
        assert!(matches!(
            copy_data(&current, &target, &progress, |_| None),
            Err(RelocateError::Cancelled)
        ));
        assert!(!target.exists());
        assert!(current.join("marina.toml").is_file());
    }
}
