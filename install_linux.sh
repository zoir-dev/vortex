#!/usr/bin/env bash
#
# install_linux.sh — build the Vortex laptop app and install it for the
# current user, with autostart on login.
#
# WHY a prod build (not `cargo run`): the Tauri prod binary EMBEDS the compiled
# Vue frontend, so the webview loads standalone (no Vite dev-server, no
# "connection refused"). --no-bundle skips the slow .deb/AppImage packaging —
# we only want the binary.
#
# What it does:
#   • installs every system dependency (sudo, once)  → packaging/install-deps.sh
#   • builds linux/ui-tauri  → release binary
#   • installs the binary    → ~/.local/bin/vortex-ui-tauri        (user-level)
#   • installs the icon      → ~/.local/share/icons/.../vortex-ui-tauri.png
#   • app-menu entry         → ~/.local/share/applications/
#   • autostart entry        → ~/.config/autostart/  (starts with the session)
#   • Nautilus "Share via Vortex" → ~/.local/share/nautilus-python/extensions/
#   • KDE Dolphin "Share via Vortex" ServiceMenu (when Dolphin is present)
#   • GNOME "Live Activities" pill extension (GNOME only) → installed + enabled
#   • restarts the running instance to the freshly installed binary
#
# Goal: the user does NOTHING by hand — deps, app, file-manager integration and
# the GNOME pill extension all land in one run.
#
# Flags:
#   --skip-deps   don't touch the package manager (deps already present)
#   --ask         interactive dependency install (the default asks NOTHING —
#                 -y everywhere and debconf silenced, so one run needs zero input)
#   --yes         accepted for compatibility (already the default)
#
# Re-run any time to update; it replaces the binary and restarts the app.
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"

REPO="$(cd "$(dirname "$0")" && pwd)"
UI="$REPO/linux/ui-tauri"
BIN_SRC="$UI/src-tauri/target/release/vortex-ui-tauri"
ICON_SRC="$UI/src-tauri/icons/icon.png"          # 512px brand logo

BIN_DIR="$HOME/.local/bin"
ICON_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
BIN_DST="$BIN_DIR/vortex-ui-tauri"

SKIP_DEPS=0
DEPS_ARGS=()
for a in "$@"; do
  case "$a" in
    --skip-deps) SKIP_DEPS=1 ;;
    --yes)       DEPS_ARGS+=(--yes) ;;
    --ask)       DEPS_ARGS+=(--ask) ;;
  esac
done

# ── 0. system dependencies (the only step that needs sudo) ────────────────────
# Auto-installs webkit2gtk, gstreamer, dbus, protobuf, pactl, NetworkManager,
# zenity, adb, nautilus-python, Pillow … plus Rust (rustup) if absent. Distro-
# aware (apt/dnf/pacman/zypper). Best-effort: never aborts the install.
if [ "$SKIP_DEPS" -eq 0 ]; then
  bash "$REPO/linux/packaging/install-deps.sh" "${DEPS_ARGS[@]}" || \
    echo "⚠ dependency step had issues — continuing; the build will flag anything truly missing."
  # rustup drops cargo at ~/.cargo but doesn't touch THIS shell's PATH — pull it
  # in so the build below finds cargo on a first-ever install.
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
else
  echo "▶ --skip-deps: assuming system dependencies are already installed."
fi

# UI dependencies. node_modules is gitignored, so a fresh clone has none — and
# the `tauri` CLI lives there, so the build below would die with "tauri: command
# not found". Install with pnpm to honor pnpm-lock.yaml when it's available
# (dev machines, Arch's `pnpm` pkg); otherwise fall back to npm, which is always
# present. We avoid `npm i -g pnpm` on purpose — a global install needs root on
# most distros. The npm fallback MUST pass --legacy-peer-deps: this is a pnpm
# project and one dep (vue-router) declares an optional peer on a newer vite than
# we pin; pnpm ignores unmet optional peers, but npm v10 hard-fails the whole
# install on the conflict. --legacy-peer-deps accepts the resolution pnpm already
# validated (verified building in an Arch container).
echo "▶ installing UI dependencies…"
( cd "$UI"
  if command -v pnpm >/dev/null 2>&1; then
    pnpm install --frozen-lockfile || pnpm install
  else
    echo "  · pnpm not found — using npm with --legacy-peer-deps"
    npm install --legacy-peer-deps
  fi )

echo "▶ building prod binary (embeds UI; --no-bundle skips packaging)…"
( cd "$UI" && npm run tauri build -- --no-bundle )
[ -x "$BIN_SRC" ] || { echo "✗ build produced no binary at $BIN_SRC" >&2; exit 1; }

echo "▶ installing files (user-level, no sudo)…"
mkdir -p "$BIN_DIR" "$APP_DIR" "$AUTOSTART_DIR"
install -m755 "$BIN_SRC" "$BIN_DST"

