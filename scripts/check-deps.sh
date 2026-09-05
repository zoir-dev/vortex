#!/usr/bin/env bash

# 🌀 Vortex-Lab dependency checker
# NOTE: This script is intentionally non-fatal. It reports what is available.

set -u

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

MISSING_REQUIRED=()
MISSING_OPTIONAL=()
MISSING_LIBS=()

print_line() {
  printf '%s\n' "=================================================="
}

print_header() {
  print_line
  printf '       🌀 Vortex-Lab Dependency Check\n'
  print_line
  printf '\n'
}

ok() {
  printf "${GREEN}✅ %s${NC}\n" "$1"
}

warn() {
  printf "${YELLOW}⚠️  %s${NC}\n" "$1"
}

err() {
  printf "${RED}❌ %s${NC}\n" "$1"
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

normalize_version() {
  printf '%s' "$1" | grep -Eo '[0-9]+([.][0-9]+)+' | head -n 1
}

version_ge() {
  local current required
  current="$1"
  required="$2"
  [ "$(printf '%s\n%s\n' "$required" "$current" | sort -V | head -n 1)" = "$required" ]
}

check_command() {
  local label cmd version_cmd
  label="$1"
  cmd="$2"
  version_cmd="$3"

  if have_cmd "$cmd"; then
    local raw
    raw="$($version_cmd 2>&1 | head -n 1)"
    ok "$(printf '%-12s %s' "$label" "$raw")"
    return 0
  fi

  err "$(printf '%-12s NOT INSTALLED' "$label")"
  return 1
}

check_version() {
  local label cmd version_cmd required optional
  label="$1"
  cmd="$2"
  version_cmd="$3"
  required="$4"
  optional="${5:-false}"

  if ! have_cmd "$cmd"; then
    if [ "$optional" = "true" ]; then
      warn "$(printf '%-12s NOT INSTALLED' "$label")"
      MISSING_OPTIONAL+=("$label")
    else
      err "$(printf '%-12s NOT INSTALLED (>= %s required)' "$label" "$required")"
      MISSING_REQUIRED+=("$label")
    fi
    return 1
  fi

  local raw detected
  raw="$($version_cmd 2>&1 | head -n 1)"
  detected="$(normalize_version "$raw")"

  if [ -z "$detected" ]; then
    warn "$(printf '%-12s %s (version parse uncertain)' "$label" "$raw")"
    return 0
  fi

  if version_ge "$detected" "$required"; then
    ok "$(printf '%-12s %s (>= %s required)' "$label" "$raw" "$required")"
    return 0
  fi

  if [ "$optional" = "true" ]; then
    warn "$(printf '%-12s %s (< %s recommended)' "$label" "$raw" "$required")"
  else
    err "$(printf '%-12s %s (< %s required)' "$label" "$raw" "$required")"
    MISSING_REQUIRED+=("$label")
  fi

  return 1
}

# Probe the LIBRARY, not a Debian package name. `dpkg -s` reports EVERY lib as
# missing on Fedora/Arch/openSUSE, where these are called something else
# entirely (libdbus-1-dev → dbus-devel, librsvg2-dev → librsvg2-devel …) —
# a clean Fedora box with all deps installed still scored 0/6. pkg-config
# answers the question we actually care about ("can the build link it?") the
# same way on every distro. Call sites keep the Debian names as labels.
check_lib() {
  local package modules m
  package="$1"

  case "$package" in
    libdbus-1-dev)         modules="dbus-1" ;;
    libwebkit2gtk-4.1-dev) modules="webkit2gtk-4.1" ;;
    libssl-dev)            modules="openssl" ;;
    # Ayatana on Debian/Arch, the older libappindicator on Fedora/openSUSE.
    # Tauri links whichever is present, so either satisfies this.
    libayatana-appindicator3-dev) modules="ayatana-appindicator3-0.1 appindicator3-0.1" ;;
    librsvg2-dev)          modules="librsvg-2.0" ;;
    protobuf-compiler)     modules="" ;;   # a binary, not a linkable library
    *)                     modules="$package" ;;
  esac

  if [ -z "$modules" ]; then
    if have_cmd protoc; then ok "$package"; return 0; fi
  elif ! have_cmd pkg-config; then
    warn "$package  UNKNOWN (pkg-config is not installed)"
    return 0
  else
    for m in $modules; do
      if pkg-config --exists "$m" 2>/dev/null; then ok "$package"; return 0; fi
    done
  fi

  warn "$package  NOT FOUND"
  MISSING_LIBS+=("$package")
}

