//! isolate.rs
//! core::process::isolate
//!
//! Kill-able child-process runner for fancy-regex work that cannot be cancelled in-process.
//! The server re-runs its own executable with --isolated-worker: one JSON job line on stdin; stdout carries span record lines, then one JSON status line.
//! fancy-spans records stream to the parent as they are found, so only the caller's own output caps bound them.
//! The parent keeps stdin open until the worker exits; stdin EOF means the parent died, so the worker exits too.
//! After the first job one idle standby worker waits on its stdin job line; the next job takes it and a replacement starts, so at most one idles.
//! Without enable_worker_spawning (unit tests, library use) run_in_worker returns Unavailable and callers stay in-process.
//!

use serde_json::{Value, json};
use std::fmt;
use std::fmt::Write as _;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub const WORKER_FLAG: &str = "--isolated-worker";
// 작업·응답 봉투 버전 (2: 스팬 기록 스트림)
pub const PROTOCOL_VERSION: u64 = 2;
pub(crate) const FANCY_BACKTRACK_LIMIT: usize = 1_000_000;
// 수집형 응답 상한 (fs-search)
const MAX_REPLY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_JOB_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;
// 스트림 청크·대기열 (부모 버퍼 상한)
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const STREAM_QUEUE: usize = 4;
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
// 고아 워커 자가 종료 (부모 kill 우선)
const WATCHDOG_GRACE: Duration = Duration::from_secs(2);
const EXIT_WATCHDOG: i32 = 3;
const EXIT_REPLY_FAILED: i32 = 4;
const EXIT_ORPHANED: i32 = 5;

static WORKER_EXE: OnceLock<PathBuf> = OnceLock::new();
static SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);
static REPLY_CAP: AtomicU64 = AtomicU64::new(MAX_REPLY_BYTES);
// 예열 워커 1개 (작업 줄 대기)
static STANDBY: Mutex<Option<Child>> = Mutex::new(None);
static STANDBY_PENDING: AtomicBool = AtomicBool::new(false);
static STANDBY_JOBS: AtomicU64 = AtomicU64::new(0);

// 1. Error type ---------------------------------------------------------------
#[derive(Debug, PartialEq, Eq)]
pub enum WorkerError {
    // 스폰 미활성 (단위 테스트·라이브러리)
    Unavailable,
    SpawnFailed(String),
    TimedOut(Duration),
    Crashed { code: Option<i32>, stderr: String },
    Protocol(String),
    // 워커·소비자가 보고한 작업 오류
    Job(String),
}
impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => write!(formatter, "isolated worker spawning is not enabled"),
            Self::SpawnFailed(error) => {
                write!(formatter, "could not start the isolated worker: {error}")
            }
            Self::TimedOut(budget) => write!(
                formatter,
                "the isolated worker was killed at its {} ms budget",
                budget.as_millis()
            ),
            Self::Crashed { code, stderr } => {
                let code = code.map_or_else(|| "none".to_string(), |code| code.to_string());
                let detail = stderr.lines().find(|line| !line.trim().is_empty());
                match detail {
                    Some(line) => write!(
                        formatter,
                        "the isolated worker crashed (exit code {code}: {})",
                        line.trim()
                    ),
                    None => write!(formatter, "the isolated worker crashed (exit code {code})"),
                }
            }
            Self::Protocol(error) => {
                write!(
                    formatter,
                    "the isolated worker sent an unusable reply: {error}"
                )
            }
            Self::Job(error) => formatter.write_str(error),
        }
    }
}

// 2. Spawn switch -------------------------------------------------------------
// 서버 진입점 전용 (워커는 호출 금지)
pub fn enable_worker_spawning() -> bool {
    match std::env::current_exe() {
        Ok(exe) => enable_worker_spawning_at(exe),
        Err(_) => false,
    }
}
pub fn enable_worker_spawning_at(exe: PathBuf) -> bool {
    let _ = WORKER_EXE.set(exe);
    WORKER_EXE.get().is_some()
}
pub fn worker_available() -> bool {
    WORKER_EXE.get().is_some()
}

// 워커 위임 작업 수 (예열 기동 제외)
pub fn spawn_count() -> u64 {
    SPAWN_COUNT.load(Ordering::Relaxed)
}

// 예열 워커 인수 작업 수
pub fn count_standby_jobs() -> u64 {
    STANDBY_JOBS.load(Ordering::Relaxed)
}

// 테스트 전용 응답 상한 축소 (이전 값 반환)
#[doc(hidden)]
pub fn set_reply_cap(bytes: u64) -> u64 {
    REPLY_CAP.swap(bytes, Ordering::Relaxed)
}

// 3. Parent side --------------------------------------------------------------
// 단일 JSON 응답 수집 (응답 상한 적용)
pub fn run_in_worker(job: &Value, budget: Duration) -> Result<Value, WorkerError> {
    let reply_cap = REPLY_CAP.load(Ordering::Relaxed);
    let mut stdout = Vec::new();
    run_job(job, budget, &mut |chunk| {
        if stdout.len() as u64 + chunk.len() as u64 > reply_cap {
            return Err(WorkerError::Protocol(format!(
                "reply passed the {reply_cap}-byte cap"
            )));
        }
        stdout.extend_from_slice(chunk);
        Ok(())
    })?;
    let reply: Value = serde_json::from_slice(&stdout)
        .map_err(|error| WorkerError::Protocol(format!("reply is not JSON: {error}")))?;
    open_status(reply)
}

