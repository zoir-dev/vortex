#!/usr/bin/env bash
#
# Vortex Android app — build + install onto a USB/adb-connected phone.
#
# Usage:
#   ./install.sh          clean build + install (safest)
#   ./install.sh --fast   skip `clean` for a quick reinstall while iterating
#
set -euo pipefail
cd "$(dirname "$0")"

# Installed package name (applicationId in app/build.gradle.kts). The CODE
# packages still live under com.vortex.a3 — component names below are
# "<PKG>/com.vortex.a3.<Class>", which is how Android addresses a component
# when applicationId and namespace differ.
PKG="io.github.zoir_dev.vortex"

# --- package-manager helper (for auto-installing JDK / adb) ------------------
# Same distro detection as linux/packaging/install-deps.sh — apt/dnf/pacman/
# zypper cover every distro we support. Only invoked when a build prereq is
# actually missing, so most runs never touch sudo. Errors stay VISIBLE — a
# hidden stderr here once masked "sudo not found" as "couldn't install JDK".
PM=""
for c in apt-get dnf pacman zypper; do
  command -v "$c" >/dev/null 2>&1 && { PM="$c"; break; }
done
SUDO=""
[ "$(id -u)" != 0 ] && SUDO="sudo"
if [ -n "$SUDO" ] && ! command -v sudo >/dev/null 2>&1; then
  SUDO=""
  NO_SUDO=1
fi

pm_install() {
  [ -z "$PM" ] && { echo "✗ no supported package manager (apt/dnf/pacman/zypper)"; return 1; }
  if [ "${NO_SUDO:-0}" = 1 ]; then
    echo "✗ not root and 'sudo' is not installed — as root, run:"
    echo "    $PM install $*"
    return 1
  fi
  echo "▶ $SUDO $PM install $*"
  case "$PM" in
    # DPkg::Lock::Timeout: wait (up to 30 min) instead of failing when another
    # apt holds the lock — e.g. install_linux.sh's dependency step running in a
    # second terminal, or unattended-upgrades right after boot.
    apt-get) $SUDO apt-get -o DPkg::Lock::Timeout=1800 update -qq || true
             $SUDO env DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=1800 install -y --no-install-recommends "$@" ;;
    dnf)     $SUDO dnf install -y "$@" ;;
    pacman)  $SUDO pacman -Sy --noconfirm --needed "$@" ;;
    zypper)  $SUDO zypper install -y "$@" ;;
  esac
}

# --- JDK selection -----------------------------------------------------------
# Gradle/AGP here need JDK 17 or 21; the system default (25) breaks the build.
# Prefer 17 — it matches the project's jvmTarget=17 (the bytecode level baked
# into the APK). The JDK is build-time only and never ships to the phone, so it
# has ZERO effect on which phones can run the app — that's `minSdk` (= 29,
# Android 10+). Don't confuse the two.
#
# The globs cover every distro layout: Debian/Ubuntu java-17-openjdk-amd64,
# Fedora/Arch java-17-openjdk[-…], openSUSE /usr/lib64/jvm/java-17-openjdk-17.
find_jdk() {
  local d
  for d in "$HOME/.local/opt/jdk-21"* "$HOME/.local/opt/jdk-17"* \
           /usr/lib/jvm/java-17-openjdk* /usr/lib64/jvm/java-17-openjdk* \
           /usr/lib/jvm/java-21-openjdk* /usr/lib64/jvm/java-21-openjdk*; do
    if [ -x "$d/bin/javac" ]; then echo "$d"; return 0; fi
  done
  return 1
}

# Portable JDK fallback — NOT optional polish. Fedora 44 ships ONLY
# java-25-openjdk: no 17, no 21, so on a clean F44 box every distro package
# name below fails and the install would otherwise dead-end. Temurin unpacks
# into ~/.local/opt/jdk-21 (the first path find_jdk looks at), needs no sudo,
# and touches nothing system-wide.
install_portable_jdk() {
  local arch tgz top
  case "$(uname -m)" in
    x86_64)  arch="x64" ;;
    aarch64) arch="aarch64" ;;
    *)       echo "✗ no portable JDK build for $(uname -m)"; return 1 ;;
  esac
  command -v curl >/dev/null 2>&1 || pm_install curl || return 1
  echo "▶ this distro has no JDK 17/21 package — fetching a portable Temurin 21"
  echo "  into ~/.local/opt/jdk-21 (no sudo, nothing system-wide is changed)…"
  tgz="$(mktemp -t vortex-jdk-XXXXXX.tar.gz)"
  if ! curl -fL --retry 3 -o "$tgz" \
       "https://api.adoptium.net/v3/binary/latest/21/ga/linux/$arch/jdk/hotspot/normal/eclipse"; then
    rm -f "$tgz"; return 1
  fi
  rm -rf "${HOME:?}/.local/opt/.jdk-unpack"
  mkdir -p "$HOME/.local/opt/.jdk-unpack"
  if ! tar -xzf "$tgz" -C "$HOME/.local/opt/.jdk-unpack"; then rm -f "$tgz"; return 1; fi
  rm -f "$tgz"
  top="$(find "$HOME/.local/opt/.jdk-unpack" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  if [ -z "$top" ] || [ ! -x "$top/bin/javac" ]; then
    echo "✗ the downloaded JDK has no bin/javac — ignoring it"
    rm -rf "${HOME:?}/.local/opt/.jdk-unpack"; return 1
  fi
  rm -rf "${HOME:?}/.local/opt/jdk-21"
  mv "$top" "$HOME/.local/opt/jdk-21"
  rm -rf "${HOME:?}/.local/opt/.jdk-unpack"
}