system_summary() {
  local system kernel pretty_name
  system="$(uname -a 2>/dev/null || true)"
  kernel="$(uname -r 2>/dev/null || true)"
  pretty_name=""

  if [ -f /etc/os-release ]; then
    pretty_name="$(grep '^PRETTY_NAME=' /etc/os-release | cut -d= -f2- | tr -d '"')"
  fi

  printf '%bSystem:%b %s\n' "$BOLD" "$NC" "${pretty_name:-Unknown Linux}"
  printf '%bKernel:%b %s\n' "$BOLD" "$NC" "${kernel:-Unknown}"
  printf '%buname:%b %s\n' "$BOLD" "$NC" "${system:-Unavailable}"
  printf '\n'
}

check_required_tools() {
  printf '[REQUIRED]\n'
  check_version "git" "git" "git --version" "2.30"
  check_version "rustc" "rustc" "rustc --version" "1.75"
  check_command "cargo" "cargo" "cargo --version" || MISSING_REQUIRED+=("cargo")
  check_version "node" "node" "node --version" "18.0"
  check_version "npm" "npm" "npm --version" "8.0"
  check_version "java" "java" "java -version" "17"
  check_version "bluetoothctl" "bluetoothctl" "bluetoothctl --version" "5.50"
  check_version "protoc" "protoc" "protoc --version" "3.19"
  printf '\n'
}

check_optional_tools() {
  printf '[OPTIONAL]\n'
  check_version "adb" "adb" "adb --version" "1.0.41" "true"

  if have_cmd gh; then
    ok "$(printf '%-12s %s' 'gh' "$(gh --version 2>&1 | head -n 1)")"
  else
    warn "$(printf '%-12s NOT INSTALLED (GitHub CLI)' 'gh')"
    MISSING_OPTIONAL+=("gh")
  fi

  if have_cmd cargo-watch; then
    ok "$(printf '%-12s INSTALLED' 'cargo-watch')"
  else
    warn "$(printf '%-12s NOT INSTALLED' 'cargo-watch')"
    MISSING_OPTIONAL+=("cargo-watch")
  fi

  if have_cmd cargo-audit; then
    ok "$(printf '%-12s INSTALLED' 'cargo-audit')"
  else
    warn "$(printf '%-12s NOT INSTALLED' 'cargo-audit')"
    MISSING_OPTIONAL+=("cargo-audit")
  fi

  printf '\n'
}

check_system_libs() {
  printf '[SYSTEM LIBS]\n'
  check_lib "libdbus-1-dev"
  check_lib "libwebkit2gtk-4.1-dev"
  check_lib "libssl-dev"
  check_lib "libayatana-appindicator3-dev"
  check_lib "protobuf-compiler"
  check_lib "librsvg2-dev"
  printf '\n'
}

check_services() {
  printf '[SERVICES]\n'

  if have_cmd systemctl; then
    local status_text
    status_text="$(systemctl is-active bluetooth.service 2>/dev/null || true)"
    if [ "$status_text" = "active" ]; then
      ok "bluetooth.service active"
    elif [ -n "$status_text" ]; then
      warn "bluetooth.service ${status_text}"
    else
      warn "bluetooth.service status unavailable or systemd is not managing it"
    fi
  else
    warn "systemctl NOT AVAILABLE"
  fi

  printf '\n'
}

print_missing_summary() {
  printf '[SUMMARY]\n'

  if [ "${#MISSING_REQUIRED[@]}" -eq 0 ]; then
    ok "No required tools missing"
  else
    err "Required missing: ${MISSING_REQUIRED[*]}"
  fi

  if [ "${#MISSING_OPTIONAL[@]}" -eq 0 ]; then
    ok "No optional tools missing"
  else
    warn "Optional missing: ${MISSING_OPTIONAL[*]}"
  fi

  if [ "${#MISSING_LIBS[@]}" -eq 0 ]; then
    ok "No system libraries missing"
  else
    warn "System libs missing: ${MISSING_LIBS[*]}"
  fi

  printf '\n'
}

final_status() {
  print_line

  if [ "${#MISSING_REQUIRED[@]}" -eq 0 ]; then
    if [ "${#MISSING_LIBS[@]}" -eq 0 ]; then
      printf 'Status: %b✅ Ready for development%b\n' "$GREEN" "$NC"
    else
      printf 'Status: %b⚠️  Core tools ready, some system libs are missing%b\n' "$YELLOW" "$NC"
    fi
  else
    printf 'Status: %b❌ Missing required dependencies%b\n' "$RED" "$NC"
  fi

  print_line
}

main() {
  print_header
  system_summary
  check_required_tools
  check_optional_tools
  check_system_libs
  check_services
  print_missing_summary
  final_status
}

main "$@"
