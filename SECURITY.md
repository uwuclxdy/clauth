# Security

clauth keeps live Claude Code OAuth tokens on disk and replaces its own binary over the network. Both are worth scrutiny, so this doc spells out what it stores, what it talks to, what can touch your account, how releases get verified, and how to switch each thing off.

## Reporting a vulnerability

Report privately through GitHub: open the repo's **Security** tab and pick **Report a vulnerability**. Please don't file a public issue for anything exploitable. A description, the affected version, and repro steps are enough to get started.

## Supported versions

Only the latest release. Binary installs stay current through the verified auto-updater below; `cargo` installs update with `cargo install clauth`.

## Data at rest

Per-profile state lives under `~/.clauth/`. On Unix that whole tree is owner-only: every clauth-owned file is `0600`, every directory `0700`. No exceptions list to drift out of date. The only credential copy outside it is the macOS Keychain item below. clauth's one other tree of its own is `~/.local/share/clauth/`, which holds the bundled plugin and no credentials; what a switch writes into Claude Code's own `~/.claude/` is listed at the end of this section.

| Path | Contents | Unix mode |
|------|----------|-----------|
| `~/.clauth/profiles/<name>/credentials.json` | OAuth token snapshot | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/mcp-logins.json` | Claude Code's MCP-server OAuth logins, parked here whenever the profile stores no Claude login of its own and merged back once it regains one. Live bearer credentials, minted against each server and belonging to no Claude account | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/session-token.json` | long-lived `claude setup-token` login, if captured (sessions run on this; no refresh token) | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/session-token.static.json` | the `claude setup-token` mint a rolling token superseded, kept so `clauth static-token <p>` (or a dead chain) can restore it | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/quarantine/<ts>-<seq>.<basename>` | credential files moved aside before repair, kept as evidence — a mis-filled sidecar (`….session-token.json`, which by definition carries a refresh token: that is what made it a mis-fill) or a backup slot whose content was not a mint (`….session-token.static.json`). Evidence can hold live credentials, which is why the dir is `0700`, why nothing prunes it automatically, and why it lives under the profile so `clauth delete` removes it with everything else that account owns | file `0600`, dir `0700` |
| `~/.clauth/profiles/<name>/config.toml` | base URL, API key (endpoint profiles), env block | `0600` |
| `~/.clauth/profiles/<name>/usage_cache.json` | last-known utilization and plan | `0600` |
| `~/.clauth/profiles/<name>/runtime-<sid>/settings.json` | one live session's Claude Code settings. An endpoint profile's key is **not** in it: the file carries an `apiKeyHelper` line naming `clauth __api-key <profile>`, which Claude Code runs per request to mint the key | `0600` |
| `~/.clauth/auth_token.json` | bearer token for the daemon's REST API, only if you have ever run `clauth daemon --listen`, `--print-token`, or `--rotate-token` | `0600` |
| `~/.clauth/tls.json` | which directory holds the REST API's lego certificate; written with the platform default the first time `clauth daemon --listen` starts. Not a secret — a path, no key material | `0600` |
| `~/.clauth/jobs/<id>.json` | backgrounded `delegate` prompt + result | file `0600`, dir `0700` |
| `~/.clauth/live_sessions/<sid>.json`, `~/.clauth/live_bare/<pid>` | liveness markers for running sessions: pid, profile name, working directory, flags. No credentials | file `0600`, dir `0700` |
| usage and price caches, session history, logs, lock files (`~/.clauth/`) | last-known usage, third-party state, burn samples, event log, advisory locks | file `0600`, dir `0700` |
| `~/.local/share/clauth/` (macOS `~/Library/Application Support/clauth/`, Windows `%APPDATA%\clauth\`) | the bundled Claude Code plugin, laid down where `claude plugin` can register it: `plugin.json`, the `hooks/` dir, a generated `marketplace.json`, and an install marker under `markers/`. Content-keyed under `versions/`, with a `current@claude` pointer at the live one. No credentials | your umask; not re-tightened |

On macOS a second copy of the active login lives outside this tree, in the login Keychain, because that is where Claude Code reads it from:

| Item | Contents | Written by |
|------|----------|------------|
| generic password `Claude Code-credentials`, account `$USER` | the OAuth pair for whichever profile is linked | `/usr/bin/security`, one item per `CLAUDE_CONFIG_DIR` |

clauth writes and clears that item; it never reads it back during normal operation. The command line goes to `security -i` over stdin rather than argv, so the token never appears in the process table. Access is whatever the login Keychain grants, not `0600`.

- Writes are atomic. The temp file gets mode `0600` at creation, not a chmod afterward, so a loose umask never leaves a readable window; it's fsynced, then renamed into place. A rotation caught mid-write lands as `credentials.json.pending` and is promoted only once it's durable.
- Modes are enforced on Unix two ways: each writer creates its file owner-only, and every launch re-tightens the whole `~/.clauth` tree, repairing a store from an older build or a loose umask.
  - The repair never follows a symlink out of the tree, so it can't touch a file clauth doesn't own. On Windows, access falls to the default user-profile ACLs, which clauth does not loosen.
- A switch rewrites three files: `~/.claude/.credentials.json`, parts of `~/.claude/settings.json` (the `env` block, the top-level `model` key, `apiKeyHelper`), and `~/.claude.json`, whose stale account-identity block is dropped so Claude Code re-derives identity from the new token.
  - The rest of `~/.claude/` is left alone. On macOS the switch writes the Keychain item above as well, because Claude Code reads the Keychain before the file there.

## Network activity

Every request clauth makes, and what rides along with it:

| Endpoint | When | Carries |
|----------|------|---------|
| `api.github.com/repos/uwuclxdy/clauth/releases/latest` + release assets | background update check on launch (binary installs only) | no credentials, just a `User-Agent` |
| `platform.claude.com/v1/oauth/token` | token refresh ahead of expiry, on a rejected request, and on `t` force-rotate | your stored refresh token |
| `claude.com/cai/oauth/authorize` | `clauth login` interactive sign-in, opened in your browser | no credentials; a PKCE challenge + random `state` |
| `platform.claude.com/v1/oauth/token` | `clauth login` authorization-code exchange | the one-time auth code + PKCE verifier (mints a fresh token pair) |
| `api.anthropic.com/api/oauth/usage` | usage poll on the refresh interval | access token (Bearer) |
| `api.anthropic.com/api/oauth/profile` | plan-tier detection, and reading which account a token belongs to (so a live re-login can be told apart) | access token |
| `api.anthropic.com/v1/messages` | auto-start kick (opt-in, off by default) | access token; a 1-token Haiku request |
| `status.claude.com/api/v2/incidents.json` | Status tab and background poll | no credentials |
| `raw.githubusercontent.com/uwuclxdy/ai-pricelog/...` | model price table for the Tokens tab cost lens, fetched and disk-cached | no credentials |
| `api.deepseek.com/user/balance` | only for profiles whose base URL is DeepSeek | that provider's API key |
| `api.z.ai/api/monitor/usage/...` | only for profiles whose base URL is Z.ai | that provider's API key |
| `openrouter.ai/api/v1/credits` and `/api/v1/key` | only for profiles whose base URL is OpenRouter | that provider's API key |
| an Alibaba console gateway (`bailian-cs.console.aliyun.com` or its regional twin for your site) | usage poll for a Model Studio profile, whose API key cannot read its own quota | that profile's stored `[console]` session, never its API key |
| `bailian.console.aliyun.com` or `modelstudio.console.alibabacloud.com` | `clauth login` on a Model Studio profile, opened in your browser to capture that console session | no credentials; the callback comes back to a loopback listener |
| a custom base URL you set | requests against an API-endpoint profile, plus a best-effort usage probe against that same origin | whatever you configured |

Your stored Claude access tokens go to `api.anthropic.com` and nowhere else. Your refresh token goes to `platform.claude.com`, which is the token endpoint Claude Code's own client refreshes against: every pair is minted there, whether from a refresh or from the interactive `clauth login`, which follows Claude Code's OAuth flow by opening `claude.com` in your browser to authorize and posting the one-time code back to `platform.claude.com`. clauth runs no telemetry or analytics; it talks to the hosts above and no others.

### Listening sockets

clauth binds a socket in exactly two places, both narrow:

| Listener | When | Reachable from |
|----------|------|----------------|
| `127.0.0.1:<random port>` | for the seconds `clauth login` waits for the browser redirect | loopback only; it checks the OAuth `state` and closes |
| the address you pass to `clauth daemon --listen` | for as long as that daemon runs | wherever you bind it |

`--listen` is off unless you ask for it, and it is the only way anything outside this
machine can reach clauth. It is TLS-only (from this host's lego certificate, read at
startup) and every route requires a bearer token, compared in constant time, stored at
`~/.clauth/auth_token.json` and printed by `clauth daemon --print-token`. It exposes two
operations, reading the status feed and switching the active account, and the feed it
serves carries what `status.json` carries: names, tiers, percentages, timestamps, never a
token or key. Connections persist and may be pipelined; `Content-Length` is the only
framing accepted, chunked is refused, and any framing error closes the connection rather
than resynchronizing, so the ambiguity request smuggling depends on does not arise. An
unauthenticated request closes the connection too, so reaching the port is not by itself
enough to occupy a connection slot.
Limits: 8 KiB of headers, 64 KiB of body, 32 concurrent connections, 100 requests and
120 seconds per connection, a 10s deadline per read or write.
`CLAUTH_NO_API=1` disables it. See `wiki/Daemon.md`.

## What acts on your behalf

A few code paths can change account state. All are narrow and all are documented.

Background, automatic:

- **Auto-start kick.** A real, billed `/v1/messages` call (`max_tokens = 1`, a fraction of a cent) under your own OAuth token, with the Claude Code client identity, to arm the 5-hour usage window.
  - It's the same request Claude Code makes on startup. Off by default, OAuth profiles only; enable it per profile on the Setup tab or with `auto_start = true`.
- **Auto-switch.** When the fallback chain is armed, clauth relinks the global credentials to another account on its own once the active one runs out of headroom, from the TUI or from `clauth daemon`.
  - It sends no inference itself. The chain is empty by default, and an account outside it is never switched to.
- **Pay-as-you-go spend.** With extra usage enabled, an auto-switch can land on an account that bills real money. Three things must all be true first: the chain-wide `allow extra usage` toggle is on, that account carries a `max spend` ceiling above $0, and billing is enabled at Anthropic.
  - All three are off or zero by default. An account with subscription quota left always wins over one that costs money, and once the ceiling is spent clauth stops using that account.
- **Token refresh.** Anthropic refresh tokens are single-use, so refreshing spends the stored token for a fresh pair. By default it fires ahead of expiry, early enough that a running `claude` never reaches its own refresh threshold.
  - Set the Config tab's `rotation` row to `lazy` to refresh only after a request is rejected. Pressing `t` forces a rotation either way.

User-invoked, only when you run the command:

- **Interactive login (`clauth login <profile>`).** Opens your browser to Claude's OAuth authorize page and binds a loopback listener on `127.0.0.1:<random port>` to catch the redirect, then exchanges the returned code for a fresh token pair written into the new profile.
  - It reproduces Claude Code's own PKCE flow, touches no other account, and never opens a usage window. On macOS this is why `clauth login` works at all: Claude Code's own `/login` under a custom config dir writes only a per-config-dir Keychain item, never the profile's credentials file.
- **`clauth start` / `clauth resume`.** Spawns `claude` against the profile you named, so everything that session sends bills to that account. clauth forwards your args and sends nothing of its own.

- **Rolling session token (`clauth rolling-token <profile>`).** Points the profile's `session-token.json` at that profile's own OAuth usage chain: the daemon re-stamps the file with the chain's current access token, minus the refresh token, so sessions still hold nothing rotatable.
  - It also **widens what that credential can reach**. A `claude setup-token` mint carries two scopes, `user:inference` and `user:sessions:claude_code`; the rolling bearer carries the chain's full granted set.
  - The browser login requests six scopes (`org:create_api_key`, `user:profile`, `user:inference`, `user:sessions:claude_code`, `user:mcp_servers`, `user:file_upload`); every real Pro/Max login observed so far grants the five without `org:create_api_key`, and the bearer carries whatever the account's grant actually was.
  - Anything that can read the sidecar, or the live `~/.claude/.credentials.json` a switch installs it into, can use every one of those scopes until the token expires — which is hours rather than the mint's year.
  - The command prints the scope list when it arms, so the widening is stated where the decision is made. `clauth static-token <profile>` restores the narrower mint; a terminally dead chain does the same automatically.

Agent-invoked, only when the Claude Code plugin is installed:

- **`delegate` (MCP tool).** Sends a real, billed `/v1/messages` request on a target profile under its own OAuth token, opening a full 5-hour usage window on that account.
  - It fires only when an agent calls the tool, and is hard-capped at recursion depth 1 (a delegated session cannot call `delegate` again).
- **`switch_profile` (MCP tool).** Relinks the global `~/.claude` credentials to another profile, the same write `clauth switch` performs. It changes which account the global session refreshes onto; it sends no inference itself.

Network-invoked, only while `clauth daemon --listen` is running:

- **`POST /v1/switch`.** The same relink as the `switch` MCP tool, performed for a
  caller that presented the bearer token. It sends no inference itself, and it refuses
  the cases that need a human (a login clauth has not saved, credentials a refresh has
  rejected, a disabled account) rather than resolving them unattended.

Nothing else sends inference or writes to your account.

## Auto-update verification

Binary installs check for a newer release in the background on launch. Every step fails closed, so if any of them errors the update is skipped and the running binary stays put:

1. Ask the GitHub releases API for the latest tag; stop if it isn't newer.
2. Download `sha256sums.txt`. A fetch error stops here (no integrity, no update).
3. Download `sha256sums.txt.minisig` and check it against a minisign public key pinned at compile time. A missing or bad signature stops the update. The key is a constant, so nothing at runtime can swap it out.
4. Download the platform asset (10 MB ceiling) and check its SHA-256 against the now-trusted sums file. A mismatch stops the update.
5. Write to a temp file, fsync, then self-replace atomically. The new binary takes over on the next launch.

`cargo` installs (binary under `~/.cargo/bin`) are told an update exists but never replaced. `CLAUTH_NO_UPDATE=1` turns the whole thing off.

Releases are signed in CI with a passwordless minisign key kept as a GitHub Actions secret; the signing step writes the key to disk and deletes it on exit. The public half is pinned in `src/update.rs`.

## Install-script verification

`install.sh` (the `curl | bash` path) uses `cargo` when it's available. When it pulls a prebuilt binary instead, it downloads `sha256sums.txt` from the same release and checks the binary against it before installing, failing closed on a download or checksum error. It writes nothing to your shell profile and only prints a `PATH` hint when the install dir isn't already on it. If piping a script to a shell isn't your thing, `cargo install clauth` does the same job.

## Process execution

Every command below goes through an argument vector, never a shell, so there is no shell-injection path.

| Command | When |
|---------|------|
| `claude` (from `PATH`) | `clauth start`, `clauth resume`, and the MCP `delegate` tool, with `CLAUDE_CONFIG_DIR` pointed at that session's runtime and your extra args forwarded |
| `clauth mcp`, `claude --version` | Plugin-tab checks: a JSON-RPC handshake against clauth's own server, and Claude Code's version |
| `claude plugin …` (`marketplace add`, `install`, `list --json`) | registering the bundled plugin and repairing a broken registration: the Plugin tab's install, the `clauth self-heal` hook, and the gated heal `clauth start`, `clauth mcp` and the daemon tick share, which runs only when a registry read says the registration is broken |
| `herdr` (from `HERDR_BIN_PATH`, else `PATH`) | `clauth herdr install` / `uninstall`, which drive herdr's own plugin installer and let herdr validate the config before it is written; plus `pane report-metadata` during a `delegate` run, with the herdr pane knobs on |
| `/usr/bin/security` | macOS only: writing and clearing the Keychain item above |
| `xdg-open` (Linux), `open` (macOS), `rundll32` (Windows) | opening a URL: the browser login page, or a status incident from the Status tab. The URL is passed as one argument |
| `kill` / `taskkill`, plus `ps` on macOS and `tasklist` on Windows | `clauth daemon --replace` only: the pid is checked against a running clauth daemon before it is signalled (Linux reads `/proc/<pid>/cmdline` instead of shelling out) |
| `hostname -f` (macOS, Linux), `powershell.exe` evaluating `[System.Net.Dns]::GetHostEntry($env:COMPUTERNAME).HostName` (Windows) | `clauth daemon --listen` only: once at startup, to learn which lego certificate to load. No part of it comes from you — the argument vector is a compile-time constant — and the answer is rejected unless it looks like a hostname before it is used as a filename in the certificate directory named by `~/.clauth/tls.json`, which is yours to edit and owner-only like the rest of the tree |

clauth runs no other external commands.

## First-run shell completions

On the first TUI launch clauth offers to install shell completions. For bash and zsh it asks before adding a `source` line to your rc file (`[Y/n]`, interactive sessions only); fish gets its own completions dir. The answer is saved to `~/.clauth/.completions_installed` so the prompt doesn't come back. `CLAUTH_NO_COMPLETIONS=1` skips it.

## Build and supply chain

- `unsafe` is denied across the crate (`unsafe_code = "deny"`, `unsafe_op_in_unsafe_fn = "deny"`).
- CI runs `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and the test suite (`--all-features`) on Linux, macOS, Windows for every push to `mommy` and every pull request that touches code.
- `cargo-deny` (advisories denied by default, license allowlist, sources locked to crates.io, `openssl` banned in favor of rustls) and `cargo-audit` both run in CI.
- `Cargo.lock` is committed and every dependency-resolving CI leg passes `--locked`, so a build resolves the versions it records or fails, rather than quietly taking whatever is newest.

## Switching behaviors off

| Switch | Effect |
|--------|--------|
| `CLAUTH_NO_UPDATE=1` | disables all background update checks and self-replacement |
| `CLAUTH_NO_COMPLETIONS=1` | skips the first-run completion-install prompt |
| `CLAUTH_NO_API=1` | stops `clauth daemon --listen` from opening its socket, whatever the flags say |
| an empty `fallback_chain` (the default) | clauth never switches accounts on its own |
| `allow extra usage` off (the default) | no auto-switch can reach an account that bills money |
| `auto_start = false` (the default) | clauth sends no inference of its own |
| `install.sh --nocargo` | forces a verified binary download instead of `cargo install` |
| `cargo install` | never self-replaces; update with `cargo install clauth` |
