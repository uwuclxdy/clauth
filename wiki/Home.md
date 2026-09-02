# clauth wiki

**Multi-account manager for Claude Code: switch accounts, watch 5h / 7d usage, auto-switch before a limit stops you.**

The [README](https://github.com/uwuclxdy/clauth#readme) is the tour. This wiki is the reference.

## Pages

| Page | Answers |
|------|---------|
| [Install](Install) | every install path, how updates get verified, shell completions |
| [Quickstart](Quickstart) | first run, capturing profiles, every CLI command and flag |
| [Interface and keys](Interface-And-Keys) | the eight tabs, every keybinding, the action menus |
| [Configuration](Configuration) | `profiles.toml`, per-profile `config.toml`, model routing, storage layout |
| [Auto-switch](Auto-Switch) | the fallback chain: thresholds, gates, burn-aware mode, spend ceilings |
| [Daemon](Daemon) | `clauth daemon`, the `status.json` read contract for external readers |
| [Claude Code plugin](Claude-Code-Plugin) | the MCP server, its four tools, `delegate` in full |
| [herdr plugin](Herdr-Plugin) | the clauth popup in herdr, the key, the per-pane account tag |
| [Tokens and cost](Tokens-And-Cost) | where the token dashboard reads from, what the cost figure means |
| [Security](Security) | where credentials live, what a switch touches, the platform split |
| [FAQ](FAQ) | common questions and what to check when something misbehaves |

## Quick answers

- Settings live in `~/.clauth/profiles.toml` (global) and `~/.clauth/profiles/<name>/config.toml` (per account). Both are hand-editable.
- Most settings in those files are editable in the TUI too: `clauth`, then the Setup / Fallback / Config / Plugin tabs.
- A switch rewrites `~/.claude/.credentials.json`, parts of `~/.claude/settings.json`, and the stale identity block in `~/.claude.json`. Nothing else in `~/.claude/` moves.
- `clauth start <profile>` runs a session in its own config dir, so it never disturbs the account your global `claude` is on.
- Trust model, network activity, and vulnerability reporting: [SECURITY.md](https://github.com/uwuclxdy/clauth/blob/mommy/SECURITY.md).
