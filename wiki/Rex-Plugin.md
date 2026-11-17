# Rex plugin

clauth ships a plugin for [Rex](https://www.superlogical.com/rex), a terminal multiplexer for coding agents that is in beta. It does what the [herdr plugin](Herdr-Plugin) does, through Rex's own extension points: it opens Claude Code through `clauth start`, opens the clauth dashboard in a floating pane, and labels every Rex pane with the account that pane is spending.

**clauth's Rex support is experimental.** Nothing heals or updates the install on its own, the way clauth heals its Claude Code and herdr plugins: rerun `clauth rex install` after upgrading clauth. The Services tab's `rex` row only reports it (installed, written by another clauth version, watcher running) and offers no fix; a missing or stale install names `clauth rex install` there. And a new Rex build can break it, since Rex's Lua API can still change under the plugin; everything here was checked against Rex builds from October 2026.

Requires Rex with Lua config (`rex config check` answers), `clauth` on `PATH`, and Linux or macOS. The watcher and resolver are POSIX shell scripts.

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

It shows what it will write and asks `[y/N]` first; `--yes` skips the prompt and is required on a non-TTY stdin or stdout. Then it runs `rex config reload` when the Rex CLI is on `PATH` (inside a Rex terminal it always is; on macOS outside one it usually is not, since the CLI ships inside the app bundle, and install says to reload by hand). Inside a Rex terminal it runs the CLI from `REX_BIN_DIR`, which Rex sets to its own server's binaries, so the reload reaches that terminal's server.

Rex's own rules pick the files: `REX_CONFIG` names `init.lua` when set, else `$XDG_CONFIG_HOME/rex/init.lua`, else `~/.config/rex/init.lua`.

Install and uninstall read `init.lua` the way Rex's Lua (gopher-lua) lexes it, so a marker or a `require` inside a comment or a string is neither, and a line comment ends at a lone `\r` as at a `\n`. A marker counts only as a comment that starts its line, and each begin line has to reach its end line before the next begin. A file whose markers do not pair up that way, or that Rex's Lua could not load either (a string or a long comment that never closes), is refused before either command changes anything, naming the line.

Install refuses to overwrite a `clauth/` directory it did not write, and a symlink there (the development setup: a link onto a checkout of `rex-plugin/`). If your `init.lua` already has its own `require("clauth")`, for example to pass options, install leaves the file alone; `require "clauth"` and a call split over lines count too, but a `pcall(require, "clauth")` or a `require(m)` through a variable does not: install then appends its block, whose `setup{}` runs again with no options. An `init.lua` that is itself a symlink, such as one into a dotfiles checkout, is written in place, so the link and the file's mode stay.

## Updates

The plugin files come from the clauth binary, so rerun `clauth rex install` after upgrading clauth to refresh them. It overwrites only the directory it wrote and adds no second block. It also stops each running pane-label watcher, since a live one keeps the old `tag-watch.lua` going until its Rex server restarts; the config reload that follows starts a fresh one. A second Rex server, which that reload does not reach, gets its watcher back at its own next config load. With no Rex CLI on `PATH` there is no reload either, so pane labels stop updating until you run `rex config reload` in a Rex terminal. A watcher that does not stop within 2 s is named with its pid, and keeps running on the old files until you stop it.

## Uninstall

```sh
clauth rex uninstall
```

Takes clauth's block out of `init.lua`, leaving the rest of the file as it was, and removes the `clauth/` directory when clauth wrote it. It stops the pane-label watcher and removes its state directory, `~/.local/state/clauth-rex/` (`$XDG_STATE_HOME/clauth-rex/` when that is set). A `clauth/` that clauth did not write (a link onto a checkout, or a directory without clauth's `.clauth-managed` marker) keeps its watcher and state. `--yes` skips the prompt, and is required on a non-TTY stdin or stdout.

