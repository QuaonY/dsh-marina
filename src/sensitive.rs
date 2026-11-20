//! Sensitive data scanning and redaction for pack export.
//!
//! Detects tokens, passwords, and local absolute paths that should not be shared,
//! and redacts them when exporting instances.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// `key = value` style secrets: a key name followed by a long value.
fn token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(api[_-]?key|apikey|access[_-]?token|auth[_-]?token|secret[_-]?key|client[_-]?secret|password|passwd|pwd|token)["\s:=]+["']?([A-Za-z0-9_\-\.\+/=]{10,})"#,
        )
        .expect("valid token pattern")
    })
}

/// Well-known secret shapes that carry no key name (`sk-…`, `ghp_…`, JWTs).
fn bare_secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            \b(?:
                sk-[A-Za-z0-9_\-]{16,}
              | gh[pousr]_[A-Za-z0-9]{20,}
              | github_pat_[A-Za-z0-9_]{20,}
              | AKIA[0-9A-Z]{16}
              | xox[baprs]-[A-Za-z0-9\-]{10,}
              | AIza[0-9A-Za-z_\-]{30,}
              | glpat-[A-Za-z0-9_\-]{20,}
              | npm_[A-Za-z0-9]{30,}
              | eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}
            )\b",
        )
        .expect("valid bare secret pattern")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveKind {
    Token,
    AbsolutePath,
}

impl SensitiveKind {
    /// Human-readable label for the finding kind.
    pub fn label(&self) -> &'static str {
        match self {
            SensitiveKind::Token => "Token/密钥",
            SensitiveKind::AbsolutePath => "绝对路径",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SensitiveFinding {
    pub file: String,
    pub kind: SensitiveKind,
    pub line: usize,
    pub excerpt: String,
}

/// Windows absolute paths that should be redacted before sharing: drive paths
/// with either separator and any case (`C:\…`, `c:/…` — JSON configs often use
/// `/`), plus UNC shares (`\\server\share\…`). The `\b` keeps URLs such as
/// `https://…` from matching the drive-letter branch.
fn abs_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:\b[a-z]:[\\/][^\s"'<>|]{2,}|\\\\[^\s"'<>|\\/]+[\\/][^\s"'<>|]{2,})"#)
            .expect("valid path pattern")
    })
}

/// Scan text for sensitive content.
///
/// - `path`: Relative path within the pack for error messages
/// - `text`: File content to scan
/// - `home`: User's home directory, if known, for path detection
pub fn scan_text(path: &str, text: &str, home: Option<&Path>) -> Vec<SensitiveFinding> {
    let mut findings = Vec::new();
    let token_pattern = token_regex();
    let bare_pattern = bare_secret_regex();
    let abs_path_pattern = abs_path_regex();
    // Windows paths are case-insensitive: match the home directory in any case.
    let home_re =
        home.and_then(|h| Regex::new(&format!("(?i){}", regex::escape(&h.to_string_lossy()))).ok());
    let home_lower = home.map(|h| h.to_string_lossy().to_ascii_lowercase());

    for (line_num, line) in text.lines().enumerate() {
        // Check for tokens: either a `key = value` pair or a bare known secret.
        if token_pattern.is_match(line) || bare_pattern.is_match(line) {
            // Create a redacted excerpt
            let mut redacted = redact_token_values(line);
            if bare_pattern.is_match(&redacted) {
                redacted = bare_pattern.replace_all(&redacted, "***").to_string();
            }
            findings.push(SensitiveFinding {
                file: path.to_string(),
                kind: SensitiveKind::Token,
                line: line_num + 1,
                excerpt: redacted,
            });
        }

        // Check for absolute paths
        if let Some(home_re) = &home_re
            && home_re.is_match(line)
        {
            let redacted = home_re.replace_all(line, home_placeholder()).to_string();
            findings.push(SensitiveFinding {
                file: path.to_string(),
                kind: SensitiveKind::AbsolutePath,
                line: line_num + 1,
                excerpt: redacted,
            });
        }

        // Check for other absolute paths (like D:\\ or E:\\)
        if abs_path_pattern.is_match(line) {
            // Check if it's not the home path (already handled above)
            let has_non_home_path = if let Some(home_lower) = &home_lower {
                abs_path_pattern
                    .find_iter(line)
                    .any(|m| !m.as_str().to_ascii_lowercase().starts_with(home_lower))
            } else {
                true
            };

            if has_non_home_path {
                findings.push(SensitiveFinding {
                    file: path.to_string(),
                    kind: SensitiveKind::AbsolutePath,
                    line: line_num + 1,
                    excerpt: line.to_string(),
                });
            }
        }
    }

    findings
}

pub struct Redaction<'a> {
    pub home: Option<&'a str>,
}

/// Replace only the secret value of a `key = value` pair.
///
/// The key, the separator (`=`, `:`, quotes, spaces) and everything after the
/// value are kept verbatim, so a redacted YAML/TOML/.env file still parses.
fn redact_token_values(text: &str) -> String {
    token_regex()
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let whole = captures.get(0).expect("pattern matched");
            let value = captures.get(2).expect("second group is required");
            let prefix = &whole.as_str()[..value.start() - whole.start()];
            // Keep the quote that was already there; add one when the value was
            // bare, because `***` alone is an alias in YAML, not a string.
            if prefix.ends_with('"') || prefix.ends_with('\'') {
                format!("{prefix}***")
            } else {
                format!("{prefix}\"***\"")
            }
        })
        .into_owned()
}

