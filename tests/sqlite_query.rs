use rusqlite::Connection;
use rust_fs_mcp::tools;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BUSY_HINT: &str =
    " — another connection holds the lock; retry, raise timeout_ms, or use readOnly:true";
const ROLLED_BACK: &str = " (rolled back: nothing from this call was committed)";
const READ_ONLY_WRITE: &str = "Error: SQLite error in statement 1: ";
const READ_ONLY_VACUUM: &str = "Error: SQLite statement 1 runs VACUUM, which writes a file, but readOnly:true allows no file writes (statements before it stay committed)";
const TRUNCATED_NOTE: &str =
    "[truncated: more rows exist past the row or size cap; add LIMIT/OFFSET or a narrower WHERE]";

// 1. Test fixtures ------------------------------------------------------------
struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rust-fs-mcp-sqlite-it-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn db(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!("cleanup failed for {}: {error}", self.root.display());
        }
    }
}

struct Reply {
    is_error: bool,
    text: String,
    structured: Value,
}

fn call(name: &str, args: Value) -> Reply {
    let result = tools::dispatch_tool_call(name, Some(args));
    let data = &result["structuredContent"]["data"];
    Reply {
        is_error: result["isError"] == json!(true),
        text: data["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        structured: data["structuredContent"].clone(),
    }
}

fn sqlite(args: Value) -> Reply {
    call("sqlite-query", args)
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

fn sha256_of(path: &Path) -> String {
    let reply = call(
        "path-stat",
        json!({ "paths": [shown(path)], "hash": "sha256" }),
    );
    let Some((_, tail)) = reply.text.split_once("sha256=") else {
        panic!("no sha256 in path-stat output: {}", reply.text);
    };
    tail.chars().take(64).collect()
}

fn query_i64(path: &Path, sql: &str) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn create_table(path: &Path, sql: &str) {
    let reply = sqlite(json!({ "path": shown(path), "create": true, "sql": sql }));
    assert!(!reply.is_error, "{}", reply.text);
}

// 2. DDL ----------------------------------------------------------------------
#[test]
fn runs_create_alter_and_drop_ddl() {
    let dir = TestDir::new("ddl");
    let db = dir.db("ddl.db");
    let created = sqlite(json!({
        "path": shown(&db),
        "create": true,
        "sql": "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT NOT NULL);\n\
                CREATE INDEX p_name ON p(name);\n\
                CREATE TABLE log(msg TEXT);\n\
                CREATE TRIGGER p_ins AFTER INSERT ON p BEGIN INSERT INTO log VALUES ('ins ' || new.name); END;\n\
                CREATE VIEW p_view AS SELECT upper(name) AS up FROM p;\n\
                ALTER TABLE p ADD COLUMN age INTEGER;\n\
                INSERT INTO p(name, age) VALUES ('a', 3);\n\
                SELECT up FROM p_view"
    }));
    assert!(!created.is_error, "{}", created.text);
    assert_eq!(
        created.text,
        "-- statement 8\n[\n{\"up\":\"A\"}\n]\nOK (8 statement(s), 2 row(s) affected)"
    );
    assert_eq!(created.structured["statementCount"], 8);
    assert_eq!(created.structured["changes"], 2);
    assert_eq!(
        query_i64(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('p', 'p_name', 'log', 'p_ins', 'p_view')"
        ),
        5
    );
    assert_eq!(
        query_i64(&db, "SELECT count(*) FROM log WHERE msg = 'ins a'"),
        1
    );
    assert_eq!(
        query_i64(
            &db,
            "SELECT count(*) FROM pragma_table_info('p') WHERE name = 'age'"
        ),
        1
    );

    let dropped = sqlite(json!({
        "path": shown(&db),
        "sql": "DROP VIEW p_view; DROP TRIGGER p_ins; DROP INDEX p_name; ALTER TABLE p RENAME TO person; DROP TABLE log"
    }));
    assert!(!dropped.is_error, "{}", dropped.text);
    assert_eq!(dropped.text, "OK (5 statement(s), 0 row(s) affected)");
    assert_eq!(
        query_i64(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'"
        ),
        1
    );
    assert_eq!(
        query_i64(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name = 'person'"
        ),
        1
    );
}

// 3. Insert and bind ----------------------------------------------------------
#[test]
fn inserts_with_params_named_params_returning_and_upsert() {
    let dir = TestDir::new("insert");
    let db = dir.db("insert.db");
    create_table(
        &db,
        "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE kv(k TEXT PRIMARY KEY, v INTEGER)",
    );

    // 위치 인자
    let positional = sqlite(json!({
        "path": shown(&db),
        "sql": "INSERT INTO t(name) VALUES (?), (?)",
        "params": ["a", "b"]
    }));
    assert_eq!(positional.text, "OK (2 row(s) affected)");
    assert_eq!(positional.structured["lastInsertRowid"], 2);

    // 이름 인자 (접두어 유무)
    let named = sqlite(json!({
        "path": shown(&db),
        "sql": "INSERT INTO t(id, name) VALUES (:id, @name)",
        "namedParams": { ":id": 10, "name": "c" }
    }));
    assert_eq!(named.text, "OK (1 row(s) affected)");

    // RETURNING
    let returning = sqlite(json!({
        "path": shown(&db),
        "sql": "INSERT INTO t(name) VALUES ($name) RETURNING id, name",
        "namedParams": { "$name": "d" }
    }));
    assert_eq!(returning.text, "[\n{\"id\":11,\"name\":\"d\"}\n]");
    assert_eq!(returning.structured["changes"], 1);
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 4);

    // UPSERT
    let upsert_sql =
        "INSERT INTO kv(k, v) VALUES (:k, :v) ON CONFLICT(k) DO UPDATE SET v = v + excluded.v";
    for _ in 0..2 {
        let reply = sqlite(json!({
            "path": shown(&db),
            "sql": upsert_sql,
            "namedParams": { "k": "a", "v": 5 }
        }));
        assert_eq!(reply.text, "OK (1 row(s) affected)");
    }
    assert_eq!(query_i64(&db, "SELECT v FROM kv WHERE k = 'a'"), 10);
    assert_eq!(query_i64(&db, "SELECT count(*) FROM kv"), 1);
}

// 4. Value types --------------------------------------------------------------
#[test]
fn renders_value_types_and_duplicate_columns() {
    let dir = TestDir::new("types");
    let db = dir.db("types.db");
    create_table(&db, "CREATE TABLE v(a)");

    // 렌더링
    let rendered = sqlite(json!({
        "path": shown(&db),
        "sql": "SELECT NULL AS n, 42 AS i, 0.5 AS r, 'text \"q\"' AS s, x'00FF10' AS b, 1e999 AS hi, -1e999 AS lo, CAST(x'FF' AS TEXT) AS bad, 1 AS d, 2 AS d"
    }));
    assert_eq!(
        rendered.text,
        "[\n{\"n\":null,\"i\":42,\"r\":0.5,\"s\":\"text \\\"q\\\"\",\"b\":\"0x00FF10\",\"hi\":\"Infinity\",\"lo\":\"-Infinity\",\"bad\":\"0xFF\",\"d\":1,\"d_2\":2}\n]"
    );
    assert_eq!(rendered.structured["columnCount"], 10);

    // JSON 값 저장 유형
    let stored = sqlite(json!({
        "path": shown(&db),
        "sql": "INSERT INTO v(a) VALUES (?), (?), (?), (?), (?)",
        "params": [null, true, 7, 1.5, "x"]
    }));
    assert_eq!(stored.text, "OK (5 row(s) affected)");
    let typed = sqlite(json!({
        "path": shown(&db),
        "sql": "SELECT typeof(a) AS t, a FROM v ORDER BY rowid"
    }));
    assert_eq!(
        typed.text,
        "[\n{\"t\":\"null\",\"a\":null},\n{\"t\":\"integer\",\"a\":1},\n{\"t\":\"integer\",\"a\":7},\n{\"t\":\"real\",\"a\":1.5},\n{\"t\":\"text\",\"a\":\"x\"}\n]"
    );

    // 거부 값
    let nested = sqlite(json!({ "path": shown(&db), "sql": "SELECT ?", "params": [[1]] }));
    assert!(nested.is_error);
    assert_eq!(
        nested.text,
        "Error: params[0]: arrays and objects are not SQL values; pass JSON text as a string or a BLOB as unhex('...')"
    );
    let huge = sqlite(json!({
        "path": shown(&db),
        "sql": "SELECT ?",
        "params": [18_446_744_073_709_551_615u64]
    }));
    assert_eq!(
        huge.text,
        "Error: params[0]: 18446744073709551615 does not fit a 64-bit signed integer"
    );
    let both = sqlite(json!({
        "path": shown(&db),
        "sql": "SELECT 1",
        "params": [1],
        "namedParams": { "a": 1 }
    }));
    assert_eq!(both.text, "Error: pass params or namedParams, not both");
}

// 5. Update and delete --------------------------------------------------------
#[test]
fn update_and_delete_are_visible_from_another_connection() {
    let dir = TestDir::new("update");
    let db = dir.db("update.db");
    create_table(
        &db,
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER); INSERT INTO t(id, v) VALUES (1, 1), (2, 2), (3, 3)",
    );

    let updated = sqlite(json!({
        "path": shown(&db),
        "sql": "UPDATE t SET v = v * 10 WHERE id >= ?",
        "params": [2]
    }));
    assert_eq!(updated.text, "OK (2 row(s) affected)");
    assert_eq!(updated.structured["changes"], 2);
    assert_eq!(query_i64(&db, "SELECT sum(v) FROM t"), 51);

    let deleted = sqlite(json!({
        "path": shown(&db),
        "sql": "DELETE FROM t WHERE id = :id",
        "namedParams": { "id": 1 }
    }));
    assert_eq!(deleted.text, "OK (1 row(s) affected)");
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 2);
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t WHERE id = 1"), 0);
}

