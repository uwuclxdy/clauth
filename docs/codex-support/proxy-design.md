# CDX-5: localhost injection proxy — design

**Status: designed + SHIPPED 2026-07-16 (own design round per PLAN.md §0.8.4).**
Delivers the tier the whole ladder points at: **true in-session, per-request codex account
fallback** — a 429 mid-conversation rotates to a sibling account and replays before the
client sees a byte, the exact semantics clauth's claude side gets from the Keychain lead.

Ground truth for every wire fact below: openai/codex @ `9ff47868` (main HEAD, 2026-07-16,
local codex-cli 0.144.5) + a prior-art source read of Soju06/codex-lb (strongest reference,
read directly), VallierDev/codex-switcher, christiandoxa/prodex, ndycode/codex-multi-auth.
Full research: session scratchpad → distilled here; feasibility §0b remains the summary.

---

## 1. Decisions (volatile-first)

### 1.1 Own process: `clauth proxy` (not inside the daemon)

The daemon's deploy discipline is `pkill` = restart — an in-daemon proxy would sever every
in-flight codex stream on each clauth deploy, turning "restart the daemon" from free into
disruptive. The proxy is its own foreground subcommand (`clauth proxy [--port N]`), its own
failure domain (proxy-down = codex-down, never clauth-down), AX can wrap it in a
LaunchAgent later. It shares state with the daemon the same way the TUI does: profile
store reads, `RotationGuard` flocks, per-profile usage cache writes.

### 1.2 SSE only — `supports_websockets` stays false (MVP boundary)

At HEAD, transport selection is: WebSocket iff the provider config sets
`supports_websockets = true` (default **false**) — otherwise the SSE HTTP POST path
(`Accept: text/event-stream`). The codex **CLI** on a default custom provider therefore
speaks plain HTTP+SSE end to end; only Codex.app (desktop) leans WS. clauth's proxy
ships SSE-only and the printed config omits `supports_websockets` — no WS bridging, no
TLS, no 2 MB WS budget trimming (the single heaviest chunk of codex-lb). Documented
limitation: Codex.app via WS is out of scope until the CLI itself moves.

### 1.3 Config wiring is printed, never written

