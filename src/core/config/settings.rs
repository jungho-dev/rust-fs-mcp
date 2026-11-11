//! settings.rs
//! core::config::settings
//!
//! Server settings loaded once per process from `~/.config/rust-fs-mcp/config.toml`.
//! RUST_FS_MCP_CONFIG overrides the path; an empty value or `none` skips the file.
//! Parses the TOML subset the file needs: `[section]`, `key = "string" | integer | bool`, `#` comments.
//! A missing file, unreadable file, or invalid key keeps the built-in defaults (warnings go to stderr).
//! An unset output.compress leaves the choice to the detected MCP host (core::client).
//! `[stats] enabled = true` turns on the per-day tool call table (tools::stats) in stats.dir, `.docs` by default.
//! `[mysql]` with host and user turns on the mysql-query tool; the password comes from password_env, password_dpapi, or password.
//!

use crate::core::config::{home_dir, resolve_path};
use std::collections::BTreeMap;
use std::env;
use std::fmt;
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
// 비밀번호 출처 (우선순위 Env > Dpapi > Plain)
#[derive(Clone, Default, Eq, PartialEq)]
pub enum PasswordSource {
    #[default]
    Unset,
    Env(String),
    Dpapi(PathBuf),
    Plain(String),
}
// 평문 비노출 Debug
impl fmt::Debug for PasswordSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unset => formatter.write_str("Unset"),
            Self::Env(name) => write!(formatter, "Env({name:?})"),
            Self::Dpapi(path) => write!(formatter, "Dpapi({path:?})"),
            Self::Plain(_) => formatter.write_str("Plain(<redacted>)"),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MysqlSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: Option<String>,
    pub password: PasswordSource,
    pub connect_timeout_ms: u64,
    // 호출 인자 상한 겸용
    pub query_timeout_ms: u64,
    pub max_rows: usize,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Settings {
    // None = 호스트 기본값
    pub compress: Option<CompressMode>,
    pub max_output_bytes: Option<usize>,
    // None = 도구 집계 끔
    pub stats_dir: Option<PathBuf>,
    // None = mysql-query 끔
    pub mysql: Option<MysqlSettings>,
}
// 상대 경로는 서버 작업 폴더 기준
const DEFAULT_STATS_DIR: &str = ".docs";
const DEFAULT_MYSQL_PORT: u64 = 3306;
const DEFAULT_MYSQL_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MYSQL_MAX_ROWS: u64 = 1_000;
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
    let mut mysql_listed = false;
    let mut mysql_texts: BTreeMap<&str, String> = BTreeMap::new();
    let mut mysql_numbers: BTreeMap<&str, u64> = BTreeMap::new();
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
            (
                "mysql",
                "host" | "user" | "database" | "password_env" | "password_dpapi" | "password",
            ) => {
                mysql_listed = true;
                // 비밀번호 공백 보존
                match parse_string(raw_value) {
                    Some(text) if key == "password" => {
                        mysql_texts.insert(key, text.to_string());
                    }
                    Some(text) if !text.trim().is_empty() => {
                        mysql_texts.insert(key, text.trim().to_string());
                    }
                    _ => warnings.push(format!(
                        "line {line_no}: mysql.{key} must be a non-empty string"
                    )),
                }
            }
            ("mysql", "port" | "connect_timeout_ms" | "query_timeout_ms" | "max_rows") => {
                mysql_listed = true;
                match raw_value.replace('_', "").parse::<u64>() {
                    Ok(number) if number > 0 => {
                        mysql_numbers.insert(key, number);
                    }
                    _ => warnings.push(format!(
                        "line {line_no}: mysql.{key} must be a positive integer"
                    )),
                }
            }
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
    if mysql_listed {
        // 1. 비밀번호 출처
        let password = if let Some(name) = mysql_texts.remove("password_env") {
            PasswordSource::Env(name)
        } else if let Some(sealed_text) = mysql_texts.remove("password_dpapi") {
            match resolve_path(&sealed_text) {
                Ok(sealed_path) => PasswordSource::Dpapi(sealed_path),
                Err(error) => {
                    warnings.push(format!(
                        "mysql.password_dpapi `{sealed_text}` unresolved: {error}"
                    ));
                    PasswordSource::Unset
                }
            }
        } else {
            mysql_texts
                .remove("password")
                .map_or(PasswordSource::Unset, PasswordSource::Plain)
        };

        // 2. 필수 키와 범위
        let number_or =
            |key: &str, default: u64| mysql_numbers.get(key).copied().unwrap_or(default);
        let port = u16::try_from(number_or("port", DEFAULT_MYSQL_PORT));
        match (mysql_texts.remove("host"), mysql_texts.remove("user"), port) {
            (Some(host), Some(user), Ok(port)) => {
                settings.mysql = Some(MysqlSettings {
                    host,
                    port,
                    user,
                    database: mysql_texts.remove("database"),
                    password,
                    connect_timeout_ms: number_or("connect_timeout_ms", DEFAULT_MYSQL_TIMEOUT_MS),
                    query_timeout_ms: number_or("query_timeout_ms", DEFAULT_MYSQL_TIMEOUT_MS),
                    max_rows: usize::try_from(number_or("max_rows", DEFAULT_MYSQL_MAX_ROWS))
                        .unwrap_or(usize::MAX),
                });
            }
            (_, _, Err(_)) => {
                warnings.push("mysql.port must be between 1 and 65535".to_string());
            }
            _ => warnings
                .push("mysql.host and mysql.user are required; mysql-query stays off".to_string()),
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
    fn parses_mysql_section() {
        let text = "[mysql]\nhost = \"db.example.invalid\"\nport = 3307\nuser = 'reader'\ndatabase = \"sample\"\npassword_env = \"SAMPLE_DB_PASS\"\npassword = \"ignored\"\nquery_timeout_ms = 5_000\nmax_rows = 200\n";
        let (settings, warnings) = parse_settings(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            settings.mysql,
            Some(MysqlSettings {
                host: "db.example.invalid".to_string(),
                port: 3307,
                user: "reader".to_string(),
                database: Some("sample".to_string()),
                password: PasswordSource::Env("SAMPLE_DB_PASS".to_string()),
                connect_timeout_ms: 10_000,
                query_timeout_ms: 5_000,
                max_rows: 200,
            })
        );
    }
    #[test]
    fn mysql_needs_host_and_user() {
        let (settings, warnings) = parse_settings("[mysql]\nhost = \"db.example.invalid\"\n");
        assert!(settings.mysql.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        let text = "[mysql]\nhost = \"h\"\nuser = \"u\"\nport = 70000\nmax_rows = 0\n";
        let (settings, warnings) = parse_settings(text);
        assert!(settings.mysql.is_none());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
    #[test]
    fn plain_password_stays_out_of_debug_output() {
        let text = "[mysql]\nhost = \"h\"\nuser = \"u\"\npassword = ' sentinel-secret '\n";
        let (settings, warnings) = parse_settings(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mysql = settings.mysql.clone().unwrap();
        assert_eq!(
            mysql.password,
            PasswordSource::Plain(" sentinel-secret ".to_string())
        );
        assert!(!format!("{settings:?}").contains("sentinel-secret"));
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
