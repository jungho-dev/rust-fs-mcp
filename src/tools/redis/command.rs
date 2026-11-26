//! command.rs
//! tools::redis::command
//!
//! redis-command handler: argv batches against a target from the config.toml `[redis]` sections only.
//! Order per call: configuration check, target and argument parsing, the gate over every command,
//! limits capped by the target values, the serialized batch run, then one JSON line per command.
//!

use super::gate::check_command_batch;
use super::render::render_batch_body;
use super::session::{BatchRequest, run_batch};
use crate::core::redis::ReplyLimits;
use crate::core::response::{RawResult, max_output_bytes};
use crate::core::settings::{RedisTarget, settings};
use serde_json::{Value, json};
use std::time::Instant;

#[derive(Debug)]
struct RedisCall<'a> {
    target: &'a RedisTarget,
    commands: Vec<Vec<String>>,
    db: u32,
    scan_all: bool,
    max_items: usize,
    timeout_ms: u64,
}

// 1. Redis command tool -------------------------------------------------------
pub fn handle_redis_command(args: &Value) -> RawResult {
    // 1. 설정 확인
    let targets = &settings().redis;
    if targets.is_empty() {
        return RawResult::error(
            "redis-command is not configured: add a [redis] or [redis.<name>] section with host (plus port, db, and password_env or password_dpapi) to the rust-fs-mcp config.toml, then restart the server",
        );
    }

    // 2. 인자와 게이트
    let call = match parse_redis_call(args, targets) {
        Ok(call) => call,
        Err(error) => return RawResult::error(error),
    };
    if let Err(reason) = check_command_batch(&call.commands, call.timeout_ms) {
        return RawResult::error(reason);
    }

    // 3. 실행 (출력 예산 절반 공유)
    let started = Instant::now();
    let request = BatchRequest {
        commands: &call.commands,
        db: call.db,
        scan_all: call.scan_all,
        timeout_ms: call.timeout_ms,
        limits: ReplyLimits {
            max_items: call.max_items,
            max_bytes: max_output_bytes() / 2,
        },
    };
    let run = match run_batch(call.target, &request) {
        Ok(run) => run,
        Err(error) => return RawResult::error(error),
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // 4. 본문과 구조화 필드 (중간 실패 = 앞선 응답 동봉)
    let body = render_batch_body(&run.lines, run.stopped, run.truncated);
    if let Some(failure) = run.failure {
        let message = if body.is_empty() {
            failure
        } else {
            format!("{failure}\n{body}")
        };
        return RawResult::error(message);
    }
    RawResult::structured(
        body,
        json!({
            "target": call.target.name,
            "db": call.db,
            "commandCount": call.commands.len(),
            "executed": run.executed,
            "errorCount": run.error_count,
            "truncated": run.truncated,
            "elapsedMs": elapsed_ms
        }),
    )
}

// 2. (내부) Argument parsing --------------------------------------------------
fn parse_redis_call<'a>(args: &Value, targets: &'a [RedisTarget]) -> Result<RedisCall<'a>, String> {
    // 1. 대상 선택
    let names: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
    let names = names.join(", ");
    let target = match args.get("target").filter(|value| !value.is_null()) {
        None if targets.len() == 1 => &targets[0],
        None => {
            return Err(format!(
                "target is required: configured targets are {names}"
            ));
        }
        Some(requested) => {
            let requested = requested
                .as_str()
                .map_or_else(|| requested.to_string(), str::to_string);
            let found = targets.iter().find(|target| target.name == requested);
            found.ok_or_else(|| {
                format!(
                    "Unknown Redis target `{requested}`; configured targets: {names} (only config.toml targets are reachable)"
                )
            })?
        }
    };

    // 2. 명령 목록 (JSON 문자열, 평면 배열 허용)
    let shape_error =
        "commands must be a non-empty array of argv arrays such as [[\"GET\",\"key\"]]";
    let decoded: Option<Value> = match args.get("commands") {
        Some(Value::String(text)) => serde_json::from_str(text).ok(),
        other => other.cloned(),
    };
    let Some(Value::Array(entries)) = decoded else {
        return Err(shape_error.to_string());
    };
    if entries.is_empty() {
        return Err(shape_error.to_string());
    }
    let is_flat = entries
        .iter()
        .all(|entry| entry.is_string() || entry.is_number());
    let entries = if is_flat {
        vec![Value::Array(entries)]
    } else {
        entries
    };
    if entries.len() > 100 {
        return Err(format!(
            "commands holds {} entries; the limit is 100 per call",
            entries.len()
        ));
    }
    let mut commands = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let words: Vec<String> = entry
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|word| match word {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .collect();
        let complete = entry
            .as_array()
            .is_some_and(|argv| !argv.is_empty() && argv.len() == words.len());
        if !complete {
            return Err(format!(
                "command {} must be a non-empty array of strings or numbers",
                index + 1
            ));
        }
        commands.push(words);
    }

    // 3. 선택 인자 (설정값 이내)
    let db = match args.get("db").filter(|value| !value.is_null()) {
        None => target.db,
        Some(db) => db
            .as_u64()
            .and_then(|db| u32::try_from(db).ok())
            .ok_or("db must be a non-negative integer")?,
    };
    let scan_all = match args.get("scanAll").filter(|value| !value.is_null()) {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("scanAll must be a boolean".to_string()),
    };
    let max_items = match args.get("maxItems").filter(|value| !value.is_null()) {
        None => target.max_items,
        Some(limit) => {
            let limit = limit
                .as_u64()
                .filter(|limit| *limit > 0)
                .ok_or("maxItems must be a positive integer")?;
            usize::try_from(limit)
                .unwrap_or(usize::MAX)
                .min(target.max_items)
        }
    };
    let timeout_ms = match args.get("timeout_ms").filter(|value| !value.is_null()) {
        None => target.command_timeout_ms,
        Some(limit) => limit
            .as_u64()
            .filter(|limit| *limit > 0)
            .ok_or("timeout_ms must be a positive integer")?
            .min(target.command_timeout_ms),
    };
    Ok(RedisCall {
        target,
        commands,
        db,
        scan_all,
        max_items,
        timeout_ms,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::PasswordSource;

    fn build_target(name: &str) -> RedisTarget {
        RedisTarget {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port: 6379,
            user: None,
            password: PasswordSource::Unset,
            db: 3,
            connect_timeout_ms: 10_000,
            command_timeout_ms: 10_000,
            max_items: 1_000,
        }
    }
    fn parse_error(args: Value, targets: &[RedisTarget]) -> String {
        parse_redis_call(&args, targets).unwrap_err()
    }
    #[test]
    fn reports_how_to_enable_without_config() {
        let result = handle_redis_command(&json!({ "commands": [["PING"]] }));
        assert!(result.is_error);
        let text = result.content[0]["text"].as_str().unwrap();
        assert!(text.contains("redis-command is not configured"), "{text}");
        assert!(text.contains("[redis"), "{text}");
    }
    #[test]
    fn selects_the_target_or_lists_configured_names() {
        let pair = [build_target("a"), build_target("b")];
        assert_eq!(
            parse_error(json!({ "commands": [["PING"]] }), &pair),
            "target is required: configured targets are a, b"
        );
        assert_eq!(
            parse_error(json!({ "target": "x", "commands": [["PING"]] }), &pair),
            "Unknown Redis target `x`; configured targets: a, b (only config.toml targets are reachable)"
        );
        let call = parse_redis_call(&json!({ "target": "b", "commands": [["PING"]] }), &pair);
        assert_eq!(call.unwrap().target.name, "b");
        let single = [build_target("local")];
        let call = parse_redis_call(&json!({ "commands": [["PING"]] }), &single).unwrap();
        assert_eq!(call.target.name, "local");
        assert_eq!(call.db, 3);
    }
    #[test]
    fn accepts_argv_arrays_flat_arrays_numbers_and_json_text() {
        let targets = [build_target("local")];
        let shapes = [
            (
                json!([["SET", "k", 5], ["GET", "k"]]),
                vec![vec!["SET", "k", "5"], vec!["GET", "k"]],
            ),
            (
                json!(["EXPIRE", "k", 1.5]),
                vec![vec!["EXPIRE", "k", "1.5"]],
            ),
            (json!("[[\"PING\"]]"), vec![vec!["PING"]]),
        ];
        for (commands, expected) in shapes {
            let call = parse_redis_call(&json!({ "commands": commands }), &targets).unwrap();
            assert_eq!(call.commands, expected);
        }
    }
    #[test]
    fn rejects_bad_command_shapes() {
        let targets = [build_target("local")];
        let shape_error =
            "commands must be a non-empty array of argv arrays such as [[\"GET\",\"key\"]]";
        assert_eq!(parse_error(json!({}), &targets), shape_error);
        assert_eq!(
            parse_error(json!({ "commands": [] }), &targets),
            shape_error
        );
        assert_eq!(
            parse_error(json!({ "commands": "GET k" }), &targets),
            shape_error
        );
        assert_eq!(
            parse_error(json!({ "commands": [["GET", "k"], []] }), &targets),
            "command 2 must be a non-empty array of strings or numbers"
        );
        assert_eq!(
            parse_error(json!({ "commands": [["GET", { "k": 1 }]] }), &targets),
            "command 1 must be a non-empty array of strings or numbers"
        );
        let too_many: Vec<Value> = (0..101).map(|_| json!(["PING"])).collect();
        assert_eq!(
            parse_error(json!({ "commands": too_many }), &targets),
            "commands holds 101 entries; the limit is 100 per call"
        );
    }
    #[test]
    fn caps_limits_by_the_target_values() {
        let targets = [build_target("local")];
        let args = json!({ "commands": [["PING"]], "db": 0, "scanAll": true, "maxItems": 5_000, "timeout_ms": 99_999 });
        let call = parse_redis_call(&args, &targets).unwrap();
        assert_eq!(
            (call.db, call.scan_all, call.max_items, call.timeout_ms),
            (0, true, 1_000, 10_000)
        );
        let args = json!({ "commands": [["PING"]], "maxItems": 7, "timeout_ms": 300 });
        let call = parse_redis_call(&args, &targets).unwrap();
        assert_eq!((call.max_items, call.timeout_ms), (7, 300));
        let errors = [
            ("maxItems", json!(0), "maxItems must be a positive integer"),
            (
                "timeout_ms",
                json!("x"),
                "timeout_ms must be a positive integer",
            ),
            ("db", json!(-1), "db must be a non-negative integer"),
        ];
        for (key, value, message) in errors {
            let mut args = json!({ "commands": [["PING"]] });
            args[key] = value;
            assert_eq!(parse_error(args, &targets), message);
        }
    }
}
