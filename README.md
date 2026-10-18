# automountd — Linux 磁盘自动挂载守护进程

用 Rust 编写的 Linux 块设备自动挂载服务：插入 U 盘/移动硬盘自动挂载到 `/media/<用户>/<卷标>`，
拔出自动卸载；支持开机挂载、`/etc/fstab` 持久化、命令行与交互式界面。

**零第三方依赖**：只使用 Rust 标准库和直接声明的 libc FFI。构建产物是单个静态链接友好的二进制，
不需要 Python、不需要 crates.io、可在离线环境构建，也避免了 root 守护进程的供应链风险。

```
     udev / 轮询                  控制 socket
        │                            │
        ▼                            ▼
   ┌──────────────────────────────────────┐
   │            事件循环（单线程）          │
   │  去抖 → 策略判定 → 挂载/卸载 → 记录    │
   └───────┬───────────────┬──────────────┘
           ▼               ▼
     mount(8)          state.tsv + 桌面通知
```

## 功能

- **热插拔**：通过 `udevadm monitor` 监听 `add`/`remove` 事件，1 秒去抖；无 udev 的容器环境自动降级为轮询。
- **开机挂载**：服务启动时扫描已插入的设备；可选 `auto_mount_internal_disks` 挂载内置数据盘。
- **归属桌面用户**：通过 `loginctl` 识别活动图形会话用户，`vfat/exfat/ntfs` 用 `uid/gid/umask` 把文件交给该用户。
- **安全默认值**：
  - 默认只自动挂载可移动/USB 设备，**系统盘永不自动挂载**；
  - 挂载选项默认带 `nosuid,nodev`；
  - 卷标净化、挂载点包含性检查、符号链接检查，防止路径逃逸；
  - UUID 白名单/黑名单；
  - 控制接口用 `SO_PEERCRED` 校验对端 uid，并受 `ctl_allowed_users` 限制；`--force` 仅 root 可用。
- **可靠卸载**：忙碌时自动重试一次，仍失败则提示占用进程；可选 `--lazy`。lazy 卸载会跳过 `eject`/断电，
  避免进程仍持有文件时造成数据丢失。
- **fstab 持久化**：`automountd fstab add` 自动写入 `UUID=... <挂载点> <类型> nofail,x-systemd.device-timeout=10`，
  备份原始文件、原子替换、拒绝写入系统目录。
- **CLI + TUI**：`automountd list / mount / umount / status / doctor / tui`，输出按中英文宽度对齐。
- **systemd 集成**：附带加固过的 unit，日志进 journald。

> 状态说明：仓库包含 113 个单元测试；Linux 专属代码通过 `cargo check --target x86_64-unknown-linux-gnu` 交叉检查，
> 但真实挂载动作需要在 Linux 实机上验证（开发机为 Windows，无法执行 mount 系统调用）。

## 安装

要求：

- Linux（内核 4.19+ 即可；exFAT 建议 5.7+，NTFS 建议 5.15+ 使用内核 `ntfs3`）
- Rust 1.74+（仅构建时需要）；`util-linux`、`systemd/udev`
- 可选：`ntfs-3g`（老内核挂 NTFS）、`exfatprogs`/`exfat-fuse`、`udisks2`（`power-off` 功能）

一键安装：

```bash
git clone <repo> automountd && cd automountd
sudo ./packaging/install.sh
```

脚本会 `cargo build --release --offline`、安装到 `/usr/local/bin/automountd`、
生成 `/etc/automountd/config.conf`、安装并启动 `automountd.service`。

手动安装：

```bash
cargo build --release --offline
sudo install -Dm755 target/release/automountd /usr/local/bin/automountd
sudo install -Dm644 packaging/automountd.service /etc/systemd/system/automountd.service
sudo automountd config init
sudo systemctl daemon-reload
sudo systemctl enable --now automountd
```

卸载：

```bash
sudo ./packaging/install.sh --uninstall          # 保留配置
sudo ./packaging/install.sh --uninstall --purge  # 连配置一起删
```

## 使用

