#!/usr/bin/env bash
# One time system part, run with sudo by the user who will control the fans:
#
#   sudo scripts/install-system.sh
#
# It installs one udev rule that gives that user write access to exactly two
# files: fan1_boost and fan2_boost of the alienware_wmi hwmon device. No
# daemon runs as root, nothing else changes owner, no capability is granted
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "需要 root：sudo $0" >&2
  exit 1
fi
user=${1:-${SUDO_USER:-}}
[[ -n $user && $user != root ]] || { echo "无法确定目标用户，请写成 sudo $0 <用户名>" >&2; exit 1; }
uid=$(id -u "$user")

root=$(cd "$(dirname "$0")/.." && pwd)
rule=/etc/udev/rules.d/71-dell-fan-control.rules
sed "s#@UID@#$uid#g" "$root/packaging/71-dell-fan-control.rules.in" > "$rule.tmp"
chmod 0644 "$rule.tmp"
mv "$rule.tmp" "$rule"
udevadm control --reload

# apply to the device that already exists, same two files only
found=0
for h in /sys/class/hwmon/hwmon*; do
  [[ $(cat "$h/name" 2>/dev/null) == alienware_wmi ]] || continue
  found=1
  for f in fan1_boost fan2_boost; do
    [[ -e $h/$f ]] && chown "$uid" "$h/$f"
  done
  ls -l "$h"/fan*_boost
done
((found)) || echo "警告：当前没有 alienware_wmi hwmon 设备，规则将在驱动加载时生效" >&2
echo "已为用户 $user（uid $uid）安装 $rule"
