# CDX: Codex account switching — implementation plan

**Status: CDX-1/1c/2 shipped 2026-07-16 (commits 6c19f29..f4198f2); CDX-3/1b/4/5 in build
(same date, second wave). Feasibility basis: [feasibility.md](feasibility.md) §0b; CDX-5
design: [proxy-design.md](proxy-design.md). Wire facts re-verified at openai/codex
`9ff47868` / codex-cli 0.144.5 (2026-07-16 research pass).**
Fork-side feature (upstream uwuclxdy/clauth#45: maintainer says out-of-scope for him, not
against it; design posted to that issue 2026-07-16).

Milestone ladder (build order = value order; each phase ships independently):

| Phase | Delivers | Session |
|---|---|---|
| CDX-1 | harness axis, codex profile store, capture, manual switch, follow/adopt-back, doctor, status.json | ✅ shipped |
| CDX-1c | `clauth resume` convenience (switch + `codex resume --last`) | ✅ shipped |
| CDX-2 | passive JSONL usage → TUI bars, status windows, plan/email identity | ✅ shipped |
| CDX-3 | OAuth standby refresh + auth_broken + browser PKCE login | ✅ shipped (wave 2) |
| CDX-1b | `clauth start <codex-profile>` via per-profile `CODEX_HOME` | ✅ shipped (wave 2) |
| CDX-4 | codex fallback chain (auto-switch at session boundary) | ✅ shipped (wave 2) |
| CDX-5 | localhost injection proxy — true in-session per-request fallback (opt-in) | ✅ shipped (wave 2) |
| INT-1 | ccu migrates its codex source from direct JSONL to the daemon feed | wave 2 |
| INT-2 | ccsbar consumes the codex status.json fields | wave 2 |

---

## 0. Volatile decisions (most likely to be revisited — read first)

### 0.1 Two independent active slots (THE structural decision)

`AppState` gains `active_codex_profile: Option<ProfileName>` (serde default, additive).
`active_profile` keeps its exact current meaning (claude). The two live credential stores
(`~/.claude/.credentials.json` + Keychain vs `~/.codex/auth.json`) are separate realities;
one shared slot would make a codex switch unlink the claude login. All existing
`is_active`/switch/follow code paths remain claude-only and untouched; codex gets parallel
paths gated on `harness`.

**Load-bearing invariant (regression-tested, T1):** a codex profile is auto-excluded from
both scheduler fetch legs today — `collect_tokens` gates on `claude_ai_oauth.is_some()`,
`collect_third_party_entries` on `api_key.is_some()`, and a codex profile has neither. That
exclusion is what makes CDX-1 shippable before CDX-2's passive leg (no Anthropic fetch /
refresh / kick / auth_broken churn against a codex profile). Encode it as a test so a
future widening of either predicate can't silently re-include codex profiles.

### 0.2 `harness` is an explicit persisted field

```rust
#[derive(..., Default)]
pub(crate) enum Harness { #[default] Claude, Codex }
```

- `ProfileConfig.harness: Harness` with `#[serde(default)]` — absent = Claude, zero migration.
- `Profile.harness` mirrors it in memory. Immutable after profile creation (no cross-harness
  conversion; delete + recreate).
- Rejected alternative: deriving kind from stored-file presence (breaks for blank profiles,
  invisible in config.toml, misleads exploration).

### 0.3 Codex credentials: whole-file raw round-trip

- Store: `~/.clauth/profiles/<name>/codex-auth.json` — byte-for-byte snapshot of a codex
  `auth.json`, atomic 0600 write. NEVER reserialized through a typed struct: codex adds
  fields fast (`agent_identity`, `personal_access_token`, `bedrock_api_key` arrived
  recently); dropping unmodeled fields corrupts logins.
- Read access through a lens: `CodexAuthFile` wraps `serde_json::Value` + typed getters
  (`account_id()`, `access_token_exp()`, `email()`, `plan()` — all from stored JWTs, zero
  network). JWT parse = split on `.`, base64url-decode payload, lenient serde_json; no
  signature verification (we trust files we wrote).
- Identity anchor = `tokens.account_id` (codex itself refuses cross-account refresh on this
  key, so it is authoritative). Anchor cached like claude's `account_id.json` for listing
  without re-parse: reuse `profile_cache` slot pattern.

### 0.4 No symlink, no keychain — content compare + adopt-back only

Live file: `~/.codex/auth.json`. Codex rewrites it in place on refresh (atomic replace), so
the claude-side symlink trick is useless and unnecessary:

- **Which profile is live** = live `tokens.account_id` vs each codex profile's anchor.
- **Follow (daemon tick)**: if live account_id == active codex profile's anchor AND bytes
  differ → copy live → store (codex refreshed its chain; adopt-back keeps the snapshot
  fresh). One direction only; the daemon NEVER writes the live file outside a switch.
  Cheap gate: stat mtime, skip when unchanged.
- **No rotation hazard in CDX-1**: clauth never refreshes any codex chain (no CDX-3 yet).
  Each chain has exactly one consumer (codex itself, on whichever copy is live).
  `refresh_token_reused` cannot fire from clauth's behavior.