// 봉투 직렬화 후 실행 (stdout 청크는 sink 전달)
fn run_job(
    job: &Value,
    budget: Duration,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), WorkerError>,
) -> Result<(), WorkerError> {
    let Some(exe) = WORKER_EXE.get() else {
        return Err(WorkerError::Unavailable);
    };
    let body = serde_json::to_vec(job)
        .map_err(|error| WorkerError::Protocol(format!("job serialization failed: {error}")))?;
    let budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
    let head = format!("{{\"v\":{PROTOCOL_VERSION},\"budgetMs\":{budget_ms},\"job\":");
    // 한 줄 프레임 (직렬화 JSON에 원문 개행 없음)
    let mut payload = Vec::with_capacity(head.len() + body.len() + 2);
    payload.extend_from_slice(head.as_bytes());
    payload.extend_from_slice(&body);
    payload.extend_from_slice(b"}\n");
    drop(body);
    // 예열 워커 인수 (종료된 워커 폐기)
    let mut standby = STANDBY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(idle) = &mut standby
        && !matches!(idle.try_wait(), Ok(None))
    {
        kill_child(idle);
        standby = None;
    }
    // 예열 보충 (첫 작업 뒤)
    let warm = standby.is_some();
    if warm {
        refill_standby(exe);
    }
    let outcome = run_child(exe, &[WORKER_FLAG], standby, payload, budget, sink);
    if !warm && !matches!(outcome, Err(WorkerError::SpawnFailed(_))) {
        refill_standby(exe);
    }
    outcome
}

// 예열 워커 보충 (동시 1건)
fn refill_standby(exe: &'static Path) {
    if STANDBY_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let refill = thread::Builder::new()
        .name("isolated-standby".to_string())
        .spawn(move || {
            let vacant = STANDBY
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none();
            if vacant && let Ok(idle) = spawn_worker(exe, &[WORKER_FLAG]) {
                *STANDBY.lock().unwrap_or_else(PoisonError::into_inner) = Some(idle);
            }
            STANDBY_PENDING.store(false, Ordering::Release);
        });
    if refill.is_err() {
        STANDBY_PENDING.store(false, Ordering::Release);
    }
}

// 워커 기동 (stdio 3종 파이프)
fn spawn_worker(exe: &Path, args: &[&str]) -> Result<Child, WorkerError> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 콘솔 미할당 (conhost 기동 생략)
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    command
        .spawn()
        .map_err(|error| WorkerError::SpawnFailed(format!("{}: {error}", exe.display())))
}

// 상태 봉투 판정 (버전·ok)
fn open_status(mut reply: Value) -> Result<Value, WorkerError> {
    if reply["v"].as_u64() != Some(PROTOCOL_VERSION) {
        return Err(WorkerError::Protocol(format!(
            "reply protocol version {} does not match {PROTOCOL_VERSION}",
            reply["v"]
        )));
    }
    match reply["ok"].as_bool() {
        Some(true) => Ok(reply.get_mut("result").map(Value::take).unwrap_or_default()),
        Some(false) => Err(WorkerError::Job(
            reply["error"]
                .as_str()
                .unwrap_or("the isolated worker failed without a message")
                .to_string(),
        )),
        None => Err(WorkerError::Protocol("reply has no ok flag".to_string())),
    }
}
enum PipeEvent {
    Stdout(Vec<u8>),
    // 읽기 오류 없이 EOF 도달 여부
    StdoutEnd(bool),
    StderrEnd,
}

