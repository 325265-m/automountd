//! 命令行入口与子命令分发。
//!
//! 参数解析为手写实现（不依赖 clap）：全局选项只能出现在子命令之前，
//! 子命令选项支持 `--name value`、`--name=value` 与单字母短选项。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, SYSTEM_CONFIG};
use crate::ctl::{self, CtlRequest, CtlResponse};
use crate::daemon::Daemon;
use crate::devices;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{CommandRunner, SystemRunner};
use crate::fstab::{self, FstabEntry};
use crate::fstypes;
use crate::inventory;
use crate::logging::{self, Level};
use crate::model::{DeviceView, MountRecord, MountResult};
use crate::mounter::MountManager;
use crate::mountinfo;
use crate::platform;
use crate::policy;
use crate::sessions;
use crate::state;
use crate::term::{self, Colors};

const VERSION: &str = crate::VERSION;

/// 进程入口：打印错误与提示，返回退出码。
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run_with_args(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误: {err}");
            if let Some(hint) = &err.hint {
                eprintln!("提示: {hint}");
            }
            err.exit_code()
        }
    }
}

pub fn run_with_args(args: &[String]) -> Result<i32> {
    let (global, rest) = parse_global(args)?;
    if global.help {
        print_help();
        return Ok(0);
    }
    if global.version {
        print_version();
        return Ok(0);
    }
    if rest.is_empty() {
        print_help();
        return Ok(0);
    }

    let command = rest[0].as_str();
    let tail = &rest[1..];

    if command == "help" {
        print_help();
        return Ok(0);
    }

    // config init 要在配置文件尚不存在时工作，不能先走常规加载流程。
    let initializing = command == "config" && tail.first().map(String::as_str) == Some("init");
    let mut config = if initializing {
        let mut defaults = Config::default();
        defaults.path = global.config.clone();
        defaults
    } else {
        Config::load(global.config.as_deref())?
    };
    if global.dry_run {
        config.dry_run = true;
    }
    let level = if global.verbose > 0 {
        Level::Debug
    } else {
        Level::parse(&config.log_level)
    };
    logging::init(level, global.quiet);

    let runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    match command {
        "daemon" => cmd_daemon(config, runner, tail),
        "status" => cmd_status(&config, tail),
        "list" | "ls" => cmd_list(&config, runner, tail),
        "mount" | "m" => cmd_mount(&config, runner, tail),
        "umount" | "unmount" | "u" => cmd_umount(&config, runner, tail),
        "reload" => cmd_reload(&config, tail),
        "config" => cmd_config(&global, tail),
        "fstab" => cmd_fstab(&config, runner, tail),
        "doctor" | "check" => cmd_doctor(&config, runner, tail),
        "tui" | "ui" => {
            if wants_help(tail) {
                println!("用法: automountd tui");
                return Ok(0);
            }
            crate::tui::run(&config)?;
            Ok(0)
        }
        "version" => {
            print_version();
            Ok(0)
        }
        other => Err(Error::config(format!("未知命令: {other}"))
            .with_hint("运行 'automountd --help' 查看所有命令")),
    }
}

// ---------------------------------------------------------------------- 全局参数
#[derive(Debug, Default)]
struct Global {
    config: Option<PathBuf>,
    verbose: u8,
    quiet: bool,
    dry_run: bool,
    help: bool,
    version: bool,
}

fn parse_global(args: &[String]) -> Result<(Global, Vec<String>)> {
    let mut global = Global::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            index += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        match arg {
            "-h" | "--help" => global.help = true,
            "-V" | "--version" => global.version = true,
            "-q" | "--quiet" => global.quiet = true,
            "--dry-run" => global.dry_run = true,
            "-v" | "--verbose" => global.verbose = global.verbose.saturating_add(1),
            "-c" | "--config" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    Error::config("--config 需要一个配置文件路径")
                        .with_hint("例如: --config ./config.conf")
                })?;
                global.config = Some(PathBuf::from(value));
            }
            other if other.starts_with("--config=") => {
                global.config = Some(PathBuf::from(&other["--config=".len()..]));
            }
            other => {
                return Err(Error::config(format!("未知全局选项: {other}"))
                    .with_hint("运行 'automountd --help' 查看用法"));
            }
        }
        index += 1;
    }
    Ok((global, args[index..].to_vec()))
}

// ---------------------------------------------------------------------- 子命令参数
struct OptSpec {
    long: &'static str,
    short: Option<char>,
    takes_value: bool,
}

#[derive(Debug, Default)]
struct Parsed {
    flags: HashSet<String>,
    values: HashMap<String, String>,
    positionals: Vec<String>,
}

impl Parsed {
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn positional(&self, index: usize) -> Option<&str> {
        self.positionals.get(index).map(String::as_str)
    }
}

