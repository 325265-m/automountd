# Contributing

English | [简体中文](CONTRIBUTING.zh.md)

Thanks for improving automountd. This guide covers the development environment, code layout, testing
requirements, and the pull-request process.

## Code of conduct

By participating you agree to follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Development environment

- Linux (required for real mounting/udev; pure-logic tests also run on macOS/Windows)
- Rust 1.74+ (`rust-version` in `Cargo.toml`)
- `util-linux` (`lsblk`, `blkid`, `mount`, `umount`)
- Optional: `systemd/udev`, `ntfs-3g`, `exfatprogs`, `udisks2`

```bash
git clone <repo>
cd automountd
cargo test --all-targets
cargo build --release
```

## Dependency policy

Third-party crates are allowed, but dependencies must stay controlled:

- Prefer the standard library or an existing implementation; do not add a large dependency for something
  a small amount of safe code can do.
- New runtime dependencies must explain in the PR: purpose, alternatives, maintenance status, and license
  (must be compatible with GPL-3.0-or-later).
- Do not add unmaintained crates, crates of unknown origin, or crates that duplicate existing capabilities.
- Update `Cargo.lock` and keep CI green (CI no longer forces `--offline`).
- Dependencies used by the root daemon, mounting, or path handling require extra security review and
  regression tests.

An empty `[dependencies]` table is a design choice, not a restriction.

## Code layout

```
src/core/     pure logic (config, model, policy, parsing); no direct system calls
src/system/   system interaction (processes, devices, mounts, fstab, state, udev, sessions)
src/app/      service layer (daemon, ctl, inventory)
src/ui/       presentation (cli, tui, term, logging)
```

Put new modules in the right layer and declare them in the layer's `mod.rs` and `src/lib.rs`.

## Conventions

- Format with `cargo fmt --all`; `cargo clippy --all-targets` must add no warnings.
- Public APIs use English doc comments; user-facing error messages are Chinese today, logs may be mixed.
- System operations must go through `system::exec::CommandRunner` so tests can inject `MockRunner`.
- Changes to paths, permissions, or mounting must add security tests (path escapes, symlinks, system-disk
  protection, non-root authorization).
- Never commit secrets, real device UUIDs, personal paths, or contents of `/etc`.
- Before submitting, run `cargo test --all-targets` on Linux at least once; run the full suite even for
  pure-logic changes.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: support mounting by label
fix(ctl): read multi-line responses completely
docs: add fstab troubleshooting
test: cover non-root unmount authorization
```

One logical change per commit; keep formatting/refactoring separate from behavior changes.

## Pull requests

1. Fork the repository and branch from `main`: `feat/...`, `fix/...`, `docs/...`.
2. Add or update tests; keep `cargo test --all-targets` green.
3. Update `README.md` / `README.zh.md` and `CHANGELOG.md` / `CHANGELOG.zh.md` for behavior changes.
4. Fill in the PR template: motivation, approach, test environment, and risks.
5. After CI passes, a maintainer reviews; Linux verification results may be requested.

## AI-assisted contributions

AI-assisted code, tests, or documentation are welcome, but the PR must disclose the tool, scope, and how
the result was verified. Contributors remain responsible for their submissions; AI is not a substitute for
tests, review, or Linux validation.

- Full policy and limitations: [AI_USAGE.md](AI_USAGE.md)
- Repository rules for AI coding agents: [AGENTS.md](AGENTS.md)

## License

By submitting a contribution you agree to license it under
[GPL-3.0-or-later](LICENSE) and confirm you have the right to do so.
