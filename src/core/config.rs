//! 配置：INI 风格文本、默认值、校验与持久化。
//!
//! 当前不引入 serde/TOML，使用轻量 INI 风格解析器。格式示例::
//!
//! ```ini
//! # 挂载根目录
//! mount_root = "/media"
//! per_user_dirs = true
//! allowed_uuids = 1234-ABCD, 5678-EFGH
//!
//! [mount_options]
//! vfat = uid={uid}, gid={gid}, umask=0022, shortname=mixed, utf8
//! exfat = uid={uid}, gid={gid}, umask=0022, iocharset=utf8
//! ```

use std::collections::BTreeMap;
use std::env;
use std::fmt::Display;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::error::{config_problems, Error, Result};
use crate::logging;

pub const ENV_CONFIG: &str = "AUTOMOUNTD_CONFIG";
pub const SYSTEM_CONFIG: &str = "/etc/automountd/config.conf";
pub const USER_CONFIG: &str = "~/.config/automountd/config.conf";

/// 给移动磁盘默认加上的安全选项。
pub const DEFAULT_SAFETY_OPTIONS: &[&str] = &["nosuid", "nodev"];

pub fn default_mount_options() -> BTreeMap<String, Vec<String>> {
    let mut map = BTreeMap::new();
    map.insert(
        "vfat".to_string(),
        strings(&[
            "uid={uid}",
            "gid={gid}",
            "umask=0022",
            "shortname=mixed",
            "utf8",
        ]),
    );
    map.insert(
        "exfat".to_string(),
        strings(&["uid={uid}", "gid={gid}", "umask=0022", "iocharset=utf8"]),
    );
    for key in ["ntfs", "ntfs3", "ntfs-3g"] {
        map.insert(
            key.to_string(),
            strings(&["uid={uid}", "gid={gid}", "umask=0022"]),
        );
    }
    for key in ["iso9660", "udf"] {
        map.insert(
            key.to_string(),
            strings(&["uid={uid}", "gid={gid}", "umask=0022"]),
        );
    }
    map
}

pub fn default_ignored_fstypes() -> Vec<String> {
    strings(&[
        "swap",
        "crypto_LUKS",
        "LVM2_member",
        "linux_raid_member",
        "isw_raid_member",
        "ddf_raid_member",
        "zfs_member",
        "BitLocker",
        "bcache",
        "VMFS",
    ])
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // 挂载位置
    pub mount_root: PathBuf,
    pub per_user_dirs: bool,
    pub user: Option<String>,

    // 触发策略
    pub auto_mount_on_hotplug: bool,
    pub auto_mount_on_boot: bool,
    pub auto_mount_internal_disks: bool,
    pub allowed_uuids: Vec<String>,
    pub blocked_uuids: Vec<String>,
    pub blocked_labels: Vec<String>,
    pub ignored_fstypes: Vec<String>,
    pub read_only_fstypes: Vec<String>,

    // 挂载参数
    pub mount_options: BTreeMap<String, Vec<String>>,
    pub default_mount_options: Vec<String>,
    pub chown_native_filesystems: bool,
    pub lazy_umount: bool,
    pub cleanup_dirs: bool,
    pub eject_after_unmount: bool,
    pub power_off_after_unmount: bool,

    // 时序
    pub debounce_seconds: f64,
    pub retry_attempts: u32,
    pub retry_delay_seconds: f64,
    pub mount_timeout_seconds: u64,
    pub unmount_timeout_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub poll_interval_seconds: f64,
    pub suppress_remount_seconds: u64,

    // 通知、状态、控制接口
    pub notify: bool,
    pub state_dir: PathBuf,
    pub ctl_socket: PathBuf,
    pub ctl_allowed_users: Vec<String>,

    // fstab 持久化
    pub fstab_path: PathBuf,
    pub persist_enabled: bool,

    // 日志
    pub log_level: String,
    pub log_file: Option<String>,
    pub dry_run: bool,

    /// 配置文件来源（不参与序列化）。
    pub path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mount_root: PathBuf::from("/media"),
            per_user_dirs: true,
            user: None,
            auto_mount_on_hotplug: true,
            auto_mount_on_boot: true,
            auto_mount_internal_disks: false,
            allowed_uuids: Vec::new(),
            blocked_uuids: Vec::new(),
            blocked_labels: Vec::new(),
            ignored_fstypes: default_ignored_fstypes(),
            read_only_fstypes: strings(&["iso9660", "squashfs", "erofs"]),
            mount_options: default_mount_options(),
            default_mount_options: strings(DEFAULT_SAFETY_OPTIONS),
            chown_native_filesystems: false,
            lazy_umount: false,
            cleanup_dirs: true,
            eject_after_unmount: false,
            power_off_after_unmount: false,
            debounce_seconds: 1.0,
            retry_attempts: 3,
            retry_delay_seconds: 2.0,
            mount_timeout_seconds: 60,
            unmount_timeout_seconds: 30,
            reconcile_interval_seconds: 30,
            poll_interval_seconds: 0.0,
            suppress_remount_seconds: 60,
            notify: true,
            state_dir: PathBuf::from("/run/automountd"),
            ctl_socket: PathBuf::from("/run/automountd/control.sock"),
            ctl_allowed_users: vec!["*".to_string()],
            fstab_path: PathBuf::from("/etc/fstab"),
            persist_enabled: true,
            log_level: "INFO".to_string(),
            log_file: None,
            dry_run: false,
            path: None,
        }
    }
}

