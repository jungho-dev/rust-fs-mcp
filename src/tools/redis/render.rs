//! render.rs
//! tools::redis::render
//!
//! redis-command output: one JSON line per command, reply values mapped to JSON, and the stop and truncation tails.
//! Bulk text stays a string when it is UTF-8, otherwise {"base64":..}; a cut reply carries truncated:true.
//!

use crate::core::redis::Reply;
use base64::{Engine as _, engine::general_purpose};
use serde_json::{Value, json};

const REDIRECT_NOTE: &str =
    "cluster redirect not followed; this tool talks only to the configured node";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchStop {
    ErrorReply { number: usize, rest: usize },
    ReadLimit { number: usize, rest: usize },
}

// 1. Reply mapping ------------------------------------------------------------
pub(super) fn render_reply(reply: &Reply, truncated: &mut bool) -> Value {
    // 1. 바이트 문자열 (잘린 다바이트 꼬리 제외)
    let render_bytes = |bytes: &[u8], cut: bool| -> Value {
        let valid_len = match std::str::from_utf8(bytes) {
            Ok(_) => bytes.len(),
            Err(error) if cut && error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return json!({ "base64": general_purpose::STANDARD.encode(bytes) }),
        };
        Value::String(String::from_utf8_lossy(&bytes[..valid_len]).into_owned())
    };

    // 2. 형별 변환
    match reply {
        Reply::Status(text) => Value::String(text.clone()),
        Reply::Error(text) => json!({ "error": text }),
        Reply::Integer(number) => json!(number),
        Reply::Bulk(bytes) => render_bytes(bytes, false),
        Reply::BulkCut { prefix, total } => {
            *truncated = true;
            json!({ "truncated": true, "bytes": total, "prefix": render_bytes(prefix, true) })
        }
        Reply::Nil => Value::Null,
        Reply::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_reply(item, truncated))
                .collect(),
        ),
        Reply::ArrayCut { items, total } => {
            *truncated = true;
            let kept: Vec<Value> = items
                .iter()
                .map(|item| render_reply(item, truncated))
                .collect();
            json!({ "truncated": true, "length": total, "items": kept })
        }
    }
}

// 2. Output lines -------------------------------------------------------------
pub(super) fn render_command_line(command: &str, reply: &Reply, truncated: &mut bool) -> String {
    let line = match reply {
        Reply::Error(text)
            if text.starts_with("MOVED ")
                || text.starts_with("ASK ")
                || text.starts_with("CLUSTERDOWN") =>
        {
            json!({ "cmd": command, "error": text, "note": REDIRECT_NOTE })
        }
        Reply::Error(text) => json!({ "cmd": command, "error": text }),
        _ => json!({ "cmd": command, "reply": render_reply(reply, truncated) }),
    };
    line.to_string()
}
pub(super) fn render_scan_line(
    command: &str,
    cursor: &str,
    items: &[Reply],
    truncated: &mut bool,
) -> String {
    let rendered: Vec<Value> = items
        .iter()
        .map(|item| render_reply(item, truncated))
        .collect();
    json!({ "cmd": command, "reply": { "cursor": cursor, "items": rendered } }).to_string()
}
pub(super) fn render_batch_body(
    lines: &[String],
    stopped: Option<BatchStop>,
    truncated: bool,
) -> String {
    let mut body = lines.join("\n");
    match stopped {
        Some(BatchStop::ErrorReply { number, rest }) => body.push_str(&format!(
            "\n[stopped: command {number} returned an error; {rest} later command(s) were not sent]"
        )),
        Some(BatchStop::ReadLimit { number, rest }) => body.push_str(&format!(
            "\n[stopped: the reply to command {number} passed the 64 MiB read limit; the connection was closed and {rest} later command(s) were not sent]"
        )),
        None => {}
    }
    if truncated {
        body.push_str(
            "\n[truncated: a reply passed maxItems or the output budget; narrow it (SCAN MATCH/COUNT, LRANGE/ZRANGE ranges, HSCAN) before retrying]",
        );
    }
    body
}
#[cfg(test)]
mod tests {
    use super::*;

