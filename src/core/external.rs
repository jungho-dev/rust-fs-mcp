//! external.rs
//! core::external
//!
//! Wrapper that spawns external CLIs (ripgrep, fd, git) resolved from PATH and collects stdout / stderr.
//! Uses reader threads and an mpsc channel to handle EOF and timeout in a single loop.
//!

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
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
  // External tools are resolved from PATH.
  fn resolve_command(self) -> String {
    self.command_name().to_string()
  }
}
#[derive(Debug)]
pub struct ToolOutput {
  pub status_code: Option<i32>,
  pub stdout: String,
  pub stderr: String,
  pub backend: &'static str,
}
// Runs a PATH command and returns its collected stdout / stderr.
pub fn run_external(tool: ExternalTool, args: &[String], cwd: Option<&Path>, timeout_ms: Option<u64>) -> Result<ToolOutput, String> {
  let timeout = Duration::from_millis(timeout_ms.unwrap_or(120_000));
  run_path(tool, args, cwd, timeout)
}
fn run_path(tool: ExternalTool, args: &[String], cwd: Option<&Path>, timeout: Duration) -> Result<ToolOutput, String> {
  let mut command = Command::new(tool.resolve_command());
  command.args(args);
  command.stdin(Stdio::null());
  command.stdout(Stdio::piped());
  command.stderr(Stdio::piped());

  if let Some(cwd) = cwd {
    command.current_dir(cwd);
  }
  let mut child = command.spawn().map_err(|error| format!("Failed to start {} from PATH: {error}. Ensure '{}' is installed and on PATH.", tool.backend_name(), tool.command_name()))?;
  let stdout = child.stdout.take().ok_or_else(|| "Failed to capture stdout".to_string())?;
  let stderr = child.stderr.take().ok_or_else(|| "Failed to capture stderr".to_string())?;
  // Drops the 5ms busy-poll (`try_wait + sleep`) and uses a channel so reader threads
  // wake the main loop immediately on pipe EOF. Once both pipes close the child is effectively
  // done so child.wait() returns at once. Timeout is handled by mpsc's recv_timeout in one go.
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
    return Err(format!("{} timed out after {}ms", tool.backend_name(), timeout.as_millis()));
  }
  let status = match child.wait() {
    Ok(status) => status,
    Err(error) => {
      return Err(format!("Failed to wait for {}: {error}", tool.backend_name()));
    }
  };

  let stdout = join_pipe(stdout_handle)?;
  let stderr = join_pipe(stderr_handle)?;

  Ok(ToolOutput { status_code: status.code(), stdout: String::from_utf8_lossy(&stdout).into_owned(), stderr: String::from_utf8_lossy(&stderr).into_owned(), backend: tool.backend_name() })
}
fn read_pipe<R: Read>(mut pipe: R) -> std::io::Result<Vec<u8>> {
  let mut bytes = Vec::new();
  pipe.read_to_end(&mut bytes)?;
  Ok(bytes)
}
fn join_pipe(handle: thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>, String> {
  handle.join().map_err(|_| "Failed to join external tool reader thread".to_string())?.map_err(|error| format!("Failed to read external tool output: {error}"))
}
