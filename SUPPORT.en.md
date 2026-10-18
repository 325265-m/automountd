# Support

English | [简体中文](SUPPORT.md)

## Documentation

- [README.en.md](README.en.md): installation, configuration, CLI, security design, troubleshooting
- [CHANGELOG.en.md](CHANGELOG.en.md): release notes
- [SECURITY.en.md](SECURITY.en.md): vulnerability reporting
- [CONTRIBUTING.en.md](CONTRIBUTING.en.md): development and PR process

## Questions and bug reports

- **Usage questions / feature requests**: use the corresponding GitHub issue template
- **Reproducible bugs**: include `automountd doctor`, `automountd list --all`, distribution/kernel version,
  and relevant logs
- **Security vulnerabilities**: do not disclose publicly; follow [SECURITY.en.md](SECURITY.en.md)

## Self-service checks

```bash
automountd doctor
automountd status
automountd list --all
journalctl -u automountd -n 100 --no-pager
udevadm monitor --udev --subsystem-match=block
```

Maintainers are volunteers with limited response time; complete information helps a lot.
