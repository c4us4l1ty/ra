#!/usr/bin/env bash
#
# Remove ra. Leaves the configuration in place unless you ask it not to.

set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/ra"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ra"

PURGE=0
[[ "${1:-}" == "--purge" ]] && PURGE=1

for file in \
  "$BIN_DIR/ra" \
  "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/ra.desktop" \
  "${XDG_CONFIG_HOME:-$HOME/.config}/labwc/autostart/ra"
do
  if [[ -e "$file" ]]; then
    rm -f "$file"
    echo "removed $file"
  fi
done

if [[ $PURGE -eq 1 ]]; then
  rm -rf "$CONFIG_DIR" "$STATE_DIR"
  echo "removed $CONFIG_DIR and $STATE_DIR"
else
  echo "kept $CONFIG_DIR (pass --purge to remove it too)"
fi

echo
echo "ra is gone. The XFCE panels are untouched; re-enable them with:"
echo "  xfce4-panel --restart"
