#!/usr/bin/env bash
# daemon-install.sh — install / uninstall the clauth daemon as a macOS
# LaunchAgent so account auto-switch + the ~/.clauth/status.json feed run at
# login with no TUI open. This is what makes the menu-bar app (ccsbar) and
# unattended "switch before the 5h window blocks" work.
#
# Usage:
#   dist/macos/daemon-install.sh            # install + load now (and at login)
#   dist/macos/daemon-install.sh uninstall  # stop + remove
#
# Logs go to ~/.clauth/daemon.log. The daemon holds a single-instance lock, so
# it's safe if you also run `clauth daemon` by hand: the unit runs `--standby`,
# so it parks and takes over when your manual run exits.
set -euo pipefail

label="com.clauth.daemon"
plist_dst="$HOME/Library/LaunchAgents/$label.plist"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
template="$repo_root/dist/macos/$label.plist"
uid="$(id -u)"

case "${1:-install}" in
  uninstall)
    launchctl bootout "gui/$uid/$label" 2>/dev/null || true
    rm -f "$plist_dst"
    echo "clauth daemon: uninstalled ($plist_dst removed)."
    exit 0
    ;;
  install) ;;
  *)
    echo "usage: daemon-install.sh [install|uninstall]" >&2
    exit 2
    ;;
esac

bin="$(command -v clauth || echo "$HOME/.cargo/bin/clauth")"
if [ ! -x "$bin" ]; then
  echo "error: clauth not found on PATH or in ~/.cargo/bin — install it first" >&2
  exit 1
fi

# Capability check: the LaunchAgent runs `clauth daemon`, which only the
# xingfanxia fork has. If the resolved binary is upstream clauth (no daemon
# subcommand), bootstrapping it would crash-loop under KeepAlive every ~10s with
# zero auto-switch. `status --json` is a fork-only, side-effect-free subcommand,
# so it is the cheapest positive proof this binary is the fork.
if ! "$bin" status --json >/dev/null 2>&1; then
  echo "error: '$bin' is not a clauth-fork daemon binary (no 'status --json')." >&2
  echo "       install the fork from source first: ./install.sh" >&2
  exit 1
fi

log="$HOME/.clauth/daemon.log"
path_env="/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:$HOME/.cargo/bin"
mkdir -p "$HOME/.clauth" "$HOME/Library/LaunchAgents"
# TECH-9 #13: ~/.clauth holds credentials, daemon.log, and the control socket —
# keep it user-private (the daemon also enforces this on boot).
chmod 700 "$HOME/.clauth"
[ -d "$HOME/.clauth/profiles" ] && chmod 700 "$HOME/.clauth/profiles"

sed -e "s|__CLAUTH_BIN__|$bin|g" \
    -e "s|__LOG__|$log|g" \
    -e "s|__PATH__|$path_env|g" \
    "$template" > "$plist_dst"

# Reload cleanly (bootout is a no-op if not loaded).
launchctl bootout "gui/$uid/$label" 2>/dev/null || true
launchctl bootstrap "gui/$uid" "$plist_dst"

echo "clauth daemon: installed and running."
echo "  binary: $bin"
echo "  plist:  $plist_dst"
echo "  log:    $log"
echo "  stop:   dist/macos/daemon-install.sh uninstall"
