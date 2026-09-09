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
  so this one reconciles rather than persists. **UPS-17 retyped the whole
  engine onto upstream's API layer** — `ProfileName` everywhere a profile name
  flows, `with_state_lock(|held| …)` witnesses, `AccountId`, upstream's
  `TokenFailure` (which carries no `Display`, so every codex log line renders
  `log_detail()` / `text_with_status()`), and upstream's `gc_stale_runtimes`
  family, into which the codex-home sweep is wired as `gc_codex_homes`.
- **Scheduler hardening**: SCW-1 per-model scoped weekly windows in both
  walks, SCW-2 per-member gates + `weekly at` override (folded into
  `ChainMember.weekly_line/scoped_line/check_scoped`), RLS-1 stuck-rate-limit
  distrust, per-harness pending switch queue (`VecDeque<PendingSwitchEntry>`),
  recovery scan scoped/kick gating.
- **Daemon surface**: status.json fork fields (`forecast`, `burn_aware`,
  `weekly_switch_threshold`, `last_error`), tokens.json feed, per-member
  gate/override socket commands, ccsbar/ccu client contracts.
- **Claude-side**: RESCUE-1
  dead-live-login reclaim, CLA-SPLIT hardening on top of merged #53
  (genuinely-long-lived engagement gate, force-snapshot guard), auth-broken
  quarantine surfaces, `--new` / `--codex` / `--browser` login flags.
  **NOT the Keychain write ordering any more** — the fork's "Keychain FIRST,
  then mutate" relink was dropped in UPS-17 for upstream's
  `publish_credential_link` + `keychain_mirror_source(Leave|SignOut)` shape,
  which rebuilt that path around a rename-not-unlink publish and an explicit
  absent-source policy. Same failure the fork's ordering was written for, now
  upstream's to keep correct.
  **NOT browser OAuth login itself** — upstream has that (`src/oauth_login.rs`
  on `mommy`, full inline PKCE + loopback). The fork's only delta there is the
  CDX-3 R4 extraction of the shared mechanics into `src/loopback.rs` so codex's
  login can reuse them, so it rides along with the codex series rather than
  being upstreamable on its own. (Corrected 2026-07-25 — this bullet used to
  claim the feature; measure with `git grep` against `upstream/mommy` before
  trusting any line in this inventory.)
- ~~**CLA-FEED session-token feed**~~ — **GONE from the fork delta (UPS-17,
  2026-09-09).** Contributed as PR #59, MERGED upstream as `rolling-token`
  (upstream commit `7340d44`, seven review rounds), and adopted back wholesale
  by this sync: the fork's `session_feed` flag, `feed_session_token`,
  `arm_feed_from_disk`, `feed_install_gate`, `claude_feed_tick`,
  `MINT_HORIZON_MS` and the `clauth feed` verb are DELETED in favour of
  upstream's `rolling_token` / `stamp_rolling_token` / `arm_rolling_from_disk`
  / `rolling_install_gate` / `restamp_rolling_token` / `sidecar_kind_of` and
  the `clauth rolling-token` + `clauth static-token [--clear]` verbs. The one
  line the fork still owns is a serde ALIAS: `#[serde(alias = "session_feed")]`
  on `ProfileConfig::rolling_token`, so a profile this fork armed under the old
  key stays armed across the upgrade (upstream deliberately carries no alias —
  no released upstream ever wrote that key). The alias is dead weight once the
  daemon rewrites each armed profile's `config.toml`; drop it at a later sync.
  Design rationale kept at `docs/cla-feed/DESIGN.md`, marked superseded.
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

**Endgame status (UPS-17, 2026-09-09):** the maintainer closed the design
thread on 2026-09-02 — "#69 supersedes it and #51 will not be merged" — and
left PR #69 in CHANGES_REQUESTED with ten required items, roughly fourteen
minors, and a head that no longer merges or compiles against `mommy`. So the
fork keeps its own codex engine (path A) until #69's round 2 lands: the two
codex layers now coexist by design, this fork's on `Profile`/`AppState`, and
upstream's future one on `codex-profiles.toml`. When #69 merges, the "Codex
engine" bullet collapses and a one-off migration moves the codex names out of
`profiles.toml` (`profiles`, `active_codex_profile`, `codex_fallback_chain`,
per-profile `harness`) into upstream's roster — neither upstream's `AppState`
nor #69's `CodexState::load` migrates them, and upstream's serde drops unknown
keys on the next save.

**The original plan (UPS-7, 2026-07-25):** upstream owns the codex
design now — `docs/codex-plan.md` on `mommy` is the spec, and we implement it
as a six-part series on a branch cut from the **v0.14 tag** (branch cut from a
TAG, not `upstream/mommy` — the one deliberate exception to the rule above).
It is a state-layer rewrite, not a port: harness moves out of
`Profile`/`AppState` into a separate `codex-profiles.toml`, dirs gain a `-cx`
suffix, and the store-mode refusal becomes a forced
`-c cli_auth_credentials_store="file"`. When that series lands, the "Codex
engine" bullet above collapses to whatever the fork still adds on top —
budget for the inventory to shrink, not grow.
