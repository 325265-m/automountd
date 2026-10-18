//! 桌面通知（可选，失败不影响挂载）。

use std::time::Duration;

use crate::config::Config;
use crate::exec::CommandRunner;
use crate::logging;
use crate::model::UserInfo;
use crate::platform;
use crate::sessions;

/// notify-send 的超时；通知卡住时不能拖住挂载流程。
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// 给桌面用户发通知；任何失败都返回 false。
pub fn send(
    config: &Config,
    runner: &dyn CommandRunner,
    user: Option<&UserInfo>,
    title: &str,
    body: &str,
    urgency: &str,
) -> bool {
    if !config.notify {
        return false;
    }
    if !runner.which("notify-send") {
        logging::debug("未找到 notify-send，跳过桌面通知");
        return false;
    }

    // 未指定用户时自动识别桌面用户（与服务端同一套逻辑）。
    let detected = if user.is_none() {
        sessions::console_user(runner, config.user.as_deref())
    } else {
        None
    };
    let Some(target) = user.or(detected.as_ref()) else {
        logging::debug("未能识别桌面用户，跳过桌面通知");
        return false;
    };

    let is_root = platform::euid() == 0;
    if is_root && target.uid == 0 {
        logging::debug("当前以 root 运行且没有普通桌面用户，跳过桌面通知");
        return false;
    }

    let urgency = normalize_urgency(urgency);
    let (program, argv) = build_notification(runner, target, title, body, urgency, is_root);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    logging::debug(format!("发送桌面通知: {program} {}", args.join(" ")));

    match runner.run(&program, &args, Some(NOTIFY_TIMEOUT)) {
        Ok(output) if output.ok() => true,
        Ok(output) => {
            logging::warn(format!(
                "notify-send 执行失败（{}）: {}",
                output.status,
                output.stderr_trimmed()
            ));
            false
        }
        Err(err) => {
            logging::warn(format!("无法执行 {program}: {err}"));
            false
        }
    }
}

pub fn notify_mounted(
    config: &Config,
    runner: &dyn CommandRunner,
    device: &str,
    target: &str,
    user: Option<&UserInfo>,
) -> bool {
    let (title, body) = mounted_texts(device, target);
    send(config, runner, user, &title, &body, "normal")
}

pub fn notify_unmounted(
    config: &Config,
    runner: &dyn CommandRunner,
    target: &str,
    user: Option<&UserInfo>,
) -> bool {
    let (title, body) = unmounted_texts(target);
    send(config, runner, user, &title, &body, "normal")
}

pub fn notify_error(
    config: &Config,
    runner: &dyn CommandRunner,
    title: &str,
    detail: &str,
    user: Option<&UserInfo>,
) -> bool {
    let title = if title.trim().is_empty() {
        "自动挂载出错"
    } else {
        title
    };
    send(config, runner, user, title, detail, "critical")
}

/// 组装 `notify-send` 调用；root 且存在普通用户时借用 runuser 切到该用户的环境。
fn build_notification(
    runner: &dyn CommandRunner,
    user: &UserInfo,
    title: &str,
    body: &str,
    urgency: &str,
    is_root: bool,
) -> (String, Vec<String>) {
    let mut argv: Vec<String> = Vec::new();
    let program;
    if is_root && user.uid != 0 && runner.which("runuser") {
        program = "runuser".to_string();
        argv.push("-u".to_string());
        argv.push(user.name.clone());
        argv.push("--".to_string());
        argv.push("env".to_string());
        argv.push(format!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{}/bus",
            user.uid
        ));
        argv.push(format!("XDG_RUNTIME_DIR=/run/user/{}", user.uid));
        argv.push("notify-send".to_string());
    } else {
        program = "notify-send".to_string();
    }
    for arg in [
        "-a",
        "automountd",
        "-i",
        "drive-harddisk",
        "-u",
        urgency,
        title,
        body,
    ] {
        argv.push(arg.to_string());
    }
    (program, argv)
}

/// 只接受 notify-send 认识的紧急级别。
fn normalize_urgency(urgency: &str) -> &'static str {
    match urgency.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "critical" => "critical",
        _ => "normal",
    }
}

fn mounted_texts(device: &str, target: &str) -> (String, String) {
    (
        "磁盘已自动挂载".to_string(),
        format!("{device} 已挂载到 {target}"),
    )
}

