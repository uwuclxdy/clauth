# Claude Code plugin

clauth ships an MCP server that hands your profiles to a live Claude Code session: compare usage across accounts, relink the active one, or hand a whole prompt to another account without spending the window you are in.

## Install

Open the TUI's Plugin tab, move to the `plugin` row, and press <kbd>f</kbd>. Confirming the prompt installs the plugin at user scope through Claude Code's own installer. If a broken registration ever trips it, the plugin repairs itself: its `SessionStart` hook runs `clauth self-heal`, which reinstalls a registered-but-broken install and leaves a deliberate uninstall alone.

Installed through the old `/plugin marketplace add uwuclxdy/clauth` flow? That registration migrates itself: the marketplace it fetched points at a manifest file the repo no longer ships, so `clauth start` re-points it at the locally materialized plugin tree before `claude` launches. That pre-flight is what has to do it: once the stale marketplace stops loading, the plugin loads no hooks, so its own `SessionStart` self-heal never fires. One `clauth start` is the whole migration.

Claude Code launches `clauth mcp` in the background for the session's lifetime. `clauth` has to be on `PATH`, which it is after any standard install.

To wire the server by hand instead, add this to `mcpServers` in `~/.claude.json`:

```json
"clauth": { "type": "stdio", "command": "clauth", "args": ["mcp"] }
```

The TUI's Plugin tab writes exactly that entry for you with <kbd>f</kbd>. The manual route gives you the same four tools, minus the bundled hooks. Without the plugin, a backgrounded `delegate` result has to be collected with `monitor`, and a conversation is never told when the account behind it changes.

## Tools

| Tool | Input | Returns | Cost |
|------|-------|---------|------|
| `profiles` | `names` (optional, case-insensitive), `scope` (`all` default, or `session`) | every profile with cached 5h / 7d percentages, provider, tier, endpoint host, active flag; `scope: "session"` returns the one account this session runs on and how it resolved | none, reads the disk cache |
| `switch_profile` | `name` | relinks the global active profile; the reply says what the switch does to THIS session | none |
| `delegate` | see below | the target account's answer, or a `job_id` | **a real usage window on the target account** |
| `monitor` | `job_ids` (optional list, capped at 256), `wait_secs` (0-3600, default 0; 1500 without progress-notification support), `return_on` (`any` default, or `all`), `cancel` (needs `job_ids`) | with `job_ids`: a backgrounded job's envelope, its running status, or a named reason for an absent id, one result per id; with none: what moved in clauth's state, as soon as it moves, plus the delegates clauth is holding, live runs first | none |

Every reply reads as prose; there is no format parameter.

## Hearing about changes: `since_your_last_call` and `monitor`

clauth's state can move under a live session: the active profile switches, the background scheduler refreshes usage figures, a rotation rewrites the credentials file. Replies that carry live usage (`profiles({scope:"session"})`, `switch_profile`, `delegate`, `monitor`) carry a `since_your_last_call` note too, naming what moved since the last reply that reported one, and only when something did.

`monitor` with no `job_ids` also lists the delegates clauth is holding — live runs first, then the most recently finished, bounded, with a count of anything left out. That is how a session recovers the id of a delegate it interrupted, since the reply carrying the id at the moment of interruption is dropped rather than sent. It then blocks until that same state moves. It polls three things: the configured active profile, that profile's usage cache, and `~/.claude/.credentials.json`. Every read is local disk, so it costs no network, no quota, and runs no background thread. `wait_secs` (0-3600, default 0, capped at 1500 on a client that cannot receive progress notifications) bounds the wait; on timeout it reports no change and how long it waited. A first digest call has nothing to compare against, so it sets the baseline: with no wait it answers at once that the baseline is armed, and with a wait it keeps polling against the baseline it just set, so a quiet window returns unchanged.

A `switch_profile` that runs never carries the digest for its own switch: the reply itself already says what it did, and it refreshes the baseline so the switch is not reported twice. A switch clauth *refuses* changes nothing, so that reply does carry the digest, naming whatever moved before the call.

`profiles({scope:"session"})` is the authority on which account owns the current session. The all-scope roster reads a cache and can lag it.