if ! JAVA_HOME="$(find_jdk)"; then
  echo "▶ JDK 17/21 not found — installing…"
  # One attempt with the CORRECT name for this distro (17, falling back to 21)
  # — a blind cross-distro name chain just buries the real error under
  # "package not found" noise. No PM at all is fine too: we fall through to
  # the portable JDK below.
  case "$PM" in
    apt-get)    pm_install openjdk-17-jdk-headless || pm_install openjdk-21-jdk-headless || true ;;
    dnf|zypper) pm_install java-17-openjdk-devel   || pm_install java-21-openjdk-devel   || true ;;
    pacman)     pm_install jdk17-openjdk           || pm_install jdk21-openjdk           || true ;;
  esac
  find_jdk >/dev/null 2>&1 || install_portable_jdk || true
  if ! JAVA_HOME="$(find_jdk)"; then
    # Name the package for the distro we are ACTUALLY on. Printing an apt name
    # on Fedora once sent a user hunting for a package that cannot exist there.
    case "$PM" in
      apt-get) hint="sudo apt install openjdk-21-jdk-headless" ;;
      dnf)     hint="sudo dnf install java-21-openjdk-devel" ;;
      zypper)  hint="sudo zypper install java-21-openjdk-devel" ;;
      pacman)  hint="sudo pacman -S jdk21-openjdk" ;;
      *)       hint="install a JDK 17 or 21 with your package manager" ;;
    esac
    echo "✗ JDK still not found — see the error above, fix it and re-run."
    echo "  Distro package: $hint"
    echo "  Or by hand:     unpack a JDK 17/21 into ~/.local/opt/jdk-21"
    exit 1
  fi
fi
export JAVA_HOME
echo "▶ JAVA_HOME=$JAVA_HOME"

# --- Android SDK --------------------------------------------------------------
# Gradle refuses to run without an SDK root ("SDK location not found").
# Resolution order: local.properties → ANDROID_HOME/ANDROID_SDK_ROOT → the
# conventional ~/Android/Sdk → bootstrap a fresh one: download Google's
# cmdline-tools, accept the licenses, preinstall the project's platform.
# With licenses accepted AGP fetches anything else it needs (build-tools…)
# during the first build on its own.
SDK_DIR=""
if [ -f local.properties ]; then
  SDK_DIR="$(sed -n 's/^sdk\.dir=//p' local.properties | head -n1)"
  [ -d "$SDK_DIR" ] || SDK_DIR=""   # stale path (file is per-machine, gitignored)
fi
if [ -z "$SDK_DIR" ]; then
  for c in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Android/Sdk"; do
    [ -n "$c" ] && [ -d "$c" ] && { SDK_DIR="$c"; break; }
  done
fi
if [ -z "$SDK_DIR" ]; then
  SDK_DIR="$HOME/Android/Sdk"
  echo "▶ Android SDK not found — bootstrapping into $SDK_DIR…"
  command -v curl  >/dev/null 2>&1 || pm_install curl  || true
  command -v unzip >/dev/null 2>&1 || pm_install unzip || true
  # Any recent cmdline-tools works — sdkmanager itself then downloads the
  # exact platform/build-tools the project pins.
  CLT_URL="https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip"
  CLT_ZIP="$(mktemp -t vortex-clt-XXXXXX.zip)"
  curl -fL --retry 3 -o "$CLT_ZIP" "$CLT_URL"
  mkdir -p "$SDK_DIR/cmdline-tools"
  unzip -q -o "$CLT_ZIP" -d "$SDK_DIR/cmdline-tools"
  rm -f "$CLT_ZIP"
  # the zip unpacks as cmdline-tools/ but sdkmanager insists on .../latest/
  [ -d "$SDK_DIR/cmdline-tools/latest" ] || mv "$SDK_DIR/cmdline-tools/cmdline-tools" "$SDK_DIR/cmdline-tools/latest"
  SDKMANAGER="$SDK_DIR/cmdline-tools/latest/bin/sdkmanager"
  # read the pinned compileSdk from the build file so this never goes stale
  API="$(sed -n 's/^[[:space:]]*compileSdk = \([0-9][0-9]*\).*/\1/p' app/build.gradle.kts | head -n1)"
  API="${API:-36}"
  echo "▶ accepting SDK licenses + installing platform android-$API…"
  yes | "$SDKMANAGER" --licenses >/dev/null 2>&1 || true
  "$SDKMANAGER" "platform-tools" "platforms;android-$API" || true
