//! Device list snapshot: combine devices, mount state and policy verdicts into the view UI/CLI needs.

use std::collections::HashSet;

use crate::config::Config;
use crate::devices;
use crate::error::Result;
use crate::exec::CommandRunner;
use crate::model::{BlockDevice, Decision, DeviceView, MountEntry, MountRecord, NON_DATA_TYPES};
use crate::mountinfo;
use crate::policy;
use crate::state::State;

/// Build the device view; when `include_all` is true, unmountable devices are kept too (for `list --all`).
pub fn snapshot(
    config: &Config,
    runner: &dyn CommandRunner,
    state: &State,
    include_all: bool,
) -> Result<Vec<DeviceView>> {
    let devices = devices::list_devices(runner, false, true);
    let mounts = mountinfo::read_mountinfo();
    let system_disks = devices::system_disk_names(&devices, &mounts);
    Ok(build_views(
        config,
        &devices,
        &mounts,
        &system_disks,
        state,
        include_all,
    ))
}

/// Pure-logic half: touches no `/proc`, `/sys` or external commands, so it is easy to unit test.
fn build_views(
    config: &Config,
    devices: &[BlockDevice],
    mounts: &[MountEntry],
    system_disks: &HashSet<String>,
    state: &State,
    include_all: bool,
) -> Vec<DeviceView> {
    let managed: Vec<MountRecord> = state.records().cloned().collect();
    let mut views: Vec<DeviceView> = Vec::with_capacity(devices.len());

    for device in devices {
        let mut mountpoints: Vec<String> = Vec::new();
        for entry in mountinfo::find_by_source(&device.path, mounts) {
            push_unique(&mut mountpoints, entry.target.clone());
        }
        if let Some(mountpoint) = &device.mountpoint {
            if !mountpoint.is_empty() {
                push_unique(&mut mountpoints, mountpoint.clone());
            }
        }
        let mounted = !mountpoints.is_empty();
        let decision = if mounted {
            Decision::deny("already-mounted")
        } else {
            policy::should_automount(device, config, system_disks, mounts)
        };
        let view = DeviceView {
            device: device.clone(),
            mountpoints,
            decision,
            managed: lookup_managed(&managed, device).cloned(),
        };
        if include_all || visible(&view) {
            views.push(view);
        }
    }

    views.sort_by(|left, right| natural_cmp(&left.device.name, &right.device.name));
    views
}

fn push_unique(target: &mut Vec<String>, value: String) {
    if !target.iter().any(|existing| existing == &value) {
        target.push(value);
    }
}

/// Without `--all`, hide devices with nothing to show; mounted devices are always kept.
fn visible(view: &DeviceView) -> bool {
    if view.mounted() {
        return true;
    }
    let device = &view.device;
    if NON_DATA_TYPES.contains(&device.dev_type.as_str()) {
        return false;
    }
    if !device.has_filesystem() {
        return false;
    }
    view.decision.code != "no-filesystem"
}

/// Match on the full path first, then fall back to the device name (basename).
fn lookup_managed<'a>(records: &'a [MountRecord], device: &BlockDevice) -> Option<&'a MountRecord> {
    records
        .iter()
        .find(|record| record.device == device.path)
        .or_else(|| {
            records
                .iter()
                .find(|record| basename(&record.device) == device.name)
        })
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Natural ordering: `sda2` sorts before `sda10`.
fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_parts = segments(left);
    let right_parts = segments(right);
    for (a, b) in left_parts.iter().zip(right_parts.iter()) {
        let ordering = match (a.0, b.0) {
            (true, true) => compare_number(a.1, b.1),
            _ => a.1.cmp(b.1),
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    left_parts.len().cmp(&right_parts.len())
}

/// Split into (is_numeric, segment) pieces.
fn segments(text: &str) -> Vec<(bool, &str)> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut digit = bytes.first().is_some_and(u8::is_ascii_digit);
    for (index, byte) in bytes.iter().enumerate() {
        let is_digit = byte.is_ascii_digit();
        if is_digit != digit {
            // A digit/non-digit boundary is always a valid UTF-8 boundary (an ASCII digit
            // occupies a single byte).
            parts.push((digit, &text[start..index]));
            start = index;
            digit = is_digit;
        }
    }
    if start < text.len() {
        parts.push((digit, &text[start..]));
    }
    parts
}

