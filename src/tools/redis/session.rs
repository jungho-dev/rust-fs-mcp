//! session.rs
//! tools::redis::session
//!
//! Serialized redis-command runs: one global lock, one idle connection per target kept for 60 s,
//! a PING before reuse with one reconnect, and a reaper thread that closes expired connections.
//! A connection is dropped after an I/O, protocol, timeout, or discard-limit failure, a cancel, or MULTI or WATCH.
//! Each command's socket timeout is the time left until the call deadline; a user command is never retried.
//!

use super::render::{BatchStop, render_command_line, render_scan_line};
use crate::core::cancel::current_cancel_flag;
use crate::core::redis::{ConnectOptions, Connection, RedisError, Reply, ReplyLimits};
use crate::core::settings::RedisTarget;
use crate::core::sql::password::resolve_secret;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

const IDLE_CLOSE: Duration = Duration::from_secs(60);
const PING_LIMITS: ReplyLimits = ReplyLimits {
    max_items: 16,
    max_bytes: 64 * 1024,
};
static IDLE_CONNECTIONS: Mutex<Vec<IdleRedis>> = Mutex::new(Vec::new());
static REAPER_RUNNING: AtomicBool = AtomicBool::new(false);

struct IdleRedis {
    target: RedisTarget,
    connection: Connection,
    parked_at: Instant,
}
pub(super) struct BatchRequest<'a> {
    pub(super) commands: &'a [Vec<String>],
    pub(super) db: u32,
    pub(super) scan_all: bool,
    pub(super) timeout_ms: u64,
    // 호출 전체 공유 예산
    pub(super) limits: ReplyLimits,
}
#[derive(Debug)]
pub(super) struct BatchRun {
    pub(super) lines: Vec<String>,
    pub(super) executed: usize,
    pub(super) error_count: usize,
    pub(super) truncated: bool,
    // 중단 명령과 사유
    pub(super) stopped: Option<BatchStop>,
    // 배치 중간 실패 문구
    pub(super) failure: Option<String>,
}
enum ScanResult {
    Merged {
        cursor: String,
        items: Vec<Reply>,
        cut: bool,
    },
    Plain(Reply),
}

