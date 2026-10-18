//! Core of mounting and unmounting.
//!
//! Every system interaction goes through [`CommandRunner`] (replaced by
//! [`crate::exec::MockRunner`] in tests), so mount plan assembly, mount point
//! selection and error diagnosis can all be verified on a non-Linux dev machine.

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

/// Maximum time to wait for the kernel to record a successful mount in mountinfo.
const MOUNT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
/// Maximum time to wait for the mount record to disappear after unmounting.
const UNMOUNT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval for mountinfo.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Performs mounts/unmounts and maintains mount point directories.
pub struct MountManager {
    config: Config,
    runner: Arc<dyn CommandRunner>,
    dry_run: bool,
    supported: HashSet<String>,
    planned: Vec<Vec<String>>,
    /// Directories created by this manager during this process; unmount/failure cleanup
    /// only removes these, so pre-created admin directories are never deleted by mistake.
    created_dirs: HashSet<PathBuf>,
    /// Only used by tests to inject mountinfo (None in real runs, which read `/proc/self/mountinfo`).
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
        debug(format!(
            "kernel reports {} supported filesystems",
            supported.len()
        ));
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

    /// Mount root directory (including the per-user subdirectory).
    pub fn base_dir(&self, user: Option<&UserInfo>) -> PathBuf {
        // Pin to the real path so the containment check and the actual mount location
        // agree when mount_root is a symlink.
        let mut base = canonicalize_root(&self.config.mount_root);
        if self.config.per_user_dirs {
            if let Some(user) = user {
                let fallback = format!("user{}", user.uid);
                base.push(naming::sanitize_label(Some(user.name.as_str()), &fallback));
            }
        }
        base
    }

