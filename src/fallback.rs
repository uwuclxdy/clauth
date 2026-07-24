use std::collections::HashMap;

use anyhow::Result;

use crate::actions::{switch_off, switch_profile};
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile};
use crate::usage::{
    FetchStatus, UsageInfo, UsageStore, UsageWindow, five_hour_live, iso_to_epoch_secs,
    now_epoch_secs, seven_day_live,
};

// Test-only per-thread counter: increments each time `next_auto_switch_target`
// takes the `UsageStore` lock. The snapshot refactor takes it exactly once per
// evaluation; the pre-snapshot shape (which locked per predicate: headroom walk
// pass 1a + 1b, serving-sink active + sibling, spend-armed active + sibling,
// budget-spent active, halt re-check), plus a re-lock per member visited in
// each walk, took the lock a dozen-plus times per evaluation on a typical chain.
// Thread-local sidesteps parallel-test counter pollution: each test thread
// sees only its own calls.
#[cfg(test)]
std::thread_local! {
    static NEXT_AUTO_SWITCH_TARGET_STORE_LOCKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// What the auto-switch evaluator decided when the active profile crossed its
/// threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchAction {
    /// Switch the active profile to this chain member.
    To(String),
    /// Turn off all accounts: clear the live credentials and unset the active
    /// profile. Emitted only in wrap-off mode when the whole chain is exhausted
    /// and no member is marked `last_resort`.
    Off,
}

/// Default 5-hour utilization threshold (percent) applied when a chain member
/// has no per-profile override. Stays below 100 as poll-lag margin: at the
/// fixed refresh cadence a window can blow past a 100% trigger between polls,
/// so the default leaves headroom to switch before the account is already
/// rate-limited.
pub(crate) const DEFAULT_THRESHOLD: f64 = 95.0;

pub(crate) fn threshold_for(profile: &Profile) -> f64 {
    profile.fallback_threshold.unwrap_or(DEFAULT_THRESHOLD)
}

/// Live 5h window for `profile`, or `None` when there's no snapshot yet or its
/// window isn't currently live (`five_hour_live`) — a lapsed or windowless
/// snapshot means the account has headroom again whatever its last-known
/// utilization says. Shared by [`is_exhausted`] and the burn-aware active
/// check ([`is_exhausted_active`]): both refuse to act on a stale/rolled-over
/// window the same way; they differ only in how they judge a live one.
fn live_five_hour(profile: &Profile) -> Option<&UsageWindow> {
    let usage = profile.usage.as_ref()?;
    if !five_hour_live(usage, now_epoch_secs()) {
        return None;
    }
    usage.five_hour.as_ref()
}

/// Weekly line for the wrap-off `Off` decision: REAL exhaustion (the API's own
/// cap), never the configurable soft line. Switching early to a sibling buys
/// headroom safety; signing every account out early buys nothing — it only
/// forfeits the tail between the soft line and the cap on the halt path.
///
/// Every surface that DESCRIBES the halted state reads the same line — the
/// all-spent banner and `soonest_resume`'s caption included — or it would claim
/// a member is spent while that member still serves requests.
pub(crate) const WEEKLY_HARD_BLOCK_PCT: f64 = 100.0;

/// Fraction of the binding spend cap a member may reach and still be picked for
/// spend reasons. The 10% slack is poll drift insurance: utilization is sampled
/// on a cadence, not streamed, so a member picked AT its cap would overshoot it
/// before the next read lands. Real money, so the margin errs low.
const SPEND_ARM_FRACTION: f64 = 0.90;

/// Dollars this member may still spend automatically, or `None` when it may not
/// spend at all. Both halves of the opt-in must be set (`budget_on` chain-wide,
/// a `ceiling > 0` on the member) AND the account must actually be billing
/// pay-as-you-go.
///
/// `enabled` is the honest field here, NOT [`crate::usage::SpendInfo::is_visible`]:
/// that one answers "is a spend bar worth rendering" and is true for an account
/// carrying a stale limit with billing switched OFF (observed live 2026-07-17).
/// Arming on it would hop the chain into an account that refuses the request,
/// with money attached.
///
/// The cap is whichever binds first — the account's own limit or the member's
/// ceiling. An account with billing on but no declared limit is bounded by the
/// ceiling alone; that is the point of the ceiling.
pub(crate) fn spend_room(spend: &crate::usage::SpendInfo, ceiling: f64) -> Option<f64> {
    // `is_finite` is load-bearing, not decoration: `max_auto_spend = inf` is
    // valid TOML, and an infinite ceiling with no account cap yields infinite
    // room — unlimited unattended spending. NaN would slip a plain `<= 0.0`
    // (every NaN comparison is false) and then `f64::min(NaN, cap)` returns the
    // cap, arming at the account's full limit. The load boundary normalizes
    // both away; this refuses them again for any other construction path.
    if !spend.enabled || !ceiling.is_finite() || ceiling <= 0.0 {
        return None;
    }
    let cap = match spend.limit {
        Some(limit) => limit.min(ceiling),
        None => ceiling,
    };
    // Unknown spend REFUSES rather than reading as $0 spent. `used` is the only
    // input bounding what gets spent, and `RawMoney::to_dollars` returns `None`
    // whenever `amount_minor` is absent or renamed — so defaulting it to 0 hands
    // back the full cap on any wire drift, which is the most permissive possible
    // answer to "how much has this already cost?". Fail closed on money.
    let used = spend.used?;
    let room = SPEND_ARM_FRACTION * cap - used;
    (room > 0.0).then_some(room)
}

/// True when the ACTIVE profile is billing pay-as-you-go and has spent the
/// budget the operator allowed it — the moment `max_auto_spend` has to stop
/// being an entry gate and start being a cap.
///
/// Deliberately narrow, because acting on it HALTS accounts: it needs the
/// chain-wide opt-in, a real ceiling on this member, and the account to actually
/// be billing. Anything else (a normal subscription account, a $0 ceiling, the
/// toggle off) is `false`, so the halt path stays bit-identical to pre-2026-07-17
/// for every chain that never opted in.
///
/// Only meaningful once the member's subscription windows are spent — an account
/// with quota left is not billing, so callers check exhaustion first (an
/// unexhausted member never reaches the halt decision anyway).
fn budget_spent(usage: Option<&crate::usage::UsageInfo>, budget_on: bool, ceiling: f64) -> bool {
    usage.and_then(|u| u.spend.as_ref()).is_some_and(|spend| {
        budget_on
            && spend.enabled
            && ceiling.is_finite()
            && ceiling > 0.0
            && spend_room(spend, ceiling).is_none()
    })
}

/// [`budget_spent`] gated the way [`blocked_reason`] gates it: only a member with
/// NO free 5h quota (a live window at/over its threshold) is BLOCKED by a spent
/// budget — one with headroom serves for free and the walk PREFERS it, never
/// consulting `budget_spent`, so a raw read would claim a block the engine never
/// acts on. The Usage-tab diagnostic reads THIS, not raw `budget_spent`, to hold
/// the render-only invariant a hint shares with `blocked_reason`.
pub(crate) fn budget_spent_blocking(config: &AppConfig, profile: &Profile) -> bool {
    live_five_hour(profile).is_some_and(|w| w.utilization >= threshold_for(profile))
        && budget_spent(
            profile.usage.as_ref(),
            config.state.spend_budget_switching,
            profile.max_auto_spend.unwrap_or(0.0),
        )
}

/// Whether this member's live config can bill with nothing stopping it: armed to
/// spend, but told to stay on the account once the budget runs out, and no free
/// parking spot to be sent to instead. `max_auto_spend` then bounds only when
/// the chain STARTS paying, never when it stops.
///
/// ONE predicate, two consumers, so the warning and the behavior cannot drift:
/// the Fallback card renders it as a DANGER tooltip under the ceiling it
/// describes, and the daemon says it at boot for the headless case where nobody
/// is watching that card. Policy, not presentation — which is why it lives here
/// beside the rules it is reading, not in `tui::render`.
pub(crate) fn spend_is_uncapped(config: &AppConfig, ceiling: f64) -> bool {
    config.state.spend_budget_switching
        && ceiling > 0.0
        && !config.state.switch_off_when_budget_spent
        // A parking spot only counts if the walk could actually send work
        // there: the same `candidate_excluded` the target walks skip on, read
        // over the CHAIN rather than every profile on disk. A sink that is
        // disabled, auth-broken, canceled, or was never added to the chain is
        // unreachable, and counting it reports "something will stop the
        // billing" while nothing can.
        && !config.state.fallback_chain.iter().any(|name| {
            !candidate_excluded(config, name) && config.find(name).is_some_and(|p| p.last_resort)
        })
}

/// The fix list for an uncapped-spend config ([`spend_is_uncapped`]): the two
/// actions that bound the bill. Shared by its three consumers (the Usage
/// `SpendUncapped` diagnostic, the Fallback always-on tooltip, the daemon
/// boot warning) so the copy cannot drift apart the way three hand-maintained
/// literals would.
pub(crate) fn uncapped_spend_fix() -> &'static str {
    "set extra usage spent to switch off all, or mark an account last resort"
}

/// [`spend_room`] over a member's usage snapshot: true when the chain may pick
/// it purely to spend money. Never consulted until every subscription member
/// with free quota has been passed over — see [`next_target`].
fn spend_armed(usage: Option<&crate::usage::UsageInfo>, budget_on: bool, ceiling: f64) -> bool {
    budget_on
        && usage
            .and_then(|u| u.spend.as_ref())
            .and_then(|s| spend_room(s, ceiling))
            .is_some()
}