// 1. Batch run ----------------------------------------------------------------
pub(super) fn run_batch(
    target: &RedisTarget,
    request: &BatchRequest<'_>,
) -> Result<BatchRun, String> {
    // 1. 연결 열기 (비밀번호 해석, 오류 문구)
    let open_connection = |io_timeout: Duration| -> Result<Connection, String> {
        let options = build_connect_options(target, request.db, io_timeout)?;
        // DNS 해석 시한 없음 (std 해석 제약)
        Connection::connect(&options).map_err(|error| match error {
            RedisError::Auth(text) => format!(
                "Redis authentication for target `{}` failed: {text}",
                target.name
            ),
            RedisError::Server(text) => format!(
                "Redis database {} is not available on target `{}`: {text}",
                request.db, target.name
            ),
            other => format!(
                "Redis connect to target `{}` ({}:{}) failed: {other}",
                target.name, target.host, target.port
            ),
        })
    };

    // 2. SCAN 반복 (cursor 재작성, 항목 병합)
    let run_scan = |connection: &mut Connection,
                    argv: &[String],
                    cursor_index: usize,
                    limits: &mut ReplyLimits,
                    deadline: Instant|
     -> Result<ScanResult, RedisError> {
        let mut step_argv: Vec<Vec<u8>> =
            argv.iter().map(|word| word.as_bytes().to_vec()).collect();
        let mut cursor = argv[cursor_index].clone();
        let mut items = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let cancelled = current_cancel_flag().is_some_and(|flag| flag.load(Ordering::Relaxed));
            if cancelled || remaining.is_zero() {
                let cut = cursor != "0";
                return Ok(ScanResult::Merged { cursor, items, cut });
            }
            step_argv[cursor_index] = cursor.as_bytes().to_vec();
            connection.set_io_timeout(remaining)?;
            // 예산 소진 페이지 = 병합분 유지 중단
            let parts = match connection.run(&step_argv, limits)? {
                Reply::Array(parts) if parts.len() == 2 => parts,
                Reply::ArrayCut { .. } => {
                    return Ok(ScanResult::Merged {
                        cursor,
                        items,
                        cut: true,
                    });
                }
                other => return Ok(ScanResult::Plain(other)),
            };
            let mut parts = parts.into_iter();
            let (Some(next), Some(page)) = (parts.next(), parts.next()) else {
                return Err(RedisError::Protocol("scan reply has no cursor".to_string()));
            };
            let next = match next {
                Reply::Bulk(next) => next,
                Reply::BulkCut { .. } => {
                    return Ok(ScanResult::Merged {
                        cursor,
                        items,
                        cut: true,
                    });
                }
                _ => return Err(RedisError::Protocol("scan reply has no cursor".to_string())),
            };
            let (page_items, cut) = match page {
                Reply::Array(page_items) => (page_items, false),
                Reply::ArrayCut {
                    items: page_items, ..
                } => (page_items, true),
                _ => {
                    return Err(RedisError::Protocol(
                        "scan reply has no item list".to_string(),
                    ));
                }
            };
            items.extend(page_items);
            cursor = String::from_utf8_lossy(&next).into_owned();
            let finished = cursor == "0";
            if cut || !connection.is_reusable() || (!finished && items.len() >= limits.max_items) {
                return Ok(ScanResult::Merged {
                    cursor,
                    items,
                    cut: true,
                });
            }
            if finished {
                return Ok(ScanResult::Merged {
                    cursor,
                    items,
                    cut: false,
                });
            }
        }
    };

    // 3. 실패 문구
    let describe_failure = |number: usize, name: &str, error: &RedisError| -> String {
        if error.is_timeout() {
            return format!(
                "Redis command {number} ({name}) did not reply within {} ms; the connection was closed and the command may still have run on the server",
                request.timeout_ms
            );
        }
        if let RedisError::Protocol(detail) = error {
            return format!("Redis protocol error at command {number}: {detail}");
        }
        format!("Redis command {number} ({name}) failed: {error}; the connection was closed")
    };

    // 4. 유휴 연결 정리 스레드
    let start_idle_reaper = || {
        if REAPER_RUNNING.swap(true, Ordering::AcqRel) {
            return;
        }
        let spawned = thread::Builder::new()
            .name("fs-mcp-redis-idle".to_string())
            .spawn(|| {
                let mut wait = IDLE_CLOSE;
                loop {
                    thread::sleep(wait);
                    let mut slot = IDLE_CONNECTIONS
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    slot.retain(|idle| idle.parked_at.elapsed() < IDLE_CLOSE);
                    let oldest = slot.iter().map(|idle| idle.parked_at.elapsed()).max();
                    // 빈 목록 = 스레드 종료 (잠금 안에서 표시 해제)
                    let Some(oldest) = oldest else {
                        REAPER_RUNNING.store(false, Ordering::Release);
                        return;
                    };
                    wait = IDLE_CLOSE.saturating_sub(oldest);
                }
            });
        if spawned.is_err() {
            REAPER_RUNNING.store(false, Ordering::Release);
        }
    };

    // 5. 직렬 실행과 연결 확보 (재사용 PING, 1회 재연결)
    let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
    let mut slot = IDLE_CONNECTIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let position = slot.iter().position(|idle| idle.target == *target);
    let parked = position.map(|position| slot.swap_remove(position));
    let first_timeout = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    let mut reused = parked
        .filter(|idle| idle.parked_at.elapsed() < IDLE_CLOSE)
        .map(|idle| idle.connection);
    let mut ping_limits = PING_LIMITS;
    let pinged = reused.as_mut().is_some_and(|connection| {
        connection.set_io_timeout(first_timeout).is_ok()
            && matches!(
                connection.run(&[b"PING".to_vec()], &mut ping_limits),
                Ok(Reply::Status(_))
            )
            && connection.is_reusable()
    });
    let mut connection = match reused {
        Some(connection) if pinged => connection,
        None => open_connection(first_timeout)?,
        Some(stale) => {
            // PING 실패 뒤 남은 시간
            drop(stale);
            let time_left = deadline.saturating_duration_since(Instant::now());
            if time_left.is_zero() {
                return Err(format!(
                    "The call reached timeout_ms {} before command 1; it and {} later command(s) were not sent",
                    request.timeout_ms,
                    request.commands.len().saturating_sub(1)
                ));
            }
            open_connection(time_left)?
        }
    };

    // 6. 데이터베이스 선택
    if connection.db() != request.db
        && let Err(error) = connection.select(request.db)
    {
        return Err(format!(
            "Redis database {} is not available on target `{}`: {error}",
            request.db, target.name
        ));
    }

    // 7. 명령 순차 실행
    let mut limits = request.limits;
    let mut run = BatchRun {
        lines: Vec::new(),
        executed: 0,
        error_count: 0,
        truncated: false,
        stopped: None,
        failure: None,
    };
    let mut in_multi = false;
    let mut used_transaction = false;
    for (index, argv) in request.commands.iter().enumerate() {
        // 7-1. 취소와 기한
        let number = index + 1;
        let name = argv[0].to_ascii_uppercase();
        let rest = request.commands.len() - number;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if current_cancel_flag().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            run.failure = Some(format!(
                "The call was cancelled before command {number}; the connection was closed"
            ));
            break;
        }
        if remaining.is_zero() {
            run.failure = Some(format!(
                "The call reached timeout_ms {} before command {number}; it and {rest} later command(s) were not sent",
                request.timeout_ms
            ));
            break;
        }

        // 7-2. 전송 (scanAll 이면 cursor 반복)
        let cursor_index = match name.as_str() {
            "SCAN" => 1,
            "SSCAN" | "HSCAN" | "ZSCAN" => 2,
            _ => 0,
        };
        let ran = if request.scan_all && cursor_index > 0 && cursor_index < argv.len() {
            run_scan(&mut connection, argv, cursor_index, &mut limits, deadline)
        } else {
            let argv_bytes: Vec<Vec<u8>> =
                argv.iter().map(|word| word.as_bytes().to_vec()).collect();
            let plain = connection
                .set_io_timeout(remaining)
                .and_then(|()| connection.run(&argv_bytes, &mut limits));
            plain.map(ScanResult::Plain)
        };
        run.executed += 1;
        let outcome = match ran {
            Ok(outcome) => outcome,
            Err(error) => {
                run.failure = Some(describe_failure(number, &name, &error));
                break;
            }
        };

        // 7-3. 응답 줄
        let is_error = matches!(outcome, ScanResult::Plain(Reply::Error(_)));
        let line = match &outcome {
            ScanResult::Merged { cursor, items, cut } => {
                run.truncated |= *cut;
                render_scan_line(&name, cursor, items, &mut run.truncated)
            }
            ScanResult::Plain(reply) => render_command_line(&name, reply, &mut run.truncated),
        };
        run.lines.push(line);
        run.error_count += usize::from(is_error);

        // 7-4. 트랜잭션 상태와 중단 (폐기 한도 초과 포함)
        used_transaction |= matches!(name.as_str(), "MULTI" | "WATCH");
        match name.as_str() {
            "MULTI" => in_multi = !is_error,
            "EXEC" | "DISCARD" => in_multi = false,
            _ => {}
        }
        if !connection.is_reusable() {
            run.truncated = true;
            run.stopped = (rest > 0).then_some(BatchStop::ReadLimit { number, rest });
            break;
        }
        if is_error && !in_multi {
            run.stopped = (rest > 0).then_some(BatchStop::ErrorReply { number, rest });
            break;
        }
    }

    // 8. 연결 보관 (실패, 취소, MULTI, WATCH = 종료)
    let cancelled = current_cancel_flag().is_some_and(|flag| flag.load(Ordering::Relaxed));
    if run.failure.is_none() && !used_transaction && !cancelled && connection.is_reusable() {
        slot.push(IdleRedis {
            target: target.clone(),
            connection,
            parked_at: Instant::now(),
        });
        start_idle_reaper();
    }
    Ok(run)
}

