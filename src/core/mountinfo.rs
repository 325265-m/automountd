//! `/proc/self/mountinfo` 解析。
//!
//! 格式（Linux 内核文档 proc(5)）::
//!
//! ```text
//!     36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
//! ```
//!
//! 路径中的空格等字符以 `\040` 形式转义。

use std::path::Path;

use crate::model::MountEntry;

/// 还原 mountinfo 中的八进制转义。
pub fn unescape(field: &str) -> String {
    let mut output = String::with_capacity(field.len());
    let mut chars = field.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let mut digits = String::new();
        while digits.len() < 3 {
            match chars.peek() {
                Some(next) if next.is_ascii_digit() && *next < '8' => {
                    digits.push(*next);
                    chars.next();
                }
                _ => break,
            }
        }
        if digits.len() == 3 {
            if let Ok(code) = u32::from_str_radix(&digits, 8) {
                if let Some(decoded) = char::from_u32(code) {
                    output.push(decoded);
                    continue;
                }
            }
        }
        output.push('\\');
        output.push_str(&digits);
    }
    output
}

/// 解析 mountinfo 文本。格式异常的行会被跳过。
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        let right_fields: Vec<&str> = right.split_whitespace().collect();
        if left_fields.len() < 6 || right_fields.len() < 3 {
            continue;
        }
        let (Ok(mount_id), Ok(parent_id)) =
            (left_fields[0].parse::<u32>(), left_fields[1].parse::<u32>())
        else {
            continue;
        };
        entries.push(MountEntry {
            mount_id,
            parent_id,
            major_minor: left_fields[2].to_string(),
            root: unescape(left_fields[3]),
            target: unescape(left_fields[4]),
            options: left_fields[5].split(',').map(str::to_string).collect(),
            fstype: right_fields[0].to_string(),
            source: unescape(right_fields[1]),
            super_options: right_fields[2].split(',').map(str::to_string).collect(),
        });
    }
    entries
}

/// 读取 `/proc/self/mountinfo`，失败时返回空列表。
pub fn read_mountinfo() -> Vec<MountEntry> {
    read_mountinfo_from(Path::new("/proc/self/mountinfo"))
}

/// 从指定文件读取挂载表（测试用）。
pub fn read_mountinfo_from(path: &Path) -> Vec<MountEntry> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_mountinfo(&text),
        Err(err) => {
            crate::logging::debug(format!("无法读取 {}: {err}", path.display()));
            Vec::new()
        }
    }
}

/// 规范化路径用于比较：解析符号链接、去掉尾部斜杠。
pub fn normalize_path(path: &str) -> String {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical.to_string_lossy().into_owned();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 按挂载点或设备查找记录（同时给出时要求都匹配）。
pub fn find_mount<'a>(
    mounts: &'a [MountEntry],
    target: Option<&str>,
    source: Option<&str>,
) -> Option<&'a MountEntry> {
    if target.is_none() && source.is_none() {
        return None;
    }
    let target = target.map(normalize_path);
    let source = source.map(normalize_path);
    mounts.iter().find(|entry| {
        target
            .as_deref()
            .map(|wanted| normalize_path(&entry.target) == wanted)
            .unwrap_or(true)
            && source
                .as_deref()
                .map(|wanted| normalize_path(&entry.source) == wanted)
                .unwrap_or(true)
    })
}

/// 判断挂载点是否已挂载。
pub fn is_mounted(target: &str, mounts: &[MountEntry]) -> bool {
    find_mount(mounts, Some(target), None).is_some()
}

/// 返回某设备的所有挂载记录。
pub fn find_by_source<'a>(source: &str, mounts: &'a [MountEntry]) -> Vec<&'a MountEntry> {
    let wanted = normalize_path(source);
    mounts
        .iter()
        .filter(|entry| normalize_path(&entry.source) == wanted)
        .collect()
}

/// 返回挂载点位于 target 之下的所有记录。
pub fn child_mounts<'a>(target: &str, mounts: &'a [MountEntry]) -> Vec<&'a MountEntry> {
    let base = normalize_path(target);
    let prefix = if base.ends_with('/') {
        base
    } else {
        format!("{base}/")
    };
    mounts
        .iter()
        .filter(|entry| normalize_path(&entry.target).starts_with(&prefix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "21 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw,errors=remount-ro\n",
        "36 21 8:17 / /media/My\\040Disk rw,nosuid,nodev shared:3 - exfat /dev/sdb1 rw,errors=remount-ro\n",
        "37 36 0:42 / /media/My\\040Disk/sub rw,nosuid shared:4 - tmpfs none rw\n",
        "bad line without separator\n",
    );

    #[test]
    fn unescapes_octal() {
        assert_eq!(unescape("/media/My\\040Disk"), "/media/My Disk");
        assert_eq!(unescape("/a\\011b"), "/a\tb");
        assert_eq!(unescape("/plain"), "/plain");
        assert_eq!(unescape("/keep\\09"), "/keep\\09");
    }

    #[test]
    fn parses_sample() {
        let entries = parse_mountinfo(SAMPLE);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].fstype, "ext4");
        assert_eq!(entries[0].source, "/dev/sda1");
        assert_eq!(entries[1].target, "/media/My Disk");
        assert_eq!(entries[1].fstype, "exfat");
        assert_eq!(entries[1].options, vec!["rw", "nosuid", "nodev"]);
        assert!(entries[1]
            .super_options
            .iter()
            .any(|o| o == "errors=remount-ro"));
    }

    #[test]
    fn finds_mounts() {
        let entries = parse_mountinfo(SAMPLE);
        let found = find_mount(&entries, Some("/media/My Disk/"), None).unwrap();
        assert_eq!(found.source, "/dev/sdb1");

        let found = find_mount(&entries, None, Some("/dev/sdb1")).unwrap();
        assert_eq!(found.target, "/media/My Disk");

        assert!(find_mount(&entries, Some("/nope"), None).is_none());
        assert!(find_mount(&entries, None, None).is_none());
        assert!(is_mounted("/media/My Disk", &entries));
    }

    #[test]
    fn finds_children() {
        let entries = parse_mountinfo(SAMPLE);
        let children = child_mounts("/media/My Disk", &entries);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].target, "/media/My Disk/sub");
    }

    #[test]
    fn reads_real_mountinfo_when_available() {
        if cfg!(target_os = "linux") {
            let entries = read_mountinfo();
            assert!(!entries.is_empty());
            assert!(entries.iter().any(|entry| entry.target == "/"));
        }
    }
}
