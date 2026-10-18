//! Control interface: Unix socket + line protocol (no serde).
//!
//! Request: `1<TAB>command<TAB>args...`; response: `OK` followed by `M<TAB>key<TAB>value`
//! metadata lines and `R<TAB>fields...` data lines, or `ERR<TAB>code<TAB>message`.
//!
//! Every field is percent-escaped (`%25`/`%09`/`%0A`/`%0D`) so that one line is one
//! message and TAB-separated messages are unambiguous. The protocol stays backward
//! compatible: a version change is rejected.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};

/// Current protocol version.
const PROTOCOL_VERSION: &str = "1";
/// Upper bound on a Unix socket path (`sun_path` is 108 bytes; leave some headroom).
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_SOCKET_PATH: usize = 100;
/// Default timeout for a single control call.
#[cfg_attr(not(unix), allow(dead_code))]
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on the bytes of one request, so a malicious or broken client cannot grow memory forever.
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// Cap on the bytes of one response (client-side limit).
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Cap on concurrently handled connections.
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_CONNECTIONS: usize = 32;
/// End-of-response marker; the client stops only once it reads this.
#[cfg_attr(not(unix), allow(dead_code))]
const END_MARKER: &str = "END";

/// Percent-escape a single field (only characters that would break the line/TAB structure).
fn encode_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

/// Inverse of `encode_field`; invalid escapes are kept verbatim.
fn decode_field(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' && index + 2 < bytes.len() {
            let hi = (bytes[index + 1] as char).to_digit(16);
            let lo = (bytes[index + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                index += 3;
                continue;
            }
        }
        out.push(byte);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

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
        let mut out = String::new();
        out.push_str(PROTOCOL_VERSION);
        out.push('\t');
        out.push_str(&encode_field(&self.command));
        for arg in &self.args {
            out.push('\t');
            out.push_str(&encode_field(arg));
        }
        out
    }

    pub fn decode(line: &str) -> Result<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(Error::protocol("控制请求为空")
                .with_hint("客户端与服务端版本可能不一致，请升级 automountd"));
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let version = fields[0].trim();
        if version != PROTOCOL_VERSION {
            return Err(Error::protocol(format!(
                "不支持的控制协议版本 '{version}'（期望 {PROTOCOL_VERSION}）"
            ))
            .with_hint("请确保 automountd 服务与命令行工具版本一致"));
        }
        let command = fields
            .get(1)
            .map(|field| decode_field(field))
            .unwrap_or_default();
        if command.trim().is_empty() {
            return Err(Error::protocol("控制请求缺少命令名"));
        }
        let args = fields
            .iter()
            .skip(2)
            .map(|field| decode_field(field))
            .collect();
        Ok(Self { command, args })
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
        if !self.ok {
            return format!(
                "ERR\t{}\t{}",
                encode_field(&self.code),
                encode_field(&self.message)
            );
        }
        let mut out = String::from("OK");
        for (key, value) in &self.meta {
            out.push('\n');
            out.push_str("M\t");
            out.push_str(&encode_field(key));
            out.push('\t');
            out.push_str(&encode_field(value));
        }
        for row in &self.rows {
            out.push('\n');
            out.push('R');
            for field in row {
                out.push('\t');
                out.push_str(&encode_field(field));
            }
        }
        out
    }

    pub fn decode(text: &str) -> Result<Self> {
        let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
        let header = loop {
            match lines.next() {
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => break line,
                None => {
                    return Err(Error::protocol("控制接口返回空响应")
                        .with_hint("守护进程可能已退出，请查看服务日志"))
                }
            }
        };
        let fields: Vec<&str> = header.split('\t').collect();
        match fields[0] {
            "OK" => {
                let mut response = Self::success();
                for line in lines {
                    if line.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = line.split('\t').collect();
                    match parts[0] {
                        "M" if parts.len() >= 3 => response
                            .meta
                            .push((decode_field(parts[1]), decode_field(&parts[2..].join("\t")))),
                        "R" => response
                            .rows
                            .push(parts.iter().skip(1).map(|f| decode_field(f)).collect()),
                        // Unknown lines are ignored so the protocol can grow without
                        // breaking old clients.
                        _ => {}
                    }
                }
                Ok(response)
            }
            "ERR" => {
                let code = fields
                    .get(1)
                    .map(|field| decode_field(field))
                    .filter(|code| !code.is_empty())
                    .unwrap_or_else(|| "error".to_string());
                let message = decode_field(&fields.get(2..).unwrap_or(&[]).join("\t"));
                Ok(Self {
                    ok: false,
                    code,
                    message,
                    ..Default::default()
                })
            }
            other => Err(Error::protocol(format!("无法识别的控制响应: {other}"))
                .with_hint("请升级 automountd 客户端与服务端到同一版本")),
        }
    }
}

