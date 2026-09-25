# Security

This page covers where your logins sit and how they move between accounts. The trust model, every host clauth contacts, the update verification chain, and vulnerability reporting live in [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md).

## Where credentials live

| Path | Holds |
|------|-------|
| `~/.clauth/profiles/<name>/credentials.json` | that account's OAuth token pair, plus the MCP-server logins described below |
| `~/.clauth/profiles/<name>/mcp-logins.json` | those MCP-server logins alone (or the keys `carried_credential_keys` lists), parked whenever the account stores no Claude login of its own and merged back once it regains one |
| `~/.clauth/profiles/<name>/session-token.json` | a long-lived `claude setup-token` login, when captured, plus the keys `carried_credential_keys` lists |
| `~/.clauth/profiles/<name>/config.toml` | the endpoint API key, for endpoint accounts |
| `~/.clauth/profiles/<name>/auth.json` | a codex profile's ChatGPT token chain, in codex's own `auth.json` shape ([Codex](Codex)) |
| `~/.clauth/profiles/<name>/auth.lkg.json` | the last-known-good copy of that chain, restored only over a store that has read unreadable for 30 seconds with no session live |
| `~/.codex/auth.json` | after `clauth login <name> --codex`, a symlink onto that profile's `auth.json`, so your own codex and clauth hold one chain |

Beside a codex store sit two files that hold no credential: `auth.attempt`, a fingerprint of the refresh token clauth last sent, and `auth.quarantine.json`, the verdict that killed the chain with the judged token's fingerprint. A codex session home links `auth.json` rather than copying it, except on a host without symlinks, where the session's `codex-home/` or `codex-home-isolated/` holds a copy converged with the store at each session start and exit.

The full tree, the quarantine slot and the Keychain row are in [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md)'s data-at-rest table. On Unix, clauth creates each file `0600` and each directory `0700` (owner-only from birth, never a later chmod) and re-tightens the whole tree on every launch (stopping at each codex home's `0700` node, the durable store and the session homes, whose contents are codex's own); Windows keeps the user profile's stock ACLs untouched. Each write is temp-file + fsync + rename, and a rotation caught mid-write parks as `credentials.json.pending`, promoted only once durable.

An endpoint account's API key reaches Claude Code through `apiKeyHelper`, so it never lands in `settings.json`.

## What a switch touches

