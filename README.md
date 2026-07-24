<p align="center">
    <img src="media/clauth.png" alt="clauth: Claude Code account switcher and usage monitor TUI" width="480" />
</p>

<h1 align="center">Claude Code multi-account manager & MCP Plugin</h1>

<p align="center">
  <a href="https://github.com/xingfanxia/clauth/actions/workflows/ci.yml"><img src="https://github.com/xingfanxia/clauth/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI status" /></a>
  <img src="https://img.shields.io/badge/platform-macOS-2b90d9" alt="macOS" />
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-green" alt="MIT license" /></a>
</p>

<p align="center"><em>macOS-focused fork — real Keychain account switching, browser-OAuth login, and a headless auto-switch daemon + <code>status.json</code> feed for a menu-bar app. Forked from the upstream <code>clauth</code> TUI (see the <code>upstream</code> git remote and LICENSE).</em></p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#install">Install</a> ·
  <a href="#quickstart">Quickstart</a> ·
  <a href="#keys">Keys</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#claude-code-plugin">Plugin</a> ·
  <a href="#alternatives">Alternatives</a> ·
  <a href="#faq">FAQ</a> ·
  <a href="#security">Security</a>
</p>

**Juggle every Claude Code account from one terminal: switch in a keypress, track live 5h / 7d usage, auto-switch before a limit stops you, even hand a task to another account from inside Claude.**

Most account tools do one half. clauth pairs instant **switching between multiple Claude Code accounts** with a live **usage monitor**, then wires the two together so a fallback chain moves you off an exhausted account before Claude Code ever blocks. Works with Claude Pro, Max, Team, Enterprise OAuth accounts or any custom API endpoint. Linux, macOS, Windows.

- 🔄 **Switch** accounts in one keypress or `clauth <name>`: OAuth (Pro / Max / Team / Enterprise) or a custom API endpoint, plan tier detected for you
- 📊 **Monitor** live 5h / 7d rate-limit bars, a global token dashboard with API-equivalent cost, plus a live Claude status-incident feed
- 🤖 **Auto-switch** down a fallback chain the moment an account hits its limit, so a long run never stalls
- 🧩 **Run in parallel**: several accounts at once in isolated config dirs, or a clean headless session with none of your global memory, plugins, or hooks
- 🔌 **From inside Claude**: an MCP plugin lets a live session list, switch, or delegate a whole prompt (even headless) to another account
- 🛠️ **Quality-of-life**: per-profile model routing, shell completions, signed self-updates, multi-instance safe

![clauth TUI demo: switching Claude Code accounts with live usage bars](media/demo.gif)

### The signature move: hot-swap a login under a live session

On macOS this fork's headline trick rests on one non-obvious fact: a running
`claude` re-reads its login from **one macOS Keychain item** (`Claude Code-credentials`)
on *every request*. So rewriting that one item hot-swaps the active account
underneath a live session — no restart, no re-login. The `clauth daemon` does it
automatically when the active account's 5-hour window fills:

<p align="center">
  <img src="media/hot-swap.gif" width="640" alt="hot-swap animation — the daemon rewrites one Keychain item at 95%, the next request picks up the new account" />
</p>

<p align="center">
  <img src="media/infographic-hotswap.jpg" width="640" alt="how the macOS Keychain hot-swap works" />
</p>

