#!/usr/bin/env bash
# Install or update Dell Fan Control for the current user, no root needed
#
#   scripts/install.sh            build in release mode and install
#   scripts/install.sh --no-build install target/release/dell-fan-control as is
#
# Layout (modelled on the LibrePods deployment on this machine):
#   ~/.local/opt/dell-fan-control/releases/<version>-<commit>/dell-fan-control
#   ~/.local/opt/dell-fan-control/current  -> releases/<new>
#   ~/.local/opt/dell-fan-control/previous -> releases/<old>   (for rollback)
#   ~/.local/bin/dell-fan-control          -> current/dell-fan-control
# Autostart is not enabled here; it is a switch in the window (default off)
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
build=1
[[ ${1:-} == --no-build ]] && build=0

if [[ $EUID -eq 0 ]]; then
  echo "请以普通用户运行，本脚本不需要 root" >&2
  exit 1
fi

cd "$root"
if ((build)); then
  cargo build --release --locked
fi
bin=$root/target/release/dell-fan-control
[[ -x $bin ]] || { echo "找不到 $bin" >&2; exit 1; }

version=$("$bin" --version | awk '{print $2}')
commit=$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo nogit)
dirty=$(git -C "$root" status --porcelain 2>/dev/null | grep -q . && echo -dirty || true)
stamp=$(date +%Y%m%d-%H%M%S)
name="$version-$commit$dirty-$stamp"

opt=$HOME/.local/opt/dell-fan-control
mkdir -p "$opt/releases/$name"
install -m 0755 "$bin" "$opt/releases/$name/dell-fan-control"

if [[ -L $opt/current ]]; then
  ln -sfn "$(readlink "$opt/current")" "$opt/previous"
fi
ln -sfn "releases/$name" "$opt/current"

mkdir -p "$HOME/.local/bin"
ln -sfn "$opt/current/dell-fan-control" "$HOME/.local/bin/dell-fan-control"

unit_dir=$HOME/.config/systemd/user
mkdir -p "$unit_dir"
install -m 0644 packaging/dell-fan-control.service "$unit_dir/dell-fan-control.service"

apps=$HOME/.local/share/applications
mkdir -p "$apps"
sed "s#@EXEC@#$opt/current/dell-fan-control#" packaging/dell-fan-control.desktop.in > "$apps/dell-fan-control.desktop"
chmod 0644 "$apps/dell-fan-control.desktop"

icons=$HOME/.local/share/icons/hicolor
install -Dm 0644 packaging/dell-fan-control.svg "$icons/scalable/apps/dell-fan-control.svg"
if command -v rsvg-convert >/dev/null; then
  for s in 32 48 64 128 256; do
    mkdir -p "$icons/${s}x${s}/apps"
    rsvg-convert -w "$s" -h "$s" packaging/dell-fan-control.svg -o "$icons/${s}x${s}/apps/dell-fan-control.png"
  done
fi
command -v update-desktop-database >/dev/null && update-desktop-database -q "$apps" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t -f "$icons" 2>/dev/null || true

systemctl --user daemon-reload
if systemctl --user is-active --quiet dell-fan-control.service; then
  # stop runs the restore, start re-applies the saved settings with the new build
  systemctl --user restart dell-fan-control.service
  echo "后台服务已重启到新版本"
fi

# keep the five newest releases plus whatever current and previous point to
keep=$(cd "$opt" && { readlink current; readlink previous 2>/dev/null || true; } | xargs -n1 basename)
ls -1t "$opt/releases" | tail -n +6 | while read -r old; do
  grep -qx "$old" <<<"$keep" || rm -r -- "$opt/releases/$old"
done

echo "已安装 $name"
echo "下一步：如需自定义与智能模式，请在有终端的窗口执行 sudo $root/scripts/install-system.sh"