fn parse_opts(args: &[String], specs: &[OptSpec]) -> Result<Parsed> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            parsed.positionals.extend(args[index + 1..].iter().cloned());
            break;
        }
        if let Some(body) = arg.strip_prefix("--") {
            if body.is_empty() {
                return Err(Error::config("遇到无法识别的 '--'"));
            }
            let (name, inline) = match body.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (body, None),
            };
            let spec = specs
                .iter()
                .find(|spec| spec.long == name)
                .ok_or_else(|| Error::config(format!("未知选项 --{name}")))?;
            if spec.takes_value {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        args.get(index)
                            .cloned()
                            .ok_or_else(|| Error::config(format!("--{name} 需要一个值")))?
                    }
                };
                parsed.values.insert(name.to_string(), value);
            } else {
                if inline.is_some() {
                    return Err(Error::config(format!("--{name} 不接受值")));
                }
                parsed.flags.insert(name.to_string());
            }
            index += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            for ch in arg[1..].chars() {
                let spec = specs
                    .iter()
                    .find(|spec| spec.short == Some(ch))
                    .ok_or_else(|| Error::config(format!("未知选项 -{ch}")))?;
                if spec.takes_value {
                    return Err(Error::config(format!(
                        "选项 -{ch} 需要参数，请使用 --{} <值>",
                        spec.long
                    )));
                }
                parsed.flags.insert(spec.long.to_string());
            }
            index += 1;
            continue;
        }
        parsed.positionals.push(arg.to_string());
        index += 1;
    }
    Ok(parsed)
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "-h" || arg == "--help")
}

// ---------------------------------------------------------------------- daemon
fn cmd_daemon(config: Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: sudo automountd daemon [--dry-run]");
        println!();
        println!("在前台运行自动挂载守护进程（通常由 systemd 启动）。");
        return Ok(0);
    }
    if !tail.is_empty() {
        return Err(Error::config(format!(
            "daemon 命令不接受参数: {}",
            tail.join(" ")
        )));
    }
    let mut daemon = Daemon::new(config, runner)?;
    daemon.run()?;
    Ok(0)
}

// ---------------------------------------------------------------------- status
fn cmd_status(config: &Config, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: automountd status");
        return Ok(0);
    }
    let colors = Colors::auto();
    if ctl::is_running(&config.ctl_socket) {
        let response = ensure_ok(ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("status"),
            Duration::from_secs(10),
        )?)?;
        println!("{}", colors.bold("automountd 状态"));
        for (key, value) in &response.meta {
            println!("  {:<14} {}", meta_label(key), display_meta(key, value));
        }
        println!("  {:<14} {}", "控制接口", config.ctl_socket.display());
        return Ok(0);
    }

    let state = state::read_state(config);
    println!("{}", colors.bold("automountd 状态"));
    println!(
        "  服务          {}（控制接口 {} 无响应）",
        colors.yellow("未运行"),
        config.ctl_socket.display()
    );
    if state.started_at > 0 {
        println!("  上次运行      pid {}", state.pid);
        println!(
            "  上次启动      {}",
            term::format_timestamp(state.started_at)
        );
    }
    if !state.is_empty() {
        println!(
            "  遗留挂载记录  {} 条（设备仍插着时服务重启会自动接管）",
            state.len()
        );
    }
    Ok(3)
}

fn meta_label(key: &str) -> &'static str {
    match key {
        "version" => "版本",
        "pid" => "进程号",
        "started_at" => "启动时间",
        "uptime_seconds" => "运行时长",
        "dry_run" => "dry-run",
        "backend" => "事件源",
        "mount_root" => "挂载根目录",
        "managed_mounts" => "托管挂载",
        "device_count" => "设备数量",
        "config" => "配置文件",
        "user" => "桌面用户",
        "allowed_uuids" => "UUID 白名单",
        "count" => "数量",
        _ => "",
    }
}

fn display_meta(key: &str, value: &str) -> String {
    match key {
        "started_at" => value
            .parse::<u64>()
            .map(term::format_timestamp)
            .unwrap_or_else(|_| value.to_string()),
        "uptime_seconds" => value
            .parse::<u64>()
            .map(human_duration)
            .unwrap_or_else(|_| value.to_string()),
        "dry_run" => yes_no_text(value),
        _ => value.to_string(),
    }
}

fn human_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分 {seconds} 秒")
    } else {
        format!("{seconds} 秒")
    }
}