### 0.5 Switch semantics (session-boundary, loss-free)

`codex_switch_profile(config, target)` under `with_state_lock`:

1. Read live auth.json (missing → nothing to preserve; unparseable → archive raw bytes to
   `~/.clauth/quarantine/<ts>-<seq>-codex-live.auth.json` and proceed).
2. Classify live owner by account_id:
   - matches a stored codex profile → adopt-back into that profile if bytes differ (loss-free
     capture of any refresh), proceed;
   - matches no stored profile (foreign) → surface divergence: CLI prompts capture/discard;
     TUI divergence flow; socket `Origin::User` archives to quarantine + proceeds (RESCUE-2
     semantics); `Origin::Scheduler` defers (CDX-4 concern, encoded now).
3. Atomic 0600 write: target's `codex-auth.json` bytes → `~/.codex/auth.json`.
4. `active_codex_profile = target`, `last_switch` stamped with harness noted in trigger.

Live-session caveat (documented in every surface): a running codex keeps its in-memory
account until its next refresh boundary, then dies with account-mismatch (it does NOT
clobber the swapped file — verified in feasibility §2.3). Switch takes effect for NEW
sessions. `pgrep -f codex` → warning line, never a refusal.

### 0.6 Keyring/ephemeral store modes are refused

If `~/.codex/config.toml` sets `cli_auth_credentials_store` ∈ {keyring, auto, ephemeral},
capture/switch fail with a clear message (file mode only in CDX-1) and doctor reports it.
Lenient TOML read: missing file / missing key = file mode (the default).

### 0.7 Chains are per-harness

`AppState.codex_fallback_chain: Vec<ProfileName>` (additive, default empty). Claude chain
validation additionally rejects codex profiles and vice versa. CDX-1 ships the field +
validation only (task T1b — an explicit task, not a side effect); the walk ships in CDX-4.

## 0.8 Open unknowns / assumptions (risk × irreversibility order)

1. **refresh-token server TTL unknown** — a codex profile parked inactive for weeks may die
   silently. Mitigation now: status shows snapshot age; doctor warns > 7 d. Real fix: CDX-3
   standby refresh. (Wrong guess costs a manual re-login, not data.)
2. **`sessions/` JSONL attribution** (CDX-2): usage snapshots belong to whoever was live at
   event time. We attribute a snapshot to a profile only when the session's active account
   is provably that profile (active at read time + account unchanged since event, else
   last-known + `resets_at < now` ⇒ assume reset). Conservative staleness over
   misattribution.
3. **codex schema drift** — auth.json fields, rollout JSONL, store modes all iterate fast.
   Standing rule: lenient decode, raw round-trip, tolerate unknown fields/types. Verified
   reference re-pinned at wave 2: openai/codex @ `9ff47868` / codex-cli 0.144.5 (wave 1
   verified at `cbc83d9` / 0.144.4 — no auth-shape drift between the two).
4. **CDX-5 HTTP stack choice** (hyper vs hand-rolled) deliberately NOT decided here —
   deferred to its own design round with a fresh source-read of codex transport defaults
   (WS vs SSE). Nothing in CDX-1/2 constrains it: the proxy layers on CDX-1b's dedicated
   carrier design.

---

## CDX-1 — harness axis + capture + switch + follow (tasks)

Each task lands TDD (failing test → impl) with tests in the matching `tests/inline/*` module.
Fixtures: HomeSandbox only; fake tokens (`at-alpha` style); fake unsigned JWTs built by a
test helper. **Never real `~/.codex`.**

- [x] **T1 `Harness` enum + data model.** `profile.rs`: `Harness` (Default=Claude, serde
  lowercase), `ProfileConfig.harness` (serde default), `Profile.harness`,
  `AppState.active_codex_profile` + `codex_fallback_chain` (defaults). `render_config_toml`
  emits `harness = "codex"` only when non-default (old files stay byte-identical).
  `AppConfig::is_active` → claude-only semantics kept; new `is_active_codex(name)`.
  Regression test for the fetch-leg exclusion invariant (§0.1).
- [x] **T1b cross-harness chain rejection.** `fallback_config::add` (and the TUI fallback
  editor path) rejects adding a codex profile to the claude chain and vice versa, with an
  actionable error; chain snapshot validation tolerates a stray cross-harness member in an
  existing file by skipping it with a logged warning (never a panic, never a switch
  target). Unit tests both directions. Without this, `scan_auto_switch` could hand the
  claude `switch_profile` path a profile with no claude credentials.