fi
export ANDROID_HOME="$SDK_DIR" ANDROID_SDK_ROOT="$SDK_DIR"
# Pin it for manual ./gradlew runs too (per-machine file, gitignored).
printf 'sdk.dir=%s\n' "$SDK_DIR" > local.properties
echo "▶ ANDROID_HOME=$SDK_DIR"

# --- device check ------------------------------------------------------------
# adb ships as `adb` on Debian/Ubuntu and inside `android-tools` elsewhere.
if ! command -v adb >/dev/null 2>&1; then
  echo "▶ adb not found — installing…"
  case "$PM" in
    apt-get) pm_install adb || true ;;
    *)       pm_install android-tools || true ;;
  esac
  if ! command -v adb >/dev/null 2>&1; then
    echo "✗ adb still not found — see the package-manager error above, fix it"
    case "$PM" in
      apt-get) echo "  (sudo apt install adb) and re-run." ;;
      dnf)     echo "  (sudo dnf install android-tools) and re-run." ;;
      zypper)  echo "  (sudo zypper install android-tools) and re-run." ;;
      pacman)  echo "  (sudo pacman -S android-tools) and re-run." ;;
      *)       echo "  (install the package providing adb) and re-run." ;;
    esac
    exit 1
  fi
fi
if ! adb get-state >/dev/null 2>&1; then
  echo "✗ no adb device ready. Plug the phone in over USB, enable Developer"
  echo "  options → USB debugging, and accept the RSA fingerprint prompt."
  adb devices
  exit 1
fi
echo "▶ device: $(adb devices | sed -n '2p')"

# --- build + install ---------------------------------------------------------
# `clean` first avoids the MIUI resource-id skew that incremental re-installs
# cause: R.layout.* ids shift between builds → "Bad notification… couldn't
# inflate contentViews" → the foreground service crash-loops. A clean build
# keeps the same ids. installDebug keeps existing pairing (no uninstall).
CLEAN="clean"
[ "${1:-}" = "--fast" ] && CLEAN=""
echo "▶ ./gradlew ${CLEAN:-(no clean)} :app:installDebug"
GRADLE_LOG="$(mktemp -t vortex-gradle-XXXXXX.log)"
if ! ./gradlew $CLEAN :app:installDebug 2>&1 | tee "$GRADLE_LOG"; then
  APK="app/build/outputs/apk/debug/app-debug.apk"
  if grep -q INSTALL_FAILED_USER_RESTRICTED "$GRADLE_LOG" && [ -f "$APK" ]; then
    # MIUI blocks APK installs over USB unless "Install via USB" is enabled,
    # which requires a Mi-account sign-in (+ sometimes a SIM). But that gate
    # only hooks the HOST-side `adb install` path — running `pm install` from
    # the ON-DEVICE shell walks right past it (live-verified on MIUI 12,
    # 2026-07-21). Try that first; fall back to hand-sideloading only if a
    # ROM closes this hole too.
    echo ""
    echo "⚠ the PHONE blocked the USB install (MIUI 'Install via USB' needs a"
    echo "  Mi account). Trying the on-device pm-install path…"
    adb push "$APK" /data/local/tmp/vortex.apk
    if adb shell pm install -r /data/local/tmp/vortex.apk 2>&1 | grep -q Success; then
      adb shell rm -f /data/local/tmp/vortex.apk
      echo "▶ installed via on-device pm — continuing with the permission grants…"
    else
      adb shell rm -f /data/local/tmp/vortex.apk
      echo "▶ that's blocked too — falling back to the NO-ACCOUNT sideload:"
      echo "▶ pushing the APK to the phone's Downloads…"
      adb push "$APK" /sdcard/Download/vortex.apk
      echo ""
      echo "  On the PHONE now: open Files → Downloads → vortex.apk → Install"
      echo "  (allow 'install unknown apps' for Files if asked — no account needed)."
      read -r -p "  Press Enter here AFTER the app is installed on the phone… " _ || true
      if ! adb shell pm list packages 2>/dev/null | grep -q "$PKG"; then
        echo "✗ $PKG still isn't installed — install vortex.apk on the"
        echo "  phone, then re-run: ./install_android.sh --fast"
        rm -f "$GRADLE_LOG"
        exit 1
      fi
      echo "▶ installed — continuing with the permission grants…"
    fi
  else
    echo ""
    echo "✗ build/install failed — see the gradle error above."
    rm -f "$GRADLE_LOG"
    exit 1
  fi
