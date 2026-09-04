<p align="center">
    <img src="media/clauth.png" alt="clauth: Claude Code account switcher and usage monitor TUI" width="480" />
</p>

<h1 align="center">Claude Code multi-account manager & MCP Plugin</h1>

<p align="center">
  <a href="https://github.com/uwuclxdy/clauth/actions/workflows/release.yml"><img src="https://github.com/uwuclxdy/clauth/actions/workflows/release.yml/badge.svg" alt="Release build status" /></a>
  <a href="https://crates.io/crates/clauth"><img src="https://shields.uwuclxdy.dev/github/v/release/uwuclxdy/clauth?sort=semver&logo=rust&label=version&color=orange" alt="latest version" /></a>
  <a href="https://github.com/uwuclxdy/clauth/releases"><img src="https://shields.uwuclxdy.dev/github/downloads/uwuclxdy/clauth/total?label=downloads&color=blue" alt="GitHub release downloads" /></a>
  <img src="https://shields.uwuclxdy.dev/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-2b90d9" alt="Linux, macOS, Windows" />
  <a href="LICENSE"><img src="https://shields.uwuclxdy.dev/badge/license-MIT-green" alt="MIT license" /></a>
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#install">Install</a> ·
  <a href="#quickstart">Quickstart</a> ·
  <a href="#claude-code-plugin">Plugin</a> ·
  <a href="#alternatives">Alternatives</a> ·
  <a href="#faq">FAQ</a> ·
  <a href="https://github.com/uwuclxdy/clauth/wiki">Wiki</a>
</p>

**Juggle every Claude Code account from one terminal: switch in a keypress, track live 5h / 7d usage, auto-switch before a limit stops you, even hand a task to another account from inside Claude.**

Most account tools do one half. clauth pairs instant **switching between multiple Claude Code accounts** with a live **usage monitor**, then wires the two together so a fallback chain moves you off an exhausted account before Claude Code ever blocks. Works with Claude Pro, Max, Team, Enterprise OAuth accounts or any custom API endpoint. Linux, macOS, Windows.

![clauth TUI demo: switching Claude Code accounts with live usage bars](media/demo.gif)

> Font is kinda off on the recording, I promise it looks better than this.

## Features

- 🔄 **Switch** accounts in one keypress or `clauth <name>`: OAuth (Pro / Max / Team / Enterprise) or a custom API endpoint, plan tier detected for you
- 📊 **Monitor** live 5h / 7d rate-limit bars, a global token dashboard with API-equivalent cost, plus a live Claude status-incident feed
- 🤖 **Auto-switch** down a fallback chain the moment an account hits its limit, with weekly-window and spend-ceiling gates so a long run never stalls and never surprises you with a bill. Opted-in accounts queue their auto-start, opening 5h windows `5h / accounts` apart instead of all at once
- 🧩 **Run in parallel**: several accounts at once in isolated config dirs, or a clean headless session with none of your global memory, plugins, or hooks
- 🔌 **From inside Claude**: an MCP plugin lets a live session list, switch, or delegate a whole prompt (even headless) to another account, and tells a session when the account behind it changed
- 🖥️ **Headless**: `clauth daemon` runs the refresh and auto-switch loop with no TUI and publishes `status.json` for a menu-bar app to read
- 🛠️ **Quality-of-life**: browse and resume past sessions under any account, per-profile model routing, shell completions, signed self-updates, multi-instance safe

