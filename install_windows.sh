#!/usr/bin/env bash
#
# install_windows.sh — cross-build the Vortex laptop app for Windows, from Linux.
#
# Produces `vortex-ui-tauri.exe` (and optionally an NSIS installer) without ever
# touching a Windows machine. Run it on the same checkout you build Linux from:
#
#     ./install_windows.sh                 # deps + .exe
#     ./install_windows.sh --installer     # also the NSIS setup.exe
#     ./install_windows.sh --skip-deps     # I already have the toolchain
#
# WHY the MSVC target and not `x86_64-pc-windows-gnu`, which needs no downloads:
# the app talks to WinRT through the `windows` crate (Bluetooth LE lives in
# Devices_Bluetooth), and Tauri links WebView2. Both are built against the MSVC
# ABI; the GNU target is a different ABI and does not link them. So the MSVC
# target it is, which means we need Microsoft's CRT and SDK — that is the one
# thing a Linux box does not have, and exactly what `cargo-xwin` fetches.
#
# What gets installed:
#   • clang / lld / llvm  — cargo-xwin drives clang-cl, lld-link and llvm-rc
#   • rustup target x86_64-pc-windows-msvc
#   • cargo-xwin          — downloads the MSVC CRT + Windows SDK on first build
#   • node/npm            — only if missing (same reasoning as install-deps.sh)
#   • makensis            — ONLY with --installer
#
# What you get is a Windows build of everything except the Linux-only
# subsystems. Screen mirror/cast, the continuity camera and the earbuds
# hand-off are GStreamer/GTK/PulseAudio/BlueZ and are compiled out (see the
# "Linux-only subsystems" gate in linux/ui-tauri/src-tauri/src/lib.rs). Pair,
# reconnect, notifications, clipboard, file transfer and Universal Control all
# build.
set -uo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BOLD='\033[1m'; NC='\033[0m'
ok()   { printf "${GREEN}✓ %s${NC}\n" "$1"; }
warn() { printf "${YELLOW}⚠ %s${NC}\n" "$1"; }
err()  { printf "${RED}✗ %s${NC}\n" "$1"; }
info() { printf "${BOLD}▶ %s${NC}\n" "$1"; }

REPO="$(cd "$(dirname "$0")" && pwd)"
UI="$REPO/linux/ui-tauri"
TARGET="x86_64-pc-windows-msvc"
OUT_DIR="$UI/src-tauri/target/$TARGET/release"
EXE="$OUT_DIR/vortex-ui-tauri.exe"

export PATH="$HOME/.cargo/bin:$PATH"

SKIP_DEPS=0
WANT_INSTALLER=0
ASSUME_YES=1
for a in "$@"; do
  case "$a" in
    --skip-deps) SKIP_DEPS=1 ;;
    --installer) WANT_INSTALLER=1 ;;
    --ask)       ASSUME_YES=0 ;;
    --yes)       ASSUME_YES=1 ;;
    -h|--help)   sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  esac
done

# ── 0. system dependencies ────────────────────────────────────────────────────
if [ "$SKIP_DEPS" -eq 0 ]; then
  PM=""
  for c in apt-get dnf pacman zypper; do
    if command -v "$c" >/dev/null 2>&1; then PM="$c"; break; fi
  done

  SUDO=""
  [ "$(id -u)" -ne 0 ] && SUDO="sudo"

  YES_FLAG=""
  [ "$ASSUME_YES" -eq 1 ] && case "$PM" in
    apt-get|dnf|zypper) YES_FLAG="-y" ;;
    pacman)             YES_FLAG="--noconfirm" ;;
  esac

  # clang-cl, lld-link and llvm-rc are the three tools cargo-xwin shells out to.
  # They are split across packages differently per distro, hence the mapping
  # rather than one name.
  declare -a PKGS=()
  case "$PM" in
    apt-get) PKGS=(clang lld llvm) ;;
    dnf)     PKGS=(clang lld llvm) ;;
    pacman)  PKGS=(clang lld llvm) ;;
    zypper)  PKGS=(clang lld llvm) ;;
    "")      warn "No supported package manager found — install clang, lld and llvm by hand." ;;
  esac

  if [ -n "$PM" ] && [ "${#PKGS[@]}" -gt 0 ]; then
    info "installing the cross toolchain (clang / lld / llvm)…"
    case "$PM" in
      apt-get) $SUDO apt-get update -qq; $SUDO apt-get install $YES_FLAG "${PKGS[@]}" ;;
      dnf)     $SUDO dnf install $YES_FLAG --skip-broken "${PKGS[@]}" ;;
      pacman)  $SUDO pacman -Sy $YES_FLAG --needed "${PKGS[@]}" ;;
      zypper)  $SUDO zypper install $YES_FLAG "${PKGS[@]}" ;;
    esac || warn "toolchain install had issues — continuing; the build will say what's missing."
  fi

  # Node only when absent: installing it unconditionally is what breaks a box
  # that already has NodeSource node (see the note in packaging/install-deps.sh).
  if ! command -v node >/dev/null 2>&1; then
    info "installing node/npm…"
    case "$PM" in
      apt-get) $SUDO apt-get install $YES_FLAG nodejs npm ;;
      dnf)     $SUDO dnf install $YES_FLAG nodejs npm ;;
      pacman)  $SUDO pacman -Sy $YES_FLAG --needed nodejs npm ;;
      zypper)  $SUDO zypper install $YES_FLAG nodejs npm ;;
    esac || warn "node install failed — install node ≥18 yourself and re-run."
  fi

  if ! command -v rustup >/dev/null 2>&1 && ! command -v cargo >/dev/null 2>&1; then
    info "installing Rust via rustup (distro Rust is usually too old for Tauri)…"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  fi
