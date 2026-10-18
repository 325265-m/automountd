//! udev 事件源：解析 `udevadm monitor --property` 输出，或在无 udev 环境轮询。

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::devices;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::CommandRunner;
use crate::logging;
use crate::platform;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdevEvent {
    pub action: String,
    pub devname: String,
    pub devpath: String,
    pub devtype: Option<String>,
    pub properties: BTreeMap<String, String>,
}

/// 单次累积的 udevadm 输出上限，防止异常输入撑爆内存。
const MAX_BUFFER: usize = 128 * 1024;
/// 轮询模式的默认间隔。
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 轮询线程退出检查的粒度。
const POLL_SLICE: Duration = Duration::from_millis(100);

/// 解析过程中累积的单个事件块。
struct PartialEvent {
    action: String,
    devpath: String,
    subsystem: String,
    properties: BTreeMap<String, String>,
}

/// 解析一段 udevadm monitor 输出。
pub fn parse_udevadm_output(text: &str) -> Vec<UdevEvent> {
    let mut events = Vec::new();
    let mut current: Option<PartialEvent> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(header) = parse_event_header(line) {
            // 连续块：新的头行直接结束上一个块（即使没有空行分隔）。
            if let Some(event) = current.take().and_then(finish_event) {
                events.push(event);
            }
            current = Some(header);
            continue;
        }
        let blank = match current.as_mut() {
            None => false,
            Some(part) => {
                if line.trim().is_empty() {
                    true
                } else {
                    if let Some((key, value)) = line.split_once('=') {
                        let key = key.trim();
                        if !key.is_empty() {
                            part.properties
                                .insert(key.to_string(), value.trim().to_string());
                        }
                    }
                    false
                }
            }
        };
        if blank {
            if let Some(event) = current.take().and_then(finish_event) {
                events.push(event);
            }
        }
    }

    if let Some(event) = current.take().and_then(finish_event) {
        events.push(event);
    }
    events
}

/// 识别 `UDEV  [1234.5678] add /devices/... (block)` 与 `KERNEL[...]` 头行。
fn parse_event_header(line: &str) -> Option<PartialEvent> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("UDEV")
        .or_else(|| trimmed.strip_prefix("KERNEL"))?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('[')?;
    let (_timestamp, rest) = rest.split_once(']')?;
    let mut tokens = rest.split_whitespace();
    let action = tokens.next()?.to_ascii_lowercase();
    let devpath = tokens.next().unwrap_or_default().to_string();
    let subsystem = tokens
        .next()
        .map(|token| token.trim_matches(|ch| ch == '(' || ch == ')').to_string())
        .unwrap_or_default();
    Some(PartialEvent {
        action,
        devpath,
        subsystem,
        properties: BTreeMap::new(),
    })
}

/// 补全并过滤一个事件块：只保留 block 子系统的 add/remove/change。
fn finish_event(part: PartialEvent) -> Option<UdevEvent> {
    let property_subsystem = part
        .properties
        .get("SUBSYSTEM")
        .map(String::as_str)
        .unwrap_or_default();
    // 头行里的 (block) 优先；缺失时看 SUBSYSTEM 属性；都没有则按 block 处理。
    let subsystem = if part.subsystem.is_empty() {
        property_subsystem
    } else {
        part.subsystem.as_str()
    };
    if !subsystem.is_empty() && !subsystem.eq_ignore_ascii_case("block") {
        return None;
    }
    if !matches!(part.action.as_str(), "add" | "remove" | "change") {
        return None;
    }
    let devname = part
        .properties
        .get("DEVNAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| devpath_basename(&part.devpath))?;
    let devtype = part
        .properties
        .get("DEVTYPE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Some(UdevEvent {
        action: part.action,
        devname,
        devpath: part.devpath,
        devtype,
        properties: part.properties,
    })
}

