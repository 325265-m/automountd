//! 控制接口：Unix socket + 行协议（JSON 无关，零依赖）。
//!
//! 请求：`1<TAB>命令<TAB>参数...`；响应：`OK` 后跟 `M<TAB>键<TAB>值` 元信息行
//! 与 `R<TAB>字段...` 数据行，或 `ERR<TAB>代码<TAB>消息`。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::config::Config;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlRequest {
    pub command: String,
    pub args: Vec<String>,
}

impl CtlRequest {
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_string(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    pub fn encode(&self) -> String {
        todo!()
    }

    pub fn decode(line: &str) -> Result<Self> {
        let _ = line;
        todo!()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CtlResponse {
    pub ok: bool,
    pub code: String,
    pub message: String,
    pub meta: Vec<(String, String)>,
    pub rows: Vec<Vec<String>>,
}

impl CtlResponse {
    pub fn success() -> Self {
        Self {
            ok: true,
            ..Default::default()
        }
    }

    pub fn failure(code: &str, message: &str) -> Self {
        Self {
            ok: false,
            code: code.to_string(),
            message: message.to_string(),
            ..Default::default()
        }
    }

    pub fn with_meta(mut self, key: &str, value: impl Into<String>) -> Self {
        self.meta.push((key.to_string(), value.into()));
        self
    }

    pub fn with_row(mut self, row: Vec<String>) -> Self {
        self.rows.push(row);
        self
    }

    pub fn meta_value(&self, key: &str) -> Option<&str> {
        self.meta
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    pub fn encode(&self) -> String {
        todo!()
    }

    pub fn decode(text: &str) -> Result<Self> {
        let _ = text;
        todo!()
    }
}

/// 服务端收到的一次调用；守护进程处理完后通过 `reply` 返回。
pub struct CtlJob {
    pub request: CtlRequest,
    pub peer_uid: u32,
    pub reply: Sender<CtlResponse>,
}

/// 监听控制 socket，把请求转发给守护进程主循环。
pub struct CtlServer {
    _private: (),
}

impl CtlServer {
    pub fn new(config: &Config, jobs: Sender<CtlJob>) -> Self {
        let _ = (config, jobs);
        todo!()
    }

    pub fn start(&mut self) -> Result<()> {
        todo!()
    }

    pub fn stop(&mut self) {
        todo!()
    }

    pub fn path(&self) -> &Path {
        todo!()
    }
}

/// 客户端发送一次请求并等待响应。
pub fn request(socket: &Path, request: &CtlRequest, timeout: Duration) -> Result<CtlResponse> {
    let _ = (socket, request, timeout);
    todo!()
}

/// 守护进程是否在运行（能连上 socket）。
pub fn is_running(socket: &Path) -> bool {
    let _ = socket;
    todo!()
}

/// 控制 socket 的默认路径（避免 CLI 直接拼字符串）。
pub fn default_socket(config: &Config) -> PathBuf {
    config.ctl_socket.clone()
}
