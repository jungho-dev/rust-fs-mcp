//! gate.rs
//! tools::sql::gate
//!
//! Read-only gate for mysql-query: accepts exactly one SELECT, SHOW, DESCRIBE, EXPLAIN, or WITH ... SELECT statement.
//! Rejects multiple statements, executable comments, timeout-lifting hints, writes behind WITH, locking reads,
//! blocking or server-file functions, EXPLAIN ANALYZE, and INTO OUTFILE / DUMPFILE.
//! The gate is one layer only: the handler also runs every statement inside a READ ONLY transaction
//! on a connection that never negotiates multi-statement support.
//!

// 허용 선두 키워드
const READ_STATEMENTS: [&str; 6] = ["SELECT", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "WITH"];
// 대기·잠금·서버 파일 함수
const BLOCKED_FUNCTIONS: [&str; 10] = [
    "SLEEP",
    "BENCHMARK",
    "GET_LOCK",
    "RELEASE_LOCK",
    "RELEASE_ALL_LOCKS",
    "LOAD_FILE",
    "MASTER_POS_WAIT",
    "SOURCE_POS_WAIT",
    "WAIT_FOR_EXECUTED_GTID_SET",
    "WAIT_UNTIL_SQL_THREAD_AFTER_GTIDS",
];
const WRITE_KEYWORDS: [&str; 4] = ["INSERT", "UPDATE", "DELETE", "REPLACE"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SqlToken<'a> {
    Word(&'a str),
    // 문자열·백틱 식별자 (따옴표 안쪽)
    Quoted(&'a str),
    Symbol(u8),
}
// 1. Read-only gate -----------------------------------------------------------
pub(super) fn check_read_only(sql: &str) -> Result<(), String> {
    // 1. 단일 문장
    let tokens = scan_sql_tokens(sql)?;
    let body = match tokens
        .iter()
        .position(|token| *token == SqlToken::Symbol(b';'))
    {
        Some(end) if end + 1 < tokens.len() => {
            return Err("only one statement per call is allowed".to_string());
        }
        Some(end) => &tokens[..end],
        None => &tokens[..],
    };

    // 2. 선두 키워드
    let lead = body
        .iter()
        .find(|token| **token != SqlToken::Symbol(b'('))
        .and_then(|token| match token {
            SqlToken::Word(word) => Some(word.to_ascii_uppercase()),
            _ => None,
        });
    let Some(lead) = lead else {
        return Err("the statement is empty".to_string());
    };
    if !READ_STATEMENTS.contains(&lead.as_str()) {
        return Err(format!(
            "only SELECT, SHOW, DESCRIBE, EXPLAIN, and WITH ... SELECT are allowed (statement starts with {lead})"
        ));
    }

    // 3. 금지 구문
    let word_at = |index: usize| match body.get(index) {
        Some(SqlToken::Word(word)) => word.to_ascii_uppercase(),
        _ => String::new(),
    };
    let mut depth = 0usize;
    for (index, token) in body.iter().enumerate() {
        let word = match token {
            SqlToken::Symbol(b'(') => {
                depth += 1;
                continue;
            }
            SqlToken::Symbol(b')') => {
                depth = depth.saturating_sub(1);
                continue;
            }
            SqlToken::Word(word) => word.to_ascii_uppercase(),
            // 따옴표 식별자 함수 호출
            SqlToken::Quoted(name) => {
                let name = name.to_ascii_uppercase();
                if body.get(index + 1) == Some(&SqlToken::Symbol(b'('))
                    && BLOCKED_FUNCTIONS.contains(&name.as_str())
                {
                    return Err(format!("{name}() is not allowed"));
                }
                continue;
            }
            _ => continue,
        };
        let next = word_at(index + 1);
        let is_call = body.get(index + 1) == Some(&SqlToken::Symbol(b'('));
        match word.as_str() {
            "INTO" if next == "OUTFILE" || next == "DUMPFILE" => {
                return Err(format!("INTO {next} writes a file on the server"));
            }
            "FOR" if next == "UPDATE" || next == "SHARE" => {
                return Err(format!("FOR {next} is a locking read"));
            }
            "LOCK" if next == "IN" => {
                return Err("LOCK IN SHARE MODE is a locking read".to_string());
            }
            "ANALYZE" if matches!(lead.as_str(), "EXPLAIN" | "DESCRIBE" | "DESC") => {
                return Err("EXPLAIN ANALYZE executes the statement; use plain EXPLAIN".to_string());
            }
            // REPLACE(), INSERT() 함수 호출은 쓰기 아님
            _ if lead == "WITH"
                && depth == 0
                && !is_call
                && WRITE_KEYWORDS.contains(&word.as_str()) =>
            {
                return Err(format!("WITH may only lead a SELECT (found {word})"));
            }
            _ if is_call && BLOCKED_FUNCTIONS.contains(&word.as_str()) => {
                return Err(format!("{word}() is not allowed"));
            }
            _ => {}
        }
    }
    Ok(())
}
// 2. Token scanner ------------------------------------------------------------
fn scan_sql_tokens(sql: &str) -> Result<Vec<SqlToken<'_>>, String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            _ if byte.is_ascii_whitespace() => index += 1,
            // 줄 주석 (-- 뒤 공백 필수)
            b'-' if bytes.get(index + 1) == Some(&b'-')
                && bytes
                    .get(index + 2)
                    .is_none_or(|next| next.is_ascii_whitespace() || next.is_ascii_control()) =>
            {
                index =
                    memchr::memchr(b'\n', &bytes[index..]).map_or(bytes.len(), |end| index + end);
            }
            b'#' => {
                index =
                    memchr::memchr(b'\n', &bytes[index..]).map_or(bytes.len(), |end| index + end);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let Some(length) = memchr::memmem::find(&bytes[index + 2..], b"*/") else {
                    return Err("unterminated /* comment".to_string());
                };
                let comment = &sql[index + 2..index + 2 + length];
                // 실행 주석 (MySQL /*!, MariaDB /*M!)
                if comment.starts_with('!') || comment.starts_with("M!") {
                    return Err("executable comments (/*! ... */) are not allowed".to_string());
                }
                let hint = comment.to_ascii_uppercase();
                if comment.starts_with('+')
                    && (hint.contains("SET_VAR") || hint.contains("MAX_EXECUTION_TIME"))
                {
                    return Err(
                        "optimizer hints that change session variables or the time limit are not allowed"
                            .to_string(),
                    );
                }
                index += length + 4;
            }
            b'\'' | b'"' | b'`' => {
                // 닫는 따옴표 탐색 (중복 따옴표, 백슬래시 이스케이프)
                let mut cursor = index + 1;
                loop {
                    match bytes.get(cursor) {
                        None => return Err("unterminated quoted text".to_string()),
                        Some(b'\\') if byte != b'`' => cursor += 2,
                        Some(found) if *found == byte && bytes.get(cursor + 1) == Some(&byte) => {
                            cursor += 2;
                        }
                        Some(found) if *found == byte => break,
                        Some(_) => cursor += 1,
                    }
                }
                tokens.push(SqlToken::Quoted(&sql[index + 1..cursor]));
                index = cursor + 1;
            }
            _ if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80 => {
                let start = index;
                while bytes.get(index).is_some_and(|next| {
                    next.is_ascii_alphanumeric() || *next == b'_' || *next == b'$' || *next >= 0x80
                }) {
                    index += 1;
                }
                tokens.push(SqlToken::Word(&sql[start..index]));
            }
            _ => {
                tokens.push(SqlToken::Symbol(byte));
                index += 1;
            }
        }
    }
    Ok(tokens)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_read_statements() {
        let accepted = [
            "SELECT 1",
            "select id, name from sample_table where id = 1 limit 10;",
            "  -- leading comment\n  SELECT 1 ;  -- trailing\n",
            "# hash comment\nSHOW TABLES",
            "SHOW CREATE TABLE `sample_table`",
            "SHOW INDEX FROM sample_table",
            "DESCRIBE sample_table",
            "desc sample_table",
            "EXPLAIN SELECT * FROM sample_table WHERE id = 1",
            "EXPLAIN FORMAT=JSON SELECT 1",
            "EXPLAIN UPDATE sample_table SET name = 'x' WHERE id = 1",
            "WITH recent AS (SELECT id FROM sample_table) SELECT * FROM recent",
            "(SELECT 1) UNION ALL (SELECT 2)",
            "SELECT 'FOR UPDATE; DROP TABLE x' AS text_value",
            "SELECT \"it's\" AS quoted, 'a''b' AS doubled, 'c\\'d' AS escaped",
            "SELECT `update`, `delete` FROM sample_table",
            "SELECT /* plain comment */ 1",
            "SELECT /*+ INDEX(sample_table idx_name) */ id FROM sample_table",
            "SELECT sleep_count, benchmark_score FROM sample_table",
            "SELECT 5--3",
            "SELECT id INTO @picked FROM sample_table LIMIT 1",
            "WITH t AS (SELECT 1 x) SELECT REPLACE(x, '1', '2'), INSERT('abc', 1, 1, 'z') FROM t",
            "WITH `cte` (a) AS (SELECT 1) SELECT a FROM `cte`",
            "SELECT `sleep` FROM sample_table",
            "SELECT 한글열 FROM 표",
        ];
        for sql in accepted {
            assert_eq!(check_read_only(sql), Ok(()), "{sql}");
        }
    }
    #[test]
    fn rejects_writes_and_side_effects() {
        let rejected = [
            ("", "empty"),
            ("   -- only a comment\n", "empty"),
            (";", "empty"),
            ("INSERT INTO sample_table VALUES (1)", "INSERT"),
            ("update sample_table set name = 'x'", "UPDATE"),
            ("DELETE FROM sample_table", "DELETE"),
            ("DROP TABLE sample_table", "DROP"),
            ("SET SESSION TRANSACTION READ WRITE", "SET"),
            ("USE other_schema", "USE"),
            ("CALL sample_procedure()", "CALL"),
            ("LOCK TABLES sample_table WRITE", "LOCK"),
            ("SELECT 1; DELETE FROM sample_table", "one statement"),
            ("SELECT 1;;", "one statement"),
            ("SELECT * FROM sample_table FOR UPDATE", "locking read"),
            ("SELECT * FROM sample_table for share", "locking read"),
            (
                "SELECT * FROM sample_table LOCK IN SHARE MODE",
                "locking read",
            ),
            ("SELECT SLEEP(5)", "SLEEP()"),
            ("select benchmark (1000000, md5('x'))", "BENCHMARK()"),
            ("SELECT GET_LOCK('name', 10)", "GET_LOCK()"),
            ("SELECT `GET_LOCK`('name', 0)", "GET_LOCK()"),
            ("SELECT `sleep` (5)", "SLEEP()"),
            ("SHOW TABLES WHERE `BENCHMARK`(1e9, 1)", "BENCHMARK()"),
            ("SELECT \"LOAD_FILE\"('/etc/hosts')", "LOAD_FILE()"),
            ("SELECT LOAD_FILE('/etc/passwd')", "LOAD_FILE()"),
            (
                "SELECT * FROM sample_table INTO OUTFILE '/tmp/x'",
                "OUTFILE",
            ),
            ("SELECT 1 INTO DUMPFILE '/tmp/x'", "DUMPFILE"),
            (
                "EXPLAIN ANALYZE SELECT * FROM sample_table",
                "EXPLAIN ANALYZE",
            ),
            ("/*!50000 DROP TABLE sample_table */ SELECT 1", "executable"),
            ("SELECT /*M!100100 1, */ 2", "executable"),
            ("SELECT /*+ MAX_EXECUTION_TIME(600000) */ 1", "hints"),
            ("SELECT /*+ set_var(sql_mode='') */ 1", "hints"),
            (
                "WITH old AS (SELECT id FROM sample_table) DELETE FROM sample_table WHERE id IN (SELECT id FROM old)",
                "WITH",
            ),
            ("SELECT 'unterminated", "unterminated"),
            ("SELECT 1 /* open", "unterminated"),
        ];
        for (sql, reason) in rejected {
            let error = check_read_only(sql).expect_err(sql);
            assert!(error.contains(reason), "{sql}: {error}");
        }
    }
}
