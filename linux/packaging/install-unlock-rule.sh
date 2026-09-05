#!/usr/bin/env bash
#
# install-unlock-rule.sh — grant THIS user the right to unlock their own
# desktop session, which is what "unlock on return" / the phone's unlock
# button need.
#
# WHY this is a separate, opt-in script and not part of install_linux.sh:
# everything else vortex installs lives under $HOME and disappears with the
# user. This writes to /etc and changes what the system authorises, so it is
# never done silently — you run it, or the feature stays off. `--remove` puts
# the machine back exactly as it was.
#
# WHAT it grants, stated plainly: the polkit action
# `org.freedesktop.login1.lock-sessions`, which logind describes as "Lock or
# unlock active sessions". Granted to ONE named user, and only from a local
# seat (never over SSH). On a shared machine that user can then also unlock
# ANOTHER logged-in user's locked session without a password — polkit has no
# finer-grained action for this, so if that matters on your machine, do not
# install the rule and leave remote unlock off.
#
# Locking never needed this: it goes through the unprivileged session bus.
#
# Usage:
#   sudo ./install-unlock-rule.sh            install for the invoking user
#   sudo ./install-unlock-rule.sh --user bob install for a named user
#   sudo ./install-unlock-rule.sh --remove   remove the rule
#   ./install-unlock-rule.sh --status        report whether it is installed
set -euo pipefail

RULE_PATH="/etc/polkit-1/rules.d/49-vortex-unlock.rules"
ACTION="org.freedesktop.login1.lock-sessions"

MODE="install"
TARGET_USER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --remove) MODE="remove" ;;
    --status) MODE="status" ;;
    --user)   TARGET_USER="${2:-}"; shift ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "✗ unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# SUDO_USER is the human who typed sudo; falls back to whoever is running us.
if [ -z "$TARGET_USER" ]; then
  TARGET_USER="${SUDO_USER:-$(id -un)}"
fi

if [ "$MODE" = "status" ]; then
  if [ -f "$RULE_PATH" ]; then
    echo "✔ installed: $RULE_PATH"
    grep -o 'subject.user == "[^"]*"' "$RULE_PATH" 2>/dev/null | sed 's/^/  /' || true
  else
    echo "✗ not installed — remote unlock will fail with access-denied."
    echo "  Install with: sudo $0"
  fi
  exit 0
fi

[ "$(id -u)" -eq 0 ] || { echo "✗ run this with sudo (it writes $RULE_PATH)" >&2; exit 1; }

if [ "$MODE" = "remove" ]; then
  if [ -f "$RULE_PATH" ]; then
    rm -f "$RULE_PATH"
    echo "✔ removed $RULE_PATH — remote unlock is off again."
  else
    echo "· nothing to remove ($RULE_PATH does not exist)."
  fi
  exit 0
fi

id -u "$TARGET_USER" >/dev/null 2>&1 || { echo "✗ no such user: $TARGET_USER" >&2; exit 1; }

# polkit needs the .rules directory to exist; on a minimal install it may not.
install -d -m 755 /etc/polkit-1/rules.d

# subject.local keeps this off SSH sessions: an attacker with the user's
# password over the network still cannot dismiss the physical lock screen.
# We deliberately do NOT also require subject.active — a session that is
# LOCKED is exactly when we need this, and gating on active state risks the
# rule not applying in the only case it exists for.
cat > "$RULE_PATH" <<EOF
// Installed by vortex: linux/packaging/install-unlock-rule.sh
// Lets $TARGET_USER unlock their desktop session from a local seat, which is
// what vortex's phone-presence unlock and the phone's unlock button use.
// Remove with: sudo <vortex>/linux/packaging/install-unlock-rule.sh --remove
polkit.addRule(function (action, subject) {
    if (action.id == "$ACTION" &&
        subject.user == "$TARGET_USER" &&
        subject.local) {
        return polkit.Result.YES;
    }
});
EOF
chmod 644 "$RULE_PATH"

echo "✔ installed $RULE_PATH"
echo "  user:   $TARGET_USER (local seats only, never over SSH)"
echo "  action: $ACTION — \"Lock or unlock active sessions\""
echo "  undo:   sudo $0 --remove"
echo
echo "polkit picks new rules up immediately; no reboot or restart is needed."
