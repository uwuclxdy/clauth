# Security

This page covers where your logins sit and how they move between accounts. The trust model, every host clauth contacts, the update verification chain, and vulnerability reporting live in [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md).

## Where credentials live

| Path | Holds |
|------|-------|
| `~/.clauth/profiles/<name>/credentials.json` | that account's OAuth token pair, plus the MCP-server logins described below |
| `~/.clauth/profiles/<name>/mcp-logins.json` | those MCP-server logins alone, parked whenever the account stores no Claude login of its own and merged back once it regains one |
| `~/.clauth/profiles/<name>/session-token.json` | a long-lived `claude setup-token` login, when captured |
| `~/.clauth/profiles/<name>/config.toml` | the endpoint API key, for endpoint accounts |

Every file clauth writes under `~/.clauth` is `0600` and every directory `0700` on Unix, created that way rather than chmod'd afterwards, and re-tightened on each launch. Windows falls back to the default user-profile ACLs, which clauth does not loosen. Writes are atomic: temp file, fsync, rename. A rotation caught mid-write lands as `credentials.json.pending` and is promoted only once it is durable.

An endpoint account's API key reaches Claude Code through `apiKeyHelper`, so it never lands in `settings.json`.

## What a switch touches

| File | Change |
|------|--------|
| `~/.claude/.credentials.json` | repointed at the target profile's stored login |
| `~/.claude/settings.json` | the `env` block, the top-level `model` key, `apiKeyHelper` |
| `~/.claude.json` | the stale account-identity block is dropped, so Claude Code re-derives identity from the new token |
| `~/.clauth/profiles/<target>/credentials.json` | gains the live file's MCP-server logins, so a switch stops signing you out of them |

Nothing else moves. Hooks, permissions, status line, projects, plugins and token stats are all left where they are.

## MCP-server logins

Claude Code keeps each MCP server's OAuth login in the same file as your Claude login, keyed by the server and its endpoint. Those logins are minted against the server itself, so they belong to no Claude account, and a switch carries them onto the account you switch to. Without that, every switch signed you out of every MCP server.

Two consequences worth knowing:

- The same MCP-server token ends up stored under more than one account. Every copy is `0600` like the rest, and your Claude logins are still never duplicated.
- Signing out of an MCP server in Claude Code propagates on your next switch. A profile you have not switched into since then keeps its old copy until you do.

macOS carries them too, through the Keychain rather than the file. Claude Code keeps your Claude login and your MCP-server logins as sibling keys inside one Keychain item, and a write replaces that whole item, so clauth reads the item first and carries the MCP logins onto the login it installs. macOS asks you to allow that read the first time it happens: answer **Always Allow** and it should not ask again, since the grant binds to `/usr/bin/security` rather than to clauth (measured against a stand-in item, not against Claude Code's own, so treat a second prompt as possible rather than a bug). You have up to 10 seconds to answer before clauth gives up and carries on without the item's contents, and a little less if the same switch already spent part of its 20-second keychain budget. Decline it, miss it, or switch over ssh where the prompt cannot be shown, and the switch still completes. clauth records what was lost on its event line, which lands in `~/.clauth/clauth.log` when you switched by hand and in the daemon's own log when the daemon switched for you, and you re-authenticate the servers that report a signed-out session.

## Per-platform behavior

Where Claude Code reads its login from is platform-split, and assuming Linux behavior on a Mac is the trap.

- **Linux.** The plaintext credentials file only, re-read whenever its modification time moves. clauth stamps that time explicitly on every swap, so a live session follows the new account.
- **macOS.** The Keychain first, the file only on a miss, and Claude Code deletes the file once it migrates tokens into the Keychain. The file swap alone is cosmetic there, so clauth mirrors each fresh login into the `Claude Code-credentials` Keychain item as well. That item is namespaced per config dir, so a `clauth start` runtime and a bare `claude` never share one. Switching to a profile that stores no Claude login, an api-key or third-party account, signs that item out instead of leaving it, so Claude Code cannot keep spending the account you switched away from.
- **Windows.** No symlinks and no Keychain: the swap is a file copy, read directly.

macOS is why `clauth login` exists at all. Claude Code's own `/login` under a custom config dir writes only a per-config-dir Keychain item, leaving the profile's credentials file empty.

## Session isolation

`clauth start <profile>` builds that session its own `CLAUDE_CONFIG_DIR` under the profile directory, so identity, settings, and billing caches never leak between accounts running at once. The tree is torn down when the session ends. `--isolated` goes further and drops your global memory, plugins, and hooks, keeping only the account's auth.

## Token rotation

clauth rotates each account's OAuth pair ahead of expiry, early enough that a running `claude` never reaches its own refresh threshold. Set Config tab `rotation` to `lazy` to refresh only after a request is rejected.

Refresh tokens are single-use. The active account shares one chain with the running `claude`, and whichever side refreshes first revokes the other, so clauth never bets on winning that race: when Claude Code rotates first, clauth adopts its fresher pair from the file mirror rather than spending a revoked token. That adoption is identity-guarded, so a login belonging to a different account is never captured unattended. A double-spend costs the loser one rejected request, never the account.

A refresh that fails terminally quarantines the account as `auth broken`. It is then excluded from every chain walk and refused as a switch target, since installing a dead token would sign out every running session. `clauth login <name>`, or any later successful refresh, clears it.

## Account-change detection

If Claude Code logged into a different account while clauth was closed, the next launch asks before overwriting anything: keep the stored login, capture the live one as a new profile, or discard it. Config tab `on mismatch` picks an answer up front.

## Switching things off

| Switch | Effect |
|--------|--------|
| `CLAUTH_NO_UPDATE=1` | no background update check, no self-replacement |
| `CLAUTH_NO_COMPLETIONS=1` | no first-run completions prompt |
| `auto_start = false` (the default) | clauth sends no inference of its own |
| an empty `fallback_chain` (the default) | clauth never switches accounts on its own |
| `allow extra usage` off (the default) | clauth never spends pay-as-you-go money |

Found something exploitable? Report it privately through the repo's **Security → Report a vulnerability**.
