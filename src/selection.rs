//! Model-aware account selection at SESSION START.
//!
//! The fallback chain decides where a session moves once it is already running.
//! This module decides where it *starts*, and it is the only place in the crate
//! that knows which models a session is about to run.
//!
//! **Why start-time is the right place.** Prompt-cache entries do not cross
//! accounts, so a mid-session swap re-sends the whole context as fresh input.
//! The chain pays that willingly to rescue a spent account — one expensive turn
//! beats a dead session — but it is a bad way to *optimise*. Choosing well at
//! launch is free; choosing badly turns the first turn into a swap.
//!
//! **Why the model matters.** A plan carries per-model weekly windows ("7d
//! fable") alongside the aggregate ones. [`crate::fallback`] treats any capped
//! per-model week as a blanket exclusion because, in the walk's own words, it
//! cannot know which model the next session runs. At launch we *can* know, so a
//! member capped only on a model this session will never touch stays a
//! candidate.
//!
//! **Why the union and not the headline model.** A `Task` subagent runs inside
//! the parent `claude` process, and the credential memo is process-wide
//! ([`crate::runtime::touch_store`]), so it cannot hold its own account — it
//! spends the parent's on whatever model it runs. Selecting for the main
//! thread's model alone would strand the session the moment a subagent used a
//! capped one, with no rescue short of the swap this exists to avoid. So the
//! demand is every family the session MAY run, and a member must clear all of
//! them.
//!
//! **Why runway and not percentage.** The failure worth preventing is picking a
//! member that strands one turn in. Utilization alone cannot see that: what
//! predicts it is headroom divided by burn — minutes of runway — which
//! [`crate::usage::compute_burn_rates_from_history`] already fits for
//! burn-aware switching. Absent burn data yields UNBOUNDED runway, never zero,
//! matching `is_exhausted_projected`'s stance that a member is never uncovered
//! for lack of data: an idle account has no samples precisely because it is
//! idle.
//!
//! **Why reset time is not the objective.** `resets_at` says when a window
//! OPENED, not how much of it is left, so ranking on "earliest reset" is
//! orthogonal to headroom rather than a proxy for it. It earns exactly one job
//! here: a member whose binding window resets within the grace is feasible
//! anyway, because a short stall is not a strand.
//!
//! **Why live sessions outrank runway.** "Pick the best member" makes N
//! simultaneous launches all pick the SAME member, which is then the worst one
//! — and usage polling lags the launch by up to a refresh interval, so nothing
//! corrects it in time. The live-session registry is the only signal that moves
//! at launch speed, which is why it is the FIRST ranking key.

use crate::live_sessions::LiveTally;
use crate::profile::{AppConfig, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageWindow};

/// Feasibility floor: a member with less than this much runway on its binding
/// window is a degraded pick. Twenty minutes is short enough that an ordinary
/// account is never rejected and long enough that a first turn plus a subagent
/// fan-out fits inside it.
pub(crate) const DEFAULT_MIN_RUNWAY_MINS: f64 = 20.0;

/// A binding window resetting within this counts as feasible however little
/// runway is left: the session stalls until the reset rather than stranding.
pub(crate) const DEFAULT_RESET_GRACE_MINS: f64 = 10.0;

/// The tunables, resolved once by the caller so the pure core takes no config.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) min_runway_mins: f64,
    pub(crate) reset_grace_mins: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_runway_mins: DEFAULT_MIN_RUNWAY_MINS,
            reset_grace_mins: DEFAULT_RESET_GRACE_MINS,
        }
    }
}

/// The model family a scoped window is named for — `"fable"`, `"opus"`, … —
/// derived from a model id the same way [`crate::tokens::model_display_name`]
/// derives its label, so both sides of the join agree by construction.
///
/// Everything after the family is dropped: a date stamp, a version, a
/// `[1m]`-style context suffix, a `-thinking` tail. A non-Anthropic id has no
/// family here, because no per-model window is ever scoped to one.
pub(crate) fn model_family(model_id: &str) -> Option<String> {
    let id = model_id.trim().to_lowercase();
    if id.is_empty() {
        return None;
    }
    // Bare aliases as Claude Code spells them in `settings.json` and `--model`.
    // `opusplan` is deliberately absent: it is two families and resolves in
    // `demand_from`, which can return both.
    if matches!(id.as_str(), "opus" | "sonnet" | "haiku" | "fable") {
        return Some(id);
    }
    let rest = id.strip_prefix("claude-")?;
    let family = rest.split(['-', '.', '[']).next()?;
    if family.is_empty() || family.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(family.to_owned())
}

