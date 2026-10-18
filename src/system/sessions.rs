//! 登录会话与桌面用户识别。
//!
//! 顺序：配置指定 -> sudo/pkexec 环境变量 -> loginctl 活动会话 -> 当前 uid -> 首个普通用户。

use std::collections::BTreeMap;
use std::time::Duration;

use crate::exec::CommandRunner;
use crate::logging;
use crate::model::UserInfo;
use crate::platform;

/// `/etc/passwd` 路径。
const PASSWD_PATH: &str = "/etc/passwd";
/// 查询用户时的命令超时（getent/loginctl 不应拖住守护进程）。
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// 普通用户 uid 下界。
const HUMAN_UID_MIN: u32 = 1000;
/// 普通用户 uid 上界（不含）。
const HUMAN_UID_MAX: u32 = 65534;
/// 依次尝试的代理环境变量（sudo / pkexec / doas）。
const PROXY_ENV_KEYS: [&str; 3] = ["SUDO_USER", "PKEXEC_USER", "DOAS_USER"];

/// 解析 `/etc/passwd` 风格的一行。
pub fn parse_passwd_line(line: &str) -> Option<UserInfo> {
    parse_passwd_fields(line).map(|(user, _shell)| user)
}

/// 解析一行并同时返回登录 shell（`first_human_user` 需要）。
fn parse_passwd_fields(line: &str) -> Option<(UserInfo, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // name:passwd:uid:gid:gecos:home:shell
    let fields: Vec<&str> = line.split(':').collect();
    if fields.len() < 7 {
        return None;
    }
    let name = fields[0].trim();
    if name.is_empty() {
        return None;
    }
    let uid = fields[2].trim().parse::<u32>().ok()?;
    let gid = fields[3].trim().parse::<u32>().ok()?;
    let home = fields[5].trim();
    let shell = fields[6].trim();
    Some((
        UserInfo {
            name: name.to_string(),
            uid,
            gid,
            home: if home.is_empty() {
                None
            } else {
                Some(home.to_string())
            },
        },
        shell.to_string(),
    ))
}

/// 按用户名查询（getent -> /etc/passwd）。
pub fn lookup_user(runner: &dyn CommandRunner, name: &str) -> Option<UserInfo> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some((user, _)) = getent_entries(runner, name)
        .into_iter()
        .find(|(user, _)| user.name == name)
    {
        return Some(user);
    }
    passwd_entries()
        .into_iter()
        .find(|(user, _)| user.name == name)
        .map(|(user, _)| user)
}

/// 按 uid 查询。
pub fn lookup_uid(runner: &dyn CommandRunner, uid: u32) -> Option<UserInfo> {
    let key = uid.to_string();
    if let Some((user, _)) = getent_entries(runner, &key)
        .into_iter()
        .find(|(user, _)| user.uid == uid)
    {
        return Some(user);
    }
    passwd_entries()
        .into_iter()
        .find(|(user, _)| user.uid == uid)
        .map(|(user, _)| user)
}

/// 返回应当拥有挂载点的桌面用户。
pub fn console_user(runner: &dyn CommandRunner, configured: Option<&str>) -> Option<UserInfo> {
    console_user_impl(runner, configured, &|key| std::env::var(key).ok())
}

