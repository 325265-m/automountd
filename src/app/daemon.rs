//! Daemon: udev event loop, boot-time mounting, control interface.
//!
//! A single-threaded event loop fed by three background producers:
//!
//! * the `udevadm monitor` (or polling) thread pushes [`UdevEvent`]s into a channel;
//! * [`CtlServer`] wraps every control connection as a [`CtlJob`] into another channel;
//! * the main loop debounces and runs mounts/unmounts serially, so concurrent
//!   operations cannot step on each other.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::ctl::{CtlJob, CtlRequest, CtlResponse, CtlServer};
use crate::devices;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::CommandRunner;
use crate::inventory;
use crate::logging;
use crate::model::{BlockDevice, MountRecord, UserInfo, NON_DATA_TYPES};
use crate::mounter::MountManager;
use crate::mountinfo;
use crate::notify;
use crate::platform;
use crate::policy;
use crate::sessions;
use crate::state::State;
use crate::udev::{PollMonitor, UdevEvent, UdevMonitor};

const EVENT_POLL: Duration = Duration::from_millis(300);
const USER_CACHE_TTL: Duration = Duration::from_secs(60);
const ERROR_NOTICE_TTL: Duration = Duration::from_secs(120);
const ADD_RETRY_DELAY: Duration = Duration::from_millis(250);

pub struct Daemon {
    config: Config,
    runner: Arc<dyn CommandRunner>,
    manager: MountManager,
    state: State,
    ctl: CtlServer,
    jobs: Receiver<CtlJob>,
    events: Receiver<UdevEvent>,
    event_tx: Sender<UdevEvent>,
    monitor: Option<UdevMonitor>,
    stop: Arc<AtomicBool>,
    user_cache: Option<(Option<UserInfo>, Instant)>,
    suppressed: HashMap<String, Instant>,
    known: HashSet<String>,
    last_reconcile: Instant,
    last_error_notice: HashMap<String, Instant>,
    started_at: u64,
    /// After a reload one full scan must be caught up on; it runs inside the event loop
    /// so a control request cannot be starved by mount operations.
    pending_reconcile: bool,
}

