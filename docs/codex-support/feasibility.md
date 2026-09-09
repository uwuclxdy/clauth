# CDX-0: Codex support feasibility research

**Status: researched 2026-07-12, re-evaluated 2026-07-16 (§0b) · decision: DEFERRED (not started).**
AX chose not to build yet; this doc preserves the full research so a future session can start
CDX-1 with zero re-research.

> **2026-07-16 re-evaluation (§0b) overturns §3.1's headline**: in-session seamless fallback IS
> achievable — not by swapping `auth.json` (still impossible, now doubly guarded at HEAD), but by
> a localhost header-rewriting proxy, the pattern four maintained community tools already ship.
> Read §0b before quoting the old verdict.

> **Partial delivery outside clauth (2026-07-12, same day):** the usage-DISPLAY slice of
> CDX-2 shipped in **ccu** (`~/projects/devtools/ccu`, `src/codex.rs`) — ccu tail-reads the
> newest session JSONL directly (§2.5–2.6 mechanics) and renders 5h/7d bars + plan + snapshot
> age. AX explicitly chose NOT to widen clauth's scope ("upstream 只有 claude code 的 scope");
> clauth remains Claude-only and every CDX milestone below remains deferred. If CDX-2 ever
> lands daemon-side, switch ccu's codex source from direct JSONL reads to the daemon feed.

**Verdict (2026-07-12, amended by §0b): feasible, and mechanically easier than the Claude side
in most dimensions.** Hot-swap under a live session is structurally impossible *via `auth.json`
rewriting* — but §0b establishes the proxy route that achieves it anyway. The recommended shape
is a `harness: claude | codex` axis on Profile, a passive usage source (session JSONL), no
backend polling, and switch semantics tiered per §0b (session-boundary MVP → opt-in proxy).