// 파이프 중계 (유계 대기열), 기한 초과 kill
fn run_child(
    exe: &Path,
    args: &[&str],
    standby: Option<Child>,
    payload: Vec<u8>,
    budget: Duration,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), WorkerError>,
) -> Result<(), WorkerError> {
    // 표현 불가 기한은 무기한 (Instant 오버플로 없음)
    let deadline = Instant::now().checked_add(budget);
    // 워커 확보 (예열 우선)
    let mut child = match standby {
        Some(idle) => {
            STANDBY_JOBS.fetch_add(1, Ordering::Relaxed);
            idle
        }
        None => spawn_worker(exe, args)?,
    };
    SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
    let (Some(mut stdin), Some(mut stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        kill_child(&mut child);
        return Err(WorkerError::Protocol(
            "worker pipes were not captured".to_string(),
        ));
    };

    // 1. 작업 기록·출력 중계 스레드 (stdin은 종료 판정까지 유지)
    let (event_tx, events) = mpsc::sync_channel::<PipeEvent>(STREAM_QUEUE);
    let stderr_tx = event_tx.clone();
    let threads = thread::Builder::new()
        .spawn(move || {
            let _ = stdin.write_all(&payload).and_then(|()| stdin.flush());
            stdin
        })
        .and_then(|writer| {
            let stdout_reader = thread::Builder::new().spawn(move || {
                let read_ok = loop {
                    let mut chunk = vec![0; STREAM_CHUNK_BYTES];
                    match stdout.read(&mut chunk) {
                        Ok(0) => break true,
                        Ok(read) => {
                            chunk.truncate(read);
                            // 수신 측 종료 (kill 경로)
                            if event_tx.send(PipeEvent::Stdout(chunk)).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break false,
                    }
                };
                let _ = event_tx.send(PipeEvent::StdoutEnd(read_ok));
            })?;
            Ok((writer, stdout_reader))
        })
        .and_then(|(writer, stdout_reader)| {
            let stderr_reader = thread::Builder::new().spawn(move || {
                let read = read_capped(stderr, MAX_STDERR_BYTES, true);
                let _ = stderr_tx.send(PipeEvent::StderrEnd);
                read
            })?;
            Ok((writer, stdout_reader, stderr_reader))
        });
    let (writer, stdout_reader, stderr_reader) = match threads {
        Ok(threads) => threads,
        Err(error) => {
            kill_child(&mut child);
            return Err(WorkerError::SpawnFailed(format!(
                "could not start a pipe thread: {error}"
            )));
        }
    };

    // 2. 청크 전달, 파이프 종료 또는 기한 대기
    let mut open_pipes = 2usize;
    let mut stdout_ok = true;
    let mut failure = None;
    let mut timed_out = false;
    let mut finished = false;
    while open_pipes > 0 {
        let remaining = deadline.map_or(Duration::MAX, |limit| {
            limit.saturating_duration_since(Instant::now())
        });
        // 기한 경과: 정상 종료 워커만 잔여 소진
        if remaining.is_zero() && !finished {
            finished = matches!(child.try_wait(), Ok(Some(status)) if status.success());
            if !finished {
                timed_out = true;
                break;
            }
        }
        let event = if finished {
            events
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            events.recv_timeout(remaining)
        };
        match event {
            Ok(PipeEvent::Stdout(chunk)) => {
                if let Err(error) = sink(&chunk) {
                    failure = Some(error);
                    break;
                }
            }
            Ok(PipeEvent::StdoutEnd(read_ok)) => {
                open_pipes -= 1;
                stdout_ok = read_ok;
            }
            Ok(PipeEvent::StderrEnd) => open_pipes -= 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = if timed_out || failure.is_some() {
        kill_child(&mut child);
        None
    } else {
        wait_until(&mut child, deadline)
    };
    // 대기열 해제 (블록된 송신 해제)
    drop(events);
    // 종료 확정 후 stdin 닫기 (워커 고아 감지 신호)
    drop(writer.join());
    let _ = stdout_reader.join();
    let stderr = stderr_reader.join().ok().and_then(Result::ok);

    // 3. 종료 상태 판정
    if let Some(error) = failure {
        return Err(error);
    }
    let Some(status) = status else {
        return Err(WorkerError::TimedOut(budget));
    };
    if !status.success() {
        let stderr = stderr
            .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        return Err(WorkerError::Crashed {
            code: status.code(),
            stderr,
        });
    }
    if !stdout_ok {
        return Err(WorkerError::Protocol(
            "worker stdout could not be read".to_string(),
        ));
    }
    Ok(())
}

// 파이프 닫힘 후 종료 대기 (기한 내)
fn wait_until(child: &mut Child, deadline: Option<Instant>) -> Option<ExitStatus> {
    #[cfg(windows)]
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    }
    // 점증 폴링 간격 (비Windows)
    #[cfg(not(windows))]
    let mut pause = Duration::from_micros(50);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => break,
        }
        let remaining = deadline.map(|limit| limit.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|left| left.is_zero()) {
            break;
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            const INFINITE: u32 = u32::MAX;
            const WAIT_FAILED: u32 = u32::MAX;
            // 잔여 기한 올림 (INFINITE 미만)
            let millis = remaining.map_or(INFINITE, |left| {
                u32::try_from(left.as_millis().saturating_add(1))
                    .unwrap_or(INFINITE - 1)
                    .min(INFINITE - 1)
            });
            // SAFETY: Child 소유 프로세스 핸들, 호출 동안 유효
            let waited = unsafe { WaitForSingleObject(child.as_raw_handle(), millis) };
            // 대기 실패 시 폴링 후퇴
            if waited == WAIT_FAILED {
                thread::sleep(Duration::from_millis(2));
            }
        }
        #[cfg(not(windows))]
        {
            thread::sleep(remaining.map_or(pause, |left| left.min(pause)));
            pause = (pause * 2).min(Duration::from_millis(2));
        }
    }
    kill_child(child);
    None
}
fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// 상한까지 보관 (drain 시 초과분 폐기)
fn read_capped<R: Read>(mut pipe: R, cap: u64, drain: bool) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    pipe.by_ref().take(cap + 1).read_to_end(&mut bytes)?;
    let capped = bytes.len() as u64 > cap;
    bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    if capped && drain {
        io::copy(&mut pipe, &mut io::sink())?;
        return Ok((bytes, false));
    }
    Ok((bytes, capped))
}

