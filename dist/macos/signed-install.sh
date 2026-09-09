#!/usr/bin/env bash
# signed-install.sh — build + install clauth, then re-sign it with a STABLE code
# signing identity so the macOS Keychain "Always Allow" grant persists.
#
# Why: clauth writes the `Claude Code-credentials` login Keychain item on a switch
# (that's how a switch actually changes the running Claude Code account on macOS —
# see SECURITY.md § "Fork surfaces (macOS)"). macOS records that "Always Allow"
# against the app's *code
# signature*. `cargo install` produces an ad-hoc/linker signature whose identity
# changes on every rebuild, so "Always Allow" never sticks and every switch
# re-prompts. Signing with a stable identity fixes that: approve once, then silent.
#
# Usage:
#   dist/macos/signed-install.sh                 # auto-pick a codesigning identity
#   CLAUTH_SIGN_IDENTITY="Apple Development: you@example.com (TEAMID)" \
#       dist/macos/signed-install.sh             # pin a specific identity
#
# After the first run, do ONE `clauth <switch>` and click "Always Allow" on the
# Keychain prompt — subsequent switches won't prompt again.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

echo "clauth: building + installing (cargo install --path . --force)…"
cargo install --path . --force

bin="$(command -v clauth || echo "$HOME/.cargo/bin/clauth")"
if [ ! -x "$bin" ]; then
  echo "error: installed clauth not found on PATH or in ~/.cargo/bin" >&2
  exit 1
fi

# Resolve a signing identity: explicit override, else the first codesigning
# identity in the login keychain.
identity="${CLAUTH_SIGN_IDENTITY:-}"
if [ -z "$identity" ]; then
  identity="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(.*\)".*/\1/p' | head -1)"
fi

if [ -z "$identity" ]; then
  echo "warning: no codesigning identity found — leaving the ad-hoc signature."
  echo "         'Always Allow' will NOT persist; every account switch will re-prompt."
  echo "         Create a local one in Keychain Access → Certificate Assistant →"
  echo "         Create a Certificate → (Code Signing), then re-run this script."
  exit 0
fi

echo "clauth: signing with identity: $identity"
codesign --force --options runtime --sign "$identity" "$bin"
codesign -dvv "$bin" 2>&1 | grep -iE 'Authority=|Signature=' | head -2 || true

# Restart the resident daemon so it runs the freshly re-signed binary. Without
# this a reinstall leaves the OLD inode making switch decisions until the next
# login — stale for weeks (TECH-8, finding #37). kickstart -k kills the running
# instance; launchd's KeepAlive re-launches it from the new binary.
label="com.clauth.daemon"
plist="$HOME/Library/LaunchAgents/$label.plist"
uid="$(id -u)"
if [ -f "$plist" ]; then
  echo "clauth: restarting the resident daemon ($label) onto the new binary…"
  launchctl kickstart -k "gui/$uid/$label" 2>/dev/null \
    || echo "         (kickstart failed — restart manually: launchctl kickstart -k gui/$uid/$label)"
else
  echo "clauth: daemon not installed (no $label.plist) — skipping restart."
fi

echo
echo "Done. Next: run one 'clauth <profile>' switch and click 'Always Allow' on the"
echo "Keychain prompt. It will persist — later switches won't prompt again."
