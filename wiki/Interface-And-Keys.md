# Interface and keys

`clauth` opens on the Overview tab. <kbd>←</kbd> <kbd>→</kbd> move between tabs except while Usage or Tokens model detail has focus: <kbd>←</kbd> returns to the selector and <kbd>→</kbd> does nothing there. <kbd>?</kbd> lists every binding for the tab you are on; <kbd>q</kbd> twice quits from the top level.

## Tabs

| Tab | Holds | You can |
|-----|-------|---------|
| **Overview** | account table, live 5h / 7d bars, chain position, selectable codex accounts | switch or reorder Claude Code accounts, browse codex accounts, pick which harness shows |
| **Usage** | per-account window breakdown: 5h, 7d, per-model weeks, extra-usage spend, peak-rate state | refresh one account, toggle estimates and the pace marker, edit the account's note |
| **Tokens** | global Claude Code token stats and API-equivalent cost | drill into models, change the period lens, count cache tokens |
| **Setup** | per-account endpoint, key, env, model routing, auto-start, 5h alert | edit any of it, log in, log out, disable, delete |
| **Fallback** | the auto-switch chain | reorder members, edit thresholds, flip gates, set a spend ceiling |
| **Config** | program-wide settings | change any of the rows in the table below |
| **Status** | incidents from status.claude.com with per-component health | open an incident's timeline or its page in a browser |
| **Services** | the shunt gateway, running delegates, the Claude Code plugin and herdr | apply one-key fixes |

