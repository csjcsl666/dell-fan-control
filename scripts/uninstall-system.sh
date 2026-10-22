#!/usr/bin/env bash
# Remove the udev rule and give the two attributes back to root
#   sudo scripts/uninstall-system.sh
set -euo pipefail
if [[ $EUID -ne 0 ]]; then
  echo "需要 root：sudo $0" >&2
  exit 1
fi
rm -f /etc/udev/rules.d/71-dell-fan-control.rules
udevadm control --reload
for h in /sys/class/hwmon/hwmon*; do
  [[ $(cat "$h/name" 2>/dev/null) == alienware_wmi ]] || continue
  for f in fan1_boost fan2_boost; do
    [[ -e $h/$f ]] && chown root "$h/$f"
  done
  ls -l "$h"/fan*_boost
done
echo "已移除 udev 规则"
