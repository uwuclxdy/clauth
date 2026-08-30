# Interface and keys

`clauth` opens on the Overview tab. <kbd>←</kbd> <kbd>→</kbd> move between tabs, <kbd>?</kbd> lists every binding for the tab you are on, <kbd>q</kbd> twice quits.

## Tabs

| Tab | Holds | You can |
|-----|-------|---------|
| **Overview** | account table, live 5h / 7d bars, chain position | switch accounts, reorder them |
| **Usage** | per-account window breakdown: 5h, 7d, per-model weeks, extra-usage spend, endpoint, merged env | refresh one account, toggle estimates and the pace marker |
| **Tokens** | global Claude Code token stats and API-equivalent cost | drill into models, change the period lens, count cache tokens |
| **Setup** | per-account endpoint, key, env, model routing, auto-start | edit any of it, log in, log out, disable, delete |
| **Fallback** | the auto-switch chain | reorder members, edit thresholds, flip gates, set a spend ceiling |
| **Config** | program-wide settings | change any of the rows in the table below |
| **Status** | incidents from status.claude.com with per-component health | open an incident's timeline or its page in a browser |
| **Plugin** | Claude Code wiring health and per-profile runtime state | apply one-key fixes |

The active account is orange. Usage numbers are cached on disk, so they stay on screen when the API is rate-limited or unreachable.

## Keys

### Everywhere

| Key | Action |
|-----|--------|
| <kbd>←</kbd> <kbd>→</kbd> (<kbd>tab</kbd> / <kbd>⇧tab</kbd> at the top level) | previous / next tab |
| <kbd>↑</kbd> <kbd>↓</kbd> | move the selection, or scroll a detail pane |
| <kbd>⏎</kbd> | act on the selected row (see below) |
| <kbd>n</kbd> | new account |
| <kbd>d</kbd> | open the divergence resolver, when one is pending |
| <kbd>x</kbd> | dismiss the newest toast, then the footer alert |
| <kbd>a</kbd> | action menu for the current row |
| <kbd>?</kbd> | keybinding help for this tab |
| <kbd>esc</kbd> | step back out of a sub-pane |
| <kbd>q</kbd> | step back, or arm quit at the top level; press again to confirm |
| <kbd>ctrl</kbd>+<kbd>c</kbd> | quit from anywhere |

### Tab-dependent

| Key | Behavior |
|-----|----------|
| <kbd>r</kbd> | Usage: refresh the selected account only. Tokens / Status / Plugin: reload that tab's data. Everywhere else: refresh every account |
| <kbd>t</kbd> | Tokens: cycle the period lens. Everywhere else: force-rotate every account's token, after a confirm |
| <kbd>⏎</kbd> | Overview: switch to the selected account. Tokens: open the model breakdown. Setup / Fallback: open a detail row, or commit an edit. Status / Plugin: open the detail |
| <kbd>⇧↑</kbd> <kbd>⇧↓</kbd> | Overview: reorder accounts. Fallback (chain focus): reorder chain members |
| <kbd>space</kbd> | Config: cycle a value. Setup `model` row and Fallback toggle rows: flip |
| <kbd>+</kbd> <kbd>-</kbd> | Fallback detail: step `rotate at` or `weekly at` by 5 |
| <kbd>e</kbd> | Usage: toggle burn estimates |
| <kbd>p</kbd> | Usage: toggle the ideal-pace marker |
| <kbd>c</kbd> | Tokens: count cache reads and writes in the token totals |
| <kbd>f</kbd> | Plugin: apply the selected row's fix |

On macOS, <kbd>t</kbd> skips any account holding a live `clauth start` session: that session's login lives in a Keychain item clauth cannot write, so rotating it would sign the session out.

## Action menus

<kbd>a</kbd> opens the actions available for whatever is selected. It lists what no key already does, so a screen whose <kbd>⏎</kbd> is the whole story carries no menu: Config, Fallback and Plugin have none. The footer only advertises <kbd>a</kbd> where something would open.

Entries above the rule act on the account named in the menu's title bar; entries below it act on the tab.

| Tab | Account | Tab-wide |
|-----|---------|----------|
| Overview | `refresh usage`, `rotate access token`, `disable account` / `enable account`, `open provider console` | `refresh all accounts`, `new account` |
| Usage | `refresh usage`, `rotate access token`, `disable account` / `enable account`, `open provider console` | `refresh all accounts`, `toggle estimates`, `toggle pace marker` |
| Tokens | none | `period: lifetime` / `daily` / `weekly` / `monthly`, `show all models` / `show claude models` / `show other models`, `toggle cache counting`, `reload stats` |
| Setup | `duplicate account`, `save as preset`, `apply preset`, `open provider console` | none |
| Status | none | `refresh status`, `open in browser` |

The active period or model filter is omitted from the Tokens menu, so the entries you see are the ones that would change something. `open provider console` follows the same idea from the other direction: it appears only on an account whose endpoint clauth knows a key page for, so an OAuth account's menu is one entry shorter.

The Setup detail pane is itself a list of actions, so <kbd>⏎</kbd> on a row is the action. What the menu adds is what works on the account as a whole, from either the account list or a settings row. On the `+ new` form there is no account yet, so nothing opens.

| Entry | Does |
|-------|------|
| `duplicate account` | asks for a name, then copies every setting onto a new account: endpoint, api key, env, models, thresholds. The stored login stays behind, as do the chain's `preferred` and `last resort` marks, which only one account may hold |
| `save as preset` | stores this account's base url and models under a name you type ([Configuration](Configuration#presets)). An existing preset asks first; a built-in's name is refused |
| `apply preset` | opens the picker, built-ins first. Applying replaces the endpoint and the whole model block, naming the fields first when any are set. <kbd>d</kbd> deletes a saved preset |
| `open provider console` | opens the page this account's api key is minted on, in your browser. Only for DeepSeek, Z.ai, OpenRouter and Alibaba Model Studio endpoints, so it is absent on an OAuth account and on any endpoint clauth does not recognise. An Alibaba account gets its own plan's page: Token Plan and Coding Plan are separate products, on separate pages, per console |

