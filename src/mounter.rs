//! 挂载与卸载核心。
//!
//! 所有系统交互都通过 [`CommandRunner`]（测试中替换为 [`crate::exec::MockRunner`]），
//! 因此挂载计划的拼装、挂载点的选择、错误诊断都可以在非 Linux 开发机上验证。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::CommandRunner;
use crate::fstypes;
use crate::logging::{debug, info, warn};
use crate::model::{BlockDevice, MountEntry, MountPlan, MountResult, UserInfo};
use crate::mountinfo;
use crate::naming;
use crate::platform;

/// 挂载成功后等待内核把记录写进 mountinfo 的最长时间。
const MOUNT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
/// 卸载后等待挂载记录消失的最长时间。
const UNMOUNT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
/// 轮询 mountinfo 的间隔。
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 执行挂载/卸载，并维护挂载点目录。
pub struct MountManager {
    config: Config,
    runner: Arc<dyn CommandRunner>,
    dry_run: bool,
    supported: HashSet<String>,
    planned: Vec<Vec<String>>,
    /// 本次进程中由本管理器创建的目录；卸载/失败清理只删这些，避免误删管理员预建目录。
    created_dirs: HashSet<PathBuf>,
    /// 仅供测试注入 mountinfo（真实运行时为 None，直接读 `/proc/self/mountinfo`）。
    mounts_override: Option<Vec<MountEntry>>,
}

impl MountManager {
    pub fn new(config: Config, runner: Arc<dyn CommandRunner>) -> Self {
        let dry_run = config.dry_run;
        Self::build(config, runner, dry_run)
    }

    pub fn with_dry_run(config: Config, runner: Arc<dyn CommandRunner>, dry_run: bool) -> Self {
        Self::build(config, runner, dry_run)
    }

    fn build(config: Config, runner: Arc<dyn CommandRunner>, dry_run: bool) -> Self {
        let supported = fstypes::read_proc_filesystems();
        debug(format!("检测到内核支持的文件系统 {} 种", supported.len()));
        Self {
            config,
            runner,
            dry_run,
            supported,
            planned: Vec::new(),
            created_dirs: HashSet::new(),
            mounts_override: None,
        }
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn set_dry_run(&mut self, dry_run: bool) {
        self.dry_run = dry_run;
    }

    /// 挂载根目录（含 per-user 子目录）。
    pub fn base_dir(&self, user: Option<&UserInfo>) -> PathBuf {
        // 固定到真实路径，避免 mount_root 是符号链接时包含性检查与 mount 落点不一致。
        let mut base = canonicalize_root(&self.config.mount_root);
        if self.config.per_user_dirs {
            if let Some(user) = user {
                let fallback = format!("user{}", user.uid);
                base.push(naming::sanitize_label(Some(user.name.as_str()), &fallback));
            }
        }
        base
    }

    /// 计算挂载点：复用已存在的空目录，避免产生 `卷标_2`。
    pub fn target_path(
        &self,
        device: &BlockDevice,
        user: Option<&UserInfo>,
        mounts: &[MountEntry],
    ) -> Result<PathBuf> {
        let base = self.base_dir(user);
        let fallback = naming::fallback_label(&device.name, device.uuid.as_deref());
        let name = naming::sanitize_label(device.label.as_deref(), &fallback);
        let reserved: HashSet<String> = mounts
            .iter()
            .map(|entry| mountinfo::normalize_path(&entry.target))
            .collect();

        let candidate = base.join(&name);
        let target = if self.can_reuse_dir(&candidate, &reserved) {
            debug(format!("复用已存在的空挂载点 {}", candidate.display()));
            candidate
        } else {
            naming::unique_dir(&base, &name, &reserved)
        };

        self.validate_target(&base, &target)?;
        Ok(target)
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
        let Some(fstype) = device
            .fstype
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            return Err(Error::device(format!(
                "设备 {} 没有可识别的文件系统，无法挂载",
                device.path
            ))
            .with_hint(
                "请先格式化该设备（例如 mkfs.exfat），或用 'automountd list --all' 确认设备状态",
            ));
        };

        let driver = fstypes::choose_driver(&fstype, &self.supported)?;
        let read_only = read_only || fstypes::is_read_only(&fstype, &self.config);
        let options = fstypes::build_options(
            &self.config,
            &fstype,
            &driver,
            user,
            read_only,
            extra,
            device.label.as_deref(),
        );
        let target = self.target_path(device, user, mounts)?;

        Ok(MountPlan {
            device: device.clone(),
            target,
            fstype,
            driver,
            options,
        })
    }

