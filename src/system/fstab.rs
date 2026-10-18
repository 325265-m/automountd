//! `/etc/fstab` persistence.
//!
//! Only the parts this project needs: parse, look up, append, delete, and safe
//! writes (back up to `.automountd.bak` first, then write a temp file and replace
//! it atomically with `rename`).
//!
//! Format (fstab(5)):
//!
//! ```text
//! UUID=1234-ABCD /media/alice/backup-disk exfat defaults,nofail 0 0
//! ```
//!
//! A space inside a field is written as `\040`, a tab as `\011`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};
use crate::mountinfo;

/// systemd-friendly options recommended for removable disks: a missing device does not hold up boot.
pub const BASE_FSTAB_OPTIONS: &[&str] = &["nofail", "x-systemd.device-timeout=10"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FstabEntry {
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub options: Vec<String>,
    pub dump: String,
    pub passno: String,
    pub line_no: usize,
    pub raw: String,
}

impl FstabEntry {
    pub fn to_line(&self) -> String {
        let options = if self.options.is_empty() {
            "defaults".to_string()
        } else {
            self.options.join(",")
        };
        let dump = if self.dump.trim().is_empty() {
            "0".to_string()
        } else {
            self.dump.trim().to_string()
        };
        let passno = if self.passno.trim().is_empty() {
            "0".to_string()
        } else {
            self.passno.trim().to_string()
        };
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            escape_field(&self.source),
            escape_field(&self.target),
            self.fstype.trim(),
            options,
            dump,
            passno
        )
    }
}

/// In fstab a space is written as `\040`.
pub fn escape_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '#' => out.push_str("\\043"),
            other => out.push(other),
        }
    }
    out
}

/// Split an fstab line on whitespace, honoring backslash escapes and stopping at
/// a `#` comment.
///
/// Escape sequences are kept verbatim (e.g. `\040` is not split into two fields);
/// use [`crate::mountinfo::unescape`] to decode them.
pub fn split_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                current.push('\\');
                match chars.next() {
                    Some(next) => current.push(next),
                    None => break,
                }
            }
            '#' => break,
            other if other.is_whitespace() => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

pub fn parse_fstab(text: &str) -> Vec<FstabEntry> {
    let mut entries = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let fields = split_fields(raw);
        if fields.len() < 4 {
            continue;
        }
        let options = fields[3]
            .split(',')
            .map(str::trim)
            .filter(|option| !option.is_empty())
            .map(str::to_string)
            .collect::<Vec<String>>();
        entries.push(FstabEntry {
            source: fields[0].clone(),
            target: mountinfo::unescape(&fields[1]),
            fstype: fields[2].clone(),
            options,
            dump: fields.get(4).cloned().unwrap_or_else(|| "0".to_string()),
            passno: fields.get(5).cloned().unwrap_or_else(|| "0".to_string()),
            line_no: index + 1,
            raw: raw.to_string(),
        });
    }
    entries
}

pub fn load_fstab(path: &Path) -> Result<Vec<FstabEntry>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(parse_fstab(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(Error::new(
            ErrorKind::Io,
            format!("无法读取 {}: {err}", path.display()),
        )),
    }
}

pub fn find_entry<'a>(
    entries: &'a [FstabEntry],
    source: Option<&str>,
    target: Option<&str>,
) -> Option<&'a FstabEntry> {
    if source.is_none() && target.is_none() {
        return None;
    }
    entries
        .iter()
        .find(|entry| entry_matches(entry, source, target))
}

/// Add an entry; returns (whether anything changed, message).
pub fn add_entry(path: &Path, entry: &FstabEntry) -> Result<(bool, String)> {
    validate_entry(entry)?;
    let resolved = resolve_target(path);
    let path = resolved.as_path();
    let text = read_or_empty(path)?;
    let entries = parse_fstab(&text);

    if let Some(existing) = find_entry(&entries, Some(&entry.source), None) {
        return Ok((
            false,
            format!(
                "fstab 中已存在 {} 的记录（挂载点 {}），无需重复添加",
                entry.source, existing.target
            ),
        ));
    }
    if let Some(existing) = find_entry(&entries, None, Some(&entry.target)) {
        return Err(Error::config(format!(
            "挂载点 {} 已被 fstab 中的 {} 占用",
            existing.target, existing.source
        ))
        .with_hint("请先删除冲突条目，或改用其他挂载点"));
    }

    let mut merged = entry.clone();
    merged.options = merge_options(&entry.options);

    let mut output = text;
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&merged.to_line());
    output.push('\n');

    backup_file(path)?;
    write_atomic(path, &output)?;

    Ok((
        true,
        format!(
            "已写入 {}: {} -> {}（选项 {}）",
            path.display(),
            merged.source,
            merged.target,
            merged.options.join(",")
        ),
    ))
}

