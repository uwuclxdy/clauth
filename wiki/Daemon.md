# `clauth daemon`: headless scheduler + status feed

`clauth daemon` runs the same background refresher the TUI runs
(`spawn_refresher`) with no UI: refresh usage, rotate expiring tokens, run
the fallback chain's auto-switches, and publish `~/.clauth/status.json` for
external readers. `clauth status --json` prints the same shape single-shot,
no daemon required. This document is the read contract for both.

Scope note: by default the daemon carries **no external surface at all**: it
publishes a file and reads a file, and everything reaches it through
`~/.clauth`. `--listen` opts into one, an authenticated TLS REST API whose only
mutation is a switch (see below); without that flag nothing listens. What holds
either way: anything that needs eyes (a diverged live login, a manual Claude
Code `/login`, an unprovable identity) is refused and logged, and the TUI stays
the only resolution surface. The REST switch refuses those cases with a 409
exactly as the scheduler and the MCP tool refuse them.

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
- **Usage-history samples**: the lease holder appends every live `/usage` reading
  to `~/.clauth/profiles/<name>/usage_history.jsonl`, the series the burn rate and
  burn-aware switching read. Headless counts: the daemon keeps it advancing with no
  TUI open. Retention is 2 days, re-trimmed on a 6-hour cadence rather than only at
  startup, so a long-lived daemon bounds the file without a restart.
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

## REST API (`--listen`)

`clauth daemon --listen 0.0.0.0:8443` also serves the feed and the switch over
HTTPS, for running the daemon on one machine and a client (clauth-tray) on
another. Off unless the flag is passed. TLS comes from this host's
[lego](https://github.com/go-acme/lego) certificate, on all three platforms
clauth supports — macOS, Linux, and Windows. Only two things differ by platform:
where that certificate lives, and how the host's own FQDN is discovered. Both
are spelled out below.

- **The address is optional.** A bare `clauth daemon --listen` binds
  `0.0.0.0:8443`, the spelling this page uses everywhere else; pass an explicit
  `ADDR:PORT` to bind anything narrower (`--listen 127.0.0.1:8443` for a
  loopback-only listener behind a reverse proxy, say). The shorthand deliberately
  defaults to every interface rather than loopback, because a listener the
  remote client cannot reach is not a useful default for this flag. Nothing
  listens unless `--listen` is passed either way — the default fills in the
  value, never the flag.
- **TLS is not optional.** There is no plaintext mode and no flag to ask for
  one, because the bearer token crosses the connection on every request. The
  certificate is read once at startup from the platform's lego directory (see
  the next bullet), in lego's own naming: `<fqdn>.crt`, `<fqdn>.issuer.crt`, and
  `<fqdn>.key`. Any certificate the issuer file adds that the leaf file already carries is
  dropped, since lego usually writes the whole chain into `<fqdn>.crt` and a
  chain that repeats a certificate is malformed. A renewal reaches the listener
  on the next daemon restart. A missing or unreadable file, or a port already
  taken, refuses to start rather than running a daemon that looks healthy while
  the remote client stays dark.
- **Where the certificate lives, and how the host is named**, are the only two
  things that differ across platforms:

  | | Default certificate directory | Host FQDN from |
  |---|---|---|
  | macOS, Linux | `/etc/lego/certificates` | `hostname -f` |
  | Windows | `%AppData%\lego\certificates` (`C:\Users\<you>\AppData\Roaming\lego\certificates` on a stock install) | PowerShell `[System.Net.Dns]::GetHostEntry($env:COMPUTERNAME).HostName` |

  The directory is a **default**, overridable through `tls.json` (next bullet).
  The FQDN is not overridable at all — it is whatever the rest of the box
  already believes it is called, and a certificate issued to a name the host
  does not answer to would be the actual bug. Point lego at the Windows
  directory with its `--path` flag: lego's own default there is `.lego` under
  the working directory, which is useless for a daemon, since the working
  directory is whatever started it. The Windows default is **per-user**,
  matching where clauth keeps everything else (`~/.clauth`) — so a daemon run as
  you finds it, while one run as a Windows *service* under `LocalSystem` would
  resolve a different `%AppData%` and needs `tls.json` pointed at a directory
  both accounts can read. Note that
  Windows' `whoami /fqdn` is *not* the lookup and could not be one: its "FQDN"
  is the current **user's** Active Directory distinguished name (`CN=…,DC=…`),
  not the machine's, and it fails outright for a local account.
