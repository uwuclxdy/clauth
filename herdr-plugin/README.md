# clauth herdr plugin

Opens [clauth](https://github.com/uwuclxdy/clauth) in a [herdr](https://herdr.dev) popup: the account table, the usage windows, and the auto-switch chain, over whatever you were doing, without a pane of its own. It labels every herdr pane with the account that pane is spending, and it shows when a delegate runs inside a pane. The popup width, the pane tag, and the delegate state all tune from the dashboard's Plugin tab.

**The manual for all of it lives in the wiki: [herdr plugin](https://github.com/uwuclxdy/clauth/wiki/Herdr-Plugin).** This file covers what the plugin itself is, for anyone reading it before letting herdr run it.

## Requires

- herdr 0.8.0 or newer. The manifest declares it, so an older herdr refuses to link with `plugin_requires_newer_herdr`.
- `clauth` on `PATH`.
- Linux or macOS. The entrypoints are POSIX shell, and herdr's own Windows support is preview-only.

## Install

```sh
clauth herdr install
```

That runs herdr's installer, then writes the two things a herdr plugin cannot declare for itself: the key that opens the dashboard, and the sidebar row that renders the pane tag. `clauth herdr uninstall` reverses both. Flags, the by-hand route, and everything the plugin does once installed are in the wiki page linked above.

## What it runs as you

Two actions and two event hooks, all of them one of the two shell scripts below, plus a per-pane background watcher:

- `clauth.open` opens the dashboard in a popup.
- `clauth.which` re-reads the account the focused pane burns and publishes it as pane metadata, and the same script runs on herdr's `pane.agent_detected` and `pane.agent_status_changed` events. Reporting a Claude Code pane starts the watcher for that pane.

The watcher re-publishes the account every few seconds until the pane closes. An account swap fires no herdr event, so the timer is what keeps the tag from going stale. The scripts write only herdr's own pane metadata plus one pidfile per watched pane in the plugin state directory.

The six knobs live in `~/.clauth/profiles.toml` under `[herdr]` and edit from the dashboard's Plugin tab (herdr row, options). The scripts read them through `clauth herdr config get <key>` and fall back to the shipped defaults when the binary predates the subcommand. The delegate state token (`clauth_delegate`) reports on the pane JSON and, with the `delegate_row_text` knob on, beside the row.

## Paste these if you installed by hand

`clauth herdr install` writes both. herdr does not let a plugin declare either one, so without them the key does nothing and the tag stays invisible.

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "clauth.open"
description = "clauth accounts"
```

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$clauth"]]
```

## Files

| File | Role |
|------|------|
| `herdr-plugin.toml` | Manifest: one popup entrypoint, two actions, two event hooks |
| `open-pane.sh` | Opens an entrypoint in the placement the `popup_width` knob picks; "popup already open" is a no-op for the popup placements only |
| `report-profile.sh` | Resolves the account a pane burns and publishes it as pane metadata |
| `watch-profile.sh` | Per-pane watcher re-publishing the account on a timer |
