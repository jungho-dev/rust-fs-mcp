//! query.rs
//! tools::sqlite::query
//!
//! sqlite-query handler: runs any SQL script against a local SQLite file or :memory: on a connection opened per call.
//! Order per call: argument checks, open, watchdog for timeout and cancel, statements in order, commit or rollback, JSON rows.
//!

use super::params::{BindValues, bind_statement, read_bind_values};
use super::render::{render_column_keys, render_row};
use crate::core::cancel::current_cancel_flag;
use crate::core::config::resolve_path;
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::args::common::{bool_field, item_content};
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::{Batch, Connection, ErrorCode, OpenFlags, Statement};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_MAX_ROWS: usize = 1_000;
const MAX_ROWS_CAP: usize = 10_000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const TIMEOUT_CAP_MS: u64 = 600_000;

// 행 수·본문 상한 상태
struct RowCap {
    max_rows: usize,
    max_bytes: usize,
    row_count: usize,
    row_bytes: usize,
    truncated: bool,
}
// 행 반환 문장 1개
struct RowSection {
    statement_no: usize,
    rows: Vec<String>,
}
struct ScriptOutput {
    sections: Vec<RowSection>,
    statement_count: usize,
    column_count: usize,
    row_count: usize,
    truncated: bool,
}
enum ScriptFailure {
    Begin(rusqlite::Error),
    Rejected {
        message: String,
        in_transaction: bool,
    },
    TransactionEnded {
        statement_no: usize,
    },
    Sqlite {
        statement_no: usize,
        error: rusqlite::Error,
        in_transaction: bool,
    },
}

