//! Interactive terminal UI: list devices, mount/unmount them by index.
//!
//! Does not depend on crossterm/ratatui: plain stdin line input plus ANSI screen clearing,
//! so it works over SSH, a serial console, or any terminal.

use std::io::{self, Write};
use std::sync::Arc;

use crate::config::Config;
use crate::ctl::{self, CtlRequest};
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{CommandRunner, SystemRunner};
use crate::inventory;
use crate::state;
use crate::term::{self, Colors};
use crate::ui::i18n;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Mount(usize),
    Umount(usize),
    Refresh,
    ToggleAll,
    Help,
    Quit,
}

/// Parses a single line of input. Indices are the 1-based numbers shown in the UI.
pub fn parse_command(line: &str) -> Option<Command> {
    if line.trim().is_empty() {
        return Some(Command::Refresh);
    }
    let mut parts = line.split_whitespace();
    let head = parts.next()?.to_ascii_lowercase();
    let rest = parts.next();
    match head.as_str() {
        "m" | "mount" | "挂载" => rest
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| *index > 0)
            .map(Command::Mount),
        "u" | "umount" | "unmount" | "卸载" => rest
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| *index > 0)
            .map(Command::Umount),
        "r" | "refresh" | "刷新" | "" => Some(Command::Refresh),
        "a" | "all" | "全部" => Some(Command::ToggleAll),
        "h" | "help" | "?" | "帮助" => Some(Command::Help),
        "q" | "quit" | "exit" | "退出" => Some(Command::Quit),
        _ => None,
    }
}

pub fn run(config: &Config) -> Result<()> {
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let colors = Colors::auto();
    let mut include_all = false;

    loop {
        let rows = fetch_rows(config, &*runner, include_all)?;
        render(config, &rows, include_all, &colors);
        print!(
            "\n{} ",
            i18n::tr(
                "命令 (m <编号> 挂载, u <编号> 卸载, r 刷新, a 全部, q 退出):",
                "Command (m <n> mount, u <n> unmount, r refresh, a all, q quit):"
            )
        );
        io::stdout().flush().ok();

        let mut line = String::new();
        let read = io::stdin().read_line(&mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "{}: {err}",
                        i18n::tr("读取输入失败", "failed to read input")
                    ),
                ));
            }
        }
        let line = line.trim().to_string();
        let Some(command) = parse_command(&line) else {
            println!(
                "{}: {line}{}",
                i18n::tr("无法识别的命令", "Unknown command"),
                i18n::tr("（输入 h 查看帮助）", " (type h for help)")
            );
            wait_enter();
            continue;
        };

        match command {
            Command::Quit => break,
            Command::Refresh => {}
            Command::ToggleAll => include_all = !include_all,
            Command::Help => {
                print_help();
                wait_enter();
            }
            Command::Mount(index) => {
                let Some(row) = rows.get(index - 1) else {
                    println!("{} {index}", i18n::tr("没有编号为", "No device with index"));
                    wait_enter();
                    continue;
                };
                let reference = row[1].clone();
                match mount_device(config, &reference) {
                    Ok(message) => {
                        println!("{message}");
                        wait_enter();
                    }
                    Err(err) => {
                        println!("{}: {err}", i18n::tr("挂载失败", "Mount failed"));
                        if let Some(hint) = &err.hint {
                            println!("{}: {hint}", i18n::tr("提示", "Hint"));
                        }
                        wait_enter();
                    }
                }
            }
            Command::Umount(index) => {
                let Some(row) = rows.get(index - 1) else {
                    println!("{} {index}", i18n::tr("没有编号为", "No device with index"));
                    wait_enter();
                    continue;
                };
                let reference = if row[6].is_empty() {
                    row[1].clone()
                } else {
                    row[6].clone()
                };
                match umount_device(config, &reference) {
                    Ok(message) => {
                        println!("{message}");
                        wait_enter();
                    }
                    Err(err) => {
                        println!("{}: {err}", i18n::tr("卸载失败", "Unmount failed"));
                        if let Some(hint) = &err.hint {
                            println!("{}: {hint}", i18n::tr("提示", "Hint"));
                        }
                        wait_enter();
                    }
                }
            }
        }
    }
    Ok(())
}

fn fetch_rows(
    config: &Config,
    runner: &dyn CommandRunner,
    include_all: bool,
) -> Result<Vec<Vec<String>>> {
    if ctl::is_running(&config.ctl_socket) {
        let response = ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("list").arg(if include_all { "1" } else { "0" }),
            std::time::Duration::from_secs(15),
        )?;
        if !response.ok {
            return Err(Error::new(ErrorKind::Mount, response.message));
        }
        return Ok(response.rows);
    }
    let state = state::read_state(config);
    let views = inventory::snapshot(config, runner, &state, include_all)?;
    Ok(views.iter().map(crate::cli::view_row).collect())
}

