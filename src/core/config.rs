//! config.rs
//! core::config
//!
//! Process-local config layer that owns RuntimeConfig and the path-allowed boundary.
//! Exposes allowedDirectories parsing, ~ home expansion, lexical normalization, and cached path validation.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::batch::{create_batch_response, run_batch};
use crate::core::response::RawResult;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeConfig {
    pub allowed_directories: Vec<PathBuf>,
}

// Keeps the public `RuntimeConfig` schema (public contract) intact while internally
// caching the normalized comparable form (`allowed_cmp`) of allowed_directories once,
// removing the per-call `comparable_path` × N cost on the path_allowed hot path.
struct ConfigState {
    config: RuntimeConfig,
    allowed_cmp: Vec<String>,
}

static CONFIG: OnceLock<RwLock<ConfigState>> = OnceLock::new();
static PATH_ALLOWED_CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
static CURRENT_DIR_CACHE: OnceLock<Result<PathBuf, String>> = OnceLock::new();

// 1. Configuration access -----------------------------------------------------
pub fn resolve_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let expanded = expand_home(path.as_ref());
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        current_dir_cached()?.join(expanded)
    };

    Ok(normalize_lexical(&absolute))
}

fn current_dir_cached() -> Result<&'static PathBuf, String> {
    CURRENT_DIR_CACHE
        .get_or_init(|| {
            env::current_dir().map_err(|error| format!("Failed to read current directory: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub fn ensure_path_allowed(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let resolved = resolve_path(path)?;
    if path_allowed(&resolved) {
        Ok(resolved)
    } else {
        Err(format!(
            "Path is outside allowedDirectories: {}",
            resolved.display()
        ))
    }
}

pub fn existing_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let resolved = ensure_path_allowed(path)?;
    if !resolved.exists() {
        return Err(format!("Path does not exist: {}", resolved.display()));
    }

    Ok(resolved)
}

pub fn target_path(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let resolved = ensure_path_allowed(path)?;
    if let Some(parent) = resolved.parent() {
        ensure_path_allowed(parent)?;
    }

    Ok(resolved)
}

// 2. set_config_values --------------------------------------------------------
pub fn handle_set_config_values(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, apply_config_item);
    create_batch_response("set_config_values", results, false)
}

fn apply_config_item(item: &Value) -> RawResult {
    let Some(key) = item.get("key").and_then(Value::as_str) else {
        return RawResult::error("key must be a string");
    };

    let value = match read_config_value(item) {
        Ok(value) => value,
        Err(error) => return RawResult::error(error),
    };

    let mut state = config_cell().write().unwrap();
    match key {
        "allowedDirectories" | "allowed_directories" => {
            let Some(paths) = string_list(&value) else {
                return RawResult::error("allowedDirectories must be a string array");
            };

            let resolved = paths
                .iter()
                .map(resolve_path)
                .collect::<Result<Vec<_>, _>>();
            let Ok(resolved) = resolved else {
                return RawResult::error(resolved.err().unwrap());
            };

            state.allowed_cmp = resolved.iter().map(|path| comparable_path(path)).collect();
            state.config.allowed_directories = resolved;
            clear_path_allowed_cache();
        }
        _ => return RawResult::error(format!("Unsupported config key: {key}")),
    }

    RawResult::structured(
        format!("Updated {key}"),
        json!({
            "key": key,
            "config": config_snapshot(&state.config)
        }),
    )
}

fn read_config_value(item: &Value) -> Result<Value, String> {
    if let Some(path) = item.get("value_path").and_then(Value::as_str) {
        let offset = item
            .get("value_offset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = item
            .get("value_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let text = read_text_slice(path, offset, length)?;
        let parsed = serde_json::from_str(&text).unwrap_or(Value::String(text));
        return Ok(parsed);
    }

    item.get("value")
        .cloned()
        .ok_or_else(|| "value or value_path is required".to_string())
}

fn config_cell() -> &'static RwLock<ConfigState> {
    CONFIG.get_or_init(|| {
        let config = default_config();
        let allowed_cmp = config
            .allowed_directories
            .iter()
            .map(|path| comparable_path(path))
            .collect();
        RwLock::new(ConfigState {
            config,
            allowed_cmp,
        })
    })
}

fn default_config() -> RuntimeConfig {
    RuntimeConfig {
        allowed_directories: env_allowed_dirs(),
    }
}

fn env_allowed_dirs() -> Vec<PathBuf> {
    let Some(value) = env::var_os("RUST_FS_MCP_ALLOWED_DIRECTORIES") else {
        return Vec::new();
    };

    env::split_paths(&value)
        .filter_map(|path| resolve_path(path).ok())
        .collect()
}

fn path_allowed(path: &Path) -> bool {
    let state = config_cell().read().unwrap();
    if state.allowed_cmp.is_empty() {
        return true;
    }
    let cache = PATH_ALLOWED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(allowed) = cache.lock().unwrap().get(path).copied() {
        return allowed;
    }

    let candidate = comparable_path(path);
    let allowed = state
        .allowed_cmp
        .iter()
        .any(|prefix| candidate.starts_with(prefix));
    drop(state);
    cache.lock().unwrap().insert(path.to_path_buf(), allowed);
    allowed
}

fn clear_path_allowed_cache() {
    if let Some(cache) = PATH_ALLOWED_CACHE.get() {
        cache.lock().unwrap().clear();
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

fn string_list(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn config_snapshot(config: &RuntimeConfig) -> Value {
    json!({
        "allowedDirectories": config
            .allowed_directories
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
    })
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
    let rest = text
        .strip_prefix("~/")
        .or_else(|| text.strip_prefix("~\\"))
        .unwrap_or("");
    home.join(rest)
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
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
}
