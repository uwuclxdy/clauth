# Auto-switch

An ordered chain of accounts clauth hops down when the active one runs out of headroom. Opt-in: an account outside the chain is never switched to or away from, and an empty chain means clauth never switches on its own.

Edit the chain on the Fallback tab, or as `fallback_chain` in `profiles.toml`.

## The decision

On every scheduler tick, and once at startup:

1. The active account has to be a chain member. Nothing happens otherwise.
2. It has to be exhausted or dead.
3. clauth walks the chain from the slot after it, wrapping, and switches to the first member with headroom.

The walk prefers members whose usage was read live over ones showing cached numbers, and falls through to accept a stale-reading member rather than strand you on an exhausted account.

The active account's own exhaustion is judged only on fresh readings, so a rate-limited poll cannot trigger a switch by itself. Two states are the exception, because neither can ever report a fresh reading again: a dead account switches away on any reading, and an account whose usage polls have been rate-limited long enough to stop draining switches away too, once its last-known numbers say it is genuinely spent.

## Exhausted

An account is exhausted when either window is past its line.

| Window | Line | Set by |
|--------|------|--------|
| 5h | `95%` | per account, Fallback tab `rotate at` / `fallback_threshold` |
| 7d aggregate | `98%` | Config tab `weekly limit`, per account `weekly at` / `weekly_threshold` |
| 7d, per model | same as aggregate | the same values, checked per model window |
| 7d hard cap | `100%` | not configurable |

The weekly lines are deliberately below 100. Topping out a week bricks an account for days rather than hours, so clauth moves off while there is still room to land the hop. The 100% hard cap blocks an account regardless of every toggle below.

