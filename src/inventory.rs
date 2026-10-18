//! 设备列表快照：把设备、挂载状态、策略结论合成 UI/CLI 需要的视图。

use crate::config::Config;
use crate::error::Result;
use crate::exec::CommandRunner;
use crate::model::DeviceView;
use crate::state::State;

/// 生成设备视图；`include_all` 为 true 时保留不可挂载设备（用于 `list --all`）。
pub fn snapshot(
    config: &Config,
    runner: &dyn CommandRunner,
    state: &State,
    include_all: bool,
) -> Result<Vec<DeviceView>> {
    let _ = (config, runner, state, include_all);
    todo!()
}