/// One call received by the server; the daemon answers through `reply` once it is done.
pub struct CtlJob {
    pub request: CtlRequest,
    pub peer_uid: u32,
    pub reply: Sender<CtlResponse>,
}

/// Listen on the control socket and forward requests to the daemon main loop.
pub struct CtlServer {
    path: PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    jobs: Sender<CtlJob>,
    #[cfg_attr(not(unix), allow(dead_code))]
    stop: Arc<AtomicBool>,
    #[cfg_attr(not(unix), allow(dead_code))]
    job_timeout: Arc<AtomicU64>,
    #[cfg_attr(not(unix), allow(dead_code))]
    active: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
    #[cfg(unix)]
    listener: Option<std::os::unix::net::UnixListener>,
}

impl CtlServer {
    pub fn new(config: &Config, jobs: Sender<CtlJob>) -> Self {
        // A mount can take longer than the default 30 seconds (large spinning disks,
        // journal replay), so the server must be able to wait it out.
        let last_resort = config
            .mount_timeout_seconds
            .max(config.unmount_timeout_seconds)
            .saturating_add(20)
            .max(DEFAULT_JOB_TIMEOUT.as_secs());
        Self {
            path: config.ctl_socket.clone(),
            jobs,
            stop: Arc::new(AtomicBool::new(false)),
            job_timeout: Arc::new(AtomicU64::new(last_resort)),
            active: Arc::new(AtomicUsize::new(0)),
            handle: None,
            #[cfg(unix)]
            listener: None,
        }
    }

    pub fn start(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            self.start_unix()
        }
        #[cfg(not(unix))]
        {
            Err(
                Error::unsupported("当前平台不支持控制接口（automountd 仅支持 Linux/Unix）")
                    .with_hint("请在 Linux 上运行本程序；Windows 上仅可用于开发与 dry-run 测试"),
            )
        }
    }

    #[cfg(unix)]
    fn start_unix(&mut self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        if self.handle.is_some() {
            return Err(Error::config(format!(
                "控制接口已在 {} 上运行",
                self.path.display()
            )));
        }
        let path_len = self.path.as_os_str().len();
        if path_len > MAX_SOCKET_PATH {
            return Err(Error::config(format!(
                "控制 socket 路径过长（{path_len} 字节，上限 {MAX_SOCKET_PATH}）: {}",
                self.path.display()
            ))
            .with_hint("请把 ctl_socket 改到更短的目录，例如 /run/automountd/control.sock"));
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        Error::new(
                            ErrorKind::Io,
                            format!("无法创建控制 socket 目录 {}: {err}", parent.display()),
                        )
                        .with_hint("请确认当前用户对上级目录有写权限")
                    })?;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
        // An abnormal exit may leave a dead socket behind, so clean up first; if something
        // still listens there, refuse to start rather than let two daemons fight over the
        // same interface.
        if self.path.exists() {
            if std::os::unix::net::UnixStream::connect(&self.path).is_ok() {
                return Err(Error::config(format!(
                    "控制 socket {} 上已有守护进程在监听",
                    self.path.display()
                ))
                .with_hint("请先停止已有实例: sudo systemctl stop automountd"));
            }
            std::fs::remove_file(&self.path).map_err(|err| {
                Error::new(
                    ErrorKind::Io,
                    format!("无法删除陈旧的控制 socket {}: {err}", self.path.display()),
                )
            })?;
            crate::logging::debug(format!("已清理陈旧的控制 socket {}", self.path.display()));
        }
        let listener = std::os::unix::net::UnixListener::bind(&self.path).map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法创建控制 socket {}: {err}", self.path.display()),
            )
            .with_hint("请确认目录存在，且没有其他 automountd 实例在运行")
        })?;
        // Let regular users connect; the real permission check is done by
        // ctl_allowed_users + peer_uid.
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o666));
        listener.set_nonblocking(true).map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法把控制 socket 设为非阻塞: {err}"),
            )
        })?;
        let worker = listener
            .try_clone()
            .map_err(|err| Error::new(ErrorKind::Io, format!("无法复制控制 socket 句柄: {err}")))?;
        self.listener = Some(listener);
        // After stop(), start() may be called again.
        self.stop.store(false, Ordering::SeqCst);

        let jobs = self.jobs.clone();
        let stop = Arc::clone(&self.stop);
        let active = Arc::clone(&self.active);
        let job_timeout = Arc::clone(&self.job_timeout);
        let handle = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match worker.accept() {
                    Ok((stream, _addr)) => {
                        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                            crate::logging::warn("控制接口连接数已达上限，拒绝新连接");
                            drop(stream);
                            continue;
                        }
                        active.fetch_add(1, Ordering::SeqCst);
                        let jobs = jobs.clone();
                        let active_for_thread = Arc::clone(&active);
                        let timeout_for_thread = Arc::clone(&job_timeout);
                        let spawned = std::thread::Builder::new()
                            .name("automountd-ctl".to_string())
                            .spawn(move || {
                                let timeout = Duration::from_secs(
                                    timeout_for_thread.load(Ordering::SeqCst).max(1),
                                );
                                handle_connection(stream, jobs, timeout);
                                active_for_thread.fetch_sub(1, Ordering::SeqCst);
                            });
                        if let Err(err) = spawned {
                            active.fetch_sub(1, Ordering::SeqCst);
                            crate::logging::error(format!("无法创建控制连接线程: {err}"));
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        crate::logging::error(format!("控制 socket 接受连接失败: {err}"));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });
        self.handle = Some(handle);
        crate::logging::info(format!("控制接口已就绪: {}", self.path.display()));
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        {
            // Release the listener before removing the path, so new connections do not
            // land on a deleted inode.
            self.listener.take();
            if self.path.exists() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sync the server-side wait cap after a config reload, avoiding the case where the
    /// client is still waiting while the server has already timed out.
    pub fn set_job_timeout(&mut self, timeout: Duration) {
        self.job_timeout
            .store(timeout.as_secs().max(1), Ordering::SeqCst);
    }
}

