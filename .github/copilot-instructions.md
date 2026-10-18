# GitHub Copilot instructions

本仓库的完整 AI 代理规则见 [AGENTS.md](../AGENTS.md)。以下是必须遵守的要点：

- 零第三方依赖：不要修改 `Cargo.toml` 的 `[dependencies]`。
- MSRV 1.74、edition 2021；不要使用更新的不稳定 API。
- 按 `core / system / app / ui` 分层放置新模块，并在对应 `mod.rs` 与 `src/lib.rs` 声明。
- 外部命令必须通过 `CommandRunner`；只有 `udevadm monitor` 流式读取例外。
- 不得削弱安全默认值：系统盘保护、`nosuid,nodev`、挂载点路径检查、`SO_PEERCRED` fail-closed、
  `--force` 仅 root、fstab 目标必须在 `mount_root` 下。
- 修改后运行：
  `cargo fmt --all -- --check`、`cargo clippy --all-targets --offline`、
  `cargo test --all-targets --offline`、`cargo build --release --offline`。
- 新增行为要补测试并更新 `CHANGELOG.md`。
- 不要提交真实设备 UUID、密钥或个人路径。

AI 使用披露与局限见 [AI_USAGE.md](../AI_USAGE.md)。
