//! config.rs
//! core::config
//!
//! Path resolution layer: ~ home expansion, Git-Bash /c drive translation, lexical
//! normalization, and mutation-batch canonical keys.
//! There is no allowed-root policy; ensure_path_allowed/target_path resolve paths directly
//! (fixed runtime model - the MCP host owns permissioning).
//!

use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

static CURRENT_DIR_CACHE: OnceLock<Result<PathBuf, String>> = OnceLock::new();

// 1. Path resolution -----------------------------------------------------------
pub fn resolve_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
  let expanded = expand_home(path.as_ref());
  let expanded = expand_posix_drive(&expanded).unwrap_or(expanded);
  let absolute = if expanded.is_absolute() { expanded } else { current_dir_cached()?.join(expanded) };

  Ok(normalize_lexical(&absolute))
}
fn current_dir_cached() -> Result<&'static PathBuf, String> {
  CURRENT_DIR_CACHE.get_or_init(|| env::current_dir().map_err(|error| format!("Failed to read current directory: {error}"))).as_ref().map_err(Clone::clone)
}
pub fn ensure_path_allowed(path: impl AsRef<Path>) -> Result<PathBuf, String> {
  resolve_path(path)
}
pub fn existing_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
  let resolved = ensure_path_allowed(path)?;
  if !resolved.exists() {
    return Err(format!("Path does not exist: {}", resolved.display()));
  }
  Ok(resolved)
}
pub fn target_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
  resolve_path(path)
}
// Mutation batch 독립성 판정용 정규 키: resolve + canonical 비교형(소문자/`/` 구분자).
pub fn canonical_key(path: &str) -> String {
  match resolve_path(path) {
    Ok(resolved) => comparable_path(&resolved),
    Err(_) => {
      let mut fallback = path.replace('\\', "/");
      if cfg!(windows) {
        fallback = fallback.to_ascii_lowercase();
      }
      fallback.trim_end_matches('/').to_string()
    }
  }
}
fn comparable_path(path: &Path) -> String {
  let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
  let normalized = normalize_lexical(&path);
  let mut value = normalized.to_string_lossy().replace('\\', "/");
  if let Some(stripped) = value.strip_prefix("//?/") {
    value = stripped.to_string();
  }
  if cfg!(windows) {
    value = value.to_ascii_lowercase();
  }
  value.trim_end_matches('/').to_string()
}
fn expand_home(path: &Path) -> PathBuf {
  let text = path.to_string_lossy();
  let is_home = text == "~" || text.starts_with("~/") || text.starts_with("~\\");
  if !is_home {
    return path.to_path_buf();
  }
  let Some(home) = home_dir() else {
    return path.to_path_buf();
  };
  let rest = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")).unwrap_or("");
  home.join(rest)
}
// Git-Bash 스타일 "/c/..." 경로는 Windows에서 드라이브 없는 루트로 조인되어 "C:\c\..."가
// 되므로, 한 글자 ASCII 세그먼트로 시작하면 "<드라이브>:\..." 절대 경로로 번역.
fn expand_posix_drive(path: &Path) -> Option<PathBuf> {
  if !cfg!(windows) {
    return None;
  }
  let text = path.to_string_lossy();
  let bytes = text.as_bytes();
  let is_sep = |byte: u8| byte == b'/' || byte == b'\\';
  if bytes.len() < 2 || !is_sep(bytes[0]) || !bytes[1].is_ascii_alphabetic() {
    return None;
  }
  let drive = bytes[1].to_ascii_uppercase() as char;
  if bytes.len() == 2 {
    return Some(PathBuf::from(format!("{drive}:\\")));
  }
  if !is_sep(bytes[2]) {
    return None;
  }
  Some(PathBuf::from(format!("{drive}:\\{}", &text[3..])))
}
fn home_dir() -> Option<PathBuf> {
  env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from)
}
fn normalize_lexical(path: &Path) -> PathBuf {
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      item => normalized.push(item.as_os_str()),
    }
  }
  normalized
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolves_relative_path() {
    let path = resolve_path(".").unwrap();
    assert!(path.is_absolute());
  }
  #[test]
  fn translates_posix_drive_path_on_windows() {
    if !cfg!(windows) {
      return;
    }
    let path = resolve_path("/c/git/repo").unwrap();
    assert_eq!(path.to_string_lossy().to_ascii_lowercase(), "c:\\git\\repo");
    let root = resolve_path("/d").unwrap();
    assert_eq!(root.to_string_lossy().to_ascii_lowercase(), "d:\\");
    // 두 글자 이상 첫 세그먼트(/dev 등)는 드라이브로 보지 않는다.
    let other = resolve_path("/dev/null").unwrap();
    assert!(other.to_string_lossy().to_ascii_lowercase().ends_with("dev\\null"));
  }
}
