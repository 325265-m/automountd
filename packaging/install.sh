#!/usr/bin/env bash
# automountd install / uninstall script
#
#   sudo ./packaging/install.sh                  # build and install
#   sudo ./packaging/install.sh --prefix /usr    # custom install prefix
#   sudo ./packaging/install.sh --no-start       # install only, do not start the service
#   sudo ./packaging/install.sh --uninstall      # uninstall (keeps the configuration)
#   sudo ./packaging/install.sh --uninstall --purge  # uninstall and delete the configuration
#
# The script builds the release binary, installs it under <prefix>/bin, writes
# the systemd unit to /etc/systemd/system, and starts the service unless
# --no-start is given. It must be run as root.

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
warn() { printf '\033[1;33mWarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix requires a value}"; shift 2 ;;
        --prefix=*) PREFIX="${1#*=}"; shift ;;
        --no-start) NO_START=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) PURGE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done
BINDIR="${PREFIX}/bin"

[ "$(id -u)" -eq 0 ] || die "run as root: sudo $0 $*"

if [ "$UNINSTALL" -eq 1 ]; then
    info "stopping and disabling the service"
    systemctl disable --now "${SERVICE}" 2>/dev/null || true
    rm -f "${UNITDIR}/${SERVICE}"
    systemctl daemon-reload 2>/dev/null || true
    rm -f "${BINDIR}/automountd"
    if [ "$PURGE" -eq 1 ]; then
        warn "removing configuration ${CONFDIR}"
        rm -rf "${CONFDIR}"
    else
        info "keeping configuration ${CONFDIR} (use --purge to remove it)"
    fi
    info "uninstall complete"
    exit 0
fi

command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust first: https://rustup.rs"

info "building release binary"
(
    cd "${ROOT_DIR}"
    cargo build --release
)
BIN="${ROOT_DIR}/target/release/automountd"
[ -x "$BIN" ] || die "build artifact not found: $BIN"

info "installing executable to ${BINDIR}/automountd"
install -Dm755 "$BIN" "${BINDIR}/automountd"

info "installing documentation"
install -Dm644 "${ROOT_DIR}/README.md" "${PREFIX}/share/doc/automountd/README.md"

info "preparing configuration directory ${CONFDIR}"
install -d -m755 "${CONFDIR}"
if [ ! -f "${CONFDIR}/config.conf" ]; then
    "${BINDIR}/automountd" --config "${CONFDIR}/config.conf" config init >/dev/null
    info "wrote default configuration ${CONFDIR}/config.conf"
else
    info "keeping existing configuration ${CONFDIR}/config.conf"
fi

info "installing systemd unit"
install -d -m755 "${UNITDIR}"
sed "s|/usr/local/bin/automountd|${BINDIR}/automountd|g" \
    "${SCRIPT_DIR}/${SERVICE}" > "${UNITDIR}/${SERVICE}"
chmod 644 "${UNITDIR}/${SERVICE}"
systemctl daemon-reload

if [ "$NO_START" -eq 0 ]; then
    info "enabling and starting automountd"
    systemctl enable --now "${SERVICE}"
    sleep 1
    systemctl --no-pager --lines=10 status "${SERVICE}" || true
else
    info "start skipped; later run: systemctl enable --now ${SERVICE}"
fi

cat <<'EOF'

Installation complete. Common commands:
  automountd status            show status
  automountd list --all        list devices
  automountd mount /dev/sdX1   mount a device
  automountd umount /media/... unmount
  automountd doctor            environment check
  automountd tui               interactive UI
  journalctl -u automountd -f  follow logs

EOF
