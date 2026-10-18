//! Login sessions and desktop-user detection.
//!
//! Order: explicit config -> sudo/pkexec environment variables -> loginctl active session -> current uid -> first regular user.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::exec::CommandRunner;
use crate::logging;
use crate::model::UserInfo;
use crate::platform;

/// Path to `/etc/passwd`.
const PASSWD_PATH: &str = "/etc/passwd";
/// Command timeout for user lookups (getent/loginctl must not stall the daemon).
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Lower bound of a human user uid.
const HUMAN_UID_MIN: u32 = 1000;
/// Upper bound (exclusive) of a human user uid.
const HUMAN_UID_MAX: u32 = 65534;
/// Proxy environment variables tried in order (sudo / pkexec / doas).
const PROXY_ENV_KEYS: [&str; 3] = ["SUDO_USER", "PKEXEC_USER", "DOAS_USER"];

/// Parse one `/etc/passwd`-style line.
pub fn parse_passwd_line(line: &str) -> Option<UserInfo> {
    parse_passwd_fields(line).map(|(user, _shell)| user)
}

/// Parse a line and also return the login shell (`first_human_user` needs it).
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

/// Look up by user name (getent -> /etc/passwd).
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

/// Look up by uid.
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

/// Return the desktop user that should own the mount points.
pub fn console_user(runner: &dyn CommandRunner, configured: Option<&str>) -> Option<UserInfo> {
    console_user_impl(runner, configured, &|key| std::env::var(key).ok())
}

/// Testable core implementation: environment variables are injected through a closure.
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

/// uid of the current process.
pub fn current_uid() -> u32 {
    platform::euid()
}

/// Preferred regular user (uid >= 1000 with a login shell).
pub fn first_human_user(runner: &dyn CommandRunner) -> Option<UserInfo> {
    // Deliberately does not use the runner: /etc/passwd is the most reliable source
    // for telling who a "human" is.
    let _ = runner;
    let text = std::fs::read_to_string(PASSWD_PATH).ok()?;
    pick_human_user(&text)
}

/// Pick the regular user with the lowest uid that can log in from `/etc/passwd` text.
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

/// Parse `loginctl list-sessions --no-legend` into (session_id, uid, user).
pub fn parse_session_rows(text: &str) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 3 {
            continue;
        }
        // Column 2 must be a numeric uid, which is what skips the header and noise lines.
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

/// Parse `loginctl show-session ... -p Key` output.
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

/// Use loginctl to find the user behind the graphical session (falling back to a text console).
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
        // show-session usually carries no user name; fall back to the list-sessions column.
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

/// Session priority: graphical session > text console > other; seat0 ranks higher.
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

/// Read the properties of a single session; on failure return an empty map (other sessions are unaffected).
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

/// Look up passwd entries with getent; returns an empty map when getent is missing or fails.
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

/// Read `/etc/passwd` directly; returns an empty map when the file is missing (e.g. a Windows dev box).
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

        // An empty home is None
        let user = parse_passwd_line("svc:x:999:999:::/usr/sbin/nologin").unwrap();
        assert_eq!(user.uid, 999);
        assert!(user.home.is_none());

        // Rare forms such as home = / must be preserved too
        let user = parse_passwd_line("nfs:x:998:998::/:/usr/sbin/nologin").unwrap();
        assert_eq!(user.home.as_deref(), Some("/"));

        // Non-numeric uid, too few fields and blank lines are not valid entries
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
        // Session 2 is a tty, session 5 is wayland; uid 4242 avoids hitting the real /etc/passwd.
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
        // Once the configured user matches, loginctl must not be asked again
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
        // With only root and nologin users there is no "human" available
        assert!(pick_human_user("root:x:0:0::/root:/bin/bash\n").is_none());
        assert!(pick_human_user("svc:x:1000:1000::/srv:/usr/sbin/nologin\n").is_none());
    }

    #[test]
    fn lookup_falls_back_when_getent_missing() {
        let runner = MockRunner::new().missing("getent");
        // When getent is missing only /etc/passwd is read; that file does not exist on
        // Windows, so the only requirement is not to panic.
        let _ = lookup_user(&runner, "alice");
        let _ = lookup_uid(&runner, 1000);
        assert!(!runner.called("getent"));
        // current_uid merely forwards to platform::euid()
        assert_eq!(current_uid(), platform::euid());
    }
}