fn yes_no_text(value: &str) -> String {
    match value {
        "1" | "true" | "yes" => "是".to_string(),
        "0" | "false" | "no" => "否".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------- list
fn cmd_list(config: &Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: automountd list [--all]");
        println!();
        println!("  默认只显示可挂载/已挂载的设备；--all 显示全部块设备。");
        return Ok(0);
    }
    let specs = [OptSpec {
        long: "all",
        short: Some('a'),
        takes_value: false,
    }];
    let opts = parse_opts(tail, &specs)?;
    let include_all = opts.flag("all");

    let rows = if ctl::is_running(&config.ctl_socket) {
        let response = ensure_ok(ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("list").arg(if include_all { "1" } else { "0" }),
            Duration::from_secs(15),
        )?)?;
        response.rows
    } else {
        let state = state::read_state(config);
        let views = inventory::snapshot(config, &*runner, &state, include_all)?;
        views.iter().map(view_row).collect()
    };

    print_rows(&rows, include_all);
    Ok(0)
}

pub(crate) fn view_row(view: &DeviceView) -> Vec<String> {
    vec![
        view.device.name.clone(),
        view.device.path.clone(),
        view.device.label.clone().unwrap_or_default(),
        view.device.fstype.clone().unwrap_or_default(),
        view.device.human_size(),
        if view.mounted() { "1" } else { "0" }.to_string(),
        view.target().unwrap_or("").to_string(),
        view.decision.code.clone(),
        view.decision.message.clone(),
        view.managed
            .as_ref()
            .map(|record| record.target.clone())
            .unwrap_or_default(),
    ]
}

fn print_rows(rows: &[Vec<String>], include_all: bool) {
    let colors = Colors::auto();
    if rows.is_empty() {
        println!("未发现设备。若刚插入磁盘，可稍等 1~2 秒再执行。");
        return;
    }
    let mut table_rows = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() < 9 {
            continue;
        }
        let mounted = row[5] == "1";
        let status = if mounted {
            colors.green("已挂载")
        } else if row[7] == "ok" {
            colors.cyan("可挂载")
        } else {
            colors.dim(&term::truncate(&row[8], 26))
        };
        let mut cells = vec![
            row[0].clone(),
            term::truncate(&row[2], 20),
            if row[3].is_empty() {
                "-".to_string()
            } else {
                row[3].clone()
            },
            row[4].clone(),
            status,
            row[6].clone(),
        ];
        if include_all {
            cells.insert(1, row[1].clone());
        }
        table_rows.push(cells);
    }
    let mut headers = vec!["设备", "卷标", "文件系统", "容量", "状态", "挂载点"];
    if include_all {
        headers.insert(1, "路径");
    }
    print!("{}", term::table(&headers, &table_rows));
    if !include_all {
        println!("提示: 使用 --all 可查看未格式化、已加密或被策略排除的设备。");
    }
}

// ---------------------------------------------------------------------- mount
fn cmd_mount(config: &Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: automountd mount <设备|UUID|卷标> [选项]");
        println!();
        println!("选项:");
        println!("  -r, --read-only  只读挂载");
        println!("  -f, --force      跳过系统盘/忽略列表检查");
        println!("      --local      不经过守护进程，直接挂载（需要 root）");
        return Ok(0);
    }
    let specs = [
        OptSpec {
            long: "read-only",
            short: Some('r'),
            takes_value: false,
        },
        OptSpec {
            long: "force",
            short: Some('f'),
            takes_value: false,
        },
        OptSpec {
            long: "local",
            short: None,
            takes_value: false,
        },
    ];
    let opts = parse_opts(tail, &specs)?;
    let reference = opts
        .positional(0)
        .ok_or_else(|| Error::config("请指定要挂载的设备，例如: automountd mount /dev/sdb1"))?;
    let read_only = opts.flag("read-only");
    let force = opts.flag("force");
    // dry-run 必须留在本地执行，否则会把请求交给真正执行挂载的守护进程。
    let use_daemon = !config.dry_run && ctl::is_running(&config.ctl_socket) && !opts.flag("local");

    if use_daemon {
        let response = ensure_ok(ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("mount")
                .arg(reference)
                .arg(if read_only { "1" } else { "0" })
                .arg(if force { "1" } else { "0" }),
            Duration::from_secs(config.mount_timeout_seconds.saturating_add(30)),
        )?)?;
        let target = response.meta_value("target").unwrap_or("?");
        if response.meta_value("already_mounted") == Some("1") {
            println!("{reference} 已经挂载在 {target}");
        } else if response.meta_value("dry_run") == Some("1") {
            println!("dry-run：将把 {reference} 挂载到 {target}");
        } else {
            println!("已挂载 {reference} -> {target}");
        }
        return Ok(0);
    }

    if config.dry_run {
        println!("提示: dry-run 模式，只打印命令");
    }
    let result = local_mount(config, runner, reference, read_only, force)?;
    if result.already_mounted {
        println!("{} 已经挂载在 {}", reference, result.target.display());
    } else if result.dry_run {
        println!(
            "dry-run：将把 {} 挂载到 {}（未执行）",
            result.device.path,
            result.target.display()
        );
    } else {
        println!(
            "已挂载 {} -> {}",
            result.device.path,
            result.target.display()
        );
        println!("  文件系统: {}（驱动 {}）", result.fstype, result.driver);
        println!("  选项: {}", result.options.join(","));
    }
    Ok(0)
}