/// Whether `info`'s live weekly window is past `weekly_pct` — treated as
/// spent until the weekly reset regardless of anything the 5h window says.
/// The line is a SOFT one below the API's 100% refusal cap, gating BOTH
/// directions of the chain walk (the active profile's switch trigger and
/// candidate acceptance, so a hop never lands on a member that would
/// re-trigger next tick).
///
/// Why not wait for 100 (the original hard cap, 2026-07-08): an account
/// riding 98%+ of its week bricks for DAYS the moment it tops out, and its
/// idle 5h window drains and then LAPSES — so waiting for the refusal means
/// dying mid-session and (before the gate) even switching INTO such a member.
/// Hopping at the line leaves the tail as slack instead (2026-07-12).
///
/// The LINE is chain-global (`AppState::weekly_switch_threshold_pct`, default
/// 98): it protects the CHAIN (a wrong hop strands days, not hours), not one
/// member's taste. Whether a member is JUDGED against it is per-account —
/// `Profile::check_weekly` off raises that member's line to the hard cap (see
/// [`weekly_line`]). Only `seven_day` gates HERE — per-model `weekly_scoped`
/// limits (e.g. "7d fable") gate separately, per-account and never as full
/// exhaustion (see [`scoped_weekly_blocked_info`]). Store-side twin logic
/// inlines this same shape (see `is_exhausted_from_usage`).
fn weekly_blocked_info(info: &crate::usage::UsageInfo, now_secs: i64, weekly_pct: f64) -> bool {
    seven_day_live(info, now_secs)
        && info
            .seven_day
            .as_ref()
            .is_some_and(|w| w.utilization >= weekly_pct)
}

/// The SOFT weekly line a member's aggregate 7d window is judged at:
/// the account's own `weekly_threshold` override when set, else the passed
/// chain-wide line — unless its `check_weekly` gate is off, which lifts the
/// soft judgment entirely (only [`WEEKLY_HARD_BLOCK_PCT`] remains: an account
/// past the API's refusal cap cannot serve at all, so no setting bypasses
/// that — hard callers use [`weekly_hard_blocked`] and never resolve here).
pub(crate) fn member_weekly_line(profile: &Profile, chain_soft: f64) -> f64 {
    if !profile.check_weekly {
        return WEEKLY_HARD_BLOCK_PCT;
    }
    profile.weekly_threshold.unwrap_or(chain_soft)
}

/// The line a member's per-model (`weekly_scoped`) windows are judged at: the
/// same per-account `weekly_threshold` override, else the chain-wide line.
/// The override rides the `weekly gate` exactly like [`member_weekly_line`]:
/// with `check_weekly` off the `weekly at` row renders dimmed and its editor
/// refuses to open, so a stored value must not keep judging windows the UI
/// says it doesn't. Scoped judgment itself stays on — `check_scoped` is its
/// gate — only the LINE falls back to the chain-wide default.
pub(crate) fn member_scoped_line(profile: &Profile, chain_soft: f64) -> f64 {
    if !profile.check_weekly {
        return chain_soft;
    }
    profile.weekly_threshold.unwrap_or(chain_soft)
}

/// [`weekly_blocked_info`] over a profile's own usage snapshot, judged at the
/// profile's own SOFT line ([`member_weekly_line`]) — `chain_soft` is the
/// chain-wide default the member's override/gate resolves against. Hard-cap
/// callers (sinks, halts, the dead-for-days chip) use [`weekly_hard_blocked`]
/// instead: a per-account override must never soften — or tighten — the
/// literal can-it-serve-at-all judgment.
pub(crate) fn weekly_blocked(profile: &Profile, chain_soft: f64) -> bool {
    profile.usage.as_ref().is_some_and(|u| {
        weekly_blocked_info(u, now_epoch_secs(), member_weekly_line(profile, chain_soft))
    })
}

/// The aggregate 7d window at/over the API's refusal cap — dead for days, no
/// per-account setting consulted (see [`weekly_blocked`]).
pub(crate) fn weekly_hard_blocked(profile: &Profile) -> bool {
    profile
        .usage
        .as_ref()
        .is_some_and(|u| weekly_blocked_info(u, now_epoch_secs(), WEEKLY_HARD_BLOCK_PCT))
}

/// Whether one window carries a parseable reset still in the future — the
/// per-window liveness primitive [`scoped_weekly_blocked_info`] applies to
/// each `weekly_scoped` entry (mirroring `seven_day_live` for the aggregate).
fn window_live(w: &UsageWindow, now_secs: i64) -> bool {
    w.resets_at
        .as_deref()
        .and_then(iso_to_epoch_secs)
        .is_some_and(|resets_at| now_secs < resets_at)
}

/// Whether any LIVE per-model weekly window (`weekly_scoped`, e.g. "7d
/// fable") is past the weekly line. Distinct from [`weekly_blocked_info`]
/// (the aggregate 7d cap): a model-scoped window at 100 blocks ONLY that
/// model, so it never counts as full exhaustion (wrap-off `Off`,
/// `soonest_resume`, recovery gates all stay on the aggregate) — but it DOES
/// keep the member out of the rotation passes while its `check_scoped` gate
/// is on, because the walk cannot know which model the next session will
/// drive and landing a session of the capped model on such a member strands
/// it (observed live 2026-07-18: "7d fable" 100% while the aggregate 7d
/// showed 65% — the old aggregate-only walk called that member the
/// healthiest target). Operators who run other models on that account flip
/// its `check_scoped` gate off to keep it in rotation.
pub(crate) fn scoped_weekly_blocked_info(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    weekly_pct: f64,
) -> bool {
    worst_scoped_window(info, now_secs, weekly_pct).is_some()
}

/// The worst LIVE per-model window at/over `weekly_pct`, or `None` when the
/// member is scoped-clear — [`scoped_weekly_blocked_info`] IS this judgment,
/// so a display surface naming the offending window (the `ScopedSpent` chip)
/// reads the same predicate rather than inlining a second opinion.
pub(crate) fn worst_scoped_window(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    weekly_pct: f64,
) -> Option<&crate::usage::ScopedWindow> {
    info.weekly_scoped
        .iter()
        .filter(|s| window_live(&s.window, now_secs) && s.window.utilization >= weekly_pct)
        .max_by(|a, b| a.window.utilization.total_cmp(&b.window.utilization))
}

/// [`scoped_weekly_blocked_info`] over a profile's own usage snapshot, gated
/// by the profile's own `check_scoped` toggle and judged at its own line
/// ([`member_scoped_line`]).
fn scoped_weekly_blocked(profile: &Profile, chain_soft: f64) -> bool {
    profile.check_scoped
        && profile.usage.as_ref().is_some_and(|u| {
            scoped_weekly_blocked_info(u, now_epoch_secs(), member_scoped_line(profile, chain_soft))
        })
}

/// Scheduler-side [`scoped_weekly_blocked`]: reads from the walk's one
/// usage snapshot, gated by the member's `check_scoped` toggle and judged at
/// its snapshot-resolved `scoped_line`. An absent entry fails safe to "not
/// blocked" (the member is then judged by the aggregate gates alone).
fn scoped_blocked_from_usage(member: &ChainMember, usage: &HashMap<String, UsageInfo>) -> bool {
    member.check_scoped
        && usage.get(&member.name).is_some_and(|info| {
            scoped_weekly_blocked_info(info, now_epoch_secs(), member.scoped_line)
        })
}

/// True when the profile's 5h utilization has crossed its own threshold. Also
/// drives the TUI's all-spent banner wording. Static regardless of burn-aware
/// mode (issue #8 follow-up b) — the target walk's candidates and
/// `soonest_resume` keep this exact behavior by design; only the ACTIVE
/// profile's own decision becomes projection-aware (see
/// [`is_exhausted_active`]).
pub(crate) fn is_exhausted(profile: &Profile, chain_soft: f64) -> bool {
    weekly_blocked(profile, chain_soft)
        || live_five_hour(profile).is_some_and(|w| w.utilization >= threshold_for(profile))
}

/// [`is_exhausted`] at the HARD weekly cap: can this account literally serve
/// right now? Sink passes, halt gates, and the all-exhausted premise key on
/// this — a member's soft-line override/gates never soften it.
pub(crate) fn is_exhausted_hard(profile: &Profile) -> bool {
    weekly_hard_blocked(profile)
        || live_five_hour(profile).is_some_and(|w| w.utilization >= threshold_for(profile))
}

/// True when `profile`'s last `/profile` read shows a canceled subscription: the
/// org dropped to `claude_free` while its cached 5h window still reads as idle
/// headroom, but `/v1/messages` 403s on every request. Kept a SEPARATE axis from
/// exhaustion (a canceled account is dead, not spent) so the chip wording and the
/// all-spent banner stay honest — but treated the same by the walk, which skips
/// it like `broken`/`kick_rejected`. Config-side twin of
/// [`is_canceled_from_usage`], reading `Profile.usage` (the UI thread's copy).
/// `pub(crate)` so the Overview row's dead-first `⊖` marker reads the same
/// predicate the walk does, never a second opinion.
pub(crate) fn is_canceled(profile: &Profile) -> bool {
    profile
        .usage
        .as_ref()
        .and_then(|u| u.plan.as_ref())
        .is_some_and(|p| p.is_canceled())
}

/// Recent burn rate (%/h) for `name`'s 5h window: durable per-profile history
/// (`usage_history.jsonl`, a plain disk read — no shared lock, so this is safe
/// to call without touching the order in `lockorder.rs`, but never while
/// holding the `AppConfig` guard) plus the current live sample, run through
/// the same recency-weighted computation and windowing `App::active_burn_rate`
/// uses for the Overview ETA line. `None` until enough distinct samples exist.
/// Sole caller is the scheduler-side [`is_exhausted_active_from_usage`] — the
/// UI-thread [`is_exhausted_active`] takes its rate as a parameter instead so
/// the render pass never triggers this disk read under the config guard.
pub(crate) fn burn_rate_for_profile(name: &str, window: &UsageWindow) -> Option<f64> {
    let history = crate::profile::load_usage_history(name);
    let pair = ("5h", window);
    crate::usage::compute_burn_rates_from_history(
        &history,
        std::slice::from_ref(&pair),
        crate::usage::BURN_LOOKBACK_MS,
        crate::usage::BURN_MIN_SAMPLES,
        crate::usage::BURN_GAP_CUT_MS,
    )
    .remove("5h")
    .flatten()
}

