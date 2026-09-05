#!/usr/bin/env bash

# 🦀 Vortex-Lab Rust environment setup helper
# NOTE: This script performs guided setup and may invoke your distro package
# manager (apt/dnf/pacman/zypper) or rustup.

set -u

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

say() {
  printf '%b%s%b\n' "$GREEN" "$1" "$NC"
}

warn() {
  printf '%b%s%b\n' "$YELLOW" "$1" "$NC"
}

fail() {
  printf '%b%s%b\n' "$RED" "$1" "$NC"
}

confirm() {
  local prompt reply
  prompt="$1"
  printf '%s [y/N]: ' "$prompt"
  read -r reply
  case "${reply:-}" in
    y|Y|yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

run_step() {
  local description
  description="$1"
  shift
  say "$description"
  "$@"
}

install_rustup() {
  if have_cmd rustup; then
    say "rustup already installed: $(rustup --version 2>/dev/null | head -n 1)"
    return 0
  fi

  warn "rustup is not installed."
  if confirm "Install rustup using the official script?"; then
    sh -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  else
    warn "Skipping rustup installation."
  fi
}

load_cargo_env() {
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1090
    . "$HOME/.cargo/env"
  fi
}

ensure_stable_toolchain() {
  if ! have_cmd rustup; then
    fail "rustup is unavailable. Cannot configure stable toolchain."
    return 1
  fi

  run_step "Installing or updating stable toolchain" rustup toolchain install stable
  run_step "Setting stable as default toolchain" rustup default stable
}

install_components() {
  if ! have_cmd rustup; then
    fail "rustup is unavailable. Cannot install components."
    return 1
  fi

  run_step "Installing rustfmt" rustup component add rustfmt
  run_step "Installing clippy" rustup component add clippy
}

install_cargo_tool() {
  local name
  name="$1"

  if have_cmd "$name"; then
    say "$name already installed"
    return 0
  fi

  if ! have_cmd cargo; then
    fail "cargo is unavailable. Cannot install $name."
    return 1
  fi

  if confirm "Install cargo tool '$name'?"; then
    cargo install "$name"
  else
    warn "Skipped cargo tool '$name'"
  fi
}

# Hand the whole job to linux/packaging/install-deps.sh — it already knows the
# package names for apt/dnf/pacman/zypper. The apt-only list that used to live
# here failed outright on Fedora (no build-essential, no libssl-dev …) and
# drifted from the real list every time a dependency was added.
install_system_deps() {
  local deps
  deps="$(cd "$(dirname "$0")/.." && pwd)/linux/packaging/install-deps.sh"

  if [ ! -f "$deps" ]; then
    fail "Not found: $deps"
    warn "Install the Tauri/Rust build dependencies with your package manager by hand."
    return 1
  fi

  warn "Rust and Tauri on Linux require several system packages."
  printf '%s\n' "They will be installed with your distro's package manager"
  printf '%s\n' "(apt / dnf / pacman / zypper), which asks before each step."

  if confirm "Run $deps now?"; then
    bash "$deps" --ask || warn "Dependency step reported problems — check the output above"
  else
    warn "Skipped system package installation"
  fi
}

print_next_steps() {
  printf '\n'
  say "Post-install next steps"
  printf '%s\n' "1. Open a new shell or source \$HOME/.cargo/env"
  printf '%s\n' "2. Verify tools with: rustc --version && cargo --version"
  printf '%s\n' "3. Run ./scripts/check-deps.sh to validate remaining dependencies"
  printf '%s\n' "4. Add Android and Tauri toolchains before working on a1/ or l1/"
  printf '%s\n' "5. Use cargo fmt and cargo clippy as baseline quality checks"
  printf '\n'
  printf '%s\n' "Useful tools expected after setup:"
  printf '%s\n' "  cargo-watch"
  printf '%s\n' "  cargo-edit"
  printf '%s\n' "  cargo-outdated"
  printf '%s\n' "  cargo-audit"
}

main() {
  say "Starting guided Rust environment setup for Vortex-Lab"
  install_rustup
  load_cargo_env
  ensure_stable_toolchain || true
  install_components || true

  install_cargo_tool cargo-watch
  install_cargo_tool cargo-edit
  install_cargo_tool cargo-outdated
  install_cargo_tool cargo-audit

  install_system_deps
  print_next_steps
}

main "$@"