// 4. Worker side --------------------------------------------------------------
// 워커 진입점: 작업 1건 실행 후 종료 코드 반환
pub fn run_worker_process(search_job: fn(&Value) -> Result<Value, String>) -> i32 {
    let mut stdin = io::BufReader::new(io::stdin());
    let mut stdout = io::BufWriter::with_capacity(STREAM_CHUNK_BYTES, io::stdout().lock());
    let reply = match read_job(&mut stdin) {
        Ok(envelope) => {
            start_watchdog(&envelope);
            watch_parent(stdin);
            dispatch_job(&envelope, search_job, &mut stdout)
        }
        Err(error) => Ok(Err(error)),
    };
    // 기록 실패 = 부모 이탈
    let Ok(reply) = reply else {
        return EXIT_REPLY_FAILED;
    };
    let body = match reply {
        Ok(result) => json!({ "v": PROTOCOL_VERSION, "ok": true, "result": result }),
        Err(error) => json!({ "v": PROTOCOL_VERSION, "ok": false, "error": error }),
    };
    let written = serde_json::to_writer(&mut stdout, &body)
        .map_err(io::Error::from)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush());
    if written.is_err() {
        return EXIT_REPLY_FAILED;
    }
    0
}

// 첫 줄 = 작업 봉투 (이후 stdin은 부모 생존 신호)
fn read_job(stdin: &mut impl BufRead) -> Result<Value, String> {
    let mut bytes = Vec::new();
    stdin
        .take(MAX_JOB_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| format!("failed to read the job: {error}"))?;
    if bytes.len() as u64 > MAX_JOB_BYTES {
        return Err(format!("job passed the {MAX_JOB_BYTES}-byte cap"));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("job is not JSON: {error}"))
}

// 예산 + 유예 경과 시 자가 종료
fn start_watchdog(envelope: &Value) {
    let Some(budget_ms) = envelope["budgetMs"].as_u64() else {
        return;
    };
    let limit = Duration::from_millis(budget_ms).saturating_add(WATCHDOG_GRACE);
    let _ = thread::Builder::new()
        .name("isolated-watchdog".to_string())
        .spawn(move || {
            thread::sleep(limit);
            let _ = writeln!(
                io::stderr(),
                "rust-fs-mcp isolated worker passed its budget by {} ms; exiting",
                WATCHDOG_GRACE.as_millis()
            );
            std::process::exit(EXIT_WATCHDOG);
        });
}

// stdin EOF = 부모 종료, 즉시 자가 종료
fn watch_parent(mut stdin: io::BufReader<io::Stdin>) {
    let _ = thread::Builder::new()
        .name("isolated-parent-watch".to_string())
        .spawn(move || {
            let _ = io::copy(&mut stdin, &mut io::sink());
            std::process::exit(EXIT_ORPHANED);
        });
}

// 이중 Result (기록 실패·작업 오류)
fn dispatch_job(
    envelope: &Value,
    search_job: fn(&Value) -> Result<Value, String>,
    out: &mut dyn Write,
) -> io::Result<Result<Value, String>> {
    if envelope["v"].as_u64() != Some(PROTOCOL_VERSION) {
        return Ok(Err(format!(
            "job protocol version {} does not match {PROTOCOL_VERSION}",
            envelope["v"]
        )));
    }
    let job = &envelope["job"];
    match job["op"].as_str() {
        Some("fancy-spans") => write_fancy_spans(job, out),
        Some("fs-search") => Ok(search_job(job)),
        other => Ok(Err(format!("unknown isolated job op {other:?}"))),
    }
}

// 5. Fancy-regex spans --------------------------------------------------------
// file-replace 컴파일 옵션 (부모·워커 공용)
pub(crate) struct FancyPattern<'p> {
    pub(crate) source: &'p str,
    pub(crate) ignore_case: bool,
    pub(crate) dot_all: bool,
    pub(crate) crlf: bool,
}
pub(crate) fn build_fancy_regex(
    pattern: &FancyPattern<'_>,
) -> Result<fancy_regex::Regex, fancy_regex::Error> {
    fancy_regex::RegexBuilder::new(pattern.source)
        .multi_line(true)
        .case_insensitive(pattern.ignore_case)
        .dot_matches_new_line(pattern.dot_all)
        .crlf(pattern.crlf)
        .backtrack_limit(FANCY_BACKTRACK_LIMIT)
        .build()
}

// 매치당 한 줄: 직전 끝 대비 시작,길이 + 그룹당 ,매치 시작 대비,길이 또는 ,-
pub(crate) fn write_fancy_spans(
    job: &Value,
    out: &mut dyn Write,
) -> io::Result<Result<Value, String>> {
    let (Some(source), Some(text)) = (job["pattern"].as_str(), job["text"].as_str()) else {
        return Ok(Err(
            "fancy-spans job needs pattern and text strings".to_string()
        ));
    };
    let built = build_fancy_regex(&FancyPattern {
        source,
        ignore_case: job["ignoreCase"].as_bool().unwrap_or(false),
        dot_all: job["dotAll"].as_bool().unwrap_or(false),
        crlf: job["crlf"].as_bool().unwrap_or(false),
    });
    let regex = match built {
        Ok(regex) => regex,
        Err(error) => return Ok(Err(format!("Invalid regex pattern (fancy-regex): {error}"))),
    };
    let limit = job["limit"].as_u64().unwrap_or(0);
    let groups = if job["groups"].as_bool().unwrap_or(false) {
        regex.captures_len().saturating_sub(1)
    } else {
        0
    };

    let mut record = String::new();
    let mut count = 0u64;
    let mut previous_end = 0i64;
    for captures in regex.captures_iter(text) {
        let captures = match captures {
            Ok(captures) => captures,
            Err(error) => {
                return Ok(Err(format!(
                    "fancy-regex stopped: {error}; simplify the pattern"
                )));
            }
        };
        let Some(whole) = captures.get(0) else {
            continue;
        };
        let start = whole.start() as i64;
        record.clear();
        let _ = write!(
            record,
            "{},{}",
            start - previous_end,
            whole.end() as i64 - start
        );
        for group in 1..=groups {
            match captures.get(group) {
                Some(found) => {
                    let _ = write!(
                        record,
                        ",{},{}",
                        found.start() as i64 - start,
                        found.end() as i64 - found.start() as i64
                    );
                }
                None => record.push_str(",-"),
            }
        }
        record.push('\n');
        // 버퍼 가득 시 부모로 즉시 전달
        out.write_all(record.as_bytes())?;
        previous_end = whole.end() as i64;
        count += 1;
        if limit > 0 && count >= limit {
            break;
        }
    }
    Ok(Ok(json!({ "count": count })))
}

