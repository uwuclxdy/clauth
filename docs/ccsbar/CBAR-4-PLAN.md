# CBAR-4 build plan — implementing the "Preflight" redesign

> Implementation plan for a **fresh agent session**. Inputs, in reading order:
> `UX-RESEARCH.md` (evidence + incidents), `CBAR-4-DESIGN.md` (the binding spec —
> wireframes, interaction map, type scale, color semantics). This file sequences
> the work and defines acceptance. Written 2026-07-04 against clauth `65c089a`
> (branch `feat/macos-keychain`) and ccsbar `4dfc26a` (master, no remote).
>
> Companion track: the Rust-side auth gate (Incident C) is specced here as
> `AUTH-1`/`AUTH-2` because CBAR4-6 consumes its output. The broader tech-debt
> backlog (TECH-\*) lives in `.agent/TECH-PLAN.md` (from the architecture audit)
> and is NOT part of this plan.

## Ground rules

- **Repos:** Swift work in `~/projects/devtools/ccsbar` (SPM, Swift 6 strict
  concurrency, macOS 14+). Rust work in `~/projects/devtools/clauth`.
- **No new dependencies** on either side.
- **Do NOT push or open PRs** — local commits only (operator constraint).
- **Never touch** the operator's real `~/.claude/.credentials.json` or the real
  `Claude Code-credentials` Keychain item in tests. OAuth/refresh tests use
  fixtures; keychain tests use a throwaway service name.
- **Live-switch testing is dangerous:** a real switch logs out every running
  Claude Code process (Incidents B/C). Test switching against a scratch
  `test2`-style profile ONLY with the operator present, or verify via the
  daemon's dry surfaces (status.json transitions on a scratch config dir).
- Verification gates per milestone: Swift — `swift build` + `swift test` + a
  `--snapshot` render reviewed against the wireframes; Rust — `cargo fmt` +
  `cargo clippy --all-targets -- -D warnings` + `cargo test`.
- Update `.agent/PROGRESS.md` (CBAR-4 section) with commit hashes + evidence
  after every milestone; keep going to the next milestone without pausing.

## Milestone sequence (dependency-ordered)

Rust first (small, unblocks honest UI states), then Swift foundation → state
machine → views. Each CBAR4-N is one focused work package with its own commit(s).

---

### AUTH-1 (Rust) — pre-switch auth gate: never install a stale/dead token

The Incident C fix. `actions.rs:114`'s "stale chains rotate lazily on first use"
is wrong for this fork: installs go to the macOS Keychain and instantly affect
every running `claude`.