/// Burn-aware exhaustion test shared by [`is_exhausted_active`] and its
/// scheduler-side store variant (issue #8 follow-up b, opt-in — off by
/// default). The static `util_pct >= threshold` check always applies, so
/// enabling burn-aware can only ever move the switch EARLIER than mode-off,
/// never later. `None` burn (no history yet, a fresh profile, or a provider
/// with none) leaves only that static check — never uncovers an account for
/// lack of data. With a rate, the projection ADDS an earlier trip inside the
/// `floor_pct.min(threshold)..threshold` band: a heavier burn crosses the 100%
/// cap before the next poll and switches sooner, a light one runs on since it
/// won't blow the cap in time. (Above `threshold` the static check has already
/// fired, so the band's upper bound is `threshold`; a `floor_pct` set above
/// `threshold` — the stock default 98 over 95 — just clamps to `threshold` and
/// burn-aware reduces to static.)
///
/// Two guards keep the projection from switching too early — the failure the
/// unbounded form hit worst on the smallest tier (Pro): the burn %/h is
/// window-relative, so the same activity reads a higher rate on a smaller
/// window and the projection trips from further below 100.
///   * `floor_pct`: the projection may not fire below `floor_pct.min(threshold)`,
///     bounding how far below `threshold` an early switch can land.
///   * `horizon_cap_ms`: the look-ahead is `min(interval_ms, horizon_cap_ms)`,
///     so a long poll cadence can't balloon the margin (it scales linearly with
///     the look-ahead). Folded in here rather than in
///     [`crate::usage::project_utilization`] so that helper keeps its one job.
fn is_exhausted_projected(
    util_pct: f64,
    threshold: f64,
    burn_pct_per_hour: Option<f64>,
    interval_ms: u64,
    floor_pct: f64,
    horizon_cap_ms: u64,
) -> bool {
    match burn_pct_per_hour {
        Some(rate) => {
            let horizon = interval_ms.min(horizon_cap_ms);
            (util_pct >= floor_pct.min(threshold)
                && crate::usage::project_utilization(util_pct, rate, horizon) >= 100.0)
                || util_pct >= threshold
        }
        None => util_pct >= threshold,
    }
}

/// ACTIVE-only exhaustion check (issue #8 follow-up b). `burn_aware` off
/// reproduces [`is_exhausted`] bit for bit — mode off must never diverge from
/// today's static behavior. On, `active_burn_pct_per_hour` — the caller's
/// in-memory rate; this function never reads disk itself, see
/// [`is_exhausted_active_from_usage`] for the disk-reading scheduler twin —
/// feeds [`is_exhausted_projected`]. Deliberately never applied to the target
/// walk's candidates or `soonest_resume` — see docs/internals.md's auto-switch
/// asymmetry; this only ever changes whether the ACTIVE profile itself is
/// judged exhausted.
pub(crate) fn is_exhausted_active(
    profile: &Profile,
    burn_aware: bool,
    interval_ms: u64,
    active_burn_pct_per_hour: Option<f64>,
    weekly_blocked_now: bool,
    floor_pct: f64,
    horizon_cap_ms: u64,
) -> bool {
    // Weekly line first: a 7d window past it is treated as dead until its
    // weekly reset whatever the 5h window (often lapsed/idle by then) says,
    // and no burn projection applies — there is nothing left to project.
    // The judgment itself is the CALLER'S (`weekly_blocked_now`): the switch
    // trigger judges at the member's soft line, the halt gate at the hard cap
    // — bundling one pct here couldn't serve both once lines went per-member.
    if weekly_blocked_now {
        return true;
    }
    let Some(window) = live_five_hour(profile) else {
        return false;
    };
    let threshold = threshold_for(profile);
    if !burn_aware {
        return window.utilization >= threshold;
    }
    is_exhausted_projected(
        window.utilization,
        threshold,
        active_burn_pct_per_hour,
        interval_ms,
        floor_pct,
        horizon_cap_ms,
    )
}

/// Name + seconds-until-reset of the chain member that resumes soonest — the
/// all-exhausted caption's data source (issue #10: the implicit
/// resume-at-soonest-reset behavior, made explicit). Valid only when every
/// member that could actually be switched TO is currently exhausted, covering
/// both wrap-off's switch-off-all (active cleared) and wrap mode's
/// stalled-active equivalent (`next_target` returns `None` with every member
/// maxed). A dead member — disabled, auth-broken, or canceled
/// ([`candidate_excluded`]) — is never a switch candidate in the first
/// place, so its usage (however idle a cached window it holds) says nothing
/// about the chain's premise and is SKIPPED, not counted. An unresolvable
/// member is the one exception: it can't be proven exhausted at all, so it
/// still bails the WHOLE result to `None` rather than being skipped around
/// (see the loop's `config.find` below). Reuses
/// [`is_exhausted`]'s wall-clock gates (`five_hour_live` and the weekly
/// `seven_day_live` block) for every OTHER member: one exhausted in neither
/// window already has headroom — `find_recovered_member` / `scan_recovery`
/// would relink it on the very next tick — so THAT member's presence bails
/// the WHOLE result to `None` rather than being skipped around; the
/// caption's premise is that nothing reachable in the chain is currently
/// usable. Ties on `resets_at` keep the earlier chain-order member.
///
/// Weekly-wise the premise keys on the HARD cap ([`WEEKLY_HARD_BLOCK_PCT`]),
/// not the soft line (2026-07-10 triage): a member past the line still serves
/// every request its live 5h window allows, so counting it spent captioned a
/// days-out weekly reset over a working account. The window PICK below keeps
/// the soft line — once a member is genuinely out of 5h, its soft-blocked week
/// is what `find_recovered_member` gates on, so its 7d reset (not the 5h one)
/// is when the chain can use it again.
pub(crate) fn soonest_resume(config: &AppConfig) -> Option<(String, i64)> {
    let chain = &config.state.fallback_chain;
    if chain.is_empty() {
        return None;
    }
    let now = now_epoch_secs();
    let weekly_pct = config.state.weekly_switch_threshold_pct();
    let mut best: Option<(&str, i64)> = None;
    for name in chain {
        // An unresolvable member can't be proven exhausted — bail rather
        // than pick around it, same as before this member-skip existed.
        let profile = config.find(name)?;
        // A dead member is never a switch candidate, so skip it around
        // instead of letting its (possibly idle) cached usage bail the
        // whole caption to None.
        if candidate_excluded(config, name) {
            continue;
        }
        if !is_exhausted_hard(profile) {
            return None;
        }
        // A weekly-dead member resumes at its 7d reset (its 5h window may be
        // lapsed or absent entirely); anyone else at their next 5h reset.
        let usage = profile.usage.as_ref()?;
        let window = if weekly_blocked(profile, weekly_pct) {
            usage.seven_day.as_ref()?
        } else {
            usage.five_hour.as_ref()?
        };
        let resets_at = window.resets_at.as_deref().and_then(iso_to_epoch_secs)?;
        if best.is_none_or(|(_, cur)| resets_at < cur) {
            best = Some((name.as_str(), resets_at));
        }
    }
    let (name, resets_at) = best?;
    Some((name.to_string(), (resets_at - now).max(0)))
}

/// Why a chain member is currently ineligible or distrusted, worst first. The
/// Fallback tab renders it as a per-row marker (selector) plus a status pill
/// (detail card); render-only, no walk consumes it. Policy lives here beside the
/// walk predicates so a chip can never claim a block the walk doesn't act on.
///
/// Precedence is "dead first": a login that can't serve at all outranks one that
/// serves but won't be picked. [`blocked_reason`] returns the first matching arm.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlockedReason {
    /// Operator disabled the account: the walk skips it as a candidate no matter
    /// what its usage says, so this outranks every quota/liveness block — those
    /// would describe a member nothing is going to pick anyway.
    Disabled,
    /// Subscription canceled (`/profile` `subscription_status == "canceled"`):
    /// the org dropped to `claude_free` and `/v1/messages` 403s, so it can't
    /// serve at all — dead-first, above every quota/limiter block.
    Canceled,
    /// OAuth refresh revoked/invalid (AUTH-1): needs re-login before anything
    /// else about this member matters.
    AuthBroken,
    /// 7d window at/over the hard cap ([`WEEKLY_HARD_BLOCK_PCT`]) — dead until the
    /// weekly reset. `resets_in` is seconds to that reset when the window carries
    /// a parseable `resets_at`.
    WeeklySpent { resets_in: Option<i64> },
    /// The messages limiter refused this member's 5h auto-start kick (switch-grade
    /// — the same block the walk routes around): clauth can't bring it online
    /// regardless of its usage headroom. `lifts_in` is seconds to the limiter's
    /// advertised ceiling (an upper bound it usually undercuts).
    KickRejected { lifts_in: i64 },
    /// Billing member that is out of free 5h quota AND has spent the
    /// `max_auto_spend` budget the operator allowed it (see [`budget_spent`]) — so
    /// it can neither serve free nor be paid-rescued. With 5h headroom the walk
    /// prefers it and budget is moot, so this never fires there.
    BudgetSpent,
    /// 5h window at/over the member's rotate threshold: exhausted for now, resets
    /// within hours. `pct` is the current utilization.
    FiveHour { pct: f64, resets_in: Option<i64> },
    /// A per-model weekly window (e.g. "7d fable") past the weekly line while
    /// this member's `check_scoped` gate is on: out of rotation, though it
    /// still serves other models. `label`/`pct` name the worst such window.
    ScopedSpent { label: String, pct: f64 },
    /// 7d window over the soft switch line but under the hard cap: still serves
    /// every request, just won't be picked. `pct` is the current utilization.
    WeeklySoft { pct: f64 },
    /// Last read wasn't live (cached / endpoint-429): the numbers are old, so the
    /// chain distrusts this member's apparent headroom.
    Stale,
}

