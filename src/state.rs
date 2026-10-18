//! 运行期状态：已挂载记录 + 守护进程元信息。
//!
//! 文件格式为带注释的 TSV（`<state_dir>/state.tsv`），权限 0644，
//! 供非 root 用户执行 `automountd status/list` 时读取。

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;
use crate::model::MountRecord;

#[derive(Debug, Clone)]
pub struct State {
    pub path: PathBuf,
    pub records: BTreeMap<String, MountRecord>,
    pub pid: u32,
    pub started_at: u64,
    pub dry_run: bool,
    pub backend: String,
}

impl State {
    pub fn new(config: &Config) -> Self {
        let _ = config;
        todo!()
    }

    /// 创建状态目录（0755）。
    pub fn prepare(&self) -> Result<()> {
        todo!()
    }

    /// 读取磁盘上的状态，文件缺失不算错误。
    pub fn load(&mut self) -> Result<()> {
        todo!()
    }

    /// 原子写入状态文件。
    pub fn save(&self) -> Result<()> {
        todo!()
    }

    pub fn add(&mut self, record: MountRecord) {
        let _ = record;
        todo!()
    }

    pub fn remove(&mut self, device: &str) -> Option<MountRecord> {
        let _ = device;
        todo!()
    }

    pub fn get(&self, device: &str) -> Option<&MountRecord> {
        let _ = device;
        todo!()
    }

    pub fn find_by_target(&self, target: &str) -> Option<&MountRecord> {
        let _ = target;
        todo!()
    }

    pub fn records(&self) -> impl Iterator<Item = &MountRecord> {
        self.records.values()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// 构造并加载状态（忽略读取错误）。
pub fn read_state(config: &Config) -> State {
    let _ = config;
    todo!()
}