pub(crate) fn local_mount(
    config: &Config,
    runner: Arc<dyn CommandRunner>,
    reference: &str,
    read_only: bool,
    force: bool,
) -> Result<MountResult> {
    let all = devices::list_devices(&*runner, false, true);
    let device = devices::resolve_device(&all, reference)?;
    let mounts = mountinfo::read_mountinfo();
    let system_disks = devices::system_disk_names(&all, &mounts);
    let decision = policy::should_mount_manual(&device, config, &system_disks, force);
    if !decision.allowed {
        return Err(Error::mount(decision.message));
    }
    let user = sessions::console_user(&*runner, config.user.as_deref());
    let mut manager = MountManager::new(config.clone(), runner);
    let result = manager.mount(&device, user.as_ref(), read_only, &[])?;
    if result.dry_run || result.already_mounted {
        return Ok(result);
    }
    {
        let mut state = state::read_state(config);
        state.add(MountRecord {
            device: device.path.clone(),
            target: result.target.display().to_string(),
            fstype: result.fstype.clone(),
            driver: result.driver.clone(),
            uuid: device.uuid.clone(),
            label: device.label.clone(),
            owner_uid: user.map(|user| user.uid),
            reason: "manual".to_string(),
            mounted_at: platform::now_epoch(),
            options: result.options.clone(),
        });
        if let Err(err) = state.save() {
            logging::warn(format!("记录挂载状态失败: {err}"));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------- umount
fn cmd_umount(config: &Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: automountd umount <设备|挂载点> [选项]");
        println!();
        println!("选项:");
        println!("  -l, --lazy   忙碌时使用 umount -l（lazy）");
        println!("  -e, --eject  卸载后弹出设备");
        println!("      --local  不经过守护进程，直接卸载（需要 root）");
        return Ok(0);
    }
    let specs = [
        OptSpec {
            long: "lazy",
            short: Some('l'),
            takes_value: false,
        },
        OptSpec {
            long: "eject",
            short: Some('e'),
            takes_value: false,
        },
        OptSpec {
            long: "local",
            short: None,
            takes_value: false,
        },
    ];
    let opts = parse_opts(tail, &specs)?;
    let reference = opts
        .positional(0)
        .ok_or_else(|| Error::config("请指定要卸载的设备或挂载点"))?;
    let lazy = opts.flag("lazy");
    let eject = opts.flag("eject");
    let use_daemon = !config.dry_run && ctl::is_running(&config.ctl_socket) && !opts.flag("local");

    if use_daemon {
        let response = ensure_ok(ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("umount")
                .arg(reference)
                .arg(if lazy { "1" } else { "0" })
                .arg(if eject { "1" } else { "0" }),
            Duration::from_secs(
                config
                    .mount_timeout_seconds
                    .max(config.unmount_timeout_seconds)
                    .saturating_add(30),
            ),
        )?)?;
        println!(
            "已卸载 {}",
            response.meta_value("target").unwrap_or(reference)
        );
        return Ok(0);
    }

    let target = local_umount(config, runner, reference, lazy, eject)?;
    println!("已卸载 {target}");
    Ok(0)
}

pub(crate) fn local_umount(
    config: &Config,
    runner: Arc<dyn CommandRunner>,
    reference: &str,
    lazy: bool,
    eject: bool,
) -> Result<String> {
    let mounts = mountinfo::read_mountinfo();
    let state_snapshot = state::read_state(config);
    let target = if reference.starts_with('/') {
        mountinfo::find_mount(&mounts, Some(reference), None)
            .map(|entry| entry.target.clone())
            .ok_or_else(|| Error::new(ErrorKind::NotMounted, format!("{reference} 当前没有挂载")))?
    } else {
        let all = devices::list_devices(&*runner, false, true);
        let device = devices::resolve_device(&all, reference)?;
        mountinfo::find_by_source(&device.path, &mounts)
            .first()
            .map(|entry| entry.target.clone())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotMounted,
                    format!("{} 当前没有挂载", device.path),
                )
            })?
    };

    if ctl::is_running(&config.ctl_socket) && !config.dry_run {
        return Err(Error::permission(
            "守护进程正在运行，请去掉 --local（由服务执行卸载）",
        ));
    }

    let root = mountinfo::normalize_path(&config.mount_root.display().to_string());
    let normalized = mountinfo::normalize_path(&target);
    let under_root = normalized.starts_with(&format!("{}/", root.trim_end_matches('/')));
    let managed = state_snapshot.find_by_target(&target).is_some();
    if !under_root && !managed {
        return Err(Error::permission(format!(
            "拒绝卸载 {target}: 它不是 automountd 管理的挂载点"
        ))
        .with_hint("系统挂载点请使用 umount 命令手动处理"));
    }

    if !config.dry_run {
        platform::require_root("卸载")?;
    }
    let mut manager = MountManager::new(config.clone(), runner);
    let unmounted = manager.unmount(&target, lazy, eject, false)?;
    if config.dry_run {
        return Ok(unmounted);
    }

    let mut state = state::read_state(config);
    if let Some(record) = state.find_by_target(&unmounted).cloned() {
        state.remove(&record.device);
        if let Err(err) = state.save() {
            logging::warn(format!("更新状态失败: {err}"));
        }
    }
    Ok(unmounted)
}

