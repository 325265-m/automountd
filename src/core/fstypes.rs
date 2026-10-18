//! Filesystem driver selection and mount option assembly.
//!
//! This module is pure logic: it runs no external commands and touches no system state, so it can
//! be unit tested on any platform. The only entry point that interacts with the system is
//! [`read_proc_filesystems`] (which reads `/proc/filesystems`); tests use
//! [`read_proc_filesystems_from`] to point at a temporary file instead.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::logging;
use crate::model::UserInfo;

/// Filesystems that have no permission model of their own, so ownership has to be handed to the
/// desktop user via uid/gid/umask.
pub const USERSPACE_FSTYPES: &[&str] = &[
    "vfat", "fat", "msdos", "umsdos", "exfat", "ntfs", "ntfs3", "ntfs-3g", "iso9660", "udf",
    "hfsplus", "hfs",
];

/// Filesystems that have a complete permission model in the kernel and can be mounted directly with
/// `mount -t <itself>`.
const NATIVE_FSTYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "jfs", "reiserfs", "nilfs2", "bcachefs", "zfs",
    "udf", "iso9660", "squashfs", "erofs", "hfsplus", "vfat", "exfat", "ntfs3", "ocfs2", "gfs2",
];

/// Read the list of filesystems the kernel supports.
pub fn read_proc_filesystems() -> HashSet<String> {
    #[cfg(target_os = "linux")]
    {
        read_proc_filesystems_from(Path::new("/proc/filesystems"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Non-Linux platforms have no /proc, so returning an empty set is fine (callers use it to
        // skip the kernel capability check).
        logging::debug(
            "no /proc/filesystems on this platform; treating kernel capabilities as unknown",
        );
        HashSet::new()
    }
}

/// Read from a specific file (used by tests).
pub fn read_proc_filesystems_from(path: &Path) -> HashSet<String> {
    let mut supported = HashSet::new();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            logging::debug(format!(
                "cannot read {} (treating as an empty list): {err}",
                path.display()
            ));
            return supported;
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("nodev") {
            continue;
        }
        if let Some(name) = line.split_whitespace().last() {
            if !name.is_empty() {
                supported.insert(name.to_string());
            }
        }
    }
    supported
}

/// Map the type detected by blkid to the driver name passed to `mount -t`.
pub fn choose_driver(fstype: &str, supported: &HashSet<String>) -> Result<String> {
    let raw = fstype.trim();
    if raw.is_empty() {
        return Err(
            Error::device("filesystem type is empty, cannot choose a mount driver")
                .with_hint("run 'blkid <device>' first to check whether the device is formatted"),
        );
    }
    let lower = raw.to_ascii_lowercase();
    let has = |name: &str| supported.is_empty() || supported.contains(name);

    match lower.as_str() {
        // NTFS: prefer the in-kernel ntfs3 driver and fall back to the userspace ntfs-3g when missing.
        "ntfs" | "ntfs3" | "ntfs-3g" => {
            if has("ntfs3") {
                Ok("ntfs3".to_string())
            } else {
                Ok("ntfs-3g".to_string())
            }
        }
        // The whole FAT family is handled by the vfat driver.
        "fat" | "fat12" | "fat16" | "fat32" | "msdos" | "umsdos" | "vfat" => Ok("vfat".to_string()),
        // Legacy HFS is also read through the hfsplus driver.
        "hfs" | "hfsplus" => Ok("hfsplus".to_string()),
        "exfat" if !has("exfat") => Err(Error::unsupported(
            "the kernel does not support the exfat filesystem (no exfat driver found)",
        )
        .with_hint(suggest_packages("exfat").join(" "))),
        "exfat" => Ok("exfat".to_string()),
        _ if NATIVE_FSTYPES.contains(&lower.as_str()) => Ok(lower),
        _ => {
            if !has(&lower) {
                return Err(Error::unsupported(format!(
                    "the kernel does not support filesystem {raw} (no {lower} in the available driver list)"
                ))
                .with_hint(suggest_packages(&lower).join(" ")));
            }
            Ok(lower)
        }
    }
}

/// Whether this filesystem should always be mounted read-only.
pub fn is_read_only(fstype: &str, config: &Config) -> bool {
    let wanted = fstype.trim();
    if wanted.is_empty() {
        return false;
    }
    config
        .read_only_fstypes
        .iter()
        .any(|item| item.trim().eq_ignore_ascii_case(wanted))
}

/// Assemble the `mount -o` options.
///
/// Order: global defaults -> defaults for this filesystem -> caller extras; for options with the
/// same name the later one overrides the earlier one.
/// The `{uid}` `{gid}` `{user}` `{label}` placeholders are supported.
pub fn build_options(
    config: &Config,
    fstype: &str,
    driver: &str,
    user: Option<&UserInfo>,
    read_only: bool,
    extra: &[String],
    label: Option<&str>,
) -> Vec<String> {
    let uid = user.map(|user| user.uid).unwrap_or(0).to_string();
    let gid = user.map(|user| user.gid).unwrap_or(0).to_string();
    let user_name = user
        .map(|user| user.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let label = label.map(sanitize_option_label).unwrap_or_default();

    let mut raw: Vec<String> = Vec::new();
    raw.extend(config.default_mount_options.iter().cloned());
    raw.extend(config.mount_options_for(driver, fstype));
    raw.extend(extra.iter().cloned());

    // key -> index of the option at that position, used for "the later one overrides".
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut options: Vec<String> = Vec::new();

    for item in raw {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let expanded = item
            .replace("{uid}", &uid)
            .replace("{gid}", &gid)
            .replace("{user}", &user_name)
            .replace("{label}", &label);
        if expanded.contains('{') || expanded.contains('}') {
            logging::debug(format!(
                "ignoring mount option with unknown placeholder: {item}"
            ));
            continue;
        }
        let key = expanded.split('=').next().unwrap_or("").trim().to_string();
        if key.is_empty() {
            continue;
        }
        // ro/rw is decided by read_only, so it is always forced in at the end.
        if key == "ro" || key == "rw" {
            continue;
        }
        match seen.get(&key) {
            Some(index) => options[*index] = expanded,
            None => {
                seen.insert(key, options.len());
                options.push(expanded);
            }
        }
    }

    options.push(if read_only { "ro" } else { "rw" }.to_string());
    options
}

/// Whether the mount point has to be chowned to the desktop user before mounting.
pub fn needs_ownership_fix(fstype: &str, config: &Config) -> bool {
    let lower = fstype.trim().to_ascii_lowercase();
    if USERSPACE_FSTYPES.iter().any(|item| *item == lower) {
        return true;
    }
    config.chown_native_filesystems
}

pub fn options_string(options: &[String]) -> String {
    options.join(",")
}

/// Installation hints for a missing driver.
pub fn suggest_packages(fstype: &str) -> Vec<String> {
    let lower = fstype.trim().to_ascii_lowercase();
    let packages: &[&str] = match lower.as_str() {
        "ntfs" | "ntfs3" | "ntfs-3g" => &["ntfs-3g", "ntfsprogs"],
        "exfat" => &["exfatprogs", "exfat-fuse"],
        "hfs" | "hfsplus" => &["hfsprogs"],
        "vfat" | "fat" | "fat12" | "fat16" | "fat32" | "msdos" | "umsdos" => &["dosfstools"],
        _ => &[],
    };

    if packages.is_empty() {
        let mut hints = vec![format!(
            "make sure the kernel has the {lower} filesystem module loaded (try modprobe {lower})"
        )];
        hints.push(
            "or install the matching userspace driver (for example fuse3 with ntfs-3g/exfat-fuse)"
                .to_string(),
        );
        return hints;
    }

    vec![
        format!(
            "the kernel lacks {lower} support; install one of the following packages: {}",
            packages.join(" / ")
        ),
        format!("Debian/Ubuntu: sudo apt install {}", packages.join(" ")),
        format!("Fedora/RHEL: sudo dnf install {}", packages.join(" ")),
        format!("Arch Linux: sudo pacman -S {}", packages.join(" ")),
    ]
}

/// A label must contain no whitespace or commas before it is written into mount options (they would
/// be taken as new options by `mount -o`).
fn sanitize_option_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| {
            if ch.is_whitespace() || ch == ',' {
                '_'
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{CommandRunner, MockRunner};
    use crate::testutil::TempDir;

    fn supported(items: &[&str]) -> HashSet<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn user() -> UserInfo {
        UserInfo {
            name: "alice".to_string(),
            uid: 1000,
            gid: 1001,
            home: Some("/home/alice".to_string()),
        }
    }

    #[test]
    fn reads_proc_filesystems_lines() {
        let temp = TempDir::new("fstypes-proc");
        let path = temp.write(
            "filesystems",
            "nodev\tsysfs\nnodev\tproc\n\text4\n\tvfat\n  \nnodev\tdevtmpfs\n\tntfs3\n",
        );
        let supported = read_proc_filesystems_from(&path);
        assert!(supported.contains("ext4"));
        assert!(supported.contains("vfat"));
        assert!(supported.contains("ntfs3"));
        assert!(!supported.contains("sysfs"));
        assert!(!supported.contains("proc"));
        assert_eq!(supported.len(), 3);

        // A missing file yields an empty set instead of a panic
        let missing = read_proc_filesystems_from(&temp.join("nope"));
        assert!(missing.is_empty());
    }

    #[test]
    fn chooses_ntfs_driver() {
        // Prefer the in-kernel driver when ntfs3 is supported
        assert_eq!(
            choose_driver("ntfs", &supported(&["ntfs3", "ext4"])).unwrap(),
            "ntfs3"
        );
        // An empty `supported` set means unknown kernel capabilities, so ntfs3 is still preferred
        assert_eq!(choose_driver("ntfs", &HashSet::new()).unwrap(), "ntfs3");
        // Fall back to userspace ntfs-3g when the kernel lacks ntfs3
        assert_eq!(
            choose_driver("ntfs", &supported(&["ext4", "vfat"])).unwrap(),
            "ntfs-3g"
        );
        assert_eq!(
            choose_driver("ntfs3", &supported(&["ext4"])).unwrap(),
            "ntfs-3g"
        );
        assert_eq!(
            choose_driver("ntfs-3g", &supported(&["ext4"])).unwrap(),
            "ntfs-3g"
        );
    }

    #[test]
    fn maps_fat_and_hfs_families() {
        for fstype in ["fat", "fat16", "fat32", "msdos", "umsdos", "vfat", "FAT"] {
            assert_eq!(
                choose_driver(fstype, &supported(&["vfat"])).unwrap(),
                "vfat",
                "fstype {fstype}"
            );
        }
        assert_eq!(
            choose_driver("hfs", &supported(&["hfsplus"])).unwrap(),
            "hfsplus"
        );
        assert_eq!(
            choose_driver("HFS", &supported(&["hfsplus"])).unwrap(),
            "hfsplus"
        );
        for native in ["ext4", "xfs", "btrfs", "f2fs", "udf", "iso9660"] {
            assert_eq!(
                choose_driver(native, &supported(&[native])).unwrap(),
                native
            );
        }
        // Known native types are returned as-is even when absent from the kernel list
        assert_eq!(choose_driver("xfs", &supported(&["ext4"])).unwrap(), "xfs");
    }

    #[test]
    fn rejects_unsupported_types() {
        let err = choose_driver("exfat", &supported(&["ext4"])).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Unsupported);
        let hint = err.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("exfatprogs"),
            "hint should mention exfatprogs: {hint}"
        );
        assert!(hint.contains("exfat-fuse"));

        let err = choose_driver("customfs", &supported(&["ext4"])).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Unsupported);
        assert!(err.message.contains("customfs"));
        assert!(err.hint.is_some());

        assert_eq!(
            choose_driver("customfs", &HashSet::new()).unwrap(),
            "customfs"
        );
        assert!(choose_driver("   ", &HashSet::new()).is_err());
    }

    #[test]
    fn builds_options_with_placeholders_and_dedupe() {
        let config = Config::default();
        let options = build_options(
            &config,
            "vfat",
            "vfat",
            Some(&user()),
            true,
            &["umask=0077".to_string(), "x-label={label}".to_string()],
            Some("My Disk"),
        );

        // Default safety options -> filesystem options -> extra options, with ro forced last
        assert_eq!(
            options,
            vec![
                "nosuid",
                "nodev",
                "uid=1000",
                "gid=1001",
                "umask=0077",
                "shortname=mixed",
                "utf8",
                "x-label=My_Disk",
                "ro",
            ]
        );
        assert_eq!(
            options_string(&options),
            "nosuid,nodev,uid=1000,gid=1001,umask=0077,shortname=mixed,utf8,x-label=My_Disk,ro"
        );

        // An option with the same name overrides the earlier one and keeps its first position
        let options = build_options(
            &config,
            "vfat",
            "vfat",
            Some(&user()),
            false,
            &["uid=4242".to_string()],
            None,
        );
        assert!(options.contains(&"uid=4242".to_string()));
        assert!(!options.contains(&"uid=1000".to_string()));
        assert_eq!(options.get(2).map(String::as_str), Some("uid=4242"));
        assert_eq!(options.last().map(String::as_str), Some("rw"));
    }

    #[test]
    fn build_options_defaults_and_unknown_placeholders() {
        let config = Config::default();
        // Without a desktop user, uid/gid fall back to 0 and {user} degrades to root; rw goes last
        let options = build_options(&config, "ext4", "ext4", None, false, &[], None);
        assert_eq!(options, vec!["nosuid", "nodev", "rw"]);

        let options = build_options(
            &config,
            "exfat",
            "exfat",
            None,
            false,
            &[
                "foo={unknown}".to_string(),
                "bar={uid".to_string(),
                "uid={uid}".to_string(),
                "gid={gid}".to_string(),
                "user={user}".to_string(),
            ],
            None,
        );
        // Options containing unknown placeholders are dropped
        assert!(!options.iter().any(|option| option.starts_with("foo=")));
        assert!(!options.iter().any(|option| option.starts_with("bar=")));
        assert!(options.contains(&"uid=0".to_string()));
        assert!(options.contains(&"gid=0".to_string()));
        assert_eq!(options.last().map(String::as_str), Some("rw"));
    }

    #[test]
    fn read_only_and_ownership_helpers() {
        let config = Config::default();
        assert!(is_read_only("iso9660", &config));
        assert!(is_read_only("ISO9660", &config));
        assert!(!is_read_only("ext4", &config));
        assert!(!is_read_only("  ", &config));

        assert!(needs_ownership_fix("vfat", &config));
        assert!(needs_ownership_fix("ntfs", &config));
        assert!(needs_ownership_fix("exfat", &config));
        assert!(!needs_ownership_fix("ext4", &config));
        assert!(!needs_ownership_fix("btrfs", &config));

        let mut config = config;
        config.chown_native_filesystems = true;
        assert!(needs_ownership_fix("ext4", &config));
    }

    #[test]
    fn suggests_packages() {
        let hints = suggest_packages("exfat");
        let joined = hints.join(" ");
        assert!(joined.contains("exfatprogs"));
        assert!(joined.contains("exfat-fuse"));
        assert!(joined.contains("install"));

        let hints = suggest_packages("ntfs");
        assert!(hints.join(" ").contains("ntfs-3g"));

        // Unknown types get generic advice instead of an empty list
        let hints = suggest_packages("myfs");
        assert!(!hints.is_empty());
        assert!(hints.join(" ").contains("myfs"));
    }

    #[test]
    fn missing_proc_file_is_not_an_error() {
        // Use TempDir to build a path that does not exist, and confirm the module needs no external command
        let temp = TempDir::new("fstypes-missing");
        let runner = MockRunner::new().missing("blkid");
        assert!(!runner.which("blkid"));
        assert!(read_proc_filesystems_from(&temp.join("filesystems")).is_empty());
        // MockRunner was never called: this module is pure logic
        assert!(runner.calls().is_empty());
    }
}