# Install the brand logo at every standard hicolor size so the launcher /
# dock / overview always finds it — a single size sometimes leaves GNOME
# falling back to a generic gear. Downscale with Pillow when present; else just
# drop the 512px source into the 256 bucket.
if command -v python3 >/dev/null 2>&1 && python3 -c 'import PIL' 2>/dev/null; then
  for s in 32 48 64 128 256 512; do
    d="$ICON_ROOT/${s}x${s}/apps"; mkdir -p "$d"
    python3 -c "from PIL import Image; Image.open('$ICON_SRC').convert('RGBA').resize(($s,$s), Image.LANCZOS).save('$d/vortex-ui-tauri.png')"
  done
else
  d="$ICON_ROOT/256x256/apps"; mkdir -p "$d"
  install -m644 "$ICON_SRC" "$d/vortex-ui-tauri.png"
fi
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -f -t "$ICON_ROOT" 2>/dev/null || true

# App-menu launcher. StartupWMClass = the app's Wayland/X11 id (the binary
# name) so GNOME links the RUNNING window to this entry and shows the logo in
# the dock/overview instead of a generic fallback.
cat > "$APP_DIR/vortex-ui-tauri.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Vortex
Comment=Phone companion — calls, messages, notifications and earbuds hand-off
Exec=env GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 $BIN_DST
Icon=vortex-ui-tauri
Terminal=false
Categories=Utility;Network;
StartupNotify=false
StartupWMClass=vortex-ui-tauri
EOF

# Autostart: the app lives in the tray and owns the BLE/LAN link, so bring it
# up with the session. The delay lets BlueZ / Secret Service / the tray host
# settle first. GDK_BACKEND=x11 (XWayland) makes the title-bar buttons respond
# on the first click under GNOME fractional scaling; WEBKIT_DISABLE_DMABUF_RENDERER=1
# avoids a blank WebKitGTK window on some GPU stacks.
cat > "$AUTOSTART_DIR/vortex-ui-tauri.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Vortex
Comment=Phone companion — starts with the session
Exec=env GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 $BIN_DST --hidden
Icon=vortex-ui-tauri
Terminal=false
Categories=Utility;Network;
StartupNotify=false
StartupWMClass=vortex-ui-tauri
X-GNOME-Autostart-enabled=true
X-GNOME-Autostart-Delay=8
EOF

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APP_DIR" 2>/dev/null || true

# GNOME Files (Nautilus) right-click "Share via Vortex" → forwards the file to
# the running app via `--share`. Resolves the binary at runtime so it never
# goes stale. Best-effort: harmless if nautilus-python isn't installed.
NAUTILUS_EXT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/nautilus-python/extensions"
mkdir -p "$NAUTILUS_EXT_DIR"
install -m644 "$REPO/linux/packaging/nautilus/vortex.py" "$NAUTILUS_EXT_DIR/vortex.py"
nautilus -q >/dev/null 2>&1 || true   # reload the extension on next open

# KDE Dolphin right-click "Share via Vortex" — the ServiceMenu counterpart of
# the Nautilus extension above, so KDE/Plasma users get the same one-click
# share. Installed only when Dolphin is actually present (KDE, or Dolphin on
# another DE). Written to BOTH the Plasma 6 (kio/servicemenus) and Plasma 5
# (kservices5/ServiceMenus) locations so it works on either version. The binary
# path is baked in — a ServiceMenu's Exec doesn't reliably have ~/.local/bin on
# PATH. %F passes the selected files AND folders (we share folders too), hence
# MimeType=all/all. Plasma 6 requires servicemenu files to be executable.
if command -v dolphin >/dev/null 2>&1; then
  for KDE_SM_DIR in \
      "${XDG_DATA_HOME:-$HOME/.local/share}/kio/servicemenus" \
      "${XDG_DATA_HOME:-$HOME/.local/share}/kservices5/ServiceMenus"; do
    mkdir -p "$KDE_SM_DIR"
    cat > "$KDE_SM_DIR/vortex-share.desktop" <<EOF
[Desktop Entry]
Type=Service
MimeType=all/all;
Actions=vortexShare;
X-KDE-Priority=TopLevel

[Desktop Action vortexShare]
Name=Share via Vortex
Icon=vortex-ui-tauri
Exec=$BIN_DST --share %F
EOF
    chmod +x "$KDE_SM_DIR/vortex-share.desktop"
  done
  echo "▶ KDE Dolphin 'Share via Vortex' installed."
fi