    pub fn mount(
        &mut self,
        device: &BlockDevice,
        user: Option<&UserInfo>,
        read_only: bool,
        extra: &[String],
    ) -> Result<MountResult> {
        if !self.dry_run {
            platform::require_root("挂载")?;
        }

        let mounts = self.current_mounts();
        if let Some(existing) = mountinfo::find_mount(&mounts, None, Some(&device.path)) {
            let fstype = device
                .fstype
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| existing.fstype.clone());
            let driver =
                fstypes::choose_driver(&fstype, &self.supported).unwrap_or_else(|_| fstype.clone());
            info(format!(
                "{} 已经挂载在 {}，跳过",
                device.path, existing.target
            ));
            return Ok(MountResult {
                device: device.clone(),
                target: PathBuf::from(&existing.target),
                fstype,
                driver,
                options: existing.options.clone(),
                already_mounted: true,
                dry_run: false,
            });
        }

        let plan = self.plan(device, user, read_only, extra, &mounts)?;
        let argv = plan.argv();
        self.planned.push(argv);

        if self.dry_run {
            info(format!("dry-run：将执行 {}", plan.describe()));
            return Ok(MountResult {
                device: plan.device.clone(),
                target: plan.target.clone(),
                fstype: plan.fstype.clone(),
                driver: plan.driver.clone(),
                options: plan.options.clone(),
                already_mounted: false,
                dry_run: true,
            });
        }

        let base = self.base_dir(user);
        let base_existed = base.exists();
        ensure_dir(&base)?;
        if !base_existed {
            self.created_dirs.insert(base.clone());
        }
        let target_existed = plan.target.exists();
        naming::check_no_symlink_components(&self.config.mount_root, &plan.target)?;
        ensure_dir(&plan.target)?;
        if !target_existed {
            self.created_dirs.insert(plan.target.clone());
        }

        if let Some(user) = user {
            if fstypes::needs_ownership_fix(&plan.fstype, &self.config) {
                match plan.target.symlink_metadata() {
                    Ok(metadata) if !metadata.file_type().is_symlink() => {}
                    _ => {
                        self.cleanup_empty_dirs(&plan.target);
                        return Err(Error::mount(format!(
                            "挂载点 {} 在创建后变成了符号链接或不存在，已中止",
                            plan.target.display()
                        ))
                        .with_hint("可能有其他进程在操作挂载目录，请检查并重试"));
                    }
                }
                if let Err(err) = platform::chown(&plan.target, user.uid, user.gid) {
                    warn(format!(
                        "无法把挂载点 {} 交给 {}({}): {err}",
                        plan.target.display(),
                        user.name,
                        user.uid
                    ));
                }
            }
        }

        let target_text = plan.target.display().to_string();
        let options_text = plan.options_string();
        let timeout = Duration::from_secs(self.config.mount_timeout_seconds.max(1));
        debug(format!("执行 {}", plan.describe()));
        let mut driver = plan.driver.clone();
        let mut fstype = plan.fstype.clone();
        let mut output = match self.run_mount_command(
            &driver,
            &options_text,
            &device.path,
            &target_text,
            timeout,
        ) {
            Ok(output) => output,
            Err(err) => {
                self.cleanup_empty_dirs(&plan.target);
                return Err(err);
            }
        };
        // NTFS 的两种驱动：内核 ntfs3 可能只是未加载的模块（不在 /proc/filesystems 里），
        // 反过来老内核也可能只装了 ntfs-3g。失败时自动换另一个驱动再试一次。
        if !output.ok() && is_unknown_filesystem(&output.stderr) {
            let alternative = match driver.as_str() {
                "ntfs3" => Some("ntfs-3g"),
                "ntfs-3g" => Some("ntfs3"),
                _ => None,
            };
            if let Some(alternative) = alternative {
                warn(format!(
                    "使用 {driver} 挂载 {} 失败，尝试改用 {alternative}",
                    device.path
                ));
                if let Ok(retry) = self.run_mount_command(
                    alternative,
                    &options_text,
                    &device.path,
                    &target_text,
                    timeout,
                ) {
                    if retry.ok() {
                        output = retry;
                        driver = alternative.to_string();
                        fstype = "ntfs".to_string();
                    }
                }
            }
        }
        if !output.ok() {
            self.cleanup_empty_dirs(&plan.target);
            let detail = command_detail(output.status, output.stderr_trimmed());
            let message = format!("挂载 {} 到 {} 失败: {detail}", device.path, target_text);
            return Err(match diagnose(&output.stderr) {
                Some(hint) => Error::mount(message).with_hint(hint),
                None => Error::mount(message),
            });
        }

