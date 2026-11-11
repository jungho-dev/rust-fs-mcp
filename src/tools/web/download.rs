//! download.rs
//! tools::web::download
//!
//! download-to-file handler: sandboxed URL -> file streaming through a same-directory temp file.
//!

use super::{fetch_options, opt_bool, opt_str, opt_u64};
use crate::core::batch::{DOWNLOAD_PLAN, create_batch_response, run_batch, run_batch_parallel};
use crate::core::config::{canonical_key, target_path};
use crate::core::response::RawResult;
use crate::core::web::{FetchOptions, MAX_ALLOWED_BYTES, http_fetch_to_writer};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

// No default cap below the hard ceiling: an omitted maxBytes downloads up to MAX_ALLOWED_BYTES.
const DOWNLOAD_DEFAULT_MAX_BYTES: u64 = MAX_ALLOWED_BYTES;
// 목적지 확인·게시 직렬화
static PUBLISH_LOCK: Mutex<()> = Mutex::new(());

// 1. download-to-file (sandboxed URL -> file) ---------------------------------
pub fn handle_download_to_file(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    let opts = fetch_options(args, DOWNLOAD_DEFAULT_MAX_BYTES);
    let allow_private = false;
    // 목적지 중복 배치 순차 실행
    let mut targets = HashSet::new();
    let distinct = items
        .iter()
        .all(|item| opt_str(item, "path").is_none_or(|path| targets.insert(canonical_key(path))));
    let results = if distinct {
        run_batch_parallel(items, DOWNLOAD_PLAN, move |item| {
            download_one(item, &opts, allow_private)
        })
    } else {
        run_batch(items, |item| download_one(item, &opts, allow_private))
    };
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

    // 원본 선삭제 없는 교체 게시
    let published = {
        let _publish_guard = PUBLISH_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        if !overwrite && target.exists() {
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        } else {
            fs::rename(&temp, &target)
        }
    };
    if let Err(error) = published {
        let _ = fs::remove_file(&temp);
        let shown = target.display();
        return RawResult::error(
            if !overwrite && error.kind() == io::ErrorKind::AlreadyExists {
                format!("File already exists (set overwrite:true): {shown}")
            } else if overwrite && target.exists() {
                format!("Failed to replace {shown}: {error}")
            } else {
                format!("Failed to write {shown}: {error}")
            },
        );
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
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to create temp file: {error}")),
        }
    }
    Err("Failed to create temp file: exhausted candidates".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::web::serve_loopback_http;

    fn make_download_dir(label: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("rust-fs-mcp-download-{label}-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn serve_text_body(body: &'static str) -> (String, std::thread::JoinHandle<usize>) {
        serve_loopback_http(0, move |_| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        })
    }

    #[test]
    fn download_requires_url_and_path() {
        let result =
            handle_download_to_file(&json!({ "items": [{ "url": "https://example.com/" }] }));
        assert!(result.is_error);
    }
    #[test]
    fn overwrite_replaces_existing_file() {
        let dir = make_download_dir("overwrite");
        let target = dir.join("data.txt");
        fs::write(&target, "old").unwrap();
        let (url, server) = serve_text_body("new body");
        let result = handle_download_to_file(&json!({
            "items": [{ "url": url, "path": target.display().to_string(), "overwrite": true }]
        }));
        server.join().unwrap();
        assert!(!result.is_error, "{result:?}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new body");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn failed_publish_keeps_directory_target() {
        let dir = make_download_dir("blocked");
        let target = dir.join("occupied");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), "keep").unwrap();
        let (url, server) = serve_text_body("new body");
        let result = handle_download_to_file(&json!({
            "items": [{ "url": url, "path": target.display().to_string(), "overwrite": true }]
        }));
        server.join().unwrap();
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("Failed to replace"), "{text}");
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "keep");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn failed_rename_keeps_locked_original() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = make_download_dir("locked");
        let target = dir.join("data.txt");
        fs::write(&target, "old").unwrap();
        // 공유 없는 핸들 (교체 차단)
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        let (url, server) = serve_text_body("new body");
        let result = handle_download_to_file(&json!({
            "items": [{ "url": url, "path": target.display().to_string(), "overwrite": true }]
        }));
        server.join().unwrap();
        drop(locked);
        assert!(result.is_error, "{result:?}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn duplicate_destination_fails_second_item() {
        let dir = make_download_dir("duplicate");
        let target = dir.join("data.txt").display().to_string();
        let (first_url, first_server) = serve_text_body("first");
        let (second_url, second_server) = serve_text_body("second");
        let result = handle_download_to_file(&json!({
            "items": [
                { "url": first_url, "path": target },
                { "url": second_url, "path": target }
            ]
        }));
        first_server.join().unwrap();
        // 미요청 서버 해제 연결
        let second_addr = second_url
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let _ = std::net::TcpStream::connect(second_addr);
        second_server.join().unwrap();
        let outcomes = &result.structured.as_ref().unwrap()["results"];
        assert_eq!(outcomes[0]["ok"], true, "{result:?}");
        assert_eq!(outcomes[1]["ok"], false, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("File already exists"), "{text}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn late_destination_is_not_replaced() {
        let dir = make_download_dir("late");
        let target = dir.join("data.txt");
        let late_target = target.clone();
        // 응답 직전 목적지 선점
        let (url, server) = serve_loopback_http(0, move |_| {
            fs::write(&late_target, "external").unwrap();
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew".to_string()
        });
        let result = handle_download_to_file(&json!({
            "items": [{ "url": url, "path": target.display().to_string() }]
        }));
        server.join().unwrap();
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("File already exists"), "{text}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "external");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