There is no `remove field`: an env row's <kbd>⏎</kbd> edits its value, and an empty value saves as empty, so the key stays. Drop one by editing the account's `config.toml`.

`disable account` from Overview or Usage asks first, since disabling drops the account from auto-switch, usage polling and status mid-flight; re-enabling is immediate. Neither runs for the active account or for one holding a live `clauth start` session; the pick names whichever is in the way.

## Setup tab rows

| Row | Sets |
|-----|------|
| `name` | the profile name |
| `auto-start` | whether clauth opens the 5h window with a 1-token ping ([Configuration](Configuration#auto-start-the-5-hour-window)) |
| `base url` | the API endpoint; blank means an OAuth account |
| `api key` | the key for that endpoint |
| `model` | the account's default model; <kbd>space</kbd> cycles presets, <kbd>⏎</kbd> types a full id |
| `+ model override` | expands to `opus`, `sonnet`, `haiku`, `fable`, `subagent` id overrides |
| env entries | extra environment variables merged into `settings.json` while this account is active; <kbd>⏎</kbd> edits a value, and an empty one keeps the key |
| `disable account` / `enable account` | hides the account from auto-switch and polling, keeping its files |
| `+ login` / `re-login` | what it runs depends on the account, the same three-way split `clauth login` has: an OAuth account mints a browser login, an api-key account re-enters its base url and key inline, and a Model Studio account opens the Alibaba console to capture the usage session its api key cannot stand in for ([Configuration](Configuration#the-alibaba-console-session)). That last one replaces the session and nothing else, since the endpoint and api key keep their own rows. A browser login onto an account that already has an endpoint and a working key leaves both standing too, and replaces the subscription login alone |
| `log out` | drops the stored credentials, keeps the profile |
| `token` | read-only state of a stored long-lived setup token: its remaining life, or `expired` / `mis-filled` with the fix beneath it ([Configuration](Configuration#account-types)) |
| `clear long-lived token` | drops that token so the account's own OAuth login installs again, or signs Claude Code out when the account has only an api key behind it; the row's hint names which. Appears while ANY long-lived piece exists — the token, the preserved mint backup, or a set `rolling_token` flag — arms on the first press, clears on the second (the full exit: flag, sidecar, and backup together). Faint and inert when clearing would strip the account's last credential; a flag-only account disarms regardless, since no credential is touched |
| `delete account` | removes the profile; arms on the first press, deletes on the second |

## Config tab rows

| Row | Options | Default |
|-----|---------|---------|
| `theme` | `full`, `compatible` | auto-detected |
| `reset display` | `relative`, `clock`, `both` | `relative` |
| `clock` | `24h`, `12h` | `24h` |
| `on mismatch` | `ask`, `overwrite`, `new`, `discard` | `ask` |
| `refresh` | 15 / 30 / 60 / 90 / 120 / 300 s, or a typed value from 10 s to 1 h | `90s` |
| `refresh spent` | keep polling accounts already at 100% | on |
| `rotation` | `preemptive`, `lazy` | `preemptive` |
| `weekly limit` | chain-wide 7d exhaustion line, 50-100% | `98%` |
| `switch mode` | `static`, `burn-aware` | `static` |
| `burn floor` | earliest projected-switch point, 90-100% | `98%` |
| `burn horizon` | how far ahead burn-aware projects | `60s` |
| `quota spent` | `stay on active`, `switch off all` | `stay on active` |
| `allow extra usage` | `off`, `pay-as-you-go` | `off` |
| `extra usage spent` | `stay on active`, `switch off all` | `switch off all` |

`clock` is inert unless `reset display` shows one. `burn floor` and `burn horizon` are inert unless `switch mode` is `burn-aware`. `extra usage spent` is inert unless `allow extra usage` is on. What each of the auto-switch rows does: [Auto-switch](Auto-Switch).

## Plugin tab

Each row is a check on your Claude Code wiring: `clauth` on `PATH`, the `mcpServers` entry, the plugin install record, `claude --version`, and each profile's runtime state. A `herdr` row joins them when [herdr](Herdr-Plugin) is installed. <kbd>f</kbd> applies a fix on rows that offer one, behind a confirm that defaults to cancel:

| Fix | When it appears |
|-----|-----------------|
| `wire mcpServers into ~/.claude.json` | the entry is missing, project-local only, or points somewhere stale |
| `repair credentials` | the active profile's stored login disagrees with the live one |
| `relink credentials` | the active profile's credential link is missing while its stored credentials are intact |
| `add the keybinding and sidebar row to herdr's config` | the herdr plugin is installed but its key is unbound or its sidebar row is untemplated |
| `install the clauth plugin` | the plugin row reads not installed, or installed project-local only; confirming runs the real `claude plugin` installer at user scope |

The `herdr` row's detail takes focus: <kbd>⏎</kbd> on the row descends, <kbd>↑</kbd>/<kbd>↓</kbd> walk the options rows, <kbd>space</kbd> or <kbd>⏎</kbd> activates one (toggle, cycle, or open the tag-refresh editor), <kbd>+</kbd>/<kbd>-</kbd> step the refresh, <kbd>esc</kbd> closes the editor and then ascends. `delegate row text` opens a confirm that defaults to cancel.
