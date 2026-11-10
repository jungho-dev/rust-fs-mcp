//! settings.rs
//! core::settings
//!
//! Server settings loaded once per process from `~/.config/rust-fs-mcp/config.toml`.
//! RUST_FS_MCP_CONFIG overrides the path; an empty value or `none` skips the file.
//! Parses the TOML subset the file needs: `[section]`, `key = "string" | integer | bool`, `#` comments.
//! A missing file, unreadable file, or invalid key keeps the built-in defaults (warnings go to stderr).
//! An unset output.compress leaves the choice to the detected MCP host (core::client).
//! `[stats] enabled = true` turns on the per-day tool call table (tools::stats) in stats.dir, `.docs` by default.
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
    // None = 도구 집계 끔
    pub stats_dir: Option<PathBuf>,
}
// 상대 경로는 서버 작업 폴더 기준
const DEFAULT_STATS_DIR: &str = ".docs";
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
    let mut stats_enabled = false;
    let mut stats_dir_text: Option<String> = None;
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
            ("stats", "enabled") => match raw_value {
                "true" => stats_enabled = true,
                "false" => stats_enabled = false,
                _ => warnings.push(format!(
                    "line {line_no}: stats.enabled must be true or false"
                )),
            },
            ("stats", "dir") => match parse_string(raw_value).map(str::trim) {
                Some(dir) if !dir.is_empty() => stats_dir_text = Some(dir.to_string()),
                _ => warnings.push(format!(
                    "line {line_no}: stats.dir must be a non-empty string"
                )),
            },
            _ => warnings.push(format!(
                "line {line_no}: unknown key `{section}.{key}` ignored"
            )),
        }
    }
    if stats_enabled {
        let dir_text = stats_dir_text.as_deref().unwrap_or(DEFAULT_STATS_DIR);
        match resolve_path(dir_text) {
            Ok(dir) => settings.stats_dir = Some(dir),
            Err(error) => warnings.push(format!("stats.dir `{dir_text}` unresolved: {error}")),
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
    #[test]
    fn parses_stats_section() {
        let (settings, warnings) = parse_settings("[stats]\nenabled = true\n");
        assert_eq!(settings.stats_dir, resolve_path(".docs").ok());
        assert!(warnings.is_empty(), "{warnings:?}");

        let custom = std::env::temp_dir().join("fs-mcp-stats");
        let text = format!("[stats]\nenabled = true\ndir = '{}'\n", custom.display());
        let (settings, warnings) = parse_settings(&text);
        assert_eq!(settings.stats_dir, resolve_path(&custom).ok());
        assert!(warnings.is_empty(), "{warnings:?}");
    }
    #[test]
    fn stats_stays_off_unless_enabled() {
        let text = "[stats]\nenabled = false\ndir = \"/tmp/x\"\n";
        let (settings, warnings) = parse_settings(text);
        assert!(settings.stats_dir.is_none());
        assert!(warnings.is_empty(), "{warnings:?}");

        let (settings, warnings) = parse_settings("[stats]\nenabled = yes\ndir = \"\"\n");
        assert!(settings.stats_dir.is_none());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