fn render(config: &Config, rows: &[Vec<String>], include_all: bool, colors: &Colors) {
    if crate::platform::stdout_is_tty() {
        print!("\x1b[2J\x1b[H");
    }
    let running = ctl::is_running(&config.ctl_socket);
    println!(
        "{}  {}",
        colors.bold(i18n::tr("automountd 交互界面", "automountd interactive UI")),
        if running {
            colors.green(i18n::tr("服务运行中", "service running"))
        } else {
            colors.yellow(i18n::tr(
                "服务未运行（直接操作需 root）",
                "service not running (direct actions need root)",
            ))
        }
    );
    println!(
        "{}: {}    {}: {}{}",
        i18n::tr("挂载根目录", "Mount root"),
        config.mount_root.display(),
        i18n::tr("设备数", "devices"),
        rows.len(),
        if include_all {
            i18n::tr("（显示全部）", " (showing all)")
        } else {
            ""
        }
    );
    println!();

    if rows.is_empty() {
        println!(
            "{}",
            i18n::tr(
                "未发现设备。插入磁盘后按 r 刷新。",
                "No devices found. Insert a disk and press r to refresh."
            )
        );
        return;
    }

    let mut table_rows = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        if row.len() < 9 {
            continue;
        }
        let mounted = row[5] == "1";
        let status = if mounted {
            colors.green(i18n::tr("已挂载", "mounted"))
        } else if row[7] == "ok" {
            colors.cyan(i18n::tr("可挂载", "mountable"))
        } else {
            colors.dim(&term::truncate(
                &crate::cli::decision_message(&row[7], &row[8]),
                24,
            ))
        };
        table_rows.push(vec![
            format!("[{}]", index + 1),
            row[0].clone(),
            term::truncate(&row[2], 18),
            if row[3].is_empty() {
                "-".to_string()
            } else {
                row[3].clone()
            },
            row[4].clone(),
            status,
            term::truncate(&row[6], 32),
        ]);
    }
    print!(
        "{}",
        term::table(
            &[
                "#",
                i18n::tr("设备", "Device"),
                i18n::tr("卷标", "Label"),
                i18n::tr("文件系统", "Filesystem"),
                i18n::tr("容量", "Size"),
                i18n::tr("状态", "Status"),
                i18n::tr("挂载点", "Mount point"),
            ],
            &table_rows
        )
    );
}

fn mount_device(config: &Config, reference: &str) -> Result<String> {
    if ctl::is_running(&config.ctl_socket) {
        let response = ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("mount").arg(reference).arg("0").arg("0"),
            std::time::Duration::from_secs(config.mount_timeout_seconds.saturating_add(30)),
        )?;
        if !response.ok {
            return Err(Error::new(ErrorKind::Mount, response.message));
        }
        return Ok(format!(
            "{} {reference} -> {}",
            i18n::tr("已挂载", "Mounted"),
            response.meta_value("target").unwrap_or("?")
        ));
    }

    crate::platform::require_root("mount")?;
    let manager_runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let result = crate::cli::local_mount(config, manager_runner, reference, false, false)?;
    Ok(format!(
        "{} {} -> {}",
        i18n::tr("已挂载", "Mounted"),
        result.device.path,
        result.target.display()
    ))
}

fn umount_device(config: &Config, reference: &str) -> Result<String> {
    if ctl::is_running(&config.ctl_socket) {
        let response = ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("umount").arg(reference).arg("0").arg("0"),
            std::time::Duration::from_secs(
                config
                    .mount_timeout_seconds
                    .max(config.unmount_timeout_seconds)
                    .saturating_add(30),
            ),
        )?;
        if !response.ok {
            return Err(Error::new(ErrorKind::Mount, response.message));
        }
        return Ok(format!(
            "{} {}",
            i18n::tr("已卸载", "Unmounted"),
            response.meta_value("target").unwrap_or(reference)
        ));
    }
    let manager_runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let target = crate::cli::local_umount(config, manager_runner, reference, false, false)?;
    Ok(format!("{} {target}", i18n::tr("已卸载", "Unmounted")))
}

fn wait_enter() {
    print!("{}", i18n::tr("按回车继续…", "Press Enter to continue..."));
    io::stdout().flush().ok();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

fn print_help() {
    if i18n::is_en() {
        println!("Commands:");
        println!("  m <n>   Mount that device");
        println!("  u <n>   Unmount that device");
        println!("  r       Refresh the list");
        println!("  a       Toggle \"show all devices\"");
        println!("  h       Show this help");
        println!("  q       Quit");
        return;
    }
    println!("命令:");
    println!("  m <编号>   挂载该设备");
    println!("  u <编号>   卸载该设备");
    println!("  r          刷新列表");
    println!("  a          切换“显示全部设备”");
    println!("  h          显示本帮助");
    println!("  q          退出");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("m 3"), Some(Command::Mount(3)));
        assert_eq!(parse_command("mount 1"), Some(Command::Mount(1)));
        assert_eq!(parse_command("u 2"), Some(Command::Umount(2)));
        assert_eq!(parse_command("r"), Some(Command::Refresh));
        assert_eq!(parse_command(""), Some(Command::Refresh));
        assert_eq!(parse_command("q"), Some(Command::Quit));
        assert_eq!(parse_command("挂载 4"), Some(Command::Mount(4)));
    }

    #[test]
    fn rejects_bad_commands() {
        assert_eq!(parse_command("m 0"), None);
        assert_eq!(parse_command("m abc"), None);
        assert_eq!(parse_command("x 1"), None);
    }
}
