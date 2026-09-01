# `clauth daemon`: headless scheduler + status feed

`clauth daemon` runs the same background refresher the TUI runs (`spawn_refresher`) with no UI: refresh usage, rotate expiring tokens, run the fallback chain's auto-switches, and publish `~/.clauth/status.json` for external readers. `clauth status --json` prints the same shape single-shot, no daemon required. This document is the read contract for both.

Scope note: the daemon carries **no external mutation surface**: no socket, no config endpoint. Anything that needs eyes (a diverged live login, a manual Claude Code `/login`, an unprovable identity) is refused and logged; the TUI stays the only resolution surface.

## Process model

- **Singleton**: one advisory lock (`~/.clauth/clauthd.lock`) held for the process lifetime, with the holder's pid written to an unlocked sidecar (`~/.clauth/clauthd.pid`) for `ps`-level diagnosis. Informational: the flock, never the number, proves presence.
  - The pid stays out of the lock file because Windows locks are mandatory: a `--status` reader in another process could not read bytes inside the daemon's exclusive lock.
  - A second `clauth daemon` exits 0 by default (`already running (pid <n>)`), so a spawner that fires repeatedly can't pile up idle daemons (#57).
  - `--standby` opts into take-over: it parks and takes over the moment the holder exits, for a supervisor's instance queueing behind a manually run one under launchd `KeepAlive{SuccessfulExit=false}` (which never restarts a clean exit).
  - The standby queue is **one deep**: a second flock (`clauthd-standby.lock`) holds the slot; any further instance exits. `--no-standby` is the default's explicit spelling, kept for callers already passing it.
  - A dead holder's flock auto-releases, so a supervisor with restart-on-crash keeps exactly one scheduler alive without pidfile bookkeeping.
  - The TUI header's `● daemon` dot reads this lock (presence) plus `status.json` freshness (green = fresh feed, amber = stalling, hidden = no daemon) to show whether one is running.
- **Asking before spawning**: `clauth daemon --status` prints `running (pid <n>, feed fresh|stale[, standby waiting])` and exits 0 when a daemon is up; with none up, exit 1 and nothing on stdout.
  - It creates nothing, so a menu-bar app or a wrapper script can gate its spawn on the exit code instead of starting a process to find out.
  - A lock file it cannot test at all (no working `flock`, e.g. some NFS/CIFS mounts) is its own failure with the io error attached, never an exit 1 that reads as "none running" and sends a supervisor into a respawn loop.
  - The header dot answers the same question by hiding instead, which is why the two read the lock through different paths. The default already exits the moment it loses the race, which suits the same callers.
  - `--standby` is the one to keep out of a pure supervisor unit: the supervisor is the sole starter and wins the race alone, so a standby only earns its keep when a manual run and a unit coexist.
- **Replacing for an upgrade**: `clauth daemon --replace` terminates the running daemon and takes over. It reads the holder's pid sidecar and confirms the pid is still a running `clauth daemon` by its argv, so another clauth subcommand sharing the binary name is never signalled.
  - It SIGTERMs the holder and waits for the flock to auto-release on death; on a timeout it escalates to SIGKILL, then claims. A pid it can't confirm bails rather than signal blind.
- **Probes take the lock they read**, briefly: both the header dot's `daemon_health` and `--status` try-lock a free file and release it. A starting daemon therefore re-tries a lost race (3 attempts, 100 ms apart) before it accepts that another instance is up.
  - A real holder keeps its lock for life, so anything that clears on a retry was a reader.
- **Watchdog**: a wedged tick can freeze the single-threaded loop. The cross-process state flock a tick may block on is capped at 25 s, so a flock-blocked tick times out and retries rather than hanging.
  - If no tick completes in 30 s at all, the daemon `abort()`s for a clean supervisor restart, freeing the usage lease.
  - A legit keychain switch sits inside both margins: on macOS it reads the Keychain item and writes it back, each of those killed at 10 s.
  - Everything one flock hold spends in `security` is capped at 20 s no matter how many reads and writes it makes.
