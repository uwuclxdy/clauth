# Tokens and cost

The Tokens tab is a dashboard over Claude Code's own token history on this machine: per-model totals, a today panel, daily peak, busiest hour, and charts that grow with the terminal.

## Where the numbers come from

| Source | Gives |
|--------|-------|
| `~/.claude/stats-cache.json` | Claude Code's own lifetime rollup |
| `~/.claude/projects/**/*.jsonl` | live session transcripts newer than that rollup |
| `~/.clauth/token_ledger.json` | clauth's durable per-day record |

Claude Code prunes old transcripts and its rollup freezes at a date, so clauth keeps its own ledger of finalized days. That ledger is what lets the dashboard keep advancing once the transcripts behind it are gone. Days already pruned before the ledger existed are unrecoverable. Transcript model ids containing `<` or `>` do not contribute token totals or model rows, but their messages still count; previously recorded placeholder model rows are dropped when the ledger loads.

The figures cover **every account sharing this machine's home directory**, since that is what Claude Code's store covers. A `clauth start --isolated` session writes into its own throwaway store, so its usage arrives here only once the run ends and its transcripts are lifted into the global store.

The tab is Claude Code only. A codex session ([Codex](Codex)) writes no transcript into Claude Code's store, so its spend is absent from every figure here rather than folded in silently; a codex account's usage shows as its 5h and 7d percentages on the Overview and in `status.json`, and nowhere on this tab.

## Period lens

<kbd>t</kbd> cycles the lens: lifetime, today, this week (from Monday), this month (from the 1st). It re-scopes the dashboard cards and the per-model breakdown.

Older days from Claude Code's rollup carry a combined in/out total with no cache split. A period that reaches back into those days shows a floor rather than an exact figure, marked with a badge, and cost renders as `$X+`.

Some third-party endpoints report usage in an OpenAI-shaped way: the full prompt (cached prefix included) lands in `input`, and cache writes are never reported. clauth detects that shape from the usage rows themselves, subtracts the double-counted prefix, and prices the corrected figure. A model detail marked `cache write not reported` comes from such an endpoint — the write metric is absent there, so it stays 0 rather than being invented.

## Cost

The cost figure is what your recorded usage **would cost on the pay-as-you-go API**. Nobody is billing you that: it is the value of what a subscription covered.

It is computed per model, never off a blended rate, and it prices the four token classes separately: input, output, cache reads, cache writes. The <kbd>c</kbd> toggle changes whether cache tokens count toward the token *totals*; cost always counts them. A model id ending in `-free` or `:free` — a reseller's free variant — prices at zero rather than showing as unpriced.

Prices come from the ai-pricelog public index, fetched daily. clauth keeps a distilled copy of it at `~/.clauth/ai_pricelog_v4_price_cache.json` and loads that before fetching, so the tab paints instantly and works offline. A model with no matching rate contributes nothing to cost: it renders as a faint dash, and the surrounding totals get a `$X+` floor. A first launch whose fetch fails before any rates are cached reads `rates unavailable` on the cost figure.

Rates are dated snapshots, and a feed-carried peak/off-peak window prices each recorded hour at the rate live at that date and hour. The same windows drive the live peak indicator: a `▲` on an Overview row and a `pricing` row on the Usage tab while the account's provider is inside a peak window. The indicator is provider-bound: it follows the provider's own schedule, never which models the profile pins, and an account on an endpoint clauth doesn't recognize shows none. Hours only exist from the hourly ledger onward, so past days recorded before it price flat at their day's hour-0 tier. The days from Claude Code's own rollup keep that flat rate permanently; ledger days get their hours backfilled once from the stored transcripts (visible one refresh later; a day the transcripts no longer fully cover keeps the flat rate). The lifetime card prices everything at today's rate.

## Model grouping

A model past a million lifetime tokens gets its own row. Smaller non-Anthropic models fold into an `others` row. The <kbd>a</kbd> menu narrows the bars and the breakdown to Claude models only, or to everything else.

## Status feed

The Status tab is separate from all of this: it polls `https://status.claude.com/api/v2/incidents.json` every five minutes for incidents, their severity, affected components, and update timeline, cached at `~/.clauth/status_cache.json`. <kbd>⏎</kbd> opens an incident's timeline, and the action menu opens it in a browser.

## Sessions

`clauth sessions` inventories every Claude Code session on this machine, newest first: the global store plus any live isolated runtime's own store. A session's id is its transcript filename, which stays stable across resumes. Codex threads are not listed; they live in the profile's own `codex-home/sessions/` ([Codex](Codex#files)).

```bash
clauth sessions              # table
clauth sessions --json       # stable field set, tokens and cost null
clauth sessions --tokens     # parse every transcript for tokens and cost
clauth info latest           # resume command, workspace, storage path
clauth resume latest         # pick a profile, then resume
```

`--tokens` reads every transcript in full, so it is slow on a large store and off by default.

`clauth switch <sid> <profile>` moves a running `clauth start` session by hand, pointing it at another profile through the same registry the fallback chain's decider writes; the session's executor performs the move and the session picks the new account up at its next request, never before it — or the executor refuses it with a logged reason and the session stays put. For a session started with `--with-fallback`, the chain's decider can supersede a manual intent on its next tick. The `<sid>` here is not the transcript id above: it is the live session's `<pid>-<seq>` id, one row per session under `~/.clauth/live_sessions/`.

Message previews in the listing are scrubbed before they render: API keys, GitHub and Slack tokens, JWTs, bearer headers, URL passwords, anything under a `token` / `secret` / `password` / `api_key` key, plus long high-entropy runs, all become `[REDACTED]`. The redaction is render-time only. The transcript files are never modified. Session ids and workspace paths are left intact.