- **`~/.clauth/tls.json` points at the certificates.** Written with this
  platform's default the first time a `--listen` daemon starts, so there is a
  real file to edit rather than a documented path to retype (unlike
  `auth_token.json`, `--print-token` does not create it — nothing but the
  listener reads a certificate):

  ```json
  {
    "schema": 1,
    "cert_dir": "/etc/lego/certificates"
  }
  ```

  Edit `cert_dir` and restart to serve from anywhere else — a Windows box whose
  lego lives outside `%AppData%`, a Linux one following a distribution's own
  packaging convention, or a staging certificate in a scratch directory. The
  file is never rewritten once it exists, so an edit survives every restart and
  every upgrade. Unlike `auth_token.json`, a `tls.json` that cannot be parsed —
  or that carries an empty `cert_dir` — **refuses to start** instead of falling
  back to the default: quietly serving out of a directory you believe you moved
  away from is a worse failure than not starting, and the error names the file.
  Delete it to get the platform default back. A `schema` newer than this build
  knows is read anyway (the field it needs is a path either way), so a
  downgrade does not strand your configured directory.
- **The certificate has to be this host's own.** Whichever lookup applies, the
  certificate the daemon serves must be issued for that exact FQDN — and clients
  must reach it by that name, `https://<fqdn>:8443`. A bare IP fails certificate
  verification no matter what is listening, which is the easy mistake to make
  once `--listen` defaults to `0.0.0.0`: binding every interface says nothing
  about which *names* the certificate covers. If the box's own name disagrees
  with the name you got the certificate for, the daemon does not fall back or
  guess — it looks for files that are not there and refuses to start. Two
  consequences worth planning for: the host needs a real, publicly resolvable
  FQDN for Let's Encrypt to validate at all, and a client on a LAN needs that
  same name to resolve to the daemon's reachable address (split-horizon DNS or a
  `hosts` entry), since the name in the certificate, not the address you dialed,
  is what gets checked.
- **Renewal is yours to automate.** Let's Encrypt certificates are valid for 90
  days, and clauth does not renew them — it only reads what lego left on disk,
  once, at startup. So a `--listen` daemon needs two automated steps, not one: a
  periodic `lego ... renew` (cron or a systemd timer on macOS and Linux, a
  Scheduled Task on Windows, or whatever else supervises the host), *and* a
  daemon restart afterwards, since the certificate is never
  re-read while the process lives. Renewing without the restart is the failure
  mode to watch for — the daemon keeps serving the now-expired certificate it
  read at boot, and every client rejects the handshake while the files on disk
  look perfectly current. `clauth daemon --replace --listen` is the in-place
  restart for that hook — `--replace` composes with `--listen`, but the hook has
  to pass both, since a `--replace` on its own takes over without a listener.
  Nothing warns you in advance: a certificate that expires under a running
  daemon produces client-side TLS errors, not a clauth log line.
- **The token.** 32 CSPRNG bytes through SHA-256, hex, so 64 characters.
  Generated on first use and stored at `~/.clauth/auth_token.json` (0600), so it
  survives restarts and only has to be copied to the client once.
  `clauth daemon --print-token` prints it (creating it if absent) and exits;
  `clauth daemon --rotate-token` replaces it, after which every client holding
  the old one gets 401s. Both work whether or not a daemon is running. The
  comparison is constant-time over digests, so neither a token's length nor its
  first wrong byte is observable in the timing.
- **Every route needs it**, health included. An unauthenticated caller gets a
  401 and learns only that something is listening, which is all a liveness probe
  needs. A 401 also closes the connection rather than keeping it alive, so
  reaching the port is not by itself enough to occupy one of the 32 slots.