- [x] **T2 `src/codex/` module.** `mod.rs`: `codex_dir()` (`home/.codex`, sandbox-aware via
  `profile::home_dir()`), live path, `read_live()`, `store_path(name)`, atomic 0600
  write/copy helpers, `store_mode()` (config.toml sniff, §0.6), quarantine archive (reuse
  claude.rs pattern incl. seq counter). `auth.rs`: `CodexAuthFile` lens + JWT payload
  decode (base64url — decode helper next to oauth_login's encode), getters
  `account_id/email/plan/access_token_exp/last_refresh`; `fingerprint()` (SipHash of
  access_token, None when blank/absent).
- [x] **T3 capture.** `actions.rs`: `codex_capture_into_profile(config, name)` — read live,
  refuse store-mode ≠ file, refuse blank/missing live with actionable message ("run `codex
  login` first"), validate name, refuse capturing an account another codex profile anchors
  (CAP-3 analogue), write snapshot + anchor cache, create-or-reauth semantics mirroring
  `capture_into_profile`/`overwrite_captured_profile`. First codex profile auto-becomes
  `active_codex_profile` when live matches it.
- [x] **T4 switch.** `actions.rs`: `codex_switch_profile` + `codex_switch_profile_discard`
  per §0.5; `main.rs cmd_switch` dispatches on target harness (`clauth <name>` stays THE
  switch verb); CLI divergence prompt mirrors `switch_profile_cli`'s `[Y/n]` reconcile;
  pgrep warning line.
- [x] **T5 CLI login surface.** `clauth login <name> --codex` → capture path (T3). `clauth
  login` without `--codex` on an existing codex profile → clear error (harness immutable).
  `clauth delete` works unchanged (dir removal covers codex-auth.json).
- [x] **T6 daemon follow + socket + MCP.** tick gains `codex_follow_live` step (§0.4, after
  claude follow), running **under `with_state_lock`** exactly like the claude follow — a
  socket-origin switch (store→live) and an adopt-back tick (live→store) on the same
  profile must serialize; inline test interleaves the two. Socket `switch` cmd +
  pending-switch drain dispatch by harness (User-origin divergence → archive+proceed;
  Scheduler-origin → defer), so ccsbar can switch codex profiles day one. MCP `switch`
  dispatches by harness through the same noninteractive path (User-origin semantics);
  `list_profiles` includes codex profiles with a harness field.
- [x] **T7 status.json.** Additive: per-profile `harness` ("claude"/"codex"), top-level
  `active_codex_profile`. Per-profile `active` stays claude-truth for claude profiles;
  codex profiles report `active` = codex-slot truth (readers see one coherent boolean per
  profile). **Pinned contract:** per-profile `codex_snapshot_at` (ISO 8601, present only on
  codex profiles) = when the stored snapshot was last captured/adopted; ccsbar
  decodeIfPresent. Mirror into DESIGN.md in the same commit.
- [x] **T8 TUI.** Accounts tab: codex profiles render with harness tag + email/plan from
  lens; Enter → codex switch confirm path; login/delete-creds rows route to capture/clear.
  Setup `+ new` gains a codex choice only if cheap — else CLI-only creation in CDX-1 (log
  as autonomous decision).
- [x] **T9 doctor.** `check_codex`: live auth.json presence/parse/store-mode, active codex
  profile linkage (anchor match), snapshot staleness warn > 7 d. WARN-level only (codex
  absence must not fail claude-only machines).
- [x] **T10 docs.** README fork-features section + DESIGN.md (ccsbar contract) + this plan
  updated in the same commits that ship behavior (in-flight knowledge sync).

**Deviations (logged, autonomous decisions):** no separate `account_id.json`-style
anchor cache for codex profiles — the stored `codex-auth.json` IS the anchor (parsed
on demand; <5 KB, switch/status-time only), so a cache file would be a second source
of truth. TUI creation of a NEW codex profile stays CLI-only in CDX-1 (`clauth login
<name> --codex`); the TUI covers switch / re-capture / logout / display. The codex
follow memo is NOT persisted across restarts (unlike claude's FollowState): it guards
log spam only — codex follow does no network and burns nothing.

**Acceptance (CDX-1):** `cargo test` green incl. new inline suites; clippy clean; fmt clean;
sandboxed end-to-end test: two fake codex accounts captured → switch → live file holds
target bytes exactly → refresh simulation (mutate live) → tick adopt-back updates store →
status.json shows harness fields. Manual acceptance (AX, unattended-forbidden): real
capture + switch on the live machine.

> **Sandboxed e2e is scripted** (ran 8/8 on 2026-07-16): `scripts/codex-sim/run.sh`
> boots a second daemon under an isolated `$HOME` with two fake accounts
> (`make_auth.py` — unsigned JWTs, fresh `last_refresh` + far-future `exp` so
> CDX-3 standby never fires a network refresh), proves a user switch swaps the
> live bytes verbatim, then forges a weekly-only (2026-07 shape) rate-limited
> session JSONL and watches the passive tick → chain scan → drain perform a
> real auto-switch — real `~/.codex` hash-verified untouched. What the sandbox
> CANNOT prove (needs a real second account, AX-manual): OpenAI accepting the
> swapped token on a fresh codex session, the real 429's JSONL shape, and
> server-side refresh-reuse behavior.

---

# Wave 2 — CDX-3 / CDX-1b / CDX-4 / CDX-5 / INT (2026-07-16)

## 0w. Wave-2 volatile decisions (most likely revisited — read first)

### 0.9 CDX-3 refresh exclusivity (THE wave-2 decision)

clauth refreshes a codex chain ONLY when it exclusively holds it. A codex refresh token
is single-use with server-side reuse detection (`refresh_token_reused` is a permanent
kill), and every mature prior-art tool converged on the same rule: **single writer per
account chain, machine-wide.** Exclusivity predicate (`codex_standby_candidates`):

- **skip the live owner** — the profile whose anchor matches the live `~/.codex/auth.json`
  `account_id`. codex itself carries that chain (proactive refresh at `exp ≤ now+5min`,
  reactive on 401 — verified at HEAD `9ff47868`, manager.rs); the daemon follow's
  adopt-back keeps our snapshot fresh.
- **skip profiles with a live isolated codex session** (CDX-1b lease) — the isolated
  `CODEX_HOME` carries that chain; the session watchdog adopts it back.
- **skip `auth_broken`** profiles (dead chain; `clauth login … --codex` is the fix).

Cross-process enforcement: the per-profile `RotationGuard` (`rotation.lock` flock —
profile names are unique across harnesses, so the claude lock file is reused) is held
across the full HTTP window, and the exclusivity predicate re-checks **inside the guard**
before the token is spent. `codex_switch_profile`/`codex_capture_into_profile` take a
**non-blocking try-probe** on the same lock first (they may already hold the state flock,
so a blocking acquire would invert the Rotation-outermost rank; a try-lock never blocks →
deadlock-free): busy → CLI/TUI error "standby refresh in flight — retry", scheduler drain
→ `fail_switch` backoff. CDX-1b's acquire holds the guard across its session-stamp window
(same as claude's `ProfileRuntime::acquire`).

### 0.10 CDX-3 wire shape (verified at HEAD `9ff47868`)

`POST https://auth.openai.com/oauth/token`, `Content-Type: application/json`, body
`{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_token":…}`.
Response `{id_token?, access_token?, refresh_token?}` — **all optional**. Apply mirrors
codex's own `persist_tokens`: overwrite each of `tokens.{id_token,access_token,refresh_token}`
**only when present**, always reset `last_refresh` to RFC3339 now; `account_id` and every
other field untouched. The mutation is surgical on the stored `serde_json::Value`
(preserve_order is on, so key order survives; whitespace may differ from codex's writer —
cosmetic, codex re-parses leniently). NEVER through a typed struct (§0.3 stands).
Note the encoding split: **refresh is JSON; the login code-exchange is form-urlencoded.**

### 0.11 CDX-3 failure taxonomy

Permanent ⇔ HTTP 401, OR response body error code (JSON `error.code` or top-level `code`,
lowercased) ∈ {`refresh_token_expired`, `refresh_token_reused`, `refresh_token_invalidated`}.
The body-code half is exactly codex's own `classify_refresh_token_failure`; the 401-alone
rule matches codex's HEAD decision (`status == 401 → Permanent`) AND clauth's claude-side
policy — noted explicitly because it is a status-only quarantine against an endpoint whose
401 semantics we take from the reference client, not from independent probing. Permanent →
`set_auth_broken(name,
true)` (the existing harness-agnostic AppState list; codex switch already refuses broken
targets, the CDX-4 walk already skips them, doctor/status already surface them) + one
logline. Everything else (network, 429, 5xx, unconfirmed 4xx) is transient: keep cached
state, retry next cadence, never quarantine. A successful CDX-3 PKCE re-login clears the
flag (mirrors the claude heal path).

### 0.12 CDX-3 cadence — keep-alive, not hot rotation

Access tokens live ~10 days; the only real deadline is the unknown server-side refresh
token TTL (§0.8.1). Standby scan rides the scheduler lease-holder tick (after the passive
leg): a parked profile is due when its stored access-token `exp` is within **48 h** OR its
`last_refresh` is absent/older than **7 d** (codex's own fallback threshold is 8 d — we
stay inside it). Transient failure → widen that profile's next attempt by 6 h
(`CODEX_STANDBY_RETRY_MS`, in-memory). Expected steady state: ~one refresh per parked
profile per week.

### 0.13 CDX-3 PKCE browser login — store-only, never live

`clauth login <name> --codex --browser` mints a codex login **directly into the profile
store** without ever touching `~/.codex/auth.json` (the live login and the active marker
stay put — the differentiator vs capture). Flow verified at HEAD: authorize at
`https://auth.openai.com/oauth/authorize` with `response_type=code`,
`client_id=app_EMoamEEZ73f0CkXaXp7hrann`, `redirect_uri=http://localhost:{1455|1457}/auth/callback`
(fixed registered ports — bind 1455, fall back 1457), scopes
`openid profile email offline_access api.connectors.read api.connectors.invoke`,
`code_challenge` S256, `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`,
`state`. Both ports occupied (a real `codex login` in flight, or a squatter) → fail with an
actionable error naming the likely holder — never a silent hang; the `state` check in the
shared loopback module stays the fatal CSRF stop it is on the claude path. Exchange:
`POST /oauth/token` **form-urlencoded**
(`grant_type=authorization_code&code&redirect_uri&client_id&code_verifier`) →
`{id_token, access_token, refresh_token}` (all required). Then the optional API-key mint
(token-exchange grant, `requested_token=openai-api-key`, subject = id_token) with codex's
own `.ok()` semantics — a failure never fails the login. Constructed snapshot:
`{"auth_mode":"chatgpt", "OPENAI_API_KEY": <key|absent>, "tokens":{id_token, access_token,
refresh_token, account_id:<chatgpt_account_id claim>}, "last_refresh":<now>}` —
`auth_mode` is explicit because codex's `resolved_mode()` infers **ApiKey** mode from a
bare `OPENAI_API_KEY` when `auth_mode` is absent. CAP-3 dedup applies; success clears
`auth_broken`; `active_codex_profile` is NOT flipped. Loopback/PKCE scaffolding is
extracted from `oauth_login.rs` into a shared `src/loopback.rs` (claude login behavior
byte-identical — its tests pin the extraction).

### 0.14 CDX-1b isolated start — reuse the claude session-lease machinery

`clauth start <codex-profile> [codex args…]` dispatches by harness. Codex runtime tree:
`~/.clauth/profiles/<name>/codex-home/` — `auth.json` seeded from the store at acquire
(only when no live sibling session; siblings share the tree exactly like claude's shared
runtime), `config.toml` **copied** (NOT symlinked) from `~/.codex/config.toml` when present
— codex persists its own config edits (project trust decisions, `/model`), and an in-place
write through a symlink would mutate the operator's real config from inside an "isolated"
session; a copy shares settings at launch and keeps session-local changes isolated (review
finding, 2026-07-16). Acquire re-checks `store_mode()` and refuses non-`file` modes — a
config that flipped to `keyring` after capture would make codex ignore the seeded
`auth.json` entirely (§0.6's capture-time refusal doesn't cover post-capture flips).
`sessions/` left for codex to create (history isolated). `CODEX_HOME` env set to the
canonicalized dir (codex hard-errors on a missing dir — create first), inherited
`CODEX_HOME` scrubbed. Lease = `codex-sessions/<pid>-<seq>` flock files — the exact claude
pattern (`prune_stale_sessions`/gc reuse) in a harness-suffixed dir so flavors never
collide. `has_live_codex_session(name)` is the new predicate (shipped alongside CDX-3,
vacuous-false until this phase creates leases).

**Two-carrier refusals (the reason 1b was deferred until now):**
- start refuses when `name` is the live owner (its chain already runs in the shared home);
- switch/capture refuse a target with a live codex lease (its chain lives in the isolated
  home; installing the store snapshot would fork it) — scheduler drain defers via the
  shared backoff, user surfaces get an actionable error;
- CDX-3 standby refresh skips leased profiles (§0.9); CDX-4's walk skips them too.

Adopt-back: a watchdog thread in the start process copies `codex-home/auth.json` → store
whenever bytes differ (60 s cadence), plus a final sync on Drop — the same ownership story
as claude's session watchdog. The daemon never reads isolated homes (single owner: the
session process).

### 0.15 CDX-4 chain semantics — reuse the store-side walk, drop the gates that don't map

Chain state: `AppState.codex_fallback_chain` (shipped in T1). Homogeneity is already
enforced both directions (T1b). `fallback_config::{add,remove,move_member,set_threshold,
set_last_resort}` route by the profile's harness to the matching chain; validation stays
shared. Scan (`scan_codex_auto_switch`, lease-holder tick, after the passive leg):

- **Active exhausted** = `is_exhausted_from_store` semantics (7d weekly block, or 5h window
  live per `resets_at` AND `utilization ≥ threshold`), OR the snapshot's
  `rate_limit_reached_type` names a window whose `resets_at` hasn't passed (the limiter's
  own verdict — stronger than the percent heuristic). No `decision_fresh` gate: passive
  data self-invalidates via `resets_at`, and `used_percent` is monotone within a window,
  so a stale read can only under-report — the conservative direction.