/// Whether a scoped window's label names `family`.
///
/// Labels are built server-side as `"7d " + display_name.to_lowercase()`
/// ([`crate::usage::ScopedWindow`]), so this strips the period prefix rather
/// than matching a fixed set — a model the server adds later joins with no
/// change here.
pub(crate) fn scoped_label_is(label: &str, family: &str) -> bool {
    // Any word, not just the last: the display name the label is built from can
    // be more than one word ("7d sonnet 4.5"), and matching only the tail would
    // silently stop recognising such a window. A family is never a period token
    // (`model_family` yields only model names), so "7d" cannot false-positive.
    label
        .split_whitespace()
        .any(|w| w.eq_ignore_ascii_case(family))
}

/// The union of model families a launch may run, deduped and order-stable.
///
/// `opusplan` expands to both families it alternates between. An id that
/// resolves to nothing is dropped rather than poisoning the set: an unresolved
/// demand must not silently narrow the candidates.
pub(crate) fn demand_from<I, S>(models: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for m in models {
        let raw = m.as_ref().trim().to_lowercase();
        let families: Vec<String> = if raw == "opusplan" {
            vec!["opus".to_owned(), "sonnet".to_owned()]
        } else {
            model_family(&raw).into_iter().collect()
        };
        for f in families {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    out
}

/// One member's inputs, resolved off disk by [`select`] and taken whole by the
/// pure [`select_from`] so the ranking is testable without a filesystem.
#[derive(Debug, Clone)]
pub(crate) struct MemberInput {
    pub(crate) name: ProfileName,
    pub(crate) usage: Option<UsageInfo>,
    /// Burn in utilization-percent per hour, per window label. Absent labels
    /// read as unbounded runway.
    pub(crate) burn: Vec<(String, f64)>,
    /// The member's own 5h line.
    pub(crate) threshold: f64,
    pub(crate) weekly_line: f64,
    pub(crate) scoped_line: f64,
    pub(crate) check_scoped: bool,
    pub(crate) preferred: bool,
    pub(crate) last_resort: bool,
    pub(crate) live_sessions: usize,
}

/// Why a member never reached the ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The subscription reads canceled at Anthropic. Read off the cached plan
    /// rather than `fallback::is_canceled`, whose `Profile.usage` only the UI
    /// thread populates — a one-shot `clauth start` has none.
    Canceled,
    /// Over a per-model week this session's demand actually needs.
    ScopedSpent { label: String },
    /// Over its aggregate weekly line.
    WeeklySpent,
}

/// A ranked member plus the numbers the decision was made on, so the caller can
/// say WHY rather than only what. Every surface in this crate renders its
/// reason; a selector that picked silently would be the odd one out.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) name: ProfileName,
    /// Minutes until the binding window is spent at the recent burn rate.
    /// `None` is unbounded — no burn samples, or no measurable burn.
    pub(crate) runway_mins: Option<f64>,
    /// Label of the window that binds, when one does.
    pub(crate) binding: Option<String>,
    /// Minutes until the binding window resets, when it says.
    pub(crate) reset_mins: Option<f64>,
    pub(crate) live_sessions: usize,
    /// Whether this member has usage data at all. An unread member reports
    /// unbounded runway for want of numbers, not because it is empty, so it must
    /// not be allowed to outrank a member whose headroom was actually measured —
    /// the same instinct behind the chain preferring live readings over cached
    /// ones. It stays selectable, just last.
    pub(crate) known: bool,
    pub(crate) feasible: bool,
    /// Feasible only because the binding window resets inside the grace — the
    /// runway alone did not clear the floor. Kept apart from `feasible` because
    /// the two carry different advice: one member has room, the other is about
    /// to get some.
    pub(crate) feasible_via_reset: bool,
}