# GNOME "Live Activities" pill extension. The phone's navigation/ride/delivery
# pills render in the top bar ONLY through this GNOME Shell extension, so it's
# GNOME-only by nature — on KDE/XFCE/etc. the daemon falls back to a tray
# readout and we skip this entirely (nothing to install there).
GNOME_EXT_UUID="vortex-live@vortex"
GNOME_EXT_SRC="$REPO/linux/gnome-extension/$GNOME_EXT_UUID"
if command -v gnome-shell >/dev/null 2>&1 \
   && printf '%s' "${XDG_CURRENT_DESKTOP:-}" | grep -qi gnome \
   && [ -d "$GNOME_EXT_SRC" ]; then
  GNOME_EXT_DST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$GNOME_EXT_UUID"
  mkdir -p "$GNOME_EXT_DST"
  install -m644 "$GNOME_EXT_SRC"/metadata.json "$GNOME_EXT_SRC"/extension.js "$GNOME_EXT_SRC"/stylesheet.css "$GNOME_EXT_DST/"
  # Enable it. On X11 this takes effect immediately. On a Wayland session a
  # FIRST-EVER install is invisible to the ALREADY-RUNNING shell, so
  # `gnome-extensions enable` fails with "does not exist" — and, crucially, it
  # is a D-Bus call into that shell, so a failure writes NOTHING to dconf and
  # nothing is queued. Falling back to writing org.gnome.shell enabled-extensions
  # ourselves is what actually makes it come up enabled after the next login;
  # without it the extension sits installed-but-off forever and the daemon stays
  # on its tray fallback. gnome-extensions may be absent on a minimal GNOME, so
  # guard it and let the gsettings path carry the install on its own.
  if command -v gnome-extensions >/dev/null 2>&1 \
     && gnome-extensions enable "$GNOME_EXT_UUID" >/dev/null 2>&1; then
    echo "▶ GNOME pill extension installed + enabled."
  elif command -v gsettings >/dev/null 2>&1; then
    # Append to the list only when absent — re-running the installer must not
    # grow a duplicate entry. python3 keeps the GVariant list quoting correct.
    if gsettings get org.gnome.shell enabled-extensions | grep -q "'$GNOME_EXT_UUID'"; then
      echo "ℹ GNOME pill extension updated; already queued to enable — log out/in once."
    elif python3 - "$GNOME_EXT_UUID" <<'PY' 2>/dev/null
import subprocess, sys, ast
uuid = sys.argv[1]
cur = subprocess.check_output(
    ["gsettings", "get", "org.gnome.shell", "enabled-extensions"], text=True).strip()
lst = ast.literal_eval(cur.replace("@as ", "", 1))
lst.append(uuid)
subprocess.check_call(["gsettings", "set", "org.gnome.shell", "enabled-extensions",
                       "[" + ", ".join("'%s'" % e for e in lst) + "]"])
PY
    then
      echo "▶ GNOME pill extension installed and marked enabled — log out/in once to see the pill."
    else
      echo "ℹ GNOME pill extension installed; enable it with: gnome-extensions enable $GNOME_EXT_UUID"
    fi
  else
    echo "ℹ GNOME pill extension installed (enable with: gnome-extensions enable $GNOME_EXT_UUID)."
  fi
else
  echo "ℹ Not a GNOME session — skipping the pill extension (the app uses its tray fallback here)."
fi

echo "▶ restarting the app on the installed binary…"
# Exact process-NAME match only — `pkill -f vortex-ui-tauri` would self-match
# this script's own command line and kill the wrong thing.
pkill -9 -x vortex-ui-tauri 2>/dev/null || true
sleep 1
# Same environment as the .desktop entries above — GDK_BACKEND=x11 included.
# Leaving it out here meant the freshly installed app ran on native Wayland
# while every later launch ran on XWayland, so anything that depends on the
# X11 path (raising the window to the front from the tray) silently did
# nothing until the user next logged out and back in.
setsid env GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 "$BIN_DST" >/dev/null 2>&1 < /dev/null &
disown 2>/dev/null || true

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "ℹ note: $BIN_DIR is not on your PATH (the app still runs; only matters for typing 'vortex-ui-tauri')." ;;
esac

echo "✓ installed. Running now, and it will auto-start on every login."

# Remote UNLOCK (phone-presence unlock, and the phone's unlock button) is the
# one feature that needs a system-level permission: logind gates Session.Unlock
# behind polkit. We never write to /etc behind the user's back — everything
# above this line lives under $HOME — so it stays opt-in and is named here
# instead. Locking works without it.
if [ ! -f /etc/polkit-1/rules.d/49-vortex-unlock.rules ]; then
  echo
  echo "ℹ Unlocking the laptop from the phone needs a one-time permission."
  echo "  Without it the laptop still LOCKS, but never unlocks. To enable it:"
  echo "      sudo $REPO/linux/packaging/install-unlock-rule.sh"
  echo "  (it prints exactly what it grants, and --remove undoes it)"
fi
