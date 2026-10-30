# Tokens and cost

The Tokens tab is a dashboard over Claude Code's own token history on this machine: per-model totals, a today panel, daily peak, busiest hour, and charts that grow with the terminal.

## Where the numbers come from

| Source | Gives |
|--------|-------|
| `~/.claude/stats-cache.json` | Claude Code's own lifetime rollup |
| `~/.claude/projects/**/*.jsonl` | live session transcripts newer than that rollup |
| `~/.clauth/token_ledger.json` | clauth's durable per-day record |

Claude Code prunes old transcripts and its rollup freezes at a date, so clauth keeps its own ledger of finalized days. That ledger is what lets the dashboard keep advancing once the transcripts behind it are gone. Days already pruned before the ledger existed are unrecoverable. Transcript model ids containing `<` or `>` do not contribute token totals or model rows, but their messages still count; previously recorded placeholder model rows are dropped when the ledger loads.

DeepSeek's endpoint answers a Claude model id with its own model and names only the id you asked for in the transcript. clauth recognizes DeepSeek's replies by their message ids and counts each under the model that served it: `deepseek-v4-pro` for an opus request, `deepseek-flash` for any other Claude id. Ledger days recorded before clauth did this are re-keyed once, on the first refresh after the upgrade, wherever the transcripts still hold all of that day's Claude, `deepseek-v4-pro` and `deepseek-flash` usage; any other day keeps its recorded rows.

A server-side advisor call (Claude Code's advisor tool, enabled by `--advisor` or the `advisorModel` setting) is not part of a transcript line's own usage: it rides in that line's `usage.iterations` list, on the advisor's own model. clauth counts each advisor call under that model, at that model's rate, standard unless its own entry records a speed (the line's speed does not carry over), on the same day and hour as the line, so an `opus` advisor beside a `sonnet` session adds `opus` tokens and cost without touching the `sonnet` row. A call counts once however many times the line is repeated in a transcript or carried into another one, and it adds nothing to the message, session or tool-call counts. A model clauth has no rate for renders unpriced like any other. Days already finalized in `~/.clauth/token_ledger.json` keep their recorded totals: updating does not re-derive them, so advisor spend appears from the days clauth records after the update, today included.

The figures cover **every account sharing this machine's home directory**, since that is what Claude Code's store covers. A `clauth start --isolated` session writes into its own throwaway store, so its usage arrives here only once the run ends and its transcripts are lifted into the global store.

The tab is Claude Code only. A codex session ([Codex](Codex)) writes no transcript into Claude Code's store, so its spend is absent from every figure here rather than folded in silently; a codex account's usage shows as its 5h and 7d percentages on the Overview, the Usage tab and in `status.json`, and nowhere on this tab.

## Token speed

The token-speed figures are a second, live read off the same transcripts, produced by a different process than the token counts: the instance holding the fetch lease ([FAQ](FAQ)) tails `~/.claude/projects/**/*.jsonl` and the isolated runtimes' own stores every tick and publishes the result to `~/.clauth/tps_cache.json`. With no daemon that producer is the TUI; with no producer at all (no daemon and no TUI) the TUI and `which --json` show no speed figure, while `clauth status --json` still prints the last cache with its `age_secs`. Two quantities, never merged: **speed** (effective, `t/s`) is one response's output tokens ÷ its generation span — the user line before the response to its last line, so a tool call whose input streams for seconds to minutes after the thinking counts in full, time-to-first-token and thinking tokens included; **rate** (consumption, `tok/s`) is the output tokens generated inside a wall-clock window ÷ the window's length, each response's tokens spread evenly over its own span. Windows: `live` = the last 10 s, `5m`, `1h`, and `today` (since 00:00 UTC); past days come from the producer's own day sums until they fold into the speed ledger. Figures read `0` at zero, one decimal below 10 and whole from 10; the Overview `t/s` cell, the header's tok/s chip and the gauge's speed suffix abbreviate past 9999 to whole thousands (`12k`, at most `999k`). A cache older than 30 s counts as no producer: a figure then reads `—`, a fresh zero reads `0`, and an Overview `t/s` cell or the gauge's speed suffix with no figure stays blank. Attribution: a session counts under the account it ran on, following swaps for live figures; an unattributed session counts toward the totals only. Speed history lives in its own ledger, `~/.clauth/token_speed.json`, written by the producer alone, apart from `token_ledger.json` so an older clauth saving that file never drops speed rows; a day no producer ran has no speed rows. The figures are Claude Code transcripts only — a codex session contributes nothing.