Pair it with **[ccsbar](https://github.com/xingfanxia/ccsbar)** (Claude Code
Switcher Bar), the native menu-bar companion that reads this daemon's
`status.json` feed and makes the next switch visible before it fires. Full
write-up: [I Taught My Claude Accounts to Rotate Themselves](https://blog.ax0x.ai/hot-swapping-claude-logins).

> Font is kinda off on the recording, I promise it looks better than this.

## Features

<details>
<summary><b>Full feature list</b></summary>

### Switch accounts

- **One-key switching**: pick a profile, <kbd>⏎</kbd>, confirm. Or `clauth <profile>` straight from the shell.
- **Log in an account**: `clauth login <profile> [--model <id>]` opens your browser for a real Claude Code OAuth login (the same PKCE flow Claude Code uses) and writes the minted tokens straight into a new profile, without touching the session you're already logged into. Pass an existing profile name to re-authenticate it in place; clauth asks to confirm before replacing that profile's saved login (a reauth also clears the profile's auth-broken quarantine). Works identically on every desktop platform (Linux, macOS, Windows), unlike running Claude Code's own `/login`, which on macOS lands only in a per-config-dir Keychain item and leaves the profile empty. `--model` sets the profile's default model (a preset alias or a full model id). Pass `--base-url <url>` and `--api-key <key>` instead to add or rotate an API-key account (DeepSeek, Z.ai, any Anthropic-compatible endpoint) with no browser. Any value a flag omits is prompted; the key is read echo-off so it stays out of shell history. Pass `--setup-token` to capture a `claude setup-token` mint instead: the pasted token (echo-off, or piped on stdin) becomes the profile's long-lived `session-token.json`, so its sessions run on a static ~1-year login that never races clauth's token refresher — the Setup tab then shows a `token` row counting down to the re-mint (`--yes` replaces an existing one unprompted). The split engages only for a genuinely long-lived token (no refresh token in the sidecar) — a mis-filled rotating pair is ignored, called out on the card, and switches keep installing the normal credentials. `--new` refuses to touch an existing profile — the race-proof create for non-TTY callers like a menu-bar app, which never see the confirm prompt.
- **Delete an account**: `clauth delete <profile> [--yes|-y] [--force]` removes a profile and all its credentials (OAuth tokens or API key, caches, the on-disk profile dir; a deleted active account is also unwired from live `~/.claude`; a deleted profile also leaves the fallback chain and the auth-broken quarantine). Confirms `[y/N]` on a TTY unless `--yes` (`-y`). Delete is irreversible, so a non-TTY run must pass `--yes`; it never deletes unprompted. A profile with a live `clauth start` session is refused unless you pass `--force` (`--yes` alone will not override it).
- **Account-change detection**: if Claude Code logged into a different account while clauth was closed, you get a `[Y/n]` prompt before stored tokens are overwritten.
- **Non-destructive**: a switch touches only the API keys and the profile's declared `env` block in `settings.json`. Nothing else moves.
- **Isolated launch**: `clauth start [--isolated] <profile> [claude args...]` runs `claude` in a per-profile `CLAUDE_CONFIG_DIR` (symlink mirror; copies on Windows without symlink privilege), so account identity and billing caches never leak between profiles. Add `--isolated` for a clean session that keeps the account's auth but drops your global `CLAUDE.md` memory, plugins, and hooks, for headless or blind runs (run it in an empty directory to skip project memory too).
- **Status-line aware**: `clauth which [--json]` prints which profile owns the loaded `credentials.json`, and with `--json` adds its plan tier.
- **Per-profile model routing**: each account can carry its own model overrides on the Setup tab (a default model plus per-tier opus / sonnet / haiku / subagent ids), so a switch or `clauth start` pins which models that account drives.
- **Codex accounts too (CDX-1)**: `clauth login <profile> --codex` captures the live `~/.codex/auth.json` (OpenAI Codex CLI) into a codex-harness profile — whole-file, byte-verbatim, so fields clauth doesn't model survive round-trips — and `clauth <profile>` switches codex accounts with the same verb as claude ones. Switches are loss-free (a rotated outgoing chain is adopted back first; an unrecognized live login is archived to `~/.clauth/quarantine/`, never destroyed) and take effect for **new** codex sessions (a running codex keeps its in-memory account; codex has no Keychain-style live re-read to hot-swap through — that's the file-swap ceiling, see `docs/codex-support/`). The daemon follows codex's own token refreshes back into the profile store, the TUI tags codex rows with their plan + email (parsed locally from the stored JWTs — zero network), `clauth doctor` gains a codex check, and the claude and codex active slots are fully independent: switching one never touches the other. Codex usage is never polled from any backend; identity and plan come from the tokens you already hold.
- **Codex, the rest of the ladder (CDX-3/1b/4/5)**: `clauth login <profile> --codex --browser` mints a *new* codex login via the PKCE flow straight into the store (the live login is untouched). The daemon keeps parked codex chains alive with a standby refresh (single-writer, so codex's single-use refresh tokens never trip reuse-detection). `clauth start <codex-profile>` runs `codex` in an isolated `CODEX_HOME` on that account. `clauth fallback add <codex-profile>` builds a codex auto-switch chain that rotates at session boundary when the active codex account is spent (the two harnesses' chains are independent). And **`clauth proxy`** is the opt-in localhost injection proxy — point codex at it (`clauth proxy --print-config`) and a mid-conversation 429 rotates to the next chain account and replays *before codex sees a byte*: true in-session fallback, the same seamlessness the claude side gets from the Keychain. The proxy strips codex's own identity headers and injects the selected account's, forwards SSE verbatim, and feeds per-account usage from the response headers — still never polling any usage backend.
- **Shell completions**: `clauth completions install [shell]` wires up bash, zsh, or fish.

### Monitor usage

- **Live usage bars**: 5h utilization from the Anthropic API on a configurable interval (default 90 s), color-coded with the next reset time. Max accounts also get a 7-day bar.
- **Per-account breakdown**: the Usage tab lays out every window (5h, 7d, 7d sonnet, 7d opus, paid extra-usage spend) plus endpoint, fallback threshold, and merged env keys.
- **Per-row activity**: a countdown to the next refresh, or a color-coded spinner (sapphire fetch, cyan token refresh, green auto-start).
- **Plan detection**: Pro, Max (5x / 20x), Team, Enterprise, identified via `/api/oauth/profile`.
- **Stale-data cues**: on the Overview tab, an account's refresh countdown turns yellow when showing last-known numbers (cache or rate limit), red when the fetch failed. A `×` marks an account whose login broke and needs a re-login.
- **Token usage dashboard**: the Tokens tab reads Claude Code's own token history (the stats cache, topped up from live session transcripts). It rolls that into per-model totals with a today panel, daily peak, busiest hour, and usage charts that grow with the terminal. Press <kbd>c</kbd> to count cache reads/writes in the totals; models past 1M tokens break out on their own. The <kbd>a</kbd> actions menu narrows the model bars and the per-model breakdown to Claude models only, or to everything else. <kbd>t</kbd> cycles a period lens (lifetime, today, this week, this month), re-scoping the dashboard cards and the per-model breakdown to that window (figures older days can't back, like cache splits, fall back to lifetime with a badge, and costs show as `$X+` floors).
- **API-equivalent cost**: the Tokens tab prices your recorded usage at live pay-as-you-go API rates, i.e. what those same tokens would cost on the API. Rates come from LiteLLM's price feed and are disk-cached, computed per model (families differ up to 10×) and cache-aware (reads and writes priced at their own rates). Cost shows on the today and total cards, the per-model detail, and the top-models bars. It stays blank until rates load.
- **Claude status feed**: the Status tab pulls live incidents from status.claude.com, with per-component health (claude.ai, API, Claude Code, Cowork), severity, and timeline, cached to disk.
- **Plugin wiring check**: the Plugin tab confirms clauth is hooked into Claude Code (`clauth` on PATH, the `mcpServers` entry or plugin install, a working `claude --version`) next to each profile's runtime state. One-key fixes cover the writes clauth can safely make itself: wire `mcpServers`, repair a diverged credential link. Plugin install stays guided.