        if !self.wait_until("挂载", &target_text, MOUNT_CONFIRM_TIMEOUT, true) {
            self.cleanup_empty_dirs(&plan.target);
            return Err(
                Error::mount(format!("挂载命令已返回，但 {target_text} 未出现在挂载表中"))
                    .with_hint("请用 dmesg 查看内核消息，确认设备仍然在线且文件系统正常"),
            );
        }

        // 防御符号链接竞态：确认内核实际挂载的位置就是预期目录。
        let expected = mountinfo::normalize_path(&target_text);
        let actual = mountinfo::find_mount(&self.current_mounts(), Some(&target_text), None)
            .map(|entry| mountinfo::normalize_path(&entry.target));
        if actual.as_deref() != Some(expected.as_str()) {
            let _ = self.runner.run(
                "umount",
                &[target_text.as_str()],
                Some(Duration::from_secs(10)),
            );
            self.cleanup_empty_dirs(&plan.target);
            return Err(Error::mount(format!(
                "实际挂载点与预期不一致（预期 {expected}，实际 {}）",
                actual.unwrap_or_else(|| "未知".to_string())
            ))
            .with_hint("挂载目录可能在挂载过程中被替换，请检查是否有其他进程操作该路径"));
        }

        info(format!("已挂载 {} 到 {target_text}", device.path));
        Ok(MountResult {
            device: plan.device.clone(),
            target: plan.target.clone(),
            fstype,
            driver,
            options: plan.options.clone(),
            already_mounted: false,
            dry_run: false,
        })
    }

    fn run_mount_command(
        &self,
        driver: &str,
        options: &str,
        device: &str,
        target: &str,
        timeout: Duration,
    ) -> Result<crate::exec::CommandOutput> {
        self.runner.run(
            "mount",
            &["-t", driver, "-o", options, device, target],
            Some(timeout),
        )
    }

    /// 卸载指定挂载点，返回被卸载的挂载点。
    pub fn unmount(
        &mut self,
        target: &str,
        lazy: bool,
        eject: bool,
        power_off: bool,
    ) -> Result<String> {
        if !self.dry_run {
            platform::require_root("卸载")?;
        }

        let mounts = self.current_mounts();
        let Some(entry) = mountinfo::find_mount(&mounts, Some(target), None) else {
            return Err(
                Error::new(ErrorKind::NotMounted, format!("{target} 当前没有挂载")).with_hint(
                    "可用 'automountd list --all' 查看挂载状态，或检查挂载点路径是否正确",
                ),
            );
        };
        let mounted_target = entry.target.clone();
        let source = entry.source.clone();

        let use_lazy = lazy || self.config.lazy_umount;
        let mut umount_args: Vec<String> = Vec::new();
        if use_lazy {
            umount_args.push("-l".to_string());
        }
        umount_args.push(mounted_target.clone());
        let mut planned = vec!["umount".to_string()];
        planned.extend(umount_args.iter().cloned());
        self.planned.push(planned);

        if self.dry_run {
            info(format!("dry-run：将执行 umount {}", umount_args.join(" ")));
            return Ok(mounted_target);
        }

        let timeout = Duration::from_secs(self.config.unmount_timeout_seconds.max(1));
        let args: Vec<&str> = umount_args.iter().map(String::as_str).collect();
        let mut output = self.runner.run("umount", &args, Some(timeout))?;
        if !output.ok() && is_busy(&output.stderr) {
            warn(format!("{mounted_target} 正忙，300ms 后重试一次"));
            std::thread::sleep(Duration::from_millis(300));
            output = self.runner.run("umount", &args, Some(timeout))?;
        }
        if !output.ok() {
            let detail = command_detail(output.status, output.stderr_trimmed());
            let message = format!("卸载 {mounted_target} 失败: {detail}");
            if is_busy(&output.stderr) {
                return Err(Error::new(ErrorKind::Busy, message).with_hint(
                    "请用 lsof / fuser -vm 找出占用该挂载点的进程，或执行 'automountd umount --lazy <挂载点>'",
                ));
            }
            return Err(match diagnose(&output.stderr) {
                Some(hint) => Error::mount(message).with_hint(hint),
                None => Error::mount(message),
            });
        }

        if !self.wait_until("卸载", &mounted_target, UNMOUNT_CONFIRM_TIMEOUT, false) {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("{mounted_target} 仍然出现在挂载表中"),
            )
            .with_hint("可能有进程占用该挂载点，请尝试 'automountd umount --lazy' 或重启后清理"));
        }

        if self.config.cleanup_dirs {
            self.cleanup_empty_dirs(Path::new(&mounted_target));
        }

        let wants_eject = eject || self.config.eject_after_unmount;
        let wants_power_off = power_off || self.config.power_off_after_unmount;
        if use_lazy && (wants_eject || wants_power_off) {
            warn(format!(
                "{} 使用 lazy 卸载，设备可能仍被进程占用；已跳过弹出/断电以避免数据丢失",
                mounted_target
            ));
        } else {
            if wants_eject {
                self.try_optional("eject", &[source.as_str()], "弹出设备");
            }
            if wants_power_off {
                self.try_optional(
                    "udisksctl",
                    &["power-off", "-b", source.as_str()],
                    "设备断电",
                );
            }
        }

        info(format!("已卸载 {mounted_target}"));
        Ok(mounted_target)
    }

    /// 按设备路径卸载（取第一个挂载点）。
    pub fn unmount_device(
        &mut self,
        device_path: &str,
        lazy: bool,
        eject: bool,
        power_off: bool,
    ) -> Result<String> {
        let mounts = self.current_mounts();
        let target = mountinfo::find_by_source(device_path, &mounts)
            .first()
            .map(|entry| entry.target.clone())
            .or_else(|| {
                mountinfo::find_mount(&mounts, None, Some(device_path))
                    .map(|entry| entry.target.clone())
            });
        let Some(target) = target else {
            return Err(Error::new(
                ErrorKind::NotMounted,
                format!("{device_path} 当前没有挂载点"),
            )
            .with_hint("可用 'automountd list --all' 确认设备路径"));
        };
        self.unmount(&target, lazy, eject, power_off)
    }

    pub fn supported_filesystems(&self) -> &HashSet<String> {
        &self.supported
    }

    pub fn planned_commands(&self) -> &[Vec<String>] {
        &self.planned
    }

    fn current_mounts(&self) -> Vec<MountEntry> {
        match &self.mounts_override {
            Some(mounts) => mounts.clone(),
            None => mountinfo::read_mountinfo(),
        }
    }

    /// 已存在的空目录可以复用，避免每次插拔都产生 `卷标_2`。
    fn can_reuse_dir(&self, candidate: &Path, reserved: &HashSet<String>) -> bool {
        if candidate.is_symlink() || !candidate.is_dir() {
            return false;
        }
        if reserved.contains(&mountinfo::normalize_path(&candidate.display().to_string())) {
            return false;
        }
        dir_is_empty(candidate)
    }

    /// 保证挂载点位于挂载根目录之内，且路径上没有符号链接。
    fn validate_target(&self, base: &Path, target: &Path) -> Result<()> {
        if base.exists() {
            naming::safe_target(base, target)?;
            naming::check_no_symlink_components(base, target)?;
            return Ok(());
        }
        let base_abs = absolute_path(base)?;
        let target_abs = absolute_path(target)?;
        if target_abs == base_abs || !target_abs.starts_with(&base_abs) {
            return Err(Error::device(format!(
                "挂载点 {} 不在挂载根目录 {} 之内",
                target.display(),
                base.display()
            ))
            .with_hint("卷标可能包含可疑字符，已拒绝操作"));
        }
        naming::check_no_symlink_components(&base_abs, &target_abs)?;
        Ok(())
    }

    /// 轮询 mountinfo，确认挂载表已经变化。
    fn wait_until(&self, action: &str, target: &str, limit: Duration, want_mounted: bool) -> bool {
        let started = Instant::now();
        loop {
            let mounted =
                mountinfo::find_mount(&self.current_mounts(), Some(target), None).is_some();
            if mounted == want_mounted {
                return true;
            }
            if started.elapsed() >= limit {
                debug(format!(
                    "{action}后 {target} 的挂载状态在 {}ms 内未稳定",
                    limit.as_millis()
                ));
                return false;
            }
            std::thread::sleep(CONFIRM_POLL_INTERVAL);
        }
    }

    /// 只清理本次由本管理器创建的、位于 mount_root 之下的空目录。
    fn cleanup_empty_dirs(&mut self, target: &Path) {
        let root = absolute_path(&self.config.mount_root)
            .unwrap_or_else(|_| self.config.mount_root.clone());
        let Ok(target_abs) = absolute_path(target) else {
            return;
        };
        if target_abs == root || !target_abs.starts_with(&root) {
            return;
        }
        if self.created_dirs.remove(&target_abs) && dir_is_empty(&target_abs) {
            if let Err(err) = fs::remove_dir(&target_abs) {
                debug(format!("删除空目录 {} 失败: {err}", target_abs.display()));
            }
        }
        let Some(parent) = target_abs.parent() else {
            return;
        };
        if parent != root
            && parent.starts_with(&root)
            && self.created_dirs.remove(parent)
            && dir_is_empty(parent)
        {
            if let Err(err) = fs::remove_dir(parent) {
                debug(format!("删除空目录 {} 失败: {err}", parent.display()));
            }
        }
    }

    /// 可选动作（eject / 断电）失败只记录警告，不影响卸载结果。
    fn try_optional(&self, program: &str, args: &[&str], purpose: &str) {
        if !self.runner.which(program) {
            warn(format!("未找到 {program}，跳过{purpose}"));
            return;
        }
        match self
            .runner
            .run(program, args, Some(Duration::from_secs(30)))
        {
            Ok(output) if output.ok() => info(format!("已{purpose}（{program}）")),
            Ok(output) => warn(format!(
                "{purpose}失败（{program} 退出码 {}）: {}",
                output.status,
                output.stderr_trimmed()
            )),
            Err(err) => warn(format!("{purpose}失败: {err}")),
        }
    }

    #[cfg(test)]
    fn set_mounts_override(&mut self, mounts: Vec<MountEntry>) {
        self.mounts_override = Some(mounts);
    }
}

