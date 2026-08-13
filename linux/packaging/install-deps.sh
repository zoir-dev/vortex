#!/usr/bin/env bash
#
# install-deps.sh — auto-install every system dependency Vortex needs, so the
# user does nothing by hand. Detects the distro's package manager
# (apt / dnf / pacman / zypper), maps the package names, and installs in one
# sudo batch. Rust (if missing) goes through rustup, since distro Rust is
# usually too old to build Tauri.
#
# Called by ../../install_linux.sh before the build. Safe to run standalone:
#     ./linux/packaging/install-deps.sh
#
# Flags:
#   --ask        interactive mode (let the package manager prompt); the default
#                is non-interactive (-y / --noconfirm) so one run needs zero input
#   --yes        accepted for compatibility (already the default)
#   --runtime    install ONLY the runtime deps (skip build toolchain & -dev libs)
#
# It is best-effort: a package that can't be found is reported, not fatal — the
# build/run step will still tell you precisely what's missing.
set -uo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; BOLD='\033[1m'; NC='\033[0m'
ok()   { printf "${GREEN}✓ %s${NC}\n" "$1"; }
warn() { printf "${YELLOW}⚠ %s${NC}\n" "$1"; }
err()  { printf "${RED}✗ %s${NC}\n" "$1"; }
info() { printf "${BOLD}▶ %s${NC}\n" "$1"; }

ASSUME_YES=1
RUNTIME_ONLY=0
for a in "$@"; do
  case "$a" in
    --yes) ASSUME_YES=1 ;;
    --ask) ASSUME_YES=0 ;;
    --runtime) RUNTIME_ONLY=1 ;;
  esac
done

# ── detect the package manager ────────────────────────────────────────────────
PM=""
for c in apt-get dnf pacman zypper; do
  if command -v "$c" >/dev/null 2>&1; then PM="$c"; break; fi
done
if [ -z "$PM" ]; then
  err "No supported package manager (apt/dnf/pacman/zypper) found."
  warn "Install these by hand: webkit2gtk-4.1, dbus, openssl, gstreamer(+plugins),"
  warn "  protobuf, librsvg, libayatana-appindicator, pactl, NetworkManager(nmcli),"
  warn "  zenity, adb, nautilus-python, python-pillow — plus rustup & node."
  exit 0
fi

# sudo only when we aren't already root
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

# ── per-distro package lists ──────────────────────────────────────────────────
# Two buckets: BUILD (toolchain + -dev headers, needed to compile from source)
# and RUN (shared libs + helper binaries the running app shells out to).
declare -a BUILD RUN V4L2_PKG=()

# NOTE: node/npm are deliberately NOT in these lists — they're installed in a
# SEPARATE, conditional step below (only when missing). Bundling them here is
# what broke a real run: a machine with NodeSource node already installed makes
# the distro `npm` package conflict, and apt then refuses the WHOLE batch
# (all-or-nothing) — taking webkit/gstreamer down with it. Keeping node out
# means one node hiccup can never block the critical libraries.
declare -a NODE_PKGS
case "$PM" in
  apt-get)
    BUILD=(build-essential pkg-config curl file
           libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev
           librsvg2-dev libdbus-1-dev libssl-dev protobuf-compiler
           libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev)
    RUN=(gstreamer1.0-plugins-base gstreamer1.0-plugins-good
         gstreamer1.0-plugins-bad gstreamer1.0-libav
         pulseaudio-utils network-manager zenity adb
         python3-nautilus python3-pil)
    NODE_PKGS=(nodejs npm)
    V4L2_PKG=(v4l2loopback-dkms)
    ;;
  dnf)
    BUILD=(gcc gcc-c++ make pkgconf-pkg-config curl file
           webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel
           librsvg2-devel dbus-devel openssl-devel protobuf-compiler
           gstreamer1-devel gstreamer1-plugins-base-devel)
    RUN=(gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-libav
         pulseaudio-utils NetworkManager zenity android-tools
         nautilus-python python3-pillow)
    NODE_PKGS=(nodejs npm)
    V4L2_PKG=(akmod-v4l2loopback)
    ;;
  pacman)
    BUILD=(base-devel pkgconf curl
           webkit2gtk-4.1 gtk3 libayatana-appindicator librsvg
           dbus openssl protobuf gst-plugins-base-libs)
    # gst-plugin-gtk carries `gtksink`, which the screen-mirror pipeline ends in
    # (see ui-tauri mirror.rs). Arch split it OUT of gst-plugins-good, so
    # installing the latter is not enough — without this the mirror window opens
    # and spins forever on "gst parse: no element gtksink".
    RUN=(gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav
         gst-plugin-gtk
         libpulse networkmanager zenity android-tools
         nautilus-python python-pillow)
    NODE_PKGS=(nodejs npm)
    V4L2_PKG=(v4l2loopback-dkms)
    ;;
  zypper)
    BUILD=(gcc gcc-c++ make pkg-config curl
           webkit2gtk3-soup3-devel gtk3-devel libayatana-appindicator3-devel
           librsvg-devel dbus-1-devel libopenssl-devel protobuf-devel
           gstreamer-devel gstreamer-plugins-base-devel)
    RUN=(gstreamer-plugins-good gstreamer-plugins-bad gstreamer-plugins-libav
         pulseaudio-utils NetworkManager zenity android-tools
         python3-nautilus python3-Pillow)
    NODE_PKGS=(nodejs npm)
    V4L2_PKG=(v4l2loopback-kmp-default)
    ;;
