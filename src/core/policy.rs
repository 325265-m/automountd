//! Automatic mount policy: decides whether a device should be mounted.

use std::collections::HashSet;

use crate::config::Config;
use crate::model::{BlockDevice, Decision, MountEntry, NON_DATA_TYPES};

/// Devices smaller than this are not mounted automatically (avoids accidentally mounting EFI or
/// boot partition leftovers).
pub const MIN_SIZE_BYTES: u64 = 4 * 1024 * 1024;

fn on_system_disk(device: &BlockDevice, system_disks: &HashSet<String>) -> bool {
    system_disks.contains(&device.name)
        || device
            .parent
            .as_ref()
            .is_some_and(|parent| system_disks.contains(parent))
}

fn is_blocked(device: &BlockDevice, config: &Config) -> Option<Decision> {
    let blocked_uuids: HashSet<String> = config
        .blocked_uuids
        .iter()
        .map(|uuid| uuid.trim().trim_start_matches("UUID=").to_ascii_uppercase())
        .collect();
    if let Some(uuid) = &device.uuid {
        if blocked_uuids.contains(&uuid.to_ascii_uppercase()) {
            return Some(Decision::deny_with(
                "blocked",
                format!("UUID {uuid} is in the exclusion list"),
            ));
        }
    }
    if let Some(label) = &device.label {
        if config
            .blocked_labels
            .iter()
            .any(|blocked| blocked.eq_ignore_ascii_case(label))
        {
            return Some(Decision::deny_with(
                "blocked",
                format!("label {label} is in the exclusion list"),
            ));
        }
    }
    None
}

fn ignored_fstype(device: &BlockDevice, config: &Config) -> bool {
    match device.fstype.as_deref() {
        Some(fstype) => config
            .ignored_fstypes
            .iter()
            .any(|ignored| ignored.eq_ignore_ascii_case(fstype)),
        None => false,
    }
}

/// Automatic mount decision for hotplug / boot.
pub fn should_automount(
    device: &BlockDevice,
    config: &Config,
    system_disks: &HashSet<String>,
    mounts: &[MountEntry],
) -> Decision {
    if NON_DATA_TYPES.contains(&device.dev_type.as_str()) {
        return Decision::deny_with(
            "not-a-data-device",
            format!(
                "device type {} does not support auto-mounting",
                device.dev_type
            ),
        );
    }
    if !device.path.starts_with("/dev/") {
        return Decision::deny("missing-devnode");
    }
    if !device.has_filesystem() {
        return Decision::deny("no-filesystem");
    }
    if ignored_fstype(device, config) {
        return Decision::deny_with(
            "ignored-fstype",
            format!(
                "filesystem {} is in ignored_fstypes",
                device.fstype.as_deref().unwrap_or("?")
            ),
        );
    }
    if device.size > 0 && device.size < MIN_SIZE_BYTES {
        return Decision::deny_with(
            "too-small",
            format!("device capacity is only {} bytes", device.size),
        );
    }
    if let Some(decision) = is_blocked(device, config) {
        return decision;
    }

    let allowed_by_uuid =
        !config.allowed_uuids.is_empty() && config.is_uuid_allowed(device.uuid.as_deref());

    if on_system_disk(device, system_disks) && !allowed_by_uuid {
        return Decision::deny("system-disk");
    }
    if !config.allowed_uuids.is_empty() && !allowed_by_uuid {
        return Decision::deny("not-allowed");
    }
    if !allowed_by_uuid && !config.auto_mount_internal_disks && !device.is_external() {
        return Decision::deny_with(
            "internal-disk",
            "internal disk auto-mount is disabled (set auto_mount_internal_disks = true or list it in allowed_uuids)",
        );
    }
    if !crate::mountinfo::find_by_source(&device.path, mounts).is_empty() {
        return Decision::deny("already-mounted");
    }
    Decision::allow("ok")
}