/// Redact sensitive content from text.
///
/// The placeholder substituted for the user's home directory in redacted
/// output: `%USERPROFILE%` on Windows, `~` elsewhere.
fn home_placeholder() -> &'static str {
    if cfg!(windows) { "%USERPROFILE%" } else { "~" }
}

/// - Replaces token values with `***`
/// - Replaces home directory paths with the platform placeholder
pub fn redact_text(text: &str, home: Option<&str>) -> String {
    let mut result = redact_token_values(text);
    result = bare_secret_regex().replace_all(&result, "***").to_string();

    // Redact home directory paths (case-insensitive, like Windows itself)
    if let Some(home_str) = home {
        let placeholder = home_placeholder();
        match Regex::new(&format!("(?i){}", regex::escape(home_str))) {
            Ok(re) => result = re.replace_all(&result, placeholder).to_string(),
            Err(_) => result = result.replace(home_str, placeholder),
        }
    }

    result
}

/// Get the user's home directory.
pub fn user_home() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scans_tokens() {
        let text = r#"apiKey: "sk-exampleKey0000000000000""#;
        let findings = scan_text("config.yml", text, None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, SensitiveKind::Token);
        assert!(!findings[0].excerpt.contains("sk-exampleKey0000000000000"));
    }

    #[test]
    fn scans_paths() {
        let home = PathBuf::from("C:\\Users\\example");
        let text = "dir: C:\\Users\\example\\Documents\\data";
        let findings = scan_text("config.yml", text, Some(&home));
        assert!(!findings.is_empty());
        assert!(
            findings
                .iter()
                .any(|f| f.kind == SensitiveKind::AbsolutePath)
        );
    }

    #[test]
    fn redacts_tokens() {
        let text = r#"{"apiKey": "sk-exampleKey0000000000000"}"#;
        let redacted = redact_text(text, None);
        assert!(!redacted.contains("sk-exampleKey0000000000000"));
        assert!(redacted.contains("***"));
    }

    #[test]
    fn redaction_keeps_the_key_separator() {
        let toml = "api_key = \"sk-exampleKey0000000000000\"\nname = \"demo\"\n";
        let redacted = redact_text(toml, None);
        assert!(redacted.contains("api_key = \"***\""), "{redacted}");
        assert!(redacted.contains("name = \"demo\""), "{redacted}");

        let json = r#"{"apiKey": "sk-exampleKey0000000000000"}"#;
        assert_eq!(redact_text(json, None), r#"{"apiKey": "***"}"#);

        let yaml = "token: ghp_exampleToken00000000000000000000\n";
        assert_eq!(redact_text(yaml, None), "token: \"***\"\n");
    }

    #[test]
    fn scans_bare_secrets() {
        for text in [
            r#"key = "sk-exampleKey0000000000000""#,
            "token: ghp_exampleToken00000000000000000000",
            "aws AKIAEXAMPLE000000000 here",
        ] {
            let findings = scan_text("config.yml", text, None);
            assert!(
                findings.iter().any(|f| f.kind == SensitiveKind::Token),
                "{text}"
            );
            let redacted = redact_text(text, None);
            assert!(redacted.contains("***"), "{text} -> {redacted}");
        }
    }

    #[test]
    fn redacts_paths() {
        let home = "C:\\Users\\example";
        let text = "dir: C:\\Users\\example\\Documents";
        let redacted = redact_text(text, Some(home));
        assert!(!redacted.contains("C:\\Users\\example"));
        assert!(redacted.contains(home_placeholder()));
    }

    #[test]
    fn scans_lowercase_forward_slash_and_json_paths() {
        for text in [
            "dir: c:\\users\\example\\documents",
            "dir: C:/Users/example/Documents",
            r#"{"dir": "C:\\Users\\example\\Documents"}"#,
        ] {
            let findings = scan_text("config.json", text, None);
            assert!(
                findings
                    .iter()
                    .any(|f| f.kind == SensitiveKind::AbsolutePath),
                "{text}"
            );
        }
    }

    #[test]
    fn scans_unc_paths() {
        let text = r"share: \\fileserver\team\docs";
        let findings = scan_text("config.yml", text, None);
        assert!(
            findings
                .iter()
                .any(|f| f.kind == SensitiveKind::AbsolutePath),
            "{text}"
        );
    }

    #[test]
    fn urls_are_not_absolute_paths() {
        let text = "homepage: https://github.com/QuaonY/dsh-marina";
        let findings = scan_text("config.yml", text, None);
        assert!(
            !findings
                .iter()
                .any(|f| f.kind == SensitiveKind::AbsolutePath),
            "{findings:?}"
        );
    }

    #[test]
    fn redacts_lowercase_home() {
        let home = "C:\\Users\\Example";
        let text = "dir: c:\\users\\example\\Documents";
        let redacted = redact_text(text, Some(home));
        assert!(redacted.contains(home_placeholder()), "{redacted}");
        assert!(
            !redacted.to_ascii_lowercase().contains("c:\\users\\example"),
            "{redacted}"
        );
        let findings = scan_text("config.yml", text, Some(&PathBuf::from(home)));
        assert!(
            findings
                .iter()
                .any(|f| f.kind == SensitiveKind::AbsolutePath),
            "{findings:?}"
        );
    }
}