/// Seconds until `window` resets, clamped at 0, or `None` when it carries no
/// parseable `resets_at`. Mirrors the arithmetic `soonest_resume` uses.
fn reset_secs(window: &UsageWindow, now: i64) -> Option<i64> {
    window
        .resets_at
        .as_deref()
        .and_then(iso_to_epoch_secs)
        .map(|t| (t - now).max(0))
}

/// The single worst [`BlockedReason`] for `profile`, or `None` when it is a live
/// member with headroom. Reads the same predicates the walk does — never a
/// second opinion — so the chip and the behavior cannot drift. See
/// [`BlockedReason`] for the precedence. `kick_lift` carries the kick-block
/// state the walk sees but a `&Profile` can't (it lives on the store twin):
/// `Some(until)` epoch secs when the member is switch-grade kick-rejected
/// ([`crate::usage::switch_grade_kick_lifts`]), else `None`.
pub(crate) fn blocked_reason(
    config: &AppConfig,
    profile: &Profile,
    kick_lift: Option<i64>,
) -> Option<BlockedReason> {
    // Disabled first, and only for a NON-active member: `snapshot_chain` /
    // `next_target` skip a disabled member as a CANDIDATE but deliberately never
    // drop a disabled ACTIVE one, so claiming a block on the active profile here
    // would be the second opinion this function must never be.
    if profile.is_disabled() && !config.is_active(&profile.name) {
        return Some(BlockedReason::Disabled);
    }
    health_blocked_reason(config, profile, kick_lift)
}

/// [`blocked_reason`] minus the `Disabled` rung: the worst reason this member's
/// own HEALTH blocks it, ignoring whether the operator disabled it.
///
/// Split out so the Fallback detail card can stack `[ disabled ]` above the
/// health reason instead of one hiding the other. It is the same ladder in the
/// same order — [`blocked_reason`] delegates to it rather than repeating it, so
/// the card and the marker can never disagree about precedence.
pub(crate) fn health_blocked_reason(
    config: &AppConfig,
    profile: &Profile,
    kick_lift: Option<i64>,
) -> Option<BlockedReason> {
    // Canceled subscription first (dead-first): a 403-ing account outranks a
    // login that could still refresh. Mirrors the walk's `is_canceled` skip.
    if is_canceled(profile) {
        return Some(BlockedReason::Canceled);
    }
    if config.is_auth_broken(&profile.name) {
        return Some(BlockedReason::AuthBroken);
    }
    let now = now_epoch_secs();
    // Weekly HARD cap first: dead for days regardless of the 5h window.
    if weekly_hard_blocked(profile) {
        let resets_in = profile
            .usage
            .as_ref()
            .and_then(|u| u.seven_day.as_ref())
            .and_then(|w| reset_secs(w, now));
        return Some(BlockedReason::WeeklySpent { resets_in });
    }
    // Kick-rejected outranks the usage blocks: the limiter won't let clauth start
    // this member at all, so its free/paid headroom is moot. Below weekly-hard
    // (days) because the kick block lifts within hours.
    if let Some(until) = kick_lift {
        return Some(BlockedReason::KickRejected {
            lifts_in: (until - now).max(0),
        });
    }
    // 5h over its rotate threshold = no free quota to serve right now. A spent
    // billing budget only BLOCKS a member in that state: with free 5h quota the
    // member serves for free, the walk PREFERS it (headroom pass, `!is_exhausted`)
    // and never consults `budget_spent` for it — so flagging it there would claim
    // a block the walk doesn't act on. Gating on 5h-maxed also excludes the
    // last-resort serving sink, which by definition still has 5h headroom.
    let five_hour_over =
        live_five_hour(profile).filter(|w| w.utilization >= threshold_for(profile));
    if five_hour_over.is_some() {
        let ceiling = profile.max_auto_spend.unwrap_or(0.0);
        if budget_spent(
            profile.usage.as_ref(),
            config.state.spend_budget_switching,
            ceiling,
        ) {
            return Some(BlockedReason::BudgetSpent);
        }
    }
    if let Some(window) = five_hour_over {
        return Some(BlockedReason::FiveHour {
            pct: window.utilization,
            resets_in: reset_secs(window, now),
        });
    }
    // The hard cap already returned above, so a soft hit here is strictly the
    // [soft, 100) band — a member that still serves.
    let soft = config.state.weekly_switch_threshold_pct();
    // A gated-on per-model window past the line: out of rotation (the walk's
    // headroom accept skips it), though other models still serve. Worst such
    // window names the chip. Above `WeeklySoft` — same still-serving band,
    // but this one is model-dead rather than merely dispreferred.
    if profile.check_scoped
        && let Some(worst) = profile
            .usage
            .as_ref()
            .and_then(|u| worst_scoped_window(u, now, member_scoped_line(profile, soft)))
    {
        return Some(BlockedReason::ScopedSpent {
            label: worst.label.clone(),
            pct: worst.window.utilization,
        });
    }
    if weekly_blocked(profile, soft) {
        let pct = profile
            .usage
            .as_ref()
            .and_then(|u| u.seven_day.as_ref())
            .map(|w| w.utilization)
            .unwrap_or(soft);
        return Some(BlockedReason::WeeklySoft { pct });
    }
    if matches!(
        profile.fetch_status,
        Some(FetchStatus::Cached | FetchStatus::RateLimited)
    ) {
        return Some(BlockedReason::Stale);
    }
    None
}

/// One chain member as observed when a `ChainSnapshot` was built. Holds enough
/// to evaluate the fallback decision without re-locking `AppConfig` — caller
/// snapshots once under the config mutex then reads `UsageStore` separately,
/// avoiding the `config ↔ store` lock inversion against `App::apply_usage`
/// (which holds `usage_store` then takes `config`).
#[derive(Debug, Clone)]
pub(crate) struct ChainMember {
    pub(crate) name: String,
    pub(crate) threshold: f64,
    /// Mirrors `Profile::last_resort` — a terminal stop for the chain walk,
    /// decoupled from `threshold` (issue #8 follow-up: a threshold no longer
    /// doubles as a sink marker).
    pub(crate) last_resort: bool,
    /// The member's resolved SOFT weekly line ([`member_weekly_line`]:
    /// `weekly_threshold` override else the chain-wide line; the hard cap
    /// when its `check_weekly` gate is off). Snapshot-folded so the lock-free
    /// walk never re-derives it.
    pub(crate) weekly_line: f64,
    /// The member's resolved per-model-window line ([`member_scoped_line`]).
    pub(crate) scoped_line: f64,
    /// Mirrors `Profile::check_scoped` — whether per-model weekly windows
    /// (e.g. "7d fable") keep this member out of rotation.
    pub(crate) check_scoped: bool,
    /// Mirrors `Profile::max_auto_spend` in dollars, `0` when unset — the
    /// member's own ceiling on unattended pay-as-you-go spending.
    pub(crate) max_spend: f64,
}

/// In-memory snapshot of the fields `next_auto_switch_target` needs: active
/// profile name + ordered chain with each member's resolved threshold. Built
/// under the `AppConfig` mutex by [`snapshot_chain`], then evaluated lock-free.
#[derive(Debug, Clone)]
pub(crate) struct ChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
    /// Snapshot of `AppState::switch_off_when_spent` — drives the switch-off-all decision.
    pub(crate) switch_off_when_spent: bool,
    /// Snapshot of `AppState::auth_broken` — members whose OAuth refresh is
    /// revoked/invalid (AUTH-1). Excluded from every walk pass so a dead token
    /// is never installed unattended.
    pub(crate) broken: Vec<String>,
    /// Snapshot of `AppState::burn_aware_switching` (issue #8 follow-up b) —
    /// gates whether the ACTIVE-side check in `next_auto_switch_target`
    /// projects ahead of the next poll instead of using the static threshold.
    pub(crate) burn_aware: bool,
    /// Snapshot of `AppState::refresh_interval_ms` — the projection's poll
    /// interval. Read through `config.state` here (this snapshot is already
    /// built once under the config lock per tick) rather than the scheduler's
    /// hot-path `Arc<AtomicU64>`, mirroring exactly how `switch_off_when_spent` reaches
    /// this struct.
    pub(crate) interval_ms: u64,
    /// Snapshot of `AppState::burn_switch_floor_pct()` — the burn-aware
    /// projection's early-switch floor (inert unless `burn_aware`).
    pub(crate) burn_floor_pct: f64,
    /// Snapshot of `AppState::burn_horizon_cap_ms()` — caps the projection's
    /// look-ahead to `min(interval_ms, this)` (inert unless `burn_aware`).
    pub(crate) burn_horizon_cap_ms: u64,
    /// Snapshot of `AppState::spend_budget_switching` — the master half of the
    /// real-money opt-in. Off (the default) makes the spend pass inert whatever
    /// any member's ceiling says.
    pub(crate) spend_budget: bool,
    /// Snapshot of `AppState::switch_off_when_budget_spent` — `switch_off_when_spent`'s twin for the case
    /// where what ran out is money rather than quota.
    pub(crate) switch_off_when_budget_spent: bool,
    /// Members whose 5h auto-start kick the messages limiter is REJECTING
    /// (switch-grade `KickBlock`s: the limiter's own `rejected` verdict, ≥2
    /// consecutive kicks, advertised ceiling still ahead). Not config state —
    /// [`snapshot_chain`] leaves it empty and the scheduler's scan fills it
    /// from the live kick-block map. A rejected member can't serve inference
    /// even with idle-looking usage, so it is walked around like `broken`, and
    /// a rejected ACTIVE bypasses the exhaustion gate the same way.
    pub(crate) kick_rejected: Vec<String>,
    /// Members whose last store read was live (`FetchStatus::Fresh`) — the same
    /// freshness `decision_fresh` gates the ACTIVE on. Not config state:
    /// [`snapshot_chain`] leaves it empty and the scheduler's scan fills it from
    /// the `StatusStore` (like `kick_rejected`). Drives the headroom walk's
    /// fresh-PREFERENCE first pass — a PREFERENCE, never a gate: when no fresh
    /// member has headroom the walk still accepts a stale-but-unexhausted one,
    /// so an exhausted active never loses its escape (2026-06-28 target
    /// asymmetry: the walk gates only the ACTIVE, never the target).
    pub(crate) fresh: Vec<String>,
}