/// What [`select_from`] decided.
#[derive(Debug, Clone, Default)]
pub(crate) struct Outcome {
    /// The pick. `None` only when nothing survived the hard filters.
    pub(crate) chosen: Option<Candidate>,
    /// Members dropped by a hard filter, with the reason, for the caller's
    /// explain line.
    pub(crate) rejected: Vec<(ProfileName, Rejected)>,
}

/// Runway on one window: headroom over the line, divided by burn.
///
/// `None` means unbounded — no rate, or a rate at or below zero. A member past
/// its line has zero runway rather than a negative one, which keeps the
/// ordering total.
fn window_runway(util: f64, line: f64, burn_pct_per_hour: Option<f64>) -> Option<f64> {
    let headroom = line - util;
    if headroom <= 0.0 {
        return Some(0.0);
    }
    let rate = burn_pct_per_hour?;
    if rate <= 0.0 {
        return None;
    }
    Some(headroom / rate * 60.0)
}

/// Minutes until `window` resets, when it carries a parseable stamp.
fn window_reset_mins(window: &UsageWindow, now_secs: i64) -> Option<f64> {
    let at = window.resets_at.as_deref()?;
    let secs = crate::usage::iso_to_epoch_secs(at)?;
    Some(((secs - now_secs).max(0)) as f64 / 60.0)
}