- **Walk** = first non-active member that is not broken, not leased (§0.14), holds a
  stored login, and is not exhausted-from-store (lapsed `resets_at` ⇒ reset ⇒ viable; no
  data ⇒ viable, same as claude); else a `last_resort` member (same one-migration rule);
  else nothing. **No `SwitchAction::Off` analogue** — switching the codex slot off would
  mean logging the live file out, which serves nothing (an exhausted codex account just
  errors; there is no metered background poll to halt).
- Enqueue `Origin::Scheduler` on the `pending_switch` queue — `drain_codex_switch`
  (shipped in T6) already applies Refuse-foreign + backoff + LastSwitch stamping.
  **Per-harness queue independence (review finding, 2026-07-16):** the queue's gates are
  harness-scoped — `enqueue_pending_switch`'s no-op-on-non-empty, both scans'
  skip-while-pending, and the drain's single-winner selection each consider only entries
  whose TARGET is their own harness, and one drain round may service one winner per
  harness. Without this, a stuck claude switch (re-queued up to its retry TTL) would block
  a codex rotation for the whole window — a bounded delay, but a direct contradiction of
  §0.1's two-independent-slots invariant. C3 tests the interleaving both ways.
- Post-switch attribution self-heals: the install rewrites live `auth.json`, so pre-switch
  JSONL events fail the mtime gate and the new active starts clean.

