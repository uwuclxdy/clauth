# Rex plugin

clauth ships a plugin for [Rex](https://www.superlogical.com/rex), a terminal multiplexer for coding agents that is in beta. It does what the [herdr plugin](Herdr-Plugin) does, through Rex's own extension points: it opens Claude Code through `clauth start`, opens the clauth dashboard in a floating pane, and labels every Rex pane with the account that pane is spending.

Requires Rex with Lua config (`rex config check` answers), `clauth` on `PATH`, and Linux or macOS. The watcher and resolver are POSIX shell scripts. Rex is in beta, so its Lua API can still change under the plugin; everything here was checked against Rex builds from October 2026.

## Install

```sh
clauth rex install
```

Rex has no plugin manager: a plugin is a Lua module beside Rex's `init.lua`, loaded with `require`. So install writes the four plugin files bundled in the clauth binary to `clauth/` beside Rex's `init.lua`, then appends one marked block that loads it:

```lua
-- clauth rex plugin: written by `clauth rex install`, removed by `clauth rex uninstall`
require("clauth").setup{}
-- end clauth rex plugin
```

It shows what it will write and asks `[y/N]` first; `--yes` skips the prompt and is required on a non-TTY stdin. Then it runs `rex config reload` when the Rex CLI is on `PATH` (inside a Rex terminal it always is; on macOS outside one it usually is not, since the CLI ships inside the app bundle, and install says to reload by hand).

Rex's own rules pick the files: `REX_CONFIG` names `init.lua` when set, else `$XDG_CONFIG_HOME/rex/init.lua`, else `~/.config/rex/init.lua`.

Install refuses to overwrite a `clauth/` directory it did not write, and a symlink there (the development setup: a link onto a checkout of `rex-plugin/`). If your `init.lua` already has its own `require("clauth")`, for example to pass options, install leaves the file alone.

## Updates

The plugin files come from the clauth binary, so rerun `clauth rex install` after upgrading clauth to refresh them. It overwrites only the directory it wrote and adds no second block.

## Uninstall

```sh
clauth rex uninstall
```

Takes clauth's block out of `init.lua`, leaving the rest of the file as it was, and removes the `clauth/` directory when clauth wrote it. `--yes` skips the prompt.

## Actions and keys

| Action | Default key | What it does |
|--------|-------------|--------------|
| `clauth_claude` | <kbd>alt+cmd+c</kbd> (right), <kbd>shift+alt+cmd+c</kbd> (down) | split a pane running Claude Code through `clauth start` |
| `clauth_claude_tab` | | the same in a new tab |
| `clauth_dashboard` | <kbd>shift+cmd+c</kbd> | the clauth dashboard in a floating pane; a second press closes it |

All three are in Rex's command palette under "clauth", and run from a shell too: `rex do clauth_claude direction=down`, `rex do clauth_claude_tab profile=work`.

A new Claude Code pane starts with `clauth start --auto --with-fallback`: the fallback-chain member with headroom for the session's models, moving to the next member as each account runs out (that needs a running `clauth daemon`, see [Auto-switch](Auto-Switch)). A `profile=` argument names the account instead.

## Options

Replace the managed block with your own `require` to pass options; install then leaves the file alone.

```lua
require("clauth").setup{
  -- clauth start flags when no profile is named
  select = { "--auto", "--with-fallback" },
  -- handed to claude after `--`
  claude_args = { "--dangerously-skip-permissions" },
  env = { CLAUDE_CODE_NO_FLICKER = "1" },
  -- false leaves a key unbound
  keys = { claude = "alt+cmd+c", claude_down = "shift+alt+cmd+c", dashboard = "shift+cmd+c" },
  -- run the pane-label watcher
  tag = true,
}
```

## Pane labels

Each pane running Claude Code gets the account appended to its label: `claude · work`, `codex · codex` for a codex whose login clauth adopted, or `claude · main (global)` for a bare `claude` running on the global login. The pane's own label comes back when the agent exits, unless someone relabeled the pane meanwhile. Only a suffix naming a known clauth profile is ever taken for a tag, so a label such as `deploy · prod` is left alone. Rex has no custom sidebar field, so the label is where the account goes.

Rex runs event handlers only in `rex do` scripts, not in `init.lua`, so on every config load `init.lua` starts `start-watch.sh`, which runs one `rex do --all tag-watch.lua` per Rex server, pinned to that server's `--listen` socket (several Rex versions can run at once). The watcher hears `terminal.process_changed`, and for a foreground `claude`, `clauth` or `codex` runs `resolve-profile.sh`, which joins the pid against `~/.clauth/live_sessions/` the same way the herdr plugin's `report-profile.sh` does. A timer re-resolves tagged panes every 10 s, because a `--with-fallback` swap and a `clauth switch` fire no Rex event.

| Env var | Default | |
|---------|---------|---|
| `CLAUTH_REX_WATCH_SECS` | `10` | re-resolve interval, clamped to 1 to 3600 s |
| `CLAUTH_REX_BIN` | the `rex` beside the server binary | the Rex CLI the watcher runs |

Both are read from the Rex server's environment. The watcher logs to `~/.local/state/clauth-rex/watch.log`.

## What it runs as you

- `start-watch.sh`, on every Rex config load; it exits at once when its server already has a watcher.
- `rex do --all tag-watch.lua`, one per Rex server, for as long as that server runs.
- `resolve-profile.sh`, when a pane's foreground program is `claude`, `clauth` or `codex`, and on the timer for each tagged pane. It reads `ps` (or `/proc` on Linux), `~/.clauth/live_sessions/`, `readlink $CODEX_HOME/auth.json`, and runs `clauth which` for a bare `claude`. It never writes clauth state.

The only writes are pane labels and the files under `~/.local/state/clauth-rex/`.