Peaks: **peak speed** is the fastest single reply (output ≥ 100 tokens) and **peak rate** the busiest 10 s, each kept per day and all-time. The Tokens today card's speed row ends `· N tok/s peak` — today's busiest 10 s across every session; the TOTAL card's `avg speed` row ends `· N t/s peak · N tok/s peak` — the all-time fastest reply, then the all-time busiest 10 s (shown while a producer runs). The Usage tab's `speed` row ends `peak N t/s` — that account's fastest reply today, `—` when no reply qualifies. A peak that does not fit its row sheds whole.

## Period lens

<kbd>t</kbd> cycles the lens: lifetime, today, this week (from Monday), this month (from the 1st). It re-scopes the dashboard cards and the per-model breakdown.

Older days from Claude Code's rollup carry a combined in/out total with no cache split. A period that reaches back into those days shows a floor rather than an exact figure, marked with a badge, and cost renders as `$X+`.

Some third-party endpoints report usage in an OpenAI-shaped way: the full prompt (cached prefix included) lands in `input`, and cache writes are never reported. clauth detects that shape from the usage rows themselves, subtracts the double-counted prefix, and prices the corrected figure. A model detail marked `cache write not reported` comes from such an endpoint — the write metric is absent there, so it stays 0 rather than being invented.

## Cost

The cost figure is what your recorded usage **would cost on the pay-as-you-go API**. Nobody is billing you that: it is the value of what a subscription covered.

It is computed per model, never off a blended rate, and it prices the four token classes separately: input, output, cache reads, cache writes. The <kbd>c</kbd> toggle changes whether cache tokens count toward the token *totals*; cost always counts them. A model id ending in `-free` or `:free` — a reseller's free variant — prices at zero rather than showing as unpriced.

Prices come from the ai-pricelog public index, fetched daily. clauth keeps a distilled copy of it at `~/.clauth/ai_pricelog_v4_price_cache.json` and loads that before fetching, so the tab paints instantly and works offline. A model with no matching rate contributes nothing to cost: it renders as a faint dash, and the surrounding totals get a `$X+` floor. A first launch whose fetch fails before any rates are cached reads `rates unavailable` on the cost figure. An update that changes how prices are read sets the cached copy aside and the TUI fetches again at once, so that start reads `rates unavailable` while offline. `clauth sessions --tokens` reads the cached copy and never fetches, so its cost stays blank until the TUI has fetched once.

Every token prices at the rate its model charged for the day and hour it was served. A price that depends on something clauth does not track per hour, such as Anthropic's fast mode, OpenAI's batch mode or a large-prompt tier, is not applied.

Rates are dated snapshots, and a feed-carried peak/off-peak window prices each recorded hour at the rate live at that date and hour. The same windows drive the live peak indicator: a `▲` on an Overview row and a `pricing` row on the Usage tab while the account's provider is inside a peak window. The indicator is provider-bound: it follows the provider's own schedule, never which models the profile pins, and an account on an endpoint clauth doesn't recognize shows none. Hours only exist from the hourly ledger onward, so past days recorded before it price flat at their day's hour-0 tier. The days from Claude Code's own rollup keep that flat rate permanently; ledger days get their hours backfilled once from the stored transcripts (visible one refresh later; a day the transcripts no longer fully cover keeps the flat rate). The lifetime card prices everything at today's rate.

## Model grouping

A model past a million lifetime tokens gets its own row. Smaller non-Anthropic models fold into an `others` row. The <kbd>a</kbd> menu narrows the bars and the breakdown to Claude models only, or to everything else.

