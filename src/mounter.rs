//! 挂载与卸载核心。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::error::Result;
use crate::exec::CommandRunner;
use crate::model::{BlockDevice, MountEntry, MountPlan, MountResult, UserInfo};

/// 执行挂载/卸载，并维护挂载点目录。
pub struct MountManager {
    _private: (),
}

impl MountManager {
    pub fn new(config: Config, runner: Arc<dyn CommandRunner>) -> Self {
        let _ = (config, runner);
        todo!()
    }

    pub fn with_dry_run(config: Config, runner: Arc<dyn CommandRunner>, dry_run: bool) -> Self {
        let _ = (config, runner, dry_run);
        todo!()
    }

    pub fn dry_run(&self) -> bool {
        todo!()
    }

    pub fn set_dry_run(&mut self, dry_run: bool) {
        let _ = dry_run;
        todo!()
    }

    /// 挂载根目录（含 per-user 子目录）。
    pub fn base_dir(&self, user: Option<&UserInfo>) -> PathBuf {
        let _ = user;
        todo!()
    }

    /// 计算挂载点：复用已存在的空目录，避免产生 `卷标_2`。
    pub fn target_path(
        &self,
        device: &BlockDevice,
        user: Option<&UserInfo>,
        mounts: &[MountEntry],
    ) -> Result<PathBuf> {
        let _ = (device, user, mounts);
        todo!()
    }

    /// 生成挂载计划（不执行副作用），供 dry-run 与测试使用。
    pub fn plan(
        &self,
        device: &BlockDevice,
        user: Option<&UserInfo>,
        read_only: bool,
        extra: &[String],
        mounts: &[MountEntry],
    ) -> Result<MountPlan> {
        let _ = (device, user, read_only, extra, mounts);
        todo!()
    }

    pub fn mount(
        &mut self,
        device: &BlockDevice,
        user: Option<&UserInfo>,
        read_only: bool,
        extra: &[String],
    ) -> Result<MountResult> {
        let _ = (device, user, read_only, extra);
        todo!()
    }

    /// 卸载指定挂载点，返回被卸载的挂载点。
    pub fn unmount(
        &mut self,
        target: &str,
        lazy: bool,
        eject: bool,
        power_off: bool,
    ) -> Result<String> {
        let _ = (target, lazy, eject, power_off);
        todo!()
    }

    /// 按设备路径卸载（取第一个挂载点）。
    pub fn unmount_device(
        &mut self,
        device_path: &str,
        lazy: bool,
        eject: bool,
        power_off: bool,
    ) -> Result<String> {
        let _ = (device_path, lazy, eject, power_off);
        todo!()
    }

    pub fn supported_filesystems(&self) -> &HashSet<String> {
        todo!()
    }

    pub fn planned_commands(&self) -> &[Vec<String>] {
        todo!()
    }
}

/// 根据 mount/umount 的 stderr 给出中文排查建议。
pub fn diagnose(stderr: &str) -> Option<&'static str> {
    let _ = stderr;
    todo!()
}
