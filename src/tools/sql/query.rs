//! query.rs
//! tools::sql::query
//!
//! mysql-query handler: one read-only statement per call against the server in the config.toml `[mysql]` section.
//! Order per call: configuration check, read-only gate, limits capped by the configured values, serialized run, JSON rows.
//!

use super::gate::check_read_only;
use super::render::render_result_rows;
use super::session::run_read_only;
use crate::core::response::{RawResult, max_output_bytes};
use crate::core::settings::settings;
use crate::core::sql::{RowLimits, StatementOutcome};
use crate::tools::args::common::item_content;
use serde_json::{Value, json};

// 1. MySQL query tool ---------------------------------------------------------
pub fn handle_mysql_query(args: &Value) -> RawResult {
    // 1. 설정 확인
    let Some(config) = settings().mysql.as_ref() else {
        return RawResult::error(
            "mysql-query is not configured: add a [mysql] section with host and user (plus database and password_env or password_dpapi) to the rust-fs-mcp config.toml, then restart the server",
        );
    };

    // 2. 문장과 읽기 전용 게이트
    let sql = match item_content(args, "sql", "sql_path") {
        Ok(sql) => sql,
        Err(error) => return RawResult::error(error),
    };
    if let Err(reason) = check_read_only(&sql) {
        return RawResult::error(format!("Rejected by the read-only gate: {reason}"));
    }

    // 3. 상한 (설정값 이내)
    let max_rows = match args.get("maxRows") {
        None | Some(Value::Null) => config.max_rows,
        Some(rows) => match rows.as_u64().filter(|rows| *rows > 0) {
            Some(rows) => usize::try_from(rows)
                .unwrap_or(usize::MAX)
                .min(config.max_rows),
            None => return RawResult::error("maxRows must be a positive integer"),
        },
    };
    let timeout_ms = match args.get("timeout_ms") {
        None | Some(Value::Null) => config.query_timeout_ms,
        Some(limit) => match limit.as_u64().filter(|limit| *limit > 0) {
            Some(limit) => limit.min(config.query_timeout_ms),
            None => return RawResult::error("timeout_ms must be a positive integer"),
        },
    };
    // 열 이름 반복분 여유
    let limits = RowLimits {
        max_rows,
        max_bytes: max_output_bytes() / 2,
    };

    // 4. 실행과 결과
    let run = match run_read_only(config, sql.trim(), &limits, timeout_ms) {
        Ok(run) => run,
        Err(error) => return RawResult::error(error),
    };
    let query_ms = u64::try_from(run.elapsed.as_millis()).unwrap_or(u64::MAX);
    match run.outcome {
        StatementOutcome::Rows(result) => {
            let mut body = render_result_rows(&result);
            if result.truncated {
                body.push_str(
                    "\n[truncated: more rows exist past the row or size cap; add LIMIT/OFFSET or a narrower WHERE]",
                );
            }
            RawResult::structured(
                body,
                json!({
                    "rowCount": result.rows.len(),
                    "columnCount": result.columns.len(),
                    "truncated": result.truncated,
                    "queryMs": query_ms
                }),
            )
        }
        StatementOutcome::Done(summary) => RawResult::structured(
            format!("OK ({} row(s) affected)", summary.affected_rows),
            json!({
                "affectedRows": summary.affected_rows,
                "warnings": summary.warnings,
                "queryMs": query_ms
            }),
        ),
    }
}