### 0.16 CDX-4 signal parity: `rate_limit_reached_type` becomes a published field

The CDX-2 parser gains `rate_limit_reached_type` (ccu's own reader has it; ours dropped
it). **Carrier (review finding, 2026-07-16 — without naming it the field could be parsed
and dropped):** `UsageInfo` itself gains an additive
`codex_rate_limit_reached: Option<String>` (`skip_serializing_if`, claude paths never set
it) — `UsageInfo` is clauth's own struct and already flows through the usage cache →
`UsageStore` → status hydrate → status.json, so one field placement feeds the §0.15 scan,
the status serializer, and the standdown TUI with zero new plumbing and one source of
truth. status.json publishes it as additive per-profile `codex_rate_limit_reached`
(string, codex-only, present while the limiter verdict's window hasn't reset). DESIGN.md
updated in the same commit. This is both the CDX-4 exhaustion input and the field INT-1
needs so ccu's RATE-LIMITED badge survives the migration with the same signal, not a
percent heuristic.

### 0.17 Deploy/order

CDX-3 → CDX-1b → CDX-4 land as one clauth deploy train (each with its own commits/tests);
CDX-5 follows on its own design doc + deploy; INT-1 (ccu) and INT-2 (ccsbar) after the
daemon ships the fields they consume.