esac

PKGS=("${RUN[@]}")
[ "$RUNTIME_ONLY" -eq 0 ] && PKGS=("${BUILD[@]}" "${RUN[@]}")

# ── install ───────────────────────────────────────────────────────────────────
info "Installing system dependencies via $PM (${#PKGS[@]} packages)…"
printf '   %s\n' "${PKGS[*]}"

YES_FLAG=""
[ "$ASSUME_YES" -eq 1 ] && case "$PM" in
  apt-get|dnf|zypper) YES_FLAG="-y" ;;
  pacman)             YES_FLAG="--noconfirm" ;;
esac

# apt only: debconf dialogs block even with -y — most notably DKMS's
# "Configuring secure-boot" MOK screen that v4l2loopback-dkms triggers on a
# Secure-Boot machine. Route debconf away entirely in non-interactive mode.
# Skipping MOK enrollment only affects the OPTIONAL camera kernel module:
# under Secure Boot it may not load until `sudo dpkg-reconfigure
# v4l2loopback-dkms` is run once by hand; nothing else is touched.
# DPkg::Lock::Timeout: wait (up to 30 min) for a concurrent apt (unattended-
# upgrades, install_android.sh in another terminal) instead of failing.
APT_GET="apt-get -o DPkg::Lock::Timeout=1800"
[ "$ASSUME_YES" -eq 1 ] && APT_GET="env DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=1800"

set +e
case "$PM" in
  apt-get)
    $SUDO $APT_GET update
    # --no-install-recommends keeps it lean; missing names are reported, not fatal
    $SUDO $APT_GET install $YES_FLAG --no-install-recommends "${PKGS[@]}"
    ;;
  dnf)
    # --skip-broken: a single unavailable pkg (e.g. gstreamer1-libav needs
    # RPM Fusion) won't abort the whole batch.
    $SUDO dnf install $YES_FLAG --skip-broken "${PKGS[@]}"
    ;;
  pacman)
    $SUDO pacman -Sy $YES_FLAG --needed "${PKGS[@]}"
    ;;
  zypper)
    $SUDO zypper install $YES_FLAG "${PKGS[@]}"
    ;;
esac
PM_RC=$?
set -e
[ "$PM_RC" -eq 0 ] && ok "System packages installed." \
                   || warn "Package manager exited $PM_RC — some names may differ on your distro; check the log above."

# ── verify gtksink ────────────────────────────────────────────────────────────
# The screen-mirror / second-screen / continuity-camera pipelines all end in
# `gtksink`. Which package ships it varies (Arch splits it out of
# gst-plugins-good entirely), and when it's absent the failure is invisible:
# the mirror window opens, spins on "Connecting…" forever, and the real reason
# ("gst parse: no element gtksink") only appears in the log. Check it here
# instead of shipping a guess for every distro.
if command -v gst-inspect-1.0 >/dev/null 2>&1; then
  if gst-inspect-1.0 gtksink >/dev/null 2>&1; then
    ok "GStreamer gtksink present (screen mirroring can render)."
  else
    warn "GStreamer 'gtksink' is MISSING — the screen features will open a window that never shows a picture."
    case "$PM" in
      pacman)  warn "  Install it with: sudo pacman -S --needed gst-plugin-gtk" ;;
      apt-get) warn "  Try: sudo apt-get install gstreamer1.0-gtk3" ;;
      dnf)     warn "  Try: sudo dnf install gstreamer1-plugins-good-gtk" ;;
      *)       warn "  Install your distro's GStreamer GTK sink plugin (provides libgstgtk.so)." ;;
    esac
    warn "  Everything else (notifications, clipboard, calls, files) works without it."
  fi
