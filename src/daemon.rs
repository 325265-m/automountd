//! 守护进程：udev 事件循环、开机挂载、控制接口。

use std::sync::Arc;

use crate::config::Config;
use crate::error::Result;
use crate::exec::CommandRunner;

pub struct Daemon {
    _private: (),
}

impl Daemon {
    pub fn new(config: Config, runner: Arc<dyn CommandRunner>) -> Result<Self> {
        let _ = (config, runner);
        todo!()
    }

    /// 阻塞运行直到收到 SIGTERM/SIGINT 或控制命令 shutdown。
    pub fn run(&mut self) -> Result<()> {
        todo!()
    }

    pub fn request_stop(&self) {
        todo!()
    }
}