/// Snapshot active profile + chain + per-member thresholds out of `AppConfig`.
/// Returns `None` when there's no active profile, the active isn't a chain
/// member, or the chain is empty — every case where `next_auto_switch_target`
/// short-circuits anyway, so callers can skip evaluation on `None`.
pub(crate) fn snapshot_chain(config: &AppConfig) -> Option<ChainSnapshot> {
    let active = config.state.active_profile.as_deref()?.to_string();
    let chain = &config.state.fallback_chain;
    if !chain.iter().any(|n| n == &active) {
        return None;
    }
    let weekly_pct = config.state.weekly_switch_threshold_pct();
    let chain = chain
        .iter()
        .filter(|name| {
            // CDX-1 T1b tolerance: a stray codex member (hand-edited into an
            // existing profiles.toml — the edit surfaces reject new ones) must
            // never become a walk candidate. Silent here (this runs every
            // tick); the rejection with a message lives in `fallback_config`.
            !config.find(name).is_some_and(|p| p.is_codex())
        })
        // A disabled NON-active member is invisible to the scheduler-side walk
        // (`next_auto_switch_target`) — dropped here rather than carried as a
        // `ChainMember` flag, so the freshness pass built from `.chain` below
        // never considers it either. The active slot must stay RESOLVABLE
        // regardless of its own `disabled` bit — `next_auto_switch_target`
        // indexes into this vec by `position(|m| m.name == snapshot.active)`,
        // so dropping a disabled active here would make that `?` return `None`
        // every tick, wedging auto-switch permanently instead of just skipping
        // it as a candidate (mirrors `next_target`'s skip closure, which never
        // skips `chain[i] == active` either — `disabled` is a candidate-only
        // exclusion, same as `broken`/`canceled`).
        .filter(|name| {
            name.as_str() == active.as_str() || !config.find(name).is_some_and(Profile::is_disabled)
        })
        .map(|name| {
            let profile = config.find(name);
            ChainMember {
                name: name.to_string(),
                threshold: profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD),
                last_resort: profile.is_some_and(|p| p.last_resort),
                weekly_line: profile
                    .map(|p| member_weekly_line(p, weekly_pct))
                    .unwrap_or(weekly_pct),
                scoped_line: profile
                    .map(|p| member_scoped_line(p, weekly_pct))
                    .unwrap_or(weekly_pct),
                check_scoped: profile.is_none_or(|p| p.check_scoped),
                max_spend: profile.and_then(|p| p.max_auto_spend).unwrap_or(0.0),
            }
        })
        .collect();
    Some(ChainSnapshot {
        active,
        chain,
        switch_off_when_spent: config.state.switch_off_when_spent,
        broken: config
            .state
            .auth_broken
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
        burn_aware: config.state.burn_aware_switching,
        interval_ms: config.state.refresh_interval_ms,
        burn_floor_pct: config.state.burn_switch_floor_pct(),
        burn_horizon_cap_ms: config.state.burn_horizon_cap_ms(),
        spend_budget: config.state.spend_budget_switching,
        switch_off_when_budget_spent: config.state.switch_off_when_budget_spent,
        kick_rejected: Vec::new(),
        fresh: Vec::new(),
    })
}

/// Scheduler-side [`is_exhausted`] over a usage snapshot: reads 5h utilization
/// from a `HashMap<String, UsageInfo>` taken under a single `UsageStore` lock
/// (see [`next_auto_switch_target`]) rather than `Profile.usage` (which only the
/// UI thread writes via `apply_usage`), judged at the caller's weekly `line`
/// (the member's snapshot-resolved one, or the literal hard cap). The snapshot
/// is taken once per evaluation so a fetch landing between this read and the
/// next pass cannot flip the decision.
fn is_exhausted_from_usage(
    member: &ChainMember,
    usage: &HashMap<String, UsageInfo>,
    line: f64,
) -> bool {
    let now = now_epoch_secs();
    usage.get(&member.name).is_some_and(|info| {
        weekly_blocked_info(info, now, line)
            || (five_hour_live(info, now)
                && info
                    .five_hour
                    .as_ref()
                    .is_some_and(|w| w.utilization >= member.threshold))
    })
}

/// Scheduler-side [`is_canceled`] over a usage snapshot — reads the plan from the
/// single-lock `UsageStore` clone, exactly like [`is_exhausted_from_usage`].
fn is_canceled_from_usage(name: &str, usage: &HashMap<String, UsageInfo>) -> bool {
    usage
        .get(name)
        .and_then(|i| i.plan.as_ref())
        .is_some_and(|p| p.is_canceled())
}

/// Scheduler-side [`is_exhausted_active`] over a usage snapshot: reads the 5h
/// window from the same single-lock snapshot as [`is_exhausted_from_usage`], so
/// the scheduler's periodic scan agrees with the UI-thread one-shot
/// (`auto_switch_if_needed`) on the ACTIVE decision. The snapshot is owned
/// (cloned under the one lock), so the (disk-only, unlocked) burn rate lookup
/// below runs with no store lock held — same property the pre-snapshot shape
/// upheld per predicate.
fn is_exhausted_active_from_usage(
    member: &ChainMember,
    burn_aware: bool,
    interval_ms: u64,
    usage: &HashMap<String, UsageInfo>,
    line: f64,
    floor_pct: f64,
    horizon_cap_ms: u64,
) -> bool {
    let now = now_epoch_secs();
    let Some(info) = usage.get(&member.name) else {
        return false;
    };
    // The weekly line trumps projection (mirrors `is_exhausted_active`).
    if weekly_blocked_info(info, now, line) {
        return true;
    }
    let Some(window) = (five_hour_live(info, now).then_some(info.five_hour.as_ref()))
        .flatten()
        .cloned()
    else {
        return false;
    };
    if !burn_aware {
        return window.utilization >= member.threshold;
    }
    let rate = burn_rate_for_profile(&member.name, &window);
    is_exhausted_projected(
        window.utilization,
        member.threshold,
        rate,
        interval_ms,
        floor_pct,
        horizon_cap_ms,
    )
}

/// Chain walk shared by [`next_target`] and [`next_auto_switch_target`]. Scans
/// every other slot starting one after `idx` and wrapping. `skip_pred(i)` skips
/// slots (active profile, or a member with no resolvable profile);
/// `accept_pred(i)` selects the first matching slot, whose index is returned.
fn walk_chain(
    idx: usize,
    len: usize,
    skip_pred: &dyn Fn(usize) -> bool,
    accept_pred: &dyn Fn(usize) -> bool,
) -> Option<usize> {
    for offset in 1..=len {
        let i = (idx + offset) % len;
        if skip_pred(i) {
            continue;
        }
        if accept_pred(i) {
            return Some(i);
        }
    }
    None
}

/// Config-side exclusions every fallback-chain walk applies before it weighs
/// usage headroom: an unresolvable, auth-broken, or user-disabled member is
/// never a candidate. Pure state (no usage read), so it holds in both the UI
/// and daemon contexts. Shared by `next_target`, `fully_clear_target`, and
/// `scan_recovery` so their skip lists can't drift apart. The `== active`
/// guard is walk-structural and stays at the call site; canceled is
/// usage-derived and split by data source (see `candidate_excluded`).
pub(crate) fn walk_excluded(config: &AppConfig, name: &str) -> bool {
    let p = config.find(name);
    p.is_none() || config.is_auth_broken(name) || p.is_some_and(Profile::is_disabled)
}

/// [`walk_excluded`] plus canceled, for the UI-thread selection walks
/// (`next_target`, `fully_clear_target`) that read `Profile.usage` — kept fresh
/// by `App::apply_usage`. `is_canceled` reads that config-cached plan; the
/// daemon recovery walk can't (its `Profile.usage` is never written headless),
/// so it excludes canceled via the store-native `plan.is_canceled()` inside
/// `find_recovered_member` instead and must NOT call this.
fn candidate_excluded(config: &AppConfig, name: &str) -> bool {
    walk_excluded(config, name) || config.find(name).is_some_and(is_canceled)
}

