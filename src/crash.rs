//! Crash attribution: compare plugin state against the last good snapshot and
//! look for package names in the DSH diagnostic file.

use crate::snapshots::{SnapshotInfo, SnapshotStore};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Return likely plugin suspects, most specific first.
///
/// The wording in the UI is deliberately "嫌疑" (suspect): Marina can only
/// compare configuration and scan text, not prove causality.
pub fn compute_suspects(
    store: &SnapshotStore,
    pre_launch_snapshot: Option<&str>,
    diagnostic_file: Option<&Path>,
) -> Vec<String> {
    let Some(pre_launch_snapshot) = pre_launch_snapshot else {
        return Vec::new();
    };
    let Ok(before) = store.get(pre_launch_snapshot) else {
        return Vec::new();
    };
    let Ok(Some(last_good)) = store.last_good() else {
        return Vec::new();
    };
    suspects_between(&before, &last_good, diagnostic_file)
}

pub fn suspects_between(
    before: &SnapshotInfo,
    last_good: &SnapshotInfo,
    diagnostic_file: Option<&Path>,
) -> Vec<String> {
    if suspects_from_texts(before, last_good, &[]).is_empty() {
        return Vec::new();
    }
    let diagnostic = diagnostic_file
        .and_then(read_diagnostic_tail)
        .unwrap_or_default();
    suspects_from_texts(before, last_good, &[diagnostic])
}

/// Largest diagnostic file read into memory.  DSH may write hundreds of MB
/// and only the end of the file matters for a crash report.
const MAX_DIAGNOSTIC_BYTES: u64 = 2 * 1024 * 1024;