/// 便于测试的内核实现：环境变量通过闭包注入。
fn console_user_impl(
    runner: &dyn CommandRunner,
    configured: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<UserInfo> {
    if let Some(name) = configured {
        let name = name.trim();
        if !name.is_empty() {
            match lookup_user(runner, name) {
                Some(user) => return Some(user),
                None => logging::warn(format!(
                    "配置中指定的用户 {name} 不存在，改为自动识别桌面用户"
                )),
            }
        }
    }

    for key in PROXY_ENV_KEYS {
        let Some(value) = env(key) else { continue };
        let name = value.trim().to_string();
        if name.is_empty() || name.eq_ignore_ascii_case("root") {
            continue;
        }
        if let Some(user) = lookup_user(runner, &name) {
            if user.uid != 0 {
                logging::debug(format!("由环境变量 {key} 识别到桌面用户 {name}"));
                return Some(user);
            }
        }
    }

    if let Some(value) = env("PKEXEC_UID") {
        if let Ok(uid) = value.trim().parse::<u32>() {
            if uid != 0 {
                if let Some(user) = lookup_uid(runner, uid) {
                    logging::debug(format!("由 PKEXEC_UID 识别到桌面用户 {uid}"));
                    return Some(user);
                }
            }
        }
    }

    if let Some(user) = active_session_user(runner) {
        return Some(user);
    }

    let uid = current_uid();
    if uid != 0 {
        if let Some(user) = lookup_uid(runner, uid) {
            return Some(user);
        }
    }

    first_human_user(runner)
}

/// 当前进程 uid。
pub fn current_uid() -> u32 {
    platform::euid()
}

/// 首选普通用户（uid >= 1000 且 shell 可登录）。
pub fn first_human_user(runner: &dyn CommandRunner) -> Option<UserInfo> {
    // 有意不使用 runner：/etc/passwd 是判断"人"的最可靠来源。
    let _ = runner;
    let text = std::fs::read_to_string(PASSWD_PATH).ok()?;
    pick_human_user(&text)
}

/// 从 `/etc/passwd` 文本里挑出 uid 最小的可登录普通用户。
fn pick_human_user(text: &str) -> Option<UserInfo> {
    let mut best: Option<UserInfo> = None;
    for line in text.lines() {
        let Some((user, shell)) = parse_passwd_fields(line) else {
            continue;
        };
        if user.uid < HUMAN_UID_MIN || user.uid >= HUMAN_UID_MAX {
            continue;
        }
        let shell = shell.to_ascii_lowercase();
        if shell.is_empty() || shell.ends_with("nologin") || shell.ends_with("false") {
            continue;
        }
        match &best {
            Some(current) if current.uid <= user.uid => {}
            _ => best = Some(user),
        }
    }
    best
}

/// 解析 `loginctl list-sessions --no-legend` 为 (session_id, uid, user)。
pub fn parse_session_rows(text: &str) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 3 {
            continue;
        }
        // 第 2 列必须是数字 uid，才能跳过表头与噪声行。
        if columns[1].is_empty() || !columns[1].chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        rows.push((
            columns[0].to_string(),
            columns[1].to_string(),
            columns[2].to_string(),
        ));
    }
    rows
}

/// 解析 `loginctl show-session ... -p Key` 输出。
pub fn parse_session_properties(text: &str) -> BTreeMap<String, String> {
    let mut properties = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        properties.insert(key.to_string(), value.trim().to_string());
    }
    properties
}

/// 通过 loginctl 找出图形会话（其次终端）对应的用户。
fn active_session_user(runner: &dyn CommandRunner) -> Option<UserInfo> {
    if !runner.which("loginctl") {
        logging::debug("未找到 loginctl，跳过活动会话探测");
        return None;
    }
    let output = match runner.run(
        "loginctl",
        &["list-sessions", "--no-legend", "--no-pager"],
        Some(LOOKUP_TIMEOUT),
    ) {
        Ok(output) if output.ok() => output,
        Ok(output) => {
            logging::debug(format!(
                "loginctl list-sessions 返回 {}: {}",
                output.status,
                output.stderr_trimmed()
            ));
            return None;
        }
        Err(err) => {
            logging::debug(format!("执行 loginctl list-sessions 出错: {err}"));
            return None;
        }
    };

    let mut best: Option<(u32, UserInfo)> = None;
    for (session, uid_text, name) in parse_session_rows(&output.stdout) {
        let Ok(uid) = uid_text.parse::<u32>() else {
            continue;
        };
        if uid == 0 {
            continue;
        }
        let properties = session_properties(runner, &session);
        if properties
            .get("Active")
            .map(|value| value.eq_ignore_ascii_case("yes"))
            != Some(true)
        {
            continue;
        }
        if properties
            .get("Remote")
            .map(|value| value.eq_ignore_ascii_case("yes"))
            == Some(true)
        {
            continue;
        }
        let score = session_score(&properties);
        // show-session 里通常不带用户名；查不到时用 list-sessions 的列兜底。
        let user = lookup_uid(runner, uid).unwrap_or_else(|| UserInfo {
            name: name.clone(),
            uid,
            gid: uid,
            home: None,
        });
        if user.uid == 0 {
            continue;
        }
        match &best {
            Some((best_score, _)) if *best_score >= score => {}
            _ => {
                logging::debug(format!("选中会话 {session} 的用户 {name}({uid})"));
                best = Some((score, user));
            }
        }
    }
    best.map(|(_, user)| user)
}

