#!/usr/bin/env bash
# Sandboxed end-to-end codex account-switch verification (PLAN.md CDX-1
# acceptance: "sandboxed end-to-end test with two fake codex accounts").
#
# Runs a SECOND clauth daemon against an isolated $HOME with two FAKE codex
# accounts, then proves: (1) a user-initiated switch swaps the fake
# ~/.codex/auth.json bytes exactly; (2) a forged rate-limited session JSONL
# (2026-07 weekly-only shape) drives the passive tick -> chain scan -> drain
# into a REAL auto-switch. The real ~/.codex and running codex CLIs are never
# touched — proven by before/after hashes.
set -uo pipefail

SIM_DIR="$(mktemp -d /tmp/clauth-codex-sim.XXXXXX)"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN="$REPO/target/release/clauth"
HOME_SIM="$SIM_DIR/home"
LOG="$SIM_DIR/daemon.log"
EVIDENCE="$SIM_DIR/EVIDENCE.txt"

PASS=0; FAIL=0
note() { echo "$*" | tee -a "$EVIDENCE"; }
check() { # check <label> <cmd...>
  local label="$1"; shift
  if "$@" >/dev/null 2>&1; then PASS=$((PASS+1)); note "  PASS: $label"
  else FAIL=$((FAIL+1)); note "  FAIL: $label"; fi
}
sim() { HOME="$HOME_SIM" CLAUTH_NO_UPDATE=1 CLAUTH_NO_COMPLETIONS=1 "$BIN" "$@"; }

DAEMON_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null && wait "$DAEMON_PID" 2>/dev/null
}
trap cleanup EXIT

if [ ! -x "$BIN" ]; then
  echo "codex-sim: build first — cargo build --release" >&2
  exit 2
fi

rm -rf "$HOME_SIM" "$EVIDENCE" "$LOG"
mkdir -p "$HOME_SIM/.codex"
: > "$EVIDENCE"

note "=== codex switch simulation · $(date -u +%FT%TZ) ==="
note "binary: $BIN ($(cd "$REPO" && git log --oneline -1))"

# -- real-home safety baseline -------------------------------------------------
REAL_BEFORE=$(shasum -a 256 ~/.codex/auth.json | awk '{print $1}')
note "real ~/.codex/auth.json sha256 (before): $REAL_BEFORE"

# -- seed + capture two fake accounts ------------------------------------------
python3 "$SCRIPT_DIR/make_auth.py" a > "$HOME_SIM/.codex/auth.json"
A_BYTES=$(cat "$HOME_SIM/.codex/auth.json")
note "--- capture sim-a ---"
sim login sim-a --codex 2>&1 | tee -a "$EVIDENCE"

python3 "$SCRIPT_DIR/make_auth.py" b > "$HOME_SIM/.codex/auth.json"
B_BYTES=$(cat "$HOME_SIM/.codex/auth.json")
note "--- capture sim-b ---"
sim login sim-b --codex 2>&1 | tee -a "$EVIDENCE"

note "--- chain config ---"
sim fallback add sim-a 2>&1 | tee -a "$EVIDENCE"
sim fallback add sim-b 2>&1 | tee -a "$EVIDENCE"
sim fallback list 2>&1 | tee -a "$EVIDENCE"

# -- daemon up -------------------------------------------------------------------
HOME="$HOME_SIM" CLAUTH_NO_UPDATE=1 CLAUTH_NO_COMPLETIONS=1 "$BIN" daemon > "$LOG" 2>&1 &
DAEMON_PID=$!
note "sandbox daemon pid $DAEMON_PID (socket+state under $HOME_SIM/.clauth)"
for _ in $(seq 1 30); do [ -f "$HOME_SIM/.clauth/status.json" ] && break; sleep 0.5; done
check "daemon wrote status.json" test -f "$HOME_SIM/.clauth/status.json"