    /// Compute the mount point: reuse an existing empty directory instead of creating `<label>_2`.
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
            debug(format!(
                "reusing existing empty mount point {}",
                candidate.display()
            ));
            candidate
        } else {
            naming::unique_dir(&base, &name, &reserved)
        };

        self.validate_target(&base, &target)?;
        Ok(target)
    }

    /// Build a mount plan (no side effects), used by dry-run and tests.
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
                "device {} has no recognizable filesystem, cannot mount it",
                device.path
            ))
            .with_hint(
                "Format the device first (e.g. mkfs.exfat), or use 'automountd list --all' to check its state",
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
            platform::require_root("mounting")?;
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
                "{} is already mounted at {}, skipping",
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
            info(format!("dry-run: would run {}", plan.describe()));
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
        // Use the canonical mount_root as the prefix so a symlinked mount_root is not misjudged.
        let canonical_root = canonicalize_root(&self.config.mount_root);
        naming::check_no_symlink_components(&canonical_root, &plan.target)?;
        ensure_dir(&plan.target)?;
        if !target_existed {
            self.created_dirs.insert(plan.target.clone());
        }
        // Pin the expected real path before mounting and re-check it afterwards, closing the
        // "swap in a symlink after the check" race.
        let expected_target = canonicalize_root(&plan.target);

        if let Some(user) = user {
            if fstypes::needs_ownership_fix(&plan.fstype, &self.config) {
                match plan.target.symlink_metadata() {
                    Ok(metadata) if !metadata.file_type().is_symlink() => {}
                    _ => {
                        self.cleanup_empty_dirs(&plan.target);
                        return Err(Error::mount(format!(
                            "mount point {} became a symlink or disappeared after creation; aborting",
                            plan.target.display()
                        ))
                        .with_hint(
                            "Another process may be manipulating the mount directory; check it and retry"
                        ));
                    }
                }
                if let Err(err) = platform::chown(&plan.target, user.uid, user.gid) {
                    warn(format!(
                        "failed to hand mount point {} to {}({}): {err}",
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
        debug(format!("running {}", plan.describe()));
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
        // The two NTFS drivers: the in-kernel ntfs3 may just be an unloaded module (and thus
        // absent from /proc/filesystems), while conversely an old kernel may only have ntfs-3g.
        // On failure, retry once with the other driver automatically.
        if !output.ok() && is_unknown_filesystem(&output.stderr) {
            let alternative = match driver.as_str() {
                "ntfs3" => Some("ntfs-3g"),
                "ntfs-3g" => Some("ntfs3"),
                _ => None,
            };
            if let Some(alternative) = alternative {
                warn(format!(
                    "mounting {} with {driver} failed, retrying with {alternative}",
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
            let message = format!(
                "failed to mount {} at {}: {detail}",
                device.path, target_text
            );
            return Err(match diagnose(&output.stderr) {
                Some(hint) => Error::mount(message).with_hint(hint),
                None => Error::mount(message),
            });
        }

        if !self.wait_until("mount", &target_text, MOUNT_CONFIRM_TIMEOUT, true) {
            self.cleanup_empty_dirs(&plan.target);
            return Err(
                Error::mount(format!(
                    "the mount command returned, but {target_text} does not appear in the mount table"
                ))
                .with_hint(
                    "Check the kernel log with dmesg; make sure the device is still online and the filesystem is healthy"
                ),
            );
        }

        // Guard against the symlink race: confirm the kernel really mounted on the directory
        // pinned before mount.
        let expected = mountinfo::normalize_path(&expected_target.display().to_string());
        let actual = mountinfo::find_mount(&self.current_mounts(), Some(&target_text), None)
            .map(|entry| mountinfo::normalize_path(&entry.target));
        let target_now = canonicalize_root(&plan.target);
        let target_now = mountinfo::normalize_path(&target_now.display().to_string());
        if actual.as_deref() != Some(expected.as_str()) || target_now != expected {
            let _ = self.runner.run(
                "umount",
                &[target_text.as_str()],
                Some(Duration::from_secs(10)),
            );
            self.cleanup_empty_dirs(&plan.target);
            return Err(Error::mount(format!(
                "actual mount point differs from the expected one (expected {expected}, actual {}, current path {target_now})",
                actual.unwrap_or_else(|| "unknown".to_string())
            ))
            .with_hint(
                "The mount directory may have been swapped during mounting; check whether another process touches that path"
            ));
        }

        info(format!("mounted {} at {target_text}", device.path));
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

    /// Unmount the given mount point and return the unmounted target.
    pub fn unmount(
        &mut self,
        target: &str,
        lazy: bool,
        eject: bool,
        power_off: bool,
    ) -> Result<String> {
        if !self.dry_run {
            platform::require_root("unmounting")?;
        }

        let mounts = self.current_mounts();
        let Some(entry) = mountinfo::find_mount(&mounts, Some(target), None) else {
            return Err(
                Error::new(ErrorKind::NotMounted, format!("{target} is not mounted")).with_hint(
                    "Use 'automountd list --all' to check the mount state, or verify the mount point path",
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
            info(format!(
                "dry-run: would run umount {}",
                umount_args.join(" ")
            ));
            return Ok(mounted_target);
        }

        let timeout = Duration::from_secs(self.config.unmount_timeout_seconds.max(1));
        let args: Vec<&str> = umount_args.iter().map(String::as_str).collect();
        let mut output = self.runner.run("umount", &args, Some(timeout))?;
        if !output.ok() && is_busy(&output.stderr) {
            warn(format!(
                "{mounted_target} is busy, retrying once after 300ms"
            ));
            std::thread::sleep(Duration::from_millis(300));
            output = self.runner.run("umount", &args, Some(timeout))?;
        }
        if !output.ok() {
            let detail = command_detail(output.status, output.stderr_trimmed());
            let message = format!("failed to unmount {mounted_target}: {detail}");
            if is_busy(&output.stderr) {
                return Err(Error::new(ErrorKind::Busy, message).with_hint(
                    "Use lsof / fuser -vm to find the process holding the mount point, or run 'automountd umount --lazy <mount-point>'",
                ));
            }
            return Err(match diagnose(&output.stderr) {
                Some(hint) => Error::mount(message).with_hint(hint),
                None => Error::mount(message),
            });
        }

        if !self.wait_until("umount", &mounted_target, UNMOUNT_CONFIRM_TIMEOUT, false) {
            return Err(Error::new(
                ErrorKind::Busy,
                format!("{mounted_target} still appears in the mount table"),
            )
            .with_hint(
                "A process may still be using the mount point; try 'automountd umount --lazy' or clean up after a reboot"
            ));
        }

        if self.config.cleanup_dirs {
            self.cleanup_empty_dirs(Path::new(&mounted_target));
        }

        let wants_eject = eject || self.config.eject_after_unmount;
        let wants_power_off = power_off || self.config.power_off_after_unmount;
        if use_lazy && (wants_eject || wants_power_off) {
            warn(format!(
                "{} was lazily unmounted and a process may still use the device; skipping eject/power-off to avoid data loss",
                mounted_target
            ));
        } else {
            if wants_eject {
                self.try_optional("eject", &[source.as_str()], "ejecting the device");
            }
            if wants_power_off {
                self.try_optional(
                    "udisksctl",
                    &["power-off", "-b", source.as_str()],
                    "powering off the device",
                );
            }
        }

        info(format!("unmounted {mounted_target}"));
        Ok(mounted_target)
    }

    /// Unmount by device path (using the first mount point).
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
                format!("{device_path} has no mount point"),
            )
            .with_hint("Use 'automountd list --all' to confirm the device path"));
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

    /// An existing empty directory can be reused, so a new `<label>_2` is not created on every replug.
    fn can_reuse_dir(&self, candidate: &Path, reserved: &HashSet<String>) -> bool {
        if candidate.is_symlink() || !candidate.is_dir() {
            return false;
        }
        if reserved.contains(&mountinfo::normalize_path(&candidate.display().to_string())) {
            return false;
        }
        dir_is_empty(candidate)
    }

    /// Ensure the mount point stays inside the mount root and that no component of the path is a symlink.
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
                "mount point {} is not inside the mount root {}",
                target.display(),
                base.display()
            ))
            .with_hint("The label may contain suspicious characters; the operation was refused"));
        }
        naming::check_no_symlink_components(&base_abs, &target_abs)?;
        Ok(())
    }

    /// Poll mountinfo until the mount table reflects the change.
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
                    "{target} mount state did not settle within {}ms after {action}",
                    limit.as_millis()
                ));
                return false;
            }
            std::thread::sleep(CONFIRM_POLL_INTERVAL);
        }
    }

    /// Remove only the empty directories created by this manager and located under mount_root.
    fn cleanup_empty_dirs(&mut self, target: &Path) {
        let root = canonicalize_root(&self.config.mount_root);
        let Ok(target_abs) = absolute_path(target) else {
            return;
        };
        let target_abs = canonicalize_root(&target_abs);
        if target_abs == root || !target_abs.starts_with(&root) {
            return;
        }
        if self.created_dirs.remove(&target_abs) && dir_is_empty(&target_abs) {
            if let Err(err) = fs::remove_dir(&target_abs) {
                debug(format!(
                    "failed to remove empty directory {}: {err}",
                    target_abs.display()
                ));
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
                debug(format!(
                    "failed to remove empty directory {}: {err}",
                    parent.display()
                ));
            }
        }
    }

    /// A failed optional action (eject / power-off) is only logged as a warning and does not
    /// affect the unmount result.
    fn try_optional(&self, program: &str, args: &[&str], purpose: &str) {
        if !self.runner.which(program) {
            warn(format!("{program} not found, skipping {purpose}"));
            return;
        }
        match self
            .runner
            .run(program, args, Some(Duration::from_secs(30)))
        {
            Ok(output) if output.ok() => info(format!("{purpose} succeeded ({program})")),
            Ok(output) => warn(format!(
                "{purpose} failed ({program} exit code {}): {}",
                output.status,
                output.stderr_trimmed()
            )),
            Err(err) => warn(format!("{purpose} failed: {err}")),
        }
    }

    #[cfg(test)]
    fn set_mounts_override(&mut self, mounts: Vec<MountEntry>) {
        self.mounts_override = Some(mounts);
    }
}