- In the switch path (one shared gate used by CLI switch, socket `switch`, and
  the daemon's auto-switch executor): before installing profile credentials,
  check `access_token_expires_at()` (treat < now + 60s as expired). Expired →
  `oauth::refresh()` → `apply_rotated_tokens_locked` → install the rotated
  tokens. Refresh failure (revoked/invalid refresh token) → the switch **fails
  loudly** with a typed error ("auth-broken profile"); CLI gets a clear message
  naming `clauth login <name>`; the socket reply carries `ok:false` with an
  `error_code` (see AUTH-2). Third-party (api-key) profiles bypass the gate.
- Daemon auto-switch (`drain_pending_switch` + the fallback executor): an
  auth-broken target is **skipped like an exhausted member** — walk to the next
  candidate; log `auth-broken, skipping 'name'` to daemon.log.
- Chain walk (`fallback.rs::next_fallback_target` region, :117-181): exclude
  auth-broken members in the first pass AND the sink pass. Never rotate into
  dead credentials unattended.
- Persist the outcome: a failed refresh marks the profile `auth_broken = true`
  (in-memory store + surfaced via status.json; clear the flag on successful
  `clauth login` / successful refresh).
- Tests (all offline, fixtures): expired-but-refreshable → rotated tokens
  installed; refresh-fails → switch refused + flag set; chain walk skips broken
  member and picks the next; sink that is broken → wrap-off semantics rather
  than install; third-party bypass.

**Acceptance:** `cargo test` green including the five new tests; clippy clean;
manual: `clauth <scratch-profile>` with a doctored expired fixture refuses with
the login hint instead of installing.

### AUTH-2 (Rust) — surface auth + switch truth in the contracts

Small, additive, schema-compatible (all new fields optional → ccsbar's
lenient decode keeps working; bump nothing).

- `status.json` per-profile: `"auth_status": "ok" | "expiring" | "broken"`
  (`expiring` = access token past due but refresh not yet attempted/needed;
  `broken` = last refresh failed). Absent field = `ok` (back-compat rule for
  older daemons).
- `status.json` top-level: `"pending_switch": "<name>" | null` (the design's
  filed upstream ask — lets the UI show in-flight truth instead of the 6s
  heuristic) and per-command socket errors gain a stable
  `"error_code"` (`"unknown_profile" | "busy" | "auth_broken" | "invalid_value"`)
  alongside the prose `error` (Swift branches on code, prose stays for humans).
- Update `docs/ccsbar/DESIGN.md` §3 (IPC contract) + the socket dispatch
  tests + `status_json` tests in the same commit.

**Acceptance:** `cargo test` green; `clauth status --json | jq` shows
`auth_status` on every profile and `pending_switch` at top level; DESIGN.md §3
matches byte-for-byte.

### AUTH-3 (Rust + Swift, post-track follow-on) — proactive dropped-login detection + one-click browser reauth

Not in the original CBAR-4 synthesis; added after the track completed on the
operator ask "有时候 oauth 会掉" (logins drop silently). **Root cause:** AUTH-1
flags `auth_broken` only on the *switch/install* path, but the daemon's
usage-refresh poll (`usage::scheduler::fetch_with_rotation`) called
`oauth::refresh` (which flattens `RefreshError` → anyhow) and on ANY failure fell
back to cache — so a dead refresh token dropped a login with no signal until the
next switch attempt.

- **Daemon** (`53e933a`): the poll calls `oauth::refresh_result` (preserves
  `RefreshError::Invalid`/`Transient`); a pure `refresh_failure_is_terminal`
  helper (Invalid→true, Transient→false) gates `oauth::mark_auth_broken(config,
  name, true)` on a terminal failure and `…, false)` (clears) after a successful
  rotation. A dead login now surfaces in `status.json` the moment the poll sees
  it; a transient network blip never false-flags. `mark_auth_broken` → `pub(crate)`.
  Two offline tests (`dead_refresh_token_is_terminal` /
  `transient_refresh_failure_is_not_terminal`), wired into feature_coverage's
  "Automatic token refresh" map.
- **ccsbar** (`d109c95`): the `auth_broken` detail-card state becomes a
  `reauthSurface` — danger shield + terracotta "Log in again" verb that spawns
  `clauth login <name>` (self-contained browser OAuth, works daemon-up or -down;
  `capture_into_profile` clears the flag). `StatusModel.reauth` — single in-flight
  guard, flag set synchronously, spawn awaited off-`@MainActor` via
  `withCheckedContinuation`, socket refresh nudged only when the daemon is
  reachable. `AccountContextMenu` gains an anthropic-only reauth item; `PanelView`
  shows a global in-flight banner; `Snapshot` gains a `reauth` variant.

**Acceptance:** `cargo test` green (2 new); `swift test` **119/119**;
`--snapshot=reauth` renders the shield + "Log in again" verb. **Operator-only:**
click "Log in again" (or run `clauth login <name>`) and confirm the flag clears in
`status --json`.

### CBAR4-1 (Swift) — foundation: type scale + color system + DaemonClient result refactor

Everything later builds on these three; do them together so no view is written
twice.

- **Theme.swift** → implement `CBAR-4-DESIGN.md` §4 + §5 exactly: the type-scale
  roles (13pt floor for names/verbs/5h numerals; 11pt metadata; 10pt only for
  glyph-like meter labels; delete every `minimumScaleFactor`), the four color
  roles (terracotta #D97757 = ACTIVE only; #B85C33 = the act verb fill; sapphire
  #43ABE5 = armed/watching — finally wire the dead token; green/amber/red as
  light/dark dynamic pairs via `NSColor(name:dynamicProvider:)` — Latte hues in
  light, Mocha in dark), threshold-keyed `usageColor` (warn ≥ 0.8×threshold,
  danger ≥ threshold).
- **DaemonClient.swift** → every command returns
  `Result<Void, DaemonError>`; `enum DaemonError { refused(code: String?,
  message: String), unreachable, malformedReply }` (~7 call sites). Parse
  AUTH-2's `error_code` when present. Move socket IO **off @MainActor** (async
  wrapper or utility queue) with a 2s connect/read timeout — the audit found
  blocking BSD-socket IO on the main thread.
- **UsageBar** gains the in-track threshold tick (design §2 row anatomy).

**Acceptance:** `swift build` clean; a temporary snapshot render shows 13pt row
names + tick marks; grep proves no `minimumScaleFactor`, no `.caption` on
primary content, no direct socket call on the main actor.

