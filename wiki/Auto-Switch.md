# Auto-switch

An ordered chain of accounts clauth hops down when the active one runs out of headroom. Opt-in: an account outside the chain is never switched to or away from, and an empty chain means clauth never switches on its own.

Edit the chain on the Fallback tab, or as `fallback_chain` in `profiles.toml`.

## The decision

After every usage refresh, and once at startup:

1. The active account has to be a chain member. Nothing happens otherwise.
2. It has to be exhausted or dead.
3. clauth walks the chain from the slot after it, wrapping, and switches to the first member with headroom.

The walk prefers members whose usage was read live over ones showing cached numbers, and falls through to accept a stale-reading member rather than strand you on an exhausted account.

The active account's own exhaustion is judged only on fresh readings, so a rate-limited poll cannot trigger a switch by itself. Dead accounts are the exception: those switch away on any reading.

## Exhausted

An account is exhausted when either window is past its line.

| Window | Line | Set by |
|--------|------|--------|
| 5h | `95%` | per account, Fallback tab `rotate at` / `fallback_threshold` |
| 7d aggregate | `98%` | Config tab `weekly limit`, per account `weekly at` / `weekly_threshold` |
| 7d, per model | same as aggregate | the same values, checked per model window |
| 7d hard cap | `100%` | not configurable |

The weekly lines are deliberately below 100. Topping out a week bricks an account for days rather than hours, so clauth moves off while there is still room to land the hop. The 100% hard cap blocks an account regardless of every toggle below.

Per-model weekly windows (a "7d fable" window, say) gate the same way: an account whose scoped week is past the line stays out of rotation, since a session of the capped model landed there would strand, and the walk cannot know which model your next session runs.

Two per-account toggles relax this:

- **`weekly gate`** off: ignore the soft weekly line for this account. The hard cap still blocks.
- **`scoped gate`** off: keep rotating to this account for other models, ignoring its capped per-model weeks.

## Excluded members

The walk skips a member for any of these, worst first. The Overview and Fallback tabs render the reason on the account's row.

| Reason | Meaning |
|--------|---------|
| `disabled` | you ran `clauth disable <name>`, or flipped it on the Setup tab |
| `canceled` | the subscription reads canceled at Anthropic |
| `auth broken` | a refresh was rejected for good; the login needs `clauth login <name>` |
| `weekly hard cap` | 7d at 100%, dead until the week resets |
| `kick rejected` | the messages limiter keeps refusing this account, twice running, with quota still ahead |
| `budget spent` | out of subscription quota and out of the spend ceiling below |
| `over threshold` | 5h past its line |
| `weekly` / `scoped` | past a weekly line, per the gates above |

Being dead is its own switch trigger. An active account marked `auth broken`, `canceled`, or `kick rejected` can never report fresh usage again, so clauth walks off it instead of wedging on the corpse.

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

An account's 5h window opens on its first real request, so a chain member you have not touched starts its clock only when the chain lands on it — and then holds you there for a full five hours before it resets. `auto_start` ([Configuration](Configuration#auto-start-the-5-hour-window)) opens that window ahead of time with a one-token ping, and `warmup_stagger` spaces those opens `5h / N` apart across the accounts that opted in, so whichever member the chain hops to has usually been cycling already and resets sooner. Both are independent of the chain itself: warm-up never switches anything, and the walk never consults it.

## Running it

The chain runs wherever the decision loop runs: an open TUI, or `clauth daemon` with the TUI closed ([Daemon](Daemon)). Only one of them decides at a time.

`clauth start <profile> --with-fallback` gives a single session its own chain, so that session hops accounts while your global one stays put. It needs a running daemon and an OAuth account inside the chain, and it does not work on macOS or alongside `--isolated` ([Quickstart](Quickstart#rules-worth-knowing)).

## Mixing account types

A chain holding both OAuth and API-key accounts raises a confirm before it is saved. Switching away from an API-key member does not unset the environment variables a running bare `claude` already read, so that session can keep using the old endpoint until it restarts. `clauth start` sessions are unaffected.
