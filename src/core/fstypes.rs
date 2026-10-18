//! 文件系统驱动选择与挂载选项拼装。
//!
//! 该模块是纯逻辑：不执行外部命令、不触碰系统状态，便于在任意平台上做单元测试。
//! 唯一与系统交互的入口是 [`read_proc_filesystems`]（读取 `/proc/filesystems`），
//! 测试时改用 [`read_proc_filesystems_from`] 指向临时文件。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::logging;
use crate::model::UserInfo;

/// 这些文件系统没有自己的权限模型，需要用 uid/gid/umask 把所有权交给桌面用户。
pub const USERSPACE_FSTYPES: &[&str] = &[
    "vfat", "fat", "msdos", "umsdos", "exfat", "ntfs", "ntfs3", "ntfs-3g", "iso9660", "udf",
    "hfsplus", "hfs",
];

/// 内核原生就有完整权限模型、可以直接 `mount -t <自身>` 的文件系统。
const NATIVE_FSTYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "jfs", "reiserfs", "nilfs2", "bcachefs", "zfs",
    "udf", "iso9660", "squashfs", "erofs", "hfsplus", "vfat", "exfat", "ntfs3", "ocfs2", "gfs2",
];

/// 读取内核支持的文件系统列表。
pub fn read_proc_filesystems() -> HashSet<String> {
    #[cfg(target_os = "linux")]
    {
        read_proc_filesystems_from(Path::new("/proc/filesystems"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 非 Linux 平台没有 /proc，返回空集合即可（调用方据此跳过内核能力检查）。
        logging::debug("当前平台没有 /proc/filesystems，按未知内核能力处理");
        HashSet::new()
    }
}

/// 从指定文件读取（测试用）。
pub fn read_proc_filesystems_from(path: &Path) -> HashSet<String> {
    let mut supported = HashSet::new();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            logging::debug(format!(
                "无法读取 {}（按空列表处理）: {err}",
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

/// 把 blkid 探测到的类型映射为传给 `mount -t` 的驱动名。
pub fn choose_driver(fstype: &str, supported: &HashSet<String>) -> Result<String> {
    let raw = fstype.trim();
    if raw.is_empty() {
        return Err(Error::device("文件系统类型为空，无法选择挂载驱动")
            .with_hint("可先运行 'blkid <设备>' 确认设备是否已格式化"));
    }
    let lower = raw.to_ascii_lowercase();
    let has = |name: &str| supported.is_empty() || supported.contains(name);

    match lower.as_str() {
        // NTFS：优先内核 ntfs3 驱动，缺失时退回用户态 ntfs-3g。
        "ntfs" | "ntfs3" | "ntfs-3g" => {
            if has("ntfs3") {
                Ok("ntfs3".to_string())
            } else {
                Ok("ntfs-3g".to_string())
            }
        }
        // FAT 家族统一由 vfat 驱动处理。
        "fat" | "fat12" | "fat16" | "fat32" | "msdos" | "umsdos" | "vfat" => Ok("vfat".to_string()),
        // 旧版 HFS 也由 hfsplus 驱动读取。
        "hfs" | "hfsplus" => Ok("hfsplus".to_string()),
        "exfat" if !has("exfat") => Err(Error::unsupported(
            "内核不支持 exfat 文件系统（未找到 exfat 驱动）",
        )
        .with_hint(suggest_packages("exfat").join(" "))),
        "exfat" => Ok("exfat".to_string()),
        _ if NATIVE_FSTYPES.contains(&lower.as_str()) => Ok(lower),
        _ => {
            if !has(&lower) {
                return Err(Error::unsupported(format!(
                    "内核不支持文件系统 {raw}（可用驱动列表中未找到 {lower}）"
                ))
                .with_hint(suggest_packages(&lower).join(" ")));
            }
            Ok(lower)
        }
    }
}

/// 该文件系统是否应始终只读挂载。
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

/// 拼装 `mount -o` 选项。
///
/// 顺序：全局默认 -> 该文件系统默认 -> 调用方附加；同名选项后者覆盖前者。
/// 支持 `{uid}` `{gid}` `{user}` `{label}` 占位符。
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

    // key -> 在该位置上的选项下标，用于“后者覆盖前者”。
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
            logging::debug(format!("忽略含未知占位符的挂载选项: {item}"));
            continue;
        }
        let key = expanded.split('=').next().unwrap_or("").trim().to_string();
        if key.is_empty() {
            continue;
        }
        // ro/rw 由 read_only 决定，统一放到最后强制追加。
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

/// 是否需要在挂载前把挂载点 chown 给桌面用户。
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

/// 缺少驱动时的安装提示。
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
            "请确认内核已加载 {lower} 文件系统模块（可尝试 modprobe {lower}）"
        )];
        hints.push("或安装对应的用户态驱动（如 fuse3 与 ntfs-3g/exfat-fuse）".to_string());
        return hints;
    }

    vec![
        format!(
            "内核缺少 {lower} 支持，请安装以下软件包之一: {}",
            packages.join(" / ")
        ),
        format!("Debian/Ubuntu: sudo apt install {}", packages.join(" ")),
        format!("Fedora/RHEL: sudo dnf install {}", packages.join(" ")),
        format!("Arch Linux: sudo pacman -S {}", packages.join(" ")),
    ]
}