- **Log hygiene**: every daemon-visible stderr line carries an ISO-8601 UTC prefix, enabled only in daemon mode. An interactive terminal instead diverts its lines to `~/.clauth/clauth.log` so a background thread never paints over the TUI; a redirected or piped stderr keeps the bare line.
  - `~/.clauth/daemon.log` is size-capped in place when a supervisor points stderr at it. The in-place trim is only sound for an APPEND-mode fd: use launchd `StandardErrorPath` or systemd `StandardError=append:...`.
  - A non-append redirect (`file:`, a plain `>`) keeps its own offset, so the next write after a trim leaves a sparse NUL hole and the size cap is defeated. The daemon checks its own stderr at boot and warns loudly when it is a non-append file, so a defeated cap shows up in the log instead of only in this page.
- **Usage-history samples**: the lease holder appends every live `/usage` reading to `~/.clauth/profiles/<name>/usage_history.jsonl`, the series the burn rate and burn-aware switching read. The daemon keeps it advancing with no TUI open.
  - Retention is 2 days, re-trimmed on a 6-hour cadence rather than only at startup, so a long-lived daemon bounds the file without a restart.
- **Single usage fetcher (`usage-fetch.lock` lease)**: every instance (the daemon and each open TUI) runs the same refresher, but only the one holding the `usage-fetch.lock` flock fetches usage, rotates tokens, and decides switches.
  - The rest hydrate from the shared disk caches instead of double-polling the usage API, double-rotating the single-use refresh chain, or re-deciding switches.
  - The lease is first-come and held for the process lifetime, no preemption, so the switch-decider never thrashes between processes; a waiter takes it over within one tick of the holder exiting (flock auto-release).
  - The daemon normally boots first and holds it, but a TUI already fetching keeps the lease until it closes, and the daemon then hydrates while still publishing `status.json` every tick.

## `~/.clauth/status.json`

Written each scheduler tick and immediately after a switch lands. Atomic (`tmp` + rename into place), `0600`. **Never carries a token, secret, or key**: names, tiers, percentages, timestamps only.

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

