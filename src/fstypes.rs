//! 文件系统驱动选择与挂载选项拼装。

use std::collections::HashSet;
use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::model::UserInfo;

/// 这些文件系统没有自己的权限模型，需要用 uid/gid/umask 把所有权交给桌面用户。
pub const USERSPACE_FSTYPES: &[&str] = &[
    "vfat", "fat", "msdos", "umsdos", "exfat", "ntfs", "ntfs3", "ntfs-3g", "iso9660", "udf",
    "hfsplus", "hfs",
];

/// 读取内核支持的文件系统列表。
pub fn read_proc_filesystems() -> HashSet<String> {
    todo!()
}

/// 从指定文件读取（测试用）。
pub fn read_proc_filesystems_from(path: &Path) -> HashSet<String> {
    todo!()
}

/// 把 blkid 探测到的类型映射为传给 `mount -t` 的驱动名。
pub fn choose_driver(fstype: &str, supported: &HashSet<String>) -> Result<String> {
    todo!()
}

/// 该文件系统是否应始终只读挂载。
pub fn is_read_only(fstype: &str, config: &Config) -> bool {
    todo!()
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
    todo!()
}

/// 是否需要在挂载前把挂载点 chown 给桌面用户。
pub fn needs_ownership_fix(fstype: &str, config: &Config) -> bool {
    todo!()
}

pub fn options_string(options: &[String]) -> String {
    todo!()
}

/// 缺少驱动时的安装提示。
pub fn suggest_packages(fstype: &str) -> Vec<String> {
    todo!()
}