### Automate & stay safe

- **Automatic token refresh**: OAuth refresh tokens are single-use, so rotation stays lazy: a stale access token rotates the moment a usage query 401s. Because a dead login often surfaces as an HTTP 429 rather than a 401, a 429 on an already clock-expired token still chases the refresh, so a revoked token is *seen* rather than masked behind stale cached usage forever. A refresh that fails terminally quarantines the account as `auth_broken` — excluded from every fallback-chain walk and refused as a switch target (installing a dead token would sign out every running `claude`) — until `clauth login <name>`, or any later successful refresh, clears it. The **active** account on macOS shares its single-use chain with the running `claude`, and whoever refreshes first revokes the other side — so clauth never bets on winning that race. When Claude Code rotates first, clauth **adopts** CC's fresher pair from its file mirror (identity-guarded: a login belonging to a different account is never captured unattended) instead of spending a revoked refresh token; when clauth rotates, it mirrors the fresh pair straight into the Keychain so the running `claude` never lapses (rotation coherence, #1). A diverged live login the endpoint **confirms dead** (identity probe rejected AND the refresh endpoint answers `invalid_grant`) is reclaimed automatically — the active profile's stored chain takes the live slot back, signing running sessions back in; a dead pair protects nothing, and parking it on a TUI decision wedged every switch while `claude` sat at "Login expired" (RESCUE-1, observed 2026-07-14). A pair the probe finds still alive is rotated back in place instead — never destroyed, never captured. An opt-in **preemptive rotation** toggle (Config tab, off by default) additionally rotates the active account a few poll intervals ahead of expiry — an optimization that makes adopt events rarer, not a correctness mechanism. <kbd>t</kbd> force-rotates every account.
- **Auto-switch on exhaustion**: opt accounts into an ordered fallback chain. When the active one crosses its 5h threshold (95% default), clauth hops to the next member with headroom. Headroom means BOTH windows: past the weekly line (7d, default 98%, tunable on the Config tab / `set_weekly_threshold` on the daemon socket — and per-account via the Fallback tab's **weekly at** override, which keeps the chain-wide value as the default) an account counts as exhausted — the active one switches away while there is still room to land the hop (topping out the week bricks an account for days, not hours), and the walk never picks such a member (one marked last resort still accepts, as the chain's parking spot) nor "recovers" it on a 5h rollover. Per-model weekly windows (e.g. "7d fable") gate the same way: an account whose scoped week is past the line stays out of rotation — a session of the capped model landed there would strand, and the walk can't know which model the next session runs. Both checks are per-account toggles on the Fallback tab: flip an account's **scoped gate** off to keep rotating to it for other models, or its **weekly gate** off to ignore the soft weekly line there (the 100% hard cap always blocks). An opt-in burn-aware mode (Config tab) switches on projected usage instead: heavy burn hops early, light burn rides closer to 100% before moving. An opt-in **allow extra usage** lets a spent chain fall back to an account's pay-as-you-go billing rather than stopping: it needs the Config-tab toggle *and* a per-account `max spend` ceiling (Fallback tab, $0 and off by default) *and* billing actually enabled on that account, so nothing is ever spent by default, and an account with subscription quota left always wins over one that costs money. The ceiling is a real cap, not just a gate on starting: once an account has spent it, clauth stops using that account, parking on your last resort if you named one and otherwise switching off. That halt is its own `extra usage spent` setting rather than `quota spent`, since staying put costs nothing when quota runs out and costs money when a budget does. Runs in the TUI, or unattended via `clauth daemon` with the TUI closed. A stuck active is a switch trigger too: an account flagged `auth_broken` (dead login), or one whose `/usage` polls stay 429'd past the retry cap (a deep-slot stuck rate limit, surfaced as `stale` in the feed), can never return a fresh read — so rather than wedging on it the daemon distrusts the frozen numbers and walks to the next healthy member. The dead-login case walks unconditionally (the login is gone); the stuck-rate-limit case still requires the last-known usage to be genuinely spent, so a throttle blip with real headroom left stays put. Meanwhile wrap-off (halt everything) keys on REAL exhaustion only: the 5h/burn limit or the 100% weekly HARD cap, never the 98% soft switch line and never the flag alone. Switching early to a sibling buys headroom safety, but signing every running session out early buys nothing, so a merely soft-blocked active with weekly room left keeps its sessions until the week is genuinely spent.
- **Headless daemon + status feed**: `clauth daemon` runs the usage-refresh + auto-switch loop with no TUI, writing `~/.clauth/status.json` (atomic, `0600`, schema-versioned, additive evolution; the read contract lives in [wiki/daemon.md](wiki/daemon.md)) and serving `~/.clauth/clauthd.sock` for a menu-bar app — snapshot / switch / refresh, plus fallback-chain configuration (`fallback_add` / `fallback_remove` / `fallback_move` / `set_threshold` / `set_last_resort` / `set_wrap_off` / `set_weekly_threshold`) and profile `rename`. The feed also publishes the daemon's own next-move `forecast` — the same chain-walk the switch decision runs, published as a *projection* ("where the chain would go next") so the menu bar renders one source of truth instead of re-deriving the walk client-side; the live decision itself additionally gates on the active account's fetch freshness and exhaustion (or its dead login, or a deep-slot stuck rate limit it distrusts) before acting. `clauth status --json` prints the same snapshot on demand. Install it at login with `dist/macos/daemon-install.sh` (macOS LaunchAgent). A single-instance lock keeps two daemons from double-firing: a second `clauth daemon` exits on the spot by default, so a spawner that fires repeatedly can't pile up idle daemons. `clauth daemon --status` answers "is one already up?" (exit 0/1) without starting anything; `--standby` instead parks and takes over when the first dies (for a launchd/systemd unit paired with a manual run); `--replace` terminates the running daemon and takes over, for an in-place upgrade. An anti-wedge watchdog aborts a stuck loop for a clean supervisor restart. The native macOS companion **[ccsbar](https://github.com/xingfanxia/ccsbar)** consumes this feed — an inspect-first account list with live 5h / 7d / Fable usage bars, the auto-switch forecast ("Watching X — would switch to Y at 95%"), and one-click switching (reads `status.json`, drives `clauthd.sock`). A TUI opened alongside the daemon detects it (singleton-lock + feed-freshness probe, re-checked every tick) and stands its own refresher down — one fetcher, one rotation writer, one switch decision-maker — re-arming within a tick of the daemon exiting.
- **Multi-instance safe**: state writes serialize through a file lock, each instance reloads on external changes, HTTP runs off the UI thread.
- **In-app help**: <kbd>?</kbd> opens a keybinding reference scoped to the current tab.