fi

command -v cargo >/dev/null 2>&1 || { err "cargo not on PATH — install Rust and re-run."; exit 1; }

# ── 1. the Windows target ─────────────────────────────────────────────────────
if rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  ok "rust target $TARGET already installed"
else
  info "adding rust target $TARGET…"
  rustup target add "$TARGET" || { err "could not add $TARGET"; exit 1; }
fi

# ── 2. cargo-xwin ─────────────────────────────────────────────────────────────
# First build downloads Microsoft's CRT and Windows SDK into ~/.cache/cargo-xwin
# (a few hundred MB, once). cargo-xwin accepts the Microsoft licence on your
# behalf for that download — if that is not acceptable to you, stop here and
# build on Windows instead.
if command -v cargo-xwin >/dev/null 2>&1; then
  ok "cargo-xwin already installed ($(cargo-xwin --version 2>/dev/null | head -1))"
else
  info "installing cargo-xwin (compiles from source, a few minutes)…"
  cargo install cargo-xwin || { err "cargo install cargo-xwin failed"; exit 1; }
fi

# ── 3. NSIS, only when asked ──────────────────────────────────────────────────
if [ "$WANT_INSTALLER" -eq 1 ] && ! command -v makensis >/dev/null 2>&1; then
  info "installing NSIS (makensis) for the installer…"
  case "${PM:-}" in
    apt-get) $SUDO apt-get install $YES_FLAG nsis ;;
    dnf)     $SUDO dnf install $YES_FLAG mingw32-nsis ;;
    zypper)  $SUDO zypper install $YES_FLAG mingw32-nsis ;;
    # Arch/CachyOS: nsis is AUR-only, so this needs an AUR helper. Not fatal —
    # the .exe below is built either way.
    pacman)
      if command -v paru >/dev/null 2>&1; then paru -S --needed --noconfirm nsis
      elif command -v yay >/dev/null 2>&1; then yay -S --needed --noconfirm nsis
      else warn "nsis is in the AUR; install it with an AUR helper (e.g. 'paru -S nsis')."
      fi ;;
  esac || warn "NSIS install failed — the .exe will still be built."
fi

# ── 4. UI dependencies ────────────────────────────────────────────────────────
# Same pnpm-then-npm fallback as install_linux.sh: npm needs --legacy-peer-deps
# because one dep declares an optional peer on a newer vite than we pin, and
# npm hard-fails the whole install on it where pnpm does not.
info "installing UI dependencies…"
( cd "$UI"
  if command -v pnpm >/dev/null 2>&1; then
    pnpm install --frozen-lockfile || pnpm install
  else
    npm install --legacy-peer-deps
  fi ) || { err "UI dependency install failed"; exit 1; }

# ── 5. build ──────────────────────────────────────────────────────────────────
# Build the Tauri APP, never `--workspace`. The daemon is consumed as a LIBRARY
# and cross-compiles fine, but its binary (daemon/src/main.rs, `vortex-l3d`) is
# a Linux CLI that uses bluer unconditionally and cannot build for Windows —
# asking for the workspace just fails on a binary nobody wants here.
BUNDLE_ARGS=(--no-bundle)
if [ "$WANT_INSTALLER" -eq 1 ] && command -v makensis >/dev/null 2>&1; then
  BUNDLE_ARGS=(--bundles nsis)
fi

info "cross-building for $TARGET (first run also downloads the MSVC CRT + SDK)…"
( cd "$UI" && npm run tauri build -- \
    --runner cargo-xwin --target "$TARGET" "${BUNDLE_ARGS[@]}" ) || {
  err "build failed"
  exit 1
}

# lld-link prints a wall of LNK4099 "cannot use debug info for libcmt.lib(…)"
# while linking. That is Microsoft shipping its CRT without the matching PDBs,
# not a problem with this build — the binary is complete and correct.

# ── 6. report ─────────────────────────────────────────────────────────────────
[ -f "$EXE" ] || { err "build reported success but no .exe at $EXE"; exit 1; }
echo
ok "Windows binary: $EXE"
file "$EXE" 2>/dev/null | sed 's/^/  /'
SETUP="$(ls "$OUT_DIR"/bundle/nsis/*-setup.exe 2>/dev/null | head -1)"
[ -n "$SETUP" ] && ok "Windows installer: $SETUP"
echo
info "On the Windows machine:"
echo "  • Copy the .exe (or run the installer) and launch it — no install step"
echo "    is needed for the bare .exe; it sits in the tray like the Linux build."
echo "  • Windows 11 already ships the WebView2 runtime. On Windows 10 without"
echo "    it the window opens blank: install the Evergreen WebView2 Runtime"
echo "    from Microsoft, then relaunch."
echo "  • Pair from the phone exactly as with Linux — same BLE + Noise flow."
[ "$WANT_INSTALLER" -eq 1 ] && [ -z "$SETUP" ] && \
  warn "--installer was requested but no setup.exe was produced (makensis missing?)."
echo
warn "Cross-built binaries are UNSIGNED. SmartScreen will warn on first run"
warn "(\"More info\" → \"Run anyway\"). Signing needs a certificate and is set up"
warn "via bundler > windows > sign_command in tauri.conf.json."
