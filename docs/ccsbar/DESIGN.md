# ccsbar — Design & Architecture Spec

Native macOS menu-bar companion for `clauth` (Claude Code account switcher).
Grounded in CodexBar (aesthetic/arch north star), CCSwitcher (feature floor +
anti-patterns), and clauth's actual data surface.

> **Contract consumers:** the `status.json` / `tokens.json` shapes specified
> here are read by TWO clients — ccsbar (`~/projects/devtools/ccsbar`, macOS
> menu bar) and **ccu** (`~/projects/devtools/ccu`, read-only terminal viewer
> for phone-width mosh/ssh sessions). Any shape change must stay additive for
> both; each carries its own schema gate.

> **Status:** Phase R (the Rust `clauth daemon` + `status.json` contract) is done
> in this repo. Phase S (the Swift menu-bar app) is built in the separate
> `~/projects/devtools/ccsbar` repo. **The shipped app pivoted from the
> `NSMenu`-hosting plan below to a SwiftUI `MenuBarExtra(.window)` panel** —
> current CodexBar uses exactly that, and the translucent SwiftUI panel reached
> the target polish without the `NSHostingView`/`intrinsicContentSize` machinery.
> §2 keeps the original `NSMenu` analysis as the superseded alternative; §4/§6
> below describe the concept, with the "shipped" notes marking where the panel
> diverged.

## 1. Goal & scope

ccsbar puts the operator's Claude Code accounts one glance and one click away
in the menu bar: for every profile it shows the **5-hour session utilization**
(the window that actually throttles you) as a bar + %, a reset countdown, and an
active-account marker — and clicking any account **switches** the global
`~/.claude` credentials to it. Its reason to exist beyond "a pretty switcher" is
the **auto-switch status readout**: it surfaces whether clauth's fallback chain
is armed and when it will rotate, so the operator dodges the 5h ceiling without
keeping the TUI open. Scope is a read-mostly status panel + switch trigger; it
does not own credentials, refresh logic, or auto-switch policy — those live in
clauth, exposed to ccsbar through a small IPC surface (§3). Cost/activity
analytics are explicitly out of v1.

## 2. Tech decision

