#!/usr/bin/env bash
# Switch current and previous release and restart the service if it runs
set -euo pipefail
opt=$HOME/.local/opt/dell-fan-control
[[ -L $opt/previous ]] || { echo "没有可回滚的上一版本" >&2; exit 1; }
cur=$(readlink "$opt/current")
prev=$(readlink "$opt/previous")
ln -sfn "$prev" "$opt/current"
ln -sfn "$cur" "$opt/previous"
if systemctl --user is-active --quiet dell-fan-control.service; then
  systemctl --user restart dell-fan-control.service
fi
echo "当前版本：$prev（上一版本：$cur）"