### CBAR4-2 (Swift) — test target + forecast engine + liveness clock

The design's two pure engines land test-first (they are the truthfulness core):

- Add a `ccsbarTests` target to Package.swift.
- **Forecast engine**: one pure function mirroring `fallback.rs:117-181`'s chain
  walk (skip-active; below-own-threshold-or-never-fetched first pass; 100%-sink
  second pass; never sink-to-sink; **skip `auth_status == broken`** per AUTH-1).
  Comment carries fallback.rs line pins. Unit tests from fixture JSON: normal
  chain, sink-only, zero-armed, broken-member-skipped, all-spent.
- **Liveness ladder**: pure function `(generatedAt age, statusMtime fallback) →
  .live / .syncing / .dead` with the <5s / 5–15s / >15s bands; unit-tested.
- **resetHint / parseISO** (already shipped) get their regression tests here,
  including the microsecond case from Incident A's era.
- StatusModel grows the 1s UI clock (countdowns, ages) alongside the 4s poll;
  poll accelerates to 0.5s only while a switch is pending, hard-capped at 6s.

**Acceptance:** `swift test` green (first ever for this repo); forecast fixtures
cover the five scenarios; clock cadences asserted via injected dates.

### CBAR4-3 (Swift) — StatusModel switch state machine

- `enum SwitchPhase { idle, arming(target), pending(target, since), confirmed,
  failed(reason) }` driving the design §2 STATE 3 flow: arm-confirm when the
  active account `has_live_session`; instant failure on `.refused`/`.unreachable`;
  6s timeout only for accepted-then-dropped; CLI fallback (daemon dead) confirmed
  by **process exit code**, labeled "auto-switch inactive until daemon starts".
- Rotation heartbeat: `active_profile` changed with no local pending switch →
  publish a transient `rotated(to:)` event (8s), actor-agnostic wording.
- Config command lifecycle: optimistic pending per row, instant revert on
  `.refused` with the reason, red "!" at 6s.

**Acceptance:** `swift test` green — state machine unit-tested with a fake
DaemonClient (refused / unreachable / silent-drop / confirm paths, arm-timeout
revert at 5s, fast-poll cancellation at confirm and at +6s).

### CBAR4-4 (Swift) — PanelView rebuild: list + detail + status strip

The visible redesign, per §2 wireframes STATE 1/2 + §3 interaction map:

- Account LIST in stable file order (rows never reorder; the terracotta ✓ badge
  moves). Row anatomy: 13pt semibold name + 11pt tier + badge cluster (sapphire
  ⚡ armed / ✓ active / "in use" / amber fetch-status / ⚑ sink / **red
  auth-broken badge** per AUTH-2); full-width 6pt 5h bar with threshold tick +
  13pt mono % + 11pt reset; half-width 4pt 7d/Fable bars with 12pt numerals.
  Third-party rows: availability dot + "checked Ns ago", never %-bars.
- Single click = INSPECT (selection wash + hairline ring; detail card
  re-targets; zero daemon traffic). Inspection resets to active on open.
