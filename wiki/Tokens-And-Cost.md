# Tokens and cost

The Tokens tab is a dashboard over Claude Code's own token history on this machine: per-model totals, a today panel, daily peak, busiest hour, and charts that grow with the terminal.

## Where the numbers come from

| Source | Gives |
|--------|-------|
| `~/.claude/stats-cache.json` | Claude Code's own lifetime rollup |
| `~/.claude/projects/**/*.jsonl` | live session transcripts newer than that rollup |
| `~/.clauth/token_ledger.json` | clauth's durable per-day record |

Claude Code prunes old transcripts and its rollup freezes at a date, so clauth keeps its own ledger of finalized days. That ledger is what lets the dashboard keep advancing once the transcripts behind it are gone. Days already pruned before the ledger existed are unrecoverable.

The figures cover **every account sharing this machine's home directory**, since that is what Claude Code's store covers. A `clauth start --isolated` session writes into its own throwaway store, so its usage arrives here only once the run ends and its transcripts are lifted into the global store.

## Period lens

<kbd>t</kbd> cycles the lens: lifetime, today, this week (from Monday), this month (from the 1st). It re-scopes the dashboard cards and the per-model breakdown.

Older days from Claude Code's rollup carry a combined in/out total with no cache split. A period that reaches back into those days shows a floor rather than an exact figure, marked with a badge, and cost renders as `$X+`.

## Cost

The cost figure is what your recorded usage **would cost on the pay-as-you-go API**. Nobody is billing you that: it is the value of what a subscription covered.

It is computed per model, never off a blended rate, and it prices the four token classes separately: input, output, cache reads, cache writes. The <kbd>c</kbd> toggle changes whether cache tokens count toward the token *totals*; cost always counts them.

Prices come from the ai-pricelog public index. clauth fetches it daily. It caches the index at `~/.clauth/ai_pricelog_price_cache.json`. clauth loads the cache first. The tab paints instantly. It works offline. A model with no matching rate contributes nothing to cost. It renders as a faint dash. It puts the surrounding totals on a `$X+` floor. A first launch whose fetch fails before any rates are cached reads `rates unavailable` on the cost figure.

Rates are dated snapshots, and a feed-carried peak/off-peak window prices each recorded hour at the rate live at that date and hour. Hours only exist from the hourly ledger onward, so past days recorded before it price flat at their day's hour-0 tier. The days from Claude Code's own rollup keep that flat rate permanently; ledger days get their hours backfilled once from the stored transcripts (visible one refresh later; a day the transcripts no longer fully cover keeps the flat rate). The lifetime card prices everything at today's rate.

## Model grouping

A model past a million lifetime tokens gets its own row. Smaller non-Anthropic models fold into an `others` row. The <kbd>a</kbd> menu narrows the bars and the breakdown to Claude models only, or to everything else.

## Status feed

The Status tab is separate from all of this: it polls `https://status.claude.com/api/v2/incidents.json` every five minutes for incidents, their severity, affected components, and update timeline, cached at `~/.clauth/status_cache.json`. <kbd>⏎</kbd> opens an incident's timeline, and the action menu opens it in a browser.

## Sessions

`clauth sessions` inventories every Claude Code session on this machine, newest first: the global store plus any live isolated runtime's own store. A session's id is its transcript filename, which stays stable across resumes.

```bash
clauth sessions              # table
clauth sessions --json       # stable field set, tokens and cost null
clauth sessions --tokens     # parse every transcript for tokens and cost
clauth info latest           # resume command, workspace, storage path
clauth resume latest         # pick a profile, then resume
```

`--tokens` reads every transcript in full, so it is slow on a large store and off by default.

Message previews in the listing are scrubbed before they render: API keys, GitHub and Slack tokens, JWTs, bearer headers, URL passwords, anything under a `token` / `secret` / `password` / `api_key` key, plus long high-entropy runs, all become `[REDACTED]`. The redaction is render-time only. The transcript files are never modified. Session ids and workspace paths are left intact.