| Field | Semantics |
|---|---|
| `schema` | Integer, currently `1`. Bumped ONLY on a breaking change; additive fields do not bump it (evolution rule below). |
| `generated_at` | Write stamp, ISO-8601 UTC with an explicit `+00:00` offset (all timestamps are; parse the offset, never key on a `Z` suffix; the writer does not emit one). Readers derive staleness from it: a stamp much older than `refresh_interval_ms` means the daemon is gone/stuck, so show last-known data with a stale cue, never spin. |
| `active_profile` | The profile whose credentials are currently installed, else `null`. |
| `pending_switch` | A switch the daemon has accepted but not yet applied (`"<name>"`), else `null`. Exists so readers can show in-flight truth instead of a timing heuristic. Always `null` from the single-shot CLI. |
| `wrap_off` | The fallback chain's stop-vs-stay-on-active flag, verbatim from state. |
| `profiles[].provider` | One of three cases: `"anthropic"` for a profile with no endpoint of its own (the managed `base_url` is unset), the recognised provider's display name (`"DeepSeek"`, `"Z.ai"`, …) for a typed third-party account, and `"generic"` for every other endpoint: an api-key account no provider recognises (litellm, LM Studio, ollama, a LAN gateway). Keyed on the managed `base_url` alone, so the label never contradicts the `base_url` published beside it; an operator-authored `ANTHROPIC_BASE_URL` reroutes requests without changing it. |
| `profiles[].tier` | Plan label (`"Max 5x"`, `"Pro"`, `"Free"`…), `null` when nothing on disk claims a tier. Opaque display string, so never switch on it. A **canceled** subscription reports its real post-cancellation tier (`"Free"`) and never the word `canceled`: cancellation is a status, not a tier, and no field here exposes it. `null` is rarer than it looks: a token-only account resolves its tier from the OAuth `subscription_type` alone, Free included. |
| `profiles[].auth_status` | `"ok" \| "expiring" \| "broken"`. `broken` = last refresh rejected as revoked/invalid → excluded from fallback walks, refused as a switch target. `expiring` = past expiry, not yet refreshed. `broken` outranks `expiring`. Reports on the credential a profile STORES, not on where its requests route: a hybrid (an OAuth pair kept alongside a `base_url`) reports `expiring` on a dead token like any other account. Absent ⇒ `"ok"`. |
| `profiles[].rolling_token` | `bool` (additive, schema stays 1; absent ⇒ `false`). `true` when the sidecar currently HOLDS a rolling bearer (content-classified — a refresh token present means mis-filled before anything else is considered, then a plan stamp or any scope beyond the setup pair means rolling; the same truth the TUI renders), not when the config flag is merely on: a dead chain degrades the sidecar onto its static mint with the flag still set, and this key follows the sidecar. While `true` the bearer is plan-stamped, refresh-less, and its expiry is HOURS out. Readers MUST key their token-row rendering off this: a rolling token drawn through the static mint's 30-day warning ramp shows a healthy credential as dying, and a mint drawn through the rolling countdown promises re-stamps nobody will make. (The 2026-07 incident this contract descends from ran the other way entirely — every surface looked healthy while the split's protection was silently off — which is why the key follows what the sidecar HOLDS rather than any flag.) A mis-filled sidecar (rotating pair) publishes `false`: it is not a rolling token, and the TUI renders it as the DANGER state it is. `false` likewise for a static `claude setup-token` mint, whose year-scale countdown is the honest one. |
| `profiles[].fetch_status` | `"Fresh" \| "Cached" \| "Failed" \| "RateLimited" \| "AuthExpired"`: the usage fetch's last outcome, so readers can distinguish live bars from last-known. `"AuthExpired"` is TERMINAL and means action required rather than a retry pending: this account's usage credential is dead (a revoked api key or an Alibaba Model Studio console session). Only an operator re-entering the key or re-logging in clears it. clauth has stopped polling it, so never read the `next_refresh_at` beside it as a scheduled attempt. Resolution order, highest first: a live daemon's OAuth store, then its third-party store, then a durable per-profile record of a dead credential, then a derivation from the profile's own cache mtime (`Fresh` within one interval, else `Cached`). `Failed` and `RateLimited` now reach third-party profiles as well as OAuth ones. An api-key profile with a warm cache is never reported as unfetched. A profile nothing has ever fetched stays `null` (no cache at all), `"AuthExpired"` included. Never appears on an OAuth profile, whose analogous state is `auth_status: "broken"`. |
| `profiles[].stale` | `bool` (additive, schema stays 1; absent ⇒ `false`). `true` when the daemon distrusts this reading as a **deep-slot stuck `RateLimited`**: `fetch_status == "RateLimited"` AND the consecutive-429 streak has passed the active-retry cap, so the `/usage` throttle never drained and no `Fresh` read is coming. This is the same judgment the daemon's auto-switch acts on: a stuck-RateLimited active bypasses the "only act on a Fresh read" gate so the chain rotates away instead of wedging (the `RateLimited` analogue of the `auth_status: "broken"` bypass); the switch still requires the active's last-known usage to be genuinely spent, so a throttle blip with headroom stays put. Readers should dim the meter / show a "stuck" cue rather than render the frozen number as current truth. `false` for a shallow/transient `RateLimited`, for every non-`RateLimited` status, and **always** for the single-shot `clauth status --json` (no daemon, no streak history). It stays OAuth-only by construction, deliberately narrower than `fetch_status` above: the streak counter it pairs with is written only by the OAuth leg's own handler, so a third-party `RateLimited` has no streak to judge and would always read as a shallow one. |
| `profiles[].next_refresh_at` | ISO-8601 UTC of the next scheduled usage refresh, or `null` when none is pending. `null` covers a never-cached profile **and**, with `refresh_spent_accounts` off, a spent (100%-capped) account the scheduler skips until its window resets. Treat `null` as "no refresh scheduled", never as overdue. A non-`null` stamp is not a promise either: an `"AuthExpired"` profile is suppressed from the cadence and still carries an ordinary future stamp, so gate on `fetch_status` before believing one. |
| `profiles[].fallback` | `null` when not in the chain; else 1-based `position`, `threshold` (%), `armed` (this member is the active one the auto-switch watches). |
| `profiles[].auto_start_queue` | Additive object `{ "position": integer, "next_open_at": ISO-8601 UTC \| null }` (schema stays `1`). `null` when the global queue toggle is off or this profile holds no slot — including while a switch-grade kick block stands against it, since a profile that cannot open a window is not a queue member and does not count toward the shared `5h / N`. `next_open_at: null` means no opening has been observed yet, so the queue is due now; a non-null stamp in the past means the same, since the stamp names the earliest moment the queue may open its next window and one that has passed has cleared the gap. The anchor is every observed opening, not only the daemon's own kicks: a window clauth opened is recorded in `usage_history.jsonl` as the kick lands, and one opened out of band (a live Claude Code session on that account) becomes the anchor once its samples hold the same boundary long enough to be sure. Single-shot `status --json` re-derives that from disk on every invocation; the daemon publishes the scheduler's in-memory value, which takes the out-of-band opening up on the next tick that could actually elect someone. Either way `next_open_at` is an estimate: taking an opening up moves the anchor FORWARD, so for fixed membership the stamp moves later than a reader was last told, and a reader that cached it will be early. |
| `profiles[].windows[]` | `label` is **derived, not an enum**: `"5h"` and `"7d"` always; the third is a plan-tier label (`"7d Opus"`…). Treat labels as opaque display strings, never keys to switch on. `utilization_pct` 0-100 float; `resets_at` nullable. |
| `profiles[].third_party` | `{ "available": bool }` for api-key profiles once probed, else `null`, including an api-key profile whose provider has never been reached (no cache yet). Plain reachability; structured balances deliberately deferred. |
| `profiles[]` membership | A user-disabled account (`clauth disable`) is excluded from `profiles[]` by default; no field marks a profile disabled, absence from the array IS the signal. The active profile is always present regardless of its own disabled flag, so `active_profile` never names a profile missing from `profiles[]`. |