/// 会话优先级：图形会话 > 终端 > 其他；seat0 更高。
fn session_score(properties: &BTreeMap<String, String>) -> u32 {
    let kind = properties
        .get("Type")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let type_score = match kind.as_str() {
        "x11" | "wayland" | "mir" => 2,
        "tty" => 1,
        _ => 0,
    };
    let seat_score = match properties.get("Seat").map(String::as_str) {
        Some("seat0") => 1,
        _ => 0,
    };
    type_score * 10 + seat_score
}

/// 读取单个会话的属性；失败返回空表（不影响其它会话）。
fn session_properties(runner: &dyn CommandRunner, session: &str) -> BTreeMap<String, String> {
    const PROPERTIES: [&str; 4] = ["Active", "Remote", "Type", "Seat"];
    let mut args: Vec<&str> = vec!["show-session", session];
    for name in PROPERTIES {
        args.push("-p");
        args.push(name);
    }
    match runner.run("loginctl", &args, Some(LOOKUP_TIMEOUT)) {
        Ok(output) if output.ok() => parse_session_properties(&output.stdout),
        Ok(output) => {
            logging::debug(format!(
                "loginctl show-session {session} 返回 {}: {}",
                output.status,
                output.stderr_trimmed()
            ));
            BTreeMap::new()
        }
        Err(err) => {
            logging::debug(format!("执行 loginctl show-session {session} 出错: {err}"));
            BTreeMap::new()
        }
    }
}

/// 用 getent 查询 passwd 条目；getent 不可用或失败时返回空表。
fn getent_entries(runner: &dyn CommandRunner, key: &str) -> Vec<(UserInfo, String)> {
    if !runner.which("getent") {
        return Vec::new();
    }
    match runner.run("getent", &["passwd", key], Some(LOOKUP_TIMEOUT)) {
        Ok(output) if output.ok() => parse_passwd_text(&output.stdout),
        Ok(output) => {
            logging::debug(format!(
                "getent passwd {key} 返回 {}: {}",
                output.status,
                output.stderr_trimmed()
            ));
            Vec::new()
        }
        Err(err) => {
            logging::debug(format!("执行 getent passwd {key} 出错: {err}"));
            Vec::new()
        }
    }
}

/// 直接读取 `/etc/passwd`；文件缺失（如 Windows 开发机）时返回空表。
fn passwd_entries() -> Vec<(UserInfo, String)> {
    match std::fs::read_to_string(PASSWD_PATH) {
        Ok(text) => parse_passwd_text(&text),
        Err(_) => Vec::new(),
    }
}