// ---------------------------------------------------------------------- reload
fn cmd_reload(config: &Config, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: automountd reload");
        return Ok(0);
    }
    if config.dry_run {
        println!("dry-run：将重新加载配置（未联系守护进程）");
        return Ok(0);
    }
    if !ctl::is_running(&config.ctl_socket) {
        return Err(Error::new(ErrorKind::NotRunning, "自动挂载服务未运行")
            .with_hint("请先启动: sudo systemctl start automountd"));
    }
    let timeout = config
        .mount_timeout_seconds
        .max(config.unmount_timeout_seconds)
        .saturating_add(30);
    let response = ensure_ok(ctl::request(
        &config.ctl_socket,
        &CtlRequest::new("reload"),
        Duration::from_secs(timeout),
    )?)?;
    println!("配置已重新加载");
    if response.meta_value("socket_changed") == Some("1") {
        println!("提示: ctl_socket 已改变，需要重启服务才能生效");
    }
    if response.meta_value("mount_root_changed") == Some("1") {
        println!("提示: mount_root 已改变，新挂载将使用新目录");
    }
    Ok(0)
}

// ---------------------------------------------------------------------- config
fn cmd_config(global: &Global, tail: &[String]) -> Result<i32> {
    if tail.is_empty() || wants_help(tail) {
        println!("用法: automountd config <show|path|init|validate>");
        println!();
        println!("  show      打印当前生效配置");
        println!("  path      打印配置文件路径");
        println!("  init      生成默认配置文件（--force 覆盖）");
        println!("  validate  校验配置文件");
        return Ok(0);
    }
    match tail[0].as_str() {
        "show" => {
            let config = Config::load(global.config.as_deref())?;
            print!("{}", config.to_conf());
            Ok(0)
        }
        "path" => {
            let config = Config::load(global.config.as_deref())?;
            match &config.path {
                Some(path) => println!("{}", path.display()),
                None => {
                    println!("(未找到配置文件，使用默认值)");
                    println!("默认路径: {SYSTEM_CONFIG}");
                }
            }
            Ok(0)
        }
        "init" => {
            let specs = [OptSpec {
                long: "force",
                short: Some('f'),
                takes_value: false,
            }];
            let opts = parse_opts(&tail[1..], &specs)?;
            let path = global
                .config
                .clone()
                .unwrap_or_else(|| PathBuf::from(SYSTEM_CONFIG));
            if path.exists() && !opts.flag("force") {
                return Err(Error::config(format!("配置文件已存在: {}", path.display()))
                    .with_hint("如需覆盖请加 --force（会保留 .bak 备份由你自行负责）"));
            }
            let config = Config::default();
            config.save(&path)?;
            println!("已写入默认配置: {}", path.display());
            println!("修改后执行: sudo systemctl restart automountd");
            Ok(0)
        }
        "validate" => {
            let config = Config::load(global.config.as_deref())?;
            config.validate()?;
            println!("配置有效");
            if let Some(path) = &config.path {
                println!("  文件: {}", path.display());
            }
            Ok(0)
        }
        other => Err(Error::config(format!("未知 config 子命令: {other}"))),
    }
}

// ---------------------------------------------------------------------- fstab
fn cmd_fstab(config: &Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    if tail.is_empty() || wants_help(tail) {
        println!("用法: automountd fstab <list|add|remove>");
        println!();
        println!("  list                                  列出 /etc/fstab 记录");
        println!("  add <设备|UUID|卷标> [--target 目录]   写入开机自动挂载");
        println!("  remove <设备|UUID|卷标> [--target 目录] 删除记录");
        return Ok(0);
    }
    match tail[0].as_str() {
        "list" => {
            let entries = fstab::load_fstab(&config.fstab_path)?;
            if entries.is_empty() {
                println!("{} 中没有有效记录", config.fstab_path.display());
                return Ok(0);
            }
            let rows: Vec<Vec<String>> = entries
                .iter()
                .map(|entry| {
                    vec![
                        entry.source.clone(),
                        entry.target.clone(),
                        entry.fstype.clone(),
                        entry.options.join(","),
                    ]
                })
                .collect();
            print!(
                "{}",
                term::table(&["设备", "挂载点", "文件系统", "选项"], &rows)
            );
            Ok(0)
        }
        "add" => fstab_add(config, runner, &tail[1..]),
        "remove" => fstab_remove(config, &tail[1..]),
        other => Err(Error::config(format!("未知 fstab 子命令: {other}"))),
    }
}