fn compare_number(left: &str, right: &str) -> std::cmp::Ordering {
    let left_digits = left.trim_start_matches('0');
    let right_digits = right.trim_start_matches('0');
    left_digits
        .len()
        .cmp(&right_digits.len())
        .then_with(|| left_digits.cmp(right_digits))
        // 12 vs 012: same numeric value, so compare the raw text to keep the order stable
        .then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::model::MountRecord;
    use crate::testutil::TempDir;

    fn device(name: &str, dev_type: &str, fstype: Option<&str>) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            path: format!("/dev/{name}"),
            dev_type: dev_type.to_string(),
            fstype: fstype.map(str::to_string),
            label: Some("USB Disk".to_string()),
            uuid: Some("1234-ABCD".to_string()),
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

    fn state_in(temp: &TempDir, records: Vec<MountRecord>) -> State {
        let mut map = BTreeMap::new();
        for record in records {
            map.insert(record.device.clone(), record);
        }
        State {
            path: temp.join("state.tsv"),
            records: map,
            pid: 4242,
            started_at: 1_700_000_000,
            dry_run: false,
            backend: "udev".to_string(),
        }
    }

    fn record(device: &str, target: &str) -> MountRecord {
        MountRecord {
            device: device.to_string(),
            target: target.to_string(),
            fstype: "exfat".to_string(),
            driver: "exfat".to_string(),
            uuid: Some("1234-ABCD".to_string()),
            label: Some("USB Disk".to_string()),
            owner_uid: Some(1000),
            reason: "hotplug".to_string(),
            mounted_at: 1_700_000_100,
            options: vec!["nosuid".to_string(), "nodev".to_string()],
        }
    }

    fn mount_entry(source: &str, target: &str) -> MountEntry {
        MountEntry {
            mount_id: 36,
            parent_id: 21,
            major_minor: "8:17".to_string(),
            root: "/".to_string(),
            target: target.to_string(),
            options: vec!["rw".to_string()],
            fstype: "exfat".to_string(),
            source: source.to_string(),
            super_options: Vec::new(),
        }
    }

    #[test]
    fn snapshot_reads_devices_from_lsblk() {
        use crate::exec::{CommandOutput, MockRunner};

        // One USB stick: one exfat partition plus one unformatted partition
        let json = r#"{"blockdevices":[{"name":"sdz","type":"disk","rm":true,
            "hotplug":true,"tran":"usb","size":32000000000,"children":[
            {"name":"sdz1","type":"part","fstype":"exfat","label":"USB Disk",
             "uuid":"1234-ABCD","rm":true,"hotplug":true,"tran":"usb","size":31000000000},
            {"name":"sdz2","type":"part","fstype":null,"size":0}]}]}"#;
        let runner = MockRunner::new();
        runner.push("lsblk", CommandOutput::success(json.to_string()));
        let temp = TempDir::new("inventory-snapshot");
        let state = state_in(&temp, Vec::new());

        let views = snapshot(&Config::default(), &runner, &state, false).expect("snapshot failed");
        assert!(runner.called("lsblk"));
        let names: Vec<&str> = views.iter().map(|view| view.device.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["sdz1"],
            "unformatted partitions should be hidden from the normal list"
        );
        assert_eq!(views[0].device.path, "/dev/sdz1");
        assert_eq!(views[0].device.parent.as_deref(), Some("sdz"));
        assert_eq!(views[0].device.label.as_deref(), Some("USB Disk"));
        assert!(views[0].decision.allowed, "{}", views[0].decision.message);

        runner.push("lsblk", CommandOutput::success(json.to_string()));
        let all = snapshot(&Config::default(), &runner, &state, true).expect("snapshot failed");
        assert_eq!(
            all.len(),
            3,
            " --all should keep whole disks and unformatted partitions"
        );
        let all_names: Vec<&str> = all.iter().map(|view| view.device.name.as_str()).collect();
        assert_eq!(all_names, vec!["sdz", "sdz1", "sdz2"]);
    }

    #[test]
    fn marks_already_mounted_devices() {
        let temp = TempDir::new("inventory-mounted");
        let state = state_in(&temp, vec![record("/dev/sdb1", "/media/u/USB-Disk")]);
        let devices = vec![device("sdb1", "part", Some("exfat"))];
        let mounts = vec![mount_entry("/dev/sdb1", "/media/u/USB-Disk")];
        let views = build_views(
            &Config::default(),
            &devices,
            &mounts,
            &HashSet::new(),
            &state,
            false,
        );
        assert_eq!(views.len(), 1);
        assert!(views[0].mounted());
        assert_eq!(views[0].target(), Some("/media/u/USB-Disk"));
        assert_eq!(views[0].decision.code, "already-mounted");
        assert!(!views[0].decision.allowed);
        // The managed record comes from the state file
        let managed = views[0]
            .managed
            .as_ref()
            .expect("expected a managed record");
        assert_eq!(managed.target, "/media/u/USB-Disk");
        assert_eq!(managed.owner_uid, Some(1000));
    }

    #[test]
    fn hides_undecorated_devices_unless_include_all() {
        let temp = TempDir::new("inventory-filter");
        let state = state_in(&temp, Vec::new());
        let mut loop_device = device("loop0", "loop", Some("squashfs"));
        loop_device.path = "/dev/loop0".to_string();
        let mut bare = device("sdb2", "part", None);
        bare.size = 0;
        let devices = vec![loop_device, bare, device("sdb1", "part", Some("exfat"))];

        let filtered = build_views(
            &Config::default(),
            &devices,
            &[],
            &HashSet::new(),
            &state,
            false,
        );
        let names: Vec<&str> = filtered.iter().map(|v| v.device.name.as_str()).collect();
        assert_eq!(names, vec!["sdb1"]);
        assert!(
            filtered[0].decision.allowed,
            "{}",
            filtered[0].decision.message
        );

        let all = build_views(
            &Config::default(),
            &devices,
            &[],
            &HashSet::new(),
            &state,
            true,
        );
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].device.name, "loop0");
        let bare_view = all
            .iter()
            .find(|view| view.device.name == "sdb2")
            .expect("sdb2 should appear in the --all results");
        assert_eq!(bare_view.decision.code, "no-filesystem");
        assert_eq!(
            all.iter()
                .find(|view| view.device.name == "sdb1")
                .unwrap()
                .decision
                .code,
            "ok"
        );
    }

    #[test]
    fn keeps_policy_denied_but_formatted_devices() {
        let temp = TempDir::new("inventory-policy");
        let state = state_in(&temp, Vec::new());
        let mut internal = device("sda3", "part", Some("ext4"));
        internal.removable = false;
        internal.hotplug = false;
        internal.transport = Some("sata".to_string());
        internal.parent = Some("sda".to_string());
        let views = build_views(
            &Config::default(),
            &[internal],
            &[],
            &HashSet::new(),
            &state,
            false,
        );
        assert_eq!(
            views.len(),
            1,
            "internal disks should appear in the normal list with a reason"
        );
        assert_eq!(views[0].decision.code, "internal-disk");
        assert!(!views[0].decision.allowed);
        assert!(views[0].managed.is_none());
    }

    #[test]
    fn sorts_names_naturally() {
        let temp = TempDir::new("inventory-sort");
        let state = state_in(&temp, Vec::new());
        let devices = ["sda10", "sda2", "sdb1", "sda1"]
            .iter()
            .map(|name| device(name, "part", Some("ext4")))
            .collect::<Vec<_>>();
        let views = build_views(
            &Config::default(),
            &devices,
            &[],
            &HashSet::new(),
            &state,
            true,
        );
        let names: Vec<&str> = views.iter().map(|v| v.device.name.as_str()).collect();
        assert_eq!(names, vec!["sda1", "sda2", "sda10", "sdb1"]);

        assert_eq!(
            natural_cmp("nvme0n1p10", "nvme0n1p2"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(natural_cmp("sda1", "sda1"), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("sdz", "sdz1"), std::cmp::Ordering::Less);
        assert_eq!(
            natural_cmp("sda01", "sda1"),
            std::cmp::Ordering::Less,
            "same numeric value compares the raw text (0 < 1) to keep the sort stable"
        );
    }

    #[test]
    fn matches_managed_record_by_basename() {
        let temp = TempDir::new("inventory-managed");
        // The record only stores the device name (for example from an older state file)
        let state = state_in(&temp, vec![record("sdb1", "/media/u/USB-Disk")]);
        let views = build_views(
            &Config::default(),
            &[device("sdb1", "part", Some("exfat"))],
            &[],
            &HashSet::new(),
            &state,
            false,
        );
        assert_eq!(views.len(), 1);
        assert_eq!(
            views[0].managed.as_ref().map(|r| r.target.as_str()),
            Some("/media/u/USB-Disk")
        );
        // An exact path match wins over basename
        let temp2 = TempDir::new("inventory-managed2");
        let mut other = record("/dev/sdc1", "/media/u/other");
        other.device = "/dev/sdc1".to_string();
        let state2 = state_in(&temp2, vec![record("sdc1", "/media/old"), other]);
        let views2 = build_views(
            &Config::default(),
            &[device("sdc1", "part", Some("exfat"))],
            &[],
            &HashSet::new(),
            &state2,
            false,
        );
        assert_eq!(
            views2[0].managed.as_ref().map(|r| r.target.as_str()),
            Some("/media/u/other")
        );
    }

    #[test]
    fn duplicates_mountpoints_are_removed() {
        let temp = TempDir::new("inventory-dedupe");
        let state = state_in(&temp, Vec::new());
        let mut dev = device("sdb1", "part", Some("exfat"));
        dev.mountpoint = Some("/media/u/USB-Disk".to_string());
        // The same device is bind-mounted twice
        let mounts = vec![
            mount_entry("/dev/sdb1", "/media/u/USB-Disk"),
            mount_entry("/dev/sdb1", "/media/u/USB-Disk"),
            mount_entry("/dev/sdb1", "/mnt/extra"),
        ];
        let views = build_views(
            &Config::default(),
            &[dev],
            &mounts,
            &HashSet::new(),
            &state,
            false,
        );
        assert_eq!(
            views[0].mountpoints,
            vec!["/media/u/USB-Disk".to_string(), "/mnt/extra".to_string()]
        );
    }
}
