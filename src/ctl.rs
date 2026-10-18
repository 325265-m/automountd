//! 控制接口：Unix socket + 行协议（JSON 无关，零依赖）。
//!
//! 请求：`1<TAB>命令<TAB>参数...`；响应：`OK` 后跟 `M<TAB>键<TAB>值` 元信息行
//! 与 `R<TAB>字段...` 数据行，或 `ERR<TAB>代码<TAB>消息`。
//!
//! 各字段使用百分号转义（`%25`/`%09`/`%0A`/`%0D`），保证一行一条消息、
//! 一条消息内以 TAB 分隔时不产生歧义。协议保持向后兼容：版本号变化即拒绝。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};

/// 当前协议版本。
const PROTOCOL_VERSION: &str = "1";
/// Unix socket 路径长度上限（`sun_path` 为 108 字节，留出余量）。
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_SOCKET_PATH: usize = 100;
/// 单次控制调用的超时时间。
#[cfg_attr(not(unix), allow(dead_code))]
const JOB_TIMEOUT: Duration = Duration::from_secs(30);

/// 单个字段的百分号转义（只转义会破坏行/TAB 结构的字符）。
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

/// `encode_field` 的逆过程；非法转义原样保留。
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
                        // 未知行忽略，便于未来扩展而不破坏旧客户端。
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

/// 服务端收到的一次调用；守护进程处理完后通过 `reply` 返回。
pub struct CtlJob {
    pub request: CtlRequest,
    pub peer_uid: u32,
    pub reply: Sender<CtlResponse>,
}

/// 监听控制 socket，把请求转发给守护进程主循环。
pub struct CtlServer {
    path: PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    jobs: Sender<CtlJob>,
    #[cfg_attr(not(unix), allow(dead_code))]
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    #[cfg(unix)]
    listener: Option<std::os::unix::net::UnixListener>,
}

impl CtlServer {
    pub fn new(config: &Config, jobs: Sender<CtlJob>) -> Self {
        Self {
            path: config.ctl_socket.clone(),
            jobs,
            stop: Arc::new(AtomicBool::new(false)),
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
                    let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
        // 上次异常退出可能留下死 socket，先清理；若仍有人监听则拒绝启动，
        // 避免两个守护进程抢同一个接口。
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
        // 允许普通用户连接，真正的权限检查由 ctl_allowed_users + peer_uid 完成。
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o666));
        listener.set_nonblocking(true).map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法把控制 socket 设为非阻塞: {err}"),
            )
        })?;
        let worker = listener.try_clone().map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法复制控制 socket 句柄: {err}"),
            )
        })?;
        self.listener = Some(listener);
        // stop() 之后允许再次 start()。
        self.stop.store(false, Ordering::SeqCst);

        let jobs = self.jobs.clone();
        let stop = Arc::clone(&self.stop);
        let handle = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match worker.accept() {
                    Ok((stream, _addr)) => {
                        let jobs = jobs.clone();
                        std::thread::spawn(move || handle_connection(stream, jobs));
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
            // 先释放监听器再删除路径，避免新连接落到已删除的 inode 上。
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
}

/// 读取对端 uid（Linux `SO_PEERCRED`）；其他平台返回 0。
#[cfg(unix)]
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> u32 {
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
        if rc == 0 {
            cred.uid
        } else {
            0
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = stream;
        0
    }
}

#[cfg(unix)]
fn handle_connection(mut stream: std::os::unix::net::UnixStream, jobs: Sender<CtlJob>) {
    use std::io::{BufRead, BufReader, Write};

    let _ = stream.set_read_timeout(Some(JOB_TIMEOUT));
    let _ = stream.set_write_timeout(Some(JOB_TIMEOUT));
    let uid = peer_uid(&stream);
    let reader_stream = match stream.try_clone() {
        Ok(handle) => handle,
        Err(err) => {
            crate::logging::warn(format!("无法复制控制连接句柄: {err}"));
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    let response = match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) => {
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
                        match receiver.recv_timeout(JOB_TIMEOUT) {
                            Ok(response) => response,
                            Err(_) => CtlResponse::failure(
                                "timeout",
                                "等待守护进程处理请求超时（30 秒）",
                            ),
                        }
                    }
                }
                Err(err) => CtlResponse::failure("protocol", &err.message),
            }
        }
        Err(err) => {
            crate::logging::warn(format!("读取控制请求失败: {err}"));
            return;
        }
    };

    let mut text = response.encode();
    text.push('\n');
    if let Err(err) = stream.write_all(text.as_bytes()) {
        crate::logging::warn(format!("写回控制响应失败: {err}"));
    }
    let _ = stream.flush();
}

/// 连接控制 socket，超时后放弃（std 没有 `connect_timeout`，用线程包一层）。
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
            format!("无法连接 automountd 控制 socket {}: {err}", socket.display()),
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

/// 客户端发送一次请求并等待响应。
pub fn request(socket: &Path, request: &CtlRequest, timeout: Duration) -> Result<CtlResponse> {
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Write};

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
        let mut text = String::new();
        reader.read_line(&mut text).map_err(|err| {
            Error::protocol(format!("读取控制响应失败: {err}"))
                .with_hint("守护进程可能已退出或响应超时")
        })?;
        if text.trim().is_empty() {
            return Err(Error::protocol("控制接口返回空响应"));
        }
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

/// 守护进程是否在运行（能连上 socket）。
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

/// 控制 socket 的默认路径（避免 CLI 直接拼字符串）。
pub fn default_socket(config: &Config) -> PathBuf {
    config.ctl_socket.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 字段内的 TAB 必须被转义，不能凭空多出字段（版本 + 命令 + 4 个参数）
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
        // 末尾换行应被容忍（socket 按行读取）
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

        // 空响应体
        assert_eq!(
            CtlResponse::decode(&CtlResponse::success().encode()).unwrap(),
            CtlResponse::success()
        );
        // 未知行忽略，兼容未来扩展
        let decoded = CtlResponse::decode("OK\nX\t扩展\t数据\nM\tk\tv\n").unwrap();
        assert_eq!(decoded.meta_value("k"), Some("v"));
        assert!(decoded.rows.is_empty());
        // 空行（例如末尾多一个换行）也应忽略
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

        // 无法识别的响应头
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

        // 服务端拒绝非法请求，不应阻塞
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
