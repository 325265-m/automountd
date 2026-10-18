# Security policy

English | [简体中文](SECURITY.md)

automountd runs as root and handles block devices and mount points, so security is a primary concern.

## Supported versions

| Version | Supported |
| --- | --- |
| 1.x | Security fixes |
| < 1.0 | Not supported |

## Reporting a vulnerability

**Please do not disclose an unfixed vulnerability in a public issue.**

Prefer GitHub private vulnerability reporting:

1. Open the repository's **Security** tab;
2. Choose **Advisories → Report a vulnerability**;
3. Include reproduction steps, affected version, kernel/distribution, and a PoC if available.

If that is unavailable, open an issue **without vulnerability details** titled
`[security] request a private channel`; a maintainer will establish private contact through GitHub.

## Response process

- Acknowledge the report within 3 business days;
- Provide an initial impact assessment within 7 business days;
- After a fix is released, credit the reporter in `CHANGELOG.md` and the GitHub Security Advisory
  (if you wish to be named).

These are best-effort targets, not a legal commitment.

## Security boundaries

In scope:

- mount-point path escapes, symlink/TOCTOU issues that mount to system directories;
- control-socket privilege escalation (`SO_PEERCRED`, `ctl_allowed_users`, `--force`, unmounting others);
- injecting extra mount entries through labels/UUIDs/fstab fields;
- making the daemon mount a system disk or use arbitrary mount options as an unprivileged user;
- sensitive data written to logs or state files;
- supply-chain risks in third-party dependencies (malicious or compromised crates).

Out of scope:

- operations after an attacker already has root;
- kernel/filesystem driver vulnerabilities (report them upstream);
- deliberately weakening defaults with `auto_mount_internal_disks`, `allowed_uuids`, or `--force`;
- physical access, malicious USB firmware, and other hardware attacks.

## Hardening recommendations

- Keep `auto_mount_internal_disks = false` and system-disk protection enabled; do not widen
  `allowed_uuids` casually.
- `ctl_allowed_users` defaults to `*` (all local users, still uid-checked); tighten it to specific
  usernames or uids on multi-user machines.
- Keep the systemd hardening options from the shipped unit and review `journalctl -u automountd` regularly.
- Confirm the mount point is under `mount_root` before persisting to fstab.
- Before adding dependencies, check maintenance status, license, and known vulnerabilities; commit the
  updated `Cargo.lock`.