- Detail card: 15pt name, three windows with reset times, chain-membership line
  (forecast-engine wording), and the ONE switch surface — "Switch to X" verb
  (#B85C33, 28pt) with the live-session arm-confirm; auth-broken target renders
  the verb disabled with "login expired — clauth login <name>". **(Superseded by
  AUTH-3:** the disabled hint became a one-click **"Log in again"** browser-reauth
  verb — see the AUTH-3 section.)
- Status strip (single exception surface, fixed priority): dead-daemon banner
  (with [Start daemon] detached spawn + [Copy]) > switch lifecycle > wrap-off
  card (with min-resets ETA) > zero-armed warning (one-click [Add active to
  chain]) > forecast sentence ("Watching xfx — would switch to cl-ax at 95% ·
  now 62%").
- Dead state (STATE 4): rows dim 60%, stamps become "as of Xm ago", bars
  greyscale, config disabled, switch verb becomes "Switch via CLI".
- Keyboard: ↑/↓ inspection, ⌘↩ switch, R refresh, Esc close, ⌘Q quit.

**Acceptance:** `swift build` clean; `--snapshot` renders of all four canonical
states (extend Snapshot.swift with per-state mocks: default / inspecting /
mid-switch / daemon-dead) visually match the §2 wireframes; hover/keyboard paths
hand-verified in the live app.

### CBAR4-5 (Swift) — config surfaces: context menu + upgraded disclosure

Per design §7 (two surfaces, no Settings window):

- Right-click context menu on every row (native `contextMenu`): Switch / Refresh
  this account / Add to–Remove from chain / Move up / Move down / "Leave chain
  at ▸" preset submenu (50/80/90/95/Last resort 100%) / Copy account name.
- Inline Configure disclosure upgraded to the hit-target standard: 28pt rows,
  13pt labels, 22×22pt glyphs in 28pt targets, threshold legend ("auto-switch
  LEAVES this account at this 5h usage"; 100% = "last resort — parks here"),
  wrap-off as an outcome-language radio ("Stay on last account" / "Switch
  everything off — credentials cleared; resumes when a window resets") — the
  "wrap-off" jargon leaves all UI copy.
- Removing the ARMED member → inline confirm ("This disables auto-switch —
  remove anyway?").

**Acceptance:** snapshot of the expanded disclosure matches spec; config
lifecycle (shimmer → confirm/revert/!) exercised against the live daemon on a
scratch chain with the operator's OK, or against a scratch config dir.

### CBAR4-6 (Swift) — menu-bar label state ladder

Per design §6: 13pt, monospaced %, all state in SF Symbol shape (template
rendering kills color). Priority ladder: dead (⚠ + name + frozen age, % withheld)
> switch-in-flight (…) > rotation flash (⇄ name, 8s, cancelled by a user switch)
> wrap-off (power-slash + "off") > no data (bare gauge) > 5h ≥ threshold (⚠ gauge)
> ≥ 0.8×threshold (gauge + dot) > disarmed chain (bolt.slash) > normal. Add
`auth_broken` on the ACTIVE account to the ⚠ tier. Third-party active: name +
availability dot, no %.

**Acceptance:** unit-test the ladder as a pure `(DaemonStatus, phase) →
LabelSpec` function — one test per rung + collision precedence; live app spot
check.

### CBAR4-7 — sync + acceptance

- Update ccsbar `README.md` (interaction model changed: click = inspect,
  switch is a verb) and `docs/ccsbar/DESIGN.md` §4 (point to CBAR-4-DESIGN.md
  as the successor visual spec); `.agent/PROGRESS.md` CBAR-4 entry with commit
  hashes + evidence; run the neat-freak reconciliation.
- Package (`Scripts/package_app.sh`), relaunch, and hand the operator the
  four-state walkthrough (the aesthetic call is theirs).
- **Operator-only acceptance:** a real switch test (it logs out running Claude
  sessions — their call when), and the CBAR-4 look.

## Risk register (from the design; implementer must own)

1. The 6s timeout is still a heuristic until AUTH-2's `pending_switch` ships —
   build the state machine to consume `pending_switch` when present, fall back
   to the timeout when absent.
2. Forecast-engine drift vs `fallback.rs` — the line-pin comment + fixture tests
   are contractual, not optional; add a `feature_coverage`-style meta-test on
   the Rust side if the walk changes.
3. `MenuBarExtra(.window)` first-open keyboard focus is flaky — `.defaultFocus`,
   accept mouse-first if it fights.
4. Checkmark badge animation across rows inside a resizing panel — degrade to a
   150ms crossfade if `matchedGeometryEffect` janks.
5. Panel height ~520pt at 3 accounts, +~58pt per extra; scroll inside
   `maxHeight` past 6.

## Suggested kickoff prompt (for the implementing session)

```
Implement CBAR-4 per docs/ccsbar/CBAR-4-PLAN.md in ~/projects/devtools/clauth
(Rust: AUTH-1, AUTH-2) and ~/projects/devtools/ccsbar (Swift: CBAR4-1 … CBAR4-7).
Read UX-RESEARCH.md and CBAR-4-DESIGN.md first; the design doc is binding — no
"could either" reinterpretation. Work the milestones in dependency order, TDD on
the pure engines (forecast, liveness, label ladder, switch state machine).
After each milestone: run the verification gate, commit locally (NO push), update
.agent/PROGRESS.md, continue. Never touch the real Keychain item or
~/.claude/.credentials.json in tests; live switch tests only with the operator.
```

/goal predicate (pair with autonomous-grind):

```
In ~/projects/devtools/clauth and ~/projects/devtools/ccsbar: (1) cargo test +
clippy -D warnings green with the AUTH-1/AUTH-2 tests; (2) swift build && swift
test green including forecast/liveness/label-ladder/state-machine tests; (3)
--snapshot renders exist for all four canonical states; (4) .agent/PROGRESS.md
records every CBAR4-N with commit hashes. Evidence pasted in transcript. OR stop
after 50 turns.
```