    fn render_plain(reply: &Reply) -> (Value, bool) {
        let mut truncated = false;
        let rendered = render_reply(reply, &mut truncated);
        (rendered, truncated)
    }
    #[test]
    fn maps_each_reply_type() {
        let reply = Reply::Array(vec![
            Reply::Status("OK".to_string()),
            Reply::Integer(-7),
            Reply::Nil,
            Reply::Bulk("h\u{e9}llo \u{d55c}".as_bytes().to_vec()),
            Reply::Bulk(vec![0xff, 0x00]),
            Reply::Error("ERR queued".to_string()),
            Reply::Array(Vec::new()),
        ]);
        assert_eq!(
            render_plain(&reply),
            (
                json!(["OK", -7, null, "h\u{e9}llo \u{d55c}", { "base64": "/wA=" }, { "error": "ERR queued" }, []]),
                false
            )
        );
    }
    #[test]
    fn maps_cut_replies_with_truncated_flags() {
        let mut prefix = b"a".to_vec();
        prefix.push("\u{d55c}".as_bytes()[0]);
        let cut_bulk = Reply::BulkCut { prefix, total: 4 };
        assert_eq!(
            render_plain(&cut_bulk),
            (
                json!({ "truncated": true, "bytes": 4, "prefix": "a" }),
                true
            )
        );
        let binary_cut = Reply::BulkCut {
            prefix: vec![0xff, b'a'],
            total: 9,
        };
        assert_eq!(
            render_plain(&binary_cut).0,
            json!({ "truncated": true, "bytes": 9, "prefix": { "base64": "/2E=" } })
        );
        let cut_array = Reply::ArrayCut {
            items: vec![Reply::Integer(1), Reply::Bulk(b"x".to_vec())],
            total: 50,
        };
        assert_eq!(
            render_plain(&cut_array),
            (
                json!({ "truncated": true, "length": 50, "items": [1, "x"] }),
                true
            )
        );
    }
    #[test]
    fn renders_command_lines_with_redirect_notes() {
        let mut truncated = false;
        let reply = Reply::Bulk(b"v".to_vec());
        assert_eq!(
            render_command_line("GET", &reply, &mut truncated),
            r#"{"cmd":"GET","reply":"v"}"#
        );
        let wrong_type = Reply::Error("WRONGTYPE Operation".to_string());
        assert_eq!(
            render_command_line("LPUSH", &wrong_type, &mut truncated),
            r#"{"cmd":"LPUSH","error":"WRONGTYPE Operation"}"#
        );
        let moved = Reply::Error("MOVED 3999 10.0.0.2:6381".to_string());
        assert_eq!(
            render_command_line("GET", &moved, &mut truncated),
            r#"{"cmd":"GET","error":"MOVED 3999 10.0.0.2:6381","note":"cluster redirect not followed; this tool talks only to the configured node"}"#
        );
        let items = [Reply::Bulk(b"k1".to_vec())];
        assert_eq!(
            render_scan_line("SCAN", "0", &items, &mut truncated),
            r#"{"cmd":"SCAN","reply":{"cursor":"0","items":["k1"]}}"#
        );
        assert!(!truncated);
    }
    #[test]
    fn appends_stop_and_truncation_tails() {
        let lines = ["a".to_string(), "b".to_string()];
        assert_eq!(render_batch_body(&lines, None, false), "a\nb");
        assert_eq!(
            render_batch_body(
                &lines,
                Some(BatchStop::ErrorReply { number: 2, rest: 3 }),
                true
            ),
            "a\nb\n[stopped: command 2 returned an error; 3 later command(s) were not sent]\n[truncated: a reply passed maxItems or the output budget; narrow it (SCAN MATCH/COUNT, LRANGE/ZRANGE ranges, HSCAN) before retrying]"
        );
        assert_eq!(
            render_batch_body(
                &lines,
                Some(BatchStop::ReadLimit { number: 1, rest: 1 }),
                true
            ),
            "a\nb\n[stopped: the reply to command 1 passed the 64 MiB read limit; the connection was closed and 1 later command(s) were not sent]\n[truncated: a reply passed maxItems or the output budget; narrow it (SCAN MATCH/COUNT, LRANGE/ZRANGE ranges, HSCAN) before retrying]"
        );
    }
}