/// Delete matching entries (a match on either source or target is enough).
pub fn remove_entry(
    path: &Path,
    source: Option<&str>,
    target: Option<&str>,
) -> Result<(bool, String)> {
    if source.is_none() && target.is_none() {
        return Ok((false, "未指定要删除的设备或挂载点".to_string()));
    }
    for value in [source, target].into_iter().flatten() {
        if value.contains('\n') || value.contains('\r') {
            return Err(Error::config("fstab 字段不能包含换行符"));
        }
    }
    let resolved = resolve_target(path);
    let path = resolved.as_path();
    let text = read_or_empty(path)?;
    let entries = parse_fstab(&text);
    let matched: Vec<&FstabEntry> = entries
        .iter()
        .filter(|entry| entry_matches(entry, source, target))
        .collect();
    if matched.is_empty() {
        let wanted = match (source, target) {
            (Some(source), Some(target)) => format!("{source} / {target}"),
            (Some(source), None) => source.to_string(),
            (None, Some(target)) => target.to_string(),
            (None, None) => String::new(),
        };
        return Ok((false, format!("fstab 中没有匹配 {wanted} 的记录")));
    }

    let removed_lines: Vec<usize> = matched.iter().map(|entry| entry.line_no).collect();
    let mut output = String::new();
    for (index, line) in text.lines().enumerate() {
        if removed_lines.contains(&(index + 1)) {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }

    backup_file(path)?;
    write_atomic(path, &output)?;
    Ok((
        true,
        format!(
            "已从 fstab 删除 {} 条记录（{}）",
            removed_lines.len(),
            matched
                .iter()
                .map(|entry| entry.target.as_str())
                .collect::<Vec<&str>>()
                .join(", ")
        ),
    ))
}

/// Compare source / target; `None` leaves that dimension unconstrained.
fn entry_matches(entry: &FstabEntry, source: Option<&str>, target: Option<&str>) -> bool {
    let source_ok = source
        .map(|wanted| normalize_source(&entry.source) == normalize_source(wanted))
        .unwrap_or(true);
    let target_ok = target
        .map(|wanted| same_path(&entry.target, wanted))
        .unwrap_or(true);
    source_ok && target_ok
}

/// The `UUID=` / `PARTUUID=` / `LABEL=` prefixes are case-insensitive, and UUID values are case-folded too.
fn normalize_source(source: &str) -> String {
    let trimmed = source.trim();
    for prefix in ["UUID=", "PARTUUID=", "LABEL="] {
        // Use get() rather than byte slicing: slicing inside a non-ASCII character would panic.
        if let Some(head) = trimmed.get(..prefix.len()) {
            if head.eq_ignore_ascii_case(prefix) {
                let value = trimmed[prefix.len()..].trim();
                let value = if prefix.eq_ignore_ascii_case("LABEL=") {
                    value.to_string()
                } else {
                    value.to_ascii_uppercase()
                };
                return format!("{}{}", prefix.to_ascii_uppercase(), value);
            }
        }
    }
    trimmed.to_string()
}

fn same_path(left: &str, right: &str) -> bool {
    let left = mountinfo::normalize_path(left);
    let right = mountinfo::normalize_path(right);
    left == right || left.trim_end_matches('/') == right.trim_end_matches('/')
}

/// Merge options: existing ones first, `BASE_FSTAB_OPTIONS` appended, de-duplicated by the key before `=`.
fn merge_options(existing: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    let candidates = existing
        .iter()
        .map(String::as_str)
        .chain(BASE_FSTAB_OPTIONS.iter().copied());
    for option in candidates {
        let option = option.trim();
        if option.is_empty() {
            continue;
        }
        let key = option_key(option);
        match merged.iter().position(|item| option_key(item) == key) {
            Some(index) => merged[index] = option.to_string(),
            None => merged.push(option.to_string()),
        }
    }
    merged
}

fn option_key(option: &str) -> &str {
    option.split('=').next().unwrap_or(option).trim()
}

fn read_or_empty(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(Error::new(
            ErrorKind::Io,
            format!("无法读取 {}: {err}", path.display()),
        )),
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fstab".to_string())
}

fn parent_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Back up to `<file name>.automountd.bak` before writing; a missing file or an
/// existing backup is left alone, so the content from before the first change can
/// always be restored.
fn backup_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path.with_file_name(format!("{}.automountd.bak", file_name_of(path)));
    if backup.exists() {
        return Ok(());
    }
    fs::copy(path, &backup).map_err(|err| {
        Error::new(
            ErrorKind::Io,
            format!(
                "备份 {} 到 {} 失败: {err}",
                path.display(),
                backup.display()
            ),
        )
    })?;
    Ok(())
}

