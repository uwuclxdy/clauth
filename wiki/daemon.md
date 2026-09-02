# `clauth daemon`: headless scheduler + status feed

`clauth daemon` runs the same background refresher the TUI runs
(`spawn_refresher`) with no UI: refresh usage, rotate expiring tokens, run
the fallback chain's auto-switches, and publish `~/.clauth/status.json` for
external readers. `clauth status --json` prints the same shape single-shot,
no daemon required. This document is the read contract for both.

Scope note: the daemon carries **no external mutation surface**: no socket,
no config endpoint. Anything that needs eyes (a diverged live login, a manual
Claude Code `/login`, an unprovable identity) is refused and logged; the TUI
stays the only resolution surface.

## Process model

- **Singleton**: one advisory lock (`~/.clauth/clauthd.lock`) held for the
  process lifetime, with the holder's pid written to an unlocked sidecar
  (`~/.clauth/clauthd.pid`) for `ps`-level diagnosis (informational: the flock,
  never the number, is what proves presence). The pid stays out of the lock file
  because Windows locks are mandatory, so a `--status` reader in another process
  couldn't read bytes held inside the daemon's exclusive lock. A second `clauth daemon` exits 0 by
  default (`already running (pid <n>)`), so a spawner that fires repeatedly
  can't pile up idle daemons (#57). `--standby` opts into the take-over
  behaviour: it parks and takes over the moment the holder exits, for a
  supervisor's instance queueing behind a manually run one under launchd
  `KeepAlive{SuccessfulExit=false}` (which never restarts a clean exit). That
  standby queue is **one deep**: a second flock (`clauthd-standby.lock`) holds
  the slot, any further instance exits. `--no-standby` is the default's explicit
  spelling, kept for callers already passing it. A dead holder's flock auto-releases, so a
  supervisor with restart-on-crash keeps exactly one scheduler alive without
  pidfile bookkeeping. The TUI header's `● daemon` dot reads this lock
  (presence) plus `status.json` freshness (green = fresh feed, amber = stalling,
  hidden = no daemon) to show whether one is running.
- **Asking before spawning**: `clauth daemon --status` prints
  `running (pid <n>, feed fresh|stale[, standby waiting])` and exits 0 when a
  daemon is up. No daemon: exit 1, nothing on stdout. It creates nothing, so a
  menu-bar app or a wrapper script can gate its spawn on the exit code instead
  of starting a process to find out. A lock file it cannot test at all (no
  working `flock`, e.g. some NFS/CIFS mounts) is its own failure with the io
  error attached, never an exit 1 that reads as "none running" and sends a
  supervisor into a respawn loop. The header dot answers the same question by
  hiding instead, which is why the two read the lock through different paths.
  The default already exits the moment it loses the race, which suits the same
  callers. `--standby` is the one to keep out of a pure supervisor unit: the
  supervisor is the sole starter and wins the race alone, so a standby only earns
  its keep when a manual run and a unit coexist.
- **Replacing for an upgrade**: `clauth daemon --replace` terminates the running
  daemon and takes over. It reads the holder's pid sidecar and confirms the pid
  is still a running `clauth daemon` by its argv, so another clauth subcommand
  sharing the binary name is never signalled. It SIGTERMs the holder and waits
  for the flock to auto-release on death; on a timeout it escalates to SIGKILL,
  then claims. A pid it can't confirm bails rather than signal blind.
- **Probes take the lock they read**, briefly: both the header dot's
  `daemon_health` and `--status` try-lock a free file and release it. A starting
  daemon therefore re-tries a lost race (3 attempts, 100 ms apart) before it
  accepts that another instance is up. A real holder keeps its lock for life, so
  anything that clears on a retry was a reader.
- **Watchdog**: a wedged tick can freeze the single-threaded loop. The
  cross-process state flock a tick may block on is capped at 25 s, so a
  flock-blocked tick times out and retries rather than hanging; if no tick
  completes in 30 s at all, the daemon `abort()`s for a clean supervisor
  restart, freeing the usage lease (below). A legit ~20 s keychain switch sits
  inside both margins.
