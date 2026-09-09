# CLA-SPLIT — static session tokens beside the usage OAuth pair

Shipped 2026-07-18 (`57cef39`). Upstream: issue uwuclxdy/clauth#52, PR #53.

## The problem this kills

Anthropic OAuth refresh tokens are **single-use**: each refresh rotates the
chain; refreshing with an already-spent token trips server-side reuse
detection, which revokes the whole chain ("login expired: refresh token
revoked or invalid").

With N long-lived Claude Code sessions open across clauth account switches,
each session holds its account's chain in memory, re-reads the credential
store only at token boundaries (~8h), and writes its rotations back into the
ONE live slot. clauth's standby refresher and pre-switch gate rotate the
stored copies of the same chains. Two writers on a single-use chain →
whoever refreshes second kills it for everyone. Observed 2026-07-16..18:
all three profiles serially died within 36h.

## The split

A profile MAY carry `~/.clauth/profiles/<name>/session-token.json` — a
static long-lived login minted by `claude setup-token` (no refresh token,
~1yr expiry), stored in the same `claudeAiOauth` JSON shape:

| File | Holds | Installed into live slot? | Who writes it |
|---|---|---|---|
| `session-token.json` | static ~1yr token (`user:inference` + `user:sessions:claude_code`) | **YES** — what sessions run on | filled by hand (yearly re-mint) |
| `credentials.json` | rotating OAuth pair (full scope incl. `user:profile`) | never (while the sidecar exists) | clauth ONLY (usage polling + its refresher) |

The one code concept is `claude::install_source_path(name)`: session token if
present, else `credentials.json` (profiles without the sidecar are
byte-identical to pre-split). Classify / first-login / link / Keychain /
snapshot / adopt / `clauth start` runtimes all resolve through it. Guards:
snapshot and the runtime watchdog never write the static token over the
usage pair; the rotation-persist path never Keychain-mirrors the usage chain
over an installed token; `ensure_installable` is `Ready` for split profiles
(a broken usage chain must not bench a switchable account) unless the token
itself is clock-dead → `Broken` + re-mint hint.

Why not oat-only (drop the OAuth entirely): setup-tokens 403 the
`/api/oauth/usage` endpoint (scope), verified live — usage-driven
auto-switch would go blind. The OAuth stays, but with exactly one writer.

## Semantics after the split

- **Switch**: installs the session token. Arms **per profile at its next
  switch** (dropping the sidecar beside the ACTIVE profile classifies
  Diverged until then — switch away and back once to arm it).
- **Running sessions**: converge onto the static token at their next token
  boundary (Claude Code re-reads the store on expiry/401) — no restart
  needed; from then on nothing rotates.
- **`auth_broken` now means "usage chain broken"**: sessions keep working;
  `clauth login <name>` restores usage polling. It also still benches the
  profile from scheduler auto-switch scoring (no usage data to score with).
- **Manual `/login` inside a session**: classifies Diverged; a confirmed
  Overwrite captures the fresh OAuth into `credentials.json` (the usage
  side — correct destination) and relink reinstalls the static token.

## Ops runbook

- **Fill**: write `session-token.json` (0600) with
  `{"claudeAiOauth": {"accessToken": "<oat>", "refreshToken": "", "expiresAt": <ms>, "scopes": ["user:inference","user:sessions:claude_code"], "subscriptionType": "max"}}`.
  AX's three tokens live in `~/.claude-fleet/claude_long_live_tokens.env`
  (NOTE: values are quoted — strip quotes) and expire ~2027-06-28.
- **Yearly re-mint**: `claude setup-token` per account → refill the file →
  switch away/back to reinstall. The gate reports `Broken` with a re-mint
  hint once the token is clock-dead.
- **Validation harness**: the herdr run (7/7: switch links live slot →
  real turn → real image turn → usage pair byte-identical → no rotation →
  host creds untouched) is reproducible via
  `scripts/codex-sim/`-style isolation; see `.agent/PROGRESS.md`
  2026-07-18 entry for the check list.

## Follow-up candidates

- `clauth login <name> --setup-token` capture flow (sidecar is hand-filled).
- TUI: show "session token · expires in N days" on split profiles.