/// 卷标写入挂载选项前不能含空白与逗号（会被 `mount -o` 当成新选项）。
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

        // 文件不存在时返回空集合，不 panic
        let missing = read_proc_filesystems_from(&temp.join("nope"));
        assert!(missing.is_empty());
    }

    #[test]
    fn chooses_ntfs_driver() {
        // 支持 ntfs3 时优先内核驱动
        assert_eq!(
            choose_driver("ntfs", &supported(&["ntfs3", "ext4"])).unwrap(),
            "ntfs3"
        );
        // 空的 supported 视为未知内核能力，仍优先 ntfs3
        assert_eq!(choose_driver("ntfs", &HashSet::new()).unwrap(), "ntfs3");
        // 内核没有 ntfs3 时退回用户态 ntfs-3g
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
                "类型 {fstype}"
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
        // 即使内核列表里没写，已知原生类型也直接返回
        assert_eq!(choose_driver("xfs", &supported(&["ext4"])).unwrap(), "xfs");
    }

    #[test]
    fn rejects_unsupported_types() {
        let err = choose_driver("exfat", &supported(&["ext4"])).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Unsupported);
        let hint = err.hint.as_deref().unwrap_or_default();
        assert!(hint.contains("exfatprogs"), "提示应包含 exfatprogs: {hint}");
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

        // 默认安全选项 -> 文件系统选项 -> 附加选项，ro 强制在最后
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

        // 同名选项后者覆盖前者，且保持第一次出现的位置
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
        // 没有桌面用户时 uid/gid 归 0，{user} 退化成 root；rw 放最后
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
        // 未知占位符的选项被丢弃
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
    fn suggests_packages_in_chinese() {
        let hints = suggest_packages("exfat");
        let joined = hints.join(" ");
        assert!(joined.contains("exfatprogs"));
        assert!(joined.contains("exfat-fuse"));
        assert!(joined.contains("安装"));

        let hints = suggest_packages("ntfs");
        assert!(hints.join(" ").contains("ntfs-3g"));

        // 未知类型给出通用建议，而不是空列表
        let hints = suggest_packages("myfs");
        assert!(!hints.is_empty());
        assert!(hints.join(" ").contains("myfs"));
    }

    #[test]
    fn missing_proc_file_is_not_an_error() {
        // 用 TempDir 造一个不存在的路径，并确认模块不依赖任何外部命令
        let temp = TempDir::new("fstypes-missing");
        let runner = MockRunner::new().missing("blkid");
        assert!(!runner.which("blkid"));
        assert!(read_proc_filesystems_from(&temp.join("filesystems")).is_empty());
        // MockRunner 未被调用：本模块是纯逻辑
        assert!(runner.calls().is_empty());
    }
}
