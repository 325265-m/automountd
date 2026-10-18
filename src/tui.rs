//! 交互式终端界面：列出设备，按编号挂载/卸载。
//!
//! 不依赖 crossterm/ratatui：用普通 stdin 行输入 + ANSI 清屏，
//! 因此可以通过 SSH、串口或任何终端使用。

use std::io::{self, Write};
use std::sync::Arc;

use crate::config::Config;
use crate::ctl::{self, CtlRequest};
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{CommandRunner, SystemRunner};
use crate::inventory;
use crate::state;
use crate::term::{self, Colors};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Mount(usize),
    Umount(usize),
    Refresh,
    ToggleAll,
    Help,
    Quit,
}

/// 解析一行输入。编号是界面上显示的 1 起始序号。
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
        print!("\n命令 (m <编号> 挂载, u <编号> 卸载, r 刷新, a 全部, q 退出): ");
        io::stdout().flush().ok();

        let mut line = String::new();
        let read = io::stdin().read_line(&mut line);
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(Error::new(ErrorKind::Io, format!("读取输入失败: {err}")));
            }
        }
        let line = line.trim().to_string();
        let Some(command) = parse_command(&line) else {
            println!("无法识别的命令: {line}（输入 h 查看帮助）");
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
                    println!("没有编号为 {index} 的设备");
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
                        println!("挂载失败: {err}");
                        if let Some(hint) = &err.hint {
                            println!("提示: {hint}");
                        }
                        wait_enter();
                    }
                }
            }
            Command::Umount(index) => {
                let Some(row) = rows.get(index - 1) else {
                    println!("没有编号为 {index} 的设备");
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
                        println!("卸载失败: {err}");
                        if let Some(hint) = &err.hint {
                            println!("提示: {hint}");
                        }
                        wait_enter();
                    }
                }
            }
        }
    }
    Ok(())
}

fn fetch_rows(config: &Config, runner: &dyn CommandRunner, include_all: bool) -> Result<Vec<Vec<String>>> {
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
        colors.bold("automountd 交互界面"),
        if running {
            colors.green("服务运行中")
        } else {
            colors.yellow("服务未运行（直接操作需 root）")
        }
    );
    println!(
        "挂载根目录: {}    设备数: {}{}",
        config.mount_root.display(),
        rows.len(),
        if include_all { "（显示全部）" } else { "" }
    );
    println!();

    if rows.is_empty() {
        println!("未发现设备。插入磁盘后按 r 刷新。");
        return;
    }

    let mut table_rows = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        if row.len() < 9 {
            continue;
        }
        let mounted = row[5] == "1";
        let status = if mounted {
            colors.green("已挂载")
        } else if row[7] == "ok" {
            colors.cyan("可挂载")
        } else {
            colors.dim(&term::truncate(&row[8], 24))
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
            &["#", "设备", "卷标", "文件系统", "容量", "状态", "挂载点"],
            &table_rows
        )
    );
}

fn mount_device(config: &Config, reference: &str) -> Result<String> {
    if ctl::is_running(&config.ctl_socket) {
        let response = ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("mount").arg(reference).arg("0").arg("0"),
            std::time::Duration::from_secs(120),
        )?;
        if !response.ok {
            return Err(Error::new(ErrorKind::Mount, response.message));
        }
        return Ok(format!(
            "已挂载 {reference} -> {}",
            response.meta_value("target").unwrap_or("?")
        ));
    }

    crate::platform::require_root("挂载")?;
    let manager_runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let result = crate::cli::local_mount(
        config,
        manager_runner,
        reference,
        false,
        false,
    )?;
    Ok(format!(
        "已挂载 {} -> {}",
        result.device.path,
        result.target.display()
    ))
}

fn umount_device(config: &Config, reference: &str) -> Result<String> {
    if ctl::is_running(&config.ctl_socket) {
        let response = ctl::request(
            &config.ctl_socket,
            &CtlRequest::new("umount")
                .arg(reference)
                .arg("0")
                .arg("0"),
            std::time::Duration::from_secs(60),
        )?;
        if !response.ok {
            return Err(Error::new(ErrorKind::Mount, response.message));
        }
        return Ok(format!(
            "已卸载 {}",
            response.meta_value("target").unwrap_or(reference)
        ));
    }
    let manager_runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let target = crate::cli::local_umount(config, manager_runner, reference, false, false)?;
    Ok(format!("已卸载 {target}"))
}

fn wait_enter() {
    print!("按回车继续…");
    io::stdout().flush().ok();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

fn print_help() {
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