</details>

## How it works

Claude Code stores its session in `~/.claude/.credentials.json` (OAuth tokens) and the `env` block of `~/.claude/settings.json` (base URL, API key). clauth keeps a per-profile snapshot of both. A switch swaps those two in place and leaves the rest of `~/.claude/` untouched. `clauth start` takes a different route: it launches `claude` against a temporary `~/.claude` mirror, so several accounts run at once.

```mermaid
flowchart LR
    P["profiles in<br/>~/.clauth"]
    P -->|"clauth work"| S["swap ~/.claude<br/>credentials + env, in place"]
    P -->|"clauth start work"| I["launch claude in an<br/>isolated config dir"]
    ANT["Anthropic<br/>(api + platform)"] -. "poll usage, refresh tokens" .-> P
    P -. "auto-switch at your limit" .-> P
```

## Install

This fork targets **macOS** (the Keychain switching, browser-OAuth login, and
daemon are macOS features). It ships **no prebuilt release binaries** and is
**not** published to crates.io under this name — build it from source. Do **not**
run `cargo install clauth`: that pulls the upstream crate without any of the
fork's features.

**Build & install from this checkout:**

```bash
git clone https://github.com/xingfanxia/clauth
cd clauth
./install.sh          # cargo install --path . --locked  → ~/.cargo/bin/clauth
```

