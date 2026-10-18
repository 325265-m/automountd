# 贡献指南 / Contributing

感谢你愿意改进 automountd。本文说明开发环境、代码结构、测试要求与提交流程。

## 行为准则

参与本项目即表示你同意遵守 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。

## 开发环境

- Linux（真实挂载/udev 功能需要；纯逻辑测试可在 macOS/Windows 上跑）
- Rust 1.74+（`rust-version` 在 `Cargo.toml` 中声明）
- `util-linux`（`lsblk`、`blkid`、`mount`、`umount`）
- 可选：`systemd/udev`、`ntfs-3g`、`exfatprogs`、`udisks2`

```bash
git clone <repo>
cd automountd
cargo test --all-targets
cargo build --release
```

## 依赖策略

本项目不再禁止第三方 crate，但依赖必须受控：

- 优先使用标准库或现有实现；能用少量安全代码解决的不引入大依赖。
- 新增运行时依赖需在 PR 中说明：用途、替代方案、维护状态、许可证（必须与 GPL-3.0-or-later 兼容）。
- 禁止来源不明、无人维护或与现有能力重复的 crate。
- 同步更新 `Cargo.lock`，确保 CI 可以正常构建（CI 不再强制 `--offline`）。
- 涉及 root 守护进程、挂载、路径处理的依赖需要额外安全审查与回归测试。

当前 `[dependencies]` 为空是设计选择，不是限制。

## 代码结构

```
src/core/     纯逻辑（配置、模型、策略、解析），不得直接执行系统命令
src/system/   系统交互（进程、设备、挂载、fstab、状态、udev、会话）
src/app/      服务层（daemon、ctl、inventory）
src/ui/       表现层（cli、tui、term、logging）
```

新增模块时请放入最合适的层，并在对应 `mod.rs` 与 `src/lib.rs` 中声明。

## 编码约定

- `cargo fmt --all` 格式化；`cargo clippy --all-targets` 无新增警告。
- 公共 API 使用中文文档注释；错误消息面向用户用中文，日志可中英混排。
- 系统操作必须通过 `system::exec::CommandRunner`，便于用 `MockRunner` 测试。
- 涉及路径、权限、挂载的改动必须补充安全测试（路径逃逸、符号链接、系统盘保护、非 root 授权）。
- 不要提交密钥、真实设备 UUID、个人路径或 `/etc` 内容。
- 提交前在 Linux 上至少运行一次 `cargo test --all-targets`；仅改纯逻辑时也要跑全量测试。

## 提交信息

推荐 [Conventional Commits](https://www.conventionalcommits.org/zh-hans/)：

```
feat: 支持按卷标挂载
fix(ctl): 修复多行响应被截断
docs: 补充 fstab 排障说明
test: 增加非 root 卸载授权用例
```

一次提交只做一件事；格式化/重构与行为变更分开。

## Pull Request 流程

1. Fork 仓库并从 `main` 创建分支：`feat/...`、`fix/...`、`docs/...`。
2. 编写或更新测试，确保 `cargo test --all-targets` 全绿。
3. 更新 `README.md` / `CHANGELOG.md`（行为变更必须写 `[Unreleased]`）。
4. 填写 PR 模板，说明动机、实现方式、测试环境与风险。
5. CI 通过后由维护者 review；必要时会要求补充 Linux 实机验证结果。

## AI 辅助贡献

允许使用 AI 辅助编写代码、测试或文档，但必须在 PR 中披露工具、参与范围与验证方式。
贡献者对自己的提交负责，AI 不能替代测试、审查或 Linux 实机验证。

- 完整政策与已知局限：[AI_USAGE.md](AI_USAGE.md)
- 给 AI 编码代理的仓库规则：[AGENTS.md](AGENTS.md)

## 许可证

提交贡献即表示你同意以 [GPL-3.0-or-later](LICENSE) 授权你的贡献，并确认你有权这样做。