impl Config {
    /// 按优先级加载：显式路径 -> 环境变量 -> 系统配置 -> 用户配置 -> 默认值。
    pub fn load(path: Option<&Path>) -> Result<Self> {
        if let Some(path) = path {
            let expanded = expand_tilde(path);
            if !expanded.is_file() {
                return Err(
                    Error::config(format!("配置文件不存在: {}", expanded.display()))
                        .with_hint("运行 'automountd config init' 生成默认配置"),
                );
            }
            return Self::from_file(&expanded);
        }

        if let Some(value) = env::var_os(ENV_CONFIG) {
            let expanded = expand_tilde(Path::new(&value));
            if expanded.is_file() {
                return Self::from_file(&expanded);
            }
            logging::warn(format!(
                "环境变量 {ENV_CONFIG} 指向的文件不存在: {}",
                expanded.display()
            ));
        }

        for candidate in [SYSTEM_CONFIG, USER_CONFIG] {
            let expanded = expand_tilde(Path::new(candidate));
            if expanded.is_file() {
                return Self::from_file(&expanded);
            }
        }

        Ok(Self::default())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .map_err(|err| Error::config(format!("无法读取配置文件 {}: {err}", path.display())))?;
        let mut config = Self::parse(&text)?;
        config.path = Some(path.to_path_buf());
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut config = Self::default();
        let mut section = String::new();

        for (index, raw_line) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = strip_comment(raw_line);
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('[') {
                if !trimmed.ends_with(']') {
                    return Err(Error::config(format!(
                        "第 {line_number} 行：节标题缺少 ']': {trimmed}"
                    )));
                }
                section = trimmed[1..trimmed.len() - 1].trim().to_string();
                if !section.is_empty() && section != "mount_options" {
                    logging::warn(format!("忽略未知配置节 [{section}]（第 {line_number} 行）"));
                }
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                return Err(Error::config(format!(
                    "第 {line_number} 行不是 key = value 形式: {trimmed}"
                )));
            };
            let key = key.trim();
            if key.is_empty() {
                return Err(Error::config(format!("第 {line_number} 行缺少键名")));
            }
            let full_key = if section.is_empty() {
                key.to_string()
            } else {
                format!("{section}.{key}")
            };
            config.set_value(&full_key, value.trim(), line_number)?;
        }