Or build without installing:

```bash
cargo build --release   # binary at ./target/release/clauth
```

The macOS menu-bar auto-switch daemon installs as a login LaunchAgent:

```bash
dist/macos/daemon-install.sh
```

This fork's binary does **not** self-update (it has no release pipeline; the
upstream self-updater is disabled so it can never replace the fork with an
upstream build). Rebuild from source to upgrade. Details in [SECURITY.md](SECURITY.md).

On first launch, clauth offers to install shell completions. It asks before touching your shell rc, and `CLAUTH_NO_COMPLETIONS=1` skips it. Re-run any time with `clauth completions install [shell]`.

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

The active profile shows in orange. Usage bars are cached locally, so they stay on screen even when the Anthropic API is rate-limited or offline. <kbd>←</kbd> <kbd>→</kbd> move between the eight tabs:

| Tab | What it holds |
|-----|---------------|
| **Overview** | switch and reorder accounts |
| **Usage** | per-account window breakdown |
| **Tokens** | global Claude Code token stats + API-equivalent cost across all models |
| **Setup** | endpoint, key, env, auto-start, per-profile model routing |
| **Fallback** | chain editor |
| **Config** | theme, reset display, refresh interval, wrap-off, divergence default |
| **Status** | Claude incident feed |
| **Plugin** | Claude Code wiring + per-profile runtime, with one-key fixes |