// 6. Transactions -------------------------------------------------------------
#[test]
fn transaction_with_failing_third_statement_changes_nothing() {
    let dir = TestDir::new("txn");
    let db = dir.db("txn.db");
    create_table(&db, "CREATE TABLE t(a UNIQUE)");

    let reply = sqlite(json!({
        "path": shown(&db),
        "transaction": true,
        "sql": "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (1)"
    }));
    assert!(reply.is_error);
    assert!(
        reply
            .text
            .starts_with("Error: SQLite error in statement 3: UNIQUE constraint failed: t.a"),
        "{}",
        reply.text
    );
    assert!(reply.text.ends_with(ROLLED_BACK), "{}", reply.text);
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 0);
}

#[test]
fn begin_without_commit_is_rolled_back() {
    let dir = TestDir::new("open-txn");
    let db = dir.db("open.db");
    create_table(&db, "CREATE TABLE t(a)");

    let reply = sqlite(json!({
        "path": shown(&db),
        "sql": "BEGIN; INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)"
    }));
    assert!(reply.is_error);
    assert_eq!(
        reply.text,
        "Error: The SQL left a transaction open (BEGIN without COMMIT or ROLLBACK), so it was rolled back; finish the transaction in the same call"
    );
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 0);
}

#[test]
fn commit_inside_transaction_script_stops_and_keeps_first_row() {
    let dir = TestDir::new("inner-commit");
    let db = dir.db("inner.db");
    create_table(&db, "CREATE TABLE t(a)");

    let reply = sqlite(json!({
        "path": shown(&db),
        "transaction": true,
        "sql": "INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2)"
    }));
    assert!(reply.is_error);
    assert_eq!(
        reply.text,
        "Error: SQLite statement 2 ended the transaction that transaction:true opened (COMMIT, END, or ROLLBACK in the script); later statements were not run, and what that statement committed or rolled back stands"
    );
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 1);
}

