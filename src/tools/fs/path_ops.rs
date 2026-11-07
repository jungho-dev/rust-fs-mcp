//! path_ops.rs
//! tools::fs::path_ops
//!
//! path-copy / path-move / path-remove / path-stat handlers.
//! path-move falls back to copy + remove-source when rename cannot cross volumes.
//!

use super::shared::{ensure_parent_dir, path_touched, transfer_touched};
use crate::core::batch::{
    STAT_PLAN, create_batch_response, run_batch_mutation, run_batch_parallel,
};
use crate::core::config::{ensure_path_allowed, target_path};
use crate::core::response::RawResult;
use crate::tools::common::bool_field;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

// 1. Path tools ---------------------------------------------------------------
pub fn handle_path_copy(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, transfer_touched, copy_item);
    create_batch_response("path-copy", results, false)
}
pub fn handle_path_move(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, transfer_touched, move_item);
    create_batch_response("path-move", results, false)
}
pub fn handle_path_remove(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, path_touched, remove_item);
    create_batch_response("path-remove", results, false)
}
pub fn handle_path_stat(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array (e.g. paths:[\"C:/absolute/path\"])");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch_parallel(&items, STAT_PLAN, move |item| {
        info_item(item, allow_missing)
    });
    create_batch_response("path-stat", results, false)
}
fn copy_item(item: &Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match ensure_path_allowed(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let Ok(source_meta) = fs::metadata(&source) else {
        return RawResult::error(format!("Path does not exist: {}", source.display()));
    };
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let recursive = bool_field(item, "recursive", false);
    let force = bool_field(item, "force", false);
    if destination.exists() && !force {
        return RawResult::error(format!("Destination exists: {}", destination.display()));
    }
    let result = if source_meta.is_dir() {
        if !recursive {
            return RawResult::error("recursive must be true to copy a directory");
        }
        copy_dir_recursive(&source, &destination)
    } else {
        if let Err(error) = ensure_parent_dir(&destination) {
            return RawResult::error(error);
        }
        fs::copy(&source, &destination)
            .map(|_| ())
            .map_err(|error| error.to_string())
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Copied {} -> {}", source.display(), destination.display()),
            json!({
                "source": source.display().to_string(),
                "destination": destination.display().to_string()
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to copy: {error}")),
    }
}
fn move_item(item: &Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match ensure_path_allowed(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if fs::metadata(&source).is_err() {
        return RawResult::error(format!("Path does not exist: {}", source.display()));
    }
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&destination) {
        return RawResult::error(error);
    }
    let moved = match fs::rename(&source, &destination) {
        Ok(()) => Ok(()),
        // rename cannot cross volumes (C: -> D: fails with NOT_SAME_DEVICE / EXDEV),
        // so fall back to copy + remove-source for cross-device moves.
        Err(error) if is_cross_device(&error) => copy_then_remove(&source, &destination),
        Err(error) => Err(error.to_string()),
    };
    match moved {
        Ok(()) => RawResult::structured(
            format!("Moved {} -> {}", source.display(), destination.display()),
            json!({
                "source": source.display().to_string(),
                "destination": destination.display().to_string()
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to move: {error}")),
    }
}
fn remove_item(item: &Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let force = bool_field(item, "force", false);
    let Ok(metadata) = fs::metadata(&path) else {
        if force {
            return RawResult::structured(
                format!("Already absent: {}", path.display()),
                json!({ "path": path.display().to_string(), "removed": false }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    };
    let result = if metadata.is_dir() {
        if !bool_field(item, "recursive", false) {
            return RawResult::error("recursive must be true to remove a directory");
        }
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Removed {}", path.display()),
            json!({ "path": path.display().to_string(), "removed": true }),
        ),
        Err(error) => RawResult::error(format!("Failed to remove {}: {error}", path.display())),
    }
}
fn info_item(item: &Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) if allow_missing => {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        Err(_) => {
            return RawResult::error(format!("Path does not exist: {}", path.display()));
        }
    };

    RawResult::structured(
        format!("{}: {} bytes", path.display(), metadata.len()),
        json!({
            "path": path.display().to_string(),
            "isDirectory": metadata.is_dir(),
            "isFile": metadata.is_file(),
            "len": metadata.len(),
            "readonly": metadata.permissions().readonly(),
            "modified": timestamp(metadata.modified().ok()),
            "created": timestamp(metadata.created().ok()),
            "accessed": timestamp(metadata.accessed().ok())
        }),
    )
}
// 2. Copy and cross-device helpers --------------------------------------------
// Windows ERROR_NOT_SAME_DEVICE(17) / POSIX EXDEV(18): rename refused across volumes.
fn is_cross_device(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::CrossesDevices {
        return true;
    }
    match error.raw_os_error() {
        Some(17) => cfg!(windows),
        Some(18) => !cfg!(windows),
        _ => false,
    }
}
fn copy_then_remove(source: &Path, destination: &Path) -> Result<(), String> {
    if source.is_dir() {
        copy_dir_recursive(source, destination)?;
        fs::remove_dir_all(source).map_err(|error| {
            format!(
                "copied but failed to remove source {}: {error}",
                source.display()
            )
        })
    } else {
        fs::copy(source, destination)
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        fs::remove_file(source).map_err(|error| {
            format!(
                "copied but failed to remove source {}: {error}",
                source.display()
            )
        })
    }
}
fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        // 심링크/정션은 따라가지 않음(조상 가리키는 링크의 순환 재귀로 인한 무한 복사·스택오버플로 방지).
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)
                .map(|_| ())
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
fn timestamp(value: Option<std::time::SystemTime>) -> Option<u64> {
    value
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;

    #[test]
    fn copy_then_remove_moves_file_and_dir() {
        let dir = make_temp_dir("rust-fs-mcp-xdev-move");
        let src_file = dir.join("a.txt");
        std::fs::write(&src_file, "payload").unwrap();
        let dst_file = dir.join("b.txt");
        copy_then_remove(&src_file, &dst_file).unwrap();
        assert!(!src_file.exists());
        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), "payload");
        let src_dir = dir.join("srcdir");
        std::fs::create_dir_all(src_dir.join("inner")).unwrap();
        std::fs::write(src_dir.join("inner").join("f.txt"), "x").unwrap();
        let dst_dir = dir.join("dstdir");
        copy_then_remove(&src_dir, &dst_dir).unwrap();
        assert!(!src_dir.exists());
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("inner").join("f.txt")).unwrap(),
            "x"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