// 1. SQLite query tool --------------------------------------------------------
pub fn handle_sqlite_query(args: &Value) -> RawResult {
    // 1. 경로와 열기 옵션
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if path.is_empty() {
        return RawResult::error("path is required: a SQLite database file path or :memory:");
    }
    if path
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        return RawResult::error("URI filenames are not supported: pass a plain file path");
    }
    let read_only = bool_field(args, "readOnly", false);
    let create = bool_field(args, "create", false);
    let transaction = bool_field(args, "transaction", false);
    if create && read_only {
        return RawResult::error("create and readOnly cannot both be true");
    }

    // 2. 상한
    let max_rows = match args.get("maxRows") {
        None | Some(Value::Null) => DEFAULT_MAX_ROWS,
        Some(rows) => match rows.as_u64().filter(|rows| *rows > 0) {
            Some(rows) => usize::try_from(rows)
                .unwrap_or(usize::MAX)
                .min(MAX_ROWS_CAP),
            None => return RawResult::error("maxRows must be a positive integer"),
        },
    };
    let timeout_ms = match args.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(limit) => match limit.as_u64().filter(|limit| *limit > 0) {
            Some(limit) => limit.min(TIMEOUT_CAP_MS),
            None => return RawResult::error("timeout_ms must be a positive integer"),
        },
    };

    // 3. SQL과 바인딩 값
    let sql = match item_content(args, "sql", "sql_path") {
        Ok(sql) => sql,
        Err(error) => return RawResult::error(error),
    };
    if sql.trim().is_empty() {
        return RawResult::error("the SQL is empty");
    }
    let values = match read_bind_values(args) {
        Ok(values) => values,
        Err(error) => return RawResult::error(error),
    };

    // 4. 대상 파일 확인
    let is_memory = path == ":memory:";
    let resolved = if is_memory {
        Ok(PathBuf::from(path))
    } else {
        resolve_path(path)
    };
    let target = match resolved {
        Ok(target) => target,
        Err(error) => return RawResult::error(error),
    };
    let shown = target.display().to_string();
    if !is_memory && target.is_dir() {
        return RawResult::error(format!("Not a database file: {shown} is a directory"));
    }
    let missing = !is_memory && !target.exists();
    if missing && !create {
        return RawResult::error(format!(
            "Database file not found: {shown}; pass create:true to create it"
        ));
    }
    if let Some(parent) = target.parent().filter(|parent| missing && !parent.is_dir()) {
        return RawResult::error(format!(
            "Parent directory does not exist: {}",
            parent.display()
        ));
    }

    // 5. 연결
    let access = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let creation = if create {
        OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::empty()
    };
    let opened =
        Connection::open_with_flags(&target, access | creation | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .and_then(|conn| {
                conn.busy_timeout(Duration::from_millis(timeout_ms))
                    .map(|()| conn)
            });
    let conn = match opened {
        Ok(conn) => conn,
        Err(error) => return RawResult::error(format!("SQLite open failed for {shown}: {error}")),
    };

    // 6. 시간 제한·취소 감시
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);
    let cancel = current_cancel_flag();
    let watch_cancel = cancel.clone();
    let interrupt = conn.get_interrupt_handle();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let spawned = thread::Builder::new()
        .name("fs-mcp-sqlite-watch".to_string())
        .spawn(move || {
            while let Err(RecvTimeoutError::Timeout) =
                stop_rx.recv_timeout(Duration::from_millis(50))
            {
                let cancelled = watch_cancel
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Relaxed));
                if cancelled || Instant::now() >= deadline {
                    interrupt.interrupt();
                }
            }
        });
    let watchdog = match spawned {
        Ok(watchdog) => watchdog,
        Err(error) => {
            return RawResult::error(format!("SQLite watchdog thread failed to start: {error}"));
        }
    };

    // 7. 실행
    let before_changes = conn.total_changes();
    let begun = if !transaction {
        Ok(())
    } else if read_only {
        conn.execute_batch("BEGIN")
    } else {
        conn.execute_batch("BEGIN IMMEDIATE")
    };
    let outcome = match begun {
        Ok(()) => run_script(
            &conn,
            &sql,
            &values,
            max_rows,
            max_output_bytes() / 2,
            transaction,
            read_only,
        ),
        Err(error) => Err(ScriptFailure::Begin(error)),
    };
    drop(stop_tx);
    if watchdog.join().is_err() {
        let _ = writeln!(
            io::stderr().lock(),
            "rust-fs-mcp: sqlite-query watchdog thread panicked"
        );
    }

    // 8. 실패 시 롤백 (연결 종료도 롤백)
    let rollback_open = || {
        if conn.is_autocommit() {
            return;
        }
        if let Err(error) = conn.execute_batch("ROLLBACK") {
            let _ = writeln!(
                io::stderr().lock(),
                "rust-fs-mcp: sqlite-query ROLLBACK failed: {error}"
            );
        }
    };
    let busy_hint = |error: &rusqlite::Error| {
        let busy = matches!(
            error.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        );
        if busy {
            " — another connection holds the lock; retry, raise timeout_ms, or use readOnly:true"
        } else {
            ""
        }
    };
    let committed_scope = |in_transaction: bool| {
        if transaction {
            " (rolled back: nothing from this call was committed)"
        } else if in_transaction {
            " (the open transaction was rolled back; statements before it stay committed)"
        } else {
            " (statements before it stay committed)"
        }
    };
    let output = match outcome {
        Ok(output) => output,
        Err(ScriptFailure::Begin(error)) => {
            return RawResult::error(format!(
                "SQLite error at BEGIN: {error}{}",
                busy_hint(&error)
            ));
        }
        Err(ScriptFailure::Rejected {
            message,
            in_transaction,
        }) => {
            rollback_open();
            return RawResult::error(format!("{message}{}", committed_scope(in_transaction)));
        }
        Err(ScriptFailure::TransactionEnded { statement_no }) => {
            return RawResult::error(format!(
                "SQLite statement {statement_no} ended the transaction that transaction:true opened (COMMIT, END, or ROLLBACK in the script); later statements were not run, and what that statement committed or rolled back stands"
            ));
        }
        Err(ScriptFailure::Sqlite {
            statement_no,
            error,
            in_transaction,
        }) => {
            rollback_open();
            let scope = committed_scope(in_transaction);
            let interrupted = error.sqlite_error_code() == Some(ErrorCode::OperationInterrupted);
            let cancelled = cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed));
            let message = if interrupted && cancelled {
                format!("The call was cancelled; statement {statement_no} was interrupted{scope}")
            } else if interrupted {
                format!(
                    "The call ran past {timeout_ms} ms; statement {statement_no} was interrupted{scope}"
                )
            } else {
                format!(
                    "SQLite error in statement {statement_no}: {error}{scope}{}",
                    busy_hint(&error)
                )
            };
            return RawResult::error(message);
        }
    };

    // 9. 확정
    if output.statement_count == 0 {
        rollback_open();
        return RawResult::error("the SQL is empty");
    }
    let committed = if transaction {
        conn.execute_batch("COMMIT")
    } else {
        Ok(())
    };
    if let Err(error) = committed {
        rollback_open();
        return RawResult::error(format!(
            "SQLite error at COMMIT: {error} (rolled back: nothing from this call was committed){}",
            busy_hint(&error)
        ));
    }
    if !conn.is_autocommit() {
        rollback_open();
        return RawResult::error(
            "The SQL left a transaction open (BEGIN without COMMIT or ROLLBACK), so it was rolled back; finish the transaction in the same call",
        );
    }

    // 10. 결과 본문
    let changes = conn.total_changes() - before_changes;
    let query_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let multi = output.statement_count > 1;
    let sections: Vec<String> = output
        .sections
        .iter()
        .map(|section| {
            let array = if section.rows.is_empty() {
                "[]".to_string()
            } else {
                format!("[\n{}\n]", section.rows.join(",\n"))
            };
            if multi {
                format!("-- statement {}\n{array}", section.statement_no)
            } else {
                array
            }
        })
        .collect();
    let summary = if multi {
        Some(format!(
            "OK ({} statement(s), {changes} row(s) affected)",
            output.statement_count
        ))
    } else if sections.is_empty() {
        Some(format!("OK ({changes} row(s) affected)"))
    } else {
        None
    };
    let lines: Vec<String> = sections.into_iter().chain(summary).collect();
    let mut body = lines.join("\n");
    if output.truncated {
        body.push_str(
            "\n[truncated: more rows exist past the row or size cap; add LIMIT/OFFSET or a narrower WHERE]",
        );
    }
    RawResult::structured(
        body,
        json!({
            "statementCount": output.statement_count,
            "rowCount": output.row_count,
            "columnCount": output.column_count,
            "changes": changes,
            "lastInsertRowid": conn.last_insert_rowid(),
            "truncated": output.truncated,
            "readOnly": read_only,
            "queryMs": query_ms
        }),
    )
}
// 2. Statement run (내부) -----------------------------------------------------
// 스크립트 순차 실행, 첫 오류에서 중단
fn run_script(
    conn: &Connection,
    sql: &str,
    values: &BindValues,
    max_rows: usize,
    max_bytes: usize,
    transaction: bool,
    read_only: bool,
) -> Result<ScriptOutput, ScriptFailure> {
    // 1. 한 문장 행 수집 (첫 행 항상 유지)
    let collect_rows = |statement: &mut Statement<'_>,
                        keys: &[String],
                        cap: &mut RowCap|
     -> rusqlite::Result<Vec<String>> {
        let mut rows = statement.raw_query();
        let mut kept = Vec::new();
        while let Some(row) = rows.next()? {
            let line = render_row(keys, row)?;
            let fits = cap.row_count < cap.max_rows && cap.row_bytes + line.len() <= cap.max_bytes;
            if cap.row_count > 0 && (cap.truncated || !fits) {
                cap.truncated = true;
                break;
            }
            cap.row_count += 1;
            cap.row_bytes += line.len() + 2;
            kept.push(line);
        }
        Ok(kept)
    };

    // 2. VACUUM 문장 판별
    let starts_with_vacuum = |sql_text: &str| -> bool {
        let mut rest = sql_text;
        loop {
            rest = rest.trim_ascii_start();
            if let Some(line_tail) = rest.strip_prefix("--") {
                rest = line_tail.split_once('\n').map_or("", |(_, next)| next);
                continue;
            }
            let Some(block_tail) = rest.strip_prefix("/*") else {
                break;
            };
            rest = block_tail.split_once("*/").map_or("", |(_, next)| next);
        }
        let keyword_len = rest
            .find(|ch: char| !ch.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        rest[..keyword_len].eq_ignore_ascii_case("vacuum")
    };

    // 3. 문장 순차 실행
    let mut cap = RowCap {
        max_rows,
        max_bytes,
        row_count: 0,
        row_bytes: 0,
        truncated: false,
    };
    let mut sections = Vec::new();
    let mut statement_count = 0;
    let mut column_count = 0;
    let mut batch = Batch::new(conn, sql);
    loop {
        let statement_no = statement_count + 1;
        let in_transaction = !conn.is_autocommit();
        let failure = |error: rusqlite::Error| ScriptFailure::Sqlite {
            statement_no,
            error,
            in_transaction,
        };
        let Some(mut statement) = batch.next().map_err(failure)? else {
            break;
        };
        statement_count = statement_no;
        if read_only
            && statement
                .expanded_sql()
                .is_none_or(|text| starts_with_vacuum(&text))
        {
            return Err(ScriptFailure::Rejected {
                message: format!(
                    "SQLite statement {statement_no} runs VACUUM, which writes a file, but readOnly:true allows no file writes"
                ),
                in_transaction,
            });
        }
        bind_statement(&mut statement, values, statement_no).map_err(|message| {
            ScriptFailure::Rejected {
                message,
                in_transaction,
            }
        })?;
        let columns: Vec<String> = statement.column_names().map(str::to_string).collect();
        let keys = render_column_keys(&columns);
        let rows = collect_rows(&mut statement, &keys, &mut cap).map_err(failure)?;
        if transaction && conn.is_autocommit() {
            return Err(ScriptFailure::TransactionEnded { statement_no });
        }
        if !keys.is_empty() {
            column_count = keys.len();
            sections.push(RowSection { statement_no, rows });
        }
    }
    Ok(ScriptOutput {
        sections,
        statement_count,
        column_count,
        row_count: cap.row_count,
        truncated: cap.truncated,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rust-fs-mcp-sqlite-{name}-{}-{stamp}.db",
            std::process::id()
        ))
    }
    fn run_sql(args: Value) -> (RawResult, String) {
        let result = handle_sqlite_query(&args);
        let text = result.content[0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (result, text)
    }
    fn count_rows(path: &PathBuf) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap()
    }
    #[test]
    fn creates_file_and_runs_multi_statement_script() {
        let path = temp_db("create");
        let (result, text) = run_sql(json!({
            "path": path.display().to_string(),
            "create": true,
            "sql": "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO t(name) VALUES ('a'), ('b'); SELECT id, name FROM t ORDER BY id"
        }));
        assert!(!result.is_error, "{text}");
        assert_eq!(
            text,
            "-- statement 3\n[\n{\"id\":1,\"name\":\"a\"},\n{\"id\":2,\"name\":\"b\"}\n]\nOK (3 statement(s), 2 row(s) affected)"
        );
        let structured = result.structured.unwrap();
        assert_eq!(structured["statementCount"], 3);
        assert_eq!(structured["rowCount"], 2);
        assert_eq!(structured["columnCount"], 2);
        assert_eq!(structured["changes"], 2);
        assert_eq!(structured["lastInsertRowid"], 2);
        let (_, single) = run_sql(json!({
            "path": path.display().to_string(),
            "sql": "DELETE FROM t WHERE id = ?",
            "params": [1]
        }));
        assert_eq!(single, "OK (1 row(s) affected)");
        assert_eq!(count_rows(&path), 1);
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn rejects_bad_paths_and_options() {
        let path = temp_db("missing");
        let shown = path.display().to_string();
        let cases = [
            (
                json!({ "sql": "SELECT 1" }),
                "path is required: a SQLite database file path or :memory:".to_string(),
            ),
            (
                json!({ "path": "file:x.db", "sql": "SELECT 1" }),
                "URI filenames are not supported: pass a plain file path".to_string(),
            ),
            (
                json!({ "path": shown, "sql": "SELECT 1" }),
                format!("Database file not found: {shown}; pass create:true to create it"),
            ),
            (
                json!({ "path": shown, "create": true, "readOnly": true, "sql": "SELECT 1" }),
                "create and readOnly cannot both be true".to_string(),
            ),
            (
                json!({ "path": std::env::temp_dir().display().to_string(), "sql": "SELECT 1" }),
                format!(
                    "Not a database file: {} is a directory",
                    resolve_path(std::env::temp_dir()).unwrap().display()
                ),
            ),
            (
                json!({ "path": ":memory:", "sql": "  " }),
                "the SQL is empty".to_string(),
            ),
            (
                json!({ "path": ":memory:", "sql": "SELECT 1", "maxRows": 0 }),
                "maxRows must be a positive integer".to_string(),
            ),
            (
                json!({ "path": ":memory:", "sql": "SELECT ?" }),
                "statement 1: parameter 1 has no value in params (statements before it stay committed)"
                    .to_string(),
            ),
            (
                json!({ "path": ":memory:", "sql": "SELECT :id", "namedParams": {} }),
                "statement 1: parameter :id has no value in namedParams (statements before it stay committed)"
                    .to_string(),
            ),
        ];
        for (args, expected) in cases {
            let (result, text) = run_sql(args);
            assert!(result.is_error, "{text}");
            assert_eq!(text, format!("Error: {expected}"));
        }
        let nested = path.join("child.db");
        let (_, text) = run_sql(
            json!({ "path": nested.display().to_string(), "create": true, "sql": "SELECT 1" }),
        );
        assert_eq!(
            text,
            format!("Error: Parent directory does not exist: {shown}")
        );
        assert!(!path.exists());
    }
    #[test]
    fn binds_params_on_memory_database() {
        let (_, text) = run_sql(
            json!({ "path": ":memory:", "sql": "SELECT ? + ? AS total", "params": [2, 3] }),
        );
        assert_eq!(text, "[\n{\"total\":5}\n]");
        let (_, text) = run_sql(json!({
            "path": ":memory:",
            "sql": "SELECT :id AS id, @name AS name",
            "namedParams": { ":id": 7, "name": "kim" }
        }));
        assert_eq!(text, "[\n{\"id\":7,\"name\":\"kim\"}\n]");
        let (_, text) = run_sql(json!({ "path": ":memory:", "sql": "SELECT sqrt(16.0) AS root" }));
        assert_eq!(text, "[\n{\"root\":4.0}\n]");
    }
    #[test]
    fn read_only_connection_rejects_writes() {
        let path = temp_db("readonly");
        let shown = path.display().to_string();
        run_sql(json!({ "path": shown, "create": true, "sql": "CREATE TABLE t(a)" }));
        let (result, text) =
            run_sql(json!({ "path": shown, "readOnly": true, "sql": "INSERT INTO t VALUES (1)" }));
        assert!(result.is_error);
        assert!(
            text.starts_with("Error: SQLite error in statement 1: ")
                && text.contains("readonly database")
                && text.ends_with(" (statements before it stay committed)"),
            "{text}"
        );
        let (result, text) = run_sql(
            json!({ "path": shown, "readOnly": true, "sql": "SELECT count(*) AS n FROM t" }),
        );
        assert_eq!(result.structured.unwrap()["readOnly"], true);
        assert_eq!(text, "[\n{\"n\":0}\n]");
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn rolls_back_failed_transaction_and_open_transaction() {
        let path = temp_db("rollback");
        let shown = path.display().to_string();
        run_sql(json!({ "path": shown, "create": true, "sql": "CREATE TABLE t(a)" }));
        let (_, text) = run_sql(json!({
            "path": shown,
            "transaction": true,
            "sql": "INSERT INTO t VALUES (1); INSERT INTO missing VALUES (2)"
        }));
        assert!(
            text.starts_with("Error: SQLite error in statement 2: no such table: missing"),
            "{text}"
        );
        assert!(
            text.ends_with(" (rolled back: nothing from this call was committed)"),
            "{text}"
        );
        assert_eq!(count_rows(&path), 0);
        let (_, text) = run_sql(json!({ "path": shown, "sql": "BEGIN; INSERT INTO t VALUES (1)" }));
        assert_eq!(
            text,
            "Error: The SQL left a transaction open (BEGIN without COMMIT or ROLLBACK), so it was rolled back; finish the transaction in the same call"
        );
        assert_eq!(count_rows(&path), 0);
        let (_, text) = run_sql(
            json!({ "path": shown, "sql": "INSERT INTO t VALUES (1); BEGIN; INSERT INTO t VALUES (2); INSERT INTO missing VALUES (3)" }),
        );
        assert!(
            text.ends_with(
                " (the open transaction was rolled back; statements before it stay committed)"
            ),
            "{text}"
        );
        assert_eq!(count_rows(&path), 1);
        std::fs::remove_file(&path).unwrap();
    }
    #[test]
    fn interrupts_runaway_query_at_timeout() {
        let started = Instant::now();
        let (_, text) = run_sql(json!({
            "path": ":memory:",
            "timeout_ms": 200,
            "sql": "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c"
        }));
        assert_eq!(
            text,
            "Error: The call ran past 200 ms; statement 1 was interrupted (statements before it stay committed)"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn rejects_extension_loading_and_truncates_rows() {
        let (result, text) =
            run_sql(json!({ "path": ":memory:", "sql": "SELECT load_extension('missing')" }));
        assert!(result.is_error);
        assert!(text.contains("not authorized"), "{text}");
        let (result, text) = run_sql(json!({
            "path": ":memory:",
            "maxRows": 2,
            "sql": "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 5) SELECT x FROM c"
        }));
        assert_eq!(
            text,
            "[\n{\"x\":1},\n{\"x\":2}\n]\n[truncated: more rows exist past the row or size cap; add LIMIT/OFFSET or a narrower WHERE]"
        );
        let structured = result.structured.unwrap();
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["rowCount"], 2);
    }
}
