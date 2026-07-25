# Fork ↔ upstream sync contract

The fork (xingfanxia/clauth, branch `main`) tracks upstream (uwuclxdy/clauth,
branch `mommy`) by **periodic true git merges** — never squash-rebases, never
cherry-pick-only sweeps. Adopted 2026-07-20 (UPS-2, merge `c112469`); the
pre-history is one 0.12.0 squash baseline, so `git log --first-parent main`
reads as the fork's own timeline.

Why merges: history and ledger-cited hashes survive; PR #51's head IS this
branch, so a merge updates the PR without a force-push; each sync pays only the
incremental conflict cost. A squash-rebase re-pays the whole fork delta every
time and invalidates every hash `.agent/PROGRESS.md` and memory cite.

## Doing a sync

1. `git fetch upstream && git log --oneline $(git merge-base main upstream/mommy)..upstream/mommy`
   — read the delta first; know what's landing.
2. Branch: `git checkout -b sync/upstream-<date>`; merge: `git merge upstream/mommy`.
3. Resolve by the principles below. `cargo test` + `cargo clippy --all-targets`
   + `cargo fmt --check` must be green before the merge commit concludes.
4. Fast-forward `main` to the sync branch, deploy (daemon + proxy restart),
   push. PR #51 picks the merge up automatically.
5. Ledger the sync in `.agent/PROGRESS.md` (UPS-N) and update the fork-delta
   inventory below if it changed.

## Resolution principles

1. **Divergence-reduction**: where both sides implement the same idea, take
   upstream's shape and re-express the fork delta on top of it. Every line the
   fork doesn't need to own is a line the next sync doesn't conflict on.
2. **Fork-only subsystems survive with behavior intact** (inventory below).
3. **Upstream-only features are adopted wholesale** — including their config
   and TUI surfaces — then gated through fork axes where an axis matters
   (e.g. settings sync skips codex-harness profiles).
4. **Hard-cap rule (the PR #55 bug class)**: the fork's `is_exhausted` /
   `weekly_blocked` FOLD per-member overrides; upstream's don't. Any upstream
   site judging the literal 100% cap through a folding predicate must be
   converted to `is_exhausted_hard` / `weekly_hard_blocked` / an explicit-line
   non-folding twin. Sweep for it on every sync:
   `grep -n 'is_exhausted(\|weekly_blocked(' src/ | grep WEEKLY_HARD_BLOCK_PCT`.
5. **Wire compatibility beats field names**: Rust fields follow upstream
   renames (`wrap_off` → `switch_off_when_spent`), but status.json keys and
   on-disk spellings ccsbar/ccu read stay stable (serde rename / literal key).
6. **Merge-both test hunks truncate**: when a `both` resolution concatenates
   two suites, the first side's last function often loses its tail before the
   second side's header. Every "unclosed delimiter" in the compile loop is
   this; restore the tail from `git show HEAD^1:<path>`.

## Fork-delta inventory (what upstream does not have)

- **Codex engine** (CDX-1..6): harness axis on `Profile`, isolated
  CODEX_HOME starts + lease/adopt-back runtime, standby OAuth refresh,
  codex fallback chain + session-boundary walk, passive JSONL usage reader,
  localhost injection proxy (`src/proxy/*`, advisory-rank two-tier selection),
  `clauth resume <codex-profile>` carryover (dispatch-shared with upstream's
  session resume), codex TUI rungs/tokens dashboard/route column, CDX-6
  read-only `wham/usage` polling per profile (60s, parked accounts included;
  AX reversal 2026-07-22, kill switch `codex_usage_poll`), and the
  `enforce_clauth_perms` codex-home exemption (the sweep tightens the
  `codex-home/` dir node to 0700 but does not DESCEND, or it strips the exec
  bit off codex's PATH-alias helper binaries under a live isolated session —
  `auth.json`'s 0600 comes from `atomic_write_600` at seed, not from the
  sweep). Upstream's `docs/codex-plan.md` phase 3 carries the same exemption,
  so this one reconciles rather than persists.
- **Scheduler hardening**: SCW-1 per-model scoped weekly windows in both
  walks, SCW-2 per-member gates + `weekly at` override (folded into
  `ChainMember.weekly_line/scoped_line/check_scoped`), RLS-1 stuck-rate-limit
  distrust, per-harness pending switch queue (`VecDeque<PendingSwitchEntry>`),
  recovery scan scoped/kick gating.
- **Daemon surface**: status.json fork fields (`forecast`, `burn_aware`,
  `weekly_switch_threshold`, `last_error`), tokens.json feed, per-member
  gate/override socket commands, ccsbar/ccu client contracts.
- **Claude-side**: macOS Keychain-first link ordering, RESCUE-1
  dead-live-login reclaim, CLA-SPLIT hardening on top of merged #53
  (genuinely-long-lived engagement gate, force-snapshot guard), auth-broken
  quarantine surfaces, `--new` / `--codex` / `--browser` login flags.
  **NOT browser OAuth login itself** — upstream has that (`src/oauth_login.rs`
  on `mommy`, full inline PKCE + loopback). The fork's only delta there is the
  CDX-3 R4 extraction of the shared mechanics into `src/loopback.rs` so codex's
  login can reuse them, so it rides along with the codex series rather than
  being upstreamable on its own. (Corrected 2026-07-25 — this bullet used to
  claim the feature; measure with `git grep` against `upstream/mommy` before
  trusting any line in this inventory.)
- **CLA-FEED session-token feed** (`docs/cla-feed/DESIGN.md`): per-profile
  `session_feed` flag; the daemon re-stamps `session-token.json` from the
  usage chain's access token on every rotation (full scopes +
  `subscriptionType`, no refresh token → plan-gated models work in sessions
  while the refresh chain stays clauth-private); switch-in gate re-feeds or
  arms (`ensure_installable` feed branches), terminal chain death restores
  the preserved static mint (`session-token.static.json`); `clauth feed
  <p> on|off`; status.json additive `session_feed` key; scheduler
  proactive-rotation feed override; EXP-2 re-feed timer (`claude_feed_tick`
  5-min scan + `refeed_session_token` with `fresh_horizon_ms` threaded
  through the feed gates — switch paths keep the 60s grace; active-profile
  Keychain mirror).
- **EXP-2 codex 401 kick**: CDX-6 poll `Unauthorized` →
  `codex_auth_kicks` → CDX-3 standby force-refresh
  (`codex_refresh_parked(force)` bypasses only `standby_due`), with a
  2-strike kick-streak breaker in `CodexPollPacing`.
- **Sessions/settings gating**: codex-harness profiles are invisible to
  upstream's settings sync and claude session machinery.

## Contributing back

Contribution branches are cut from `upstream/mommy`, never from fork `main`
(`feat/scoped-weekly-walk` = PR #55 is the template): port the feature onto
upstream's shape, let the fork adopt the upstream form back on the next sync.
The fork's standing upstream threads live in `.agent/PROGRESS.md`.

**The codex engine's endgame (UPS-7, 2026-07-25):** upstream owns the codex
design now — `docs/codex-plan.md` on `mommy` is the spec, and we implement it
as a six-part series on a branch cut from the **v0.14 tag** (branch cut from a
TAG, not `upstream/mommy` — the one deliberate exception to the rule above).
It is a state-layer rewrite, not a port: harness moves out of
`Profile`/`AppState` into a separate `codex-profiles.toml`, dirs gain a `-cx`
suffix, and the store-mode refusal becomes a forced
`-c cli_auth_credentials_store="file"`. When that series lands, the "Codex
engine" bullet above collapses to whatever the fork still adds on top —
budget for the inventory to shrink, not grow.
