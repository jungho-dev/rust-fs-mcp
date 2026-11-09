//! external.rs
//! core::external
//!
//! Wrapper that spawns external CLIs (ripgrep, fd, git, ast-grep) resolved from PATH and collects stdout / stderr.
//! Uses reader threads and an mpsc channel to handle EOF and timeout in a single loop.
//! ast-grep resolves to an absolute path (RUST_FS_MCP_AST_GREP, the server folder on Windows, PATH, or an npm @ast-grep/cli install) and streams stdout by line.
//! On Windows the ast-grep child runs in a kill-on-close Job Object, so a timeout also stops the processes it launched.
//!

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalTool {
    Git,
    AstGrep,
}
impl ExternalTool {
    // Command name resolved from PATH. The system PATH binary is invoked directly.
    fn command_name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::AstGrep => "ast-grep",
        }
    }
    pub fn backend_name(self) -> &'static str {
        match self {
            Self::Git => "path-git",
            Self::AstGrep => "path-ast-grep",
        }
    }
    // External tools are resolved from PATH. git caches a de-shuttled path, ast-grep its resolved binary.
    fn resolve_command(self) -> Result<OsString, String> {
        match self {
            Self::Git => git_command().map(OsString::from),
            Self::AstGrep => ast_grep_program().map(|program| program.as_os_str().to_owned()),
        }
    }
}
// git.exe 해석 캐시: Git-for-Windows의 cmd\git.exe는 mingw64\bin\git.exe를 다시 실행하는
// 셔틀이라 호출당 ~13ms를 더 쓴다(실측 77ms vs 64ms). PATH에서 찾은 경로가 셔틀이면
// 실제 바이너리로 1회 치환해 캐시한다. 상대 PATH 항목은 저장소 cwd 기준 실행이 되므로 제외한다.
static GIT_COMMAND: OnceLock<Option<String>> = OnceLock::new();
fn git_command() -> Result<&'static str, String> {
    GIT_COMMAND
        .get_or_init(resolve_git)
        .as_deref()
        .ok_or_else(|| {
            "Failed to start path-git: git was not found in any absolute PATH directory. Ensure 'git' is installed and on PATH.".to_string()
        })
}
fn resolve_git() -> Option<String> {
    let found = find_on_path("git")?;
    if let Some(parent) = found.parent()
        && parent.file_name().and_then(|name| name.to_str()) == Some("cmd")
        && let Some(root) = parent.parent()
    {
        let direct =
            root.join("mingw64")
                .join("bin")
                .join(if cfg!(windows) { "git.exe" } else { "git" });
        if direct.is_file() {
            return Some(direct.to_string_lossy().into_owned());
        }
    }
    Some(found.to_string_lossy().into_owned())
}
// PATH 절대 경로 항목만 탐색 (빈·상대 항목 제외)
fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in_dirs(&std::env::var_os("PATH")?, name)
}
fn find_in_dirs(paths: &OsStr, name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    for dir in std::env::split_paths(paths) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
// ast-grep 실행 파일 지정 환경변수
pub const AST_GREP_ENV: &str = "RUST_FS_MCP_AST_GREP";
// ast-grep 미설치 안내 (Linux sg는 무관 명령)
pub const AST_GREP_MISSING: &str = "ast-grep CLI not found (checked RUST_FS_MCP_AST_GREP, ast-grep beside the rust-fs-mcp executable on Windows, ast-grep on PATH, and the npm @ast-grep/cli package beside an ast-grep.cmd shim); install it (cargo install ast-grep --locked, npm i -g @ast-grep/cli, brew install ast-grep, or a release binary from https://github.com/ast-grep/ast-grep/releases) or set RUST_FS_MCP_AST_GREP to the ast-grep executable, then retry";
// npm 플랫폼 패키지 (postinstall 링크 누락 대비)
const NPM_PLATFORM_PACKAGES: [&str; 3] = [
    "cli-win32-x64-msvc",
    "cli-win32-arm64-msvc",
    "cli-win32-ia32-msvc",
];
// ast-grep 경로 캐시 (성공만 저장)
static AST_GREP_PROGRAM: OnceLock<PathBuf> = OnceLock::new();
pub(crate) fn ast_grep_program() -> Result<&'static Path, String> {
    if let Some(program) = AST_GREP_PROGRAM.get() {
        return Ok(program);
    }
    let cwd = std::env::current_dir().ok();
    let exe = std::env::current_exe().ok();
    let search_path = std_search_path(std::env::var_os("PATH"));
    let resolved = resolve_ast_grep(
        search_path.as_deref(),
        std::env::var_os(AST_GREP_ENV).as_deref(),
        cwd.as_deref(),
        exe.as_deref().and_then(Path::parent),
        &|candidate| candidate.is_file(),
    )?;
    Ok(AST_GREP_PROGRAM.get_or_init(|| resolved))
}
// std Command 의 Windows 탐색 순서 보존 (System32, Windows 폴더를 PATH 앞에)
fn std_search_path(path_var: Option<OsString>) -> Option<OsString> {
    if !cfg!(windows) {
        return path_var;
    }
    let Some(root) = std::env::var_os("SystemRoot").map(PathBuf::from) else {
        return path_var;
    };
    let mut dirs = vec![root.join("System32"), root];
    if let Some(path_var) = &path_var {
        dirs.extend(std::env::split_paths(path_var));
    }
    std::env::join_paths(dirs).ok().or(path_var)
}
// 환경변수, 서버 폴더(Windows), PATH, npm 네이티브 바이너리 순 (sg 제외, 절대 경로만)
fn resolve_ast_grep(
    path_var: Option<&OsStr>,
    explicit: Option<&OsStr>,
    cwd: Option<&Path>,
    exe_dir: Option<&Path>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let binary = if cfg!(windows) {
        "ast-grep.exe"
    } else {
        "ast-grep"
    };
    let mut shim_dirs: Vec<PathBuf> = Vec::new();
    let mut wrapper: Option<PathBuf> = None;
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        // 1. 지정값 (자식 cwd 무관 절대 경로)
        let given = Path::new(explicit);
        let program = match cwd {
            Some(cwd) if given.is_relative() => cwd.join(given),
            _ => given.to_path_buf(),
        };
        if !program.is_absolute() {
            return Err(format!(
                "{AST_GREP_ENV} is set to the relative path {}, which cannot be resolved against the server working directory; set an absolute path",
                given.display()
            ));
        }
        if !is_file(&program) {
            return Err(format!(
                "{AST_GREP_ENV} is set to {}, which is not an existing file; point it at the ast-grep executable or unset it",
                program.display()
            ));
        }
        // Windows 래퍼 스크립트는 옆 npm 패키지로 치환
        let script = cfg!(windows)
            && program
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|ext| {
                    ["cmd", "bat", "ps1"]
                        .iter()
                        .any(|wrapper| ext.eq_ignore_ascii_case(wrapper))
                });
        if !script {
            return Ok(program);
        }
        shim_dirs.extend(program.parent().map(Path::to_path_buf));
        wrapper = Some(program);
    } else {
        // 2. 서버 실행 파일 폴더 (std Command의 Windows 첫 탐색 위치)
        if cfg!(windows)
            && let Some(dir) = exe_dir.filter(|dir| dir.is_absolute())
            && is_file(&dir.join(binary))
        {
            return Ok(dir.join(binary));
        }

        // 3. PATH 절대 경로 항목 (빈·상대 항목 제외)
        let dirs: Vec<PathBuf> = path_var
            .map(|paths| {
                std::env::split_paths(paths)
                    .filter(|dir| dir.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(found) = dirs
            .iter()
            .map(|dir| dir.join(binary))
            .find(|candidate| is_file(candidate))
        {
            return Ok(found);
        }
        // npm 래퍼만 있는 PATH 폴더 (Windows)
        if cfg!(windows) {
            shim_dirs = dirs
                .into_iter()
                .filter(|dir| is_file(&dir.join("ast-grep.cmd")))
                .collect();
        }
    }

    // 4. 래퍼 폴더의 npm 네이티브 바이너리
    for dir in &shim_dirs {
        let mut module_roots = vec![dir.join("node_modules")];
        // 로컬 node_modules/.bin 래퍼
        if dir.file_name() == Some(OsStr::new(".bin"))
            && let Some(modules) = dir.parent()
        {
            module_roots.push(modules.to_path_buf());
        }
        for modules in &module_roots {
            let cli = modules.join("@ast-grep").join("cli");
            let mut candidates = vec![cli.join(binary)];
            for package in NPM_PLATFORM_PACKAGES {
                let nested = cli.join("node_modules").join("@ast-grep").join(package);
                candidates.push(nested.join(binary));
                candidates.push(modules.join("@ast-grep").join(package).join(binary));
            }
            if let Some(found) = candidates.into_iter().find(|candidate| is_file(candidate)) {
                return Ok(found);
            }
        }
    }
    match wrapper {
        Some(wrapper) => Err(format!(
            "{AST_GREP_ENV} is set to {}, a script wrapper rather than the native ast-grep executable, and no npm @ast-grep/cli binary was found beside it; a timeout could not stop the process it launches, so point {AST_GREP_ENV} at ast-grep.exe (for an npm install: <npm prefix>\\node_modules\\@ast-grep\\cli\\ast-grep.exe)",
            wrapper.display()
        )),
        None => Err(AST_GREP_MISSING.to_string()),
    }
}
// ast-grep --version 캐시 (성공만 저장)
static AST_GREP_VERSION: OnceLock<String> = OnceLock::new();
pub fn ast_grep_version() -> Result<&'static str, String> {
    if let Some(version) = AST_GREP_VERSION.get() {
        return Ok(version);
    }
    let program = ast_grep_program()?;
    let output = run_external(
        ExternalTool::AstGrep,
        &["--version".to_string()],
        None,
        Some(10_000),
    )?;
    let first_line = output.stdout.lines().next().unwrap_or("").trim();
    let Some(version) = first_line.strip_prefix("ast-grep ").map(str::trim) else {
        return Err(format!(
            "'{}' is not the ast-grep CLI (--version printed {first_line:?}, exit {:?}); {AST_GREP_MISSING}",
            program.display(),
            output.status_code
        ));
    };
    if output.status_code != Some(0) || version.is_empty() {
        return Err(format!(
            "ast-grep --version failed (exit {:?}): {}",
            output.status_code,
            output.stderr.trim()
        ));
    }
    Ok(AST_GREP_VERSION.get_or_init(|| version.to_string()))
}
// spawn 실패 메시지 (ast-grep 부재 시 설치 안내)
fn spawn_error_message(tool: ExternalTool, error: &std::io::Error) -> String {
    if tool == ExternalTool::AstGrep && error.kind() == std::io::ErrorKind::NotFound {
        return AST_GREP_MISSING.to_string();
    }
    format!(
        "Failed to start {} from PATH: {error}. Ensure '{}' is installed and on PATH.",
        tool.backend_name(),
        tool.command_name()
    )
}
#[derive(Debug)]
pub struct ToolOutput {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub backend: &'static str,
    // 출력 상한 도달 (종료 코드 무효)
    pub stdout_capped: bool,
}
// 파이프 버퍼 상한 (응답은 수십 KB)
pub const MAX_PIPE_BYTES: u64 = 16 * 1024 * 1024;
// Runs a PATH command and returns its collected stdout / stderr.
pub fn run_external(
    tool: ExternalTool,
    args: &[String],
    cwd: Option<&Path>,
    timeout_ms: Option<u64>,
) -> Result<ToolOutput, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(120_000));
    run_path(tool, args, cwd, timeout)
}
fn run_path(
    tool: ExternalTool,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<ToolOutput, String> {
    let program = tool.resolve_command()?;
    let mut spawned = spawn_child(tool, &program, args, cwd)?;
    let stdout = spawned
        .child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;
    let stderr = spawned
        .child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture stderr".to_string())?;
    // Drops the 5ms busy-poll (`try_wait + sleep`) and uses a channel so reader threads
    // wake the main loop immediately on pipe EOF. Once both pipes close the child is effectively
    // done so child.wait() returns at once. Timeout is handled by mpsc's recv_timeout in one go.
    // 파이프별 상한 도달 신호 (송신 시각 동봉)
    let (done_tx, done_rx) = mpsc::channel::<(Instant, bool)>();
    let stdout_done = done_tx.clone();
    let stderr_done = done_tx.clone();
    drop(done_tx);
    let stdout_handle = thread::spawn(move || {
        let result = read_pipe(stdout);
        let capped = matches!(&result, Ok((_, true)));
        let _ = stdout_done.send((Instant::now(), capped));
        result
    });
    let stderr_handle = thread::spawn(move || {
        let result = read_pipe(stderr);
        let _ = stderr_done.send((Instant::now(), false));
        result
    });

    // 기한 overflow 시 무기한
    let deadline = Instant::now().checked_add(timeout);
    let mut closed = 0usize;
    let mut timed_out = false;
    let mut capped = false;
    while closed < 2 {
        match recv_by_deadline(&done_rx, deadline) {
            Ok(pipe_capped) => {
                closed += 1;
                // 상한 도달 시 자식 종료
                if pipe_capped {
                    capped = true;
                    spawned.kill_tree();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if timed_out {
        spawned.kill_tree();
        let _ = spawned.child.wait();
        return Err(format!(
            "{} timed out after {}ms",
            tool.backend_name(),
            timeout.as_millis()
        ));
    }
    let status = match spawned.child.wait() {
        Ok(status) => status,
        Err(error) => {
            return Err(format!(
                "Failed to wait for {}: {error}",
                tool.backend_name()
            ));
        }
    };

    let (stdout, _) = join_pipe(stdout_handle)?;
    let (stderr, _) = join_pipe(stderr_handle)?;

    Ok(ToolOutput {
        status_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        backend: tool.backend_name(),
        stdout_capped: capped,
    })
}
// spawn된 자식 (ast-grep은 Windows 작업 개체 동반)
struct Spawned {
    child: Child,
    #[cfg(windows)]
    job: Option<kill_job::KillJob>,
    // 자식 전용 프로세스 그룹 (ast-grep)
    #[cfg(unix)]
    own_group: bool,
}
impl Spawned {
    // 자식과 자손 프로세스 종료
    fn kill_tree(&mut self) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        #[cfg(unix)]
        if self.own_group
            && let Ok(pid) = i32::try_from(self.child.id())
        {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            // SAFETY: 인자만 받는 시스템 호출, 음수 pid는 자식 전용 그룹
            unsafe {
                kill(-pid, SIGKILL);
            }
        }
        let _ = self.child.kill();
    }
}
// 명령 구성과 spawn (도구별 인자·창 플래그)
fn spawn_child(
    tool: ExternalTool,
    program: &OsStr,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<Spawned, String> {
    let mut command = Command::new(program);
    if tool == ExternalTool::Git {
        // 비ASCII 경로 원문, 선택적 락 억제
        command.args(["-c", "core.quotepath=false"]);
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // ast-grep 자손 일괄 종료용 전용 그룹 (Unix)
    #[cfg(unix)]
    let own_group = tool == ExternalTool::AstGrep;
    #[cfg(unix)]
    if own_group {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // 콘솔 없는 부모(windowsHide로 상주하는 MCP 서버)가 콘솔 자식(git 등)을 기본 플래그로
    // spawn하면 Windows가 자식용 새 콘솔 창을 만들어 깜빡이므로 CREATE_NO_WINDOW로 억제한다.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // 서버 콘솔 상속 (자식별 conhost 생략)
        let flags = if tool == ExternalTool::AstGrep && console_attached() {
            0
        } else {
            CREATE_NO_WINDOW
        };
        // 작업 개체 편입 전 일시정지 (손자 누락 방지)
        let suspended = if tool == ExternalTool::AstGrep {
            kill_job::CREATE_SUSPENDED
        } else {
            0
        };
        command.creation_flags(flags | suspended);
    }
    let child = command
        .spawn()
        .map_err(|error| spawn_error_message(tool, &error))?;
    #[cfg(windows)]
    {
        let adopted = if tool == ExternalTool::AstGrep {
            kill_job::KillJob::adopt_suspended(&child)
        } else {
            Ok(None)
        };
        match adopted {
            Ok(job) => Ok(Spawned { child, job }),
            // 재개 실패 자식 회수
            Err(error) => {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
    #[cfg(unix)]
    return Ok(Spawned { child, own_group });
    #[cfg(not(any(windows, unix)))]
    Ok(Spawned { child })
}
// 서버 콘솔 연결 여부 (1회 판정)
#[cfg(windows)]
fn console_attached() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(process_list: *mut u32, process_count: u32) -> u32;
    }
    static ATTACHED: OnceLock<bool> = OnceLock::new();
    *ATTACHED.get_or_init(|| {
        let mut first = 0u32;
        // SAFETY: 길이 1 버퍼, 호출 중에만 쓰기
        let attached = unsafe { GetConsoleProcessList(&mut first, 1) };
        attached > 0
    })
}
// 자손 트리 종료용 작업 개체 (닫으면 소속 프로세스 종료)
#[cfg(windows)]
mod kill_job {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr;

    type Handle = *mut c_void;
    pub(super) const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    // JOBOBJECT_EXTENDED_LIMIT_INFORMATION 배치
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
        io_counters: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, size: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    // 일시정지 프로세스 재개 (XP 이후 ntdll export)
    // raw-dylib: msvc SDK ntdll.lib 불필요
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtResumeProcess(process: Handle) -> i32;
    }

    pub(super) struct KillJob(Handle);
    impl KillJob {
        // 일시정지 자식 편입 후 재개 (편입 실패 시 None, 재개 실패 시 오류)
        pub(super) fn adopt_suspended(child: &Child) -> Result<Option<Self>, String> {
            // 1. 종료 연동 작업 개체 편입
            // SAFETY: 보안 속성·이름 없음, 반환 핸들은 KillJob 소유
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            let job = (!handle.is_null())
                .then_some(KillJob(handle))
                .filter(|job| {
                    let limits = ExtendedLimits {
                        limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..ExtendedLimits::default()
                    };
                    // SAFETY: 유효 작업 핸들, 구조체 크기 일치, 호출 중에만 읽기
                    let configured = unsafe {
                        SetInformationJobObject(
                            job.0,
                            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                            (&raw const limits).cast(),
                            size_of::<ExtendedLimits>() as u32,
                        )
                    } != 0;
                    // SAFETY: 작업·프로세스 핸들 모두 호출 동안 유효
                    configured
                        && unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) } != 0
                });

            // 2. 자식 재개 (스레드 열거 없는 단일 호출)
            // SAFETY: 호출 동안 유효한 자식 프로세스 핸들
            let status = unsafe { NtResumeProcess(child.as_raw_handle()) };
            if status < 0 {
                return Err(format!(
                    "Failed to resume the suspended ast-grep process (pid {}, NTSTATUS {status:#010x})",
                    child.id()
                ));
            }
            Ok(job)
        }
        // 소속 프로세스 전체 종료
        pub(super) fn terminate(&self) {
            // SAFETY: 소유 중인 유효 작업 핸들
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }
    impl Drop for KillJob {
        fn drop(&mut self) {
            // SAFETY: 소유 핸들 1회 닫기
            unsafe { CloseHandle(self.0) };
        }
    }
}
#[derive(Debug)]
pub struct StreamOutput {
    pub status_code: Option<i32>,
    pub stderr: String,
    // 출력 상한 도달 (종료 코드 무효)
    pub stdout_capped: bool,
    // 기한 도달로 종료 (받은 줄만 유효)
    pub timed_out: bool,
}
// Runs a PATH command and hands each stdout line to on_line as it arrives.
// Output sent before the deadline still counts; past it the child is killed and the lines delivered so far stand (timed_out).
pub fn run_external_lines(
    tool: ExternalTool,
    args: &[String],
    cwd: Option<&Path>,
    timeout_ms: Option<u64>,
    on_line: &mut dyn FnMut(&str),
) -> Result<StreamOutput, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(120_000));
    let program = tool.resolve_command()?;
    let mut spawned = spawn_child(tool, &program, args, cwd)?;
    let (Some(stdout), Some(stderr)) = (spawned.child.stdout.take(), spawned.child.stderr.take())
    else {
        spawned.kill_tree();
        let _ = spawned.child.wait();
        return Err("Failed to capture stdout / stderr".to_string());
    };
    let deadline = Instant::now().checked_add(timeout);
    let streamed = stream_pipes(
        stdout,
        stderr,
        deadline,
        &mut || spawned.kill_tree(),
        on_line,
    );
    // 실패 경로도 자식 회수 (kill 후)
    let status = spawned.child.wait();
    let streamed = streamed?;
    let status =
        status.map_err(|error| format!("Failed to wait for {}: {error}", tool.backend_name()))?;
    Ok(StreamOutput {
        status_code: status.code(),
        stderr: String::from_utf8_lossy(&streamed.stderr).into_owned(),
        stdout_capped: streamed.capped,
        timed_out: streamed.timed_out,
    })
}
struct Streamed {
    stderr: Vec<u8>,
    capped: bool,
    timed_out: bool,
}
enum PipeEvent {
    Stdout(Vec<u8>),
    // 상한 도달 여부
    StdoutEnd(std::io::Result<bool>),
    StderrEnd(std::io::Result<(Vec<u8>, bool)>),
}
// stdout 줄 분배 루프 (상한·기한·읽기 오류 시 kill)
fn stream_pipes<O, E>(
    stdout: O,
    stderr: E,
    deadline: Option<Instant>,
    kill: &mut dyn FnMut(),
    on_line: &mut dyn FnMut(&str),
) -> Result<Streamed, String>
where
    O: Read + Send + 'static,
    E: Read + Send + 'static,
{
    // 1. 파이프 리더 스레드 (송신 시각 동봉)
    let (event_tx, event_rx) = mpsc::channel::<(Instant, PipeEvent)>();
    let stderr_tx = event_tx.clone();
    thread::spawn(move || {
        let end = read_pipe(stderr);
        let _ = stderr_tx.send((Instant::now(), PipeEvent::StderrEnd(end)));
    });
    thread::spawn(move || {
        let mut stdout = stdout;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut room = MAX_PIPE_BYTES;
        let end = loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break Ok(false),
                Ok(read) => {
                    let kept = (read as u64).min(room);
                    room -= kept;
                    let chunk = buffer.get(..kept as usize).unwrap_or_default().to_vec();
                    // 수신측 종료 (기한 도달)
                    if event_tx
                        .send((Instant::now(), PipeEvent::Stdout(chunk)))
                        .is_err()
                    {
                        return;
                    }
                    if kept < read as u64 {
                        break Ok(true);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => break Err(error),
            }
        };
        let _ = event_tx.send((Instant::now(), PipeEvent::StdoutEnd(end)));
    });

    // 2. 이벤트 루프
    let mut streamed = Streamed {
        stderr: Vec::new(),
        capped: false,
        timed_out: false,
    };
    let mut pending: Vec<u8> = Vec::new();
    let mut open = 2usize;
    while open > 0 {
        let event = match recv_by_deadline(&event_rx, deadline) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill();
                streamed.timed_out = true;
                return Ok(streamed);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match event {
            PipeEvent::Stdout(chunk) => {
                pending.extend_from_slice(&chunk);
                let mut start = 0usize;
                while let Some(rest) = pending.get(start..)
                    && let Some(offset) = memchr::memchr(b'\n', rest)
                {
                    let line = rest.get(..offset).unwrap_or_default();
                    on_line(&String::from_utf8_lossy(
                        line.strip_suffix(b"\r").unwrap_or(line),
                    ));
                    start += offset + 1;
                }
                pending.drain(..start);
            }
            PipeEvent::StdoutEnd(Ok(capped)) => {
                open -= 1;
                // 잘린 마지막 줄 버림
                if capped {
                    streamed.capped = true;
                    pending.clear();
                    kill();
                }
            }
            PipeEvent::StderrEnd(Ok((bytes, _))) => {
                open -= 1;
                streamed.stderr = bytes;
            }
            PipeEvent::StdoutEnd(Err(error)) | PipeEvent::StderrEnd(Err(error)) => {
                kill();
                return Err(format!("Failed to read external tool output: {error}"));
            }
        }
    }

    // 3. 개행 없는 마지막 줄
    if !pending.is_empty() {
        on_line(&String::from_utf8_lossy(
            pending.strip_suffix(b"\r").unwrap_or(&pending),
        ));
    }
    Ok(streamed)
}
// 기한 전 송신 이벤트 수신 (기한 뒤 큐 잔여분 포함)
fn recv_by_deadline<T>(
    events: &mpsc::Receiver<(Instant, T)>,
    deadline: Option<Instant>,
) -> Result<T, mpsc::RecvTimeoutError> {
    let Some(deadline) = deadline else {
        return events
            .recv()
            .map(|(_, event)| event)
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    // 기한 도달 후 대기 없는 큐 확인
    let (sent, event) = match events.recv_timeout(remaining) {
        Err(mpsc::RecvTimeoutError::Timeout) => events.try_recv().map_err(|error| match error {
            mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
            mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
        })?,
        received => received?,
    };
    // 기한 뒤 송신분 (미완료 실행)
    if sent > deadline {
        return Err(mpsc::RecvTimeoutError::Timeout);
    }
    Ok(event)
}
// 상한까지 읽기 (초과 여부 반환)
fn read_pipe<R: Read>(pipe: R) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    pipe.take(MAX_PIPE_BYTES + 1).read_to_end(&mut bytes)?;
    let capped = bytes.len() as u64 > MAX_PIPE_BYTES;
    bytes.truncate(MAX_PIPE_BYTES as usize);
    Ok((bytes, capped))
}
fn join_pipe(
    handle: thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), String> {
    handle
        .join()
        .map_err(|_| "Failed to join external tool reader thread".to_string())?
        .map_err(|error| format!("Failed to read external tool output: {error}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};

    // 가짜 파일 트리 존재 판정 (상대 경로는 root/server 기준)
    fn fake_tree(root: &Path, files: &[&str]) -> impl Fn(&Path) -> bool + use<> {
        let known: HashSet<PathBuf> = files
            .iter()
            .map(|file| {
                file.split('/')
                    .fold(root.to_path_buf(), |path, part| path.join(part))
            })
            .collect();
        let server_cwd = root.join("server");
        move |candidate| known.contains(&server_cwd.join(candidate))
    }
    fn fake_root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { "C:\\fake" } else { "/fake" })
    }
    fn path_var(root: &Path, dirs: &[&str]) -> OsString {
        std::env::join_paths(dirs.iter().map(|dir| {
            if dir.is_empty() {
                PathBuf::new()
            } else {
                dir.split('/')
                    .fold(root.to_path_buf(), |path, part| path.join(part))
            }
        }))
        .unwrap()
    }
    // 첫 청크 뒤 송신측 drop 까지 대기
    struct GateReader {
        chunks: VecDeque<Vec<u8>>,
        gate: mpsc::Receiver<()>,
    }
    impl Read for GateReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                let _ = self.gate.recv();
                return Ok(0);
            };
            let size = chunk.len().min(buffer.len());
            buffer[..size].copy_from_slice(&chunk[..size]);
            Ok(size)
        }
    }
    // 예정 시각 청크, EOF 신호
    struct ScheduledReader {
        chunks: VecDeque<(Option<Instant>, Vec<u8>)>,
        eof: Option<mpsc::Sender<()>>,
    }
    impl Read for ScheduledReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let Some((at, chunk)) = self.chunks.pop_front() else {
                if let Some(eof) = self.eof.take() {
                    let _ = eof.send(());
                }
                return Ok(0);
            };
            if let Some(at) = at {
                thread::sleep(at.saturating_duration_since(Instant::now()));
            }
            let size = chunk.len().min(buffer.len());
            buffer[..size].copy_from_slice(&chunk[..size]);
            Ok(size)
        }
    }

    #[test]
    fn pipe_reads_stop_at_the_cap() {
        let oversized = vec![b'x'; MAX_PIPE_BYTES as usize + 10];
        let (bytes, capped) = read_pipe(std::io::Cursor::new(oversized)).unwrap();
        assert!(capped);
        assert_eq!(bytes.len() as u64, MAX_PIPE_BYTES);
        let (bytes, capped) = read_pipe(std::io::Cursor::new(b"small".to_vec())).unwrap();
        assert!(!capped);
        assert_eq!(bytes, b"small");
    }
    #[test]
    fn relative_path_entries_never_resolve_binaries() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("rust-fs-mcp-relpath-{}", std::process::id()));
        let absolute = root.join("abs");
        std::fs::create_dir_all(&absolute).unwrap();
        let file = if cfg!(windows) { "git.exe" } else { "git" };
        std::fs::write(absolute.join(file), b"").unwrap();
        // 상대 항목 무시, 절대 항목만 채택
        let relative = Path::new("target").join("definitely-relative");
        let only_relative = std::env::join_paths([relative.clone()]).unwrap();
        assert_eq!(find_in_dirs(&only_relative, "git"), None);
        let mixed = std::env::join_paths([relative, absolute.clone()]).unwrap();
        assert_eq!(find_in_dirs(&mixed, "git"), Some(absolute.join(file)));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn std_search_order_puts_system_dirs_before_path() {
        // 플랫폼별 구분자 없는 절대 경로
        let tools = if cfg!(windows) { "C:/tools" } else { "/tools" };
        let path = std::env::join_paths([Path::new(tools)]).unwrap();
        let searched = std_search_path(Some(path.clone())).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&searched).collect();
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            let root = PathBuf::from(root);
            assert_eq!(dirs[0], root.join("System32"));
            assert_eq!(dirs[1], root);
            assert_eq!(dirs[2], PathBuf::from(tools));
        } else {
            assert_eq!(searched, path);
        }
    }
    #[test]
    fn missing_ast_grep_gets_install_guidance() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let message = spawn_error_message(ExternalTool::AstGrep, &missing);
        assert_eq!(message, AST_GREP_MISSING);
        assert!(message.contains("cargo install ast-grep --locked"));
        assert!(message.contains("set RUST_FS_MCP_AST_GREP"), "{message}");
        // 기존 도구 문구 유지
        let git = spawn_error_message(ExternalTool::Git, &missing);
        assert!(
            git.starts_with("Failed to start path-git from PATH: "),
            "{git}"
        );
        assert!(
            git.ends_with(". Ensure 'git' is installed and on PATH."),
            "{git}"
        );
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let other = spawn_error_message(ExternalTool::AstGrep, &denied);
        assert!(
            other.starts_with("Failed to start path-ast-grep from PATH: "),
            "{other}"
        );
    }
    #[test]
    fn unbounded_timeout_does_not_overflow_the_deadline() {
        let output = run_path(
            ExternalTool::Git,
            &["--version".to_string()],
            None,
            Duration::MAX,
        )
        .unwrap();
        assert_eq!(output.status_code, Some(0));
        assert!(
            output.stdout.starts_with("git version"),
            "{}",
            output.stdout
        );
    }
    #[test]
    fn streamed_lines_stop_at_the_deadline() {
        let (gate_tx, gate_rx) = mpsc::channel();
        let stdout = GateReader {
            chunks: VecDeque::from([b"one\r\ntw".to_vec(), b"o\npartial".to_vec()]),
            gate: gate_rx,
        };
        let mut kills = 0usize;
        let mut lines = Vec::new();
        let started = Instant::now();
        let streamed = stream_pipes(
            stdout,
            std::io::Cursor::new(Vec::new()),
            Instant::now().checked_add(Duration::from_millis(300)),
            &mut || kills += 1,
            &mut |line| lines.push(line.to_string()),
        )
        .unwrap();
        assert!(streamed.timed_out);
        assert!(!streamed.capped);
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(kills, 1);
        assert!(started.elapsed() < Duration::from_secs(10));
        drop(gate_tx);
    }
    #[test]
    fn streamed_lines_flush_the_unterminated_tail() {
        let mut kills = 0usize;
        let mut lines = Vec::new();
        let streamed = stream_pipes(
            std::io::Cursor::new(b"a\r\nb\n\nc".to_vec()),
            std::io::Cursor::new(b"warn".to_vec()),
            None,
            &mut || kills += 1,
            &mut |line| lines.push(line.to_string()),
        )
        .unwrap();
        assert!(!streamed.timed_out);
        assert_eq!(lines, ["a", "b", "", "c"]);
        assert_eq!(streamed.stderr, b"warn");
        assert_eq!(kills, 0);
    }
    #[test]
    fn streamed_events_queued_before_the_deadline_still_count() {
        // 기한 전 완주, 기한 뒤 소비
        let deadline = Instant::now() + Duration::from_secs(1);
        let (eof_tx, eof_rx) = mpsc::channel();
        let stdout = ScheduledReader {
            chunks: VecDeque::from([
                (None, b"one\n".to_vec()),
                (None, b"two\n".to_vec()),
                (None, b"three".to_vec()),
            ]),
            eof: Some(eof_tx.clone()),
        };
        let stderr = ScheduledReader {
            chunks: VecDeque::from([(None, b"warn".to_vec())]),
            eof: Some(eof_tx),
        };
        let mut kills = 0usize;
        let mut lines: Vec<String> = Vec::new();
        let streamed = stream_pipes(
            stdout,
            stderr,
            Some(deadline),
            &mut || kills += 1,
            &mut |line| {
                if lines.is_empty() {
                    for _ in 0..2 {
                        eof_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                    thread::sleep(
                        deadline.saturating_duration_since(Instant::now())
                            + Duration::from_millis(200),
                    );
                }
                lines.push(line.to_string());
            },
        )
        .unwrap();
        assert!(!streamed.timed_out);
        assert_eq!(lines, ["one", "two", "three"]);
        assert_eq!(streamed.stderr, b"warn");
        assert_eq!(kills, 0);

        // 기한 뒤 송신분 제외
        let deadline = Instant::now() + Duration::from_millis(300);
        let stdout = ScheduledReader {
            chunks: VecDeque::from([
                (None, b"one\n".to_vec()),
                (
                    Some(deadline + Duration::from_millis(100)),
                    b"late\n".to_vec(),
                ),
            ]),
            eof: None,
        };
        let mut kills = 0usize;
        let mut lines: Vec<String> = Vec::new();
        let streamed = stream_pipes(
            stdout,
            std::io::Cursor::new(Vec::new()),
            Some(deadline),
            &mut || kills += 1,
            &mut |line| {
                if lines.is_empty() {
                    thread::sleep(
                        deadline.saturating_duration_since(Instant::now())
                            + Duration::from_millis(300),
                    );
                }
                lines.push(line.to_string());
            },
        )
        .unwrap();
        assert!(streamed.timed_out);
        assert_eq!(lines, ["one"]);
        assert_eq!(kills, 1);
    }
    #[test]
    fn explicit_ast_grep_override_wins_and_must_exist() {
        let root = fake_root();
        let exists = fake_tree(
            &root,
            &["bin/ast-grep.exe", "bin/ast-grep", "opt/sg-native"],
        );
        let path = path_var(&root, &["bin"]);
        let explicit = root.join("opt").join("sg-native");
        let found = resolve_ast_grep(Some(&path), Some(explicit.as_os_str()), None, None, &exists);
        assert_eq!(found, Ok(explicit));
        let missing = root.join("opt").join("missing");
        let error = resolve_ast_grep(Some(&path), Some(missing.as_os_str()), None, None, &exists)
            .unwrap_err();
        assert!(
            error.starts_with("RUST_FS_MCP_AST_GREP is set to "),
            "{error}"
        );
        // 빈 값은 미설정
        let found =
            resolve_ast_grep(Some(&path), Some(OsStr::new("")), None, None, &exists).unwrap();
        assert_eq!(found.parent(), Some(root.join("bin").as_path()));
    }
    #[test]
    fn path_binary_first_dir_wins_and_sg_is_never_used() {
        let root = fake_root();
        let binary = if cfg!(windows) {
            "ast-grep.exe"
        } else {
            "ast-grep"
        };
        let sg_only = fake_tree(&root, &["a/sg.exe", "a/sg"]);
        let path = path_var(&root, &["", "a"]);
        assert_eq!(
            resolve_ast_grep(Some(&path), None, None, None, &sg_only),
            Err(AST_GREP_MISSING.to_string())
        );
        let first = format!("b/{binary}");
        let second = format!("c/{binary}");
        let both = fake_tree(&root, &["a/sg.exe", &first, &second]);
        let path = path_var(&root, &["a", "", "b", "c"]);
        assert_eq!(
            resolve_ast_grep(Some(&path), None, None, None, &both),
            Ok(root.join("b").join(binary))
        );
        assert_eq!(
            resolve_ast_grep(None, None, None, None, &both),
            Err(AST_GREP_MISSING.to_string())
        );
    }
    #[cfg(windows)]
    #[test]
    fn npm_shim_resolves_the_native_binary() {
        let root = fake_root();
        let path = path_var(&root, &["npm"]);
        let cases = [
            "npm/node_modules/@ast-grep/cli/ast-grep.exe",
            "npm/node_modules/@ast-grep/cli/node_modules/@ast-grep/cli-win32-arm64-msvc/ast-grep.exe",
            "npm/node_modules/@ast-grep/cli-win32-x64-msvc/ast-grep.exe",
        ];
        for native in cases {
            let exists = fake_tree(&root, &["npm/ast-grep.cmd", "npm/ast-grep", native]);
            let expected = native
                .split('/')
                .fold(root.clone(), |path, part| path.join(part));
            assert_eq!(
                resolve_ast_grep(Some(&path), None, None, None, &exists),
                Ok(expected)
            );
        }
        // 래퍼 없는 폴더는 node_modules 미탐색
        let unshimmed = fake_tree(&root, &["npm/node_modules/@ast-grep/cli/ast-grep.exe"]);
        assert_eq!(
            resolve_ast_grep(Some(&path), None, None, None, &unshimmed),
            Err(AST_GREP_MISSING.to_string())
        );
        // 로컬 node_modules/.bin 래퍼
        let local = path_var(&root, &["app/node_modules/.bin"]);
        let exists = fake_tree(
            &root,
            &[
                "app/node_modules/.bin/ast-grep.cmd",
                "app/node_modules/@ast-grep/cli/ast-grep.exe",
            ],
        );
        assert_eq!(
            resolve_ast_grep(Some(&local), None, None, None, &exists),
            Ok(root
                .join("app")
                .join("node_modules")
                .join("@ast-grep")
                .join("cli")
                .join("ast-grep.exe"))
        );
    }
    #[test]
    fn relative_ast_grep_paths_resolve_against_the_server_cwd() {
        let root = fake_root();
        let cwd = root.join("server");
        let binary = if cfg!(windows) {
            "ast-grep.exe"
        } else {
            "ast-grep"
        };
        let tools = format!("server/tools/{binary}");
        let relative_dir = format!("server/rel/{binary}");
        let absolute_dir = format!("abs/{binary}");
        let exists = fake_tree(&root, &[&tools, &relative_dir, &absolute_dir]);
        // 상대 지정값은 서버 cwd 기준 절대 경로
        let relative = Path::new("tools").join(binary);
        assert_eq!(
            resolve_ast_grep(None, Some(relative.as_os_str()), Some(&cwd), None, &exists),
            Ok(cwd.join("tools").join(binary))
        );
        // cwd 미상 시 거절
        let error =
            resolve_ast_grep(None, Some(relative.as_os_str()), None, None, &exists).unwrap_err();
        assert!(error.contains("absolute path"), "{error}");
        if cfg!(windows) {
            // 드라이브 상대 경로 거절
            let drive_relative = OsStr::new("C:tools\\ast-grep.exe");
            let error = resolve_ast_grep(None, Some(drive_relative), Some(&cwd), None, &exists)
                .unwrap_err();
            assert!(error.contains("absolute path"), "{error}");
        }
        // 상대 PATH 항목 건너뜀
        let path = std::env::join_paths([PathBuf::from("rel"), root.join("abs")]).unwrap();
        assert_eq!(
            resolve_ast_grep(Some(&path), None, Some(&cwd), None, &exists),
            Ok(root.join("abs").join(binary))
        );
        let only_relative = std::env::join_paths([PathBuf::from("rel")]).unwrap();
        assert_eq!(
            resolve_ast_grep(Some(&only_relative), None, Some(&cwd), None, &exists),
            Err(AST_GREP_MISSING.to_string())
        );
    }
    #[test]
    fn server_exe_dir_precedes_path_on_windows() {
        let root = fake_root();
        let binary = if cfg!(windows) {
            "ast-grep.exe"
        } else {
            "ast-grep"
        };
        let in_app = format!("app/{binary}");
        let in_bin = format!("bin/{binary}");
        let in_server_app = format!("server/app/{binary}");
        let exists = fake_tree(&root, &[&in_app, &in_bin, &in_server_app]);
        let path = path_var(&root, &["bin"]);
        let app = root.join("app");
        let on_path = root.join("bin").join(binary);
        // std Command 탐색 순서 (Windows만)
        let expected = if cfg!(windows) {
            app.join(binary)
        } else {
            on_path.clone()
        };
        assert_eq!(
            resolve_ast_grep(Some(&path), None, None, Some(&app), &exists),
            Ok(expected)
        );
        // 폴더에 없으면 PATH
        let empty = root.join("empty");
        assert_eq!(
            resolve_ast_grep(Some(&path), None, None, Some(&empty), &exists),
            Ok(on_path.clone())
        );
        // 상대 실행 파일 폴더 무시
        assert_eq!(
            resolve_ast_grep(Some(&path), None, None, Some(Path::new("app")), &exists),
            Ok(on_path.clone())
        );
        // 환경변수 우선
        assert_eq!(
            resolve_ast_grep(
                Some(&path),
                Some(on_path.as_os_str()),
                None,
                Some(&app),
                &exists
            ),
            Ok(on_path)
        );
    }
    #[cfg(windows)]
    #[test]
    fn wrapper_override_resolves_to_the_native_binary() {
        let root = fake_root();
        let native = "npm/node_modules/@ast-grep/cli/ast-grep.exe";
        let local = "app/node_modules/@ast-grep/cli-win32-x64-msvc/ast-grep.exe";
        let exists = fake_tree(
            &root,
            &[
                "npm/ast-grep.cmd",
                "npm/ast-grep.ps1",
                "npm/AST-GREP.BAT",
                native,
                "app/node_modules/.bin/ast-grep.cmd",
                local,
                "other/wrap.bat",
                "tools/AST-GREP.EXE",
            ],
        );
        let at = |file: &str| {
            file.split('/')
                .fold(root.clone(), |path, part| path.join(part))
        };
        let resolve =
            |file: &str| resolve_ast_grep(None, Some(at(file).as_os_str()), None, None, &exists);
        // npm 래퍼는 네이티브 exe로 치환
        for shim in ["npm/ast-grep.cmd", "npm/ast-grep.ps1", "npm/AST-GREP.BAT"] {
            assert_eq!(resolve(shim), Ok(at(native)), "{shim}");
        }
        assert_eq!(resolve("app/node_modules/.bin/ast-grep.cmd"), Ok(at(local)));
        // 해석 불가 래퍼 거절
        let error = resolve("other/wrap.bat").unwrap_err();
        assert!(
            error.contains("wrapper") && error.contains("ast-grep.exe"),
            "{error}"
        );
        // 네이티브 exe는 그대로
        assert_eq!(resolve("tools/AST-GREP.EXE"), Ok(at("tools/AST-GREP.EXE")));
    }
    #[cfg(windows)]
    #[test]
    fn tree_kill_reaches_wrapper_descendants() {
        use std::io::BufRead;
        let dir = std::env::temp_dir().join(format!(
            "rfm-kill-tree-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let wrapper = dir.join("wrapper.cmd");
        // 손자 ping (파이프 상속)
        std::fs::write(
            &wrapper,
            "@echo off\r\necho started\r\nping -n 9 127.0.0.1\r\n",
        )
        .unwrap();
        let mut spawned =
            spawn_child(ExternalTool::AstGrep, wrapper.as_os_str(), &[], None).unwrap();
        // 한도 설정·편입 성공 (구조체 크기 일치)
        assert!(spawned.job.is_some());
        let stdout = spawned.child.stdout.take().unwrap();
        let (line_tx, line_rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .split(b'\n')
                .map_while(Result::ok)
            {
                let line = String::from_utf8_lossy(&line).trim().to_string();
                if line_tx.send(line).is_err() {
                    return;
                }
            }
        });
        // ping 출력 시작까지 대기
        loop {
            let line = line_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if !line.is_empty() && line != "started" {
                break;
            }
        }
        spawned.kill_tree();
        let killed = Instant::now();
        // 손자 종료 시 파이프 EOF
        while line_rx.recv_timeout(Duration::from_secs(20)).is_ok() {}
        let elapsed = killed.elapsed();
        let _ = spawned.child.wait();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            elapsed < Duration::from_secs(3),
            "pipe stayed open {elapsed:?} after the tree kill"
        );
    }
    #[cfg(windows)]
    #[test]
    fn console_probe_follows_the_spawn_flags() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        for (flags, expect) in [
            (CREATE_NO_WINDOW, "attached"),
            (DETACHED_PROCESS, "detached"),
        ] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "core::external::tests::console_probe_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("RFM_CONSOLE_EXPECT", expect)
                .creation_flags(flags)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{expect}:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "child process of console_probe_follows_the_spawn_flags"]
    fn console_probe_child() {
        let expect = std::env::var("RFM_CONSOLE_EXPECT").unwrap();
        assert_eq!(console_attached(), expect == "attached");
        // 두 플래그 경로 모두 실행 가능
        let output =
            run_external(ExternalTool::Git, &["--version".to_string()], None, None).unwrap();
        assert_eq!(output.status_code, Some(0));
        if ast_grep_program().is_ok() {
            let output = run_external(
                ExternalTool::AstGrep,
                &["--version".to_string()],
                None,
                None,
            )
            .unwrap();
            assert!(output.stdout.starts_with("ast-grep "), "{}", output.stdout);
        }
    }
}
