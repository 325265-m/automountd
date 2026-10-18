//! 登录会话与桌面用户识别。
//!
//! 顺序：配置指定 -> sudo/pkexec 环境变量 -> loginctl 活动会话 -> 当前 uid -> 首个普通用户。

use std::collections::BTreeMap;

use crate::exec::CommandRunner;
use crate::model::UserInfo;

/// 解析 `/etc/passwd` 风格的一行。
pub fn parse_passwd_line(line: &str) -> Option<UserInfo> {
    let _ = line;
    todo!()
}

/// 按用户名查询（getent -> /etc/passwd）。
pub fn lookup_user(runner: &dyn CommandRunner, name: &str) -> Option<UserInfo> {
    let _ = (runner, name);
    todo!()
}

/// 按 uid 查询。
pub fn lookup_uid(runner: &dyn CommandRunner, uid: u32) -> Option<UserInfo> {
    let _ = (runner, uid);
    todo!()
}

/// 返回应当拥有挂载点的桌面用户。
pub fn console_user(runner: &dyn CommandRunner, configured: Option<&str>) -> Option<UserInfo> {
    let _ = (runner, configured);
    todo!()
}

/// 当前进程 uid。
pub fn current_uid() -> u32 {
    todo!()
}

/// 首选普通用户（uid >= 1000 且 shell 可登录）。
pub fn first_human_user(runner: &dyn CommandRunner) -> Option<UserInfo> {
    let _ = runner;
    todo!()
}

/// 解析 `loginctl list-sessions --no-legend` 为 (session_id, uid, user)。
pub fn parse_session_rows(text: &str) -> Vec<(String, String, String)> {
    let _ = text;
    todo!()
}

/// 解析 `loginctl show-session ... -p Key` 输出。
pub fn parse_session_properties(text: &str) -> BTreeMap<String, String> {
    let _ = text;
    todo!()
}
