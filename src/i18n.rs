//! UI text lookup (design NFR-8 "文字全部用 tr() 包起来便于以后翻译").
//!
//! Chinese is the source language: `tr("设置")` returns the text itself unless
//! a translation for the active language is registered.  The language comes
//! from `DSH_MARINA_LANG` (for example `en`); translations live in
//! `<data root>\lang\<code>.toml` as `"中文原文" = "translation"` pairs, so a
//! translation can be added without rebuilding Marina.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

static TABLE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Load the table for the active language once at startup.
pub fn init(data_root: &Path) {
    let table = std::env::var("DSH_MARINA_LANG")
        .ok()
        .filter(|code| {
            !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .and_then(|code| {
            std::fs::read_to_string(data_root.join("lang").join(format!("{code}.toml"))).ok()
        })
        .map(|text| parse(&text))
        .unwrap_or_default();
    let _ = TABLE.set(table);
}

fn parse(text: &str) -> HashMap<String, String> {
    toml::from_str::<HashMap<String, String>>(text).unwrap_or_default()
}

/// The translation of `source`, or `source` itself.
pub fn tr(source: &str) -> String {
    TABLE
        .get()
        .and_then(|table| table.get(source))
        .cloned()
        .unwrap_or_else(|| source.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_the_source_text() {
        assert_eq!(tr("设置"), "设置");
        let table = parse("\"设置\" = \"Settings\"\n\"退出\" = \"Quit\"\n");
        assert_eq!(table.get("设置").map(String::as_str), Some("Settings"));
        assert!(parse("not toml = = ").is_empty());
    }
}
