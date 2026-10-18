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
        .expect("failed to run --version");
    assert!(
        output.status.success(),
        "unexpected exit code: {:?}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("automountd"), "output: {stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "output should contain the package version: {stdout}"
    );
}

#[test]
fn help_lists_primary_commands() {
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("failed to run --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for command in [
        "daemon", "status", "list", "mount", "umount", "doctor", "tui",
    ] {
        assert!(
            stdout.contains(command),
            "--help is missing command {command}"
        );
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
        .expect("failed to run config init");
    assert!(
        init.status.success(),
        "config init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let text = fs::read_to_string(&config).expect("configuration file was not created");
    assert!(text.contains("mount_root"));
    assert!(text.contains("GPL") || text.contains("mount_options"));

    let validate = Command::new(binary())
        .arg("--config")
        .arg(&config)
        .args(["config", "validate"])
        .output()
        .expect("failed to run config validate");
    assert!(
        validate.status.success(),
        "config validate failed: {}",
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
        .expect("failed to run list --all");
    assert!(
        output.status.success(),
        "list --all failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("panicked"), "unexpected output: {stdout}");
}

#[test]
fn english_help_and_version() {
    let help = Command::new(binary())
        .args(["--lang", "en", "--help"])
        .output()
        .expect("failed to run --lang en --help");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        stdout.contains("Usage:"),
        "help did not switch to English: {stdout}"
    );
    assert!(stdout.contains("Linux disk auto-mount daemon"));

    let version = Command::new(binary())
        .args(["--lang", "en", "--version"])
        .output()
        .expect("failed to run --lang en --version");
    assert!(version.status.success());
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(
        stdout.contains("Target:"),
        "version output did not switch to English: {stdout}"
    );
}

#[test]
fn rejects_unknown_language() {
    let output = Command::new(binary())
        .args(["--lang", "fr", "--help"])
        .output()
        .expect("failed to run --lang fr");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Unsupported language"));
}