// 2. Shutdown -----------------------------------------------------------------
pub fn close_idle_redis_session() {
    let idle = std::mem::take(
        &mut *IDLE_CONNECTIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    drop(idle);
}

// 3. (내부) Connect options ---------------------------------------------------
fn build_connect_options(
    target: &RedisTarget,
    db: u32,
    io_timeout: Duration,
) -> Result<ConnectOptions, String> {
    let label = format!("redis.{}", target.name);
    Ok(ConnectOptions {
        host: target.host.clone(),
        port: target.port,
        user: target.user.clone(),
        password: resolve_secret(&target.password, &label)?,
        db,
        connect_timeout: Duration::from_millis(target.connect_timeout_ms).min(io_timeout),
        io_timeout,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::redis::fake_server::spawn_fake_redis;
    use crate::core::settings::PasswordSource;

    fn build_target(name: &str, port: u16) -> RedisTarget {
        RedisTarget {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port,
            user: None,
            password: PasswordSource::Unset,
            db: 0,
            connect_timeout_ms: 5_000,
            command_timeout_ms: 5_000,
            max_items: 100,
        }
    }
    fn build_commands(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|word| word.to_string()).collect())
            .collect()
    }
    fn build_request(
        commands: &[Vec<String>],
        scan_all: bool,
        max_items: usize,
    ) -> BatchRequest<'_> {
        BatchRequest {
            commands,
            db: 0,
            scan_all,
            timeout_ms: 5_000,
            limits: ReplyLimits {
                max_items,
                max_bytes: 64 * 1024,
            },
        }
    }
    fn build_argv(words: &[&str]) -> Vec<Vec<u8>> {
        words.iter().map(|word| word.as_bytes().to_vec()).collect()
    }
    // 대상 유휴 연결 수동 종료 (다른 테스트 유지)
    fn take_parked(target: &RedisTarget) -> bool {
        let mut slot = IDLE_CONNECTIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let before = slot.len();
        slot.retain(|idle| idle.target != *target);
        slot.len() < before
    }
    #[test]
    fn stops_at_the_first_error_outside_a_transaction() {
        let replies = vec![
            Some(b"+OK\r\n".to_vec()),
            Some(b"-WRONGTYPE Operation against a key\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("stop-on-error", port);
        let commands = build_commands(&[&["set", "k", "v"], &["LPUSH", "k", "x"], &["GET", "k"]]);
        let run = run_batch(&target, &build_request(&commands, false, 100)).unwrap();
        assert_eq!(
            run.lines,
            [
                r#"{"cmd":"SET","reply":"OK"}"#,
                r#"{"cmd":"LPUSH","error":"WRONGTYPE Operation against a key"}"#,
            ]
        );
        assert_eq!((run.executed, run.error_count), (2, 1));
        assert_eq!(
            run.stopped,
            Some(BatchStop::ErrorReply { number: 2, rest: 1 })
        );
        assert!(run.failure.is_none());
        assert!(take_parked(&target));
        assert_eq!(
            server.join().unwrap(),
            [
                build_argv(&["set", "k", "v"]),
                build_argv(&["LPUSH", "k", "x"])
            ]
        );
    }
    #[test]
    fn carries_queued_errors_to_exec_and_drops_the_connection() {
        let replies = vec![
            Some(b"+OK\r\n".to_vec()),
            Some(b"-ERR wrong number of arguments for 'set' command\r\n".to_vec()),
            Some(b"+QUEUED\r\n".to_vec()),
            Some(b"-EXECABORT Transaction discarded because of previous errors.\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("queued-errors", port);
        let commands = build_commands(&[
            &["MULTI"],
            &["SET", "k"],
            &["INCR", "n"],
            &["EXEC"],
            &["GET", "k"],
        ]);
        let run = run_batch(&target, &build_request(&commands, false, 100)).unwrap();
        assert_eq!((run.executed, run.error_count), (4, 2));
        assert_eq!(
            run.stopped,
            Some(BatchStop::ErrorReply { number: 4, rest: 1 })
        );
        assert_eq!(run.lines[2], r#"{"cmd":"INCR","reply":"QUEUED"}"#);
        assert!(!take_parked(&target));
        assert_eq!(server.join().unwrap().len(), 4);
    }
    #[test]
    fn drops_the_connection_after_watch() {
        let replies = vec![Some(b"+OK\r\n".to_vec()), Some(b"$-1\r\n".to_vec())];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("watch-drop", port);
        let commands = build_commands(&[&["watch", "k"], &["GET", "k"]]);
        let run = run_batch(&target, &build_request(&commands, false, 100)).unwrap();
        assert_eq!(run.lines[1], r#"{"cmd":"GET","reply":null}"#);
        assert!(run.stopped.is_none());
        assert!(!take_parked(&target));
        assert_eq!(server.join().unwrap().len(), 2);
    }
    #[test]
    fn pings_a_parked_connection_before_reuse() {
        let replies = vec![
            Some(b"+OK\r\n".to_vec()),
            Some(b"+PONG\r\n".to_vec()),
            Some(b"$1\r\nv\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("ping-reuse", port);
        let first = build_commands(&[&["SET", "k", "v"]]);
        run_batch(&target, &build_request(&first, false, 100)).unwrap();
        let second = build_commands(&[&["GET", "k"]]);
        let run = run_batch(&target, &build_request(&second, false, 100)).unwrap();
        assert_eq!(run.lines, [r#"{"cmd":"GET","reply":"v"}"#]);
        assert!(take_parked(&target));
        assert_eq!(
            server.join().unwrap(),
            [
                build_argv(&["SET", "k", "v"]),
                build_argv(&["PING"]),
                build_argv(&["GET", "k"]),
            ]
        );
    }
    #[test]
    fn reconnects_once_when_the_ping_fails() {
        // 1. 실패할 유휴 연결 (다른 서버)
        let (stale_port, stale_server) = spawn_fake_redis(vec![Some(b"-ERR gone\r\n".to_vec())]);
        let (port, server) = spawn_fake_redis(vec![Some(b"$1\r\nv\r\n".to_vec())]);
        let target = build_target("ping-reconnect", port);
        let stale_options = ConnectOptions {
            host: "127.0.0.1".to_string(),
            port: stale_port,
            user: None,
            password: String::new(),
            db: 0,
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(5),
        };
        IDLE_CONNECTIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(IdleRedis {
                target: target.clone(),
                connection: Connection::connect(&stale_options).unwrap(),
                parked_at: Instant::now(),
            });

        // 2. PING 실패 뒤 새 연결
        let commands = build_commands(&[&["GET", "k"]]);
        let run = run_batch(&target, &build_request(&commands, false, 100)).unwrap();
        assert_eq!(run.lines, [r#"{"cmd":"GET","reply":"v"}"#]);
        assert_eq!(stale_server.join().unwrap(), [build_argv(&["PING"])]);
        assert!(take_parked(&target));
        assert_eq!(server.join().unwrap(), [build_argv(&["GET", "k"])]);
    }
    #[test]
    fn merges_scan_all_pages_until_the_cursor_returns() {
        // 1. 커서 0 복귀
        let replies = vec![
            Some(b"*2\r\n$1\r\n7\r\n*2\r\n$3\r\np:1\r\n$3\r\np:2\r\n".to_vec()),
            Some(b"*2\r\n$1\r\n0\r\n*1\r\n$3\r\np:3\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("scan-merge", port);
        let commands = build_commands(&[&["scan", "0", "MATCH", "p:*"]]);
        let run = run_batch(&target, &build_request(&commands, true, 100)).unwrap();
        assert_eq!(
            run.lines,
            [r#"{"cmd":"SCAN","reply":{"cursor":"0","items":["p:1","p:2","p:3"]}}"#]
        );
        assert!(!run.truncated);
        assert!(take_parked(&target));
        assert_eq!(
            server.join().unwrap(),
            [
                build_argv(&["scan", "0", "MATCH", "p:*"]),
                build_argv(&["scan", "7", "MATCH", "p:*"]),
            ]
        );

        // 2. maxItems 도달 중단
        let replies = vec![Some(
            b"*2\r\n$1\r\n9\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n".to_vec(),
        )];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("scan-cap", port);
        let commands = build_commands(&[&["HSCAN", "h", "0"]]);
        let run = run_batch(&target, &build_request(&commands, true, 2)).unwrap();
        assert_eq!(
            run.lines,
            [r#"{"cmd":"HSCAN","reply":{"cursor":"9","items":["f","v"]}}"#]
        );
        assert!(run.truncated);
        assert!(take_parked(&target));
        assert_eq!(server.join().unwrap(), [build_argv(&["HSCAN", "h", "0"])]);
    }
    #[test]
    fn keeps_merged_scan_items_when_the_budget_cuts_a_page() {
        let replies = vec![
            Some(b"*2\r\n$1\r\n7\r\n*2\r\n$3\r\np:1\r\n$3\r\np:2\r\n".to_vec()),
            Some(b"*2\r\n$1\r\n0\r\n*1\r\n$3\r\np:3\r\n".to_vec()),
        ];
        let (port, server) = spawn_fake_redis(replies);
        let target = build_target("scan-budget-cut", port);
        let commands = build_commands(&[&["scan", "0", "MATCH", "p:*"]]);
        let mut request = build_request(&commands, true, 100);
        request.limits.max_bytes = 7;
        let run = run_batch(&target, &request).unwrap();
        assert_eq!(
            run.lines,
            [r#"{"cmd":"SCAN","reply":{"cursor":"7","items":["p:1","p:2"]}}"#]
        );
        assert!(run.truncated);
        assert!(run.failure.is_none());
        assert!(take_parked(&target));
        assert_eq!(
            server.join().unwrap(),
            [
                build_argv(&["scan", "0", "MATCH", "p:*"]),
                build_argv(&["scan", "7", "MATCH", "p:*"]),
            ]
        );
    }
    #[test]
    fn skips_the_reconnect_when_the_ping_used_the_time() {
        // 1. PING 무응답 유휴 연결, AUTH 무응답 대상
        let (stale_port, stale_server) = spawn_fake_redis(vec![None]);
        let (port, _server) = spawn_fake_redis(vec![None]);
        let mut target = build_target("ping-timeout", port);
        target.password = PasswordSource::Plain("pw".to_string());
        let stale_options = ConnectOptions {
            host: "127.0.0.1".to_string(),
            port: stale_port,
            user: None,
            password: String::new(),
            db: 0,
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(5),
        };
        IDLE_CONNECTIONS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(IdleRedis {
                target: target.clone(),
                connection: Connection::connect(&stale_options).unwrap(),
                parked_at: Instant::now(),
            });

        // 2. 호출 전체 = 시한 1회분
        let commands = build_commands(&[&["GET", "k"]]);
        let mut request = build_request(&commands, false, 100);
        request.timeout_ms = 1_000;
        let started = Instant::now();
        let outcome = run_batch(&target, &request);
        let elapsed = started.elapsed();
        assert!(outcome.is_err(), "{outcome:?}");
        assert!(elapsed < Duration::from_millis(2_000), "{elapsed:?}");
        assert!(!take_parked(&target));
        assert_eq!(stale_server.join().unwrap(), [build_argv(&["PING"])]);
    }
    #[test]
    fn stops_and_drops_past_the_discard_limit() {
        let (port, server) = spawn_fake_redis(vec![Some(b"$73400320\r\nabcd".to_vec())]);
        let target = build_target("discard-limit", port);
        let commands = build_commands(&[&["GET", "big"], &["GET", "k"]]);
        let mut request = build_request(&commands, false, 100);
        request.limits.max_bytes = 4;
        let run = run_batch(&target, &request).unwrap();
        assert_eq!(
            run.lines,
            [r#"{"cmd":"GET","reply":{"bytes":73400320,"prefix":"abcd","truncated":true}}"#]
        );
        assert!(run.truncated);
        assert_eq!(
            run.stopped,
            Some(BatchStop::ReadLimit { number: 1, rest: 1 })
        );
        assert_eq!(run.error_count, 0);
        assert!(!take_parked(&target));
        assert_eq!(server.join().unwrap(), [build_argv(&["GET", "big"])]);
    }
    #[test]
    fn bounds_the_connect_timeout_by_the_time_left() {
        let target = build_target("connect-clamp", 6379);
        let options = build_connect_options(&target, 2, Duration::from_millis(300)).unwrap();
        assert_eq!(
            (options.connect_timeout, options.io_timeout, options.db),
            (Duration::from_millis(300), Duration::from_millis(300), 2)
        );
        let options = build_connect_options(&target, 0, Duration::from_secs(60)).unwrap();
        assert_eq!(options.connect_timeout, Duration::from_millis(5_000));
    }
}