## Keys

Keys are scoped to the current tab; <kbd>?</kbd> lists every binding for the tab you're on.

<details>
<summary><b>All keys</b></summary>

| Keys | Action |
|------|--------|
| <kbd>←</kbd> <kbd>→</kbd> (or <kbd>tab</kbd> / <kbd>⇧tab</kbd> at the top level) | move between tabs |
| <kbd>↑</kbd> <kbd>↓</kbd> | move the selection |
| <kbd>⇧↑</kbd> <kbd>⇧↓</kbd> | reorder the selected account or fallback member |
| <kbd>⏎</kbd> | switch to the selected profile, or confirm an edit |
| <kbd>n</kbd> | add a new account |
| <kbd>r</kbd> | refresh usage now (per-tab: reloads Tokens / Status / Plugin) |
| <kbd>t</kbd> | force-refresh every account's token now (Tokens tab: cycle the period lens instead) |
| <kbd>a</kbd> | open the context action menu for the current tab |
| <kbd>+</kbd> <kbd>-</kbd> | step the fallback threshold by 5% |
| <kbd>c</kbd> | Tokens tab: count cache reads/writes in the totals |
| <kbd>p</kbd> | Usage tab: toggle the ideal-pace marker |
| <kbd>f</kbd> | Plugin tab: apply the selected row's fix |
| <kbd>esc</kbd> <kbd>q</kbd> | step back, or quit (press <kbd>q</kbd> twice at the top) |
| <kbd>?</kbd> | full keybinding help for the current tab |

</details>

## Configuration

Per-profile settings live in `~/.clauth/profiles/<name>/config.toml`. Profile order, the fallback chain, theme, and refresh interval live in `~/.clauth/profiles.toml`. Both are safe to hand-edit, and everything is editable in the TUI (Setup / Fallback / Config tabs).

### Profile types

**Claude Pro / Max / Team / Enterprise (OAuth):** leave the base URL blank. clauth captures the OAuth token from your running session, restores it on switch, and detects the plan tier for you.

**API endpoint:** set a base URL and, optionally, an API key. Works with the official Anthropic API or any compatible proxy. Edit the URL or key any time without losing stored credentials.

### Auto-start the 5-hour timer

The 5-hour window only opens after a real inference call; the OAuth refresh clauth runs at launch doesn't trip it. Toggle auto-start on the **Setup** tab, or set it in `config.toml`:

```toml
auto_start = true
```

clauth then sends a tiny Haiku ping (`max_tokens = 1`, fractions of a cent) on launch and on each refresh tick while no window is running. On a cold start it fetches usage before the first ping, so it never fires over a window that might already be live (the timer can arm one tick late as a result). Default off, OAuth profiles only. The older field name `kick_timer = true` still works on read.