/// devpath 的最后一段作为设备名（DEVNAME 缺失时的兜底）。
fn devpath_basename(devpath: &str) -> Option<String> {
    let name = devpath.trim_end_matches('/').rsplit('/').next()?;
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 启动 `udevadm monitor` 子进程并在后台线程读取事件。
pub struct UdevMonitor {
    pid: u32,
}

impl UdevMonitor {
    pub fn spawn(runner: &dyn CommandRunner, sender: Sender<UdevEvent>) -> Result<Self> {
        if !runner.which("udevadm") {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "未找到 udevadm，无法监听 udev 热插拔事件",
            )
            .with_hint(
                "请安装 systemd/udev；在容器等无 udev 的环境中可设置 poll_interval_seconds 改用轮询模式",
            ));
        }

        let mut command = if runner.which("stdbuf") {
            let mut command = Command::new("stdbuf");
            command.args(["-oL", "udevadm"]);
            command
        } else {
            Command::new("udevadm")
        };
        command
            .args([
                "monitor",
                "--udev",
                "--subsystem-match=block",
                "--property",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|err| {
            Error::new(
                ErrorKind::Command,
                format!("无法启动 udevadm monitor: {err}"),
            )
            .with_hint("请确认 udevadm 可执行，并检查守护进程是否有权限读取内核事件")
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        logging::debug(format!("已启动 udevadm monitor（pid {pid}）"));

        std::thread::spawn(move || {
            let mut child = child;
            let mut buffer = String::new();
            let sender_ref = &sender;
            if let Some(stdout) = stdout {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if line.trim().is_empty() {
                                if !flush_events(&mut buffer, sender_ref) {
                                    break;
                                }
                            } else {
                                buffer.push_str(&line);
                                if buffer.len() > MAX_BUFFER
                                    && !flush_events(&mut buffer, sender_ref)
                                {
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            logging::debug(format!("读取 udevadm monitor 输出失败: {err}"));
                            break;
                        }
                    }
                }
            }
            let _ = flush_events(&mut buffer, sender_ref);
            reap(&mut child);
            logging::debug("udevadm monitor 已退出");
        });

        Ok(Self { pid })
    }

    /// 终止子进程（守护进程退出时调用）。
    pub fn stop(&self) {
        if self.pid == 0 {
            return;
        }
        if !platform::terminate_process(self.pid) {
            logging::debug(format!("无法向 udevadm monitor（pid {}）发送终止信号", self.pid));
        }
        std::thread::sleep(Duration::from_millis(300));
        if platform::process_alive(self.pid) && platform::kill_process(self.pid) {
            logging::debug(format!("已强制结束 udevadm monitor（pid {}）", self.pid));
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// 解析并转发缓冲区中的事件；返回 false 表示接收端已关闭。
fn flush_events(buffer: &mut String, sender: &Sender<UdevEvent>) -> bool {
    if buffer.trim().is_empty() {
        buffer.clear();
        return true;
    }
    let events = parse_udevadm_output(buffer);
    buffer.clear();
    for event in events {
        if sender.send(event).is_err() {
            logging::debug("udev 事件接收端已关闭，停止读取");
            return false;
        }
    }
    true
}

fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 无 udev 时按固定间隔对比设备快照，生成 add/remove 事件。
pub struct PollMonitor;

impl PollMonitor {
    pub fn spawn(
        runner: Arc<dyn CommandRunner>,
        interval: Duration,
        sender: Sender<UdevEvent>,
    ) -> Self {
        let interval = if interval.is_zero() {
            DEFAULT_POLL_INTERVAL
        } else {
            interval
        };
        std::thread::spawn(move || {
            logging::debug(format!(
                "启动轮询模式监听块设备，间隔 {} 毫秒",
                interval.as_millis()
            ));
            let mut previous: HashSet<String> = HashSet::new();
            loop {
                if platform::stop_requested() {
                    return;
                }
                let devices = devices::list_devices(&*runner, false, true);
                let current: HashSet<String> =
                    devices.iter().map(|device| device.name.clone()).collect();

                for name in current.difference(&previous) {
                    if sender.send(poll_event("add", name)).is_err() {
                        logging::debug("轮询事件接收端已关闭，停止轮询");
                        return;
                    }
                }
                for name in previous.difference(&current) {
                    if sender.send(poll_event("remove", name)).is_err() {
                        logging::debug("轮询事件接收端已关闭，停止轮询");
                        return;
                    }
                }
                previous = current;

                if !sleep_until(interval) {
                    return;
                }
            }
        });
        Self
    }
}

fn poll_event(action: &str, name: &str) -> UdevEvent {
    UdevEvent {
        action: action.to_string(),
        devname: name.to_string(),
        devpath: String::new(),
        devtype: None,
        properties: BTreeMap::new(),
    }
}

/// 分片睡眠，便于及时响应停止请求；返回 false 表示应退出。
fn sleep_until(interval: Duration) -> bool {
    let mut slept = Duration::ZERO;
    while slept < interval {
        if platform::stop_requested() {
            return false;
        }
        let step = POLL_SLICE.min(interval - slept);
        std::thread::sleep(step);
        slept += step;
    }
    !platform::stop_requested()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::MockRunner;
    use crate::testutil::TempDir;

    const SINGLE_EVENT: &str = "\
UDEV  [1234.5678] add      /devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/host6/target6:0:0/6:0:0:0/block/sdb (block)
ACTION=add
DEVLINKS=/dev/disk/by-id/usb-Kingston
DEVNAME=/dev/sdb
DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/host6/target6:0:0/6:0:0:0/block/sdb
DEVTYPE=disk
MAJOR=8
MINOR=16
SUBSYSTEM=block
";

    #[test]
    fn parses_single_event() {
        let events = parse_udevadm_output(SINGLE_EVENT);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.action, "add");
        assert_eq!(event.devname, "/dev/sdb");
        assert_eq!(event.devtype.as_deref(), Some("disk"));
        assert!(event.devpath.ends_with("/block/sdb"));
        assert_eq!(
            event.properties.get("SUBSYSTEM").map(String::as_str),
            Some("block")
        );
        assert_eq!(event.properties.get("MAJOR").map(String::as_str), Some("8"));
    }

    #[test]
    fn parses_multiple_events_and_filters_non_block() {
        // 两个连续块（中间有空行）+ 一个 input 子系统事件 + 一个 move 事件
        let text = format!(
            "{SINGLE_EVENT}\n\
KERNEL[1234.6000] remove   /devices/pci0000:00/block/sdb/sdb1 (block)
ACTION=remove
DEVNAME=/dev/sdb1
DEVTYPE=partition
SUBSYSTEM=block
\n\
UDEV  [1234.7000] add      /devices/pci0000:00/input/input20 (input)
ACTION=add
DEVNAME=/dev/input/event5
SUBSYSTEM=input
\n\
UDEV  [1234.8000] move     /devices/pci0000:00/block/sdc (block)
ACTION=move
DEVNAME=/dev/sdc
SUBSYSTEM=block
"
        );
        let dir = TempDir::new("udev");
        let path = dir.write("monitor.txt", &text);
        let events = parse_udevadm_output(&std::fs::read_to_string(path).unwrap());
        assert_eq!(events.len(), 2, "只保留 block 的 add/remove/change: {events:?}");
        assert_eq!(events[0].action, "add");
        assert_eq!(events[0].devname, "/dev/sdb");
        assert_eq!(events[1].action, "remove");
        assert_eq!(events[1].devname, "/dev/sdb1");
        assert_eq!(events[1].devtype.as_deref(), Some("partition"));
    }

    #[test]
    fn falls_back_to_devpath_basename_without_devname() {
        let text = "\
KERNEL[99.0001] change   /devices/pci0000:00/block/sdd/sdd2 (block)
ACTION=change
DEVPATH=/devices/pci0000:00/block/sdd/sdd2
SUBSYSTEM=block
";
        let events = parse_udevadm_output(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].action, "change");
        assert_eq!(events[0].devname, "sdd2");
        assert!(events[0].devtype.is_none());

        // 既没有 DEVNAME 也没有 devpath 的块被丢弃
        assert!(
            parse_udevadm_output("UDEV  [1.0] add\nACTION=add\nSUBSYSTEM=block\n").is_empty()
        );
        // 空输入、噪声输入都安全
        assert!(parse_udevadm_output("").is_empty());
        assert!(parse_udevadm_output("monitor will print the received events for:\n").is_empty());
    }

    #[test]
    fn spawn_reports_missing_udevadm() {
        let (sender, _receiver) = std::sync::mpsc::channel();
        let runner = MockRunner::new().missing("udevadm");
        let error = match UdevMonitor::spawn(&runner, sender) {
            Ok(monitor) => panic!("缺少 udevadm 时应报错，却拿到了 pid {}", monitor.pid()),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert!(error.message.contains("udevadm"));
        assert!(error.hint.is_some());
    }

    #[test]
    fn poll_events_carry_device_names() {
        let add = poll_event("add", "sdb1");
        assert_eq!(add.action, "add");
        assert_eq!(add.devname, "sdb1");
        assert!(add.devpath.is_empty());
        assert!(add.devtype.is_none());
        assert!(add.properties.is_empty());

        let remove = poll_event("remove", "sdc");
        assert_eq!(remove.action, "remove");
        assert_eq!(remove.devname, "sdc");
    }
}