ndycode/codex-multi-auth's auto-editing of `~/.codex/config.toml` orphaned user configs
(their issue #614). clauth NEVER writes the live codex config. `clauth proxy --print-config`
emits the exact block; AX pastes it (live-config edits are operator actions per the
standing constraints):

```toml
model_provider = "clauth"

[model_providers.clauth]
name = "openai"                                    # lowercase — codex ≥2026-05-23 resolves case-sensitively
base_url = "http://127.0.0.1:4517/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
```

`requires_openai_auth = true` mirrors codex-lb's proven config: codex keeps its normal
login state (live `auth.json` untouched, codex attaches its own auth headers), and the
proxy **strips and replaces** the identity headers per request. (feasibility §0b's survey
line says `requires_openai_auth = false` — that is the *unauthenticated* community route,
which also works; this design deliberately picks the codex-lb `true` route because codex's
own login/refresh state stays intact and turning the proxy off is a pure config revert.
Both statements are true of different tools; this doc is the decision.) Plain
`http://127.0.0.1` is accepted verbatim (no scheme check at HEAD; built-in OSS providers
ship plain http). Default port 4517 (unclaimed; configurable).

### 1.4 Identity injection = strip + two headers

Per request the proxy removes inbound `Authorization` and `ChatGPT-Account-ID` (**all**
occurrences, case-insensitive — duplicates are stripped too, so a smuggled second header
can never ride along), plus `Host`, `Content-Length` (recomputed), `Accept-Encoding`
(forced to identity — see §1.8: the proxy must be able to relay bytes without owning a
decompression story), and the hop-by-hop set (`Connection`, `Keep-Alive`, `Proxy-*`,
`Transfer-Encoding`, `TE`, `Trailer`, `Upgrade`). It injects the selected account's
`Authorization: Bearer <access_token>` + `ChatGPT-Account-ID: <account_id>` (the exact
pair codex's `BearerAuthProvider` builds). Everything else passes through verbatim —
`originator`, `User-Agent`, `session-id`, `thread-id`, `x-client-request-id`,
`x-openai-subagent`, `x-codex-turn-state`, `OpenAI-Beta`, `Accept` — the request stays
indistinguishable from a native one apart from the account identity. Upstream =
`https://chatgpt.com/backend-api/codex<path-suffix>` — scheme+authority are a compiled-in
constant (client bytes can never relocate them; an absolute-form request target or a path
that doesn't begin under the expected prefix is answered 404 without forwarding), with a
test-only base override threaded through the listener constructor so the sandbox e2e can
point at a local stub upstream (production code path never reads it from config).

### 1.5 Pool = the codex fallback chain; sticky-active selection

The pool is `codex_fallback_chain` when non-empty, else every codex profile with a stored
login. Selection is **sticky to the active codex profile** (prompt-cache affinity — the
prior-art lesson: cache keys are per-account) and rotates only on signal: upstream 429,
`x-codex-rate-limit-reached-type` verdict, or 401 after a refresh attempt. Rotation order
= chain order (the CDX-4 walk's order), skipping auth_broken / leased members. A member
whose CACHED usage reads spent ranks LAST instead of being skipped (two-tier walk): the
cache is advisory — it can be arbitrarily stale (a plan upgrade resets real limits without
touching it, and while the proxy serves it is the sole usage writer, so refusing to route
on cache alone wedges into synthesized 429s with no correction path; observed live
2026-07-20). Upstream's own answer is the authority; a real 429 stamps the cooldown.
Rotation is **pre-commit only** (prodex's rule): a response that has
streamed any byte to the client is never retried; a 429/401/5xx **before the first byte**
picks the next account and replays the buffered request. Per-account cooldown after a
429 = its advertised reset when the `x-codex-*` headers carry one, else 60 s.

### 1.6 Token freshness: live-owner reads live; parked chains refresh via CDX-3

For the live-owner profile the proxy injects the access token from the **live**
`~/.codex/auth.json` (read-only — codex itself keeps that chain fresh; the stored snapshot
may lag between adopt-backs). For every other pool member it injects from the store, and
when the access token is inside the expiry margin it refreshes through the CDX-3 machinery
— same `RotationGuard`, same exclusivity predicate, same taxonomy — so the proxy and the
daemon's standby scan can never double-spend a chain (the flock is cross-process; codex-lb
had to bolt on "refresh claims" for exactly this race). An account whose token can't be
made fresh is skipped with a logline, never a 500.

### 1.7 Free usage feed: the flowing `x-codex-*` headers

Every upstream response carries `x-codex-primary-*`/`x-codex-secondary-*`/
`x-codex-rate-limit-reached-type` headers. The proxy parses them (same shapes as
`codex::usage`) and writes the owning profile's usage cache file — per-ACCOUNT live usage
with zero extra requests, better attribution than the passive JSONL leg (which only ever
sees the live account). The daemon's cache hydrate/status.json pick it up unchanged.
`wham/usage` stays never-called *by the proxy* — since 2026-07-22 the daemon's CDX-6 leg
polls it read-only per profile (AX reversal; see feasibility §2.5's dated note).

**Passive-leg handoff (review finding, 2026-07-16 — the two feeds are NOT benignly
concurrent):** once the proxy rotates a request to account Y, codex logs **Y's** rate-limit
counters into session JSONLs that the identity-less passive leg would attribute to the
active profile X (the mtime gate never trips — the proxy doesn't touch the live file). So
the proxy maintains a heartbeat file (`~/.clauth/codex-proxy.json`: port + last-request
stamp, atomic write), and `codex_passive_tick` **stands down from publishing while the
heartbeat is fresh** (< 2× the refresh interval) — while the proxy serves, the proxy's
header feed is the only codex usage writer. Heartbeat gone stale (proxy stopped) → the
passive leg resumes seamlessly.

### 1.8 HTTP stack: hand-rolled listener + ureq upstream (no new runtime)

clauth's tokio is `rt,io-std` only and ureq is blocking; adding hyper/axum for one
loopback endpoint is the heavy path feasibility flagged. The proxy is a
`std::net::TcpListener` accept loop with a thread per connection (codex holds few
concurrent requests; thread-per-conn is fine at this scale), a minimal HTTP/1.1 request
reader, and ureq 3 for upstream (`send(&[u8])` for the buffered body, streaming
`body_mut().as_reader()` copied chunkwise to the client socket for SSE). Request bodies
are buffered in full anyway for replay (§1.5); responses stream through with a small
copy buffer. Timeouts: 30 s connect-idle on the client read, upstream connect timeout
10 s, and a 2 h `timeout_global` on the upstream call as a pure LEAK BACKSTOP. Two
ureq-3 semantics are pinned by tests because each one caused (or would cause) a
stream-truncation incident: `timeout_global` fires even while bytes are actively
flowing, and `timeout_recv_response` keeps running through the BODY read — it is NOT
a headers-only bound. The 2026-07-18 incident's assassin was a 30 s
`timeout_recv_response`: every model turn whose stream outlived 30 s was truncated
mid-body (`TRUNCATED … timeout: receive response` at 29 s once the relay summaries
existed), which codex reports as "stream closed before response.completed" and
replays from scratch — hence PROXY_AGENT sets NO recv-response timeout at all.
Turn-end is detected by the relay itself, never by a timeout — see the
terminal-sniffer paragraph below.

**Request-read contract:** request line + headers + `Content-Length`-sized body.
`Transfer-Encoding: chunked` requests are answered `411 Length Required` (codex/reqwest
sends sized JSON bodies; the 411 is the explicit contract, not a hang). Header block
capped at 64 KiB, body at 64 MiB (a full resent conversation is single-digit MiB) —
over-cap answers `431`/`413` and closes. These caps are robustness, not a security
boundary (§1.9's trust model already grants local processes the quota).

**Response-relay contract:** status line + upstream headers relayed verbatim MINUS the
hop-by-hop set (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `Proxy-*`, `TE`,
`Trailer`, `Upgrade`); bytes are forwarded exactly as read from ureq's response reader.
Because §1.4 forces identity encoding upstream, no compression negotiation ever reaches
the relay. Framing toward the client: `Content-Length` passes through when upstream sent
one; otherwise the proxy answers `Connection: close` and delimits by EOF (SSE always
takes this path). Every connection is single-request (`Connection: close` on all
responses) — loopback TCP setup is negligible and it removes keep-alive state entirely.

**Turn-end detection (`proxy::sse::TerminalSniffer`, added 2026-07-18):** the upstream
does NOT reliably close the response after the turn's final `response.completed` —
it can hold the SSE stream open with keepalives while codex closes its own side.
Waiting for upstream EOF therefore leaked a lingering thread per turn (one spurious
"connection error" logline each, pile-up toward the 64-connection cap). The relay
sniffs the stream for terminal events (`response.completed`/`.failed`/`.incomplete`,
`[DONE]`), in both wire forms (`event:` name line and `data: {"type":...}` payload
line — live-captured 2026-07-18). Two hard-won rules, both pinned by tests: the
terminal DATA line is one huge line (100s of KB), so lines are classified by prefix
without waiting for their newline; and classification only ARMS — the close FIRES at
the event's blank-line terminator, so the terminal event is always relayed in full
(firing on the prefix truncates the completed event and reintroduces the exact
failure this exists to prevent). Model text quoting "response.completed" can never
false-trigger: it rides mid-line inside a delta's JSON string. A wire-format drift
degrades to EOF/backstop behavior — never truncation. Each request now logs ONE
timestamped summary line (account, method, path, status, bytes, seconds, end shape:
`completed` / `upstream EOF` / `client closed` / `TRUNCATED … upstream error`).

### 1.9 Loopback trust (accepted risk, logged)

Bind `127.0.0.1` only. No client auth token in the MVP (codex-lb ships the same posture;
codex has no header slot for a proxy secret without config gymnastics) — any local
process can spend the pool's quota. Single-user machine; documented in README +
`--print-config` output; a shared-secret knob is a follow-up if ever needed.

### 1.10 Non-goals

WebSocket bridging (§1.2) · TLS on loopback · mid-stream rotation (2 KB-window tricks —
codex-switcher's territory, replay-unsafe by construction) · request/response body
inspection or rewriting (bodies pass through byte-exact) · multi-upstream/provider
translation · auto-editing codex config · daemon-embedded mode.

---

## 2. Request lifecycle (one turn)

```
codex CLI ──POST /backend-api/codex/responses──▶ clauth proxy (127.0.0.1:4517)
  1. read request head + sized body (buffer)
  2. pick account: sticky active → chain walk (skip broken/leased/cooldown; cache-spent ranks last)
  3. ensure fresh access token (live-owner: read live; parked: CDX-3 refresh if margin)
  4. strip identity headers, inject Authorization + ChatGPT-Account-ID
  5. ureq POST https://chatgpt.com/backend-api/codex/responses (stream response)
  6a. status < 400 → stream bytes to client; parse x-codex-* headers → usage cache
  6b. 429/401/5xx BEFORE first byte → stamp cooldown/flag → next account → replay (≤ pool size attempts)
  6c. error AFTER first byte → propagate as-is (pre-commit rule)
```

## 3. Tasks

- [x] **P1 `src/proxy/http.rs`** — minimal HTTP/1.1 read/write primitives (request head
  parse, sized body read, response head write, chunked-free SSE relay loop). Pure parse
  fns unit-tested over byte fixtures (torn heads, oversized heads AND bodies, missing
  length, chunked request → 411, absolute-form target, duplicate Authorization headers).
- [x] **P2 `src/proxy/pool.rs`** — account selection (§1.5/§1.6): pure `select_account`
  over a pool snapshot (sticky/cooldown/skip predicates — exhaustive table tests) +
  token-freshness resolution seam (injectable so tests never touch HTTP).
- [x] **P3 `src/proxy/mod.rs`** — listener loop (upstream base injectable for the stub-
  upstream e2e, §1.4), per-connection handler, replay loop (§2), header strip/inject
  (unit-tested as pure header-map transforms), usage-header capture → cache write (reuse
  `codex::usage` shapes + `write_profile_cache`), heartbeat file (§1.7) + the
  `codex_passive_tick` stand-down gate + its test. Loglines are token-value-free by
  invariant (names/status/reasons only — the `token_parse_error` discipline).
- [x] **P4 CLI** — `clauth proxy [--port N]`, `clauth proxy --print-config [--port N]`;
  help text; README fork-features section.
- [x] **P5 surfaces** — doctor: proxy check (port listening? config block present in
  `~/.codex/config.toml`? — read-only sniff) WARN-only; DESIGN.md note that usage
  cache freshness may now come from the proxy.

**Acceptance (CDX-5):** suite green; sandbox e2e with a stub upstream server (local
TcpListener fixture): request forwarded with injected identity + stripped inbound auth;
429-then-200 rotates account and replays; post-first-byte failure propagates; usage
headers land in the cache file; SSE bytes relayed exactly. Live end-to-end against the
real backend (config paste + real codex turn + real 429 rotation) is AX-manual
acceptance — never run unattended.

## 3b. Post-ship adversarial review (2026-07-16 — all findings fixed)

A 4-dimension adversarial workflow + refute pass ran over the shipped proxy. It
confirmed one **CRIT** and folded fixes for it plus every MED/LOW back in:

- **CRIT (fixed): parked-chain double-spend.** `ensure_fresh_parked` was a
  SECOND, guardless refresh implementation that read the refresh token BEFORE
  acquiring the `RotationGuard` and never re-checked inside it — two proxy
  threads (or proxy-vs-daemon) could both spend the same single-use token →
  server-side reuse-detection → permanent chain kill. Fix: deleted the copy;
  the proxy now delegates to the daemon's correct `codex_refresh_parked` — THE
  single machine-wide entry point (adopt-back-first → guard → in-guard re-read
  + `standby_due` re-check → spend → apply-time token-identity re-check).
- **MED (fixed): SIGKILL-stranded chain re-spend.** A crashed isolated
  `clauth start` session can leave a chain in its `codex-home` fresher than the
  store; the standby refresh would then spend the store's already-rotated token.
  `codex_refresh_parked` now runs `gc_one_codex_runtime` (adopt-back) BEFORE the
  guard, so the store reflects the freshest chain first.
- **MED (fixed): `switch_backoff` cross-harness clobber.** The single backoff
  slot ping-ponged between a stuck claude target and a stuck codex target,
  re-arming the 1/tick log storm. Now keyed by harness (`HashMap<Harness, …>`).
- **LOW (fixed): transport-error pool-walk** → fail fast with 502
  (`UpstreamUnreachable`) instead of walking every account against the same
  unreachable fixed host. **LOW (fixed): non-POST silently rewritten** → 405.
  **LOW (fixed): unbounded upstream body read** → `PROXY_AGENT` with a 15-min
  global timeout (longer than any single turn's stream). **LOW (fixed):
  unbounded thread-per-connection** → 64-connection cap (503 over it).
- Confirmed CLEAN and upheld: pre-commit replay boundary, header strip/inject
  (all casings/duplicates), fixed-authority URL construction, SSE framing,
  no token exposure in logs, no `~/.codex` writes, parser underflow-free.

## 4. ToS posture

Unchanged from feasibility §0b: same risk class as the claude side. Guardrails carried
into the design: rotation only on limiter signal with per-account cooldown (no rapid
free rotation), identity headers always a coherent pair from one account, native
headers preserved verbatim, no backend polling (`wham/usage` never called), passive +
flow-through usage only. Community trackers show no ban reports for this pattern;
risk stays non-zero and stays AX's call — the proxy is opt-in by config paste.