`profiles` answers for every profile by default, and `names` narrows it to the ones you ask for. Six fields appear only when they have something to say: the live-session flag when a clauth-managed session already owns that profile, then `disabled`, `login expired` and `no api key`, printed together so they read as one group. `disabled` and `no api key` refuse a `delegate` on that account outright; `login expired` refuses one except on an account that runs its own endpoint with its own key, where it reports a stale usage reading instead. Then `subscription canceled`, which is not a refusal: it tells you the account dropped to whatever the free plan allows. Last come the throughput rows, when a model there is degraded or was recently rate-limited. An account whose endpoint sits on your own machine or your own network carries `local endpoint` in its bracket. The word says where the endpoint is, never who pays for it or whether it is your cheapest target. On a 27-profile fleet that reply is just over half the size it would otherwise be, which matters because the model is told to call it at the start of every session.

### When the account behind a conversation changes

A conversation can end up on a different account three ways, and none of them is visible from inside it. You resume it under another profile, which keeps the same conversation and appends to the same transcript. A `clauth switch` lands while a global session is working. A `clauth start --with-fallback` session swaps credentials mid-run. The first case has cost a real session a round of reasoning spent working out which account it was on.

With the plugin installed, clauth says so. A resume reads ``clauth note: session resumed under `DS4`; earlier turns ran under `z.ai`.`` A move under a live session reads ``clauth note: the active profile for this session switched from `kerry` to `cld`; its 5h window is 42% used.`` The 5h figure appears when clauth holds a usage reading for the new account, and is left off when it does not. The note arrives on the next prompt or the next tool call, whichever comes first, and separately for each subagent running at the time. It comes back once more after a compaction, which drops it along with everything else clauth injected.

Nothing is said while the account holds still. The check that decides costs a stat of the credential file plus a quick scan of your profile directory on each tool call, and only reads the accounts themselves when something has moved. clauth stays quiet when it cannot tell which of your accounts the loaded credentials belong to, rather than naming one it is guessing at.

Where the connect brief names a `clauth start` session's runtime directory, it keeps naming the profile you launched on. That is the directory's name and it does not move. The note is what answers which account you are spending.

## `switch_profile` inside a session

`switch_profile` repoints the global `~/.claude` credentials. A session running on those adopts the new account at its next token refresh, mid-task. A `clauth start` session runs against its own profile and is unaffected; the reply says which case your session is in.

To use another account without disturbing the current session, use `delegate`.

## `delegate`

Runs a headless `claude -p` under another profile and returns what it produced.

| Field | Type | Default |
|-------|------|---------|
| `profiles` | array of strings | one name = one target; two or more = fan-out, one row per account |
| `prompt` | string | exactly one of `prompt` / `prompt_file` |
| `prompt_file` | string | exactly one of `prompt` / `prompt_file`; path relative to `cwd` |
| `model` | string | the profile's own default |
| `cwd` | string | the server's working directory |
| `env` | object | none |
| `args` | array | none, appended to the `claude` invocation |
| `idle_secs` | int | `300`, max 3600 |
| `timeout_secs` | int | `idle_secs`, max 3600; see Kill rules |
| `resume` | string | none, a session id |
| `isolated` | bool | `false` |
| `background` | bool | `false` |

**Cost.** A delegate to a subscription account opens a real 5-hour window there. To a pay-as-you-go API-key account it bills real money. To a prepaid plan account it draws down quota you already bought, so it costs nothing extra. A loopback or LAN host is free. Call `profiles` first to pick the account with headroom.

The `cost` figure in a finished delegate's reply comes from the delegated `claude` itself, which prices every run at Anthropic's rates whatever endpoint answered it. So it is the real amount only on a subscription account. On any account whose requests go somewhere else, the reply labels the figure as Anthropic-priced rather than letting it read as the bill. That covers an account with an endpoint host and an account whose endpoint is set only as an `ANTHROPIC_BASE_URL` entry in its `[env]`, which the roster's host column does not show. Where clauth cannot read an account's config at all it says the endpoint is unknown instead of guessing. It never reprices the figure, because the envelope reports the model id that was sent, never the model the endpoint actually served.

**What it sees.** Only the prompt you pass. It has no view of the calling conversation, so the prompt has to carry the whole task.

**Prompt file.** `prompt_file` reads the prompt from a path relative to the delegate's `cwd` instead of passing it inline, so a long reusable prompt costs your context nothing to hand over. It is validated against `cwd` and refused by name when it is absolute, escapes `cwd`, resolves through a symlink outside `cwd`, is not a regular file, is not valid UTF-8, or is over 64 KiB. Give exactly one of `prompt` / `prompt_file`.