## Status feed

The Status tab is separate from all of this: it polls `https://status.claude.com/api/v2/incidents.json` every five minutes for incidents, their severity, affected components, and update timeline, cached at `~/.clauth/status_cache.json`. <kbd>⏎</kbd> opens an incident's timeline, and the action menu opens it in a browser.

## Sessions

`clauth sessions` inventories every Claude Code session on this machine, newest first: the global store plus any live isolated runtime's own store. A session's id is its transcript filename, which stays stable across resumes. Codex threads are not listed; they live in the profile's own `codex-home/sessions/` ([Codex](Codex#files)).

`clauth sessions --live` lists only the running sessions clauth launched (`clauth start` sessions, delegates and background sessions), claude and codex alike, read from `~/.clauth/live_sessions/` and Claude Code's per-process records without opening a transcript; a `claude` started outside clauth has no row there and is not listed. Each row carries the live `<pid>-<seq>` id (the one `clauth switch` takes, except for a session on an account's fixed background tree or a codex session), the account (`start → current` once the session moved), harness, age since start (its start date from 30 days on), transcript id and cwd (for a claude session, the dir its Claude Code record names, so a `--worktree` session lists its worktree; for a codex session, or a claude one whose record is missing, the dir it was launched from, even under codex's own `--worktree` or `-C`), `-` where the transcript id or cwd is unknown, and a `delegate` tag on a delegate and a `background` tag on a session on an account's fixed background tree (`-- --bg` without `--with-fallback`; an adopted `--with-fallback` background session carries none); `--json` carries the same rows as `sid`, `start_profile`, `current_profile`, `harness`, `cwd`, `started_at` (an ISO 8601 UTC stamp, `+00:00`), `transcript_id`, `delegate` and `background`, with `null` where the table shows `-`. Nothing running exits 1 with `no live sessions found`; a registry directory clauth cannot list exits 1 with `cannot list live sessions: <dir> is unreadable`. It refuses `--tokens`, which reads every transcript.

```bash
clauth sessions              # table
clauth sessions --json       # stable field set, tokens and cost null
clauth sessions --tokens     # parse every transcript for tokens and cost
clauth sessions --live       # running sessions only, no transcript read
clauth info latest           # resume command, workspace, storage path
clauth resume latest         # pick a profile, then resume
```

`--tokens` reads every transcript in full, so it is slow on a large store and off by default.

`clauth switch <sid> <profile>` moves a running `clauth start` session or delegate by hand, pointing it at another profile through the same registry the fallback chain's decider writes; the session's executor performs the move and the session picks the new account up at its next request, never before it — or the executor refuses it with a logged reason and the session stays put. For a session started with `--with-fallback`, the chain's decider can supersede a manual intent on its next tick. A session on the account's fixed background tree (`clauth start <p> -- --bg`, no `--with-fallback`) is refused: it stays on its account. An adopted `--with-fallback` background session is switchable like any live session; switching a handed-off one that no daemon holds yet fails and records nothing: `session '<sid>' is a background session no clauth daemon holds yet`; switch it again once `clauth daemon` holds it. A codex session is refused too, as `session '<sid>' is a codex session; switch is claude-only`. The `<sid>` here is not the transcript id above: it is the live session's `<pid>-<seq>` id, one row per session under `~/.clauth/live_sessions/`, which `clauth sessions --live` lists (a handed-off one no daemon holds yet is not listed); a sid that names no live session is refused with `no live session '<sid>'`, then ``see `clauth sessions --live` ``, plus ``to switch the global account: `clauth switch <sid>` `` when `<sid>` is an account name.

Message previews in the listing are scrubbed before they render: API keys, GitHub and Slack tokens, JWTs, bearer headers, URL passwords, anything under a `token` / `secret` / `password` / `api_key` key, plus long high-entropy runs, all become `[REDACTED]`. The redaction is render-time only. The transcript files are never modified. Session ids and workspace paths are left intact. A `first_message`/`last_message` preview past 200 chars ends in `...`.