```bash
# 服务状态与日志
automountd status
journalctl -u automountd -f

# 设备列表（--all 显示未格式化/加密/被策略排除的设备）
automountd list
automountd list --all

# 手动挂载 / 卸载（非 root 会自动走守护进程）
automountd mount /dev/sdb1
automountd mount UUID=1234-ABCD --read-only
automountd umount /media/alice/U盘

# 让某个磁盘开机自动挂载（写入 /etc/fstab）
sudo automountd fstab add UUID=1234-ABCD
sudo automountd fstab list

# 配置
automountd config show
automountd config path
automountd config validate
sudo automountd config init
sudo automountd reload

# 交互式界面
automountd tui

# 环境自检
automountd doctor
```

`automountd tui` 是一个行输入式界面（无需 ncurses/GTK，SSH 可用）：

```
 [1]  sdb1   U盘      exfat   28.9 GiB  可挂载   -
 [2]  sdc1   备份盘    ext4    1.8 TiB   已挂载   /media/alice/备份盘
命令 (m <编号> 挂载, u <编号> 卸载, r 刷新, a 全部, q 退出):
```

## 配置

默认配置文件：`/etc/automountd/config.conf`（INI 风格，支持 `#` 注释）。
查找顺序：`--config` → 环境变量 `AUTOMOUNTD_CONFIG` → `/etc/automountd/config.conf` →
`~/.config/automountd/config.conf` → 内置默认值。

完整示例见 [`examples/config.conf`](examples/config.conf)。常用项：

| 配置项 | 默认值 | 说明 |
| --- | --- | --- |
| `mount_root` | `/media` | 挂载根目录，不能是 `/` 或系统目录 |
| `per_user_dirs` | `true` | 挂载到 `<mount_root>/<用户名>/<卷标>` |
| `user` | 空 | 强制指定挂载点所有者；空 = 自动识别桌面用户 |
| `auto_mount_on_hotplug` | `true` | 插入时自动挂载 |
| `auto_mount_on_boot` | `true` | 服务启动时挂载已插入设备 |
| `auto_mount_internal_disks` | `false` | 是否也挂载内置磁盘（系统盘仍被保护） |
| `allowed_uuids` | 空 | 非空时只自动挂载这些 UUID |
| `blocked_uuids` / `blocked_labels` | 空 | 排除列表 |
| `ignored_fstypes` | swap/LUKS/RAID… | 不自动挂载的文件系统类型 |
| `default_mount_options` | `nosuid, nodev` | 所有挂载都会附加的选项 |
| `retry_attempts` / `retry_delay_seconds` | `3` / `2` | 挂载失败重试 |
| `suppress_remount_seconds` | `60` | 手动卸载后多少秒内不自动重挂 |
| `poll_interval_seconds` | `0` | 无 udev 时的轮询间隔；0=自动 |
| `ctl_allowed_users` | `*` | 允许通过控制接口操作的用户 |
| `notify` | `true` | 桌面通知（notify-send） |

每个文件系统的挂载选项在 `[mount_options]` 中配置，支持占位符
`{uid}`、`{gid}`、`{user}`、`{label}`：

```ini
[mount_options]
vfat = uid={uid}, gid={gid}, umask=0022, shortname=mixed, utf8
exfat = uid={uid}, gid={gid}, umask=0022, iocharset=utf8
```

### 挂载点命名

1. 优先使用卷标，经净化：保留 Unicode 字母/数字/`.`/`-`/`_`，空白转 `_`，去掉首尾点横线，最长 48 字符；
2. 无卷标时用 UUID 前 8 位：`disk-1234ABCD`；
3. 目录冲突时追加 `_2`、`_3`；已存在的空目录会复用，避免反复插拔产生 `U盘_2`。

## 安全设计

- **只处理块设备**：设备路径必须来自 lsblk/sysfs 且以 `/dev/` 开头；loop/RAID/LVM/加密容器默认跳过。
- **系统盘保护**：从 `/proc/self/mountinfo` 找出 `/`、`/boot`、`/usr`、`/var`、`/etc` 所在整盘，
  其上的分区默认拒绝自动挂载（除非 UUID 在 `allowed_uuids` 中或手动 `--force`）。