Primary sources: openai/codex @ `9e552e9` (main HEAD, 2026-07-11) read directly; local
verification on codex-cli 0.144.1 (AX's machine, 2026-07-12). File paths below are
`codex-rs/…` paths at that SHA. §0b re-verified at openai/codex @ `cbc83d9` / codex-cli 0.144.4
(2026-07-16).

---

## 0b. CDX-0b: 2026-07-16 re-evaluation — in-session fallback IS achievable (proxy route)

Four parallel tracks (source re-read at HEAD `cbc83d9`, web/prior-art intel, local 0.144.4
binary+JSONL verification, clauth architecture fit). Net: **the "structurally impossible"
verdict was correct only for the mechanism §3.1 evaluated (auth.json swap). A localhost
header-rewriting proxy delivers true per-request in-session fallback, and it is the
established, actively-maintained community pattern.**

### What still can't work — and is now harder

- Zero auth-path commits `9e552e9..cbc83d9`; no native multi-account work at HEAD. All six
  upstream asks still open; the only candidate PR (Ducksss `--auth-profile`) is
  session-boundary and unmerged.
- File-swap is now **doubly** guarded: besides the guarded reload's permanent
  account-mismatch error (`manager.rs:2109-2142`), HEAD adds a header-attach-layer guard
  (`model-provider/src/auth.rs:131-151`) that refuses to attach auth headers when the cached
  account_id / chatgpt_user_id / workspace differs from the session's identity — "never cross
  an account boundary without rebuilding that state."
- 429 is terminal-for-the-turn: no retry (`retry_429: false`), no disk reload, and the NEXT
  turn still reads the cached snapshot — swap-then-next-turn does not work either.
- Local nuance: the real access token lives ~**10 days** (JWT exp verified locally), so the
  proactive-refresh reload window almost never opens; only the reactive-401 path ever re-reads
  disk, and that's exactly the guarded one.

### The proxy route (mechanism c) — viable, proven in the wild

Codex becomes a dumb HTTP client to `127.0.0.1`; clauth owns the outbound identity per request:

- **Routing**: either the documented `[model_providers.X]` block (`base_url`,
  `requires_openai_auth = false`, `wire_api = "responses"` — one community route; codex-lb
  runs `= true` and CDX-5 adopted that route, see proxy-design.md §1.3) or
  the `openai_base_url` / `chatgpt_base_url` overrides (both present in the 0.144.4 binary;
  `openai_base_url` overrides even ChatGPT-mode `https://chatgpt.com/backend-api/codex`).
- **Identity = exactly two coherent headers**: `Authorization: Bearer <access_token>` +
  `ChatGPT-Account-ID: <account_id>` (`bearer_auth_provider.rs:31-46`), both from the per-account
  blob clauth would hold. The backend accepts consistent rewrites — proven daily by
  ndycode/codex-multi-auth (391★, pushed 2026-07-14), VallierDev/codex-switcher (本地代理 +
  无损自动切号 + 限额时自动换号并重发), christiandoxa/prodex. On 429 the proxy rotates account
  and REPLAYS before first byte — exactly clauth's Claude fallback semantics.
- **No account pinning in the body**: `store=false` (client resends full context each turn);
  `session_id` is conversation-scoped, not account-scoped.
- **Free usage source**: the `x-codex-*` rate-limit headers / SSE `codex.rate_limits` events
  flow THROUGH the proxy — live usage per account with zero extra requests (no `wham/usage`).

### Client-integration verified with the REAL codex CLI (2026-07-16, isolated)

The stub-upstream e2e (`tests/inline/proxy.rs`) proves the proxy's own inject/rotate/replay;
it does NOT exercise the real `codex` binary. A separate isolated probe closes that half —
`scripts/codex-sim/proxy-client-integration.sh` (isolated `CODEX_HOME`, fake fresh tokens, a
local capture-stub in place of `clauth proxy`; the real `~/.codex` and chatgpt.com are never
touched for the model turn). Run 2026-07-16 on herdr (Ubuntu 24.04, **codex-cli 0.144.5**):

- ✅ With the exact `[model_providers.clauth]` block from `proxy --print-config`, codex routed
  **`GET /backend-api/codex/models`** + **`POST /backend-api/codex/responses`** through the
  loopback base, each carrying `Authorization: Bearer …` + `ChatGPT-Account-ID: …`
  (originator `codex_exec`). This is exactly the injection point the proxy owns.
- ✅ **Resolves open question #3** for the provider-block route: codex accepted a plain
  **`http://127.0.0.1`** base for the model provider — no HTTPS/local-cert needed on this path.
- ✅ Real `~/.codex/auth.json` sha256 identical before/after — a co-resident live codex session
  (the host's own account) is never disturbed; isolation holds.
- ⚠️ **Scope: the proxy is a MODEL-TURN interceptor, not total-traffic.** codex also fires a
  usage/`rmcp` preflight that bypasses the provider base and hits chatgpt.com directly (401s on
  the fake token here). That is by design — the 429s that drive fallback ride on `/responses`,
  which IS intercepted; clauth never wants the `wham/usage` leg anyway.
- ✅ **Server-side acceptance now PROVEN** — see the real-backend run below; this bullet's old
  "unproven / AX-manual" caveat is resolved for the happy path.

### Real-backend confirmed with the REAL `clauth proxy` (2026-07-16, isolated herdr)

The probe above used a stub upstream. This run used the **shipped `clauth proxy` binary**
against the **live** `chatgpt.com/backend-api/codex`, with a real paid account
(`ax-codex-cl`, Plus). Fully isolated on herdr: the binary was `cargo build`-ed from source
there, run under an **isolated `$HOME`** (pool = just `ax-codex-cl`, its `codex-auth.json`
scp'd in and deleted after), with an isolated-`CODEX_HOME` real `codex exec` pointed at the
proxy. The host's own live codex session (`~/.codex`, a different account) was never read or
written — sha256 identical before/after; that account was refresh-safe because a fresh token
never trips `standby_due` in a minutes-long window.

- ✅ **OpenAI HONORS the proxy-injected identity.** `codex exec` returned a real model answer
  (`PROXY_OK 42`, 15,856 tokens) — produced by the live backend after the proxy overwrote the
  client's headers with `ax-codex-cl`'s `Authorization` + `ChatGPT-Account-ID` and forwarded
  `POST /backend-api/codex/responses`. Proof it was the injected (not client) identity: the
  client's own fake token 401s on any leg that bypasses the proxy (the rmcp leg below), while
  the proxied `/responses` succeeded. This closes the core CDX-5 feasibility question.
- ⚠️ **NEW (stub missed this): the shipped proxy is POST-only and 405s `GET …/models`.** codex's
  model-metadata refresh (`GET /backend-api/codex/models`) got `405 clauth proxy: POST only`.
  codex 0.144.5 **degraded gracefully** (fell back to a default model; the conversation
  succeeded), but the stub answered that GET with 200 and hid the gap. **CDX-5 follow-up:** the
  proxy should forward `GET …/models` upstream (with injected identity) or return a canned list,
  so a stricter future codex that hard-requires the model list can't break. Not blocking today.
- ⚠️ Confirms the earlier probe's out-of-band note: codex's `rmcp`/usage preflight bypasses the
  provider base and hits chatgpt.com directly with the CLIENT token → 401, non-fatal. The model
  turn (which the proxy DOES own) carried the real identity and succeeded.

### Costs / open questions before building

1. **clauth has no async HTTP server stack** (tokio is `rt,io-std` only; ureq is blocking) —
   hand-rolled `TcpListener`+SSE passthrough vs adding hyper/axum is the dominant build cost.
2. **WebSocket transport**: 0.144.4 has a Responses-WebSocket path with HTTPS fallback — proxy
   the WS upgrade or force SSE via config. (Default transport at 0.144.x unverified.)
3. **http://127.0.0.1 acceptance** for the ChatGPT root is unverified statically — if https is
   enforced, a locally-trusted cert is needed.
4. **Refresh ownership**: the carrier `auth.json` must be a chain clauth EXCLUSIVELY holds
   (per-session `CODEX_HOME`, mechanism-b style) — codex's `refresh_token_reused` is a
   permanent kill. Keep the carrier fresh (same invariant as the Keychain lead) or answer
   `CODEX_REFRESH_TOKEN_URL_OVERRIDE`.
5. **UX cost of the provider-override route**: current codex builds filter local thread history
   by active `model_provider` — prior ChatGPT-mode sessions hide (not lost) while proxied.
6. **ToS posture**: same risk class as the Claude side, plus community-established guardrails —
   rotation interval ≥60 s (rapid rotation trips anti-abuse and invalidates tokens in
   sequence), stable IP, mimic native originator/endpoint (the proxy pattern already does).
   2026 ban-wave reporting centered on personal-plan OAuth tokens in third-party tools; risk is
   non-zero and stays AX's call.
7. Binary strings reveal a richer in-turn auth-recovery subsystem
   (`recovery_succeeded/failed_permanent/failed_transient`, `auth_recovery_*` telemetry) than
   the baseline knew — worth a source read, but on current evidence it only covers
   same-account recovery, not cross-account adoption.

### Revised mechanism ladder (supersedes §4's sizing for switch semantics)

| Tier | Mechanism | Seamlessness | Cost / risk |
|---|---|---|---|
| CDX-1 | session-boundary `auth.json` swap | next session | small — simpler than the Claude switch (no Keychain/symlink/LinkState/follow machinery; plain content compare); atomic 0600 write + round-trip unmodeled fields |
| CDX-1b | per-profile `CODEX_HOME` (`clauth start codex`) | isolation only | near-free (start.rs/runtime.rs already do this for Claude); zero rotation hazard; no shared chain |
| CDX-1c | swap + `codex resume --last` wrapper | semi-seamless (new process, same conversation) | trivial on top of CDX-1 — resume re-reads auth.json fresh at startup; `store=false` means the new account serves the resumed conversation |
| CDX-2 | passive JSONL usage → status feed | n/a | zstd dep; feeds existing UsageStore/windows shape (windows routed by their OWN `window_minutes` — ≤24h→5h slot, longer→7d slot; positional primary→5h only as a no-minutes fallback. OpenAI re-shaped the limiter 2026-07: primary became the 10080-min weekly window, 5h temporarily gone); no PollStreaks/kick (passive reads can't 429) |
| CDX-5 | **localhost injection proxy** | **true in-session, per-request** | HTTP server stack + WS/TLS questions above; proxy-down = codex-down; opt-in tier layered on CDX-1b (dedicated carrier) |

Scheduler note (sharpens §7): the 0.11.0 merge's fetch/kick/streak leg (StreakCounts,
KickBlocks, AuthClient wire parity) is Anthropic-endpoint-shaped — a codex profile BYPASSES the
OAuth fetch leg entirely and gets its own passive leg; only the cadence/queue framework is
shared. Fallback chains need a harness axis with chain homogeneity (all-claude or all-codex);
status.json gains an additive per-profile `harness` field (schema stays 1, ccsbar
decodeIfPresent).

---

## 1. Why it maps: structural isomorphism

| clauth concept | Codex counterpart | Verified |
|---|---|---|
| `~/.clauth/profiles/*/credentials.json` snapshot | `~/.codex/auth.json` — plaintext file is the DEFAULT store mode, 0600, pretty-printed | ✅ local + source |
| macOS Keychain hot-swap (`Claude Code-credentials`) | **none** — see §3.1 | ✅ source |
| 5h / 7d usage windows, `resets_at`, plan tier | `primary`/`secondary` with `used_percent`, `window_minutes`, `resets_at` (unix secs), `plan_type` — slotted by duration, not position (2026-07: primary IS the 10080-min weekly window, secondary null) | ✅ local JSONL |
| `clauth start` per-profile `CLAUDE_CONFIG_DIR` | `CODEX_HOME` env var (must pre-exist as a dir — canonicalized, hard error otherwise) | ✅ local test |
| Plan detection via `/api/oauth/profile` | id_token JWT claims carry email / `chatgpt_plan_type` / `chatgpt_account_id` — **no network call needed** | ✅ source |
| tokens.rs reading Claude Code stats cache | `sessions/YYYY/MM/DD/rollout-*.jsonl` `token_count` events: per-turn + cumulative input/cached_input/output/reasoning_output/total | ✅ local JSONL |
| CAP-1 identity anchors (`account_id.json`) | `tokens.account_id` lives IN the credential file; codex itself refuses to refresh across a changed account_id | ✅ source |
| auth_broken quarantine + browser re-login | same concept; failure codes are explicit (§2.3) | ✅ source |

Codex has **no native multi-account auth support** as of 0.144.1: `--profile <name>` /
`<name>.config.toml` is config-only (model/provider/permissions), auth.json is shared per
`CODEX_HOME`. Open asks: openai/codex issues #4432 (working fork, unmerged, stale), #22026,
#12029, #14330, #18806, discussion #25630. The tool gap is real.

---

## 2. Codex CLI auth internals (distilled)

### 2.1 Credential storage

- Location: `$CODEX_HOME/auth.json`; `CODEX_HOME` defaults to `~/.codex` (`utils/home-dir/src/lib.rs`).
- Store modes (`cli_auth_credentials_store` in config.toml; `config/src/types.rs`):
  `file` (**default**) · `keyring` · `auto` · `ephemeral`. Keyring backend `direct` puts the whole
  blob in the OS keyring under service **`"Codex Auth"`**, key `cli|<first-16-hex sha256(CODEX_HOME)>`.
  AX's machine: no override → file mode.
- Perms 0600 on Unix (`login/src/auth/storage.rs`).
- Schema (`AuthDotJson` + `TokenData`):

```jsonc
{
  "OPENAI_API_KEY": "sk-…",          // present after ChatGPT login too (token-exchange mints one)
  "auth_mode": "chatgpt",            // chatgpt | apikey | …
  "tokens": {
    "id_token": "<JWT>",             // claims: email, plan_type, account_id (§2.4)
    "access_token": "<JWT>",         // exp claim drives refresh timing
    "refresh_token": "<opaque>",     // SINGLE-USE, rotates (§2.3)
    "account_id": "<chatgpt_account_id>"
  },
  "last_refresh": "RFC3339",
  "agent_identity": {…},             // newer optional fields — MUST round-trip unmodeled fields
  "personal_access_token": "…",
  "bedrock_api_key": {…}
}
```

- Separate file `$CODEX_HOME/.credentials.json` is **MCP OAuth**, not CLI auth — don't confuse.

### 2.2 OAuth login flow

Authorization Code + PKCE (S256), localhost callback (`login/src/server.rs`):

- Client ID `app_EMoamEEZ73f0CkXaXp7hrann`; issuer `https://auth.openai.com`.
- Callback `http://localhost:1455/auth/callback` (fallback port 1457).
- Authorize: `GET /oauth/authorize`, scopes `openid profile email offline_access
  api.connectors.read api.connectors.invoke`, plus `id_token_add_organizations=true`,
  `codex_cli_simplified_flow=true`.
- Exchange: `POST /oauth/token` (`grant_type=authorization_code` + `code_verifier`) →
  `{id_token, access_token, refresh_token}`; then a second token-exchange
  (`grant_type=urn:ietf:params:oauth:grant-type:token-exchange`, subject = id_token) mints an
  **API key stored alongside** the OAuth tokens.
- Device flow exists (`codex login --device-auth`) but is OpenAI-custom
  (`/deviceauth/usercode` + `/deviceauth/token`), not RFC 8628.
- clauth analogue: `oauth_login.rs` is the template; CDX login is a parallel module, same shape.

### 2.3 Token refresh — the load-bearing part

- **Trigger** (`manager.rs` `should_refresh_proactively`, on every `auth()`): access_token JWT
  `exp` within **5 min** → refresh; if `exp` unparsable, fall back to `last_refresh` older than
  **8 days**. Reactive refresh on HTTP 401.
- **Call**: `POST https://auth.openai.com/oauth/token`, JSON
  `{client_id, grant_type:"refresh_token", refresh_token}`. Response fields all optional;
  each overwrites only when present; `last_refresh` always reset.
- **Rotation is real and server-enforced.** Permanent-failure codes: `refresh_token_expired`,
  **`refresh_token_reused`** (single-use reuse detection), `refresh_token_invalidated`.
  Two copies of one chain each refreshing once ⇒ the stale copy's next refresh kills the
  credential. This is Anthropic's rotation hazard, but harsher (explicit reuse detection).
- **Multi-process discipline is built into codex itself**: refresh serialized by a lock, and it
  guarded-reloads auth.json from disk first — if disk is fresher, skip network; if disk
  `account_id` differs from memory, **permanent account-mismatch error** (never silently adopts
  a different identity).
- Refresh-token TTL: server-controlled, no code constant. **[UNVERIFIED]**
- clauth mapping: adopt-first discipline transfers verbatim — live auth.json is source of truth
  for the ACTIVE profile; clauth only proactively refreshes chains it exclusively holds
  (inactive profiles). Bonus laxity vs Anthropic: passive usage monitoring needs NO token at
  all (§2.5), so refresh only matters at switch time + a periodic standby keep-alive.

### 2.4 Account identity

id_token JWT claims (`login/src/token_data.rs`): top-level `email` (or
`https://api.openai.com/profile` → email); `https://api.openai.com/auth` →
`chatgpt_plan_type` (free|plus|pro|business|enterprise|edu), `chatgpt_user_id`,
`chatgpt_account_id`, `chatgpt_account_is_fedramp`. `tokens.account_id` is set at login from
`chatgpt_account_id`. Plan + email + identity anchor all come from parsing the stored JWT —
zero network.

### 2.5 Usage / rate limits

- **The CLI never polls a usage endpoint.** Rate-limit data rides on real Responses API calls to
  the ChatGPT-mode base `https://chatgpt.com/backend-api/codex` (corrected 2026-07-16 — an
  earlier draft wrote `backend-api/responses`, which is not the ChatGPT-mode path; see §0b), via
  (a) `x-codex-primary-*` / `x-codex-secondary-*`
  response headers, or (b) SSE event `codex.rate_limits` (`codex-api/src/rate_limits.rs`).
- Both parse into `RateLimitSnapshot{primary, secondary, credits, plan_type,
  rate_limit_reached_type}`; window = `{used_percent, window_minutes, resets_at}` (unix secs;
  older releases used resets-in-seconds — tolerate both).
- **Every `token_count` JSONL event embeds the full snapshot** (§2.6) → the passive source.
- Out-of-band endpoints `GET chatgpt.com/backend-api/wham/usage` and `/backend-api/accounts`
  exist and are used by third-party tools — originally classified **the ToS-detection-risk
  path; do not use** (following loongphy/codex-auth's own README warning).
  **REVERSED for `wham/usage` only (2026-07-22, AX, CDX-6)**: re-investigation found codex
  CLI itself polls that endpoint ~every 60s (openai/codex#10869) and three sibling projects
  (steipete/CodexBar, mryll/codexbar, MacSteini/Codex-Usage) ship it with no reported
  incidents — the risk re-classified as "private API may change without notice", not
  detection. `codex::poll` now polls it read-only per profile at codex's own cadence
  (stored access token, never a refresh; CDX-3 remains the sole renewer; kill switch
  `codex_usage_poll = false`). `/backend-api/accounts` and the credit endpoints stay banned.
- Local freshness measurement (2026-07-12): newest session's rate_limits snapshot was 14 s old
  while codex was active. Idle accounts go stale, but idle accounts aren't burning — staleness
  only over-estimates `used_percent` (conservative for headroom walks), and `resets_at < now`
  lets clauth infer a window reset without any call.

### 2.6 Sessions JSONL (tokens dashboard source)

- Layout: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<conversation_id>.jsonl`, possibly
  `.jsonl.zst` (transparent zstd compression — must handle).
- Line: `{timestamp, ordinal?, type, payload}`; types: `session_meta, response_item,
  turn_context, event_msg, compacted, …`. Line 1 `session_meta` carries session id, cwd,
  originator, cli_version, model_provider.
- Token record: `event_msg` / `payload.type == "token_count"`:
  `info.total_token_usage` + `info.last_token_usage`
  (`input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens, total_tokens`)
  + `rate_limits` snapshot + `model_context_window`.
- Schema drifted 2025→2026 (flat records → tagged RolloutItem; resets_in → resets_at). Decode
  leniently, tolerate missing `ordinal`, old flat files, and zst.

---

## 3. The three design-shaping differences vs Claude

### 3.1 Hot-swap via auth.json is impossible → but see §0b for the proxy route that supersedes this section's "no live rescue" conclusion

Claude Code re-reads the Keychain item on every request (the fork's signature move). Codex
caches auth in memory (`AuthManager` RwLock); a mid-session auth.json swap is invisible until
the next refresh boundary, and the guarded reload then **errors permanently on account_id
mismatch** rather than adopting. So CDX auto-switch semantics = "daemon rewrites auth.json when
the active account exhausts; the NEXT codex session lands on the new account." Still valuable
for headless/cron codex runs; live sessions cannot be rescued. (ndycode/codex-multi-auth's
loopback rotation-proxy achieves per-request injection but is out of scope — complexity + risk.)

### 3.2 Usage is passive-only

No sanctioned pollable endpoint (§2.5). Source = newest session JSONL per account +
`resets_at` decay inference. An auto-start-kick analogue (real `/responses` call) would burn
usage exactly like the Claude kick — possible, but leave out of the MVP.

### 3.3 Rotation is harsher, but the discipline already exists

Single-use refresh tokens + server-side reuse detection (§2.3). clauth's existing rotation
coherence / adopt / auth_broken machinery is the exact answer; the CDX variant is *simpler*
because standby monitoring needs no live token.

---

## 4. Proposed staged architecture (when un-deferred)

Add `harness: claude | codex` to Profile; per-harness credential schema, switch mechanics,
refresh, usage source. Fallback chains stay per-harness (never cross-switch a codex account for
a claude one).

- **CDX-1 capture + switch**: auth.json snapshot per profile; switch = atomic whole-file swap
  (**round-trip unmodeled fields** — `agent_identity`/PAT/bedrock arrived recently, more will
  come); warn when live codex processes exist (pgrep); `clauth start` analogue via per-profile
  `CODEX_HOME` (dir must pre-exist).
- **CDX-2 usage display**: passive JSONL parsing → 5h/7d bars, plan, email in TUI;
  status.json schema gains a harness field (ccsbar follows). *Display half already live in
  ccu via direct JSONL reads (see status note above) — landing this daemon-side means
  migrating ccu's codex source to the feed, not adding a second renderer.*
- **CDX-3 refresh + quarantine**: parallel oauth module (endpoints/client-id in §2.2–2.3);
  refresh on switch + periodic standby keep-alive; `refresh_token_reused/expired/invalidated`
  → auth_broken + browser re-login (PKCE flow per §2.2, `oauth_login.rs` as template).
- **CDX-4 fallback chain + tokens feed**: codex chain with session-boundary semantics; JSONL
  usage into tokens.json with a source dimension (ccsbar/ccu follow).

**Explicit non-goals**: live-session hot-swap; ~~`wham/usage` polling~~ (adopted 2026-07-22
as CDX-6 — see the reversal note above); keyring store mode
(detect `cli_auth_credentials_store = "keyring"` in config.toml and refuse with a clear
message); auto-start kick.

Sizing: Tier-4, four milestones across clauth + ccsbar; CDX-1+2 deliver most of the value.

---

## 5. Risks

- **Upstream fork surface**: fork-only feature; upstream (uwuclxdy/clauth) is Claude-only. The
  fork is already deeply diverged (Keychain, daemon, ccsbar), so marginal merge cost, not zero.
- **Native support may land** (#4432 has a working fork) — long-stale; don't bet on it, but
  re-check before starting CDX-1.
- **Schema drift**: codex iterates fast (auth.json fields, rollout schema, store modes).
  Lenient decode + whole-file round-trip is the standing rule.
- **ToS posture**: swapping one's own auth.json ≈ what `codex login` itself does; passive local
  reads add zero requests. The risky path is backend polling — excluded by design.

## 6. Prior art

| Tool | Approach | Lesson |
|---|---|---|
| Ducksss/codex-profiles | per-account `CODEX_HOME` isolation | cleanest, zero rotation hazard — CDX-1's `start` analogue |
| enerai/codex-auth-snap | offline auth.json snapshot swap, zero network | the safe switch baseline |
| loongphy/codex-auth | swap + live TUI, but polls `wham/usage` by default | its own README warns of detection/ToS risk — the path to avoid |
| ndycode/codex-multi-auth | separate pool + loopback rotation proxy | per-request injection is possible but heavy; treats `refresh_token_reused` as re-login-required |
| Lampese/codex-switcher | desktop app, auto warm-up each 5h reset | kick-equivalent exists in the wild; still out of MVP |

## 7. clauth coupling map (what a harness axis touches)

Claude-specific today: `profile.rs` (`ClaudeCredentials`/`OAuthToken` shape), `claude.rs` +
`keychain.rs` (switch mechanics, LinkState vs `~/.claude`), `oauth.rs`/`oauth_login.rs`
(Anthropic endpoints, client id, kick, CC system prompt), `usage/fetch.rs`
(`api.anthropic.com/api/oauth/{usage,profile}`), `tokens.rs` (Claude Code stats cache),
`start.rs` (`CLAUDE_CONFIG_DIR`), MCP plugin (Claude-side). Harness-agnostic already:
fallback-chain engine, daemon scaffolding (tick/status/socket), TUI framework, profile store,
lock/rotation-guard infrastructure, `providers/` (third-party API-key stats — orthogonal axis).

---

*Research: session 2026-07-12 (post-TOK-6). Codex source read at openai/codex `9e552e9`;
key files: `login/src/auth/{storage,manager}.rs`, `login/src/{token_data,server,pkce,device_code_auth}.rs`,
`config/src/{types,config_toml}.rs`, `codex-api/src/rate_limits.rs`, `protocol/src/protocol.rs`,
`rollout/src/{lib,recorder,compression}.rs`, `docs/authentication.md`. External: issues #4432
#22026 #12029 #14330 #18806, discussion #25630; learn.chatgpt.com/docs/auth.*