/// Read the peer uid (Linux `SO_PEERCRED`).
///
/// Failure must return an error rather than 0: 0 is root, and failing open would hand
/// any local user the highest privileges.
#[cfg(unix)]
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::os::raw::{c_int, c_void};
        use std::os::unix::io::AsRawFd;

        #[repr(C)]
        struct UCred {
            pid: i32,
            uid: u32,
            gid: u32,
        }

        extern "C" {
            fn getsockopt(
                fd: c_int,
                level: c_int,
                optname: c_int,
                optval: *mut c_void,
                optlen: *mut u32,
            ) -> c_int;
        }

        const SOL_SOCKET: c_int = 1;
        const SO_PEERCRED: c_int = 17;

        let mut cred = UCred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<UCred>() as u32;
        let rc = unsafe {
            getsockopt(
                stream.as_raw_fd(),
                SOL_SOCKET,
                SO_PEERCRED,
                &mut cred as *mut UCred as *mut c_void,
                &mut len,
            )
        };
        if rc != 0 {
            return Err(Error::new(
                ErrorKind::Permission,
                format!(
                    "无法读取控制连接的对端身份: {}",
                    std::io::Error::last_os_error()
                ),
            )
            .with_hint("出于安全考虑已拒绝该连接，请检查内核/沙箱配置"));
        }
        if len as usize != std::mem::size_of::<UCred>() {
            return Err(Error::new(
                ErrorKind::Permission,
                "控制连接的对端身份信息不完整",
            ));
        }
        Ok(cred.uid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = stream;
        Err(
            Error::new(ErrorKind::Permission, "当前平台无法校验控制连接身份")
                .with_hint("automountd 控制接口只在 Linux 上启用"),
        )
    }
}

