# Codex

clauth runs OpenAI codex accounts beside your Claude Code ones. A codex profile is a ChatGPT login clauth stores, keeps refreshed and launches `codex` under, in a `CODEX_HOME` of its own, so several ChatGPT accounts can be used from one machine and rotate under their own auto-switch chain.

Requires the `codex` CLI on `PATH` and a ChatGPT login (`codex login`); an API-key-only codex setup has no token chain to manage and is refused. Built and verified against codex 0.145.

## Add an account

Two ways in. Neither switches to the profile, and neither needs Claude Code.

### Adopt the login your own codex holds

```sh
codex login                # once, in your own shell, if you have not already
clauth login work --codex  # adopt it as codex profile 'work'
```

clauth reads `~/.codex/auth.json` (or `$CODEX_HOME/auth.json` when that variable is set), copies the chain into `~/.clauth/profiles/work/auth.json` with every key codex wrote and `last_refresh` set to the capture time, records the profile in `~/.clauth/codex-profiles.toml`, then replaces `~/.codex/auth.json` with a symlink onto that store. It prints:

```
clauth: captured the operator codex login into codex profile 'work'
clauth: /home/you/.codex/auth.json now follows the profile store — your own codex and clauth sessions share one chain
clauth: while it does, `codex login` and `codex logout` reach 'work's chain through that link and revoke it server-side — remove the link first if you mean to mint a chain for a different account
```

One physical file is the point. A codex refresh token is single-use and rotates on every refresh, so two copies of one chain kill each other: the first copy to refresh spends the token the other still holds. With the link in place your own `codex`, every `clauth start work` session and clauth's own refresher all read and write the same file, and codex reloads it before spending, so they cooperate.

Do not run `codex login` or `codex logout` through that link. codex's own login and logout revoke the login they find at `~/.codex/auth.json` on the server (clauth refuses a capture through such a link for the same reason), and through the link that login is the profile's chain: the profile dies on the server, and no re-capture brings it back. To log your own codex into a different ChatGPT account, remove the link first (`rm ~/.codex/auth.json` leaves the profile intact), then `codex login`, then capture that into a second profile.

Running the capture again on an existing codex name re-captures in place. It refuses to swap the ChatGPT account under a name (`'work' stores ChatGPT account <old>, but this login is account <new> — log into a new profile, or delete 'work' first`), and it refuses while a session of that profile is live.

### Mint a fresh login in the browser

```sh
clauth login spare --codex --browser
```

clauth runs codex's browser login itself and lands the minted chain straight in the profile store. Your `~/.codex` is not read and not changed. It prints `clauth: opening <url>` and `clauth: if the browser did not open, paste that URL into it`, waits for the sign-in, then:

```
clauth: logged a fresh codex chain into codex profile 'spare'
clauth: ChatGPT account <id>
clauth: run it with `clauth start spare` — your own ~/.codex is untouched
```

The sign-in page sends the browser back to `http://localhost:1455/auth/callback` (port 1457 when 1455 is taken), and the listener binds the loopback address alone, so the browser has to run on the machine clauth runs on, or have that port forwarded to it. With both ports held: `codex's login ports (1455 and 1457) are both in use — close whatever holds them (another codex or clauth login?) and retry`. Five minutes without a callback ends in `timed out waiting for the codex login callback`; declining the consent page ends in `you declined the authorization request`. Either way nothing is written and the command can be re-run.

A name already held by a Claude Code profile is refused for either form: `'work' is a claude profile — profile names span both harnesses, pick another`. Profile names are one namespace across both rosters, which is what lets every other command take a bare name.

## Run

```sh
clauth start work              # codex under work's own CODEX_HOME
clauth start work --isolated   # the same login, none of your ~/.codex surfaces
```

`clauth start` on a codex name launches `codex`, not `claude`. It prints `clauth: preparing codex home` as it builds the session's home, sets `CODEX_HOME` to it, and passes two `-c` overrides before anything you typed: `cli_auth_credentials_store="file"`, so codex reads the linked `auth.json` rather than a keyring, and `sqlite_home=<that home>`, so the session's state databases land in it. Anything after the profile name reaches `codex` verbatim; put `--` before a flag both programs spell. Several sessions of one profile can run at once; they share the one chain.

The session home is `~/.clauth/profiles/work/codex-home-<sid>/`:

| Entry | Shared (default) | `--isolated` |
|-------|------------------|--------------|
| `auth.json` | a link to the profile's chain | the same link |
| `config.toml`, and every `*.config.toml` layer `codex --profile` reads | copies of your `~/.codex` ones with `sqlite_home`, `cli_auth_credentials_store` and `debug.config_lockfile` removed (codex writes its config in place, so it is never a link) | the same copies |
| `skills`, `rules`, `agents`, `templates`, `references`, `AGENTS.md`, `plugins` | links to your `~/.codex` entries | absent |
| `hooks.json` | linked only when the profile's `config.toml` sets `hooks_json = true` | absent |
| `sessions/`, `archived_sessions/`, `history.jsonl`, the `*.sqlite` state stores | links into `~/.clauth/profiles/work/codex-home/`, the store every session of the profile shares | per session, discarded at exit |

A thread is in the profile's store the moment codex writes it, so a later session lists it, and archiving one keeps it. The per-session home is removed when the session ends; the store never is.

The environment is scrubbed before the spawn: `CODEX_HOME`, `CODEX_SQLITE_HOME`, `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `CODEX_REFRESH_TOKEN_URL_OVERRIDE`, `CODEX_REVOKE_TOKEN_URL_OVERRIDE` and `CODEX_APP_SERVER_LOGIN_CLIENT_ID` are dropped, as are the active Claude Code profile's own `[env]` keys and a `CLAUDE_CONFIG_DIR` naming a clauth runtime, so the session spends the account it names and nothing inherited.

`--with-fallback` is refused: `--with-fallback is not available on a codex profile: codex reads auth.json once at start, so a chain lands at the NEXT start, not mid-session — start without the flag`. `--explain` prints `would start on 'work'` and exits. `--auto` walks the Claude Code chain and never lands on a codex profile. A chain clauth has declared dead is refused before anything is built ([below](Codex#when-a-chain-dies)). ``failed to launch codex — is the `codex` CLI installed and on PATH?`` is what a missing `codex` looks like.

Inside the session, `clauth which` prints the profile name, and `clauth which --json` answers `{"profile": "work", "source": "codex_home", "harness": "codex", ...}`.

## Switch

```sh
clauth work
# clauth: switched codex to 'work'
```

A codex switch moves the active marker in `codex-profiles.toml`. When your own `~/.codex/auth.json` is the link clauth installed into a profile's store, the link follows the switch (`clauth: ~/.codex/auth.json now follows 'work'`), so the codex you run yourself uses the account the marker names; a plain file, or a link clauth did not install, is left alone. Nothing under `~/.claude` moves, and no running session changes account. The marker is what the codex chain anchors on, what the Overview marks, and what `status.json` publishes as `active_codex_profile`; `clauth start` runs any codex profile whether or not it is the active one. A name held on both rosters switches the Claude Code one and says so: `clauth: note — 'work' also names a codex profile; switching the CLAUDE one`.

## Remove

```sh
clauth delete work
# clauth: delete profile 'work' and all its credentials? [y/N]
```

The same gate as a Claude Code delete: `--yes` skips the prompt and is required on a non-TTY stdin (`refusing to delete 'work' without confirmation; pass --yes for a non-interactive delete`), `--force` is the only way past `'work' has a live session, pass --force to remove it anyway`, and anything but `y` or `yes` leaves it with `clauth: aborted. 'work' left in place.` A confirmed delete removes `~/.clauth/profiles/work/` whole, the profile's threads and state stores under `codex-home/` included, then drops the roster row, and prints `clauth: removed codex profile 'work'.`

When `~/.codex/auth.json` is the link a capture installed onto this profile, the delete removes that link too and says what that means:

```
clauth: /home/you/.codex/auth.json followed that profile's chain and is detached now, so your own codex has no login; run `codex login` to mint a fresh one
```

A link onto another profile, or a real file (you logged your own codex back in), is left as it is.

## Auto-switch

The codex chain is separate from the Claude Code one and lives in `~/.clauth/codex-profiles.toml`, hand-edited: `fallback_chain` in walk order, `wrap_off`, and its own `weekly_switch_threshold` ([Configuration](Configuration#codex-profilestoml)). It is walked with the same rules ([Auto-switch](Auto-Switch#codex)): the active codex profile has to be a chain member, it has to be exhausted (5h past 95%, or the weekly window past the codex file's line) or dead, and the walk takes the next member with headroom, preferring one whose usage was read live. A dead member is one whose chain clauth has quarantined, or whose usage polls keep answering 401 past two forced refreshes.

What differs is when the switch lands. codex reads `auth.json` once at start, so a switch moves the active marker and takes effect at the next `clauth start`; a running session finishes on the account it started with. The daemon log reads `clauth: codex auto-switched to '<name>' — live at the next codex session`. With `wrap_off = true` and every member spent, the active slot is cleared (`clauth: every codex account is spent — codex active slot cleared`) and stays clear until you switch to a member by hand.

## Usage and refresh

The refresh loop, an open TUI or `clauth daemon`, whichever holds the fetch lease, polls each codex profile's usage once per refresh interval from ChatGPT's usage endpoint, using the profile's own chain. The two windows it reports fill the Overview's 5h and 7d columns and `status.json`'s `windows[]`; when the server reports the account as blocked, the fuller window reads 100% whatever its own percentage says. The plan cell is the ChatGPT plan word the account reports.

The same loop rotates a profile's chain ten minutes before its access token expires, under three rules:

- **No replay.** A refresh token works once. A failed refresh is never retried with the same token on the routine pass; the token clauth last spent is remembered in `auth.attempt`, which survives a restart. A usage poll answered 401 buys one forced attempt past that memo, and after two such attempts in a row clauth stops forcing until a poll succeeds again.
- **Stand down for a live session.** Inside the five minutes before expiry codex refreshes on its own, and a running session that holds the chain gets that window to itself. On a host without symlinks the session holds a separate copy, so clauth stands down for the whole session.
- **Keep a last-known-good copy.** Every well-formed read of the store is copied to `auth.lkg.json`; the copy is restored only after the store reads unreadable for 30 seconds with no session live, logged as `clauth: '<name>' codex auth.json read bad for <n>s — restored the last-known-good copy`.

A landed rotation logs `clauth: rotated codex chain for '<name>'`; a failed one, `clauth: codex refresh for '<name>' failed: <why>`.

### When a chain dies

A verdict the server calls final quarantines the chain. From then on the Overview row carries `×`, `status.json` reports `auth_status: "broken"`, the chain walk skips the member, and `clauth start` or a switch onto it refuses with:

```
'work': codex chain is broken (<kind> since <time>), run `clauth login work --codex --browser`
```

| Kind | Meaning |
|------|---------|
| `reused` | the refresh token had already been spent: a second copy of the chain refreshed first, or a reply was lost |
| `expired` | the chain aged out on the server |
| `invalidated` | the chain was revoked on the server side |
| `lost` | clauth's own verdict: the server accepted a rotation, but the new pair could not be written to the store, so the token the store holds is spent (``clauth: '<name>' codex rotation succeeded on the wire but the store write failed (<why>); the chain is spent, run `clauth login <name> --codex --browser` ``) |

No refresh revives a dead chain; a new login is the only exit, and the browser form is the one that works for every profile (a re-capture finds `~/.codex/auth.json` already pointing at the profile and captures nothing). The verdict is bound to the token it judged, so a fresh chain clears it however it lands. A refusal the server does not spell out about the chain itself (an unrecognized 4xx) never quarantines: those keep the no-replay memo and the two forced retries and nothing more.

## Managed config

Before every codex start clauth reads `/etc/codex/managed_config.toml`, the file codex lets an administrator use to outrank a session's own flags. A `cli_auth_credentials_store` there other than `"file"` refuses the start, because codex would ignore the session's linked `auth.json`; a `debug.config_lockfile.load_path` refuses it, because codex would replay that lockfile as its whole config and drop the file store; a `sqlite_home` starts the session with a warning, since every profile's state databases then land in that one directory. Both refusals say what to do (`ask whoever manages this machine to remove the key`) and why (`clauth cannot override a managed config`). On macOS codex can also take a managed config from an MDM profile; clauth does not read that one, so a key delivered that way is not caught before the spawn. On Windows codex reads its managed config from inside `CODEX_HOME`, which the session home never holds.

## When something is refused

| Refusal | What to do |
|---------|------------|
| `the operator codex does not keep its login in auth.json (cli_auth_credentials_store = "<mode>" in <home>/config.toml), so there is nothing current to capture there` | set that key to `"file"` in your `~/.codex/config.toml`, run `codex login`, capture again |
| ``no codex login to capture — <path> does not exist; run `codex login` first`` | log your own codex in first |
| `failed to parse <path> — codex writes this file in place, so a login caught mid-write reads half-written; re-run the capture` | run it again |
| ``<path> holds no ChatGPT token chain (an API-key-only setup?) — only a `codex login` chain can be captured`` | this setup has no chain; log in with `codex login` |
| `clauth: <path> already follows codex profile '<name>' — nothing to capture` | not a refusal: that login is already this profile |
| `<path> is already captured as codex profile '<other>' — one chain, one profile.` | the slot is another profile's link; the line names the steps (remove the link, `codex login`, capture into the new name) |
| ``CODEX_HOME points into a clauth codex session home — run the capture from a shell outside `clauth start` `` | run it from a shell that is not inside a clauth codex session |
| `'<name>' has a live codex session, which holds the chain this capture would replace — close it first` | close that session |
| `'<name>' has a live codex session — close it before re-authenticating` | the browser form's spelling of the same |
| ``'<name>': codex chain is broken (<kind> since <time>), run `clauth login <name> --codex --browser` `` | the browser login |
| `'<name>' is a codex profile; <verb> is claude-only` | `disable`, `enable`, `rolling-token` and `static-token` take Claude Code profiles alone |
| `clauth: could not repoint <path> (no symlink support?) — it is now a SEPARATE copy of a single-use rotating chain` | a host without symlinks: run codex only through `clauth start <name>` from then on, or `codex login` again for your own use |

## Files

| Path | Holds |
|------|-------|
| `~/.clauth/codex-profiles.toml` | the codex roster: `active_profile`, `profiles`, `fallback_chain`, `wrap_off`, `weekly_switch_threshold` |
| `~/.clauth/profiles/<name>/auth.json` | the chain, owner-only, written atomically by clauth and in place by codex through the links |
| `~/.clauth/profiles/<name>/config.toml` | `harness = "codex"`, plus `hooks_json` when you set it |
| `~/.clauth/profiles/<name>/auth.lkg.json` | the last-known-good copy of the chain |
| `~/.clauth/profiles/<name>/auth.attempt` | the no-replay memo: a fingerprint of the refresh token last sent, never the token |
| `~/.clauth/profiles/<name>/auth.quarantine.json` | the verdict that killed the chain, its time, and the judged token's fingerprint |
| `~/.clauth/profiles/<name>/usage_cache.json` | the last usage reading and plan |
| `~/.clauth/profiles/<name>/codex-home/` | the durable store: `sessions/`, `archived_sessions/`, `history.jsonl`, the sqlite state stores |
| `~/.clauth/profiles/<name>/codex-home-<sid>/`, `codex-home-isolated-<sid>/` | one live session's `CODEX_HOME`, removed at exit |
| `~/.clauth/profiles/<name>/sessions-<sid>/`, `sessions-isolated-<sid>/` | that session's PID file, flock-held while it runs |
| `~/.codex/auth.json` | after a capture, a symlink onto the profile's `auth.json` |

Everything under `~/.clauth` is owner-only, as on the [Security](Security#where-credentials-live) page. `~/.codex/config.toml` is copied into sessions and never written back.

## What stays Claude Code only

The Tokens tab and `clauth sessions` / `resume` / `info` read Claude Code's transcript stores; a codex session writes none, so its spend is absent from every figure there rather than folded in. `clauth list` lists Claude Code accounts; `clauth status --json` and the daemon's `status.json` carry codex entries with `"harness": "codex"` ([Daemon](Daemon#clauth-status---json)). The Claude Code plugin's `profiles`, `switch_profile` and `delegate` refuse a codex name as a codex account they do not manage ([Claude Code plugin](Claude-Code-Plugin)). `disable`, `enable`, `rolling-token` and `static-token` refuse one as `'<name>' is a codex profile; <verb> is claude-only`. The Setup, Usage and Fallback tabs list no codex rows, and there is no TUI form for creating one: the shell verbs above are the whole surface.

On the Overview, codex accounts sit in a read-only section under the Claude Code rows, and <kbd>c</kbd> cycles which harness the tab shows ([Interface and keys](Interface-And-Keys#tab-dependent)).

## Windows and hosts without symlinks

Where clauth cannot create symlinks (Windows without the symlink privilege, or a home on exFAT, FAT32 or SMB), a session home holds a copy of `auth.json` instead of a link, converged with the profile's store at every session start and exit, with the later `last_refresh` winning. That copy is a second carrier of a single-use chain, so on such a host clauth stands its refresh down for the whole session, refuses a shared and an isolated session of one profile at the same time, and the capture keeps your `~/.codex/auth.json` as a separate copy with a warning: run codex only through `clauth start <name>` from then on.
