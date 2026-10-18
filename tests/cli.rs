//! Command-line integration tests: run the compiled `automountd` binary directly.

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
    // Dev machines and CI may have no block devices at all, or may have some;
    // here we only require that the command exits 0 consistently.
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

#[test]
fn english_help_and_version() {
    let help = Command::new(binary())
        .args(["--lang", "en", "--help"])
        .output()
        .expect("运行 --lang en --help 失败");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("Usage:"), "帮助未切换为英文: {stdout}");
    assert!(stdout.contains("Linux disk auto-mount daemon"));

    let version = Command::new(binary())
        .args(["--lang", "en", "--version"])
        .output()
        .expect("运行 --lang en --version 失败");
    assert!(version.status.success());
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(stdout.contains("Target:"), "版本输出未切换为英文: {stdout}");
}

#[test]
fn rejects_unknown_language() {
    let output = Command::new(binary())
        .args(["--lang", "fr", "--help"])
        .output()
        .expect("运行 --lang fr 失败");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("不支持的语言") || stderr.contains("Unsupported language"));
}
