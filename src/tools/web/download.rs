//! download.rs
//! tools::web::download
//!
//! download-to-file handler: sandboxed URL -> file streaming through a same-directory temp file.
//!

use super::{fetch_options, opt_bool, opt_str, opt_u64};
use crate::core::batch::{DOWNLOAD_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::target_path;
use crate::core::response::RawResult;
use crate::core::web::{FetchOptions, MAX_ALLOWED_BYTES, http_fetch_to_writer};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// No default cap below the hard ceiling: an omitted maxBytes downloads up to MAX_ALLOWED_BYTES.
const DOWNLOAD_DEFAULT_MAX_BYTES: u64 = MAX_ALLOWED_BYTES;

// 1. download-to-file (sandboxed URL -> file) ---------------------------------
pub fn handle_download_to_file(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    let opts = fetch_options(args, DOWNLOAD_DEFAULT_MAX_BYTES);
    let allow_private = false;
    let results = run_batch_parallel(items, DOWNLOAD_PLAN, move |item| {
        download_one(item, &opts, allow_private)
    });
    create_batch_response("download-to-file", results, true)
}

fn download_one(item: &Value, base_opts: &FetchOptions, allow_private: bool) -> RawResult {
    let Some(url) = opt_str(item, "url") else {
        return RawResult::error("url must be a string");
    };
    let Some(path) = opt_str(item, "path") else {
        return RawResult::error("path must be a string");
    };
    let target = match target_path(path) {
        Ok(target) => target,
        Err(error) => return RawResult::error(error),
    };
    let overwrite = opt_bool(item, "overwrite", false);
    if target.exists() && !overwrite {
        return RawResult::error(format!(
            "File already exists (set overwrite:true): {}",
            target.display()
        ));
    }

    let mut opts = base_opts.clone();
    if let Some(value) = opt_u64(item, "maxBytes") {
        opts.max_bytes = value;
    }
    if let Some(value) = opt_u64(item, "timeoutMs") {
        opts.timeout_ms = value;
    }
    if let Some(parent) = target.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return RawResult::error(format!("Failed to create parent directory: {error}"));
    }
    // 전체 본문을 메모리에 버퍼링하지 않고 임시 파일로 스트림 후 rename(go 동일: 부분
    // 다운로드가 목적지 경로에 남지 않고, 대용량도 RSS를 부풀리지 않는다).
    let temp = match create_download_temp(&target) {
        Ok(temp) => temp,
        Err(error) => return RawResult::error(error),
    };
    let meta = {
        let mut file = match fs::OpenOptions::new().write(true).open(&temp) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return RawResult::error(format!("Failed to open temp file: {error}"));
            }
        };
        match http_fetch_to_writer(url, &opts, allow_private, &mut file) {
            Ok(meta) => {
                if let Err(error) = file.flush() {
                    drop(file);
                    let _ = fs::remove_file(&temp);
                    return RawResult::error(format!(
                        "Failed to write {}: {error}",
                        target.display()
                    ));
                }
                meta
            }
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&temp);
                return RawResult::error(error);
            }
        }
    };
    if overwrite
        && target.exists()
        && let Err(error) = fs::remove_file(&target)
    {
        let _ = fs::remove_file(&temp);
        return RawResult::error(format!("Failed to replace {}: {error}", target.display()));
    }
    if let Err(error) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return RawResult::error(format!("Failed to write {}: {error}", target.display()));
    }

    RawResult::structured(
        format!(
            "Downloaded {} bytes from {} to {}",
            meta.bytes,
            meta.final_url,
            target.display()
        ),
        json!({
            "url": url,
            "finalUrl": meta.final_url,
            "path": target.display().to_string(),
            "bytes": meta.bytes,
            "status": meta.status,
            "contentType": meta.content_type
        }),
    )
}

// 목적지와 같은 디렉터리에 충돌 없는 임시 파일을 만든다(create_new로 경합 방지).
fn create_download_temp(target: &Path) -> Result<PathBuf, String> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    for attempt in 0u32..16 {
        let candidate = dir.join(format!(".download-{stamp}-{attempt}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to create temp file: {error}")),
        }
    }
    Err("Failed to create temp file: exhausted candidates".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_requires_url_and_path() {
        let result =
            handle_download_to_file(&json!({ "items": [{ "url": "https://example.com/" }] }));
        assert!(result.is_error);
    }
}