impl Daemon {
    pub fn new(config: Config, runner: Arc<dyn CommandRunner>) -> Result<Self> {
        config.validate()?;
        let (job_tx, job_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let ctl = CtlServer::new(&config, job_tx);
        let manager = MountManager::new(config.clone(), runner.clone());
        let state = State::new(&config);
        Ok(Self {
            config,
            runner,
            manager,
            state,
            ctl,
            jobs: job_rx,
            events: event_rx,
            event_tx,
            monitor: None,
            stop: Arc::new(AtomicBool::new(false)),
            user_cache: None,
            suppressed: HashMap::new(),
            known: HashSet::new(),
            last_reconcile: Instant::now(),
            last_error_notice: HashMap::new(),
            started_at: platform::now_epoch(),
            pending_reconcile: false,
        })
    }

    /// Run blocking until SIGTERM/SIGINT arrives or the control command shutdown is issued.
    pub fn run(&mut self) -> Result<()> {
        platform::reset_stop();
        platform::install_signal_handlers()?;

        if !self.config.dry_run {
            platform::require_root("启动自动挂载服务")?;
        }

        self.state.prepare()?;
        if let Err(err) = self.state.load() {
            logging::warn(format!("读取上次状态失败，将重新开始: {err}"));
        }
        self.state.pid = std::process::id();
        self.state.started_at = self.started_at;
        self.state.dry_run = self.config.dry_run;
        self.prune_state();
        self.state.save()?;

        self.ctl.start()?;
        self.spawn_monitor()?;

        logging::info(format!(
            "automountd {} 已启动（pid {}，模式 {}，挂载根目录 {}）",
            crate::VERSION,
            std::process::id(),
            self.state.backend,
            self.config.mount_root.display()
        ));

        self.reconcile(true);
        self.last_reconcile = Instant::now();

        let result = self.event_loop();
        self.cleanup();
        result
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        platform::request_stop();
    }

    // ------------------------------------------------------------------ startup
    fn spawn_monitor(&mut self) -> Result<()> {
        match UdevMonitor::spawn(&*self.runner, self.event_tx.clone()) {
            Ok(monitor) => {
                logging::info("使用 udev 监听块设备热插拔事件");
                self.monitor = Some(monitor);
                self.state.backend = "udev".to_string();
            }
            Err(err) => {
                logging::warn(format!("无法使用 udev 事件源（{err}），改用轮询模式"));
                let seconds = if self.config.poll_interval_seconds > 0.0 {
                    self.config.poll_interval_seconds
                } else {
                    5.0
                };
                PollMonitor::spawn(
                    self.runner.clone(),
                    Duration::from_secs_f64(seconds.max(1.0)),
                    self.event_tx.clone(),
                );
                self.state.backend = "poll".to_string();
            }
        }
        self.state.save()
    }

    /// Drop stale records that are no longer in the mount table (e.g. after a reboot
    /// or when a device was unplugged).
    fn prune_state(&mut self) {
        let mounts = mountinfo::read_mountinfo();
        let stale: Vec<String> = self
            .state
            .records()
            .filter(|record| mountinfo::find_mount(&mounts, Some(&record.target), None).is_none())
            .map(|record| record.device.clone())
            .collect();
        for device in stale {
            logging::debug(format!("清理过期挂载记录 {device}"));
            self.state.remove(&device);
        }
    }

    // ------------------------------------------------------------------ loop
    fn event_loop(&mut self) -> Result<()> {
        loop {
            if self.should_stop() {
                break;
            }

            match self.events.recv_timeout(EVENT_POLL) {
                Ok(first) => {
                    let batch = self.collect_batch(first);
                    self.process_batch(batch);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    logging::warn("事件通道已关闭，事件循环退出");
                    break;
                }
            }

            self.drain_jobs();

            if self.pending_reconcile {
                self.pending_reconcile = false;
                self.reconcile(true);
                self.last_reconcile = Instant::now();
            }

            if self.should_stop() {
                break;
            }

            let interval = self.config.reconcile_interval_seconds;
            if interval > 0 && self.last_reconcile.elapsed() >= Duration::from_secs(interval) {
                self.reconcile(false);
                self.last_reconcile = Instant::now();
            }
        }
        Ok(())
    }

    fn should_stop(&self) -> bool {
        platform::stop_requested() || self.stop.load(Ordering::SeqCst)
    }

    /// Collect the events that arrive within a short debounce window.
    fn collect_batch(&mut self, first: UdevEvent) -> Vec<UdevEvent> {
        let mut batch = vec![first];
        let window = self.config.debounce_seconds.clamp(0.0, 5.0);
        let deadline = Instant::now() + Duration::from_secs_f64(window);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self
                .events
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {
                    if batch.len() > 1 {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.drain_jobs();
        }
        batch
    }

    fn process_batch(&mut self, batch: Vec<UdevEvent>) {
        let mut actions: HashMap<String, String> = HashMap::new();
        for event in batch {
            let name = basename(&event.devname);
            if name.is_empty() {
                continue;
            }
            match event.action.as_str() {
                "add" => {
                    actions.insert(name, "add".to_string());
                }
                "remove" => {
                    actions.insert(name, "remove".to_string());
                }
                _ => {}
            }
        }
        for (name, action) in actions {
            if action == "add" {
                self.handle_add(&name);
            } else {
                self.handle_remove(&name);
            }
        }
    }

    fn handle_add(&mut self, name: &str) {
        let Some(mut device) = self.wait_for_device(name) else {
            logging::debug(format!("忽略 {name}：设备已消失或尚未就绪"));
            return;
        };
        if !device.has_filesystem() {
            device = devices::refresh_device(&device, &*self.runner);
        }
        if !device.has_filesystem() {
            logging::debug(format!("忽略 {}：没有可识别的文件系统", device.path));
            return;
        }

        let all = devices::list_devices(&*self.runner, false, true);
        let mounts = mountinfo::read_mountinfo();
        let system_disks = devices::system_disk_names(&all, &mounts);
        let decision = policy::should_automount(&device, &self.config, &system_disks, &mounts);
        if !decision.allowed {
            logging::debug(format!("跳过 {}：{}", device.path, decision.message));
            return;
        }
        if !self.config.auto_mount_on_hotplug {
            logging::debug(format!("自动挂载已关闭，跳过 {}", device.path));
            return;
        }
        if self.is_suppressed(&device.name) {
            logging::info(format!(
                "{} 刚刚被手动卸载，{} 秒内不再自动挂载",
                device.name, self.config.suppress_remount_seconds
            ));
            return;
        }

        let user = self.current_user();
        let _ = self.mount_device(&device, user.as_ref(), "hotplug");
    }

    fn wait_for_device(&self, name: &str) -> Option<BlockDevice> {
        for attempt in 0..5 {
            let all = devices::list_devices(&*self.runner, false, true);
            if let Some(device) = devices::find_by_name(&all, name) {
                if device.has_filesystem() || attempt == 4 {
                    return Some(device);
                }
            }
            std::thread::sleep(ADD_RETRY_DELAY);
        }
        devices::find_by_name(&devices::list_devices(&*self.runner, false, true), name)
    }

    fn handle_remove(&mut self, name: &str) {
        let device_path = format!("/dev/{name}");
        let record = self.state.get(&device_path).cloned().or_else(|| {
            self.state
                .records()
                .find(|record| basename(&record.device) == name)
                .cloned()
        });
        let Some(record) = record else {
            self.suppressed.remove(name);
            return;
        };
        self.known.remove(name);

        let mounts = mountinfo::read_mountinfo();
        if mountinfo::find_mount(&mounts, Some(&record.target), None).is_some() {
            match self.manager.unmount(&record.target, false, false, false) {
                Ok(target) => logging::info(format!("设备移除，已卸载 {target}")),
                Err(err) => {
                    logging::warn(format!(
                        "设备 {} 已移除，但正常卸载 {} 失败（{err}），尝试 lazy 卸载",
                        record.device, record.target
                    ));
                    // The device has physically disappeared, so a lazy unmount can safely
                    // detach the leftover mount from the namespace.
                    match self.manager.unmount(&record.target, true, false, false) {
                        Ok(target) => logging::info(format!("已 lazy 卸载 {target}")),
                        Err(lazy_err) => logging::warn(format!(
                            "lazy 卸载 {} 也失败，保留记录等待下次扫描: {lazy_err}",
                            record.target
                        )),
                    }
                }
            }
        }
        if mountinfo::find_mount(&mountinfo::read_mountinfo(), Some(&record.target), None).is_some()
        {
            logging::warn(format!("{} 仍处于挂载状态，暂不删除记录", record.target));
            return;
        }

        self.state.remove(&record.device);
        let _ = self.state.save();
        self.suppressed.remove(name);
        let user = self.current_user();
        notify::notify_unmounted(&self.config, &*self.runner, &record.target, user.as_ref());
    }

    fn mount_device(
        &mut self,
        device: &BlockDevice,
        user: Option<&UserInfo>,
        reason: &str,
    ) -> Option<crate::model::MountResult> {
        let attempts = self.config.retry_attempts.max(1);
        for attempt in 1..=attempts {
            match self.manager.mount(device, user, false, &[]) {
                Ok(result) => {
                    if result.already_mounted {
                        logging::debug(format!("{} 已经挂载，无需操作", device.path));
                        return Some(result);
                    }
                    let target = result.target.display().to_string();
                    let record = MountRecord {
                        device: device.path.clone(),
                        target: target.clone(),
                        fstype: result.fstype.clone(),
                        driver: result.driver.clone(),
                        uuid: device.uuid.clone(),
                        label: device.label.clone(),
                        owner_uid: user.map(|user| user.uid),
                        reason: reason.to_string(),
                        mounted_at: platform::now_epoch(),
                        options: result.options.clone(),
                    };
                    self.state.add(record);
                    if let Err(err) = self.state.save() {
                        logging::warn(format!("保存状态失败: {err}"));
                    }
                    logging::info(format!(
                        "已自动挂载 {} 到 {}（{}）",
                        device.path, target, result.fstype
                    ));
                    self.known.insert(device.name.clone());
                    notify::notify_mounted(
                        &self.config,
                        &*self.runner,
                        &device.path,
                        &target,
                        user,
                    );
                    return Some(result);
                }
                Err(err) => {
                    let retry = attempt < attempts && is_retryable(&err);
                    logging::warn(format!(
                        "挂载 {} 失败（第 {attempt}/{attempts} 次）: {err}",
                        device.path
                    ));
                    if !retry {
                        self.notify_failure(device, &err, user);
                        return None;
                    }
                    std::thread::sleep(Duration::from_secs_f64(
                        self.config.retry_delay_seconds.max(0.1),
                    ));
                }
            }
        }
        None
    }

    fn notify_failure(&mut self, device: &BlockDevice, err: &Error, user: Option<&UserInfo>) {
        let now = Instant::now();
        if let Some(last) = self.last_error_notice.get(&device.name) {
            if now.duration_since(*last) < ERROR_NOTICE_TTL {
                return;
            }
        }
        self.last_error_notice.insert(device.name.clone(), now);
        let detail = match &err.hint {
            Some(hint) => format!("{}\n{hint}", err.message),
            None => err.message.clone(),
        };
        notify::notify_error(&self.config, &*self.runner, "磁盘挂载失败", &detail, user);
    }

    /// Mount at boot, or scan for devices missed by earlier periodic scans.
    fn reconcile(&mut self, boot: bool) {
        if boot && !self.config.auto_mount_on_boot {
            logging::info("auto_mount_on_boot 已关闭，跳过开机挂载");
        }
        if !boot && !self.config.auto_mount_on_hotplug {
            return;
        }

        let all = devices::list_devices(&*self.runner, false, true);
        let mounts = mountinfo::read_mountinfo();
        let system_disks = devices::system_disk_names(&all, &mounts);
        let user = self.current_user();

        for device in &all {
            let is_new = !self.known.contains(&device.name);
            if !boot && !is_new {
                continue;
            }
            if boot && !self.config.auto_mount_on_boot {
                continue;
            }
            if NON_DATA_TYPES.contains(&device.dev_type.as_str()) || !device.has_filesystem() {
                continue;
            }
            if self.is_suppressed(&device.name) {
                continue;
            }
            let decision = policy::should_automount(device, &self.config, &system_disks, &mounts);
            if !decision.allowed {
                logging::debug(format!("{} 不自动挂载：{}", device.path, decision.message));
                continue;
            }
            let reason = if boot { "boot" } else { "scan" };
            let _ = self.mount_device(device, user.as_ref(), reason);
        }

        self.known = all.iter().map(|device| device.name.clone()).collect();
    }

    // ------------------------------------------------------------------ user
    fn current_user(&mut self) -> Option<UserInfo> {
        let now = Instant::now();
        if let Some((cached, at)) = &self.user_cache {
            if now.duration_since(*at) < USER_CACHE_TTL {
                return cached.clone();
            }
        }
        let user = sessions::console_user(&*self.runner, self.config.user.as_deref());
        self.user_cache = Some((user.clone(), now));
        user
    }

    fn is_suppressed(&mut self, name: &str) -> bool {
        if let Some(until) = self.suppressed.get(name) {
            if *until > Instant::now() {
                return true;
            }
        }
        self.suppressed.remove(name);
        false
    }

    fn suppress(&mut self, name: &str) {
        let seconds = self.config.suppress_remount_seconds;
        if seconds == 0 {
            self.suppressed.remove(name);
        } else {
            self.suppressed.insert(
                name.to_string(),
                Instant::now() + Duration::from_secs(seconds),
            );
        }
    }

    // ------------------------------------------------------------------ control
    fn drain_jobs(&mut self) {
        while let Ok(job) = self.jobs.try_recv() {
            let response = self.dispatch(&job.request, job.peer_uid);
            let _ = job.reply.send(response);
        }
    }

    fn dispatch(&mut self, request: &CtlRequest, peer_uid: u32) -> CtlResponse {
        if let Err(err) = self.authorize(peer_uid, &request.command) {
            return failure_response(&err);
        }

        let result = match request.command.as_str() {
            "ping" => Ok(CtlResponse::success().with_meta("pong", "1")),
            "status" => Ok(self.status_response()),
            "list" => {
                let include_all = request.args.first().map(|v| v == "1").unwrap_or(false);
                self.list_response(include_all)
            }
            "mount" => self.mount_request(request, peer_uid),
            "umount" | "unmount" => self.unmount_request(request, peer_uid),
            "reload" => self.reload_response(),
            "shutdown" => {
                self.request_stop();
                Ok(CtlResponse::success().with_meta("message", "automountd 正在停止"))
            }
            other => Err(Error::protocol(format!("未知控制命令: {other}"))
                .with_hint("请升级命令行工具，或执行 'automountd --help' 查看支持的命令")),
        };

        match result {
            Ok(response) => response,
            Err(err) => failure_response(&err),
        }
    }

    fn authorize(&mut self, uid: u32, command: &str) -> Result<()> {
        if uid == 0 {
            return Ok(());
        }
        if !self.user_allowed(uid) {
            return Err(Error::permission(format!(
                "uid {uid} 不在 ctl_allowed_users 中，无权访问自动挂载服务"
            ))
            .with_hint("请让 root 修改配置中的 ctl_allowed_users"));
        }
        if matches!(command, "shutdown" | "reload") {
            let console_uid = self.current_user().map(|user| user.uid);
            if console_uid != Some(uid) {
                return Err(Error::permission(
                    "只有 root 或当前桌面登录用户可以执行该命令",
                ));
            }
        }
        Ok(())
    }

    fn user_allowed(&self, uid: u32) -> bool {
        if self
            .config
            .ctl_allowed_users
            .iter()
            .any(|entry| entry == "*")
        {
            return true;
        }
        if self
            .config
            .ctl_allowed_users
            .iter()
            .any(|entry| entry.parse::<u32>().ok() == Some(uid))
        {
            return true;
        }
        match sessions::lookup_uid(&*self.runner, uid) {
            Some(user) => self
                .config
                .ctl_allowed_users
                .iter()
                .any(|entry| entry == &user.name),
            None => false,
        }
    }

    fn status_response(&mut self) -> CtlResponse {
        let user = self.current_user();
        let devices = devices::list_devices(&*self.runner, false, true);
        let mut response = CtlResponse::success()
            .with_meta("version", crate::VERSION)
            .with_meta("pid", std::process::id().to_string())
            .with_meta("started_at", self.started_at.to_string())
            .with_meta(
                "uptime_seconds",
                (platform::now_epoch().saturating_sub(self.started_at)).to_string(),
            )
            .with_meta("dry_run", yes_no(self.config.dry_run))
            .with_meta("backend", self.state.backend.clone())
            .with_meta("mount_root", self.config.mount_root.display().to_string())
            .with_meta("managed_mounts", self.state.len().to_string())
            .with_meta("device_count", devices.len().to_string())
            .with_meta(
                "config",
                self.config
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(默认值)".to_string()),
            )
            .with_meta(
                "user",
                user.as_ref()
                    .map(|user| user.display())
                    .unwrap_or_else(|| "未识别".to_string()),
            );
        if self.config.allowed_uuids.is_empty() {
            response = response.with_meta("allowed_uuids", "-");
        } else {
            response = response.with_meta("allowed_uuids", self.config.allowed_uuids.join(","));
        }
        response
    }

    fn list_response(&self, include_all: bool) -> Result<CtlResponse> {
        let views = inventory::snapshot(&self.config, &*self.runner, &self.state, include_all)?;
        let mut response = CtlResponse::success().with_meta("count", views.len().to_string());
        for view in views {
            response = response.with_row(vec![
                view.device.name.clone(),
                view.device.path.clone(),
                view.device.label.clone().unwrap_or_default(),
                view.device.fstype.clone().unwrap_or_default(),
                view.device.human_size(),
                yes_no(view.mounted()).to_string(),
                view.target().unwrap_or("").to_string(),
                view.decision.code.clone(),
                view.decision.message.clone(),
                view.managed
                    .as_ref()
                    .map(|record| record.target.clone())
                    .unwrap_or_default(),
            ]);
        }
        Ok(response)
    }

    fn mount_request(&mut self, request: &CtlRequest, peer_uid: u32) -> Result<CtlResponse> {
        let reference = request
            .args
            .first()
            .map(String::as_str)
            .ok_or_else(|| Error::config("mount 命令缺少设备参数"))?;
        let read_only = request.args.get(1).map(|v| v == "1").unwrap_or(false);
        let force = request.args.get(2).map(|v| v == "1").unwrap_or(false);
        if force && peer_uid != 0 {
            return Err(
                Error::permission("只有 root 可以通过控制接口强制挂载（--force）")
                    .with_hint("普通用户挂载系统盘或忽略策略需要 sudo 直接执行"),
            );
        }

        let all = devices::list_devices(&*self.runner, false, true);
        let device = devices::resolve_device(&all, reference)?;
        let mounts = mountinfo::read_mountinfo();
        if let Some(entry) = mountinfo::find_mount(&mounts, None, Some(&device.path)) {
            return Ok(CtlResponse::success()
                .with_meta("already_mounted", "1")
                .with_meta("target", entry.target.clone())
                .with_meta("fstype", entry.fstype.clone())
                .with_meta("device", device.path.clone()));
        }

        let system_disks = devices::system_disk_names(&all, &mounts);
        let decision = policy::should_mount_manual(&device, &self.config, &system_disks, force);
        if !decision.allowed {
            return Err(Error::mount(decision.message));
        }

        let user = self.current_user();
        let result = self.manager.mount(&device, user.as_ref(), read_only, &[])?;
        if !result.already_mounted {
            let record = MountRecord {
                device: device.path.clone(),
                target: result.target.display().to_string(),
                fstype: result.fstype.clone(),
                driver: result.driver.clone(),
                uuid: device.uuid.clone(),
                label: device.label.clone(),
                owner_uid: user.as_ref().map(|user| user.uid),
                reason: "manual".to_string(),
                mounted_at: platform::now_epoch(),
                options: result.options.clone(),
            };
            self.state.add(record);
            let _ = self.state.save();
            notify::notify_mounted(
                &self.config,
                &*self.runner,
                &device.path,
                &result.target.display().to_string(),
                user.as_ref(),
            );
        }

        Ok(CtlResponse::success()
            .with_meta("device", device.path.clone())
            .with_meta("target", result.target.display().to_string())
            .with_meta("fstype", result.fstype.clone())
            .with_meta("driver", result.driver.clone())
            .with_meta("options", result.options.join(","))
            .with_meta("already_mounted", yes_no(result.already_mounted))
            .with_meta("dry_run", yes_no(result.dry_run)))
    }

    fn unmount_request(&mut self, request: &CtlRequest, peer_uid: u32) -> Result<CtlResponse> {
        let reference = request
            .args
            .first()
            .map(String::as_str)
            .ok_or_else(|| Error::config("umount 命令缺少设备或挂载点参数"))?;
        let lazy = request.args.get(1).map(|v| v == "1").unwrap_or(false);
        let eject = request.args.get(2).map(|v| v == "1").unwrap_or(false);

        let mounts = mountinfo::read_mountinfo();
        let (target, record) =
            if let Some(entry) = mountinfo::find_mount(&mounts, Some(reference), None) {
                (
                    entry.target.clone(),
                    self.state.find_by_target(reference).cloned(),
                )
            } else {
                let all = devices::list_devices(&*self.runner, false, true);
                let device = devices::resolve_device(&all, reference)?;
                let entry = mountinfo::find_by_source(&device.path, &mounts)
                    .first()
                    .map(|entry| entry.target.clone())
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::NotMounted,
                            format!("{} 当前没有挂载", device.path),
                        )
                    })?;
                let record = self.state.find_by_target(&entry).cloned();
                (entry, record)
            };