### Evolution rule (the load-bearing part)

- **Writers**: additive only under the same `schema`: new fields may appear, existing fields never change type/meaning. A breaking change bumps `schema`.
- **Readers**: ignore unknown fields; default absent optional fields (absent `auth_status` ⇒ `"ok"`); refuse only on `schema` greater than what they know, showing "daemon newer than me" rather than garbage.

## `clauth status --json`

Same schema, produced single-shot from the on-disk caches with no daemon and no network fetch: `pending_switch` is always `null`, `generated_at` is the print stamp, and freshness/next-refresh derive from each profile's usage-cache mtime. One code path builds both (`daemon::status_json::build_status`), so the key SHAPE cannot drift between the daemon feed and the CLI snapshot. Values can: the single-shot form derives `fetch_status` from cache mtimes, so a profile the live daemon shows as `Failed` or `RateLimited` reads as `Cached` here at the same instant. Poll the feed, not the CLI, when the fetch outcome matters.

`"AuthExpired"` is the one exception, exact on both paths. Single-shot it comes from a durable per-profile record written when a fetch died on a credential that cannot self-heal. That record applies only as long as the credential recorded with it still matches the one on disk, so a re-login retires it with no daemon and no timer involved. A profile the daemon has never fetched carries no record and reads `null`.

`clauth status --json --all` (or its `--disabled` spelling, equivalent) is the one way to reveal disabled accounts in `profiles[]`; the running daemon's own published `~/.clauth/status.json` file always hides them (every daemon-side `build_status` call passes `include_disabled: false`), so a reader that needs the disabled roster must shell out to the single-shot form, never poll the feed file for it.
