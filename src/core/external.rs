//! external.rs
//! core::external
//!
//! Wrapper that spawns external CLIs (ripgrep, fd, git) resolved from PATH and collects stdout / stderr.
//! Uses reader threads and an mpsc channel to handle EOF and timeout in a single loop.
//!

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalTool {
  Rg,
  Fd,
  Git,
  Obscura,
}
impl ExternalTool {
  // Command name resolved from PATH. The system PATH binary is invoked directly.
  fn command_name(self) -> &'static str {
    match self {
      Self::Rg => "rg",
      Self::Fd => "fd",
      Self::Git => "git",
      Self::Obscura => "obscura",
    }
  }
  pub fn backend_name(self) -> &'static str {
    match self {
      Self::Rg => "path-rg",
      Self::Fd => "path-fd",
      Self::Git => "path-git",
      Self::Obscura => "path-obscura",
    }
  }
  // External tools are resolved from PATH. git additionally caches a de-shuttled path.
  fn resolve_command(self) -> String {
    if self == Self::Git {
      return git_command().to_string();
    }
    self.command_name().to_string()
  }
}
// git.exe 해석 캐시: Git-for-Windows의 cmd\git.exe는 mingw64\bin\git.exe를 다시 실행하는
// 셔틀이라 호출당 ~13ms를 더 쓴다(실측 77ms vs 64ms). PATH에서 찾은 경로가 셔틀이면
// 실제 바이너리로 1회 치환해 캐시하고, 못 찾으면 기존처럼 PATH의 "git"에 맡긴다.
static GIT_COMMAND: OnceLock<String> = OnceLock::new();
fn git_command() -> &'static str {
  GIT_COMMAND.get_or_init(|| {
    let Some(found) = find_on_path("git") else {
      return "git".to_string();
    };
    if let Some(parent) = found.parent()
      && parent.file_name().and_then(|name| name.to_str()) == Some("cmd")
      && let Some(root) = parent.parent()
    {
      let direct = root.join("mingw64").join("bin").join(if cfg!(windows) { "git.exe" } else { "git" });
      if direct.is_file() {
        return direct.to_string_lossy().into_owned();
      }
    }
    found.to_string_lossy().into_owned()
  })
}
fn find_on_path(name: &str) -> Option<PathBuf> {
  let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
  let paths = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&paths) {
    if dir.as_os_str().is_empty() {
      continue;
    }
    let candidate = dir.join(&file);
    if candidate.is_file() {
      return Some(candidate);
    }
  }
  None
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
pub fn run_external(tool: ExternalTool, args: &[String], cwd: Option<&Path>, timeout_ms: Option<u64>) -> Result<ToolOutput, String> {
  let timeout = Duration::from_millis(timeout_ms.unwrap_or(120_000));
  run_path(tool, args, cwd, timeout)
}
fn run_path(tool: ExternalTool, args: &[String], cwd: Option<&Path>, timeout: Duration) -> Result<ToolOutput, String> {
  let mut command = Command::new(tool.resolve_command());
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
  // 콘솔 없는 부모(windowsHide로 상주하는 MCP 서버)가 콘솔 자식(git 등)을 기본 플래그로
  // spawn하면 Windows가 자식용 새 콘솔 창을 만들어 깜빡이므로 CREATE_NO_WINDOW로 억제한다.
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
  }
  let mut child = command.spawn().map_err(|error| format!("Failed to start {} from PATH: {error}. Ensure '{}' is installed and on PATH.", tool.backend_name(), tool.command_name()))?;
  let stdout = child.stdout.take().ok_or_else(|| "Failed to capture stdout".to_string())?;
  let stderr = child.stderr.take().ok_or_else(|| "Failed to capture stderr".to_string())?;
  // Drops the 5ms busy-poll (`try_wait + sleep`) and uses a channel so reader threads
  // wake the main loop immediately on pipe EOF. Once both pipes close the child is effectively
  // done so child.wait() returns at once. Timeout is handled by mpsc's recv_timeout in one go.
  // 파이프별 상한 도달 신호
  let (done_tx, done_rx) = mpsc::channel::<bool>();
  let stdout_done = done_tx.clone();
  let stderr_done = done_tx.clone();
  drop(done_tx);
  let stdout_handle = thread::spawn(move || {
    let result = read_pipe(stdout);
    let capped = matches!(&result, Ok((_, true)));
    let _ = stdout_done.send(capped);
    result
  });
  let stderr_handle = thread::spawn(move || {
    let result = read_pipe(stderr);
    let _ = stderr_done.send(false);
    result
  });

  let deadline = Instant::now() + timeout;
  let mut closed = 0usize;
  let mut timed_out = false;
  let mut capped = false;
  while closed < 2 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      timed_out = true;
      break;
    }
    match done_rx.recv_timeout(remaining) {
      Ok(pipe_capped) => {
        closed += 1;
        // 상한 도달 시 자식 종료
        if pipe_capped {
          capped = true;
          let _ = child.kill();
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
    let _ = child.kill();
    let _ = child.wait();
    return Err(format!("{} timed out after {}ms", tool.backend_name(), timeout.as_millis()));
  }
  let status = match child.wait() {
    Ok(status) => status,
    Err(error) => {
      return Err(format!("Failed to wait for {}: {error}", tool.backend_name()));
    }
  };

  let (stdout, _) = join_pipe(stdout_handle)?;
  let (stderr, _) = join_pipe(stderr_handle)?;

  Ok(ToolOutput { status_code: status.code(), stdout: String::from_utf8_lossy(&stdout).into_owned(), stderr: String::from_utf8_lossy(&stderr).into_owned(), backend: tool.backend_name(), stdout_capped: capped })
}
// 상한까지 읽기 (초과 여부 반환)
fn read_pipe<R: Read>(pipe: R) -> std::io::Result<(Vec<u8>, bool)> {
  let mut bytes = Vec::new();
  pipe.take(MAX_PIPE_BYTES + 1).read_to_end(&mut bytes)?;
  let capped = bytes.len() as u64 > MAX_PIPE_BYTES;
  bytes.truncate(MAX_PIPE_BYTES as usize);
  Ok((bytes, capped))
}
fn join_pipe(handle: thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>) -> Result<(Vec<u8>, bool), String> {
  handle.join().map_err(|_| "Failed to join external tool reader thread".to_string())?.map_err(|error| format!("Failed to read external tool output: {error}"))
}
#[cfg(test)]
mod tests {
  use super::*;

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
}