/// Validate the fields before writing fstab so a newline cannot inject an extra mount entry.
fn validate_entry(entry: &FstabEntry) -> Result<()> {
    for (name, value) in [
        ("source", entry.source.as_str()),
        ("target", entry.target.as_str()),
        ("fstype", entry.fstype.as_str()),
        ("dump", entry.dump.as_str()),
        ("passno", entry.passno.as_str()),
    ] {
        if value.contains('\n') || value.contains('\r') {
            return Err(Error::config(format!("fstab 字段 {name} 不能包含换行符")));
        }
    }
    if entry.target.is_empty() || entry.source.is_empty() {
        return Err(Error::config("fstab 的 source/target 不能为空"));
    }
    for option in &entry.options {
        if option.contains('\n') || option.contains('\r') {
            return Err(Error::config("fstab 挂载选项不能包含换行符"));
        }
    }
    Ok(())
}

/// If fstab itself is a symlink, write to its real target so the link is not replaced by a regular file.
fn resolve_target(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(_) => path.to_path_buf(),
    }
}

/// Temp file plus an atomic `rename`, preserving the original permissions and owner where possible.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let parent = parent_of(path);
    fs::create_dir_all(&parent).map_err(|err| {
        Error::new(
            ErrorKind::Io,
            format!("无法创建目录 {}: {err}", parent.display()),
        )
    })?;
    let tmp = parent.join(format!(
        ".{}.automountd.{}.tmp",
        file_name_of(path),
        std::process::id()
    ));
    {
        let mut file = fs::File::create(&tmp).map_err(|err| {
            Error::new(
                ErrorKind::Io,
                format!("无法写入临时文件 {}: {err}", tmp.display()),
            )
        })?;
        if let Err(err) = file.write_all(text.as_bytes()) {
            let _ = fs::remove_file(&tmp);
            return Err(Error::new(
                ErrorKind::Io,
                format!("写入 {} 失败: {err}", tmp.display()),
            ));
        }
        if let Err(err) = file.sync_all() {
            let _ = fs::remove_file(&tmp);
            return Err(Error::new(
                ErrorKind::Io,
                format!("同步 {} 失败: {err}", tmp.display()),
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(path).ok();
        let mode = metadata
            .as_ref()
            .map(|meta| meta.permissions().mode() & 0o7777)
            .unwrap_or(0o644);
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode));
        if let Some(meta) = metadata {
            if let Err(err) = crate::platform::chown(&tmp, meta.uid(), meta.gid()) {
                crate::logging::warn(format!("恢复 {} 的属主失败: {err}", tmp.display()));
            }
        }
    }
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        Error::new(
            ErrorKind::Io,
            format!("替换 {} 失败: {err}", path.display()),
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn sample_entry(source: &str, target: &str, options: &[&str]) -> FstabEntry {
        FstabEntry {
            source: source.to_string(),
            target: target.to_string(),
            fstype: "exfat".to_string(),
            options: options.iter().map(|item| (*item).to_string()).collect(),
            dump: "0".to_string(),
            passno: "0".to_string(),
            line_no: 0,
            raw: String::new(),
        }
    }

    #[test]
    fn splits_fields_and_keeps_escapes() {
        let fields = split_fields("UUID=1234-ABCD /media/My\\040Disk exfat defaults 0 0 # 注释");
        assert_eq!(
            fields,
            vec![
                "UUID=1234-ABCD",
                "/media/My\\040Disk",
                "exfat",
                "defaults",
                "0",
                "0"
            ]
        );
        assert_eq!(mountinfo::unescape(&fields[1]), "/media/My Disk");
        assert!(split_fields("   # 整行注释").is_empty());
        assert_eq!(split_fields("a\\tb c"), vec!["a\\tb", "c"]);
    }

    #[test]
    fn parses_entries_with_spaces_and_comments() {
        let text = concat!(
            "# /etc/fstab\n",
            "UUID=1111-2222 /media/My\\040Disk exfat defaults,nofail 0 2\n",
            "\n",
            "不完整的一行\n",
            "# LABEL=old /mnt/old ext4 defaults 0 0\n",
        );
        let entries = parse_fstab(text);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.source, "UUID=1111-2222");
        assert_eq!(entry.target, "/media/My Disk");
        assert_eq!(entry.fstype, "exfat");
        assert_eq!(entry.options, vec!["defaults", "nofail"]);
        assert_eq!(entry.dump, "0");
        assert_eq!(entry.passno, "2");
        assert_eq!(entry.line_no, 2);
        assert!(entry.raw.starts_with("UUID=1111-2222"));
    }

    #[test]
    fn escapes_and_roundtrips_line() {
        assert_eq!(escape_field("/media/My Disk"), "/media/My\\040Disk");
        assert_eq!(escape_field("a\tb"), "a\\011b");
        assert_eq!(escape_field("a\\b"), "a\\\\b");

        let entry = sample_entry("UUID=ABCD-1234", "/media/备份 盘", &["defaults", "nofail"]);
        let line = entry.to_line();
        let parsed = parse_fstab(&line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].target, "/media/备份 盘");
        assert_eq!(parsed[0].source, entry.source);
        assert_eq!(parsed[0].options, entry.options);
    }

    #[test]
    fn finds_entry_ignoring_prefix_case_and_trailing_slash() {
        let text = "uuid=abcd-1234 /media/My\\040Disk exfat defaults 0 0\n";
        let entries = parse_fstab(text);
        assert!(find_entry(&entries, Some("UUID=abcd-1234"), None).is_some());
        assert!(find_entry(&entries, None, Some("/media/My Disk/")).is_some());
        assert!(find_entry(&entries, Some("LABEL=nope"), None).is_none());
        assert!(find_entry(&entries, None, None).is_none());
    }

    #[test]
    fn adds_entry_idempotently_with_backup() {
        let dir = TempDir::new("fstab-add");
        let path = dir.join("fstab");
        std::fs::write(&path, "# 现有 fstab\nUUID=AAAA / ext4 defaults 0 1").unwrap();

        let entry = sample_entry("UUID=1234-ABCD", "/media/My Disk", &["uid=1000"]);
        let (changed, message) = add_entry(&path, &entry).unwrap();
        assert!(changed, "{message}");
        assert!(message.contains("已写入"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'), "补换行失败: {text:?}");
        assert!(text.contains("UUID=AAAA / ext4 defaults 0 1\n"));
        let entries = parse_fstab(&text);
        assert_eq!(entries.len(), 2);
        let added = &entries[1];
        assert!(added.options.contains(&"nofail".to_string()));
        assert!(added
            .options
            .contains(&"x-systemd.device-timeout=10".to_string()));
        assert_eq!(added.options[0], "uid=1000");

        let backup = dir.join("fstab.automountd.bak");
        assert!(backup.is_file(), "未生成备份文件");
        assert!(std::fs::read_to_string(&backup)
            .unwrap()
            .contains("UUID=AAAA"));

        // a second add must be idempotent
        let (changed, message) = add_entry(&path, &entry).unwrap();
        assert!(!changed);
        assert!(message.contains("已存在"));
        assert_eq!(
            parse_fstab(&std::fs::read_to_string(&path).unwrap()).len(),
            2
        );
    }

    #[test]
    fn rejects_conflicting_target() {
        let dir = TempDir::new("fstab-conflict");
        let path = dir.write("fstab", "UUID=AAAA /media/same exfat defaults 0 0\n");
        let entry = sample_entry("UUID=BBBB", "/media/same", &[]);
        let err = add_entry(&path, &entry).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);
        assert!(err.message.contains("已被 fstab 中的"), "{}", err.message);
    }

    #[test]
    fn removes_entry_by_source_or_target() {
        let dir = TempDir::new("fstab-remove");
        let path = dir.write(
            "fstab",
            concat!(
                "# 头部注释\n",
                "UUID=AAAA / ext4 defaults 0 1\n",
                "UUID=1234-ABCD /media/My\\040Disk exfat defaults 0 0\n",
                "UUID=BBBB /media/other exfat defaults 0 0\n",
            ),
        );
        let (removed, message) = remove_entry(&path, Some("uuid=1234-abcd"), None).unwrap();
        assert!(removed, "{message}");
        let entries = parse_fstab(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(entries.len(), 2);
        assert!(find_entry(&entries, Some("UUID=1234-ABCD"), None).is_none());
        assert!(dir.join("fstab.automountd.bak").is_file());

        let (removed, _) = remove_entry(&path, None, Some("/media/other/")).unwrap();
        assert!(removed);
        assert_eq!(
            parse_fstab(&std::fs::read_to_string(&path).unwrap()).len(),
            1
        );

        let (removed, message) = remove_entry(&path, Some("UUID=FFFF"), None).unwrap();
        assert!(!removed);
        assert!(message.contains("没有匹配"));
    }

    #[test]
    fn missing_file_is_treated_as_empty() {
        let dir = TempDir::new("fstab-missing");
        let path = dir.join("fstab");
        assert!(load_fstab(&path).unwrap().is_empty());
        let entry = sample_entry("UUID=1234-ABCD", "/media/data", &[]);
        let (changed, _) = add_entry(&path, &entry).unwrap();
        assert!(changed);
        assert!(!dir.join("fstab.automountd.bak").exists());
        assert_eq!(load_fstab(&path).unwrap().len(), 1);
    }
}