/// Rank `members` for a session that may run `demand`, and pick one.
///
/// Hard filters first (a member that cannot serve the demand is not a degraded
/// pick, it is not a pick), then feasibility, then the ordering. When nothing is
/// feasible the best infeasible member is returned with `feasible: false` rather
/// than nothing at all: refusing to launch is a worse answer than launching
/// somewhere thin and saying so.
pub(crate) fn select_from(
    members: &[MemberInput],
    demand: &[String],
    limits: Limits,
    now_secs: i64,
) -> Outcome {
    let mut rejected = Vec::new();
    let mut ranked: Vec<(&MemberInput, Candidate)> = Vec::new();

    for m in members {
        let usage = m.usage.as_ref();

        // Hard filter: a canceled subscription. It serves whatever the free
        // plan allows and no window figure says so, which is exactly why the
        // chain excludes it too.
        if usage
            .and_then(|u| u.plan.as_ref())
            .is_some_and(crate::usage::PlanInfo::is_canceled)
        {
            rejected.push((m.name.clone(), Rejected::Canceled));
            continue;
        }

        // Hard filter: a per-model week this session actually needs.
        //
        // Narrowed to the demand when we have one; with none resolved the
        // member's own `check_scoped` gate does the blanket job instead, which
        // is exactly today's behaviour for a launch we know nothing about.
        // Both spellings go through `worst_scoped_window_for` so the narrowed
        // judgment cannot drift from the chain's blanket one — the liveness
        // rule in particular, without which a window that has already reset
        // would keep blocking off a stale cache.
        let families = (!demand.is_empty()).then_some(demand);
        let scoped_block = usage
            .filter(|_| families.is_some() || m.check_scoped)
            .and_then(|u| {
                crate::fallback::worst_scoped_window_for(u, now_secs, m.scoped_line, families)
            });
        if let Some(s) = scoped_block {
            rejected.push((
                m.name.clone(),
                Rejected::ScopedSpent {
                    label: s.label.clone(),
                },
            ));
            continue;
        }

        // Hard filter: the aggregate week. A member past it strands on every
        // model, so no demand makes it viable. Same predicate the chain uses,
        // liveness included.
        if usage.is_some_and(|u| crate::fallback::weekly_blocked_info(u, now_secs, m.weekly_line)) {
            rejected.push((m.name.clone(), Rejected::WeeklySpent));
            continue;
        }

        // Runway across every window that can bind: the 5h, the aggregate week,
        // and each per-model week this session's demand touches. The minimum is
        // what the session actually runs out of.
        let mut binding: Option<(String, f64)> = None;
        let mut reset_mins = None;
        let mut consider = |label: &str, w: &UsageWindow, line: f64| {
            let burn = m.burn.iter().find(|(l, _)| l == label).map(|(_, r)| *r);
            if let Some(r) = window_runway(w.utilization, line, burn)
                && binding.as_ref().is_none_or(|(_, best)| r < *best)
            {
                binding = Some((label.to_owned(), r));
                reset_mins = window_reset_mins(w, now_secs);
            }
        };
        if let Some(u) = usage {
            if let Some(w) = &u.five_hour {
                consider(crate::usage::LABEL_5H, w, m.threshold);
            }
            if let Some(w) = &u.seven_day {
                consider(crate::usage::LABEL_7D, w, m.weekly_line);
            }
            for s in &u.weekly_scoped {
                if demand.iter().any(|f| scoped_label_is(&s.label, f)) {
                    consider(&s.label, &s.window, m.scoped_line);
                }
            }
        }

        let runway_mins = binding.as_ref().map(|(_, r)| *r);
        let over_floor = runway_mins.is_none_or(|r| r >= limits.min_runway_mins);
        let feasible_via_reset =
            !over_floor && reset_mins.is_some_and(|m| m <= limits.reset_grace_mins);
        let feasible = over_floor || feasible_via_reset;
        ranked.push((
            m,
            Candidate {
                name: m.name.clone(),
                runway_mins,
                binding: binding.map(|(l, _)| l),
                reset_mins,
                live_sessions: m.live_sessions,
                known: usage.is_some(),
                feasible,
                feasible_via_reset,
            },
        ));
    }

    ranked.sort_by(|(ma, a), (mb, b)| {
        // Feasible before degraded, then the parking spot last, then a member
        // we have numbers for over one we do not, then spread across live
        // sessions BEFORE headroom (see the module header), then headroom, then
        // the home account, then the name so ties are stable.
        b.feasible
            .cmp(&a.feasible)
            .then(ma.last_resort.cmp(&mb.last_resort))
            .then(b.known.cmp(&a.known))
            .then(a.live_sessions.cmp(&b.live_sessions))
            .then(
                runway_key(b.runway_mins)
                    .partial_cmp(&runway_key(a.runway_mins))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(mb.preferred.cmp(&ma.preferred))
            .then(a.name.as_str().cmp(b.name.as_str()))
    });

    Outcome {
        chosen: ranked.into_iter().next().map(|(_, c)| c),
        rejected,
    }
}

/// Sort key for an optionally-unbounded runway: `None` is the largest value
/// there is, so an idle member with no samples outranks a measured one.
fn runway_key(r: Option<f64>) -> f64 {
    r.unwrap_or(f64::INFINITY)
}

/// Build [`MemberInput`]s for the fallback chain and rank them.
///
/// The candidate set is the chain rather than every profile: the chain is the
/// operator's own statement of which accounts may be entered unattended, and
/// launching onto an account they never rotate would be a surprise this module
/// has no standing to spring. An empty chain therefore selects nothing, and the
/// caller names the fix.
pub(crate) fn select(config: &AppConfig, demand: &[String], limits: Limits) -> Outcome {
    let weekly_pct = config.state.weekly_switch_threshold_pct();
    let tally = LiveTally::collect(config);
    let now = crate::usage::now_epoch_secs();

    let members: Vec<MemberInput> = config
        .state
        .fallback_chain
        .iter()
        .filter(|name| !crate::fallback::walk_excluded(config, name))
        .filter_map(|name| {
            let profile = config.find(name)?;
            Some(build_member(
                profile,
                weekly_pct,
                tally.member(name).sessions,
            ))
        })
        .collect();

    select_from(&members, demand, limits, now)
}

/// One chain member's inputs, read off the per-profile caches.
///
/// Mirrors `fallback::build_chain_snapshot`'s line resolution exactly rather
/// than re-deriving it, so a member cannot be judged at one line here and
/// another there.
fn build_member(profile: &Profile, weekly_pct: f64, live_sessions: usize) -> MemberInput {
    let usage = crate::profile_cache::load_profile_cache::<UsageInfo>(
        &profile.name,
        crate::profile_cache::USAGE_CACHE_FILE,
    );
    let burn = usage
        .as_ref()
        .map(|u| burn_for(&profile.name, u))
        .unwrap_or_default();
    MemberInput {
        name: profile.name.clone(),
        usage,
        burn,
        threshold: crate::fallback::threshold_for(profile),
        weekly_line: crate::fallback::member_weekly_line(profile, weekly_pct),
        scoped_line: crate::fallback::member_scoped_line(profile, weekly_pct),
        check_scoped: profile.check_scoped,
        preferred: profile.preferred,
        last_resort: profile.last_resort,
        live_sessions,
    }
}

/// Per-window burn for one profile, over the same history and windowing
/// burn-aware switching uses. One call for every window rather than one per
/// window: the fit reads the whole history either way.
fn burn_for(name: &ProfileName, usage: &UsageInfo) -> Vec<(String, f64)> {
    let history = crate::profile::load_usage_history(name);
    let windows = usage.windows();
    let rates = crate::usage::compute_burn_rates_from_history(
        &history,
        &windows,
        crate::usage::BURN_LOOKBACK_MS,
        crate::usage::BURN_MIN_SAMPLES,
        crate::usage::BURN_GAP_CUT_MS,
    );
    rates
        .into_iter()
        .filter_map(|(label, rate)| rate.map(|r| (label, r)))
        .collect()
}

/// Every model string in effect for a launch, from the sources a launcher can
/// actually see before the session exists.
///
/// Three, in the order they override: the live `settings.json` `model` and its
/// subagent key, the same two as process environment, and an explicit
/// `--model` in the passthrough args. All three go in — this is a UNION, not a
/// resolution, because the session may run every one of them (see the module
/// header on why the headline model is not enough).
///
/// Deliberately absent: the per-profile `[models]` block, which cannot inform a
/// choice that has not picked a profile yet; and Claude Code's utility haiku
/// calls, which no per-model window is scoped to on any plan observed so far.
/// Both would be guesses, and a guessed family narrows the candidate set.
pub(crate) fn launch_models(claude_args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = crate::claude::claude_settings_models().unwrap_or_default();
    for key in ["ANTHROPIC_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            out.push(v);
        }
    }
    out.extend(models_from_args(claude_args));
    out
}