// 6. Span stream --------------------------------------------------------------
// 복원 매치 (텍스트 절대 바이트 오프셋)
pub(crate) struct SpanMatch<'g> {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) groups: &'g [Option<(usize, usize)>],
}

// 워커 기록 즉시 on_match 전달 (스팬 버퍼 없음)
pub(crate) fn stream_fancy_spans<F>(
    job: &Value,
    groups: usize,
    budget: Duration,
    on_match: F,
) -> Result<u64, WorkerError>
where
    F: FnMut(&SpanMatch<'_>) -> Result<bool, String>,
{
    let mut stream = SpanStream::new(groups, on_match);
    run_job(job, budget, &mut |chunk| stream.feed(chunk))?;
    stream.finish()
}

// 줄 단위 스팬 복원기 (신뢰 불가 입력)
pub(crate) struct SpanStream<F> {
    groups: usize,
    on_match: F,
    pending: Vec<u8>,
    captured: Vec<Option<(usize, usize)>>,
    previous_end: i64,
    seen: u64,
    // 소비자 한도 도달
    stopped: bool,
    status: Option<Result<Value, WorkerError>>,
}
impl<F> SpanStream<F>
where
    F: FnMut(&SpanMatch<'_>) -> Result<bool, String>,
{
    pub(crate) fn new(groups: usize, on_match: F) -> Self {
        Self {
            groups,
            on_match,
            pending: Vec::new(),
            captured: Vec::with_capacity(groups),
            previous_end: 0,
            seen: 0,
            stopped: false,
            status: None,
        }
    }
    // 완성 줄 처리, 미완성 줄 보관
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<(), WorkerError> {
        let mut rest = chunk;
        while let Some(newline) = memchr::memchr(b'\n', rest) {
            let (line, tail) = rest.split_at(newline);
            rest = tail.get(1..).unwrap_or_default();
            if self.pending.is_empty() {
                self.take_line(line)?;
            } else {
                let mut joined = std::mem::take(&mut self.pending);
                joined.extend_from_slice(line);
                self.take_line(&joined)?;
                joined.clear();
                self.pending = joined;
            }
        }
        if self.pending.len() + rest.len() > MAX_LINE_BYTES {
            return Err(WorkerError::Protocol(format!(
                "a reply line passed the {MAX_LINE_BYTES}-byte cap"
            )));
        }
        self.pending.extend_from_slice(rest);
        Ok(())
    }
    fn take_line(&mut self, line: &[u8]) -> Result<(), WorkerError> {
        if self.status.is_some() {
            return Err(WorkerError::Protocol(
                "data followed the final status line".to_string(),
            ));
        }
        // 최종 상태 줄
        if line.first() == Some(&b'{') {
            let status: Value = serde_json::from_slice(line).map_err(|error| {
                WorkerError::Protocol(format!("final status line is not JSON: {error}"))
            })?;
            self.status = Some(open_status(status));
            return Ok(());
        }
        let record = self.seen + 1;
        let malformed = |detail: &str| {
            WorkerError::Protocol(format!("unusable match span record {record}: {detail}"))
        };
        if self.stopped {
            return Err(malformed("it passed the replacement limit"));
        }

        // 1. 상대 오프셋 판독 ("-"는 미참여 그룹)
        let mut tokens = line.split(|byte| *byte == b',');
        let mut next = || match tokens.next() {
            Some(b"-") => Ok(None),
            Some(token) => std::str::from_utf8(token)
                .ok()
                .and_then(|token| token.parse::<i64>().ok())
                .map(Some)
                .ok_or_else(|| malformed("bad number")),
            None => Err(malformed("too few numbers")),
        };
        let locate = |base: i64, offset: Option<i64>| {
            offset
                .and_then(|offset| base.checked_add(offset))
                .and_then(|position| usize::try_from(position).ok())
                .ok_or_else(|| malformed("missing or negative offset"))
        };
        let start = locate(self.previous_end, next()?)?;
        let end = locate(start as i64, next()?)?;
        self.captured.clear();
        for _ in 0..self.groups {
            let Some(offset) = next()? else {
                self.captured.push(None);
                continue;
            };
            let group_start = locate(start as i64, Some(offset))?;
            let group_end = locate(group_start as i64, next()?)?;
            self.captured.push(Some((group_start, group_end)));
        }
        if tokens.next().is_some() {
            return Err(malformed("too many numbers"));
        }

        // 2. 소비자 전달 (겹침·범위 검증은 소비자)
        let more = (self.on_match)(&SpanMatch {
            start,
            end,
            groups: &self.captured,
        })
        .map_err(WorkerError::Job)?;
        self.stopped = !more;
        self.previous_end = end as i64;
        self.seen = record;
        Ok(())
    }
    // 최종 상태 필수, 개수 일치 확인
    pub(crate) fn finish(mut self) -> Result<u64, WorkerError> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.take_line(&line)?;
        }
        let Some(status) = self.status.take() else {
            return Err(WorkerError::Protocol(
                "reply ended before its final status line".to_string(),
            ));
        };
        let count = status?["count"]
            .as_u64()
            .ok_or_else(|| WorkerError::Protocol("final status has no match count".to_string()))?;
        if count != self.seen {
            return Err(WorkerError::Protocol(format!(
                "the worker counted {count} match(es) but sent {} span record(s)",
                self.seen
            )));
        }
        Ok(count)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> (PathBuf, Vec<&str>) {
        if cfg!(windows) {
            (PathBuf::from("cmd"), vec!["/C", script])
        } else {
            (PathBuf::from("sh"), vec!["-c", script])
        }
    }
    // 수집형 실행 (run_in_worker 상한 동일)
    fn collect(
        exe: &Path,
        args: &[&str],
        payload: Vec<u8>,
        budget: Duration,
        cap: usize,
    ) -> Result<Vec<u8>, WorkerError> {
        let mut stdout = Vec::new();
        run_child(exe, args, None, payload, budget, &mut |chunk| {
            if stdout.len() + chunk.len() > cap {
                return Err(WorkerError::Protocol(format!(
                    "reply passed the {cap}-byte cap"
                )));
            }
            stdout.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(stdout)
    }
    fn endless_printer() -> (PathBuf, Vec<&'static str>) {
        if cfg!(windows) {
            shell("for /L %i in (0,0,1) do @echo 0,1")
        } else {
            shell("while :; do echo 0,1; done")
        }
    }
    // 워커 출력 전체 (기록 + 상태 줄)
    fn worker_output(job: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        let reply = write_fancy_spans(job, &mut out).unwrap();
        let body = match reply {
            Ok(result) => json!({ "v": PROTOCOL_VERSION, "ok": true, "result": result }),
            Err(error) => json!({ "v": PROTOCOL_VERSION, "ok": false, "error": error }),
        };
        out.extend_from_slice(body.to_string().as_bytes());
        out.push(b'\n');
        out
    }
    // 청크 분할 복원 (매치 목록)
    type Decoded = Vec<(usize, usize, Vec<Option<(usize, usize)>>)>;
    fn decode(
        output: &[u8],
        groups: usize,
        chunk: usize,
        limit: usize,
    ) -> Result<Decoded, WorkerError> {
        let mut found: Decoded = Vec::new();
        let mut stream = SpanStream::new(groups, |span: &SpanMatch<'_>| {
            found.push((span.start, span.end, span.groups.to_vec()));
            Ok(limit == 0 || found.len() < limit)
        });
        for piece in output.chunks(chunk) {
            stream.feed(piece)?;
        }
        stream.finish()?;
        Ok(found)
    }

    #[test]
    fn spawning_is_unavailable_until_enabled() {
        assert!(!worker_available());
        let job = json!({ "op": "fancy-spans" });
        assert_eq!(
            run_in_worker(&job, Duration::from_secs(1)),
            Err(WorkerError::Unavailable)
        );
        let streamed = stream_fancy_spans(&job, 0, Duration::from_secs(1), |_| Ok(true));
        assert_eq!(streamed, Err(WorkerError::Unavailable));
    }
    #[test]
    fn child_past_its_budget_is_killed() {
        let (exe, args) = if cfg!(windows) {
            (PathBuf::from("ping"), vec!["-n", "30", "127.0.0.1"])
        } else {
            (PathBuf::from("sleep"), vec!["30"])
        };
        let started = Instant::now();
        let outcome = collect(
            &exe,
            &args,
            b"job".to_vec(),
            Duration::from_millis(300),
            1024,
        );
        assert_eq!(
            outcome,
            Err(WorkerError::TimedOut(Duration::from_millis(300)))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn standby_child_takes_the_job_and_keeps_its_budget() {
        // 1. 대기 자식에 작업 줄 전달
        let (exe, args) = if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec!["/V:ON", "/C", "set /p line=&echo !line!"],
            )
        } else {
            shell("read line; echo \"$line\"")
        };
        let idle = spawn_worker(&exe, &args).unwrap();
        let served = count_standby_jobs();
        let unused = Path::new("rust-fs-mcp-no-such-worker");
        let mut stdout = Vec::new();
        let outcome = run_child(
            unused,
            &[],
            Some(idle),
            b"job\n".to_vec(),
            Duration::from_secs(10),
            &mut |chunk| {
                stdout.extend_from_slice(chunk);
                Ok(())
            },
        );
        assert_eq!(outcome, Ok(()));
        assert!(String::from_utf8_lossy(&stdout).starts_with("job"));
        assert_eq!(count_standby_jobs(), served + 1);

        // 2. 예산 초과 대기 자식 kill
        let (exe, args) = if cfg!(windows) {
            (PathBuf::from("ping"), vec!["-n", "30", "127.0.0.1"])
        } else {
            (PathBuf::from("sleep"), vec!["30"])
        };
        let idle = spawn_worker(&exe, &args).unwrap();
        let started = Instant::now();
        let outcome = run_child(
            unused,
            &[],
            Some(idle),
            b"job\n".to_vec(),
            Duration::from_millis(300),
            &mut |_| Ok(()),
        );
        assert_eq!(
            outcome,
            Err(WorkerError::TimedOut(Duration::from_millis(300)))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(count_standby_jobs(), served + 2);
    }
    #[test]
    fn streaming_child_is_killed_at_its_budget() {
        let (exe, args) = endless_printer();
        let mut received = 0usize;
        let started = Instant::now();
        let outcome = run_child(
            &exe,
            &args,
            None,
            Vec::new(),
            Duration::from_millis(500),
            &mut |chunk| {
                received += chunk.len();
                Ok(())
            },
        );
        assert_eq!(
            outcome,
            Err(WorkerError::TimedOut(Duration::from_millis(500)))
        );
        assert!(received > 0);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn sink_error_kills_a_streaming_child() {
        let (exe, args) = endless_printer();
        let started = Instant::now();
        let outcome = run_child(
            &exe,
            &args,
            None,
            Vec::new(),
            Duration::from_secs(30),
            &mut |_| Err(WorkerError::Job("output cap".to_string())),
        );
        assert_eq!(outcome, Err(WorkerError::Job("output cap".to_string())));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn finished_child_survives_a_slow_sink_past_the_deadline() {
        let (exe, args) = shell("echo ok");
        let mut stdout = Vec::new();
        let outcome = run_child(
            &exe,
            &args,
            None,
            Vec::new(),
            Duration::from_millis(1000),
            &mut |chunk| {
                thread::sleep(Duration::from_millis(1500));
                stdout.extend_from_slice(chunk);
                Ok(())
            },
        );
        assert_eq!(outcome, Ok(()));
        assert!(String::from_utf8_lossy(&stdout).starts_with("ok"));
    }
    #[test]
    fn crash_reports_exit_code_and_stderr() {
        let (exe, args) = if cfg!(windows) {
            shell("echo boom 1>&2 & exit 7")
        } else {
            shell("echo boom >&2; exit 7")
        };
        let outcome = collect(&exe, &args, Vec::new(), Duration::from_secs(10), 1024);
        let Err(error) = outcome else {
            panic!("crash was not reported: {outcome:?}");
        };
        let WorkerError::Crashed { code, stderr } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*code, Some(7));
        assert!(stderr.contains("boom"), "{stderr}");
        assert!(error.to_string().contains("exit code 7: boom"), "{error}");
    }
    #[test]
    fn missing_executable_and_oversized_reply_are_errors() {
        let missing = collect(
            Path::new("rust-fs-mcp-no-such-worker"),
            &[],
            Vec::new(),
            Duration::from_secs(5),
            1024,
        );
        assert!(
            matches!(missing, Err(WorkerError::SpawnFailed(_))),
            "{missing:?}"
        );
        let (exe, args) = shell("echo 0123456789");
        let oversized = collect(&exe, &args, Vec::new(), Duration::from_secs(10), 4);
        assert_eq!(
            oversized,
            Err(WorkerError::Protocol(
                "reply passed the 4-byte cap".to_string()
            ))
        );
        let (exe, args) = shell("echo ok");
        let fits = collect(&exe, &args, Vec::new(), Duration::from_secs(10), 1024).unwrap();
        assert!(String::from_utf8_lossy(&fits).starts_with("ok"));
    }
    #[test]
    fn unrepresentable_budget_runs_without_deadline() {
        let (exe, args) = shell("echo ok");
        let fits = collect(&exe, &args, b"job".to_vec(), Duration::MAX, 1024).unwrap();
        assert!(String::from_utf8_lossy(&fits).starts_with("ok"));
    }
    #[test]
    fn stderr_past_its_cap_is_drained() {
        let mut flood = vec![b'e'; MAX_STDERR_BYTES as usize * 3];
        flood.extend_from_slice(b"tail");
        let (kept, capped) = read_capped(io::Cursor::new(flood), MAX_STDERR_BYTES, true).unwrap();
        assert!(!capped);
        assert_eq!(kept.len() as u64, MAX_STDERR_BYTES);
        let (kept, capped) = read_capped(io::Cursor::new(b"abcdef".to_vec()), 4, false).unwrap();
        assert_eq!((kept.as_slice(), capped), (&b"abcd"[..], true));
    }
    #[test]
    fn job_is_the_first_line_and_rest_stays_unread() {
        let mut stdin = io::Cursor::new(b"{\"v\":1,\"job\":{\"text\":\"a\\nb\"}}\nrest".to_vec());
        let envelope = read_job(&mut stdin).unwrap();
        assert_eq!(envelope["job"]["text"], "a\nb");
        let mut rest = String::new();
        stdin.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest");
        let broken = read_job(&mut io::Cursor::new(b"{\"v\":\n1}".to_vec()));
        assert!(broken.unwrap_err().contains("not JSON"));
    }
    #[test]
    fn jobs_check_version_and_op() {
        let search = |_: &Value| Ok(json!("searched"));
        let mut out = Vec::new();
        let stale = json!({ "v": 1, "job": { "op": "fs-search" } });
        let rejected = dispatch_job(&stale, search, &mut out).unwrap();
        assert!(rejected.unwrap_err().contains("version"));
        let unknown = json!({ "v": PROTOCOL_VERSION, "job": { "op": "nope" } });
        let rejected = dispatch_job(&unknown, search, &mut out).unwrap();
        assert!(rejected.unwrap_err().contains("unknown"));
        let routed = json!({ "v": PROTOCOL_VERSION, "job": { "op": "fs-search" } });
        let routed = dispatch_job(&routed, search, &mut out).unwrap();
        assert_eq!(routed, Ok(json!("searched")));
        assert!(out.is_empty());
    }
    #[test]
    fn fancy_spans_stream_offsets_groups_and_limit() {
        let job = json!({
            "pattern": r"(a)?(b)(?=c)",
            "text": "abc bc bd",
            "groups": true
        });
        let output = worker_output(&job);
        let text = String::from_utf8(output.clone()).unwrap();
        assert_eq!(
            text,
            "0,2,0,1,1,1\n2,1,-,0,1\n{\"ok\":true,\"result\":{\"count\":2},\"v\":2}\n"
        );
        let whole = decode(&output, 2, output.len(), 0).unwrap();
        assert_eq!(
            whole,
            vec![
                (0, 2, vec![Some((0, 1)), Some((1, 2))]),
                (4, 5, vec![None, Some((4, 5))])
            ]
        );
        // 바이트 단위 분할 동일
        assert_eq!(decode(&output, 2, 1, 0).unwrap(), whole);
        let limited = worker_output(&json!({
            "pattern": r"b(?=c)", "text": "bc bc bc", "limit": 2
        }));
        assert_eq!(
            decode(&limited, 0, 3, 2).unwrap(),
            vec![(0, 1, vec![]), (3, 4, vec![])]
        );
        let invalid = worker_output(&json!({ "pattern": "(?=", "text": "" }));
        let invalid = decode(&invalid, 0, 64, 0).unwrap_err().to_string();
        assert!(invalid.contains("fancy-regex"), "{invalid}");
        let stopped =
            worker_output(&json!({ "pattern": r"(a|ab|b)*\1(?=c)", "text": "ab".repeat(30) }));
        let stopped = decode(&stopped, 1, 64, 0).unwrap_err().to_string();
        assert!(stopped.contains("fancy-regex stopped"), "{stopped}");
    }
    #[test]
    fn untrusted_span_streams_are_rejected() {
        let done = |count: u64| {
            format!("{{\"v\":{PROTOCOL_VERSION},\"ok\":true,\"result\":{{\"count\":{count}}}}}\n")
        };
        for (output, groups, detail) in [
            (format!("0,1\n{}", done(2)), 0, "counted 2"),
            ("0,1\n".to_string(), 0, "before its final status"),
            (
                format!("0,1\n{}0,1\n", done(1)),
                0,
                "followed the final status",
            ),
            (format!("0,1,5\n{}", done(1)), 0, "too many numbers"),
            (format!("0\n{}", done(1)), 0, "too few numbers"),
            (format!("0,1,-\n{}", done(1)), 0, "too many numbers"),
            (format!("0,1\n{}", done(1)), 1, "too few numbers"),
            (format!("x,1\n{}", done(1)), 0, "bad number"),
            (format!("-9,1\n{}", done(1)), 0, "negative offset"),
            (format!("-,1\n{}", done(1)), 0, "negative offset"),
            (format!("0,1,-5,1\n{}", done(1)), 1, "negative offset"),
            (
                format!("9223372036854775807,9\n{}", done(1)),
                0,
                "negative offset",
            ),
            (format!("0,1\n0,1\n{}", done(2)), 0, "replacement limit"),
            ("{\"v\":1,\"ok\":true}\n".to_string(), 0, "version 1"),
            ("{\"v\":2,\"ok\":tr".to_string(), 0, "not JSON"),
            (format!("{{\"v\":{PROTOCOL_VERSION}}}\n"), 0, "no ok flag"),
            (
                format!("{{\"v\":{PROTOCOL_VERSION},\"ok\":true}}\n"),
                0,
                "no match count",
            ),
        ] {
            let error = decode(output.as_bytes(), groups, 3, 1)
                .unwrap_err()
                .to_string();
            assert!(error.contains(detail), "{output:?}: {error}");
        }
        let empty = decode(done(0).as_bytes(), 1, 5, 0).unwrap();
        assert!(empty.is_empty());
        let mut stream = SpanStream::new(0, |_: &SpanMatch<'_>| Ok(true));
        let unterminated = vec![b'0'; MAX_LINE_BYTES + 1];
        let capped = stream.feed(&unterminated).unwrap_err().to_string();
        assert!(capped.contains("byte cap"), "{capped}");
        let mut rejecting = SpanStream::new(0, |_: &SpanMatch<'_>| Err("cap".to_string()));
        assert_eq!(
            rejecting.feed(b"0,1\n"),
            Err(WorkerError::Job("cap".to_string()))
        );
    }
}
