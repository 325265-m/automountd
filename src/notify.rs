//! 桌面通知（可选，失败不影响挂载）。

use crate::config::Config;
use crate::exec::CommandRunner;
use crate::model::UserInfo;

/// 给桌面用户发通知；任何失败都返回 false。
pub fn send(
    config: &Config,
    runner: &dyn CommandRunner,
    user: Option<&UserInfo>,
    title: &str,
    body: &str,
    urgency: &str,
) -> bool {
    let _ = (config, runner, user, title, body, urgency);
    todo!()
}

pub fn notify_mounted(
    config: &Config,
    runner: &dyn CommandRunner,
    device: &str,
    target: &str,
    user: Option<&UserInfo>,
) -> bool {
    let _ = (config, runner, device, target, user);
    todo!()
}

pub fn notify_unmounted(
    config: &Config,
    runner: &dyn CommandRunner,
    target: &str,
    user: Option<&UserInfo>,
) -> bool {
    let _ = (config, runner, target, user);
    todo!()
}

pub fn notify_error(
    config: &Config,
    runner: &dyn CommandRunner,
    title: &str,
    detail: &str,
    user: Option<&UserInfo>,
) -> bool {
    let _ = (config, runner, title, detail, user);
    todo!()
}
