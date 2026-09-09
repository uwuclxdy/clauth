#!/usr/bin/env bash
# CDX-5 proxy — CLIENT-INTEGRATION probe (manual, needs a local `codex` binary).
#
# The Rust stub-upstream e2e (tests/inline/proxy.rs) proves the proxy's OWN
# behavior: identity injection, 429 rotate-and-replay, SSE relay, usage capture.
# What it CANNOT prove is the other half — that the real `codex` CLI actually
# routes its model traffic through a localhost proxy when config.toml carries
# clauth's `[model_providers.clauth]` block. This probe closes that gap.
#
# It is SAFE to run anywhere codex is installed, including a host with a live
# codex session, because everything is isolated:
#   - isolated CODEX_HOME (mktemp)  → the real ~/.codex is never read or written
#   - FAKE fresh tokens             → no real account is touched (exp+10d so codex
#                                      trusts them and skips refresh)
#   - a local capture-stub stands in for `clauth proxy` and answers on 127.0.0.1
#                                      → chatgpt.com is never contacted for the
#                                        model turn (see the out-of-band note below)
#
# Verified 2026-07-16 on herdr (Ubuntu 24.04, codex-cli 0.144.5): 6/6 — codex
# routed GET /backend-api/codex/models + POST /backend-api/codex/responses through
# the localhost base, carrying Authorization + ChatGPT-Account-ID; real ~/.codex
# untouched (sha256 identical).
#
# OUT-OF-BAND NOTE: codex ALSO fires a usage/rmcp preflight that does NOT use the
# provider base_url — it hits chatgpt.com directly (401s on the fake token here,
# harmlessly). So the proxy is a MODEL-TURN interceptor by design: the 429s that
# drive fallback ride on /responses, which IS intercepted. Do not expect the proxy
# to see codex's usage-preflight traffic (clauth never calls wham/usage anyway).
set -uo pipefail

command -v codex >/dev/null || { echo "SKIP: no codex binary on PATH"; exit 0; }

EXP="$(mktemp -d)/codex-exp"
mkdir -p "$EXP"
LOG="$EXP/stub-requests.log"
PORTFILE="$EXP/port"
REAL_AUTH="$HOME/.codex/auth.json"
if [ -f "$REAL_AUTH" ]; then
  REAL_SHA_BEFORE="$(sha256sum "$REAL_AUTH" 2>/dev/null | cut -d' ' -f1 || shasum -a256 "$REAL_AUTH" | cut -d' ' -f1)"
fi

