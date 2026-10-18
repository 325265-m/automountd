# AI 使用说明 / AI Usage

**简体中文** | [English](AI_USAGE.en.md)

本文说明 automountd 在开发过程中如何使用 AI、哪些部分由 AI 参与、如何验证，以及贡献者使用 AI 时的披露要求。
**本文不是对 AI 输出正确性的背书**；最终正确性由测试、审查与实际运行结果决定。

## 摘要

- **本项目由 `deepseekv4flash` 完成**：代码、测试与文档通过 DeepSeek Harness 的 `ai_team` 多代理编排生成，
  人类维护者负责需求、验收与发布决策。
- 项目在开发中使用了 **DeepSeek Harness 的 `ai_team` 多代理编排**：多个 AI worker 并行实现模块，另有多轮独立审查。
- 架构、模块契约、集成、修复与最终验证由主代理（`deepseekv4flash`）完成；AI 产出不是“直接合并”的。
- 所有代码经过：单元测试、CLI 集成测试、`clippy`、格式检查、Linux target 交叉类型检查与 Release 构建。
- 开发机为 Windows，**真实 `mount`/udev 热插拔未在本机执行**；Linux 运行期行为仍需在目标发行版上人工验证。

## 开发中如何使用 AI

### 1. 先固定契约

先建立工程骨架、共享类型（`model`、`error`、`config`、`exec`）与每个模块的公开函数签名，
再让多个 worker 在**互不重叠的文件**上实现。这样可以避免并行写同一文件产生冲突。

### 2. 并行实现

四个 worker 分别负责：

| 工作包 | 模块 |
| --- | --- |
| A | `devices`、`fstypes`（设备发现、驱动选择） |
| B | `mounter`、`fstab`、`state`（挂载执行、持久化、状态） |
| C | `sessions`、`udev`、`notify`（会话、事件源、通知） |
| D | `ctl`、`inventory`（控制协议、设备视图） |

主代理实现守护进程主循环、CLI/TUI、配置、平台层与打包，并负责集成与修复。

### 3. 多轮审查

- **第一轮（3 名审查员）**：按协议/授权、路径与挂载安全、解析与策略分片审查。
  发现 2 个阻塞问题（控制接口只读首行、`SO_PEERCRED` 失败时 fail-open 成 root）以及若干 high/medium。
- **第二轮（2 名审查员）**：针对修复本身做回归审查。
  发现 3 个 high（canonical 挂载根与路径检查不一致、挂载后校验恒真、`reload` 同步挂载阻塞控制请求）。

这些问题均已修复，并在 `CHANGELOG.md` 与提交历史中记录。

### 4. 验证

```bash
cargo test --all-targets
cargo clippy --all-targets
cargo fmt --all -- --check
cargo check --all-targets --target x86_64-unknown-linux-gnu
cargo build --release
```

当前结果：**114 个单元测试 + 4 个 CLI 集成测试通过**，`clippy` 无警告，Linux target 交叉检查通过。

## AI 参与范围

| 部分 | 参与方式 |
| --- | --- |
| 架构与模块边界 | 主代理设计；worker 按契约实现 |
| 核心模块初稿 | 多 worker 并行生成，主代理集成与修复 |
| 守护进程/CLI/TUI | 主代理实现 |
| 测试 | worker 编写模块测试，主代理补集成测试与回归测试 |
| 审查 | 多轮多代理只读审查，主代理裁决与修复 |
| 文档、打包、许可证 | 主代理编写，人工确认方向 |

## 已知局限

- AI 审查者可能共享同类盲点，不能替代人类安全审计。
- 单元测试主要覆盖解析、策略、协议与命令拼装；内核 `mount` 行为、udev 事件时序、真实权限模型需实机验证。
- 本项目在 Windows 开发机上完成，Linux 专属路径（Unix socket、`SO_PEERCRED`、udev 子进程、实际挂载）
  通过交叉类型检查与代码推演覆盖，**没有在本机执行真实挂载**。
- AI 可能生成看似合理但错误的边界处理；安全相关改动必须附带回归测试与人工 review。

## 贡献者使用 AI 的政策

允许并欢迎使用 AI 辅助贡献，但需要遵守以下规则：

1. **披露**：在 PR 描述中说明使用的工具/模型、参与范围（代码/测试/文档）以及你如何验证。
2. **责任**：你对自己提交的内容负责；AI 不能作为版权持有人，也不能替代你对许可证合规性的确认。
3. **禁止**：不要粘贴密钥、真实设备 UUID、客户数据或无权授权的专有代码到提示词中。
4. **质量**：AI 生成的代码必须满足与手写代码相同的测试、格式、`clippy` 与审查标准；不接受“能跑就行”。
5. **安全**：涉及 root 权限、挂载点路径、符号链接、控制 socket、fstab 写入的改动需要额外测试与说明。
6. **透明**：如果 AI 生成了大段代码，请在 PR 中标注，便于审查者重点关注。

AI 编码代理请先阅读 [AGENTS.md](AGENTS.md)。

## 相关文件

- [AGENTS.md](AGENTS.md)：给 AI 编码代理的仓库规则与完成定义
- [CONTRIBUTING.md](CONTRIBUTING.md)：人类贡献者流程
- [SECURITY.md](SECURITY.md)：漏洞报告流程
- [CHANGELOG.md](CHANGELOG.md)：AI 审查发现并修复的问题记录