#[test]
fn bind_error_after_committed_statement_reports_committed_scope() {
    let dir = TestDir::new("bind-scope");
    let db = dir.db("bind.db");
    create_table(&db, "CREATE TABLE t(a)");

    let reply = sqlite(json!({
        "path": shown(&db),
        "sql": "INSERT INTO t VALUES (1); INSERT INTO t VALUES (?)"
    }));
    assert!(reply.is_error);
    assert_eq!(
        reply.text,
        "Error: statement 2: parameter 1 has no value in params (statements before it stay committed)"
    );
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 1);
}

// 7. Timeout and locking ------------------------------------------------------
#[test]
fn endless_recursive_cte_is_interrupted_at_timeout() {
    let dir = TestDir::new("timeout");
    let db = dir.db("timeout.db");
    create_table(&db, "CREATE TABLE t(a)");

    let started = Instant::now();
    let reply = sqlite(json!({
        "path": shown(&db),
        "timeout_ms": 300,
        "sql": "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c"
    }));
    let elapsed = started.elapsed();
    assert!(reply.is_error);
    assert_eq!(
        reply.text,
        "Error: The call ran past 300 ms; statement 1 was interrupted (statements before it stay committed)"
    );
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    eprintln!("timeout case elapsed: {elapsed:?}");
}

