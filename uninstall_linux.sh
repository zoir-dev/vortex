#!/usr/bin/env bash
#
# uninstall_linux.sh — remove everything install_linux.sh put on this machine.
#
# There was no way back. `install_linux.sh` writes a binary, icons, an app-menu
# entry, an autostart entry, file-manager integrations, a GNOME extension and a
# custom Super+V keybinding — and deleting the binary by hand leaves an
# autostart entry that fails at every login, a keybinding pointing at nothing,
# and a shell extension for an app that is gone.
#
# Two things are deliberately NOT removed unless you ask:
#   • your pairing and settings (--purge does that)
#   • the polkit unlock rule, which needs sudo and has its own remover:
#       sudo linux/packaging/install-unlock-rule.sh --remove
#
# Usage:
#   ./uninstall_linux.sh            remove the app, keep your data
#   ./uninstall_linux.sh --purge    also delete settings, caches and pairing
set -euo pipefail

PURGE=0
for a in "$@"; do
  case "$a" in
    --purge) PURGE=1 ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  esac
done

BIN_DIR="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}"
GNOME_EXT_UUID="vortex-live@vortex"

echo "▶ stopping Vortex…"
# Exact name match only — `pkill -f` would match this script's own command line.
pkill -x vortex-ui-tauri 2>/dev/null || true
sleep 1
pkill -9 -x vortex-ui-tauri 2>/dev/null || true

echo "▶ removing the autostart entry (this is what would otherwise fail every login)…"
rm -f "$CONFIG/autostart/vortex-ui-tauri.desktop"

echo "▶ removing the app-menu entry, binary and icons…"
rm -f "$DATA/applications/vortex-ui-tauri.desktop"
rm -f "$BIN_DIR/vortex-ui-tauri"
rm -f "$DATA"/icons/hicolor/*/apps/vortex-ui-tauri.png
command -v update-desktop-database >/dev/null 2>&1 &&
  update-desktop-database "$DATA/applications" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 &&
  gtk-update-icon-cache -f -t "$DATA/icons/hicolor" 2>/dev/null || true

echo "▶ removing file-manager integration…"
rm -f "$DATA/nautilus-python/extensions/vortex_share.py"
rm -f "$DATA/kio/servicemenus/vortex-share.desktop"
rm -f "$DATA/kservices5/ServiceMenus/vortex-share.desktop"

echo "▶ removing the GNOME extension…"
if command -v gnome-extensions >/dev/null 2>&1; then
  gnome-extensions disable "$GNOME_EXT_UUID" 2>/dev/null || true
fi
rm -rf "$DATA/gnome-shell/extensions/$GNOME_EXT_UUID"

# The Super+V binding: the installer added a custom keybinding AND repointed
# GNOME's own toggle-message-tray. Put both back rather than leaving a shortcut
# that opens nothing.
if command -v gsettings >/dev/null 2>&1; then
  echo "▶ restoring the Super+V shortcut…"
  base="org.gnome.settings-daemon.plugins.media-keys"
  list=$(gsettings get "$base" custom-keybindings 2>/dev/null || echo "@as []")
  if printf '%s' "$list" | grep -q "vortex"; then
    cleaned=$(python3 - "$list" <<'PY' 2>/dev/null || echo ""
import ast, sys
try:
    items = ast.literal_eval(sys.argv[1].replace("@as ", ""))
except Exception:
    sys.exit(1)
print(str([i for i in items if "vortex" not in i]).replace("'", '"'))
PY
)
    [ -n "$cleaned" ] && gsettings set "$base" custom-keybindings "$cleaned" 2>/dev/null || true
  fi
  gsettings reset "$base" toggle-message-tray 2>/dev/null || true
fi

if [ "$PURGE" -eq 1 ]; then
  echo "▶ --purge: deleting settings, caches and PAIRING…"
  rm -rf "$CONFIG/vortex" "$DATA/vortex" "$CACHE/vortex"
  echo "  (identity and trusted peers live in the login keyring; remove the"
  echo "   'vortex' entries there with seahorse if you want them gone too)"
else
  echo "ℹ Settings and pairing kept. Re-run with --purge to delete them."
fi

echo
echo "✓ Vortex removed."
if [ -f /etc/polkit-1/rules.d/49-vortex-unlock.rules ]; then
  echo "ℹ The unlock permission is still installed. To remove it:"
  echo "    sudo linux/packaging/install-unlock-rule.sh --remove"
fi
