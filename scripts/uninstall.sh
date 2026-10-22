#!/usr/bin/env bash
# Remove the per user installation. Fans are handed back to the firmware
# first. Settings and logs are kept unless --purge is given
set -euo pipefail
purge=0
[[ ${1:-} == --purge ]] && purge=1
opt=$HOME/.local/opt/dell-fan-control
bin=$opt/current/dell-fan-control

if systemctl --user is-active --quiet dell-fan-control.service; then
  "$bin" quit || true
fi
systemctl --user disable --now dell-fan-control.service 2>/dev/null || true
# whatever is still recorded (for example after a crash) is undone here
[[ -x $bin ]] && "$bin" restore || true

rm -f "$HOME/.config/systemd/user/dell-fan-control.service"
systemctl --user daemon-reload
rm -f "$HOME/.local/share/applications/dell-fan-control.desktop"
find "$HOME/.local/share/icons/hicolor" -name 'dell-fan-control.*' -delete 2>/dev/null || true
[[ -L $HOME/.local/bin/dell-fan-control ]] && rm -f "$HOME/.local/bin/dell-fan-control"
rm -rf -- "$opt"
if ((purge)); then
  rm -rf -- "$HOME/.config/dell-fan-control" "$HOME/.local/state/dell-fan-control"
fi
echo "已卸载。udev 规则需另外执行 sudo scripts/uninstall-system.sh"
