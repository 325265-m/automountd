//! Mount point naming and path safety.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Turn a volume label into a safe directory name.
///
/// Unicode letters (for example Chinese labels), digits, `.`, `-`, and `_` are allowed;
/// whitespace and control characters become `_`, any other character is dropped to `_`, and leading
/// or trailing dots and dashes are trimmed off.
pub fn sanitize_label(label: Option<&str>, fallback: &str) -> String {
    let Some(label) = label else {
        return fallback.to_string();
    };
    let mut cleaned = String::with_capacity(label.len());
    let mut last_underscore = false;
    for ch in label.trim().chars() {
        if ch.is_alphanumeric() || ch == '.' || ch == '-' {
            cleaned.push(ch);
            last_underscore = false;
        } else if !ch.is_control() && !last_underscore {
            cleaned.push('_');
            last_underscore = true;
        }
    }
    let trimmed: String = cleaned
        .trim_matches(|c| c == '.' || c == '-' || c == '_')
        .to_string();
    let trimmed: String = trimmed.chars().take(48).collect();
    let trimmed = trimmed
        .trim_matches(|c| c == '.' || c == '-' || c == '_')
        .to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        fallback.to_string()
    } else {
        trimmed
    }
}

/// When the volume label is missing, build a directory name from the UUID prefix or the device name.
pub fn fallback_label(device_name: &str, uuid: Option<&str>) -> String {
    if let Some(uuid) = uuid {
        let short: String = uuid
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        if !short.is_empty() {
            return format!("disk-{short}");
        }
    }
    let cleaned = sanitize_label(Some(device_name), "disk");
    if cleaned.is_empty() {
        "disk".to_string()
    } else {
        cleaned
    }
}

/// Append `_2`, `_3`, ... when the directory already exists.
pub fn unique_dir(parent: &Path, name: &str, reserved: &HashSet<String>) -> PathBuf {
    let mut candidate = parent.join(name);
    let mut index = 1;
    while candidate.exists() || reserved.contains(&candidate.display().to_string()) {
        index += 1;
        candidate = parent.join(format!("{name}_{index}"));
        if index > 200 {
            break;
        }
    }
    candidate
}

/// Verify that target lies below root and return the normalized path.
pub fn safe_target(root: &Path, target: &Path) -> Result<PathBuf> {
    let root_real = std::fs::canonicalize(root).map_err(|err| {
        Error::device(format!(
            "mount root {} is not usable: {err}",
            root.display()
        ))
        .with_hint("make sure the directory exists and is accessible")
    })?;
    let target_real = canonicalize_allow_missing(target)?;
    if target_real == root_real {
        return Err(Error::device(format!(
            "mount point must not be the mount root {}",
            root.display()
        )));
    }
    if !target_real.starts_with(&root_real) {
        return Err(Error::device(format!(
            "mount point {} is outside the allowed root {}",
            target.display(),
            root.display()
        ))
        .with_hint("the label may contain suspicious characters; the operation was rejected"));
    }
    Ok(target_real)
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path).map_err(|err| {
            Error::device(format!("cannot resolve path {}: {err}", path.display()))
        });
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::device(format!("path {} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::device(format!("path {} has no file name", path.display())))?;
    let parent_real = canonicalize_allow_missing(parent)?;
    Ok(parent_real.join(name))
}

/// Check that no existing path component between root and target is a symlink.
///
/// A lexical path is used (target is not canonicalized first), otherwise `strip_prefix` would fail
/// when root itself is a symlink and the check would be skipped silently.
pub fn check_no_symlink_components(root: &Path, target: &Path) -> Result<()> {
    let root_abs = absolute_path(root)?;
    let target_abs = absolute_path(target)?;
    let Ok(relative) = target_abs.strip_prefix(&root_abs) else {
        return Err(Error::device(format!(
            "mount point {} is outside the allowed root {}",
            target.display(),
            root.display()
        ))
        .with_hint("the label may contain suspicious characters; the operation was rejected"));
    };
    let mut current = root_abs;
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                current.push(part);
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(Error::device(format!(
                            "mount path contains a symlink: {}",
                            current.display()
                        ))
                        .with_hint(
                            "to avoid path hijacking, remove the link or use a different label",
                        ));
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                    Err(_) => break,
                }
            }
            Component::ParentDir => {
                return Err(Error::device("mount path must not contain .."));
            }
            _ => {}
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|err| {
        Error::device(format!("cannot determine current working directory: {err}"))
    })?;
    Ok(cwd.join(path))
}

/// Terminal display width (wide characters such as Chinese count as 2).
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    let code = ch as u32;
    if code == 0 {
        return 0;
    }
    if code < 0x20 || (0x7f..0xa0).contains(&code) {
        return 0;
    }
    let wide = (0x1100..=0x115f).contains(&code)
        || (0x2e80..=0xa4cf).contains(&code)
        || (0xac00..=0xd7a3).contains(&code)
        || (0xf900..=0xfaff).contains(&code)
        || (0xfe10..=0xfe19).contains(&code)
        || (0xfe30..=0xfe6f).contains(&code)
        || (0xff00..=0xff60).contains(&code)
        || (0xffe0..=0xffe6).contains(&code)
        || (0x1f300..=0x1f64f).contains(&code)
        || (0x1f900..=0x1f9ff).contains(&code);
    if wide {
        2
    } else {
        1
    }
}