**Targets.** `profiles` takes one name or several. One name runs a single delegate, blocking by default; set `background` to get a job id back at once. Two or more fan out one delegate per named account and spend one real usage window per account, waiting for every one and returning one row per account, or one `job_id` per account with `background: true`. Duplicate names (case-insensitive), unknown names, a disabled member, a member clauth has quarantined after its refresh token was rejected, a member with no inference auth (no usable api key and no `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY` in its `[env]`), and an empty list on anything but a `resume` are refused before any spawn. Each refusal names the command that fixes it: `clauth enable <name>` for a disabled account, `clauth login <name>` for a quarantined one (the same sentence `switch_profile` refuses with), and `clauth login <name> --api-key <key>` for a missing key. An account with its own endpoint and a working key is **not** refused for a quarantine, whether or not clauth recognises the provider: the login clauth quarantined feeds usage polling, and the delegate runs on the api key regardless. Every other account still takes that refusal, an endpoint with no key and a key with no endpoint alike: the exemption is for an account that demonstrably routes and authenticates on its own. Separately, an account on a recognised endpoint with no key is refused for the missing key, which is what actually stops it. An endpoint clauth has no provider for is exempt from that one, since it may be a local model needing no key at all.

**Recursion.** Hard-capped at depth 1. A delegated session cannot call `delegate` again.

**Kill rules.** It dies once nothing has arrived for `idle_secs`, and that is the only deadline a normal delegate has. One that keeps producing output is never cut off, no matter how long it runs. `timeout_secs` binds only a run whose `args` pin their own `--output-format`: that turns the event stream off, silence stops meaning anything, and a wall clock becomes the only thing left that can end a hung child. Unset there, it takes `idle_secs`' value. A killed run still returns the text it had written, in `partial_result`, along with `timed_out`, `elapsed_secs`, and a `session_id` when the work is resumable. A run that exits non-zero, or whose output clauth cannot read as an answer, hands back the same two: the account paid for that text either way. Where there is no resume handle the reply says why, so silence never has to be read as one.

**Resume.** Pass a killed run's `session_id` back as `resume` rather than paying for the work again. Leave `profiles` off and it runs on the account clauth recorded for that session; one it cannot attribute is refused by name instead. clauth runs the resume in the workspace that session was recorded in, and refuses a `cwd` that disagrees with it. Both kinds are resumable: a shared delegate's transcript already lives in the global store, and an isolated one's is lifted there before its throwaway runtime is deleted. After a crash, the handle lives in `clauth jobs --json`: a job's record keeps the `session_id` the run had reached, for a day. A crashed isolated run is the exception — its transcript is lifted only when the run returns, so there is nothing left to resume.

**Isolated.** `isolated: true` drops your `CLAUDE.md`, plugins, hooks, skills, subagent types, and every MCP server, keeping the account's auth. What reaches the run is your prompt plus the project `CLAUDE.md` of its `cwd`, and nothing else. That makes it right for blind runs and evals. For ordinary delegated work leave it off: a native Claude Code subagent runs with your context loaded, so the shared default is the one that behaves like a subagent does. It is not a token saving in either direction. Dropping the MCP servers can take the session back under Claude Code's tool-deferral threshold, which then sends every built-in tool schema in full; measured on one machine with five MCP servers configured, an isolated run billed about 15% more input than a shared one, and a machine with no MCP servers configured can measure the opposite.

**Background.** `background: true` returns a `job_id` immediately so the session keeps working, along with that account's headroom and anything that moved in clauth's state since the last reply, exactly as a blocking delegate's answer carries them. A fan-out reply carries headroom for every account it just spent. A fan-out without `background` waits for every account and returns one row per account; `background: true` is still how you get ids to poll. With the plugin installed, a bundled `PostToolUse` hook delivers the result as soon as the job finishes. That covers a job you asked for with `background: true`. It does not cover one clauth created for you because you interrupted a blocking delegate: Claude Code runs that hook only after a tool call that succeeded, so an interrupted call never triggers it. clauth still keeps the run going and saves its result, since the account paid for it either way, and you collect that one yourself: call `monitor` with no `job_ids` to list the delegates it is holding, take the id from there, then collect it. `clauth jobs` shows the same list in the terminal. A fan-out is delivered the same way: the hook waits on every job and prints each finished envelope together. A job still running at the hook's deadline is named, so collect it with `monitor`. Otherwise call `monitor` with each id in `job_ids`: one id returns that job's envelope or running status, several return one result per id (a done envelope, a running status, or a named reason for an absent id). Either way it long-polls up to 3600 s on a client that supports progress notifications. A delegate has no wall clock of its own, so a wait that ends at that ceiling means call again rather than that the run must be over. clauth caps the wait at 1500 s for a client that cannot receive them, because the progress it sends is what keeps a long call from tripping the client's own idle timeout. With several ids it comes back as soon as the first job lands; pass `return_on: "all"` to wait for the slowest instead. To stop a regretted fan-out rather than paying it out to its deadline, pass `cancel: true` alongside the ids: each named job is asked to stop and keeps whatever text it had already written, plus a `session_id` where the run is resumable. The same call then waits a short grace and reports how far each job got, so a second call is usually unnecessary but a job still winding down comes back as `running`. It waits for all of them by default. Any id this server holds no running job for is named rather than reported as running: it may already be finishing, or it may belong to a `clauth mcp` that has since exited. Jobs live in `~/.clauth/jobs/`, and a finished one is kept for a day after it finishes, so a late collect still gets its result even after a reboot.

