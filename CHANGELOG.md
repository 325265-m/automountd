# Changelog

**简体中文** | [English](CHANGELOG.en.md)

本项目的所有重要变更都记录在此文件。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- 新增运行时中英文界面：全局 `--lang zh|en`、`AUTOMOUNTD_LANG` / `LANG` 自动检测，
  覆盖帮助、TUI、列表/状态、doctor 与主要 CLI 输出。
- 新增英文 Issue 表单（Bug / Feature）与中英双语 Pull Request 模板。
- 新增英文版文档：`README.en.md`、`CONTRIBUTING.en.md`、`SECURITY.en.md`、`SUPPORT.en.md`、
  `AI_USAGE.en.md`、`AGENTS.en.md`、`CHANGELOG.en.md`。
- 在 README 与 AI_USAGE 中声明本项目由 **deepseekv4flash** 完成。
- 新增 `AI_USAGE.md`（AI 参与范围、验证、局限与贡献者披露政策）、`AGENTS.md`（AI 编码代理规则）
  与 `.github/copilot-instructions.md`。

### Changed

- 移除“零第三方依赖”的硬性约束，改为受控的依赖审查策略；当前 `[dependencies]` 仍为空。
- CI、Makefile、安装脚本与文档不再强制 `--offline`，以便未来引入经审查的 crate。
- CLI `--version` 的“零第三方依赖”文案改为“当前无第三方运行时依赖”。

### Fixed

- 暂无

## [1.0.0] - 2026-10-18

首个公开版本。

### Added

- 基于 udev 的块设备热插拔监听，支持 `add` / `remove` 事件、去抖与无 udev 环境轮询降级。
- 开机挂载已插入设备；可选挂载内置数据盘。
- 自动识别活动图形会话用户，并通过 `uid/gid/umask` 把 `vfat/exfat/ntfs` 挂载点交给该用户。
- `/etc/fstab` 持久化：备份、原子替换、`fsync`、`nofail` 与 `x-systemd.device-timeout`。
- 命令行：`daemon`、`status`、`list`、`mount`、`umount`、`reload`、`fstab`、`config`、`doctor`、`tui`。
- 无第三方依赖的交互式 TUI。
- 分层源码结构：`core`（纯逻辑）、`system`（系统交互）、`app`（服务层）、`ui`（表现层）。
- 适配 systemd 的加固 unit、安装/卸载脚本与示例配置。
- 114 个单元测试与 4 个 CLI 集成测试。

### Security

- 默认拒绝自动挂载系统盘，挂载选项默认带 `nosuid,nodev`。
- 卷标净化、挂载点包含性检查、符号链接检查，防路径逃逸。
- UUID 白名单/黑名单。
- 控制接口使用 `SO_PEERCRED` 校验对端 uid，并受 `ctl_allowed_users` 限制。
- `mount --force`、卸载他人挂载、`shutdown`/`reload` 仅限 root 或当前桌面用户。
- fstab 写入拒绝 `mount_root` 之外的挂载点与含换行字段。

### Fixed

- 控制接口客户端完整读取多行响应（以 `END` 结束），修复 `list/status` 元数据丢失。
- `SO_PEERCRED` 获取失败时 fail-closed，不再默认视为 root。
- 服务运行时 `--dry-run` 强制本地执行，避免真实挂载。
- 卸载失败时保留状态记录，并使用 lazy 卸载清理残留挂载。
- `reload` 改为异步触发全量扫描，避免阻塞控制请求。
- 统一客户端/服务端控制超时，避免一端超时另一端仍在执行。
- NTFS 在 `ntfs3` 与 `ntfs-3g` 之间自动回退。
- sysfs 兜底时从父盘继承可移动属性；轮询模式首轮只建立基线。