        if !self.is_safe_target(&target) {
            return Err(Error::permission(format!(
                "拒绝卸载 {target}：只能卸载 automountd 管理的挂载点或 mount_root 下的挂载点"
            ))
            .with_hint("该挂载点可能由 /etc/fstab 或系统管理，请使用 umount 命令手动处理"));
        }

        if peer_uid != 0 {
            let owner = record.as_ref().and_then(|record| record.owner_uid);
            let console_uid = self.current_user().map(|user| user.uid);
            if owner != Some(peer_uid) && console_uid != Some(peer_uid) {
                return Err(Error::permission(format!(
                    "uid {peer_uid} 不能卸载 {target}：不是该挂载的所有者，也不是当前桌面用户"
                ))
                .with_hint("如确需卸载，请使用 sudo"));
            }
        }

        self.manager.unmount(&target, lazy, eject, false)?;
        if let Some(record) = record {
            self.state.remove(&record.device);
            let _ = self.state.save();
            self.suppress(&basename(&record.device));
        }
        let user = self.current_user();
        notify::notify_unmounted(&self.config, &*self.runner, &target, user.as_ref());
        Ok(CtlResponse::success()
            .with_meta("target", target)
            .with_meta("lazy", yes_no(lazy)))
    }

    fn is_safe_target(&self, target: &str) -> bool {
        const CRITICAL: &[&str] = &["/", "/boot", "/boot/efi", "/usr", "/var", "/etc", "/home"];
        let normalized = mountinfo::normalize_path(target);
        if CRITICAL.contains(&normalized.as_str()) {
            return false;
        }
        if self.state.find_by_target(target).is_some() {
            return true;
        }
        let root = mountinfo::normalize_path(&self.config.mount_root.display().to_string());
        let root_prefix = format!("{}/", root.trim_end_matches('/'));
        normalized.starts_with(&root_prefix)
    }

    fn reload_response(&mut self) -> Result<CtlResponse> {
        let new_config = Config::load(self.config.path.as_deref())?;
        let socket_changed = new_config.ctl_socket != self.config.ctl_socket;
        let root_changed = new_config.mount_root != self.config.mount_root;
        self.config = new_config;
        self.manager = MountManager::new(self.config.clone(), self.runner.clone());
        self.state.dry_run = self.config.dry_run;
        self.user_cache = None;
        // The config may now allow new devices or internal disks: clear the known set so
        // the event loop catches up with one full scan on its next round. Do not mount
        // synchronously inside this control request -- a slow disk would make the CLI time
        // out and block status.
        self.known.clear();
        self.pending_reconcile = true;
        let job_timeout = self
            .config
            .mount_timeout_seconds
            .max(self.config.unmount_timeout_seconds)
            .saturating_add(20);
        self.ctl.set_job_timeout(Duration::from_secs(job_timeout));
        self.state.save()?;
        logging::info("配置已重新加载");
        Ok(CtlResponse::success()
            .with_meta("reloaded", "1")
            .with_meta("socket_changed", yes_no(socket_changed))
            .with_meta("mount_root_changed", yes_no(root_changed)))
    }

    fn cleanup(&mut self) {
        if let Some(monitor) = &self.monitor {
            monitor.stop();
        }
        self.ctl.stop();
        if let Err(err) = self.state.save() {
            logging::warn(format!("退出前保存状态失败: {err}"));
        }
        logging::info("automountd 已停止");
    }
}

fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

fn is_retryable(err: &Error) -> bool {
    !matches!(
        err.kind,
        ErrorKind::Permission | ErrorKind::Unsupported | ErrorKind::Config | ErrorKind::Device
    )
}

fn failure_response(err: &Error) -> CtlResponse {
    let mut message = err.message.clone();
    if let Some(hint) = &err.hint {
        message.push_str("\n提示: ");
        message.push_str(hint);
    }
    CtlResponse::failure(kind_code(err.kind), &message)
}

fn kind_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Config => "config",
        ErrorKind::Io => "io",
        ErrorKind::Command => "command",
        ErrorKind::Device => "device",
        ErrorKind::Mount => "mount",
        ErrorKind::NotMounted => "not-mounted",
        ErrorKind::Busy => "busy",
        ErrorKind::Permission => "permission",
        ErrorKind::NotRunning => "not-running",
        ErrorKind::Protocol => "protocol",
        ErrorKind::Unsupported => "unsupported",
    }
}

/// Lets the CLI decide whether this error is worth retrying.
pub fn should_retry(err: &Error) -> bool {
    is_retryable(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::MockRunner;

    fn test_config() -> Config {
        let temp = crate::testutil::TempDir::new("daemon-test");
        let path = temp.path().to_path_buf();
        let mut config = Config::default();
        config.mount_root = path.join("media");
        config.state_dir = path.join("state");
        config.ctl_socket = path.join("control.sock");
        config.notify = false;
        config
    }

    #[test]
    fn responds_to_ping_and_status() {
        let config = test_config();
        let mut daemon = Daemon::new(config, Arc::new(MockRunner::new())).unwrap();
        let ping = daemon.dispatch(&CtlRequest::new("ping"), 0);
        assert!(ping.ok);
        let status = daemon.dispatch(&CtlRequest::new("status"), 0);
        assert!(status.ok);
        assert_eq!(status.meta_value("dry_run"), Some("0"));
        assert!(status.meta_value("version").is_some());
    }

    #[test]
    fn rejects_unknown_command() {
        let config = test_config();
        let mut daemon = Daemon::new(config, Arc::new(MockRunner::new())).unwrap();
        let response = daemon.dispatch(&CtlRequest::new("explode"), 0);
        assert!(!response.ok);
        assert_eq!(response.code, "protocol");
    }

    #[test]
    fn authorization_respects_allowlist() {
        let mut config = test_config();
        config.ctl_allowed_users = vec!["1000".to_string()];
        let runner = Arc::new(MockRunner::new().missing("getent"));
        let mut daemon = Daemon::new(config, runner).unwrap();
        assert!(daemon.authorize(0, "shutdown").is_ok());
        assert!(daemon.authorize(1000, "status").is_ok());
        assert!(daemon.authorize(1001, "status").is_err());
    }

    #[test]
    fn brackets_are_not_safe_targets() {
        let config = test_config();
        let daemon = Daemon::new(config, Arc::new(MockRunner::new())).unwrap();
        assert!(!daemon.is_safe_target("/"));
        assert!(!daemon.is_safe_target("/boot"));
        assert!(!daemon.is_safe_target("/home"));
    }

    #[test]
    fn non_root_cannot_force_mount() {
        let config = test_config();
        let mut daemon = Daemon::new(config, Arc::new(MockRunner::new())).unwrap();
        let request = CtlRequest::new("mount").arg("/dev/sdb1").arg("0").arg("1");
        let response = daemon.dispatch(&request, 1000);
        assert!(!response.ok);
        assert_eq!(response.code, "permission");
    }

    #[test]
    fn basename_handles_dev_prefix() {
        assert_eq!(basename("/dev/sdb1"), "sdb1");
        assert_eq!(basename("sdb1"), "sdb1");
        assert_eq!(basename("/dev/"), "dev");
    }
}