/// The `--model` values in a passthrough arg list, in both spellings.
///
/// Split out of [`launch_models`] so the parsing is exercised without a home:
/// its siblings there read `settings.json` and the environment, and this crate's
/// tests panic rather than resolve the operator's real home.
pub(crate) fn models_from_args(claude_args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = claude_args.iter();
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--model=") {
            out.push(v.to_owned());
        } else if a == "--model"
            && let Some(v) = args.next()
        {
            out.push(v.clone());
        }
    }
    out
}

/// One line saying what was picked and on what evidence, for the operator to
/// read at launch. Every other decision surface in this crate renders its
/// reason; a selector that picked silently would be the odd one out.
pub(crate) fn explain(chosen: &Candidate, demand: &[String]) -> String {
    let mut s = format!("selected `{}`", chosen.name.as_str());
    if !demand.is_empty() {
        s.push_str(&format!(" for {}", demand.join(" + ")));
    }
    match (chosen.runway_mins, &chosen.binding) {
        (Some(r), Some(label)) => {
            s.push_str(&format!(" · ~{r:.0} min on {label}"));
        }
        // Two different silences: no usage at all, versus usage with no burn
        // samples behind it. The first is a warning, the second is an idle
        // account, and reporting them the same way hides the warning.
        _ if !chosen.known => s.push_str(" · NO USAGE DATA for this account"),
        _ => s.push_str(" · no measured burn"),
    }
    if chosen.live_sessions > 0 {
        s.push_str(&format!(" · {} live session(s) here", chosen.live_sessions));
    }
    if chosen.feasible_via_reset
        && let Some(m) = chosen.reset_mins
    {
        // Only when the reset is what made this feasible. Printing it whenever
        // a reset is merely known would put a figure beside every pick that
        // reads as the reason and usually is not.
        s.push_str(&format!(" · thin, but resets in ~{m:.0} min"));
    }
    if !chosen.feasible {
        s.push_str(" · THIN: nothing in the chain cleared the runway floor");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window whose reset is still ahead. Every gate here is liveness-gated
    /// (a window that already reset carries stale numbers and must not block),
    /// so a fixture with no `resets_at` would silently test nothing.
    fn window(util: f64) -> UsageWindow {
        UsageWindow {
            utilization: util,
            resets_at: Some("2100-01-01T00:00:00Z".to_owned()),
        }
    }

    /// The same numbers, already reset.
    fn stale_window(util: f64) -> UsageWindow {
        UsageWindow {
            utilization: util,
            resets_at: Some("1970-01-01T00:00:00Z".to_owned()),
        }
    }

    fn scoped(label: &str, util: f64) -> crate::usage::ScopedWindow {
        crate::usage::ScopedWindow {
            label: label.to_owned(),
            window: window(util),
        }
    }

    fn member(
        name: &str,
        five: f64,
        seven: f64,
        scoped_windows: Vec<crate::usage::ScopedWindow>,
    ) -> MemberInput {
        MemberInput {
            name: ProfileName::from(name),
            usage: Some(UsageInfo {
                five_hour: Some(window(five)),
                seven_day: Some(window(seven)),
                weekly_scoped: scoped_windows,
                ..Default::default()
            }),
            burn: Vec::new(),
            threshold: 95.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
            preferred: false,
            last_resort: false,
            live_sessions: 0,
        }
    }

    #[test]
    fn family_parses_ids_and_aliases() {
        assert_eq!(model_family("claude-fable-5-1").as_deref(), Some("fable"));
        assert_eq!(model_family("claude-opus-5[1m]").as_deref(), Some("opus"));
        assert_eq!(
            model_family("claude-sonnet-4-5-20250929").as_deref(),
            Some("sonnet")
        );
        assert_eq!(model_family("opus").as_deref(), Some("opus"));
        assert_eq!(model_family("gpt-4o"), None);
        assert_eq!(model_family(""), None);
    }

    #[test]
    fn opusplan_demands_both_families() {
        assert_eq!(demand_from(["opusplan"]), vec!["opus", "sonnet"]);
    }

    #[test]
    fn demand_dedupes_and_drops_unresolvable() {
        assert_eq!(
            demand_from(["claude-opus-5", "opus", "gpt-4o"]),
            vec!["opus"]
        );
    }

    #[test]
    fn scoped_label_matches_on_family() {
        assert!(scoped_label_is("7d fable", "fable"));
        assert!(!scoped_label_is("7d fable", "opus"));
        assert!(!scoped_label_is("7d", "fable"));
    }

    /// The whole point: a member capped on a model this session will not run
    /// stays a candidate, and the same member is rejected once the session
    /// actually demands that model.
    #[test]
    fn scoped_cap_gates_only_the_demanded_family() {
        let members = vec![member("capped", 0.0, 57.0, vec![scoped("7d fable", 100.0)])];

        let opus = select_from(&members, &["opus".to_owned()], Limits::default(), 0);
        assert_eq!(
            opus.chosen
                .expect("opus can use a fable-capped member")
                .name
                .as_str(),
            "capped"
        );

        let fable = select_from(&members, &["fable".to_owned()], Limits::default(), 0);
        assert!(fable.chosen.is_none());
        assert_eq!(
            fable.rejected,
            vec![(
                ProfileName::from("capped"),
                Rejected::ScopedSpent {
                    label: "7d fable".to_owned()
                }
            )]
        );
    }

    /// With no demand resolved, the blanket `check_scoped` behaviour stands —
    /// an unknown launch must not be routed onto a capped model.
    #[test]
    fn empty_demand_keeps_the_blanket_gate() {
        let members = vec![member("capped", 0.0, 57.0, vec![scoped("7d fable", 100.0)])];
        assert!(
            select_from(&members, &[], Limits::default(), 0)
                .chosen
                .is_none()
        );
    }

    /// ...unless the operator turned that member's gate off, which is exactly
    /// what `check_scoped = false` has always meant.
    #[test]
    fn empty_demand_respects_check_scoped_off() {
        let mut m = member("capped", 0.0, 57.0, vec![scoped("7d fable", 100.0)]);
        m.check_scoped = false;
        assert!(
            select_from(&[m], &[], Limits::default(), 0)
                .chosen
                .is_some()
        );
    }

    #[test]
    fn a_canceled_subscription_is_never_a_candidate() {
        let mut m = member("gone", 0.0, 0.0, vec![]);
        if let Some(u) = m.usage.as_mut() {
            u.plan = Some(crate::usage::PlanInfo {
                subscription_status: Some("canceled".to_owned()),
                ..Default::default()
            });
        }
        let out = select_from(&[m], &[], Limits::default(), 0);
        assert!(out.chosen.is_none());
        assert_eq!(out.rejected[0].1, Rejected::Canceled);
    }

    /// A display name of more than one word still matches. Pinned because the
    /// first cut compared only the label's last word, which happens to be right
    /// for every window observed so far and silently wrong for the first one
    /// that is not.
    #[test]
    fn scoped_label_matches_a_multi_word_display_name() {
        assert!(scoped_label_is("7d sonnet 4.5", "sonnet"));
        assert!(!scoped_label_is("7d sonnet 4.5", "opus"));
    }

    /// A member clauth has never read reports unbounded runway for want of
    /// numbers, not because it is empty. It must not therefore outrank a member
    /// whose headroom was actually measured — an api-key account in the chain
    /// keeps its usage elsewhere and would otherwise win every selection.
    #[test]
    fn an_unread_member_never_outranks_a_measured_one() {
        let mut measured = member("measured", 40.0, 10.0, vec![]);
        measured.burn = vec![("5h".to_owned(), 1.0)];
        let unread = MemberInput {
            usage: None,
            ..member("unread", 0.0, 0.0, vec![])
        };

        let out = select_from(&[unread, measured], &[], Limits::default(), 0);
        let c = out.chosen.expect("a pick");
        assert_eq!(c.name.as_str(), "measured");
        assert!(c.known);
    }

    /// It is still selectable when it is all there is, and says so rather than
    /// reporting the silence as an idle account.
    #[test]
    fn an_unread_member_is_still_selectable_and_flagged() {
        let unread = MemberInput {
            usage: None,
            ..member("unread", 0.0, 0.0, vec![])
        };
        let c = select_from(&[unread], &[], Limits::default(), 0)
            .chosen
            .expect("a pick");
        assert!(!c.known && c.feasible);
        assert!(explain(&c, &[]).contains("NO USAGE DATA"));
    }

    #[test]
    fn weekly_cap_rejects_regardless_of_demand() {
        let members = vec![member("spent", 0.0, 99.0, vec![])];
        let out = select_from(&members, &["opus".to_owned()], Limits::default(), 0);
        assert!(out.chosen.is_none());
        assert_eq!(out.rejected[0].1, Rejected::WeeklySpent);
    }

    /// Spread beats headroom: a fresh member already hosting sessions loses to
    /// an emptier one, because usage polling cannot see the launches yet.
    #[test]
    fn live_sessions_outrank_runway() {
        let mut busy = member("busy", 0.0, 10.0, vec![]);
        busy.burn = vec![("5h".to_owned(), 1.0)];
        busy.live_sessions = 2;
        let mut idle = member("idle", 40.0, 10.0, vec![]);
        idle.burn = vec![("5h".to_owned(), 1.0)];

        let out = select_from(&[busy, idle], &[], Limits::default(), 0);
        assert_eq!(out.chosen.expect("a pick").name.as_str(), "idle");
    }

    /// A member with no burn samples reads as unbounded runway rather than
    /// zero — it is idle, not exhausted.
    #[test]
    fn missing_burn_is_unbounded_not_empty() {
        let out = select_from(
            &[member("fresh", 50.0, 10.0, vec![])],
            &[],
            Limits::default(),
            0,
        );
        let c = out.chosen.expect("a pick");
        assert!(c.runway_mins.is_none());
        assert!(c.feasible);
    }

    /// Nothing feasible still launches — somewhere thin, flagged as thin.
    #[test]
    fn degrades_rather_than_refusing() {
        let mut thin = member("thin", 94.0, 10.0, vec![]);
        thin.burn = vec![("5h".to_owned(), 60.0)];
        let out = select_from(&[thin], &[], Limits::default(), 0);
        let c = out.chosen.expect("a degraded pick, not nothing");
        assert!(!c.feasible);
        assert_eq!(c.binding.as_deref(), Some("5h"));
    }

    /// A window about to reset is feasible however thin: a stall is not a
    /// strand.
    #[test]
    fn imminent_reset_rescues_feasibility() {
        let mut thin = member("thin", 94.0, 10.0, vec![]);
        thin.burn = vec![("5h".to_owned(), 60.0)];
        if let Some(u) = thin.usage.as_mut()
            && let Some(w) = u.five_hour.as_mut()
        {
            w.resets_at = Some("1970-01-01T00:05:00Z".to_owned());
        }
        let c = select_from(&[thin], &[], Limits::default(), 0)
            .chosen
            .expect("a pick");
        assert!(c.feasible);
        assert!(
            c.feasible_via_reset,
            "the reset is what rescued it, and the explain line says so"
        );
    }

    /// A member with genuine runway is NOT reported as reset-rescued, however
    /// far off its reset happens to be. Pinned because the first cut printed the
    /// reset beside every pick, where it read as the reason.
    #[test]
    fn ordinary_headroom_is_not_reported_as_a_reset_rescue() {
        let mut roomy = member("roomy", 10.0, 10.0, vec![]);
        roomy.burn = vec![("5h".to_owned(), 1.0)];
        let c = select_from(&[roomy], &[], Limits::default(), 0)
            .chosen
            .expect("a pick");
        assert!(c.feasible && !c.feasible_via_reset);
    }

    #[test]
    fn last_resort_is_the_parking_spot() {
        let mut parked = member("parked", 0.0, 10.0, vec![]);
        parked.last_resort = true;
        let ordinary = member("ordinary", 80.0, 10.0, vec![]);
        let out = select_from(&[parked, ordinary], &[], Limits::default(), 0);
        assert_eq!(out.chosen.expect("a pick").name.as_str(), "ordinary");
    }

    /// A capped window that has ALREADY reset must not block: the cache is
    /// stale, not the account. Pinned because dropping the liveness gate would
    /// have looked correct in every other test here.
    #[test]
    fn a_reset_scoped_window_no_longer_blocks() {
        let mut m = member("recovered", 0.0, 10.0, vec![]);
        if let Some(u) = m.usage.as_mut() {
            u.weekly_scoped = vec![crate::usage::ScopedWindow {
                label: "7d fable".to_owned(),
                window: stale_window(100.0),
            }];
        }
        let out = select_from(&[m], &["fable".to_owned()], Limits::default(), 0);
        assert!(
            out.chosen.is_some(),
            "a window past its reset carries stale numbers and must not gate"
        );
    }

    /// The aggregate weekly gate is liveness-gated the same way.
    #[test]
    fn a_reset_weekly_window_no_longer_blocks() {
        let mut m = member("recovered", 0.0, 0.0, vec![]);
        if let Some(u) = m.usage.as_mut() {
            u.seven_day = Some(stale_window(99.0));
        }
        assert!(
            select_from(&[m], &[], Limits::default(), 0)
                .chosen
                .is_some(),
            "a 7d window past its reset must not gate"
        );
    }

    #[test]
    fn model_args_are_read_in_both_spellings() {
        let split = vec!["--model".to_owned(), "claude-fable-5-1".to_owned()];
        assert_eq!(models_from_args(&split), ["claude-fable-5-1"]);
        let joined = vec!["--model=opus".to_owned()];
        assert_eq!(models_from_args(&joined), ["opus"]);
        let none = vec!["-p".to_owned(), "hi".to_owned()];
        assert!(models_from_args(&none).is_empty());
    }

    /// A trailing `--model` with nothing behind it must not panic or invent a
    /// value; `claude` will reject it on its own.
    #[test]
    fn a_dangling_model_flag_yields_nothing() {
        assert!(models_from_args(&["--model".to_owned()]).is_empty());
    }
}