**Shipped: SwiftUI `MenuBarExtra(.window)`** — a `@main App` whose scene is a
`MenuBarExtra { PanelView } label: { … }.menuBarExtraStyle(.window)`. The window
style gives a translucent vibrancy panel that reads as first-party macOS, and it
is what **current CodexBar actually uses** (the earlier read below that "both
reference apps rejected `MenuBarExtra`" no longer holds). All UI is plain SwiftUI
views (`PanelView`/`ConfigView`), so controls are custom `.plain`-styled to match
the panel's drawn capsules — no `NSHostingView`/`intrinsicContentSize` bridging.

> **Superseded plan (kept for rationale):** the original recommendation was AppKit
> `NSStatusItem` + `NSMenu` with SwiftUI content hosted as custom menu rows — NOT
> `MenuBarExtra`. The reasoning below stands as a record of the tradeoffs
> considered; the pivot happened because `MenuBarExtra(.window)` matched CodexBar
> and hit the polish target with far less machinery.

- `MenuBarExtra` clips taller / multi-element content and gives no real control
  over per-provider switching or rich cards. Both reference apps rejected it.
- An `NSMenu` gives real vibrancy, semantic-color highlight flipping, keyboard
  nav, ESC-dismiss, and appearance pinning for free. CodexBar embeds SwiftUI into
  the menu via an `NSHostingView` subclass with `allowsVibrancy = true`, routing
  the SwiftUI-measured height through an overridden **`intrinsicContentSize`**
  (NSMenu lays out custom rows from `intrinsicContentSize`, not `frame`). That one
  mechanism is the whole trick.
- App shell is a thin SwiftUI `@main App` whose Scene body is only a hidden 1×1
  keepalive `WindowGroup` + a `Settings {}` scene; all real UI is driven through
  `@NSApplicationDelegateAdaptor(AppDelegate.self)` → a
  `StatusItemController: NSObject, NSMenuDelegate`.

v1 pragmatic narrowing: host the **switcher + all account cards as ONE SwiftUI
custom row** (one `MenuHostingView`), and keep the action rows (Refresh / Add
Account / Settings / Quit) as **native `NSMenuItem`s** with SF Symbols +
`.subtitle`. One row to measure, native everything else.

> Rejected alternative: `NSPopover(NSHostingController)` (CCSwitcher's route)
> gives full SwiftUI layout freedom but forces you to reimplement dismissal +
> vibrancy, and its solid-material default is what made CCSwitcher look
> non-native. Only fall back to it if the one-hosted-row NSMenu approach fights
> you; if so, host in an `NSVisualEffectView(.menu)`, never a solid fill.

| Item | Decision |
|---|---|
| Repo name | **`ccsbar`** (own repo at `~/projects/devtools/ccsbar`, flat sibling to `clauth`) |
| Language/toolchain | Swift 6, StrictConcurrency, **Swift Package Manager, no `.xcodeproj`** |
| Min macOS | **14.0 (Sonoma)**; `NSMenuItem.subtitle` on 14.4+ with an `NSStackView` fallback below |
| Activation | `LSUIElement = true` (accessory/agent, no Dock icon) |
| Packaging | `Scripts/package_app.sh` → ad-hoc signed for dev, Developer-ID signed + notarized for release |
| Distribution | Homebrew cask `ccsbar` + GitHub Releases + Sparkle appcast |
| Decomposition | SwiftUI views: `PanelView` (switcher/usage/chain/actions) + `ConfigView` (inline chain editor) + `StatusModel` (`ObservableObject` poll/commands) + `Theme` (tokens/`UsageBar`) + `Snapshot` (headless render) |

## 3. Data & IPC contract

**The core gap: nothing refreshes usage from the network unless the TUI is open.**
clauth's `usage_cache.json` is written only by the scheduler's `apply_outcome`,
which runs only inside the TUI; `fetch_status`, "armed", and next-refresh live in
in-memory stores; switching needs `clauth`, not a file write. Reading cache files
alone gives frozen numbers and can't switch.

**Mechanism: a headless `clauth daemon`** that (a) runs the existing scheduler
with no ratatui loop, (b) atomically writes a stable `~/.clauth/status.json` every
tick, and (c) listens on a unix socket for switch/snapshot/refresh. ccsbar
**polls `status.json` for display and sends switches over the socket.** This is a
small extraction — `spawn_refresher` / `tick` / `bootstrap_fetch` already take only
`Arc<RankedMutex<…>>` stores + a `ConfigHandle` + mpsc channels, never ratatui. A
`clauth status --json` single-shot is shipped too, as the canonical read format
and a debugging/fallback surface.

### `~/.clauth/status.json` (daemon writes atomically each tick)

```json
{
  "schema": 1,
  "generated_at": "2026-07-03T19:04:40+00:00",
  "clauth_version": "0.11.0",
  "active_profile": "kitty",
  "pending_switch": null,
  "last_switch": { "from": "doggy", "to": "kitty", "at": "2026-07-03T18:59:02+00:00", "trigger": "user" },
  "last_error": null,
  "wrap_off": false,
  "weekly_switch_threshold": 95,
  "burn_aware": true,
  "forecast": { "action": "switch", "to": "doggy" },
  "refresh_interval_ms": 300000,
  "fallback_chain": ["kitty", "doggy"],
  "profiles": [
    {
      "name": "kitty",
      "active": true,
      "provider": "anthropic",
      "base_url": null,
      "tier": "Max 5x",
      "has_live_session": true,
      "auth_status": "ok",
      "account_email": "kitty@example.com",
      "fetch_status": "Fresh",
      "stale": false,
      "fetched_at": "2026-07-03T19:04:20+00:00",
      "next_refresh_at": "2026-07-03T19:09:20+00:00",
      "auto_start": true,
      "bell_threshold": 90,
      "fallback": { "position": 1, "threshold": 95, "armed": true, "last_resort": false },
      "windows": [
        { "label": "5h",      "utilization_pct": 42.0, "resets_at": "2026-07-03T23:00:00+00:00" },
        { "label": "7d",      "utilization_pct": 18.0, "resets_at": "2026-07-08T17:00:00+00:00" },
        { "label": "7d Opus", "utilization_pct": 30.0, "resets_at": "2026-07-08T17:00:00+00:00" }
      ],
      "third_party": null
    }
  ]
}
```

Top-level fields beyond the original schema-1 core are **additive** (readers
use `decodeIfPresent`; schema stays 1):

- `clauth_version` — always present (daemon and single-shot) so CLI↔daemon
  skew is detectable.
- `last_switch` — the hero event: last executed switch with `from`/`to`/`at`/
  `trigger` (`"user"` or `"scheduler"`); `null` until one lands. Daemon-only.
- `last_error` — most recent switch skip/failure `{at, message}`, sticky until
  a newer reason replaces it; `null` until a drain records one. Daemon-only.
- `weekly_switch_threshold` — the wrap-off walk's weekly cap (percent).
- `burn_aware` — whether the ACTIVE-side switch decision projects on burn rate
  instead of the static threshold ("would switch at N%" rendering needs it).
- `forecast` — the daemon's own next-move forecast, `{action: "switch"|"off"|
  "none", to}`; the single source of truth for every "would switch to X"
  string (never re-derive the walk client-side).
- `fallback_chain` — ordered member names of the auto-switch chain, so the
  menu bar renders the chain without sorting per-profile `fallback.position`.
- per-profile `fallback.last_resort` — the exclusive last-resort mark (the
  member the walk's sink pass accepts even while exhausted).
- `active_codex_profile` (CDX-1) — the codex-harness active slot: which
  profile's chain lives in `~/.codex/auth.json`. Independent of
  `active_profile` (switching one never moves the other); `null` on
  claude-only installs / older daemons.
- per-profile `harness` (CDX-1) — `"claude" | "codex"`, which CLI the
  profile's credentials belong to. Absent (older daemon) reads as
  `"claude"`. **Per-slot `active` truth:** a codex profile's `active` is the
  codex-slot answer, a claude profile's the claude-slot answer — one coherent
  boolean per row, no cross-referencing needed. Codex rows report `provider:
  "openai"`, `tier` = the ChatGPT plan (`free|plus|pro|business|enterprise|
  edu`, from the stored JWT), and `account_email` from the stored id_token —
  no cache/backfill lag.
- per-profile `codex_snapshot_at` (CDX-1, codex-only, else `null`) — when the
  stored codex auth snapshot was last captured/adopted (ISO-8601). Readers can
  surface age; a parked snapshot >7 days also warns in `clauth doctor`.
- codex `windows` (CDX-2): the ACTIVE codex profile publishes real 5h/7d
  windows through the same `windows` array shape — sourced passively from
  codex's own session logs (never a backend poll), refreshed on the shared
  interval, attributed only to events newer than the last account change.
  Inactive codex profiles keep their last-known windows (their logs stop
  moving off-slot); `fetch_status` reads `Fresh` on a live publish. Codex
  profiles are never members of `fallback_chain` (chains are per-harness).
- per-profile `codex_rate_limit_reached` (CDX-4, codex-only, else `null`) —
  codex's OWN limiter verdict from the session-log snapshot: which window
  (`"primary"` = 5h, `"secondary"` = 7d) rejected the last request. Readers
  cross-check the named window's `resets_at`: a lapsed window means the
  verdict has expired (render no badge). Stronger than a percent heuristic —
  it is the daemon's codex-chain exhaustion input too.
- top-level `codex_fallback_chain` (CDX-4) — the codex auto-switch order,
  same shape as `fallback_chain`; empty on codex-less installs. Codex chain
  members carry the same per-profile `fallback` block (`position` within the
  CODEX chain, `threshold`, `armed` against the codex active slot,
  `last_resort`). The daemon auto-switches the codex slot at session boundary
  when the active codex profile is exhausted (percent shape or limiter
  verdict); the two harnesses' switch decisions are fully independent.

`fallback` is `null` when the profile is not in its harness's chain. `third_party` is
`{ "available": true }` for api-key profiles, else `null` — a plain
reachability flag; the structured balance (`balance_usd` / `currency`) is **not**
carried by `status.json` yet (deferred, tracked at `status_json.rs`: "structured
third-party balance isn't carried by ThirdPartyStats"). Freshness is `fetched_at`
if present, else the cache-file mtime. For api-key profiles that freshness comes
from the third-party cache, so a healthy hourly-refreshed account reports a live
`fetch_status`/`fetched_at` rather than null.

**`stale`** (RLS-1, per profile, additive — schema stays 1) is a boolean the
daemon sets `true` when it distrusts this profile's last reading as a **deep-slot
stuck `RateLimited`**: `fetch_status == "RateLimited"` AND the consecutive-429
streak has passed the active-retry cap, so the `/usage` throttle window never
drained and no `Fresh` read is coming. This is the same judgment the daemon's own
auto-switch acts on — a stuck-RateLimited active bypasses the "only act on a
Fresh read" gate so the chain rotates away from it instead of wedging (the
`RateLimited` analogue of the `auth_status: "broken"` bypass) — published so
readers dim the meter / show a "stuck" cue instead of rendering the frozen number
as current truth. `false` for a shallow/transient `RateLimited` (still expected to
clear), for every non-`RateLimited` status, and **always** for the single-shot
`clauth status --json` (no daemon, no streak history). An **absent** field (older
daemon) reads as `false`.

**Timestamps** — every clauth-written timestamp (`generated_at`, `fetched_at`,
`next_refresh_at`) is ISO-8601 UTC with an explicit `+00:00` offset, never a `Z`
suffix (`epoch_secs_to_iso`); `windows[].resets_at` is the API's own value in the
same `+00:00` shape. Parse the offset — do not key on a trailing `Z`.

The `windows[].label` values are **derived**, not a fixed enum: `"5h"` and `"7d"`
are always present, and the third is a plan-tier label (`"7d Opus"` on a
Max/Opus plan, `"7d fable"` / `"7d fable 5"` on a Fable plan) — treat it as an
opaque display string, not a key to switch on.

**`auth_status`** (AUTH-2, per profile) is `"ok" | "expiring" | "broken"`:
`"broken"` = the last OAuth refresh was rejected as revoked/invalid, so the
account is excluded from every fallback walk and refused as a switch target
(installing its dead token would log out every running `claude` — Incident C);
`"expiring"` = the access token is past its expiry but not yet flagged broken;
`"ok"` otherwise. `broken` outranks `expiring`. Keyed on the credential a
profile STORES, not on where its requests route: a hybrid (an OAuth pair kept
alongside a `base_url`) reports `expiring` on a dead token like any other
account (upstream 0.11.0). An **absent** field (older daemon) reads as `"ok"`.

**`next_refresh_at`** is `null` when no refresh is pending — a never-cached
profile **and**, with the `refresh_spent_accounts` toggle off, a spent
(100%-capped) account the scheduler skips until its window resets (upstream
0.11.0). Treat `null` as "no refresh scheduled", never as overdue.

**Single usage fetcher** (upstream 0.11.0, `usage-fetch.lock`): every instance
(the daemon and each open TUI) runs the same refresher, but only the flock
holder fetches usage, rotates tokens, and decides switches; the rest hydrate
from the shared disk caches. First-come, held for the process lifetime, taken
over within a tick of the holder exiting. The daemon's anti-wedge watchdog is
30s (tightened from 60s) so a wedged holder frees the lease fast. Feed-reader
impact: none — the daemon writes `status.json` every tick whether or not it
holds the lease.

**`account_email`** (CAP-3, per profile, additive — schema stays 1) is the
account email the profile's stored login last authenticated as (the identity
anchor's readable half: seeded by `clauth login`'s probe, moved by sanctioned
captures, backfilled by the daemon's hourly `/profile` fetch), else `null`
(older daemon / not yet backfilled / api-key profile). Readers show it so a
wrong-account capture is visible at a glance — the 2026-07-12 double-poll
incident was invisible precisely because nothing displayed WHICH account each
profile held. Consumers: TUI Setup tab (`account` row), TUI Usage header
(`account` row), TUI Overview `email` column (spare-width-carved; em-dash =
OAuth anchor not yet seeded, blank = api-key/provider), ccsbar detail card +
account-row caption + row tooltip + VoiceOver label, ccu profile line. **`pending_switch`** (top level, AUTH-2) is the switch
target the daemon has accepted but not yet applied (`"<name>"`), else `null` —
lets the UI show in-flight truth instead of a timing heuristic; always `null` for
the single-shot `clauth status --json` (no daemon).

### `~/.clauth/tokens.json` (daemon writes atomically; TOK-3)

A second daemon-written file, beside `status.json`, carrying a compact
token-usage rollup for the menu bar's usage panel. **This is not per-profile
clauth state** — it is Claude Code's OWN local usage history (`stats-cache.json`
+ recent transcripts), which is **machine-wide across every account and profile**
that shares the home dir, exactly the data the TUI's Tokens tab renders. There is
no notion of a clauth profile here. It is a rebuildable cache, so the daemon
writes it non-durably (`atomic_write_600_fast`), the same as `status.json`.

The feed is refreshed off the daemon's tick loop by the same two background
workers the TUI uses — the token loader (stats-cache parse + ~90s transcript
top-up) and the pricing loader (LiteLLM rate feed, disk-cached, ~24h cadence);
`tokens.json` is rewritten whenever either produces a new value. It is absent
until the first top-up completes (a menu bar must tolerate a missing file).

```json
{
  "schema": 1,
  "generated_at": "2026-07-09T19:04:40+00:00",
  "clauth_version": "0.11.0",
  "topped_up_through": "2026-07-09",
  "periods": {
    "today":    { "…": "PERIOD" },
    "week":     { "…": "PERIOD" },
    "month":    { "…": "PERIOD" },
    "lifetime": { "…": "PERIOD" }
  }
}
```

Each `periods[*]` is a `PERIOD`:

```json
{
  "from": "2026-07-06",  // "YYYY-MM-DD"; null for lifetime. today: from == to
  "to":   "2026-07-09",  // null for lifetime
  "input": 1200000, "output": 340000, "cache_read": 8100000, "cache_create": 260000,
  "in_out": 1540000,     // input + output — the headline "work" metric
  "total": 9840000,      // all four buckets
  "complete": true,      // false when a day in range published in+out with no per-model split
  "cost_usd": 12.47,     // null when no price table has loaded yet
  "cost_is_floor": false,// true → render "$X+" (an unpriced model, or an incomplete split)
  "models": [            // DESC by in_out, at most 8 rows; the tail folds into one "others"
    {
      "model": "claude-opus-4-8", "display": "opus 4.8",
      "input": 900000, "output": 220000,
      "cache_read": 6000000, "cache_create": 180000,
      "in_out": 1120000, "split_complete": true,
      "cost_usd": 9.85   // null when unpriced (unknown model) or no table
    }
  ]
}
```

Field semantics:

- **`in_out`** (input + output) is the always-exact work metric (known from
  `dailyModelTokens` even for split-less days). The split buckets `input`/
  `output`/`cache_read`/`cache_create` (and `total`) are a **floor** whenever
  `complete` is `false`: `week`/`month` can include a stats-cache day that
  recorded only its combined in+out with no per-model split, so those buckets
  undercount while `in_out` stays exact. `today` and `lifetime` are always
  `complete`.
- **Display basis is the cache-inclusive `total`, not `in_out`** (TOK-6,
  2026-07-12). `cost_usd` always prices cache tokens, and under 1h-TTL prompt
  caching `in_out` is a fraction of a percent of billed volume — ccsbar's
  first strip headlined `in_out` and read as a broken counter ("1.03M ·
  $319"). ccsbar now renders `max(total, in_out)` everywhere (per-model rows
  sum the four buckets client-side), decorated `N+` when `complete` /
  `split_complete` is false — the same floor idiom as `"$X+"`.
- **Cost** is priced per model from the LiteLLM table and summed, and **always
  counts cache tokens** (API pricing — never a blended rate). `cost_usd` is
  `null` until a price table loads. `cost_is_floor` is `true` when the split is
  incomplete OR a model carrying tokens has no matching rate — render the cost as
  `"$X+"` in that case.
- **`topped_up_through`** is the latest `YYYY-MM-DD` the live transcript top-up
  reached (`null` before the first top-up).
- **`display`** is the friendly model name (`model_display_name`); the folded
  tail row is `{"model": "others", "display": "others", …}` and prices to `null`.

**Additive fields keep `schema` 1** — a reader decodes unknown/missing fields as
absent (Swift `decodeIfPresent`), same rule as `status.json`. Only a
breaking-shape change bumps the version. The assembly is a single pure builder
(`tokens::build_tokens_snapshot`) shared by the daemon and any future writer, so
the shape cannot drift between producers.

### `~/.clauth/clauthd.sock` (unix socket, newline-delimited JSON)

```
→ {"cmd":"snapshot"}                        ← {"ok":true,"status": <status.json body>}
→ {"cmd":"switch","profile":"work"}         ← {"ok":true} | {"ok":false,"error":"...","error_code":"..."}
→ {"cmd":"refresh","profile":"work"}        ← {"ok":true}   (force refetch; profile optional = all)
# fallback configuration (CBAR-2):
→ {"cmd":"fallback_add","profile":"work"}   ← {"ok":true}   (append to the chain)
→ {"cmd":"fallback_remove","profile":"work"}← {"ok":true}
→ {"cmd":"fallback_move","profile":"work","dir":"up"}  ← {"ok":true}   (dir: up|down)
→ {"cmd":"set_threshold","profile":"work","value":90}  ← {"ok":true}   (0..=100)
→ {"cmd":"set_last_resort","profile":"work","value":true} ← {"ok":true}   (exclusive last-resort mark)
→ {"cmd":"set_wrap_off","value":true}       ← {"ok":true}
→ {"cmd":"set_weekly_threshold","value":95} ← {"ok":true}   (wrap-off walk's weekly cap, 50..=100)
→ {"cmd":"rename","profile":"work","new_name":"work2"} ← {"ok":true} | {"ok":false,"error":"...","error_code":"..."}
```

`switch` / `refresh` enqueue into `pending_switch` / `refetch_queue`; the
`fallback_*` / `set_*` / `rename` commands enqueue a `ConfigOp` into
`pending_config_ops`, which the main loop drains and applies via the shared
`fallback_config` edit primitives (add/remove/move/threshold/wrap-off/rename +
persistence). `rename` renames the profile directory + every reference and
force-relinks the credential mirror when the profile is active (rewriting the
Keychain on macOS, so a live session follows the rename). All mutation still
happens on the daemon's main thread; an `ok` reply means "accepted", and the
caller polls `status.json` to see it land. `status.json` also carries a top-level
`fallback_chain` (ordered member names) for rendering the chain.

Every `ok:false` reply carries a stable **`error_code`** (AUTH-2) alongside the
human-readable `error` — Swift branches on the code, the prose stays for humans:
`"unknown_profile"` (name did not resolve), `"auth_broken"` (target's login is
revoked/expired — a `switch` to it is refused synchronously with the `clauth
login <name>` hint rather than silently enqueued), `"invalid_value"` (malformed
command or out-of-range argument), and `"busy"` (target mid-fetch/rotation —
reserved; the drain currently retries rather than rejecting synchronously).

> Scope note: v1 called ccsbar "read-mostly" (§ above). CBAR-2 extended the
> socket so the menu bar can also *configure* the fallback chain; auto-switch
> *policy/execution* still lives entirely in clauth.

### What clauth (Rust) adds

1. **`daemon` dispatch arm** in `main.rs`. `daemon::serve()` builds the same `Arc`
   stores `app.rs` builds, calls `bootstrap_fetch` + `bootstrap_third_party` +
   `spawn_refresher`, drains `pending_switch`/`pending_switch_off` to execute
   auto-switches headlessly, and rewrites `status.json` each tick. The scheduler
   already persists `usage_cache.json` inside `apply_outcome`, so the daemon shares
   one cache with the TUI.
2. **Status serializer** — shared profile→JSON view helpers (`provider_label` /
   `tier_label` / `windows_json`, extracted to `src/profile_json.rs` so `mcp` and
   the daemon share one source of truth), plus `fallback` (position/threshold/
   armed), `wrap_off`, `fetch_status` (from `StatusStore`), and `next_refresh_at`
   (from `NextRefreshPerProfile`). Written via `profile.rs::atomic_write_600` (0600).
3. **Single-instance guard** — daemon holds an exclusive lock so two schedulers
   never double-fire. TUI-vs-daemon coordination (skip the TUI's own scheduler when
   a daemon is live) is a follow-up.
4. **Socket listener** — `clauthd.sock`, the commands above, wired to the
   `pending_switch` / `refetch_queue` / `pending_config_ops` stores. The
   `fallback_*` / `set_*` edits go through `src/fallback_config.rs` (one home for
   chain/threshold/wrap-off mutation + persistence, shared-ready with the TUI).
5. **`clauth status --json`** — single-shot arm: same serializer, reads caches,
   prints, exits. (Note: `src/status.rs` is the unrelated Statuspage feed — the
   status serializer is in the `daemon`/`profile_json` modules, not there.)
6. **launchd plist** shipped with the cask to run `clauth daemon` at login.

**Minimal switch path for ccsbar:** prefer the socket (`switch`, low latency);
fall back to shelling `clauth <name>` if the socket is absent.

## 4. Visual design

> **Superseded by [`CBAR-4-DESIGN.md`](./CBAR-4-DESIGN.md) (the binding "Preflight"
> visual spec).** The CBAR-4 redesign inverted the interaction model — the panel is
> now **inspect-first** (single click inspects; switching is a deliberate verb with a
> live-session arm-confirm), laid out **status strip → account list → detail card →
> chain rail → actions** rather than a tap-to-switch tile row. The color-role,
> usage-color, and daemon-liveness semantics recorded here still hold and feed the
> CBAR-4 spec; the *layout* below (the "Shipped panel" tile row) is historical.
> For anything visual, `CBAR-4-DESIGN.md` wins.
>
> **TABS-1 (2026-07-16):** the panel gained a codexbar-style provider tab bar on top
> (Overview / Claude / Codex). The CBAR-4 anatomy above is now the CLAUDE page;
> the Codex page mirrors it harness-scoped (codex strip → codex accounts → detail →
> codex chain rail reading `codex_fallback_chain`), and Overview is a read-only
> cross-harness glance. Plan of record: `ccsbar/docs/provider-tabs/PLAN.md`.

North star: CodexBar's deep native integration. Palette maps to clauth's TUI
(Catppuccin Mocha, `src/tui/theme.rs`) for **brand + usage semantics only**; all
structural text runs through **semantic NSColors** (`controlTextColor` /
`secondaryLabelColor` / `tertiaryLabelColor` / `selectedContentBackgroundColor`)
so hierarchy and highlight flip correctly in light/dark. Never hardcode chrome hex.

**Menu-bar glyph:** 18×18pt monochrome **template** `NSImage`, pixel-snapped. A
rounded-capsule usage meter whose left→right fill = active account's **5h
remaining** (dims to ~0.55 alpha when `fetch_status != Fresh`). Template image →
auto-tints to both menu-bar appearances. Optional single trailing metric (`42%`)
is opt-in.

**Shipped panel (SwiftUI `MenuBarExtra(.window)`):** top-to-bottom — an
**account-switcher tile row** (a tile per account with a tiny 5h meter, active
filled in accent, tap = switch) → the **active account's** Session/Weekly/Fable
usage meters (third-party api-key accounts show an availability dot) → the
**fallback-chain strip** (chips + arrows, armed member glowing, wrap-off state) →
a collapsible **Configure** disclosure → **Refresh** / **Quit** rows. This
replaces the original NSMenu "one hosted card per account" layout; the palette,
usage-color, and active-marker semantics below still hold.

**Original NSMenu concept (color/semantics reference):**

```
┌───────────────────────────────────────────────────┐
│  ● kitty            Max 5x                 ⟳ 4m    │   ● active dot: accent_2 orange #D97757
│   5h    ████████░░░░░░░░░   42%    resets 3h 53m   │   bar util_color(42)= text_dim (neutral)
│   7d    ███░░░░░░░░░░░░░░   18%    resets Wed 10a  │
│   7d Opus ████░░░░░░░░░░░   30%    resets Wed 10a  │
│   ⚡ auto-switch armed · #1 @ 95%                   │   info sky #74C7EC, quiet
├───────────────────────────────────────────────────┤
│  ○ work             Pro                    ⟳ 2m    │   ○ hollow = inactive; whole card click = switch
│   5h    ██████████████░░   88%    resets 1h 02m   │   88% ≥80 → danger pink #F38BA8
├───────────────────────────────────────────────────┤   ← native NSMenu divider
│  􀅼  Add Account…                              ⌘N   │   native NSMenuItems, SF Symbols
│  􀈄  Refresh now                                    │
│  􀍟  Settings…                                 ⌘,   │
│      Quit ccsbar                            ⌘Q   │
└───────────────────────────────────────────────────┘
```

**Active indicator:** filled dot + bold name in **accent_2 Claude orange
`#D97757`**; keyboard-focused/hovered row uses **accent Sapphire `#43ABE5`** for
the caret/selection (orange = "is active", sapphire = "is selected"). A fired bell
shows `!` in danger pink instead of the dot.

**Usage bars:** one SwiftUI `Canvas` pass, 6pt tall, fully-rounded capsule, track
= `line_strong #45475A` at ~22% alpha. Color by clauth's `util_color(pct)`:
**<60 → `text_dim #A6ADC8`**, **60–80 → warning yellow `#F9E2AF`**, **≥80 → danger
pink `#F38BA8`**. Fallback-chain members instead use `health_color(pct, threshold)`
(≥threshold danger, ≥0.8·threshold warning, else success `#A6E3A1`). **No
`.blendMode` / `.compositingGroup`** (they triggered CodexBar's Metal-shader
icon-vanish bug), no implicit animation inside the menu.

**CCSwitcher anti-patterns explicitly avoided:** no full-panel saturated fill; no
complementary orange+blue clash / four screaming hues (ONE accent orange on a
neutral base, sapphire only for focus, usage colors only on bars); no card-in-card
nesting; no clip-art brain glyph per row; no giant saturated number; no chunky iOS
segmented pill; no pill/badge overload; no cramped corner icon huddle; no menu-bar
strip over-density.

## 5. Feature set (v1)

**Ships in v1:** vertical list of all accounts (each a clickable card = one-click
switch, active pinned top); per-account 5h % + bar + reset countdown, plus 7d % and
per-model `7d <model>` windows; plan badge; auto-switch status (armed + chain
position `@ threshold`, `wrap_off` awareness, per-account next-refresh countdown);
staleness cue from `fetch_status`; Refresh now; **Add Account…** → `clauth login
<name>`; **Settings…** (native tabbed: refresh interval, launch-at-login, glyph
content); Quit; Sparkle auto-update. Third-party balance line for api-key profiles
only if trivially present in `status.json`.

**Shipped divergence (pre-CBAR-4, historical):** the panel showed *switcher tiles*
for every account (name + a tiny 5h meter) and the full Session/Weekly/Fable meters
for the *active* account only — not a full card per account. **This tile layout was
replaced by the CBAR-4 "Preflight" inspect-first panel** ([`CBAR-4-DESIGN.md`](./CBAR-4-DESIGN.md));
tiles → a file-order account list, tap-to-switch → inspect-first + a deliberate
Switch verb. **Add Account…** and Sparkle remain deferred; the dedicated **Settings…**
window was rejected in CBAR-4 (two config surfaces replace it).

**Deferred (v2+):** cost/activity analytics via local `~/.claude` JSONL parsing;
dense iStats-style menu-bar strip; cost/credits history charts; WidgetKit widget;
global hotkey; a bespoke `NSPopover` layout.

## 6. Build sequencing

**Phase R — clauth (Rust) first. This phase alone delivers the core requirement:
unattended auto-switch + live usage with the TUI closed.**

- **R1** headless daemon owner — `daemon` arm; `daemon::serve()` builds the stores,
  runs the scheduler, drains `pending_switch`/`pending_switch_off`, shares
  `usage_cache.json`. *Verify:* `clauth daemon`, watch cache mtimes advance with the
  TUI closed; active account auto-switches when a chain member's 5h crosses.
- **R2** `status.json` serializer — shared `windows_json`/`tier_label`; add
  `fallback`/`threshold`/`wrap_off`/`fetch_status`/`next_refresh_at`;
  `atomic_write_600` each tick. *Verify:* schema matches §3; `jq` parses.
- **R3** single-instance guard — daemon exclusive lock; DONE 2026-07-12: the TUI
  probes the flock + status.json freshness per tick and stands its own refresher
  down while a daemon is live (`daemon::probe`, scheduler `standdown_tick`),
  re-arming within a tick of daemon death/wedge. *Verify:* two daemons can't
  both run; TUI beside a daemon fetches nothing (its rows render daemon-fed
  countdowns).
- **R4** `clauthd.sock` listener — 3 commands wired to `pending_switch` /
  `refetch_queue`. *Verify:* `echo '{"cmd":"switch","profile":"work"}' | nc -U
  ~/.clauth/clauthd.sock` flips `active_profile` in the next `status.json`.
- **R5** `clauth status --json` — single-shot serializer. *Verify:* byte-shape
  matches `status.json`.
- **R6** launchd plist to run `clauth daemon` at login; ship in the cask.

**Phase S — ccsbar (SwiftUI) second — built.** Shipped as a
`MenuBarExtra(.window)` SwiftUI panel (S1 SPM shell → S2 menu-bar label + status
polling → S3 `status.json` read + socket client → **S4 the SwiftUI panel: switcher
tiles + usage meters + fallback-chain strip, done** → S5/S6 switch + config wiring
+ semantic colors → CBAR-2 inline fallback config → CodexBar-aesthetic redesign →
**CBAR-4 "Preflight" inspect-first rebuild** (forecast/liveness/switch-machine/label
engines + status strip → account list → detail card → chain rail; see
[`CBAR-4-DESIGN.md`](./CBAR-4-DESIGN.md) / [`CBAR-4-PLAN.md`](./CBAR-4-PLAN.md)).
Still deferred: Sparkle + Homebrew cask + Developer-ID notarization (`.app` bundling
via `Scripts/package_app.sh` is done; the CBAR-4 Settings window was rejected).

Rationale for R-before-S: the daemon is the load-bearing dependency and the
standalone value; building the Swift UI first would force it to read frozen caches
and reimplement mtime/toml/chain parsing in Swift, then get thrown away once the
daemon lands.
