#!/usr/bin/env bash
# automountd 安装 / 卸载脚本
#
#   sudo ./packaging/install.sh                  # 构建并安装
#   sudo ./packaging/install.sh --prefix /usr    # 自定义安装前缀
#   sudo ./packaging/install.sh --no-start       # 只安装，不启动服务
#   sudo ./packaging/install.sh --uninstall      # 卸载（保留配置）
#   sudo ./packaging/install.sh --uninstall --purge  # 卸载并删除配置

set -euo pipefail

PREFIX="/usr/local"
BINDIR=""
CONFDIR="/etc/automountd"
UNITDIR="/etc/systemd/system"
SERVICE="automountd.service"
NO_START=0
UNINSTALL=0
PURGE=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m错误:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix 需要参数}"; shift 2 ;;
        --prefix=*) PREFIX="${1#*=}"; shift ;;
        --no-start) NO_START=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) PURGE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "未知参数: $1" ;;
    esac
done
BINDIR="${PREFIX}/bin"

[ "$(id -u)" -eq 0 ] || die "请使用 root 运行：sudo $0 $*"

if [ "$UNINSTALL" -eq 1 ]; then
    info "停止并禁用服务"
    systemctl disable --now "${SERVICE}" 2>/dev/null || true
    rm -f "${UNITDIR}/${SERVICE}"
    systemctl daemon-reload 2>/dev/null || true
    rm -f "${BINDIR}/automountd"
    if [ "$PURGE" -eq 1 ]; then
        warn "删除配置文件 ${CONFDIR}"
        rm -rf "${CONFDIR}"
    else
        info "保留配置 ${CONFDIR}（如需删除请加 --purge）"
    fi
    info "卸载完成"
    exit 0
fi

command -v cargo >/dev/null 2>&1 || die "未找到 cargo，请先安装 Rust 工具链：https://rustup.rs"

info "构建 release 版本"
(
    cd "${ROOT_DIR}"
    cargo build --release
)
BIN="${ROOT_DIR}/target/release/automountd"
[ -x "$BIN" ] || die "构建产物不存在: $BIN"

info "安装可执行文件到 ${BINDIR}/automountd"
install -Dm755 "$BIN" "${BINDIR}/automountd"

info "安装文档"
install -Dm644 "${ROOT_DIR}/README.md" "${PREFIX}/share/doc/automountd/README.md"

info "准备配置目录 ${CONFDIR}"
install -d -m755 "${CONFDIR}"
if [ ! -f "${CONFDIR}/config.conf" ]; then
    "${BINDIR}/automountd" --config "${CONFDIR}/config.conf" config init >/dev/null
    info "已生成默认配置 ${CONFDIR}/config.conf"
else
    info "保留已有配置 ${CONFDIR}/config.conf"
fi

info "安装 systemd 服务"
install -d -m755 "${UNITDIR}"
sed "s|/usr/local/bin/automountd|${BINDIR}/automountd|g" \
    "${SCRIPT_DIR}/${SERVICE}" > "${UNITDIR}/${SERVICE}"
chmod 644 "${UNITDIR}/${SERVICE}"
systemctl daemon-reload

if [ "$NO_START" -eq 0 ]; then
    info "启用并启动 automountd"
    systemctl enable --now "${SERVICE}"
    sleep 1
    systemctl --no-pager --lines=10 status "${SERVICE}" || true
else
    info "已跳过启动；稍后执行: systemctl enable --now ${SERVICE}"
fi

cat <<'EOF'

安装完成。常用命令：
  automountd status            查看状态
  automountd list --all        列出设备
  automountd mount /dev/sdX1   手动挂载
  automountd umount /media/... 卸载
  automountd doctor            环境自检
  automountd tui               交互式界面
  journalctl -u automountd -f  查看日志

EOF
