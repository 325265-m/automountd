# 支持 / Support

**简体中文** | [English](SUPPORT.en.md)

## 文档

- [README.md](README.md)：安装、配置、CLI、安全设计与排障
- [CHANGELOG.md](CHANGELOG.md)：版本变更
- [SECURITY.md](SECURITY.md)：漏洞报告流程
- [CONTRIBUTING.md](CONTRIBUTING.md)：开发与 PR 流程

## 提问与报错

- **使用问题 / 功能建议**：使用 GitHub Issues 的对应模板
- **可复现的缺陷**：请附上 `automountd doctor`、`automountd list --all`、发行版/内核版本与相关日志
- **安全漏洞**：不要公开披露，按 [SECURITY.md](SECURITY.md) 私密报告

## 自助排查

```bash
automountd doctor
automountd status
automountd list --all
journalctl -u automountd -n 100 --no-pager
udevadm monitor --udev --subsystem-match=block
```

维护者为志愿者，响应时间有限；请优先提供完整信息以便定位。
