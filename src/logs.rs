//! Read-only view of the redacted DSH stdout logs that Marina captures under
//! `<instance>/marina/stdout-*.log`.  Lines are already token-redacted when
//! written; this module only reads the newest file's tail and classifies lines.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Chunk size when reading the newest log backwards.
const TAIL_CHUNK_BYTES: u64 = 64 * 1024;
/// Upper bound on bytes read from the end of the newest log.
const MAX_TAIL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// `HH:MM:SS` when the line carries a recognisable timestamp, else empty.
    pub time: String,
    /// `INFO` | `WARN` | `ERROR` | `DEBUG`.
    pub level: &'static str,
    pub text: String,
}

/// Newest `stdout-*.log` in `marina_dir` (names sort chronologically).
pub fn latest_log(marina_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(marina_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("stdout-") && name.ends_with(".log"))
        })
        .max()
}

/// Last `max_lines` non-empty lines of `path`, classified.  Reads backwards in
/// chunks, so the cost depends on `max_lines`, not on the file size.
pub fn tail(path: &Path, max_lines: usize) -> std::io::Result<Vec<LogEntry>> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut start = length;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut non_empty = 0usize;
    // Whether the leading, possibly partial line read so far has text.  It is
    // counted once a newline before it is seen.
    let mut leading_has_text = false;
    let has_text = |bytes: &[u8]| bytes.iter().any(|byte| !byte.is_ascii_whitespace());
    while start > 0 && non_empty <= max_lines && length - start < MAX_TAIL_BYTES {
        let chunk = TAIL_CHUNK_BYTES.min(start);
        start -= chunk;
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0u8; chunk as usize];
        file.read_exact(&mut buffer)?;
        match buffer.iter().position(|&byte| byte == b'\n') {
            Some(first_newline) => {
                let mut segments = buffer[first_newline + 1..].split(|&byte| byte == b'\n');
                // The last segment continues the previous leading line.
                let last = segments.next_back().unwrap_or_default();
                non_empty += segments.filter(|segment| has_text(segment)).count();
                non_empty += usize::from(has_text(last) || leading_has_text);
                leading_has_text = has_text(&buffer[..first_newline]);
            }
            None => leading_has_text |= has_text(&buffer),
        }
        chunks.push(buffer);
    }
    let mut bytes = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for chunk in chunks.iter().rev() {
        bytes.extend_from_slice(chunk);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    // A partial first line is expected when reading from the middle.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let lines: Vec<&str> = lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let skip = lines.len().saturating_sub(max_lines);
    Ok(lines.into_iter().skip(skip).map(classify).collect())
}

/// Split a raw line into time / level / message.  Understands the Koishi/Cordis
/// style `2025-01-01 12:00:00 [I] scope message` and plain `INFO`/`WARN`/`ERROR`
/// words; anything else is shown as INFO with no time.
pub fn classify(line: &str) -> LogEntry {
    let line = strip_ansi(line);
    let mut rest = line.trim_end();
    let mut time = String::new();
    if let Some((found, remaining)) = take_time(rest) {
        time = found;
        rest = remaining.trim_start();
    }
    let mut level = detect_level(rest);
    for (tag, tagged) in [
        ("[I]", "INFO"),
        ("[W]", "WARN"),
        ("[E]", "ERROR"),
        ("[D]", "DEBUG"),
    ] {
        if let Some(stripped) = rest.strip_prefix(tag) {
            level = tagged;
            rest = stripped.trim_start();
            break;
        }
    }
    LogEntry {
        time,
        level,
        text: rest.to_owned(),
    }
}

fn detect_level(text: &str) -> &'static str {
    let upper = text.to_ascii_uppercase();
    if upper.contains("ERROR") || upper.contains("FATAL") || upper.contains("PANIC") {
        "ERROR"
    } else if upper.contains("WARN") {
        "WARN"
    } else if upper.starts_with("DEBUG") {
        "DEBUG"
    } else {
        "INFO"
    }
}

/// Accepts an optional leading `YYYY-MM-DD[ T]` date followed by `HH:MM:SS`.
fn take_time(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    if bytes.len() >= 11
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b' ' || bytes[10] == b'T')
    {
        index = 11;
    }
    let clock = bytes.get(index..index + 8)?;
    let digits = [0, 1, 3, 4, 6, 7];
    if digits.iter().all(|&i| clock[i].is_ascii_digit()) && clock[2] == b':' && clock[5] == b':' {
        let mut end = index + 8;
        // Drop fractional seconds / timezone suffixes such as `.123Z`.
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        Some((text[index..index + 8].to_owned(), &text[end..]))
    } else {
        None
    }
}

fn strip_ansi(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_cordis_lines() {
        let entry = classify("2025-11-01 20:01:02 [W] loader plugin slow");
        assert_eq!(entry.time, "20:01:02");
        assert_eq!(entry.level, "WARN");
        assert_eq!(entry.text, "loader plugin slow");
    }

    #[test]
    fn classifies_plain_and_ansi_lines() {
        let entry = classify("\u{1b}[31mError: boom\u{1b}[0m");
        assert_eq!(entry.level, "ERROR");
        assert_eq!(entry.text, "Error: boom");
        assert_eq!(entry.time, "");
        let entry = classify("12:00:00.123 dsh web: http://127.0.0.1:5140");
        assert_eq!(entry.time, "12:00:00");
        assert_eq!(entry.level, "INFO");
    }

    #[test]
    fn tails_the_newest_log() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("stdout-20250101T000000.log"), "old\n").unwrap();
        let newest = directory.path().join("stdout-20250102T000000.log");
        let content: String = (0..50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&newest, content).unwrap();
        assert_eq!(
            latest_log(directory.path()).as_deref(),
            Some(newest.as_path())
        );
        let entries = tail(&newest, 10).unwrap();
        assert_eq!(entries.len(), 10);
        assert_eq!(entries.last().unwrap().text, "line 49");
    }

    #[test]
    fn tails_large_files_across_chunk_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stdout-big.log");
        let mut content = String::new();
        for index in 0..60_000 {
            content.push_str(&format!("2025-11-01 20:01:02 [I] scope line {index:05}\n"));
            if index % 7 == 0 {
                content.push_str("\n   \n");
            }
        }
        std::fs::write(&path, &content).unwrap();
        let entries = tail(&path, 2000).unwrap();
        assert_eq!(entries.len(), 2000);
        assert_eq!(entries[0].text, "scope line 58000");
        assert_eq!(entries.last().unwrap().text, "scope line 59999");

        let all = tail(&path, 100_000).unwrap();
        assert_eq!(all.len(), 60_000);
        assert_eq!(all[0].text, "scope line 00000");

        let no_newline = directory.path().join("stdout-one.log");
        std::fs::write(&no_newline, "x".repeat(200_000)).unwrap();
        assert_eq!(tail(&no_newline, 10).unwrap().len(), 1);
    }
}
