//! `/etc/fstab` 持久化。

use std::path::Path;

use crate::error::Result;

/// 推荐给移动磁盘的 systemd 友好选项：设备缺席时不影响开机。
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
        todo!()
    }
}

/// fstab 中用 `\040` 表示空格。
pub fn escape_field(text: &str) -> String {
    todo!()
}

/// 按空白拆分 fstab 行，尊重反斜杠转义，遇到 `#` 注释截止。
pub fn split_fields(line: &str) -> Vec<String> {
    todo!()
}

pub fn parse_fstab(text: &str) -> Vec<FstabEntry> {
    todo!()
}

pub fn load_fstab(path: &Path) -> Result<Vec<FstabEntry>> {
    todo!()
}

pub fn find_entry<'a>(
    entries: &'a [FstabEntry],
    source: Option<&str>,
    target: Option<&str>,
) -> Option<&'a FstabEntry> {
    todo!()
}

/// 添加一条记录，返回 (是否修改, 说明)。
pub fn add_entry(path: &Path, entry: &FstabEntry) -> Result<(bool, String)> {
    todo!()
}

/// 删除匹配记录（source 或 target 任一匹配）。
pub fn remove_entry(
    path: &Path,
    source: Option<&str>,
    target: Option<&str>,
) -> Result<(bool, String)> {
    todo!()
}
