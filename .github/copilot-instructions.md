# GitHub Copilot instructions

See [AGENTS.md](../AGENTS.md) for the full repository rules. Key requirements:

- Dependencies are controlled: do not add a crate without documenting its purpose, alternatives,
  license, and security impact; update `Cargo.lock` and keep CI buildable.
- MSRV 1.74, edition 2021; do not use newer unstable APIs.
- Place new modules in `core / system / app / ui` and declare them in the layer's `mod.rs` and `src/lib.rs`.
- External commands must go through `CommandRunner`; only the streaming `udevadm monitor` is an exception.
- Do not weaken security defaults: system-disk protection, `nosuid,nodev`, mount-point path checks,
  `SO_PEERCRED` fail-closed, root-only `--force`, and fstab targets under `mount_root`.
- After changes run:
  `cargo fmt --all -- --check`, `cargo clippy --all-targets`,
  `cargo test --all-targets`, `cargo build --release`.
- Add tests and update `CHANGELOG.md` / `CHANGELOG.zh.md` for new behavior.
- Never commit real device UUIDs, secrets, or personal paths.

See [AI_USAGE.md](../AI_USAGE.md) for AI disclosure and limitations.
