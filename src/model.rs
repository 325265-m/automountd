//! 跨模块共享的数据结构。

use std::path::PathBuf;

/// 永远不当作可挂载数据盘的设备类型。
pub const NON_DATA_TYPES: &[&str] = &[
    "loop",
    "ram",
    "zram",
    "crypt",
    "lvm",
    "raid0",
    "raid1",
    "raid4",
    "raid5",
    "raid6",
    "raid10",
    "mpath",
    "nbd",
    "rbd",
    "dm",
];

/// 一个块设备（整盘或分区）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDevice {
    pub name: String,
    pub path: String,
    pub dev_type: String,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub partuuid: Option<String>,
    pub size: u64,
    pub removable: bool,
    pub read_only: bool,
    pub hotplug: bool,
    pub transport: Option<String>,
    pub mountpoint: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub parent: Option<String>,
    pub children: Vec<String>,
}

impl BlockDevice {
    pub fn is_partition(&self) -> bool {
        self.dev_type == "part"
    }

    /// 可移动介质，或通过 USB / 读卡器 / IEEE1394 接入。
    pub fn is_external(&self) -> bool {
        self.removable
            || self.hotplug
            || matches!(
                self.transport.as_deref().map(str::to_ascii_lowercase).as_deref(),
                Some("usb") | Some("mmc") | Some("ieee1394") | Some("firewire") | Some("sdio")
            )
    }

    pub fn has_filesystem(&self) -> bool {
        match self.fstype.as_deref() {
            Some(fstype) if !fstype.is_empty() => fstype != "swap",
            _ => false,
        }
    }

    pub fn human_size(&self) -> String {
        crate::term::format_size(self.size)
    }

    pub fn title(&self) -> &str {
        self.label
            .as_deref()
            .or(self.uuid.as_deref())
            .unwrap_or(&self.name)
    }

    pub fn devnode(&self) -> &str {
        &self.path
    }
}

/// /proc/self/mountinfo 中的一条挂载记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_id: u32,
    pub parent_id: u32,
    pub major_minor: String,
    pub root: String,
    pub target: String,
    pub options: Vec<String>,
    pub fstype: String,
    pub source: String,
    pub super_options: Vec<String>,
}

impl MountEntry {
    pub fn read_only(&self) -> bool {
        self.options.iter().any(|o| o == "ro")
    }
}

/// 自动挂载策略的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub code: String,
    pub message: String,
}

impl Decision {
    pub fn allow(code: impl Into<String>) -> Self {
        let code = code.into();
        let message = explain(&code).to_string();
        Self {
            allowed: true,
            code,
            message,
        }
    }

    pub fn deny(code: impl Into<String>) -> Self {
        let code = code.into();
        let message = explain(&code).to_string();
        Self {
            allowed: false,
            code,
            message,
        }
    }

    pub fn deny_with(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            allowed: false,
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 中文原因说明。
pub fn explain(code: &str) -> &'static str {
    match code {
        "ok" => "可自动挂载",
        "no-filesystem" => "未检测到文件系统（未格式化、已加密或只是分区表）",
        "ignored-fstype" => "该文件系统类型被配置忽略",
        "system-disk" => "属于系统盘，出于安全考虑不自动挂载",
        "internal-disk" => "内置磁盘（未开启 auto_mount_internal_disks）",
        "not-allowed" => "不在 allowed_uuids 白名单内",
        "blocked" => "被 blocked_uuids / blocked_labels 排除",
        "not-a-data-device" => "不是可挂载的数据设备（loop/raid/lvm 等）",
        "too-small" => "容量过小，忽略",
        "already-mounted" => "已经处于挂载状态",
        "missing-devnode" => "设备节点不存在",
        "unsupported" => "内核不支持该文件系统",
        _ => "未知原因",
    }
}

/// 桌面登录用户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: Option<String>,
}

impl UserInfo {
    pub fn display(&self) -> String {
        format!("{}({})", self.name, self.uid)
    }
}

/// 由本守护进程挂载、需要跟踪的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRecord {
    pub device: String,
    pub target: String,
    pub fstype: String,
    pub driver: String,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub owner_uid: Option<u32>,
    pub reason: String,
    pub mounted_at: u64,
    pub options: Vec<String>,
}

/// 设备列表中的一行（设备 + 挂载状态 + 策略结论）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub device: BlockDevice,
    pub mountpoints: Vec<String>,
    pub decision: Decision,
    pub managed: Option<MountRecord>,
}

impl DeviceView {
    pub fn mounted(&self) -> bool {
        !self.mountpoints.is_empty()
    }

    pub fn target(&self) -> Option<&str> {
        self.mountpoints.first().map(String::as_str)
    }
}

/// 挂载计划（尚未执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPlan {
    pub device: BlockDevice,
    pub target: PathBuf,
    pub fstype: String,
    pub driver: String,
    pub options: Vec<String>,
}

impl MountPlan {
    pub fn options_string(&self) -> String {
        self.options.join(",")
    }

    pub fn argv(&self) -> Vec<String> {
        vec![
            "mount".to_string(),
            "-t".to_string(),
            self.driver.clone(),
            "-o".to_string(),
            self.options_string(),
            self.device.path.clone(),
            self.target.display().to_string(),
        ]
    }

    pub fn describe(&self) -> String {
        self.argv().join(" ")
    }
}

/// 挂载执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountResult {
    pub device: BlockDevice,
    pub target: PathBuf,
    pub fstype: String,
    pub driver: String,
    pub options: Vec<String>,
    pub already_mounted: bool,
    pub dry_run: bool,
}
