//! settings.rs
//! core::settings
//!
//! Server settings loaded once per process from `~/.config/rust-fs-mcp/config.toml`.
//! RUST_FS_MCP_CONFIG overrides the path; an empty value or `none` skips the file.
//! Parses the TOML subset the file needs: `[section]`, `key = "string" | integer`, `#` comments.
//! A missing file, unreadable file, or invalid key keeps the built-in defaults (warnings go to stderr).
//! An unset output.compress leaves the choice to the detected MCP host (core::client).
//!

use crate::core::config::{home_dir, resolve_path};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompressMode {
    // 기존 envelope (structuredContent JSON)
    #[default]
    Off,
    // 본문 평문 + 메타 한 줄
    Lossless,
}
impl CompressMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lossless => "lossless",
        }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Settings {
    // None = 호스트 기본값
    pub compress: Option<CompressMode>,
    pub max_output_bytes: Option<usize>,
}
static SETTINGS: OnceLock<Settings> = OnceLock::new();

// 1. Settings access ----------------------------------------------------------
pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(load_settings)
}
pub fn config_path() -> Option<PathBuf> {
    let Some(raw_path) = env::var_os("RUST_FS_MCP_CONFIG") else {
        return home_dir().map(|home| home.join(".config").join("rust-fs-mcp").join("config.toml"));
    };
    let raw_path = raw_path.to_string_lossy();
    let trimmed = raw_path.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return None;
    }
    resolve_path(trimmed).ok()
}
fn load_settings() -> Settings {
    let Some(path) = config_path() else {
        return Settings::default();
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Settings::default(),
        Err(error) => {
            eprintln!(
                "rust-fs-mcp: config read failed ({}): {error}",
                path.display()
            );
            return Settings::default();
        }
    };
    let (settings, warnings) = parse_settings(&text);
    for warning in warnings {
        eprintln!("rust-fs-mcp: {}: {warning}", path.display());
    }
    settings
}

// 2. TOML subset parser -------------------------------------------------------
pub fn parse_settings(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut warnings = Vec::new();
    let mut section = String::new();
    for (index, raw_line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line_no = index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            warnings.push(format!("line {line_no}: expected `key = value`"));
            continue;
        };
        let key = key.trim();
        let raw_value = raw_value.trim();
        match (section.as_str(), key) {
            ("output", "compress") => match parse_string(raw_value) {
                Some("off") => settings.compress = Some(CompressMode::Off),
                Some("lossless") => settings.compress = Some(CompressMode::Lossless),
                _ => warnings.push(format!(
                    "line {line_no}: output.compress must be \"off\" or \"lossless\""
                )),
            },
            ("output", "max_bytes") => match raw_value.replace('_', "").parse::<usize>() {
                Ok(bytes) => settings.max_output_bytes = Some(bytes),
                Err(_) => warnings.push(format!(
                    "line {line_no}: output.max_bytes must be a positive integer"
                )),
            },
            _ => warnings.push(format!(
                "line {line_no}: unknown key `{section}.{key}` ignored"
            )),
        }
    }
    (settings, warnings)
}
// 따옴표 밖 `#` 이후 제거
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (offset, ch) in line.char_indices() {
        match (quote, ch) {
            (None, '"' | '\'') => quote = Some(ch),
            (Some(open), _) if ch == open => quote = None,
            (None, '#') => return &line[..offset],
            _ => {}
        }
    }
    line
}
fn parse_string(raw_value: &str) -> Option<&str> {
    let inner = raw_value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'));
    inner.or_else(|| {
        raw_value
            .strip_prefix('\'')
            .and_then(|rest| rest.strip_suffix('\''))
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_output_section() {
        let text = "\u{feff}# rust-fs-mcp\n[output]\ncompress = \"lossless\" # 본문 평문\nmax_bytes = 40_000\n";
        let (settings, warnings) = parse_settings(text);
        assert_eq!(settings.compress, Some(CompressMode::Lossless));
        assert_eq!(settings.max_output_bytes, Some(40_000));
        assert!(warnings.is_empty(), "{warnings:?}");
    }
    #[test]
    fn keeps_defaults_and_warns_on_invalid_entries() {
        let text = "[output]\ncompress = \"max\"\nmax_bytes = -1\nbroken line\n[other]\nkey = 1\n";
        let (settings, warnings) = parse_settings(text);
        assert_eq!(settings, Settings::default());
        assert_eq!(warnings.len(), 4, "{warnings:?}");
    }
    #[test]
    fn empty_text_uses_defaults() {
        let (settings, warnings) = parse_settings("");
        assert!(settings.compress.is_none());
        assert!(settings.max_output_bytes.is_none());
        assert!(warnings.is_empty());
    }
    #[test]
    fn explicit_off_differs_from_unset() {
        let (settings, warnings) = parse_settings("[output]\ncompress = \"off\"\n");
        assert_eq!(settings.compress, Some(CompressMode::Off));
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
