//! Desktop notifications (optional; a failure never affects mounting).

use std::time::Duration;

use crate::config::Config;
use crate::exec::CommandRunner;
use crate::logging;
use crate::model::UserInfo;
use crate::platform;
use crate::sessions;

/// Timeout for notify-send; a stuck notification must not block the mount path.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Notify the desktop user; any failure returns false.
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
        logging::debug("notify-send not found; skipping desktop notification");
        return false;
    }

    // When no user is given, detect the desktop user (same logic as the daemon).
    let detected = if user.is_none() {
        sessions::console_user(runner, config.user.as_deref())
    } else {
        None
    };
    let Some(target) = user.or(detected.as_ref()) else {
        logging::debug("could not identify the desktop user; skipping desktop notification");
        return false;
    };

    let is_root = platform::euid() == 0;
    if is_root && target.uid == 0 {
        logging::debug(
            "running as root with no regular desktop user; skipping desktop notification",
        );
        return false;
    }

    let urgency = normalize_urgency(urgency);
    let (program, argv) = build_notification(runner, target, title, body, urgency, is_root);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    logging::debug(format!(
        "sending desktop notification: {program} {}",
        args.join(" ")
    ));

    match runner.run(&program, &args, Some(NOTIFY_TIMEOUT)) {
        Ok(output) if output.ok() => true,
        Ok(output) => {
            logging::warn(format!(
                "notify-send failed ({}): {}",
                output.status,
                output.stderr_trimmed()
            ));
            false
        }
        Err(err) => {
            logging::warn(format!("failed to run {program}: {err}"));
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
        "Automount error"
    } else {
        title
    };
    send(config, runner, user, title, detail, "critical")
}

/// Assemble the `notify-send` invocation; when running as root with a regular user present,
/// borrow runuser to enter that user's environment.
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

/// Accept only the urgency levels notify-send understands.
fn normalize_urgency(urgency: &str) -> &'static str {
    match urgency.trim().to_ascii_lowercase().as_str() {
        "low" => "low",
        "critical" => "critical",
        _ => "normal",
    }
}

fn mounted_texts(device: &str, target: &str) -> (String, String) {
    (
        "Disk mounted automatically".to_string(),
        format!("{device} mounted at {target}"),
    )
}

fn unmounted_texts(target: &str) -> (String, String) {
    ("Disk unmounted".to_string(), format!("{target} unmounted"))
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
            "Title",
            "Body",
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
            "Disk mounted automatically",
            "/dev/sdb1 mounted at /media/alice/UDISK",
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
                "Disk mounted automatically",
                "/dev/sdb1 mounted at /media/alice/UDISK",
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
            "Title",
            "Body",
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
                "Title",
                "Body",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<String>>()
        );
        // Without runuser, notify-send is called directly even as root
        let runner = MockRunner::new().missing("runuser");
        let (program, argv) = build_notification(
            &runner,
            &user(1000, "alice"),
            "Title",
            "Body",
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
            "Disk mounted automatically",
            "Body",
            "low"
        ));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        // A root environment (including Windows test machines, where platform::euid()
        // returns 0) goes through runuser; non-root (a normal Linux user) calls
        // notify-send directly.
        let program = if platform::euid() == 0 {
            assert_eq!(calls[0].0, "runuser");
            assert!(calls[0].1.iter().any(|arg| arg == "notify-send"));
            "runuser"
        } else {
            assert_eq!(calls[0].0, "notify-send");
            "notify-send"
        };
        assert!(calls[0].1.iter().any(|arg| arg == "low"));
        assert!(calls[0]
            .1
            .iter()
            .any(|arg| arg == "Disk mounted automatically"));

        // send must return false when the program actually run exits non-zero
        let runner = MockRunner::new().respond(
            program,
            CommandOutput::failure(1, "Cannot autolaunch D-Bus"),
        );
        assert!(!send(
            &config,
            &runner,
            Some(&user(1000, "alice")),
            "Title",
            "Body",
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
    fn messages_are_english_and_urgency_normalized() {
        let (title, body) = mounted_texts("/dev/sdb1", "/media/alice/UDISK");
        assert!(title.contains("mounted"));
        assert!(body.contains("/dev/sdb1"));
        assert!(body.contains("/media/alice/UDISK"));

        let (title, body) = unmounted_texts("/media/alice/UDISK");
        assert!(title.contains("unmounted"));
        assert!(body.contains("/media/alice/UDISK"));

        assert_eq!(normalize_urgency("LOW"), "low");
        assert_eq!(normalize_urgency("critical"), "critical");
        assert_eq!(normalize_urgency(""), "normal");
        assert_eq!(normalize_urgency("unknown"), "normal");

        // notify_error uses critical: as root with no regular user it bails out early,
        // so the arguments are only checked when a notification program really ran.
        let config = Config::default();
        let runner = MockRunner::new();
        let _ = notify_error(
            &config,
            &runner,
            "Mount failed",
            "/dev/sdb1: NTFS inconsistent",
            None,
        );
        for (program, args) in runner.calls() {
            if program == "notify-send" || program == "runuser" {
                assert!(args.iter().any(|arg| arg == "critical"), "args: {args:?}");
                assert!(args.iter().any(|arg| arg == "Mount failed"));
            }
        }
    }
}