---

## CDX-3 — standby refresh + PKCE login (tasks)

- [x] **R1 `src/codex/oauth.rs`.** `refresh_result` sibling for codex (§0.10 wire, §0.11
  taxonomy — pure `classify_refresh_failure(status, body)` with a pinned truth table;
  token-value-free errors mirroring `token_parse_error` discipline). `apply_refresh` =
  surgical Value mutation + atomic store write, unmodeled-field round-trip test.
- [x] **R2 standby scan.** `codex_standby_candidates` (§0.9 predicate, pure, unit-tested)
  + `codex_standby_tick` on the scheduler lease-holder tick: due check (§0.12), worker
  holds RotationGuard, in-guard re-check, HTTP, apply-or-flag under the state lock;
  transient widening in-memory. Activity slot `Refreshing` so the TUI row spins and the
  switch try-probe story (§0.9) has its user-visible other half.
- [x] **R3 switch/capture try-probe.** Rotation-lock try-probe in `codex_switch_profile` +
  `codex_capture_into_profile` (busy → actionable error; drain converts to backoff).
- [x] **R4 loopback extraction.** `src/loopback.rs` from `oauth_login.rs` (PKCE pair,
  percent codecs, listener/wait_for_code/callback pages) parametrized by bind strategy
  (ephemeral vs fixed-port list) + callback path; `oauth_login` re-exports — its inline
  tests unchanged and green prove the extraction.
- [x] **R5 codex PKCE login.** `codex_browser_login(name)` per §0.13 (authorize URL golden
  test, form-encoded exchange body golden test, snapshot-shape test incl. explicit
  `auth_mode`); CLI `clauth login <name> --codex --browser` (parse + routing tests);
  clears `auth_broken` on success; TUI login row for a broken codex profile routes here.
- [x] **R6 surfaces.** doctor: codex check gains auth_broken line + last_refresh staleness
  (> 8 d → WARN "standby refresh not keeping up"); status.json already carries
  auth_status "broken" via the flag (verify + test).

**Acceptance (CDX-3):** suite green incl. golden wire-shape tests (no network in tests —
fixture HTTP via injected endpoint override? No: `refresh_result` mirrors the claude
pattern of pure body-builders + terminal-classification truth tables, HTTP layer stays
thin and untested beyond that, same as `oauth.rs`); sandbox e2e: fake parked profile with
near-expiry JWT → standby tick marks due → (HTTP stubbed at the candidates/apply seam)
apply mutates only the token fields; real refresh + real PKCE login are AX-manual
acceptance (never run unattended).

## CDX-1b — isolated start (tasks)

- [x] **S1 codex session leases.** `codex-sessions/` refcount dir + `has_live_codex_session`
  + gc sweep coverage (reuse `prune_stale_sessions`).
- [x] **S2 codex runtime acquire.** `CodexRuntime::acquire` (§0.14 tree, RotationGuard
  window, live-owner refusal, store-mode re-check, config.toml COPY) + watchdog
  adopt-back + final-sync Drop; unit tests over a sandbox home (fake `codex` binary not
  needed — command construction is pure).
- [x] **S3 CLI dispatch.** `cmd_start` harness dispatch → spawn `codex` with `CODEX_HOME`
  (env scrub, signal forwarding reuse); usage/help text.
- [x] **S4 refusal wiring.** switch/capture/standby/walk lease checks (§0.14) each with an
  adversarial test (the two-carrier scenarios).