Uninstall refuses, before it changes anything, while `init.lua` may still load the module once `clauth/` is gone: any string naming `clauth` outside a comment and outside clauth's block counts, since every spelling of a load names the module somewhere (a `local m = "clauth"` handed to `require(m)`, a `dofile` of a path into `clauth/`). A string naming it for another reason costs a refusal too. It also refuses a block holding a line clauth did not write, which would otherwise go with the block. It prints the line to move or take out; Rex's next config load would otherwise fail on a module that is no longer there.

## Actions and keys

| Action | Default key | What it does |
|--------|-------------|--------------|
| `clauth_claude` | <kbd>alt+cmd+c</kbd> (right), <kbd>shift+alt+cmd+c</kbd> (down) | split a pane running Claude Code through `clauth start` |
| `clauth_claude_tab` | | the same in a new tab |
| `clauth_dashboard` | <kbd>shift+cmd+c</kbd> | the clauth dashboard in a floating pane; a second press closes it |

All three are in Rex's command palette under "clauth", and run from a shell too: `rex do clauth_claude direction=down`, `rex do clauth_claude_tab profile=work`.

A new Claude Code pane starts with `clauth start --auto --with-fallback`: the first fallback-chain member with headroom for the session's models, moving to the next member as each account runs out (that needs a running `clauth daemon`, see [Auto-switch](Auto-Switch)). A `profile=` argument names the account instead.

## Options

Replace the managed block with your own `require` to pass options; install then leaves the file alone. Write it outside the block's markers: uninstall refuses a block holding a line clauth did not write, the block's own `require` edited included. Take your `require` out before `clauth rex uninstall`, which refuses while it is there.

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

Each pane running Claude Code gets the account appended to its label: `claude · work`, `codex · codex` for a codex started through clauth or on a login clauth adopted, or `claude · main (global)` for a bare `claude` running on the global login. The pane's own label comes back when the agent exits, unless someone relabeled the pane meanwhile. Only a suffix naming the account the pane spends is ever taken for a tag, so a label such as `deploy · prod` keeps its `· prod` even when `prod` is a clauth profile. Rex has no custom sidebar field, so the label is where the account goes.

Rex runs event handlers only in `rex do` scripts, not in `init.lua`, so on every config load `init.lua` starts `start-watch.sh`, which runs one `rex do --all tag-watch.lua` per Rex server, pinned to that server's `--listen` address (several Rex versions can run at once). The watcher hears `terminal.process_changed`, and for a foreground `claude`, `clauth` or `codex`, or a `node`, `bun` or `deno` that may run an npm-installed one, runs `resolve-profile.sh`, which asks `clauth which --pid` for the account, the same answer the herdr plugin's `report-profile.sh` reads. A timer re-resolves tagged panes every 10 s, because a `--with-fallback` swap and a `clauth switch` fire no Rex event. A pane that resolves to no account, such as a codex on its own login or a node dev server, is asked again with a doubling gap, up to 32 intervals (about 5 minutes at the default 10 s).

| Env var | Default | |
|---------|---------|---|
| `CLAUTH_REX_WATCH_SECS` | `10` | re-resolve interval, clamped to 1 to 3600 s |
| `CLAUTH_REX_BIN` | the `rex` beside the server binary | the Rex CLI the watcher runs |

Both are read from the Rex server's environment. The watcher logs to `watch.log` in its state directory, `~/.local/state/clauth-rex/` (`$XDG_STATE_HOME/clauth-rex/` when that is set).

## What it runs as you

- `start-watch.sh`, on every Rex config load; it exits at once when its server already has a watcher.
- `rex do --all tag-watch.lua`, one per Rex server, for as long as that server runs.
- `resolve-profile.sh`, when a pane's foreground program is `claude`, `clauth`, `codex`, `node`, `bun` or `deno`, and on the timer for each tagged or pending pane. It runs `clauth which --pid <pid> --json`, which finds the live-session row up that process's parent chain, else a bare codex's adopted login, else the global login a bare `claude` reads. A clauth that cannot answer (one older than `which --pid`) leaves the pane's label as it is. It never writes clauth state.

The only writes are pane labels and the files in the watcher's state directory.