        config.validate()?;
        Ok(config)
    }

    fn set_value(&mut self, key: &str, value: &str, line: usize) -> Result<()> {
        if let Some(name) = key.strip_prefix("mount_options.") {
            if name.is_empty() {
                return Err(Error::config(format!(
                    "第 {line} 行：mount_options 下的文件系统名为空"
                )));
            }
            self.mount_options
                .insert(name.to_string(), parse_string_list(value));
            return Ok(());
        }

        match key {
            "mount_root" => self.mount_root = PathBuf::from(parse_string(value)),
            "per_user_dirs" => self.per_user_dirs = parse_bool(value, key, line)?,
            "user" => {
                let name = parse_string(value);
                self.user = if name.is_empty() { None } else { Some(name) };
            }
            "auto_mount_on_hotplug" => self.auto_mount_on_hotplug = parse_bool(value, key, line)?,
            "auto_mount_on_boot" => self.auto_mount_on_boot = parse_bool(value, key, line)?,
            "auto_mount_internal_disks" => {
                self.auto_mount_internal_disks = parse_bool(value, key, line)?
            }
            "allowed_uuids" => self.allowed_uuids = parse_string_list(value),
            "blocked_uuids" => self.blocked_uuids = parse_string_list(value),
            "blocked_labels" => self.blocked_labels = parse_string_list(value),
            "ignored_fstypes" => self.ignored_fstypes = parse_string_list(value),
            "read_only_fstypes" => self.read_only_fstypes = parse_string_list(value),
            "default_mount_options" => self.default_mount_options = parse_string_list(value),
            "chown_native_filesystems" => {
                self.chown_native_filesystems = parse_bool(value, key, line)?
            }
            "lazy_umount" => self.lazy_umount = parse_bool(value, key, line)?,
            "cleanup_dirs" => self.cleanup_dirs = parse_bool(value, key, line)?,
            "eject_after_unmount" => self.eject_after_unmount = parse_bool(value, key, line)?,
            "power_off_after_unmount" => {
                self.power_off_after_unmount = parse_bool(value, key, line)?
            }
            "debounce_seconds" => self.debounce_seconds = parse_number(value, key, line)?,
            "retry_attempts" => self.retry_attempts = parse_number(value, key, line)?,
            "retry_delay_seconds" => self.retry_delay_seconds = parse_number(value, key, line)?,
            "mount_timeout_seconds" => self.mount_timeout_seconds = parse_number(value, key, line)?,
            "unmount_timeout_seconds" => {
                self.unmount_timeout_seconds = parse_number(value, key, line)?
            }
            "reconcile_interval_seconds" => {
                self.reconcile_interval_seconds = parse_number(value, key, line)?
            }
            "poll_interval_seconds" => self.poll_interval_seconds = parse_number(value, key, line)?,
            "suppress_remount_seconds" => {
                self.suppress_remount_seconds = parse_number(value, key, line)?
            }
            "notify" => self.notify = parse_bool(value, key, line)?,
            "state_dir" => self.state_dir = PathBuf::from(parse_string(value)),
            "ctl_socket" => self.ctl_socket = PathBuf::from(parse_string(value)),
            "ctl_allowed_users" => self.ctl_allowed_users = parse_string_list(value),
            "fstab_path" => self.fstab_path = PathBuf::from(parse_string(value)),
            "persist_enabled" => self.persist_enabled = parse_bool(value, key, line)?,
            "log_level" => self.log_level = parse_string(value),
            "log_file" => {
                let text = parse_string(value);
                self.log_file = if text.is_empty() { None } else { Some(text) };
            }
            "dry_run" => self.dry_run = parse_bool(value, key, line)?,
            "version" | "config_version" => {}
            other => {
                logging::warn(format!("第 {line} 行：忽略未知配置项 {other}"));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();

        check_absolute(&mut problems, "mount_root", &self.mount_root);
        check_absolute(&mut problems, "state_dir", &self.state_dir);
        check_absolute(&mut problems, "ctl_socket", &self.ctl_socket);
        check_absolute(&mut problems, "fstab_path", &self.fstab_path);

        if self.mount_root.is_absolute() || self.mount_root.to_string_lossy().starts_with('/') {
            let normalized = normalize_path_text(&self.mount_root);
            if normalized == "/" {
                problems.push("mount_root 不能是 /".to_string());
            }
            for forbidden in [
                "/proc", "/sys", "/dev", "/boot", "/etc", "/usr", "/bin", "/sbin",
            ] {
                if normalized == forbidden || normalized.starts_with(&format!("{forbidden}/")) {
                    problems.push(format!("mount_root 不能位于 {forbidden} 之下"));
                    break;
                }
            }
        }

        if let Some(user) = &self.user {
            if user.trim().is_empty() {
                problems.push("user 不能是空字符串".to_string());
            }
        }

        if self.mount_options.values().any(|opts| opts.is_empty()) {
            problems.push("mount_options 中存在空选项列表".to_string());
        }

        for (name, value, low, high) in [
            ("debounce_seconds", self.debounce_seconds, 0.0, 30.0),
            ("retry_delay_seconds", self.retry_delay_seconds, 0.1, 60.0),
            (
                "poll_interval_seconds",
                self.poll_interval_seconds,
                0.0,
                3600.0,
            ),
        ] {
            if !value.is_finite() || value < low || value > high {
                problems.push(format!("{name} 必须在 {low}~{high} 之间，当前为 {value}"));
            }
        }

        if self.retry_attempts < 1 || self.retry_attempts > 10 {
            problems.push(format!(
                "retry_attempts 必须在 1~10 之间，当前为 {}",
                self.retry_attempts
            ));
        }

        for (name, value, low, high) in [
            ("mount_timeout_seconds", self.mount_timeout_seconds, 5, 600),
            (
                "unmount_timeout_seconds",
                self.unmount_timeout_seconds,
                5,
                600,
            ),
            (
                "reconcile_interval_seconds",
                self.reconcile_interval_seconds,
                0,
                3600,
            ),
            (
                "suppress_remount_seconds",
                self.suppress_remount_seconds,
                0,
                86_400,
            ),
        ] {
            if value < low || value > high {
                problems.push(format!("{name} 必须在 {low}~{high} 之间，当前为 {value}"));
            }
        }

        match self.log_level.to_ascii_uppercase().as_str() {
            "ERROR" | "WARN" | "WARNING" | "INFO" | "DEBUG" => {}
            other => problems.push(format!(
                "log_level 必须是 ERROR/WARN/INFO/DEBUG，当前为 {other}"
            )),
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(config_problems(&problems))
        }
    }

    pub fn mount_options_for(&self, driver: &str, fstype: &str) -> Vec<String> {
        if let Some(options) = self.mount_options.get(driver) {
            return options.clone();
        }
        if let Some(options) = self.mount_options.get(fstype) {
            return options.clone();
        }
        Vec::new()
    }

    pub fn is_uuid_allowed(&self, uuid: Option<&str>) -> bool {
        if self.allowed_uuids.is_empty() {
            return true;
        }
        let Some(uuid) = uuid else {
            return false;
        };
        let wanted = uuid.trim().to_ascii_uppercase();
        self.allowed_uuids
            .iter()
            .map(|item| strip_uuid_prefix(item).to_ascii_uppercase())
            .any(|item| item == wanted)
    }

    /// 生成带注释的配置文件文本。
    pub fn to_conf(&self) -> String {
        let mut out = String::new();
        out.push_str("# automountd 配置文件\n");
        out.push_str("# 修改后执行: sudo systemctl restart automountd\n");
        out.push_str("# 或: sudo automountd reload（无需重启服务）\n\n");

        out.push_str("# 挂载根目录。per_user_dirs=true 时挂载到 <mount_root>/<用户名>/<卷标>\n");
        out.push_str(&format!("mount_root = \"{}\"\n", self.mount_root.display()));
        out.push_str("per_user_dirs = ");
        out.push_str(bool_str(self.per_user_dirs));
        out.push_str("\n# 强制指定挂载点所有者；留空表示自动识别当前桌面登录用户\n");
        out.push_str(&format!(
            "user = \"{}\"\n\n",
            self.user.as_deref().unwrap_or("")
        ));

        out.push_str("# ---------- 触发策略 ----------\n");
        out.push_str("# 插入可移动磁盘时自动挂载\n");
        out.push_str(&format!(
            "auto_mount_on_hotplug = {}\n",
            bool_str(self.auto_mount_on_hotplug)
        ));
        out.push_str("# 服务启动时挂载已插入的磁盘\n");
        out.push_str(&format!(
            "auto_mount_on_boot = {}\n",
            bool_str(self.auto_mount_on_boot)
        ));
        out.push_str("# 是否也自动挂载内置 SATA/NVMe 数据盘（默认关闭，避免碰系统盘）\n");
        out.push_str(&format!(
            "auto_mount_internal_disks = {}\n",
            bool_str(self.auto_mount_internal_disks)
        ));
        out.push_str("# 白名单：非空时只自动挂载这些 UUID（可用于放行内置磁盘）\n");
        out.push_str(&format!(
            "allowed_uuids = {}\n",
            self.allowed_uuids.join(", ")
        ));
        out.push_str("# 排除列表\n");
        out.push_str(&format!(
            "blocked_uuids = {}\n",
            self.blocked_uuids.join(", ")
        ));
        out.push_str(&format!(
            "blocked_labels = {}\n",
            self.blocked_labels.join(", ")
        ));
        out.push_str(&format!(
            "ignored_fstypes = {}\n",
            self.ignored_fstypes.join(", ")
        ));
        out.push_str(&format!(
            "read_only_fstypes = {}\n\n",
            self.read_only_fstypes.join(", ")
        ));

        out.push_str("# ---------- 挂载参数 ----------\n");
        out.push_str("# 所有设备都会附加的安全选项；ext4/xfs 等原生文件系统同样适用\n");
        out.push_str(&format!(
            "default_mount_options = {}\n",
            self.default_mount_options.join(", ")
        ));
        out.push_str("# 原生文件系统（ext4/xfs/btrfs）是否强行 chown 挂载点（一般不必要）\n");
        out.push_str(&format!(
            "chown_native_filesystems = {}\n",
            bool_str(self.chown_native_filesystems)
        ));
        out.push_str("# 卸载失败时自动改用 umount -l（lazy）\n");
        out.push_str(&format!("lazy_umount = {}\n", bool_str(self.lazy_umount)));
        out.push_str("# 卸载后删除空挂载点目录\n");
        out.push_str(&format!("cleanup_dirs = {}\n", bool_str(self.cleanup_dirs)));
        out.push_str(&format!(
            "eject_after_unmount = {}\n",
            bool_str(self.eject_after_unmount)
        ));
        out.push_str(&format!(
            "power_off_after_unmount = {}\n\n",
            bool_str(self.power_off_after_unmount)
        ));

        out.push_str("# ---------- 时序（秒） ----------\n");
        out.push_str("# udev 事件去抖窗口\n");
        out.push_str(&format!("debounce_seconds = {}\n", self.debounce_seconds));
        out.push_str("# 挂载失败重试\n");
        out.push_str(&format!("retry_attempts = {}\n", self.retry_attempts));
        out.push_str(&format!(
            "retry_delay_seconds = {}\n",
            self.retry_delay_seconds
        ));
        out.push_str(&format!(
            "mount_timeout_seconds = {}\n",
            self.mount_timeout_seconds
        ));
        out.push_str(&format!(
            "unmount_timeout_seconds = {}\n",
            self.unmount_timeout_seconds
        ));
        out.push_str("# 定期扫描间隔；0 表示关闭\n");
        out.push_str(&format!(
            "reconcile_interval_seconds = {}\n",
            self.reconcile_interval_seconds
        ));
        out.push_str("# 无 udev 环境（容器）改用轮询的间隔；0 表示自动\n");
        out.push_str(&format!(
            "poll_interval_seconds = {}\n",
            self.poll_interval_seconds
        ));
        out.push_str("# 用户手动卸载后，多少秒内不再自动挂载\n");
        out.push_str(&format!(
            "suppress_remount_seconds = {}\n\n",
            self.suppress_remount_seconds
        ));

        out.push_str("# ---------- 通知与接口 ----------\n");
        out.push_str(&format!("notify = {}\n", bool_str(self.notify)));
        out.push_str(&format!("state_dir = \"{}\"\n", self.state_dir.display()));
        out.push_str(&format!("ctl_socket = \"{}\"\n", self.ctl_socket.display()));
        out.push_str("# 允许通过控制 socket 操作的用户；* 表示所有本地用户\n");
        out.push_str(&format!(
            "ctl_allowed_users = {}\n\n",
            self.ctl_allowed_users.join(", ")
        ));

        out.push_str("# ---------- fstab 持久化 ----------\n");
        out.push_str(&format!("fstab_path = \"{}\"\n", self.fstab_path.display()));
        out.push_str(&format!(
            "persist_enabled = {}\n\n",
            bool_str(self.persist_enabled)
        ));

        out.push_str("# ---------- 日志 ----------\n");
        out.push_str(&format!("log_level = \"{}\"\n", self.log_level));
        out.push_str(&format!(
            "log_file = \"{}\"\n",
            self.log_file.as_deref().unwrap_or("")
        ));
        out.push_str("# 只打印将要执行的命令，不真正挂载（调试用）\n");
        out.push_str(&format!("dry_run = {}\n\n", bool_str(self.dry_run)));

        out.push_str("# ---------- 各文件系统挂载选项 ----------\n");
        out.push_str("# 可用占位符: {uid} {gid} {user} {label}\n");
        out.push_str("[mount_options]\n");
        for (fstype, options) in &self.mount_options {
            out.push_str(&format!("{fstype} = {}\n", options.join(", ")));
        }
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = self.to_conf();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|err| Error::config(format!("无法创建目录 {}: {err}", parent.display())))?;
        let tmp = parent.join(format!(".automountd.conf.{}.tmp", std::process::id()));
        {
            let mut file = fs::File::create(&tmp)
                .map_err(|err| Error::config(format!("无法写入 {}: {err}", tmp.display())))?;
            file.write_all(text.as_bytes())
                .map_err(|err| Error::config(format!("写入 {} 失败: {err}", tmp.display())))?;
            file.flush().ok();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644));
        }
        fs::rename(&tmp, path).map_err(|err| {
            let _ = fs::remove_file(&tmp);
            Error::config(format!("替换 {} 失败: {err}", path.display()))
        })?;
        Ok(())
    }

    pub fn default_config_path() -> PathBuf {
        PathBuf::from(SYSTEM_CONFIG)
    }
}

