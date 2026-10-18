# AI usage

English | [简体中文](AI_USAGE.md)

This document explains how AI was used to develop automountd, which parts it touched, how the result was
verified, and what contributors must disclose when using AI. **It is not an endorsement of AI correctness**;
correctness is determined by tests, review, and real-world results.

## Summary

- **This project was completed by `deepseekv4flash`**: code, tests, and documentation were produced through
  DeepSeek Harness's `ai_team` multi-agent orchestration; the human maintainer set requirements, accepted
  the result, and made release decisions.
- Multiple AI workers implemented modules in parallel, followed by independent review rounds.
- Architecture, module contracts, integration, fixes, and final verification were handled by the lead agent
  (`deepseekv4flash`); AI output was not merged blindly.
- Everything passed unit tests, CLI integration tests, `clippy`, formatting checks, Linux target
  cross-compilation checks, and a release build.
- The development host is Windows, so **real `mount`/udev hotplug was not executed here**; Linux runtime
  behavior still requires validation on the target distribution.

## How AI was used

### 1. Contract first

The project skeleton, shared types (`model`, `error`, `config`, `exec`), and public function signatures were
fixed before parallel implementation. Workers edited **disjoint files** to avoid conflicts.

### 2. Parallel implementation

Four workers owned:

| Work package | Modules |
| --- | --- |
| A | `devices`, `fstypes` (device discovery, driver selection) |
| B | `mounter`, `fstab`, `state` (mount execution, persistence, state) |
| C | `sessions`, `udev`, `notify` (sessions, event source, notifications) |
| D | `ctl`, `inventory` (control protocol, device views) |

The lead agent implemented the daemon loop, CLI/TUI, configuration, platform layer, and packaging, then
integrated and fixed issues.

### 3. Review rounds

- **Round 1 (3 reviewers)**: split by protocol/authorization, path and mount safety, parsing and policy.
  Found two blockers (the control client read only the first response line; `SO_PEERCRED` failure defaulted
  to root) plus several high/medium issues.
- **Round 2 (2 reviewers)**: regression review of the fixes. Found three high issues (canonical mount root
  inconsistent with path checks, post-mount verification always true, `reload` blocking control requests).

All were fixed and recorded in `CHANGELOG.md` and the commit history.

### 4. Verification

```bash
cargo test --all-targets
cargo clippy --all-targets
cargo fmt --all -- --check
cargo check --all-targets --target x86_64-unknown-linux-gnu
cargo build --release
```

Current result: **114 unit tests + 4 CLI integration tests pass**, no `clippy` warnings, Linux target
cross-check passes.

## Scope of AI involvement

| Area | Involvement |
| --- | --- |
| Architecture and module boundaries | Lead agent designed; workers implemented contracts |
| Core module drafts | Parallel workers; lead agent integrated and fixed |
| Daemon / CLI / TUI | Lead agent |
| Tests | Workers wrote module tests; lead agent added integration and regression tests |
| Review | Multiple read-only AI review rounds; lead agent adjudicated and fixed |
| Docs, packaging, license | Lead agent; human confirmed direction |

## Known limitations

- AI reviewers can share blind spots; they do not replace human security audits.
- Unit tests cover parsing, policy, protocol, and command construction; kernel `mount` behavior, udev timing,
  and real permission models need a Linux host.
- The project was built on Windows; Linux-only paths (Unix socket, `SO_PEERCRED`, udev child process, real
  mounting) were covered by cross-compilation and reasoning, **not executed locally**.
- AI can produce plausible but incorrect edge-case handling; security changes require regression tests and
  human review.

## Contributor AI policy

AI-assisted contributions are welcome under these rules:

1. **Disclose** the tool/model, scope (code/tests/docs), and how you verified the result in the PR.
2. **Responsibility**: you are accountable for your submission; AI cannot hold copyright or replace your
   license-compliance checks.
3. **Do not** paste secrets, real device UUIDs, customer data, or proprietary code you are not allowed to
   share into prompts.
4. **Quality**: AI-generated code must meet the same tests, formatting, `clippy`, and review standards.
5. **Security**: changes involving root, mount-point paths, symlinks, the control socket, or fstab writes
   need extra tests and explanation.
6. **Transparency**: mark large AI-generated sections in the PR so reviewers can focus.

AI coding agents should read [AGENTS.en.md](AGENTS.en.md) first.

## Related documents

- [AGENTS.en.md](AGENTS.en.md): repository rules and definition of done for AI agents
- [CONTRIBUTING.en.md](CONTRIBUTING.en.md): human contributor workflow
- [SECURITY.en.md](SECURITY.en.md): vulnerability reporting
- [CHANGELOG.en.md](CHANGELOG.en.md): issues found and fixed by AI review