/// Picks the next chain member to switch to, starting one slot after the active
/// profile and wrapping. Returns None when nothing is viable.
///
///   1. Any member with real headroom (5h utilization below threshold, or no
///      usage data fetched yet) that its own gates leave in rotation: the
///      weekly line per `check_weekly`, per-model weekly windows per
///      `check_scoped`.
///   2. Last resort: a member marked `last_resort`, accepted even while
///      exhausted. Claude Code shows its own "out of 5h limit" message on
///      arrival. `last_resort` is independent of `threshold` — a member can
///      still switch away at, say, 80% utilization and remain the chain's
///      last resort once nothing else has headroom.
///   3. Wrap-off only: no headroom, no `last_resort` member anywhere, and the
///      active profile itself exhausted → [`SwitchAction::Off`] to halt usage.
///
/// `active_burn_pct_per_hour` is the caller's in-memory 5h burn rate for the
/// active profile, forwarded to [`is_exhausted_active`] for step 3's
/// burn-aware projection; ignored unless `burn_aware_switching` is on. This
/// function never reads disk — callers must supply the rate themselves.
pub(crate) fn next_target(
    config: &AppConfig,
    active_burn_pct_per_hour: Option<f64>,
) -> Option<SwitchAction> {
    let active = config.state.active_profile.as_deref()?;
    let chain = &config.state.fallback_chain;
    let active_idx = chain.iter().position(|n| n == active)?;
    let len = chain.len();
    let weekly_pct = config.state.weekly_switch_threshold_pct();

    let skip = |i: usize| chain[i] == active || candidate_excluded(config, &chain[i]);
    let walk = |accept: &dyn Fn(&Profile) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| {
            config.find(&chain[i]).is_some_and(&accept)
        });
        pick.map(|i| chain[i].to_string())
    };

    // Headroom accept: clear on the aggregate gates AND — while the member's
    // own `check_scoped` gate is on — every per-model weekly window. A
    // "7d fable" at 100 strands a fable session even while the aggregate 7d
    // shows room, and the walk cannot know which model the next session will
    // drive, so a gate-on model-blocked member is out of rotation entirely
    // (its operator flips the gate off to keep it rotating for other models).
    // Both predicates read each member's own gates: `check_weekly` off drops
    // that member's soft weekly line, `check_scoped` off ignores its scoped
    // windows. Freshness stays a PREFERENCE, not a gate (2026-06-28 asymmetry
    // — the final pass runs at any freshness so an exhausted active still
    // escapes to a stale-but-viable member): prefer a candidate whose usage
    // read we TRUST (`fetch_status == Fresh`).
    let is_fresh = |p: &Profile| p.fetch_status == Some(FetchStatus::Fresh);
    let clear = |p: &Profile| !is_exhausted(p, weekly_pct) && !scoped_weekly_blocked(p, weekly_pct);
    if let Some(name) = walk(&|p| clear(p) && is_fresh(p)) {
        return Some(SwitchAction::To(name));
    }
    if let Some(name) = walk(&clear) {
        return Some(SwitchAction::To(name));
    }

    // A `last_resort` sink that still SERVES for free outranks paying. The
    // headroom passes gate on the SOFT weekly line, so a sink riding 98-99.99%
    // of its week reads "exhausted" to them yet still answers every request its
    // live 5h window allows — parking there keeps work going at zero cost, which
    // beats spending real money. Only a sink that can no longer serve (weekly at
    // the HARD cap, or 5h maxed) ranks BELOW spend, via the unconditional
    // `last_resort` walk further down.
    //
    // `active_is_last_resort` is hoisted here for this pass and the dead-sink
    // guard below. Active is a still-serving sink → already parked free, stay
    // put (the sink ping-pong guard, hard-cap flavored). A serving SIBLING sink
    // is chased only when the active is NOT itself a sink, preserving the
    // existing "once on a sink, don't hop to another" rule (a DEAD sink active
    // keeps its stay-put-or-spend behavior below).
    let active_is_last_resort = config.find(active).is_some_and(|p| p.last_resort);
    if active_is_last_resort && config.find(active).is_some_and(|p| !is_exhausted_hard(p)) {
        return None;
    }
    if !active_is_last_resort && let Some(name) = walk(&|p| p.last_resort && !is_exhausted_hard(p))
    {
        return Some(SwitchAction::To(name));
    }

    // Spend-armed members rank between a still-serving sink (above) and a DEAD
    // `last_resort` sink / wrap-off (below): every free-quota member and every
    // sink still serving for free was already passed over, so reaching here
    // paying is the only way to keep working — which an operator who set a
    // ceiling asked for. Opt-in twice over (see `spend_armed`), so stock chains
    // skip this.
    let spend_budget = config.state.spend_budget_switching;

    // An active that is ITSELF still within budget stays put — the same loop
    // guard `last_resort` needs, for the same reason: hopping between two paying
    // members buys nothing and relinks live credentials every tick. Everything
    // with free quota was already passed over above, so the choice here is only
    // "which account to pay", and the one already installed wins. An OVER-budget
    // active isn't armed, so it falls through to the halt gate below instead of
    // parking here.
    let active_is_spend_armed = config.find(active).is_some_and(|p| {
        spend_armed(
            p.usage.as_ref(),
            spend_budget,
            p.max_auto_spend.unwrap_or(0.0),
        )
    });
    if active_is_spend_armed {
        return None;
    }
    if let Some(name) = walk(&|p| {
        spend_armed(
            p.usage.as_ref(),
            spend_budget,
            p.max_auto_spend.unwrap_or(0.0),
        )
    }) {
        return Some(SwitchAction::To(name));
    }

    // Fall back to a DEAD `last_resort` sink only when the active profile is NOT
    // itself a sink. Two sinks switching to each other indefinitely gains
    // nothing — one migration is fine, but the next tick must stay put.
    // (`active_is_last_resort` hoisted above the serving-sink pass.)
    if active_is_last_resort {
        return None;
    }
    if let Some(name) = walk(&|p| p.last_resort) {
        return Some(SwitchAction::To(name));
    }

    // No headroom, no `last_resort` member anywhere. In wrap-off mode, turn off
    // all accounts — but only when the active profile is itself exhausted,
    // since this picker is also exercised on a healthy active. The ACTIVE
    // check is burn-aware (issue #8 follow-up b) so this agrees with
    // `next_auto_switch_target`'s scheduler-side gate; the candidate walk
    // above stays on the static `is_exhausted`. Weekly-wise `Off` keys on the
    // HARD cap (`WEEKLY_HARD_BLOCK_PCT`), not the soft line: a soft-blocked
    // active with real weekly room left stays put instead of signing every
    // running claude out over the tail it could still spend.
    //
    // Which flag answers that depends on WHAT ran out. An active that spent its
    // pay-as-you-go budget reads `switch_off_when_budget_spent` instead of `switch_off_when_spent`: `wrap
    // off` means "the chain is out of free quota", where staying costs nothing
    // but rate-limit errors, and this is the case where staying IS the spending.
    // Note the `last_resort` walk above already ran, so an over-budget active
    // parks on a sink when one exists — stopping the billing without signing
    // anyone out — and only reaches this halt with nowhere free to go.
    let halt_flag = if budget_spent(
        config.find(active).and_then(|p| p.usage.as_ref()),
        config.state.spend_budget_switching,
        config
            .find(active)
            .and_then(|p| p.max_auto_spend)
            .unwrap_or(0.0),
    ) {
        config.state.switch_off_when_budget_spent
    } else {
        config.state.switch_off_when_spent
    };
    if halt_flag
        && config.find(active).is_some_and(|p| {
            is_exhausted_active(
                p,
                config.state.burn_aware_switching,
                config.state.refresh_interval_ms,
                active_burn_pct_per_hour,
                weekly_hard_blocked(p),
                config.state.burn_switch_floor_pct(),
                config.state.burn_horizon_cap_ms(),
            )
        })
    {
        return Some(SwitchAction::Off);
    }
    None
}

/// Scheduler-side [`auto_switch_if_needed`]: same logic over an in-memory
/// [`ChainSnapshot`] taken under the config mutex, reading utilization from the
/// shared `UsageStore`. Returns the member to switch to, or `None`.
///
/// The store/config lock split is load-bearing: `App::apply_usage` locks
/// `usage_store` then `config`, so the scheduler must never hold `config` while
/// taking `usage_store`. Caller builds the snapshot under `config.lock()`, drops
/// the guard, then calls this.
///
/// The usage store is locked EXACTLY ONCE per evaluation, cloned into an owned
/// `HashMap<String, UsageInfo>`, and released before any pass runs. Every
/// predicate (headroom walk, serving-sink, spend-armed, halt) reads from that
/// same snapshot, so a fetch landing mid-evaluation (a concurrent
/// `App::apply_usage` acquiring `usage_store` between two predicate locks under
/// the old per-predicate-lock shape) cannot flip a free-sibling decision into a
/// paid one. The clone is bounded — one entry per profile.
pub(crate) fn next_auto_switch_target(
    snapshot: &ChainSnapshot,
    store: &UsageStore,
) -> Option<SwitchAction> {
    // One lock window — fail-safe to an empty map on poison, matching the
    // per-predicate shape's `Err(_) => false` paths in aggregate (every
    // predicate reads a missing entry the same way it reads no entry).
    let usage: HashMap<String, UsageInfo> = match store.lock() {
        Ok(s) => {
            #[cfg(test)]
            NEXT_AUTO_SWITCH_TARGET_STORE_LOCKS.with(|c| c.set(c.get() + 1));
            s.clone()
        }
        Err(_) => HashMap::new(),
    };
    next_auto_switch_target_with_usage(snapshot, &usage)
}

