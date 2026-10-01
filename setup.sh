#!/bin/sh
# One-shot setup for this machine. Safe to re-run.
#   1. installs protectord as a systemd user service (armed, starts at login)
#   2. stops the ASMedia USB4 chip (PCI vendor 0x1b21) from sleeping — it
#      failed to wake on 2026-10-02 and froze the machine (asks for sudo)
#
#   sh setup.sh                 # both steps
#   sh setup.sh --auto-reboot   # also: clean reboot after a 3 min kernel stall
set -eu
cd "$(dirname "$0")"
[ "$(id -u)" != 0 ] || { echo "run as yourself, not root — it asks for sudo when needed" >&2; exit 1; }

RULE=/etc/udev/rules.d/80-asmedia-no-sleep.rules
CONFIG="${PROTECTOR_HOME:-$HOME/.protector}/config.json"

echo "== 1/2 protector service"
sh service/install.sh
# The first start writes the config; only then can the option be set.
i=0
while [ ! -f "$CONFIG" ] && [ $i -lt 10 ]; do sleep 1; i=$((i + 1)); done
if [ "${1:-}" = --auto-reboot ] && [ -f "$CONFIG" ]; then
  sed -i 's/"stall_reboot_s": *[0-9.]*/"stall_reboot_s": 180.0/' "$CONFIG"
  systemctl --user restart protector
fi
grep -H stall_reboot_s "$CONFIG" || echo "no $CONFIG yet — the service writes it on first start"

echo "== 2/2 keep the USB4 chip awake"
sync # waking the chip is the step that failed once — flush to disk first
echo 'ACTION=="add", SUBSYSTEM=="pci", ATTR{vendor}=="0x1b21", ATTR{power/control}="on"' |
  sudo tee "$RULE" >/dev/null
sudo udevadm control --reload
for d in /sys/bus/pci/devices/*; do
  [ "$(cat "$d/vendor")" = 0x1b21 ] || continue
  echo on | sudo tee "$d/power/control" >/dev/null
  echo "$(basename "$d") control=$(cat "$d/power/control") status=$(cat "$d/power/runtime_status")"
done
echo "done — every line above should say control=on"