/// Safety checks for a manual mount (command line / interactive interface).
pub fn should_mount_manual(
    device: &BlockDevice,
    config: &Config,
    system_disks: &HashSet<String>,
    force: bool,
) -> Decision {
    let data_type = !NON_DATA_TYPES.contains(&device.dev_type.as_str()) || device.dev_type == "lvm";
    if !data_type && !force {
        return Decision::deny_with(
            "not-a-data-device",
            format!("device type {} does not support mounting", device.dev_type),
        );
    }
    if !device.has_filesystem() {
        return Decision::deny("no-filesystem");
    }
    if ignored_fstype(device, config) && !force {
        return Decision::deny("ignored-fstype");
    }
    if on_system_disk(device, system_disks) && !force {
        return Decision::deny_with(
            "system-disk",
            "this device holds system directories; use --force to mount it anyway",
        );
    }
    Decision::allow("ok")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MountEntry;

    fn device(name: &str, fstype: &str) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            path: format!("/dev/{name}"),
            dev_type: "part".to_string(),
            fstype: Some(fstype.to_string()),
            label: Some("USB".to_string()),
            uuid: Some("ABCD-1234".to_string()),
            partuuid: None,
            size: 32 * 1024 * 1024 * 1024,
            removable: true,
            read_only: false,
            hotplug: true,
            transport: Some("usb".to_string()),
            mountpoint: None,
            model: None,
            serial: None,
            parent: Some("sdb".to_string()),
            children: Vec::new(),
        }
    }

    fn internal(name: &str) -> BlockDevice {
        let mut dev = device(name, "ext4");
        dev.removable = false;
        dev.hotplug = false;
        dev.transport = Some("sata".to_string());
        dev.parent = Some("sda".to_string());
        dev
    }

    #[test]
    fn mounts_removable_usb() {
        let config = Config::default();
        let decision = should_automount(&device("sdb1", "exfat"), &config, &HashSet::new(), &[]);
        assert!(decision.allowed, "{}", decision.message);
    }

    #[test]
    fn refuses_internal_by_default() {
        let config = Config::default();
        let decision = should_automount(&internal("sda3"), &config, &HashSet::new(), &[]);
        assert!(!decision.allowed);
        assert_eq!(decision.code, "internal-disk");
    }

    #[test]
    fn allows_internal_when_configured() {
        let mut config = Config::default();
        config.auto_mount_internal_disks = true;
        assert!(should_automount(&internal("sda3"), &config, &HashSet::new(), &[]).allowed);
    }

    #[test]
    fn refuses_system_disk_even_if_internal_enabled() {
        let mut config = Config::default();
        config.auto_mount_internal_disks = true;
        let mut disks = HashSet::new();
        disks.insert("sda".to_string());
        let decision = should_automount(&internal("sda3"), &config, &disks, &[]);
        assert!(!decision.allowed);
        assert_eq!(decision.code, "system-disk");
    }

    #[test]
    fn allowlist_forces_system_disk() {
        let mut config = Config::default();
        config.allowed_uuids = vec!["ABCD-1234".to_string()];
        let mut disks = HashSet::new();
        disks.insert("sda".to_string());
        assert!(should_automount(&internal("sda3"), &config, &disks, &[]).allowed);
    }

    #[test]
    fn skips_unformatted_and_encrypted() {
        let config = Config::default();
        let mut dev = device("sdb1", "crypto_LUKS");
        assert!(!should_automount(&dev, &config, &HashSet::new(), &[]).allowed);
        dev.fstype = None;
        assert_eq!(
            should_automount(&dev, &config, &HashSet::new(), &[]).code,
            "no-filesystem"
        );
    }

    #[test]
    fn skips_already_mounted() {
        let config = Config::default();
        let entry = MountEntry {
            mount_id: 1,
            parent_id: 0,
            major_minor: "8:17".to_string(),
            root: "/".to_string(),
            target: "/media/USB".to_string(),
            options: vec!["rw".to_string()],
            fstype: "exfat".to_string(),
            source: "/dev/sdb1".to_string(),
            super_options: Vec::new(),
        };
        let decision =
            should_automount(&device("sdb1", "exfat"), &config, &HashSet::new(), &[entry]);
        assert_eq!(decision.code, "already-mounted");
    }

    #[test]
    fn manual_mount_can_force_system_disk() {
        let config = Config::default();
        let mut disks = HashSet::new();
        disks.insert("sda".to_string());
        assert!(!should_mount_manual(&internal("sda3"), &config, &disks, false).allowed);
        assert!(should_mount_manual(&internal("sda3"), &config, &disks, true).allowed);
    }
}
