# Quickstart

## Capture your first profile

Launch the TUI while logged into Claude Code:

```bash
clauth
```

Pick `+ new from current profile` on the Overview tab and name it, e.g. `work`. clauth snapshots the OAuth token and endpoint settings your running session is using. Log into a second account in Claude Code, run `clauth` again, and capture that one too.

To add an account without touching the session you are in, use `clauth login` instead: it opens a browser, runs Claude Code's own OAuth flow, and writes the minted tokens into a fresh profile.

```bash
clauth login personal                                   # browser login
clauth login deepseek --base-url https://api.deepseek.com --api-key sk-...
```

For a third-party endpoint clauth recognises, `open provider console` in the TUI action menu opens the page that key is minted on ([Configuration](Configuration#where-the-keys-come-from)).

## Switch

In the TUI: move to the account, <kbd>⏎</kbd>, confirm. From the shell:

```bash
clauth work
# switched to 'work'
```

A switch repoints the credentials your global `claude` reads. A session already running adopts the new account on its next token refresh.

## Run two accounts at once

```bash
clauth start personal                  # claude under personal's own config dir
clauth start personal -- --model haiku # flags for claude go after --
```

`clauth start` gives the session its own `CLAUDE_CONFIG_DIR`, so identity, settings, and billing caches never mix between accounts, and the global session is untouched.

For a session that keeps the account's auth while dropping your global `CLAUDE.md`, plugins, and hooks:

```bash
clauth start --isolated personal -p < prompt.txt
```

Pass the prompt on stdin when you use `-p`. A variadic `claude` flag would otherwise swallow a trailing positional prompt forwarded through clauth. Run it in an empty directory to skip project memory too.

## Check what is loaded

```bash
clauth which          # profile that owns the current session's credentials
clauth which --json   # plus plan tier and endpoint
clauth list           # account table with cached usage, no network
```

## Commands

| Command | Flags | Does |
|---------|-------|------|
| `clauth` | | open the TUI (with stdout not a terminal: command help on stderr, exit 2) |
| `clauth <profile>` | | switch to that profile and exit |
| `clauth start <profile> [claude args…]` | `--isolated`, `--with-fallback` | run `claude` under that profile's own config dir |
| `clauth login <profile>` | `--manual`, `--base-url`, `--api-key`, `--setup-token`, `--yes`, `--model` | add an account, or re-authenticate one in place |
| `clauth rolling-token <profile>` | | serve the profile's sessions a rolling token re-stamped from its usage chain |
| `clauth static-token <profile>` | `--clear`, `--yes` | bare: restore the preserved mint a rolling token superseded; `--clear` removes the long-lived token entirely |
| `clauth delete <profile>` | `--yes`, `--force` | remove a profile and every credential it holds |
| `clauth disable <profile>` | `--yes` | hide it from auto-switch, polling, and the status feed; files stay |
| `clauth enable <profile>` | | put a disabled profile back |
| `clauth which` | `--json` | print the profile owning the loaded credentials |
| `clauth list` | `--all` (`--disabled`) | account table from the on-disk caches, never fetches |
| `clauth jobs` | `--json` | what the delegates are doing: account, elapsed, last output, next deadline, live runs first; `--json` also carries each run's `session_id`, the handle `delegate({resume})` takes after a crash, and whether the run was isolated, which is what decides whether that id is a handle at all |
| `clauth sessions` | `--json`, `--tokens` | list Claude Code sessions, newest first |
| `clauth resume <id\|latest>` | `--profile <name>` | resume a session under a chosen account |
| `clauth info <id\|latest>` | | print a session's resume command, workspace, and storage path |
| `clauth daemon` | `--status`, `--standby`, `--replace`, `--no-standby` | run the refresh + auto-switch loop with no TUI |
| `clauth status --json` | `--all`, `--disabled` | print the daemon's status shape once, from disk |
| `clauth mcp` | | stdio MCP server; Claude Code launches this, not you |
| `clauth completions <bash\|zsh\|fish\|install> [shell]` | | print or install a completion script |
| `clauth herdr install` | `--key <spec>`, `--no-config`, `--yes` | install the [herdr](https://herdr.dev) plugin and bind a key to it |
| `clauth herdr uninstall` | `--no-config`, `--yes` | remove that plugin and the config lines it added |
| `clauth herdr config get <key>` | | print one herdr knob: `popup_width`, `pane_tag`, `tag_watch_secs`, `border_label`, `delegate_dot`, `delegate_row_text` |

`--theme <full\|compatible>` is global and forces a color depth for the TUI.

### Rules worth knowing

- **`start` argument order.** clauth's own flags go before the profile name. Anything clauth does not recognize is forwarded to `claude` verbatim, leading hyphens included. Use `--` for a spelling both programs own, like `--help`.
- **`start --with-fallback`** hands the session its own fallback chain. Refused by name when combined with `--isolated`, on macOS, on Windows without symlink privilege, or for a non-OAuth account.
  - Also refused for an account outside the chain, when the chain has no other member, or when no `clauth daemon` is running.
- **`start --isolated` keeps the session.** Its transcripts and session state are lifted into your global store before the throwaway runtime is discarded, so the run stays resumable and its tokens are counted. A hard kill (SIGKILL) skips that teardown; the next stale-runtime sweep lifts the tree into the global store before deleting it, so a killed session is rescued too. The `--rescue`/`--no-rescue` flags and the `auto_rescue` setting that used to decide this are gone; there is nothing to opt into and no way to opt out.
- **`delete` and `disable` want a TTY.** Both prompt `[y/N]`; on a non-TTY stdin they refuse unless you pass `--yes`. `--force` is the only way past `delete`'s live-session guard, and `--yes` alone does not override it.
- **`login <existing>`** re-authenticates in place. The chain slot, env block, and model settings survive; a browser re-login replaces the subscription login after a confirm. On an account that has an endpoint and a key it can still authenticate with, whether or not clauth recognises the provider, a browser re-login replaces the subscription login alone and leaves the endpoint and key where they are: it is the stored OAuth chain you came to renew, and the key is what that account's inference actually runs on. An endpoint with nothing left behind it is cleared as before, so a re-login never leaves a bare endpoint standing in front of a fresh subscription login. An api-key re-login replaces the endpoint set, and so does any capture that brings one of the fields; a headless one (non-interactive stdin) with no `--base-url` reuses the stored endpoint instead of prompting. The stored OAuth chain survives an api-key re-login: it is what usage polling and `rolling-token` roll from.
- **`login <alibaba account>`** opens the Alibaba Model Studio console instead, because that plan's usage figures run on a console session its api key cannot stand in for. It replaces that session and nothing else: endpoint, api key and model settings all stay put. There is no confirm either, since re-running it is the routine repair. The window it captures is measured from your aliyun console sign-in ([Configuration](Configuration#the-alibaba-console-session)). Passing `--base-url` or `--api-key` still takes the ordinary api-key path. Starting one from nothing is two steps for that reason: give the account a Model Studio endpoint first (a Qwen preset on the Setup tab, or `--base-url` here), then run a bare `clauth login <name>`. The console a session comes from is read off the endpoint, so a name that has none yet has no console to open.
- **`login --manual`** is the same subscription login with no browser on this host, for an ssh session or a headless box. clauth prints a link; open it on any device, sign in, and the page shows a code to paste back (read echo-off). It is Claude Code's own "Browser didn't open?" path, so it mints exactly what a browser login mints: usage polling, plan tier, and `rolling-token` all work. The Setup tab has the same thing as a `manual login (no browser)` row under `web login`, with <kbd>c</kbd> to put the link on your local clipboard through the terminal.
  - Refused on an Alibaba account, whose login is its console session. A non-TTY stdin is read as one line, for a driver that takes the link off stdout and feeds the code back to the same process; the code is bound to that process, so it cannot be piped in from an earlier run.
- **`login --setup-token`** captures a `claude setup-token` mint (echo-off, or piped on stdin) as the profile's long-lived login.
  - That token never races clauth's refresher. It engages only for a genuinely long-lived token; a rotating pair pasted here is ignored and called out on the card.
- **`rolling-token <profile>`** points the profile's sidecar at its own clauth-private usage chain instead of a static mint: the daemon re-stamps it with the chain's current access token — full scopes and the account's `subscriptionType`, but **no refresh token** — so sessions hold nothing rotatable (the split's whole point) while running a bearer the API recognizes as the plan it is, and plan-gated models work in a clauth-managed session. A `claude setup-token` mint carries neither `user:profile` nor a subscription stamp and gets capped. Arming widens what a session's credential can reach, and the command says so. It needs the daemon running: the bearer dies in hours, and the daemon's scan is what re-stamps it before then. The mint it supersedes is preserved at `session-token.static.json`; the bare `clauth static-token <profile>` — or a terminally dead usage chain — restores it rather than signing sessions out. The Setup tab's `token` row switches to an hours-scale `rolling · re-stamps in ~Nh` countdown and reads `rolling token stalled` if the re-stamping ever stops.
- **`static-token --clear`** is the way back out. A stored long-lived token is what every switch installs, so a plain `clauth login <profile>` refreshes only the OAuth pair clauth polls usage with, and never reaches a session. The login prints a note saying so. Clearing is the FULL exit: it drops the token, the preserved mint backup, and the `rolling_token` flag together (a lingering flag would have the daemon re-stamp a fresh sidecar over the removal, and a lingering backup keeps a year-scale credential on disk under a command that just said "cleared"), then relinks the live credentials when the profile is active. It is refused when clearing would strip the profile's last credential — a stored token (or preserved mint) with no other login behind it; a profile whose only rolling piece is the flag disarms regardless, since no credential is touched. An **api key counts as that other login**, so an api-key profile clears with no OAuth pair to fall back to: the live credentials are removed rather than relinked, Claude Code is signed out (on macOS, out of the Keychain too), and the profile carries on authenticating by api key. A flag-only profile has no login at all behind it, so the sign-out leaves nothing serving and the line says to log in before switching to it. Every line clauth prints for the clear names which of those three happened.
- **`resume latest`** refuses rather than silently picking the second-newest when a live isolated session holds a newer one. `clauth info` names where any transcript actually lives.
- **`sessions --tokens`** parses every transcript in full to total tokens and cost. On a large store that takes a while, which is why it is opt-in.
- **`herdr install`** runs herdr's own installer and passes its preview and confirm straight through, then adds the two things a herdr plugin cannot declare for itself: the key that opens the clauth dashboard, and the sidebar row that renders which account each Claude Code pane burns. Both land in your herdr `config.toml`, appended after a diff and a `[y/N]`, and herdr validates the result before anything is written. Run it a second time and it adds nothing. `--yes` skips both prompts, herdr's install preview included, and is required on a non-TTY stdin. **`herdr uninstall`** reverses both halves behind one confirm, and declining leaves both alone; it removes only the blocks clauth marked as its own. For either command `--no-config` covers the plugin and leaves `config.toml` untouched. After that, clauth keeps the plugin current on its own: it lands the plugin at the latest release and compares the installed checkout's commit against that release's commit, reinstalling when they differ, up to once per 30 minutes (`CLAUTH_NO_UPDATE=1` opts out). The whole surface, including the per-pane account tag: [herdr plugin](Herdr-Plugin).

### Environment variables

| Variable | Effect |
|----------|--------|
| `CLAUTH_NO_UPDATE=1` | disables the background update check and self-replacement |
| `CLAUTH_NO_COMPLETIONS=1` | skips the first-run completions prompt |
| `CLAUDE_CONFIG_DIR` | scopes `which` and `start` to that config dir's credentials |
| `SHELL` | how `completions install` detects your shell when you do not name one |
| `COLORTERM` | what the TUI auto-detects its color depth from: `truecolor` or `24bit` picks `full`, anything else `compatible`. `--theme` and the `theme` key in `profiles.toml` both beat it |
| `HERDR_CONFIG_PATH` | which config file `herdr install` writes into, matching how herdr itself reads the override |
| `HERDR_BIN_PATH` | which `herdr` binary clauth runs, else `herdr` on `PATH`. herdr injects it into every pane process itself |

### Exit codes

`0` success, `1` failure, `2` usage error (unknown profile, bad flags). `clauth daemon --status` exits `0` when a daemon is running and `1` when none is.