/// Snapshot-driven core of [`next_auto_switch_target`]: every predicate reads
/// from `usage`, the single per-evaluation clone of the `UsageStore` map. Split
/// out so tests can drive the evaluation against a frozen snapshot and prove
/// the decision depends only on its content, never on a later store mutation.
fn next_auto_switch_target_with_usage(
    snapshot: &ChainSnapshot,
    usage: &HashMap<String, UsageInfo>,
) -> Option<SwitchAction> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    // AUTH-4: an auth-broken active bypasses the exhaustion gate. Its fetches
    // can never succeed again, so its store entry is frozen at the last read —
    // usually a lapsed 5h window that reads as idle headroom — and requiring
    // exhaustion here wedged the daemon on the dead account while a viable
    // sibling idled (observed 2026-07-09). The flag is terminal-confirmed (set
    // only after a rejected refresh AND a failed live-mirror adopt), and the
    // walk below never consults the broken active's own usage.
    let active_broken = snapshot.broken.iter().any(|b| b == &active.name);
    // A kick-rejected active is broken's messages-limiter analogue: its usage
    // can read as idle headroom (`/usage` stays 200 through the outage) while
    // every inference request is rejected, so exhaustion can't be a
    // precondition for leaving it either. Unlike `broken` it is transient —
    // the block clears itself once a kick lands — so only the switch-grade
    // form (limiter-confirmed `rejected`, ≥2 kicks, ceiling ahead) reaches
    // this snapshot at all.
    let active_kick_rejected = snapshot.kick_rejected.iter().any(|k| k == &active.name);
    // A canceled active is `broken`'s subscription analogue: its cached usage
    // reads as idle headroom while `/v1/messages` 403s, so exhaustion can't gate
    // leaving it. Sourced from the usage snapshot (the plan the scheduler holds),
    // not a snapshot flag — unlike `broken`/`kick_rejected` it needs no separate
    // channel.
    let active_canceled = is_canceled_from_usage(&active.name, usage);
    // Judged at the active's own folded SOFT line (SCW-2) — the healthy-active
    // early return lives below the walk defs, so the scoped active trigger can
    // still hop a per-model-blocked healthy active.
    let active_exhausted = is_exhausted_active_from_usage(
        active,
        snapshot.burn_aware,
        snapshot.interval_ms,
        usage,
        active.weekly_line,
        snapshot.burn_floor_pct,
        snapshot.burn_horizon_cap_ms,
    );

    // Skip the active slot and any auth-broken member (AUTH-1) — the broken
    // exclusion covers the headroom pass AND the 100%-sink pass below.
    let skip = |i: usize| {
        snapshot.chain[i].name == active.name
            || snapshot.broken.iter().any(|b| b == &snapshot.chain[i].name)
            || snapshot
                .kick_rejected
                .iter()
                .any(|k| k == &snapshot.chain[i].name)
            || is_canceled_from_usage(&snapshot.chain[i].name, usage)
    };
    let walk = |accept: &dyn Fn(&ChainMember) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| accept(&snapshot.chain[i]));
        pick.map(|i| snapshot.chain[i].name.clone())
    };
    // Headroom accept, lockstep with [`next_target`]: clear on the aggregate
    // gates AND — per the member's own `check_scoped` gate — every per-model
    // weekly window (see `scoped_weekly_blocked_info`).
    let clear = |m: &ChainMember| {
        !is_exhausted_from_usage(m, usage, m.weekly_line) && !scoped_blocked_from_usage(m, usage)
    };

    if !active_broken && !active_kick_rejected && !active_canceled && !active_exhausted {
        // Scoped active trigger: a per-model weekly line crossed on an
        // otherwise-healthy active (its `check_scoped` gate on) hops ONLY
        // when a clear member exists. When every sibling is equally blocked
        // the hop buys nothing and the chain would ping-pong — stay put.
        // A pinned sink stays parked: `last_resort` means "serve here for
        // free until dead", and a scoped hop off it re-enters the rotation
        // it was deliberately parked out of — oscillating on the sibling's
        // 5h period. Same precedence the serving-sink pass gives it below.
        if !active.last_resort
            && scoped_blocked_from_usage(active, usage)
            && let Some(name) = walk(&clear)
        {
            return Some(SwitchAction::To(name));
        }
        return None;
    }

    // Freshness stays a preference, not a gate, lockstep with [`next_target`]
    // (2026-06-28 target asymmetry): prefer a member whose usage read we
    // TRUST (`snapshot.fresh`, the same StatusStore liveness `decision_fresh`
    // gates the ACTIVE on), but the any-freshness pass always runs.
    let is_fresh = |m: &ChainMember| snapshot.fresh.iter().any(|n| n == &m.name);
    if let Some(name) = walk(&|m| clear(m) && is_fresh(m)) {
        return Some(SwitchAction::To(name));
    }
    if let Some(name) = walk(&clear) {
        return Some(SwitchAction::To(name));
    }

    // Serving-sink pass, lockstep with [`next_target`]: a `last_resort` sink
    // with real headroom at the HARD weekly cap still serves every request for
    // free, so it outranks paying. The soft-line headroom passes above miss it.
    // Active being such a sink → already parked free, stay put; a serving
    // sibling sink is chased only when the active isn't itself a sink (the
    // "don't hop between sinks" rule — a DEAD sink active keeps its
    // stay-put-or-spend behavior below).
    let active_is_last_resort = active.last_resort;
    if active_is_last_resort && !is_exhausted_from_usage(active, usage, WEEKLY_HARD_BLOCK_PCT) {
        return None;
    }
    if !active_is_last_resort
        && let Some(name) =
            walk(&|m| m.last_resort && !is_exhausted_from_usage(m, usage, WEEKLY_HARD_BLOCK_PCT))
    {
        return Some(SwitchAction::To(name));
    }

    // Spend-armed pass, lockstep with [`next_target`]: between a serving sink
    // (above) and a DEAD sink / wrap-off (below), and the spend block rides the
    // store's `UsageInfo` exactly like utilization does. Same loop guard — an
    // active still within budget stays put rather than ping-ponging between two
    // paying members.
    let active_is_spend_armed = spend_armed(
        usage.get(&active.name),
        snapshot.spend_budget,
        active.max_spend,
    );
    if active_is_spend_armed {
        return None;
    }
    if let Some(name) =
        walk(&|m| spend_armed(usage.get(&m.name), snapshot.spend_budget, m.max_spend))
    {
        return Some(SwitchAction::To(name));
    }

    // Dead-sink fallback (`active_is_last_resort` hoisted above the serving-sink
    // pass): a sink active stays put, else the first sink anywhere parks it.
    if active_is_last_resort {
        return None;
    }
    if let Some(name) = walk(&|m| m.last_resort) {
        return Some(SwitchAction::To(name));
    }

    // No headroom, no `last_resort` member anywhere. In wrap-off mode, halt
    // all usage instead of staying on the spent profile — keyed on REAL
    // exhaustion only: a broken-but-unspent active stays put (AUTH-4), since
    // the live session's own Keychain chain may still be healthy and switching
    // off would log it out over a flag, not over spent quota. Same principle
    // weekly-wise: `active_exhausted` above (the switch trigger) honors the
    // soft line, but Off re-checks at the HARD cap
    // ([`WEEKLY_HARD_BLOCK_PCT`]) — a soft-blocked active with real weekly
    // room left stays put rather than signing everything out over the tail.
    // Budget-aware halt flag, lockstep with [`next_target`]: an active that
    // spent its pay-as-you-go budget reads `switch_off_when_budget_spent`, since staying on
    // it keeps costing money rather than merely erroring.
    let active_budget_spent = budget_spent(
        usage.get(&active.name),
        snapshot.spend_budget,
        active.max_spend,
    );
    let halt_flag = if active_budget_spent {
        snapshot.switch_off_when_budget_spent
    } else {
        snapshot.switch_off_when_spent
    };
    if halt_flag
        && is_exhausted_active_from_usage(
            active,
            snapshot.burn_aware,
            snapshot.interval_ms,
            usage,
            WEEKLY_HARD_BLOCK_PCT,
            snapshot.burn_floor_pct,
            snapshot.burn_horizon_cap_ms,
        )
    {
        return Some(SwitchAction::Off);
    }
    None
}

/// The walk for the ACTIVE-side per-model trigger — the first non-active,
/// non-broken member clear on the aggregate gates AND (per its own
/// `check_scoped` gate) every per-model weekly window. Same accept as
/// `next_target`'s headroom passes, without its sink/spend/halt tail: when
/// the hop's whole reason is a scoped window, a blocked target is no target.
fn fully_clear_target(config: &AppConfig, weekly_pct: f64) -> Option<String> {
    let active = config.state.active_profile.as_deref()?;
    let chain = &config.state.fallback_chain;
    let active_idx = chain.iter().position(|n| n == active)?;
    let skip = |i: usize| chain[i] == active || candidate_excluded(config, &chain[i]);
    let pick = walk_chain(active_idx, chain.len(), &skip, &|i| {
        config
            .find(&chain[i])
            .is_some_and(|p| !is_exhausted(p, weekly_pct) && !scoped_weekly_blocked(p, weekly_pct))
    });
    pick.map(|i| chain[i].to_string())
}

/// Find the first chain member whose utilization is below its threshold
/// (has recovered headroom after switch-off-all). Returns the member name.
/// Safe to call without holding the config lock — reads from [`UsageStore`].
/// `kick_rejected` members are never "recovered": their idle-looking usage is
/// exactly the shape a messages-limiter rejection freezes them in. A canceled
/// member is the same twin of that bug `next_auto_switch_target_with_usage`
/// already guards via `is_canceled_from_usage`: `/v1/messages` 403s no matter
/// how idle its cached window reads, so it never recovers either. Auth-broken
/// members carry no plan/usage marker to catch here — the caller filters them
/// out of `chain` before this runs (mirrors the `is_disabled` filter).
pub(crate) fn find_recovered_member(
    chain: &[ChainMember],
    store: &UsageStore,
    kick_rejected: &[String],
) -> Option<String> {
    let now = now_epoch_secs();
    let recovered = |member: &ChainMember, require_scoped_clear: bool| -> Option<bool> {
        if kick_rejected.iter().any(|k| k == &member.name) {
            return Some(false);
        }
        // A fetched entry whose 5h window is absent or past its reset is idle
        // headroom; a live window recovers only below the member's threshold.
        // An absent entry (never fetched) stays undecidable. A weekly-dead
        // member NEVER recovers — its 5h window lapsing every few hours is
        // exactly what made it look reborn while the 7d cap still blocks it.
        // Nor does a canceled one — its plan flips to `claude_free` while the
        // cached 5h window keeps reading idle.
        match store.lock() {
            Ok(s) => s.get(&member.name).map(|info| {
                !info.plan.as_ref().is_some_and(|p| p.is_canceled())
                    && !weekly_blocked_info(info, now, member.weekly_line)
                    && (!require_scoped_clear
                        || !member.check_scoped
                        || !scoped_weekly_blocked_info(info, now, member.scoped_line))
                    && (!five_hour_live(info, now)
                        || info
                            .five_hour
                            .as_ref()
                            .is_none_or(|w| w.utilization < member.threshold))
            }),
            Err(_) => None,
        }
    };
    // Prefer a member clear of every per-model weekly window it gates on,
    // then fall back to the aggregate-only recovery — the chain is OFF here,
    // so relinking onto a model-blocked member beats staying off, but only
    // as a last pick.
    for member in chain {
        if recovered(member, true) == Some(true) {
            return Some(member.name.clone());
        }
    }
    for member in chain {
        if recovered(member, false) == Some(true) {
            return Some(member.name.clone());
        }
    }
    None
}