#[cfg(unix)]
fn handle_connection(
    mut stream: std::os::unix::net::UnixStream,
    jobs: Sender<CtlJob>,
    job_timeout: Duration,
) {
    use std::io::{BufRead, BufReader, Read, Write};

    // Wait at most 10 seconds for the first packet, so connect-only connections cannot
    // hog a thread for long.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let write_response = |stream: &mut std::os::unix::net::UnixStream, response: &CtlResponse| {
        let mut text = response.encode();
        text.push('\n');
        text.push_str(END_MARKER);
        text.push('\n');
        if let Err(err) = stream.write_all(text.as_bytes()) {
            crate::logging::warn(format!("写回控制响应失败: {err}"));
        }
        let _ = stream.flush();
    };

    let uid = match peer_uid(&stream) {
        Ok(uid) => uid,
        Err(err) => {
            write_response(
                &mut stream,
                &CtlResponse::failure("permission", &err.to_string()),
            );
            return;
        }
    };

    let reader_stream = match stream.try_clone() {
        Ok(handle) => handle,
        Err(err) => {
            crate::logging::warn(format!("无法复制控制连接句柄: {err}"));
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut limited = reader.by_ref().take(MAX_REQUEST_BYTES);
    let mut line = String::new();

    let response = match limited.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) => {
            if !line.ends_with('\n') {
                CtlResponse::failure("protocol", "控制请求过长或缺少换行（上限 64 KiB）")
            } else {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                match CtlRequest::decode(trimmed) {
                    Ok(request) => {
                        crate::logging::debug(format!(
                            "收到控制请求: {} {:?}（uid={uid}）",
                            request.command, request.args
                        ));
                        let (reply, receiver) = std::sync::mpsc::channel();
                        if let Err(err) = jobs.send(CtlJob {
                            request,
                            peer_uid: uid,
                            reply,
                        }) {
                            crate::logging::error(format!("无法把控制请求转交主循环: {err}"));
                            CtlResponse::failure("not-running", "守护进程主循环已退出")
                        } else {
                            match receiver.recv_timeout(job_timeout) {
                                Ok(response) => response,
                                Err(_) => CtlResponse::failure(
                                    "timeout",
                                    &format!(
                                        "等待守护进程处理请求超时（{} 秒）；操作可能仍在进行，请用 status 确认",
                                        job_timeout.as_secs()
                                    ),
                                ),
                            }
                        }
                    }
                    Err(err) => CtlResponse::failure("protocol", &err.message),
                }
            }
        }
        Err(err) => {
            crate::logging::warn(format!("读取控制请求失败: {err}"));
            return;
        }
    };

    write_response(&mut stream, &response);
}

/// Connect to the control socket, giving up after the timeout (std has no
/// `connect_timeout`, so it is wrapped in a thread).
#[cfg(unix)]
fn connect_with_timeout(
    socket: &Path,
    timeout: Duration,
) -> Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;

    let wanted = socket.to_path_buf();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(UnixStream::connect(&wanted));
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(Error::new(
            ErrorKind::NotRunning,
            format!(
                "无法连接 automountd 控制 socket {}: {err}",
                socket.display()
            ),
        )
        .with_hint("请先启动服务: sudo systemctl start automountd")),
        Err(_) => Err(Error::new(
            ErrorKind::NotRunning,
            format!(
                "连接 automountd 控制 socket {} 超时（{} 秒）",
                socket.display(),
                timeout.as_secs_f32().max(0.1)
            ),
        )
        .with_hint("守护进程可能正在启动，请稍后重试")),
    }
}

/// Read one complete response: multi-line content up to the `END` line (or peer close).
#[cfg_attr(not(unix), allow(dead_code))]
fn read_response_text<R: std::io::BufRead>(reader: &mut R) -> Result<String> {
    let mut text = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|err| {
            Error::protocol(format!("读取控制响应失败: {err}"))
                .with_hint("守护进程可能已退出或响应超时")
        })?;
        if read == 0 {
            break;
        }
        if line.trim_end_matches(['\r', '\n']) == END_MARKER {
            break;
        }
        if text.len() + line.len() > MAX_RESPONSE_BYTES {
            return Err(Error::protocol("控制接口响应过大，已中止"));
        }
        text.push_str(&line);
    }
    if text.trim().is_empty() {
        return Err(Error::protocol("控制接口返回空响应"));
    }
    Ok(text)
}

