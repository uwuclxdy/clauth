# Security

clauth keeps live Claude Code OAuth tokens on disk and replaces its own binary over
the network. Both are worth scrutiny, so this doc spells out what it stores, what it
talks to, what can touch your account, how releases get verified, and how to switch
each thing off.

## Reporting a vulnerability

Report privately through GitHub: open the repo's **Security** tab and pick **Report a
vulnerability**. Please don't file a public issue for anything exploitable. A
description, the affected version, and repro steps are enough to get started.

## Supported versions

Only the latest release. Binary installs stay current through the verified
auto-updater below; `cargo` installs update with `cargo install clauth`.

## Data at rest

Per-profile state lives under `~/.clauth/`. On Unix the whole tree is owner-only: every
clauth-owned file is `0600`, every directory `0700`. No exceptions list to drift out of date.

| Path | Contents | Unix mode |
|------|----------|-----------|
| `~/.clauth/` (root) | everything below | dir `0700` |
| `~/.clauth/profiles/<name>/credentials.json` | OAuth token snapshot | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/session-token.json` | long-lived `claude setup-token` login, if captured (sessions run on this; no refresh token) | file `0600`, dirs `0700` |
| `~/.clauth/profiles/<name>/config.toml` | base URL, API key (endpoint profiles), env block | `0600` |
| `~/.clauth/profiles/<name>/usage_cache.json` | last-known utilization and plan | `0600` |
| `~/.clauth/profiles/<name>/runtime/settings.json` | per-session Claude Code settings (an endpoint profile's API key rides here as `ANTHROPIC_AUTH_TOKEN`) | `0600` |
| `~/.clauth/jobs/<id>.json` | backgrounded `delegate` prompt + result | file `0600`, dir `0700` |
| `~/.clauth/status.json` | menu-bar feed: profile names, usage %, active/pending switch | `0600` |
| `~/.clauth/tokens.json` | menu-bar feed: machine-wide token counts + API-equivalent cost (no token values, no credentials) | `0600` |
| `~/.clauth/daemon.log` | daemon stderr (may echo a config-parse error) | `0600` (under the `0700` dir) |
| `~/.clauth/clauthd.sock` | daemon control socket | `0600` |
| provider stats cache, logs, lock files (`~/.clauth/`) | third-party state, event log, advisory locks | file `0600`, dir `0700` |

- Writes are atomic. The temp file gets mode `0600` at creation, not a chmod
  afterward, so a loose umask never leaves a readable window; it's fsynced, then
  renamed into place (the parent dir is fsynced too, so a power loss can't leave a
  zero-length `profiles.toml`). The `status.json` (1/s) and `tokens.json`
  (per loader update) writes are the exceptions to the fsync — both are
  rebuildable caches, still written `0600`. A rotation caught
  mid-write lands as `credentials.json.pending` and is promoted only once durable.
- Modes are enforced on Unix two ways. Each writer creates its file owner-only. Every
  launch re-tightens the whole tree, so a store from an older build (or a loose umask)
  is repaired the next time clauth runs — on the fork, the daemon chmods an existing
  looser tree back to `0700` on boot (older builds could leave `~/.clauth` or
  `~/.clauth/profiles` at `0755`). The repair never follows a symlink out of the tree,
  so it can't touch a file clauth doesn't own. `daemon.log` is opened by launchd (not
  clauth) at the process umask, so the daemon also chmods it to `0600` on boot to match
  the table above. On Windows, access falls to the default user-profile ACLs, which
  clauth does not loosen.
- A switch rewrites two things: `~/.claude/.credentials.json` and the `env` block of
  `~/.claude/settings.json` (written `0600`). **On macOS it also writes the `Claude
  Code-credentials` login Keychain item** — that Keychain write is what actually
  moves the running Claude Code account; the file writes alone are cosmetic there.
  The rest of `~/.claude/` is left alone. See **Fork surfaces (macOS)** below.

## Network activity

Every request clauth makes, and what rides along with it:

| Endpoint | When | Carries |
|----------|------|---------|
| `api.github.com/repos/uwuclxdy/clauth/releases/latest` + release assets | background update check on launch (binary installs only) | no credentials, just a `User-Agent` |
| `platform.claude.com/v1/oauth/token` | token refresh: lazy (on a 401) for most accounts, a few poll intervals ahead of expiry for the ACTIVE account on macOS (rotation coherence), and `t` force-rotate | your stored refresh token |
| `claude.com/cai/oauth/authorize` | `clauth login` interactive sign-in, opened in your browser | no credentials; a PKCE challenge + random `state` |
| `platform.claude.com/v1/oauth/token` | `clauth login` authorization-code exchange | the one-time auth code + PKCE verifier (mints a fresh token pair) |
| `api.anthropic.com/api/oauth/usage` | usage poll on the refresh interval | access token (Bearer) |
| `api.anthropic.com/api/oauth/profile` | plan-tier detection, and reading which account a token belongs to (so a live re-login can be told apart) | access token |
| `api.anthropic.com/v1/messages` | auto-start kick (opt-in, off by default) | access token; a 1-token Haiku request |
| `status.claude.com/api/v2/incidents.json` | Status tab and background poll | no credentials |
| `raw.githubusercontent.com/BerriAI/litellm/...` | model price table for the Tokens tab cost lens, fetched and disk-cached | no credentials |
| `api.deepseek.com/user/balance` | only for profiles whose base URL is DeepSeek | that provider's API key |
| a custom base URL you set | requests against an API-endpoint profile | whatever you configured |

Your stored access tokens go to `api.anthropic.com` and nowhere else. Your refresh token
goes to `platform.claude.com`, which is the token endpoint Claude Code's own client
refreshes against: every pair is minted there, whether from a refresh or from the
interactive `clauth login`, which follows Claude Code's OAuth flow by opening `claude.com`
in your browser to authorize and posting the one-time code back to `platform.claude.com`.
clauth runs no telemetry or analytics; it talks to the hosts above and no others.

## What acts on your behalf

A few code paths can change account state. All are narrow and all are documented.

Background, automatic:

- **Auto-start kick.** A real, billed `/v1/messages` call (`max_tokens = 1`, a
  fraction of a cent) under your own OAuth token, with the Claude Code client
  identity. It's the same request Claude Code makes on startup, and it exists to arm
  the 5-hour usage window. Off by default, OAuth profiles only; enable it per profile
  on the Setup tab or with `auto_start = true`.
- **Token refresh.** Anthropic refresh tokens are single-use, so refreshing spends
  the stored token for a fresh pair. For most accounts it's lazy: it fires only when
  a usage query returns 401, unless you press `t` to force it. The one exception is
  the ACTIVE account on macOS (rotation coherence, since 2026-07-07): the running
  `claude` reads the same single-use chain out of the Keychain, and whoever refreshes
  first revokes the other side — so clauth rotates that one account a few poll
  intervals ahead of expiry and mirrors the fresh pair into the Keychain, keeping the
  live login permanently valid. If Claude Code refreshes first anyway, clauth adopts
  the fresher pair from `~/.claude/.credentials.json` instead of spending a token —
  gated on the account uuid from `/api/oauth/profile` matching, so a login belonging
  to a different account is never captured unattended.

User-invoked, only when you run the command:

- **Interactive login (`clauth login <profile>`).** Opens your browser to Claude's
  OAuth authorize page and binds a loopback listener on `127.0.0.1:<random port>` to
  catch the redirect, then exchanges the returned code for a fresh token pair written
  into the new profile. It reproduces Claude Code's own PKCE flow, touches no other
  account, and never opens a usage window. On macOS this is why `clauth login` works at
  all — Claude Code's own `/login` under a custom config dir writes only a per-config-dir
  Keychain item, never the profile's credentials file.

Agent-invoked, only when the Claude Code plugin is installed:

- **`delegate` (MCP tool).** Sends a real, billed `/v1/messages` request on a target
  profile under its own OAuth token, opening a full 5-hour usage window on that
  account. It fires only when an agent calls the tool, and is hard-capped at
  recursion depth 1 (a delegated session cannot call `delegate` again).
- **`switch` (MCP tool).** Relinks the global `~/.claude` credentials to another
  profile, the same write `clauth switch` performs. It changes which account the
  global session refreshes onto; it sends no inference itself.

Nothing else sends inference or writes to your account.

## Fork surfaces (macOS)

This fork adds three macOS-specific surfaces on top of upstream's file/symlink model.

- **Keychain write on switch (`/usr/bin/security`).** On macOS, Claude Code reads
  its login from the `Claude Code-credentials` login Keychain item, not
  `~/.claude/.credentials.json`. So every switch — CLI, the MCP `switch` tool, and
  **the unattended daemon auto-switch** — shells out to Apple's code-signed
  `/usr/bin/security` to add-or-update that item, and a switch-off deletes it.
  clauth never *reads* Claude's Keychain item (that would prompt on every call); it
  is write/delete only. The secret travels over the child's **stdin** to
  `security -i`'s command line (PR #21) — it never appears in argv, so a same-UID
  process list (or an EDR exec log) sees only the flags, not the token (see the
  note in `src/keychain.rs`). Each `security` call has a 20-second kill deadline so a
  stuck Keychain (an unanswered "Always Allow" ACL prompt) can't pin the daemon.
  `dist/macos/signed-install.sh` re-signs the binary with a stable identity so the
  one-time "Always Allow" grant persists across rebuilds.
- **Control socket (`~/.clauth/clauthd.sock`) command authority is same-UID.** The
  daemon listens on a Unix socket chmod'd `0600` under the `0700` dir. Any process
  running as the same user can connect and issue `switch` / `refresh` /
  fallback-config commands — there is no additional auth beyond OS file ownership.
  This is the same trust boundary as the files themselves (a same-UID process can
  already read the tokens); the socket only enqueues, and the daemon validates every
  command. It is not exposed on any network.
- **`status.json` / `daemon.log` content.** `status.json` carries profile names,
  usage percentages, and the active/pending switch — **no tokens or API keys**.
  `daemon.log` is daemon stderr; it may echo a config-parse error whose Display
  could include a profile's `config.toml` snippet, so it is written under the `0700`
  dir. Neither is sent anywhere; both are local, user-private files.

## Auto-update verification

Binary installs check for a newer release in the background on launch. Every step
fails closed, so if any of them errors the update is skipped and the running binary
stays put:

1. Ask the GitHub releases API for the latest tag; stop if it isn't newer.
2. Download `sha256sums.txt`. A fetch error stops here (no integrity, no update).
3. Download `sha256sums.txt.minisig` and check it against a minisign public key
   pinned at compile time. A missing or bad signature stops the update. The key is a
   constant, so nothing at runtime can swap it out.
4. Download the platform asset (10 MB ceiling) and check its SHA-256 against the
   now-trusted sums file. A mismatch stops the update.
5. Write to a temp file, fsync, then self-replace atomically. The new binary takes
   over on the next launch.

`cargo` installs (binary under `~/.cargo/bin`) are told an update exists but never
replaced. `CLAUTH_NO_UPDATE=1` turns the whole thing off.

Releases are signed in CI with a passwordless minisign key kept as a GitHub Actions
secret; the signing step writes the key to disk and deletes it on exit. The public
half is pinned in `src/update.rs`.

## Install-script verification

`install.sh` (the `curl | bash` path) uses `cargo` when it's available. When it pulls
a prebuilt binary instead, it downloads `sha256sums.txt` from the same release and
checks the binary against it before installing, failing closed on a download or
checksum error. It writes nothing to your shell profile and only prints a `PATH` hint
when the install dir isn't already on it. If piping a script to a shell isn't your
thing, `cargo install clauth` does the same job.

## Process execution

- `clauth start <profile> [claude args...]` runs `claude` (found on `PATH`) with
  `CLAUDE_CONFIG_DIR` pointed at the profile's isolated runtime, forwarding your extra
  args. Args go through an argument vector, never a shell, so there's no
  shell-injection path.
- On Linux, opening a status-incident link runs `xdg-open <url>` with null stdio. The
  URL comes from the Statuspage feed and is passed as a single argument.
- clauth runs no other external commands.

## First-run shell completions

On the first TUI launch clauth offers to install shell completions. For bash and zsh
it asks before adding a `source` line to your rc file (`[Y/n]`, interactive sessions
only); fish gets its own completions dir. The answer is saved to
`~/.clauth/.completions_installed` so the prompt doesn't come back.
`CLAUTH_NO_COMPLETIONS=1` skips it.

## Build and supply chain

- `unsafe` is denied across the crate (`unsafe_code = "deny"`,
  `unsafe_op_in_unsafe_fn = "deny"`).
- CI runs `cargo fmt --check`, `cargo clippy --all-targets --all-features -D warnings`,
  and the test suite (`--all-features`) on Linux, macOS, Windows for every push and PR.
- `cargo-deny` (advisories denied by default, license allowlist, sources locked to
  crates.io, `openssl` banned in favor of rustls) and `cargo-audit` both run in CI.
- `Cargo.lock` is committed and dependency versions are pinned.

## Switching behaviors off

| Switch | Effect |
|--------|--------|
| `CLAUTH_NO_UPDATE=1` | disables all background update checks and self-replacement |
| `CLAUTH_NO_COMPLETIONS=1` | skips the first-run completion-install prompt |
| `install.sh --nocargo` | forces a verified binary download instead of `cargo install` |
| `cargo install` | never self-replaces; update with `cargo install clauth` |