- **挂载点约束**：目标必须位于 `mount_root` 之下，且路径组件不允许符号链接；`..` 直接拒绝。
- **权限模型**：守护进程以 root 运行；控制 socket 权限 0666 但每个连接用 `SO_PEERCRED` 取真实 uid，
  按 `ctl_allowed_users` 授权；`shutdown`/`reload` 仅限 root 或当前桌面用户；
  `mount --force` 与卸载他人挂载仅 root 可用。控制请求有 64 KiB 上限、并发连接数上限与超时。
- **不自动执行 fsck**、不自动解锁 LUKS、不修改分区表：只做挂载/卸载。
- **fstab 写入**：先备份为 `fstab.automountd.bak`，临时文件 + rename 原子替换并 `fsync`；使用 `nofail`
  避免设备缺席时卡住开机；拒绝把挂载点写到 `mount_root` 之外或含换行的字段。

## fstab 持久化

```bash
sudo automountd fstab add UUID=1234-ABCD
```

写入形如：

```
UUID=1234-ABCD /media/alice/U盘 exfat uid=1000,gid=1000,umask=0022,nosuid,nodev,nofail,x-systemd.device-timeout=10 0 0
```

删除：

```bash
sudo automountd fstab remove UUID=1234-ABCD
sudo automountd fstab remove --target /media/alice/U盘
```

## 故障排查

| 现象 | 处理 |
| --- | --- |
| 服务起不来 | `systemctl status automountd` / `journalctl -u automountd -n 100` |
| 插入没反应 | `udevadm monitor --udev --subsystem-match=block` 看事件；`automountd list --all` 看策略原因 |
| NTFS 挂载失败（脏标志） | Windows 上 `chkdsk /f`，Linux 上 `sudo ntfsfix /dev/sdXN`，或只读挂载 |
| `unknown filesystem type` | 安装 `ntfs-3g` / `exfatprogs`；`automountd doctor` 会提示 |
| 卸载 busy | `lsof +D <挂载点>` 或 `fuser -vm <挂载点>`；紧急用 `automountd umount --lazy` |
| 权限不对 | 检查 `per_user_dirs`、`user`；`vfat/exfat/ntfs` 依赖 uid/gid 挂载选项 |
| 想看会执行什么 | `automountd --dry-run mount /dev/sdb1`（服务运行时也强制本地 dry-run） |

退出码：`0` 成功；`1` 一般错误；`3` 服务未运行；`4` 权限不足。

## 开发与测试

```bash
cargo test --offline          # 113 个单元测试（纯逻辑，可在非 Linux 上跑）
cargo check --all-targets     # 类型检查
cargo clippy --all-targets    # 静态检查
cargo build --release --offline
```

Linux 专属代码（Unix socket、`SO_PEERCRED`、`geteuid`/`chown`、udev 子进程）通过
`cargo check --target x86_64-unknown-linux-gnu` 交叉类型检查；`#[cfg(unix)]` 测试需要 Linux 实机运行。

目录结构：

```
src/
  main.rs cli.rs        CLI 解析与子命令
  daemon.rs             事件循环、控制命令处理
  udev.rs               udevadm 解析 / 轮询降级
  devices.rs            设备发现（手写 JSON 解析 lsblk）
  mountinfo.rs          /proc/self/mountinfo
  policy.rs            自动挂载策略
  mounter.rs            挂载/卸载、挂载点管理
  fstypes.rs            驱动选择与挂载选项
  fstab.rs              /etc/fstab 编辑
  state.rs              运行状态持久化
  ctl.rs                Unix socket 协议与服务端
  sessions.rs           桌面用户识别
  notify.rs             桌面通知
  tui.rs                交互式界面
  config.rs             配置解析/校验/模板
  exec.rs platform.rs   进程执行与平台 FFI
  naming.rs term.rs     命名/终端输出
packaging/               systemd unit 与安装脚本
examples/config.conf     示例配置
```

## 已知限制

- 不自动解锁 LUKS、不组装 RAID/LVM、不处理网络文件系统。
- 不替代 udisks2：不提供 PolicyKit 细粒度授权，也不发 desktop 托盘图标（提供 TUI）。
- 挂载策略以“可移动/USB”为主；内置数据盘需显式开启或加入 `allowed_uuids`。
- 单元测试覆盖解析、策略、协议和命令拼装；真实 `mount` 行为依赖内核与工具链，需在目标发行版上验证。

## License

MIT，见 [LICENSE](LICENSE)。
