use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundledTool {
    Rg,
    Fd,
    Bat,
    Jq,
    Sd,
    Hyperfine,
    Tokei,
    Git,
}

impl BundledTool {
    // PATH 에서 찾을 명령 이름. vendor 패키징을 제거하고 시스템 PATH 명령을 직접 호출한다.
    fn command_name(self) -> &'static str {
        match self {
            Self::Rg => "rg",
            Self::Fd => "fd",
            Self::Bat => "bat",
            Self::Jq => "jq",
            Self::Sd => "sd",
            Self::Hyperfine => "hyperfine",
            Self::Tokei => "tokei",
            Self::Git => "git",
        }
    }

    pub fn backend_name(self) -> &'static str {
        match self {
            Self::Rg => "path-rg",
            Self::Fd => "path-fd",
            Self::Bat => "path-bat",
            Self::Jq => "path-jq",
            Self::Sd => "path-sd",
            Self::Hyperfine => "path-hyperfine",
            Self::Tokei => "path-tokei",
            Self::Git => "path-git",
        }
    }
}

#[derive(Debug)]
pub struct ToolOutput {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub backend: &'static str,
}

// PATH 명령을 실행하고 stdout/stderr 를 모아 반환한다.
pub fn run_bundled(
    tool: BundledTool,
    args: &[String],
    cwd: Option<&Path>,
    timeout_ms: Option<u64>,
) -> Result<ToolOutput, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(120_000));
    run_path(tool, args, cwd, timeout)
}

fn run_path(
    tool: BundledTool,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<ToolOutput, String> {
    let mut command = Command::new(tool.command_name());
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to start {} from PATH: {error}. Ensure '{}' is installed and on PATH.",
            tool.backend_name(),
            tool.command_name()
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture stderr".to_string())?;
    // 5ms busy-poll(`try_wait + sleep`) 을 제거하고, reader 스레드가 pipe EOF 에 도달할 때 즉시
    // 깨어나도록 채널로 신호한다. 양 파이프가 모두 닫히면 자식은 사실상 종료된 상태이므로
    // child.wait() 는 곧바로 반환한다. 타임아웃은 mpsc 의 recv_timeout 으로 한 번에 처리한다.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let stdout_done = done_tx.clone();
    let stderr_done = done_tx.clone();
    drop(done_tx);
    let stdout_handle = thread::spawn(move || {
        let result = read_pipe(stdout);
        let _ = stdout_done.send(());
        result
    });
    let stderr_handle = thread::spawn(move || {
        let result = read_pipe(stderr);
        let _ = stderr_done.send(());
        result
    });

    let deadline = Instant::now() + timeout;
    let mut closed = 0usize;
    let mut timed_out = false;
    while closed < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        match done_rx.recv_timeout(remaining) {
            Ok(()) => closed += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "{} timed out after {}ms",
            tool.backend_name(),
            timeout.as_millis()
        ));
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            return Err(format!(
                "Failed to wait for {}: {error}",
                tool.backend_name()
            ));
        }
    };

    let stdout = join_pipe(stdout_handle)?;
    let stderr = join_pipe(stderr_handle)?;

    Ok(ToolOutput {
        status_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        backend: tool.backend_name(),
    })
}

fn read_pipe<R: Read>(mut pipe: R) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe(handle: thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>, String> {
    handle
        .join()
        .map_err(|_| "Failed to join bundled tool reader thread".to_string())?
        .map_err(|error| format!("Failed to read bundled tool output: {error}"))
}