fn bool_str(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn check_absolute(problems: &mut Vec<String>, name: &str, path: &Path) {
    // 同时接受 Windows 路径，便于在开发机上跑单元测试；Linux 上以 `/` 开头即可
    let looks_absolute = path.is_absolute() || path.to_string_lossy().starts_with('/');
    if !looks_absolute {
        problems.push(format!("{name} 必须是绝对路径，当前为 {}", path.display()));
    }
}

fn strip_uuid_prefix(value: &str) -> &str {
    let trimmed = value.trim();
    match trimmed.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("UUID=") => trimmed[5..].trim(),
        _ => trimmed,
    }
}

fn normalize_path_text(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut normalized = String::new();
    for part in text.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        normalized.push('/');
        normalized.push_str(part);
    }
    if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

/// 去掉行内注释，尊重单双引号与转义。
fn strip_comment(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            output.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => {
                output.push(ch);
                escaped = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                output.push(ch);
            }
            '\'' if !in_double => {
                in_single = !in_single;
                output.push(ch);
            }
            '#' if !in_single && !in_double => break,
            _ => output.push(ch),
        }
    }
    output
}

fn parse_string(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() < 2 {
        return trimmed.to_string();
    }
    let first = trimmed.chars().next().unwrap_or(' ');
    let last = trimmed.chars().last().unwrap_or(' ');
    if !((first == '"' && last == '"') || (first == '\'' && last == '\'')) {
        return trimmed.to_string();
    }
    let inner: String = trimmed
        .chars()
        .skip(1)
        .take(trimmed.chars().count() - 2)
        .collect();
    let mut output = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            output.push(match ch {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            output.push(ch);
        }
    }
    output
}