| Route | What it does |
|---|---|
| `GET /v1/health` | `{"ok":true,"version":"<ver>","schema":1}`. `schema` is the status feed's, so a reader can refuse a daemon newer than it knows. |
| `GET /v1/status` | The `status.json` body below, byte for byte off disk. `?all=1` rebuilds it to include disabled accounts, which the published file always hides. |
| `POST /v1/switch` | Body `{"profile":"<name>"}` (resolved case-insensitively). Returns `{"ok":true,"previous":…,"active":…}`. |

`POST /v1/switch` failures: `404` unknown profile · `409` refused, because the
target is disabled, its credentials were rejected by a refresh, or the live
login is one clauth has not saved (the body's `reason` names the fix, and
nothing was changed) · `409` `switch_in_progress` when another switch is still
running · `503` another clauth process is holding the state lock, so the same
request will work shortly · `400` a malformed body. Refusals carry
`{"ok":false,"error":"<code>","reason":…}`.

The switch is the same action `clauth <name>` and the MCP tool perform, so it
inherits their gates rather than reimplementing them, and the daemon's main loop
picks the result up on its next tick through the ordinary reload path.

Connections persist. HTTP/1.1 keeps the connection open unless the client sends
`Connection: close` (HTTP/1.0 needs `Connection: keep-alive` to opt in), so a
polling client handshakes once rather than once per poll. Every response states
its own `Connection:` disposition and carries a `Content-Length`, and a
kept-alive one advertises `Keep-Alive: timeout=<seconds left>, max=<requests
left>`: the figures are what remains of this connection's budget, not a nominal
maximum, so a client is never told it has time it does not have. Pipelined
requests are served strictly in order, one at a time.

What makes that safe is that framing is never ambiguous: `Content-Length` is the
only accepted framing, chunked transfer encoding is refused outright, two
`Content-Length` headers that disagree are a hard error, and any framing error
answers once and closes rather than trying to resynchronize. Those are the
conditions request smuggling needs, and none of them is available here.

Limits: 8 KiB of headers, 64 KiB of body, 100 requests and 120 seconds per
connection, a 10s deadline on any single read or write, and 32 concurrent
connections. The 10s and the 120s are separate bounds and mean different things.
A read that times out while the connection is idle between requests is just an
idle connection, so the wait resumes up to the 120s budget; the same timeout
part-way through a request is a peer trickling bytes to hold a slot, and fails
with `408`. A connection that has not sent its first request gets only the 10s,
since a client that connects and says nothing has not earned the idle allowance.
`CLAUTH_NO_API=1` disables the listener whatever the flags say, for killing it
without editing the unit that passes `--listen`.

Readers should follow the same evolution rule as the feed (below): ignore
unknown fields, and refuse only on a `schema` greater than what they know.

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

| Field | Semantics |
|---|---|
| `schema` | Integer, currently `1`. Bumped ONLY on a breaking change; additive fields do not bump it (evolution rule below). |
| `generated_at` | Write stamp, ISO-8601 UTC with an explicit `+00:00` offset (all timestamps are; parse the offset, never key on a `Z` suffix; the writer does not emit one). Readers derive staleness from it: a stamp much older than `refresh_interval_ms` means the daemon is gone/stuck, so show last-known data with a stale cue, never spin. |
| `active_profile` | The profile whose credentials are currently installed, else `null`. |
| `pending_switch` | A switch the daemon has accepted but not yet applied (`"<name>"`), else `null`. Exists so readers can show in-flight truth instead of a timing heuristic. Always `null` from the single-shot CLI. |
| `wrap_off` | The fallback chain's stop-vs-stay-on-active flag, verbatim from state. |
| `profiles[].provider` | `"anthropic"` for OAuth profiles, else the recognised provider's display name. |
| `profiles[].tier` | Plan label (`"Max 5x"`, `"Pro"`, `"Free"`…), `null` when nothing on disk claims a tier. Opaque display string, so never switch on it. A **canceled** subscription reports its real post-cancellation tier (`"Free"`) and never the word `canceled`: cancellation is a status, not a tier, and no field here exposes it. `null` is rarer than it looks: a token-only account resolves its tier from the OAuth `subscription_type` alone, Free included. |
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
