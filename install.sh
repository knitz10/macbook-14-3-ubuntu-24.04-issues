#!/bin/bash
# Install tiny-dfr-fix bundle to system paths. Review before running.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"

if [[ $EUID -ne 0 ]]; then
  echo "Run as root: sudo $0"
  exit 1
fi

echo "==> Installing appletbdrm (DKMS)"
mkdir -p /usr/src/appletbdrm-1.2
cp "$ROOT/appletbdrm/appletbdrm.c" "$ROOT/appletbdrm/dkms.conf" /usr/src/appletbdrm-1.2/
dkms remove appletbdrm/1.2 --all 2>/dev/null || true
dkms add -m appletbdrm -v 1.2
dkms build appletbdrm/1.2
dkms install appletbdrm/1.2 --force

echo "==> Installing tiny-dfr binary"
install -m 755 "$ROOT/bin/tiny-dfr" /usr/bin/tiny-dfr

echo "==> Installing udev rules"
cp "$ROOT/udev/"*.rules /etc/udev/rules.d/
udevadm control --reload-rules

echo "==> Installing systemd units"
cp "$ROOT/systemd/"*.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable macbook-quirks.service tiny-dfr.service

echo "==> Installing suspend/resume sleep hooks"
mkdir -p /etc/systemd/system-sleep
install -m 755 "$ROOT/systemd/sleep/touchbar-resume" /etc/systemd/system-sleep/touchbar-resume
install -m 755 "$ROOT/systemd/sleep/wifi-resume"     /etc/systemd/system-sleep/wifi-resume

if [[ -f "$ROOT/libinput/local-overrides.quirks" ]]; then
  mkdir -p /etc/libinput
  cp "$ROOT/libinput/local-overrides.quirks" /etc/libinput/
fi

if [[ ! -f /etc/tiny-dfr/config.toml ]]; then
  echo "==> Creating /etc/tiny-dfr/config.toml from template"
  mkdir -p /etc/tiny-dfr
  if [[ -f /usr/share/tiny-dfr/config.toml ]]; then
    cp /usr/share/tiny-dfr/config.toml /etc/tiny-dfr/config.toml
  fi
fi

echo "==> Done. Reboot recommended."
echo "    systemctl start macbook-quirks tiny-dfr   # or reboot"