fn fstab_add(config: &Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    let specs = [
        OptSpec {
            long: "target",
            short: Some('t'),
            takes_value: true,
        },
        OptSpec {
            long: "options",
            short: Some('o'),
            takes_value: true,
        },
        OptSpec {
            long: "read-only",
            short: Some('r'),
            takes_value: false,
        },
    ];
    let opts = parse_opts(tail, &specs)?;
    let reference = opts
        .positional(0)
        .ok_or_else(|| Error::config("请指定要持久化的设备"))?;
    if !config.persist_enabled {
        return Err(Error::config("persist_enabled = false，已禁止写入 fstab"));
    }

    let all = devices::list_devices(&*runner, false, true);
    let device = devices::resolve_device(&all, reference)?;
    let Some(fstype) = device.fstype.clone() else {
        return Err(Error::device(format!("{} 没有文件系统", device.path)));
    };
    let supported = fstypes::read_proc_filesystems();
    let driver = fstypes::choose_driver(&fstype, &supported)?;
    let user = sessions::console_user(&*runner, config.user.as_deref());
    let mounts = mountinfo::read_mountinfo();
    let manager = MountManager::new(config.clone(), runner.clone());

    let target = match opts.value("target") {
        Some(path) => PathBuf::from(path),
        None => manager.target_path(&device, user.as_ref(), &mounts)?,
    };
    validate_fstab_target(config, &target)?;

    let system_disks = devices::system_disk_names(&all, &mounts);
    let decision = policy::should_mount_manual(&device, config, &system_disks, false);
    if !decision.allowed {
        return Err(Error::mount(decision.message));
    }

    let extra: Vec<String> = opts
        .value("options")
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mut options = fstypes::build_options(
        config,
        &fstype,
        &driver,
        user.as_ref(),
        opts.flag("read-only"),
        &extra,
        device.label.as_deref(),
    );

    let entry = FstabEntry {
        source: match &device.uuid {
            Some(uuid) => format!("UUID={uuid}"),
            None => device.path.clone(),
        },
        target: target.display().to_string(),
        fstype: fstype.clone(),
        options: std::mem::take(&mut options),
        dump: "0".to_string(),
        passno: if matches!(fstype.as_str(), "ext2" | "ext3" | "ext4" | "xfs" | "btrfs") {
            "2".to_string()
        } else {
            "0".to_string()
        },
        line_no: 0,
        raw: String::new(),
    };

    if config.dry_run {
        println!("dry-run：将向 {} 写入:", config.fstab_path.display());
        println!("  {}", entry.to_line());
        return Ok(0);
    }
    platform::require_root("写入 fstab")?;
    let (changed, message) = fstab::add_entry(&config.fstab_path, &entry)?;
    println!("{message}");
    if changed {
        println!(
            "提示: 立即挂载可执行 'sudo systemctl daemon-reload' 后重启服务，或直接插拔设备。"
        );
    }
    Ok(0)
}

fn validate_fstab_target(config: &Config, target: &std::path::Path) -> Result<()> {
    let text = target.to_string_lossy();
    if !target.is_absolute() && !text.starts_with('/') {
        return Err(Error::config(format!("fstab 挂载点必须是绝对路径: {text}")));
    }
    if text.contains('\n') || text.contains('\r') {
        return Err(Error::config("fstab 挂载点不能包含换行符"));
    }

    let mut parts: Vec<&str> = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(Error::config(format!("fstab 挂载点不能包含 ..: {text}"))),
            other => parts.push(other),
        }
    }
    let normalized = format!("/{}", parts.join("/"));
    if normalized == "/" {
        return Err(Error::config("fstab 挂载点不能是 /"));
    }

    let root = config.mount_root.to_string_lossy();
    let mut root_parts: Vec<&str> = Vec::new();
    for part in root.split('/') {
        if !part.is_empty() && part != "." {
            root_parts.push(part);
        }
    }
    let root_prefix = format!("/{}/", root_parts.join("/"));
    let root_normalized = format!("/{}", root_parts.join("/"));
    if normalized == root_normalized {
        return Err(Error::config("fstab 挂载点不能等于挂载根目录本身"));
    }
    if !format!("{normalized}/").starts_with(&root_prefix) {
        return Err(Error::config(format!(
            "fstab 挂载点必须位于 mount_root（{}）之下，当前为 {normalized}",
            config.mount_root.display()
        ))
        .with_hint("如需挂到其他位置，请先修改配置中的 mount_root"));
    }
    Ok(())
}

