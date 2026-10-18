//! Abstraction over external command execution.
//!
//! Every call to `lsblk` / `mount` / `udevadm` goes through [`CommandRunner`];
//! tests substitute [`MockRunner`] so the real system is never touched.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{Error, ErrorKind, Result};

/// Result of a command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            status: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failure(status: i32, stderr: impl Into<String>) -> Self {
        Self {
            status,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn stderr_trimmed(&self) -> &str {
        self.stderr.trim()
    }

    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// Run external commands and check whether a program exists.
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], timeout: Option<Duration>)
        -> Result<CommandOutput>;

    fn which(&self, program: &str) -> bool;
}

/// Actually spawns operating-system processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<CommandOutput> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|err| {
            Error::new(ErrorKind::Command, format!("无法执行 {program}: {err}"))
                .with_hint("请确认 util-linux 等基础工具已安装，且 PATH 正确")
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || read_stream(stdout));
        let stderr_thread = std::thread::spawn(move || read_stream(stderr));

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::new(
                        ErrorKind::Command,
                        format!("等待 {program} 结束时出错: {err}"),
                    ));
                }
            }
            if let Some(limit) = timeout {
                if started.elapsed() > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::new(
                        ErrorKind::Command,
                        format!("命令 {program} 执行超时（{} 秒）", limit.as_secs()),
                    )
                    .with_hint("设备可能无响应；可检查 dmesg 或尝试重新插拔"));
                }
            }
            std::thread::sleep(Duration::from_millis(15));
        };

        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();
        Ok(CommandOutput {
            status: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    }

    fn which(&self, program: &str) -> bool {
        if program.contains('/') || program.contains('\\') {
            return std::path::Path::new(program).is_file();
        }
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return true;
            }
            #[cfg(windows)]
            {
                for ext in ["exe", "bat", "cmd", "com"] {
                    if candidate.with_extension(ext).is_file() {
                        return true;
                    }
                }
            }
        }
        false
    }
}

fn read_stream<R: Read>(stream: Option<R>) -> String {
    let mut buffer = Vec::new();
    if let Some(mut stream) = stream {
        let _ = stream.read_to_end(&mut buffer);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Fake runner with scripted responses, for use in unit tests.
#[derive(Debug, Default)]
pub struct MockRunner {
    missing: HashSet<String>,
    responses: Mutex<HashMap<String, VecDeque<CommandOutput>>>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl MockRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a program as missing (`which` returns false).
    pub fn missing(mut self, program: &str) -> Self {
        self.missing.insert(program.to_string());
        self
    }

    /// Append one canned response.
    pub fn respond(self, program: &str, output: CommandOutput) -> Self {
        self.push(program, output);
        self
    }

    pub fn push(&self, program: &str, output: CommandOutput) {
        let mut responses = self.responses.lock().unwrap();
        responses
            .entry(program.to_string())
            .or_default()
            .push_back(output);
    }

    pub fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn called(&self, program: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == program)
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl CommandRunner for MockRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _timeout: Option<Duration>,
    ) -> Result<CommandOutput> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
        ));
        let mut responses = self.responses.lock().unwrap();
        if let Some(queue) = responses.get_mut(program) {
            if let Some(output) = queue.pop_front() {
                return Ok(output);
            }
        }
        Ok(CommandOutput::success(""))
    }

    fn which(&self, program: &str) -> bool {
        !self.missing.contains(program)
    }
}
