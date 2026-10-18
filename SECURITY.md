# 安全策略 / Security Policy

automountd 以 root 运行并处理块设备与挂载点，安全问题是首要关注项。

## 支持版本

| 版本 | 支持状态 |
| --- | --- |
| 1.x | 接收安全修复 |
| < 1.0 | 不支持 |

## 报告漏洞

**请不要在公开 issue 中披露未修复的漏洞。**

优先使用 GitHub 的私密漏洞报告功能：

1. 打开仓库的 **Security** 标签页；
2. 选择 **Advisories → Report a vulnerability**；
3. 提供复现步骤、影响版本、内核/发行版信息，以及 PoC（如有）。

如果无法使用该功能，请创建一个**不含漏洞细节**的 issue，标题注明 `[security] 请求私下联系渠道`，
维护者会通过 GitHub 与你建立私密沟通。

## 响应流程

- 3 个工作日内确认收到报告；
- 7 个工作日内给出初步影响评估；
- 修复发布后，在 `CHANGELOG.md` 与 GitHub Security Advisory 中致谢报告者（如你愿意署名）。

以上为尽力而为的时间目标，不是法律承诺。

## 安全边界

在范围内：

- 挂载点路径逃逸、符号链接/TOCTOU 导致挂载到系统目录；
- 控制 socket 的越权（`SO_PEERCRED`、`ctl_allowed_users`、`--force`、卸载他人挂载）；
- 通过卷标/UUID/fstab 字段注入额外挂载项；
- 以普通用户身份让守护进程挂载系统盘或执行任意 mount 选项；
- 敏感信息写入日志或状态文件；
- 第三方依赖的供应链风险（新增/升级依赖引入的恶意或被接管代码）。

不在范围内：

- 已经获得 root 权限后的操作；
- 内核/文件系统驱动自身的漏洞（请报告给对应上游）；
- 用户主动配置 `auto_mount_internal_disks`、`allowed_uuids` 或 `--force` 绕过默认保护；
- 物理接触、恶意 USB 固件等硬件攻击。

## 加固建议

- 保持默认的 `auto_mount_internal_disks = false` 与系统盘保护，不要随意放开 `allowed_uuids`；
- `ctl_allowed_users` 默认 `*` 表示所有本地用户（仍会校验 uid）；多用户机器建议收紧为具体用户名或 uid；
- 使用发行版提供的 systemd 加固参数，并定期查看 `journalctl -u automountd`；
- 在执行 `fstab` 持久化前确认挂载点位于 `mount_root` 下；
- 新增依赖前检查维护状态、许可证与已知漏洞，并锁定 `Cargo.lock`。