fn fstab_remove(config: &Config, tail: &[String]) -> Result<i32> {
    let specs = [OptSpec {
        long: "target",
        short: Some('t'),
        takes_value: true,
    }];
    let opts = parse_opts(tail, &specs)?;
    let reference = opts.positional(0).map(str::to_string);
    let target = opts.value("target").map(str::to_string);
    if reference.is_none() && target.is_none() {
        return Err(Error::config("请指定设备/UUID，或使用 --target <挂载点>"));
    }

    if config.dry_run {
        println!(
            "dry-run：将从 {} 删除 source={:?} target={:?}",
            config.fstab_path.display(),
            reference,
            target
        );
        return Ok(0);
    }
    platform::require_root("修改 fstab")?;

    let source = reference;
    let (changed, message) =
        fstab::remove_entry(&config.fstab_path, source.as_deref(), target.as_deref())?;
    println!("{message}");
    if !changed {
        return Ok(1);
    }
    Ok(0)
}

// ---------------------------------------------------------------------- doctor
fn cmd_doctor(config: &Config, runner: Arc<dyn CommandRunner>, tail: &[String]) -> Result<i32> {
    if wants_help(tail) {
        println!("用法: automountd doctor");
        return Ok(0);
    }
    let colors = Colors::auto();
    let mut checks: Vec<(String, bool, bool, String)> = Vec::new();

    for program in [
        "lsblk",
        "blkid",
        "mount",
        "umount",
        "udevadm",
        "systemctl",
        "loginctl",
        "notify-send",
        "runuser",
    ] {
        let present = runner.which(program);
        let critical = matches!(program, "lsblk" | "mount" | "umount");
        checks.push((
            format!("命令 {program}"),
            present,
            critical,
            if present {
                "已找到".to_string()
            } else if critical {
                "缺失，自动挂载无法工作".to_string()
            } else {
                "缺失（相关功能降级）".to_string()
            },
        ));
    }

    let filesystems = fstypes::read_proc_filesystems();
    if filesystems.is_empty() {
        checks.push((
            "文件系统".to_string(),
            false,
            false,
            "无法读取 /proc/filesystems（非 Linux 环境或权限受限）".to_string(),
        ));
    } else {
        for fstype in ["ext4", "vfat", "exfat", "ntfs3"] {
            let supported = filesystems.contains(fstype);
            let detail = if supported {
                "内核支持".to_string()
            } else if fstype == "ntfs3" {
                "内核不支持 ntfs3；可安装 ntfs-3g（mount 会自动降级）".to_string()
            } else if fstype == "exfat" {
                "内核不支持 exfat；可安装 exfatprogs 或升级内核到 5.7+".to_string()
            } else {
                "内核不支持（若需要请安装对应驱动）".to_string()
            };
            checks.push((format!("文件系统 {fstype}"), supported, false, detail));
        }
    }

    let config_ok = config.validate().is_ok();
    checks.push((
        "配置".to_string(),
        config_ok,
        true,
        config
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "使用默认值（未找到配置文件）".to_string()),
    ));

    let state_dir = &config.state_dir;
    let state_ok = state_dir.exists()
        || state_dir
            .parent()
            .map(|parent| parent.is_dir())
            .unwrap_or(false);
    checks.push((
        "状态目录".to_string(),
        state_ok,
        false,
        state_dir.display().to_string(),
    ));

    let root_ok = config.mount_root.exists()
        || config
            .mount_root
            .parent()
            .map(|parent| parent.is_dir())
            .unwrap_or(false);
    checks.push((
        "挂载根目录".to_string(),
        root_ok,
        true,
        config.mount_root.display().to_string(),
    ));

    let service_paths = [
        "/etc/systemd/system/automountd.service",
        "/lib/systemd/system/automountd.service",
        "/usr/lib/systemd/system/automountd.service",
    ];
    let service_installed = service_paths
        .iter()
        .any(|path| std::path::Path::new(path).is_file());
    checks.push((
        "systemd 服务".to_string(),
        service_installed,
        false,
        if service_installed {
            "已安装 automountd.service".to_string()
        } else {
            "未安装；可运行 packaging/install.sh".to_string()
        },
    ));

    checks.push((
        "运行身份".to_string(),
        true,
        false,
        if platform::is_root() {
            "root（可以执行挂载）".to_string()
        } else {
            format!(
                "uid {}（挂载/卸载需要 root 或通过服务执行）",
                platform::euid()
            )
        },
    ));

    let mut rows = Vec::with_capacity(checks.len());
    let mut critical_failures = 0;
    for (name, ok, critical, detail) in checks {
        if !ok && critical {
            critical_failures += 1;
        }
        let mark = if ok {
            colors.green("OK")
        } else if critical {
            colors.red("!!")
        } else {
            colors.yellow("--")
        };
        rows.push(vec![mark, name, detail]);
    }
    print!("{}", term::table(&["", "检查项", "说明"], &rows));

    if critical_failures == 0 {
        println!("{}", colors.green("关键检查全部通过。"));
        Ok(0)
    } else {
        println!(
            "{}",
            colors.red(&format!(
                "有 {critical_failures} 项关键检查失败，请先修复。"
            ))
        );
        Ok(1)
    }
}

