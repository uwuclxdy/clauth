# `clauth daemon`: headless scheduler + status feed

`clauth daemon` runs the same background refresher the TUI runs (`spawn_refresher`) with no UI: refresh usage, rotate expiring tokens, run the fallback chain's auto-switches, and publish `~/.clauth/status.json` for external readers. `clauth status --json` prints the same shape single-shot, no daemon required. This document is the read contract for both.

Scope note: by default the daemon carries **no external surface at all**: it
publishes a file and reads a file, and everything reaches it through
`~/.clauth`. `--listen` opts into one, an authenticated TLS REST API whose only
mutation is a switch (see below); without that flag nothing listens. What holds
either way: anything that needs eyes (a diverged live login, a manual Claude
Code `/login`, an unprovable identity) is refused and logged, and the TUI stays
the only resolution surface. The REST switch refuses those cases with a 409
exactly as the scheduler and the MCP tool refuse them.

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

## REST API (`--listen`)

`clauth daemon --listen 0.0.0.0:8443` also serves the feed and the switch over
HTTPS, for running the daemon on one machine and a client (clauth-tray) on
another. Off unless the flag is passed. TLS comes from this host's
[lego](https://github.com/go-acme/lego) certificate, on all three platforms
clauth supports — macOS, Linux, and Windows. Only two things differ by platform:
where that certificate lives, and how the host's own FQDN is discovered. Both
are spelled out below, and `--cert`/`--key` skip the derivation entirely for
hosts where it cannot work.

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
- **`--cert` and `--key` skip the derivation entirely.** For the hosts where it
  cannot work rather than merely points somewhere else: on a tailnet node
  `hostname -f` answers a name no certificate covers, and
  `tailscale cert <node>.<tailnet>.ts.net` writes a `.crt` and a `.key` with no
  issuer file beside them and none of lego's naming. Passing both by path reads
  exactly those two files — no `hostname -f`, no `tls.json`, and no sibling
  `.issuer.crt` even if one happens to sit there, so the chain served is the one
  in the file you named. The pair is required together and only alongside
  `--listen`; a lone `--cert` is a usage error rather than a silent fall back to
  lego. Everything else is unchanged: clients still dial the name the
  certificate covers, and the certificate is still read once at startup, so the
  restart hook below applies to a `tailscale cert` renewal too.
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
  the old one gets 401s. Both work whether or not a daemon is running. A rotation
  takes effect against a **running** daemon: the file is re-read per request, so
  the old token stops working without a restart. The comparison is constant-time
  over digests, so neither a token's length nor its first wrong byte is
  observable in the timing.
- **The file records a tier**, `"tier": "control"`, which is the only value there
  is: this token does everything the API exposes. It is written now so that a
  narrower token later — read-only for a wall display, for instance — is a new
  value in a field every deployed file already carries rather than a schema bump
  with a migration behind it. A file from before the field reads as `control`,
  which is what it was, so an upgrade rotates nothing. A tier this build does
  *not* know refuses to start and leaves the file alone: serving it would promote
  a token a newer clauth deliberately restricted, and replacing it would revoke,
  from a downgrade, a credential you had distributed on purpose. A file that is
  unusable for any other reason — bad JSON, a truncated token — is replaced, and
  now says so in the log rather than silently 401ing every client.
- **Every route needs it**, health included. An unauthenticated caller gets a
  401 and learns only that something is listening, which is all a liveness probe
  needs. A 401 also closes the connection rather than keeping it alive, so an
  unauthenticated client cannot *hold* one of the 32 slots — but it does occupy
  one while it is connected. The slot is taken at `accept()`, before the TLS
  handshake and before any token is seen, because refusing at that point is the
  cheapest refusal available and the alternative is spawning threads for
  unauthenticated peers without bound. What bounds the occupancy is the clock,
  not the token: a client that connects and says nothing gets only the 10s
  first-request timeout, not the 120s lifetime, and one that says something
  unauthenticated is answered and closed at once.

| Route | What it does |
|---|---|
| `GET /api/v1/health` | `{"ok":true,"version":"<ver>","schema":1}`. `schema` is the status feed's, so a reader can refuse a daemon newer than it knows. |
| `GET /api/v1/status` | The `status.json` body below, byte for byte off disk. Conditional: the `ETag` digests everything in the body except `generated_at`, so a feed rewritten by a tick that changed nothing answers `304` with no body. `?wait=<secs>` on a request already carrying the current tag holds it open until the accounts actually move (capped at 60s, inside the connection's own 120s lifetime), which is how a client follows a switch without polling. `?all=1` rebuilds the body to include disabled accounts, which the published file always hides, and never waits. |
| `POST /api/v1/switch` | Body `{"profile":"<name>"}` (resolved case-insensitively). Returns `{"ok":true,"previous":…,"active":…}`. Republishes `status.json` before answering, so every reader parked on `GET /api/v1/status?wait=` is woken by the same switch rather than by the next tick. |

`POST /api/v1/switch` failures: `404` unknown profile · `409` refused, because the
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

**Neither bound is a response-time budget, and a switch is the case where that
matters.** The 10s applies to one read or one write, not to the handler between
them: `POST /api/v1/switch` waits up to 25 seconds for the cross-process state
flock (`clauth`'s own bound, sized around a macOS Keychain switch) and may run a
token refresh after that, so a switch can legitimately take tens of seconds
before a single byte of the response is written. Nothing times out — the whole
of it sits inside the connection's 120s lifetime — but a client that sets a 10s
socket deadline because this page mentions 10s will give up on a switch that was
about to succeed. Size a client timeout against the 120s, and treat a `409`
(state lock held) as the retryable answer rather than an abandoned request.

Readers should follow the same evolution rule as the feed (below): ignore
unknown fields, and refuse only on a `schema` greater than what they know.

## `~/.clauth/status.json`

Written each daemon tick, and by whichever process lands a switch when no
daemon is running to do it — so the published `active_profile` is never an
account the operator has already switched away from, whether the switch came
from the TUI, `clauth <name>`, or the MCP tool. A running daemon owns the file:
a switch made elsewhere leaves the write to that daemon's next tick (≤1s), which
republishes it with the scheduler's live `fetch_status` / `next_refresh_at` /
`pending_switch` that a single-shot build cannot see. A switch the daemon itself
performs through `POST /api/v1/switch` is the exception: it republishes before
answering, so a client waiting on the feed is woken by the switch rather than by
the tick after it. Atomic (`tmp` + rename into place), `0600`. **Never carries a
token, secret, or key**: names, tiers, percentages, timestamps only.

A reader that wants a switch the moment it happens has two ways in. Over the
network, `GET /api/v1/status?wait=<secs>` with the current `ETag` blocks until the
feed's content actually changes — no polling interval to lose time to. On the
same machine, watch this file *and* `~/.clauth/profiles.toml`, which every switch
surface rewrites and nothing else touches that often.

Compare more than the modification time when you do. A daemon rewrites this file
every second through `tmp` + rename, two writes can land inside one filesystem
clock granule, and a feed republished for a different active account is often
exactly as long as the one it replaced — so `(mtime, len)` can miss a real
switch. The renamed inode always differs, which is what makes the change
detectable whatever the clock did.

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
      "rolling_token": false,
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
      "auto_start_queue": { "position": 1, "next_open_at": "2026-07-03T21:34:20+00:00" },
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
