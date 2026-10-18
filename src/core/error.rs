//! Unified error type.

use std::fmt;
use std::io;

/// Error category, which determines the process exit code and how the message is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Config,
    Io,
    Command,
    Device,
    Mount,
    NotMounted,
    Busy,
    Permission,
    NotRunning,
    Protocol,
    Unsupported,
}

impl ErrorKind {
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Permission => 4,
            ErrorKind::NotRunning => 3,
            _ => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Config => "配置错误",
            ErrorKind::Io => "IO 错误",
            ErrorKind::Command => "命令执行失败",
            ErrorKind::Device => "设备错误",
            ErrorKind::Mount => "挂载错误",
            ErrorKind::NotMounted => "未挂载",
            ErrorKind::Busy => "设备忙碌",
            ErrorKind::Permission => "权限不足",
            ErrorKind::NotRunning => "服务未运行",
            ErrorKind::Protocol => "通信错误",
            ErrorKind::Unsupported => "不支持",
        }
    }
}

/// An error with an optional troubleshooting hint.
#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub hint: Option<String>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub fn device(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Device, message)
    }

    pub fn mount(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Mount, message)
    }

    pub fn command(program: &str, detail: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Command,
            format!("命令 {program} 执行失败: {}", detail.into()),
        )
    }

    pub fn permission(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Permission, message)
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Error::new(ErrorKind::Io, value.to_string())
    }
}

/// Add context to an IO error.
pub fn io_context(context: impl fmt::Display, err: io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("{context}: {err}"))
}

pub type Result<T> = std::result::Result<T, Error>;

/// Aggregate several validation problems into a single configuration error.
pub fn config_problems(problems: &[String]) -> Error {
    let mut message = String::from("配置无效:");
    for problem in problems {
        message.push_str("\n  - ");
        message.push_str(problem);
    }
    Error::config(message)
}
