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
use crate::core::encoding::{decode_bytes, has_utf16_bom, looks_utf16_prefix};
use crate::core::response::RawResult;
use crate::core::sql::Sha256;
use crate::tools::args::common::bool_field;
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

// 0-1. Content scan -----------------------------------------------------------
// path-stat lines/hash: 64 KiB 청크 한 번 스트리밍, 첨 8 KiB NUL 이면 바이너리
const SCAN_CHUNK_BYTES: usize = 64 * 1024;
const BINARY_PROBE_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy)]
struct ContentProbe {
    lines: bool,
    hash: bool,
}
struct TextStats {
    line_count: u64,
    crlf_lines: u64,
    lf_lines: u64,
    final_newline: bool,
    encoding: &'static str,
    lossy: bool,
}
struct ContentStats {
    sha256: Option<String>,
    text: Option<TextStats>,
}
// 줄바꿈 계수기: 바이트 단위(UTF-8, EUC-KR) 또는 UTF-16 코드 단위, 청크 경계의 \r|\n 과 홀수 바이트 이어 받기
#[derive(Default)]
struct NewlineCounter {
    started: bool,
    utf16: Option<bool>,
    pending: Option<u8>,
    last: Option<u16>,
    newlines: u64,
    crlf: u64,
}
impl NewlineCounter {
    fn feed(&mut self, chunk: &[u8]) {
        // 1. 첫 청크에서 BOM 판정 후 건너뜀
        let mut chunk = chunk;
        let first_chunk = !self.started;
        self.started = true;
        if first_chunk && has_utf16_bom(chunk) {
            self.utf16 = Some(chunk[0] == 0xFE);
            chunk = &chunk[2..];
        } else if first_chunk && chunk.starts_with(&[0xEF, 0xBB, 0xBF]) {
            chunk = &chunk[3..];
        } else if first_chunk && looks_utf16_prefix(chunk) {
            // BOM 없는 UTF-16: 스니프한 바이트 순서
            self.utf16 = Some(decode_bytes(chunk.to_vec()).encoding.name() == "UTF-16BE");
        }
        let Some(big_endian) = self.utf16 else {
            // 2. 바이트 단위
            for index in memchr::memchr_iter(b'\n', chunk) {
                self.newlines += 1;
                let previous = index
                    .checked_sub(1)
                    .map_or(self.last, |at| Some(u16::from(chunk[at])));
                self.crlf += u64::from(previous == Some(u16::from(b'\r')));
            }
            if let Some(&byte) = chunk.last() {
                self.last = Some(u16::from(byte));
            }
            return;
        };
        // 3. UTF-16 코드 단위
        let mut bytes = chunk.iter().copied();
        while let Some(first) = self.pending.take().or_else(|| bytes.next()) {
            let Some(second) = bytes.next() else {
                self.pending = Some(first);
                break;
            };
            let unit = if big_endian {
                u16::from_be_bytes([first, second])
            } else {
                u16::from_le_bytes([first, second])
            };
            if unit == u16::from(b'\n') {
                self.newlines += 1;
                self.crlf += u64::from(self.last == Some(u16::from(b'\r')));
            }
            self.last = Some(unit);
        }
    }
}

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
    let hash = match args.get("hash") {
        None | Some(Value::Null) => false,
        Some(Value::String(name)) if name == "sha256" => true,
        Some(_) => return RawResult::error("hash must be \"sha256\""),
    };
    let probe = ContentProbe {
        lines: bool_field(args, "lines", false),
        hash,
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch_parallel(&items, STAT_PLAN, move |item| {
        info_item(item, allow_missing, probe)
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
        // 자기 하위 복사 차단 (무한 재귀)
        let source_root = fs::canonicalize(&source).unwrap_or_else(|_| source.clone());
        let nests_in_source = destination
            .ancestors()
            .find_map(|ancestor| fs::canonicalize(ancestor).ok())
            .is_some_and(|existing| existing.starts_with(&source_root));
        if nests_in_source {
            return RawResult::error(format!(
                "Cannot copy a directory into itself: {} -> {}",
                source.display(),
                destination.display()
            ));
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
fn info_item(item: &Value, allow_missing: bool, probe: ContentProbe) -> RawResult {
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

    let mut text = format!("{}: {} bytes", path.display(), metadata.len());
    let mut structured = json!({
        "path": path.display().to_string(),
        "isDirectory": metadata.is_dir(),
        "isFile": metadata.is_file(),
        "len": metadata.len(),
        "readonly": metadata.permissions().readonly(),
        "modified": timestamp(metadata.modified().ok()),
        "created": timestamp(metadata.created().ok()),
        "accessed": timestamp(metadata.accessed().ok())
    });
    if !metadata.is_file() || !(probe.lines || probe.hash) {
        return RawResult::structured(text, structured);
    }

    // 내용 통계 (일반 파일만)
    let stats = match scan_content(&path, probe) {
        Ok(stats) => stats,
        Err(error) => return RawResult::error(error),
    };
    match (&stats.text, probe.lines) {
        (Some(lines), true) => {
            text.push_str(&format!(
                " lines={} crlf={} lf={} finalNewline={} encoding={}",
                lines.line_count,
                lines.crlf_lines,
                lines.lf_lines,
                lines.final_newline,
                lines.encoding
            ));
            structured["lineCount"] = json!(lines.line_count);
            structured["crlfLines"] = json!(lines.crlf_lines);
            structured["lfLines"] = json!(lines.lf_lines);
            structured["finalNewline"] = json!(lines.final_newline);
            structured["encoding"] = json!(lines.encoding);
            if lines.lossy {
                structured["lossy"] = json!(true);
            }
        }
        (None, true) => {
            text.push_str(" binary=true");
            structured["binary"] = json!(true);
        }
        (_, false) => {}
    }
    if let Some(sha256) = stats.sha256 {
        text.push_str(&format!(" sha256={sha256}"));
        structured["sha256"] = json!(sha256);
    }
    RawResult::structured(text, structured)
}
// path-find lines: 텍스트 파일 줄 수 (바이너리·읽기 실패는 None)
pub(crate) fn text_line_count(path: &Path) -> Option<u64> {
    let probe = ContentProbe {
        lines: true,
        hash: false,
    };
    scan_content(path, probe)
        .ok()?
        .text
        .map(|text| text.line_count)
}
// path-stat 내용 스트리밍: 해시와 줄 통계를 한 번에 계산
fn scan_content(path: &Path, probe: ContentProbe) -> Result<ContentStats, String> {
    // 1. 청크 스트리밍
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Failed to open {}: {error}", path.display()))?;
    let mut buffer = vec![0u8; SCAN_CHUNK_BYTES];
    let mut hasher = probe.hash.then(Sha256::new);
    let mut counter = NewlineCounter::default();
    let mut sample: Vec<u8> = Vec::new();
    let mut sample_truncated = false;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(chunk);
        }
        if probe.lines {
            counter.feed(chunk);
        }
        let room = SCAN_CHUNK_BYTES - sample.len();
        sample_truncated |= read > room;
        sample.extend_from_slice(&chunk[..read.min(room)]);
    }
    let sha256 = hasher.map(|hasher| {
        hasher
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });

    // 2. 바이너리 판정과 인코딩 표본
    let probe_end = sample.len().min(BINARY_PROBE_BYTES);
    let binary = memchr::memchr(0, &sample[..probe_end]).is_some() && !looks_utf16_prefix(&sample);
    if !probe.lines || binary {
        return Ok(ContentStats { sha256, text: None });
    }
    // 잘린 표본 끝의 미완성 문자는 판정에서 뚝다
    // UTF-16: 홀수 바이트와 상위 서로게이트, UTF-8: 미완성 연속, EUC-KR: 짝 없는 앞 바이트
    let even = sample.len() & !1;
    let high_surrogate = counter
        .utf16
        .zip(even.checked_sub(2).map(|at| [sample[at], sample[at + 1]]))
        .is_some_and(|(big_endian, pair)| {
            let unit = if big_endian {
                u16::from_be_bytes(pair)
            } else {
                u16::from_le_bytes(pair)
            };
            (0xD800..0xDC00).contains(&unit)
        });
    let utf8 =
        std::str::from_utf8(&sample).map_err(|error| (error.valid_up_to(), error.error_len()));
    let high_tail = sample
        .iter()
        .rev()
        .take_while(|byte| **byte >= 0x80)
        .count();
    let keep = match (sample_truncated, counter.utf16, utf8) {
        (false, _, _) | (true, None, Ok(_)) => sample.len(),
        (true, Some(_), _) => even - if high_surrogate { 2 } else { 0 },
        (true, None, Err((valid_up_to, None))) => valid_up_to,
        (true, None, Err((_, Some(_)))) => sample.len() - high_tail % 2,
    };
    sample.truncate(keep);
    let decoded = decode_bytes(sample);

    // 3. 줄 통계
    let final_newline = counter.last == Some(u16::from(b'\n'));
    let open_tail = u64::from(counter.last.is_some() && !final_newline);
    Ok(ContentStats {
        sha256,
        text: Some(TextStats {
            line_count: counter.newlines + open_tail,
            crlf_lines: counter.crlf,
            lf_lines: counter.newlines - counter.crlf,
            final_newline,
            encoding: decoded.encoding.name(),
            lossy: decoded.lossy,
        }),
    })
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

    fn stat_one(path: &Path, extra: Value) -> Value {
        let mut args = json!({ "paths": [path.display().to_string()] });
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        let result = info_item(
            &json!({ "path": path.display().to_string() }),
            false,
            ContentProbe {
                lines: bool_field(&args, "lines", false),
                hash: args.get("hash").is_some(),
            },
        );
        assert!(!result.is_error, "{result:?}");
        result.structured.unwrap()
    }

    #[test]
    fn path_stat_counts_lines_and_hashes() {
        let dir = make_temp_dir("rust-fs-mcp-stat-lines");
        let both = json!({ "lines": true, "hash": "sha256" });
        // (이름, 내용, lineCount, crlfLines, lfLines, finalNewline)
        type LineCase<'a> = (&'a str, &'a [u8], u64, u64, u64, bool);
        let cases: [LineCase; 4] = [
            ("crlf.txt", b"a\r\nb\r\n", 2, 2, 0, true),
            ("mixed.txt", b"a\nb\r\nc", 3, 1, 1, false),
            ("empty.txt", b"", 0, 0, 0, false),
            ("bom.txt", b"\xEF\xBB\xBFx\n", 1, 0, 1, true),
        ];
        for (name, body, lines, crlf, lf, final_newline) in cases {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            let stat = stat_one(&path, both.clone());
            assert_eq!(stat["lineCount"], lines, "{name}");
            assert_eq!(stat["crlfLines"], crlf, "{name}");
            assert_eq!(stat["lfLines"], lf, "{name}");
            assert_eq!(stat["finalNewline"], final_newline, "{name}");
            assert_eq!(stat["encoding"], "UTF-8", "{name}");
        }
        let empty = stat_one(&dir.join("empty.txt"), both.clone());
        assert_eq!(
            empty["sha256"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        // 64 KiB 경계에 걸친 \r\n 과 잘린 표본 끝 한글
        let mut boundary = "가".repeat(SCAN_CHUNK_BYTES / 3).into_bytes();
        boundary.truncate(SCAN_CHUNK_BYTES - 1);
        boundary.resize(SCAN_CHUNK_BYTES - 1, b'x');
        boundary.extend_from_slice("\r\n가가\n".as_bytes());
        let path = dir.join("boundary.txt");
        std::fs::write(&path, &boundary).unwrap();
        let stat = stat_one(&path, json!({ "lines": true }));
        assert_eq!(stat["crlfLines"], 1);
        assert_eq!(stat["lfLines"], 1);
        assert_eq!(stat["lineCount"], 2);
        assert!(stat.get("sha256").is_none());

        // EUC-KR, UTF-16LE BOM, 바이너리
        let euc = dir.join("euc.txt");
        std::fs::write(&euc, b"\xC7\xD1\xB1\xDB\r\nab\r\n").unwrap();
        let stat = stat_one(&euc, json!({ "lines": true }));
        assert_eq!(stat["encoding"], "EUC-KR");
        assert_eq!(stat["crlfLines"], 2);
        let utf16 = dir.join("utf16.txt");
        std::fs::write(&utf16, b"\xFF\xFEa\x00\r\x00\n\x00b\x00").unwrap();
        let stat = stat_one(&utf16, json!({ "lines": true }));
        assert_eq!(stat["encoding"], "UTF-16LE");
        assert_eq!(stat["lineCount"], 2);
        assert_eq!(stat["crlfLines"], 1);
        let bare16: Vec<u8> = "가a\r\nb\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let bare = dir.join("utf16-nobom.txt");
        std::fs::write(&bare, &bare16).unwrap();
        let stat = stat_one(&bare, json!({ "lines": true }));
        assert_eq!(stat["encoding"], "UTF-16LE");
        assert_eq!(stat["lineCount"], 2);
        assert_eq!(stat["crlfLines"], 2);
        // 64 KiB 표본 경계에 걸친 EUC-KR 쌍과 UTF-16 서로게이트 쌍
        let mut euc_edge = vec![b'a'];
        for _ in 0..(SCAN_CHUNK_BYTES / 2) {
            euc_edge.extend_from_slice(&[0xC7, 0xD1]);
        }
        let euc_edge_path = dir.join("euc-edge.txt");
        std::fs::write(&euc_edge_path, &euc_edge).unwrap();
        let stat = stat_one(&euc_edge_path, json!({ "lines": true }));
        assert_eq!(stat["encoding"], "EUC-KR");
        assert!(stat.get("lossy").is_none());
        let mut utf16_edge: Vec<u8> = "a\n"
            .repeat(SCAN_CHUNK_BYTES / 4 - 1)
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        utf16_edge.extend("\u{1F600}\n".encode_utf16().flat_map(u16::to_le_bytes));
        utf16_edge.splice(0..0, [b'a', 0]);
        let utf16_edge_path = dir.join("utf16-edge.txt");
        std::fs::write(&utf16_edge_path, &utf16_edge).unwrap();
        let stat = stat_one(&utf16_edge_path, json!({ "lines": true }));
        assert_eq!(stat["encoding"], "UTF-16LE");
        assert_eq!(stat["lineCount"], SCAN_CHUNK_BYTES / 4);
        let binary = dir.join("bin.dat");
        std::fs::write(&binary, b"ab\x00cd").unwrap();
        let stat = stat_one(&binary, both.clone());
        assert_eq!(stat["binary"], true);
        assert!(stat.get("lineCount").is_none());
        assert_eq!(stat["sha256"].as_str().unwrap().len(), 64);

        // 디렉터리는 새 필드 없음, 잘못된 hash 는 호출 오류
        let stat = stat_one(&dir, both);
        assert!(stat.get("lineCount").is_none() && stat.get("sha256").is_none());
        let wrong =
            handle_path_stat(&json!({ "paths": [dir.display().to_string()], "hash": "md5" }));
        assert!(wrong.is_error);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
