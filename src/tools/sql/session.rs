//! session.rs
//! tools::sql::session
//!
//! One MySQL connection per server process: statements run one at a time inside a READ ONLY transaction,
//! an idle connection is reused for a short window and then closed by a reaper thread, and a connection
//! that timed out, broke, or returned a truncated result is dropped instead of reused.
//! The server-side time limit is set per session; a client-side timeout also sends KILL QUERY.
//!

use crate::core::settings::MysqlSettings;
use crate::core::sql::password::resolve_password;
use crate::core::sql::{ConnectOptions, Connection, MysqlError, RowLimits, StatementOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

// 유휴 연결 유지 시간
const IDLE_CLOSE: Duration = Duration::from_secs(60);
// 서버 제한 뒤 클라이언트 대기 여유
const IO_MARGIN: Duration = Duration::from_secs(2);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
// ER_QUERY_TIMEOUT (MySQL), ER_STATEMENT_TIMEOUT (MariaDB)
const SERVER_TIMEOUT_CODES: [u16; 2] = [3024, 1969];
// 렉서 전제를 깨는 sql_mode (백슬래시 이스케이프, 큰따옴표 문자열)
const LEXER_BREAKING_MODES: [&str; 8] = [
    "NO_BACKSLASH_ESCAPES",
    "ANSI_QUOTES",
    "ANSI",
    "DB2",
    "MAXDB",
    "MSSQL",
    "ORACLE",
    "POSTGRESQL",
];
const MODE_LIMITS: RowLimits = RowLimits {
    max_rows: 1,
    max_bytes: 64 * 1024,
};
const NO_LIMITS: RowLimits = RowLimits {
    max_rows: 0,
    max_bytes: 0,
};

struct IdleSession {
    connection: Connection,
    parked_at: Instant,
    // 세션에 적용된 서버 제한
    limit_ms: u64,
}
pub(super) struct QueryRun {
    pub(super) outcome: StatementOutcome,
    pub(super) elapsed: Duration,
}
static IDLE_SESSION: Mutex<Option<IdleSession>> = Mutex::new(None);
static REAPER_RUNNING: AtomicBool = AtomicBool::new(false);

// 1. Read-only run ------------------------------------------------------------
pub(super) fn run_read_only(
    config: &MysqlSettings,
    sql: &str,
    limits: &RowLimits,
    timeout_ms: u64,
) -> Result<QueryRun, String> {
    // 1. 직렬 실행 (슬롯 잠금 유지)
    let mut slot = IDLE_SESSION.lock().unwrap_or_else(PoisonError::into_inner);
    let parked = slot
        .take()
        .filter(|idle| idle.connection.is_reusable() && idle.parked_at.elapsed() < IDLE_CLOSE);
    let was_parked = parked.is_some();
    let mut session = match parked {
        Some(idle) => idle,
        None => open_session(config, timeout_ms)?,
    };

    // 2. 읽기 전용 트랜잭션 (끊긴 유휴 연결은 1회 재연결)
    let mut begun = begin_read_only(&mut session, timeout_ms);
    if begun.is_err() && was_parked {
        session = open_session(config, timeout_ms)?;
        begun = begin_read_only(&mut session, timeout_ms);
    }
    begun.map_err(|error| format!("Failed to start the read-only transaction: {error}"))?;

    // 3. 문장 실행
    let started = Instant::now();
    let ran = session.connection.run_statement(sql, limits);
    let elapsed = started.elapsed();

    // 4. 롤백 후 재사용 대기
    let mut was_cancelled = false;
    if session.connection.is_reusable()
        && session
            .connection
            .run_statement("ROLLBACK", &NO_LIMITS)
            .is_ok()
        && session.connection.is_reusable()
    {
        session.parked_at = Instant::now();
        *slot = Some(session);
        start_idle_reaper();
    } else if matches!(&ran, Err(error) if error.is_timeout()) {
        // 서버 잔존 문장 중단 (1회 재시도)
        let connection_id = session.connection.connection_id();
        was_cancelled = cancel_running_query(config, connection_id)
            || cancel_running_query(config, connection_id);
    }
    match ran {
        Ok(outcome) => Ok(QueryRun { outcome, elapsed }),
        Err(error) if error.is_timeout() && was_cancelled => Err(format!(
            "The statement ran past {timeout_ms} ms and was cancelled; narrow it (indexed WHERE, smaller LIMIT) before retrying"
        )),
        Err(error) if error.is_timeout() => Err(format!(
            "The statement ran past {timeout_ms} ms and the cancel request did not go through, so it may still be running on the server; do not retry until it has ended"
        )),
        Err(MysqlError::Server { code, .. }) if SERVER_TIMEOUT_CODES.contains(&code) => {
            Err(format!(
                "The statement ran past {timeout_ms} ms and the server stopped it; narrow it (indexed WHERE, smaller LIMIT) before retrying"
            ))
        }
        Err(MysqlError::Server {
            code,
            sql_state,
            message,
        }) => Err(format!("MySQL error {code} ({sql_state}): {message}")),
        Err(error) => Err(format!("MySQL query failed: {error}")),
    }
}

// 2. Session setup ------------------------------------------------------------
fn open_session(config: &MysqlSettings, timeout_ms: u64) -> Result<IdleSession, String> {
    let options = ConnectOptions {
        host: config.host.clone(),
        port: config.port,
        user: config.user.clone(),
        password: resolve_password(&config.password)?,
        database: config.database.clone(),
        connect_timeout: Duration::from_millis(config.connect_timeout_ms),
        io_timeout: Duration::from_millis(timeout_ms) + IO_MARGIN,
    };
    let mut connection = Connection::connect(&options).map_err(|error| match error {
        MysqlError::Server {
            code,
            sql_state,
            message,
        } => format!("MySQL connect failed: error {code} ({sql_state}): {message}"),
        other => format!("MySQL connect failed: {other}"),
    })?;

    // 게이트 렉서 전제 sql_mode 고정
    let modes = match connection.run_statement("SELECT @@SESSION.sql_mode", &MODE_LIMITS) {
        Ok(StatementOutcome::Rows(result)) => result
            .rows
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .map(|cell| String::from_utf8_lossy(&cell).into_owned())
            .unwrap_or_default(),
        Ok(StatementOutcome::Done(_)) => String::new(),
        Err(error) => return Err(format!("MySQL session setup failed: {error}")),
    };
    let kept: Vec<&str> = modes
        .split(',')
        .filter(|mode| !mode.is_empty() && !LEXER_BREAKING_MODES.contains(mode))
        .collect();
    if kept.len() != modes.split(',').filter(|mode| !mode.is_empty()).count() {
        let is_plain = kept.iter().all(|mode| {
            mode.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        });
        let fixed_mode = format!("SET SESSION sql_mode = '{}'", kept.join(","));
        if !is_plain || connection.run_statement(&fixed_mode, &NO_LIMITS).is_err() {
            return Err(
                "MySQL session setup failed: the server sql_mode changes quoting rules and could not be reset for this session"
                    .to_string(),
            );
        }
    }
    // 서버 제한 미적용 표시
    Ok(IdleSession {
        connection,
        parked_at: Instant::now(),
        limit_ms: 0,
    })
}
fn begin_read_only(session: &mut IdleSession, timeout_ms: u64) -> Result<(), MysqlError> {
    // 1. 서버 실행 시간 제한 (MySQL, 실패 시 MariaDB 변수)
    if session.limit_ms != timeout_ms {
        session
            .connection
            .set_io_timeout(Duration::from_millis(timeout_ms) + IO_MARGIN)?;
        let mysql_limit = format!("SET SESSION MAX_EXECUTION_TIME = {timeout_ms}");
        match session.connection.run_statement(&mysql_limit, &NO_LIMITS) {
            Ok(_) => {}
            Err(MysqlError::Server { .. }) => {
                let seconds = timeout_ms.div_ceil(1_000);
                let mariadb_limit = format!("SET SESSION max_statement_time = {seconds}");
                match session.connection.run_statement(&mariadb_limit, &NO_LIMITS) {
                    Ok(_) | Err(MysqlError::Server { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
        session.limit_ms = timeout_ms;
    }

    // 2. 쓰기 차단 트랜잭션
    session
        .connection
        .run_statement("START TRANSACTION READ ONLY", &NO_LIMITS)
        .map(drop)
}

// 3. Timeout cancel -----------------------------------------------------------
fn cancel_running_query(config: &MysqlSettings, connection_id: u32) -> bool {
    let Ok(password) = resolve_password(&config.password) else {
        return false;
    };
    let options = ConnectOptions {
        host: config.host.clone(),
        port: config.port,
        user: config.user.clone(),
        password,
        database: None,
        connect_timeout: KILL_TIMEOUT,
        io_timeout: KILL_TIMEOUT,
    };
    Connection::connect(&options).is_ok_and(|mut killer| {
        killer
            .run_statement(&format!("KILL QUERY {connection_id}"), &NO_LIMITS)
            .is_ok()
    })
}

// 4. Idle connection lifetime -------------------------------------------------
fn start_idle_reaper() {
    if REAPER_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = thread::Builder::new()
        .name("fs-mcp-sql-idle".to_string())
        .spawn(|| {
            let mut wait = IDLE_CLOSE;
            loop {
                thread::sleep(wait);
                let mut slot = IDLE_SESSION.lock().unwrap_or_else(PoisonError::into_inner);
                let idle_for = slot.as_ref().map(|idle| idle.parked_at.elapsed());
                match idle_for {
                    // 남은 유휴 시간만큼 재대기
                    Some(idle_for) if idle_for < IDLE_CLOSE => wait = IDLE_CLOSE - idle_for,
                    _ => {
                        // 만료 연결 종료 (잠금 안에서 표시 해제)
                        drop(slot.take());
                        REAPER_RUNNING.store(false, Ordering::Release);
                        return;
                    }
                }
            }
        });
    if spawned.is_err() {
        REAPER_RUNNING.store(false, Ordering::Release);
    }
}
pub fn close_idle_session() {
    let idle = IDLE_SESSION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    drop(idle);
}
