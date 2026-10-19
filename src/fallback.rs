use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::actions::{switch_off_locked, switch_profile_locked};
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile, ProfileName, WalkOrder};
use crate::usage::{
    FetchStatus, KickBlocks, LABEL_30D, PollStreaks, StatusStore, ThirdPartyStatusStore,
    ThirdPartyStreaks, ThirdPartyUsageStore, UsageInfo, UsageStore, UsageWindow, five_hour_live,
    iso_to_epoch_secs, long_window_live, now_epoch_secs,
};

// Test-only per-thread counter: increments each time an evaluator entry point
// (`next_auto_switch_target`) takes the
// `UsageStore` lock. The snapshot refactor takes it exactly once per
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

/// The evaluator's action plus whether the ACTIVE's key rejection was necessary
/// for exactly that action. `scan_auto_switch` records the key-rejection cause
/// only when [`AutoSwitchDecision::key_rejection_necessary`] is true, so a
/// repair invalidates only a switch the rejection actually produced while an
/// ordinary exhaustion/home move beside the same rejection still executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoSwitchDecision {
    pub(crate) action: SwitchAction,
    /// True when removing only the active from the key-rejected set, rebuilding
    /// the whole chain snapshot, yields no action or a different one.
    pub(crate) key_rejection_necessary: bool,
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

/// The one band a fallback threshold may hold: finite, 0..=100. The TUI's
/// text parser ([`parse_threshold`]) and the chain-edit action
/// (`actions::set_member_threshold`) both read it, so the two surfaces cannot
/// drift on the range.
pub(crate) const MIN_THRESHOLD: f64 = 0.0;
pub(crate) const MAX_THRESHOLD: f64 = 100.0;

pub(crate) fn threshold_in_range(value: f64) -> bool {
    value.is_finite() && (MIN_THRESHOLD..=MAX_THRESHOLD).contains(&value)
}

/// Parse a typed threshold, valid only inside [`threshold_in_range`].
pub(crate) fn parse_threshold(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok().filter(|v| threshold_in_range(*v))
}