fn unmounted_texts(target: &str) -> (String, String) {
    ("磁盘已卸载".to_string(), format!("{target} 已卸载"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CommandOutput, MockRunner};

    fn user(uid: u32, name: &str) -> UserInfo {
        UserInfo {
            name: name.to_string(),
            uid,
            gid: uid,
            home: Some(format!("/home/{name}")),
        }
    }

    #[test]
    fn disabled_notification_does_nothing() {
        let mut config = Config::default();
        config.notify = false;
        let runner = MockRunner::new();
        assert!(!send(
            &config,
            &runner,
            Some(&user(1000, "alice")),
            "标题",
            "正文",
            "normal"
        ));
        assert_eq!(runner.call_count(), 0);
    }

    #[test]
    fn missing_notify_send_returns_false() {
        let config = Config::default();
        let runner = MockRunner::new().missing("notify-send");
        assert!(!notify_mounted(
            &config,
            &runner,
            "/dev/sdb1",
            "/media/alice/UDISK",
            Some(&user(1000, "alice"))
        ));
        assert_eq!(runner.call_count(), 0);
    }

    #[test]
    fn root_prefixes_runuser_and_session_env() {
        let runner = MockRunner::new();
        let (program, argv) = build_notification(
            &runner,
            &user(1000, "alice"),
            "磁盘已自动挂载",
            "/dev/sdb1 已挂载到 /media/alice/UDISK",
            "normal",
            true,
        );
        assert_eq!(program, "runuser");
        assert_eq!(
            argv,
            vec![
                "-u",
                "alice",
                "--",
                "env",
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
                "XDG_RUNTIME_DIR=/run/user/1000",
                "notify-send",
                "-a",
                "automountd",
                "-i",
                "drive-harddisk",
                "-u",
                "normal",
                "磁盘已自动挂载",
                "/dev/sdb1 已挂载到 /media/alice/UDISK",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<String>>()
        );
    }

    #[test]
    fn non_root_calls_notify_send_directly() {
        let runner = MockRunner::new();
        let (program, argv) = build_notification(
            &runner,
            &user(1000, "alice"),
            "标题",
            "正文",
            "critical",
            false,
        );
        assert_eq!(program, "notify-send");
        assert_eq!(
            argv,
            vec![
                "-a",
                "automountd",
                "-i",
                "drive-harddisk",
                "-u",
                "critical",
                "标题",
                "正文",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<String>>()
        );
        // runuser 不可用时即使是 root 也直接调用 notify-send
        let runner = MockRunner::new().missing("runuser");
        let (program, argv) = build_notification(
            &runner,
            &user(1000, "alice"),
            "标题",
            "正文",
            "normal",
            true,
        );
        assert_eq!(program, "notify-send");
        assert_eq!(argv.first().map(String::as_str), Some("-a"));
    }

    #[test]
    fn send_uses_runner_and_reports_status() {
        let config = Config::default();
        let runner = MockRunner::new();
        assert!(send(
            &config,
            &runner,
            Some(&user(1000, "alice")),
            "磁盘已自动挂载",
            "正文",
            "low"
        ));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        // root 环境（含 Windows 测试机，platform::euid() 返回 0）会走 runuser；
        // 非 root（普通 Linux 用户）直接调用 notify-send。
        let program = if platform::euid() == 0 {
            assert_eq!(calls[0].0, "runuser");
            assert!(calls[0].1.iter().any(|arg| arg == "notify-send"));
            "runuser"
        } else {
            assert_eq!(calls[0].0, "notify-send");
            "notify-send"
        };
        assert!(calls[0].1.iter().any(|arg| arg == "low"));
        assert!(calls[0].1.iter().any(|arg| arg == "磁盘已自动挂载"));

        // 实际执行的程序返回非 0 时 send 必须返回 false
        let runner = MockRunner::new().respond(
            program,
            CommandOutput::failure(1, "Cannot autolaunch D-Bus"),
        );
        assert!(!send(
            &config,
            &runner,
            Some(&user(1000, "alice")),
            "标题",
            "正文",
            "normal"
        ));

        let runner = MockRunner::new().respond(program, CommandOutput::failure(1, "no display"));
        assert!(!notify_unmounted(
            &config,
            &runner,
            "/media/alice/UDISK",
            Some(&user(1000, "alice"))
        ));
    }

    #[test]
    fn messages_are_chinese_and_urgency_normalized() {
        let (title, body) = mounted_texts("/dev/sdb1", "/media/alice/UDISK");
        assert!(title.contains("挂载"));
        assert!(body.contains("/dev/sdb1"));
        assert!(body.contains("/media/alice/UDISK"));

        let (title, body) = unmounted_texts("/media/alice/UDISK");
        assert!(title.contains("卸载"));
        assert!(body.contains("/media/alice/UDISK"));

        assert_eq!(normalize_urgency("LOW"), "low");
        assert_eq!(normalize_urgency("critical"), "critical");
        assert_eq!(normalize_urgency(""), "normal");
        assert_eq!(normalize_urgency("unknown"), "normal");

        // notify_error 用 critical：root 且无普通用户时直接跳过，
        // 因此只在真正调用了通知程序时检查参数。
        let config = Config::default();
        let runner = MockRunner::new();
        let _ = notify_error(&config, &runner, "挂载失败", "/dev/sdb1: NTFS 不一致", None);
        for (program, args) in runner.calls() {
            if program == "notify-send" || program == "runuser" {
                assert!(args.iter().any(|arg| arg == "critical"), "参数: {args:?}");
                assert!(args.iter().any(|arg| arg == "挂载失败"));
            }
        }
    }
}
