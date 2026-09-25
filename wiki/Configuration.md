# Configuration

Three files, all TOML, all safe to hand-edit while clauth runs (it reloads on external change):

- `~/.clauth/profiles.toml` for everything program-wide: profile order, the active marker, the fallback chain, appearance, the scheduler.
- `~/.clauth/profiles/<name>/config.toml` for one account: endpoint, key, env, model routing, its chain settings.
- `~/.clauth/codex-profiles.toml` for the codex roster: its own active marker, chain and weekly line ([Codex](Codex)).

Most keys below have a TUI equivalent on the Setup, Fallback, Config or Plugin tab ([Interface and keys](Interface-And-Keys#config-tab-rows)). A few are written only by a command or by clauth itself; those cells say which. The codex file has no TUI equivalent at all.

## Account types

**Claude Pro / Max / Team / Enterprise.** Leave `base_url` blank. clauth captures the OAuth token from your session or mints one through `clauth login`, then detects the plan tier from Anthropic's profile endpoint. The login also stamps the account's `rateLimitTier` into the credential, which Claude Code reads at startup for plan-gated flags; a profile minted earlier picks the stamp up on its next usage poll (the hourly `/profile` fetch), or immediately with one `clauth login <name>`.

**API endpoint.** Set `base_url`, and `api_key` if the endpoint wants one. Works against the Anthropic API or any compatible proxy. The key is handed to Claude Code through `apiKeyHelper` rather than written into `settings.json`.

**Long-lived setup token.** `clauth login <name> --setup-token` stores a `claude setup-token` mint as `session-token.json`. Sessions run on that static login, which never races clauth's token refresher. The Setup tab then shows a `token` row counting down to the re-mint. The mint reads as untiered to Claude Code (its scope set cannot read `/profile`, so no tier can be stamped), and plan-gated flags evaluate against their untiered default.

The token outranks the profile's OAuth pair at every switch for as long as it exists, so a later `clauth login <name>` updates only what clauth polls usage with. `clauth static-token <name> --clear` drops it and puts the OAuth login back in front of sessions — the full exit: the preserved mint backup and the `rolling_token` flag go with it, so nothing re-creates a sidecar afterwards.

A mint is a narrower credential than a `/login` session: it carries `user:inference` and `user:sessions:claude_code` and no refresh token, against the five scopes a browser login stores. Claude Code turns off anything gated on the wider set, Claude in Chrome by name. Clear the token if you want those features back — or arm `clauth rolling-token <name>`, which has the daemon re-stamp the sidecar from the profile's own usage chain: still no refresh token in front of sessions, but the chain's full scope set, plan stamp and `rateLimitTier`, so plan-gated models and flags work. A second live session holding one rotating login earns a warning naming `clauth rolling-token <name>` before a refresh signs the others out, and a session started before the arming converges onto the sidecar on its own next poll. The superseded mint waits at `session-token.static.json` and the bare `clauth static-token <name>` puts it back.

### Third-party usage data

Five providers get typed usage panels:

| Provider | Base URL | Shows |
|----------|----------|-------|
| DeepSeek | `https://api.deepseek.com` | balance rows per currency: api balance, granted, topped up |
| Z.ai | `https://api.z.ai` | percentage bars per limit window (5h / 7d / 30d), per-tool rows, plan level, 7-day per-model token totals |
| OpenRouter | `https://openrouter.ai` | wallet rows from the credits endpoint: api balance (remaining credits, red when overdrawn), used, purchased; then today / this week / this month usage, per-key cap rows when set, free-tier flag |
| Alibaba Model Studio | the four Qwen preset endpoints below | a 7d bar carrying your tier's absolute allowance, a 5h bar when the API reports one, plan tier, subscription status and days left |
| MiniMax | `https://api.minimax.io` | Token Plan bars for the 5h interval and the 7d window, plus a remaining row per plan bucket. The bars follow `general`, the bucket Claude Code bills against — or the lone bucket when the account has exactly one; with more than one bucket and no `general`, no bars are drawn. `video` and any other bucket ride as rows only. The mainland-China endpoint is not covered — it is a separate account on a different host, so it falls to the best-effort scan below |

Any other endpoint is scanned best-effort: clauth probes a short list of usage paths on the origin your key already authorizes, and renders whatever percentage, fraction-left window, or balance shapes come back. Those panels carry a "looks wrong? report it" line, since the shape is guessed. An endpoint that returns nothing usable is rescanned at most once every five minutes (or once per refresh interval, whichever is longer), and <kbd>r</kbd> forces a rescan immediately. A dead api key stops polling the same way, on any endpoint: the provider rejected it — a 401 on most endpoints, an in-band code inside an HTTP 200 on MiniMax — so the Usage tab reads `api key rejected, re-enter it on the setup tab` (a `[ key rejected ]` chip beside cached numbers instead) and `clauth list` marks the account `(key rejected)`.

#### Where the keys come from

For those five, `open provider console` in the TUI action menu ([Interface and keys](Interface-And-Keys#action-menus)) opens the page the account's key is minted on. The pages, if you would rather go directly:

| Endpoint | Page |
|----------|------|
| DeepSeek | <https://platform.deepseek.com/api_keys> |
| Z.ai | <https://z.ai/manage-apikey/apikey-list> |
| OpenRouter | <https://openrouter.ai/settings/keys> |
| MiniMax | <https://platform.minimax.io/user-center/payment/token-plan> |
| Alibaba Token Plan, international | <https://modelstudio.console.alibabacloud.com/ap-southeast-1?tab=plan#/efm/subscription/overview> |
| Alibaba Token Plan, mainland China | <https://bailian.console.aliyun.com/cn-beijing?tab=plan#/efm/subscription/overview> |
| Alibaba Coding Plan, international | <https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=globalset#/efm/coding_plan> |
| Alibaba Coding Plan, mainland China | <https://bailian.console.aliyun.com/cn-beijing/?tab=plan#/efm/subscription/coding-plan> |

Alibaba gets four rows because Token Plan and Coding Plan are separate products with separate quotas, sold on two consoles that do not share accounts. Take the key from the page matching the endpoint you configured, or your calls bill against a plan you did not mean. The key is the `sk-sp-…` one on your plan's own page, not a workspace key from the general api-key page, which is billed pay-as-you-go instead of drawing your plan's quota.

An AccessKey pair from the RAM console is a different thing again and clauth has no use for one. It cannot read a Solo plan's quota (measured, on the signed OpenAPI), so nothing here asks for one.

#### The Alibaba console session

Alibaba is the one provider whose api key cannot read its own quota: every quota endpoint ignores the key outright. Those panels run on a separate console session instead. `clauth login <account>` on a Model Studio account opens the Alibaba console in your browser and stores the session it hands back as a `[console]` table in that account's `config.toml`. Nothing else on the account changes. The console returns an api key and an endpoint alongside the session, both scoped to a workspace rather than to your plan and billed separately from it, so clauth discards them.

The 48-hour clock runs from your aliyun console sign-in, so it is already ticking by the time `clauth login` captures the session. Running the login again inherits whatever is left of that window, which can be minutes; sign in to the Alibaba console afresh first if you want a full one. Once it lapses, clauth stops polling that account: the Usage tab reads `console login expired, run clauth login` and `clauth list` marks it `(login expired)`. Polling resumes the moment a new session lands on disk, with no restart.

An account with no api key at all still gets its usage panel, since the quota rides the console session. It just cannot run Claude Code until you add a key (or an auth token in its `[env]`).

The Setup tab's `re-login` row runs this same console flow on a Model Studio account, so the shell is no longer the only route to it. Starting one from nothing is still two steps, because the console a session comes from is read off the endpoint: give the account a Qwen preset first, then log in.

## Model routing

Per account, on the Setup tab or in `config.toml`:

```toml
[models]
default  = "opusplan"                     # preset alias or a full model id
opus     = "claude-opus-4-5-20251101"     # ANTHROPIC_DEFAULT_OPUS_MODEL
sonnet   = "claude-sonnet-4-5-20250929"   # ANTHROPIC_DEFAULT_SONNET_MODEL
haiku    = "claude-haiku-4-5-20251001"    # ANTHROPIC_DEFAULT_HAIKU_MODEL
fable    = "claude-fable-5"               # ANTHROPIC_DEFAULT_FABLE_MODEL
subagent = "claude-sonnet-4-5-20250929"   # CLAUDE_CODE_SUBAGENT_MODEL
```

`default` lands as the top-level `model` key in `settings.json`; the rest ride in its `env` block. A switch or a `clauth start` applies whichever account you land on.

## Presets

A preset is a named `base_url` + `[models]` pair you can stamp onto any account from the Setup tab's <kbd>a</kbd> menu. Eight ship built in:

| Preset | Endpoint |
|---|---|
| `DeepSeek` | `https://api.deepseek.com/anthropic` |
| `Z.ai` | `https://api.z.ai/api/anthropic` |
| `OpenRouter` | `https://openrouter.ai/api` |
| `MiniMax` | `https://api.minimax.io/anthropic` |
| `Qwen-TokenPlan-Intl` | `https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic` |
| `Qwen-TokenPlan-CN` | `https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic` |
| `Qwen-CodingPlan-Intl` | `https://coding-intl.dashscope.aliyuncs.com/apps/anthropic` |
| `Qwen-CodingPlan-CN` | `https://coding.dashscope.aliyuncs.com/apps/anthropic` |

`DeepSeek`, `Z.ai`, `OpenRouter` and `MiniMax` set the endpoint plus a base model, leaving the tier rows yours to pin afterwards. The four Alibaba ones fill every row instead, because those endpoints reject a Claude model id outright rather than serving something for it, so any alias left unpinned fails on use. All eight leave the api key alone; pick the region your plan was bought in, since a key issued for one is not accepted by the other. Once a preset is stamped on, `open provider console` in the same menu opens that endpoint's own key page ([above](Configuration#where-the-keys-come-from)).

`save as preset` stores the focused account's own endpoint and models under a name you type, in `~/.clauth/presets/<name>.json`:

```json
{ "base_url": "https://api.example/anthropic", "models": { "default": "my-model" } }
```

`apply preset` opens the picker, built-ins first. Applying replaces the account's endpoint and its whole `[models]` block, so a tier the preset leaves unset is cleared rather than kept; the picker warns and names the fields first when the account already carries any. The account's own api key is never touched, and a preset never carries one. <kbd>d</kbd> in the picker deletes a saved preset; the built-ins have no file and stay.

## Auto-start the 5-hour window

The 5h window opens on a real inference call. clauth's own token refresh does not trip it, so an account can read 0% while the clock has yet to start.

```toml
auto_start = true    # per profile; older spelling kick_timer still reads
```

clauth then sends a 1-token Haiku ping on launch and on each refresh tick while no window is running. On a cold start it fetches usage before the first ping, so it never fires over a window that might already be live. That costs a fraction of a cent and it is a real billed `/v1/messages` call under your own token. Default off, OAuth accounts only.

If the messages limiter is blocking Claude Code, a live 5h window will not clear it. clauth re-tests with the same ping on the poll cadence and can rotate the chain around an account whose ping keeps getting rejected. The weekly (7d) window gets the same treatment at its own boundary: when an account's week was fully spent and its 7d window resets with the 5h window still live, clauth pings once to prove the account serves again, so the chain can return to it without waiting for a failed request.

### Interleaving it across accounts

With several accounts on `auto_start`, every window reopens the instant it lapses — so they stay in whatever phase they started in and all reset together, leaving you with everything at once and then nothing for five hours. `auto_start_queue` (off by default, Config tab `auto-start queue`) spaces them instead: a member may open a window only when no other member opened one in the last `5h / N`, so a freshly reset account comes within reach every `5h / N` — 2h30m with two accounts, 1h40m with three.

The spacing is self-organising. Accounts that all lapse together are spread across the first cycle and stay spread, because every window is exactly five hours. Nothing needs configuring beyond turning `auto_start` on per account; `N` counts the accounts that can actually open a window, so a quarantined, kick-rejected or credential-less one does not hold a slot open.

What it promises is **spacing of at least `5h / N`, converging on that figure** — not a reset on a fixed clock. After a lapse a member can wait up to `(N-1) x 5h / N` for its turn, and an account you actually use opens its window on demand regardless. With one account on `auto_start` nothing changes at all: the gap is the whole window, so that account still opens one the moment its last lapses.

clauth keeps no file for the queue: it derives the last open from `usage_history.jsonl`, where a window it opened is recorded the moment the kick lands and one opened out of band shows up as a reset boundary the samples agree on. A single usage-cache snapshot cannot stand in. `/usage` reports an idle window's reset five hours out, identical to a window that just opened, and anchoring on that would wedge the queue shut. A restart can read a stretch of idle samples as a boundary; the queue then opens one window late and re-spaces itself on the next cycle.

## `profiles.toml`

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `active_profile` | string | none | the account currently linked into `~/.claude` |
| `profiles` | list | `[]` | display order |
| `fallback_chain` | list | `[]` | ordered chain members ([Auto-switch](Auto-Switch)) |
| `refresh_interval_ms` | int | `90000` | usage poll cadence, 10 s to 1 h |
| `context_nudge_threshold_tokens` | int | none | context-window nudge threshold in tokens, 50k to 2M |
| `refresh_spent_accounts` | bool | `true` | keep polling accounts at 100% |
| `auto_start_queue` | bool | `false` | interleave the `auto_start` ping so windows open `5h / N` apart |
| `preemptive_rotation` | bool | `true` | rotate OAuth ahead of expiry; `false` waits for a rejection |
| `weekly_switch_threshold` | float | `98.0` | chain-wide 7d exhaustion line, 50-100; the codex chain has its own copy of this key in `codex-profiles.toml` ([below](Configuration#codex-profilestoml)) |
| `burn_aware_switching` | bool | `false` | project usage forward instead of comparing to the threshold |
| `burn_switch_floor_pct` | float | `98.0` | earliest point burn-aware may switch, 90-100 |
| `burn_horizon_cap_ms` | int | `60000` | how far ahead burn-aware projects |
| `walk_order` | string | `chain` | reorder each accept pass by the soonest-resetting 7d window: `chain` or `soonest-weekly-reset` |
| `wrap_off` | bool | `false` | switch off all accounts once the chain is out of quota |
| `spend_budget_switching` | bool | `false` | master switch for pay-as-you-go fallback |
| `switch_off_when_budget_spent` | bool | `true` | switch off once the spend ceiling is used up |
| `default_divergence` | string | none | auto-resolve a credential mismatch: `Overwrite`, `NewProfile`, `Discard` |
| `carried_credential_keys` | list | `["mcpOAuth"]` | the keys of Claude Code's credential store a switch copies from the live login onto the incoming account, e.g. `["mcpOAuth", "designOauth"]` to keep a `/design-login` across accounts. Replaces the built-in list, and unlike it also reaches a setup-token sidecar and survives its re-mint. `claudeAiOauth` is refused. Not yet honored inside `clauth start` sessions |
| `theme` | string | auto | `full` or `compatible` |
| `reset_display` | string | `relative` | `relative`, `clock`, `both` |
| `clock_format` | string | `24h` | `24h` or `12h` |
| `home_tab` | string | `overview` | the tab every launch opens on: `overview`, `usage`, `tokens`, `setup`, `fallback`, `config`, `status`, or `plugin`; edited from the Config tab's `home tab` row. the first herdr launch lands on `plugin` with the herdr row open instead |
| `show_estimates` | bool | `true` | burn estimates on the Usage tab |
| `show_pace` | bool | `false` | ideal-pace marker on usage bars |
| `count_cache` | bool | `false` | count cache tokens in the Tokens totals |
| `auth_broken` | list | `[]` | accounts quarantined after a permanent OAuth rejection; clauth writes this |
| `[herdr]` | table | `{}` | the herdr-plugin knobs the Plugin tab edits, plus the first-launch marker ([herdr plugin](Herdr-Plugin)) |
| `[herdr] popup_width` | string | `fit` | `fit` (focused-pane width, 540-column cap), `half` (herdr's default), `split-right`, or `split-top` (a real pane right of or above the focused one); a saved `full` loads as `fit` |
| `[herdr] pane_tag` | bool | `true` | publish the `clauth=$profile` pane-metadata tag; off clears it on every pane |
| `[herdr] tag_watch_secs` | int | `5` | seconds between the per-pane tag watcher's re-publishes |
| `[herdr] border_label` | bool | `false` | publish `--display-agent "$profile"` so split-pane borders name the account; off clears the stale label |
| `[herdr] delegate_dot` | bool | `true` | report `clauth_delegate=working\|idle` pane metadata during delegate runs |
| `[herdr] delegate_row_text` | bool | `false` | append `$clauth_delegate` to the sidebar row `install` writes |
| `[herdr] first_landing_done` | bool | `false` | set to `true` once the first herdr launch lands; later launches open `home_tab` |
| `[serve]` | table | `{}` | the daemon-wide session-creation switch ([Daemon](Daemon)) |
| `[serve] session_creation` | bool | `false` | whether `POST /api/v1/sessions` is served at all; each calling device also needs its own `sessions` grant (`clauth devices allow-sessions <name>`) |

A key clauth does not know (written by a newer release, or added by hand) is kept verbatim across every rewrite, under a `# keys preserved from the previous file` marker. Nothing a newer version of clauth wrote into these files is lost by running an older one beside it.

## `codex-profiles.toml`

The codex roster, kept apart from `profiles.toml` so a codex switch never rewrites the Claude Code file and an older clauth never opens this one. `clauth login <name> --codex` creates it; the threshold and `wrap_off` keys are hand-edited only, there is no tab for it, while `active_profile` moves with `clauth <name>` and the codex auto-switch, and `profiles` and `fallback_chain` with each codex login and delete ([Codex](Codex)).

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `active_profile` | string | none | the codex profile the codex chain anchors on and the Overview marks; `clauth <name>` on a codex name moves it |
| `profiles` | list | `[]` | the codex profiles, in display order |
| `fallback_chain` | list | `[]` | the codex chain, in walk order ([Auto-switch](Auto-Switch#codex)) |
| `wrap_off` | bool | `false` | clear the active slot once every codex member is spent, instead of staying on the last one |
| `weekly_switch_threshold` | float | `98.0` | the codex chain's 7d exhaustion line, 50-100; a value outside the band reads as the default and is rewritten to `98.0` on the next save, and a key the file never carried is not invented into it |

A codex profile's own `config.toml` carries `harness = "codex"` and one optional key of its own, `hooks_json` (below); the Claude Code keys in the next table do not apply to it.

## `config.toml`

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `base_url` | string | none | API endpoint; unset means an OAuth account |
| `api_key` | string | none | key for that endpoint |
| `auto_start` | bool | `false` | the 1-token window-opening ping (alias `kick_timer`) |
| `disabled` | bool | `false` | hide from auto-switch, polling, and the status feed |
| `fallback_threshold` | float | `95.0` | 5h utilization % that switches away from this account |
| `weekly_threshold` | float | chain value | per-account override of the 7d line, 50-100 |
| `check_weekly` | bool | `true` | count the aggregate weekly window against this account |
| `check_scoped` | bool | `true` | count per-model weekly windows against this account |
| `last_resort` | bool | `false` | the chain's parking spot |
| `preferred` | bool | `false` | the home account clauth returns to once it is clear |
| `preferred_days` | string array | `[]` | weekdays this account is home, in local time; claims those days against every account, while `preferred` keeps the days no list claims |
| `max_auto_spend` | float | `0.0` | dollar ceiling on pay-as-you-go fallback |
| `bell_threshold` | float | none | 5h % that fires a bell toast |
| `rolling_token` | bool | `false` | daemon re-stamps the sidecar from the usage chain; set by `clauth rolling-token`, cleared by `clauth static-token` (bare or `--clear`) |
| `[env]` | table | `{}` | extra environment variables merged into `settings.json` while active |
| `[models]` | table | `{}` | `default`, `opus`, `sonnet`, `haiku`, `fable`, `subagent` |
| `[console]` | table | `{}` | Alibaba Model Studio usage session: `token`, `site` (`international` / `domestic`), `region` (`ap-southeast-1` / `cn-beijing`). `clauth login` writes it ([above](Configuration#the-alibaba-console-session)) |
| `hooks_json` | bool | `false` | codex profiles only: link your `~/.codex/hooks.json` into that profile's shared session homes, so those hooks run inside `clauth start` sessions too. Off, the file is left out of every session home ([Codex](Codex#run)) |

`last_resort` and `preferred` are radio toggles across the chain: marking one clears it everywhere else, and no account can be both.

`preferred_days` has a `preferred days` row on the member's Fallback card: <kbd>space</kbd> steps `never` → `weekdays` → `weekends` → `every day` and back to `never` (a custom set jumps straight to `never`), and <kbd>⏎</kbd> opens a day picker for any other set ([Interface and keys](Interface-And-Keys#tab-dependent)). A list matching no preset shows its days. The card's `preferred` row names the days once a list is set, and `+ add` names a list an account still carries from before it left the chain. In a hand-written list, full names and three-letter forms parse in any case (`["sat", "Sunday"]`), and an entry that does not parse is dropped on the next rewrite. The list is re-read per chain build, so the rollover at midnight needs no restart.

A list only claims from an account the chain walk would actually visit, so one on an account that is off the chain, disabled or auth-broken claims nothing: a member's `preferred days` row names a disabled or auth-broken account, `+ add` names one that is off the chain, and saving such a list warns (once per day-picker visit). A list can also go inert later, or arrive by hand-editing the file, so clauth says the same at run time: once a day, naming the account, the reason, and whether another list carried the day or it fell back to `preferred`.

**A named day is claimed against every account.** On a day some list names, only the accounts naming it are home; a bare `preferred = true` elsewhere stands down for that day and takes charge again on the days no list claims. So the usual split is one line in one profile:

```toml
# ~/.clauth/profiles/personal/config.toml — work keeps plain `preferred = true`
preferred_days = ["sat", "sun"]
```

Two accounts naming the same day is not rejected: the chain returns to whichever of them reads clear first, so nothing is left with nobody home. clauth still says so once — a toast in the TUI, a line in the log — and says it again at the midnight rollover or after an edit, never once per tick.

## Storage layout

```
~/.clauth/
  profiles.toml            # everything in the table above
  codex-profiles.toml      # the codex roster: active marker, chain, wrap_off, weekly line
  ai_pricelog_v4_price_cache.json  # ai-pricelog model prices for the cost lens
  status_cache.json        # status.claude.com incident feed
  status.json              # the daemon's published snapshot (see Daemon)
  devices.json             # devices paired with the REST API: name, tier, sessions grant, a SHA-256 of each token (0600)
  pairing.json             # the waiting pairing code's SHA-256 while `clauth devices pair` runs (0600)
  tls.json                 # REST API certificate directory, written on the first `--listen` start
  session_profiles.json    # which account each Claude Code session ran on
  token_ledger.json        # the per-day token ledger behind the Tokens tab
  clauth.log, daemon.log   # event lines from the TUI and the daemon
  clauthd.pid              # the running daemon's process id
  completions/             # generated shell completion scripts
  .completions_installed   # marker: completions have been installed
  conversations/<sid>[.<agent_id>].json  # the account a live conversation is on
  jobs/<id>.json           # backgrounded delegate jobs, GC'd after a day
  live_bare/<pid>          # one marker per live bare `claude` session
  live_sessions/<sid>.json # one row per live `clauth start` session
  presets/<name>.json      # endpoint + model presets you saved
  rotation-locks/<name>.lock  # one OAuth-rotation lock per account
  keychain-quarantine/     # macOS: raw bytes of a corrupted Keychain item, saved before clauth overwrites or deletes it
  keychain-item-owners.json # which per-session Keychain items clauth seeded; the census deletes nothing else
  profiles/
    work/
      config.toml          # everything in the table above
      credentials.json     # OAuth snapshot (.pending while a rotation is mid-write)
      mcp-logins.json      # MCP-server logins parked while this profile stores no Claude login
      session-token.json   # long-lived setup-token login, when captured
      session-token.static.json # the mint a rolling token superseded, kept for the restore
      usage_cache.json     # last-known utilization and plan
      usage_history.jsonl  # 2 days of samples, feeding burn-aware switching
      wallet_history.jsonl # 2 days of balance readings, feeding the wallet-burn rate
      third_party_cache.json
      third_party_auth.json# set while the usage login is expired; a hash, never the credential
      account_id.json      # which account this is, so a re-login can be told apart
      profile_fetched.json # when the plan tier was last read
      kick_block.json      # messages-limiter block state
      throughput_cache.json# observed delegate tokens/sec per model
      touch-receipt.json   # what the last credential swap wrote, for the watchdog
      quarantine/          # credentials parked after a refresh token was rejected
      runtime-<sid>/       # one CLAUDE_CONFIG_DIR tree per live session
      runtime-isolated-<sid>/
      sessions-<sid>/      # that session's PID file, flock-held while it runs
    cx/                    # a codex profile (see Codex)
      config.toml          # harness = "codex", plus hooks_json
      auth.json            # the ChatGPT token chain; ~/.codex/auth.json links here after a capture
      auth.lkg.json        # last-known-good copy of auth.json
      auth.attempt         # no-replay memo: a fingerprint of the refresh token last sent
      auth.quarantine.json # the verdict that killed the chain, while its token is still in the store
      usage_cache.json     # last usage reading and plan
      codex-home/          # the durable store: sessions/, archived_sessions/, history.jsonl, the sqlite state stores
      codex-home-<sid>/    # one live session's CODEX_HOME, removed at exit
      codex-home-isolated-<sid>/
      sessions-<sid>/      # that session's PID file, flock-held while it runs
      sessions-isolated-<sid>/

~/.local/share/clauth/     # macOS ~/Library/Application Support/, Windows %APPDATA%
  current@claude           # points at the version dir Claude Code registers
  versions/<ver>-<hash>@claude/  # the bundled plugin: plugin.json, hooks/, marketplace.json
  markers/<hash>           # the install record `clauth self-heal` keys on
```

Five static lock files sit alongside and are never deleted on purpose: `.lock`, `clauthd.lock`, `clauthd-standby.lock`, `usage-fetch.lock`, `conversations/.lock`. That is the whole tree: every path clauth writes is listed above, so a file you find here that is not is a leftover from an older version. Everything under `~/.clauth` is `0600`, every directory `0700`, re-tightened on each launch. The plugin tree is not: it carries no credentials and lands at your umask.

Deleting any `*_cache.json`, `third_party_auth.json`, or `status.json` costs you history and nothing else. Deleting `usage_history.jsonl` costs burn-aware switching its samples and the queue its anchor, so the queue re-spaces from scratch over the next cycle. Deleting `wallet_history.jsonl` costs the wallet-burn rate its series; the rate rebuilds from the next day of fetches. Deleting `credentials.json` or `session-token.json` signs that profile out. Deleting a codex profile's `auth.json` signs it out too, and your own codex with it when `~/.codex/auth.json` links there.

The `-<sid>` suffix appears on every isolated session, and on a shared one wherever the OS grants symlinks. Where it does not (a home on exFAT, FAT32 or SMB, or Windows without the symlink privilege) clauth builds a shared runtime tree by copying `~/.claude/`, so every shared session of one profile lands on a single unsuffixed `runtime/` instead of paying for a copy each. An isolated session copies nothing from `~/.claude/`, so it keeps its own suffixed tree there too and its transcripts are rescued on its own exit rather than the last one out. A codex session home follows the same rule with both flavors collapsing: `codex-home-<sid>` and `codex-home-isolated-<sid>` where symlinks work, the bare `codex-home` (the store itself) and `codex-home-isolated` where they do not ([Codex](Codex#windows-and-hosts-without-symlinks)).