The active account is orange. A `▲` on an account's row means the provider behind it is on peak-rate hours right now — some providers charge more at set times of day (DeepSeek roughly doubles its API rate; Z.ai's GLM peak hours consume the coding plan's quota faster). The active account's `●` outranks `▲` on its own row, so an active account on peak hours keeps its dot and `▲` shows on the other accounts. A red `!` marks an account whose 5h alert fired, until its usage falls back below the line, and it outranks `●`. The Usage tab's `pricing` row names the state, the countdown to the next switch, and nothing renders for flat-rate accounts. The marker follows the provider's own published rate schedule, never which models the profile pins; an account on an endpoint clauth doesn't recognize shows none. A codex account's banked usage-limit resets show as `↺ N` on its Overview row while one is available — `clauth limit-reset` spends one ([Codex](Codex#use-a-usage-limit-reset)). Usage numbers are cached on disk, so they stay on screen when the API is rate-limited or unreachable. Once those figures age past the refresh cadence's stale threshold, the Usage tab's status block adds a `[ stale ]` pill; it reads the age of the reading — an OAuth account's own fetch stamp, one it cannot date reading stale at once, or a third-party account's cache write time — not the last fetch outcome, so a `[ cached ]` pill and it can appear together. On the fallback chain panel below the accounts, `↲ ~40m` beside a member names the account the chain switches to next, and roughly when.

Codex accounts ([Codex](Codex)) sit under the Claude Code rows in a section headed `codex — switch with clauth <name>` (the command in the blue accent), in the same columns: name (bold, in the blue accent, when it is the active codex profile), plan, 5h and 7d, with `—` where nothing is cached, and a dim `↺ N` after the columns while the account holds a banked usage-limit reset ([Codex](Codex#use-a-usage-limit-reset)). The rows are selectable: <kbd>↑</kbd> and <kbd>↓</kbd> reach every codex account, including the last one in a short viewport; <kbd>⏎</kbd> does not switch a codex account, and <kbd>a</kbd> opens tab-wide actions without an account group. There is no live or timer cell. A codex row shows the same `×` marker when its chain is quarantined, and its plan cell falls back to the plan the account's login claims while no poll has cached one. The accounts panel title carries the active filter — plain `ACCOUNTS` unfiltered, `ACCOUNTS ─ claude` / `ACCOUNTS ─ codex` filtered, the harness name in dim text (the border rule supplies the dashes) — and its right edge shows the by-harness counts (`52 claude · 1 codex`, an empty roster's harness omitted), the first thing to go on a narrow terminal. The header's line 2 opens with `[ N live ]`, every live session clauth can see, whatever account it runs on (dim at `[ 0 live ]`, `[ — live ]` when clauth cannot read where it tracks them), then holds the gauge and the status dot; a narrow terminal drops the live count first.

## Keys

### Everywhere

| Key | Action |
|-----|--------|
| <kbd>←</kbd> <kbd>→</kbd> (or <kbd>tab</kbd> / <kbd>⇧tab</kbd>) | previous / next tab; in descended Usage or Tokens model detail, <kbd>←</kbd> returns to the selector and <kbd>→</kbd> does nothing, while <kbd>tab</kbd> / <kbd>⇧tab</kbd> still switch tabs |
| <kbd>↑</kbd> <kbd>↓</kbd> | move the selection through long lists without snapping it to an edge, or scroll a detail pane |
| <kbd>⏎</kbd> | act on the selected row (see below) |
| <kbd>n</kbd> | new account · on the usage tab: edit the account's note |
| <kbd>d</kbd> | open the divergence resolver, when one is pending |
| <kbd>x</kbd> | dismiss the oldest toast, then the footer alert |
| <kbd>a</kbd> | action menu for the current row |
| <kbd>?</kbd> | keybinding help for this tab |
| <kbd>esc</kbd> | step back out of a sub-pane; in an open text field, put back the saved value (the footer reads `esc revert`), or on the new-account form keep what you typed (`esc done`) |
| <kbd>q</kbd> | step back, or arm quit at the top level; press again to confirm |
| <kbd>ctrl</kbd>+<kbd>c</kbd> | quit from anywhere |

A descended Usage or Tokens model detail scrolls one line with <kbd>↑</kbd> / <kbd>↓</kbd> and one current viewport with <kbd>PageUp</kbd> / <kbd>PageDown</kbd>; <kbd>esc</kbd> or <kbd>←</kbd> returns to its selector without changing the selected account or model. Choice dialogs and action menus follow the selected option so it remains visible; an overflowing choice shows a scrollbar. A custom-field overwrite whose consequence cannot fit in the choice dialog opens a confirmation with its full explanation scrollable and cancel selected by default; a fitting choice acts at once. Toasts stack in the top-right corner, at most three lines each: a longer message ends its third line in `…`.

### Tab-dependent

| Key | Behavior |
|-----|----------|
| <kbd>r</kbd> | Usage: refresh the selected account only. Tokens / Status / Services: reload that tab's data. Everywhere else: refresh every Claude Code account |
| <kbd>t</kbd> | Tokens: cycle the period lens. Everywhere else: force-rotate every Claude Code account's token, after a confirm |
| <kbd>⏎</kbd> | Overview: switch to the selected Claude Code account; inert on a codex row. Usage: descend into the selected account's detail to scroll its full breakdown. Tokens: open the model breakdown, then descend into the selected model's detail; a view with no model has no detail to open. Setup / Fallback: open a detail row, or commit an edit; on a Fallback card's `preferred days` row, open the day picker. Status / Services: open the detail (on the `shunt` card's focused `enabled` row, flip it) |
| <kbd>⇧↑</kbd> <kbd>⇧↓</kbd> | Overview: reorder Claude Code accounts; inert on a codex row. Fallback (chain focus): reorder chain members |
| <kbd>space</kbd> | Config: cycle a value. Setup `model` row and Fallback toggle rows: flip. Fallback `preferred days`: step `never` → `weekdays` → `weekends` → `every day` and back to `never`, saving each step; a custom set is one more stop after `every day` until you leave the card. Services `shunt` card, on the focused `enabled` row: flip it |
| <kbd>+</kbd> <kbd>-</kbd> | Fallback detail: step `rotate at` or `weekly at` by 5; Setup: step `alert at` by 5 |
| <kbd>e</kbd> | Usage: toggle burn estimates |
| <kbd>p</kbd> | Usage: toggle the ideal-pace marker |
| <kbd>c</kbd> | Overview: cycle the harness filter (both → claude → codex → both; the accounts panel title names the active one). Tokens: count cache reads and writes in the token totals |
| <kbd>f</kbd> | Services: apply the fix the footer names (the selected row's first, the focused problem in the plugin detail, or the focused `f` line on the shunt card) |

The day picker on a Fallback card's `preferred days` row shows each weekday as `[x]` or `[ ]`: <kbd>←</kbd> <kbd>→</kbd> walk the days (and do not switch tabs while it is open), <kbd>space</kbd> toggles the day and saves at once, <kbd>⏎</kbd>, <kbd>esc</kbd> or <kbd>q</kbd> leave it, and <kbd>↑</kbd> <kbd>↓</kbd> leave it and move to the row above or below. <kbd>?</kbd> and <kbd>x</kbd> keep working while it is open; every other key does nothing until you leave.

On macOS, <kbd>t</kbd> skips any account holding a live `clauth start` session: that session's login lives in a Keychain item clauth cannot write, so rotating it would sign the session out.

The footer labels <kbd>c</kbd> `harness` on the Overview; the <kbd>?</kbd> help for that tab does not list it. With the Overview showing codex rows alone, <kbd>↑</kbd> and <kbd>↓</kbd> select codex accounts and the scrollbar follows; <kbd>⇧↑</kbd>, <kbd>⇧↓</kbd> and <kbd>⏎</kbd> do not change a Claude Code account, and <kbd>a</kbd> opens tab-wide entries alone. <kbd>n</kbd>, <kbd>r</kbd> and <kbd>t</kbd> keep working on the Claude Code accounts; neither refresh nor rotation reaches a codex row (codex usage polls on the refresh interval alone, and a codex chain rotates only in the background). On every other tab <kbd>c</kbd> keeps its own meaning or none.

## Action menus

<kbd>a</kbd> opens the actions available for whatever is selected. It lists what no key already does, so on Config, Fallback and Services, where every action already has a key, it holds only the daemon and shunt entries every tab ends on. The footer only advertises <kbd>a</kbd> where something would open.

Entries above the rule act on the account named in the menu's title bar; entries below it act on the tab, and the daemon and shunt entries, when shown, come last.

| Tab | Account | Tab-wide |
|-----|---------|----------|
| Overview | `refresh usage`, `rotate access token`, `disable account` / `enable account`, `open provider console` | `refresh all accounts`, `new account` |
| Usage | `refresh usage`, `rotate access token`, `disable account` / `enable account`, `open provider console` | `refresh all accounts`, `toggle estimates`, `toggle pace marker` |
| Tokens | none | `period: lifetime` / `daily` / `weekly` / `monthly`, `show all models` / `show claude models` / `show other models`, `toggle cache counting`, `reload stats` |
| Setup | `duplicate account`, `save as preset`, `apply preset`, `open provider console` | none |
| Status | none | `refresh status`, `open in browser` |
| every tab | none | `start daemon` / `stop daemon`, then `start shunt` / `stop shunt`, last |

The usage tab claims <kbd>n</kbd> for the selected account's note: a free-form text block shown in full on the tab under `notes:` (`press n to add notes` while the account has none). <kbd>⏎</kbd> saves, <kbd>esc</kbd> cancels, <kbd>⌃j</kbd> starts a new line. The note is stored as `profiles/<name>/note.txt` (see [Configuration](Configuration#storage-layout)), so it follows the account on rename and goes with it on delete; everywhere else <kbd>n</kbd> still starts a new account.

`start daemon` shows while no daemon runs and, once you confirm, starts `clauth daemon` in the background, detached from the TUI: it keeps running after you quit the TUI or close its terminal, and writes its log to `~/.clauth/daemon.log`. If a daemon came up elsewhere while the confirm was open, it starts nothing and says `daemon already running`. It starts the plain daemon, so it serves no REST API; for the clauth app, run `clauth daemon --listen` yourself ([Daemon](Daemon)). `stop daemon` shows while one runs and stops it the way `clauth daemon --replace` does, taking the shunt gateway and every enabled proxy down with it (a daemon that takes over at once, such as a standby, runs its own). Neither shows while one of them is still working; a toast reports how it went.

`stop shunt` shows while the daemon runs the shunt gateway and its status is fresh. It asks first, naming how many live sessions run on an account whose base URL points at the gateway when any do and clauth can count them, then holds the gateway off until the daemon next restarts; it never changes the card's `enabled` setting. `start shunt` shows in four cases: with no gateway adopted it opens the Services tab on the `shunt` card, on its `f  adopt config` line when the card offers one; with the gateway disabled it lands on the card's `enabled` row; with no daemon running it asks to start the daemon (hidden while a daemon start or stop is still working); and on a held gateway it lifts the hold. In any other state, and while a shunt action is still working, neither shows. A pick whose entry no longer applies, because the gateway changed since the menu opened, says so and does nothing.

The active period or model filter is omitted from the Tokens menu, so the entries you see are the ones that would change something. `open provider console` follows the same idea from the other direction: it appears only on an account whose endpoint clauth knows a key page for, so an OAuth account's menu is one entry shorter.

The Setup detail pane is itself a list of actions, so <kbd>⏎</kbd> on a row is the action. What the menu adds is what works on the account as a whole, from either the account list or a settings row. On the `+ new` form there is no account to duplicate or save, so its account entry is `apply preset` alone, stamping the draft's endpoint and model fields.

| Entry | Does |
|-------|------|
| `duplicate account` | asks for a name, then copies every setting onto a new account: endpoint, api key, env, models, thresholds. The stored login stays behind, as do the chain's `preferred` and `last resort` marks, which only one account may hold |
| `save as preset` | stores this account's base url and models under a name you type ([Configuration](Configuration#presets)). An existing preset asks first; a built-in's name is refused |
| `apply preset` | opens the picker, built-ins first. Applying replaces the endpoint and the whole model block, naming the fields first when any are set. <kbd>d</kbd> deletes a saved preset |
| `open provider console` | opens the page this account's api key is minted on, in your browser. Only for DeepSeek, Z.ai, OpenRouter, MiniMax and Alibaba Model Studio endpoints, so it is absent on an OAuth account and on any endpoint clauth does not recognise. An Alibaba account gets its own plan's page: Token Plan and Coding Plan are separate products, on separate pages, per console |

There is no `remove field` because none is needed: an env row's <kbd>⏎</kbd> edits its value, and an empty value removes the key. Editing it back is the same row.

`disable account` from Overview or Usage asks first, since disabling drops the account from auto-switch, usage polling and status mid-flight; re-enabling is immediate. Neither runs for the active account or for one holding a live `clauth start` session; the pick names whichever is in the way.

## Setup tab rows

The account list ends in an action row: `+ new`, which turns this pane into the create form. On that form, below `+ login`, `+ capture current login` stashes the login Claude Code is using now (it appears only when that login exists and no saved account owns it); <kbd>⏎</kbd> on `create account` then saves it under the name you typed.

| Row | Sets |
|-----|------|
| `status` | read-only, and present only while the account is disabled |
| `type` | read-only `api` or `oauth`, off the base-url row and tracking what you type into it |
| `email` | read-only: the address of the account's Claude login, on an `oauth` account once clauth has read it with the plan tier (re-read hourly); hidden after a log out |
| `provider` | read-only, present only for an endpoint clauth recognises: which provider it typed the account as |
| `token` | read-only state of a stored long-lived setup token, above the editable rows, in one of eight states. Static: `long-lived · ~Nd left`, `expires in ~Nd` inside a month, `long-lived · no recorded expiry`, and `expired`. Rolling: `rolling · re-stamps in ~Nh`, `rolling · re-stamp due` inside the last hour, `rolling · no recorded expiry`, and `rolling token stalled` once nothing re-stamped it in time. `mis-filled` is neither: the sidecar holds a rotating pair the split cannot use. The charged states carry the fix beneath them ([Configuration](Configuration#account-types)) |
| `name` | the profile name |
| `auto-start` | whether clauth opens the 5h window with a 1-token ping ([Configuration](Configuration#auto-start-the-5-hour-window)) |
| `alert at` | the 5h % at which this account's alert fires: the TUI toast, and the daemon's `bell_command` ([Configuration](Configuration#profilestoml)); <kbd>+</kbd>/<kbd>-</kbd> step it by 5, starting at 95% from `off`; <kbd>⏎</kbd> types a value from 0 to 100, and an empty one turns it off |
| `base url` | the API endpoint; blank means an OAuth account |
| `api key` | the key for that endpoint |
| `model` | the account's default model; <kbd>space</kbd> cycles `default`, `fable`, `opus`, `sonnet`, `haiku`, `opusplan`; <kbd>⏎</kbd> types a full id |
| `+ model override` | expands to `fable`, `opus`, `sonnet`, `haiku`, `subagent` id overrides; a set one always shows, and one you empty keeps its row until you leave the pane or apply a preset |
| env entries | extra environment variables merged into `settings.json` while this account is active; <kbd>⏎</kbd> edits a value, and an empty one removes the key |
| `+ login` / `re-login` | what it runs depends on the account, the same three-way split `clauth login` has: an OAuth account mints a browser login, an api-key account re-enters its base url and key inline, and a Model Studio account opens the Alibaba console to capture the usage session its api key cannot stand in for ([Configuration](Configuration#the-alibaba-console-session)). That last one replaces the session and nothing else, since the endpoint and api key keep their own rows. A browser login onto an account that already has an endpoint and a working key leaves both standing too, and replaces the subscription login alone. While a browser login runs its modal offers <kbd>r</kbd> to open the browser again, <kbd>c</kbd> to copy the sign-in link for another device to your local clipboard through the terminal (OSC 52: kitty, WezTerm, Windows Terminal, tmux with `set-clipboard on`, and iTerm2 once `General > Selection > Applications in terminal may access clipboard` is on), and <kbd>p</kbd> to turn that row into a code field: type or paste the code the link's page shows, <kbd>⏎</kbd> submits, <kbd>esc</kbd> brings the row back; a bad paste clears the field and says why |
| `log out` | drops the stored credentials, keeps the profile |
| `clear long-lived token` | drops that token so the account's own OAuth login installs again, or signs Claude Code out when the account has only an api key behind it; the row's hint names which. Appears while ANY long-lived piece exists — the token, the preserved mint backup, or a set `rolling_token` flag — arms on the first press, clears on the second (the full exit: flag, sidecar, and backup together). Faint and inert when clearing would strip the account's last credential; a flag-only account disarms regardless, since no credential is touched |
| `disable account` / `enable account` | hides the account from auto-switch and polling, keeping its files |
| `delete account` | removes the profile; arms on the first press, deletes on the second |

## Config tab rows

| Row | Options | Default |
|-----|---------|---------|
| `theme` | `full`, `compatible` | auto-detected |
| `reset display` | `relative`, `clock`, `both` | `relative` |
| `clock` | `24h`, `12h` | `24h` |
| `home tab` | `overview`, `usage`, `tokens`, `setup`, `fallback`, `config`, `status`, `services` | `overview` |
| `on mismatch` | `ask`, `overwrite`, `new`, `discard` | `ask` |
| `non-login keys` | keep a `/design-login` with the account it was made on | on |
| `refresh` | 15 / 30 / 60 / 90 / 120 / 300 s, or a typed value from 10 s to 1 h | `90s` |
| `refresh spent` | keep polling accounts already at 100% | on |
| `context nudge` | off / 300k / 400k / 600k / 900k, or a typed value from 50k to 2M tokens | off |
| `auto-start queue` | space `auto_start` accounts' 5h window opens `5h / N` apart | off |
| `rotation` | `lazy`, `preemptive` | `preemptive` |
| `auto-update` | checks for updates at launch, unless `CLAUTH_NO_UPDATE=1` | on |
| `weekly limit` | chain-wide 7d exhaustion line, 50-100% | `98%` |
| `switch mode` | `static`, `burn-aware` | `static` |
| `burn floor` | earliest projected-switch point: 97 / 98 / 99 / 100% | `98%` |
| `burn horizon` | how far ahead burn-aware projects | `60s` |
| `walk order` | `chain`, `soonest weekly reset`, `most weekly headroom` | `chain` |
| `quota spent` | `stay on active`, `switch off all` | `stay on active` |
| `allow extra usage` | `off`, `pay-as-you-go` | `off` |
| `extra usage spent` | `stay on active`, `switch off all` | `switch off all` |

`clock` is inert unless `reset display` shows one. `burn floor` and `burn horizon` are inert unless `switch mode` is `burn-aware`. `extra usage spent` is inert unless `allow extra usage` is on. `auto-start queue` is inert until some account has `auto_start` on. What each of the auto-switch rows does: [Auto-switch](Auto-Switch).

## Services tab

Four rows: `shunt`, `delegates`, `plugin`, and `herdr` (shown once herdr resolves; the tab probes for it in the background the first time you open it, and <kbd>r</kbd> probes again). A row's dot carries its health; the full readout lives in the detail pane. <kbd>⏎</kbd> descends into the selected row's detail (`delegates` excepted: its detail takes no keys). A fixable problem shows an `f  <verb>` line under the problem it fixes (the `f` in the blue accent, the verb dim), and the footer shows `f <verb>`. On the list <kbd>f</kbd> fixes the first fixable problem on the selected row; in the `plugin` detail <kbd>↑</kbd>/<kbd>↓</kbd> walk the problems, the focused one highlighted, and <kbd>f</kbd> fixes it. In the `plugin` and `herdr` details <kbd>PageUp</kbd>/<kbd>PageDown</kbd> scroll diagnostic lines by the visible page; while reading them, a hidden fix or option cannot act, and an arrow restores action focus. In the `shunt` detail <kbd>↑</kbd>/<kbd>↓</kbd> walk the card's action lines, <kbd>f</kbd> fixes a focused `f` line, and on the focused `enabled` row <kbd>space</kbd> or <kbd>⏎</kbd> flips it.

`shunt` reports the managed gateway (the daemon's live verdict when it runs the gateway, else the gateway record's own) and carries its controls. It names the binary, config and version it resolved, the state and its reason, then the pid and port. With no gateway adopted the state reads `not adopted`, followed by the config shunt would load from where the dashboard runs and a shunt already answering on the address that config sets, each only when there is one; while the card shows that answering shunt, the card checks it again every 5 seconds and drops the line once it stops answering. With no config found the check asks shunt's default address; `SHUNT_SERVER__BIND` moves either, as it moves shunt's own, read from the environment the adopt reads: the running daemon's, else the dashboard's. A daemon that recorded no environment shows `the running daemon recorded no environment; restart it` and offers no adopt. When clauth cannot read the address (a YAML config, a `${...}` reference, port 0, an empty value), a `bind  not read (...)` line says why and the check falls back to shunt's default address; when `SHUNT_SERVER__BIND` hides that the config itself is YAML (a `shunt.toml` linked onto a YAML file counts as YAML), unreadable or not valid TOML, a `config  not read (...)` line says so, and the adopt itself refuses a config that does not parse. A found TOML config that clauth read, with a bind it read, adds an `f  adopt config` line: confirming it takes the config over (a standalone shunt answering on that address refuses the adopt), plans the store move, and writes `~/.clauth/gateway.toml`: with nothing to move and no refusal it adopts `enabled` and a running daemon starts it at once, otherwise it stays off until the move lands or you turn `enabled` on. Once adopted, the card adds the `enabled` toggle, the admin fix, and the codex pool note: `f  add admin key` sits under `admin  clauth's key is not in [server.admin]` when the config has that table but not clauth's key, `f  add admin table` sits under `admin  no [server.admin] table` when it has none (a running gateway restarts once to load it, [Daemon](Daemon)), and `codex  provider <name> needs its own pool login` names each provider with no pool login while the gateway runs healthy with clauth's key (a failed read shows a `pool  <error>` line instead). While the gateway is off and the plan moves a file or refuses, a `MOVE PLAN` section lists `moves` / `stays` rows above `f  move stores in`, which moves the stores in and turns the gateway on, or a single `refused` row saying why no move can run. Descended, <kbd>↑</kbd>/<kbd>↓</kbd> walk these action lines (`enabled` and the `f` lines), landing on each as it scrolls into view; on the focused `enabled` row <kbd>space</kbd> or <kbd>⏎</kbd> flips it, and on a focused `f` line <kbd>f</kbd> applies it. Every fix confirms first.

`delegates` lists the `clauth mcp` job store in its detail. The row's dot reads green while a job runs and dim when none; a recent delegate rate-limit warning names the accounts in its detail. The detail binds no key: a delegate is stopped through the plugin's `monitor` tool ([Claude Code plugin](Claude-Code-Plugin)).

`plugin` folds the Claude Code wiring checks into one detail: the plugin install record first, then the `mcpServers` entry (`mcp entry` registered or not, `mcp server` ok or won't start, `mcp source`), then the `claude` version, whether `clauth` is on `PATH`, and the clauth data dir. Its health is the worst of the four, and two fixes can apply, in this order:

| Verb | When it appears |
|------|-----------------|
| `install plugin` | the plugin reads not installed, installed for this project only, or installed at a version older than this clauth (its `version` line then names both, `0.16.0 (this clauth is 0.17.0)`); installs at user scope, which updates an older install, and turns the plugin on |
| `wire mcp server` | the `mcpServers` entry is missing, project-local only, or drifted off the current launch line; writes the clauth entry into `~/.claude.json` |

To install the plugin: select the `plugin` row, press <kbd>f</kbd>, confirm.

The `herdr` row's detail takes focus: <kbd>⏎</kbd> on the row descends, <kbd>↑</kbd>/<kbd>↓</kbd> walk the options rows, <kbd>space</kbd> or <kbd>⏎</kbd> activates one (toggle, cycle, or open the tag-refresh editor), <kbd>+</kbd>/<kbd>-</kbd> step the refresh, <kbd>esc</kbd> closes the editor and then ascends. `delegate row text` opens a confirm that defaults to cancel. <kbd>f</kbd> applies `heal herdr config` when the herdr plugin is installed and its key is unbound or its sidebar row is untemplated.
