# AGENTS.md

English | [简体中文](AGENTS.zh.md)

This file is for AI coding agents working in this repository (Codex, Cursor, Copilot, Claude Code, DSH, etc.).
Human contributors should read [CONTRIBUTING.md](CONTRIBUTING.md).

## Read first

1. [README.md](README.md): features, installation, configuration, security design
2. [AI_USAGE.md](AI_USAGE.md): AI disclosure and verification requirements
3. [SECURITY.md](SECURITY.md): security boundaries; understand before changing code
4. `src/lib.rs` and the `mod.rs` of each layer: current module layout

## Non-negotiable constraints

1. **Controlled dependencies (third-party crates are no longer forbidden)**: an empty `[dependencies]` table
   is a design choice, not a ban. New dependencies must:
   - prefer std or existing implementations; avoid large dependencies for small, safe code;
   - document purpose, alternatives, maintenance status, and license (must be GPL-3.0-or-later compatible);
   - update `Cargo.lock` and keep CI buildable;
   - receive extra security review when used by the root daemon, mounting, or path handling;
   - never be unmaintained, of unknown origin, or duplicate existing capabilities.
2. **MSRV 1.74 / edition 2021**: do not use standard-library APIs or language features stabilized later.
3. **Layering**:
   - `src/core/`: pure logic (config, model, policy, parsing). Do not add system calls; the existing
     `/proc` read in `mountinfo` is a known exception.
   - `src/system/`: system interaction (commands, devices, mounts, fstab, state, udev, sessions).
   - `src/app/`: daemon, control interface, device views.
   - `src/ui/`: CLI, TUI, terminal, logging.
   Declare new modules in the layer's `mod.rs` and in `src/lib.rs`.
4. **External commands**: always go through `system::exec::CommandRunner` so tests can inject `MockRunner`.
   The only exception is the streaming `udevadm monitor` child process in `system/udev.rs`.
5. **`unsafe`**: only allowed for libc FFI in `system/platform.rs` and `SO_PEERCRED` in `app/ctl.rs`.
   New `unsafe` requires a safety justification.
6. **Never weaken security defaults**:
   - system disks are not auto-mounted;
   - default mount options include `nosuid,nodev`;
   - mount points must be under `mount_root`; `..` and symlinks are rejected;
   - `SO_PEERCRED` failures must fail closed;
   - `mount --force`, `shutdown`, `reload`, and unmounting other users' mounts are root/desktop-user only;
   - fstab writes must reject targets outside `mount_root` and fields containing newlines.
   Do not relax these rules to make tests pass.
7. **License**: code is GPL-3.0-or-later. Do not introduce GPL-incompatible code; do not modify
   `LICENSE` / `NOTICE`.

## Common commands

```bash
# Full test suite (unit + CLI integration)
cargo test --all-targets

# Formatting and lints
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets

# Build
cargo build --release

# Linux-only code cross-check (target must be installed)
cargo check --all-targets --target x86_64-unknown-linux-gnu
```

CI also checks MSRV with Rust 1.74. Do not submit code that only compiles on the latest nightly.

## Code map

```
src/core/config.rs      config parsing, validation, template
src/core/model.rs       shared data structures (devices, mounts, decisions, records)
src/core/error.rs       unified error type and exit codes
src/core/naming.rs      label sanitization, path containment, symlink checks
src/core/policy.rs      auto-mount / manual-mount policy
src/core/fstypes.rs     driver selection and mount options
src/core/mountinfo.rs   /proc/self/mountinfo parser
src/system/platform.rs  euid/chown/signals/terminal FFI
src/system/exec.rs      CommandRunner / SystemRunner / MockRunner
src/system/devices.rs   lsblk JSON, sysfs fallback, device resolution
src/system/mounter.rs   mount/umount, mount-point management, diagnostics
src/system/fstab.rs     fstab parsing and atomic writes
src/system/state.rs     runtime state TSV
src/system/sessions.rs  desktop user detection
src/system/udev.rs      udevadm parsing, polling fallback
src/system/notify.rs    notify-send
src/app/daemon.rs       event loop, control dispatch, boot scan
src/app/ctl.rs          Unix socket protocol, server, client
src/app/inventory.rs    device view snapshots
src/ui/cli.rs           argument parsing and subcommands
src/ui/tui.rs           interactive UI
src/ui/i18n.rs          Chinese/English UI strings
src/ui/term.rs          tables/colors/time formatting
src/ui/logging.rs       logging
```

## Workflow

1. Read the relevant modules and tests first; do not invent public APIs.
2. Prefer minimal changes to existing functions/types; keep `pub` signatures stable unless the task requires it.
3. Every bug fix needs a regression test; every feature needs a success path and at least one failure path.
4. Pure-logic tests must run off Linux; put Linux-only code behind `#[cfg(unix)]` and explain how to verify it.
5. Update docs: behavior changes go to `README.md` / `README.zh.md` and `CHANGELOG.md` / `CHANGELOG.zh.md`
   (`[Unreleased]`).
6. Run the full checks before committing:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets
cargo test --all-targets
cargo build --release
```

## Definition of done

- [ ] `cargo test --all-targets` passes
- [ ] `cargo clippy --all-targets` adds no warnings
- [ ] `cargo fmt --all -- --check` passes
- [ ] Linux target `cargo check` passes when `cfg(unix)` code changes
- [ ] Tests added/updated, including failure paths
- [ ] Security-relevant changes have regression tests and a risk note
- [ ] `CHANGELOG.md` updated; `README.md` when needed
- [ ] New dependencies have a reason, license, and security impact (currently no runtime deps)
- [ ] No real UUIDs, secrets, or personal paths committed

## Prohibited

- Do not delete or weaken security tests to make the suite pass.
- Do not bypass hooks with `git commit --no-verify` (if hooks exist).
- Do not commit `target/`, debug binaries, IDE configuration, or real device information.
- Do not reformat large unrelated areas; keep diffs reviewable.
- Do not fabricate test results or claim Linux validation you did not perform.