/// Read at most [`MAX_DIAGNOSTIC_BYTES`] from the end of a diagnostic file.
fn read_diagnostic_tail(path: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length > MAX_DIAGNOSTIC_BYTES {
        file.seek(SeekFrom::Start(length - MAX_DIAGNOSTIC_BYTES))
            .ok()?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_DIAGNOSTIC_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Like [`suspects_between`], but ranks by package names found in any of
/// `texts` (diagnostic file contents, [`read_report_text`] of crash dumps…).
/// Suspects mentioned earlier in an earlier text come first.
pub fn suspects_from_texts(
    before: &SnapshotInfo,
    last_good: &SnapshotInfo,
    texts: &[String],
) -> Vec<String> {
    let previous = last_good
        .plugins
        .iter()
        .map(|plugin| (plugin.name.as_str(), plugin.version.as_str()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut changed = BTreeSet::new();
    for plugin in &before.plugins {
        match previous.get(plugin.name.as_str()) {
            None => {
                changed.insert(plugin.name.clone());
            }
            Some(version) if *version != plugin.version => {
                changed.insert(plugin.name.clone());
            }
            _ => {}
        }
    }
    if changed.is_empty() {
        return Vec::new();
    }

    let texts = texts
        .iter()
        .map(|text| text.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let mut suspects = changed.into_iter().collect::<Vec<_>>();
    suspects.sort_by_key(|name| {
        let lowered = name.to_ascii_lowercase();
        texts
            .iter()
            .enumerate()
            .find_map(|(index, text)| {
                text.find(&lowered)
                    .map(|position| (0usize, index, position))
            })
            .unwrap_or((1, usize::MAX, usize::MAX))
    });
    suspects
}

/// Directories below an instance's Electron `--user-data-dir` where Crashpad
/// and Electron leave crash dumps.
const CRASH_DIRS: &[&str] = &[
    "Crashpad/reports",
    "Crashpad/completed",
    "Crashpad/pending",
    "Crashpad/new",
    "Crashpad",
    "crashes",
    "crashes/reports",
    "crashes/completed",
    "crashes/pending",
    "logs",
];

/// Bytes of a dump examined by [`read_report_text`].
pub const REPORT_TEXT_BYTES: usize = 256 * 1024;
const MIN_STRING_CHARS: usize = 4;

/// Crash dumps (`*.dmp`) in `electron_dir` modified after `since`, newest
/// first (design §FR-5 crash signal 2).
pub fn crash_reports_since(electron_dir: &Path, since: SystemTime) -> Vec<PathBuf> {
    let mut reports = Vec::new();
    let mut seen = HashSet::new();
    for directory in CRASH_DIRS {
        let Ok(entries) = fs::read_dir(electron_dir.join(directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dump = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("dmp"));
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if is_dump && metadata.is_file() && modified > since && seen.insert(path.clone()) {
                reports.push((modified, path));
            }
        }
    }
    reports.sort_by_key(|report| std::cmp::Reverse(report.0));
    reports.into_iter().map(|(_, path)| path).collect()
}

/// Printable strings from the first [`REPORT_TEXT_BYTES`] of a binary dump,
/// one per line: ASCII runs and UTF-16LE runs of at least 4 characters, like
/// `strings`.  Empty when the file cannot be read.
pub fn read_report_text(path: &Path) -> String {
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::with_capacity(REPORT_TEXT_BYTES);
    if file
        .take(REPORT_TEXT_BYTES as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    let printable = |byte: u8| byte == b'\t' || (0x20..0x7f).contains(&byte);
    let mut output = String::new();
    let mut flush = |run: &mut String| {
        if run.len() >= MIN_STRING_CHARS {
            output.push_str(run);
            output.push('\n');
        }
        run.clear();
    };

    let mut run = String::new();
    for &byte in &bytes {
        if printable(byte) {
            run.push(byte as char);
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);

    for offset in 0..2 {
        for pair in bytes[offset..].as_chunks::<2>().0 {
            if pair[1] == 0 && printable(pair[0]) {
                run.push(pair[0] as char);
            } else {
                flush(&mut run);
            }
        }
        flush(&mut run);
    }
    output
}

/// Read the tail of a diagnostic file for the crash report dialog.
pub fn diagnostic_excerpt(path: &Path, max_chars: usize) -> Option<String> {
    let text = read_diagnostic_tail(path)?;
    if text.chars().count() <= max_chars {
        return Some(text);
    }
    let tail: String = text
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Some(format!("…（仅显示末尾 {max_chars} 个字符）\n{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshots::SnapshotPlugin;
    use tempfile::tempdir;

    fn snapshot(id: &str, plugins: &[(&str, &str)]) -> SnapshotInfo {
        SnapshotInfo {
            id: id.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            reason: "test".into(),
            files: Vec::new(),
            plugins: plugins
                .iter()
                .map(|(name, version)| SnapshotPlugin {
                    name: (*name).into(),
                    version: (*version).into(),
                    enabled: true,
                    entry_id: (*name).into(),
                })
                .collect(),
            last_good: false,
        }
    }

    #[test]
    fn reports_added_and_upgraded_plugins_and_prioritizes_diagnostics() {
        let root = tempdir().unwrap();
        let diagnostic = root.path().join("diagnostic.txt");
        fs::write(&diagnostic, "Error while loading b-plugin\nstack...").unwrap();
        let before = snapshot(
            "before",
            &[
                ("a-plugin", "1.0.0"),
                ("b-plugin", "2.0.0"),
                ("c-plugin", "1.0.0"),
            ],
        );
        let good = snapshot("good", &[("a-plugin", "1.0.0"), ("b-plugin", "1.0.0")]);

        let suspects = suspects_between(&before, &good, Some(&diagnostic));
        assert_eq!(suspects, vec!["b-plugin", "c-plugin"]);
    }

    #[test]
    fn no_changes_means_no_suspects() {
        let before = snapshot("before", &[("a", "1")]);
        let good = snapshot("good", &[("a", "1")]);
        assert!(suspects_between(&before, &good, None).is_empty());
    }

    #[test]
    fn finds_new_crash_dumps_and_extracts_their_strings() {
        let root = tempdir().unwrap();
        let reports = root.path().join("Crashpad/reports");
        let pending = root.path().join("Crashpad/pending");
        fs::create_dir_all(&reports).unwrap();
        fs::create_dir_all(&pending).unwrap();
        let old = reports.join("old.dmp");
        fs::write(&old, b"old").unwrap();
        let since = fs::metadata(&old).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));

        let mut dump = b"MDMP\x00\x01ab\x00node_modules\\dsh-plugin-a\\index.js\x00\xff".to_vec();
        for unit in "C:\\x\\dsh-plugin-b\\lib.js".encode_utf16() {
            dump.extend_from_slice(&unit.to_le_bytes());
        }
        dump.extend_from_slice(&[0, 0, 0xff]);
        let new = pending.join("new.dmp");
        fs::write(&new, &dump).unwrap();
        fs::write(pending.join("notes.txt"), b"not a dump").unwrap();

        assert_eq!(crash_reports_since(root.path(), since), vec![new.clone()]);
        let text = read_report_text(&new);
        assert!(text.contains("node_modules\\dsh-plugin-a\\index.js"));
        assert!(text.contains("C:\\x\\dsh-plugin-b\\lib.js"));
        assert!(!text.contains("ab\n"));

        let before = snapshot(
            "before",
            &[("dsh-plugin-a", "1"), ("dsh-plugin-b", "1"), ("c", "1")],
        );
        let good = snapshot("good", &[]);
        let texts = vec!["loading dsh-plugin-b".to_owned(), text];
        assert_eq!(
            suspects_from_texts(&before, &good, &texts),
            vec!["dsh-plugin-b", "dsh-plugin-a", "c"]
        );
    }

    #[test]
    fn missing_electron_dir_has_no_reports() {
        let root = tempdir().unwrap();
        assert!(crash_reports_since(&root.path().join("none"), SystemTime::UNIX_EPOCH).is_empty());
        assert!(read_report_text(&root.path().join("none.dmp")).is_empty());
    }
}
