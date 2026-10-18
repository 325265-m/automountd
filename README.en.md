# automountd — Linux disk auto-mount daemon

English | [简体中文](README.md)

[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-1.74%2B-orange.svg)
![Platform](https://img.shields.io/badge/platform-Linux-lightgrey.svg)
![Developed by deepseekv4flash](https://img.shields.io/badge/developed%20by-deepseekv4flash-blueviolet.svg)

> **Development notice**: this project was completed by **deepseekv4flash** (code, tests, and documentation).

A block-device auto-mount service for Linux written in Rust: plug in a USB stick or portable drive and it is
mounted at `/media/<user>/<label>`; unplug it and it is unmounted. It also supports boot-time mounting,
`/etc/fstab` persistence, a CLI, and an interactive TUI.

**No third-party runtime dependencies at the moment**: only the Rust standard library and directly declared
libc FFI are used, producing a single binary. Third-party crates are not forbidden; new runtime dependencies
must go through the [dependency policy](CONTRIBUTING.en.md#dependency-policy). An empty `[dependencies]` table
is a design choice, not a restriction.

```
     udev / polling                 control socket
        │                               │
        ▼                               ▼
   ┌──────────────────────────────────────────┐
   │          event loop (single thread)       │
   │ debounce → policy → mount/umount → state  │
   └──────────┬────────────────────┬───────────┘
              ▼                    ▼
        mount(8)          state.tsv + desktop notifications
```

## Features

- **Hotplug**: listens to `add`/`remove` events from `udevadm monitor` with a 1-second debounce;
  automatically falls back to polling in containers without udev.
- **Boot-time mounting**: scans devices already present when the service starts; optional
  `auto_mount_internal_disks` also mounts internal data disks.
- **Desktop ownership**: identifies the active graphical session user via `loginctl`; for
  `vfat/exfat/ntfs`, `uid/gid/umask` are used so the files belong to that user.
- **Safe defaults**:
  - only removable/USB devices are auto-mounted by default, and **system disks are never auto-mounted**;
  - mount options include `nosuid,nodev` by default;
  - label sanitization, mount-point containment checks, and symlink checks prevent path escapes;
  - UUID allowlist/blocklist;
  - the control socket verifies the peer uid via `SO_PEERCRED` and is restricted by `ctl_allowed_users`;
    `--force` is root-only.
- **Reliable unmount**: retries once when busy and then points at the processes holding the mount;
  `--lazy` is available. Lazy unmount skips `eject`/power-off to avoid data loss while processes still
  hold files.
- **fstab persistence**: `automountd fstab add` writes
  `UUID=... <mountpoint> <fstype> nofail,x-systemd.device-timeout=10`, backs up the original file,
  replaces it atomically, and refuses system directories.
- **CLI + TUI**: `automountd list / mount / umount / status / doctor / tui`, with output aligned for
  CJK and Latin widths.
- **systemd integration**: ships a hardened unit and logs to journald.

> Status: 116 unit tests + 6 CLI integration tests. Linux-only code is cross-checked with
> `cargo check --target x86_64-unknown-linux-gnu`; real mount operations still need validation on a
> Linux machine (the development host is Windows and cannot execute mount syscalls).

## Installation

Requirements:

- Linux (kernel 4.19+ works; exFAT is best on 5.7+, NTFS on 5.15+ with the in-kernel `ntfs3` driver)
- Rust 1.74+ (build time only); `util-linux`, `systemd/udev`
- Optional: `ntfs-3g` (NTFS on older kernels), `exfatprogs`/`exfat-fuse`, `udisks2` (for `power-off`)

One-liner:

```bash
git clone <repo> automountd && cd automountd
sudo ./packaging/install.sh
```

The script runs `cargo build --release`, installs `/usr/local/bin/automountd`, creates
`/etc/automountd/config.conf`, installs `automountd.service`, and starts it.

Manual installation:

```bash
cargo build --release
sudo install -Dm755 target/release/automountd /usr/local/bin/automountd
sudo install -Dm644 packaging/automountd.service /etc/systemd/system/automountd.service
sudo automountd config init
sudo systemctl daemon-reload
sudo systemctl enable --now automountd
```

Uninstall:

```bash
sudo ./packaging/install.sh --uninstall          # keep configuration
sudo ./packaging/install.sh --uninstall --purge  # remove configuration too
```

## Usage

```bash
# Service status and logs
automountd status
journalctl -u automountd -f

# Device list (--all also shows unformatted/encrypted/policy-excluded devices)
automountd list
automountd list --all

# Manual mount / unmount (non-root requests go through the daemon)
automountd mount /dev/sdb1
automountd mount UUID=1234-ABCD --read-only
automountd umount /media/alice/USB

# Make a disk mount automatically at boot (writes /etc/fstab)
sudo automountd fstab add UUID=1234-ABCD
sudo automountd fstab list

# Configuration
automountd config show
automountd config path
automountd config validate
sudo automountd config init
sudo automountd reload

# Interactive UI
automountd tui

# Environment check
automountd doctor
```

`automountd tui` is a line-oriented interface (no ncurses/GTK required; works over SSH):

```
 [1]  sdb1   USB      exfat   28.9 GiB  mountable  -
 [2]  sdc1   Backup   ext4    1.8 TiB   mounted    /media/alice/Backup
Command (m <n> mount, u <n> unmount, r refresh, a all, q quit):
```

### UI language

The CLI/TUI follows `LANG` / `LC_ALL` / `AUTOMOUNTD_LANG`; when unset or unrecognized it defaults to
English. Use `--lang` to force a language:

```bash
automountd --lang zh list
automountd --lang en status
AUTOMOUNTD_LANG=zh automountd doctor
```

`--lang` must appear before the subcommand. English currently covers help, the TUI, list/status,
doctor, and the main command messages; detailed errors from `core`/`system` are still being translated.

## Configuration

Default config file: `/etc/automountd/config.conf` (INI-style, `#` comments supported).
Lookup order: `--config` → `AUTOMOUNTD_CONFIG` → `/etc/automountd/config.conf` →
`~/.config/automountd/config.conf` → built-in defaults.

See [`examples/config.conf`](examples/config.conf) for a complete example. Common options:

| Option | Default | Description |
| --- | --- | --- |
| `mount_root` | `/media` | Mount root; must not be `/` or a system directory |
| `per_user_dirs` | `true` | Mount at `<mount_root>/<username>/<label>` |
| `user` | empty | Force mount-point owner; empty = detect the desktop user |
| `auto_mount_on_hotplug` | `true` | Mount when a device is inserted |
| `auto_mount_on_boot` | `true` | Mount devices present at service start |
| `auto_mount_internal_disks` | `false` | Also mount internal disks (system disks stay protected) |
| `allowed_uuids` | empty | If non-empty, only these UUIDs are auto-mounted |
| `blocked_uuids` / `blocked_labels` | empty | Exclusion lists |
| `ignored_fstypes` | swap/LUKS/RAID… | Filesystem types that are never auto-mounted |
| `default_mount_options` | `nosuid, nodev` | Options appended to every mount |
| `retry_attempts` / `retry_delay_seconds` | `3` / `2` | Mount retry policy |
| `suppress_remount_seconds` | `60` | Do not auto-remount for this many seconds after a manual unmount |
| `poll_interval_seconds` | `0` | Polling interval without udev; 0 = automatic |
| `ctl_allowed_users` | `*` | Users allowed to operate through the control socket |
| `notify` | `true` | Desktop notifications via notify-send |

Filesystem-specific options live in `[mount_options]` and support the placeholders
`{uid}`, `{gid}`, `{user}`, and `{label}`:

```ini
[mount_options]
vfat = uid={uid}, gid={gid}, umask=0022, shortname=mixed, utf8
exfat = uid={uid}, gid={gid}, umask=0022, iocharset=utf8
```

### Mount-point naming

1. Prefer the volume label, sanitized: Unicode letters/digits/`.`/`-`/`_` are kept, whitespace becomes
   `_`, leading/trailing dots and dashes are removed, max 48 characters.
2. Without a label, use the first 8 UUID characters: `disk-1234ABCD`.
3. On collision append `_2`, `_3`; existing empty directories are reused to avoid `USB_2` after replugging.

## Security design

- **Block devices only**: device paths must come from lsblk/sysfs and start with `/dev/`;
  loop/RAID/LVM/encrypted containers are skipped by default.
- **System disk protection**: the whole disk containing `/`, `/boot`, `/usr`, `/var`, or `/etc`
  (from `/proc/self/mountinfo`) is never auto-mounted unless its UUID is in `allowed_uuids` or the
  user explicitly passes `--force`.
- **Mount-point constraints**: the target must be under `mount_root`, path components must not be
  symlinks, and `..` is rejected.
- **Permission model**: the daemon runs as root. The control socket is mode 0666 but every connection
  is checked with `SO_PEERCRED` and authorized by `ctl_allowed_users`; `shutdown`/`reload` are limited
  to root or the current desktop user; `mount --force` and unmounting other users' mounts are root-only.
  Control requests have a 64 KiB limit, a connection cap, and timeouts.
- **No automatic fsck**, no LUKS unlock, no partition-table changes: mount/umount only.
- **fstab writes**: back up to `fstab.automountd.bak`, atomically replace with temp file + rename +
  `fsync`, use `nofail` to avoid blocking boot, and reject targets outside `mount_root` or fields
  containing newlines.

## fstab persistence

```bash
sudo automountd fstab add UUID=1234-ABCD
```

Writes a line like:

```
UUID=1234-ABCD /media/alice/USB exfat uid=1000,gid=1000,umask=0022,nosuid,nodev,nofail,x-systemd.device-timeout=10 0 0
```

Remove:

```bash
sudo automountd fstab remove UUID=1234-ABCD
sudo automountd fstab remove --target /media/alice/USB
```

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| Service will not start | `systemctl status automountd` / `journalctl -u automountd -n 100` |
| Nothing happens on plug-in | `udevadm monitor --udev --subsystem-match=block`; check policy reasons with `automountd list --all` |
| NTFS mount fails (dirty flag) | Run `chkdsk /f` on Windows, `sudo ntfsfix /dev/sdXN` on Linux, or mount read-only |
| `unknown filesystem type` | Install `ntfs-3g` / `exfatprogs`; `automountd doctor` reports missing helpers |
| Unmount is busy | `lsof +D <mountpoint>` or `fuser -vm <mountpoint>`; use `automountd umount --lazy` as a last resort |
| Wrong ownership | Check `per_user_dirs`, `user`; `vfat/exfat/ntfs` rely on uid/gid mount options |
| Want to see the command first | `automountd --dry-run mount /dev/sdb1` (forces a local dry-run even when the service is running) |

Exit codes: `0` success; `1` general error; `3` service not running; `4` permission denied.

## Development and testing

```bash
cargo test --all-targets   # 116 unit tests + 6 CLI integration tests
cargo check --all-targets  # type check
cargo clippy --all-targets # lints
cargo build --release
```

Linux-only code (Unix socket, `SO_PEERCRED`, `geteuid`/`chown`, udev child process) is cross-checked with
`cargo check --target x86_64-unknown-linux-gnu`; `#[cfg(unix)]` tests need a real Linux host.

Repository layout (layered by responsibility):

```
src/
  main.rs               process entry point
  lib.rs                module declarations and compatibility re-exports
  core/                 pure logic (no system calls; cross-platform tests)
    config.rs           config parsing/validation/template
    model.rs            shared data structures
    error.rs            unified error type
    naming.rs           label sanitization and path safety
    policy.rs           auto-mount policy
    fstypes.rs          filesystem drivers and mount options
    mountinfo.rs        /proc/self/mountinfo parser
    testutil.rs         test helpers
  system/               system interaction
    platform.rs         libc FFI / signals / permissions
    exec.rs             external command abstraction
    devices.rs          lsblk/sysfs device discovery
    mounter.rs          mount/umount and mount-point management
    fstab.rs            /etc/fstab editing
    state.rs            runtime state persistence
    sessions.rs         desktop user detection
    udev.rs             udevadm parsing / polling fallback
    notify.rs           desktop notifications
  app/                  service layer
    daemon.rs           event loop and control command dispatch
    ctl.rs              Unix socket protocol/server
    inventory.rs        device view snapshot
  ui/                   presentation layer
    cli.rs              CLI parsing and subcommands
    tui.rs              interactive UI
    i18n.rs             Chinese/English UI strings
    term.rs             terminal formatting
    logging.rs          logging
packaging/              systemd unit and install scripts
examples/config.conf    example configuration
tests/cli.rs            CLI integration tests
.github/                CI, release workflow, issue/PR templates, dependabot
CONTRIBUTING.en.md      contribution guide
CODE_OF_CONDUCT.md      code of conduct
SECURITY.en.md          security policy
SUPPORT.en.md           support notes
CHANGELOG.en.md         changelog
NOTICE                  GPL copyright and attribution
AGENTS.en.md            rules for AI coding agents
AI_USAGE.en.md          AI usage disclosure and policy
```

## Known limitations

- Does not unlock LUKS, assemble RAID/LVM, or handle network filesystems.
- Not a udisks2 replacement: no PolicyKit per-action authorization and no desktop tray icon (a TUI is provided).
- Auto-mount is geared toward removable/USB devices; internal data disks require explicit opt-in or
  `allowed_uuids`.
- Unit tests cover parsing, policy, protocol, and command construction; real `mount` behavior depends on
  the kernel and tooling and must be validated on the target distribution.

## AI usage

This project was developed with AI assistance (DeepSeek Harness `ai_team` multi-agent implementation and
review rounds). AI output is covered by tests, static checks, and review, but is not a correctness guarantee;
real mounting still needs Linux validation.

- AI scope, verification, limitations, and contributor disclosure: [AI_USAGE.en.md](AI_USAGE.en.md)
- Repository rules for AI coding agents (Codex / Cursor / Copilot / Claude Code / DSH): [AGENTS.en.md](AGENTS.en.md)

## Contributing

- Development setup, conventions, and PR flow: [CONTRIBUTING.en.md](CONTRIBUTING.en.md)
- AI-assisted contribution disclosure: [AI_USAGE.en.md](AI_USAGE.en.md)
- Code of conduct: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- Vulnerability reports (please do not disclose publicly): [SECURITY.en.md](SECURITY.en.md)
- Questions and support: [SUPPORT.en.md](SUPPORT.en.md)
- Changelog: [CHANGELOG.en.md](CHANGELOG.en.md)

Issues and pull requests are welcome in English or Chinese; English issue forms are provided.
Changes touching mounting, permissions, or paths should include verification results from a real Linux machine.

## License

GNU General Public License v3.0 or later (`GPL-3.0-or-later`); see [LICENSE](LICENSE) for the full text.
Copyright (C) 2026 automountd contributors; see [NOTICE](NOTICE) for the complete notice.

This is free software: you may redistribute and/or modify it under the terms of the GNU General Public
License as published by the Free Software Foundation, either version 3 of the License, or any later version.
When distributing, keep the license and copyright notices and provide the corresponding source code as
required by the GPL.