- **Log hygiene**: every daemon-visible stderr line carries an ISO-8601 UTC
  prefix, enabled only in daemon mode. An interactive terminal instead diverts
  its lines to `~/.clauth/clauth.log` so a background thread never paints over
  the TUI; a redirected or piped stderr keeps the bare line. `~/.clauth/daemon.log`
  is size-capped in place when a supervisor points stderr at it. The in-place trim is only sound for an APPEND-mode fd: use
  launchd `StandardErrorPath` or systemd `StandardError=append:...`. A
  non-append redirect (`file:`, a plain `>`) keeps its own offset, so the
  next write after a trim leaves a sparse NUL hole and the size cap is
  defeated. The daemon checks its own stderr at boot and warns loudly when it
  is a non-append file, so a defeated cap shows up in the log instead of only
  in this page.
- **Single usage fetcher (`usage-fetch.lock` lease)**: every instance (the
  daemon and each open TUI) runs the same refresher, but only the one holding
  the `usage-fetch.lock` flock fetches usage, rotates tokens, and decides
  switches. The rest hydrate from the shared disk caches instead of double-polling
  the usage API, double-rotating the single-use refresh chain, or re-deciding
  switches. The lease is first-come and held for the process lifetime; no
  preemption, so the switch-decider never thrashes between processes; a waiter
  takes it over within one tick of the holder exiting (flock auto-release). The
  daemon normally boots first and holds it, but a TUI already fetching keeps the
  lease until it closes, and the daemon then hydrates while still publishing
  `status.json` every tick.

## `~/.clauth/status.json`

Written each scheduler tick and immediately after a switch lands. Atomic
(`tmp` + rename into place), `0600`. **Never carries a token, secret, or
key**: names, tiers, percentages, timestamps only.