> [!IMPORTANT]
> The ping is a real, billed `/v1/messages` call under your own OAuth token, the same request Claude Code fires on startup (see [what acts on your behalf](SECURITY.md#what-acts-on-your-behalf)). Leave auto-start off if you'd rather only the live `claude` process open a window.

### Auto-switch chain

The **Fallback** tab holds an ordered chain of profiles clauth hops between when one runs out of 5-hour budget. It lives in `profiles.toml` (`fallback_chain`, ordered) and per-profile `config.toml` (`fallback_threshold`).

- Each member has its own threshold (5h utilization %, default 95%); edit inline (<kbd>+</kbd> / <kbd>-</kbd> or type).
- After each usage refresh (at startup and on every tick), clauth checks the active profile. If it's a chain member at or above its threshold, clauth walks the chain (wrapping) and switches to the first member under its own threshold. The `◆` marker shifts in place.
- Mark a member **last resort** (a toggle row on its Fallback card) to make it the chain's parking spot: chosen only when every other member is past its threshold, never switched away from. The chain has one parking spot, so marking a member clears the mark on the rest. Claude Code then surfaces its own *"out of 5h limit"* message once that account also runs out. The threshold itself only means "switch away at N%", so a 100% threshold is just a late switch point.
- The chain-global **wrap-off** toggle (Config tab) decides what happens when everyone is exhausted and no member is marked last resort: off keeps you on the last account; on switches off all accounts, then re-arms once any member drops back under its threshold.
- A chain-global **switch mode** toggle (Config tab, default `static`) picks how the active account's "time to move" is judged. `static` = the plain threshold check above. `burn-aware` = clauth projects utilization at the next refresh from your recent burn rate and switches once the projection would cross 100%: heavy burn moves you early, light burn rides past the threshold toward 100%. Accounts without enough burn history fall back to their static threshold.
- No eligible target keeps clauth put. If the active profile isn't in the chain, auto-switch is disabled. Profiles outside the chain are never switched away from or to. It's opt-in.

<details>
<summary><b>Storage layout</b>: what clauth writes under <code>~/.clauth/</code></summary>

```
~/.clauth/
  profiles.toml          # profile order, active marker, fallback chain, wrap-off, theme, refresh interval
  price_cache.json       # cached model price table (LiteLLM rates) for the Tokens cost lens
  status_cache.json      # cached Claude status incident feed
  profiles/
    work/
      config.toml        # base_url, api_key, auto_start, fallback_threshold, [env], [models]
      credentials.json   # OAuth token snapshot (credentials.json.pending while a rotation is mid-write)
      usage_cache.json   # last known utilization + plan info
      account_id.json    # which account this profile is, so a live re-login can be told apart
      profile_fetched.json  # when the plan/tier was last fetched, so a restart doesn't re-ask
      runtime/           # per-profile CLAUDE_CONFIG_DIR tree for `clauth start`
      runtime-isolated/  # same, for `clauth start --isolated` (no operator memory/plugins/hooks)
      sessions/          # per-session PID files (ref-counting live launches)
      sessions-isolated/ # per-session PID files for isolated launches
      throughput_cache.json  # observed delegate tok/s + rate-limit hits per model
    personal/
      ...
```

</details>

## Claude Code plugin

clauth ships a plugin that exposes your profiles to a live Claude Code session via MCP. Add this repo as a plugin marketplace in Claude Code, then install the `clauth` plugin:

```
/plugin marketplace add xingfanxia/clauth
/plugin install clauth@clauth
```

Claude Code launches `clauth mcp` in the background for the session's lifetime; `clauth` must be on `PATH` (it already is after any standard install).

Once active, Claude Code can call five tools:

| Tool | What it does | Quota |
|------|--------------|-------|
| `list_profiles` | All profiles with cached 5h/7d usage %, provider, account tier, active flag, live-session flag, observed per-model throughput | zero (disk cache) |
| `which` | Which profile owns the current session (+ its resolved plan and observed throughput) | zero (filesystem) |
| `switch` | Relink the global active profile to another name | zero (no prime) |
| `delegate` | Delegate a headless prompt to another profile and return the answer (or a `job_id` with `background: true`) | **real usage window on the target account** |
| `delegate_result` | Fetch a `background` delegate's result by `job_id` (optional `wait_secs` long-poll) | zero (filesystem) |

<details>
<summary><b>Caveats to know</b></summary>

- `switch` relinks the global `~/.claude` credentials. A `clauth start` session runs against its own profile and is unaffected; a session on the global credentials adopts the new profile on its next token refresh, so it changes the running account mid-session. To reach another profile without disturbing the current session, use `delegate`.
- `delegate` burns a real 5h usage window on the target account. It is hard-capped at recursion depth 1, so a delegated session cannot call `delegate` again.
- `delegate` accepts `model` (which model the run uses), `cwd`, `env`, `args`, `timeout_secs` (default 300, max 3600), `isolated` (a clean delegate with no operator memory, plugins, or hooks), `background`, plus `monitor` (a backgrounded job then reports elapsed time and the target's live usage on a `delegate_result` poll). clauth records the delegate's observed tokens/sec per model and flags it as degraded or recently rate-limited in `list_profiles` / `which`. That's the only throughput signal available, since subscription throttle is per-model and absent from the usage snapshot.
- `background: true` returns a `job_id` immediately so the session keeps working while the delegate runs. The result auto-arrives via a bundled `PostToolUse` hook; with hooks disabled, fetch it with `delegate_result`.

</details>

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

**How do I switch between multiple Claude Code accounts without logging out?**
Install clauth, save each logged-in session as a profile once, then switch with `clauth <name>` or a single keypress in the TUI. No browser, no re-login.

**Can I run Claude Code with multiple accounts at the same time?**
Yes. `clauth start <profile>` launches `claude` in an isolated `CLAUDE_CONFIG_DIR`, so parallel sessions don't share identity, settings, or billing caches.

**How do I run Claude Code without my global `CLAUDE.md`, plugins, or hooks?**
`clauth start --isolated <profile>` keeps the account's auth but drops your operator memory, plugins, and hooks, leaving a clean session for headless work or blind evals. Run it in an empty directory to skip project memory too. The same is available on the MCP `delegate` tool via `isolated: true`.

**Can Claude Code switch accounts automatically when I hit the 5-hour limit?**
With clauth open, yes: put accounts in the fallback chain and clauth switches to the next member with headroom the moment the active one crosses its threshold.

**Is there a Claude Code MCP server / plugin to switch accounts from inside a chat?**
Yes. clauth ships a Claude Code plugin that runs as an MCP server (`clauth mcp`). Add the repo as a plugin marketplace, install `clauth@clauth`, then a live session can `list_profiles`, `which`, `switch`, or `delegate` a headless prompt to another account (optional `model`, `cwd`, `env`, `args`, `timeout_secs`, `isolated`, `background`, `monitor`) without leaving the chat.

**How do I monitor Claude Code usage and rate limits?**
The Overview tab shows color-coded 5h (and 7-day) bars per account with reset times; the Usage tab breaks down every rate-limit window the API reports; the Tokens tab adds a global token dashboard with API-equivalent cost.

**Does it work with Claude Pro, Max, Team, and Enterprise?**
Yes. OAuth profiles cover all paid tiers (plan auto-detected, including Max 5x / 20x). API-endpoint profiles cover the Anthropic API or any compatible proxy.

**Where does clauth store my Claude Code credentials?**
Locally under `~/.clauth/`, with `0600` permissions on Unix. Tokens only ever go to Anthropic. See [SECURITY.md](SECURITY.md) for the full breakdown.

## Development

```bash
cargo build --release
cargo clippy --all-targets   # CI gates clippy -D warnings + fmt --check + test on every push
cargo test
```

> [!TIP]
> `cargo test showcase -- --ignored --nocapture` drives the real interactive TUI on fake data against a throwaway home dir (no network, never compiled into the binary). Handy for screenshots.

## Security

clauth handles live OAuth tokens and replaces its own binary over the network, so [SECURITY.md](SECURITY.md) lays out the trust model: where credentials live, every host clauth contacts, how updates get verified, and how to switch each behavior off. Found something exploitable? Report it privately through the repo's **Security → Report a vulnerability**.

## License

MIT
