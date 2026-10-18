# Changelog

English | [简体中文](CHANGELOG.md)

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Runtime Chinese/English UI: global `--lang zh|en`, automatic detection from
  `AUTOMOUNTD_LANG` / `LANG`, covering help, TUI, list/status, doctor, and main CLI output.
- English issue forms (bug / feature) and a bilingual pull-request template.
- English documentation: `README.en.md`, `CONTRIBUTING.en.md`, `SECURITY.en.md`, `SUPPORT.en.md`,
  `AI_USAGE.en.md`, `AGENTS.en.md`, and `CHANGELOG.en.md`.
- Declared that this project was completed by **deepseekv4flash**.
- Added `AI_USAGE.md` (AI scope, verification, limitations, contributor disclosure), `AGENTS.md`
  (rules for AI coding agents), and `.github/copilot-instructions.md`.

### Changed

- Removed the hard "zero third-party dependencies" constraint in favor of a controlled dependency
  review policy; `[dependencies]` is still empty.
- CI, Makefile, install script, and documentation no longer force `--offline`, so vetted crates can be
  added later.
- CLI `--version` now reports "no third-party runtime dependencies currently" instead of "zero
  third-party dependencies".

### Fixed

- None.

## [1.0.0] - 2026-10-18

First public release.

### Added

- udev-based block-device hotplug monitoring for `add` / `remove`, with debounce and polling fallback
  where udev is unavailable.
- Boot-time mounting of already-inserted devices; optional mounting of internal data disks.
- Desktop-user detection and `uid/gid/umask` ownership for `vfat/exfat/ntfs`.
- `/etc/fstab` persistence with backup, atomic replace, `fsync`, `nofail`, and
  `x-systemd.device-timeout`.
- CLI: `daemon`, `status`, `list`, `mount`, `umount`, `reload`, `fstab`, `config`, `doctor`, `tui`.
- Dependency-free interactive TUI.
- Layered source layout: `core` (pure logic), `system` (system interaction), `app` (service layer),
  `ui` (presentation).
- Hardened systemd unit, install/uninstall scripts, and example configuration.
- 114 unit tests and 4 CLI integration tests.

### Security

- System disks are never auto-mounted by default; mount options include `nosuid,nodev`.
- Label sanitization, mount-point containment, and symlink checks prevent path escapes.
- UUID allowlist/blocklist.
- Control socket verifies peer uid with `SO_PEERCRED` and honors `ctl_allowed_users`.
- `mount --force`, unmounting other users' mounts, and `shutdown`/`reload` are root/desktop-user only.
- fstab writes reject targets outside `mount_root` and fields containing newlines.

### Fixed

- Control client reads complete multi-line responses (terminated by `END`), fixing lost
  `list/status` metadata.
- `SO_PEERCRED` failure now fails closed instead of defaulting to root.
- `--dry-run` forces a local run when the service is running, avoiding real mounts.
- State records are retained when unmount fails, and lazy unmount cleans up stale mounts.
- `reload` triggers a background full scan instead of blocking control requests.
- Client/server control timeouts are aligned so one side does not give up while the other is still working.
- NTFS retries automatically between `ntfs3` and `ntfs-3g`.
- sysfs fallback inherits removable flags from the parent disk; the polling monitor establishes a
  baseline on its first pass.