/// 根据 mount/umount 的 stderr 给出中文排查建议。
pub fn diagnose(stderr: &str) -> Option<&'static str> {
    let text = stderr.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);

    if has("permission denied") || has("operation not permitted") {
        return Some("权限不足：挂载需要 root，且要确认设备节点可读（可检查 udev 规则与 SELinux/AppArmor 策略）");
    }
    if has("already mounted") || has("is already mounted on") {
        return Some("设备已经挂载：请先卸载已有挂载点，或直接使用当前挂载点");
    }
    if has("target is busy") || has("device is busy") || has("mountpoint is not empty") {
        return Some("挂载点被占用：请关闭正在使用该目录的程序，或换一个挂载点后重试");
    }
    if has("unknown filesystem type") || has("wrong fs type") || has("bad option") {
        return Some("内核缺少该文件系统驱动或选项不被支持：请安装对应工具（exfatprogs / ntfs-3g / hfsprogs）并检查挂载选项");
    }
    if has("ntfs") && (has("inconsistent") || has("chkdsk") || has("hiberfil") || has("dirty")) {
        return Some("NTFS 卷可能残留 Windows 快速启动/休眠标志：建议在 Windows 上执行 chkdsk /f，或改用 ntfs-3g 并加 remove_hiberfile 选项");
    }
    if has("transport endpoint is not connected") {
        return Some(
            "FUSE 连接已断开（Transport endpoint is not connected）：请先卸载再重新挂载该设备",
        );
    }
    if has("read-only") || has("readonly") {
        return Some(
            "文件系统处于只读状态：可能是硬件写保护、文件系统错误或挂载选项冲突，请检查 dmesg",
        );
    }
    if has("no such device") || has("does not exist") {
        return Some("设备节点已消失：请确认设备仍插在接口上，然后重新插拔");
    }
    None
}