/// Turn mount/umount stderr into a troubleshooting hint.
pub fn diagnose(stderr: &str) -> Option<&'static str> {
    let text = stderr.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);

    if has("permission denied") || has("operation not permitted") {
        return Some("Permission denied: mounting needs root, and the device node must be readable (check udev rules and SELinux/AppArmor policy)");
    }
    if has("already mounted") || has("is already mounted on") {
        return Some("Device already mounted: unmount the existing mount point first, or just use the current one");
    }
    if has("target is busy") || has("device is busy") || has("mountpoint is not empty") {
        return Some("Mount point is busy: close the programs using that directory, or pick another mount point and retry");
    }
    if has("unknown filesystem type") || has("wrong fs type") || has("bad option") {
        return Some("The kernel lacks this filesystem driver or does not support the options: install the matching tools (exfatprogs / ntfs-3g / hfsprogs) and check the mount options");
    }
    if has("ntfs") && (has("inconsistent") || has("chkdsk") || has("hiberfil") || has("dirty")) {
        return Some("The NTFS volume may still carry Windows fast-startup/hibernation flags: run chkdsk /f on Windows, or switch to ntfs-3g with the remove_hiberfile option");
    }
    if has("transport endpoint is not connected") {
        return Some(
            "FUSE connection lost (Transport endpoint is not connected): unmount the device, then mount it again",
        );
    }
    if has("read-only") || has("readonly") {
        return Some(
            "The filesystem is read-only: possibly hardware write protection, filesystem errors or conflicting mount options; check dmesg",
        );
    }
    if has("no such device") || has("does not exist") {
        return Some(
            "The device node is gone: make sure the device is still plugged in, then re-plug it",
        );
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
        .map_err(|err| Error::device(format!("cannot determine the current directory: {err}")))?;
    Ok(cwd.join(path))
}

/// Canonicalize the mount root; on Windows the `\\?\` prefix is stripped so it compares cleanly with other paths.
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
        Error::device(format!(
            "failed to create directory {}: {err}",
            path.display()
        ))
        .with_hint(
            "Make sure the mount root exists, is writable, and holds no file of the same name",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Do not chmod through a symlink: if the directory was swapped for a link, leave it
        // as is and let the subsequent checks report the problem.
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_dir() {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            }
        }
    }
    Ok(())
}