/// Live 5h window for `profile`, or `None` when there's no snapshot yet or its
/// window isn't currently live (`five_hour_live`) — a lapsed or windowless
/// snapshot means the account has headroom again whatever its last-known
/// utilization says. Shared by [`is_exhausted_hard`], [`health_blocked_reason`]
/// and the burn-aware active check ([`is_exhausted_active_from_usage`]): each
/// refuses to act on a stale/rolled-over window the same way; they differ only
/// in how they judge a live one.
fn live_five_hour(usage: Option<&UsageInfo>) -> Option<&UsageWindow> {
    let usage = usage?;
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
pub(crate) fn budget_spent_blocking(
    config: &AppConfig,
    profile: &Profile,
    usage: Option<&UsageInfo>,
) -> bool {
    live_five_hour(usage).is_some_and(|w| w.utilization >= threshold_for(profile))
        && budget_spent(
            usage,
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
pub(crate) fn spend_is_uncapped(
    config: &AppConfig,
    ceiling: f64,
    key_rejected: &HashSet<ProfileName>,
    canceled: impl Fn(&ProfileName) -> bool,
) -> bool {
    config.state.spend_budget_switching
        && ceiling > 0.0
        && !config.state.switch_off_when_budget_spent
        // A parking spot only counts if the walk could actually send work
        // there: the same dead-member predicate (`candidate_excluded`) the
        // reachability checks share, read over the CHAIN rather than every
        // profile on disk. A sink that is disabled, auth-broken, key-rejected,
        // canceled, or was never added to the chain is unreachable, and
        // counting it reports "something will stop the billing" while nothing
        // can. `canceled` is the caller's own-context reading (the TUI's
        // per-frame snapshot, or disk for the daemon's boot warning).
        && !config.state.fallback_chain.iter().any(|name| {
            !candidate_excluded(config, name, key_rejected, canceled(name))
                && config.find(name).is_some_and(|p| p.last_resort)
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
/// with free quota has been passed over — see [`next_auto_switch_target_with_usage`].
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
/// [`weekly_line`]). Only the LONG window gates HERE — the aggregate 7d, else a
/// provider-marked monthly inference quota — while per-model `weekly_scoped`
/// limits (e.g. "7d fable") gate separately, per-account and never as full
/// exhaustion (see [`scoped_weekly_blocked_info`]). Store-side twin logic
/// inlines this same shape (see `is_exhausted_from_usage`).
pub(crate) fn weekly_blocked_info(
    info: &crate::usage::UsageInfo,
    now_secs: i64,
    weekly_pct: f64,
) -> bool {
    long_window_live(info, now_secs)
        && info
            .long_window()
            .is_some_and(|(_, w)| w.utilization >= weekly_pct)
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
pub(crate) fn weekly_blocked(
    profile: &Profile,
    chain_soft: f64,
    usage: Option<&UsageInfo>,
) -> bool {
    usage.is_some_and(|u| {
        weekly_blocked_info(u, now_epoch_secs(), member_weekly_line(profile, chain_soft))
    })
}

/// The account's LONG window (aggregate 7d, else a monthly inference quota)
/// at/over the API's refusal cap — dead for days, no per-account setting
/// consulted (see [`weekly_blocked`]).
pub(crate) fn weekly_hard_blocked(usage: Option<&UsageInfo>) -> bool {
    usage.is_some_and(|u| weekly_blocked_info(u, now_epoch_secs(), WEEKLY_HARD_BLOCK_PCT))
}

/// Whether one window carries a parseable reset still in the future — the
/// per-window liveness primitive [`scoped_weekly_blocked_info`] applies to
/// each `weekly_scoped` entry (mirroring `long_window_live` for the aggregate).
pub(crate) fn window_live(w: &UsageWindow, now_secs: i64) -> bool {
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
    worst_scoped_window_for(info, now_secs, weekly_pct, None)
}

/// [`worst_scoped_window`] narrowed to the model families a caller actually
/// cares about.
///
/// `None` weighs every per-model window, which is the chain walk's case and its
/// only correct one: it cannot know which model the next session runs, so any
/// capped window is disqualifying. A caller that DOES know — start-time
/// selection, which reads the models a launch will run — passes them, and a
/// member capped only on a family this session never touches stays a candidate.
///
/// Liveness and the comparison live here rather than at either call site, so
/// the narrowed judgment cannot drift from the blanket one.
pub(crate) fn worst_scoped_window_for<'a>(
    info: &'a crate::usage::UsageInfo,
    now_secs: i64,
    weekly_pct: f64,
    families: Option<&[String]>,
) -> Option<&'a crate::usage::ScopedWindow> {
    info.weekly_scoped
        .iter()
        .filter(|s| families.is_none_or(|fams| fams.iter().any(|f| scoped_label_is(&s.label, f))))
        .filter(|s| window_live(&s.window, now_secs) && s.window.utilization >= weekly_pct)
        .max_by(|a, b| a.window.utilization.total_cmp(&b.window.utilization))
}

/// The model family a scoped window is named for — `"fable"`, `"opus"`, … —
/// derived from a model id. Leading all-digit components are skipped and the
/// first alphabetic token is the family, so a `claude-3.x` id (its date and
/// version digits prefix the family) still resolves. A non-Anthropic id has no
/// family: no per-model window is ever scoped to one.
pub(crate) fn model_family(model_id: &str) -> Option<String> {
    let id = model_id.trim().to_lowercase();
    let base = crate::pricing::strip_bracket_suffix(&id);
    if base.is_empty() {
        return None;
    }
    // Bare aliases as Claude Code spells them in `settings.json` and `--model`,
    // a `[1m]` context suffix stripped above. `opusplan` is deliberately absent:
    // it is two families and resolves in `demand_from`, which can return both.
    if matches!(base, "opus" | "sonnet" | "haiku" | "fable") {
        return Some(base.to_owned());
    }
    let rest = base.strip_prefix("claude-")?;
    rest.split(['-', '.', '['])
        .find(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphabetic()))
        .map(str::to_owned)
}

/// Whether a scoped window's label names `family`.
///
/// Labels are built server-side as `"7d " + display_name.to_lowercase()`
/// ([`crate::usage::ScopedWindow`]), so this matches a word rather than a fixed
/// set — a model the server adds later joins with no change here. Any word, not
/// just the last: a display name can be more than one word (`"7d sonnet 4.5"`).
pub(crate) fn scoped_label_is(label: &str, family: &str) -> bool {
    label
        .split_whitespace()
        .any(|w| w.eq_ignore_ascii_case(family))
}

/// The union of model families a launch may run, deduped and order-stable.
///
/// `opusplan` and `best` each expand to both families they can resolve to. An
/// id that resolves to nothing (`default`, an unknown provider id) is dropped
/// rather than poisoning the set: an unresolved demand must not silently narrow
/// the candidates.
pub(crate) fn demand_from<I, S>(models: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for m in models {
        let raw = m.as_ref().trim().to_lowercase();
        let base = crate::pricing::strip_bracket_suffix(&raw);
        let families: Vec<String> = if base == "opusplan" {
            vec!["opus".to_owned(), "sonnet".to_owned()]
        } else if base == "best" {
            vec!["fable".to_owned(), "opus".to_owned()]
        } else {
            model_family(base).into_iter().collect()
        };
        for f in families {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    out
}

/// Store-side scoped gate: reads from the walk's one
/// usage snapshot, gated by the member's `check_scoped` toggle and judged at
/// its snapshot-resolved `scoped_line`. An absent entry fails safe to "not
/// blocked" (the member is then judged by the aggregate gates alone).
fn scoped_blocked_from_usage(member: &ChainMember, usage: &HashMap<String, UsageInfo>) -> bool {
    member.check_scoped
        && usage.get(member.name.as_str()).is_some_and(|info| {
            scoped_weekly_blocked_info(info, now_epoch_secs(), member.scoped_line)
        })
}

/// Store-side credit gate: reads from the walk's one usage snapshot, gated by
/// the member's `check_credit_cap` toggle. An absent entry fails safe to "not
/// blocked" (the member is then judged by the aggregate gates alone).
fn credit_capped_from_usage(member: &ChainMember, usage: &HashMap<String, UsageInfo>) -> bool {
    member.check_credit_cap
        && usage
            .get(member.name.as_str())
            .is_some_and(UsageInfo::credit_capped)
}

/// [`weekly_blocked_info`] + live-5h-over-threshold at the HARD weekly cap: can
/// this account literally serve right now? Sink passes, halt gates, and the
/// all-exhausted premise key on this — a member's soft-line override/gates
/// never soften it.
pub(crate) fn is_exhausted_hard(profile: &Profile, usage: Option<&UsageInfo>) -> bool {
    weekly_hard_blocked(usage)
        || live_five_hour(usage).is_some_and(|w| w.utilization >= threshold_for(profile))
}

/// True when `profile`'s last `/profile` read shows a canceled subscription: the
/// org dropped to `claude_free` while its cached 5h window still reads as idle
/// headroom, but `/v1/messages` 403s on every request. Kept a SEPARATE axis from
/// exhaustion (a canceled account is dead, not spent) so the chip wording and the
/// all-spent banner stay honest — but treated the same by the walk, which skips
/// it like `broken`/`kick_rejected`. Reads the plan off the caller's usage
/// snapshot (the per-frame one in the TUI, the store one in the scheduler);
/// a caller holding a name-keyed map uses [`is_canceled_from_usage`], one
/// holding no store or snapshot [`crate::profile_json::is_canceled_cached`].
pub(crate) fn is_canceled_from_info(usage: Option<&UsageInfo>) -> bool {
    usage
        .and_then(|u| u.plan.as_ref())
        .is_some_and(|p| p.is_canceled())
}

/// True when clauth has no way to read `profile`'s usage while something is
/// still serving sessions from it: the `claude setup-token` shape (#110), a
/// long-lived session token with no OAuth pair beside it. `/usage` needs that
/// pair's `user:profile` scope, so the scheduler never polls such a member, and
/// the walk, finding no reading, counts it as having headroom (see
/// [`no_usage_hazard`] for what that does once it is active).
///
/// The poll test is [`crate::usage::any_fetch_leg_admits`], the membership half
/// of the ONE gate the Overview's cached-reading mark reads, with
/// [`crate::usage::usage_poll_pair`] folded into it rather than asked here as a
/// second opinion. The two poll work-lists do not read that gate: each carries
/// its own filter, and the gate is the hoisted form of their membership rule,
/// held to them by `any_fetch_leg_polls_matches_the_work_lists_across_profile_shapes`.
/// So what this marker shares with the poller is the rule, not the call.
///
/// `usage_poll_pair` answers for an OAuth pair alone, so a member some OTHER
/// leg polls kept this marker up beside its reading; and when the #110
/// inference probe joins that rule, this marker retires itself with no change
/// here.
///
/// The membership half, and not the whole gate, because disabled-ness is a
/// different question with a different answer (#116 review): whether anything
/// polls this member is a question of its SHAPE, and a member the operator
/// switched off is the `Disabled` rung's to describe — including on the
/// Fallback card, which stacks this ladder under `[ disabled ]` rather than
/// stopping at it (`member_detail_stacks_the_health_pill_under_disabled`). Read
/// whole, the gate had this rung call every disabled member holding a login and
/// a long-lived token unmeasurable there, and [`no_usage_fix`] ask it for the
/// login it already holds — a shape normal operation reaches, since a setup
/// token beside a login is what that fix line leads to. One membership rule
/// still answers for this rung and the poller alike; only the axis that was
/// never this rung's went back to the rung that owns it.
///
/// `has_session_token` is the sidecar fact a `&Profile` cannot answer, since it
/// lives on disk: the caller supplies it so no render path stats a file per
/// frame (the TUI reads its per-frame `App::session_tokens` cache, the daemon
/// [`crate::claude::has_session_token`] once at boot). The long-lived state is
/// the one every switch keys on, expiry or not: an expired token stops serving,
/// it does not make the account measurable.
///
/// Three shapes are deliberately NOT this:
/// * a member with no login at all, which serves nothing and carries its own
///   logged-out state,
/// * an account carrying inference auth of its own, with or without an
///   endpoint: it spends a balance or a provider's own windows, and a window
///   nobody publishes has no line to cross. The test is
///   [`crate::claude::has_inference_auth`], the one
///   `build_claude_settings_json` wires the spawned `claude` from, and not
///   `api_key.is_some()`, which was wrong in BOTH directions: it cannot see a
///   key carried in the profile's own `[env]` (`ANTHROPIC_API_KEY` /
///   `ANTHROPIC_AUTH_TOKEN`), where the managed field is `None`, so such a
///   member got marked and told to add a login; and it counted a blank
///   `api_key` as a credential, which HID the marker on a member that
///   authenticates nothing with it,
/// * a member routed elsewhere by `[env] ANTHROPIC_BASE_URL`, whose requests
///   never reach the pool an Anthropic login would measure.
///   [`Profile::routing_endpoint`] reads both halves, where
///   [`Profile::is_oauth`] reads the managed field alone and would flag such a
///   member, then tell the operator to add a login whose windows the walk would
///   be reading for another pool's traffic.
///
/// Pure (no disk read), so a render path can ask it per frame.
pub(crate) fn no_usage_source(profile: &Profile, has_session_token: bool) -> bool {
    has_session_token
        && profile.routing_endpoint().is_none()
        && !crate::claude::has_inference_auth(profile)
        && !crate::usage::any_fetch_leg_admits(profile)
}

/// What a member without a usage source ([`no_usage_source`]) does to the
/// chain, and the one place that copy lives: shared by the add confirm, the
/// member card and the daemon boot warning so the three cannot drift apart.
///
/// Two ways in, and they differ. The exhaustion walk reaches such a member only
/// once no freshly read member is clear, because its fresh-first pass runs
/// before the any-clear one. The scoped hop has no fresh-first pass at all: a
/// per-model weekly line crossed on an otherwise-healthy active walks the clear
/// list in chain order, and an unmeasured member reads clear (no reading, so no
/// aggregate gate and no scoped window bites), so that hop lands on it ahead of
/// a fresh member with room.
///
/// Either way it stays: once it is active the scan finds no reading to act on
/// (`reading_is_actionable` wants a `Fresh` status, and nothing polls this
/// member), so only a dead login or the operator moves the chain off it.
///
/// This is the INVARIANT half, the same words on every surface. The lead in
/// front of it carries the tense, which differs per surface and per state:
/// [`no_usage_hazard_lead`] for a surface looking at one member, and the
/// daemon's own wording for a line that speaks about a list.
pub(crate) fn no_usage_hazard() -> &'static str {
    "clauth won't switch away from it on its own"
}

/// The lead that puts [`no_usage_hazard`] in the right tense for the member a
/// surface is showing. A run in a real terminal found the card saying "once
/// it's active" about the member that WAS active: the chain had walked onto it
/// after the account ahead of it went auth-broken. That is the state the whole
/// warning exists for, and it was the one state where the sentence read as a
/// hypothetical.
///
/// Kept beside the invariant rather than in the render layer so a third surface
/// cannot invent a third tense.
pub(crate) fn no_usage_hazard_lead(active: bool) -> &'static str {
    if active {
        "it's active, and "
    } else {
        "once it's active, "
    }
}

/// The one fix line for a member clauth cannot measure ([`no_usage_source`]):
/// the login that gives `/usage` a pair to poll, while sessions keep running on
/// the session token. Shared by the member card and the daemon boot line the
/// way [`uncapped_spend_fix`] is, so the two cannot drift; `names` carries every
/// member the line is about, which is why the daemon's line spells them out
/// instead of leaving a placeholder beside a list that just named them.
///
/// `clauth login <name>` and not `--setup-token`: the login has to be for the
/// SAME account the token was minted from, and nothing on the profile ties the
/// two together, so the line says which account rather than implying clauth
/// could check. The command sits in backticks, the form shared copy uses
/// ([`crate::tui::render::prose::mark_commands`] marks it where it renders).
pub(crate) fn no_usage_fix(names: &[&str]) -> String {
    let logins = names
        .iter()
        .map(|n| format!("`clauth login {n}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let (account, whose) = if names.len() == 1 {
        ("account", "its")
    } else {
        ("accounts", "their")
    };
    format!("log in the same {account} with {logins} so clauth can read {whose} usage")
}

/// Recent burn rate (%/h) for `name`'s 5h window: durable per-profile history
/// (`usage_history.jsonl`, a plain disk read — no shared lock, so this is safe
/// to call without touching the order in `lockorder.rs`, but never while
/// holding the `AppConfig` guard) plus the current live sample, run through
/// the same recency-weighted computation and windowing `App::active_burn_rate`
/// uses for the Overview ETA line. `None` until enough distinct samples exist.
/// Read by [`is_exhausted_active_from_usage`] alone, and only under
/// [`ActiveBurnRate::FromDisk`]; a [`ActiveBurnRate::Supplied`] caller hands
/// the walk its in-memory rate instead, so the render pass and the startup
/// one-shot never trigger this disk read under the config guard.
fn burn_rate_for_profile(name: &ProfileName, window: &UsageWindow) -> Option<f64> {
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

/// How the walk obtains the ACTIVE member's burn rate for the burn-aware
/// projection ([`is_exhausted_active_from_usage`]). The scheduler's scan reads
/// it from `usage_history.jsonl` on disk (after dropping its guards); the TUI's
/// caption and startup one-shot supply their in-memory `history_cache` rate —
/// no disk read while a config guard is held.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ActiveBurnRate {
    /// Read the rate from disk ([`burn_rate_for_profile`]) — the scheduler's
    /// path. Never while a config guard is held.
    FromDisk,
    /// A caller-supplied in-memory rate; `None` reads as "no burn history yet"
    /// and leaves the static threshold to decide.
    Supplied(Option<f64>),
}

/// Burn-aware exhaustion test shared by [`is_exhausted_active_from_usage`] and
/// [`is_exhausted_projected`] (issue #8 follow-up b, opt-in — off by
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
///     the look-ahead). Folded in [`projected_exhausted`] rather than in
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
            projected_exhausted(
                util_pct,
                threshold,
                rate,
                interval_ms,
                floor_pct,
                horizon_cap_ms,
            ) || util_pct >= threshold
        }
        None => util_pct >= threshold,
    }
}

/// The floor-guarded projection arm of [`is_exhausted_projected`], on its own.
///
/// Split out because the headroom nudge's gate (`hook_note.rs`) is exactly
/// this arm, over ITS OWN horizon — the seconds left until the window resets,
/// never the poll interval — and must not re-derive the guard. The static
/// half is deliberately not part of the nudge: for a rate-bearing window it
/// is subsumed here anyway (`util_pct >= threshold` implies the floor
/// conjunct), and the nudge emits only when the projection reaches the cap
/// before the reset — its approved copy claims the cap instant, so a static
/// fire whose projection misses the reset must stay silent.
pub(crate) fn projected_exhausted(
    util_pct: f64,
    threshold: f64,
    burn_pct_per_hour: f64,
    interval_ms: u64,
    floor_pct: f64,
    horizon_cap_ms: u64,
) -> bool {
    let horizon = interval_ms.min(horizon_cap_ms);
    util_pct >= floor_pct.min(threshold)
        && crate::usage::project_utilization(util_pct, burn_pct_per_hour, horizon) >= 100.0
}

/// Name + seconds-until-reset of the chain member that resumes soonest — the
/// all-exhausted caption's data source (issue #10: the implicit
/// resume-at-soonest-reset behavior, made explicit). Valid only when every
/// member that could actually be switched TO is currently exhausted, covering
/// both wrap-off's switch-off-all (active cleared) and wrap mode's
/// stalled-active equivalent (the walk returns `None` with every member
/// maxed). A dead member — disabled, auth-broken, or canceled
/// ([`candidate_excluded`]) — is never a switch candidate in the first
/// place, so its usage (however idle a cached window it holds) says nothing
/// about the chain's premise and is SKIPPED, not counted. An unresolvable
/// member is the one exception: it can't be proven exhausted at all, so it
/// still bails the WHOLE result to `None` rather than being skipped around
/// (see the loop's `config.find` below). Reuses
/// [`is_exhausted_hard`]'s wall-clock gates (`five_hour_live` and the weekly
/// `long_window_live` block) for every OTHER member: one exhausted in neither
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
pub(crate) fn soonest_resume(
    config: &AppConfig,
    key_rejected: &HashSet<ProfileName>,
    usage: &HashMap<String, UsageInfo>,
) -> Option<(String, i64)> {
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
        if candidate_excluded(
            config,
            name,
            key_rejected,
            is_canceled_from_usage(name, usage),
        ) {
            continue;
        }
        if !is_exhausted_hard(profile, usage.get(name.as_str())) {
            return None;
        }
        // A weekly-dead member resumes at its long-window reset (its 5h window
        // may be lapsed or absent entirely); anyone else at their next 5h reset.
        let usage = usage.get(name.as_str())?;
        let window = if weekly_blocked(profile, weekly_pct, Some(usage)) {
            usage.long_window().map(|(_, w)| w)?
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
    /// Third-party api KEY rejected by the provider (the credential its sessions
    /// spend): treated as auth-broken on every chain axis, but a re-login is the
    /// wrong fix — the key must be re-entered. A lapsed Alibaba console session
    /// is NOT this (its key still serves).
    KeyRejected,
    /// Long window (7d, else the provider-marked monthly quota) at/over the
    /// hard cap ([`WEEKLY_HARD_BLOCK_PCT`]) — dead until the
    /// window's reset. `resets_in` is seconds to that reset when the window
    /// carries a parseable `resets_at`; `monthly` names the monthly arm so the
    /// copy never calls a monthly-only block "weekly".
    WeeklySpent {
        resets_in: Option<i64>,
        monthly: bool,
    },
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
    /// Long window (7d, else the provider-marked monthly quota) over the soft
    /// switch line but under the hard cap: still serves
    /// every request, just won't be picked. `pct` is the current utilization;
    /// `monthly` names the monthly arm so the copy never calls a monthly-only
    /// block "weekly".
    WeeklySoft { pct: f64, monthly: bool },
    /// Last read wasn't live (cached / endpoint-429): the numbers are old, so the
    /// chain distrusts this member's apparent headroom.
    Stale,
    /// No reading at all, on a member clauth cannot measure
    /// ([`no_usage_source`]): the walk counts it as having headroom, so a switch
    /// here lands blind and nothing switches off it ([`no_usage_hazard`]).
    /// Lowest rung, because it blocks nothing and every rung above says more.
    /// Like `Stale` it is never fresh, so the EXHAUSTION walk takes it only in
    /// the pass that accepts any freshness, at its place in that pass's order.
    /// The scoped hop runs no freshness pass at all and can land on it ahead of
    /// a freshly read member with room (see [`no_usage_hazard`]).
    NoUsage,
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
/// ([`crate::usage::switch_grade_kick_lifts`]), else `None`. `key_rejected`
/// is the key-rejected member set the caller read once outside the config
/// guard (never a per-candidate disk read here). `session_token` is the same
/// kind of fact for the sidecar: whether a long-lived `session-token.json` is
/// what switches install for this member, which lives on disk and so is read
/// once per frame by the caller (`App::session_tokens`), never stat-ed per
/// candidate here.
pub(crate) fn blocked_reason(
    config: &AppConfig,
    profile: &Profile,
    kick_lift: Option<i64>,
    key_rejected: &HashSet<ProfileName>,
    usage: Option<&UsageInfo>,
    status: Option<FetchStatus>,
    session_token: bool,
) -> Option<BlockedReason> {
    // Disabled first, and only for a NON-active member: `snapshot_chain` and
    // the chain walks skip a disabled member as a CANDIDATE but deliberately never
    // drop a disabled ACTIVE one, so claiming a block on the active profile here
    // would be the second opinion this function must never be.
    if profile.is_disabled() && !config.is_active(&profile.name) {
        return Some(BlockedReason::Disabled);
    }
    health_blocked_reason(
        config,
        profile,
        kick_lift,
        key_rejected,
        usage,
        status,
        session_token,
    )
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
    key_rejected: &HashSet<ProfileName>,
    usage: Option<&UsageInfo>,
    status: Option<FetchStatus>,
    session_token: bool,
) -> Option<BlockedReason> {
    // Canceled subscription first (dead-first): a 403-ing account outranks a
    // login that could still refresh. Mirrors the walk's `is_canceled` skip.
    if is_canceled_from_info(usage) {
        return Some(BlockedReason::Canceled);
    }
    if config.is_auth_broken(&profile.name) {
        return Some(BlockedReason::AuthBroken);
    }
    // Key-rejected ranks with auth-broken (dead-first): a dead api KEY is a
    // dead account, not a quota/liveness block. Distinct chip + fix from
    // `AuthBroken` — a re-login does not fix a rejected key.
    if key_rejected.contains(&profile.name) {
        return Some(BlockedReason::KeyRejected);
    }
    let now = now_epoch_secs();
    // Weekly HARD cap first: dead for days regardless of the 5h window.
    if weekly_hard_blocked(usage) {
        let (monthly, resets_in) = usage
            .map(|u| u.long_window())
            .map(|l| {
                (
                    l.is_some_and(|(label, _)| label == LABEL_30D),
                    l.and_then(|(_, w)| reset_secs(w, now)),
                )
            })
            .unwrap_or((false, None));
        return Some(BlockedReason::WeeklySpent { resets_in, monthly });
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
    // member serves for free, the walk PREFERS it (headroom pass, the
    // clear-headroom accept) and never consults `budget_spent` for it — so flagging it there would claim
    // a block the walk doesn't act on. Gating on 5h-maxed also excludes the
    // last-resort serving sink, which by definition still has 5h headroom.
    let five_hour_over = live_five_hour(usage).filter(|w| w.utilization >= threshold_for(profile));
    if five_hour_over.is_some() {
        let ceiling = profile.max_auto_spend.unwrap_or(0.0);
        if budget_spent(usage, config.state.spend_budget_switching, ceiling) {
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
        && let Some(worst) =
            usage.and_then(|u| worst_scoped_window(u, now, member_scoped_line(profile, soft)))
    {
        return Some(BlockedReason::ScopedSpent {
            label: worst.label.clone(),
            pct: worst.window.utilization,
        });
    }
    if weekly_blocked(profile, soft, usage) {
        let pct = usage
            .and_then(|u| u.long_window())
            .map(|(_, w)| w.utilization)
            .unwrap_or(soft);
        let monthly =
            usage.is_some_and(|u| u.long_window().is_some_and(|(label, _)| label == LABEL_30D));
        return Some(BlockedReason::WeeklySoft { pct, monthly });
    }
    if matches!(status, Some(FetchStatus::Cached | FetchStatus::RateLimited)) {
        return Some(BlockedReason::Stale);
    }
    // Keyed on the SHAPE, not on the reading: no fetch leg's rule admits this
    // member (the test is `any_fetch_leg_admits`, the membership half of the
    // gate the Overview's cached-reading mark reads — the two work lists carry
    // their own filters and are held to that gate by its test, not by a call),
    // so a reading it carries is never a current one. A log-out does not leave
    // one behind either: `usage::forget` drops the half's name-keyed store
    // entries beside its on-disk cache (`forget_memory` / `forget_disk`, driven
    // by the `ForgetPlan` the change site builds), so the figure goes with the
    // login. Gating on `usage.is_none()` would still ask the wrong question —
    // what has been READ, rather than whether anything polls this member at
    // all.
    //
    // Lowest rung by design, and that is what keeps it honest: every rung above
    // reads a reading, so when one of them fires, the card and the walk are
    // acting on the same figure, and when none does, the walk is counting this
    // member as having room, which is the state the operator has to see.
    if no_usage_source(profile, session_token) {
        return Some(BlockedReason::NoUsage);
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
    pub(crate) name: ProfileName,
    pub(crate) threshold: f64,
    /// Mirrors `Profile::last_resort` — a terminal stop for the chain walk,
    /// decoupled from `threshold` (issue #8 follow-up: a threshold no longer
    /// doubles as a sink marker).
    pub(crate) last_resort: bool,
    /// Today's home account, resolved by [`AppConfig::is_home_today`] rather
    /// than read off `Profile::preferred`. On a day some profile names, every
    /// chain member naming it carries this; on an unclaimed day the bare flag
    /// decides and at most one does. Drives the return-to-preferred pass in
    /// [`next_auto_switch_target`] and preferred-wins wrap-off recovery in
    /// [`find_recovered_member`], both of which walk ALL carriers.
    pub(crate) preferred: bool,
    /// Mirrors `Profile::max_auto_spend` in dollars, `0` when unset — the
    /// member's own ceiling on unattended pay-as-you-go spending.
    pub(crate) max_spend: f64,
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
    /// Mirrors `Profile::check_credit_cap` — whether a spent usage-credit cap
    /// keeps this member out of the walk's target passes.
    pub(crate) check_credit_cap: bool,
}

/// In-memory snapshot of the fields `next_auto_switch_target` needs: active
/// profile name + ordered chain with each member's resolved threshold. Built
/// under the `AppConfig` mutex by [`snapshot_chain`], then evaluated lock-free.
#[derive(Debug, Clone)]
pub(crate) struct ChainSnapshot {
    pub(crate) active: ProfileName,
    pub(crate) chain: Vec<ChainMember>,
    /// Snapshot of `AppState::switch_off_when_spent` — drives the switch-off-all decision.
    pub(crate) switch_off_when_spent: bool,
    /// Snapshot of `AppState::auth_broken` — members whose OAuth refresh is
    /// revoked/invalid (AUTH-1). Excluded from every walk pass so a dead token
    /// is never installed unattended.
    pub(crate) broken: Vec<ProfileName>,
    /// Snapshot of `AppState::burn_aware_switching` (issue #8 follow-up b) —
    /// gates whether the ACTIVE-side check in `next_auto_switch_target`
    /// projects ahead of the next poll instead of using the static threshold.
    pub(crate) burn_aware: bool,
    /// Snapshot of `AppState::walk_order()` (issue #86) — which member each
    /// accept pass lands on. `Chain` is the default and today's walk byte for
    /// byte; `SoonestWeeklyReset` orders each pass by the soonest-resetting
    /// weekly window, `MostWeeklyHeadroom` by the least-used week. Orthogonal
    /// to `burn_aware`: burn-aware decides WHEN to leave the active, walk
    /// order WHERE to land.
    pub(crate) walk_order: WalkOrder,
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
    pub(crate) kick_rejected: Vec<ProfileName>,
    /// Members whose usage-reading channel is DEAD: deep-slot stuck
    /// `RateLimited` (the streak that already opens the scan bypass
    /// `reading_is_actionable`) with a windowless or absent store entry, so
    /// the live window this walk's exhaustion gate needs as evidence can
    /// never arrive (issue #83: a persistent `/usage` 429 writes no windows).
    /// Not config state — [`snapshot_chain`] leaves it empty and the
    /// scheduler's scans fill it from the status/streak/store triple (like
    /// `kick_rejected`). A dead-reading ACTIVE bypasses the exhaustion gate
    /// like `broken`/`kick_rejected`/`canceled` — windowless reads as
    /// never-exhausted, which would hold the walk on the member forever.
    /// Deliberately NOT a member holding any window at all: a lapsed or
    /// headroom window is a trustworthy last read the existing rules already
    /// judge (RLS-1). The codex twin stays empty — its usage leg is passive
    /// polling with no stuck-RateLimited concept.
    pub(crate) reading_dead: Vec<ProfileName>,
    /// Members whose last store read was live (`FetchStatus::Fresh`) — the same
    /// freshness `decision_fresh` gates the ACTIVE on. Not config state:
    /// [`snapshot_chain`] leaves it empty and the scheduler's scan fills it from
    /// the `StatusStore` (like `kick_rejected`). Drives the headroom walk's
    /// fresh-PREFERENCE first pass — a PREFERENCE, never a gate: when no fresh
    /// member has headroom the walk still accepts a stale-but-unexhausted one,
    /// so an exhausted active never loses its escape (2026-06-28 target
    /// asymmetry: the walk gates only the ACTIVE, never the target).
    pub(crate) fresh: Vec<ProfileName>,
}

/// Snapshot active profile + chain + per-member thresholds out of `AppConfig`.
/// Returns `None` when there's no active profile, the active isn't a chain
/// member, or the chain is empty — every case where `next_auto_switch_target`
/// short-circuits anyway, so callers can skip evaluation on `None`.
pub(crate) fn snapshot_chain(
    config: &AppConfig,
    key_rejected: &HashSet<ProfileName>,
) -> Option<ChainSnapshot> {
    let active = config.state.active_profile.as_ref().cloned()?;
    snapshot_chain_from(config, &active, key_rejected)
}

/// [`snapshot_chain`]'s codex twin, built from `codex-profiles.toml` instead of
/// `AppConfig`. Chains are strictly per-harness (decision 4), so this reads the
/// codex active slot, the codex chain, and the codex wrap-off — and nothing
/// from the claude state.
///
/// Every member takes the DEFAULTS a claude member would inherit from a missing
/// `Profile` record, because a codex profile has none: `profiles.toml` is
/// untouched by the file split, so `config.find` cannot answer for these names
/// and the per-profile knobs (last_resort, preferred, max_spend, per-member
/// thresholds) simply do not exist on this harness yet.
///
/// `check_scoped` and `check_credit_cap` are DISARMED for every codex member,
/// per the delivery spec: per-model weekly windows and the usage-credit cap
/// verdict are claude concepts (`"7d fable"` and friends come from the
/// anthropic `limits[]` array, the cap from its `spend`/`extra_usage` blocks),
/// and `wham/usage` has no equivalent. An armed gate would judge codex members
/// against windows or a cap that can never appear.
///
/// `broken` and `kick_rejected` are filled HERE, where the claude builder
/// leaves the second to the scheduler's scan: the codex verdicts live beside
/// each store (`codex_auth::read_quarantine`) and in the kick map, not in any
/// scheduler store, so this is the one place that can read them.
pub(crate) fn snapshot_codex_chain(
    state: &crate::codex_profiles::CodexState,
    interval_ms: u64,
) -> Option<ChainSnapshot> {
    let active = state.active_profile()?.clone();
    let chain: Vec<ProfileName> = state.fallback_chain().to_vec();
    if !chain.iter().any(|n| n == &active) {
        return None;
    }
    let weekly_pct = state.weekly_switch_threshold_pct();
    let broken = chain
        .iter()
        .filter(|name| crate::codex_auth::read_quarantine(name.as_str()).is_some())
        .cloned()
        .collect();
    let kick_rejected = chain
        .iter()
        .filter(|name| crate::codex_auth::kick_breaker_tripped(name.as_str()))
        .cloned()
        .collect();
    Some(ChainSnapshot {
        active,
        chain: chain
            .into_iter()
            .map(|name| ChainMember {
                name,
                threshold: DEFAULT_THRESHOLD,
                last_resort: false,
                preferred: false,
                max_spend: 0.0,
                weekly_line: weekly_pct,
                scoped_line: weekly_pct,
                check_scoped: false,
                check_credit_cap: false,
            })
            .collect(),
        switch_off_when_spent: state.switch_off_when_spent(),
        broken,
        burn_aware: false,
        // Codex stays on the stock chain-position walk (like `burn_aware`,
        // the codex snapshot takes the claude-side default; a codex profile
        // record has no knob to set this).
        walk_order: WalkOrder::Chain,
        interval_ms,
        burn_floor_pct: 0.0,
        burn_horizon_cap_ms: 0,
        spend_budget: false,
        switch_off_when_budget_spent: false,
        kick_rejected,
        reading_dead: Vec::new(),
        fresh: Vec::new(),
    })
}

/// [`snapshot_chain`] anchored on an explicit member instead of the global
/// active. The headroom nudge's replay (`hook_note::chain_would_act`) is the
/// one caller: its gate reads the account the session's credentials RESOLVE
/// to, which a pinned runtime session or a divergence can keep apart from
/// `active_profile`, and a walk anchored on the global active would answer
/// about a switch that never moves that session. Same `None` contract, same
/// walk — the anchor is the only difference.
pub(crate) fn snapshot_chain_from(
    config: &AppConfig,
    member: &ProfileName,
    key_rejected: &HashSet<ProfileName>,
) -> Option<ChainSnapshot> {
    if !config.state.fallback_chain.iter().any(|n| n == member) {
        return None;
    }
    Some(build_chain_snapshot(
        config,
        member.clone(),
        &|_| false,
        key_rejected,
    ))
}

/// [`snapshot_chain`] for ONE live session: the same chain, evaluated from the
/// member THAT session sits on rather than from the global active.
///
/// Two differences, and both are why the global builder cannot serve here:
///
/// - **No global active is required.** After a wrap-off switch-off-all there is
///   none, and that is precisely when a session most needs to move; gating on it
///   would leave every session with no decision, forever and silently.
/// - **A candidate the executor would refuse on config grounds is skipped**
///   ([`crate::runtime::swap_eligible`]). The walk picks the first clear member
///   after the session's, so a refusable one WEDGES it: `precondition` refuses
///   every tick, the intent never changes, and the session never reaches the next
///   viable member. A walk PREFERENCE in the exact sense `ChainSnapshot::fresh` is
///   one — the executor stays the safety gate, and the store-exists check and the
///   transport/platform refusals are deliberately not re-derived here.
///
/// `launch` is reconstructed from the session's `start_profile` as it reads NOW,
/// while what is live in the child's `process.env` is what `acquire` was handed —
/// so an operator editing that profile mid-session moves this view away from the
/// truth. A wrong guess either way costs a logged refusal or a stay-put, never a
/// burned chain.
pub(crate) fn snapshot_session_chain(
    config: &AppConfig,
    member: &ProfileName,
    launch: &crate::runtime::LaunchTransport,
    key_rejected: &HashSet<ProfileName>,
) -> Option<ChainSnapshot> {
    if !config.state.fallback_chain.iter().any(|n| n == member) {
        return None;
    }
    Some(build_chain_snapshot(
        config,
        member.clone(),
        &|profile| profile.is_none_or(|p| crate::runtime::swap_eligible(p, launch).is_err()),
        key_rejected,
    ))
}

/// One chain member resolved out of `config` for `name` — the fields
/// [`build_chain_snapshot`] folds per member and [`start_walk`] needs for a
/// single member, shared so the two cannot drift on a field.
fn chain_member(
    config: &AppConfig,
    name: &ProfileName,
    weekly_pct: f64,
    key_rejected: &HashSet<ProfileName>,
) -> ChainMember {
    let profile = config.find(name);
    ChainMember {
        name: name.clone(),
        threshold: profile.map(threshold_for).unwrap_or(DEFAULT_THRESHOLD),
        last_resort: profile.is_some_and(|p| p.last_resort),
        preferred: config.is_home_today(name, key_rejected),
        max_spend: profile.and_then(|p| p.max_auto_spend).unwrap_or(0.0),
        weekly_line: profile
            .map(|p| member_weekly_line(p, weekly_pct))
            .unwrap_or(weekly_pct),
        scoped_line: profile
            .map(|p| member_scoped_line(p, weekly_pct))
            .unwrap_or(weekly_pct),
        check_scoped: profile.is_none_or(|p| p.check_scoped),
        check_credit_cap: profile.is_none_or(|p| p.check_credit_cap),
    }
}

/// The shared body of [`snapshot_chain`] and [`snapshot_session_chain`],
/// parameterized on the name that plays "active" plus an extra per-candidate skip.
///
/// `skip_candidate` never applies to `active` itself: the walk locates that slot by
/// `position(|m| m.name == snapshot.active)`, so filtering it out returns `None`
/// every tick and wedges the decision on the one member it must leave.
fn build_chain_snapshot(
    config: &AppConfig,
    active: ProfileName,
    skip_candidate: &dyn Fn(Option<&Profile>) -> bool,
    key_rejected: &HashSet<ProfileName>,
) -> ChainSnapshot {
    let weekly_pct = config.state.weekly_switch_threshold_pct();
    let chain = config
        .state
        .fallback_chain
        .iter()
        // A disabled or unresolvable NON-active member is invisible to both
        // chain walks — dropped here rather than carried as a `ChainMember`
        // flag, so the freshness pass built from `.chain` below never considers
        // it either. The active slot must stay RESOLVABLE regardless of its own
        // `disabled` bit — `next_auto_switch_target` indexes into this vec by
        // `position(|m| m.name == snapshot.active)`, so dropping a disabled
        // active here would make that `?` return `None` every tick, wedging
        // auto-switch permanently instead of just skipping it as a candidate
        // (the walk's own skip closure never skips `chain[i] == active` either
        // — `disabled` and unresolvable are candidate-only exclusions, same as
        // `broken`/`canceled`).
        .filter(|name| {
            // Bound once: `find` is a linear scan and `skip_candidate`'s argument is
            // evaluated eagerly, so spelling it twice here charged the GLOBAL path a
            // third scan per member for a closure that is `|_| false` there.
            let profile = config.find(name);
            *name == active.as_str()
                || (profile.is_some()
                    && !profile.is_some_and(Profile::is_disabled)
                    && !skip_candidate(profile))
        })
        .map(|name| chain_member(config, name, weekly_pct, key_rejected))
        .collect();
    ChainSnapshot {
        active,
        chain,
        switch_off_when_spent: config.state.switch_off_when_spent,
        broken: config.state.auth_broken.clone(),
        burn_aware: config.state.burn_aware_switching,
        walk_order: config.state.walk_order(),
        interval_ms: config.state.refresh_interval_ms,
        burn_floor_pct: config.state.burn_switch_floor_pct(),
        burn_horizon_cap_ms: config.state.burn_horizon_cap_ms(),
        spend_budget: config.state.spend_budget_switching,
        switch_off_when_budget_spent: config.state.switch_off_when_budget_spent,
        kick_rejected: Vec::new(),
        reading_dead: Vec::new(),
        fresh: Vec::new(),
    }
}

/// [`is_exhausted_from_usage`]'s body over an already-resolved usage snapshot —
/// the aggregate half of the start walk's accept, shared with [`start_block`]
/// rather than a third spelling.
fn is_exhausted_from_info(
    member: &ChainMember,
    info: Option<&UsageInfo>,
    line: f64,
    now: i64,
) -> bool {
    info.is_some_and(|info| {
        weekly_blocked_info(info, now, line)
            || (five_hour_live(info, now)
                && info
                    .five_hour
                    .as_ref()
                    .is_some_and(|w| w.utilization >= member.threshold))
    })
}

/// Store-side exhaustion over a usage snapshot: reads 5h utilization
/// from a `HashMap<String, UsageInfo>` taken under a single `UsageStore` lock
/// (see [`next_auto_switch_target`]), judged at the caller's weekly `line`
/// (the member's snapshot-resolved one, or the literal hard cap). The snapshot
/// is taken once per evaluation so a fetch landing between this read and the
/// next pass cannot flip the decision.
fn is_exhausted_from_usage(
    member: &ChainMember,
    usage: &HashMap<String, UsageInfo>,
    line: f64,
) -> bool {
    is_exhausted_from_info(
        member,
        usage.get(member.name.as_str()),
        line,
        now_epoch_secs(),
    )
}

/// Scheduler-side canceled check over a usage snapshot — reads the plan from the
/// single-lock `UsageStore` clone, exactly like [`is_exhausted_from_usage`].
pub(crate) fn is_canceled_from_usage(
    name: &ProfileName,
    usage: &HashMap<String, UsageInfo>,
) -> bool {
    is_canceled_from_info(usage.get(name.as_str()))
}

/// Store-side ACTIVE exhaustion check over a usage snapshot: reads the 5h
/// window from the same single-lock snapshot as [`is_exhausted_from_usage`], so
/// the scheduler's periodic scan agrees with the TUI's caption and startup
/// one-shot on the ACTIVE decision. The snapshot is owned (cloned under the one
/// lock), so the disk-only burn rate lookup under [`ActiveBurnRate::FromDisk`]
/// runs with no store lock held — same property the pre-snapshot shape upheld
/// per predicate. [`ActiveBurnRate::Supplied`] uses the caller's in-memory rate
/// instead, so a caller holding no config guard and doing no disk read gets the
/// same projection.
#[allow(clippy::too_many_arguments)]
fn is_exhausted_active_from_usage(
    member: &ChainMember,
    burn_aware: bool,
    interval_ms: u64,
    usage: &HashMap<String, UsageInfo>,
    line: f64,
    floor_pct: f64,
    horizon_cap_ms: u64,
    third_party: bool,
    rate: ActiveBurnRate,
) -> bool {
    let now = now_epoch_secs();
    let Some(info) = usage.get(member.name.as_str()) else {
        return false;
    };
    // The weekly line trumps projection.
    if weekly_blocked_info(info, now, line) {
        return true;
    }
    let Some(window) = (five_hour_live(info, now).then_some(info.five_hour.as_ref()))
        .flatten()
        .cloned()
    else {
        return false;
    };
    if !burn_aware || third_party {
        // A third-party member's provider window has no burn history to pair
        // (no third-party leg appends `usage_history.jsonl`), so the static
        // threshold decides — never a cross-pool projection of the OAuth leg's
        // burn onto the provider window (maintainer ruling 2026-09-10).
        return window.utilization >= member.threshold;
    }
    let rate = match rate {
        ActiveBurnRate::FromDisk => burn_rate_for_profile(&member.name, &window),
        ActiveBurnRate::Supplied(r) => r,
    };
    is_exhausted_projected(
        window.utilization,
        member.threshold,
        rate,
        interval_ms,
        floor_pct,
        horizon_cap_ms,
    )
}

/// The soonest-reset ranking key for one member's cached usage: the epoch min
/// over the account's long window's `resets_at` and every `weekly_scoped`
/// entry's, `None` when no weekly window carries a parseable reset (an
/// idle/cold account). The 5h `resets_at` is deliberately NOT folded in — it
/// slides at `now + 5h` on idle windows and is not a ranking key (measured).
/// Scoped windows carry the aggregate's instant today, so the min is
/// future-proofing rather than a second source.
fn weekly_reset_key(info: &UsageInfo) -> Option<i64> {
    let mut best: Option<i64> = info
        .long_window()
        .and_then(|(_, w)| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs);
    for scoped in &info.weekly_scoped {
        if let Some(t) = scoped
            .window
            .resets_at
            .as_deref()
            .and_then(iso_to_epoch_secs)
        {
            best = Some(best.map_or(t, |b| b.min(t)));
        }
    }
    best
}

/// The most-weekly-headroom ranking key for one member's cached usage: its
/// live long-window utilization, then its live 5h utilization. A window past
/// its reset or carrying no parseable reset, and an absent 5h window, read as
/// unused: the same wall-clock rule the exhaustion predicates apply. `None`
/// when the reading holds no weekly window at all: an unknown week cannot be
/// proven emptier than a read one.
fn weekly_headroom_key(info: &UsageInfo, now_secs: i64) -> Option<(f64, f64)> {
    let (_, seven) = info.long_window()?;
    let seven_used = if long_window_live(info, now_secs) {
        seven.utilization
    } else {
        0.0
    };
    let five_used = match &info.five_hour {
        Some(window) if five_hour_live(info, now_secs) => window.utilization,
        _ => 0.0,
    };
    Some((seven_used, five_used))
}

/// One accept pass's candidate visit order under `mode`, the shared ordering
/// helper every fallback walk pass threads through (issue #86). It replaces
/// the old `walk_chain` primitive: the visit sequence is the same, the
/// skip/accept folding moved to each call site's `find`. `reading` is a
/// slot's cached usage, `None` when nothing has been read for it.
///
/// `WalkOrder::Chain` returns the old walk's exact visit sequence — every
/// slot starting one after `idx` and wrapping — so the default is byte for
/// byte today's walk. `WalkOrder::SoonestWeeklyReset` stable-sorts that
/// sequence by [`weekly_reset_key`] (soonest first; `None` — no parseable
/// weekly reset — after every `Some`, since an unknown reset cannot be
/// proven soon). `WalkOrder::MostWeeklyHeadroom` stable-sorts it by
/// [`weekly_headroom_key`] (least-used week first, 5h breaking a weekly tie;
/// no weekly window to read, or no reading at all, after every member that
/// has one). The stable sort keeps the chain-position walk order as the tie
/// rule: ties fall back to chain position, the exact tie rule
/// `soonest_resume` documents and pins. Skip predicates stay at the call
/// sites, unchanged — the mode reorders only WITHIN one accept pass, never
/// across the pass ladder.
fn ordered_walk<'u>(
    mode: WalkOrder,
    idx: usize,
    len: usize,
    reading: &dyn Fn(usize) -> Option<&'u UsageInfo>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (1..=len).map(|offset| (idx + offset) % len).collect();
    match mode {
        WalkOrder::Chain => {}
        WalkOrder::SoonestWeeklyReset => order.sort_by_key(|&i| {
            let k = reading(i).and_then(weekly_reset_key);
            (k.is_none(), k)
        }),
        WalkOrder::MostWeeklyHeadroom => {
            let now = now_epoch_secs();
            let keys: Vec<Option<(f64, f64)>> = (0..len)
                .map(|i| reading(i).and_then(|info| weekly_headroom_key(info, now)))
                .collect();
            order.sort_by(|&a, &b| match (keys[a], keys[b]) {
                (Some((a7, a5)), Some((b7, b5))) => a7.total_cmp(&b7).then(a5.total_cmp(&b5)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        }
    }
    order
}

/// Why a chain member is ineligible for a START (not a mid-session switch),
/// rendered as one label per arm. [`start_block`]'s ladder mirrors
/// [`health_blocked_reason`]'s precedence, minus the rungs a start does not
/// decide on (budget, staleness, no usage) and plus the two a start alone knows
/// (`NotOauth`, the narrowed per-model demand).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StartBlock {
    Disabled,
    AuthBroken,
    Canceled,
    KickRejected,
    NotOauth,
    WeeklySpent { monthly: bool },
    WeeklySoft { pct: f64, monthly: bool },
    FiveHour { pct: f64 },
    ScopedSpent { label: String, pct: f64 },
    CreditCap,
}

/// The worst reason `member` cannot be STARTED on, or `None` when it is clear.
///
/// `usage` is the member's resolved snapshot (absent when nothing has been read
/// yet), `kick_rejected` the switch-grade kick flag the start walk read off
/// disk, and `families` the models a launch is about to run: `Some` narrows the
/// per-model weekly judgment to exactly those families regardless of
/// `check_scoped` (a KNOWN demand supersedes the blanket gate), `None` keeps
/// today's blanket behaviour gated by `check_scoped`. `credit_billed` is the
/// set of model families Claude Code bills against usage credits, read once by
/// the walk — a known demand crossing it is refused whatever the credit gate
/// says, an unknown one only while the gate is on. `needs_oauth` is
/// `--with-fallback`, which refuses a member the chain could never move the
/// session off.
///
/// The aggregate decision is computed once by [`is_exhausted_from_info`] (the
/// walk's own predicate); the `WeeklySpent`/`FiveHour`/`WeeklySoft` rungs only
/// NAME which of its clauses fired, so none can fire while the predicate is
/// clear. `None` means the member is not excluded (disabled, canceled,
/// auth-broken, kick-rejected, non-OAuth under `--with-fallback`) AND not
/// exhausted AND scoped-clear AND credit-clear. Every rung but `NotOauth` is
/// one the switch walk also skips or refuses on, except the demand-narrowed
/// rungs (`ScopedSpent`, `CreditCap`) under a known demand; the active-slot
/// skip is the walk's alone, since a start has no active to leave.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_block(
    config: &AppConfig,
    member: &ChainMember,
    profile: &Profile,
    usage: Option<&UsageInfo>,
    kick_rejected: bool,
    needs_oauth: bool,
    families: Option<&[String]>,
    credit_billed: &[String],
) -> Option<StartBlock> {
    if profile.is_disabled() {
        return Some(StartBlock::Disabled);
    }
    if usage
        .and_then(|u| u.plan.as_ref())
        .is_some_and(|p| p.is_canceled())
    {
        return Some(StartBlock::Canceled);
    }
    if config.is_auth_broken(&member.name) || third_party_key_rejected(profile, &member.name) {
        return Some(StartBlock::AuthBroken);
    }
    if needs_oauth && !profile.is_oauth() {
        return Some(StartBlock::NotOauth);
    }
    let now = now_epoch_secs();
    let exhausted = is_exhausted_from_info(member, usage, member.weekly_line, now);
    if exhausted && usage.is_some_and(|info| weekly_blocked_info(info, now, WEEKLY_HARD_BLOCK_PCT))
    {
        let monthly = usage.is_some_and(|info| {
            info.long_window()
                .is_some_and(|(label, _)| label == LABEL_30D)
        });
        return Some(StartBlock::WeeklySpent { monthly });
    }
    if kick_rejected {
        return Some(StartBlock::KickRejected);
    }
    if exhausted
        && let Some(info) = usage
        && five_hour_live(info, now)
        && let Some(window) = info.five_hour.as_ref()
        && window.utilization >= member.threshold
    {
        return Some(StartBlock::FiveHour {
            pct: window.utilization,
        });
    }
    let scoped = match families {
        Some(fams) => usage
            .and_then(|info| worst_scoped_window_for(info, now, member.scoped_line, Some(fams))),
        None if member.check_scoped => {
            usage.and_then(|info| worst_scoped_window_for(info, now, member.scoped_line, None))
        }
        None => None,
    };
    if let Some(worst) = scoped {
        return Some(StartBlock::ScopedSpent {
            label: worst.label.clone(),
            pct: worst.window.utilization,
        });
    }
    // A spent usage-credit cap refuses a launch Claude Code would bill against
    // usage credits. A KNOWN demand decides by its own families whatever the
    // credit gate says; an unknown demand (no model resolved) is judged
    // credit-billed only while the gate is on — the scoped gate's blanket rule.
    // Keyed on `credit_capped()`, never a stale percent or a billing-off limit.
    if usage.is_some_and(UsageInfo::credit_capped) {
        let billed = match families {
            Some(fams) => fams.iter().any(|f| credit_billed.contains(f)),
            None => member.check_credit_cap,
        };
        if billed {
            return Some(StartBlock::CreditCap);
        }
    }
    if exhausted && usage.is_some_and(|info| weekly_blocked_info(info, now, member.weekly_line)) {
        let pct = usage
            .and_then(|info| info.long_window())
            .map(|(_, w)| w.utilization)
            .unwrap_or(member.weekly_line);
        let monthly = usage.is_some_and(|info| {
            info.long_window()
                .is_some_and(|(label, _)| label == LABEL_30D)
        });
        return Some(StartBlock::WeeklySoft { pct, monthly });
    }
    None
}

/// One chain member as the start walk judged it, with enough to render the
/// `--explain` row: the verdict, the cache age (for the staleness cell), whether
/// that age reads stale, and whether the reading is DATED and fresh (pass one's
/// preference). `demand` is the member's own model-family demand — the one its
/// per-model judgment ran under, printed when it is the pick; the refusal
/// header names it only when every judged member carries the same demand
/// ([`shared_demand`]).
pub(crate) struct StartCandidate {
    pub(crate) name: ProfileName,
    pub(crate) block: Option<StartBlock>,
    pub(crate) age: crate::profile_json::OauthAge,
    pub(crate) stale: bool,
    pub(crate) fresh: bool,
    pub(crate) demand: Vec<String>,
    /// The walk's reference instant (epoch seconds), so a DATED age's ISO-date
    /// arm can be derived as `now - age` without a second clock read.
    pub(crate) now: i64,
}

/// The `--auto` walk: every resolvable fallback-chain member in chain order,
/// each judged by [`start_block`] with the models THAT member's session will
/// run (its own runtime overlay plus the process-env pair passed in and the
/// passthrough args — see [`crate::start::member_launch_models`]), plus the
/// index of the first clear member (freshness PREFERRED in pass one, any
/// freshness accepted in pass two — the same preference the switch walk gives
/// its target in [`next_auto_switch_target`]). `env_models` is the process-env
/// model pair, read once by `cmd_start`, so this walk never reads the process
/// environment itself. Each pass follows the `walk_order` mode: chain position
/// by default, the soonest-resetting weekly window under `soonest-weekly-reset`,
/// the least-used week under `most-weekly-headroom`.
pub(crate) fn start_walk(
    config: &AppConfig,
    isolation: crate::runtime::Isolation,
    claude_args: &[String],
    env_models: [Option<String>; 2],
    needs_oauth: bool,
) -> (Vec<StartCandidate>, Option<usize>) {
    let weekly_pct = config.state.weekly_switch_threshold_pct();
    let now = now_epoch_secs();
    let walk_order = config.state.walk_order();
    let names = &config.state.fallback_chain;
    let kick = crate::usage::switch_grade_kick_blocked_from_cache(names, now);
    // The CLI has no refresher's live `ThirdPartyBroken` set, so the home
    // judgment (`chain_member.preferred`) reads the durable verdict — once,
    // not once per candidate.
    let key_rejected = durable_key_rejected(config);
    // Claude Code's credit-billed model families, read once for the whole walk —
    // a per-row read would re-parse `.claude.json` for every member.
    let credit_billed = crate::claude_json::credit_billed_families();
    // The outgoing profile's env keys strip the base settings before the member
    // overlay re-adds its own — the same set, builder and base choice the
    // runtime write uses, so the demand's settings-derived models are exactly
    // the runtime's `settings.json` models.
    let stale_env_keys = crate::actions::outgoing_env_keys(config);
    let mut rows: Vec<StartCandidate> = Vec::with_capacity(names.len());
    // The reading each row's walk-order ranking reads, built in the same loop
    // as the rows (parallel vec, same indices) so the walk below can order
    // them without widening `StartCandidate`'s public shape. Owned, because a
    // third-party row's reading lives only inside its loop iteration. Only
    // the ranking modes consult it, so under `chain` (the default) nothing is
    // copied and the walk is byte-identical.
    let mut readings: Vec<Option<UsageInfo>> = Vec::with_capacity(names.len());
    for name in names {
        let Some(profile) = config.find(name) else {
            continue;
        };
        let member = chain_member(config, name, weekly_pct, &key_rejected);
        let windows = crate::profile_json::profile_windows(profile);
        let third_party_usage = match &windows {
            crate::profile_json::ProfileWindows::ThirdParty { stats, .. } => {
                stats.as_ref().and_then(|s| s.to_usage_info())
            }
            _ => None,
        };
        let (usage, age) = match &windows {
            crate::profile_json::ProfileWindows::Oauth { usage, age } => (usage.as_deref(), *age),
            crate::profile_json::ProfileWindows::ThirdParty { age_secs, .. } => (
                third_party_usage.as_ref(),
                match age_secs {
                    Some(secs) => crate::profile_json::OauthAge::Dated(secs.saturating_mul(1000)),
                    None => crate::profile_json::OauthAge::Absent,
                },
            ),
        };
        let stale = windows.stale();
        let fresh = windows.fresh();
        let kick_rejected = kick.iter().any(|k| k == name);
        let demand = demand_from(crate::start::member_launch_models(
            profile,
            isolation,
            &stale_env_keys,
            claude_args,
            &env_models,
        ));
        let families = (!demand.is_empty()).then_some(demand.as_slice());
        let block = start_block(
            config,
            &member,
            profile,
            usage,
            kick_rejected,
            needs_oauth,
            families,
            &credit_billed,
        );
        readings.push(if walk_order == WalkOrder::Chain {
            None
        } else {
            usage.cloned()
        });
        rows.push(StartCandidate {
            name: name.clone(),
            block,
            age,
            stale,
            fresh,
            demand,
            now,
        });
    }
    if rows.is_empty() {
        return (rows, None);
    }
    let len = rows.len();
    let order = ordered_walk(walk_order, len - 1, len, &|i| readings[i].as_ref());
    let pick = order
        .iter()
        .copied()
        .find(|&i| rows[i].block.is_none() && rows[i].fresh)
        .or_else(|| order.iter().copied().find(|&i| rows[i].block.is_none()));
    (rows, pick)
}

/// The refusal header's model suffix demand: `Some` only when every judged
/// member carries the same non-empty set of model families (one member, or
/// members that agree) in any order — the same families still run whichever
/// order they arrive in. The first member's order is kept. Mixed demands — or
/// an empty walk — yield `None`, because naming one family would be false for
/// the members judged against another, so the header falls back to the
/// template's own no-demand form.
pub(crate) fn shared_demand(rows: &[StartCandidate]) -> Option<Vec<String>> {
    let first = rows.first()?;
    if first.demand.is_empty() {
        return None;
    }
    if rows.iter().skip(1).any(|row| {
        row.demand.len() != first.demand.len()
            || row.demand.iter().any(|f| !first.demand.contains(f))
    }) {
        return None;
    }
    Some(first.demand.clone())
}

/// The `--auto` start's pick: the picked member's name, the walk's rows, the
/// pick index, and the demand its pick/launch line prints.
pub(crate) type StartPick = (ProfileName, Vec<StartCandidate>, Option<usize>, Vec<String>);

/// The `--auto` start's pick-and-demand: walk the fallback chain, refuse when
/// no member has headroom, and return the picked member's name and the demand
/// its pick/launch line prints. Held as a function — not inline in `cmd_start`
/// — so the arm's `rows[pick]` choice is pinned at its real call site (see
/// `tests/inline/fallback.rs`), not a hand-built-rows stand-in. `env_models`
/// is the process-env model pair, read once by `cmd_start`, so this never
/// reads the process environment itself.
pub(crate) fn start_pick(
    config: &AppConfig,
    isolation: crate::runtime::Isolation,
    claude_args: &[String],
    env_models: [Option<String>; 2],
    follows_chain: bool,
) -> Result<StartPick> {
    anyhow::ensure!(
        !config.state.fallback_chain.is_empty(),
        "--auto picks from the fallback chain and it is empty; add accounts on the \
         fallback tab, or name one"
    );
    let (rows, pick) = start_walk(config, isolation, claude_args, env_models, follows_chain);
    let Some(pick) = pick else {
        let demand = shared_demand(&rows);
        anyhow::bail!(
            "{}",
            crate::format::start_refusal(demand.as_deref().unwrap_or(&[]), &rows)
        );
    };
    let name = rows[pick].name.clone();
    let demand = rows[pick].demand.clone();
    Ok((name, rows, Some(pick), demand))
}

/// Whether `profile`'s durable per-credential verdict names a dead api KEY (the
/// credential its sessions spend) — the third-party analogue of an OAuth
/// auth-broken member. A lapsed Alibaba console session (usage-only) is NOT
/// this: its api key still serves. Read off disk, so it works with no
/// refresher's live `ThirdPartyBroken` set in the process. It agrees with that
/// live set on a credential-change/live-body transition (a new key changes the
/// fingerprint; a live body clears the record); on a restart the live set is
/// empty and on a leg panic it is cleared while the durable record persists, so
/// the durable verdict is the authoritative read where no live set exists. Used
/// where no live set exists — the CLI's [`start_block`], the durable set
/// [`durable_key_rejected`] builds for the startup one-shot and the hook
/// replay, and the TUI's construction/reload seed for its live `ThirdPartyBroken`
/// mirror.
pub(crate) fn third_party_key_rejected(profile: &Profile, name: &ProfileName) -> bool {
    profile.console.is_none()
        && profile.provider != Some(crate::providers::Provider::Alibaba)
        && crate::usage::profile_credential_fingerprint(profile)
            .is_some_and(|fp| crate::profile_cache::auth_expired_matches(name, fp))
}

/// The key-rejected chain members read off the durable per-credential verdict —
/// the no-live-set form (the startup one-shot and the hook replay). One read per
/// keyed chain member, computed once, never per walk candidate.
pub(crate) fn durable_key_rejected(config: &AppConfig) -> HashSet<ProfileName> {
    let mut set = HashSet::new();
    for name in &config.state.fallback_chain {
        if let Some(p) = config.find(name)
            && third_party_key_rejected(p, name)
        {
            set.insert(name.clone());
        }
    }
    set
}

/// Config-side exclusions every fallback-chain walk applies before it weighs
/// usage headroom: an unresolvable, auth-broken, key-rejected, or user-disabled
/// member is never a candidate. Pure state (no usage read, no disk read) —
/// `key_rejected` is the key-rejected set the caller computed once outside the
/// config guard, so it holds in both the UI and daemon contexts. Shared by
/// `scan_recovery` and the render-side walks so their skip lists can't drift
/// apart. The `== active` guard is walk-structural and stays at the
/// call site; canceled is usage-derived and split by data source (see
/// `candidate_excluded`).
pub(crate) fn walk_excluded(
    config: &AppConfig,
    name: &ProfileName,
    key_rejected: &HashSet<ProfileName>,
) -> bool {
    let p = config.find(name);
    p.is_none()
        || config.is_auth_broken(name)
        || p.is_some_and(Profile::is_disabled)
        || key_rejected.contains(name)
}

/// Whether the chain walk would ever visit `name`: a member of the chain, and
/// past [`walk_excluded`].
///
/// The ONE eligibility behind `AppConfig::is_home_on` — an account the walk
/// cannot reach is home on no day, whether a list would claim it or the flag
/// would — and behind the day-list editor's saved-anyway warning, which needs
/// the same judgment in the reason form [`day_claim_blocker`] carries. Defined
/// in terms of that function rather than beside it, so the two cannot drift.
pub(crate) fn serves_the_chain(
    config: &AppConfig,
    name: &ProfileName,
    key_rejected: &HashSet<ProfileName>,
) -> bool {
    day_claim_blocker(config, name, key_rejected).is_none()
}

/// Why a day list on `name` claims nothing, phrased for the editor's
/// saved-anyway warning and its row hint; `None` when the account could
/// actually serve the days it names. The reason form of [`serves_the_chain`].
///
/// The gates are [`walk_excluded`]'s, plus the chain-membership test that
/// `AppConfig::is_home_on`'s claim scan runs ahead of it, ordered the way an
/// operator would fix them: put the account on the chain first, then get it
/// healthy. Only the first is reported — a list cannot be less inert for
/// clearing one of two blockers, and naming both reads as two problems.
pub(crate) fn day_claim_blocker(
    config: &AppConfig,
    name: &ProfileName,
    key_rejected: &HashSet<ProfileName>,
) -> Option<&'static str> {
    if config.find(name).is_some() && !config.state.fallback_chain.iter().any(|n| n == name) {
        return Some("it is not on the fallback chain");
    }
    walk_blocker(config, name, key_rejected)
}

/// [`walk_excluded`]'s gates in [`day_claim_blocker`]'s reason form, chain
/// membership aside: the `+ add` picker asks it of an account that is off the
/// chain by definition.
pub(crate) fn walk_blocker(
    config: &AppConfig,
    name: &ProfileName,
    key_rejected: &HashSet<ProfileName>,
) -> Option<&'static str> {
    let Some(profile) = config.find(name) else {
        return Some("no such account");
    };
    if profile.is_disabled() {
        return Some("the account is disabled");
    }
    if config.is_auth_broken(name) {
        return Some("its login is auth-broken");
    }
    if key_rejected.contains(name) {
        return Some("its api key was rejected");
    }
    None
}

/// [`walk_excluded`] plus canceled, for the render-side walks
/// (`soonest_resume`) and [`spend_is_uncapped`]. `canceled` is the caller's
/// own-context reading: the TUI's per-frame snapshot, or disk for the daemon's
/// boot warning. The daemon's recovery walk does not call this; it excludes
/// canceled off the store's plan inside `find_recovered_member`.
fn candidate_excluded(
    config: &AppConfig,
    name: &ProfileName,
    key_rejected: &HashSet<ProfileName>,
    canceled: bool,
) -> bool {
    walk_excluded(config, name, key_rejected) || canceled
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
/// One lock window over the `UsageStore` — fail-safe to an empty map on poison,
/// matching the per-predicate shape's `Err(_) => false` paths in aggregate
/// (every predicate reads a missing entry the same way it reads no entry).
pub(crate) fn usage_snapshot(store: &UsageStore) -> HashMap<String, UsageInfo> {
    match store.lock() {
        Ok(s) => {
            #[cfg(test)]
            NEXT_AUTO_SWITCH_TARGET_STORE_LOCKS.with(|c| c.set(c.get() + 1));
            s.clone()
        }
        Err(_) => HashMap::new(),
    }
}

/// The per-member choice behind the picker, shared by the walk's snapshot
/// builder ([`usage_snapshot_for_chain`]) and the TUI's `apply_usage`, so the
/// two cannot drift: a third-party-classified member takes its provider stats'
/// `to_usage_info()` (or none), any other member keeps the merged store's
/// entry. `third_party` is `Profile::usage_cache_is_third_party`, the same
/// predicate the disk readers key on.
pub(crate) fn picked_usage(
    third_party: bool,
    oauth: Option<&UsageInfo>,
    stats: Option<&crate::providers::ThirdPartyStats>,
) -> Option<UsageInfo> {
    if third_party {
        stats.and_then(crate::providers::ThirdPartyStats::to_usage_info)
    } else {
        oauth.cloned()
    }
}

/// The in-memory twin of `profile_json::windows_of`: the chain walk's usage
/// snapshot, with each member's windows picked from its own half — the OAuth
/// (or codex) reading in the merged store, or a third-party member's
/// provider-derived windows in the third-party store. `third_party_names` is
/// the set the caller derived from `Profile::usage_cache_is_third_party`, the
/// same predicate the disk readers key on, so no reader re-derives "third
/// party" from a neighbouring predicate. `polled` is the work-list subset of
/// `third_party_names`; a member outside it reads as no reading here, as
/// `6836c1a6`'s absent store entry did. `None` REMOVES the entry — a provider
/// that stopped publishing windows must not keep answering the walk with a
/// frozen figure. Each store is locked once and released before any pass runs, so
/// no two tracked locks are ever held together.
pub(crate) fn usage_snapshot_for_chain(
    store: &UsageStore,
    third_party: &ThirdPartyUsageStore,
    third_party_names: &HashSet<ProfileName>,
    polled: &HashSet<ProfileName>,
    members: &[ChainMember],
) -> HashMap<String, UsageInfo> {
    let mut usage = usage_snapshot(store);
    let third_party_map = third_party.lock().ok().map(|s| s.clone());
    for member in members {
        let is_third_party = third_party_names.contains(&member.name);
        // A display-only third-party name (disabled or keyless) keeps its cached
        // reading for the frame but no automatic walk reads it — the "polled
        // readings only" filter. It reads as no reading here, exactly as
        // 6836c1a6's absent store entry did.
        let picked = if is_third_party && !polled.contains(&member.name) {
            None
        } else {
            picked_usage(
                is_third_party,
                usage.get(member.name.as_str()),
                third_party_map
                    .as_ref()
                    .and_then(|s| s.get(member.name.as_str())),
            )
        };
        match picked {
            Some(info) => {
                usage.insert(member.name.to_string(), info);
            }
            None => {
                usage.remove(member.name.as_str());
            }
        }
    }
    usage
}

pub(crate) fn next_auto_switch_target(
    snapshot: &ChainSnapshot,
    store: &UsageStore,
) -> Option<SwitchAction> {
    next_auto_switch_target_with_usage(
        snapshot,
        &usage_snapshot(store),
        &HashSet::new(),
        ActiveBurnRate::FromDisk,
    )
}

/// Snapshot-driven core of [`next_auto_switch_target`]: every predicate reads
/// from `usage`, the single per-evaluation clone of the `UsageStore` map. Split
/// out so tests can drive the evaluation against a frozen snapshot and prove
/// the decision depends only on its content, never on a later store mutation.
#[cfg(test)]
pub(crate) fn next_auto_switch_target_for_test(
    snapshot: &ChainSnapshot,
    usage: &HashMap<String, UsageInfo>,
) -> Option<SwitchAction> {
    next_auto_switch_target_with_usage(snapshot, usage, &HashSet::new(), ActiveBurnRate::FromDisk)
}

/// The simple (no-cause) "with key rejection" evaluation: the active's effective
/// broken/preferred read straight off the snapshot, where the caller already
/// folded key rejection into `broken` and `preferred`. `third_party_names` is
/// the picker's classification, carried so the burn-aware active check never
/// pairs a third-party member's provider window with the OAuth burn history.
/// `rate` is the ACTIVE burn-rate source (see [`ActiveBurnRate`]).
pub(crate) fn next_auto_switch_target_with_usage(
    snapshot: &ChainSnapshot,
    usage: &HashMap<String, UsageInfo>,
    third_party_names: &HashSet<ProfileName>,
    rate: ActiveBurnRate,
) -> Option<SwitchAction> {
    next_auto_switch_target_core(snapshot, usage, third_party_names, rate)
}

/// Evaluate twice and compare: once on `with_snapshot` (key rejection folded
/// in), once on `without_snapshot` (the whole chain rebuilt with only the
/// active removed from the key-rejected set). When the two runs disagree, key
/// rejection was necessary for the exact action — an independently produced
/// exhaustion/scoped/home/overlap move comes out identical and carries no
/// cause.
pub(crate) fn next_auto_switch_decision_with_usage(
    with_snapshot: &ChainSnapshot,
    without_snapshot: &ChainSnapshot,
    usage: &HashMap<String, UsageInfo>,
    third_party_names: &HashSet<ProfileName>,
    rate: ActiveBurnRate,
) -> Option<AutoSwitchDecision> {
    let with = next_auto_switch_target_core(with_snapshot, usage, third_party_names, rate)?;
    let without = next_auto_switch_target_core(without_snapshot, usage, third_party_names, rate);
    let key_rejection_necessary = Some(&with) != without.as_ref();
    Some(AutoSwitchDecision {
        action: with,
        key_rejection_necessary,
    })
}

/// The walk body, reading the ACTIVE's effective broken and preferred straight
/// off `snapshot` — the caller hands it either the with-rejection snapshot or
/// the whole-chain counterfactual one, so no per-run facts are spliced in.
fn next_auto_switch_target_core(
    snapshot: &ChainSnapshot,
    usage: &HashMap<String, UsageInfo>,
    third_party_names: &HashSet<ProfileName>,
    rate: ActiveBurnRate,
) -> Option<SwitchAction> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    let active_broken = snapshot.broken.iter().any(|b| b == &snapshot.active);
    let active_preferred = active.preferred;
    // AUTH-4: an auth-broken active bypasses the exhaustion gate. Its fetches
    // can never succeed again, so its store entry is frozen at the last read —
    // usually a lapsed 5h window that reads as idle headroom — and requiring
    // exhaustion here wedged the daemon on the dead account while a viable
    // sibling idled (observed 2026-07-09). The flag is terminal-confirmed (set
    // only after a rejected refresh AND a failed live-mirror adopt), and the
    // walk below never consults the broken active's own usage.
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
    // A dead-reading active is the family's fourth member: its usage-reading
    // channel is dead (deep-stuck `RateLimited`, windowless or absent store
    // entry), so the live window this gate needs as evidence can never
    // arrive — windowless reads as never-exhausted, which held the walk on
    // the member forever while a viable sibling idled (issue #83). Unlike
    // the neighbors the account itself may be fine; what is dead is the
    // CHANNEL, and only a deep streak plus no window at all qualifies — a
    // lapsed or headroom window keeps the walk's own judgment, since the
    // last Fresh read is a trustworthy verdict the existing rules already
    // weigh.
    let active_reading_dead = snapshot.reading_dead.iter().any(|n| n == &active.name);
    let active_exhausted = is_exhausted_active_from_usage(
        active,
        snapshot.burn_aware,
        snapshot.interval_ms,
        usage,
        active.weekly_line,
        snapshot.burn_floor_pct,
        snapshot.burn_horizon_cap_ms,
        third_party_names.contains(&active.name),
        rate,
    );

    let skip = |i: usize| {
        snapshot.chain[i].name == active.name
            || snapshot.broken.iter().any(|b| b == &snapshot.chain[i].name)
            || snapshot
                .kick_rejected
                .iter()
                .any(|k| k == &snapshot.chain[i].name)
            || is_canceled_from_usage(&snapshot.chain[i].name, usage)
    };
    let reading = |i: usize| usage.get(snapshot.chain[i].name.as_str());
    let order = ordered_walk(snapshot.walk_order, active_idx, len, &reading);
    let walk = |accept: &dyn Fn(&ChainMember) -> bool| -> Option<String> {
        order
            .iter()
            .copied()
            .find(|&i| !skip(i) && accept(&snapshot.chain[i]))
            .map(|i| snapshot.chain[i].name.to_string())
    };
    // Headroom accept: clear on the aggregate
    // gates AND — per the member's own `check_scoped` gate — every per-model
    // weekly window (see `scoped_weekly_blocked_info`).
    let clear = |m: &ChainMember| {
        !is_exhausted_from_usage(m, usage, m.weekly_line)
            && !scoped_blocked_from_usage(m, usage)
            && !credit_capped_from_usage(m, usage)
    };

    if !active_broken
        && !active_kick_rejected
        && !active_canceled
        && !active_reading_dead
        && !active_exhausted
    {
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
        // Return-to-preferred: a healthy active that is not itself the operator's
        // preferred (home) account walks back the instant preferred reads clear
        // AND fresh. No dwell timer, no hysteresis — it is self-limiting: had
        // preferred stayed clear this whole time, the exhaustion walk could only
        // ever have left it when preferred was the SPENT member, so a clear
        // preferred that is not the active means work drifted off it on
        // preferred's own exhaustion and can now come home. The one-shot nature
        // is structural, not timed: the moment it fires, the active IS preferred
        // and the `pref != active.name` guard bars any re-fire.
        //
        // Freshness is a HARD gate on BOTH sides here, unlike the exhaustion
        // walk below where target freshness is only a PREFERENCE (2026-06-28
        // asymmetry). That exemption exists to never strand an EXHAUSTED active
        // without an escape; a HEALTHY active has no forced escape to preserve,
        // so it does not transfer. A stale/Cached preferred (hydrated idle at
        // the last read but really near-blocked) must not pull live work — and
        // every following session — onto a near-dead account; and a
        // stuck-RateLimited active, frozen at an idle-looking read and therefore
        // never in `snapshot.fresh`, must not be moved on numbers the scheduler
        // declared untrustworthy (mirrors the ACTIVE-side `decision_fresh` gate).
        //
        // No `!active.last_resort` guard here, unlike the scoped trigger above:
        // a `last_resort` sink is a refuge ("serve here for free until dead"),
        // not a permanent park — the operator's home account outranks it when
        // home is clear and fresh.
        // Every home member is a candidate, not just the first. A day list can
        // name more than one account, and on a claimed day they all carry the
        // marker; taking `find`'s first and then failing the gates below would
        // bail the whole return pass while a later lister sits clear.
        if snapshot.fresh.iter().any(|n| n == &active.name)
            && let Some(pref) = snapshot
                .chain
                .iter()
                .enumerate()
                .filter(|(_, m)| m.preferred)
                .find(|(pi, m)| {
                    m.name != active.name
                        && !skip(*pi)
                        && clear(m)
                        && snapshot.fresh.iter().any(|n| n == &m.name)
                })
                .map(|(_, m)| m)
        {
            return Some(SwitchAction::To(pref.name.to_string()));
        }
        // Re-computed home (issue #86, walk-order mode): with no `preferred`
        // member anywhere in the chain, `soonest-weekly-reset` walks a
        // healthy active "home" to the soonest-reset clear+fresh member —
        // the operator's home recomputed as "where the quota expires
        // first", so work parks where it will be spent rather than where it
        // will expire. The walk is one-shot in the same structural sense as
        // the return-to-preferred pass: the already-home guard below bars
        // any re-fire once the active IS the soonest member. The same hard
        // freshness gates as the return-to-preferred pass above (fresh
        // ACTIVE and fresh target), and the same absence of a `last_resort`
        // guard: a sink is a refuge, not a permanent park, and the
        // recomputed home outranks it. The ordering rides the shared
        // `order` (the soonest-reset sequence with the chain-position tie
        // rule). `chain` (the default) has no home pass — a healthy active
        // stays put, byte-identical to today — and neither has
        // `most-weekly-headroom`: utilization moves with every poll, so a
        // home re-ranked by it would hop between near-equal members as they
        // burn (the #86 objection to a headroom order); it ranks hop targets
        // only. A chain that DOES carry `preferred` keeps
        // preferred's behavior exactly whatever the mode (the guard above).
        // Already-home guard (the `pref != active.name` analog): the walk
        // fires only toward a clear+fresh member whose weekly reset is
        // STRICTLY sooner than the active's own — the active qualifies as a
        // home itself, so once the chain lands on the soonest member no
        // candidate is sooner and it parks. Without the guard a second
        // clear+fresh member ping-pongs the walk every tick (the active is
        // skipped as a candidate, so the walk always finds a "home" to walk
        // TO — one credential relink per scheduler tick, forever). Ties with
        // the active keep the current position: walking between tied members
        // buys nothing and oscillates the same way. A candidate with no
        // parseable reset never outranks a Some active, and an active with
        // none defers to any candidate that has one.
        let active_key = usage.get(active.name.as_str()).and_then(weekly_reset_key);
        if snapshot.walk_order == WalkOrder::SoonestWeeklyReset
            && !snapshot.chain.iter().enumerate().any(|(i, m)| {
                if i == active_idx {
                    active_preferred
                } else {
                    m.preferred
                }
            })
            && snapshot.fresh.iter().any(|n| n == &active.name)
            && let Some(pi) = order.iter().copied().find(|&i| {
                !skip(i)
                    && clear(&snapshot.chain[i])
                    && snapshot.fresh.iter().any(|n| n == &snapshot.chain[i].name)
                    && reading(i)
                        .and_then(weekly_reset_key)
                        .is_some_and(|candidate_key| active_key.is_none_or(|a| candidate_key < a))
            })
        {
            return Some(SwitchAction::To(snapshot.chain[pi].name.to_string()));
        }
        return None;
    }

    // Freshness stays a preference, not a gate
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

    // Serving-sink pass: a `last_resort` sink
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

    // Spend-armed pass: between a serving sink
    // (above) and a DEAD sink / wrap-off (below), and the spend block rides the
    // store's `UsageInfo` exactly like utilization does. Same loop guard — an
    // active still within budget stays put rather than ping-ponging between two
    // paying members.
    let active_is_spend_armed = spend_armed(
        usage.get(active.name.as_str()),
        snapshot.spend_budget,
        active.max_spend,
    );
    if active_is_spend_armed {
        return None;
    }
    if let Some(name) = walk(&|m| {
        !credit_capped_from_usage(m, usage)
            && spend_armed(
                usage.get(m.name.as_str()),
                snapshot.spend_budget,
                m.max_spend,
            )
    }) {
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
    // Budget-aware halt flag: an active that
    // spent its pay-as-you-go budget reads `switch_off_when_budget_spent`, since staying on
    // it keeps costing money rather than merely erroring.
    let active_budget_spent = budget_spent(
        usage.get(active.name.as_str()),
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
            third_party_names.contains(&active.name),
            rate,
        )
    {
        return Some(SwitchAction::Off);
    }
    None
}

/// Find the first chain member whose utilization is below its threshold
/// (has recovered headroom after switch-off-all), in `walk_order` order
/// within each pass. Returns the member name.
/// Safe to call without holding the config lock — reads from [`UsageStore`].
/// `kick_rejected` members are never "recovered": their idle-looking usage is
/// exactly the shape a messages-limiter rejection freezes them in. A canceled
/// member is the same twin of that bug `next_auto_switch_target_with_usage`
/// already guards via `is_canceled_from_usage`: `/v1/messages` 403s no matter
/// how idle its cached window reads, so it never recovers either. Auth-broken
/// members carry no plan/usage marker to catch here — the caller filters them
/// out of `chain` before this runs (mirrors the `is_disabled` filter).
#[cfg(test)]
pub(crate) fn find_recovered_member_for_test(
    chain: &[ChainMember],
    store: &UsageStore,
    kick_rejected: &[ProfileName],
    walk_order: WalkOrder,
) -> Option<String> {
    find_recovered_member(chain, &usage_snapshot(store), kick_rejected, walk_order)
}

/// The recovery walk over an already-resolved usage snapshot — the scheduler's
/// recovery scan builds the picker snapshot once ([`usage_snapshot_for_chain`])
/// and shares it with the dead-reading pass.
pub(crate) fn find_recovered_member(
    chain: &[ChainMember],
    usage: &HashMap<String, UsageInfo>,
    kick_rejected: &[ProfileName],
    walk_order: WalkOrder,
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
        usage.get(member.name.as_str()).map(|info| {
            !info.plan.as_ref().is_some_and(|p| p.is_canceled())
                && !weekly_blocked_info(info, now, member.weekly_line)
                && (!require_scoped_clear
                    || !member.check_scoped
                    || !scoped_weekly_blocked_info(info, now, member.scoped_line))
                && (!require_scoped_clear || !member.check_credit_cap || !info.credit_capped())
                && (!five_hour_live(info, now)
                    || info
                        .five_hour
                        .as_ref()
                        .is_none_or(|w| w.utilization < member.threshold))
        })
    };
    // The operator's preferred (home) account wins recovery outright: if it is
    // among the recovered members, re-arm onto it rather than let the
    // scoped-clear pass below hand recovery to a lower-priority member that
    // merely cleared its per-model windows first. The caller has already gated
    // `chain` on `decision_fresh_any`, so a preferred reaching here is a trusted
    // read; `recovered(_, false)` matches "recovered on the aggregate" — home
    // beats staying off even while a per-model window still gates it. Walk
    // order never outranks it: the mode orders only the passes past it.
    // Same as the return pass: all home members are candidates, so a first
    // lister that has not recovered does not hide a later one that has.
    if let Some(pref) = chain
        .iter()
        .filter(|m| m.preferred)
        .find(|m| recovered(m, false) == Some(true))
    {
        return Some(pref.name.to_string());
    }
    // Prefer a member clear of every per-model weekly window it gates on,
    // then fall back to the aggregate-only recovery — the chain is OFF here,
    // so relinking onto a model-blocked member beats staying off, but only
    // as a last pick. Each pass visits its members in walk-order mode
    // (`chain` keeps today's vector order).
    let order = ordered_walk(
        walk_order,
        chain.len().saturating_sub(1),
        chain.len(),
        &|i| usage.get(chain[i].name.as_str()),
    );
    for i in order.iter().copied() {
        if recovered(&chain[i], true) == Some(true) {
            return Some(chain[i].name.to_string());
        }
    }
    for i in order {
        if recovered(&chain[i], false) == Some(true) {
            return Some(chain[i].name.to_string());
        }
    }
    None
}

// ── test-only decision/dispatch rendezvous ───────────────────────────────────
//
// `auto_switch_if_needed`'s decision and dispatch must share one state-flock
// hold; this seam is what makes that falsifiable. A test arms it with the
// action it expects the automatic actor to decide; when the actor reaches the
// decision/dispatch boundary with exactly that decision in hand it reports and
// parks until the controller releases it — under whatever locks the production
// path holds at that point. Scheduling only: the decision, the dispatch fns,
// and every persisted value are the real ones. Fires at most once per arming
// and only on an exact action match, so every unarmed caller passes through
// untouched.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct AutoDecisionDone {
    pub(crate) action: SwitchAction,
}

#[cfg(test)]
struct AutoDecisionRendezvous {
    action: SwitchAction,
    reached: std::sync::mpsc::Sender<AutoDecisionDone>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static AUTO_DECISION_RENDEZVOUS: std::sync::Mutex<Option<AutoDecisionRendezvous>> =
    std::sync::Mutex::new(None);

/// Arm the rendezvous for `action`: the next matching decision reports on
/// `reached` and parks until a send on `release`. One rendezvous at a time;
/// tests arming one hold a `HomeSandbox`, which serializes them.
#[cfg(test)]
pub(crate) fn install_auto_decision_rendezvous(
    action: SwitchAction,
) -> (
    std::sync::mpsc::Receiver<AutoDecisionDone>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut slot = AUTO_DECISION_RENDEZVOUS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        slot.is_none(),
        "only one auto-decision rendezvous may be armed"
    );
    *slot = Some(AutoDecisionRendezvous {
        action,
        reached: reached_tx,
        release: release_rx,
    });
    (reached_rx, release_tx)
}

/// The boundary's park: report the decision, then hold the actor here until
/// the controller releases the dispatch.
#[cfg(test)]
fn auto_decision_rendezvous(action: &SwitchAction) {
    let rendezvous = {
        let mut slot = AUTO_DECISION_RENDEZVOUS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(hook) if &hook.action == action => slot.take(),
            _ => None,
        }
    };
    let Some(rendezvous) = rendezvous else {
        return;
    };
    rendezvous
        .reached
        .send(AutoDecisionDone {
            action: action.clone(),
        })
        .expect("rendezvous controller stays alive until the boundary is reached");
    rendezvous
        .release
        .recv()
        .expect("rendezvous controller releases the dispatch");
}

// ── test-only decision/hold gap ──────────────────────────────────────────────
//
// `auto_switch_if_needed` decides BEFORE it takes the config+flock hold; this
// seam is what makes the gap between the two falsifiable. A test arms it; when
// the actor reaches the gap with a decision in hand it reports and parks until
// the controller releases it — holding NO locks, since the decision phase
// already dropped the config guard — so the controller can land a disk-only
// switch (the shape `clauth switch` / MCP takes) that the re-validate under the
// hold must then see and drop. Fires at most once per arming, so every unarmed
// caller passes through untouched.
#[cfg(test)]
struct AutoDecisionGap {
    reached: std::sync::mpsc::Sender<SwitchAction>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static AUTO_DECISION_GAP: std::sync::Mutex<Option<AutoDecisionGap>> = std::sync::Mutex::new(None);

/// Arm the gap seam: the next decision reports its action on `reached` and
/// parks until a send on `release`. One gap at a time; tests arming one hold a
/// `HomeSandbox`, which serializes them. The returned handle disarms the slot
/// on drop, so a test that arms and drops without the actor firing leaves the
/// next arm working (a panicking test never strands the seam).
#[cfg(test)]
pub(crate) struct AutoDecisionGapHandle {
    pub(crate) reached: std::sync::mpsc::Receiver<SwitchAction>,
    pub(crate) release: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
impl Drop for AutoDecisionGapHandle {
    fn drop(&mut self) {
        let mut slot = AUTO_DECISION_GAP.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None;
    }
}

#[cfg(test)]
pub(crate) fn install_auto_decision_gap() -> AutoDecisionGapHandle {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut slot = AUTO_DECISION_GAP.lock().unwrap_or_else(|e| e.into_inner());
    assert!(slot.is_none(), "only one auto-decision gap may be armed");
    *slot = Some(AutoDecisionGap {
        reached: reached_tx,
        release: release_rx,
    });
    AutoDecisionGapHandle {
        reached: reached_rx,
        release: release_tx,
    }
}

/// The gap's park: report the decision, then hold the actor here until the
/// controller releases it — under no locks, exactly like the production path
/// between the decision and the config+flock hold.
#[cfg(test)]
fn auto_decision_gap(action: &SwitchAction) {
    let gap = {
        let mut slot = AUTO_DECISION_GAP.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    let Some(gap) = gap else {
        return;
    };
    gap.reached
        .send(action.clone())
        .expect("gap controller stays alive until the gap is reached");
    gap.release
        .recv()
        .expect("gap controller releases the actor");
}

/// If the active profile is a chain member the scheduler walk leaves — past its
/// threshold, a kick/canceled/dead-reading bypass, or a healthy return home —
/// switch to the next viable member, or, in wrap-off mode when the whole chain
/// is spent and no sink exists, turn off all accounts. Returns the action taken,
/// or None.
///
/// The decision is the scheduler walk's ([`next_auto_switch_target_with_usage`])
/// over a [`ChainSnapshot`] + usage map built from the caller's stores, with the
/// caller's in-memory burn rate (`active_burn_pct_per_hour`, ignored unless
/// burn-aware mode is on) — never a disk read of `usage_history.jsonl` under any
/// guard. The snapshot is built BEFORE the config+flock hold (the stores rank
/// below `Config` in [`crate::lockorder`], so the fillers cannot run under it).
/// The dispatch then re-validates under ONE config-guard + state-flock hold that
/// the active is still the member the snapshot decided about, in BOTH places it
/// can have moved: the in-memory `config.state.active_profile` (an in-process
/// switch) and the on-disk active marker ([`crate::profile::load_app_state`],
/// the same fresh read `finish_switch` makes under that flock) — an explicit
/// switch (CLI/TUI/MCP) that lands after the decision but before the hold is
/// seen by that re-check and the stale decision is dropped; one that starts
/// after the hold waits it out and lands last — the property the
/// `auto_decision_rendezvous` seam pins. The dispatch goes through the `_locked`
/// cores over the held guard — the guard-taking wrappers would re-lock a mutex
/// this call already holds — and the guard is dropped before the feed republish
/// below, so that sweep runs under no config guard. A caller holding a guard
/// across this call self-deadlocks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn auto_switch_if_needed(
    config: &crate::profile::ConfigHandle,
    active_burn_pct_per_hour: Option<f64>,
    store: &UsageStore,
    third_party_usage_store: &ThirdPartyUsageStore,
    status: &StatusStore,
    third_party_status: &ThirdPartyStatusStore,
    streaks: &PollStreaks,
    third_party_streaks: &ThirdPartyStreaks,
    kick_blocks: &KickBlocks,
) -> Result<Option<SwitchAction>> {
    let (decided_active, decision) = {
        let (mut snapshot, third_party_names, polled, key_rejected) = {
            #[allow(
                clippy::expect_used,
                reason = "config mutex poisoning is unrecoverable"
            )]
            let cfg = config.lock().expect("config mutex poisoned");
            // The startup one-shot reads the DURABLE key-rejected verdict: no
            // live `ThirdPartyBroken` set exists yet, so the fingerprint-bound
            // record on disk is the authoritative read.
            let key_rejected = durable_key_rejected(&cfg);
            let Some(snapshot) = snapshot_chain(&cfg, &key_rejected) else {
                return Ok(None);
            };
            let third_party_names: HashSet<ProfileName> = cfg
                .profiles
                .iter()
                .filter(|p| p.usage_cache_is_third_party())
                .map(|p| p.name.clone())
                .collect();
            let polled = crate::usage::polled_third_party_names(&cfg);
            (snapshot, third_party_names, polled, key_rejected)
        };
        let active = snapshot.active.clone();
        let usage = crate::usage::fill_chain_snapshot_stores(
            &mut snapshot,
            store,
            third_party_usage_store,
            status,
            third_party_status,
            streaks,
            third_party_streaks,
            kick_blocks,
            &third_party_names,
            &polled,
        );
        snapshot.broken.extend(key_rejected.iter().cloned());
        let decision = next_auto_switch_target_with_usage(
            &snapshot,
            &usage,
            &third_party_names,
            ActiveBurnRate::Supplied(active_burn_pct_per_hour),
        );
        (active, decision)
    };
    let Some(action) = decision else {
        return Ok(None);
    };
    #[cfg(test)]
    auto_decision_gap(&action);
    #[allow(
        clippy::expect_used,
        reason = "config mutex poisoning is unrecoverable"
    )]
    let mut guard = config.lock().expect("config mutex poisoned");
    let (action, changed) = with_state_lock(|_held| {
        let config = &mut *guard;
        // Re-validate under the hold: the decision was made about
        // `decided_active`; an explicit switch that landed in the gap between
        // the snapshot and this hold changed the active, so the decision is
        // stale and must not overwrite it. The active can move in two places —
        // the in-memory config (an in-process switch) and the on-disk marker
        // (a cross-process CLI/MCP switch, which never touches this process's
        // config) — so BOTH must still name the decided member.
        let disk_active = crate::profile::load_app_state()?.active_profile;
        if config.state.active_profile.as_deref() != Some(decided_active.as_str())
            || disk_active.as_deref() != Some(decided_active.as_str())
        {
            return Ok((None, false));
        }
        #[cfg(test)]
        auto_decision_rendezvous(&action);
        let changed = match &action {
            SwitchAction::To(target) => {
                switch_profile_locked(config, &ProfileName::from(target.clone()))?
            }
            SwitchAction::Off => switch_off_locked(config)?,
        };
        Ok((Some(action), changed))
    })?;
    drop(guard);
    if changed {
        crate::daemon::publish_status(config);
    }
    Ok(action)
}

#[cfg(test)]
#[path = "../tests/inline/fallback.rs"]
mod tests;