Per-model weekly windows (a "7d fable" window, say) gate the same way: an account whose scoped week is past the line stays out of rotation, since a session of the capped model landed there would strand, and the walk cannot know which model your next session runs. A session that has not started yet is the one case where the model *is* knowable, which is what [`start --auto`](Auto-Switch#choosing-where-a-session-starts) uses.

Two per-account toggles relax this:

- **`weekly gate`** off: ignore the soft weekly line for this account. The hard cap still blocks.
- **`scoped gate`** off: keep rotating to this account for other models, ignoring its capped per-model weeks. Blunt by nature — it drops the gate for every model, so a session of the capped model can then land here too. `start --auto` narrows the same judgment to the models a session will actually run, and needs no toggle.

## Excluded members

The walk skips a member for any of these, worst first. The Overview and Fallback tabs render the reason on the account's row.

| Reason | Meaning |
|--------|---------|
| `disabled` | you ran `clauth disable <name>`, or flipped it on the Setup tab |
| `canceled` | the subscription reads canceled at Anthropic |
| `auth broken` | a refresh was rejected for good; the login needs `clauth login <name>` |
| `weekly spent` | 7d at 100%, dead until the week resets |
| `claude code blocked` | the messages limiter keeps refusing this account, twice running, with quota still ahead |
| `extra usage spent` | out of subscription quota and out of the spend ceiling below |
| `5h <pct>%` | 5h past its line |
| `weekly <pct>%` / `<model> <pct>%` | past a weekly line, per the gates above |

Being dead is its own switch trigger. An active account marked `auth broken`, `canceled`, or `claude code blocked` can never report fresh usage again, so clauth walks off it instead of wedging on the corpse.

## Last resort and preferred

Two radio toggles on a member's Fallback card. Marking one clears it on every other member, and no member can hold both.

- **`last resort`** is the parking spot: chosen only once every other member is past its line, and never switched away from. Claude Code then surfaces its own out-of-limit message when that account runs dry too.
- **`preferred`** is the home account. Once it reads clear and fresh, clauth walks back to it on its own, from wherever the chain left you.

## When everyone is out

With no `last resort` member, the chain-global `quota spent` setting decides:

- `stay on active` (default) keeps you on the last account, so Claude Code shows its own limit message.
- `switch off all` clears the live credentials and unsets the active account. It re-arms automatically onto the first member that recovers.

## Burn-aware switching

Config tab `switch mode`, default `static`.

`static` compares utilization to the threshold. `burn-aware` instead projects utilization forward from your recent burn rate and switches once the projection would cross 100% before the next refresh. Heavy burn moves you early; light burn rides past the threshold toward 100%.

| Knob | Default | Effect |
|------|---------|--------|
| `burn floor` | `98%` | the earliest utilization burn-aware may switch at |
| `burn horizon` | `60s` | how far ahead it projects, capped by the refresh interval |

The rate is a recency-weighted fit over the last hour of samples in `usage_history.jsonl`, ignoring idle gaps and reset boundaries. It needs three samples to compute anything; an account without them falls back to its static threshold. Burn-aware can only switch earlier than static, never later.

## Extra usage (real money)

Off by default, and three separate things must all be true before a dollar is spent:

1. Config tab `allow extra usage` is `pay-as-you-go`.
2. That account's `max spend` ceiling is above $0.
3. Billing is actually enabled on the account at Anthropic.

An account with subscription quota left always outranks one that costs money, and a `last resort` member that still serves for free outranks a paying one. clauth arms against 90% of the smaller of your ceiling and the account's own limit, leaving margin for the gap between polls. An account whose spend it cannot read is never armed.

The ceiling is a real stop, not just a gate on starting: once the account has spent it, clauth stops using that account. It parks on your `last resort` member if you named one; otherwise the `extra usage spent` setting decides, defaulting to switching off all accounts. That default is the opposite of `quota spent` on purpose, since staying put costs nothing when quota runs out and costs money when a budget does.

An armed account with `extra usage spent` set to `stay on active` and no `last resort` member in the chain can spend without a stop. clauth marks that on the card and the daemon warns about it at boot.

## Keeping the chain warm

An account's 5h window opens on its first real request, so a chain member you have not touched starts its clock only when the chain lands on it — and then holds you there for a full five hours before it resets. `auto_start` ([Configuration](Configuration#auto-start-the-5-hour-window)) opens that window ahead of time with a one-token ping, and the shared queue ([Configuration](Configuration#interleaving-it-across-accounts)) spaces those opens `5h / N` apart across the accounts that opted in, so whichever member the chain hops to has usually been cycling already and resets sooner. Both are independent of the chain itself: auto-start never switches anything, and the walk never consults it.

## Running it

The chain runs wherever the decision loop runs: an open TUI, or `clauth daemon` with the TUI closed ([Daemon](Daemon)). Only one of them decides at a time.

`clauth start <profile> --with-fallback` gives a single session its own chain, so that session hops accounts while your global one stays put. It needs a running daemon and an OAuth account inside a chain that holds a second member to move to, and it does not work on macOS or alongside `--isolated` ([Quickstart](Quickstart#rules-worth-knowing)).

## Choosing where a session starts

The chain decides where a session *moves*. `clauth start --auto` decides where one **starts**, and it is the only place in clauth that knows which models a session is about to run.

It weighs the chain and launches on the best member that can serve every model family the session may run, printing the choice and the numbers behind it. `--explain` prints that and exits without launching.

**It is the union of families, never the headline model.** A `Task` subagent runs inside the parent's process and spends the parent's account on whatever model it runs, so choosing for the main thread alone would strand the session the moment a subagent used a capped family. The union comes from your `settings.json` model, `CLAUDE_CODE_SUBAGENT_MODEL`, and any `--model` you pass. When none of those resolves, the demand is empty and the blanket `scoped gate` above applies unchanged — precision only where there is information.

**Feasibility is runway, not utilization.** A member with minutes left strands one turn in and the chain then swaps it, which costs the whole context re-read: cache entries do not cross accounts. So headroom is divided by the same burn rate [burn-aware switching](Auto-Switch#burn-aware-switching) fits, and the result is minutes. No burn samples means unbounded, never zero — an idle account has none *because* it is idle. A member whose binding window resets within the grace is feasible however thin, since a stall until the reset is not a strand.

**Ranking.** Feasible first, then the `last_resort` member last, then a member clauth has numbers for over one it does not, then fewest live sessions, then most runway, then `preferred`. Live sessions outrank runway deliberately: launching several sessions at once would otherwise send them all to the same member, and usage polling cannot see the launches until its next refresh.

When nothing is feasible it launches on the best of a bad set and says so, rather than refusing. The candidate set is the fallback chain — the accounts you have already said may be entered unattended — so an empty chain refuses and names the fix.

This never moves a running session. `--with-fallback` remains the only thing that does, and the two compose: pick the entry point, then let the chain rescue it if that account runs out.

The floor and the grace are `selection_min_runway_mins` and `selection_reset_grace_mins` ([Configuration](Configuration#profilestoml)).

## Mixing account types

A chain holding both OAuth and API-key accounts raises a confirm before it is saved. Switching away from an API-key member does not unset the environment variables a running bare `claude` already read, so that session can keep using the old endpoint until it restarts. `clauth start` sessions are unaffected.