else
  warn "gst-inspect-1.0 not found — cannot verify 'gtksink'; screen mirroring may not render."
fi

# ── Node + npm (separate & conditional) ───────────────────────────────────────
# Only touch node when it's genuinely missing — if a working node+npm already
# exists (NodeSource, nvm, distro, …) we leave it alone, which is exactly what
# avoids the apt nodejs⇄npm conflict. Its own install call, so a failure here
# never rolls back the libraries above.
if [ "$RUNTIME_ONLY" -eq 0 ]; then
  if command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1; then
    ok "Node $(node --version) + npm already present — leaving as-is."
  else
    info "Installing Node + npm…"
    set +e
    case "$PM" in
      apt-get) $SUDO $APT_GET install $YES_FLAG --no-install-recommends "${NODE_PKGS[@]}" ;;
      dnf)     $SUDO dnf install $YES_FLAG --skip-broken "${NODE_PKGS[@]}" ;;
      pacman)  $SUDO pacman -Sy $YES_FLAG --needed "${NODE_PKGS[@]}" ;;
      zypper)  $SUDO zypper install $YES_FLAG "${NODE_PKGS[@]}" ;;
    esac
    NODE_RC=$?
    set -e
    if command -v node >/dev/null 2>&1; then ok "Node installed."
    else warn "Couldn't install Node via $PM (exit $NODE_RC) — install Node ≥18 from nodesource/nvm."; fi
  fi
fi

# ── v4l2loopback (separate & best-effort) ─────────────────────────────────────
# The virtual webcam for the EXPERIMENTAL continuity-camera feature (/dev/video42).
# Installed in its OWN call so it can NEVER take down the toolchain/libraries
# above: it builds a kernel module via DKMS (needs headers) and on Fedora it lives
# in RPM Fusion, so the name is often unresolvable — and an unresolved name is
# FATAL to the whole transaction on dnf (--skip-broken doesn't cover "no match")
# and on pacman (all-or-nothing). A failure here just means no continuity camera;
# the rest of the app is unaffected.
if [ "${#V4L2_PKG[@]}" -gt 0 ]; then
  info "Installing v4l2loopback (optional — continuity camera)…"
  set +e
  case "$PM" in
    apt-get) $SUDO $APT_GET install $YES_FLAG --no-install-recommends "${V4L2_PKG[@]}" ;;
    dnf)     $SUDO dnf install $YES_FLAG --skip-broken "${V4L2_PKG[@]}" ;;
    pacman)  $SUDO pacman -Sy $YES_FLAG --needed "${V4L2_PKG[@]}" ;;
    zypper)  $SUDO zypper install $YES_FLAG "${V4L2_PKG[@]}" ;;
  esac
  set -e
  command -v modprobe >/dev/null 2>&1 && [ -d /lib/modules/"$(uname -r)" ] \
    && ok "v4l2loopback step done (camera available if the module built)." \
    || warn "v4l2loopback may not have built — continuity camera will be unavailable (everything else still works)."
fi

# ── Rust (rustup) — distro Rust is usually too old for Tauri ───────────────────
if [ "$RUNTIME_ONLY" -eq 0 ] && ! command -v cargo >/dev/null 2>&1; then
  info "Rust not found — installing via rustup…"
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1090,SC1091
    . "$HOME/.cargo/env" 2>/dev/null || true
    command -v cargo >/dev/null 2>&1 && ok "Rust installed (open a new shell, or 'source ~/.cargo/env')." \
                                     || warn "rustup ran but cargo isn't on PATH yet."
  else
    warn "curl missing — install Rust manually from https://rustup.rs"
  fi
fi

# Node sanity: Tauri's Vite build wants Node ≥ 18; distro repos sometimes ship older.
if command -v node >/dev/null 2>&1; then
  NODE_MAJ="$(node --version 2>/dev/null | sed 's/^v//;s/\..*//')"
  [ -n "$NODE_MAJ" ] && [ "$NODE_MAJ" -lt 18 ] 2>/dev/null && \
    warn "Node $(node --version) is older than 18 — the frontend build may fail. Consider nodesource/nvm."
fi

ok "Dependency step done."
