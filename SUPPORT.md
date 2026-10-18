# Support

English | [简体中文](SUPPORT.zh.md)

## Documentation

- [README.md](README.md): installation, configuration, CLI, security design, troubleshooting
- [CHANGELOG.md](CHANGELOG.md): release notes
- [SECURITY.md](SECURITY.md): vulnerability reporting
- [CONTRIBUTING.md](CONTRIBUTING.md): development and PR process

## Questions and bug reports

- **Usage questions / feature requests**: use the corresponding GitHub issue template
  (English forms are available as `Bug report (English)` and `Feature request (English)`)
- **Reproducible bugs**: include `automountd doctor`, `automountd list --all`, distribution/kernel version,
  and relevant logs
- **Security vulnerabilities**: do not disclose publicly; follow [SECURITY.md](SECURITY.md)

## Self-service checks

```bash
automountd doctor
automountd status
automountd list --all
journalctl -u automountd -n 100 --no-pager
udevadm monitor --udev --subsystem-match=block
```

Maintainers are volunteers with limited response time; complete information helps a lot.