#[test]
fn write_under_another_connections_lock_reports_busy_hint() {
    let dir = TestDir::new("busy");
    let db = dir.db("busy.db");
    create_table(&db, "CREATE TABLE t(a)");
    let holder = Connection::open(&db).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = Instant::now();
    let reply = sqlite(json!({
        "path": shown(&db),
        "timeout_ms": 300,
        "sql": "INSERT INTO t VALUES (1)"
    }));
    let elapsed = started.elapsed();
    holder.execute_batch("ROLLBACK").unwrap();
    eprintln!("busy case elapsed: {elapsed:?}; text: {}", reply.text);
    assert!(reply.is_error);
    assert!(
        reply
            .text
            .starts_with("Error: SQLite error in statement 1: "),
        "{}",
        reply.text
    );
    assert!(reply.text.ends_with(BUSY_HINT), "{}", reply.text);
    assert_eq!(query_i64(&db, "SELECT count(*) FROM t"), 0);
}

// 8. Open modes ---------------------------------------------------------------
#[test]
fn read_only_write_is_rejected_and_file_is_unchanged() {
    let dir = TestDir::new("readonly");
    let db = dir.db("ro.db");
    create_table(&db, "CREATE TABLE t(a); INSERT INTO t VALUES (1)");
    let before = sha256_of(&db);
    assert_eq!(before.len(), 64, "{before}");

    let reply = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": "INSERT INTO t VALUES (2)"
    }));
    assert!(reply.is_error);
    assert!(
        reply.text.starts_with(READ_ONLY_WRITE) && reply.text.contains("readonly database"),
        "{}",
        reply.text
    );
    assert_eq!(sha256_of(&db), before);

    let read = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": "SELECT count(*) AS n FROM t"
    }));
    assert_eq!(read.text, "[\n{\"n\":1}\n]");
    assert_eq!(read.structured["readOnly"], true);
    assert_eq!(sha256_of(&db), before);
}

#[test]
fn read_only_vacuum_into_is_rejected_and_creates_no_file() {
    let dir = TestDir::new("vacuum-into");
    let db = dir.db("ro.db");
    let copy = dir.db("copy.db");
    create_table(&db, "CREATE TABLE t(a); INSERT INTO t VALUES (1)");

    let reply = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": format!("VACUUM INTO '{}'", shown(&copy))
    }));
    assert!(reply.is_error);
    assert_eq!(reply.text, READ_ONLY_VACUUM);
    assert!(!copy.exists());

    let before = sha256_of(&db);
    let reply = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": "/* c */ vacuum"
    }));
    assert!(reply.is_error);
    assert_eq!(reply.text, READ_ONLY_VACUUM);
    assert_eq!(sha256_of(&db), before);
    assert!(!copy.exists());
}

#[test]
fn read_only_allows_statements_that_write_no_file() {
    let dir = TestDir::new("readonly-allowed");
    let db = dir.db("ro.db");
    create_table(&db, "CREATE TABLE x(a); INSERT INTO x VALUES (1)");
    let before = sha256_of(&db);

    let journal = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": "PRAGMA journal_mode"
    }));
    assert!(!journal.is_error, "{}", journal.text);
    assert_eq!(journal.structured["rowCount"], 1);

    let temp = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": "CREATE TEMP TABLE t(a); INSERT INTO t VALUES (1); SELECT a FROM t"
    }));
    assert!(!temp.is_error, "{}", temp.text);
    assert_eq!(temp.structured["rowCount"], 1);

    let explain = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": "EXPLAIN INSERT INTO x VALUES (1)"
    }));
    assert!(!explain.is_error, "{}", explain.text);
    assert_eq!(query_i64(&db, "SELECT count(*) FROM x"), 1);
    assert_eq!(sha256_of(&db), before);
}

#[test]
fn missing_file_without_create_leaves_no_file() {
    let dir = TestDir::new("missing");
    let db = dir.db("absent.db");

    let reply = sqlite(json!({ "path": shown(&db), "sql": "SELECT 1" }));
    assert!(reply.is_error);
    assert!(
        reply.text.starts_with("Error: Database file not found: "),
        "{}",
        reply.text
    );
    assert!(
        reply.text.ends_with("; pass create:true to create it"),
        "{}",
        reply.text
    );
    assert!(!db.exists());
}

