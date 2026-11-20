//! Detection of the legacy hand-written NSIS install (v0.1.x).
//!
//! The new cargo-packager installer uses its own registry layout and a fixed
//! per-user install directory. Users who installed an earlier release through
//! `installer/dsh-marina.nsi` still have a separate copy registered under
//! `HKCU\Software\DSH Marina`. We detect it at startup so Marina can surface a
//! notice rather than silently orphaning the old copy. Removal stays
//! user-driven (the old uninstaller is never run automatically).

use std::path::{Path, PathBuf};

use crate::links::registry;

const LEGACY_KEY: &str = r"Software\DSH Marina";
const LEGACY_UNINST_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\DSHMarina";

/// A previously installed legacy copy found in the registry.
pub struct LegacyInstall {
    pub install_dir: PathBuf,
    pub version: Option<String>,
    pub uninstaller: Option<PathBuf>,
}

/// Read the legacy install location, if it still exists on disk.
pub fn detect() -> Option<LegacyInstall> {
    let dir = registry::read(LEGACY_KEY, Some("InstallDir"))?;
    let install_dir = PathBuf::from(dir);
    if !install_dir.is_dir() {
        return None;
    }
    let version = registry::read(LEGACY_KEY, Some("Version"));
    let uninstaller = registry::read(LEGACY_UNINST_KEY, Some("UninstallString"))
        .map(|raw| PathBuf::from(raw.trim().trim_matches('"')));
    Some(LegacyInstall {
        install_dir,
        version,
        uninstaller,
    })
}

/// Whether the legacy copy lives in a different directory than the running
/// executable — i.e. an old copy the new installer did not overwrite.
pub fn is_separate_copy(legacy: &LegacyInstall, current_exe_dir: &Path) -> bool {
    !same_dir(&legacy.install_dir, current_exe_dir)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // Canonicalisation can fail for a removed/locked path; fall back to a
        // case-insensitive comparison of the literal paths.
        _ => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_copy_compares_directories() {
        let legacy = LegacyInstall {
            install_dir: PathBuf::from(r"C:\Users\X\AppData\Local\Programs\DSH Marina"),
            version: Some("0.1.2".into()),
            uninstaller: None,
        };
        assert!(is_separate_copy(
            &legacy,
            Path::new(r"C:\Users\X\AppData\Local\DSH Marina")
        ));
        assert!(!is_separate_copy(
            &legacy,
            Path::new(r"C:\Users\X\AppData\Local\Programs\DSH Marina")
        ));
    }
}