cleanup() { [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null; rm -rf "$(dirname "$EXP")"; }
trap cleanup EXIT

# --- fake FRESH auth (exp+10d so codex trusts it and skips any refresh) -------
python3 - "$EXP/auth.json" <<'PY'
import base64, json, sys, time
from datetime import datetime, timezone
def b64(d): return base64.urlsafe_b64encode(d).decode().rstrip("=")
def jwt(c): return f"{b64(json.dumps({'alg':'RS256','typ':'JWT'}).encode())}.{b64(json.dumps(c).encode())}.fake-sig"
now=int(time.time()); acct="acct-exp-local"
auth={"auth_mode":"chatgpt","tokens":{
 "id_token":jwt({"email":"exp@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"pro","chatgpt_account_id":acct}}),
 "access_token":jwt({"exp":now+10*86400,"sub":"exp","https://api.openai.com/auth":{"chatgpt_account_id":acct}}),
 "refresh_token":"rt-exp-local-FAKE","account_id":acct},
 "last_refresh":datetime.now(timezone.utc).isoformat()}
open(sys.argv[1],"w").write(json.dumps(auth,indent=2))
PY
chmod 600 "$EXP/auth.json"

# --- capture-stub: binds :0, logs each request, answers a codex-shaped SSE ----
python3 - "$LOG" "$PORTFILE" <<'PY' &
import socket, sys, time
log, portfile = sys.argv[1], sys.argv[2]
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0)); srv.listen(8)
open(portfile, "w").write(str(srv.getsockname()[1]))
srv.settimeout(25)
body = b'data: {"type":"response.output_text.delta","delta":"hi"}\n\ndata: {"type":"response.completed","response":{"id":"r","status":"completed","output":[]}}\n\ndata: [DONE]\n\n'
resp = (b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"
        b"x-codex-primary-used-percent: 42.0\r\nx-codex-primary-reset-at: 1900000000\r\n"
        b"x-codex-secondary-used-percent: 3.0\r\n"
        b"Content-Length: %d\r\nConnection: close\r\n\r\n" % len(body)) + body
end = time.time() + 22
while time.time() < end:
    try: conn, _ = srv.accept()
    except socket.timeout: break
    data = b""
    conn.settimeout(4)
    try:
        while b"\r\n\r\n" not in data:
            chunk = conn.recv(4096)
            if not chunk: break
            data += chunk
    except socket.timeout: pass
    head = data.split(b"\r\n\r\n",1)[0].decode("latin1")
    lines = head.split("\r\n")
    reqline = lines[0] if lines else "?"
    hdr = {l.split(":",1)[0].strip().lower(): l.split(":",1)[1].strip() for l in lines[1:] if ":" in l}
    with open(log,"a") as f:
        f.write("REQLINE: %s\n" % reqline)
        f.write("  has-authorization: %s\n" % ("yes" if "authorization" in hdr else "NO"))
        f.write("  chatgpt-account-id: %s\n" % hdr.get("chatgpt-account-id","<absent>"))
        f.write("  originator: %s\n" % hdr.get("originator","<absent>"))
    try: conn.sendall(resp); conn.close()
    except Exception: pass
PY
STUB_PID=$!

for _ in $(seq 1 50); do [ -s "$PORTFILE" ] && break; sleep 0.1; done
PORT="$(cat "$PORTFILE" 2>/dev/null)"
[ -z "$PORT" ] && { echo "FAIL: stub never bound a port"; exit 1; }

# --- config.toml = EXACT `clauth proxy --print-config` block ------------------
cat > "$EXP/config.toml" <<CFG
model_provider = "clauth"

[model_providers.clauth]
name = "openai"
base_url = "http://127.0.0.1:${PORT}/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
CFG

echo "=== isolated CODEX_HOME=$EXP, proxy-stub on 127.0.0.1:$PORT ==="
CODEX_HOME="$EXP" timeout 30 codex exec --skip-git-repo-check -c model="gpt-5.6" \
  "reply with the single word: hi" >"$EXP/codex.out" 2>&1
echo "codex exec exit code: $?"
sleep 0.5

echo; echo "=== STUB CAPTURE LOG ==="
[ -s "$LOG" ] && cat "$LOG" || echo "(stub captured NOTHING — codex did not route to the localhost proxy)"

echo; echo "=== ASSERTIONS ==="
PASS=0; TOTAL=0
chk() { TOTAL=$((TOTAL+1)); if eval "$2"; then echo "  PASS  $1"; PASS=$((PASS+1)); else echo "  FAIL  $1"; fi; }
chk "codex routed a request to the localhost proxy (stub saw traffic)" '[ -s "$LOG" ]'
chk "the routed request hit /backend-api/codex/responses" 'grep -q "REQLINE: POST /backend-api/codex/responses" "$LOG"'
chk "codex attached OpenAI auth (Authorization present)" 'grep -q "has-authorization: yes" "$LOG"'
chk "codex sent a ChatGPT-Account-ID for the proxy to overwrite" 'grep -q "chatgpt-account-id: acct-exp-local" "$LOG"'
chk "request carried a codex originator (codex_exec / codex_cli_rs)" 'grep -Eq "originator: codex_(exec|cli)" "$LOG"'
if [ -f "$REAL_AUTH" ]; then
  REAL_SHA_AFTER="$(sha256sum "$REAL_AUTH" 2>/dev/null | cut -d' ' -f1 || shasum -a256 "$REAL_AUTH" | cut -d' ' -f1)"
  chk "REAL ~/.codex/auth.json UNTOUCHED (sha256)" '[ "$REAL_SHA_BEFORE" = "$REAL_SHA_AFTER" ]'
fi

echo; echo "=== RESULT: $PASS/$TOTAL ==="
[ "$PASS" = "$TOTAL" ] && echo "ALL GREEN — real codex CLI routes its model turn through the clauth proxy block; real account untouched." || echo "PARTIAL — see failures above."