fn parse_string_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| parse_string(item.trim()))
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn parse_bool(value: &str, key: &str, line: usize) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(Error::config(format!(
            "第 {line} 行：{key} 需要布尔值（true/false），当前为 {other}"
        ))),
    }
}

fn parse_number<T>(value: &str, key: &str, line: usize) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    value.trim().parse::<T>().map_err(|err| {
        Error::config(format!(
            "第 {line} 行：{key} 数值无效（{err}），当前为 {value}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_values() {
        let config = Config::parse(
            r#"
mount_root = "/mnt/disks"
per_user_dirs = false
retry_attempts = 5
allowed_uuids = AAA-111, BBB-222
default_mount_options = nosuid,nodev
"#,
        )
        .unwrap();
        assert_eq!(config.mount_root, PathBuf::from("/mnt/disks"));
        assert!(!config.per_user_dirs);
        assert_eq!(config.retry_attempts, 5);
        assert_eq!(config.allowed_uuids, vec!["AAA-111", "BBB-222"]);
        assert_eq!(config.default_mount_options, vec!["nosuid", "nodev"]);
    }

    #[test]
    fn parses_mount_option_section() {
        let config = Config::parse(
            r#"
[mount_options]
vfat = uid={uid}, gid={gid}, umask=0022
exfat = uid={uid}
"#,
        )
        .unwrap();
        assert_eq!(
            config.mount_options.get("vfat").unwrap(),
            &vec!["uid={uid}", "gid={gid}", "umask=0022"]
        );
        assert_eq!(
            config.mount_options.get("exfat").unwrap(),
            &vec!["uid={uid}"]
        );
        // 默认值仍然保留
        assert!(config.mount_options.contains_key("ntfs3"));
    }

    #[test]
    fn comments_respect_quotes() {
        let config = Config::parse(
            r#"
mount_root = "/media#1"  # 行内注释
blocked_labels = "备份 # 盘", 旧盘
"#,
        )
        .unwrap();
        assert_eq!(config.mount_root, PathBuf::from("/media#1"));
        assert_eq!(config.blocked_labels, vec!["备份 # 盘", "旧盘"]);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("retry_attempts = abc").is_err());
        assert!(Config::parse("per_user_dirs = maybe").is_err());
        assert!(Config::parse("mount_root = relative/path").is_err());
        assert!(Config::parse("mount_root = /").is_err());
        assert!(Config::parse("mount_root = /etc/mounts").is_err());
    }

    #[test]
    fn roundtrips_through_template() {
        let config = Config::default();
        let text = config.to_conf();
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(config.mount_root, parsed.mount_root);
        assert_eq!(config.mount_options, parsed.mount_options);
        assert_eq!(config.auto_mount_on_boot, parsed.auto_mount_on_boot);
        assert_eq!(config.retry_attempts, parsed.retry_attempts);
    }

    #[test]
    fn uuid_allowlist() {
        let mut config = Config::default();
        config.allowed_uuids = vec!["uuid=abcd-1234".to_string()];
        assert!(config.is_uuid_allowed(Some("ABCD-1234")));
        assert!(!config.is_uuid_allowed(Some("FFFF-0000")));
        assert!(!config.is_uuid_allowed(None));
    }
}
