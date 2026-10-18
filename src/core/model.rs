//! Data structures shared across modules.

use std::path::PathBuf;

/// Device types that are never treated as mountable data disks.
pub const NON_DATA_TYPES: &[&str] = &[
    "loop", "ram", "zram", "crypt", "lvm", "raid0", "raid1", "raid4", "raid5", "raid6", "raid10",
    "mpath", "nbd", "rbd", "dm",
];

/// A block device (a whole disk or a partition).
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

    /// Removable media, or attached via USB / card reader / IEEE1394.
    pub fn is_external(&self) -> bool {
        self.removable
            || self.hotplug
            || matches!(
                self.transport
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
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

/// One mount record from /proc/self/mountinfo.
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

/// Outcome of the automatic mount policy decision.
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

/// Human-readable explanation of a reason code.
pub fn explain(code: &str) -> &'static str {
    match code {
        "ok" => "can be auto-mounted",
        "no-filesystem" => {
            "no filesystem detected (unformatted, encrypted, or a bare partition table)"
        }
        "ignored-fstype" => "this filesystem type is ignored by configuration",
        "system-disk" => "on the system disk; not auto-mounted for safety",
        "internal-disk" => "internal disk (auto_mount_internal_disks is disabled)",
        "not-allowed" => "not in the allowed_uuids allowlist",
        "blocked" => "excluded by blocked_uuids / blocked_labels",
        "not-a-data-device" => "not a mountable data device (loop/raid/lvm, etc.)",
        "too-small" => "capacity too small; ignored",
        "already-mounted" => "already mounted",
        "missing-devnode" => "device node does not exist",
        "unsupported" => "the kernel does not support this filesystem",
        _ => "unknown reason",
    }
}

/// A desktop login user.
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

/// A mount performed by this daemon that has to be tracked.
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

/// One row of the device list (device + mount state + policy verdict).
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

/// A mount plan (not executed yet).
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

/// Result of a mount attempt.
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