/// Client side: send one request and wait for the response.
pub fn request(socket: &Path, request: &CtlRequest, timeout: Duration) -> Result<CtlResponse> {
    #[cfg(unix)]
    {
        use std::io::{BufReader, Write};

        let stream = connect_with_timeout(socket, timeout)?;
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));

        let mut writer = stream
            .try_clone()
            .map_err(|err| Error::protocol(format!("无法复制控制连接: {err}")))?;
        let mut line = request.encode();
        line.push('\n');
        writer
            .write_all(line.as_bytes())
            .and_then(|_| writer.flush())
            .map_err(|err| {
                Error::protocol(format!("发送控制请求失败: {err}"))
                    .with_hint("守护进程可能已退出，请检查服务状态")
            })?;

        let mut reader = BufReader::new(stream);
        // The response is multi-line: the server terminates it with an END line
        // (an old version or an abnormal exit leaves us with EOF instead).
        let text = read_response_text(&mut reader)?;
        CtlResponse::decode(&text)
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, request, timeout);
        Err(
            Error::new(ErrorKind::NotRunning, "当前平台不支持 Unix socket 控制接口")
                .with_hint("automountd 的控制接口只在 Linux 上可用"),
        )
    }
}

/// Whether the daemon is running (the socket can be connected to).
pub fn is_running(socket: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(socket).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
        false
    }
}

