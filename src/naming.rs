//! 挂载点命名与路径安全。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// 把卷标转换成安全的目录名。
///
/// 允许 Unicode 字母（例如中文卷标）、数字、`.`、`-`、`_`；
/// 空白与控制字符转成 `_`，其余字符丢弃为 `_`，并去掉开头结尾的点/横线。
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
        } else if ch.is_whitespace() || ch == '_' {
            if !last_underscore {
                cleaned.push('_');
                last_underscore = true;
            }
        } else if !ch.is_control() {
            if !last_underscore {
                cleaned.push('_');
                last_underscore = true;
            }
        }
    }
    let trimmed: String = cleaned.trim_matches(|c| c == '.' || c == '-' || c == '_').to_string();
    let trimmed: String = trimmed.chars().take(48).collect();
    let trimmed = trimmed.trim_matches(|c| c == '.' || c == '-' || c == '_').to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        fallback.to_string()
    } else {
        trimmed
    }
}

/// 卷标缺失时用 UUID 前缀或设备名生成目录名。
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

/// 目录已存在时追加 `_2`、`_3`……
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

/// 确认 target 位于 root 之下，返回规范化路径。
pub fn safe_target(root: &Path, target: &Path) -> Result<PathBuf> {
    let root_real = std::fs::canonicalize(root).map_err(|err| {
        Error::device(format!("挂载根目录 {} 不可用: {err}", root.display()))
            .with_hint("请确认该目录存在且可访问")
    })?;
    let target_real = canonicalize_allow_missing(target)?;
    if target_real == root_real {
        return Err(Error::device(format!(
            "挂载点不能等于挂载根目录 {}",
            root.display()
        )));
    }
    if !target_real.starts_with(&root_real) {
        return Err(Error::device(format!(
            "挂载点 {} 不在允许的根目录 {} 之内",
            target.display(),
            root.display()
        ))
        .with_hint("卷标可能包含可疑字符，已拒绝操作"));
    }
    Ok(target_real)
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .map_err(|err| Error::device(format!("无法解析路径 {}: {err}", path.display())));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::device(format!("路径 {} 没有父目录", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::device(format!("路径 {} 缺少文件名", path.display())))?;
    let parent_real = canonicalize_allow_missing(parent)?;
    Ok(parent_real.join(name))
}

/// 检查 root 到 target 之间已存在的路径组件不是符号链接。
pub fn check_no_symlink_components(root: &Path, target: &Path) -> Result<()> {
    let root_abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target_abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| Error::device(format!("无法获取当前目录: {err}")))?
            .join(target)
    };
    let Ok(relative) = target_abs.strip_prefix(&root_abs) else {
        return Ok(());
    };
    let mut current = root_abs.clone();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                current.push(part);
                if current.is_symlink() {
                    return Err(Error::device(format!(
                        "挂载路径中包含符号链接: {}",
                        current.display()
                    ))
                    .with_hint("为避免路径劫持，请删除该链接或更换卷标"));
                }
                if !current.exists() {
                    break;
                }
            }
            Component::ParentDir => {
                return Err(Error::device("挂载路径不能包含 .."));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 终端显示宽度（中文等宽字符按 2 计算）。
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
