//! 命令行集成测试：直接运行编译出的 `automountd` 二进制。

use std::fs;
use std::process::Command;

use automountd::testutil::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_automountd")
}

#[test]
fn version_reports_package_version() {
    let output = Command::new(binary())
        .arg("--version")
        .output()
        .expect("运行 --version 失败");
    assert!(output.status.success(), "退出码 {:?}", output.status.code());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("automountd"), "输出: {stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "输出应包含版本号: {stdout}"
    );
}

#[test]
fn help_lists_primary_commands() {
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("运行 --help 失败");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for command in [
        "daemon", "status", "list", "mount", "umount", "doctor", "tui",
    ] {
        assert!(stdout.contains(command), "--help 缺少命令 {command}");
    }
}

#[test]
fn config_init_and_validate_roundtrip() {
    let temp = TempDir::new("cli-integration");
    let config = temp.join("config file.conf");

    let init = Command::new(binary())
        .arg("--config")
        .arg(&config)
        .args(["config", "init"])
        .output()
        .expect("运行 config init 失败");
    assert!(
        init.status.success(),
        "config init 失败: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let text = fs::read_to_string(&config).expect("配置文件未生成");
    assert!(text.contains("mount_root"));
    assert!(text.contains("GPL") || text.contains("mount_options"));

    let validate = Command::new(binary())
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .output()
        .expect("运行 config validate 失败");
    assert!(
        validate.status.success(),
        "config validate 失败: {}",
        String::from_utf8_lossy(&validate.stderr)
    );
}

#[test]
fn list_all_succeeds_without_devices() {
    // 开发机/CI 可能没有任何块设备，也可能有；这里只要求命令稳定退出 0。
    let output = Command::new(binary())
        .args(["list", "--all"])
        .output()
        .expect("运行 list --all 失败");
    assert!(
        output.status.success(),
        "list --all 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("panicked"), "输出异常: {stdout}");
}