/// Default path of the control socket (so the CLI does not build the string itself).
pub fn default_socket(config: &Config) -> PathBuf {
    config.ctl_socket.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_multiline_response_and_end_marker() {
        let text = "OK\nM\tcount\t2\nR\t/dev/sdb1\tU 盘\nR\t/dev/sdc1\t100%\nEND\n";
        let mut cursor = std::io::Cursor::new(text.as_bytes());
        let body = read_response_text(&mut cursor).unwrap();
        assert!(body.contains("R\t/dev/sdb1"));
        let response = CtlResponse::decode(&body).unwrap();
        assert_eq!(response.meta_value("count"), Some("2"));
        assert_eq!(response.rows.len(), 2);
        assert_eq!(response.rows[0][1], "U 盘");
    }

    #[test]
    fn reads_response_without_end_marker_at_eof() {
        let text = "OK\nM\tk\tv\n";
        let mut cursor = std::io::Cursor::new(text.as_bytes());
        let response = CtlResponse::decode(&read_response_text(&mut cursor).unwrap()).unwrap();
        assert_eq!(response.meta_value("k"), Some("v"));
    }

    #[test]
    fn rejects_empty_response() {
        let mut cursor = std::io::Cursor::new("".as_bytes());
        assert!(read_response_text(&mut cursor).is_err());
    }

    #[test]
    fn request_encode_decode_roundtrip() {
        let request = CtlRequest::new("mount")
            .arg("/dev/sdb1")
            .arg("卷标\t带制表符")
            .arg("多行\n内容")
            .arg("100% 完成");
        let encoded = request.encode();
        assert!(encoded.starts_with("1\tmount\t"));
        assert!(!encoded.contains('\n'), "编码后不能含换行: {encoded}");
        // A TAB inside a field must be escaped, so no extra fields appear
        // (version + command + 4 args)
        assert_eq!(encoded.split('\t').count(), 6);
        let decoded = CtlRequest::decode(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.args[1], "卷标\t带制表符");
        assert_eq!(decoded.args[2], "多行\n内容");
    }

    #[test]
    fn request_decode_rejects_bad_input() {
        let err = CtlRequest::decode("2\tlist").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Protocol);
        assert!(err.message.contains("协议版本"));

        let err = CtlRequest::decode("1\t\t").unwrap_err();
        assert!(err.message.contains("命令名"));

        assert!(CtlRequest::decode("").is_err());
        // A trailing newline must be tolerated (the socket is read line by line)
        assert_eq!(
            CtlRequest::decode("1\tlist\n").unwrap(),
            CtlRequest::new("list")
        );
    }

    #[test]
    fn response_encode_decode_roundtrip() {
        let response = CtlResponse::success()
            .with_meta("count", "2")
            .with_meta("说明", "制表\t与换行\n都在这里")
            .with_row(vec!["/dev/sdb1".to_string(), "U 盘\tA".to_string()])
            .with_row(vec!["/dev/sdc1".to_string(), "100%".to_string()]);
        let text = response.encode();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "OK");
        assert_eq!(lines.len(), 5);
        let decoded = CtlResponse::decode(&text).unwrap();
        assert_eq!(decoded, response);
        assert_eq!(decoded.meta_value("说明"), Some("制表\t与换行\n都在这里"));
        assert_eq!(decoded.rows[1][1], "100%");

        // Empty response body
        assert_eq!(
            CtlResponse::decode(&CtlResponse::success().encode()).unwrap(),
            CtlResponse::success()
        );
        // Unknown lines are ignored, keeping room for future extensions
        let decoded = CtlResponse::decode("OK\nX\t扩展\t数据\nM\tk\tv\n").unwrap();
        assert_eq!(decoded.meta_value("k"), Some("v"));
        assert!(decoded.rows.is_empty());
        // Blank lines (e.g. one extra trailing newline) must be ignored too
        assert!(CtlResponse::decode("OK\n\n\n").unwrap().ok);
    }

    #[test]
    fn error_response_roundtrip() {
        let response = CtlResponse::failure("permission", "需要 root 权限\t请用 sudo");
        let text = response.encode();
        assert!(text.starts_with("ERR\tpermission\t"));
        assert!(!text.contains('\n'));
        let decoded = CtlResponse::decode(&text).unwrap();
        assert!(!decoded.ok);
        assert_eq!(decoded.code, "permission");
        assert_eq!(decoded.message, "需要 root 权限\t请用 sudo");
        assert!(decoded.meta.is_empty() && decoded.rows.is_empty());

        // Unrecognised response header
        assert!(CtlResponse::decode("WAT\t?").is_err());
        assert!(CtlResponse::decode("").is_err());
    }

    #[test]
    fn percent_escapes_roundtrip() {
        for text in ["100%", "%25", "a\tb\nc\rd", "%zz 非法", "中文 卷标%"] {
            assert_eq!(decode_field(&encode_field(text)), text);
        }
        assert_eq!(encode_field("100%"), "100%25");
        assert_eq!(decode_field("%zz"), "%zz");
    }

    #[test]
    fn request_to_missing_socket_fails() {
        let temp = crate::testutil::TempDir::new("ctl-missing");
        let socket = temp.join("absent.sock");
        let err = request(&socket, &CtlRequest::new("status"), Duration::from_secs(2)).unwrap_err();
        assert!(!err.message.is_empty());
        assert!(!is_running(&socket));
    }

    #[test]
    fn socket_path_defaults_to_config() {
        let mut config = Config::default();
        config.ctl_socket = PathBuf::from("/run/automountd/custom.sock");
        let server = CtlServer::new(&config, std::sync::mpsc::channel().0);
        assert_eq!(server.path(), Path::new("/run/automountd/custom.sock"));
        assert_eq!(default_socket(&config), config.ctl_socket);
    }

    #[cfg(unix)]
    #[test]
    fn server_serves_one_request() {
        let temp = crate::testutil::TempDir::new("ctl-server");
        let mut config = Config::default();
        config.ctl_socket = temp.join("control.sock");
        let (jobs, receiver) = std::sync::mpsc::channel::<CtlJob>();
        let mut server = CtlServer::new(&config, jobs);
        server.start().expect("启动控制接口失败");
        assert!(is_running(&config.ctl_socket));

        let waiter = std::thread::spawn(move || {
            let job = receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("没有收到控制请求");
            assert_eq!(job.request.command, "list");
            assert_eq!(job.request.args, vec!["--all".to_string()]);
            job.reply
                .send(
                    CtlResponse::success()
                        .with_meta("count", "1")
                        .with_row(vec!["sdb1".to_string(), "U 盘\tA".to_string()]),
                )
                .expect("回写响应失败");
        });

        let response = request(
            &config.ctl_socket,
            &CtlRequest::new("list").arg("--all"),
            Duration::from_secs(5),
        )
        .expect("控制请求失败");
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.meta_value("count"), Some("1"));
        assert_eq!(response.rows.len(), 1);
        assert_eq!(response.rows[0][1], "U 盘\tA");
        waiter.join().unwrap();

        // The server rejects the invalid request but must not block
        let bad = request(
            &config.ctl_socket,
            &CtlRequest::new("status"),
            Duration::from_secs(5),
        );
        assert!(bad.is_ok(), "第二个请求也应收到协议层回复");

        server.stop();
        assert!(!config.ctl_socket.exists(), "stop 后应删除 socket 文件");
        assert!(!is_running(&config.ctl_socket));
    }
}
