//! udev 事件源：解析 `udevadm monitor --property` 输出，或在无 udev 环境轮询。

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::error::Result;
use crate::exec::CommandRunner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdevEvent {
    pub action: String,
    pub devname: String,
    pub devpath: String,
    pub devtype: Option<String>,
    pub properties: BTreeMap<String, String>,
}

/// 解析一段 udevadm monitor 输出。
pub fn parse_udevadm_output(text: &str) -> Vec<UdevEvent> {
    let _ = text;
    todo!()
}

/// 启动 `udevadm monitor` 子进程并在后台线程读取事件。
pub struct UdevMonitor {
    pid: u32,
}

impl UdevMonitor {
    pub fn spawn(runner: &dyn CommandRunner, sender: Sender<UdevEvent>) -> Result<Self> {
        let _ = (runner, sender);
        todo!()
    }

    /// 终止子进程（守护进程退出时调用）。
    pub fn stop(&self) {
        todo!()
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// 无 udev 时按固定间隔对比设备快照，生成 add/remove 事件。
pub struct PollMonitor;

impl PollMonitor {
    pub fn spawn(
        runner: Arc<dyn CommandRunner>,
        interval: Duration,
        sender: Sender<UdevEvent>,
    ) -> Self {
        let _ = (runner, interval, sender);
        todo!()
    }
}