#[test]
fn max_rows_two_reports_truncated() {
    let dir = TestDir::new("maxrows");
    let db = dir.db("rows.db");
    create_table(
        &db,
        "CREATE TABLE t(x); INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    );

    let reply = sqlite(json!({
        "path": shown(&db),
        "maxRows": 2,
        "sql": "SELECT x FROM t ORDER BY x"
    }));
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(
        reply.text,
        format!("[\n{{\"x\":1}},\n{{\"x\":2}}\n]\n{TRUNCATED_NOTE}")
    );
    assert_eq!(reply.structured["truncated"], true);
    assert_eq!(reply.structured["rowCount"], 2);
}

// 9. Engine features ----------------------------------------------------------
#[test]
fn runs_attach_vacuum_wal_fts5_json_and_window_functions() {
    let dir = TestDir::new("features");
    let db = dir.db("main.db");
    let aux = dir.db("aux.db");
    create_table(&db, "CREATE TABLE t(x)");
    Connection::open(&aux)
        .unwrap()
        .execute_batch("CREATE TABLE a(v); INSERT INTO a VALUES (7)")
        .unwrap();

    // ATTACH
    let attached = sqlite(json!({
        "path": shown(&db),
        "sql": format!("ATTACH '{}' AS aux; INSERT INTO t SELECT v FROM aux.a; SELECT x FROM t; DETACH aux", shown(&aux))
    }));
    assert!(!attached.is_error, "{}", attached.text);
    assert_eq!(
        attached.text,
        "-- statement 3\n[\n{\"x\":7}\n]\nOK (4 statement(s), 1 row(s) affected)"
    );

    // VACUUM
    let vacuum = sqlite(json!({ "path": shown(&db), "sql": "VACUUM" }));
    assert_eq!(vacuum.text, "OK (0 row(s) affected)");

    // WAL
    let wal = sqlite(json!({ "path": shown(&db), "sql": "PRAGMA journal_mode=WAL" }));
    assert_eq!(wal.text, "[\n{\"journal_mode\":\"wal\"}\n]");
    let mode: String = Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");

    // FTS5
    let fts = sqlite(json!({
        "path": shown(&db),
        "sql": "CREATE VIRTUAL TABLE doc USING fts5(body); INSERT INTO doc(body) VALUES ('alpha beta'), ('gamma delta'); SELECT body FROM doc WHERE doc MATCH 'gamma'"
    }));
    assert!(!fts.is_error, "{}", fts.text);
    let changes = fts.structured["changes"].as_i64().unwrap_or_default();
    assert!(changes >= 2, "{changes}");
    assert_eq!(
        fts.text,
        format!(
            "-- statement 3\n[\n{{\"body\":\"gamma delta\"}}\n]\nOK (3 statement(s), {changes} row(s) affected)"
        )
    );
    assert_eq!(query_i64(&db, "SELECT count(*) FROM doc"), 2);

    // JSON 함수
    let json_fn = sqlite(json!({
        "path": shown(&db),
        "sql": "SELECT json_extract(?, '$.a[1]') AS v, json_array(1, 'b') AS arr",
        "params": ["{\"a\":[1,2]}"]
    }));
    assert_eq!(json_fn.text, "[\n{\"v\":2,\"arr\":\"[1,\\\"b\\\"]\"}\n]");

    // 윈도 함수
    let window = sqlite(json!({
        "path": shown(&db),
        "sql": "WITH s(x) AS (VALUES (1), (2), (3)) SELECT x, sum(x) OVER (ORDER BY x) AS running FROM s ORDER BY x"
    }));
    assert_eq!(
        window.text,
        "[\n{\"x\":1,\"running\":1},\n{\"x\":2,\"running\":3},\n{\"x\":3,\"running\":6}\n]"
    );
}

#[test]
fn read_only_connection_rejects_attach_uri_with_mode_rwc() {
    let dir = TestDir::new("attach-uri");
    let db = dir.db("main.db");
    let target = dir.db("created.db");
    create_table(&db, "CREATE TABLE t(x)");
    let uri_path = shown(&target).replace('\\', "/");

    let reply = sqlite(json!({
        "path": shown(&db),
        "readOnly": true,
        "sql": format!("ATTACH 'file:{uri_path}?mode=rwc' AS x")
    }));
    eprintln!("attach uri text: {}", reply.text);
    assert!(reply.is_error, "{}", reply.text);
    assert!(!target.exists());
}
