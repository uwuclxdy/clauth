# clauth for Rex

The [Rex](https://www.superlogical.com/rex) counterpart of `herdr-plugin/`. clauth's Rex support
is experimental: nothing heals or updates the install on its own (rerun `clauth rex install`
after upgrading clauth), the Services tab's `rex` row only reports it and names
`clauth rex install` when it is missing or stale, and Rex is in beta, so a new Rex build can
break it. It does three things:

- **Launch Claude Code through clauth.** The `clauth_claude` action splits a pane running
  `clauth start --auto --with-fallback`, so a new session starts on the fallback-chain member
  with headroom and moves on as each account runs out. `clauth_claude_tab` opens a tab instead.
- **Open the clauth dashboard** in a floating pane (`clauth_dashboard`). A second press closes it.
- **Label each pane with the account it spends**: `claude · work`, `codex · codex` for a
  codex started through clauth or on a login clauth adopted, or `claude · main (global)` for a
  bare `claude` running on the global login.

All three actions are in the command palette under "clauth".

## Requires

- clauth on the PATH a login shell sets (`clauth start` runs through it), or in
  `~/.local/bin`, `/opt/homebrew/bin` or `/usr/local/bin` for the watcher, which Rex runs with a
  bare system PATH.
- `--with-fallback` needs a running `clauth daemon` to decide the switches.
- Rex with Lua config (`rex config check`).

## Install

```sh
clauth rex install
```

That writes these files, as bundled in the clauth binary, to `~/.config/rex/clauth/` (Rex's own
rules: `REX_CONFIG`, then `$XDG_CONFIG_HOME/rex/`), appends a marked `require("clauth").setup{}`
block to `init.lua`, stops the running watcher, and reloads Rex, which starts a fresh one.
`clauth rex uninstall` takes the files and the block back out, stops the watcher and removes
`~/.local/state/clauth-rex/`. See the
[wiki page](https://github.com/uwuclxdy/clauth/wiki/Rex-Plugin) for the details.

To develop against a checkout instead, link it in place of the installed copy (install then
refuses to overwrite the link):

```sh
ln -s /path/to/clauth/rex-plugin ~/.config/rex/clauth
printf 'require("clauth").setup{}\n' >> ~/.config/rex/init.lua
rex config reload
```

`setup{}` takes `select`, `claude_args`, `env`, `keys` and `tag`; the header of `init.lua`
documents each. Default keys: `alt+cmd+c` splits right, `shift+alt+cmd+c` splits down,
`shift+cmd+c` toggles the dashboard.

## What it runs as you

- On every config load, `init.lua` starts `start-watch.sh`, which runs one `rex do --all
  tag-watch.lua` per Rex server and exits. Several servers (one per installed Rex version) each
  get their own watcher, pinned to the server's `--listen` socket.
- The watcher runs `resolve-profile.sh` when a pane's foreground program is `claude`, `clauth`
  or `codex`, and every 10 s for each pane it tags. The resolver is one
  `clauth which --pid <pid> --json` call; a clauth older than `--pid` leaves labels as they are.
  It never writes clauth state.
- The only writes are pane labels and the files under `~/.local/state/clauth-rex/`.

## How the labels work

Rex runs event handlers only in `rex do` scripts, not in `init.lua`, hence the separate watcher.
It hears `terminal.process_changed`, resolves the foreground pid through `clauth which --pid`,
as `herdr-plugin/report-profile.sh` does (the `clauth start` supervisor is the terminal's
foreground process and keys its live-session row), and appends the account to the pane's label.
The original label comes back when the agent exits, unless someone relabeled the pane meanwhile.
Only a suffix naming a known clauth profile is ever taken for a tag, so a label such as
`deploy · prod` is left alone.

A timer re-resolves tagged panes, since a `--with-fallback` swap and a `clauth switch` fire no
Rex event.

## Settings

| Env var | Default | |
| --- | --- | --- |
| `CLAUTH_REX_WATCH_SECS` | `10` | Re-resolve interval, clamped to 1 to 3600 s. |
| `CLAUTH_REX_BIN` | the `rex` beside the server | The Rex CLI the watcher runs. |

Both must be in the Rex server's environment.

## Files

| File | Role |
| --- | --- |
| `init.lua` | The module `require("clauth")` loads: actions, keys, watcher start. |
| `start-watch.sh` | Starts one watcher per server. |
| `tag-watch.lua` | The `rex do` script that labels panes. |
| `resolve-profile.sh` | Pid to account, shared logic with the herdr plugin. |

State: `~/.local/state/clauth-rex/watch.log` and `watch-<server>.pid`. `clauth rex install` and
`uninstall` read the pidfiles to stop the watcher, so keep their names and contents as they are.

## Limits

- Rex has no custom sidebar field, so the account goes in the pane label.
- `resolve-profile.sh` repeats the live-session join in `herdr-plugin/report-profile.sh`;
  change the two together.
