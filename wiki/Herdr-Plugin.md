# herdr plugin

clauth ships a plugin for [herdr](https://herdr.dev) that does three things: it opens the clauth dashboard in a popup over whatever you were doing, it labels every herdr pane with the account that pane is spending, and it shows when a delegate runs inside a pane.

Requires herdr 0.8.0 or newer, `clauth` on `PATH`, and Linux or macOS. The entrypoints are POSIX shell scripts, so the manifest declares those two platforms; herdr on Windows is preview-only anyway.

## Install

```sh
clauth herdr install
```

One command for the whole setup. It runs herdr's own installer, passing herdr's preview of every command the plugin would run as you straight through, then adds the two things a herdr plugin cannot declare for itself: the key that opens the dashboard, and the sidebar row that renders the pane tag. Both land in your herdr `config.toml`, appended after a diff and a `[y/N]`, and herdr validates the result before anything is written.

Run it a second time and it adds nothing. `--key` picks the keybinding, and a re-run with a new key re-binds an existing clauth binding to it; the Plugin tab's heal keeps the installed key instead. Run it from a clauth checkout and it links the local `herdr-plugin/` directory instead of fetching the published one, so an edit is live on the next open.

| Flag | Effect |
|------|--------|
| `--key <spec>` | the key that opens the dashboard, in herdr's own binding syntax; default `prefix+a` |
| `--no-config` | install the plugin, leave `config.toml` alone, and print the blocks to paste |
| `--yes` | skip both prompts, herdr's install preview included; required on a non-TTY stdin |

`HERDR_CONFIG_PATH` overrides which config file gets written, matching how herdr itself reads that variable.

To install by hand instead, run `herdr plugin install uwuclxdy/clauth/herdr-plugin` and paste the two blocks from the [plugin README](https://github.com/uwuclxdy/clauth/tree/mommy/herdr-plugin#readme).

## Updates

clauth keeps the plugin current on its own. herdr has no `plugin update`; an install over an existing GitHub source is the update, so the daemon and every `clauth mcp` startup check the installed version against the running clauth and reinstall when it trails, at most once per 30 minutes per process. A stale install converges after a session or two with nothing to do.

The check never resurrects an uninstall, never re-enables a plugin you disabled in herdr, and never touches a checkout linked from a clauth source tree. `CLAUTH_NO_UPDATE=1` opts out, same as clauth's own self-update. The plugin is Linux and macOS only, so Windows has nothing to update.

## Uninstall

```sh
clauth herdr uninstall
```

Removes the plugin from herdr and drops the config blocks clauth added, as one operation behind one confirm. Declining leaves both halves exactly as they were. `--no-config` removes only the plugin; `--yes` skips the prompt.

It only removes blocks clauth marked as its own, so anything you wrote elsewhere stays. If the plugin is already gone it says so and still cleans the config, which is the state a half-finished install leaves behind.

## Actions

| Action | Qualified id | What it does |
|--------|--------------|--------------|
| Open clauth | `clauth.open` | the clauth dashboard in a popup; quit it with <kbd>q</kbd>, same as anywhere else |
| Show this pane's clauth account | `clauth.which` | re-reads the account the focused pane burns and republishes it as pane metadata |

There is no account picker. Switching is a keystroke inside the dashboard, so a second switch surface would only know less than the first.

herdr allows one popup per session, so pressing the open key with clauth already up does nothing rather than reporting an error.

## The key

A herdr plugin cannot declare a keybinding, so that line lives in your own herdr `config.toml` and nothing happens until it is there. `clauth herdr install` writes it:

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "clauth.open"
description = "clauth accounts"
```

`command` takes the qualified action id from the table above. `prefix+` is herdr's own leader. Without a binding the actions are still reachable through `herdr plugin action invoke clauth.open`. herdr 0.8.2 has no menu that lists plugin actions; that is an upstream ask clauth has drafted, not something this plugin can add.

## The pane tag

Every herdr pane running Claude Code spends some account, and which one is invisible from the pane itself. The plugin hooks agent detection, publishes the answer as pane metadata under the name `clauth`, and starts a per-pane watcher that re-publishes it every few seconds until the pane closes.

The watcher is what keeps the tag right across an account swap, which fires no herdr event. A `clauth start --with-fallback` session that moves onto the next chain member, or a bare `claude` that follows a `clauth switch`, both repoint the account invisibly. herdr detects other agents too, and a pane running one of those is left untagged rather than labelled with an account it never touches.

herdr renders a reported value only where your own agent-row template asks for it, so **the tag stays invisible until `$clauth` is in a row**. `clauth herdr install` adds this one; Claude Code panes take the `rows_by_agent` template rather than the generic `rows`:

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$clauth"]]
```

That reads `claude · D1` in the sidebar for a pane started as `clauth start D1`. A pane running Claude Code some other way reports whichever account owns the global credentials. Point `CLAUDE_CONFIG_DIR` somewhere else yourself and the tag stops matching what that pane spends.

## Delegate state

When a `claude` delegate runs in a pane, the pane's `clauth mcp` server publishes a second token: `clauth_delegate` reads `working` for the run's duration and `idle` once the last in-flight delegate ends. The token self-clears a minute after its last report, so a dead server never leaves a stale `working` behind.

The agent-panel status dot does not follow it. The dot is herdr's own lifecycle signal; a metadata token renders only as text where a row template names it. The row above names `$clauth` alone, so delegate state reads on `herdr pane get` / `herdr pane list`. Turn the `delegate row text` knob on and `clauth herdr install` appends `$clauth_delegate` to the row.

## Herdr mode

A clauth TUI opened inside a herdr pane (`HERDR_ENV=1`) adds one thing: the header carries a dim `[ herdr ]` tag and the TUI opens on the Plugin tab with the `herdr` row selected. Everything else is the same TUI.

## Herdr options

Six knobs tune the plugin. They live in the dashboard's Plugin tab: select `herdr`, press <kbd>⏎</kbd>, and an `options` section at the bottom of the detail lists them as form rows. They persist in `~/.clauth/profiles.toml` under `[herdr]`, never in herdr's own `config.toml`. The plugin scripts read them through `clauth herdr config get <key>`, which prints one line (`fit`, `on`, `off`, or a count). Knob changes apply immediately: moving `pane tag` or `border label` re-reports every pane at once (from the plugin-pane launch only; a standalone TUI has no panes to reach, and a bare pane lacks the plugin root).

| Knob | Default | What it does |
|------|---------|--------------|
| `popup width` | `fit` | the placement `clauth.open` uses: `fit` opens a popup sized against the focused pane (full width up to 540 columns, then a centered 540), `half` is herdr's default half-size popup, `split-right` opens a real pane right of the focused one, `split-top` opens a real pane directly above it (a downward split of the pane above; no pane above splits the focused pane instead). `full` folded into `fit`, so a saved `full` loads as `fit`. If the snapshot herdr serves cannot be read, the popup opens without sizing flags |
| `pane tag` | on | publish the `clauth=$profile` token; off clears the token on every pane |
| `tag refresh` | 5 | seconds between the per-pane watcher's re-publishes |
| `border label` | off | publish `--display-agent "$profile"` so split-pane borders name the account; off clears the stale label |
| `delegate dot` | on | the `clauth mcp` server reports `clauth_delegate=working\|idle` during delegate runs; off disables the reporting entirely |
| `delegate row text` | off | the sidebar row `install` writes gains the `$clauth_delegate` token, so a running delegate reads as text beside the row; toggling it in the TUI rewrites only the blocks clauth itself wrote (a block you edited by hand is kept whole), behind a confirm that defaults to cancel |

The options render whether the TUI runs inside herdr or standalone. herdr mode differs only in the header tag and the landing tab.

## Checking it from the TUI

The dashboard's [Plugin tab](Interface-And-Keys#plugin-tab) carries a `herdr` row, shown only if herdr is installed. It reports the herdr version, whether the plugin is linked or installed and whether it is enabled, the key you bound and its spelling, and whether the sidebar row is templated. A registry entry whose checkout has been moved or deleted reads as danger, since herdr keeps the entry and the plugin cannot run.

<kbd>f</kbd> on that row appends whichever of the keybinding and the sidebar row is missing, behind a confirm that defaults to cancel. It is the same write `clauth herdr install` performs, so it is the repair for a config edited by hand since. If your config spells one of those tables in a way clauth cannot extend by appending, it says so and leaves that half to you rather than guessing.

There is deliberately no "newer version available" check. herdr 0.8.0 ships no `plugin update`, and re-running `clauth herdr install` already is the refresh path.

## What this plugin cannot do, by design of herdr's plugin v1

Plugin UI is pane-scoped. herdr documents runtime action registration and native non-terminal plugin UI as outside plugin v1, so none of this is a missing feature here:

- no button or row beside the sidebar spaces list, and no status-bar item
- no menu outside a pane, and no menu that lists plugin actions
- no mouse binding of any kind: herdr's key parser rejects mouse tokens, and the only click routed to a plugin is a Control-click on a URL matching a `link_handlers` pattern
- no click-outside dismiss; a popup holds every keystroke, <kbd>Esc</kbd> included, until its command exits

Two more are drafted as upstream asks and not clauth-side work: popups have no position control on herdr 0.8.2, and the agents panel title has no config key to hide it.