/// If the active profile is a chain member past its threshold, switch to the
/// next viable member — or, in wrap-off mode when the whole chain is spent and
/// no sink exists, turn off all accounts. Returns the action taken, or None.
///
/// `active_burn_pct_per_hour` is the caller's in-memory burn rate for the
/// active profile (ignored unless burn-aware mode is on) — same contract as
/// [`next_target`], which this forwards it to.
pub(crate) fn auto_switch_if_needed(
    config: &mut AppConfig,
    active_burn_pct_per_hour: Option<f64>,
) -> Result<Option<SwitchAction>> {
    with_state_lock(|| {
        let Some(active_name) = config.state.active_profile.as_deref() else {
            return Ok(None);
        };
        if !config.state.fallback_chain.iter().any(|n| n == active_name) {
            return Ok(None);
        }
        let Some(active) = config.find(active_name) else {
            return Ok(None);
        };
        // AUTH-4 parity with the scheduler-side walk: an auth-broken active's
        // usage is frozen-stale (its fetches can't succeed), so exhaustion
        // cannot be a precondition for leaving it. A canceled active is the same
        // shape — a dead account whose cached window reads as idle headroom while
        // every request 403s — so it also bypasses the exhaustion gate.
        let weekly_pct = config.state.weekly_switch_threshold_pct();
        if !config.is_auth_broken(active_name)
            && !is_canceled(active)
            && !is_exhausted_active(
                active,
                config.state.burn_aware_switching,
                config.state.refresh_interval_ms,
                active_burn_pct_per_hour,
                weekly_blocked(active, weekly_pct),
                config.state.burn_switch_floor_pct(),
                config.state.burn_horizon_cap_ms(),
            )
        {
            // Scoped active trigger (parity with `next_auto_switch_target`):
            // a per-model weekly line crossed on an otherwise-healthy active
            // (its `check_scoped` gate on) hops ONLY onto a clear member —
            // hopping between equally model-blocked members buys nothing.
            // Parity with the scheduler walk: a pinned sink stays parked.
            if !active.last_resort
                && scoped_weekly_blocked(active, weekly_pct)
                && let Some(target) = fully_clear_target(config, weekly_pct)
            {
                switch_profile(config, &target)?;
                return Ok(Some(SwitchAction::To(target)));
            }
            return Ok(None);
        }

        let Some(action) = next_target(config, active_burn_pct_per_hour) else {
            return Ok(None);
        };

        match &action {
            SwitchAction::To(target) => switch_profile(config, target)?,
            SwitchAction::Off => switch_off(config)?,
        }
        Ok(Some(action))
    })
}

// ---------------------------------------------------------------------------
// CDX-4: the codex chain walk (PLAN.md §0.15) — session-boundary auto-switch
// off the passive signal. Reuses the claude walk's shapes where they map
// (ChainMember, wrapped scan order, the store-side exhaustion predicate,
// last_resort's one-migration rule) and drops the gates that don't: no
// decision_fresh (passive data self-invalidates via resets_at, and
// used_percent is monotone within a window so a stale read only
// UNDER-reports), no kick_rejected (no kicks), no `Off` action (logging the
// live codex out serves nothing — an exhausted account just errors).
// ---------------------------------------------------------------------------

/// Snapshot for the codex scan, built under the config lock by
/// [`snapshot_codex_chain`]; `leased`/`loginless` are IO-derived and filled by
/// the caller AFTER the lock drops (the `kick_rejected` pattern).
#[derive(Debug, Clone)]
pub(crate) struct CodexChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
    pub(crate) broken: Vec<String>,
    pub(crate) weekly_pct: f64,
    /// Members with a live isolated `clauth start` session — their chain
    /// lives in that session's CODEX_HOME (§0.14); installing the store
    /// snapshot would fork it. Walked around like `broken`.
    pub(crate) leased: Vec<String>,
    /// Members with no stored codex login — nothing to install.
    pub(crate) loginless: Vec<String>,
}

/// Snapshot the codex chain out of `AppConfig`. `None` for every degenerate
/// marker the scan must not act on: no codex active, an active that names a
/// deleted or claude profile (hand-edited state), an active outside the
/// chain, or an empty chain. Stray non-codex chain members are silently
/// skipped, mirroring `snapshot_chain`'s T1b tolerance.
pub(crate) fn snapshot_codex_chain(config: &AppConfig) -> Option<CodexChainSnapshot> {
    let active = config.state.active_codex_profile.as_deref()?.to_string();
    if !config.find(&active).is_some_and(|p| p.is_codex()) {
        return None;
    }
    let chain = &config.state.codex_fallback_chain;
    if !chain.iter().any(|n| n.as_str() == active) {
        return None;
    }
    let chain: Vec<ChainMember> = chain
        .iter()
        .filter(|name| config.find(name).is_some_and(|p| p.is_codex()))
        .map(|name| {
            let profile = config.find(name);
            ChainMember {
                name: name.to_string(),
                threshold: profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD),
                last_resort: profile.is_some_and(|p| p.last_resort),
                // Inert for codex members: the codex walk judges through its
                // own limiter/threshold fns, never these claude-weekly lines
                // or the claude spend passes.
                weekly_line: WEEKLY_HARD_BLOCK_PCT,
                scoped_line: WEEKLY_HARD_BLOCK_PCT,
                check_scoped: false,
                max_spend: 0.0,
            }
        })
        .collect();
    Some(CodexChainSnapshot {
        active,
        chain,
        broken: config
            .state
            .auth_broken
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
        weekly_pct: config.state.weekly_switch_threshold_pct(),
        leased: Vec::new(),
        loginless: Vec::new(),
    })
}

/// Codex's own limiter verdict, cross-checked against the named window's
/// `resets_at` (§0.16): `rate_limit_reached_type` says WHICH window rejected
/// the last request, and the verdict stands only while that window hasn't
/// reset. An unrecognized window name is checked against both windows —
/// trust the verdict while either could still be in force, self-clearing at
/// the later reset (never a permanent wedge).
fn codex_limiter_blocked(info: &crate::usage::UsageInfo, now_secs: i64) -> bool {
    match info.codex_rate_limit_reached.as_deref() {
        Some("primary") => crate::usage::five_hour_live(info, now_secs),
        Some("secondary") => crate::usage::seven_day_live(info, now_secs),
        Some(_) => {
            crate::usage::five_hour_live(info, now_secs)
                || crate::usage::seven_day_live(info, now_secs)
        }
        None => false,
    }
}

/// Whether one codex `UsageInfo` snapshot reads as exhausted at `threshold`:
/// the shared percent shape (weekly line, or live 5h window over threshold)
/// OR codex's own limiter verdict. The pure core shared by the store walk and
/// the CDX-5 proxy's per-account cache check.
pub(crate) fn codex_info_exhausted_at(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    threshold: f64,
    weekly_pct: f64,
) -> bool {
    codex_limiter_blocked(info, now_secs)
        || weekly_blocked_info(info, now_secs, weekly_pct)
        || (crate::usage::five_hour_live(info, now_secs)
            && info
                .five_hour
                .as_ref()
                .is_some_and(|w| w.utilization >= threshold))
}

/// [`codex_info_exhausted_at`] at the DEFAULT threshold — the CDX-5 proxy's
/// availability check (it has no per-member threshold snapshot).
pub(crate) fn codex_info_exhausted(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    weekly_pct: f64,
) -> bool {
    codex_info_exhausted_at(info, now_secs, DEFAULT_THRESHOLD, weekly_pct)
}

/// Store-side exhaustion for one codex profile. A poisoned store lock fails
/// safe to "not exhausted", same as the claude side.
fn codex_exhausted_from_store(
    name: &str,
    threshold: f64,
    store: &UsageStore,
    weekly_pct: f64,
) -> bool {
    let now = now_epoch_secs();
    match store.lock() {
        Ok(s) => s
            .get(name)
            .is_some_and(|info| codex_info_exhausted_at(info, now, threshold, weekly_pct)),
        Err(_) => false,
    }
}

/// The codex scan's decision: the next chain member to install, or `None`.
/// Fires only when the ACTIVE codex profile is exhausted (percent shape or
/// limiter verdict); candidates must be installable (not broken / leased /
/// loginless) and not exhausted themselves — a member with no data, or whose
/// cached windows have lapsed (`resets_at` passed ⇒ reset), is viable. Then
/// the `last_resort` pass under the same one-migration rule as the claude
/// walk. No `Off` arm by design (module note above).
pub(crate) fn next_codex_auto_switch_target(
    snapshot: &CodexChainSnapshot,
    store: &UsageStore,
) -> Option<String> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    let active_broken = snapshot.broken.iter().any(|b| b == &active.name);
    if !active_broken
        && !codex_exhausted_from_store(&active.name, active.threshold, store, snapshot.weekly_pct)
    {
        return None;
    }

    let skip = |i: usize| {
        let m = &snapshot.chain[i];
        m.name == snapshot.active
            || snapshot.broken.iter().any(|b| b == &m.name)
            || snapshot.leased.iter().any(|l| l == &m.name)
            || snapshot.loginless.iter().any(|l| l == &m.name)
    };
    let pick = walk_chain(active_idx, len, &skip, &|i| {
        let m = &snapshot.chain[i];
        !codex_exhausted_from_store(&m.name, m.threshold, store, snapshot.weekly_pct)
    });
    if let Some(i) = pick {
        return Some(snapshot.chain[i].name.clone());
    }

    // Last resort: accepted even while exhausted — but never from a
    // last-resort active (two sinks migrating to each other gains nothing).
    if active.last_resort {
        return None;
    }
    walk_chain(active_idx, len, &skip, &|i| snapshot.chain[i].last_resort)
        .map(|i| snapshot.chain[i].name.clone())
}

#[cfg(test)]
#[path = "../tests/inline/fallback.rs"]
mod tests;
