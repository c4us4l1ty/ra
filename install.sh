#!/usr/bin/env bash
#
# ra installer.
#
#   git clone https://github.com/c4us4l1ty/ra
#   cd ra
#   ./install.sh
#
# Then remove the XFCE panels and log back in. That last part is deliberately
# not automated: taking a desktop's panel away before its replacement is
# running is how you end up staring at an empty desktop with no way to get it
# back. ra is designed to sit alongside xfce4-panel until you are happy with it.
#
# Re-running this script is safe: it rebuilds, reinstalls the binary, and
# refreshes the config and stylesheet only if you have not edited them.

set -euo pipefail

BOLD=$'\033[1m'
DIM=$'\033[2m'
RED=$'\033[31m'
GREEN=$'\033[32m'
YELLOW=$'\033[33m'
RESET=$'\033[0m'

say()  { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
info() { printf '    %s%s%s\n' "$DIM" "$*" "$RESET"; }
ok()   { printf '%s  ok%s %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die()  { printf '%serror%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/ra"
DATA_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ra"
CONFIG_FILE="$CONFIG_DIR/config.toml"
STYLESHEET="$CONFIG_DIR/style.css"

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ---------------------------------------------------------------------------
# 1. Build dependencies
# ---------------------------------------------------------------------------

say "Checking build dependencies"

need_pkg() {
  local pkgs="$1" import="$2" label="$3" hint="$4"
  if pkg-config --exists "$import" 2>/dev/null; then
    ok "$label"
    return
  fi
  printf '    %s needs %s (missing: %s)%s\n' "$label" "$import" "$pkgs" "$hint" >&2
  MISSING_DEPS=1
}

MISSING_DEPS=0
need_pkg "gtk4-devel"            "gtk4"                     "gtk4"              ""
need_pkg "gtk4-layer-shell-devel" "gtk4-layer-shell-0"      "gtk4-layer-shell"  ""

if ! command -v cargo >/dev/null 2>&1; then
  printf '    cargo is missing\n' >&2
  MISSING_DEPS=1
fi

if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then
  printf '    a C compiler (gcc or clang) is missing\n' >&2
  MISSING_DEPS=1
fi

if [[ $MISSING_DEPS -eq 1 ]]; then
  echo >&2
  warn "Install the packages above, then run this script again."
  cat >&2 <<'EOF'

    Fedora / RHEL:   sudo dnf install gtk4-devel gtk4-layer-shell-devel rust cargo gcc
    Debian / Ubuntu: sudo apt install libgtk-4-dev libgtk4-layer-shell-dev cargo gcc
    Arch:            sudo pacman -S gtk4 gtk4-layer-shell rust gcc
EOF
  exit 1
fi

# ---------------------------------------------------------------------------
# 2. Runtime dependencies
# ---------------------------------------------------------------------------
#
# These are what ra talks to. It degrades gracefully without them, but a
# feature that silently does nothing is worse than being told up front.

say "Checking runtime dependencies"

have() { command -v "$1" >/dev/null 2>&1; }

RUNTIME_MISSING=()
check_runtime() {
  local label="$1" cmd="$2" why="$3"
  if have "$cmd"; then
    ok "$label"
  else
    RUNTIME_MISSING+=("$label")
    warn "$label not found — $why"
  fi
}

check_runtime "NetworkManager" nmcli "Wi-Fi and network tiles will be disabled"
check_runtime "WirePlumber"   wpctl  "volume tiles will be disabled"
check_runtime "bluetoothd"    bluetoothctl "Bluetooth tiles will be disabled"

# Optional: only affects whether the night-light tile can do anything.
if have gamut || have gammastep || have wl-gammastep || have redshift || have sct || have hyprsunset; then
  ok "night light back-end"
else
  warn "no night-light back-end (gamut, gammastep, wl-gammastep, redshift or sct)"
fi

# Optional: brightness needs a backlight device, most desktops do not have one.
if [[ -d /sys/class/backlight ]] && compgen -G "/sys/class/backlight/*/brightness" >/dev/null; then
  ok "backlight"
else
  info "no backlight device; the brightness slider will be hidden"
fi

# ---------------------------------------------------------------------------
# 3. Build
# ---------------------------------------------------------------------------

say "Building (this takes a few minutes the first time)"
info "cargo build --release --locked"

if ! command -v cargo >/dev/null 2>&1; then
  die "cargo is not on PATH"
fi

# `--locked` only when a lockfile exists, so a fresh clone still builds.
if [[ -f Cargo.lock ]]; then
  cargo build --release --locked
else
  cargo build --release
fi

BINARY="$REPO_DIR/target/release/ra"
[[ -x "$BINARY" ]] || die "the build produced no binary at $BINARY"

# ---------------------------------------------------------------------------
# 4. Install
# ---------------------------------------------------------------------------

say "Installing to $BIN_DIR/ra"
mkdir -p "$BIN_DIR"
install -m 0755 "$BINARY" "$BIN_DIR/ra"
ok "$("$BIN_DIR/ra" --version)"

# ---------------------------------------------------------------------------
# 5. Configuration
# ---------------------------------------------------------------------------

mkdir -p "$CONFIG_DIR" "$DATA_DIR"

write_if_absent() {
  local target="$1" source="$2"
  if [[ -e "$target" ]]; then
    info "kept your existing $(basename "$target")"
  else
    install -m 0644 "$source" "$target"
    ok "installed $(basename "$target")"
  fi
}

write_if_absent "$CONFIG_FILE"   "$REPO_DIR/config/ra.toml"
write_if_absent "$STYLESHEET"    "$REPO_DIR/config/style.css"

# ---------------------------------------------------------------------------
# 6. Autostart
# ---------------------------------------------------------------------------

say "Registering autostart"

autostart_installed=0

# XFCE reads ~/.config/autostart for every session type, including the Wayland
# one, so this is the portable choice.
XFCE_AUTOSTART="${XDG_CONFIG_HOME:-$HOME/.config}/autostart/ra.desktop"
mkdir -p "$(dirname "$XFCE_AUTOSTART")"
cat > "$XFCE_AUTOSTART" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=ra panel
Comment=Layer-shell panel replacing xfce4-panel
Exec=$BIN_DIR/ra
TryExec=$BIN_DIR/ra
Terminal=false
NoDisplay=false
X-GNOME-Autostart-enabled=true
X-XFCE-Autostart=true
EOF
ok "XFCE autostart"
autostart_installed=1

# labwc also has its own autostart hook, which is what actually runs it under
# the XFCE Wayland session (that session's compositor is labwc).
LABWC_AUTOSTART="${XDG_CONFIG_HOME:-$HOME/.config}/labwc/autostart"
if [[ -d "$LABWC_AUTOSTART" || -d "${XDG_RUNTIME_DIR:-/nonexistent}" ]]; then
  mkdir -p "$LABWC_AUTOSTART"
  # Only add our line if it is not already there, so re-running is idempotent.
  if [[ -f "$LABWC_AUTOSTART/ra" ]] || grep -qs 'ra$' "$LABWC_AUTOSTART"; then
    info "labwc autostart already mentions ra"
  else
    printf '%s/ra >/dev/null 2>&1 &\n' "$BIN_DIR" > "$LABWC_AUTOSTART/ra"
    chmod +x "$LABWC_AUTOSTART/ra"
    ok "labwc autostart"
  fi
  autostart_installed=1
fi

# ---------------------------------------------------------------------------
# 7. Summary
# ---------------------------------------------------------------------------

cat <<EOF

${BOLD}ra is installed.${RESET}

  binary     $BIN_DIR/ra
  config     $CONFIG_FILE
  stylesheet $STYLESHEET
  state      $DATA_DIR/state.toml   (do-not-disturb, night light, dark mode)

${BOLD}Next steps${RESET}

  1. Start it now:

       $BIN_DIR/ra

     On XFCE/Wayland it pins itself to the screen edge. On any other
     compositor (GNOME, X11) it opens as an ordinary window, which is enough
     to try everything out.

  2. Check what it can see:

       $BIN_DIR/ra --diagnose

  3. When you are happy with it, remove the XFCE panels:

       xfconf-query -c xfce4-panel -p /panels -a

     ${DIM}That prints the panels to delete. Remove each one with:${RESET}

       xfconf-query -c xfce4-panel -p /panels -t int -n <number> -r

     ${DIM}then log out and back in. Keep at least one panel until you have${RESET}
     ${DIM}confirmed ra starts on login - that is the only way back if it does not.${RESET}

  4. Edit the config; it reloads as you save:

       \$EDITOR $CONFIG_FILE

EOF

if [[ ${#RUNTIME_MISSING[@]} -gt 0 ]]; then
  warn "Optional services not found: ${RUNTIME_MISSING[*]}"
  warn "Their tiles will be disabled. ra --diagnose shows what is available."
fi

say "Done"