/// When mount(8) says "unknown filesystem type", it usually means the NTFS driver has to be switched.
fn is_unknown_filesystem(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("unknown filesystem type")
        || stderr.contains("wrong fs type")
        || stderr.contains("not supported")
        || stderr.contains("no such device")
}

fn command_detail(status: i32, stderr: &str) -> String {
    if stderr.is_empty() {
        format!("exit code {status}")
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
            .expect("dry-run mount should succeed");
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
        assert!(options.contains(&"nosuid"), "missing nosuid: {options:?}");
        assert!(options.contains(&"nodev"), "missing nodev: {options:?}");
        assert!(
            options.contains(&"uid=1000"),
            "missing uid=1000: {options:?}"
        );
        assert!(
            options.contains(&"gid=1000"),
            "missing gid=1000: {options:?}"
        );
        assert!(options.contains(&"rw"), "missing rw: {options:?}");
        assert_eq!(argv[5], "/dev/sdb1");
        assert!(argv[6].ends_with("USB_DISK"));
        // dry-run must not actually invoke mount
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

        // non-empty directory -> cannot be reused
        std::fs::write(base.join("USB_DISK").join("data.txt"), "x").unwrap();
        assert_eq!(
            manager.target_path(&device, Some(&user), &[]).unwrap(),
            base.join("USB_DISK_2")
        );

        // directory is already someone else's mount point -> also not reusable
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
        manager.set_mounts_override(vec![mount_entry("/dev/sdb1", "/media/existing-disk")]);

        let device = test_device("sdb1", Some("exfat"), Some("USB"));
        let result = manager.mount(&device, None, false, &[]).unwrap();
        assert!(result.already_mounted);
        assert!(!result.dry_run);
        assert_eq!(result.target, PathBuf::from("/media/existing-disk"));
        assert_eq!(result.fstype, "exfat");
        assert!(
            manager.planned_commands().is_empty(),
            "an already mounted device must not produce a plan"
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
            Ok(result) => panic!("should have failed but returned {result:?}"),
            Err(err) if err.kind == ErrorKind::Permission => {
                // A non-root environment returns early at require_root, so skip the assertions
            }
            Err(err) => {
                assert_eq!(err.kind, ErrorKind::Mount, "{}", err.message);
                assert!(err.message.contains("mount"), "{}", err.message);
                let hint = err.hint.expect("should carry a troubleshooting hint");
                assert!(hint.contains("driver"), "{hint}");
                assert!(
                    !target.exists(),
                    "the empty mount point should be removed after a failure"
                );
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
        assert!(err.message.contains("not mounted"), "{}", err.message);

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
            Ok(target) => panic!("should not succeed while busy: {target}"),
            Err(err) if err.kind == ErrorKind::Permission => {}
            Err(err) => {
                assert_eq!(err.kind, ErrorKind::Busy, "{}", err.message);
                assert!(err.message.contains("busy"), "{}", err.message);
                let hint = err.hint.expect("should hint at lsof/fuser");
                assert!(hint.contains("lsof"), "{hint}");
                assert_eq!(
                    runner
                        .calls()
                        .iter()
                        .filter(|(name, _)| name == "umount")
                        .count(),
                    2,
                    "umount should be retried once while busy"
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
            err.message.contains("no recognizable filesystem"),
            "{}",
            err.message
        );

        // iso9660 is read-only by default
        let iso = test_device("sr0", Some("iso9660"), Some("CD-ROM"));
        let plan = manager
            .plan(&iso, Some(&test_user()), false, &[], &[])
            .unwrap();
        assert_eq!(plan.driver, "iso9660");
        assert!(
            plan.options.iter().any(|option| option == "ro"),
            "{:?}",
            plan.options
        );
        assert_eq!(plan.target, dir.join("mnt").join("alice").join("CD-ROM"));
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
                .contains("driver")
        );
        assert!(diagnose("mount: /media/x: wrong fs type, bad option")
            .unwrap()
            .contains("driver"));
        assert!(diagnose("umount: /media/x: target is busy.")
            .unwrap()
            .contains("busy"));
        assert!(diagnose("mount: /media/x: permission denied")
            .unwrap()
            .contains("Permission"));
        assert!(diagnose("mount: /media/x: already mounted on /media/y")
            .unwrap()
            .contains("already mounted"));
        assert!(diagnose("Transport endpoint is not connected")
            .unwrap()
            .contains("FUSE"));
        assert!(diagnose("mount: /media/x: read-only file system")
            .unwrap()
            .contains("read-only"));
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
