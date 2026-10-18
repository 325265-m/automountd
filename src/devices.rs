//! 块设备发现：优先 `lsblk -J`，否则回退 sysfs + blkid。

use std::collections::{BTreeMap, HashSet};

use crate::error::Result;
use crate::exec::CommandRunner;
use crate::model::{BlockDevice, MountEntry};

/// 完整列集合（较新的 util-linux）。
pub const FULL_COLUMNS: &str = "NAME,TYPE,FSTYPE,LABEL,UUID,PARTUUID,SIZE,RM,RO,HOTPLUG,TRAN,MOUNTPOINT,MODEL,SERIAL,PKNAME";
/// 兼容旧版 util-linux 的最小列集合。
pub const MINIMAL_COLUMNS: &str = "NAME,TYPE,FSTYPE,LABEL,UUID,SIZE,RM,RO,MOUNTPOINT";

/// 列出当前所有块设备。
pub fn list_devices(
    runner: &dyn CommandRunner,
    include_loop: bool,
    prefer_lsblk: bool,
) -> Vec<BlockDevice> {
    todo!()
}

/// 解析 `lsblk -J` 输出（嵌套 JSON 展平）。
pub fn parse_lsblk(text: &str) -> Result<Vec<BlockDevice>> {
    todo!()
}

/// 用 blkid 读取文件系统信息；失败或无文件系统时返回空表。
pub fn probe_blkid(runner: &dyn CommandRunner, device_path: &str) -> BTreeMap<String, String> {
    todo!()
}

/// 重新探测文件系统（udev 事件到达时分区可能尚未就绪）。
pub fn refresh_device(device: &BlockDevice, runner: &dyn CommandRunner) -> BlockDevice {
    todo!()
}

/// 按 设备名 / 路径 / UUID / PARTUUID / 卷标 查找设备。
///
/// 支持 `sdb1`、`/dev/sdb1`、`UUID=xxxx`、`/dev/disk/by-uuid/xxxx`、
/// `LABEL=备份盘`、`备份盘` 等写法。
pub fn resolve_device(devices: &[BlockDevice], reference: &str) -> Result<BlockDevice> {
    todo!()
}

/// 按设备名查找。
pub fn find_by_name(devices: &[BlockDevice], name: &str) -> Option<BlockDevice> {
    todo!()
}

/// 向上追到整盘名（sda1 -> sda）。
pub fn top_level_name(devices: &[BlockDevice], device: &BlockDevice) -> String {
    todo!()
}

/// 找出承载系统关键目录（/、/boot、/usr 等）的整盘名。
pub fn system_disk_names(devices: &[BlockDevice], mounts: &[MountEntry]) -> HashSet<String> {
    todo!()
}
