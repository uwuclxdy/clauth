# clauth macOS fork — progress ledger

Branch: `main` (the fork's default; renamed from `feat/macos-keychain` 2026-07-08, off `upstream/mommy`). Fork of `uwuclxdy/clauth`
adding real macOS Keychain account switching, a self-contained browser OAuth
login, and a headless daemon + `status.json` feed for a menu-bar app.

**Core requirement (drives everything):** auto-switch accounts *before* the active
one's 5-hour window blocks a running Claude Code session — unattended, with the
TUI closed. A running `claude` re-reads the Keychain per request, so a Keychain
rewrite switches it mid-session (user-verified). The daemon delivers the
"unattended, TUI closed" half.

## Status by milestone

| Milestone | State | Evidence |
|---|---|---|
| **KC** — switch writes the `Claude Code-credentials` Keychain item via `/usr/bin/security` | ✅ done | `src/keychain.rs`; `clauth xfx` switches a running session (user-verified) |
| **Divergence fix** — no false "uncaptured credentials" prompt on every switch | ✅ done | `classify_link_at` compares OAuth token, not symlink identity; also upstream PR #4 |
| **Switch fix** — force-relink so a captured Keychain mirror isn't refused | ✅ done | `actions::switch_profile`; regression test `switch_replaces_active_account_mirror_without_refusing` |
| **OAUTH** — `clauth login` = self-contained browser PKCE login | ✅ done | `src/oauth_login.rs`; upstream's CC-`/login` fails on macOS (see Ground truth) |
| **Daemon R1** — headless scheduler owner + auto-switch execution | ✅ done | `src/daemon/mod.rs`; on-device boot/tick/guard verified |
| **Daemon R2** — `~/.clauth/status.json` serializer | ✅ done | `src/daemon/status_json.rs`; `status --json` shape correct vs real caches |
| **Daemon R4** — `clauthd.sock` (snapshot/switch/refresh) | ✅ done | `src/daemon/socket.rs`; live socket round-trip verified |
| **Daemon R5** — `clauth status --json` single-shot | ✅ done | dispatch arm in `main.rs` |
| **Daemon R6** — launchd LaunchAgent | ✅ done | `dist/macos/com.clauth.daemon.plist` + `daemon-install.sh` |
| **Daemon R3** — TUI↔daemon coordination | ⏸ deferred | see below |
| **Phase S** — `ccsbar` menu-bar app | ✅ built | own repo `~/projects/devtools/ccsbar`; `swift build` clean, launches; S7 (Settings/Sparkle/cask) deferred. **Renamed clauthbar → ccsbar (Claude Code Switcher Bar) 2026-07-06** — repo/dir/module/bundle all `ccsbar`; CBAR-4 milestone codenames kept as historical |
| **CBAR-2** — menu-bar 7d/fable usage + fallback chain/status + inline config | ✅ built | daemon config socket + `fallback_config` primitives + rich ccsbar UI; new commands verified live |
| **CBAR-3** — CodexBar-aesthetic SwiftUI redesign | ✅ built | `MenuBarExtra(.window)` panel replaces `NSMenu`+block-char bars; ccsbar `745dae3`; `swift build` clean, snapshot-verified, relaunched |
| **resetHint fix** — daemon's microsecond `resets_at` timestamps parsed | ✅ fixed | ccsbar `4dfc26a`; "resets in …" hints were silently nil against real data; snapshot mock now regression-guards |
| **CBAR-4 research** — UX research → "Preflight" design → build plan (docs only) | ✅ recorded | `docs/ccsbar/{UX-RESEARCH,CBAR-4-DESIGN,CBAR-4-PLAN}.md`; 3 researchers → 4 proposals → 3 judges → synthesis; 3 live incidents captured incl. Incident C (stale-token switch logs out every running claude → AUTH-1/2 specced) |
| **CBAR-4 build** — "Preflight" inspect-first panel shipped + live-hardened | ✅ built | ccsbar: inspect-first list + detail-card switch + chain rail + inline config (CBAR4-1..7); weekly-reset timer `a1d8fd5`, config Move-up/down reorder `8583229`, switch-latency `49de467` (settle waits for the actual effect) + daemon tick-wake `69cc0c3` (0.92s→0.034s), UX-clarity pass `ec76521` (⚡"watching" chip, switch-discoverability hint, "spent" exhaustion pill). `swift build` clean, 113 tests green, snapshot-verified, running live |
| **TECH audit** — 6-dimension architecture audit → 14-milestone backlog (docs only) | ✅ recorded | `.agent/TECH-PLAN.md` (42 confirmed / 6 refuted findings, adversarially verified); kickoff prompts in `.agent/GOAL-PROMPT.md` |
| **UP-1** — upstream sync: PRs #4/#18/#19 merged by uwuclxdy + follow-ups reconciled | ✅ done | merge `387607f` (2026-07-06): upstream took all 3 PRs, added windows `rundll32` open_url fix, loopback callback security tests, `Some("")` classify guard, TUI login modal (`803840b`/`ca2d248`), burn-aware switching (#8-b), exclusive `last_resort`; fork keeps reauth-existing `clauth login` (upstream parked as #7 follow-up). 667→670 tests, clippy clean |
| **UP-3** — daemon publishes its own forecast in status.json (kills ccsbar mirror drift) | ✅ done | `81c00a2`: upstream's walk-semantics change (exclusive `last_resort` replaced the threshold-100 sink convention + burn-aware active check) silently invalidated ccsbar's line-pinned Swift mirror — semantic flip, green fixture tests, wrong prediction. Fix: additive `forecast{action,to}` + `burn_aware` + per-profile `fallback.last_resort` in status.json, computed by the same `fallback::next_target` the switch decision runs; ccsbar renders it (mirror = fallback for old daemons; ccsbar `e4f2d24`). Companion `set_last_resort` socket command (`2410a5b`) + ccsbar real-flag config UI replacing the threshold-100 convention (ccsbar `858001d`, 134 tests). Deployed live 2026-07-07: clauth 0.7.4 installed, daemon restarted, ccsbar repackaged+relaunched. 674 tests |
| **UP-4** — contribute-back round 2 | ✅ done 2026-07-07 | PR [#21](https://github.com/uwuclxdy/clauth/pull/21) + PR [#22](https://github.com/uwuclxdy/clauth/pull/22) **merged** (v0.8.0) with upstream follow-ups: `acdcf86` reap-on-stdin-failure, `089a696` confirm-before-reauth. Fork re-synced in `19c50f9`. AUTH-1 + daemon offers on [#1](https://github.com/uwuclxdy/clauth/issues/1) still unanswered |
| **UP-5** — rotation coherence (the #1 residual, fired live) | 🔶 in flight | 2026-07-07 incident: daemon rotation revoked CC's Keychain chain → running claude "Not logged in" while every clauth copy showed `ok` (and the reverse direction quarantined xfx/xfx-backup — see daemon.log). Fix: (a) `apply_rotated_tokens_locked` mirrors the ACTIVE profile's fresh pair into the Keychain (foreign-login gate: `live_login_is_foreign`), (b) proactive rotation `ACTIVE_ROTATE_LEAD_MS`=30 min ahead of expiry so the daemon always wins the single-use-chain race. 692 tests. Upstream PR [#24](https://github.com/uwuclxdy/clauth/pull/24) opened + on-device report on [#1](https://github.com/uwuclxdy/clauth/issues/1#issuecomment-4910017468). **v2 after review (2026-07-08)**: adopt-don't-race — `try_adopt_live_rotation` adopts CC's fresher pair from the live file mirror (identity-guarded via `/api/oauth/profile` uuid, anchor cached per profile), lead reframed to interval-derived (3 polls, 3-min floor); fork 710 tests + PR branch through `f34f0aa`. **Multipass review round (2026-07-08)**: 2 confirmed findings fixed — adopted pair now returned + synced into the TokenList as `rotated` (HIGH: stale entry would spend the revoked refresh and falsely quarantine), blank-uuid identity refused (LOW: Some("")==Some("") shape drift). Deployed live |
| **UP-6** — weekly-cap truth + contribute round 4 (2026-07-08) | 🔶 review | 7d=100 members were fallback targets AND kept "recovering" every 5h rollover (fork `dd5ccf9`, upstream PR [#26](https://github.com/uwuclxdy/clauth/pull/26)); the daemon-published forecast walked BLIND (`Profile.usage` is TUI-only — hydrated from disk caches, `044781f`); ccsbar legacy mirror got weekly parity. PR #25 review round done (comments→tests, `3e23619`); daemon/ccsbar appetite discussion opened as [#27](https://github.com/uwuclxdy/clauth/issues/27) |
| **AUTH-4** — a dead login is itself a switch trigger (the broken-active wedge) | ✅ done 2026-07-09 | Live incident: ax-main's refresh chain died mid-window (no identity anchor → adopt couldn't verify → quarantined); the daemon then sat on the corpse for hours while ax-backup idled at 0% — `scan_auto_switch` requires a `Fresh` active read (impossible for a dead login) and `next_auto_switch_target` requires exhaustion (a frozen-lapsed 5h window reads as headroom). Fix (three sites): broken-active bypasses the freshness gate (`scan_auto_switch`) + the exhaustion gate (`next_auto_switch_target`, `auto_switch_if_needed` TUI parity); **wrap-off still keys on REAL exhaustion only** — the flag alone never halts a possibly-healthy live session. Companion fixes: (B3) the reauth path `overwrite_captured_profile` now lifts the quarantine like `capture_into_profile` always did (the old AUTH-3 test covered the wrong path — reauth via menu bar left the flag + banner up after a successful login); (B5) identity anchor (`account_id.json`) backfills on the hourly `/profile` tier fetch, write-if-missing, zero extra HTTP — closes the "anchor-less profile wedges forever once its pair dies" hole that caused this incident. Review round (3-lens + adversarial verify, correctness lens clean): `mark_auth_broken` + the reauth clear now persist as **narrow `update_app_state` deltas** (blind whole-state saves could cross-process clobber, TECH-7 surface); forecast doc + README reworded — the published `forecast` is a PROJECTION of the walk, the live decision additionally gates on freshness/exhaustion/broken; anchor seed's non-atomic missing-check documented as accepted (fail-safe: wrong anchor only ever REFUSES adoption). Mutation-tested: gate reverts fail the new tests (`scan_auto_switch_walks_off…`, `auto_switch_if_needed_walks_off…`, `mark_auth_broken_merges…`); the two wrap-off tests are deliberate invariant guards (non-distinguishing pre-fix, they pin the `&& active_exhausted` Off key). Also hardened `cross_thread_with_state_lock_serializes` (unsandboxed → raced the global home override; now HomeSandbox-pinned). 719 tests ×4, clippy+fmt clean. Known accepted debt: `overwrite_captured_profile`'s tail is still a blind whole-state save for NON-auth_broken fields (pre-existing; TECH-7 parity is a separate follow-up). Deployed: fork `bd5c116` pushed, daemon restarted on it, stale flags hand-cleared from `profiles.toml` (`ax-main` + rename-ghost `xfx-cl`) BEFORE the AUTH-4 binary started (order matters — the new gate would have acted on the stale flag). Ported upstream: PR #25 `7ecdda8` (AUTH-4 gates; its branch already had the reauth clear — the FORK had drifted behind its own PR there, which is what made B3 fork-only) + `82a6354` (fail-alive test hardening cherry-picked from #24; the flake reproduced during port verification), PR #24 `48179cb` (anchor backfill), comments posted on both. Attribution note: the ax-main→ax-backup switch during the fix window was executed by the OLD binary (new daemon's `last_switch` is null); daemon.log lines carry no timestamps, which made reconstruction guesswork — fixed same-day as TECH-15 |
| **TECH-15** — timestamped daemon.log lines | ✅ done 2026-07-10 | AUTH-4's forensics had to guess hours of switch/quarantine ordering from undated lines. New `src/logline.rs`: `logline!` macro (drop-in for `eprintln!`) + process-sticky `enable_timestamps()` flipped first thing in `daemon::serve()` — daemon stderr gains an ISO-8601 UTC prefix (`2026-07-10T09:13:13+00:00 clauth daemon: …`), CLI/TUI stderr stays bare (interactive noise). All 24 daemon-reachable sites converted (daemon/{mod,socket,tick}.rs, oauth.rs transitions/adopt, profile.rs prune warning); `clauth start` session-wrapper stderr (runtime.rs/start.rs) deliberately left alone — different surface, flag never set there. Format pinned by `render()` unit tests. 721 tests, clippy/fmt clean; deployed + verified live (standby's waiting line stamps too, since enable precedes the lock wait). **Upstream: HELD — verified 2026-07-10 that `upstream/mommy` has NO daemon module at all** (no `src/daemon/`, no `daemon` subcommand; the entire daemon/socket/status.json surface is fork-only, which is exactly what issue [#27](https://github.com/uwuclxdy/clauth/issues/27) is negotiating). TECH-15 upstream is inert until the daemon itself lands there. **#27 ANSWERED YES 2026-07-11** (minimal scope: usage-refresh + auto-switch + logging) — the daemon-module PR (with TECH-15 aboard) is actionable once #24/#25 land |
| **CBAR-5** — in-app "Add account…" (new-profile browser sign-in) | ✅ done 2026-07-08 | ccsbar `5bbf8d6` (feature: ⊕ row + inline banner + AddAccountValidation mirroring clauth's rules + injected-run tests, 149 green) + `9546851` (review round: all 4 verified findings fixed). clauth side: `login --new` flag (`7d0a894` — race-proof create; refuses reauth of an existing name against freshly-loaded config; 695 tests) because a UI-side collision pre-block is a TOCTOU and non-TTY spawns skip the reauth confirm. Deployed: app repackaged + relaunched. Follow-up ccsbar `4ca23cd`: SwitchMachine pending deadline now EXTENDS while status.json `pending_switch` still holds the target (2s re-checks, 30s ceiling) — first switch to a fresh account was false-failing at the blind 6s timeout while the daemon legitimately deferred its mid-fetch target |
| **UP-2** — `security -i` stdin keychain write (answers upstream's PR #18 EDR note) | ✅ done | `write_at` feeds `add-generic-password` to `security -i` over stdin — token no longer in argv (out of `es_event_exec_t` EDR logs). Tokenizer rules pinned empirically: quoted `\`/`"` escapes round-trip byte-identical; unquoted whitespace splits; inner exit code propagates (0/44/2); newline in value = loud refusal. KC-1 `--ignored` round-trip re-run vs real Keychain (throwaway service) incl. hostile-content token — passes |

### CBAR-2 — rich menu bar + fallback configuration
Added a second layer to the menu bar (the operator asked for weekly + fable usage,
visible fallback chain/status, a clearer active-account indicator, more dropdown
info, and *editing* the chain from the bar):
- **Rust:** `src/fallback_config.rs` — one home for chain edits (add/remove/move/
  threshold/wrap-off) + persistence (`save_app_state`/`save_profile`). The daemon
  exposes them over `clauthd.sock` as five new commands — `fallback_add`,
  `fallback_remove`, `fallback_move` (`dir`), `set_threshold` (`value` 0..=100),
  `set_wrap_off` (`value` bool) — which enqueue a `ConfigOp` (rank
  `PendingConfigOps`=1600) that the main loop drains + applies + persists (same
  leaf-drain-then-`config`-lock discipline as `drain_pending_switch`). `status.json`
  gained a top-level `fallback_chain` (ordered names). The TUI's own fallback editor
  still has parallel inline logic — migrating it onto `fallback_config` is a noted
  follow-up (see the module doc), left out to keep this change socket-scoped.
- **Swift (ccsbar):** account rows now show 5h/7d/fable bars + reset hints + the
  fallback line; a `Fallback chain` summary row; a `Configure ▸` submenu (per-account
  threshold picker / move up-down / add-remove + a wrap-off toggle) driving the new
  socket commands; the menu-bar title shows the active account **name** + 5h%.
- **Verified live:** all five config commands round-tripped over the socket against
  real config (add/move/remove chain, threshold 95→80→95, wrap-off on/off), every
  error case returns `ok:false`, config restored to baseline. `cargo test` passed,
  clippy clean; `swift build` clean; new daemon + ccsbar relaunched.
- **Multipass review (3-lens adversarial):** 7 findings, 6 fixed —
  (1) glyph showed green "0%" for a never-fetched active account → now "—";
  (2) fable window matched an exact server-derived label → lenient `"7d …fable…"`;
  (3) `drain_config_ops` bumped `last_state_mtime` without a real `profiles.toml`
  write → primitives now return `Ok(true/false)` (wrote-state), bump gated on it;
  (4) `add()` two-file write non-atomic → all primitives roll back the in-memory
  mutation on save failure (memory never diverges from disk);
  (7) third-party rows showed three empty bars → now render an availability dot.
  (5, low) account-row custom colors didn't invert under the NSMenu hover
  highlight — an `attributedTitle` limitation → **resolved by CBAR-3**: the
  SwiftUI `MenuBarExtra(.window)` panel draws its own tiles/controls, so there is
  no NSMenu highlight to fight.

### CBAR-3 — CodexBar-aesthetic SwiftUI redesign (ccsbar `745dae3`)
The operator asked to "make it prettier, matching CodexBar." Rebuilt the menu bar
from `NSMenu` + block-character (█░) bars to a SwiftUI `MenuBarExtra(.window)`
translucent panel — the model **current CodexBar actually uses** (superseding the
DESIGN §2 `NSMenu`-hosting recommendation; DESIGN.md updated to match).
- **Structure:** account-switcher tiles (active filled in the terracotta accent) →
  the active account's Session/Weekly/Fable meters (thin rounded `UsageBar`s, "%
  used" + "resets in …", days for long windows) → an armed-glow fallback-chain
  strip → an inline `Configure` disclosure → Refresh/Quit. Third-party api-key
  accounts render an availability dot. Every control is custom `.plain`-styled to
  match the panel's drawn capsules (no `Menu`/`Toggle` system chrome mismatch).
- **Files:** `Theme` (SwiftUI tokens + `UsageBar` + `resetHint`), `StatusModel`
  (`@MainActor ObservableObject` poll + daemon commands), `PanelView`, `ConfigView`,
  `AppMain` (`MenuBarExtra(.window)` + `--snapshot`), `Snapshot` (headless
  `ImageRenderer` panel→PNG design-review harness). `StatusItemController` deleted.
- **Verified:** `swift build`/release clean (Swift 6 strict concurrency); the
  `--snapshot` render confirmed the layout + caught a reset-time bug ("136h" → "5d
  16h"); packaged `.app` relaunched against the live daemon. No Rust changes.

### R3 deferral (documented, not forgotten)
Daemon-side single-instance guard is done (advisory flock on `~/.clauth/clauthd.lock`).
The remaining half — TUI detects a live daemon and skips its own `spawn_refresher`,
following the daemon's on-disk caches — is deferred: it needs a "follow daemon"
display mode + routing manual refresh (`r`) to the socket. When both run
concurrently, the cross-process state flock prevents corruption; the cost is a doubled request rate /
auto-start kicks. Marker comment at `src/tui/app.rs::start_scheduler`.

## Ground truth (macOS, CC v2.1.199) — do not re-derive

- CC reads its live login from the **Keychain** (`service="Claude Code-credentials",
  account=$USER`), and *mirrors* it into `~/.claude/.credentials.json` (a 0600 plain
  file) every run. Editing the file alone does NOT switch a running `claude`;
  rewriting the Keychain does.
- A **fresh** `/login` under a custom `CLAUDE_CONFIG_DIR` writes **only** a per-config-
  dir hashed Keychain item (`Claude Code-credentials-<hash>`), never that dir's
  `.credentials.json`. → upstream's `clauth login` reports "no login detected" on
  macOS. This is why the fork uses its own browser-PKCE login (writes the file
  directly). Corrected upstream: uwuclxdy/clauth#1 (comment).
- Always-Allow ACL binds to the **calling binary's code signature**. Route Keychain
  writes through Apple's stable `/usr/bin/security` so the grant persists across
  ad-hoc-signed rebuilds (`dist/macos/signed-install.sh` pins a stable identity).
- The scheduler (`usage::scheduler::spawn_refresher`) already persists
  `usage_cache.json` inside `apply_outcome`, so the daemon and TUI share one cache.
- Lock ranks: `CONFIG` (400) outranks `USAGE_STATUS` (350); snapshot the live stores
  before taking `config` (the daemon's `write_status` does this).

## Real-usage acceptance (run 2026-07-03 with the operator's explicit authorization)

Ran against the live config/accounts (xfx↔cl-ax), all reversible; verified each
switch by hashing the `Claude Code-credentials` Keychain item (no secret printed)
and confirmed state fully restored to xfx afterward.

1. **Manual switch** ✅ — `clauth cl-ax` rewrote the Keychain (fp `6961c0be…`→
   `0462b617…`); `clauth xfx` restored it. A switch really rewrites the Keychain.
2. **Daemon auto-switch (the core requirement)** ✅ — lowered xfx's
   `fallback_threshold` to 1 (< its 2% 5h), ran `clauth daemon` headless (no TUI);
   it auto-switched xfx→cl-ax in **~3 s** (Keychain rewritten; `daemon.log`:
   `auto-switched to 'cl-ax'`). Restored threshold + active. Unattended switch works.
3. **ccsbar socket IPC** ✅ — drove `clauthd.sock` with the exact newline-JSON
   ccsbar sends (`{"cmd":"snapshot"}`, `{"cmd":"switch","profile":…}`); daemon
   replied `{"ok":true}` and executed the switch (Keychain verified). Rust↔Swift
   `status.json` contract matches field-for-field (incl. window shape).

Still operator-only (can't be automated): **browser OAuth `clauth login`** (needs a
human to approve in the browser) and the **menu-bar visual/aesthetic** (their call).

## Verification (this branch)
- `cargo test` → 484 passed, 0 failed. `cargo clippy --all-targets` clean; `cargo fmt --all -- --check` clean.
- `clauth status --json` against real caches: schema 1, correct active/fallback/
  windows; never-fetched profile nulls out.
- `clauth daemon`: boots, writes `status.json` (0600), ticks (generated_at advances),
  single-instance guard rejects a second instance; `clauthd.sock` (0600) answers
  snapshot/switch/refresh. Auto-switch + socket switch verified live (see above).
- Note: `status.json` is only rewritten while a daemon runs; a manual `clauth <name>`
  with no daemon leaves the file stale (by design — ccsbar shows a staleness cue).

## CBAR-4 track progress (from `docs/ccsbar/CBAR-4-PLAN.md`)

| Milestone | State | Commit | Evidence |
|---|---|---|---|
| **AUTH-1** — pre-install auth gate (Incident C): never install a stale/dead token | ✅ done | `f9701fb` | `oauth::ensure_installable` gates every install choke point (CLI `switch_profile_cli`; socket + daemon auto-switch via `drain_pending_switch`): third-party bypass, refresh an expiring token before install, quarantine a revoked one (`AppState.auth_broken`, persisted) + refuse loudly. Permanent (4xx→quarantine) vs transient (network/5xx→retry, no quarantine) split; refresh runs under RotationGuard with no config lock held; injected refresher = offline-testable. Chain walk (`next_target` / `next_auto_switch_target` via `ChainSnapshot.broken` / `scan_recovery`) skips broken in headroom AND sink pass; broken-sink+wrap-off → OFF. 10 new tests. `cargo test` 494, clippy/fmt clean |
| **AUTH-2** — surface auth + switch truth in the contracts | ✅ done | `d4c0f41` | status.json per-profile `auth_status` (ok/expiring/broken, broken outranks expiring, absent=ok) + top-level `pending_switch` (via `LiveSignals`); socket `ok:false` gains stable `error_code` (unknown_profile/busy/auth_broken/invalid_value) + synchronous `switch`→auth_broken refusal with login hint; DESIGN.md §3 synced. 4 new tests. `cargo test` 498, clippy/fmt clean. Verified live: `clauth status --json` shows `auth_status` on every profile + top-level `pending_switch` |
| **CBAR4-1** — foundation: color roles + type floor + threshold tick | ✅ done | ccsbar `9f1d3c8` | Implements CBAR-4-DESIGN §4 (type scale) + §5 (color semantics) foundation. **Color — four roles, one meaning per hue** (dissolves the five-meanings-of-terracotta overload): `accent` #D97757 terracotta = ACTIVE only (no longer the healthy-bar fill); NEW `actVerb` #B85C33 = the ACT-verb button fill (≥4.5:1 AA under white); `sapphire` #43ABE5 = ARMED/WATCHING (sole armed hue); `success`/`warning`/`danger` now **light/dark DYNAMIC pairs** via `NSColor(name:dynamicProvider:)` (Catppuccin Latte in light, Mocha in dark) — fixes the flat-Mocha 1.3–2.3:1 light-mode contrast failures; `usageColor` healthy band → GREEN headroom keyed to each account's own threshold (terracotta never on a bar). **Type floor (§4):** AccountTile names → `.body` (13pt) + lineLimit(1) + tail truncation + `.help`; **`minimumScaleFactor` removed everywhere** (overflow truncates, never shrinks). **UsageBar:** optional in-track threshold tick (§8 Roster graft) — a hairline at the account's own auto-switch threshold so distance-to-rotation is pre-attentive; suppressed at 100 (sink/no-threshold); wired into row + Session 5h bars. **DaemonClient `Result` refactor: already satisfied by TECH-11's `CommandOutcome`** (.ok/.daemonError(code,message)/.unreachable + off-`@MainActor` `Task.detached` socket I/O + immediate error surfacing, 6s reserved for accepted-then-dropped) — functionally equivalent to the plan's `Result<Void, DaemonError>`; not re-churned. Acceptance: `swift build` clean, `swift test` **40/40** (usageColor bands re-pinned green-healthy + a roles-distinct test), `rg minimumScaleFactor Sources` → none, socket off-main, `--snapshot=healthy` renders. |
| **CBAR4-2** — forecast engine + liveness ladder (truthfulness core) | ✅ done | ccsbar `f24869e` | Two PURE test-first engines. **`ForecastEngine.nextTarget(_:now:)`** — a CONTRACTUAL mirror of clauth `fallback.rs::next_target` (line-pinned: walk_chain :127, next_target :154, is_exhausted :33, threshold_for :24 DEFAULT 95); predicts the daemon's next auto-switch target for every "would switch to X" string (naive position+1 banned, risk #2). Pass order headroom → 100%-sink → wrap-off→OFF; skip set = active / unresolvable / auth-broken (AUTH-1); `five_hour_live` = resets_at in the future (lapsed window = headroom); `now` injected. **`LivenessLadder.freshness`** — graded live <5s / syncing 5–15s / dead ≥15s on the 1s write cadence (§8 Roster graft, supersedes the binary 10s cliff) + statusMtime cross-check (younger age wins). **Keyed to the FIXED 1s cadence, NOT `refresh_interval_ms`** — `StatusModel.staleness` now uses it, **fixing TECH-4's refresh-interval-keyed threshold** that let a dead daemon read fresh for minutes (same cadence-vs-refetch trap TECH-12's doctor fixed on the Rust side). `DaemonStatus` gains additive per-profile `auth_status` + `authBroken` (AUTH-2 field the Swift model wasn't decoding). Tests: **16 new** (forecast all five scenarios + wrap-around-past-end + unresolvable + sink-pass-auth-broken + lapsed + degenerate; liveness band + cross-check boundaries via injected ages). Acceptance: `swift build` clean, `swift test` **54/54**. Reviewed (code-reviewer, opus): **mirror confirmed FAITHFUL across all 8 decision points + both cross-boundary contracts** (auth_status/is_auth_broken, threshold/threshold_for) verified against `status_json.rs`; 3 test-coverage-gap findings (wrap-around untested — risk #2's whole point — + unresolvable + sink-pass-auth-broken unpinned) all fixed with new fixtures. |
| **CBAR4-3** — switch state machine (arm/pending/confirm/fail) | ✅ done | ccsbar `ab822c3` | A PURE `SwitchMachine` reducer + its StatusModel effect wiring (design §2 STATE 3). **Machine (unit-tested):** Phase {idle, arming(target), pending(target), confirmed(target,viaCLI), failed(reason)} driven by events (requestSwitch w/ live-session flag, confirmArm, cancel, dispatched, observedActive, arm/pending timeouts, dismiss); instant failure on refused/unreachable, 6s timeout reserved for accepted-then-silently-dropped, double-tap mid-switch ignored. **`DaemonClient.switchTo` → `SwitchDispatch` {accepted, confirmedByCLI, refused(code,message), unreachable}** — the accepted-vs-confirmedByCLI split is load-bearing: a socket switch confirms by OBSERVING status.json, a CLI switch (daemon dead) confirms by EXIT CODE because a dead daemon never rewrites status.json (design §8 CLI-fallback honesty fix); no-fallback-on-rejection invariant preserved via a testable send/cli seam. **StatusModel** drives it: enter() runs per-phase effects (arm 5s / pending 6s timers, off-main dispatch, a status.json observation ladder feeding .observedActive); `switchInFlight` now derived from the phase; **rotation heartbeat** flashes "rotated to X" (8s) on an unattended auto-switch, guarded so a local switch's landing never false-flashes. Tests: **19 new** (SwitchMachine all paths + CommandOutcome dispatch paths incl. CLI confirm/refuse/unreachable), 73 total green. Reviewed (code-reviewer, opus): **wiring SOUND, no CRITICAL/HIGH** (actor isolation, timer cancellation, dispatch re-entrancy, no-fallback, double-confirm, rotation-flash ordering all verified); **3 findings fixed** — [MED] observe ladder's per-iteration sleeps summed PAST the 6s timeout (only 3 reads fit) so a switch landing in (4.2s,6s) false-failed → tightened cadence to land all reads <6s + added a final confirm-check at the deadline before declaring failure; [LOW×2] per-iteration-vs-cumulative ladder comments + dismiss-timer safety note. Acceptance: `swift build` clean, `swift test` **73/73**, four snapshot states render. **arm-confirm UI + the "Switch to X" verb land in CBAR4-4** (machine drives straight to pending until the confirm button exists). |
| **CBAR4-6** — menu-bar label state ladder | ✅ done (out of order — pure, before the CBAR4-4 visual rebuild) | ccsbar `48e3e95` | A PURE `MenuBarLabelLadder.spec(status, switchInFlight, rotationFlash, now) → LabelSpec` (design §6) — ALL state encoded in SF Symbol SHAPE, never color (the menu bar template-renders + flattens custom hues); the `%` always means the ACTIVE account's 5h window. Priority ladder (highest wins): (1) daemon DEAD (generated_at age >15s) → warning triangle + frozen age, **% WITHHELD**; (2) switch in flight → trailing ellipsis; (3) rotation flash → ⇄ name; (4) wrap-off all-off → power glyph + "off"; (5) no status.json → bare gauge; (6) 5h ≥ threshold → high-gauge; (7) ≥ 0.8×threshold → gauge + near-threshold dot; (9) normal; with (8) DISARMED (empty chain or zero armed) appending bolt.slash. Third-party active → availability dot, never %. Name tail-truncated to 12 chars. `MenuBarLabel` view rewired to render the spec (the old ad-hoc gauge+dim is gone). Tests: **16 new** — one per rung + collision precedence (dead>in-flight>rotation>wrap-off; over-threshold+disarmed co-occur) + truncation + third-party. **89 total green.** Done before CBAR4-4 because it's pure+independent (drives the separate menu-bar view) — same "land the tested truthfulness engine first" rationale the plan uses for forecast/liveness. SF Symbol NAME choices are the operator's visual call (CBAR4-7); the rung LOGIC is fully pinned by tests. |
| **CBAR4-4** — the inspect-first Preflight panel rebuild | ✅ done | ccsbar `e7b5e1a` | The visible redesign (design §2 wireframes / §3 interaction map): **status strip → account LIST → detail card → chain rail → actions**. Browse freely (single click inspects, **zero daemon traffic**), switch deliberately (**one** verb in the detail card). Consumes every CBAR4-1/2/3/6 engine. **New components:** `StatusStrip` — the SINGLE exception surface, priority-ordered §3.10 (dead-daemon banner w/ [Start daemon] spawn + [Copy] > switch lifecycle arm/pending/confirmed/failed incl. "via CLI · auto-switch inactive" > wrap-off resume-ETA card > zero-armed warning > forecast sentence + liveness stamp). `AccountRow` — **file-order rows that NEVER reorder** (§2 — only the ✓ badge + inspection ring move); badge cluster (armed bolt / sink flag / in-use / auth-broken / fetch), full-width 5h bar + threshold tick + 13pt %, half-width 7d/Fable bars, third-party availability line (never %-bars), 60% dim + greyscale + "as of Xm ago" when dead. `DetailCard` — inspected windows, forecast-driven chain-membership line, and THE one switch surface: static "Active account" / disabled "login expired" (→ upgraded to a one-click reauth verb in **AUTH-3**) / a **unified** Switch verb sharing the arm-confirm→pending cycle in BOTH the live and offline (Switch via CLI) paths. **StatusModel:** inspection state (inspect/reset/isInspected, **chain-head fallback when active is null** §3.1), forecast/forecastSentence/chainLine (ordinal-aware), autoSwitchIdle, wrapOffResumeETA, freshness/frozen-age helpers, startDaemon; `listProfiles` is now file-order; `isPreview` gates the open-reset so snapshot states survive rendering. `PanelView` orchestrates strip+accounts+detail+chain+actions with state routing (.outOfDate/.down/.ok/.stalled) + keyboard floor (⌘↩ switch, R refresh, ⌘Q quit, ↑/↓ inspect). Snapshot extended to the FOUR canonical states (default / inspecting / mid-switch / daemon-dead), all rendered + visually verified against the wireframes. Tests: **PanelLogicTests** (ordinal 11/12/13/111 edges, forecast sentence, chain line sink-vs-armed, autoSwitchIdle, wrap-off ETA, chain-head inspection fallback) + rewritten StatusModel inspection tests. **94 total green.** Reviewed (code-reviewer, opus): rebuild logically SOUND, no CRITICAL/HIGH; ALL findings fixed — [MED] dead/offline switch button could arm with no confirm affordance (unified the two buttons so the arm-confirm cycle works offline too); [MED] inspection fallback used first profile not first CHAIN member on active-null; [LOW] detail stamp shows age "Fresh · 3s" not the self-contradictory "· frozen"; [LOW] honest startDaemon comment + surface missing-binary. Live-app hover/keyboard/aesthetic sign-off is the operator's (CBAR4-7). Acceptance: `swift build` clean, `swift test` **94/94**, four `--snapshot` states render. |
| **CBAR4-5** — config surfaces: context menu + upgraded disclosure | ✅ done | ccsbar `d9b64e0` | The two §7 config surfaces (no Settings window), sharing **one** vocabulary. **FAST PATH** `AccountContextMenu` — a native right-click menu on every row at free NSMenu metrics: Switch / Refresh `<name>` / Add–Remove / Move up–down / "Leave chain at ▸" preset submenu (50/80/90/95/**Last resort 100%**) / Copy name. Switch **inspects-then-fires** so a live-session arm surfaces its Confirm in the detail card (not stranded) and relabels to "via CLI (daemon offline)" when the daemon's down. **CANONICAL EDITOR** — the inline Configure disclosure rebuilt to the **28pt hit-target standard** (13pt labels, 22×22pt glyphs in 28pt targets, threshold legend, wrap-off as an **outcome-language radio** "Stay on last account" / "Switch everything off — credentials cleared; resumes when a window resets"); the **"wrap-off" jargon is retired from ALL user-facing copy** (test-enforced sweep). **`ChainEdit`** — the single source of truth both surfaces + the chain rail read (presets, labels, legends, wrap-off copy, `currentThresholdLabel`, `whenSpentSummary`, and the pure `removalConsequence` armed-member gate). **Armed-member removal confirm** — removing an ARMED member routes through `requestRemove` → `pendingRemoval` (**no socket fired**) and surfaces a **PANEL-LEVEL** confirm banner (visible from BOTH the context menu and the disclosure, disclosure collapsed by default); copy is truthful about whether auto-switch actually stops (`disablesAutoSwitch` vs `armedMember`). **Lifecycle** — honest `configInFlight`/`configBusy` "Applying…" shimmer on real edits (refreshes suppress it); revert-with-reason + red banner via the TECH-11 `run()` path. Snapshot gains `config` + `remove-confirm` variants (both rendered + verified vs §7). Tests: **ConfigSurfaceTests** (preset list + sink label, no-jargon sweep, `removalConsequence` all four branches with truthful prompts, `requestRemove` arms without dispatching a socket) — **8 new, 102 total green**; tests never touch the real `~/.clauth` socket (operator constraint) — only pure decisions + the armed path that returns before any command. Reviewed (code-reviewer, opus): **ALL findings fixed** — [HIGH] context-menu armed removal was a silent no-op with the disclosure collapsed (lifted the confirm to a panel-level banner); [MED] chain-rail wrap-off copy hand-rolled a third surface (routed through `ChainEdit`); [LOW×4] current-threshold label divergence + 0-based test fixture position + offline-switch label + refresh shimmer; [NIT] `armedMember` prompt assertion. Concurrency (`configInFlight` main-actor 1:1, no leak/negative) + socket safety verified clean. Live-daemon config-lifecycle on a scratch chain is operator acceptance (CBAR4-7). Acceptance: `swift build` clean, `swift test` **102/102**, `--snapshot=config`+`=remove-confirm` render + match §7. |
| **CBAR4-7** — sync + acceptance (docs, packaging; live tests operator-only) | ✅ done (agent scope); ⏳ operator acceptance pending | ccsbar `c3fed2b` (README), clauth `<this>` (DESIGN §4 + ledger) | **Docs synced to the Preflight model:** ccsbar `README.md` rewritten off the pre-CBAR-4 tap-to-switch tiles → click=inspect / switch-is-a-verb, the status-strip→list→detail→chain→actions layout, the truthfulness engines, two config surfaces, the SF-Symbol label ladder, six `--snapshot` variants, and a full Architecture table (points at `CBAR-4-DESIGN.md` as binding). clauth `docs/ccsbar/DESIGN.md` §4 gains a **superseded banner** → CBAR-4-DESIGN.md (color/liveness semantics still feed it; the tile *layout* is historical); "Shipped divergence" + §6 sequencing marked pre-CBAR-4 with the CBAR-4 rebuild appended. **neat-freak reconciliation run** — enumerated both repos' docs + memory; only the two files above drifted (memory dir empty → in-repo ledger is source of truth; clauth README's daemon/socket contract unchanged + accurate); self-check clean (no relative-time, no stray present-tense tile refs, all doc links resolve). **Packaging:** `Scripts/package_app.sh` builds `build/ccsbar.app` (release, LSUIElement, ad-hoc signed) — **release build clean**. **Operator-only acceptance (NOT run — per constraints):** (a) `open build/ccsbar.app` + the four-state walkthrough (default/inspecting/mid-switch/daemon-dead) — the aesthetic sign-off is the operator's; (b) a **real two-account switch** (logs out every running Claude Code process → operator's call when). Hand-off commands below. |
| — **CBAR-4 track COMPLETE** (agent scope) | ✅ all 9 milestones done | AUTH-1 `f9701fb` · AUTH-2 `d4c0f41` · CBAR4-1 `9f1d3c8` · CBAR4-2 `f24869e` · CBAR4-3 `ab822c3` · CBAR4-6 `48e3e95` · CBAR4-4 `e7b5e1a` · CBAR4-5 `d9b64e0` · CBAR4-7 `c3fed2b` | Rust: `cargo test` 498 + clippy/fmt clean (AUTH gates). Swift: `swift build`/release clean, `swift test` **102/102**, six `--snapshot` states render + verified vs §2/§7 wireframes. All commits LOCAL (no push — operator constraint). **Remaining = operator acceptance only:** launch the packaged app, walk the four states, run one real switch. |
| **AUTH-3** — proactive dropped-login detection + one-click browser reauth (operator ask: "有时候 oauth 会掉") | ✅ done (post-track follow-on) | daemon `53e933a` · ccsbar `d109c95` | **Root cause:** AUTH-1 flags `auth_broken` only on the *switch/install* path; the daemon's usage-refresh poll (`usage::scheduler::fetch_with_rotation`) called `oauth::refresh` (flattens `RefreshError`→anyhow) and on ANY failure silently fell back to cache — so a dead refresh token dropped a login with **no signal** until the next switch attempt. **Daemon fix:** poll now calls `oauth::refresh_result` (preserves `RefreshError::Invalid`/`Transient`); a pure `refresh_failure_is_terminal` helper (Invalid→true, Transient→false) gates `oauth::mark_auth_broken(config,name,true)` on a terminal failure and `…,false)` (clears) after a successful rotation — so a dead login surfaces in `status.json` the moment the poll sees it, and a transient network blip never false-flags. `mark_auth_broken` → `pub(crate)`. 2 new inline tests (`dead_refresh_token_is_terminal`, `transient_refresh_failure_is_not_terminal`) wired into feature_coverage's "Automatic token refresh" map. **ccsbar fix (d109c95):** the `auth_broken` detail-card state (was a *disabled* "login expired" hint in CBAR4-4) becomes a **`reauthSurface`** — danger shield + terracotta **"Log in again"** verb that spawns `clauth login <name>` (self-contained browser OAuth, works daemon-up or -down; capture clears the flag). `StatusModel.reauth` — single in-flight guard, flag set synchronously, spawn awaited off-`@MainActor` via `withCheckedContinuation`, socket refresh nudged **only** when the daemon is reachable (a daemon-down `.ok` must not surface a spurious refresh error); `reauthFailureMessage` maps the outcome to a loud message with the exact terminal fallback command. `AccountContextMenu` gains an anthropic-only "Log in again / Re-authenticate (browser)" item; `PanelView` shows a **global** in-flight banner (a proactive reauth of a healthy account still gets feedback); `Snapshot` gains a `reauth` variant. Reviewed (code-reviewer, opus): all findings fixed — [MED-HIGH] daemon-down spurious refresh error (gated on `daemonReachable`), [MED] parked-thread → async continuation, [MED] context-menu no-feedback → global banner, [LOW-MED] untested async lifecycle → 2 async tests, [LOW×2] fixture anthropic filter + reauthSurface provider guard, [NIT×2] stale CLI hint + timeout-contract doc. Acceptance: `cargo test` **539** (2 new), clippy/fmt clean; `swift build`/release clean, `swift test` **119/119**, `--snapshot=reauth` renders the shield + "Log in again" verb (visually verified). |
| **AUTH-3 hardening** — three bugs found during operator acceptance, all fixed + verified live | ✅ done | daemon `e599b7c` `5b06ade` · ccsbar `e003960` | Operator ran the real "Log in again" flow and hit three defects the initial AUTH-3 missed. **(1) Reauth was a no-op** (`e599b7c`): `clauth login <name>` bailed "already exists" for any existing profile (`validate_profile_name` `exclude=None`) — so the button spawned a command that failed before the browser opened. `cmd_login` is now idempotent (new name creates, existing RE-AUTHENTICATES); `config.add` upserts so the capture replaces rather than duplicates. **(2) A dead token behind a 429 was masked** (`e599b7c`): `fetch_with_rotation` bailed on ANY 429 without attempting a refresh, so a dropped login whose usage endpoint 429s showed "RateLimited" forever instead of `auth_broken`. Now a 429 on a **clock-expired** token falls through to the refresh (mirrors `auto_start_kick`'s 429 gate via pure `token_clock_expired`), surfacing the dead token; a 429 on a valid token still bails (a refresh can't fix an endpoint limit + would re-spend the single-use token). **(3) Reauth of the ACTIVE account never rewrote the Keychain** (`5b06ade`) — THE "Not logged in · run /login" bug: `capture_into_profile` re-linked the live credential only when it ADOPTED active (disk had no active); re-authing the already-active account left the Keychain on the dead token, so a running `claude` (which reads the Keychain, not the file) kept failing. Now when the captured profile IS the on-disk active one, **force-relink** (rewrites the Keychain with fresh tokens; the divergence guard is bypassed correctly since we replace this profile's OWN stale login). **(4) ccsbar** (`e003960`): `DetailCard.switchSurface` checked `p.active` before `p.authBroken`, hiding the reauth verb on an active broken account (the most urgent case) behind "Active account" — reordered so a broken login outranks the active readout; `reauth-active` snapshot variant regression-covers it. Tests: `token_clock_expired` ×3, `reauth_capture_upserts…`, `reauth_of_the_active_account_force_relinks_the_stale_mirror`; **550** daemon + **127** ccsbar green, clippy/fmt clean. **Operator acceptance PASSED live** — "login again 可以了 修好了" (the live `claude` session recovered immediately after the Keychain rewrite). |
| **RENAME-1** — rename a profile from ccsbar | ✅ done | daemon `4d114c4` · ccsbar `89d1a6d` | New `rename` control-socket command so the menu-bar app can rename a profile. **Daemon:** `fallback_config::rename(old,new)` — validates the new name (charset + collision, self-rename allowed), renames the profile dir, updates every reference (name list / chain / active marker / auth_broken) via the TECH-7 RMW delta (no clobber of a concurrent switch's `active_profile`), and force-relinks the credential mirror when the renamed profile is active (rewrites the Keychain → a live session follows the rename; same tokens, so no logout). Rolls back dir+state on a persist failure. `ConfigOp::Rename` drained under the config lock; socket `rename` validates synchronously (immediate `ok:false` on a taken/invalid name) with a new `new_name` field. **ccsbar:** context-menu "Rename…" (any provider) opens an inline `RenameBanner` at the panel top (TextField + live client-side validation mirroring the daemon rule + Rename/Cancel/⏎); an invalid name surfaces a loud error and never fires the socket; a valid one runs the settle ladder (waits for the renamed profile in status.json). Tests: 6 daemon (3 primitive + 3 socket) + 8 ccsbar (`RenameTests`); DESIGN.md §3 IPC contract + both READMEs synced. `--snapshot=rename` renders the banner. |

## TECH backlog progress (from `.agent/TECH-PLAN.md`)

| Milestone | State | Commits | Evidence |
|---|---|---|---|
| **TECH-1** — branch backup, ledger truth & CI activation | ✅ done | `630afa7` (fmt), `178db03` (ledger+CI) | `feat/macos-keychain` pushed to origin (`178db03c…`, backup of the SPOF fork); `feat/macos-keychain-oauth` deleted → archived at tag `archive/feat-macos-keychain-oauth` (5531b9b); ci.yml push-trigger added → CI **run 28703818109 green** (check[fmt+clippy+test]/audit/deny all pass on ubuntu); ledger test count 465→484 = `cargo test` total; no PR opened (push-trigger path, per operator constraint) |
| **TECH-2** — fork install/update retargeting + Windows build gate | ✅ done | `bb7fe39` | `update.rs` `FORK_BUILD` disables the self-updater (`spawn`→None, `should_self_replace`→false; API_URL not retargeted); `install.sh` rewritten as source-build (no releases path / no crates.io); README + plugin-marketplace retargeted, **no `uwuclxdy`** in install.sh/README.md; `daemon-install.sh` capability check refuses a non-fork binary (**exit 1** verified); unix socket `#[cfg(unix)]`-gated + no-op stub → **`cargo check --target x86_64-pc-windows-gnu` exits 0** (was E0433; mingw local); ci.yml `windows-check` job added. Native: 500 tests, clippy -D/fmt clean. CI-green-on-push pending operator OK to push |
| **TECH-3** — daemon anti-wedge watchdog + standby supervision + keychain timeout | ✅ done | `26d3304` | tick heartbeat (AtomicU64) + watchdog thread `abort()`s on a >60s stall (pure `watchdog_check` unit-tested via injected flag — `cargo test daemon::watchdog` 4 pass); single-instance guard now BLOCKS as standby so launchd takes over when a manual daemon exits (#35); `run_with_deadline` gives every `security` call a 20s kill-on-expiry deadline so a stuck keychain can't pin the flock (`cargo test keychain_timeout` 2 pass, via `sleep`/`true` — no real Keychain). No unsafe. 506 tests, clippy -D/fmt clean, windows still compiles. Standby live kickstart test = operator-manual (real daemon relinks → touches real Keychain) |
| **TECH-5** — Daemon::tick extraction + inline test harness | ✅ done | `e53cea1` | Loop body (reload_if_changed / drain_pending_switch / drain_pending_switch_off / drain_config_ops + rebuild_tokens) extracted to new `src/daemon/tick.rs` behind `Daemon::tick()`; **pure refactor, no runtime behavior change** (tick() preserves exact call order; run() calls self.tick() then stamps heartbeat). mod.rs **632→421 LOC** (under the 500 gate); tick.rs 244. New `tests/inline/daemon_mod.rs` (mirrors socket.rs:211 `mod tests`) pins CURRENT behavior on 5 tests — `cargo test daemon::tests` **5 pass, deterministic 5/5 runs**: tick empty-queue no-op writes status; drain_pending_switch executes idle+clean target; **skips on active_diverged_unsaved** (CC re-login); drain_config_ops threshold(Ok(false)) does **not** suppress a same-tick external reload (the ledger's eyeballed mtime regression, now pinned); reload_if_changed fires on external mtime. All via `testutil::HomeSandbox` + keychain-disabled — **no real ~/.clauth / ~/.claude / Keychain touched**. feature_coverage 'Headless daemon' row now names the tick tests (`cargo test features_have_test_coverage` pass). Total 506→511, clippy -D/fmt clean. Unblocks TECH-6/7 (TDD-able). **Carry-forward debt (→ TECH-14 hygiene):** `runtime::tests::has_live_session_*` / `live_session_count_*` are a *pre-existing* flock-release/HOME-isolation flake (~1/3–1/5 full-suite runs on the **clean tree too**, correlated cluster, different subsystem, outside this diff) — NOT introduced or causally worsened by TECH-5 (daemon_mod is 100% green every run). |
| **TECH-13** — ccsbar test target + status.json fixture contract | ✅ done | ccsbar `f2ce638` | The executable had ZERO tests (SwiftPM can't test an executable's internals). **Library split:** `CCSBarKit` library (all logic, everything `internal`) + thin `ccsbar` executable (`Entry.swift`: `@main`→`runCCSBar()`, the one `public` symbol) + `CCSBarKitTests` via `@testable import`. `@main` stays in the executable; `CCSBarApp.main()` called explicitly as before — **no behavior change** (reviewed). **Single source of truth:** inline `Snapshot.mockJSON` deleted → checked-in fixture `Sources/CCSBarKit/Fixtures/status.json` (`.copy` resource), loaded via `Fixtures.statusJSONData()`/`Bundle.module` — the `--snapshot` render AND the decode contract test now share ONE fixture (no drift). Documented dev-only invariant (fixture never on the app hot path → `package_app.sh` omits the bundle; `Bundle.module` would fatalError there). **Two pure extractions** made finding-prone logic testable (behavior-preserving): `Theme.resetHintText(secondsRemaining:)` (split from `Date.now`) + `StatusModel.isStale(ageSeconds:refreshIntervalMs:)` (`nonisolated`, split from `staleness`). **23 tests** (all real behavior, not mock-existence): parseISO micro/plain/Zulu/garbage (the microsecond test caught that Foundation parses 6-digit fractions to ms, not the strip fallback), resetHint d/h/m boundaries, usageColor bands (75/76/94/95 + default-100), isStale 270/271 boundary + 15s floor, orderedProfiles active-first, isHealthy per liveness, SchemaProbe, fixture decode contract + leniency (missing fallback_chain→[], null tier, name+active-only, fableWeek `7d fable`/`7d fable 5`/`7d Fable` vs plain `7d` rejected). `Scripts/regen-fixture.sh` refreshes from `clauth status --json`. Acceptance: `swift test` **23/23**, `swift build` clean, `rg mockJSON Snapshot.swift` **gone**, `--snapshot=healthy` still renders (fixture via Bundle.module). Reviewed (code-reviewer): clean split, real tests, 3 findings (Bundle.module dev-only invariant comment, `try? XCTUnwrap`→`try`, exact isStale boundary) all fixed inline. |
| **TECH-4** — ccsbar daemon-liveness truth (staleness + schema gate) | ✅ done | ccsbar `632bff1` | **Tier-1 hero-TRUST** (a dead daemon must not render as fresh). `DaemonClient.readStatus` → `StatusRead` (ok/fileMissing/schemaUnsupported/decodeFailed): a `{schema}` probe runs BEFORE the full decode so a future schema bump reads as "ccsbar out of date", NOT "no daemon"; a real decode failure is logged via `os.Logger`, not swallowed. `StatusModel` gains a `Liveness` state (ok / stalled(since:) / outOfDate(schema:) / down); `reload()` computes staleness from `generated_at` age > `max(3×refresh_interval_ms, 15s)` (per spec — a file that stopped advancing is stalled even though its bytes don't change). `PanelView` renders each distinctly: a loud "Daemon stalled — data from HH:MM" banner OVER the last-known content, a separate "ccsbar out of date (schema N)" state, and the daemon-down empty state; `MenuBarLabel` dims the glyph when not live. `ProfileStatus` decodes leniently (only name+active load-bearing) for additive-era survival. Snapshot harness adds `--snapshot=stale` / `--snapshot=schema2` variants printing the resolved liveness. Acceptance: `swift build` clean; `--snapshot=stale` → `daemonStalled=true` + stale-banner PNG (visually verified), `--snapshot=schema2` → `outOfDate(schema: 2)` + out-of-date PNG (NOT "not running", visually verified), healthy → `ok` + content; `generatedAt`/`statusMtime` now live call sites (dead-helper acceptance); README stale-case claim corrected. **Also TECH-14 #31 folded in** (it makes `statusMtime` live): `reload()` mtime-gates the re-decode/republish (still recomputing liveness so ok→stalled transitions) + `timer.tolerance=1.0`. |
| **TECH-14** — hygiene sweep (Rust a/b/c ✅; Swift (d)✅ (e)✅ (f)✅) | ✅ done | `5bd4af1` + ccsbar `632bff1` (d) + `c5ad408` (e/f) | Three independent low-risk drift fixes (the Rust-repo items of the cherry-pickable batch). **(b) #23 completions drift:** added the missing user-facing subcommands (`status`, `daemon`, `doctor`, `completions`) to all three shells' first-token lists + their second-token args, and reworded the `login` description from the stale "via claude /login" to the browser-OAuth reality. Drift-guard tests assert every user-facing dispatch arm appears in bash/zsh/fish + that "claude /login" is gone. **(c) #24 macOS-semantics un-gated:** chose the finding's **option 2** (rationale, NOT `cfg(macos)` gate) because gating `classify_link_at`'s content-compare (claude.rs) and `switch_profile`'s force-relink (actions.rs) would make a non-symlink credentials file on Linux fall through to `read_link` and **error** — both branches are correct cross-platform; replaced the macOS-only comments with an explicit cross-platform rationale, existing non-gated regression tests stay green. **(a) #12 DESIGN.md:** corrected the `third_party` sample from the never-emitted `{balance_usd,currency}` to the real `{available:true}` (structured balance deferred, matches `status_json.rs:138`) + added the derived-label caveat for `windows[].label` (`7d Opus`/`7d fable` — opaque display string, not a switch key). Acceptance: `rg available docs/ccsbar/DESIGN.md` ✅, `rg 'daemon\|doctor\|status' src/completions.rs` all shells ✅ + zero `claude /login`, cross-platform rationale comments present + `cargo test` green (the non-gated Linux regression). **533 tests, clippy -D/fmt clean.** **Swift items:** (d) #31 mtime republish gate + `timer.tolerance=1.0` **DONE** (ccsbar `632bff1`, folded into TECH-4). **(e) #33 single-instance guard + (f) #42 login-item DONE** (ccsbar `c5ad408`): `SingleInstance.acquire()` refuses a second ccsbar — packaged `.app` enumerates `NSRunningApplication` by pid + `LSMultipleInstancesProhibited` (LaunchServices-enforced), bare `swift run` falls back to an advisory `flock` on `~/.clauth/ccsbar.lock` (pure `tryFlock` core, tested exclusivity+release+unavailable); `LoginItem` registers via `SMAppService.mainApp` on first launch (opt-out persisted; `.requiresApproval` counts as ON so the panel toggle doesn't snap back) surfaced as a "Start at login" toggle; README + `package_app.sh` retargeted to the auto-register reality. **Shared hardening folded into TECH-11's `Notifier`:** new `AppBundle.isMainApp` gates every system integration on the SPECIFIC app bundle id (`com.xingfanxia.ccsbar`), not "any bundle id" — `swift test` runs under the xctest tool (which HAS its own id), so the old generic guard would have fired `UNUserNotificationCenter`/`SMAppService` under test; `package_app.sh` fails the build on Info.plist↔`AppBundle.mainAppID` drift. Acceptance: `swift build` clean, `swift test` **39/39**, `rg NSRunningApplication|flock` + `SMAppService` present, README documents autostart. Reviewed (code-reviewer, opus): no CRITICAL/HIGH; **all 8 findings fixed** (LSUIElement "raise panel" overpromise→dedup-only; bundle-id drift guard; opt-out persist gated on availability; `LSMultipleInstancesProhibited`; `.requiresApproval` handling; stale package_app text; lockFD + guard-divergence comments). SMAppService/NSRunningApplication live behavior = operator acceptance (needs the packaged `.app`). **Rust LOC debt RESOLVED** (`a254a99`): extracted the pure daemon types (`ConfigOp`/`LastError`/`LastSwitch`/`SwitchBackoff` + `switch_backoff_ms`) to new `src/daemon/types.rs`, re-exported from mod — pure move, no behavior change, `mod.rs` **559→490** (under the gate), 533 tests green. |
| **TECH-12** — clauth doctor diagnostic + daemon.log rotation | ✅ done | `db48be7` | **`clauth doctor`** (new `src/doctor/` — pure `core.rs` + impure `mod.rs`): read-only health checks encoding the "why didn't it switch last night?" runbook — LaunchAgent state, code-sign identity (ad-hoc vs stable → the Always-Allow-persistence root cause), Keychain write-grant, daemon lock, control-socket `snapshot` round-trip, status.json freshness, CLI↔daemon version/schema skew. `run()` prints pass/fail + `process::exit`s non-zero on any FAIL. **Constraint-safe keychain check:** write-grant proven via a THROWAWAY `clauth-doctor-probe` item (delete-first self-heal → add → delete); the real `Claude Code-credentials` item is only ever read for presence (no `-w`, no secret, no prompt), NEVER written — respects "never touch the real item." All probes shell out via **argv (never a shell)** to absolute-path `/bin/launchctl` `/usr/bin/codesign` `/usr/bin/security`. **daemon.log rotation (#39):** launchd holds the log's `O_APPEND` fd and never reopens it, so rename orphans future writes — the only rotation that survives is an **in-place trim-to-tail** (`src/daemon/log_rotate.rs`: keep last ~1 MiB once >~5 MiB, same inode, `set_len`), called every ~5 min + at boot from the run loop (cheap stat no-ops under cap). Tests: 5 pure-core (freshness bands, skew, exit_code, `iso_to_ms` reader↔writer round-trip, render) + 3 log-trim (over-cap tail+line-boundary, under-cap no-op, absent no-op). Acceptance: `clauth --help \| grep -q doctor` ✅, `rg 'rotate' src/daemon` ✅, `cargo test doctor` 5 pass. **531 tests, clippy -D/fmt clean.** Reviewed: **security-reviewer 🟢 LOW** (throwaway isolation + argv safety confirmed; no state mutation; hero invariant safe; live-switch/prompt impossible) + **code-reviewer**; ALL findings fixed inline — **[HIGH]** freshness re-anchored to the 1s write cadence bounded by the ~60s watchdog (was wrongly keyed to `refresh_interval_ms`, up to 1h → a dead daemon could read PASS for 4h); **[MED]** `iso_to_ms` doc+test corrected to the real `+00:00` writer format (was fabricated `Z`) with a `epoch_secs_to_iso`↔`iso_to_ms` round-trip test; **[LOW]** `try_lock` distinguishes `WouldBlock` (Pass) from a real error (Warn, was false PASS); **[LOW]** header softened + probe delete-first; **[WARN/LOC]** doctor split into `core.rs` (170) + `mod.rs` (344), both < 500. Live `clauth doctor` on a real install = operator acceptance (touches launchctl/Keychain/socket). **Carry-forward debt (→ TECH-14):** `src/daemon/mod.rs` now **559 LOC** (>500) — TECH-9's boot-migrate + TECH-12's run-loop rotation wiring grew it; extract the run loop / consts or allowlist in the TECH-14 batch. |
| **TECH-10** — control-socket I/O hardening (daemon `4b80b9a` + Swift `b088e1e`) | ✅ **both halves** | `4b80b9a` + ccsbar `b088e1e` | **Daemon (Rust) half of the two-repo milestone.** `src/daemon/socket.rs`: each accepted stream now gets `set_read_timeout`/`set_write_timeout(2s)`, and `handle()` runs on a **short-lived per-connection thread** (`SocketHandles` derives `Clone` — four Arc bumps) so a slow read no longer serializes the accept loop and a `handle()` panic dies with its thread instead of killing the listener (#3/#7); the reader is bounded with `.take(64 KiB)` so a newline-less stream can't OOM the daemon. No thread cap **by design** (same-UID socket authority — a same-UID process can already read tokens; rationale encoded at the spawn site). Rust std ignores SIGPIPE → write-to-closed-peer returns Err (swallowed), no SO_NOSIGPIPE needed here (that's the Swift half). Test `hung_connection_read_timeout_does_not_block_accept_loop` (real UnixListener via `serve()` on a tempdir sock): a first connection sending no newline does **not** block a second connection's valid `switch` command — **5/5 deterministic**. Acceptance: `rg 'set_read_timeout\|set_write_timeout\|take\(' src/daemon/socket.rs` present; `cargo test read_timeout` 1 pass (NB: the plan's `cargo test 'socket.*timeout'` is a *regex* but cargo test filters by **substring** — the working invocation is `cargo test read_timeout` / `daemon::socket`). **523 tests, clippy -D/fmt clean.** Reviewed (concurrency pass, code-reviewer): no CRITICAL/HIGH/MEDIUM — no lock-inversion with the main drain (socket never nests locks; rank stack is thread-local), cheap Arc Clone, correct `WouldBlock` drop without spin; 2 LOW/NIT (timeout-comment precision, no-cap rationale) fixed inline. **Swift half DONE** (ccsbar `b088e1e`): `DaemonClient.sendRaw` sets SO_NOSIGPIPE (write-to-closed → EPIPE not a fatal signal) + SO_RCVTIMEO/SO_SNDTIMEO 2s, loops write() until the full payload is sent, loops read() to the newline/EOF bounded 1 MiB; `StatusModel` command methods run the blocking socket I/O off @MainActor via `Task.detached` then hop back to settle (call sites stay sync — SwiftUI views untouched). `swift build` clean; `--snapshot` still renders the panel. Beach-ball-absence with a mid-switch daemon = operator-manual. |
| **TECH-11** — command-outcome feedback + observability (ccsbar) | ✅ done | ccsbar `81f6cb9` | **Errors must be loud, and a rejection must not masquerade as absence.** Daemon commands return a three-way `CommandOutcome` (`.ok` / `.daemonError(code,message)` / `.unreachable`) instead of a `Bool`: `switchTo` shell-falls-back to `clauth <name>` **only** on `.unreachable`, NEVER on a daemon rejection (which would fire the daemon-absence path against a PRESENT daemon and hide the real error). Pure `classifyReply` (unit-tested: `ok:true`→ok, `ok:false`→daemonError w/ code+message, nil/non-object/unparseable→unreachable) is split from the socket I/O. `StatusModel.run` fires the blocking socket call off `@MainActor` (`Task.detached`) then on success runs a **settle ladder** (re-read at 0.6/1.2/2.4/4.8s until `generated_at` advances AND the expected effect holds → kills the double-tap-because-nothing-changed reflex), on error surfaces a loud auto-clearing banner. `DaemonStatus` gains additive `clauth_version` / `pending_switch` / `last_switch{from,to,at,trigger}` / `last_error{at,message}` (lenient `decodeIfPresent` — an older daemon omitting them decodes to nil, not a throw). `PanelView` renders a command-error banner + a quiet footer (last-switch line + soft version-skew badge); `ConfigView` disables its controls when the daemon's unreachable. `Notifier` (bundle-id-guarded `UNUserNotification`) posts on an UNATTENDED switch + a new auto-switch error — a **true no-op** in bare `swift run`/tests (no bundle id → guarded before `.current()`), operator-verifiable only in the packaged `.app`; `maybeNotify` baseline suppresses the launch burst + the user's own tap + re-notification. Snapshot adds `--snapshot=skew` (mismatched `clauth_version` → skew badge). Acceptance: `swift build` clean; `swift test` **35/35**; `--snapshot=skew` → `skew=9.9.9` in stderr + badge; acceptance greps (`lastCommandError`/`CommandOutcome`/`daemonError`/`UNUserNotification`/`versionSkew`) present. Reviewed (code-reviewer, opus): no CRITICAL/HIGH; **all 7 MEDIUM/LOW findings fixed** — **M1** `sendRaw`→`RawReply` so a reply-TIMEOUT (daemon busy w/ a ~3s Keychain rewrite past the 2s read deadline) classifies as `.daemonError(no_reply)`, NOT `.unreachable` (was the one residual double-switch = double-logout hole); **M2** testable `switchTo(_:send:)` seam + 2 tests pinning the no-fallback-on-rejection invariant (a regression adding `case .daemonError: shellClauth` would now fail a test); **M3** `pending_switch` doc softened (decoded for forward-compat, not yet surfaced — code/doc one-source-of-truth); **M4** `daemonReachable` gates on `liveness == .ok` (reactive) so a crashed daemon w/ a stale socket file still disables controls; **M5** `switchInFlight` guard blocks concurrent double-tap switches (tiles disable mid-switch); **M6** numeric-truthy `ok` tolerance; **M7** `sun_path` length guard (refuse rather than silently connect to a truncated wrong path). **Live browser-OAuth + real two-account switch = operator-manual acceptance** (a real switch logs out every running Claude Code process). |
| **TECH-9** — secret hygiene + durable-state perms/fsync + SECURITY.md sync | ✅ done | `c07fc16` | **Redaction (#14):** new `oauth::token_parse_error` replaces both 2xx token-body parse sites (refresh + code-exchange); a 2xx body that fails to deserialize still holds live access+refresh tokens, so the error now emits only serde **category + line/column** (NOT the serde Display `{e}`, which can echo an offending scalar), HTTP status, and body length — one redacted `e` feeds last_error/status.json/daemon.log. **Perms+durability (#5/#13/#15):** `write_and_rename(path,content,mode,durable)` is the single atomic writer — temp via `create_new`+`mode` (0o600 at open, never chmod-after), fsync data before rename + best-effort parent-dir fsync after when durable; `atomic_write`/`atomic_write_600` durable, `atomic_write_600_fast` non-durable **only** for the 1/s rebuildable status.json. Write/sync-failure cleanup removes **only a temp we opened** — a concurrent same-PID `create_new` AlreadyExists propagates without deleting the winner's in-flight temp (regression caught + fixed: `sync_credentials_unlocked_concurrent…` 5/5). 0o600 on settings.json (claude.rs/runtime.rs), config.toml (normal + drift-rewrite), credentials, status.json; `mkdir_700` for ~/.clauth + profiles; daemon boot tightens a looser tree → 0o700 **and** the launchd-created daemon.log → 0o600. **Docs:** SECURITY.md data-at-rest table + Fork-surfaces reconciled with code; keychain.rs comment corrected (argv same-UID/root visible incl. EDR exec logs; no-value `-w` prompts on the **tty**, not stdin). Tests: `token_parse_error_redacts_the_2xx_body` (value-free channel: no SECRETLEAK, reports status+len+`column`), `config_toml_is_0600_including_the_drift_rewrite_path` (NaN-threshold lever forces the Err-branch rewrite), `clauth_tree_migrated_to_0700_on_boot` (0o700 tree + 0o600 daemon.log). Acceptance all green: `rg 'has no stdin' src/keychain.rs` **gone**, `rg '0700\|Keychain\|clauthd.sock' SECURITY.md` present, `atomic_write_600` at all changed sites, `cargo test token_parse_error` pass. **522 tests, clippy -D/fmt clean.** Reviewed by security-reviewer (🟢 LOW, no CRITICAL/HIGH — redaction sound, no looser-than-0600 window, fsync ordering correct) + code-reviewer (⚠️ approve-with-warnings); all findings fixed inline. All via HomeSandbox + keychain-disabled — **no real ~/.clauth / ~/.claude / Keychain touched**. **Carry-forward debt (→ TECH-14 hygiene):** `src/daemon/mod.rs` 497→524 LOC (the +10-line boot `migrate_clauth_perms_700` pushed it just over the 500 gate) — small+cohesive, decompose or allowlist in the TECH-14 batch; oauth.rs (842)/profile.rs (1025)/runtime.rs (1106) are pre-existing over-budget, untouched-in-size here. |
| **TECH-8** — switch-event observability, backoff & upgrade restart | ✅ done | `df58463` | status.json gains **additive** (SCHEMA stays 1) `clauth_version` (`env!(CARGO_PKG_VERSION)`, always present → CLI↔daemon skew detectable) + `last_switch{from,to,at,trigger}` (every executed switch/wrap-off; null single-shot). Daemon **backoff+dedup** (#38): a persistently-failing switch (keychain denial/diverged/busy) no longer logs+retries ~1/s — failure log is deduped (emit only when target/reason changes) and retries are spaced by `switch_backoff_ms` (first 2 immediate → preserves TECH-6 "lands once idle"; then 2s→4s→8s… cap 60s), gated in the drain so scan_auto_switch cadence is untouched; success clears backoff + records last_switch. `signed-install.sh` now `launchctl kickstart -k gui/$UID/com.clauth.daemon` after re-signing (#37 — reinstall no longer leaves the stale inode deciding switches until next login). Tests: `switch_backoff_ms_grows_exponentially_and_caps`, `switch_failure_backoff_dedups_log_over_many_ticks` (**≤2 log emissions over 30 ticks** on a permanently-busy target), `successful_switch_records_last_switch_event`, status_json asserts version==0.7.1 + last_switch shape. Acceptance all green: `status --json \| jq -e '.clauth_version and …'` true, version==0.7.1, `grep kickstart` present, `cargo test switch_failure_backoff` pass. 519 tests, clippy -D/fmt clean. SCHEMA not bumped; ccsbar untouched (TECH-11 consumes). |
| **TECH-7** — cross-process state RMW atomicity (lost-update) | ✅ done | `3bc823e` | New `profile::update_app_state(delta)` reads-modifies-writes `AppState` **inside** the state flock (reload disk → apply narrow delta → write merged), so two writers touching different fields commute. Converted the cross-process racers to deltas: `finish_switch`/`switch_off` own only `active_profile`; fallback chain primitives own only `fallback_chain`/`wrap_off` (move recomputes position on the disk chain, not a stale swap); `capture_into_profile` (login) owns its profile entry + `auth_broken` and adopts active **only if disk has none** (never clobbers a concurrent switch). `set_threshold` untouched (writes config.toml). Daemon holds the flock **across** the switch AND the post-write `app_state_mtime()` read (re-entrant `with_state_lock`), closing the :354 self-adoption window. Tests (daemon_mod, HomeSandbox + keychain-disabled — no real creds): `cargo test lost_update` = switch from a stale snapshot **preserves an externally-appended profile**; `cargo test rmw` = daemon adopts its own write's mtime (no spurious reload) yet still reloads a later external write — **both pass**. 516 tests, clippy -D/fmt clean. Residual (documented): rename/delete/create_blank/reorder keep blind save_app_state (TUI-single-process dominant, outside the CLI-login/switch vector); TUI↔daemon chain-edit race is the R3 deferral. **Manual smoke (operator, touches real Keychain):** concurrent `clauth login C` + threshold-forced auto-switch leaves C in `status --json \| jq '.profiles'`. |
| **TECH-6** — pending_switch queue correctness + last_error | ✅ done | `84f8815` | `pending_switch` HashSet→ordered `VecDeque<PendingSwitchEntry{target,origin:User\|Scheduler,retry_until}>`. Fixes finding #4 (≡#6≡#9): (1) a busy/diverged/transient target is **re-queued** (120s TTL from the ack) instead of dropped after one attempt — a user tap during a fetch window no longer evaporates post-`{ok:true}`; (2) precedence is deterministic — `enqueue_pending_switch` clears queued Scheduler targets on a User request, `select_switch_winner` makes User outrank Scheduler (last-writer-wins) at drain time (both enqueue- and drain-side). Additive `status.json` `last_error{at,message}` records every skip/fail (**SCHEMA stays 1**; `clauth status --json \| jq -e 'has("last_error")'` → true, null in single-shot). `cargo test daemon::tests` **8 pass** incl. the 3 acceptance tests `busy_target_requeued_not_dropped` / `user_switch_outranks_same_tick_auto` / `user_switch_clears_queued_scheduler`; daemon_status_json pins the last_error shape. TUI drain + 2 construction sites adapted (type-forced; TUI carries only scheduler targets → behavior preserved). NOT bumped SCHEMA; scan_auto_switch stays level-triggered; leaf-drain-then-config-lock preserved. `rg HashSet src/daemon/mod.rs src/usage/scheduler.rs` shows no pending_switch HashSet. 514 tests, clippy -D/fmt clean. All via HomeSandbox + keychain-disabled — no real creds touched. |

## Next
- **ccsbar is built + shipped** at `~/projects/devtools/ccsbar`
  (github.com/xingfanxia/ccsbar). Runnable via `swift run` or the packaged
  `.app`; reads the R1–R6 + CBAR-2 contract, needs no more clauth changes for v1.
- **Operator acceptance for upstream (asked in uwuclxdy/clauth#1 + PR #19):**
  on current upstream `mommy` (or this fork post-`387607f`): (1) `clauth login
  <tmp>` end-to-end browser flow, (2) one `t` force-rotate on the minted
  profile — proves a `platform.claude.com`-minted pair refreshes at
  `api.anthropic.com` (upstream's one open question), (3) a `clauth <name>`
  switch with a bare `claude` picking up the new account. All three touch the
  real Keychain/browser → manual, per the security constraints.
- **Rotation-coherence: FIRED + FIXED 2026-07-07**, hardened through review
  round 2 (2026-07-11). Correctness = adopt + mirror-on-rotate; the
  ahead-of-expiry rotate is now the opt-in `preemptive_rotation` toggle —
  **enabled in the live profiles.toml on this machine** (upstream default off).
- **Contribute-back review round 2 DONE (2026-07-11):** uwuclxdy requested
  changes on both PRs; all findings addressed + replied.
  - [#24](https://github.com/uwuclxdy/clauth/pull/24) `95d6928`+`296216a`:
    `preemptive_rotation` opt-in (off by default, Config-tab row),
    README reframed around adoption, `apply_rotated_tokens_locked` captures
    mirror creds under the flock but runs the `security` shell-out AFTER
    release (never hold the state flock across a subprocess),
    `try_adopt_live_rotation` takes a `&RotationGuard` proof-of-lock param
    (scheduler acquires the guard at rotation-leg entry), adopt tests
    un-gated from cfg(macos), doc-link fix.
  - [#25](https://github.com/uwuclxdy/clauth/pull/25) `ef75932`+`43e0887`+`5f6edd6`:
    terminal-400 double-spend guard (`fresher_disk_pair` re-read under the
    guard; only an unchanged-store 400 quarantines), 403→Transient unless the
    body confirms `invalid_grant`, `ensure_installable` gate on EVERY switch
    entry (`switch_profile_noninteractive` takes ConfigHandle+refresher; TUI
    gets a flag-only synchronous refusal), and — pre-push self-review catch —
    the carry LIFTS a stale quarantine (`carry_external_rotation`), else a
    recovered account stays excluded forever.
  - [#26](https://github.com/uwuclxdy/clauth/pull/26) **APPROVED** (he fixes
    two stale doc lines himself at merge).
  - All ported to fork main (`be8b1de`, `583af22`, `8669847`, `3256f3c`) and
    deployed. Worktrees: `scratchpad/wt-coherence`, `scratchpad/wt-auth1`.
- **Contribute-back round 4 OPEN (2026-07-11):** issue
  [#29](https://github.com/uwuclxdy/clauth/issues/29) (divergence UX) + PR
  [#30](https://github.com/uwuclxdy/clauth/pull/30) (retry-after:0 backoff)
  + PR [#31](https://github.com/uwuclxdy/clauth/pull/31) (non-blocking
  divergence, fixes #29) — both cut from upstream/mommy v0.9.0. Worktrees:
  `scratchpad/wt-up-backoff`, `scratchpad/wt-up-diverge` (session tmp).
- **Issue #27 ANSWERED YES (2026-07-11):** uwuclxdy wants the minimal daemon
  (usage-refresh + auto-switch + logging), fears conflicts/TUI-only flows;
  design reply posted (no new mutation paths, refuse-and-log on anything
  interactive, timestamped logs). **Daemon PR (and TECH-15 riding on it) is
  now actionable — after #24/#25 land** (it builds on their machinery).
- **INCIDENT 2026-07-10 evening (attributed via TECH-15 timestamps):** ax-main
  (11:15Z) and ax-backup (17:06Z) quarantined by the classic double-spend
  (CC rotated the shared chain first; the old binary had no carry guard —
  exactly the #25 round-2 fix); a TUI switch toward ax-cl at 19:49Z
  half-landed (live symlink moved to ax-cl, state kept active=ax-main) →
  phantom "DIFFERENT account" divergence wedged auto-switch all evening; the
  open TUI's whole-state blind saves (the known TECH-7 residual) kept
  resurrecting stale `auth_broken` entries (all three flagged at once with no
  daemon-side transition line). Resolved overnight by AX: `clauth login`
  re-auths for ax-cl (09:12Z) + ax-backup (10:04Z), daemon switched to ax-cl
  (09:29Z), state/symlink consistent again. Remaining: **ax-main stays
  quarantined** — its chain's live continuation was lost when the Keychain
  was overwritten by later switches; needs a manual `clauth login ax-main`.
  Follow-up candidates: convert the TUI's remaining blind `save_app_state`
  writers to `update_app_state` deltas (TECH-7 residual is no longer
  theoretical), and snapshot-before-walk-away so a flagged active's live pair
  isn't orphaned by the AUTH-4 switch.
- **INCIDENT 2026-07-11 afternoon (same family, new episode):** AX re-logged
  Claude Code itself into ax-main (~15:00 PDT) while clauth's state still said
  active=ax-cl → every TUI open got blocked by the DIVERGENCE modal ("live ≠
  ax-cl", all three options wrong for this case). Root causes underneath:
  (1) ax-cl's chain had already died at 17:07Z — the 4.5-min preemptive lead
  (interval*3, 90 s cadence) fired INSIDE Claude Code's own ~5-min refresh
  margin, lost the race, 400d with the store unchanged → correct quarantine,
  but the fresh chain lived only in the unreadable Keychain and was orphaned.
  (2) No path recognizes "the diverged live login belongs to a SIBLING
  profile" (candidate improvement — the divergence flow could offer "this is
  ax-main; switch bookkeeping to it"). Resolution (agent, 15:12–15:25 PDT):
  killed the stale TUIs, corrected `active_profile` → ax-main (bookkeeping
  catch-up — CC was already there; no Keychain/live-file writes), hand-adopted
  CC's fresher same-chain pair into ax-main's store (identity-verified against
  the anchor, atomic rename — the mechanical equivalent of
  `try_adopt_live_rotation`, which wouldn't have run until the 18:25 PDT
  rotation window), relaunched a fresh TUI in the tmux pane. Verified: no
  divergence/deferral lines post-restart, active=ax-main auth ok.
- **FIX for the "re-login every other day" instability (`e47f723`, deployed
  2026-07-11 15:23 PDT):** `ACTIVE_ROTATE_LEAD_FLOOR_MS` 3 min → **15 min**
  (fork divergence from upstream's #24-settled 3-min floor, justified by the
  17:07Z race loss above). The whole death spiral was: lead inside CC's
  margin → lose race → 400 → quarantine → AUTH-4 walks away → switch
  overwrites Keychain → the chain's only fresh copy gone → re-login. A 15-min
  lead makes clauth decisively the chain's last writer; adopt + carry remain
  the backstops. Consider proposing upstream later with this incident as
  evidence (their toggle default-off makes it lower-stakes there).
  Still open for AX: `clauth login ax-cl` to revive ax-cl (chain dead for
  real); ax-main/ax-backup healthy.
- **RateLimited mystery SOLVED (2026-07-11 ~15:35 PDT) — R3 priority UP:**
  ax-main + ax-backup pinned at `RateLimited` while CC's own `/usage` worked
  and ax-cl (freshly re-logged) fetched 200 in the SAME tick from the SAME
  IP. Probe confirmed: per-ACCOUNT 429 (`retry-after: 0`) on
  `/api/oauth/usage`. Cause: **every open TUI runs its own full scheduler**
  (`tui/app.rs:1553`, the documented R3 deferral) — daemon + two stale TUIs =
  2–3× the 90 s cadence per account for days, draining Anthropic's
  per-account polling quota; ax-cl escaped because its dead token wasn't
  consuming quota during the heavy window. Killed the extra TUIs (single
  poller = the daemon now); buckets refill passively. **R3 (TUI defers to a
  running daemon and reads status.json instead of polling) is no longer just
  a race concern — it has a hard API-quota justification now.** Also worth
  citing in the upstream daemon PR (#27): shipping a daemon without R3 gives
  every daemon+TUI user this same quota burn. Note: ccsbar numbers during a
  429 window are CACHED (stale reset times/utilization) — the "ax-main resets
  in 55m vs CC says 5:50pm" confusion was staleness, not a wrong account.
- **DIVERGE-1 SHIPPED (2026-07-11 ~16:00 PDT, `0e17845`):** the divergence
  flow no longer blocks the TUI (1Hz poll + startup raise a non-blocking
  banner; <kbd>d</kbd> opens the resolver; switch-shaped actions still raise
  it), the resolver identifies the live login's OWNER
  (`identify_live_login_owner`: exact token match, else `~/.claude.json`
  uuid vs the profile's anchor) and leads with "switch to '<owner>'"
  (reusing AdoptDivergence), and the DAEMON follows CC to a sibling
  unattended when ownership is proven at the adopt bar (token equality or
  network-verified uuid vs anchor; `follow_live_login` tick step) — the fix
  for both wedges this week. Adopt refusal logs fingerprint-deduped.
  Upstream: issue #29 + PR [#31](https://github.com/uwuclxdy/clauth/pull/31)
  (TUI part, token-tier only — the uuid tier follows once #24's anchors
  land). ccsbar: nothing to change (it reads status.json; benefits
  automatically from the daemon self-heal).
- **RATE-1: the "pinned RateLimited forever" root cause (`b9575ac`,
  deployed 2026-07-11 15:58 PDT, recovery confirmed 16:02):** the usage
  endpoint answers EVERY 429 with `retry-after: 0` and its sliding window
  counts rejected requests; `next_slot_deferral` honored the 0 verbatim
  (the exponential ladder only engaged for a MISSING header), so once an
  account crossed the limit (daemon + two stale TUIs = 3× polling for a
  day — every open TUI runs a full scheduler, the R3 deferral) it re-polled
  at cadence and never drained. Fix: deferral = max(hint, interval +
  ladder) on a 429. Live proof: ax-main/ax-backup pinned RateLimited for
  ~50 min through restarts, went Fresh 4 minutes after the deploy.
  Upstream: same bug verbatim in v0.9.0 — PR [#30](https://github.com/uwuclxdy/clauth/pull/30).
  Also noted there: v0.9.0's clean-base `runtime::tests` fail-alive flake is
  hot under parallel load (2/3 full-suite runs on this machine; serial =
  green) — hardening for two of the tests rides #24/#25. **Root-cause lead
  (port-agent finding, 2026-07-11):** the tests use hardcoded fake PIDs
  (11111/22222/33333, 12345) that can collide with REAL processes on a busy
  machine, plus same-process flock re-acquisition in `is_session_alive`'s
  probe — explains both the load-correlation and the random-member-per-run
  signature. Proper fix (follow-up, not the settled-poll bandage): unique
  per-test fake PIDs guaranteed dead, or a probe seam. Applies to fork AND
  upstream.
- **Deploy shape CHANGED (2026-07-10 ~21:31 PDT):** the daemon now runs as the
  `com.clauth.daemon` LaunchAgent (KeepAlive — `pkill` respawns it; that IS
  the restart path after `cargo install`). Memory updated.
- **Follow-ups:** S7 (Settings window + Sparkle auto-update + Developer-ID signing/
  notarization + Homebrew cask), daemon R3 (TUI↔daemon scheduler coordination).
- **Upstream v0.9.0 MERGED into fork main (2026-07-11, `1a30a04`):** 25
  upstream commits — tokens-dashboard TUI wave, `start [--isolated]`
  semantics (shared delegate default, isolated opt-in), string polish
  ("login" verb everywhere; feature-coverage pins "Log in an account"),
  `http_error()` body-capping in oauth, and our own #26 (weekly cap)
  landing upstream. 9 conflict files resolved keep-fork-features /
  adopt-upstream-polish (TECH-9 hardened writes kept with lowercase
  messages; RefreshError split kept, both arms now route through
  `http_error()`; fork subcommands + `--new` kept in usage/completions;
  DivergenceAction kept in modals; README keeps fork bullets with the
  post-#26 last-resort wording). 773 tests green. Installed (`clauth
  0.9.0`) + daemon respawned on it; all three profiles auth=ok/Fresh.
- **PRs #24/#25 updated to v0.9.0 (2026-07-11, `2c0cc53`/`c311201`):** the
  release made both CONFLICTING/DIRTY; merged `mommy` into each branch
  (merge, not rebase — review anchors preserved, matches upstream's own
  merge-commit style). #24: README + usage re-export union (647 green).
  #25: RefreshError × http_error reconciliation + README union (663
  green). Both MERGEABLE again, CI green/pending; update comments posted.
  #30/#31 were already cut from v0.9.0 — untouched, still CLEAN. The 25
  new upstream commits contain NO independent fix for #30's backoff hole
  or #31's divergence UX — both PRs remain necessary.
- **INCIDENT 2026-07-12 00:08Z — cross-profile credential copy (root-caused
  same hour via TECH-15 timestamps + token-hash forensics):** post-23:31Z
  auto-switch to ax-backup, a running claude's own refresh wrote its
  (ax-main's) rotated pair back over the live Keychain+mirror; an
  identity-blind live→store capture then copied that foreign pair into
  ax-backup's store (its anchor untouched since Jul 10 — store/anchor
  split-brain), leaving ax-main+ax-backup polling ONE account → the
  "RateLimited 还没解决" pin AX screenshotted (identical windows to the
  second across both rows was the tell). The daemon's own 00:08:03 follow +
  00:08:19 switch were identity-correct (verified against every mtime/hash).
  Remediation: CAP-1 (below) + **AX must `clauth login ax-backup`** to
  re-mint its real account (old chain unrecoverable — overwritten).
  **DIRECTION CORRECTED 2026-07-12 (post-relogin /profile forensics):** the
  double-polled account is xiaxbackup@gmail.com — so the profile that LOST
  its own account is **ax-main** (its real account = the orphan; its
  pre-incident anchor d0ab…, written Jul 11 09:29Z, was truthful), not
  ax-backup. AX's 02:24Z `clauth login ax-backup` browser re-login therefore
  landed CORRECTLY (browser session was the backup account) — but left
  ax-main still holding backup-account credentials. Outstanding manual step:
  browser → log into the MAIN account → `clauth login ax-main`.
- **CAP-1 shipped (2026-07-12, `9f43ed1`):** (1) snapshot_active_credentials
  decides and writes on ONE read (closes the classify→re-read capture
  window); (2) every sanctioned capture moves the identity anchor with the
  store (claude.json hint) or DROPS a stale anchor (hourly /profile backfill
  re-proves); (3) daemon duplicate-login tripwire (memoized logline naming
  the pair — fired on the live box at 00:33:54Z naming ax-main/ax-backup
  within seconds of deploy); (4) follow_live_login logline whitespace fix.
  787 tests (+6). Upstream: the capture TOCTOU + anchor coherence hole is
  upstream code too, but the anchor layer arrives with PR #24 — raise after
  #24 lands (fold into the daemon PR or its own).
- **TOK-1..5 tokens feed shipped (2026-07-12):** clauth `e047779` — daemon
  publishes machine-wide `~/.clauth/tokens.json` (schema 1; today/week/month/
  lifetime, raw four-bucket splits + in_out headline + per-model rows capped
  8 with "others" fold + LiteLLM cost with floor flags; pure
  tokens::build_tokens_snapshot shared TUI/daemon; worker reuses
  tokens::spawn+pricing::spawn, cfg(not(test))-gated; contract pinned in
  docs/ccsbar/DESIGN.md §3 + SECURITY.md). ccsbar `176a730` — TokensStrip
  above StatusStrip (collapsed one-liner "Tokens today 41.2M · $12.40",
  hover expands in-place period table + top-3 models; neutral palette; own
  schema gate + additive decode; reads inside the existing 4s poll on its
  own mtime gate; 167 tests; light/dark snapshots verified). Adversarial
  review: clauth 0 findings; ccsbar 1 low (abbreviateCount 4-sig-fig band
  crossing) fixed + pinned. Live verify: tokens.json in 17s post-deploy
  (today $169 / lifetime 93.7B total, month floor correct), ccsbar decode
  clean, TUI relaunched on the CAP-1 binary.
- **TOK-6 display-basis fix (2026-07-12, ccsbar):** AX flagged the strip as
  "severely wrong" ("today 1.03M · $319" — ~$310/MTok). Root cause: TOK-4
  headlined `in_out` (cache-EXCLUDED) next to a cost that always prices
  cache; with 1h-TTL caching, cache_read was 306.7M of that day's 316M true
  total (in_out = 0.34%). Fix in ccsbar only (feed already carried `total`):
  strip now renders cache-inclusive `displayTokens` = max(total, in_out)
  (per-model rows sum the four buckets client-side, saturating), decorated
  `N+` when `complete`/`split_complete` is false, mirroring `"$X+"`; models
  basis keys on the same count. DESIGN.md §tokens.json field semantics
  updated in the same batch. Feed and schema unchanged.
- **CAP-2 anchor provenance + same-account tripwire (2026-07-12):** the
  incident's SECOND recurrence vector, found live: (a) `clauth login`
  probed the fresh token's account uuid and wrote the anchor, but the
  capture then re-anchored from CC's live-login hint — an UNRELATED
  account (whatever was live during the login) — clobbering the
  authoritative probe (observed: ax-backup's anchor read ax-cl's uuid
  minutes after a correct re-login). Fix: `CaptureIdentity` provenance on
  `CaptureSnapshot` — `Known(uuid)` (browser-mint, probed; the uuid rides
  the snapshot and wins), `LiveLogin` (bytes came from live; hint stays
  correct), `Unknown` (TUI re-login flow, unprobed; drop the stale anchor,
  first-fetch backfill seeds truth). (b) the CAP-1 tripwire only paired
  byte-identical tokens — blind to two DIFFERENT chains minted for one
  account (this incident's actual end state); added
  `duplicate_account_pairs` (anchor-equality, whitespace-blank guarded)
  with its own logline, memoized jointly, token-pairs filtered out to
  avoid double reporting. 791 tests (+4: probed-anchor-beats-live-hint,
  unprobed-drop, fresh-capture-Known, anchor-pair detector). Polluted live
  anchors (ax-main stale d0ab, ax-backup wrong 14d0) dropped by hand —
  backfill re-seeds truthfully. Upstream: rides the same future CAP PR as
  CAP-1 (anchor layer lands with #24).
- **ccu shipped (2026-07-12, github.com/xingfanxia/ccu `f856673`):** AX's
  mobile-mosh usage ask pivoted from "responsive TUI layout" to a
  standalone read-only terminal viewer over the daemon's published
  status.json + tokens.json (zero upstream surface — the third client of
  the publish-render pattern after ccsbar). Rust/ratatui, narrow-first
  (~44-col phone portrait over mosh; bars absorb width, tier drops before
  the state badge, <24 cols drops bars), per-account windows + countdowns
  + badges + forecast + machine tokens (TOK-6 cache-inclusive rule
  honored from day one), mtime-gated auto-refresh, schema-gated additive
  decode, formatter pins shared with ccsbar. 21 tests; installed as
  `ccu`; live-verified at 44 cols against the real feeds.
- **CAP-2 + ccu adversarial review (2026-07-12, workflow, 11/11 findings
  confirmed and ALL fixed same-day):** clauth — anchor-file reads lifted
  out of the config lock (duplicate_account_pairs now takes a
  names-snapshot; slow disk can't hold rank-Config against the fetchers),
  two-blank-anchor fixture actually locks the is_empty guard,
  account_only_pairs extracted pure + tested (token pair reports once).
  ccu — `width - 24` usize-underflow PANIC on malformed status below 24
  cols (debug profile) fixed by routing EVERY free-text row through
  trunc(.., width); stale/missing/schema-gap/forecast/tokens rows all
  width-capped; header drops the version tag before overflowing; panic
  hook restores raw-mode/alt-screen before printing; scroll clamp
  saturates instead of `as u16` wrap. Width-invariant test now sweeps all
  5 feed states × widths 20-120. clauth 792 tests (+1), ccu 21.
- **CDX-0 Codex-support feasibility researched, build DEFERRED
  (2026-07-12):** full research + staged design preserved in
  `docs/codex-support/feasibility.md` — start there before any CDX work
  (zero re-research needed). Verdict: feasible; 5h/7d windows + auth.json
  + CODEX_HOME + session-JSONL token_count map 1:1 onto clauth concepts,
  BUT hot-swap is structurally impossible on Codex (in-memory AuthManager
  + account_id-guarded reload) → CDX auto-switch must be session-boundary
  semantics. Usage source is passive-only (JSONL rate_limits snapshots;
  backend `wham/usage` polling is the community-documented ToS-risk path,
  excluded by design). Refresh rotation harsher than Anthropic
  (single-use + `refresh_token_reused` detection) but the existing
  adopt/auth_broken discipline transfers. Proposed CDX-1..4 milestones +
  non-goals in the doc. Codex source read at openai/codex `9e552e9`.
  *Follow-up (2026-07-12, later session): the usage-DISPLAY slice shipped
  in **ccu** (`src/codex.rs`, direct session-JSONL tail-reads) — AX chose
  to keep clauth Claude-scoped; all CDX milestones here stay deferred.
  See the status note atop `docs/codex-support/feasibility.md`.*
- **RESOLUTION (2026-07-12):** AX ran the browser `clauth login ax-main`
  (claude.com on the MAIN account) — probe-anchored by the CAP-2 binary
  back to its pre-incident uuid (d0ab…). All three anchors distinct,
  windows diverged (ax-main fresh at 5h=1%), both tripwires silent. The
  credential triple-swap saga is closed.
- **CAP-3 account-email surfacing + same-account login block
  (2026-07-12, AX ask):** "显示每个profile实际linked的account email …
  加一层dedup … block 防止误添加". (a) the `/profile` probe + backfill now
  carry `email` beside the uuid (`AccountIdentity`,
  `ACCOUNT_EMAIL_CACHE_FILE` — written/dropped in lockstep with the uuid
  anchor by login captures, `refresh_account_anchor` (claude.json
  `emailAddress` hint), and the hourly seed). (b) `clauth login` REFUSES
  (pre-write, side-effect-free — the minted tokens are discarded) when
  the fresh account is already held by a SIBLING profile:
  `actions::account_owner` (uuid authoritative, cached-email
  case-insensitive fallback); re-login of the holder itself stays a
  refresh. (c) surfaced on every reader: `status.json` additive
  `account_email` (schema 1; DESIGN.md field doc + consumers note), TUI
  Setup tab `account` row (cursor-profile cache read in build_snap),
  ccsbar detail card + row tooltip (fixture + decode pins), ccu profile
  line, and the same-account tripwire logline now names the email. Live
  caches primed from authoritative probes (ax-cl=x@computelabs.ai,
  ax-main=xingfanxia@gmail.com, ax-backup=xiaxbackup@gmail.com); all
  three surfaces verified live. clauth 793 / ccsbar 174 / ccu 21 tests.
- **CAP-3 adversarial review (2026-07-12, workflow, 5/5 findings confirmed
  and ALL fixed same-day):** (1) double-hold wedge — account_owner now
  treats "the minted account == exclude's own anchor" as a refresh (never
  refused), so recovering a double-hold isn't circular; a THIRD profile
  is still refused (test matrix extended). (2) drop_account_anchor drops
  email BEFORE uuid (a torn drop leaves the harmless uuid-only state) and
  seed_identity_anchor drops a predating email when seeding a fresh uuid
  (torn-drop survivor can't pair with a new account). (3)
  refresh_account_anchor reads BOTH halves from one ~/.claude.json read
  (live_oauth_account_pair) — two reads could straddle a rewrite. (4)
  status.json gates account_email on is_oauth(), matching the TUI (an
  OAuth→API conversion keeps the cached anchor; stale email must not
  surface — ccsbar/ccu inherit the fix through the feed). (5) ccsbar
  VoiceOver row label now carries the email (tooltip parity). clauth 795
  tests (+2), ccsbar 174.

- **CAP-3 follow-up: email on Overview + Usage pages, ccsbar row caption
  (2026-07-11, AX ask):** AX wanted the linked email visible on the TUI
  Overview and Usage pages and in ccsbar's account list (like ccu).
  Usage detail header gains an `account` row between `plan` and `status`
  (rendered from `HeaderState.account_email` — no IO in the line
  builder). Overview gains an `email` column carved ONLY from the width
  left over after every upstream column is at full size (never shrinks a
  column; layouts without it stay bit-identical to upstream for rebase
  friendliness); ccsbar AccountRow gains a dim caption under the name.
  Two traps found live: `section_box` pads 1 col each side (pane 50 →
  inner 46), and upstream's `fixed_overview_width` omits TIMER_SLOT(5) —
  the first carve consumed that phantom spare and clipped the 5h column;
  fixed by carving from real spare (`total - base - TIMER_SLOT`) while
  ungranted layouts keep upstream's verbatim gap math. Upstream quirk
  observed, left untouched: `fixed_split` silently drops one char with
  no ellipsis when a string is exactly width+1 ("Max 20x" → "Max 20").
  Grant threshold for the live roster ≈ pane 57; narrower panes read the
  email on the Usage tab (any width) or ccu. Adversarial review
  (workflow, 6/6 confirmed findings fixed, 1 refuted): em-dash
  placeholder now means "OAuth anchor pending" only (api-key/provider
  rows render blank — every other surface omits the field); per-frame
  N-file disk IO replaced by a tick-gated cache (`App::overview_emails`,
  ~2s reload); DESIGN.md consumer list updated; tautological em-dash
  test rewritten as count-deltas vs the no-column layout; grant
  boundary (51→0, 52→ACCOUNT_MIN), ACCOUNT_MAX cap + gap overflow, and
  rendered-row-fits-width all pinned. clauth 799 tests (+4), ccsbar 174.

- **UPSTREAM GREEN LIGHT + contribute-back batch (2026-07-12):** upstream
  merged ALL five open PRs (#24 #25 #26 #30 #31), released v0.9.1, and
  uwuclxdy answered #27: **yes to (a) daemon loop + (b) status feed, hold
  (c) socket/fallback_config**, with conditions — spec-first review of the
  status.json contract, TUI-stands-down-when-daemon-lives (R3) before the
  daemon PR lands, keep `.agent/`/`docs/ccsbar/`/`media/` out of the diff.
  His review also flagged two fork bugs + one race; ALL addressed this
  session: (1) socket `snapshot` embedded pretty JSON in the
  newline-delimited protocol (line-readers got truncated JSON) → reply
  re-serialized compact, test fixture now real to_vec_pretty (6c675dc);
  (2) SECURITY.md still described pre-#21 `security -w` argv → stdin
  `security -i` (b74fad5); (3) pre-RotationGuard token-snapshot race in
  ensure_installable (stale snapshot spends a sibling-rotated single-use
  token → healthy login wrongly quarantined) → refresh leg split into
  gate_under_guard, no token args, authoritative re-read under the guard
  (c54878a; upstream port = PR #34). Plus two upstream PRs from this
  session's own finds, both red-test-proven on mommy: **#32** fixed_split
  off-by-one (value exactly width+1 silently drops a char, no ellipsis —
  fork ca85d4e) and **#33** overview gap widening from the
  TIMER_SLOT-undercounted base overflows the row and clips the 5h "%"
  (fork b49810b; supersedes the "left untouched" note in the CAP-3
  follow-up row above). **Spec posted on #27** (status.json field table +
  additive-evolution rule + `status --json` single-shot; account_email
  explicitly flagged fork-only) — daemon PR waits on his contract review.
  Deployed: daemon PID 2383 + TUI on the 4-fix binary. clauth 807 tests.
  **NEXT MILESTONE — R3 (TUI stands down when a daemon is live):** probe
  singleton flock + status.json freshness, stop own fetch scheduling,
  re-arm on daemon death; precondition for the daemon PR, also kills the
  stale-TUI 429 amplification (RateLimited mystery row above). **FORK
  SYNC NEEDED:** upstream/mommy has 33 commits the fork lacks (quarantine
  polling backoff e7387de/5a9648f, divergence-banner-via-system-banner
  bc521cc, gate `flagged`-overrides-clock semantics the fork's
  ensure_installable does NOT yet have, v0.9.1) — schedule a merge/rebase
  before the daemon PR so the port starts from a synced base.

- **2026-07-12 (evening) — SYNC-1 + R3 + daemon PR batch.** All three
  blockers from the previous row cleared in order. **SYNC-1** (387865d):
  merged upstream/mommy v0.9.1 — 18 conflicted files. Kept fork-side:
  daemon module + queue machinery, CAP identity layer, TECH-7
  update_app_state deltas, TECH-15 logline, two-tier
  identify_live_login_owner, 15-min ACTIVE_ROTATE_LEAD_FLOOR (900_000 —
  upstream still 180_000; incident rationale comment restored verbatim).
  Took upstream: gate `flagged`-overrides-clock (folded into the fork's
  PR-#34 re-read-under-guard structure — fork now runs the merged form),
  quarantine polling backoff, rotation_bail_context (429 context survives
  a failed unmask), adopt-on-any-refresh-failure, active-profile gate
  exemption in CLI/noninteractive switch, config-sourced expiry in the
  rotate leg, divergence-banner-via-system-banner. collect_tokens is now
  ONE definition in scheduler.rs taking &AppConfig (upstream had grown a
  second copy in app.rs; fork's daemon copy missed the new auth_broken
  field — the compile caught it). 831→839 tests.
  **R3** (6dce76b): daemon::probe::daemon_is_live = singleton flock HELD
  + status.json generated_at < 30s (window rides above the 20s keychain
  kill deadline). scheduler standdown_probe gate — TUI true / daemon
  false (self-probe would deadlock on its own flock); standdown_tick
  hydrates stores from the daemon's disk caches via try_seed_cache,
  republishes countdowns off cache mtimes, drains forced names + clears
  Queued marks, skips fetch/rotate/scan. finish_bootstrap's startup
  one-shot also defers to a live daemon. 8 new tests.
  **Nits** (237d9b9): watchdog spawn .ok() → loud logline; shutting_down
  documented as deliberately never-set by the daemon.
  **Daemon PR opened as DRAFT** (branch pr/daemon-status-feed, worktree
  port off mommy b530b54): (a)+(b) exactly — daemon loop + status.json +
  status --json + stand-down + logline; NO socket / tokens.json /
  follow_live_login / config ops / forecast / account_email /
  last_switch / last_error / clauth_version (all listed in the PR body as
  additive candidates); drain adapted to upstream's HashSet
  PendingSwitch with the 120s TTL moved into SwitchBackoff.retry_until;
  docs/daemon.md carries the #27 contract verbatim. 733 tests on the
  branch. Draft honors his spec-first order — flips to ready on contract
  sign-off. PRs #32/#33/#34 checked: zero comments yet, all three already
  based on mommy tip (no rebase needed).

- **2026-07-12 (late evening) — daemon PR #35 opened (draft) + review round.**
  Two independent adversarial reviews on the port before opening: 1 HIGH
  (profile_json claimed mcp/daemon/CLI share one shape but the port left
  mcp's private copies — fork had already consolidated; hunk carried
  over) + 3 LOW (orphaned test comments; wrong rank-rationale comment in
  write_status; backoff gate ignored retry_until at the window's edge —
  fixed on BOTH sides, fork ad3e287) + doc acknowledgements (30s probe
  window vs 60s watchdog overlap; third_party null-when-never-probed).
  Two live-deploy catches, both fixed fork (8a1a9d8, 6dc3f5c) + PR: the
  stand-down transition logline painted over the ratatui screen (now
  gated off live terminals), and bootstrap's cache-due Queued pre-marks
  spun forever under stand-down (standdown_tick now sweeps all Queued
  marks; in-flight kinds survive). **PR #35 (DRAFT)**
  https://github.com/uwuclxdy/clauth/pull/35 — flips to ready on his #27
  contract sign-off. Deployed: daemon PID 134 + TUI on 6dc3f5c; live
  stand-down verified (3 rows daemon-fed countdowns, no spinner, no
  bleed). Fork 840 tests / PR branch 735.
  **OPERATOR ACTION NEEDED (CAP-1 tripwire, daemon.log 13:57Z):**
  'ax-cl' and 'ax-main' are anchored to the SAME ACCOUNT
  (xingfanxia@gmail.com) under different tokens — a wrong-account
  re-login; they double-poll one account. Recovery is a manual browser
  `clauth login <the-one-that-lost-its-account>` with the browser logged
  into that account (AX-manual per security constraints).

- **2026-07-12 (night) — ax-main auth_broken episode: forensics + the
  display fix trio.** AX's relogin healed both accounts (14:53Z fresh
  chain, flag cleared, daemon switched back). Forensics: the flag was
  set by a NON-daemon process (no transition line in daemon.log after
  07-10; mark_auth_broken always logs transitions) — the only other
  poller alive was last night's OLD-binary TUI, whose loglines paint
  into the ratatui screen and vanish. Mechanism: after the 12:50Z switch
  to ax-main its single-use chain had three players (daemon + old TUI +
  live claude); a losing racer spent a superseded token → 400 →
  correctly quarantined; the 13:07Z CC login into ax-cl orphaned
  ax-main's winning chain (lived only in CC, mirror overwritten) → only
  a browser relogin could recover. All three enabling holes were ALREADY
  closed by today's deploy (R3 single-scheduler, flagged gate, TECH-7
  narrow writes). NEW finding fixed everywhere: a dead login DISPLAYED
  as "RateLimited" (the 429 mask outranked the broken state on every
  surface) — ccsbar row badge is now a worded danger pill + fetch-status
  text suppressed while broken (3be62b5); ccu badge order broken-first +
  regression test (ead7b34); clauth TUI gained a system-banner tier
  naming the broken member + recovery command, previously INVISIBLE
  outside a switch toast (d8ad828). clauth 841 tests / ccu 22 / ccsbar
  suites green; all deployed (daemon PID 13454). The stale-anchor scare
  ("ax-cl+ax-main same account") was this morning's pre-guard label
  residue, not real token mixing — all three anchors verified distinct
  and correct after AX's relogins.

- **2026-07-12 (evening) — SYNC-2 + the RateLimited-freshness root fix +
  weekly 98% soft line + delete-ghost HIGH.** Merged upstream v0.9.1 +
  api-key login/profile delete (`3121836`; 6 conflicts — kept the fork's
  spare-based overview gap math + CAP-2/3 identity grafted into
  upstream's refactored `cmd_login`/`run_oauth_browser` shape, `--new`
  rides upstream's `LoginArgs`; union-merged completions/README/help).
  Adversarial 4-agent review of the merge confirmed a real HIGH:
  upstream's new `clauth delete` × the daemon's queued switch target —
  the ghost drain tore down the live credentials link BEFORE the
  too-late existence check and the retry nulled the ACTIVE profile's
  stored credentials. Fixed with two guards (`switch_profile` validates
  existence first — every caller; drain drops vanished targets, logged)
  in `5707fe4`, ported to PR #35. **AX's stuck-RateLimited complaint
  root-caused**: the 429 back-off ladder (calibrated 2026-07-11 against
  the since-fixed double-poll pathology) parked the ACTIVE profile in a
  15-min blind slot while the endpoint had already recovered (probe:
  ax-cl 200 while daemon waited; the account's limiter is dominated by
  the running claude's own traffic, so clauth's poll neither pins nor
  drains it). Fix `841e2bd`: active profile's ladder caps at 2× cadence
  (~3 min), idle keeps the full drain ladder, real retry-after still
  wins. **AX feature ask shipped** (`4ba6b38`): weekly (7d) window now
  gates at a 98% SOFT line instead of the 100% hard cap, applied to both
  walk directions (either-window switch trigger + candidate rejection +
  recovery + resume captions) — `WEEKLY_EXHAUST_PCT` in fallback.rs, a
  const by design (protects the chain, not per-member taste). Verified
  live: ax-cl hit 7d 98.0% minutes after deploy and is correctly
  weekly-excluded from the walk. Also closed the ledger's runtime
  fake-PID flake follow-up (`6db2733`, fail-alive poll windows 2s→10s).
  Upstream: fork main pushed (`a7d4ba4`), PR #34 rebased onto v0.9.1
  (725 green), PR #35 rebased clean + ghost-guard + flake commits (757
  green, comment posted explaining the delete interaction). Deployed:
  binary installed, daemon restarted (all three accounts Fresh
  post-deploy), TUI relaunched. Follow-up candidates for upstream after
  live soak: the active ladder cap + the weekly soft line (both change
  shared scheduler/fallback semantics uwuclxdy may want to weigh in on).

- **2026-07-12 (late evening) — weekly line configurable + free-typed
  thresholds (AX ask).** clauth: `WEEKLY_EXHAUST_PCT` const →
  `AppState.weekly_switch_threshold` (additive in profiles.toml, None →
  98 default, out-of-band values clamp back via
  `weekly_switch_threshold_pct()`), threaded through every fallback
  predicate + `ChainSnapshot.weekly_pct` + `scan_recovery`; new
  `set_weekly_threshold` socket op (validated 50..=100 on socket AND
  write-side op, TECH-7 persist) + `ConfigOp::SetWeeklyThreshold`;
  status.json publishes `weekly_switch_threshold`; TUI Config tab gains
  a "weekly limit" row (presets 90/95/98/100 + ⏎ custom editor 50–100,
  mirrors the refresh-row grammar). 865 tests, clippy clean
  (`03d231f`). ccsbar: decodes the field (98 fallback for old daemons),
  **ForecastEngine mirror now reads the line from status.json instead
  of hard-coding the retired 100 cap**; Configure disclosure gains the
  chain-wide "Weekly limit" row (presets + Custom…); per-account 5h
  threshold gets Custom… free-typed input on BOTH surfaces (capsule
  menu swaps to an inline field; context-menu Custom… opens the
  disclosure with the field armed); parsers mirror socket validation
  exactly (5h whole 0–100, weekly 50–100 decimals). 177 tests
  (`fccf472`). Deployed both; socket smoke test ok
  (`{"ok":true}` → `weekly_switch_threshold = 98.0` in profiles.toml).

- **2026-07-12 (night) — contribute-back: upstream PRs #36/#37/#38
  opened (AX ask).** Ports of this session's three shipped changes onto
  `upstream/mommy` (which has fallback.rs + scheduler.rs but NO daemon
  module), each branch independently green (727/729/729 tests + clippy)
  and pushed to origin. **#36** `pr/switch-vanished-guard` — the
  delete-ghost HIGH's actions.rs half only (existence-first guard in
  `switch_profile` + RED/GREEN test); daemon drain half dropped; the
  hazard is live upstream today (their v0.9.1 delete × async switch
  gate), so this outranks #35's copy — when #36 merges, #35's actions.rs
  hunk rebases away. **#37** `pr/active-backoff-cap` — 841e2bd port;
  applied clean because upstream already carries the rotation-coherence
  is_active plumbing; fork-only R3 stand-down tests spliced out of the
  test conflict. **#38** `pr/weekly-soft-line` — 4ba6b38 + 03d231f as
  two commits; dropped daemon socket/status_json/tick/types +
  fallback_config.rs (upstream persists via direct `save_app_state`, so
  step/commit handlers rewritten to that idiom — validation preserved:
  presets always in-band, `parse_weekly_pct` guards the editor, accessor
  clamps hand-edits); update_banner conflict resolved keeping ONLY the
  weekly_pct threading (fork's auth_broken banner d8ad828 does NOT ride
  along); README bullets re-merged upstream-side (no daemon/socket
  mentions). #37+#38 both touch scheduler.rs — disjoint hunks, noted in
  the PR body. Closes the "follow-up candidates after live soak" note
  above. Upstream had 1 new commit since SYNC-2 (9a982d4, TUI
  api-account rows — unrelated, no re-sync needed).
- **2026-07-12 (night) — upstream merged #34–38; corrections absorbed back
  into fork main (merge `66c109c`, AX ask).** uwuclxdy squash-merged all five
  contributed PRs but pushed fixes ON TOP of each before merging; the fork
  carried the pre-correction versions, so every fix was a real live bug on the
  fork. Strategy: `git merge upstream/mommy` (fork is 148 commits ahead — rebase
  was never an option), resolve every file to `--ours`, then port uwuclxdy's
  deltas surgically (isolated via `git diff pr/<branch>..<squash>` in a 6-agent
  read-only workflow). Ported: **#34** `adopt_disk_rotation` + `&RotationGuard`
  witness in oauth.rs (cross-process peer rotation was refreshing a spent token →
  false quarantine); **#36** shared `ensure_profile_exists` guarding ALL THREE
  switch primitives (fork guarded only `switch_profile`; `switch_profile_discard`
  / `_reconciled` tore down the live link on a ghost target); **#37**
  `ACTIVE_CAP_MAX_STREAK=6` releases the active 429 cap past a shallow streak (the
  #30 window-pinning failure); **#38** wrap-off `Off` keys on `WEEKLY_HARD_BLOCK_PCT`
  (100%) not the soft line + profile.rs wording clamp→reset + `WEEKLY_PRESETS`
  dedup; **#35(a)** status_json third-party freshness (api-key profiles derived
  `never-fetched` — a real ccsbar/ccu contract bug) + **#35(e)** reserved-name
  guard in `validate_profile_name` (rejects the 11 bare subcommand tokens incl.
  hidden `__complete`/`mcp-await-job`; NOT `completions`) + DESIGN.md `Z`→`+00:00`.
  +11 ported tests. **Skipped**: upstream's api-account re-login TUI rows (9a982d4
  — fork re-logins via inline BaseUrl/ApiKey rows; porting breaks 5 fork tests)
  and the overview orange-sweep pulse (070375f — cosmetic). **Dropped** upstream's
  `wiki/daemon.md` (fork canonical = `docs/ccsbar/DESIGN.md`). GOTCHA logged:
  `git checkout --ours` is a NO-OP on auto-merged (non-conflicted) files — the
  four skipped-feature files (config/format/header.rs, tui_render_mod.rs) silently
  kept upstream's code until re-reverted with `git checkout main -- <file>` (one
  test caught it). Verify: 876 tests + fmt + clippy clean; 6-agent adversarial
  port-coherence review returned 5 CONFIRMED_CORRECT + 1 low CONCERN (the two
  hidden reserved subcommands), fixed before commit. Fork daemon/fallback_config/
  doctor/logline/profile_json modules kept as-is (upstream now also has a daemon
  module from #35, but the fork's is the production version, ahead).
- **2026-07-12 (night) — adopted the two skipped upstream TUI features after
  all (AX: "upstream 更好就跟 upstream，以后 update 简单些；TUI 外观我没
  preference，基本走 ccsbar").** Reversed the initial skip once AX set the
  policy — the real win is DIVERGENCE REDUCTION, not looks: cherry-pick (`-x`,
  upstream authorship preserved) **B** `2eacd44` (070375f overview orange-sweep
  pulse — format.rs/header.rs now 0-diff vs upstream; overview.rs conflict
  resolved keeping the fork-only email column AFTER the pulse cell) and **A**
  `d455131` (9a982d4 api-account re-login/log-out rows — only app.rs's
  actions-import list conflicted, `CaptureIdentity` + `clear_profile_api_key`
  both kept; config_rows ungated so api accounts show Login+DeleteCreds; the 5
  visibility assertions in tui_app.rs auto-inverted to "api SHOWS login/delete"
  and pass; `clear_profile_api_key` = real api log-out that also purges
  THIRD_PARTY_CACHE_FILE, a genuine fork gap). 879 tests + fmt + clippy clean.
  Net: the fork's overview/format/header/config TUI surfaces now track upstream,
  so future syncs of those files stop conflicting.
- **2026-07-12 (night) — RLS-1: RateLimited staleness (uwuclxdy's #37 "principled
  follow-up", opted into).** The consumer-side complement to #37's poll-side cap:
  #37 keeps the active's 429 retries frequent so the `/usage` throttle window
  drains; RLS-1 handles the case where it stays stuck anyway. **Root wedge**:
  `scan_auto_switch` gated on `decision_fresh(active)` — only a `Fresh` active read
  is trusted to decide — so a stuck-RateLimited active (never `Fresh`) returned
  early every tick and the daemon wedged on it, exactly the AUTH-4 auth-broken
  wedge one class over. **Fix, 2 surgical parts mirroring AUTH-4**: (1) DECISION —
  a new `is_stuck_rate_limited(status, streak)` predicate (`RateLimited && streak >
  ACTIVE_CAP_MAX_STREAK`, reusing #37's own cap boundary as the deep-slot line);
  `scan_auto_switch` adds it as a 3rd bypass beside `active_broken`, so the walk
  RUNS — but unlike auth-broken it still faces `next_auto_switch_target`'s
  last-known exhaustion gate (guarded by `five_hour_live` + weekly reset), so a
  genuinely-spent stuck active rotates away (wedge fixed) while a throttle artifact
  with real headroom STAYS (no false switch). `fallback.rs` deliberately untouched
  — Gate 2 IS the safety. (2) CONTRACT — additive per-profile `stale: bool` in
  `status.json` (same shared predicate → decision & display can't drift, per the
  publish-don't-mirror rule); schema stays 1; single-shot `status --json` always
  false (no streak store). Threaded via `LiveSignals.streaks` (daemon snapshots
  `rate_limit_streaks` before the config lock, like the sibling stores). **TUI
  deliberately skipped** (AX: ccsbar-first, no TUI preference) — uwuclxdy's TUI cue
  is dead weight here; the decision fix delivers the anti-wedge win with zero
  client change. **Review**: 4-dimension × adversarial-refuter workflow
  (wf_caa4aad9-6a8) → 0 implementation defects (both verifiers confirmed the code
  correct), 2 CONFIRMED medium TEST-gaps fixed before commit — the scheduler test's
  "stay put" case used a LIVE low-util window (passed trivially, never exercised
  the `five_hour_live` guard) so a **lapsed-stale-HIGH** case was added (the real
  post-storm shape: `apply_outcome` freezes the last Fresh window at 100%, it
  lapses after ~5h → regained headroom → STAY; this is the exact RLS-1↔AUTH-4
  asymmetry, a broken active WALKS on the same store), and the `stale` status test
  was single-profile (couldn't catch compute-once-apply-to-all) so it now proves
  per-profile keying with work=stuck-RL(stale)+home=Fresh(not) in one body; 1
  finding REFUTED (no ccsbar wire collision — ccsbar's `isStale` is a client-side
  computed prop, not a decoded key, so the new field is cleanly additive). Verify:
  881 tests (+2, 4-case & 3-assert) + fmt + clippy clean. **ccsbar/ccu follow-up
  (deferred, additive)**: consume `stale` to dim a distrusted reading, preferring
  the daemon flag over ccsbar's existing client-side `isStale` derivation.

## 2026-07-15 — upstream 0.11.0 merge + RESCUE-1 (dead-live-login reclaim)

**Incident (2026-07-14→15)**: live `.credentials.json` diverged ("matches no
stored account"), daemon rotated stored ax-backup 3×8h without write-back, live
refresh lineage died → every Claude session "Login expired"; all switches
deferred ("unsaved credentials — resolve in the TUI") → ccsbar "switch didn't
take". Root cause chain: 0.9.1's outdated wire shape made the `/profile`
identity probe fail; `fetch_account_identity` collapses probe failure into
"identity unproven"; `follow_live_login` memoized that verdict against the
login (follow_memo) → one bad probe = permanent wedge, TUI-only escape.

**Fix 1 — merge upstream/mommy v0.11.0+12 (66→0885756)**: wire-parity
(AuthClient::Profile, CC-shaped refresh/login), quarantine only on
endpoint-confirmed dead (48ba4b7), sibling identity by account uuid (daa4c98),
single-fetcher FetchLease (adopted, replaces fork never-probe), PollStreaks
{rate_limit, refresh_fail} (adopted; RLS-1 deduped to upstream's is_stuck_streak).
Kept fork: VecDeque pending-switch queue, feed superset (forecast/last_error/
last_switch/account_email), CaptureIdentity model (upstream's account_uuid
commit-seeding reconciled INTO it; TUI relogin now anchors from the mint's own
probe — Known — instead of Unknown), burn_rate_for_profile pub(crate),
docs/ccsbar/DESIGN.md as contract home (wiki/ stays deleted; lease/auth_status/
next_refresh_at-null notes ported). Upstream's strict per-profile key-set test
adopted + account_email added. 1044 tests, fmt, clippy clean.

**Fix 2 — RESCUE-1 (tick.rs)**: `LiveOwner::Unknown` now carries a reason
(ForeignAccount | AccessDead | Unproven). Only a PROVEN-foreign login memoizes;
probe failures retry on a 30-min timer (follow_retry_at) — memo-poison fixed.
AccessDead → `rescue_dead_live_login`: refresh-probe the live refresh token —
`invalid_grant` → reclaim (AUTH-1 install gate + fingerprint race guard +
force_link back to the active's stored chain); refresh SUCCESS → the pair was
alive, write it back in place (`claude::write_live_oauth_pair`, surgical JSON
update preserving mcpOAuth etc.) and re-identify; transient → timed retry.
New: usage::IdentityProbe{Proven/Rejected/Indeterminate} (401→Rejected,
403 stays Indeterminate — WAF caution mirrors refresh_rejection_is_terminal).
6 new tests (reclaim / transient-backoff / alive-write-back / broken-stored-
refuse / refreshless-reclaim / mid-probe race abort).

**RESCUE-1b addendum (same night)**: live deploy revealed the actual wedged
state was CC's logged-out SHELL (accessToken/refreshToken both "", expiresAt 0
— CC blanks the file when its refresh dies, keeping mcpOAuth). A shell has no
fingerprint → follow bailed before the rescue engaged. Added
claude::live_login_is_empty + shell branch reclaiming through the shared
reclaim_live_slot (extracted from the dead-login tail; AUTH-1 gate + late
still-unchanged re-check + timer on failure legs). 4 tests.

**Adversarial review (12 agents)**: merge-semantics 0 findings, security 0.
Rescue-correctness 2 CONFIRMED + fixed (write-back leg missing fingerprint
guard → clobber risk on concurrent CC login; write-back zeroed follow_retry_at
→ per-tick refresh storm on a still-401ing-yet-refreshable token — now arms
the 30-min window; the user is already unblocked by the write-back itself).
Test-fidelity 5 gaps closed (identity classification extracted pure +
truth-table pinned: 401-only Rejected; probe-outage no-memo; reclaim-failure
and write-back-failure arms via chmod-locked sandbox dirs; gate-Transient;
blank-uuid; foreign-during-backoff). 1 refuted. Final: 1056 tests, clippy 0.

**Deployed 2026-07-15 23:10 PDT**: clauth 0.11.0, daemon respawned, RESCUE-1b
fired ONE TICK after boot: "live login for 'ax-backup' is a logged-out shell
(no tokens left) — reclaimed the live slot". .credentials.json → symlink to
healthy stored chain; doctor 7/7; last_error null; live expiry +7h. The
2026-07-14 outage class (probe failure → divergence memo-poison → dual-lineage
race → dead live login → wedged switches) is closed end-to-end: wire parity
prevents the misclassification, timed retries replace the memo-poison, and
the rescue reclaims both corpse variants (endpoint-confirmed-dead, shell).

---

## 2026-07-16 — RESCUE-2: full-audit fixes (switch unwedge + follow/rescue hardening) + CDX-0b re-eval

**Trigger**: user asked "还有什么问题你仔细fully review audit一遍". 27-agent audit
workflow (6 dimensions + adversarial verify per finding): 15 confirmed / 6
refuted of 21 raw. Production timeline corroborated the top finding: 07-14
21:02 "matches no stored account" (a genuine 4th account in the live slot) →
33h wedge — 4× 8h rotations refused, user switches 04:41-04:50 "gave up" —
healed only when the foreign login died into a shell (RESCUE-1b reclaim
06:10:16). Refuted class worth remembering: rotating the STORED chain while
foreign-diverged does NOT kill the foreign live login (separate chains;
Keychain write already guarded by live_login_is_foreign) — the overnight death
was CC-side, not ours.

**RESCUE-2 (HIGH F1, tick.rs drain_pending_switch + claude.rs)**: a queued
Origin::User switch no longer wedges behind a diverged live login — the
socket path (ccsbar tap) had no DivergenceChoice and fail_switch'd forever.
Now: archive the unsaved login to `~/.clauth/quarantine/<ms>-<seq>-<name>.
credentials.json` (0600, newest-20 retention, seq breaks same-ms collisions)
then `switch_profile_discard`. Divergence re-verified INSIDE the state flock;
archive failure fails the switch (login untouched). Scheduler-origin still
defers — automation may not discard a login clauth doesn't own; a user tap IS
the operator decision the old TUI-only escape waited for.

**RESCUE-2b (F2/F3/F5/F8/F11)**: (a) ForeignAccount verdict now requires
COMPLETE anchor coverage over login-holding profiles — incomplete → Unproven
+ 30-min timer + once-per-window log (a legit profile can lack an anchor:
dropped on unproven re-login, backfilled by the hourly /profile poll; memo
would wedge an owned login). (b) blank-access+present-refresh live file (torn
write): reclaim iff the refresh byte-matches the active's stored chain, else
timer-gated visible no-op — was a silent per-tick nothing. (c) sibling
capture/overwrite failures arm the timer instead of memoizing (memo is
proven-foreign ONLY now — the stated RESCUE-1 invariant, enforced); adoption
attempt itself timer-gated. (d) follow_memo/follow_retry_at persist across
restarts (`~/.clauth/daemon-follow.json`, saved on change by the
follow_live_login_with wrapper) — a launchd respawn inside the 30-min window
no longer re-probes/re-spends per boot (restarts ARE a real event class:
2× within 8 min on 07-16).

**RESCUE-2c (F4/F6/F7)**: write_live_oauth_pair takes the expected fingerprint
and re-checks it INSIDE the flock, returning LiveWriteBack::{Written,
Superseded} — the symlink case is now benign Superseded (was a scary "its
chain is lost; re-login" Err for a race that loses nothing);
force_link_profile_credentials_if re-checks still_unchanged under the flock
(reclaim's check→mutate window closed to clauth actors; CC-side residual is
inherent). proactive_rotation_due gains an active_diverged input — never
preemptively rotate a diverged active's stored chain (mirrors
rotation_candidates' skip_active).

**Tests**: +13 (drain user-archive/scheduler-defer, anchor-gap no-memo,
overwrite-failure timer, degraded-copy reclaim, unrecognized-refresh timer,
restart persistence ×2, Refreshed-gate reclaim, truth-table Parse +
blank-email, write-back supersede ×2 + surgical-write, quarantine retention).
1069 pass, clippy 0, fmt clean. Docs: DESIGN.md status.json example gained
the 8 shipped-but-undocumented fields (clauth_version/last_switch/last_error/
weekly_switch_threshold/burn_aware/forecast/fallback_chain/fallback.
last_resort) + set_last_resort/set_weekly_threshold socket commands + version
literals bumped. Note: "docs/wire-parity.md" never existed — wire-parity
lives in code + fetch truth-table tests; don't reference it as a file.

**Post-fix adversarial re-review (opus)**: verdict SHIP, 0 CRIT/HIGH; 5 LOW
all addressed (lock poison test sandbox-pinned — parallel flake; quarantine
retention; anchor-gap log; two documented trade-offs: Superseded deliberately
doesn't arm the timer, tier-1 adoption waits out an unrelated armed window).

**CDX-0b (docs/codex-support/feasibility.md §0b)**: 4-track re-eval overturns
the 07-12 "in-session hot-swap structurally impossible" headline — true for
auth.json swap (now doubly guarded at HEAD cbc83d9), REFUTED for a localhost
header-rewriting proxy (Authorization + ChatGPT-Account-ID; proven by
ndycode/codex-multi-auth 391★ et al; rate-limit telemetry rides through free).
Revised ladder: CDX-1 session-boundary swap → CDX-1b per-CODEX_HOME → CDX-1c
swap+resume wrapper → CDX-2 passive JSONL usage → CDX-5 injection proxy
(opt-in seamless tier). Still DEFERRED — no build decision made.

## 2026-07-16 (later) — CDX-1/1c/2 SHIPPED: codex account switching (session-boundary MVP + passive usage)

AX un-deferred the CDX milestone ("那我们继续搞codex"). Upstream issue #45:
uwuclxdy says out-of-scope for him (no codex subscription), not against it,
asks for a rough system design in the issue — posted after this ship (with
LLM disclosure, per his request). Fork-side feature.

**Plan**: docs/codex-support/PLAN.md (reviewed via plan-design pass: 2 HIGH
promoted to tasks T1b + MCP-in-T6 before build). Design pillars: TWO
independent active slots (`active_profile` claude / `active_codex_profile` —
switching one never touches the other); explicit persisted `harness` field
(serde default claude, zero migration, byte-stable render); whole-file raw
byte round-trip store `profiles/<name>/codex-auth.json` (unmodeled fields
survive; identity/plan/email parsed locally from stored JWTs — zero network);
no symlink/Keychain — content compare by `tokens.account_id` anchor +
one-directional adopt-back (daemon NEVER writes live outside a switch);
loss-free switch (rotated outgoing chain adopted back first; foreign login
archived to quarantine `.codex-auth.json` suffix or refused per
ForeignLivePolicy — User origins archive, Scheduler defers, RESCUE-2
semantics); keyring/auto/ephemeral store modes refused; per-harness chains
(codex_fallback_chain stored, walk lands with CDX-4).

**Shipped surfaces**: `clauth login <name> --codex` (capture; CAP-3 dedup;
reauth-in-place), `clauth <name>` harness-dispatched switch (CLI [y/N]
archive prompt for foreign; pgrep session-boundary note), `clauth resume
<name>` (CDX-1c: switch + `codex resume --last` — semi-seamless carryover;
"resume" now reserved), socket/daemon drain codex arm, MCP switch dispatch +
list_profiles harness field, TUI (Codex tag, openai provider, plan/email
from lens, per-slot active dot, Enter→codex switch, Setup rows drop ALL
claude-shaped dead ends, log-out via codex_clear_profile_auth), doctor
check_codex (silent claude-only, WARN-only, >7d snapshot staleness),
status.json additive: per-profile harness + per-slot active +
codex_snapshot_at + top-level active_codex_profile (DESIGN.md synced).
CDX-2: passive usage — src/codex/usage.rs ports ccu's session-JSONL reader
(mtime-ordered capped walk, tail reads, resumed-session-in-old-dir catch,
.zst recognized-skipped, NO zstd dep — 0/1136 local files compressed);
scheduler lease-holder tick gains codex_passive_tick publishing through the
exact fetch-outcome channels (cache/store/status/cadence) for the ACTIVE
codex profile; attribution gate = event ts >= live auth.json mtime (every
account change rewrites it) — conservative staleness over misattribution.

**Invariants encoded as tests**: codex profiles NEVER enter either Anthropic
fetch leg (explicit guards + adversarial regression test); harness
immutability BOTH directions (capture refuses claude targets; cmd_login +
overwrite_captured_profile + edit_profile_endpoint refuse codex targets —
the last found by a post-fix re-verify agent: TUI BaseUrl row was reachable);
switching TO the live owner never rolls a fresher chain back; wholesale
`config.state = update_app_state(...)` sync (claude-follow parity — partial
copy-back dropped same-tick external edits).

**Review**: 13-agent workflow (4 dimensions × adversarial verify): 8
confirmed (1 HIGH login-corruption, 2 MED MCP/status-test, rest LOW/NIT) all
fixed, 1 refuted; HIGH fix re-verified by a dedicated agent (found the
edit_profile_endpoint gap, fixed + tested). Final cross-phase pass on the
CDX-2/1c diff. ~72 new tests; suite 1069 → 1141, clippy 0.

**Deferred**: CDX-1b isolated CODEX_HOME start (needs session refcounting —
two-carrier refresh_token_reused hazard; revisit with CDX-3 refresh
ownership); CDX-3 refresh/PKCE login; CDX-4 codex chain walk; CDX-5 proxy
(own design round — nothing in CDX-1/2 constrains it). tokens.json codex
feed = follow-up with ccu migration (ccu keeps direct JSONL reads for now).

**AX manual acceptance (never run unattended)**: `clauth login <name>
--codex` against the real live login, a real two-account codex switch, and
`clauth resume`. Daemon deploy = cargo install + pkill (authorized).

---

## CDX wave 2 — CDX-3 / CDX-1b / CDX-4 / CDX-5 (2026-07-16, second wave)

The deferred ladder, shipped end to end. Commits caf2f01 (CDX-3) · e2d0fd2
(CDX-1b) · 47eb421 (CDX-4) · <this> (CDX-5). Wire facts re-verified at
openai/codex `9ff47868` / codex-cli 0.144.5.

- **CDX-3 standby refresh + PKCE login**: `src/codex/oauth.rs` (JSON refresh
  wire, all-optional response only-if-present overwrite, failure taxonomy
  mirroring codex's classifier, surgical apply_refresh raw round-trip,
  standby_due). Scheduler `codex_standby_tick` refreshes ONLY chains clauth
  exclusively holds (never the live owner, never a leased/broken profile) —
  `codex_standby_candidates` + RotationGuard across the spend + in-guard
  re-check + discard-if-chain-moved. `RotationProbe` (non-blocking, rank-
  exempt) guards switch/capture against a mid-flight refresh. `src/loopback.rs`
  extracted from oauth_login (claude tests pin it byte-identical); `src/codex/
  login.rs` = codex browser PKCE (fixed ports 1455/1457, form-encoded
  exchange, optional API-key mint, explicit auth_mode). `clauth login <name>
  --codex --browser` → store-only (live untouched). doctor: quarantine +
  stale-keep-alive WARNs.
- **CDX-1b isolated start**: `CodexRuntime` (codex-sessions/ lease flocks,
  live-owner + store-mode + loginless refusals, config.toml COPIED not
  symlinked, 60s adopt-back watchdog + final sync, gc rescue). `clauth start
  <codex-profile>` spawns codex in its own CODEX_HOME. Two-carrier refusals
  wired into switch/capture/standby.
- **CDX-4 codex chain**: `snapshot_codex_chain` + `next_codex_auto_switch_
  target` (claude walk shapes minus decision_fresh/kick/Off; exhaustion =
  percent shape OR codex's own rate_limit_reached_type verdict). `UsageInfo`
  carries `codex_rate_limit_reached` (single source cache→store→status).
  Per-harness pending-switch queue independence (PendingSwitchEntry.harness;
  drain services one winner per harness). Chain edits route by harness;
  `clauth fallback` CLI; status.json `codex_fallback_chain` + per-harness
  fallback blocks.
- **CDX-5 injection proxy** (`src/proxy/`): `clauth proxy` — opt-in localhost
  SSE proxy. Strips codex identity headers, injects the selected pool
  account's, forwards to chatgpt.com/backend-api/codex, streams SSE back; a
  pre-commit 429/401/5xx rotates to the next chain account and replays before
  the client sees a byte. Pool = codex chain (sticky-active, per-account
  cooldown ≥60s). Token freshness: live-owner reads live, parked chains
  refresh via CDX-3 (single-writer flock shared with the daemon). Usage from
  the flow-through x-codex-* headers → per-account cache; heartbeat file makes
  the passive JSONL leg stand down while the proxy serves (no misattribution).
  `clauth proxy --print-config` prints the config.toml block (never edited).
  doctor: proxy heartbeat + config-pointed check. Stub-upstream e2e proves
  injection + rotate-replay + SSE relay + usage capture.

Suite 1141 → ~1225 (clippy 0, fmt clean). Plan-review round (5-dim
adversarial workflow + refute) folded in before build: per-harness queue
gates, UsageInfo carrier, config.toml copy, proxy compression/framing/caps,
passive-leg heartbeat, doc reconciliations. Design posted to uwuclxdy/clauth#45
remains the wave-1 doc; wave-2 detail in docs/codex-support/{PLAN,proxy-design}.md.

**AX manual acceptance (never run unattended)**: real `clauth login <name>
--codex --browser`, a real codex chain switch, real `clauth proxy` against the
live backend (config paste + real 429 rotation). ToS posture unchanged
(passive + flow-through only; wham/usage never called).

## 2026-07-16 (later) — sandboxed switch verification + narrow-TUI (phone) mode

- **codex-sim harness** (`scripts/codex-sim/`, commit 5b570f1): the PLAN.md
  CDX-1 "sandboxed end-to-end" acceptance is now a script. Second daemon under
  an isolated `$HOME`, two fake accounts (unsigned JWTs; fresh `last_refresh`
  + far-future `exp` so CDX-3 standby never fires network), user switch swaps
  live bytes verbatim, forged weekly-only rate-limited JSONL drives a REAL
  auto-switch (verdict remapped to the weekly slot), real `~/.codex`
  hash-verified untouched. 8/8 PASS. Residual (needs a real 2nd paid account,
  AX-manual): OpenAI accepting the swapped token, real 429 JSONL shape,
  server-side refresh-reuse. NOTE for advice-giving: a switch never disturbs
  RUNNING codex processes (auth cached in memory) — only new sessions pick up
  the new account, so live-testing with a free account doesn't stop in-flight
  work.
- **Narrow-TUI (phone ≈45 col) adaptation** (commit f10fe99): single
  breakpoint `panes::NARROW_BODY_W = 60`; master-detail stacks (shared
  `panes::master_detail`, 6 call sites incl. tokens Models), tokens dashboard
  stacks all cards, chain-row gauge sized from leftover width (exact span
  budget), footer measured degradation (essentials always survive), header
  dangling-`·` fix, status duration drops to its own line, modals pre-split
  over-wide lines by char (`chunk_line` — exact height by construction).
  Desktop byte-identical ≥60 (pinned by tests). Suite 1228 green.
  **Upstreamed**: issue uwuclxdy/clauth#48 + PR #49 (branch `feat/narrow-tui`
  off `mommy`, 970 upstream tests green). Review pass: MED gauge budget
  off-by-one (fixed, 3-digit threshold test added), LOW modal word-wrap
  height (fixed via chunk_line), NITs fixed.
- Upstream is now 12 ahead of the fork's merge-base (v0.12.0: owner-only
  perms fix 7f2d4db, durable token ledger c8de3fe) — candidates for the next
  upstream-sync pass.

## 2026-07-16 (later) — CDX-5 real-backend proof · GET /models fix · 0.12.0 sync + history squash · upstream codex PR

- **CDX-5 proxy PROVEN against the LIVE backend** (feasibility.md §"Real-backend
  confirmed"; deploy-shape memory updated). Shipped `clauth proxy` binary built
  on an isolated throwaway host, run under an isolated `$HOME` (pool = a real
  paid `ax-codex-cl` Plus profile, `codex-auth.json` scp'd in + deleted after),
  isolated-`CODEX_HOME` real `codex exec` → real `chatgpt.com/backend-api/codex`
  → real model answer (`PROXY_OK 42`, 15,856 tokens). **OpenAI honors the
  proxy-injected `Authorization`+`ChatGPT-Account-ID`.** The host's own live
  codex session (different account) untouched (sha256 identical). Probe kept:
  `scripts/codex-sim/proxy-client-integration.sh` (client-integration half; commit
  0fd4e20). Still not exercised: a real forced-429 rotation (unit-proven only).
- **GET /models fix** (commit 906ff8e): the real run showed the proxy 405'd
  codex's `GET /backend-api/codex/models` refresh (POST-only gate). Now admits
  GET too and forwards with the request's own method (generic ureq header helper
  serves both body typestates). Tests: GET /models forwarded + method preserved
  + identity injected; DELETE still 405s.
- **Synced fork to upstream 0.12.0** (merge 4f930e0 → then squashed): 19-file
  conflict merge (ratatui refactors + owner-only perms vs fork TUI/profile),
  resolved via a parallel resolver workflow + hand-resolution of scheduler.rs
  (kept upstream's scan_recovery tests, adapted to the fork's VecDeque
  PendingSwitchEntry API) + a tokens::spawn arity fix (0.12.0 added the
  durable-ledger `clauth_dir` arg). Suite **1254 green**, clippy/fmt clean.
- **History squash** (force-pushed 57917d9): collapsed ~192 fork commits into
  **8 subsystem commits** on 0.12.0 (codex+proxy / usage-scheduler / daemon /
  tui / tests / docs / core / meta) so future upstream rebases replay 8, not
  192. Tree byte-identical to the verified merge (`git diff` empty — safety
  gate). NOT feature-pure (shared files interleave codex/narrow-tui/keychain —
  can't file-split); coherent-by-subsystem was AX's chosen granularity.
- **Upstream codex PR**: uwuclxdy/clauth **#51** (`xingfanxia:main` → `mommy`),
  the complete compilable+tested fork codex support on 0.12.0. Supersedes #49
  (narrow-tui overlaps in the render files). Upstream has no codex sub → can't
  test codex; claude-side is byte-identical.

## 2026-07-17 — post-0.12.0 upstream catch-up (merge 2497a36)

- **Why PRs went CONFLICTING**: uwuclxdy pushed 4 commits to `mommy` AFTER our
  0.12.0 sync — reload fingerprinting (c4faec0: config.toml edits hot-reload
  without restart), bounded state-lock acquisition (26b1e3e), auto-start window
  kick fix (1470147), wiki note (63d8f21). Merged them in (5 conflicts:
  tick.rs ×7 hunks, mod.rs, daemon_mod/profile tests, wiki modify/delete).
- **Fingerprint conversion**: every fork-only `last_state_mtime` site (codex
  follow/switch, claude follow, drain_config_ops, 2 TUI sites upstream never
  touched — auto-merged but referencing the dead field) converted to
  `last_reload_fp`/`reload_fingerprint()`. `drain_config_ops` keeps the fork's
  pinned contract: config.toml-only edit (Ok(false)) does NOT adopt — one
  harmless self-reload next tick beats swallowing a same-tick external edit.
- **wiki/daemon.md restored** to upstream's copy — the fork had deleted `wiki/`
  long ago, so every upstream wiki edit re-conflicted with our deletion; keeping
  the file kills that conflict class permanently and cleans the PR diff.
- Suite **1262 green** (+8 upstream tests), clippy/fmt clean. PR #51 back to
  MERGEABLE; #49 got a supersession comment (offer to rebuild standalone if
  uwuclxdy wants narrow-tui alone). Local daemon reinstalled + respawned.

## 2026-07-17 (later) — CDX-5 proxy deployed on AX's Mac (AX-approved)

- **Trigger**: ccsbar switch to ax-codex-cl looked "broken" — running codex had
  the old (limit-hit) auth cached in memory; on-disk switch was fine. Diagnosis
  chain: JWT-claim compare (live == cl profile), fresh `codex exec` turn OK.
- **Deploy shape**: LaunchAgent `com.clauth.proxy` (KeepAlive-on-crash, port
  4517, `~/.clauth/proxy.log`) + global `model_provider = "clauth"` in
  `~/.codex/config.toml`; `codex --profile direct` = bypass; pre-change backup
  kept. Live-verified: PROXY_OK (profile), GLOBAL_PROXY_OK (default),
  DIRECT_OK (escape hatch).
- **Codex ≥0.144 gotcha**: `[profiles.x]` in config.toml is now a hard error —
  profiles moved to `~/.codex/<name>.config.toml` overlay files.
- **`codex exec` stdin gotcha**: blocks in `resolve_root_prompt` until stdin
  EOF — a backgrounded shell's held-open pipe hangs it forever; `< /dev/null`.

## 2026-07-18 — CLA-SPLIT: long-lived session tokens (commit 57cef39)

- **Why**: 2026-07-16..18 all three claude accounts serially died with
  `refresh token revoked` — N long-lived CC sessions + clauth's refresher all
  rotating single-use chains through the one live slot; a stale-token refresh
  trips reuse detection and revokes the whole chain. oat-only migration was
  ruled out live (setup-token scope = inference+sessions → usage endpoint 403s).
- **Design (split credentials per profile)**: `session-token.json` (static
  `claude setup-token` mint) is what switches install — sessions run on a
  never-rotating token; `credentials.json` OAuth becomes clauth-private,
  usage-only, single-writer. `install_source_path` is the one concept: classify
  /first-login/link/keychain/snapshot/adopt/`clauth start` runtime all resolve
  through it. Guards: snapshot & runtime watchdog never write the token over
  the usage pair; rotation persist never mirrors the usage chain over an
  installed token; gate is Ready for split profiles unless the token is
  clock-dead (Broken + re-mint hint). Non-split profiles byte-identical.
- **Review (opus code-reviewer)**: HIGH `clauth start` runtime still handed
  sessions credentials.json (fixed: canonical_credentials → install source +
  watchdog never adopts over the token); MED gate ignored token's own clock
  (fixed); LOW TUI relink affordance + keychain double-stat TOCTOU (fixed).
  force_snapshot left as-is: user-confirmed Overwrite writes the captured
  OAuth into credentials.json (usage side) and relink reinstalls the token —
  correct destination, no clobber.
- **herdr real-backend validation 7/7 PASS** (isolated $HOME, zylos sha256
  unchanged): switch links live→session-token.json; real turn `SPLIT_OK` on
  the clauth-installed token; **image turn `IMAGE_OK red`** (file_upload-scope
  concern cleared — images ride messages inline); usage pair byte-identical
  after real turns; live token unrotated.
- **Deployed**: three Mac profiles filled from `~/.claude-fleet/
  claude_long_live_tokens.env` (tokens verified ALIVE + sha256-matched to the
  fleet hosts' mints — first probe false-401'd by quoted-value parsing, fixed);
  binary installed, daemon restarted. Split arms per profile at its NEXT
  switch (live slot still holds the old OAuth until then). 1266 tests green.

## 2026-07-18 (later) — CLA-SPLIT upstreamed + docs

- Upstream contribution: issue **uwuclxdy/clauth#52** (root-cause writeup) +
  PR **#53** (`feat/session-token-split` cherry-picked onto CURRENT mommy,
  re-adapted to upstream's pre-CAP-1 snapshot/adopt idioms — no fork private
  refactors dragged along; 1111 tests green, clippy/fmt clean). Claude-side,
  so upstream can test it himself (unlike codex PR #51).
- Feature doc: `docs/claude-split/README.md` (problem, split table,
  semantics incl. per-profile arming + auth_broken="usage broken", ops
  runbook: fill shape, yearly re-mint ~2027-06-28, quoted-env gotcha).
- Mac state: all 3 profiles filled; ax-backup armed (live slot = its
  session token, verified symlink + no-refresh-token); ax-main/ax-cl arm at
  their next switch; all 3 usage legs Fresh after AX's re-logins.

## 2026-07-18 (PROX-1) — proxy stream-death incident: root-caused + fixed

- **Incident**: overnight codex sessions (xhigh, 20M+ ctx) died repeatedly
  with "stream disconnected before completion: stream closed before
  response.completed", reconnect-looping with no visible progress; proxy.log
  was 211/215 lines of unstamped "connection error" noise. NOT an OpenAI
  outage — three interlocking proxy defects:
  1. `PROXY_AGENT` `timeout_global(15min)` truncated ACTIVE streams (ureq's
     global timeout covers the body read and fires even while bytes flow) —
     real xhigh reasoning requests exceed 15 min; each kill made codex replay
     the turn from scratch (no server-side resume) → "半天看不到 update".
  2. Upstream holds the SSE stream open past `response.completed` (keepalives;
     codex closes first) → relay lingered per turn until dead-client write or
     backstop → spurious error per SUCCESSFUL turn + thread pile-up toward
     the 64-conn cap (over-cap = 503s).
  3. No timestamps, no per-request summaries → forensics near-impossible.
- **Fix** (src/proxy/sse.rs NEW + mod.rs relay rewrite + oauth.rs):
  `TerminalSniffer` — prefix-classifies SSE lines (terminal data line is one
  100s-of-KB line; classifying at its newline never matches — sniffer bug #1
  caught live), ARMS on `event:`/`data:` terminal forms, FIRES at the event's
  blank-line terminator (firing on the prefix truncated the completed event —
  sniffer bug #2, caught live within minutes). timeout_global 15min → 2h pure
  leak backstop. `enable_timestamps()` + one summary logline per request
  (account/method/path/status/bytes/secs/end-shape).
- **Method**: real wire captured via scratchpad TCP tee (codex → tee → proxy):
  `event: <name>` + `data: {"type":"<name>",...}` + blank line; terminal data
  line carries the whole response object on ONE line. Fixture gap that hid it:
  test SSE bodies had Content-Length + short lines.
- **Validated**: 1277 tests green (9 new sniffer units + lingering-upstream
  e2e asserting FULL-event relay + ureq active-stream-truncation semantics
  pin); live `codex exec` turn → log `POST /responses → 200 · 91KB in 1s ·
  completed`, clean close, no spurious errors. Proxy + daemon restarted on
  the new binary.

## 2026-07-18 (PROX-2) — the ACTUAL assassin was timeout_recv_response(30s)

- PROX-1's timestamped summaries immediately exposed the deeper truth: live
  turns died `TRUNCATED after …B in 29s · upstream error: timeout: receive
  response`. In ureq 3, `timeout_recv_response` keeps running through the
  BODY read (pinned: `ureq_recv_response_timeout_kills_the_streaming_body`)
  — the 30s value killed EVERY >30s stream all along; the 15min global was
  likely never the binding constraint. PROXY_AGENT now sets NO recv-response
  timeout (connect 10s + global 2h backstop only).
- Review findings folded in: sniffer also terminates on the Responses API
  top-level `error` stream event (`event: error` / `data: {"type":"error"`);
  residual format-drift 2h-linger documented as accepted tradeoff.
- Live-proved: 48s / 793KB single stream → `completed` clean close (old
  binary died at 29s).
- Config timeline: AX rolled `~/.codex/config.toml` back to
  `.bak-pre-proxy` (05:02, direct mode); the re-appended
  `[model_providers.clauth]` DEFINITION block was commented out at 05:11 —
  **by AX manually** (CORRECTION: first attributed to zylos on
  timing+file-header inference without verification; AX confirmed it was
  their own hand. No agent contests this file — zylos only manages its
  project trust entries per the header). `~/.codex/proxy.config.toml`
  overlay exists for opt-in routing (`codex --profile proxy`, sibling of
  direct.config.toml); the ccsbar PROXY-1 toggle edits config.toml freely.

## 2026-07-18 (SCW-1) — per-model weekly windows join the claude chain walk

- Trigger: Anthropic's "7d fable" per-model weekly window (~half the
  aggregate weekly). Live shape: ax-cl at 7d 65% / 5h 0% but fable 100% —
  the aggregate-only walk scored it healthiest and stranded fable sessions.
- Shipped (797bff8): two-tier walk, no new config. Tier 1 = clear on
  aggregate gates AND every live scoped window; tier 2 (old acceptance) only
  when no fully-clear member exists. Healthy-but-scoped-blocked active hops
  only to a fully-clear member (no ping-pong). find_recovered_member prefers
  fully-clear. Scoped windows NEVER count as full exhaustion — wrap-off Off /
  soonest_resume / recovery premise stay on the aggregate cap. 7 new tests.
- ccsbar PROXY-1 (98fe1e6): codex-page "Proxy mode" toggle — flips the
  top-level model_provider line in ~/.codex/config.toml (definition block
  always kept for session resume), bootstraps the proxy LaunchAgent, caption
  warns routed-but-not-serving; state re-read per panel open so any outside
  edit of the file is visible. Pure transforms unit-tested.

## 2026-07-18 (TSW-1) — timeout-semantics sweep (post-PROX-2 class hunt)

- Swept all 11 timeout/deadline sites in clauth + ccsbar + ccu against the
  pinned ureq-3 fact (recv_response counts through the body). Fixed
  (8083d5f): mcp serve() gc moved off the initialize path (BUG — 3s probe
  killed healthy servers while gc deleted multi-GB stale isolated trees);
  claude.rs link/force_link Keychain write now precedes the symlink swap
  (ordering BUG — failed ACL-prompt write stranded link=new/Keychain=old);
  loopback OAuth catcher 10s→60s (could drop the real callback + its
  single-use code); pricing 15s→60s, status feed 10s→30s (through-body
  semantics on multi-hundred-KB fetches); update.rs caution comment (dead
  FORK_BUILD code, exact incident shape if re-enabled). ccsbar (8285a47):
  truncated daemon reply now surfaces as connectedNoReply instead of
  misreading as unreachable → CLI double-apply. Verified SAFE: proxy client
  socket, daemon socket, doctor, lock chain 25s/20s/30s, oauth AGENT, ccu.

## 2026-07-18 (SCW-1 upstreamed) — issue #54 + PR #55

- `feat/scoped-weekly-walk` (36affbe) off current upstream/mommy (f524a8a).
  Adaptation: upstream's candidate walk already carries a FRESHNESS
  preference pass — composed as a 4-tier stack (scoped-clear+fresh →
  scoped-clear → fresh → any; scoped-clear outranks freshness: known
  model-block > stale-read uncertainty). Upstream ChainMember grew
  `max_spend` (spend-budget feature) — test initializers updated. proxy/
  sse.rs hunk dropped from the pick (fork-only). 1114 tests green, clippy
  clean. NOTE: upstream mommy moved again (3 TUI/docs commits post-2497a36)
  — PRs #51/#53 may need another conflict check.

## 2026-07-18 (SCW-2) — PR #55 reworked to per-account gates (maintainer review)

- uwuclxdy on PR #55: wants per-account Fallback-tab toggles, not a global
  preference — "weekly usage" / "scoped usage" checks switchable per member
  ("users may still want to use that account with other models or exclude
  only the maxxed out fable account from rotation"). Implemented on
  `feat/scoped-weekly-walk` (3d312a1, after merging mommy 6a793d5):
  `Profile::check_weekly` / `check_scoped` (default ON; config.toml keys,
  absent = on), ChainMember mirrors, TUI rows `weekly gate` / `scoped gate`
  (11-col labels; maintainer's wording lives in the state-flipping hints).
  SEMANTICS CHANGE vs SCW-1: scoped-blocked (gate on) is now a HARD
  rotation exclusion — the 4-tier preference stack collapsed back to
  2 passes (clear+fresh → clear) with a stronger accept; `check_weekly`
  off lifts the soft weekly line only (`weekly_line()` — the 100% hard cap
  always blocks); scoped still never counts as full exhaustion (wrap-off /
  soonest_resume / recovery premise stay aggregate-keyed); recovery still
  relinks a model-blocked member as last pick. New `BlockedReason::
  ScopedSpent` chip (◇, "7d fable 100% · other models ok"). 1152 tests
  green, clippy clean. Replied on #55: defaults + hard-cap floor flagged
  for veto; weekly-limit-per-account answered as "override with global
  default, follow-up PR" (rotate-at pattern).
- FORK NOTE: fork main still runs SCW-1 preference-stack semantics — the
  SCW-2 gate rework is upstream-branch only until AX wants it adopted.

## 2026-07-18 (CLA-SPLIT-2) — #53 follow-ups into the same PR + fork + ccsbar

- PR #53 follow-ups implemented on `feat/session-token-split` (2022c4e,
  after merging mommy) and cherry-picked to fork main (f55146f; conflicts:
  fork's --new/--codex login flags + account_email row composed with the
  new --setup-token/--yes + session row):
  * `clauth login <p> --setup-token [--yes] [--model <id>]` — capture a
    `claude setup-token` mint into session-token.json: echo-off rpassword
    paste on a TTY, ONE stdin line when piped (the GUI/script path);
    validate_setup_token (sk-ant- prefix, no interior whitespace, ≥40)
    before any write; atomic 0600; expiresAt stamped now+365d (documented
    lifetime — the mint carries no expiry); scopes recorded. Additive:
    never touches the live slot (takes effect on next switch), usage pair/
    env/chain/models survive; new name → blank profile; existing sidecar →
    confirm or --yes; --setup-token excludes api/codex/--new at parse.
  * Setup-tab `session` row: "static token · expires in ~Nd" (WARNING ≤30d,
    DANGER + "re-mint: claude setup-token" when expired, "no recorded
    expiry" for hand-rolled sidecars). session_token_expiry() reads only
    the sidecar. Upstream branch 1143 green; fork 1294 green; clippy clean
    both. Deployed: cargo install + daemon/proxy restarted.
- ccsbar (01cbcee): context-menu "Install/Replace session token…" → inline
  SecureField banner (validation mirrors clauth's; token piped to
  `clauth login <p> --setup-token --yes` stdin — NEVER argv/ps/logs);
  DetailCard shows the sidecar state under the account email (direct
  sidecar read, CodexProxyMode idiom — daemon-down safe; only expiresAt
  decoded). LoginMode.setupToken joins the single-login guard + flight
  banner. 210+5 tests green; repackaged + redeployed to /Applications.

## 2026-07-19 (WKO-1) — weekly override + long-lived detection/rename + full ccsbar integration

- PR #55 (b12ec28): folded the maintainer's weekly-limit-per-account ask in
  as an OVERRIDE (`weekly at` row; Config-tab `weekly limit` stays the
  chain default; empty commit clears). One per-account line governs the
  aggregate + scoped judgments; NEVER the hard-cap ones. Predicates split
  soft (member_weekly_line/member_scoped_line) vs hard (weekly_hard_blocked/
  is_exhausted_hard); ChainMember snapshots resolved weekly_line/scoped_line;
  is_exhausted_active takes the caller's weekly bool. 1159 green.
- PR #53 (2244a46): maintainer's remaining ask — content-aware long-lived
  detection. `session_token_status`: long-lived IFF no refresh token; a
  mis-filled rotating pair DISENGAGES the split (install source back to
  credentials.json, gate logs the re-capture hint, Setup row shows DANGER
  "not long-lived (has a refresh token) · ignored"). NAMING (AX): user-facing
  term is now **long-lived token** everywhere (claude setup-token's own
  wording; "session token" reads as the opposite) — TUI row keyed `token`;
  sidecar filename session-token.json unchanged. 1144 green.
- FORK main ADOPTED SCW-2+WKO (a108ae0..51c177b): hand-port, not cherry-pick
  (fork fallback.rs diverges — codex harness chains, no freshness pass, no
  spend budget, no burn floor/horizon, wrap_off naming). Codex ChainMembers
  get inert neutral lines. Fork now runs the per-account gate semantics live.
  Daemon grew socket verbs `set_member_weekly` (null clears) /
  `set_check_weekly` / `set_check_scoped` + status.json fallback fields
  check_weekly/check_scoped/weekly_threshold. 1304 green; deployed (cargo
  install + daemon/proxy restart; status.json verified carrying the fields).
- ccsbar (97e114b): context-menu "Weekly limit here ▸" (follow-default/
  presets/custom via ThresholdEditTarget.memberWeekly) + "Check weekly
  usage"/"Check per-model weekly (7d fable)" gate toggles; FallbackInfo
  decodes the new fields with old-daemon defaults (gates ON); long-lived
  rename across all copy. 210+8 tests; repackaged + redeployed.

## 2026-07-20 (UPS-1) — #53 MERGED · #55 review fixes · proxy advisory-rank · #47 committed

- **PR #53 MERGED upstream** (re-authored on mommy as c01a2f6 backend +
  d8105e1 TUI; closes #52). Maintainer stacked claude-driven fixes on merge;
  fork was missing three and has them now (0e3926e): force_snapshot guard
  (the confirmed divergence-Overwrite could clobber a long-lived profile's
  clauth-private usage pair — fork's CAP-1 shape guards the force path
  directly), token-row sub-day-expiry gate (`now >= expiry`, not truncated
  days — "~0d" WARNING mislabel), login completions full flag set
  (--codex/--browser/--setup-token/--yes) + SECURITY.md session-token row.
- **PR #55 CHANGES_REQUESTED → fixed** (eb8394a): 4 hard-cap sites judged at
  member-resolved soft lines after the member_weekly_line refactor —
  next_target's serving-sink passes (the MONEY divergence: TUI twin paid a
  spend-armed sibling while the daemon twin parked free on the sink),
  usage-tab weekly_hard diag, update_banner any_spent. All → is_exhausted_hard/
  weekly_hard_blocked. Added the exact-divergence tests (RED-verified on
  pre-fix code), `weekly at` in ? help modal + chain.rs module doc,
  cargo fmt reflow (CI red). 1161 green; replied on PR. Fork had ONE
  exposed site (any_spent banner; no spend/sink-serving passes) — fixed in
  the same 0e3926e.
- **Codex Plus→Pro incident root-caused** (x@computelabs.ai / ax-codex-cl):
  (1) usage stuck 100% = the window between the upgrade and the first
  DIRECT session — passive JSONL leg had only pre-upgrade events (weekly
  reset days out); self-healed at 14:09Z once direct sessions wrote fresh
  rate_limits (status.json now 1%). (2) proxy-mode "usage limit reached" =
  REAL BUG: cached_exhausted marked members unavailable → all unavailable →
  synthesized 429 without consulting upstream → and as sole usage writer
  while serving, the proxy could never observe the correction (wedge).
  FIXED (82ef6bd): PoolMember.cached_spent is an ADVISORY second tier in
  select_account/next_after_failure — cache-clear members first, spent
  members last, only authoritative exclusion (auth_broken/leased/real-429
  cooldown) 429s the client. proxy-design.md §1.5 synced. (3) tier shows
  "plus" = STALE AT SOURCE: live ~/.codex/auth.json last_refresh 07-16
  (pre-upgrade), claims still plus, access token valid to ~Aug 1 so codex
  won't refresh soon; adopt-back is healthy (stored copy 14 min fresh) —
  fixes itself on codex's next token mint; NOT a clauth bug.
- Fork 1309 green, clippy/fmt clean; deployed (cargo install, daemon PID
  81028 + proxy 81032 restarted, status.json verified). ccsbar 620a321:
  same sub-day-expiry truncation fix in SessionToken.statusLine + test;
  218 tests green; repackaged + redeployed (PID 84523).
- **Issue #47**: maintainer decided daemon should honor `on mismatch`
  (ask → TUI stays) and pinged for implementation. Replied YES — next PR
  after #55 lands: honor Overwrite/NewProfile/Discard through the guarded
  paths, propose 4th option `follow` (proven-sibling switch, port of fork
  layer 1). Rescue layer explicitly kept out of on-mismatch.
- Waiting on maintainer: #55 re-review (defaults still vetoable), #51 (he
  refactors first), #47 option-shape verdict.

## 2026-07-20 (UPS-2) — sustainable sync: true merge of upstream/mommy (70 commits)

AX call: "重新match一波upstream,不只是cherry pick,要 sustainable". Ended the
squash-rebase era — fork main now tracks upstream by periodic true merges
(contract in `docs/fork-sync/SYNC.md`, the durable artifact of this round).
Merge commit `c112469` on `sync/upstream-2026-07-20` → ff'd to main.

- Divergence at merge time: merge-base `1470147` (07-16), upstream +70 /
  fork +33. 32 conflicted files, ~90 hunks. Mechanical hunks applied by a
  per-file decision script; 39 custom hunks hand-composed off 4 parallel
  subsystem briefs (Workflow, opus) + 3 adversarial verifiers (all CLEAN,
  audit trails in the session's workflow journals).
- Adopted upstream wholesale: sessions subsystem (index/resume/info +
  isolated-session rescue), opt-in spend budget (max_spend, spend-armed
  passes, `switch_off_when_budget_spent`), single-usage-snapshot scheduler
  walk (0ba3538 — the free-vs-paid race fix), burn floor/horizon bounds,
  fresh-read two-pass preference, canceled-subscription exclusion,
  settings sync + jsonsync + burn.rs, apiKeyHelper, owner-only dirs,
  wrap_off→switch_off_when_spent rename (status.json key stays `wrap_off`),
  TUI rework (config banding, block-reason pills, session_token_lines,
  reset clocks, max-spend editor).
- Fork re-expressed on the new shapes: SCW-1/2 folded lines ride
  ChainMember beside max_spend; `scoped_blocked_from_usage` replaces the
  store twin; walk = headroom(fresh→any, scoped-gated, member soft lines) →
  serving sink(hard) → spend → dead sink → halt(hard), verified pass-for-pass
  identical across both twins. THREE upstream literal-100 sites converted to
  hard predicates (the #55 bug class under fork folding — `blocked_reason`
  WeeklySpent + both next_target sink sites). `clauth resume` collision:
  known codex profile → carryover, else upstream session resume
  (`cmd_resume_dispatch`). settings sync + env-key union skip codex-harness
  dirs (`is_codex_profile_dir`, new test). Overview route column + usage-tab
  active pill/account rows + keychain-first ordering + CLA-SPLIT guards all
  survived (adversarially verified, not assumed).
- Evidence: 1538 tests green (both parents' suites — upstream's spend tests
  pass against the composed walk, fork's scoped tests against the snapshot
  shape), clippy 0, fmt clean; release binary smoke-tested read-only
  (status.json carries schema 1 + fork fields + 6 profiles; completions
  carry sessions/resume). Deployed same session (daemon + proxy restart).

## 2026-07-21 (CLA-SPLIT-3) — mis-filled sidecar incident: "Login expired" while ccsbar read healthy

Live incident (AX report, ~03:29Z): claude sessions died "Login expired ·
Please run /login" while ccsbar showed ax-main Fresh + "Long-lived token ·
expires in ~342d". Root cause chain, forensically pinned:

1. 07-18 10:05:45Z batch fill wrote all THREE session-token.json as full
   credential-shaped JSON — the RIGHT mints (hash-verified vs
   ~/.claude-fleet/claude_long_live_tokens.env: 1f1b7d5a/d59c050e/fb740963)
   but with a spurious refreshToken + scopes + subscriptionType template.
2. 07-19 genuinely-long-lived detection (159224e, the #53 maintainer ask)
   classifies refresh-token-present sidecars NotLongLived → silently
   DISENGAGED the split on all three profiles retroactively.
3. Switches then fell back to rotating pairs (daemon loglines 07-20 07:41Z
   ax-backup, 14:02Z ax-main) — CC sessions and clauth's usage leg back on
   ONE single-use refresh chain, the exact class CLA-SPLIT exists to kill.
4. 21:19:58Z: the shared pair's access token expired; clauth's usage leg
   rotated first (stored+live-symlink updated — the mtimes are
   microsecond-identical because ~/.claude/.credentials.json is a SYMLINK
   into the profile store); CC's copy of the chain was left spent → its next
   refresh attempt died → Login expired. AX /login'd at 03:30:36Z (Keychain
   acct=xingfanxia mdat matches to the second).
5. ccsbar could not surface any of this: SessionToken.state decoded ONLY
   expiresAt, so a mis-filled sidecar displayed as a healthy ~342d countdown.

Fixes (all deployed 07-21):
- Sidecars re-captured via `clauth login <p> --setup-token --yes` (stdin
  pipe from the fleet env, values never printed); verified pure-mint shape
  {accessToken, expiresAt, scopes} + hash match. Split arms at each
  profile's next switch (AX-manual). Meanwhile the /login chain is safe:
  has_session_token=true now short-circuits the rotation Keychain-mirror.
- ccsbar 7ebe3f9: new SessionTokenState.misfilled (mirrors clauth's
  refresh-token check) → DANGER "mis-filled (rotating pair) — not in
  effect; re-capture", never the stamped countdown. 210+11 tests green,
  deployed + relaunched.

Forensic facts worth keeping: CC on this Mac reads/writes Keychain item
acct=xingfanxia svce="Claude Code-credentials" (an acct=unknown shell from
05-31 is a dead item — ignore it in future forensics); the live credentials
file is a clauth-owned symlink, so "live file == stored" is definitional,
not evidence of a write.

CLA-SPLIT-3 addendum (60f99ea): the sidecar repair itself exposed a second
predicate deadlock — with the install source flipped to session-token.json,
the STALE live symlink (→ credentials.json) classifies Diverged, the
unsaved-credentials gates routed the switch through archive_live_credentials,
and the archive refuses symlinks by design → every switch failed "nothing
unsaved to archive" and deferred forever (AX's ccsbar switch to ax-backup
hung live). Fix: live_login_is_clauth_symlink() exempts clauth-owned
symlinks in both unsaved gates (daemon + TUI) — a symlink's content is a
profile store by construction, nothing unsaved; regular-file live logins
(a real CC /login) archive exactly as before. Regression test pins the
Diverged-but-not-unsaved transition state. 1539 tests, deployed.

CLA-SPLIT-3 upstreamed: the stale-symlink switch-deadlock fix ported onto
upstream/mommy (upstream shares the full shape via merged #53 — their daemon
retries "unsaved credentials" to TTL and the TUI false-prompts; repro is
their own `--setup-token` on the active profile) and opened as PR
[#58](https://github.com/uwuclxdy/clauth/pull/58)
(`fix/stale-sidecar-symlink-divergence`, 1237 tests green). #51/#55/#47
needed no update: #51's head is fork main (auto-updated by today's pushes),
#55 still awaits re-review, #47 unchanged.

## 2026-07-21 (CLA-FEED-1) — daemon-fed session token: Fable-capable long-lived slot

Why: the CLA-SPLIT static setup-token carries only `user:inference
user:sessions:claude_code`, no `subscriptionType` — Claude Code's Fable 5
gate refuses it ("requires usage credits"). AX's "前两天还可以" window was the
mis-fill regression running vanilla (Fable + race deaths); the durable answer
is CLA-FEED (AX picked option B, then the design simplified past dual-grant
when the usage chain turned out to already BE a full-scope max mint — see
docs/cla-feed/DESIGN.md).

Design in one line: the daemon feeds session-token.json with the usage
chain's current access token (real expiry, full scopes, subscriptionType, NO
refresh token) — sessions get Fable-capable bearers, the classifier stays
LongLived so every split guard is unchanged, the refresh chain never leaves
clauth custody, and CC picks up re-stamps because it re-reads the Keychain
per request (verified on-device 2026-07-07, oauth.rs rotation-coherence #1).

Shipped (fork main):
- `Profile.session_feed` (config.toml, default off) + `clauth feed <p> on|off`
  (validates chain Fable-capability, arms immediately, installs live when
  active; off restores the static mint).
- Rotation hook in `apply_rotated_tokens_locked`'s CLA-SPLIT quiet branch:
  feeds on EVERY rotation (parked included; absent sidecar arms; NotLongLived
  mis-fill never overwritten); active profiles ship the fed sidecar through
  the existing post-flock `mirror` Keychain write. State flock is re-entrant
  per-thread (lock.rs:10) so the nested feed write is safe.
- `ensure_installable` feed branches: fresh fed sidecar Ready; stale one
  re-feeds from the stored chain (no spend) or the guarded refresh leg (its
  persist re-feeds via the hook); absent sidecar arms instead of falling
  through to the vanilla-pair install; terminal chain death restores the
  static mint (Ready, degraded) else Broken. `has_live_session` bail exempted
  for feed (fed sessions never advance the chain).
- Static-mint preservation: first feed copies a genuine mint (no refresh, no
  subscriptionType, ≥30d horizon) to `session-token.static.json`; re-mint on
  a feed profile refreshes the backup; `restore_static_mint` round-trips.
- Scheduler: `proactive_rotation_due` feed override (active feed profiles
  rotate inside the lead window regardless of the global toggle or Keychain
  mirror — off macOS the symlinked sidecar IS live).
- Surfaces: status.json additive per-profile `session_feed`; TUI token row
  renders `fed · refreshes in ~Nh` (accent) vs the mint's 30d warning ramp,
  `feed stalled` DANGER when expired; help/completions.
- Tests: 1554 green (feed gate truth table, rotation hook, mint
  preservation/restore, config round-trip, proactive truth-table rows,
  status key inventory). clippy 0, fmt clean.

ccsbar (CLA-FEED-2, pending): render `session_feed` from status.json so a fed
token shows as maintenance, not an expiring mint.

CLA-FEED-1 review round (adversarial workflow, 3 opus dimensions × sonnet
refuters; 10 findings, 9 confirmed, all fixed):
- ensure_installable restructured: feed profiles dispatch to a single
  `feed_install_gate` (vanilla gate restored byte-identical) — a mis-filled
  sidecar HEALS (evidence quarantined to ~/.clauth/quarantine/, static mint
  restored) when a backup exists; without one the disengaged-vanilla posture
  stands, loudly (pinned by two new gate tests).
- Pinned-pair double-spend closed twice: the scheduler's live-session bail
  exemption now requires an ARMED sidecar (`session_feed &&
  has_session_token`), and `clauth start` arms the feed from disk
  (`arm_feed_from_disk` in canonical_credentials, guard-serialized,
  best-effort) so a session can no longer be launched onto the rotating pair
  inside an arming window.
- TOCTOU class: `write_session_token_with_backup` stamps mint+backup from one
  serialized byte buffer in one flock section (re-mint path; replaces the
  read-back `refresh_static_backup`); `feed_from_stored_chain` takes the
  RotationGuard as witness; `clauth feed off` (flag flip + mint restore)
  serializes under the guard.
- `feed_install_gate` refuses to spend when a live session holds an un-armed
  profile (Transient, actionable message); `arm_session_feed` reports arming
  failure instead of a false OK when the sidecar didn't arm.
- Keychain mirror hardening staged: refresh-none content belt on the fed
  mirror; absent-sidecar feed profiles never ship the pair (NotLongLived
  mis-fills deliberately keep the vanilla mirror — that's what keeps CC alive
  while disengaged, CLA-SPLIT-3).
Post-fix gate: 1557 tests green, clippy 0, fmt clean.

CLA-FEED-1 deploy incident (2026-07-22 01:30Z, AX report "切backup失败+被登出"):
a STALE-BINARY WRITER wiped `session_feed` from all three config.tomls hours
after enablement — a `clauth` TUI running since 07-18 (tmux, pre-CLA-FEED
image; binary replaced under it 07-21) plus the un-restarted proxy. Old
ProfileConfig doesn't know the key, so any of its config rewrites drops it
(wipe mtimes 20:11Z/21:23Z/01:31Z each match a daemon "external change"
logline). Flag off → nobody re-fed → ax-backup's fed token expired under the
live session → CC 401 with no refresh token → "Login expired" (AX /login'd);
daemon benched backup ("long-lived token has expired" — the vanilla gate
reading the dead fed token as an expired mint) and auto-switched to ax-main
(static mint → Fable gone). RECOVERY: killed the stale TUI, restarted the
proxy (new image), re-ran `clauth feed on` for backup/cl (re-fed 7h/2h
tokens), re-set ax-main's flag by hand (daemon arms it on next rotation).
LESSONS: (1) binary replacement must restart EVERY clauth process — daemon,
proxy, AND any long-lived TUI (tmux windows!); pgrep by image age, not by
role. (2) config.toml unknown-key wipe by old binaries is a standing hazard —
follow-up: preserve unknown keys in maybe_rewrite_config_toml (toml_edit
round-trip) so future flags survive stale writers.

## 2026-07-22 (CDX-6) — read-only wham/usage polling: parked codex accounts get live usage

AX report: parked codex accounts' usage froze at their last live session
(ax-codex-xfx wore "week spent" days after its weekly reset). Investigation
(3 sibling projects source-read): everyone polls
`GET chatgpt.com/backend-api/wham/usage` with the stored access token — no
live session needed — and codex CLI itself polls the same endpoint ~60s
(openai/codex#10869). AX REVERSED the feasibility §2.5 ban (option A;
cadence "那我们也每分钟刷新"). Bounding fixes shipped first: walk was already
reset-aware; ccsbar spent-pill made reset-aware (b702841).

Shipped (fork main):
- `src/codex/poll.rs`: tolerant wham parse (rate_limit|rate_limits,
  primary_window|primary aliases, resets_at|resets_in_seconds normalize)
  through the SAME route_windows slotting as the passive leg; PollError
  {Unauthorized, RateLimited, Other}; bare headers (Authorization + Accept +
  ChatGPT-Account-Id — no invented UA). READ-ONLY invariant: never refreshes,
  never writes auth; 401 waits for CDX-3.
- `codex_poll_tick` in the scheduler (lease-holder leg after standby): all
  codex profiles, 60s/profile cadence, clock-expired tokens stand down,
  Unauthorized widens 15m, shape-drift widens 1h, same-error log dedup;
  publishes via cache+store+status like every leg.
- `AppState.codex_usage_poll` kill switch, default ON.
- Docs: feasibility §2.5 dated reversal note (accounts/credit endpoints stay
  banned), proxy-design + PLAN non-goal annotations, usage.rs module doc,
  SYNC.md CDX-1..6.
- Tests: 4 parse + 4 tick orchestration (offline, injected poll seam);
  1566 green, clippy 0.

CDX-6 addendum (live-shape + plan tier): first deploy revealed the REAL wham
dialect (reset_at / reset_after_seconds / limit_window_seconds, verdict
top-level) — parser now accepts both dialects, live shape pinned as fixture
(a0a5766). Second AX report: tier label stale after a plan upgrade (plus→pro;
the id_token claim only re-mints on codex's own refresh) — wham's top-level
plan_type now rides the poll into CODEX_PLAN_CACHE_FILE (written on change
only) and tier_label prefers it over the claim. Verified live: xfx 7d=0%
(the reset the stale cache hid), cl 11%, all Fresh at 60s cadence.

## UPS-3 — upstream review rounds served: #58 r2, #55 r2 (rebase), #51 design answer (2026-07-21)

All three open upstream threads carried CHANGES_REQUESTED / questions; both
fix rounds pushed + replied, standing contribute-back authorization.

#58 (fix/stale-sidecar-symlink-divergence, b53226d append, no force):
- reviewer: exemption reached 2 of 5 gates + no defer regression test.
- one `claude::live_diverged_and_unsaved(active)` owns the triple + symlink
  term; daemon/TUI gates + switch_profile_cli + switch_profile_noninteractive
  route through it. poll_credentials_divergence keeps its cascade with the
  atomic symlink early-return (adopt side-effect interleave + 1Hz read cost)
  — deviation disclosed in the reply.
- drain_pending_switch_proceeds_over_a_stale_clauth_symlink +
  divergence_poll_ignores_a_stale_clauth_symlink; both mutation-checked red.
  Delegated to a worktree agent; 1239 green.

#55 (feat/scoped-weekly-walk, eb8394a → 51338fb, force-with-lease after the
requested REBASE onto moved mommy — 4 commits replayed over the store→usage
snapshot rework, ROWS_BEFORE→rows_start, ⊖ consolidation, canceled axis):
- rebase ports: scoped_blocked_from_store→_from_usage, member-based
  is_exhausted_from_usage(member, usage, line), active_canceled joins the
  trigger condition, member_detail 10-arg signature sweep (19 call sites).
- findings fixed on top (3 commits): member_scoped_line rides check_weekly
  (override inert when gate off; scoped stays judged at chain line);
  !active.last_resort parking guard on BOTH scoped triggers (oscillation);
  chip routes worst_scoped_window (= scoped_weekly_blocked_info, no second
  opinion); parse_weekly_override→parse_weekly_pct (50..=100); load boundary
  RESETS out-of-band weekly_threshold to unset (never clamps — 0.98 typo
  class) + kills nan; focused member card scrolls cursored row into view
  (caret subtracts scroll; 40x24 remove-row reachable); ScopedSpent rides ⊘
  warning (◇ retired); edit weekly at naming; default_reminder reuse.
- every discriminator mutation-checked red (6 mutations); 1368 green,
  clippy 0, fmt clean. Both rounds' replies posted as issue comments.

#51: maintainer asked for a clauth mode-switcher design (c codex/c claude).
Answered from fork experience: harness = PROFILE AXIS not app mode;
per-harness active/chain/pending-queue; dispatch points (switch mechanics
session-boundary vs link, single-use rotating refresh single-writer, wham
usage dialect, feature GATING not porting); flagged isolated-CODEX_HOME
auto-start as the missed item; offered incremental upstreaming (axis first)
after #55/#58 land. #47 follow-up (daemon honors on-mismatch + follow)
stays queued behind #55. No new PR opened — nothing else is ripe.

## UPS-4 — #55 MERGED; #58 round-3 (macOS regular-file mirror) (2026-07-22)

- **#55 MERGED into mommy 2026-07-22T13:12Z** (per-model weekly windows +
  per-account gates + `weekly at` override). Fork main already carries the
  feature (SCW-1/2 hand-port); next fork↔upstream sync reconciles to the
  merged form per docs/fork-sync/SYNC.md. #53, #49 previously merged.
- **#58 round-3** (e66b9cb append, no force): maintainer traced a deeper
  macOS steady-state bug — the round-2 exemption keyed on `is_symlink()`, but
  CC rewrites the live slot as a regular-file Keychain mirror after every run,
  so once a sidecar flip makes classify read Diverged the switch defers again.
  Fix: `live_login_is_clauth_symlink` → `live_login_is_stored(active)` =
  content match against BOTH stored files (credentials.json + session-token.json;
  covers both flip directions) OR a structural symlink clause (dangling case).
  SECOND site found via an end-to-end daemon test: `switch_profile`'s
  `uncaptured_relogin` ran the same triple WITHOUT the exemption → macOS mirror
  took the guarded-link path and byte-rejected the switch even after the gate
  passed. Routed it through `live_diverged_and_unsaved` too. Regression: cross-
  platform regular-file-mirror cases on helper + daemon-drain + 1Hz-poll (all
  red without the content half; daemon also reds if uncaptured_relogin keeps the
  raw triple), symlink cases kept, dangling-symlink assertion added. 1242 green,
  clippy/fmt clean. Replied disclosing the both-stores deviation + the second
  site. Awaiting re-review.
- **#51** still awaiting maintainer on the mode-switcher design answer (harness
  = profile axis). No new PR ripe.

## UPS-5 — #58 MERGED; #51 codex-mode proposals answered (2026-07-23)

- **#58 APPROVED + MERGED 2026-07-23T09:08Z** ("This is it... lgtm ✅").
  Maintainer endorsed the round-3 content-based rework verbatim, including
  the both-stores deviation ("strictly wider, same cost") and the
  uncaptured_relogin second-site fix he'd waved through as fine in R1/R2.
  Both #55 and #58 now in mommy → fork↔upstream sync is DUE (true merge,
  reconcile #55 to merged form + #58 to merged form, hard-cap sweep) per
  docs/fork-sync/SYNC.md. Not started.
- **#51: maintainer moved the codex-mode design forward** (2026-07-23):
  proposed CODEX_HOME repoint to profile dirs + `clauth start {codex
  account}`, asked to rebase the branch on v0.14 when it lands, asked us to
  review his codex-mode changes, and proposed `{name}-cc`/`{name}-c` dir
  suffixes. Answered from CDX-1b experience
  (https://github.com/uwuclxdy/clauth/pull/51#issuecomment-5063434276):
  CODEX_HOME → `profiles/{name}/codex-home/` subdir (auth.json seeded from
  store, config.toml COPY not symlink, isolated sessions/) + the sharp edges
  (canonicalize, env scrub, file-store-mode check, TWO-CARRIER refusal,
  60s adopt-back watchdog); v0.14 rebase agreed via a dedicated branch cut
  from v0.14 ported incrementally (PR head is fork main — never rewritten in
  place); suffixes: keep typed `harness` field authoritative (suffix =
  naming convention, not parsed discriminator), suffix only the codex side
  for back-compat, offer `-cx` over `-c`. Awaiting maintainer.
- **#47 follow-up** (daemon honors on-mismatch + `follow`) now fully
  unblocked (#55 and #58 both landed). Not started.

## UPS-6 — fork↔upstream sync: v0.13.0 + v0.13.1 merged (2026-07-23)

- **True merge `45f2f43`** (sync/upstream-2026-07-23 → ff main, pushed; PR #51
  head auto-updated). ~60 upstream commits, 34 conflicted files.
- **Reconciled to merged forms**: #55 (fork's SCW hand-port → upstream's
  ChainMember snapshot shape, `is_exhausted_from_usage(member, usage, line)`,
  `worst_scoped_window`, parse_weekly_pct band, weekly-at scoped-line gate
  semantics) and #58 (fork's round-1 `live_login_is_clauth_symlink` DELETED →
  upstream's final `live_login_is_stored` + `live_diverged_and_unsaved`
  everywhere).
- **Adopted upstream features, gated through fork axes**: account-disable
  toggle (codex backstop lives in `ensure_switch_target_ok` — codex bail
  BEFORE disabled check; fork route/email cells joined the disabled dim
  flatten), daemon singleton cap (`--status`/`--standby`/`--replace`, pid
  sidecar; fork per-harness switch_backoff map kept), clap-derive CLI
  (`src/cli.rs` extended with Fallback/Feed/Proxy/Doctor variants + login
  `--new`/`--codex`/`--browser`; hand parsers deleted; print_help deleted),
  d954fb6 non-login capture guards PORTED into fork's CAP-1 shape (non-force
  None leg no longer clears the store; force path skips absent/shell; CAP-1
  first-login leg now requires a non-empty token — upstream's 92c56e7),
  5h post-reset repoll, walk_excluded shared skip predicate, finite_pct /
  weekly reset-not-clamp load normalization.
- **Test reconciliation strategy** (the bulk of the work): per-file whole-file
  rebuilds — upstream base + fork-only blocks (claude, cli, fallback, oauth,
  profile, tui_render_format/overview/usage) or FORK base + upstream blocks
  where src kept fork shapes (daemon_mod, daemon_status_json); scheduler
  recovery tests adapted HashSet→VecDeque; upstream tests adapted to fork
  signatures (render_overview_row 6-arg, OverviewWidths 3-arg, HeaderState
  is_active/account_email, LiveSignals last_error/last_switch, build_status
  4-arg, per-harness stage_switch). Superseded fork tests dropped (round-1
  #58 suite, hand-parser CLI suite, blocked_pill PILL_LINES test).
- **Deploy**: plist template + live LaunchAgent now run `daemon --standby`
  (upstream's new exit-if-running default would strand a launchd respawn race
  as a clean exit KeepAlive never restarts). Binary replaced via fresh inode
  (in-place cp = SIGKILL, macOS signature kill). Daemon (pid --standby) +
  proxy restarted, status.json fresh, all fork wire keys verified present.
- Gates: 1773 green, clippy 0, fmt clean, hard-cap sweep clean.

## EXP-1 — login-expiry root-cause research (3-agent, 2026-07-23)

Reports: external (codexbar/codex-auth/CRS/cc-switch + vendor issues), local
forensics (daemon.log 07-10..23 + metadata-only cred inspection), codex-auth
deep study. Consolidated verdict:

- **Universal law** (vendor-confirmed both sides): refresh tokens are
  single-use rotating with reuse detection — openai/codex #15410/#15502/#6498
  (codex), claude-code #24317 + CodexBar #1161 (claude). Access-token TTL is
  NEVER the killer; a second holder of the chain is. Mature projects converge
  on "exactly one writer rotates-and-writes-back; everyone else read-only".
- **Codex `ax-codex-cl` death (07-23 20:36) fully explained**: parked chain
  minted 07-16, externally rotated/revoked (~07-23 09:27 wham 401); direct-mode
  default bypasses the proxy so adopt-back never saw the rotation; RC-2: standby
  refresh is AGE-scheduled (48h-to-expiry / 7d) — a wham 401 does NOT
  accelerate it (scheduler.rs:2751-2758 uncoupled from codex/oauth.rs:181-194),
  so the death stayed masked ~11h; RC-3: auth_broken = no self-recovery.
- **Claude "revoked" deaths (07-16..22) = CLA-SPLIT-3 regression** (split
  disengaged by mis-filled sidecar → CC raced the usage chain). Healed on disk
  07-21/22; 1.6 days clean since. Ongoing exposures: RC-B external co-consumer
  of the usage chain (structural, non-fatal to sessions post-split) and RC-C
  fed-sidecar 7h clock-death across daemon idle gaps (re-feed is ONLY a
  rotation side effect, oauth.rs:1040-1066; no independent timer; multi-hour
  idle windows + fetch-lease contention observed). Discriminator for the next
  death: "refresh token revoked" = RC-B; "login expired / long-lived token
  expired" = RC-C.
- **codex-auth study**: clauth ahead on all core mechanics (locking, rotation
  guard, isolated homes, proxy fallback); worth adopting = versioned --json
  contract discipline, bulk `import --codex`, `switch --previous`; explicitly
  NOT: lockless file-copy swap, codext CLI fork, config injection, curl client.
- **Proposed fixes (not started)**: F1 health-triggered codex standby (wham
  401 → immediate guarded refresh attempt); F2 CLA-FEED independent re-feed
  timer + wake-from-sleep check (+ verify clock-death degrade to static mint);
  F3 codex self-heal re-capture (live ~/.codex login matching an auth_broken
  profile's account → auto re-adopt); F4 codex-auth UX adoptions.

## EXP-2 — F1+F2 implemented: 401-kicked codex standby + claude re-feed timer (2026-07-23)

The two "small and certain" fixes from EXP-1, shipped together:

- **F1 (codex, RC-2 fix)**: CDX-6 poll's `PollError::Unauthorized` now inserts
  the profile into `SchedulerState.codex_auth_kicks` (shared leaf mutex); the
  CDX-3 standby tick drains kicks BEFORE its scan gate and calls
  `codex_refresh_parked(..., force: true)`. `force` bypasses ONLY the
  `standby_due` age check (the 401 is server-side proof of death); candidates
  membership (parked/clauth-held — live owners excluded), RotationGuard, and
  the apply-time chain-identity re-check all stay. Kicks bypass both the scan
  cadence and the 6h transient widening (poll's 15-min unauthorized cadence is
  the upstream rate limit). Permanent → auth_broken as before, but now within
  ~1 poll interval of the revocation instead of masked for days.
- **F2 (claude, RC-C fix)**: new scheduler leg `claude_feed_tick` (5-min scan,
  lease-holder only) + `oauth::refeed_session_token`: for every feed-enabled
  profile whose fed sidecar is within `FEED_REFEED_HORIZON_MS` (2h) of clock
  death, run the FULL existing feed decision table — no-spend re-stamp from a
  comfortable chain / guarded refresh / mint degrade — via a new
  `fresh_horizon_ms` param threaded through `feed_install_gate`,
  `feed_from_stored_chain`, `gate_under_guard`. Switch-time callers pass the
  old `AUTH_GATE_GRACE_MS` (60s) so switch behavior is byte-identical. Active
  profile no-spend re-stamps also mirror the refresh-less bearer to the
  Keychain under the config mutex (the rotation-hook discipline; running
  claude re-reads Keychain per request). Due predicate is stateless wall-clock
  → machine-sleep gaps self-correct on the first tick after wake; predicate
  (exp <= now+H) and gate freshness (exp > now+H) are complementary, so no
  scan no-op loop.
- Tests: +10 (4 kick orchestration, 3 feed-tick orchestration, 3 refeed
  gate-level incl. the switch-gate-vs-refeed contrast). 1783 green, clippy 0,
  fmt clean, hard-cap sweep clean.
- Still open from EXP-1: F3 codex self-heal re-capture (CDX-7 scale), F4
  codex-auth UX adoptions (json contract / import / switch --previous).
  F4-routing decision (AX delegated): direct default STAYS; RC-1 exposure is
  accepted until F3 lands (self-heal re-capture is the mitigation, not proxy
  re-route).
- Review round (opus code-reviewer): 0 CRIT / 0 HIGH / 1 MED / 4 LOW / 1 NIT,
  all fixed: MED = kick circuit breaker (`kick_streak` in CodexPollPacing,
  cap 2 consecutive 401s without a poll success — a wham-only auth quirk can
  no longer rotate a healthy chain every 15 min forever; genuine dead chains
  still exit via Permanent→auth_broken); LOW = degrade-masked Ready now paces
  like a transient (still-due after Ready → FEED_RETRY_MS widen), poisoned
  pacing can no longer drop drained kicks (gate checked before drain),
  refeed Keychain mirror holds the config guard across the write
  (rotation-hook discipline), all-feed-profiles scan documented as intended;
  +3 tests (kick-permanent termination, breaker stand-down/re-arm,
  degrade pacing) → 1786 green.