```json
{
  "schema": 1,
  "generated_at": "2026-07-03T19:04:40+00:00",
  "active_profile": "kitty",
  "pending_switch": null,
  "wrap_off": false,
  "refresh_interval_ms": 300000,
  "profiles": [
    {
      "name": "kitty",
      "active": true,
      "provider": "anthropic",
      "base_url": null,
      "tier": "Max 5x",
      "has_live_session": true,
      "auth_status": "ok",
      "fetch_status": "Fresh",
      "stale": false,
      "fetched_at": "2026-07-03T19:04:20+00:00",
      "next_refresh_at": "2026-07-03T19:09:20+00:00",
      "auto_start": true,
      "bell_threshold": 90,
      "fallback": { "position": 1, "threshold": 95.0, "armed": true },
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

### Field notes

> Fork-only additive fields — `harness`, `account_email`, `session_feed` (post-#59: `rolling_token`), `codex_snapshot_at`, `codex_rate_limit_reached`, `codex_reset_credits`, top-level `active_codex_profile` / `codex_fallback_chain` / `forecast` / `burn_aware` — and the full socket verb list are specified in [docs/ccsbar/DESIGN.md](../docs/ccsbar/DESIGN.md), the contract both GUI clients (ccsbar, Pulse) read. The table below is the upstream core.

| Field | Semantics |
|---|---|
| `schema` | Integer, currently `1`. Bumped ONLY on a breaking change; additive fields do not bump it (evolution rule below). |
| `generated_at` | Write stamp, ISO-8601 UTC with an explicit `+00:00` offset (all timestamps are; parse the offset, never key on a `Z` suffix; the writer does not emit one). Readers derive staleness from it: a stamp much older than `refresh_interval_ms` means the daemon is gone/stuck, so show last-known data with a stale cue, never spin. |
| `active_profile` | The profile whose credentials are currently installed, else `null`. |
| `pending_switch` | A switch the daemon has accepted but not yet applied (`"<name>"`), else `null`. Exists so readers can show in-flight truth instead of a timing heuristic. Always `null` from the single-shot CLI. |
| `wrap_off` | The fallback chain's stop-vs-stay-on-active flag, verbatim from state. |
| `profiles[].provider` | `"anthropic"` for OAuth profiles, else the recognised provider's display name. |
| `profiles[].tier` | Plan label (`"Max 5x"`…), `null` when unknown. Opaque display string. |
| `profiles[].auth_status` | `"ok" \| "expiring" \| "broken"`. `broken` = last refresh rejected as revoked/invalid → excluded from fallback walks, refused as a switch target. `expiring` = past expiry, not yet refreshed. `broken` outranks `expiring`. Reports on the credential a profile STORES, not on where its requests route: a hybrid (an OAuth pair kept alongside a `base_url`) reports `expiring` on a dead token like any other account. Absent ⇒ `"ok"`. |
| `profiles[].fetch_status` | `"Fresh" \| "Cached" \| "Failed" \| "RateLimited"`: the usage fetch's last outcome, so readers can distinguish live bars from last-known. `Failed`/`RateLimited` come only from a live daemon's OAuth fetch leg; api-key profiles (and any name the live stores don't carry yet) derive `Fresh`/`Cached` from their own cache's mtime instead; an api-key profile with a warm cache is never reported as unfetched. `null` = no cache at all (genuinely never fetched). |
| `profiles[].stale` | `bool` (additive, schema stays 1; absent ⇒ `false`). `true` when the daemon distrusts this reading as a **deep-slot stuck `RateLimited`**: `fetch_status == "RateLimited"` AND the consecutive-429 streak has passed the active-retry cap, so the `/usage` throttle never drained and no `Fresh` read is coming. This is the same judgment the daemon's auto-switch acts on: a stuck-RateLimited active bypasses the "only act on a Fresh read" gate so the chain rotates away instead of wedging (the `RateLimited` analogue of the `auth_status: "broken"` bypass); the switch still requires the active's last-known usage to be genuinely spent, so a throttle blip with headroom stays put. Readers should dim the meter / show a "stuck" cue rather than render the frozen number as current truth. `false` for a shallow/transient `RateLimited`, for every non-`RateLimited` status, and **always** for the single-shot `clauth status --json` (no daemon, no streak history). |
| `profiles[].next_refresh_at` | ISO-8601 UTC of the next scheduled usage refresh, or `null` when none is pending. `null` covers a never-cached profile **and**, with `refresh_spent_accounts` off, a spent (100%-capped) account the scheduler skips until its window resets. Treat `null` as "no refresh scheduled", never as overdue. |
| `profiles[].fallback` | `null` when not in the chain; else 1-based `position`, `threshold` (%), `armed` (this member is the active one the auto-switch watches). |
| `profiles[].windows[]` | `label` is **derived, not an enum**: `"5h"` and `"7d"` always; the third is a plan-tier label (`"7d Opus"`…). Treat labels as opaque display strings, never keys to switch on. `utilization_pct` 0-100 float; `resets_at` nullable. |
| `profiles[].third_party` | `{ "available": bool }` for api-key profiles once probed, else `null`, including an api-key profile whose provider has never been reached (no cache yet). Plain reachability; structured balances deliberately deferred. |
| `profiles[]` membership | A user-disabled account (`clauth disable`) is excluded from `profiles[]` by default; no field marks a profile disabled, absence from the array IS the signal. The active profile is always present regardless of its own disabled flag, so `active_profile` never names a profile missing from `profiles[]`. |

### Evolution rule (the load-bearing part)

- **Writers**: additive only under the same `schema`: new fields may appear,
  existing fields never change type/meaning. A breaking change bumps `schema`.
- **Readers**: ignore unknown fields; default absent optional fields (absent
  `auth_status` ⇒ `"ok"`); refuse only on `schema` greater than what they
  know, showing "daemon newer than me" rather than garbage.

## `clauth status --json`

Same schema, produced single-shot from the on-disk caches with no daemon and
no network fetch: `pending_switch` is always `null`, `generated_at` is the
print stamp, and freshness/next-refresh derive from each profile's usage-cache
mtime. One code path builds both (`daemon::status_json::build_status`), so the
key SHAPE cannot drift between the daemon feed and the CLI snapshot. Values can:
the single-shot form derives `fetch_status` from cache mtimes, so it only ever
reports `Fresh`/`Cached`/`null`: a profile the live daemon shows as `Failed` or
`RateLimited` reads as `Cached` here at the same instant. Poll the feed, not the
CLI, when the fetch outcome matters.

`clauth status --json --all` (or its `--disabled` spelling, equivalent) is the
one way to reveal disabled accounts in `profiles[]`; the running daemon's own
published `~/.clauth/status.json` file always hides them (every daemon-side
`build_status` call passes `include_disabled: false`), so a reader that needs
the disabled roster must shell out to the single-shot form, never poll the feed file for it.