fn is_busy(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("busy")
}

fn dir_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|err| Error::device(format!("无法获取当前工作目录: {err}")))?;
    Ok(cwd.join(path))
}

/// canonicalize 挂载根目录；Windows 上去掉 `\\?\` 前缀，便于与其他路径比较。
fn canonicalize_root(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => {
            #[cfg(windows)]
            {
                let text = canonical.to_string_lossy();
                if let Some(stripped) = text.strip_prefix(r"\\?\") {
                    return PathBuf::from(stripped);
                }
            }
            canonical
        }
        Err(_) => path.to_path_buf(),
    }
}

fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|err| {
        Error::device(format!("无法创建目录 {}: {err}", path.display()))
            .with_hint("请确认挂载根目录存在、可写，且没有同名文件")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 不跟随符号链接 chmod：目录被替换成链接时保持原状，交给后续检查报错。
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_dir() {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            }
        }
    }
    Ok(())
}

/// mount(8) 报“内核不认识这个类型”时，通常意味着要换 NTFS 驱动。
fn is_unknown_filesystem(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("unknown filesystem type")
        || stderr.contains("wrong fs type")
        || stderr.contains("not supported")
        || stderr.contains("no such device")
}

fn command_detail(status: i32, stderr: &str) -> String {
    if stderr.is_empty() {
        format!("退出码 {status}")
    } else {
        stderr.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CommandOutput, MockRunner};
    use crate::testutil::TempDir;

    fn test_config(dir: &TempDir) -> Config {
        let mut config = Config::default();
        config.mount_root = dir.join("mnt");
        config.state_dir = dir.join("state");
        config.fstab_path = dir.join("fstab");
        config.ctl_socket = dir.join("control.sock");
        config.per_user_dirs = true;
        config
    }

    fn test_device(name: &str, fstype: Option<&str>, label: Option<&str>) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            path: format!("/dev/{name}"),
            dev_type: "part".to_string(),
            fstype: fstype.map(str::to_string),
            label: label.map(str::to_string),
            uuid: Some("1234-ABCD".to_string()),
            partuuid: Some("0000-part1".to_string()),
            size: 16_000_000_000,
            removable: true,
            read_only: false,
            hotplug: true,
            transport: Some("usb".to_string()),
            mountpoint: None,
            model: Some("USB Flash".to_string()),
            serial: None,
            parent: Some("sdb".to_string()),
            children: Vec::new(),
        }
    }

    fn test_user() -> UserInfo {
        UserInfo {
            name: "alice".to_string(),
            uid: 1000,
            gid: 1000,
            home: Some("/home/alice".to_string()),
        }
    }

    fn mount_entry(source: &str, target: &str) -> MountEntry {
        MountEntry {
            mount_id: 42,
            parent_id: 21,
            major_minor: "8:17".to_string(),
            root: "/".to_string(),
            target: target.to_string(),
            options: vec!["rw".to_string(), "nosuid".to_string()],
            fstype: "exfat".to_string(),
            source: source.to_string(),
            super_options: vec!["rw".to_string()],
        }
    }

    #[test]
    fn dry_run_records_mount_argv_with_safety_options() {
        let dir = TempDir::new("mounter-dry");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());
        let mut manager = MountManager::with_dry_run(config, runner.clone(), true);
        let device = test_device("sdb1", Some("vfat"), Some("USB DISK"));
        let user = test_user();

        let result = manager
            .mount(&device, Some(&user), false, &[])
            .expect("dry-run 挂载应当成功");
        assert!(result.dry_run);
        assert!(!result.already_mounted);
        assert_eq!(result.fstype, "vfat");
        assert_eq!(result.driver, "vfat");
        assert_eq!(
            result.target,
            dir.join("mnt").join("alice").join("USB_DISK")
        );

        let planned = manager.planned_commands();
        assert_eq!(planned.len(), 1);
        let argv = &planned[0];
        assert_eq!(argv[0], "mount");
        assert_eq!(argv[1], "-t");
        assert_eq!(argv[2], "vfat");
        assert_eq!(argv[3], "-o");
        let options: Vec<&str> = argv[4].split(',').collect();
        assert!(options.contains(&"nosuid"), "缺少 nosuid: {options:?}");
        assert!(options.contains(&"nodev"), "缺少 nodev: {options:?}");
        assert!(options.contains(&"uid=1000"), "缺少 uid=1000: {options:?}");
        assert!(options.contains(&"gid=1000"), "缺少 gid=1000: {options:?}");
        assert!(options.contains(&"rw"), "缺少 rw: {options:?}");
        assert_eq!(argv[5], "/dev/sdb1");
        assert!(argv[6].ends_with("USB_DISK"));
        // dry-run 不应该真的调用 mount
        assert!(!runner.called("mount"));
        assert!(manager.dry_run());
    }

    #[test]
    fn reuses_existing_empty_target_but_not_foreign_mountpoints() {
        let dir = TempDir::new("mounter-target");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());
        let manager = MountManager::with_dry_run(config, runner, true);
        let device = test_device("sdb1", Some("exfat"), Some("USB DISK"));
        let user = test_user();
        let base = dir.join("mnt").join("alice");

        std::fs::create_dir_all(base.join("USB_DISK")).unwrap();
        assert_eq!(
            manager.target_path(&device, Some(&user), &[]).unwrap(),
            base.join("USB_DISK")
        );

        // 目录非空 -> 不能再复用
        std::fs::write(base.join("USB_DISK").join("data.txt"), "x").unwrap();
        assert_eq!(
            manager.target_path(&device, Some(&user), &[]).unwrap(),
            base.join("USB_DISK_2")
        );

        // 目录已经是别人的挂载点 -> 也不能复用
        let mounts = vec![mount_entry(
            "/dev/sdz1",
            &base.join("USB_DISK").display().to_string(),
        )];
        let target = manager.target_path(&device, Some(&user), &mounts).unwrap();
        assert_ne!(target, base.join("USB_DISK"));
        assert!(target.display().to_string().contains("USB_DISK"));
    }

    #[test]
    fn reports_already_mounted_without_running_commands() {
        let dir = TempDir::new("mounter-mounted");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());
        let mut manager = MountManager::with_dry_run(config, runner.clone(), true);
        manager.set_mounts_override(vec![mount_entry("/dev/sdb1", "/media/已有盘")]);

        let device = test_device("sdb1", Some("exfat"), Some("USB"));
        let result = manager.mount(&device, None, false, &[]).unwrap();
        assert!(result.already_mounted);
        assert!(!result.dry_run);
        assert_eq!(result.target, PathBuf::from("/media/已有盘"));
        assert_eq!(result.fstype, "exfat");
        assert!(
            manager.planned_commands().is_empty(),
            "已挂载时不应再生成计划"
        );
        assert!(!runner.called("mount"));
    }

    #[test]
    fn mount_failure_maps_to_hint_and_cleans_dirs() {
        let dir = TempDir::new("mounter-fail");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new().respond(
            "mount",
            CommandOutput::failure(32, "mount: /mnt/x: unknown filesystem type 'exfat'."),
        ));
        let mut manager = MountManager::with_dry_run(config, runner, false);
        let device = test_device("sdb1", Some("exfat"), Some("USB DISK"));

        let target = dir.join("mnt").join("USB_DISK");
        match manager.mount(&device, None, false, &[]) {
            Ok(result) => panic!("应当失败，却返回 {result:?}"),
            Err(err) if err.kind == ErrorKind::Permission => {
                // 非 root 环境会在 require_root 处提前返回，跳过断言
            }
            Err(err) => {
                assert_eq!(err.kind, ErrorKind::Mount, "{}", err.message);
                assert!(err.message.contains("挂载"), "{}", err.message);
                let hint = err.hint.expect("应当带有排查建议");
                assert!(hint.contains("驱动"), "{hint}");
                assert!(!target.exists(), "失败后应删除空挂载点");
            }
        }
    }

    #[test]
    fn unmount_reports_not_mounted_and_dry_run_records_argv() {
        let dir = TempDir::new("mounter-umount");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());
        let mut manager = MountManager::with_dry_run(config, runner.clone(), true);

        let err = manager
            .unmount("/media/not-there", false, false, false)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::NotMounted);
        assert!(err.message.contains("当前没有挂载"), "{}", err.message);

        manager.set_mounts_override(vec![mount_entry("/dev/sdb1", "/media/alice/USB")]);
        let target = manager
            .unmount("/media/alice/USB/", true, false, false)
            .unwrap();
        assert_eq!(target, "/media/alice/USB");
        assert_eq!(
            manager.planned_commands().last().unwrap(),
            &vec![
                "umount".to_string(),
                "-l".to_string(),
                "/media/alice/USB".to_string()
            ]
        );
        assert!(!runner.called("umount"));
    }

    #[test]
    fn busy_unmount_retries_then_reports_busy() {
        let dir = TempDir::new("mounter-busy");
        let config = test_config(&dir);
        let runner = Arc::new(
            MockRunner::new()
                .respond(
                    "umount",
                    CommandOutput::failure(32, "umount: /media/x: target is busy."),
                )
                .respond(
                    "umount",
                    CommandOutput::failure(32, "umount: /media/x: target is busy."),
                ),
        );
        let mut manager = MountManager::with_dry_run(config, runner.clone(), false);
        manager.set_mounts_override(vec![mount_entry("/dev/sdb1", "/media/x")]);

        match manager.unmount("/media/x", false, false, false) {
            Ok(target) => panic!("忙碌时不应成功：{target}"),
            Err(err) if err.kind == ErrorKind::Permission => {}
            Err(err) => {
                assert_eq!(err.kind, ErrorKind::Busy, "{}", err.message);
                assert!(
                    err.message.contains("正忙") || err.message.contains("busy"),
                    "{}",
                    err.message
                );
                let hint = err.hint.expect("应当提示 lsof/fuser");
                assert!(hint.contains("lsof"), "{hint}");
                assert_eq!(
                    runner
                        .calls()
                        .iter()
                        .filter(|(name, _)| name == "umount")
                        .count(),
                    2,
                    "忙碌时应重试一次"
                );
            }
        }
    }

    #[test]
    fn unmount_device_uses_source_lookup() {
        let dir = TempDir::new("mounter-umount-dev");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());
        let mut manager = MountManager::with_dry_run(config, runner, true);
        manager.set_mounts_override(vec![mount_entry("/dev/sdb1", "/media/alice/USB")]);
        assert_eq!(
            manager
                .unmount_device("/dev/sdb1", false, false, false)
                .unwrap(),
            "/media/alice/USB"
        );
        assert_eq!(
            manager
                .unmount_device("/dev/sr0", false, false, false)
                .unwrap_err()
                .kind,
            ErrorKind::NotMounted
        );
    }

    #[test]
    fn plans_require_filesystem_and_respect_read_only() {
        let dir = TempDir::new("mounter-plan");
        let config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());
        let manager = MountManager::with_dry_run(config, runner, true);

        let blank = test_device("sdc", None, None);
        let err = manager.plan(&blank, None, false, &[], &[]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Device);
        assert!(
            err.message.contains("没有可识别的文件系统"),
            "{}",
            err.message
        );

        // iso9660 默认只读
        let iso = test_device("sr0", Some("iso9660"), Some("光盘"));
        let plan = manager
            .plan(&iso, Some(&test_user()), false, &[], &[])
            .unwrap();
        assert_eq!(plan.driver, "iso9660");
        assert!(
            plan.options.iter().any(|option| option == "ro"),
            "{:?}",
            plan.options
        );
        assert_eq!(plan.target, dir.join("mnt").join("alice").join("光盘"));
    }

    #[test]
    fn base_dir_follows_per_user_setting() {
        let dir = TempDir::new("mounter-base");
        let mut config = test_config(&dir);
        let runner = Arc::new(MockRunner::new());

        let manager = MountManager::with_dry_run(config.clone(), runner.clone(), true);
        assert_eq!(
            manager.base_dir(Some(&test_user())),
            dir.join("mnt").join("alice")
        );
        assert_eq!(manager.base_dir(None), dir.join("mnt"));

        config.per_user_dirs = false;
        let manager = MountManager::with_dry_run(config, runner, true);
        assert_eq!(manager.base_dir(Some(&test_user())), dir.join("mnt"));
    }

    #[test]
    fn diagnoses_common_errors() {
        assert!(
            diagnose("mount: /media/x: unknown filesystem type 'exfat'.")
                .unwrap()
                .contains("驱动")
        );
        assert!(diagnose("mount: /media/x: wrong fs type, bad option")
            .unwrap()
            .contains("驱动"));
        assert!(diagnose("umount: /media/x: target is busy.")
            .unwrap()
            .contains("占用"));
        assert!(diagnose("mount: /media/x: permission denied")
            .unwrap()
            .contains("权限"));
        assert!(diagnose("mount: /media/x: already mounted on /media/y")
            .unwrap()
            .contains("已经挂载"));
        assert!(diagnose("Transport endpoint is not connected")
            .unwrap()
            .contains("FUSE"));
        assert!(diagnose("mount: /media/x: read-only file system")
            .unwrap()
            .contains("只读"));
        assert!(
            diagnose("ntfs3: sdb1: volume is dirty and \"force\" flag is not set")
                .unwrap()
                .contains("chkdsk")
        );
        assert!(diagnose("").is_none());
        assert!(diagnose("some unrelated failure").is_none());
        assert!(!is_busy("unknown filesystem type"));
        assert!(is_busy("umount: /media/x: target is busy."));
    }
}