A switch rewrites exactly the files in SECURITY.md's switch list: the global credentials link, the `env`/`model`/`apiKeyHelper` parts of `~/.claude/settings.json`, and the stale identity block in `~/.claude.json`, plus the target profile's stored MCP logins so you are not signed out of them. Nothing else moves. Hooks, permissions, status line, projects, plugins and token stats are all left where they are. A switch onto a codex profile rewrites `~/.clauth/codex-profiles.toml` and nothing else: no file under `~/.claude` or `~/.codex` moves ([Codex](Codex#switch)).

## MCP-server logins

Claude Code keeps each MCP server's OAuth login in the same file as your Claude login, keyed by the server and its endpoint. Those logins are minted against the server itself, so they belong to no Claude account, and a switch carries them onto the account you switch to. Without that, every switch signed you out of every MCP server.

Two consequences worth knowing:

- The same MCP-server token ends up stored under more than one account. Every copy is `0600` like the rest, and your Claude logins are still never duplicated.
- Signing out of an MCP server in Claude Code propagates on your next switch. A profile you have not switched into since then keeps its old copy until you do.

`carried_credential_keys` in `profiles.toml` ([Configuration](Configuration#profilestoml)) widens this on purpose: every key it lists is copied the same way, into every account's `credentials.json` and `session-token.json`, and into `mcp-logins.json` when a profile stores no login. A listed `designOauth` is a Claude Design login with its own refresh token, so that token then sits under every account too, and Claude Code rotates it on refresh: a copy that stays behind in a store you do not switch into goes stale. On macOS the same list governs what crosses inside the Keychain item, and a sign-out onto an account that stores no login keeps the listed keys in the item. Your Claude logins themselves (`claudeAiOauth`) are never copied. Unsetting the list stops the copying but removes no copy already made. Inside a `clauth start` session running on a setup token the list is not honored yet, and a session there that refreshes a carried login loses the refreshed copy, so leave the list unset on such a host.

macOS carries them too, through the Keychain rather than the file. Claude Code keeps your Claude login and your MCP-server logins as sibling keys inside one Keychain item, and a write replaces that whole item, so clauth reads the item first and carries the MCP logins onto the login it installs. macOS asks you to allow that read the first time it happens: answer **Always Allow** and it should not ask again, since the grant binds to `/usr/bin/security` rather than to clauth (measured against a stand-in item, not against Claude Code's own, so treat a second prompt as possible rather than a bug). You have up to 10 seconds to answer before clauth gives up and carries on without the item's contents, and a little less if the same switch already spent part of its 20-second keychain budget. Decline it, miss it, or switch over ssh where the prompt cannot be shown, and the switch still completes. clauth records what was lost on its event line, which lands in `~/.clauth/clauth.log` when you switched by hand and in the daemon's own log when the daemon switched for you, and you re-authenticate the servers that report a signed-out session. If the item ever reads back as anything other than the JSON object Claude Code expects, its raw bytes are preserved under `~/.clauth/keychain-quarantine/` before the switch replaces or deletes the item, and the event line names the file: the Claude login head usually survives that kind of corruption, so you can slice it back out or re-authenticate the servers that complain. Every Keychain write is read back and verified the same way — one that comes back corrupt fails the switch rather than completing on a broken login, and one that cannot be checked, over ssh or out of time, completes with a note on the event line, since retrying the switch re-runs the write.

## Per-platform behavior

Where Claude Code reads its login from is platform-split, and assuming Linux behavior on a Mac is the trap.

- **Linux.** The plaintext credentials file only, re-read whenever its modification time moves. clauth stamps that time explicitly on every swap, so a live session follows the new account.
- **macOS.** The Keychain first, the file only on a miss, and Claude Code deletes the file once it migrates tokens into the Keychain. The file swap alone is cosmetic there, so clauth mirrors each fresh login into the `Claude Code-credentials` Keychain item as well. That item is namespaced per config dir, so a `clauth start` runtime and a bare `claude` never share one, and a `--with-fallback` session's swaps write its runtime's namespaced item so the session follows its chain. Switching to a profile that stores no Claude login, an api-key or third-party account, signs that item out instead of leaving it, so Claude Code cannot keep spending the account you switched away from.
- **Windows.** No symlinks and no Keychain: the swap is a file copy, read directly.

macOS is why `clauth login` exists at all. Claude Code's own `/login` under a custom config dir writes only a per-config-dir Keychain item, leaving the profile's credentials file empty.

## Session isolation

`clauth start <profile>` builds that session its own `CLAUDE_CONFIG_DIR` under the profile directory, so identity, settings, and billing caches never leak between accounts running at once. The tree is torn down when the session ends. `--isolated` goes further and drops your global memory, plugins, and hooks, keeping only the account's auth.

A codex profile gets the same treatment under `CODEX_HOME`: its own home per session with the environment's codex credential variables scrubbed, the store forced to the linked `auth.json` by a `-c` flag, and a start refused where `/etc/codex/managed_config.toml` would outrank that ([Codex](Codex#run)).

## Token rotation

clauth rotates each account's OAuth pair ahead of expiry, early enough that a running `claude` never reaches its own refresh threshold. Set Config tab `rotation` to `lazy` to refresh only after a request is rejected.

Refresh tokens are single-use. The active account shares one chain with the running `claude`, and whichever side refreshes first revokes the other, so clauth never bets on winning that race: when Claude Code rotates first, clauth adopts its fresher pair from the file mirror rather than spending a revoked token. That adoption is identity-guarded, so a login belonging to a different account is never captured unattended. A double-spend costs the loser one rejected request, never the account.

A refresh that fails terminally quarantines the account as `auth broken`. It is then excluded from every chain walk and refused as a switch target, since installing a dead token would sign out every running session. `clauth login <name>`, or any later successful refresh, clears it.

A codex chain's refresh token is single-use too, and the server answers a replay by killing the chain for good, so clauth never sends the same token twice: the token it last spent is remembered on disk beside the store, a failed refresh is not retried with it, and a session live inside codex's own five-minute pre-expiry window gets the refresh left to codex. A verdict the server calls final quarantines the chain as `broken`, and only a new login clears it: `clauth login <name> --codex --browser` ([Codex](Codex#when-a-chain-dies)).

## Account-change detection

If Claude Code logged into a different account while clauth was closed, the next launch asks before overwriting anything: keep the stored login, capture the live one as a new profile, or discard it. Config tab `on mismatch` picks an answer up front.

## Switching things off

The off-switches are SECURITY.md's table: `CLAUTH_NO_UPDATE=1`, `CLAUTH_NO_COMPLETIONS=1`, `CLAUTH_NO_API=1`, an empty `fallback_chain`, `allow extra usage` off, and `auto_start = false` are all default-safe and named there with their effects.

Found something exploitable? Report it privately through the repo's **Security → Report a vulnerability**.