active_codex() { python3 -c "
import json,sys
s=json.load(open('$HOME_SIM/.clauth/status.json'))
print(next((p['name'] for p in s.get('profiles',[]) if p.get('active') and p.get('harness')=='codex'),''))
" 2>/dev/null; }

note "initial active codex: $(active_codex) (last capture wins)"

# ================================================================================
note ""
note "=== TEST 1 · user-initiated switch (socket -> drain -> write_live) ==="
sim sim-a 2>&1 | tee -a "$EVIDENCE"
for _ in $(seq 1 40); do [ "$(active_codex)" = "sim-a" ] && break; sleep 0.5; done
check "status.json active codex == sim-a" test "$(active_codex)" = "sim-a"
check "live auth.json holds sim-a bytes VERBATIM" test "$(cat "$HOME_SIM/.codex/auth.json")" = "$A_BYTES"
check "profiles.toml active_codex_profile == sim-a" grep -q 'active_codex_profile = "sim-a"' "$HOME_SIM/.clauth/profiles.toml"

# ================================================================================
note ""
note "=== TEST 2 · rate-limit auto-switch (forged 2026-07 weekly-only JSONL) ==="
# Attribution gate: the event must be stamped NOT OLDER than the live auth.json
# mtime set by test 1's switch — wait out the second boundary, then stamp now+2s.
sleep 2
SESS_DIR="$HOME_SIM/.codex/sessions/$(date -u +%Y/%m/%d)"
mkdir -p "$SESS_DIR"
python3 - "$SESS_DIR/rollout-sim.jsonl" <<'PY'
import json, sys, time
from datetime import datetime, timedelta, timezone
now = datetime.now(timezone.utc) + timedelta(seconds=2)
line = {
    "timestamp": now.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z",
    "payload": {
        "type": "token_count",
        "rate_limits": {
            # 2026-07 reality: primary IS the 10080-min weekly window; no 5h.
            "primary": {
                "used_percent": 100.0,
                "resets_at": int(time.time()) + 4 * 24 * 3600,
                "window_minutes": 10080,
            },
            "rate_limit_reached_type": "primary",
        },
    },
}
open(sys.argv[1], "w").write(json.dumps(line) + "\n")
PY
note "forged $SESS_DIR/rollout-sim.jsonl (weekly 100% + limiter verdict, resets in 4d)"

ROTATED=""
for _ in $(seq 1 120); do
  if [ "$(active_codex)" = "sim-b" ]; then ROTATED=yes; break; fi
  sleep 1
done
check "daemon auto-rotated active codex to sim-b" test "$ROTATED" = "yes"
check "live auth.json now holds sim-b bytes VERBATIM" test "$(cat "$HOME_SIM/.codex/auth.json")" = "$B_BYTES"
note "--- status.json codex view after rotation ---"
python3 -c "
import json
s=json.load(open('$HOME_SIM/.clauth/status.json'))
for p in s.get('profiles',[]):
    if p.get('harness')=='codex':
        print(' ', p['name'], '| active:', p.get('active'),
              '| email:', p.get('account_email'),
              '| 7d:', (p.get('seven_day') or {}).get('utilization'),
              '| verdict:', p.get('codex_rate_limit_reached'),
              '| fallback:', (p.get('fallback') or {}).get('position'))
" 2>&1 | tee -a "$EVIDENCE"
note "--- daemon log (switch lines) ---"
grep -iE "switch|rotat|codex" "$LOG" | tail -15 | tee -a "$EVIDENCE"

# ================================================================================
note ""
note "=== safety: real home untouched ==="
REAL_AFTER=$(shasum -a 256 ~/.codex/auth.json | awk '{print $1}')
note "real ~/.codex/auth.json sha256 (after):  $REAL_AFTER"
check "real ~/.codex/auth.json unchanged" test "$REAL_BEFORE" = "$REAL_AFTER"
check "real ~/.clauth NOT written by sim (no sim profiles)" bash -c "! grep -rq 'sim-a\|sim-b' ~/.clauth/profiles.toml"

note ""
note "=== RESULT: $PASS pass / $FAIL fail ==="
note "evidence: $EVIDENCE · daemon log: $LOG"
exit $FAIL