**Acceptance (CDX-1b):** sandbox e2e: acquire builds the tree + copies store bytes; mutate
isolated auth.json → watchdog adopts back to store; drop tears down at zero sessions;
live-owner start refused; leased switch refused/deferred.

## CDX-4 — codex fallback chain (tasks)

- [x] **C1 chain edits route by harness.** `fallback_config` + every edit surface (TUI
  chain editor, MCP/socket config ops) — codex profiles join `codex_fallback_chain` with
  shared validation; tests both directions (claude profile can't land in the codex chain).
- [x] **C2 parser + field.** `rate_limit_reached_type` through `codex::usage` (§0.16) +
  status.json `codex_rate_limit_reached` + DESIGN.md.
- [x] **C3 scan.** `scan_codex_auto_switch` per §0.15 (pure walk fn + snapshot struct,
  exhaustive unit tests: exhausted-active/no-headroom/last-resort/broken/leased/lapsed-
  window cases, PLUS the degenerate markers — active not a chain member, chain of one,
  active marker naming a deleted or claude profile) wired into the lease-holder tick;
  per-harness queue-gate tests (§0.15: a pending claude entry must not block a codex
  enqueue, and vice versa); inline daemon test: fixture usage → pending switch → drain
  installs target.
- [x] **C4 surfaces.** status.json top-level `codex_fallback_chain` + per-profile
  `fallback` block for codex members (position/threshold/armed against the codex chain);
  TUI: codex chain membership rendering in the fallback tab; DESIGN.md.

**Acceptance (CDX-4):** sandbox e2e: two fake codex profiles in the codex chain, active
exhausted via fixture JSONL → tick enqueues → drain switches live bytes; claude chain
behavior untouched (regression suite).

## INT-1 — ccu daemon-feed migration (tasks, repo ~/projects/devtools/ccu)

- [ ] **U1 decode.** `ProfileRow` + `harness`/`codex_snapshot_at`/`codex_rate_limit_reached`;
  `Status` + `active_codex_profile`/`codex_fallback_chain` (all `#[serde(default)]`).
- [ ] **U2 render from the feed.** codex block(s) = `profiles` filtered by harness,
  reusing `usage_row`; RATE-LIMITED badge from `codex_rate_limit_reached` (same
  resets_at cross-check, now feed-side); usage-freshness display maps to
  `fetched_at`/`generated_at` — NOT `codex_snapshot_at`, which is stored-CREDENTIAL age
  (review finding: a migrator could wire the wrong stamp); delete `codex.rs` +
  `codex_tests.rs` (598 LOC across the pair) + the codex-specific poll gate in `app.rs`.
- [ ] **U3 docs.** README codex paragraph rewritten (daemon feed, not direct reads).

**Acceptance:** cargo test green; render tests over synthetic codex ProfileRows; README
current. Autonomous decision logged: no JSONL fallback retained — the daemon feed is the
single source (AX's machines always run the daemon; staleness already renders via
`generated_at` age).

## INT-2 — ccsbar codex fields (tasks, repo ~/projects/devtools/ccsbar)

- [ ] **B1 decode.** `ProfileStatus.harness`/`codexSnapshotAt`/`codexRateLimitReached`
  (+`isCodex`), `DaemonStatus.activeCodexProfile` — decodeIfPresent, fixture bump,
  decode-contract tests. (Interim window note: until B2 lands, a deployed ccsbar's
  `active: .first { $0.active }` can grab a codex-active row and feed claude-rotation
  UI — known, bounded to AX's own machines, closed by B2.)
- [ ] **B2 model split.** `StatusModel.active` → `activeClaude` (existing consumers are
  claude-rotation machinery) + `activeCodex`; codex auto-switch notification baseline
  (`lastNotifiedActiveCodex`).
- [ ] **B3 render.** AccountRow harness badge + VoiceOver; window-bar gate widened
  (`provider == "anthropic" || isCodex` — codex rows publish `provider: "openai"` and
  would otherwise fall to the third-party dot view); DetailCard codex freshness line
  (`codex_snapshot_at` = stored-credential age, distinct from usage freshness).
- [x] **B4 (revisited — shipped as ccsbar TABS-1, 2026-07-16):** the codex chain rail,
  a codexbar-style provider tab bar (Overview / Claude / Codex), and full codex
  management parity (switch confirmed against `active_codex_profile`, add/reauth via
  `clauth login --codex [--browser]`, harness-scoped chain editor) landed in ccsbar —
  see `ccsbar/docs/provider-tabs/PLAN.md`. Still deferred there: the menu-bar codex
  rung, codex rotation notifications (needs daemon-side codex switch provenance), and
  a daemon-published codex forecast (`forecast` is claude-only today).

**Acceptance:** swift test green (177 + new); `--snapshot` render over a mixed
claude+codex fixture.

## Wave-2 deviations (logged, autonomous decisions — filled at ship)

- **CDX-4 C4 TUI scope:** the TUI fallback tab stays claude-only this wave. Its
  inline chain editor predates `fallback_config` (whose migration is already the
  documented follow-up at the top of that module) — duplicating ~800 lines of
  inline editor for a second chain would double the debt the follow-up removes.
  The codex chain's day-1 edit surfaces are the new `clauth fallback` CLI
  (routes by harness, edits both chains) and the daemon socket; display comes
  via status.json (ccu/ccsbar).
- **`clauth fallback` CLI added** (not in the original task list): the socket
  was the only chain-edit surface besides the TUI, so codex chains would have
  had no operator-facing editor. One command over the routed `fallback_config`
  primitives serves both harnesses; `fallback` added to the reserved names.
- **Wave-1 gap fixed in passing:** `fallback_config::rename`'s on-disk TECH-7
  merge omitted `codex_fallback_chain` + `active_codex_profile` (the in-memory
  rename covered them) — a socket rename of a codex profile would have stranded
  the old name in profiles.toml.

- Plan-review round (2026-07-16, 5-dimension adversarial workflow + refute pass): 4
  highest-severity findings REFUTED against the working tree; every CONFIRMED MED/LOW
  folded back into §0.9–0.17, the task lists, and proxy-design.md in the same commit —
  no severity was skip-licensed. Token-value-free logging is an explicit invariant on
  every new logline (refresh outcomes, proxy request logs, doctor output), inherited
  from the claude-side `token_parse_error` discipline.

---

## CDX-1b original deferral note (2026-07-16, superseded by wave 2)

Deferred out of the wave-1 session (autonomous decision): a correct isolated start
needs session refcounting — without it, profile X live in the shared home AND
running isolated is two carriers of one refresh chain (`refresh_token_reused`
kill), and a later shared-home switch to X can't know an isolated session
holds the fresher chain. Resolved by §0.14 (leases + refusals) riding CDX-3's
refresh-ownership discipline (§0.9).

## CDX-2 — passive usage (tasks)

- [x] **U1 rollout reader.** `src/codex/usage.rs`: discover newest
  `sessions/YYYY/MM/DD/rollout-*.jsonl{,.zst}` (walk 3 fixed levels, newest by name/mtime);
  tail-parse `token_count` events leniently (tokens.rs patterns; ccu
  `~/projects/devtools/ccu/src/codex.rs` already ships this parser — port, don't reinvent);
  zstd via `ruzstd` (pure Rust) unless ccu proved `zstd` crate necessary. Output:
  `RateLimitSnapshot`-shaped `{primary, secondary, plan}` → map to `UsageWindow`
  (`utilization`, `resets_at` unix→ISO).
- [x] **U2 scheduler passive leg.** codex profiles partition into a passive branch: read
  JSONL for the active codex profile; inactive = cache + `resets_at < now` ⇒ synthetic
  reset. No PollStreaks/kick/rotation (passive reads cannot 429). Cadence: reuse
  per-profile interval; reads are local-only so the cheap default is fine.
- [x] **U3 surfaces.** TUI usage bars + plan/email for codex profiles; status.json windows
  (existing shape — additive); tokens.json untouched this phase (Claude-Code-specific
  rollup; codex token feed = follow-up with ccu migration).

**Acceptance (CDX-2):** fixture JSONL parse tests incl. old flat schema + missing ordinal
(`.zst` files recognized and skipped with a log — no zstd dep until reality produces them:
0 of 1136 local session files are compressed at 0.144.4); sandbox e2e: fixture sessions dir
→ UsageStore windows → status.json; TUI assertion via ratatui `TestBackend` buffer (harness
tag + usage bar present), not eyeballing.

## CDX-1c — resume convenience (stretch)

- [ ] `clauth resume <name>`: codex-harness only — switch (§0.5) then exec `codex resume
  --last` in current terminal. Documented as semi-seamless carryover.

---

## Non-goals (standing)

Live-session hot-swap via **file** (impossible — but CDX-5's proxy delivers true in-session
fallback the other way, shipped); ~~`wham/usage` or any backend polling~~ (REVERSED
2026-07-22, AX — CDX-6 polls `wham/usage` read-only per profile at codex's own 60s cadence;
the proxy's usage still comes only from flow-through headers; feasibility §2.5 carries the
full reversal note); keyring store mode; auto-start kick for
codex; cross-harness fallback chains (each chain is single-harness by construction);
`clauth which` stays claude-only (codex has no `CLAUDE_CONFIG_DIR`-style session
classification — `clauth start <codex-profile>` isolates via `CODEX_HOME` instead, CDX-1b).

## Verification map

| Change | Gate |
|---|---|
| data model (T1) | round-trip serde tests old⇄new config; byte-stability test for existing files |
| file ops (T2/T3/T4) | sandboxed unit + property-ish tests (unmodeled-field round-trip, 0600, atomicity via temp+rename) |
| daemon (T6) | inline daemon tests driving tick with fixture homes |
| surfaces (T5/T7/T8/T9) | CLI integration tests, status_json snapshot tests, doctor check tests |
| whole milestone | `cargo test` + `cargo clippy -- -D warnings` + `cargo fmt --check` + code-review passes per rules/workflow.md |