A check on a job that is still running is worth the turn it costs: it names the account being spent, how long the run has gone, how long ago the delegate last wrote anything, how many seconds are left on the deadline before clauth kills it, and the newest line of the delegate's own answer. It carries that account's headroom where clauth has it cached: a subscription account's 5h/7d windows, or an api-key account's own provider figures. Every figure is dated off its cache and marked `(stale)` once past any refresh cadence. An api-key account whose provider publishes usage windows of its own shows them and nothing else (`pro: 5h 100%, 7d 96%, 30d 13%`); one that publishes none says so before its figure (`no 5h/7d limits; api balance: 3.06 USD`), so the number is never read as a window that will reset. `unknown` appears only where clauth has no figure. A run with no idle deadline says so rather than reading as a missing figure; pinning your own `--output-format` in `args` turns that leg off, because silence then carries no information.

**Pane state.** In herdr, a delegate's pane state reports as a metadata token (`clauth_delegate`) that reads `working` while any delegate runs, sync and background alike, and `idle` once the last one ends. The agent icon in the agents panel is herdr's own: on a pane where herdr's Claude Code integration is active, that icon moves with the pane's own agent, and clauth's report never moves it. The report lands in the pane's metadata beside the account tag instead. Where the state reads: `herdr pane get` and `herdr pane list` print the pane's tokens; the sidebar row `clauth herdr install` writes names the token only with the `delegate_row_text` knob on, which is off by default. The report applies only inside a herdr pane, is dropped without an error when herdr refuses it, and carries a 60 s TTL, so a clauth server that dies mid-delegate leaves a stale `working` for at most that long. Two Claude Code sessions sharing one pane both report under the same name, so one session's `idle` can clear the other's live `working`. A stuck report is killed after two seconds, so it can never fail a delegate. It does cost that wait: a delegate sends one report when it starts and one when it ends, and the MCP server is single-threaded, so a stuck herdr holds up your other tool calls for up to two seconds per report that runs there. For a blocking delegate that is both reports, four seconds in total; a background delegate's end report runs off that thread and holds up nothing.

**Permissions.** A delegate spawns with Claude Code's permission gate armed and nobody to answer it, so a task that writes files fails on a denial rather than doing the work. Pass the permission flag through `args` when the delegate is meant to edit anything, and add `--add-dir` for reads outside `cwd`. Denials come back in a `permission_denials` array, so check that field rather than the prose result.

**Throughput.** clauth records observed tokens/sec per model per profile and flags an account as degraded or recently rate-limited in `profiles`. Subscription throttling is per model and absent from the usage snapshot, so this is the only signal for it.

## What the server tells the model

On connect it sends a short brief: a one-line index of the four tools, what `switch_profile` would do to this specific session, and a roster of your profiles as of session start. A `clauth start` session gets one more note: each entry in its runtime directory mirrors the same-named entry under your real `~/.claude`, so an edit there reaches the global file. On a symlink host it lands directly. On a host that copies the tree (no symlink privilege), clauth's background sync lands it at sync cadence. The session-aware switch note and the runtime-paths note ride `switch_profile`'s and `profiles({scope:"session"})`'s replies too, so a client that never shows the connect brief still sees them.

The roster groups profiles that share a provider, tier and endpoint host onto one line, and leads with the account that has the most window left. Live usage numbers are deliberately left out of that snapshot, since they go stale immediately; only the ordering reflects them. `profiles` is the live read.

Everything specific to one tool rides that tool's own description rather than the brief: what a delegate costs, that it sees only its prompt, the depth cap. It loads when the tool does, and is never stated twice.
