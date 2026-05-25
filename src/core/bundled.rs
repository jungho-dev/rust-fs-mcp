use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundledTool {
    Rg,
    Fd,
    Bat,
}

impl BundledTool {
    fn file_name(self) -> &'static str {
        match self {
            Self::Rg => "rg.exe",
            Self::Fd => "fd.exe",
            Self::Bat => "bat.exe",
        }
    }

    pub fn backend_name(self) -> &'static str {
        match self {
            Self::Rg => "bundled-rg",
            Self::Fd => "bundled-fd",
            Self::Bat => "bundled-bat",
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

pub fn run_bundled(
    tool: BundledTool,
    args: &[String],
    cwd: Option<&Path>,
    timeout_ms: Option<u64>,
) -> Result<ToolOutput, String> {
    let path = resolve_bundled_tool(tool)?;
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(120_000));
    run_path(tool, &path, args, cwd, timeout)
}

pub fn resolve_bundled_tool(tool: BundledTool) -> Result<PathBuf, String> {
    let platform = platform_dir()?;
    let exe_dir = env::current_exe()
        .map_err(|error| format!("Failed to resolve current executable: {error}"))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Failed to resolve executable directory".to_string())?;
    let mut candidates = vec![
        exe_dir.join("tools").join(platform).join(tool.file_name()),
        exe_dir
            .join("..")
            .join("tools")
            .join(platform)
            .join(tool.file_name()),
    ];

    if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(
            PathBuf::from(manifest_dir)
                .join("vendor")
                .join("tools")
                .join(platform)
                .join(tool.file_name()),
        );
    }

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| format!("Bundled tool not found: {}", tool.file_name()))
}

fn platform_dir() -> Result<&'static str, String> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Ok("win32-x64")
    } else {
        Err("Bundled tools are only packaged for win32-x64".to_string())
    }
}

fn run_path(
    tool: BundledTool,
    path: &Path,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<ToolOutput, String> {
    let mut command = Command::new(path);
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to start bundled tool {} at {}: {error}",
            tool.backend_name(),
            path.display()
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
    let stdout_handle = thread::spawn(move || read_pipe(stdout));
    let stderr_handle = thread::spawn(move || read_pipe(stderr));
    let start = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} timed out after {}ms",
                        tool.backend_name(),
                        timeout.as_millis()
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Failed to wait for {}: {error}",
                    tool.backend_name()
                ));
            }
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