Full reference: **[the wiki](https://github.com/uwuclxdy/clauth/wiki)**.

## How it works

Claude Code stores its session in `~/.claude/.credentials.json` (OAuth tokens) and the `env` block of `~/.claude/settings.json` (base URL, API key). clauth keeps a per-profile snapshot of both. A switch swaps those two in place and leaves the rest of `~/.claude/` untouched. `clauth start` takes a different route: it launches `claude` against a temporary `~/.claude` mirror, so several accounts run at once.

## Install

Linux, macOS, Windows (Git Bash / MSYS2).

```bash
cargo install clauth
```

```bash
# no Rust toolchain needed; --nocargo forces a binary download
curl -fsSL https://raw.githubusercontent.com/uwuclxdy/clauth/mommy/install.sh | bash
```

Binary installs update themselves in the background, checksum and signature verified before anything is replaced; `CLAUTH_NO_UPDATE=1` turns that off. Cargo installs upgrade with `cargo install clauth`. On first launch clauth offers to install shell completions, asking before it touches your shell rc. More: [Install](https://github.com/uwuclxdy/clauth/wiki/Install).

## Quickstart

Capture your current Claude Code session as a profile:

```bash
clauth
# Select "+ new from current profile", enter a name, e.g. "work"
```

Repeat while logged in to a different account, then switch in the TUI (<kbd>⏎</kbd> + confirm) or directly by name:

```bash
clauth work
# switched to 'work'
```

Run claude under a profile without touching the global config:

```bash
clauth start personal -- --model haiku
# spawns claude with personal's credentials in a per-profile CLAUDE_CONFIG_DIR
```

For a clean, blind session (auth only, no global memory, plugins, or hooks):

```bash
clauth start --isolated personal -p < prompt.txt
# pass the prompt on stdin: a variadic claude flag (e.g. --disallowedTools a,b,c)
# would otherwise swallow a trailing positional prompt forwarded through clauth
```

| Command | Does |
|---------|------|
| `clauth` | open the TUI |
| `clauth <profile>` | switch and exit |
| `clauth start <profile>` | run `claude` under that account, in its own config dir |
| `clauth login <profile>` | add or re-authenticate an account: browser, manual (no browser, for ssh), or API key |
| `clauth list` / `clauth which` | account table with cached usage / who owns this session |
| `clauth sessions`, `resume`, `info` | browse past Claude Code sessions and resume one anywhere |
| `clauth daemon` | headless refresh + auto-switch loop |

Every command and flag: [Quickstart](https://github.com/uwuclxdy/clauth/wiki/Quickstart#commands).

The active profile shows in orange. Usage bars are cached locally, so they stay on screen even when the Anthropic API is rate-limited or offline. <kbd>←</kbd> <kbd>→</kbd> move between the eight tabs, <kbd>?</kbd> lists the keys for the tab you are on.

| Tab | What it holds |
|-----|---------------|
| **Overview** | switch and reorder accounts |
| **Usage** | per-account window breakdown |
| **Tokens** | global Claude Code token stats + API-equivalent cost across all models |
| **Setup** | endpoint, key, env, auto-start, per-profile model routing, account presets |
| **Fallback** | chain editor |
| **Config** | appearance, scheduler, auto-switch defaults |
| **Status** | Claude incident feed |
| **Plugin** | Claude Code wiring + per-profile runtime, with one-key fixes |

## Claude Code plugin

clauth ships a plugin that exposes your profiles to a live Claude Code session via MCP. Install it from the TUI: Plugin tab, `plugin` row, <kbd>f</kbd>, confirm. That drives Claude Code's own installer against a plugin tree clauth materializes locally, so there is nothing to add by hand. `/plugin marketplace add uwuclxdy/clauth` then `/plugin install clauth@clauth` works too; it registers the same plugin against this repo instead, and clauth re-points it at the local tree the next time it runs. Either way the plugin's tools are `clauth mcp`, so the binary has to be on your `PATH`.

A registration that breaks repairs itself: `clauth mcp` heals one at startup, so does the daemon's tick, and `clauth start` heals one before `claude` launches. That last one covers what a hook cannot, since a marketplace too broken to load means the plugin never loads and its hooks never fire.

| Tool | What it does | Quota |
|------|--------------|-------|
| `profiles` | every account with cached 5h/7d usage, provider, tier, live-session flag, observed throughput, and the account states worth a look before spending (disabled and no api key, both of which refuse a delegate; login expired, which refuses one except on an account that runs its own endpoint with its own key; a canceled subscription, which never refuses); `scope: "session"` names the account this session runs on | zero (disk cache) |
| `switch_profile` | relink the global active profile; the reply says what it does to this session | zero |
| `delegate` | hand a headless prompt to another account and return the answer (or a `job_id`) | **real usage window on the target account** |
| `monitor` | check, collect or wait on backgrounded delegates' results, or wait on clauth's state (active profile, its usage cache, the credentials file) | zero (filesystem) |

`delegate` fields, kill and resume rules, the manual `mcpServers` entry: [Claude Code plugin](https://github.com/uwuclxdy/clauth/wiki/Claude-Code-Plugin).

## Alternatives

clauth is the only one of these that pairs account switching with a live usage monitor and ties them together with an auto-switch chain, in a single TUI.

| Tool | What it does | Compared to clauth |
|------|--------------|--------------------|
| [claude-swap](https://github.com/realiti4/claude-swap) | CLI account switcher (token backup/restore) | no usage view, no auto-switch |
| [CCSwitcher](https://github.com/XueshiQiao/CCSwitcher), [claude-account-switcher](https://github.com/Symbioose/claude-account-switcher) | macOS menu-bar switchers | macOS-only, no fallback chain |
| [cc-account-switcher](https://github.com/ming86/cc-account-switcher) | credential-swap scripts | no TUI, no usage |
| [Claude-Code-Usage-Monitor](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor) | real-time usage monitor with predictions | monitoring only, single account |
| [claude-code-statusline](https://github.com/ohugonnot/claude-code-statusline) | rate-limit status line inside Claude Code | in-session display, no switching |
| `CLAUDE_CONFIG_DIR` by hand | manual per-account config dirs | what `clauth start` automates |

## FAQ

**How do I switch between multiple Claude Code accounts without logging out?** Install clauth, save each logged-in session as a profile once, then switch with `clauth <name>` or a single keypress in the TUI. No browser, no re-login.

**Can I run Claude Code with multiple accounts at the same time?** Yes. `clauth start <profile>` launches `claude` in an isolated `CLAUDE_CONFIG_DIR`, so parallel sessions don't share identity, settings, or billing caches.

**How do I run Claude Code without my global `CLAUDE.md`, plugins, or hooks?** `clauth start --isolated <profile>` keeps the account's auth but drops your `CLAUDE.md`, plugins, hooks, skills, MCP servers and tools, leaving a clean session for headless work or blind evals. The MCP `delegate` tool takes `isolated: true` for the same thing.

**Can Claude Code switch accounts automatically when I hit the 5-hour limit?** Yes: put accounts in the fallback chain and clauth switches to the next member with headroom the moment the active one crosses its threshold. It runs in the TUI or headless via `clauth daemon`.

**Is there a Claude Code MCP server / plugin to switch accounts from inside a chat?** Yes. clauth ships a plugin that runs as an MCP server (`clauth mcp`), so a live session can list accounts, `switch_profile`, or `delegate` a headless prompt to another account without leaving the chat.

**How do I monitor Claude Code usage and rate limits?** The Overview tab shows color-coded 5h (and 7-day) bars per account with reset times; the Usage tab breaks down every rate-limit window the API reports; the Tokens tab adds a global token dashboard with API-equivalent cost.

**Does it work with Claude Pro, Max, Team, and Enterprise?** Yes. OAuth profiles cover all paid tiers (plan auto-detected, including Max 5x / 20x). API-endpoint profiles cover the Anthropic API or any compatible proxy.

**Where does clauth store my Claude Code credentials?** Locally under `~/.clauth/`, with `0600` permissions on Unix. Tokens only ever go to Anthropic. See [SECURITY.md](SECURITY.md).

More, including what to check when something misbehaves: [FAQ](https://github.com/uwuclxdy/clauth/wiki/FAQ).

## Documentation

| Page | Covers |
|------|--------|
| [Install](https://github.com/uwuclxdy/clauth/wiki/Install) | every install path, update verification, completions |
| [Quickstart](https://github.com/uwuclxdy/clauth/wiki/Quickstart) | first run, every command, flag, and env var |
| [Interface and keys](https://github.com/uwuclxdy/clauth/wiki/Interface-And-Keys) | the eight tabs, every keybinding, the action menus |
| [Configuration](https://github.com/uwuclxdy/clauth/wiki/Configuration) | both TOML files key by key, model routing, storage layout |
| [Auto-switch](https://github.com/uwuclxdy/clauth/wiki/Auto-Switch) | thresholds, exclusion rules, burn-aware mode, spend ceilings |
| [Daemon](https://github.com/uwuclxdy/clauth/wiki/Daemon) | `clauth daemon` and the `status.json` read contract |
| [Claude Code plugin](https://github.com/uwuclxdy/clauth/wiki/Claude-Code-Plugin) | the MCP server and `delegate` in full |
| [herdr plugin](https://github.com/uwuclxdy/clauth/wiki/Herdr-Plugin) | the clauth popup in herdr, the key, the per-pane account tag |
| [Tokens and cost](https://github.com/uwuclxdy/clauth/wiki/Tokens-And-Cost) | where the dashboard reads from, what the cost figure means |
| [Security](https://github.com/uwuclxdy/clauth/wiki/Security) | where credentials live and how they move |

## Development

```bash
cargo build --release
cargo clippy --all-targets
cargo test
```

CI gates `fmt --check`, `clippy -D warnings`, the test suite, `cargo-deny` and `cargo audit` on every push to `mommy` and every pull request; a doc-only change is skipped.

> [!TIP] `cargo test showcase -- --ignored --nocapture` drives the real interactive TUI on fake data against a throwaway home dir (no network, never compiled into the binary). Handy for screenshots.

## Security

clauth handles live OAuth tokens and replaces its own binary over the network, so [SECURITY.md](SECURITY.md) lays out the trust model: where credentials live, every host clauth contacts, how updates get verified, and how to switch each behavior off. Found something exploitable? Report it privately through the repo's **Security → Report a vulnerability**.

## License

MIT