// ---------------------------------------------------------------------- 杂项
fn ensure_ok(response: CtlResponse) -> Result<CtlResponse> {
    if response.ok {
        return Ok(response);
    }
    Err(Error::new(kind_from_code(&response.code), response.message))
}

fn kind_from_code(code: &str) -> ErrorKind {
    match code {
        "config" => ErrorKind::Config,
        "io" => ErrorKind::Io,
        "command" => ErrorKind::Command,
        "device" => ErrorKind::Device,
        "mount" => ErrorKind::Mount,
        "not-mounted" => ErrorKind::NotMounted,
        "busy" => ErrorKind::Busy,
        "permission" => ErrorKind::Permission,
        "not-running" => ErrorKind::NotRunning,
        "protocol" => ErrorKind::Protocol,
        "unsupported" => ErrorKind::Unsupported,
        _ => ErrorKind::Mount,
    }
}

fn print_version() {
    println!("automountd {VERSION}");
    println!(
        "目标平台: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("构建配置: 零第三方依赖（std only）");
}

fn print_help() {
    println!("automountd {VERSION} — Linux 磁盘自动挂载守护进程");
    println!();
    println!("用法: automountd [全局选项] <命令> [参数...]");
    println!();
    println!("命令:");
    println!("  daemon                 前台运行守护进程（systemd 使用）");
    println!("  status                 查看服务状态");
    println!("  list, ls               列出块设备与挂载状态（--all 显示全部）");
    println!("  mount, m               挂载设备（--read-only / --force / --local）");
    println!("  umount, unmount, u     卸载设备（--lazy / --eject / --local）");
    println!("  reload                 重新加载配置");
    println!("  fstab list|add|remove  开机自动挂载持久化（写 /etc/fstab）");
    println!("  config show|path|init|validate  配置管理");
    println!("  doctor                 环境自检");
    println!("  tui                    交互式界面");
    println!("  version                版本信息");
    println!("  help                   本帮助");
    println!();
    println!("全局选项:");
    println!("  -c, --config <路径>    指定配置文件");
    println!("  -v, --verbose          输出调试日志（可重复）");
    println!("  -q, --quiet            安静模式");
    println!("      --dry-run          只打印将要执行的命令");
    println!("  -V, --version          显示版本");
    println!("  -h, --help             显示帮助");
    println!();
    println!("常用示例:");
    println!("  sudo systemctl start automountd");
    println!("  automountd list --all");
    println!("  automountd mount /dev/sdb1");
    println!("  automountd umount /media/alice/USB");
    println!("  sudo automountd fstab add UUID=1234-ABCD");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_mount_root(root: &str) -> Config {
        let mut config = Config::default();
        config.mount_root = PathBuf::from(root);
        config
    }

    #[test]
    fn fstab_target_must_be_under_mount_root() {
        let config = config_with_mount_root("/media");
        assert!(validate_fstab_target(&config, std::path::Path::new("/media/alice/U盘")).is_ok());
        assert!(validate_fstab_target(&config, std::path::Path::new("/etc")).is_err());
        assert!(validate_fstab_target(&config, std::path::Path::new("/usr")).is_err());
        assert!(validate_fstab_target(&config, std::path::Path::new("/media/../etc")).is_err());
        assert!(validate_fstab_target(&config, std::path::Path::new("media/USB")).is_err());
        assert!(validate_fstab_target(&config, std::path::Path::new("/media")).is_err());
        assert!(validate_fstab_target(&config, std::path::Path::new("/media/a\nb")).is_err());
    }

    #[test]
    fn config_init_works_before_file_exists() {
        let temp = crate::testutil::TempDir::new("cli-init");
        let path = temp.join("config.conf");
        let args = vec![
            "--config".to_string(),
            path.display().to_string(),
            "config".to_string(),
            "init".to_string(),
        ];
        assert_eq!(run_with_args(&args).unwrap(), 0);
        assert!(path.is_file());
        let config = Config::from_file(&path).unwrap();
        assert_eq!(config.mount_root, PathBuf::from("/media"));
        assert!(config.validate().is_ok());
    }
}
