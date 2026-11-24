#!/usr/bin/env bash
# Build and install jff-portal (run from anywhere, paths are relative to this script).
#
#   ./install.sh            build and install
#   ./install.sh --no-restart   do not restart the portal services afterwards
#   PREFIX=/usr/local ./install.sh
#
# Needs the PipeWire development files (libpipewire) and clang (for the bindings).
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

PREFIX="${PREFIX:-/usr}"
RESTART=1
[[ "${1:-}" == "--no-restart" ]] && RESTART=0

command -v cargo >/dev/null || {
  echo "cargo not found" >&2
  exit 1
}
if command -v pkg-config >/dev/null && ! pkg-config --exists libpipewire-0.3; then
  echo "warning: libpipewire-0.3 not found by pkg-config; the build will probably fail" >&2
  echo "         (Arch/CachyOS: pacman -S pipewire clang)" >&2
fi

echo "==> building"
cargo build --release

# Run the installs as root only when needed.
SUDO=""
[[ $EUID -ne 0 ]] && SUDO="sudo"

echo "==> installing to $PREFIX"
$SUDO install -Dm755 target/release/jff-portal "$PREFIX/bin/jff-portal"
$SUDO install -Dm644 data/jff.portal "$PREFIX/share/xdg-desktop-portal/portals/jff.portal"
$SUDO install -Dm644 data/org.freedesktop.impl.portal.desktop.jff.service \
  "$PREFIX/share/dbus-1/services/org.freedesktop.impl.portal.desktop.jff.service"
$SUDO install -Dm644 data/xdg-desktop-portal-jff.service \
  "$PREFIX/lib/systemd/user/xdg-desktop-portal-jff.service"

# The unit and the D-Bus service file name the binary by path: warn if PREFIX moved it.
if [[ "$PREFIX" != "/usr" ]]; then
  echo "note: the .service files in data/ may point at /usr/bin/jff-portal; check ExecStart/Exec" >&2
fi

if [[ $RESTART -eq 1 ]]; then
  echo "==> restarting the portal"
  systemctl --user daemon-reload || true
  # A running old jff-portal would keep serving the old binary.
  systemctl --user restart xdg-desktop-portal-jff.service 2>/dev/null || pkill -x jff-portal || true
  systemctl --user restart xdg-desktop-portal.service || true
fi

echo "done. Restart the app that shares the screen (OBS, browser)."