fn parse_passwd_text(text: &str) -> Vec<(UserInfo, String)> {
    text.lines().filter_map(parse_passwd_fields).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CommandOutput, MockRunner};
    use crate::testutil::TempDir;

    #[test]
    fn parses_passwd_lines() {
        let user = parse_passwd_line("alice:x:1000:1000:Alice:/home/alice:/bin/bash").unwrap();
        assert_eq!(user.name, "alice");
        assert_eq!(user.uid, 1000);
        assert_eq!(user.gid, 1000);
        assert_eq!(user.home.as_deref(), Some("/home/alice"));

        // home 为空时是 None
        let user = parse_passwd_line("svc:x:999:999:::/usr/sbin/nologin").unwrap();
        assert_eq!(user.uid, 999);
        assert!(user.home.is_none());

        // home 为 / 这类少见写法也要保留
        let user = parse_passwd_line("nfs:x:998:998::/:/usr/sbin/nologin").unwrap();
        assert_eq!(user.home.as_deref(), Some("/"));

        // 非数字 uid、字段不足、空行都不是有效条目
        assert!(parse_passwd_line("bob:x:abc:100:Bob:/home/bob:/bin/sh").is_none());
        assert!(parse_passwd_line("bob:x:1000:100").is_none());
        assert!(parse_passwd_line("").is_none());
        assert!(parse_passwd_line("   ").is_none());
        assert!(parse_passwd_line(":x:1000:1000::/home/x:/bin/sh").is_none());
    }

    #[test]
    fn parses_session_rows_and_properties() {
        let text = "\
SESSION  UID  USER   SEAT  TTY
  2     1000 alice  seat0 tty2
  7     1001 bob    -     -
c3      1002 carol
notanumber x y z
";
        let rows = parse_session_rows(text);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            ("2".to_string(), "1000".to_string(), "alice".to_string())
        );
        assert_eq!(rows[1].2, "bob");
        assert_eq!(rows[2].1, "1002");

        let properties = parse_session_properties(
            "Active=yes\nRemote=no\nType=wayland\nSeat=seat0\n\nbroken line\n",
        );
        assert_eq!(properties.get("Active").map(String::as_str), Some("yes"));
        assert_eq!(properties.get("Type").map(String::as_str), Some("wayland"));
        assert_eq!(properties.get("Seat").map(String::as_str), Some("seat0"));
        assert!(!properties.contains_key("broken line"));
    }

    #[test]
    fn prefers_wayland_session_from_loginctl() {
        // 会话 2 是 tty，会话 5 是 wayland；uid 用 4242 避免落到真实 /etc/passwd。
        let runner = MockRunner::new()
            .respond(
                "loginctl",
                CommandOutput::success(
                    "  2 4242 ttyuser seat0 tty2\n  5 4242 deskuser seat0 tty5\n",
                ),
            )
            .respond(
                "loginctl",
                CommandOutput::success("Active=yes\nRemote=no\nType=tty\nSeat=seat0\n"),
            )
            .respond(
                "loginctl",
                CommandOutput::success("Active=yes\nRemote=no\nType=wayland\nSeat=seat0\n"),
            )
            .respond(
                "getent",
                CommandOutput::success("deskuser:x:4242:4242::/home/deskuser:/bin/bash\n"),
            );

        let user = console_user_impl(&runner, None, &|_| None).expect("应识别到图形会话用户");
        assert_eq!(user.name, "deskuser");
        assert_eq!(user.uid, 4242);
    }

    #[test]
    fn configured_user_wins_via_getent() {
        let runner = MockRunner::new().respond(
            "getent",
            CommandOutput::success("alice:x:4300:4300:Alice:/home/alice:/bin/bash\n"),
        );
        let user = console_user_impl(&runner, Some("alice"), &|_| None).expect("应返回配置用户");
        assert_eq!(user.name, "alice");
        assert_eq!(user.uid, 4300);
        // 配置用户命中后不应再去问 loginctl
        assert!(!runner.called("loginctl"));
    }

    #[test]
    fn env_proxy_user_skips_root() {
        let runner = MockRunner::new().respond(
            "getent",
            CommandOutput::success("alice:x:4301:4301::/home/alice:/bin/bash\n"),
        );
        let user = console_user_impl(&runner, None, &|key| match key {
            "SUDO_USER" => Some("root".to_string()),
            "PKEXEC_USER" => Some("alice".to_string()),
            _ => None,
        })
        .expect("应跳过 root 并使用 PKEXEC_USER");
        assert_eq!(user.name, "alice");
        assert_eq!(user.uid, 4301);
    }

    #[test]
    fn picks_first_human_user_from_passwd_text() {
        let dir = TempDir::new("sessions");
        let path = dir.write(
            "passwd",
            "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
alice:x:1000:1000::/home/alice:/bin/bash
bob:x:1001:1001::/home/bob:/bin/bash
svc:x:1002:1002::/srv:/usr/sbin/nologin
",
        );
        let text = std::fs::read_to_string(path).unwrap();
        let user = pick_human_user(&text).expect("应找到普通用户");
        assert_eq!(user.name, "alice");
        assert_eq!(user.uid, 1000);
        // 只有 root 与 nologin 用户时没有"人"可用
        assert!(pick_human_user("root:x:0:0::/root:/bin/bash\n").is_none());
        assert!(pick_human_user("svc:x:1000:1000::/srv:/usr/sbin/nologin\n").is_none());
    }

    #[test]
    fn lookup_falls_back_when_getent_missing() {
        let runner = MockRunner::new().missing("getent");
        // getent 缺失时只读 /etc/passwd；Windows 上没有该文件，因此只要求不 panic。
        let _ = lookup_user(&runner, "alice");
        let _ = lookup_uid(&runner, 1000);
        assert!(!runner.called("getent"));
        // current_uid 只是转发 platform::euid()
        assert_eq!(current_uid(), platform::euid());
    }
}