fi
rm -f "$GRADLE_LOG"

# --- post-install grants (best-effort) ---------------------------------------
# Android apps ship self-contained (everything is in the APK) — there are no
# system libraries to install on the phone. What the app DOES need is a few
# special accesses turned on; all but MIUI Autostart can be set over adb
# (adb shell holds WRITE_SECURE_SETTINGS), so the user does nothing by hand.

# Clipboard auto-capture (phone→laptop) needs the background READ_CLIPBOARD
# appop; Android 10+ blocks background clipboard reads without it.
if adb shell appops set "$PKG" READ_CLIPBOARD allow 2>/dev/null; then
  echo "▶ granted READ_CLIPBOARD (clipboard auto-sync)"
fi

# MIUI "Show on Lock screen" (custom appop 10020): without it MIUI blocks the
# find-my-phone "Found it!" page from popping over the lockscreen. 10021
# ("display pop-up windows in background") helps the unlocked case on ROMs
# that have it. Both are MIUI-only — harmless no-ops elsewhere.
if adb shell appops set "$PKG" 10020 allow 2>/dev/null; then
  adb shell appops set "$PKG" 10021 allow 2>/dev/null || true
  echo "▶ granted MIUI show-on-lockscreen (find-my-phone full-screen page)"
fi

# MIUI Autostart (custom appop 10008 = AUTO_START): the Security-Center toggle
# has no public API, but on many MIUI/HyperOS builds it maps to this appop.
# Best-effort — on ROMs where it doesn't stick, the in-app hint card still
# walks the user to the right screen; verify once in Security → Autostart.
if adb shell appops set "$PKG" 10008 allow 2>/dev/null; then
  echo "▶ granted MIUI autostart (appop 10008 — verify once in Security → Autostart)"
fi

# Battery-optimization whitelist (works on every ROM incl. stock Android):
# exempts the foreground service from Doze restrictions — the other half of
# surviving in the background.
adb shell dumpsys deviceidle whitelist +"$PKG" >/dev/null 2>&1 \
  && echo "▶ battery-optimization whitelist (Doze exemption)"

# Accessibility service (screen-control injection + browsing-handoff URL read).
# Append our component to the secure list idempotently, then flip the master
# switch. ("null" = the list was unset.)
A11Y="$PKG/com.vortex.a3.service.VortexInputService"
CUR="$(adb shell settings get secure enabled_accessibility_services 2>/dev/null | tr -d '\r')"
[ "$CUR" = "null" ] && CUR=""
case ":$CUR:" in
  *":$A11Y:"*) ;;                                   # already enabled
  *) adb shell settings put secure enabled_accessibility_services "${CUR:+$CUR:}$A11Y" >/dev/null 2>&1 ;;
esac
adb shell settings put secure accessibility_enabled 1 >/dev/null 2>&1
# Verify it actually landed: MIUI gates secure-settings writes behind the
# separate "USB debugging (Security settings)" developer toggle (Mi-account
# bound, auto-revokes after idle) — when it's off, the puts above are denied.
if adb shell settings get secure enabled_accessibility_services 2>/dev/null | tr -d '\r' | grep -q "$A11Y"; then
  echo "▶ enabled Accessibility (screen control + browsing-handoff pill)"
else
  echo "⚠ couldn't enable Accessibility over adb (MIUI: needs 'USB debugging"
  echo "  (Security settings)' ON, or flip it by hand ONCE on the phone:"
  echo "  Settings → Accessibility → Vortex ✓ — the in-app card links there too)"
fi

# Notification access (notification / SMS / call mirroring + media live
# activities). `cmd notification allow_listener` works from adb on Android 10+.
NL="$PKG/com.vortex.a3.core.media.MediaNotificationListenerService"
adb shell cmd notification allow_listener "$NL" >/dev/null 2>&1 \
  && echo "▶ enabled Notification access (mirroring + media pills)"

# Launch the app.
adb shell monkey -p "$PKG" -c android.intent.category.LAUNCHER 1 \
  >/dev/null 2>&1 || true

echo "✓ installed + launched $PKG (accesses auto-enabled over adb)"
echo "  Autostart was attempted over adb too (MIUI appop) — still verify it once:"
echo "   • Security app → Autostart → Vortex ✓  (MIUI can't be read back reliably)"
