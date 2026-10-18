# AGENTS.md

**简体中文** | [English](AGENTS.md)

本文件面向在本仓库工作的 AI 编码代理（Codex、Cursor、Copilot、Claude Code、DSH 等）。
人类贡献者请阅读 [CONTRIBUTING.md](CONTRIBUTING.zh.md)。

## 先读这些

1. [README.md](README.zh.md)：功能、安装、配置与安全设计
2. [AI_USAGE.md](AI_USAGE.zh.md)：AI 使用披露与验证要求
3. [SECURITY.md](SECURITY.zh.md)：安全边界，改动前必须理解
4. `src/lib.rs` 与各层 `mod.rs`：当前模块划分

## 不可协商的约束

1. **依赖受控（不再禁止第三方 crate）**：当前 `[dependencies]` 为空是设计选择，不是禁令。
   新增依赖必须：
   - 优先标准库或现有实现；能用少量安全代码解决的不引入大依赖；
   - 在 PR 中说明用途、替代方案、维护状态、许可证（必须与 GPL-3.0-or-later 兼容）；
   - 同步更新 `Cargo.lock`，确保 CI 可构建；
   - 运行时依赖（尤其涉及 root 守护进程、挂载、路径处理）需要额外安全审查；
   - 禁止来源不明、无人维护或与现有能力重复的 crate。
2. **MSRV 1.74 / edition 2021**：不要使用更高版本才稳定的标准库 API 或语言特性。
3. **分层**：
   - `src/core/`：纯逻辑（配置、模型、策略、解析）。不要新增系统调用；现有 `mountinfo` 读取 `/proc` 是既定例外。
   - `src/system/`：系统交互（命令、设备、挂载、fstab、状态、udev、会话）。
   - `src/app/`：守护进程、控制接口、设备视图。
   - `src/ui/`：CLI、TUI、终端、日志。
   新模块必须放入正确的层，并在层 `mod.rs` 与 `src/lib.rs` 中声明。
4. **外部命令**：必须通过 `system::exec::CommandRunner` 执行，以便测试注入 `MockRunner`。
   唯一例外是 `system/udev.rs` 需要流式读取 `udevadm monitor` 子进程。
5. **`unsafe`**：只允许在 `system/platform.rs` 的 libc FFI 与 `app/ctl.rs` 的 `SO_PEERCRED` 中使用；
   新增 `unsafe` 必须写明安全性理由。
6. **安全默认值不得削弱**：
   - 系统盘默认拒绝自动挂载；
   - 挂载选项默认包含 `nosuid,nodev`；
   - 挂载点必须位于 `mount_root` 下，拒绝 `..` 与符号链接；
   - 控制接口 `SO_PEERCRED` 失败必须 fail-closed；
   - `mount --force`、`shutdown`、`reload`、卸载他人挂载只允许 root / 当前桌面用户；
   - fstab 写入必须拒绝 `mount_root` 之外的挂载点与含换行字段。
   不要为了让测试通过而放宽这些规则。
7. **许可证**：代码以 GPL-3.0-or-later 授权。不要引入与 GPL 不兼容的代码；不要修改 `LICENSE` / `NOTICE`。

## 常用命令

```bash
# 全量测试（单元 + CLI 集成）
cargo test --all-targets

# 格式与静态检查
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets

# 构建
cargo build --release

# Linux 专属代码的交叉类型检查（需安装 target）
cargo check --all-targets --target x86_64-unknown-linux-gnu
```

CI 还会用 Rust 1.74 做 MSRV 检查。不要提交只在最新 nightly 上能编译的代码。

## 代码地图

```
src/core/config.rs      配置解析、校验、模板
src/core/model.rs       共享数据结构（设备、挂载、决策、记录）
src/core/error.rs       统一错误类型与退出码
src/core/naming.rs      卷标净化、路径包含性、符号链接检查
src/core/policy.rs      自动挂载/手动挂载策略
src/core/fstypes.rs     驱动选择与挂载选项
src/core/mountinfo.rs   /proc/self/mountinfo 解析
src/system/platform.rs  euid/chown/信号/终端等 FFI
src/system/exec.rs      CommandRunner / SystemRunner / MockRunner
src/system/devices.rs   lsblk JSON、sysfs 兜底、设备解析
src/system/mounter.rs   mount/umount、挂载点管理、错误诊断
src/system/fstab.rs     fstab 解析与原子写入
src/system/state.rs     运行状态 TSV
src/system/sessions.rs  桌面用户识别
src/system/udev.rs      udevadm 解析、轮询降级
src/system/notify.rs    notify-send
src/app/daemon.rs       事件循环、控制命令分发、开机扫描
src/app/ctl.rs          Unix socket 协议、服务端、客户端
src/app/inventory.rs    设备视图快照
src/ui/cli.rs           参数解析与子命令
src/ui/tui.rs           交互式界面
src/ui/i18n.rs          中英文界面文案
src/ui/term.rs          表格/颜色/时间格式化
src/ui/logging.rs       日志
```

## 修改流程

1. 先读相关模块与测试，确认现有约定；不要凭猜测新增公共 API。
2. 优先在既有函数/类型上做最小改动；保持 `pub` 签名稳定，除非任务明确要求。
3. 每个 bug 修复都补一个回归测试；每个新功能至少覆盖正常路径与一个失败路径。
4. 纯逻辑测试必须能在非 Linux 上跑；Linux 专属代码放 `#[cfg(unix)]` 并说明如何在 Linux 验证。
5. 更新文档：行为变更同步 `README.md` 与 `CHANGELOG.md`（`[Unreleased]`）。
6. 提交前跑完整检查：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets
cargo test --all-targets
cargo build --release
```

## 完成定义（Definition of Done）

- [ ] 代码在 `cargo test --all-targets` 下全绿
- [ ] `cargo clippy --all-targets` 无新增警告
- [ ] `cargo fmt --all -- --check` 通过
- [ ] Linux target `cargo check` 通过（涉及 `cfg(unix)` 时）
- [ ] 新增/更新测试，包含失败路径
- [ ] 安全相关改动有回归测试与风险说明
- [ ] 更新 `CHANGELOG.md`，必要时更新 `README.md`
- [ ] 新增依赖已说明理由、许可证与安全影响（当前无运行时依赖）
- [ ] 未泄露真实 UUID、密钥、个人路径

## 禁止事项

- 不要为了让测试通过而删除/放宽安全测试。
- 不要 `git commit --no-verify` 绕过钩子（如存在）。
- 不要提交 `target/`、调试二进制、IDE 配置或真实设备信息。
- 不要大段重排无关代码；保持 diff 可审查。
- 不要伪造测试结果或声称在 Linux 上验证过却未实际运行。
