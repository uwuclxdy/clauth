//! Behaviour tests for `next_target` — the fallback-chain target picker.
//!
//! Tests stay hermetic: no filesystem I/O, no `switch_profile`. All scenarios
//! construct an in-memory `AppConfig` and assert on `next_target`'s return value.
//!
//! Issue #8 follow-up: `Profile::last_resort` replaced the old `threshold >=
//! 100.0` sentinel as the chain-walk terminal-stop marker. Several scenarios
//! below deliberately mark a member `last_resort` at a threshold OTHER than
//! 100 to prove the two are now independent.

use crate::lockorder::RankedMutex;
use std::collections::HashMap;
use std::sync::Arc;

use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::usage::{
    PlanInfo, PlanTier, SpendInfo, UsageInfo, UsageStore, UsageWindow, epoch_secs_to_iso,
    now_epoch_secs,
};

/// ISO reset an hour ahead — a live 5h window.
fn live_reset() -> String {
    epoch_secs_to_iso(now_epoch_secs() + 3600)
}

/// ISO reset an hour ago — a lapsed 5h window.
fn expired_reset() -> String {
    epoch_secs_to_iso(now_epoch_secs() - 3600)
}

fn window(utilization: f64, resets_at: Option<String>) -> UsageWindow {
    UsageWindow {
        utilization,
        resets_at,
    }
}

fn usage_info(five_hour: Option<UsageWindow>) -> UsageInfo {
    UsageInfo {
        five_hour,
        ..UsageInfo::default()
    }
}

/// A canceled account: its `/profile` dropped to Free with a `canceled` status,
/// yet its cached 5h window still reads as idle headroom (5%) — the exact shape
/// that made a dead account look like a prime rotation target.
fn canceled_usage() -> UsageInfo {
    UsageInfo {
        five_hour: Some(window(5.0, Some(live_reset()))),
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..UsageInfo::default()
    }
}

fn profile_with_usage(name: &str, threshold: Option<f64>, usage: Option<UsageInfo>) -> Profile {
    use std::collections::BTreeMap;
    Profile {
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: threshold,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// Profile whose 5h window is live (future reset) at the given utilization —
/// the exhaustion predicates only trust a live window.
fn profile_with_util(name: &str, threshold: Option<f64>, utilization: Option<f64>) -> Profile {
    profile_with_usage(
        name,
        threshold,
        utilization.map(|u| usage_info(Some(window(u, Some(live_reset()))))),
    )
}

/// Marks a profile as the chain's last resort (`Profile::last_resort = true`).
/// A separate wrapper — rather than threading a bool through every helper —
/// so call sites read as "this member is the sink" instead of inferring it
/// from an incidental threshold value.
fn mark_last_resort(mut p: Profile) -> Profile {
    p.last_resort = true;
    p
}

/// Marks a profile user-disabled (`Profile::disabled = true`) — invisible to
/// the fallback-chain walk though it still sits in `fallback_chain` on disk.
fn mark_disabled(mut p: Profile) -> Profile {
    p.disabled = true;
    p
}

/// Marks a profile's last usage read as live — the UI twin's freshness input.
/// The snapshot twin carries the same judgment on `ChainSnapshot::fresh`.
fn mark_fresh(mut p: Profile) -> Profile {
    p.fetch_status = Some(FetchStatus::Fresh);
    p
}

fn spend_block(enabled: bool, used: f64, limit: Option<f64>) -> SpendInfo {
    SpendInfo {
        enabled,
        used: Some(used),
        limit,
        percent: None,
        currency: None,
    }
}

/// A spent 5h window plus a pay-as-you-go block — the shape a member must have
/// before the spend pass can pick it (every earlier pass needs it exhausted).
fn usage_spent_with_spend(spend: SpendInfo) -> UsageInfo {
    UsageInfo {
        five_hour: Some(window(100.0, Some(live_reset()))),
        spend: Some(spend),
        ..UsageInfo::default()
    }
}

/// Member whose windows are spent but whose account is billing, with `ceiling`
/// dollars allowed unattended.
fn spend_member(name: &str, ceiling: f64, used: f64, limit: Option<f64>) -> Profile {
    let mut p = profile_with_usage(
        name,
        Some(95.0),
        Some(usage_spent_with_spend(spend_block(true, used, limit))),
    );
    p.max_auto_spend = Some(ceiling);
    p
}

fn config_with_chain(profiles: Vec<Profile>, active: &str) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: Some(active.into()),
            profiles: names.clone(),
            fallback_chain: names,
            ..AppState::default()
        },
        profiles,
    }
}

// Both maxed and BOTH marked last_resort at a non-100 threshold: active is
// itself the sink, so it stays parked without the walk ever reaching B.
#[test]
fn all_maxed_sinks_no_switch() {
    let config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(90.0), Some(95.0))),
            mark_last_resort(profile_with_util("b", Some(90.0), Some(95.0))),
        ],
        "a",
    );
    assert_eq!(next_target(&config, None), None);
}

// Active (unmarked, threshold 95) at 100%; B is marked last_resort at an 80%
// threshold — not 100 — proving the migration follows the mark, not the
// threshold value. One migration allowed.
#[test]
fn non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string()))
    );
}

// B active and marked last_resort (80% threshold, not 100) — no further
// migration, exactly like the old threshold==100 sentinel parked.
#[test]
fn sink_active_maxed_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))),
        ],
        "b",
    );
    assert_eq!(next_target(&config, None), None);
}

// Active marked last_resort (80% threshold, maxed), B has headroom (95% @
// 50%) — migrates to B. The headroom pass always wins before the
// last-resort loop guard is even consulted.
#[test]
fn sink_active_switches_to_member_with_headroom() {
    let config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(80.0), Some(100.0))),
            profile_with_util("b", Some(95.0), Some(50.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string()))
    );
}

// No member marked last_resort (both threshold 95 at 100%) — returns None.
#[test]
fn no_sink_available_returns_none() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config, None), None);
}

// ── issue #8 follow-up: threshold no longer implies last_resort ─────────────

// A threshold of 100 alone must NOT act as a sink anymore. With switch_off_when_spent on
// and NEITHER member marked last_resort, switch-off-all still fires even
// though the active sits at an unmarked 100% threshold. The OLD
// `threshold >= 100.0` sentinel treated the active as a sink here and
// returned `None` (the walk stopped dead) — this assertion fails against it.
#[test]
fn unmarked_hundred_threshold_active_no_longer_acts_as_sink() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(100.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

// Same decoupling from the other direction: an unmarked 100%-threshold OTHER
// member (not the active) must not block switch-off-all either. The OLD
// sentinel would have accepted B as a last-resort switch target instead of
// firing Off; unmarked, B is just another exhausted, non-viable member.
#[test]
fn wrap_off_switches_off_when_unmarked_hundred_threshold_member_present() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(100.0), Some(100.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

// ── next_auto_switch_target ───────────────────────────────────────────────────
//
// Same scenarios via the scheduler-side path: snapshot the chain from AppConfig,
// read utilization from UsageStore (not Profile.usage). The split avoids the
// config ↔ store lock inversion against App::apply_usage.

/// Store entries with live 5h windows (future reset) at the given utilizations.
fn store_with_utils(pairs: &[(&str, f64)]) -> UsageStore {
    store_with_infos(
        pairs
            .iter()
            .map(|(name, util)| (*name, usage_info(Some(window(*util, Some(live_reset()))))))
            .collect(),
    )
}

fn store_with_infos(entries: Vec<(&str, UsageInfo)>) -> UsageStore {
    let map: HashMap<String, UsageInfo> = entries
        .into_iter()
        .map(|(name, info)| (name.to_string(), info))
        .collect();
    Arc::new(RankedMutex::new(map))
}

#[test]
fn snapshot_chain_captures_thresholds_and_active() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(20.0))),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    assert_eq!(snap.active, "a");
    assert_eq!(snap.chain.len(), 2);
    assert_eq!(snap.chain[0].name, "a");
    assert!((snap.chain[0].threshold - 95.0).abs() < f64::EPSILON);
    assert!(!snap.chain[0].last_resort);
    assert_eq!(snap.chain[1].name, "b");
    assert!((snap.chain[1].threshold - 80.0).abs() < f64::EPSILON);
    assert!(
        snap.chain[1].last_resort,
        "last_resort is captured independent of threshold"
    );
}

#[test]
fn snapshot_chain_none_when_active_not_in_chain() {
    let mut config = config_with_chain(vec![profile_with_util("a", Some(95.0), Some(50.0))], "a");
    // active is set but absent from the chain
    config.state.fallback_chain = vec!["other".into()];
    assert!(snapshot_chain(&config).is_none());
}

#[test]
fn auto_switch_returns_none_when_active_below_threshold() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 90.0), ("b", 10.0)]); // active at 90% < 95% → no switch
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn auto_switch_picks_member_with_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 20.0)]); // active maxed, B has headroom
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
    );
}

// Both marked last_resort at a non-100 threshold (80%) — active is itself the
// sink; the loop guard holds and no migration to B forms.
#[test]
fn auto_switch_sink_loop_guard_holds() {
    let config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(80.0), None)),
            mark_last_resort(profile_with_util("b", Some(80.0), None)),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // both maxed sinks → no migration
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// Parity with `next_target`'s decoupling on the snapshot walk: an unmarked
// threshold-100 member is a late switch point, never a sink — with everyone
// exhausted and nothing marked, wrap-off switches off instead of parking.
#[test]
fn auto_switch_unmarked_hundred_threshold_member_is_not_a_sink() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(100.0), None),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
        "threshold 100 without the mark must not park the snapshot walk"
    );
}

// A (unmarked, threshold 95%) is not a sink; B is marked last_resort at an
// 80% threshold — one migration to B.
#[test]
fn auto_switch_non_sink_active_migrates_to_sink_once() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            mark_last_resort(profile_with_util("b", Some(80.0), None)),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // active not a sink, B is → one migration
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
    );
}

#[test]
fn auto_switch_missing_util_is_not_exhausted() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("b", 10.0)]); // active absent from store → not exhausted → no switch
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// ── wrap-off mode ───────────────────────────────────────────────────────────
//
// When no member is marked last_resort and the whole chain is exhausted,
// wrap-off turns off all accounts instead of staying put.

// next_target: switch_off_when_spent on, no last_resort member, all exhausted → Off.
#[test]
fn wrap_off_switches_off_when_chain_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

// next_target: switch_off_when_spent on but a last_resort member exists (at an 80%
// threshold, not 100) → migrate there, not Off.
#[test]
fn wrap_off_prefers_sink_over_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string()))
    );
}

// next_target: switch_off_when_spent on but active still has headroom → no Off.
#[test]
fn wrap_off_skips_off_when_active_has_headroom() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    // a at 50% < 95% → not exhausted → stay
    assert_eq!(next_target(&config, None), None);
}

// next_target: same spent chain, switch_off_when_spent off → legacy None.
#[test]
fn wrap_off_disabled_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    assert_eq!(next_target(&config, None), None);
}

// ── soonest_resume ───────────────────────────────────────────────────────────
//
// All-exhausted caption data source (issue #10 follow-up): names the chain
// member that resumes soonest, valid only when EVERY member is currently
// exhausted.

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

#[test]
fn soonest_resume_empty_chain_is_none() {
    let config = config_with_chain(vec![], "a");
    assert_eq!(soonest_resume(&config), None);
}

#[test]
fn soonest_resume_picks_the_soonest_reset() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(1800)))))),
            ),
        ],
        "a",
    );
    let (name, eta) = soonest_resume(&config).expect("all exhausted");
    assert_eq!(name, "b", "b resets sooner than a");
    assert!((1700..=1800).contains(&eta), "eta ~1800s, got {eta}");
}

// Ties on `resets_at` keep the earlier chain-order member.
#[test]
fn soonest_resume_ties_keep_earlier_chain_order() {
    let reset = reset_in(3600);
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset.clone()))))),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset))))),
            ),
        ],
        "a",
    );
    let (name, _) = soonest_resume(&config).expect("all exhausted");
    assert_eq!(name, "a", "a tie keeps the earlier chain-order member");
}

// b's utilization is below its own threshold — already recovered, so the
// all-exhausted premise doesn't hold; recovery would relink it next tick.
#[test]
fn soonest_resume_none_when_one_member_recovered() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
            ),
            profile_with_util("b", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(soonest_resume(&config), None);
}

// b's 5h window already reset — headroom again whatever its stale util says
// (same wall-clock rule `find_recovered_member`/`is_exhausted` use).
#[test]
fn soonest_resume_none_when_a_member_window_expired() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(expired_reset()))))),
            ),
        ],
        "a",
    );
    assert_eq!(soonest_resume(&config), None);
}

// A chain member with no resolvable profile (deleted, still listed) can't be
// proven exhausted — bail rather than pick around it.
#[test]
fn soonest_resume_none_when_chain_member_missing_profile() {
    let mut config = config_with_chain(
        vec![profile_with_usage(
            "a",
            Some(95.0),
            Some(usage_info(Some(window(100.0, Some(reset_in(3600)))))),
        )],
        "a",
    );
    config.state.fallback_chain.push("ghost".into());
    assert_eq!(soonest_resume(&config), None);
}

// A disabled member is never a switch candidate, so its idle cached 5h
// window must not bail the whole caption to None: the reachable, genuinely
// hard-exhausted member still reports its reset.
#[test]
fn soonest_resume_skips_a_disabled_member_holding_an_idle_window() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "reachable",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(1800)))))),
            ),
            mark_disabled(profile_with_util("dead", Some(95.0), Some(5.0))),
        ],
        "reachable",
    );
    let (name, eta) = soonest_resume(&config).expect("reachable member is hard-exhausted");
    assert_eq!(name, "reachable", "the disabled member must be skipped");
    assert!((1700..=1800).contains(&eta), "eta ~1800s, got {eta}");
}

// Same shape, auth-broken instead of disabled — the other `candidate_excluded`
// axis that isn't usage-derived.
#[test]
fn soonest_resume_skips_an_auth_broken_member_holding_an_idle_window() {
    let mut config = config_with_chain(
        vec![
            profile_with_usage(
                "reachable",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(1800)))))),
            ),
            profile_with_util("dead", Some(95.0), Some(5.0)),
        ],
        "reachable",
    );
    config.set_auth_broken("dead", true);
    let (name, eta) = soonest_resume(&config).expect("reachable member is hard-exhausted");
    assert_eq!(name, "reachable", "the auth-broken member must be skipped");
    assert!((1700..=1800).contains(&eta), "eta ~1800s, got {eta}");
}

// Same shape, canceled instead of disabled — the exact regression fixture:
// a canceled account whose cached 5h window still reads idle headroom.
#[test]
fn soonest_resume_skips_a_canceled_member_holding_an_idle_window() {
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "reachable",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(reset_in(1800)))))),
            ),
            profile_with_usage("dead", Some(95.0), Some(canceled_usage())),
        ],
        "reachable",
    );
    let (name, eta) = soonest_resume(&config).expect("reachable member is hard-exhausted");
    assert_eq!(name, "reachable", "the canceled member must be skipped");
    assert!((1700..=1800).contains(&eta), "eta ~1800s, got {eta}");
}

// Guard: a chain of just ONE dead member, sitting at/over its own
// threshold — it would read as hard-exhausted if the skip didn't apply, so
// an unfixed/reverted loop would happily report it as the soonest-resume
// answer. A disabled member is never a candidate at all, so the correct
// result is None (nothing left to report), not Some(dead, eta). This is the
// fixture that actually discriminates the fix from a revert: an EARLIER
// version of this guard put a real-headroom "reachable" member first in the
// chain, which returns None on its own (already pinned by
// `soonest_resume_none_when_one_member_recovered`) regardless of whether the
// dead-member skip exists at all.
#[test]
fn soonest_resume_none_when_the_only_chain_member_is_a_dead_one_over_threshold() {
    let config = config_with_chain(
        vec![mark_disabled(profile_with_util(
            "dead",
            Some(95.0),
            Some(100.0),
        ))],
        "dead",
    );
    assert_eq!(soonest_resume(&config), None);
}

// next_auto_switch_target: scheduler-side wrap-off → Off when chain spent.
#[test]
fn auto_switch_wrap_off_switches_off_when_chain_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.switch_off_when_spent);
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]); // both over 95% threshold, no sink → Off
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
    );
}

// ── recovery_target ──────────────────────────────────────────────────────────
//
// After switch-off-all (no active profile), find a chain member whose
// utilization has dropped below its threshold.

#[test]
fn find_recovered_returns_first_member_below_threshold() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
    ];
    let store = store_with_utils(&[("a", 100.0), ("b", 40.0)]);
    assert_eq!(
        find_recovered_member(&members, &store, &[]),
        Some("b".to_string()),
    );
    assert_eq!(
        find_recovered_member(&members, &store, &["b".to_string()]),
        None,
        "a kick-rejected member's idle usage is not recovery — its account \
         still refuses inference"
    );
}

#[test]
fn find_recovered_skips_exhausted_members() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
    ];
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(find_recovered_member(&members, &store, &[]), None);
}

#[test]
fn find_recovered_returns_none_when_no_member_has_data() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
    ];
    let store = store_with_utils(&[]); // no usage data for any member
    assert_eq!(find_recovered_member(&members, &store, &[]), None);
}

#[test]
fn find_recovered_uses_threshold_per_member() {
    let members = vec![
        ChainMember {
            name: "a".into(),
            threshold: 90.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        }, // 95% util ≥ 90 → exhausted
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        }, // 94% util < 95 → recovered
    ];
    let store = store_with_utils(&[("a", 95.0), ("b", 94.0)]);
    assert_eq!(
        find_recovered_member(&members, &store, &[]),
        Some("b".to_string()),
    );
}

// A member whose 5h window passed its reset has headroom again whatever its
// last-known utilization says — the wall clock recovered it (issue #2: a sole
// switched-off profile stayed "spent" forever on a stale 100% snapshot).
#[test]
fn find_recovered_recovers_when_window_expired() {
    let members = vec![ChainMember {
        name: "a".into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
    }];
    let store = store_with_infos(vec![(
        "a",
        usage_info(Some(window(100.0, Some(expired_reset())))),
    )]);
    assert_eq!(
        find_recovered_member(&members, &store, &[]),
        Some("a".to_string()),
    );
}

// A fetched entry with no 5h window at all (idle account after a reset) is
// recovered; only an ABSENT store entry stays undecidable.
#[test]
fn find_recovered_recovers_when_windowless() {
    let members = vec![ChainMember {
        name: "a".into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
    }];
    let store = store_with_infos(vec![("a", usage_info(None))]);
    assert_eq!(
        find_recovered_member(&members, &store, &[]),
        Some("a".to_string()),
    );
}

// No resets_at means the window can't be proven live, so it can't hold the
// member exhausted — same reading `five_hour_live` gives the auto-start leg.
#[test]
fn find_recovered_treats_missing_resets_at_as_lapsed() {
    let members = vec![ChainMember {
        name: "a".into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
    }];
    let store = store_with_infos(vec![("a", usage_info(Some(window(100.0, None))))]);
    assert_eq!(
        find_recovered_member(&members, &store, &[]),
        Some("a".to_string()),
    );
}

// The scheduler-side gate must not switch away from an active whose spent
// window already reset — stale-high utilization, wall clock passed.
#[test]
fn auto_switch_ignores_expired_window_active() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(50.0, Some(live_reset()))))),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// A chain member whose 100% window lapsed is a viable switch target again.
#[test]
fn next_target_accepts_member_with_expired_window() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_info(Some(window(100.0, Some(expired_reset()))))),
            ),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".into()))
    );
}

// next_auto_switch_target: switch_off_when_spent off, spent chain → legacy None.
#[test]
fn auto_switch_wrap_off_disabled_stays_put() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// ── AUTH-1: auth-broken members excluded from the chain walk ──────────────────
//
// A member whose OAuth refresh is revoked/invalid (`AppState::auth_broken`) must
// never be picked as a switch target — installing its dead token would log out
// every running claude (Incident C). Excluded in the headroom pass AND the sink
// pass, on both the config-side (`next_target`) and scheduler-side
// (`next_auto_switch_target`) walks.

// next_target: broken member with headroom is skipped; the next viable one wins.
#[test]
fn next_target_skips_broken_member_picks_next() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted
            profile_with_util("b", Some(95.0), Some(20.0)),  // headroom but broken
            profile_with_util("c", Some(95.0), Some(20.0)),  // headroom, viable
        ],
        "a",
    );
    config.set_auth_broken("b", true);
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into()))
    );
}

// next_target: the only non-active member is broken → nothing viable → None.
#[test]
fn next_target_returns_none_when_only_alternative_is_broken() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted
            profile_with_util("b", Some(95.0), Some(20.0)),  // headroom but broken
        ],
        "a",
    );
    config.set_auth_broken("b", true);
    assert_eq!(next_target(&config, None), None);
}

// next_auto_switch_target: the scheduler-side walk skips broken members too
// (snapshot_chain carries `auth_broken` into ChainSnapshot.broken).
#[test]
fn auto_switch_skips_broken_member_picks_next() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    config.set_auth_broken("b", true);
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.broken.iter().any(|n| n == "b"));
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0), ("c", 10.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".into())),
    );
}

// A broken 100%-sink is excluded from the sink pass: with wrap-off on and no
// other viable member, the chain switches OFF rather than installing the dead
// sink's token.
#[test]
fn next_target_broken_sink_wrap_off_switches_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted, not a sink
            profile_with_util("b", Some(100.0), Some(100.0)), // sink — but broken
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    config.set_auth_broken("b", true);
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

// ── canceled subscription: excluded from selection, own blocked-reason chip ───
//
// A canceled account keeps a stale low-utilization 5h window (filip2 cached at 5h
// 5%), so by headroom it looks like a PRIME rotation target — yet every session
// started there 403s. Treated like `broken`: skipped as a candidate on both
// walks, and a canceled ACTIVE is itself a switch trigger.

// next_target: a canceled member with idle-looking headroom is skipped; the
// next viable member wins.
#[test]
fn next_target_skips_canceled_member_picks_next() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted
            profile_with_usage("b", Some(95.0), Some(canceled_usage())), // headroom but canceled
            profile_with_util("c", Some(95.0), Some(20.0)),  // headroom, viable
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into())),
    );
}

// next_target: the only alternative is canceled → nothing viable → None.
#[test]
fn next_target_returns_none_when_only_alternative_is_canceled() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_usage("b", Some(95.0), Some(canceled_usage())),
        ],
        "a",
    );
    assert_eq!(next_target(&config, None), None);
}

// next_auto_switch_target: the scheduler-side walk skips a canceled member too,
// reading the canceled state straight off the usage snapshot.
#[test]
fn auto_switch_skips_canceled_member_picks_next() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", canceled_usage()),
        ("c", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".into())),
    );
}

// ── disabled: USER CHOICE exclusion, broader than auth_broken ────────────────
//
// A disabled account stays in `fallback_chain` on disk (the TUI still lists
// it) but must never be picked as a switch source, target, or last_resort
// sink by either walk. Treated the same as `broken`/`canceled` by the walk,
// but never surfaced as a chip reason here (render-only, a later pair's job).

// next_target: chain [a, b(disabled), c] — b has headroom but is disabled, so
// c (also headroom) is picked instead. Pins the exact spec scenario.
#[test]
fn next_target_skips_disabled_member_picks_next() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted
            mark_disabled(profile_with_util("b", Some(95.0), Some(20.0))), // headroom but disabled
            profile_with_util("c", Some(95.0), Some(20.0)),  // headroom, viable
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into())),
    );
}

// next_target: the only alternative is disabled → nothing viable → None.
#[test]
fn next_target_returns_none_when_only_alternative_is_disabled() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_disabled(profile_with_util("b", Some(95.0), Some(20.0))),
        ],
        "a",
    );
    assert_eq!(next_target(&config, None), None);
}

// A disabled member marked `last_resort` must never serve as the chain's
// sink — active (not itself a sink) has nothing viable to migrate to, so the
// walk returns None instead of parking on the disabled sink.
#[test]
fn next_target_disabled_last_resort_is_never_a_sink() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted, not a sink
            mark_disabled(mark_last_resort(profile_with_util(
                "b",
                Some(80.0),
                Some(100.0),
            ))),
        ],
        "a",
    );
    assert_eq!(next_target(&config, None), None);
}

// next_auto_switch_target: the scheduler-side walk skips a disabled member too
// (snapshot_chain drops it out of ChainSnapshot.chain entirely).
#[test]
fn auto_switch_skips_disabled_member_picks_next() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            mark_disabled(profile_with_util("b", Some(95.0), None)),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(
        !snap.chain.iter().any(|m| m.name == "b"),
        "a disabled member must not enter the snapshot's chain at all"
    );
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0), ("c", 10.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".into())),
    );
}

// A disabled ACTIVE must never wedge the scheduler-side walk. Unlike a
// disabled non-active member (dropped from `.chain` above), the active slot
// has to stay RESOLVABLE: `next_auto_switch_target_with_usage` looks itself up
// via `snapshot.chain.iter().position(|m| m.name == snapshot.active)`, and a
// missing active means that `?` returns `None` every tick forever — no
// candidate ever gets evaluated again. `disabled` is a candidate-only
// exclusion (mirrors `next_target`'s skip closure, which never skips
// `chain[i] == active`), never a reason to drop the active itself.
#[test]
fn auto_switch_disabled_active_still_evaluates_instead_of_wedging() {
    let config = config_with_chain(
        vec![
            mark_disabled(profile_with_util("a", Some(95.0), None)),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot must resolve even with a disabled active");
    assert!(
        snap.chain.iter().any(|m| m.name == "a"),
        "the active member must stay in the snapshot's chain even when disabled — \
         next_auto_switch_target_with_usage's position() lookup wedges forever otherwise"
    );
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".into())),
        "a disabled active must still be evaluated and walked away from, not stuck forever"
    );
}

// A canceled ACTIVE whose cached window reads as idle headroom still triggers
// the walk — every request 403s, so staying is fatal (mirrors AUTH-4).
#[test]
fn auto_switch_canceled_active_walks_away_despite_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", canceled_usage()), // active, canceled, 5% (idle headroom)
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".into())),
    );
}

// The render-only chip mirrors the walk: a canceled profile reports the
// dead-first `Canceled` reason, above every quota/limiter block.
#[test]
fn blocked_reason_reports_canceled_first() {
    let config = config_with_chain(
        vec![profile_with_usage("a", Some(95.0), Some(canceled_usage()))],
        "a",
    );
    let profile = config.find("a").expect("profile");
    assert_eq!(
        blocked_reason(&config, profile, None),
        Some(BlockedReason::Canceled),
    );

    // A genuine free account (no canceled status) with headroom is NOT blocked.
    let config = config_with_chain(vec![profile_with_util("a", Some(95.0), Some(20.0))], "a");
    let profile = config.find("a").expect("profile");
    assert_eq!(blocked_reason(&config, profile, None), None);
}

// ── AUTH-4: an auth-broken ACTIVE is itself a switch trigger ──────────────────
//
// A dead login can never produce a fresh usage read again, so its store entry
// freezes at whatever it last held — typically a lapsed 5h window that reads
// as idle headroom. Gating the walk on the active's exhaustion therefore
// wedged the daemon on the dead account forever while a viable sibling idled
// (observed 2026-07-09: the active broke mid-window; the next member sat at
// 0%). The broken flag is terminal-confirmed (set only after a rejected
// refresh AND a failed live-mirror adopt), so walking away is the only move
// that keeps the machine serving.

// Broken active whose frozen last read is a lapsed window (reads as idle
// headroom, NOT exhausted) → the walk runs anyway and leaves for the sibling.
#[test]
fn auto_switch_broken_active_walks_away_despite_stale_headroom() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.set_auth_broken("a", true);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        // The active's last-ever read: maxed on a window that has since
        // lapsed — the exact frozen shape the wedge held.
        ("a", usage_info(Some(window(100.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".into())),
    );
}

// Kick-rejected active (the messages limiter refusing its inference while
// `/usage` stays green): idle-looking usage must not hold the chain on it —
// the walk bypasses the exhaustion gate exactly like `broken` and leaves for
// the healthy sibling.
#[test]
fn auto_switch_kick_rejected_active_walks_away_despite_idle_usage() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let mut snap = snapshot_chain(&config).expect("snapshot");
    snap.kick_rejected = vec!["a".to_string()];
    let store = store_with_infos(vec![
        // The rejected active's live read: an idle, lapsed window — exactly the
        // shape the 2026-07-15 outage froze uwuclxdy in while healthy siblings
        // idled with live windows.
        ("a", usage_info(Some(window(2.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".into())),
    );
}

// A kick-rejected CANDIDATE is walked around in both passes — headroom and
// last-resort alike — because its account rejects inference no matter how idle
// its usage reads.
#[test]
fn auto_switch_never_targets_a_kick_rejected_member() {
    let mut profiles = vec![
        profile_with_util("a", Some(95.0), None),
        profile_with_util("b", Some(95.0), None),
        profile_with_util("c", Some(95.0), None),
    ];
    profiles[2].last_resort = true;
    let config = config_with_chain(profiles, "a");
    let mut snap = snapshot_chain(&config).expect("snapshot");
    snap.kick_rejected = vec!["b".to_string()];
    let store = store_with_infos(vec![
        // Exhausted active, idle-but-rejected b → the walk must land on c.
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(5.0, Some(live_reset()))))),
        ("c", usage_info(Some(window(20.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".into())),
    );

    // Same chain with the rejection also covering the last resort: nothing
    // viable remains and the active stays put (no Off — switch_off_when_spent is unset).
    snap.kick_rejected = vec!["b".to_string(), "c".to_string()];
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// Fresh-PREFERENCE walk, asserted on both twins in each direction. The UI twin
// reads `Profile.fetch_status`, the snapshot twin reads `ChainSnapshot::fresh`
// (filled by the scheduler's scan from the same `StatusStore` the ACTIVE gate
// consults), so a drift between them fails here.

// Two members have headroom and the one the walk reaches FIRST is stale: the
// fresh member wins anyway, since pass 1a outranks walk order.
#[test]
fn next_target_prefers_fresh_member_over_earlier_stale_one() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(10.0)),
            mark_fresh(profile_with_util("c", Some(95.0), Some(20.0))),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".to_string())),
        "b is reached first but its read is stale — the trusted c must win"
    );
}

#[test]
fn auto_switch_prefers_fresh_member_over_earlier_stale_one() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    let mut snap = snapshot_chain(&config).expect("snapshot");
    snap.fresh = vec!["c".to_string()];
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0), ("c", 20.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".to_string())),
        "b is reached first but is absent from `fresh` — the trusted c must win"
    );
}

// The not-a-gate guard: with the only headroom candidate stale, pass 1b still
// accepts it. Freshness may reorder the walk, never veto it — an exhausted
// active must keep its escape (2026-06-28 target asymmetry).
#[test]
fn next_target_still_picks_a_stale_member_when_no_fresh_one_has_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string())),
        "freshness is a preference, not a gate: the stale escape must stay open"
    );
}

#[test]
fn auto_switch_still_picks_a_stale_member_when_no_fresh_one_has_headroom() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(
        snap.fresh.is_empty(),
        "snapshot_chain cannot know freshness"
    );
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
        "freshness is a preference, not a gate: the stale escape must stay open"
    );
}

// ── spend budget: the money math (`spend_room`) ────────────────────────────
// Pure, and shared by both twins, so the dollar rules are pinned once here and
// the walk tests below only cover ordering.

// The load-bearing one: `is_visible()` is TRUE for an account carrying a stale
// limit with billing switched OFF (kerry, observed live 2026-07-17). Arming on
// it would hop the chain into an account that refuses the request, with money
// attached. Both halves are asserted so the contrast can't rot: if a later
// change makes `is_visible` mean "will bill", this test says so out loud.
#[test]
fn spend_room_needs_billing_actually_on_not_merely_visible() {
    // kerry's live shape, verbatim: billing off, a $50 limit still on file,
    // $31.07 spent under it. The ceiling is deliberately $50 so that arming on
    // `is_visible` would find real room ($45 - $31.07) and hop — a fixture
    // whose spend already exceeded the room would pass for the wrong reason.
    let billing_off = spend_block(false, 31.07, Some(50.0));
    assert!(
        billing_off.is_visible(),
        "precondition: this shape renders a bar — that is exactly the trap"
    );
    assert_eq!(spend_room(&billing_off, 50.0), None);

    // Flip billing on and the very same numbers do arm, proving the refusal
    // above came from `enabled` alone and not from the dollar math.
    let billing_on = spend_block(true, 31.07, Some(50.0));
    assert!(spend_room(&billing_on, 50.0).is_some());
}

// A $0 ceiling can never be entered for spend reasons, whatever the account
// allows — the "both defaults off = today's behavior" guarantee.
#[test]
fn spend_room_zero_or_negative_ceiling_never_arms() {
    let billing_on = spend_block(true, 0.0, Some(500.0));
    assert_eq!(spend_room(&billing_on, 0.0), None);
    assert_eq!(spend_room(&billing_on, -5.0), None);
}

// A non-finite ceiling is refused rather than treated as "no limit". Both are
// valid TOML floats: `inf` with no account cap yields infinite room (unlimited
// unattended spending), and NaN slips every `<= 0.0` test while `f64::min`
// silently drops it, arming at the account's full limit.
#[test]
fn spend_room_non_finite_ceiling_never_arms() {
    let uncapped = spend_block(true, 0.0, None);
    assert_eq!(spend_room(&uncapped, f64::INFINITY), None);
    assert_eq!(spend_room(&uncapped, f64::NAN), None);

    let capped = spend_block(true, 0.0, Some(500.0));
    assert_eq!(spend_room(&capped, f64::INFINITY), None);
    assert_eq!(
        spend_room(&capped, f64::NAN),
        None,
        "f64::min(NAN, cap) is the cap — a NaN ceiling must never inherit it"
    );
}

// Whichever cap binds first wins: the account's own limit can be lower than the
// member's ceiling, and then it — not the ceiling — sets the room.
#[test]
fn spend_room_binds_on_the_lower_of_account_cap_and_ceiling() {
    // Account cap $10 under a $100 ceiling → 90% of $10, less $4 spent.
    assert_eq!(
        spend_room(&spend_block(true, 4.0, Some(10.0)), 100.0),
        Some(5.0)
    );
    // Ceiling $10 under a $100 account cap → same room, other side binding.
    assert_eq!(
        spend_room(&spend_block(true, 4.0, Some(100.0)), 10.0),
        Some(5.0)
    );
    // Billing on with no declared account cap → the ceiling alone bounds it.
    assert_eq!(spend_room(&spend_block(true, 4.0, None), 10.0), Some(5.0));
}

// The 10% margin is a hard stop, not a hint: at or past it the member is done.
// The exact leftover a cent under the line is float noise, so only its sign is
// asserted; the line itself lands exactly (`0.9 * 10.0 == 9.0` in f64), so the
// at-the-margin and past-it cases pin an exact `None`.
#[test]
fn spend_room_stops_at_the_ninety_percent_margin() {
    assert!(spend_room(&spend_block(true, 8.99, Some(10.0)), 10.0).is_some());
    assert_eq!(spend_room(&spend_block(true, 9.0, Some(10.0)), 10.0), None);
    assert_eq!(spend_room(&spend_block(true, 50.0, Some(10.0)), 10.0), None);
}

// Unknown spend must refuse, not read as $0 spent. `RawMoney::to_dollars`
// returns `None` whenever `amount_minor` is absent or renamed, and `used` is the
// only input that bounds spending — so a wire rename defaulting it to 0 would
// hand back the full cap on an account that may already be maxed.
#[test]
fn spend_room_refuses_when_used_is_unknown() {
    let unknown = SpendInfo {
        enabled: true,
        used: None,
        limit: Some(50.0),
        percent: None,
        currency: None,
    };
    assert_eq!(spend_room(&unknown, 20.0), None);
}

// Two members with budget must not ping-pong. Once the chain is paying on one,
// every free-quota member has already been passed over, so hopping to the other
// paying member buys nothing and relinks live credentials every tick. Same loop
// guard `last_resort` carries, for the same reason.
#[test]
fn next_target_does_not_hop_between_two_spend_armed_members() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 20.0, 0.0, Some(50.0)),
            spend_member("c", 20.0, 0.0, Some(50.0)),
        ],
        "b",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        None,
        "b is paying and still within budget: hopping to c gains nothing"
    );

    // A member with FREE quota still wins, though — the guard must not strand
    // the chain on a paying account when someone can serve for nothing.
    config.profiles[2] = profile_with_util("c", Some(95.0), Some(10.0));
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".to_string())),
        "free quota always beats staying on a paying active"
    );
}

#[test]
fn auto_switch_does_not_hop_between_two_spend_armed_members() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            spend_member("b", 20.0, 0.0, Some(50.0)),
            spend_member("c", 20.0, 0.0, Some(50.0)),
        ],
        "b",
    );
    config.state.spend_budget_switching = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
        (
            "c",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        None,
        "the scheduler must hold the same loop guard as the UI twin"
    );
}

// ── spend budget: the ceiling is a CAP, not just an entry gate ──────────────

// The walk skips the ACTIVE member in every pass, so nothing re-checks a
// spend-armed member once the chain has landed on it. Without a budget-aware
// halt it settles there and bills to the ACCOUNT's cap — `max_auto_spend` would
// silently mean "may start paying below this" rather than the ceiling it is
// named for. Asserted at the steady state (the armed member IS active), which
// is the tick the entry-side tests never reach.
#[test]
fn next_target_over_budget_active_switches_off_by_default() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            // $4.60 spent against a $5 ceiling: past 90% of it, so the budget is
            // gone even though the ACCOUNT would happily bill to $50.
            spend_member("b", 5.0, 4.6, Some(50.0)),
        ],
        "b",
    );
    config.state.spend_budget_switching = true;
    assert!(
        config.state.switch_off_when_budget_spent,
        "a spent budget halts by default"
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::Off),
        "the ceiling must stop the spending, not merely gate entry to it"
    );

    // Still inside the budget → keeps working, which is the point of the knob.
    config.profiles[1].usage = Some(usage_spent_with_spend(spend_block(true, 1.0, Some(50.0))));
    assert_eq!(
        next_target(&config, None),
        None,
        "under budget, stay and bill"
    );
}

// `switch_off_when_budget_spent` is its own decision, not `switch_off_when_spent`'s: staying costs
// nothing when free quota runs out and costs money when a budget does, so an
// operator may want stay-on-active for one and switch-off-all for the other.
#[test]
fn next_target_over_budget_active_can_be_told_to_keep_billing() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 5.0, 4.6, Some(50.0)),
        ],
        "b",
    );
    config.state.spend_budget_switching = true;
    config.state.switch_off_when_budget_spent = false;
    assert_eq!(
        next_target(&config, None),
        None,
        "explicitly told to stay on a spent budget: keeps billing"
    );

    // ...and `switch_off_when_spent` must not answer for it in either direction: on, it
    // halts a chain out of QUOTA, and this chain is out of MONEY.
    config.state.switch_off_when_spent = true;
    assert_eq!(
        next_target(&config, None),
        None,
        "switch_off_when_spent must not halt an over-budget active that was told to stay"
    );
}

// A free parking spot beats halting: moving to the sink stops the billing
// without signing anyone out, so the halt is the last resort, not the first.
#[test]
fn next_target_over_budget_active_parks_on_a_sink_before_halting() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 5.0, 4.6, Some(50.0)),
            mark_last_resort(profile_with_util("c", Some(95.0), Some(100.0))),
        ],
        "b",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".to_string())),
        "park on the sink to stop billing rather than halting outright"
    );
}

// The bit-identical guarantee at the halt gate: with the master toggle off, a
// billing account over any ceiling must read `switch_off_when_spent` exactly like before.
#[test]
fn next_target_over_budget_halt_is_inert_with_the_toggle_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 5.0, 4.6, Some(50.0)),
        ],
        "b",
    );
    assert!(!config.state.spend_budget_switching, "default is off");
    assert_eq!(
        next_target(&config, None),
        None,
        "switch_off_when_spent off + toggle off → stay, exactly like before the budget existed"
    );

    config.state.switch_off_when_spent = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::Off),
        "switch_off_when_spent on → Off, decided by switch_off_when_spent alone"
    );
}

// A plain subscription active must never reach the budget halt: `budget_spent`
// is what separates "out of money" from "out of quota", and reading it wrong
// would halt ordinary accounts that were only ever told to stay on active.
#[test]
fn budget_spent_never_fires_on_a_subscription_account() {
    let plain = usage_info(Some(window(100.0, Some(live_reset()))));
    assert!(
        !budget_spent(Some(&plain), true, 5.0),
        "no spend block at all"
    );

    let billing_off = usage_spent_with_spend(spend_block(false, 99.0, Some(50.0)));
    assert!(
        !budget_spent(Some(&billing_off), true, 5.0),
        "billing switched off: spending is not what is happening here"
    );

    let no_ceiling = usage_spent_with_spend(spend_block(true, 99.0, Some(50.0)));
    assert!(
        !budget_spent(Some(&no_ceiling), true, 0.0),
        "a $0 ceiling never opted in, so it can never be over budget"
    );
    assert!(
        !budget_spent(Some(&no_ceiling), false, 5.0),
        "master toggle off: the whole feature is inert"
    );
}

// The uncapped-config predicate behind both the Fallback card's DANGER tooltip
// and the daemon's boot warning. "Uncapped" is precisely: armed to spend, told
// to stay once the budget runs out, and no sink to be parked on instead —
// remove any one of those and something stops the billing.
#[test]
fn spend_is_uncapped_only_when_nothing_can_stop_the_billing() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 5.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    config.state.switch_off_when_budget_spent = false;
    assert!(
        spend_is_uncapped(&config, 5.0),
        "armed + stay-on-active + no sink = the ceiling never stops anything"
    );

    // Each of the three, alone, caps it again.
    config.state.switch_off_when_budget_spent = true;
    assert!(
        !spend_is_uncapped(&config, 5.0),
        "halting stops the billing"
    );

    config.state.switch_off_when_budget_spent = false;
    config.profiles[0].last_resort = true;
    assert!(
        !spend_is_uncapped(&config, 5.0),
        "a sink to park on stops the billing without halting"
    );

    config.profiles[0].last_resort = false;
    config.state.spend_budget_switching = false;
    assert!(!spend_is_uncapped(&config, 5.0), "never armed at all");

    config.state.spend_budget_switching = true;
    assert!(
        !spend_is_uncapped(&config, 0.0),
        "a $0 ceiling never spends"
    );
}

// A parking spot the walk cannot reach is not a parking spot. The sink has to
// be a chain member the target walk would actually accept, so a scan of every
// profile on disk reads an unreachable sink as safety that isn't there and
// leaves the DANGER tooltip and the daemon boot warning silent while the chain
// bills.
#[test]
fn spend_is_uncapped_ignores_a_sink_the_walk_cannot_reach() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 5.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    config.state.switch_off_when_budget_spent = false;
    config.profiles[0].last_resort = true;
    assert!(
        !spend_is_uncapped(&config, 5.0),
        "a reachable sink stops the billing"
    );

    // Same sink, now auth-broken: the walk skips it, so it parks nothing.
    config.state.auth_broken = vec!["a".into()];
    assert!(
        spend_is_uncapped(&config, 5.0),
        "an auth-broken sink is not a parking spot"
    );

    // Same sink, off the chain entirely: unreachable for the same reason.
    config.state.auth_broken.clear();
    config.state.fallback_chain.retain(|n| n.as_str() != "a");
    assert!(
        spend_is_uncapped(&config, 5.0),
        "a sink outside the chain is not a parking spot"
    );
}

// `budget_spent_blocking` (the Usage-tab diagnostic's reader) gates a spent
// budget behind 5h-exhaustion exactly like `blocked_reason`: a billing member
// that maxed its money budget but still has free 5h quota serves for FREE and the
// walk prefers it, so the diagnostic must NOT claim it blocked. Only once the 5h
// window is also over threshold is the budget the thing stopping it. Guards the
// render-only invariant against a regression to a raw `budget_spent` read.
#[test]
fn budget_spent_blocking_needs_5h_exhaustion_too() {
    let member = |five_h: f64| {
        let mut p = profile_with_usage(
            "a",
            Some(95.0),
            Some(UsageInfo {
                five_hour: Some(window(five_h, Some(live_reset()))),
                spend: Some(spend_block(true, 99.0, Some(50.0))),
                ..UsageInfo::default()
            }),
        );
        p.max_auto_spend = Some(5.0);
        p
    };
    let cfg = |p: Profile| {
        let mut c = config_with_chain(vec![p], "a");
        c.state.spend_budget_switching = true;
        c
    };

    // 5h has headroom (50% < 95%): serves free, so a maxed money budget is moot.
    let free = cfg(member(50.0));
    assert!(
        !budget_spent_blocking(&free, &free.profiles[0]),
        "free 5h quota → budget is not a block"
    );

    // 5h over threshold (100% >= 95%): nothing free left, so the budget blocks.
    let spent = cfg(member(100.0));
    assert!(
        budget_spent_blocking(&spent, &spent.profiles[0]),
        "no free quota + budget maxed → really blocked"
    );
}

// ── spend budget: walk ordering ─────────────────────────────────────────────

// Everything is spent and one member is billing with room → the chain pays
// rather than parking.
#[test]
fn next_target_picks_a_spend_armed_member_when_the_chain_is_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string()))
    );
}

// The bit-identical guard: the SAME fixture with the master toggle off must
// behave exactly like today — nothing viable, so the walk stays put.
#[test]
fn next_target_spend_budget_off_never_spends() {
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    assert!(!config.state.spend_budget_switching, "default must be off");
    assert_eq!(next_target(&config, None), None);
}

// Toggle on, ceiling $0 → identical to the toggle being off. Both halves of the
// opt-in are required before a cent is spent.
#[test]
fn next_target_zero_ceiling_never_spends_even_with_the_toggle_on() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 0.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(next_target(&config, None), None);
}

// Free quota always beats paying, whatever the walk order says: c has headroom
// and is reached LAST, b is spend-armed and reached first.
#[test]
fn next_target_subscription_headroom_beats_a_spend_armed_member() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 20.0, 0.0, Some(50.0)),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".to_string())),
        "a member with free quota must always win over one that costs money"
    );
}

// Pre-last-resort: paying to keep working outranks parking on a spent sink.
#[test]
fn next_target_spend_armed_member_outranks_last_resort_parking() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(profile_with_util("b", Some(95.0), Some(100.0))),
            spend_member("c", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".to_string()))
    );

    // Same chain, toggle off → the sink parks it, exactly like today.
    config.state.spend_budget_switching = false;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string()))
    );
}

// ...and outranks wrap-off's halt: an operator who set a ceiling asked to keep
// working, and Off stops every running claude.
#[test]
fn next_target_spend_armed_member_outranks_wrap_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string()))
    );

    // Toggle off → wrap-off halts, exactly like today.
    config.state.spend_budget_switching = false;
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

// ── finding (2026-07-17): a still-SERVING sink outranks spending real money ──
//
// The two `*_outranks_*` tests above use a 5h-MAXED sink (dead — it can't
// serve), which still ranks below spend. A sink only SOFT-blocked past the
// weekly line answers every request its live 5h window allows, for free, so it
// must win over paying. `weekly_soft_profile` is exactly that shape (7d 98.5%,
// 5h 40%): the soft-line headroom passes skip it, the serving-sink pass catches
// it before spend.

#[test]
fn next_target_serving_last_resort_sink_outranks_a_spend_armed_member() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(weekly_soft_profile("b")),
            spend_member("c", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string())),
        "a sink still serving for free must beat spending money"
    );
}

// The active-side facet: a serving-sink active is already parked free, so it
// stays put rather than hopping to a billing sibling.
#[test]
fn next_target_serving_last_resort_active_stays_put_instead_of_paying() {
    let mut config = config_with_chain(
        vec![
            mark_last_resort(weekly_soft_profile("a")),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        None,
        "a serving-sink active never pays a sibling — it already serves free"
    );
}

// Store twin, lockstep: the sink's soft-blocked-but-serving reading rides the
// `UsageStore`, `ChainMember::last_resort` comes from the marked profile.
#[test]
fn auto_switch_serving_last_resort_sink_outranks_a_spend_armed_member() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            mark_last_resort(profile_with_util("b", Some(95.0), None)),
            spend_member("c", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_both(
                Some(window(40.0, Some(live_reset()))),
                Some(window(98.5, Some(live_reset()))),
            ),
        ),
        (
            "c",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
        "the store twin must also park on a serving sink before paying"
    );
}

#[test]
fn auto_switch_serving_last_resort_active_stays_put_instead_of_paying() {
    let mut config = config_with_chain(
        vec![
            mark_last_resort(profile_with_util("a", Some(95.0), None)),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_both(
                Some(window(40.0, Some(live_reset()))),
                Some(window(98.5, Some(live_reset()))),
            ),
        ),
        (
            "b",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        None,
        "a serving-sink active stays parked free in the store twin too"
    );
}

// The store twin, in lockstep: same ordering, but the ceiling rides
// `ChainMember::max_spend` and the spend block rides the `UsageStore`.
#[test]
fn auto_switch_picks_a_spend_armed_member_when_the_chain_is_spent() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    assert_eq!(snap.chain[1].max_spend, 20.0, "ceiling must reach the walk");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string()))
    );

    // Toggle off → the same store is inert, exactly like today.
    config.state.spend_budget_switching = false;
    let snap_off = snapshot_chain(&config).expect("snapshot");
    assert_eq!(next_auto_switch_target(&snap_off, &store), None);
}

#[test]
fn auto_switch_subscription_headroom_beats_a_spend_armed_member() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            spend_member("b", 20.0, 0.0, Some(50.0)),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
        ("c", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".to_string())),
        "free quota must win over money on the store walk too"
    );
}

// The store twin's steady state, in lockstep with `next_target`: an over-budget
// active halts on `switch_off_when_budget_spent`, and `switch_off_when_spent` does not answer for it.
#[test]
fn auto_switch_over_budget_active_switches_off_by_default() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            spend_member("b", 5.0, 4.6, Some(50.0)),
        ],
        "b",
    );
    config.state.spend_budget_switching = true;
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_spent_with_spend(spend_block(true, 4.6, Some(50.0))),
        ),
    ]);
    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(
        snap.switch_off_when_budget_spent,
        "a spent budget halts by default"
    );
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
        "the scheduler must cap the spend exactly like the UI twin"
    );

    // Told to stay → keeps billing, and `switch_off_when_spent` must not override that.
    config.state.switch_off_when_budget_spent = false;
    config.state.switch_off_when_spent = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        None,
        "switch_off_when_spent must not halt an over-budget active that was told to stay"
    );
}

#[test]
fn auto_switch_zero_ceiling_never_spends_even_with_the_toggle_on() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            spend_member("b", 0.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.state.spend_budget_switching = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_spent_with_spend(spend_block(true, 0.0, Some(50.0))),
        ),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// Broken active with NO viable member: stays put. Wrap-off in particular must
// not fire off the broken flag alone — the live session's own Keychain chain
// may still be healthy, and only real usage exhaustion may halt it.
#[test]
fn auto_switch_broken_active_without_viable_member_never_wraps_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    config.set_auth_broken("a", true);
    config.set_auth_broken("b", true); // the only sibling is dead too
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(expired_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

// Broken AND genuinely exhausted (live window over threshold): the Off leg
// keys on the real exhaustion, which broken must not suppress either.
#[test]
fn auto_switch_broken_and_exhausted_active_still_wraps_off() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    config.set_auth_broken("a", true);
    config.set_auth_broken("b", true);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(100.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
    );
}

// The last-resort pass excludes a broken member too (not just the headroom
// pass): a `last_resort` sink whose token is dead must not be migrated to. The
// base case migrates a→b; quarantining b makes the last-resort walk find
// nothing.
#[test]
fn next_target_skips_broken_last_resort_member() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)), // active, exhausted, not last_resort
            mark_last_resort(profile_with_util("b", Some(80.0), Some(100.0))), // last_resort sink
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".into())),
        "base case: the last-resort pass migrates to the sink"
    );
    config.set_auth_broken("b", true);
    assert_eq!(
        next_target(&config, None),
        None,
        "a broken last_resort sink is excluded from the last-resort pass"
    );
}

// ── issue #8 follow-up b: burn-aware auto-switch (opt-in, default off) ─────
//
// `is_exhausted_projected`/`is_exhausted_active`/`is_exhausted_active_from_usage`
// only ever change the ACTIVE profile's own exhaustion decision; candidate
// selection stays on the static `is_exhausted`/`is_exhausted_from_usage`
// tested above (unchanged by this section).

#[test]
fn is_exhausted_projected_never_switches_later_than_static() {
    // Regression: burn-aware may only ever move the switch EARLIER than mode-off,
    // never later. 96% is over the 95% static threshold, so mode-off switches —
    // but with the default floor 98 above the threshold, the old predicate
    // required 96 ≥ 98 and held, running the window LONGER than static. The
    // static check now always fires here, whatever a slow burn projects.
    assert!(is_exhausted_projected(
        96.0,
        95.0,
        Some(4.0),
        90_000,
        98.0,
        60_000
    ));
}

#[test]
fn is_exhausted_projected_none_burn_falls_back_to_static_threshold() {
    assert!(
        is_exhausted_projected(96.0, 95.0, None, 90_000, 98.0, 60_000),
        "over threshold, no rate available → static check fires (floor/cap unused)"
    );
    assert!(
        !is_exhausted_projected(90.0, 95.0, None, 90_000, 98.0, 60_000),
        "under threshold, no rate available → static check doesn't fire"
    );
}

#[test]
fn is_exhausted_projected_heavy_burn_crosses_cap() {
    // Guards relaxed (floor 0, cap ≥ interval) and util below threshold so the
    // static check can't mask it — isolating the raw projection: a 1200 %/h burn
    // over a 90s poll projects 90 → 120, past the 100% cap.
    // (The floor's effect on the same burn is a separate test below.)
    assert!(is_exhausted_projected(
        90.0,
        95.0,
        Some(1200.0),
        90_000,
        0.0,
        90_000
    ));
}

#[test]
fn is_exhausted_projected_light_burn_stays_under_cap() {
    // Inside the sub-threshold early band (floor 90, threshold 98): 96% with a
    // light 4 %/h burn over a 90s poll barely moves, nowhere near the 100% cap,
    // so the projection holds. Kept below threshold so the static check can't
    // mask it — that isolates the hold to the burn being light.
    assert!(!is_exhausted_projected(
        96.0,
        98.0,
        Some(4.0),
        90_000,
        90.0,
        90_000
    ));
}

#[test]
fn is_exhausted_projected_floor_blocks_a_transient_burst() {
    // The Pro over-switch fix: a heavy burst (window-relative %/h reads high on a
    // small window) projects past 100, but the floor (90, below the 95 threshold)
    // refuses an early switch further from the cap than that — at 88% the window
    // is not spent yet, so it holds despite the burst.
    assert!(!is_exhausted_projected(
        88.0,
        95.0,
        Some(1200.0),
        90_000,
        90.0,
        90_000
    ));
    // Above the floor but still below threshold, the same burn switches early —
    // the floor is a lower bound on the switch point, not a veto on switching.
    assert!(is_exhausted_projected(
        93.0,
        95.0,
        Some(1200.0),
        90_000,
        90.0,
        90_000
    ));
}

#[test]
fn is_exhausted_projected_horizon_cap_tightens_the_margin() {
    // 98.5% + 80 %/h, inside the sub-threshold band (floor 98, threshold 99) so
    // the static check can't mask the difference. Over the full 90s interval the
    // projection reaches 100.5 and fires; capped to a 30s look-ahead it only
    // reaches ~99.2 and holds — so the cap reclaims the tail a long look-ahead
    // would switch away early.
    // Mutation-check: ignoring the cap (using the full interval) flips the
    // second assertion.
    assert!(is_exhausted_projected(
        98.5,
        99.0,
        Some(80.0),
        90_000,
        98.0,
        90_000
    ));
    assert!(!is_exhausted_projected(
        98.5,
        99.0,
        Some(80.0),
        90_000,
        98.0,
        30_000
    ));
}

#[test]
fn is_exhausted_active_mode_off_matches_static_is_exhausted() {
    // Mode off must reproduce `is_exhausted` bit for bit — no divergence from
    // today's static behavior regardless of what a rate would otherwise say.
    let exhausted = profile_with_util("a", Some(95.0), Some(100.0));
    let headroom = profile_with_util("a", Some(95.0), Some(50.0));
    assert_eq!(
        is_exhausted_active(
            &exhausted,
            false,
            90_000,
            None,
            weekly_blocked(&exhausted, 98.0),
            98.0,
            90_000
        ),
        is_exhausted(&exhausted, 98.0)
    );
    assert_eq!(
        is_exhausted_active(
            &headroom,
            false,
            90_000,
            None,
            weekly_blocked(&headroom, 98.0),
            98.0,
            90_000
        ),
        is_exhausted(&headroom, 98.0)
    );
    assert!(is_exhausted_active(
        &exhausted,
        false,
        90_000,
        None,
        weekly_blocked(&exhausted, 98.0),
        98.0,
        90_000
    ));
    assert!(!is_exhausted_active(
        &headroom,
        false,
        90_000,
        None,
        weekly_blocked(&headroom, 98.0),
        98.0,
        90_000
    ));
}

// Burn-aware ON but no rate available (fresh profile / first tick, or the
// caller's in-memory history is empty): falls back to the same static
// comparison mode-off uses — never leaves an account uncovered for lack of
// data. `is_exhausted_active` takes the rate as a parameter and never reads
// disk itself (that's the caller's job — `App::active_burn_rate` in-memory on
// the UI thread, `fallback::burn_rate_for_profile` on disk for the scheduler),
// so this needs no sandboxed HOME.
#[test]
fn is_exhausted_active_burn_aware_falls_back_without_rate() {
    let exhausted = profile_with_util("a", Some(95.0), Some(100.0));
    let headroom = profile_with_util("a", Some(95.0), Some(50.0));
    assert!(is_exhausted_active(
        &exhausted,
        true,
        90_000,
        None,
        weekly_blocked(&exhausted, 98.0),
        98.0,
        90_000
    ));
    assert!(!is_exhausted_active(
        &headroom,
        true,
        90_000,
        None,
        weekly_blocked(&headroom, 98.0),
        98.0,
        90_000
    ));
}

// Same fallback, exercised through the full `next_target` entry point (wrap-off
// path) rather than the `is_exhausted_active` unit — pins that mode-on with no
// rate available agrees with mode-off all the way through the public walk.
#[test]
fn next_target_burn_aware_none_rate_falls_back_to_static_threshold() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    config.state.burn_aware_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::Off),
        "no rate available → static 100% >= 95% threshold fires, same as mode off"
    );
}

/// Writes `entries` as `usage_history.jsonl` lines for `name` — the on-disk
/// shape `crate::profile::load_usage_history` parses. Takes the sandbox so the
/// write can only land in a sandboxed home, never the real one.
fn write_history(_home: &crate::testutil::HomeSandbox, name: &str, entries: &[(u64, UsageInfo)]) {
    let path = crate::profile::profile_history_path(name).expect("history path");
    std::fs::create_dir_all(path.parent().expect("parent dir")).expect("mkdir");
    let mut body = String::new();
    for (ts, usage) in entries {
        let line = serde_json::json!({ "ts": ts, "name": name, "usage": usage });
        body.push_str(&serde_json::to_string(&line).expect("serialize history line"));
        body.push('\n');
    }
    std::fs::write(&path, body).expect("write history");
}

// End-to-end proof that the UI-thread walk (`next_target`) and the
// scheduler-side walk (`next_auto_switch_target`) agree, and that burn-aware
// never holds the active where static would switch (the burn-floor regression):
// a heavy burn climbs the active to 96%, over the 95% threshold, so static signs
// everything out. Burn-aware with the default floor 98 once HELD here (96 < 98),
// running the window longer than static; the static check now always applies, so
// both walks switch off too.
//
// `b` is pinned exhausted (100%, no `last_resort`) so the headroom-walk and
// last-resort-walk (unaffected by burn-aware mode by design) both come up empty
// either way; the only thing that can move the outcome is the ACTIVE-only Off
// decision — the wrap-off Off-check inside `next_target`, and the entry gate
// inside `next_auto_switch_target`.
#[test]
fn burn_aware_never_holds_the_active_where_static_switches_on_both_walks() {
    let _home = crate::testutil::HomeSandbox::new();
    let now = crate::usage::now_ms();
    // Perfectly linear climb, 36 → 96 over 6 minutes = 600 %/h.
    write_history(
        &_home,
        "a",
        &[
            (
                now - 360_000,
                usage_info(Some(window(36.0, Some(live_reset())))),
            ),
            (
                now - 240_000,
                usage_info(Some(window(56.0, Some(live_reset())))),
            ),
            (
                now - 120_000,
                usage_info(Some(window(76.0, Some(live_reset())))),
            ),
        ],
    );

    // Static (mode off): 96% is over the 95% threshold → active exhausted, and
    // with `b` spent too, wrap-off signs everything out.
    let mut static_config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(96.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    static_config.state.switch_off_when_spent = true;
    assert_eq!(
        next_target(&static_config, None),
        Some(SwitchAction::Off),
        "static mode: 96% is over the 95% threshold → Off"
    );

    // `next_target` takes the burn rate as a parameter — it never reads disk
    // itself. Source it the same way `burn_rate_for_profile` does here,
    // standing in for the caller's in-memory `history_cache` lookup
    // (`App::active_burn_rate`) that feeds the real UI-thread call site.
    let active_window = window(96.0, Some(live_reset()));
    let rate = burn_rate_for_profile("a", &active_window).expect("rate computed from history");
    assert!((rate - 600.0).abs() < 1.0, "expected ~600 %/h, got {rate}");

    // Burn-aware: 96% is over the 95% threshold, so the static check inside the
    // projection fires and the active switches Off — same as mode-off. Before the
    // fix the default 98% floor held it here (96 < 98), running the window longer
    // than static.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(96.0)),
            profile_with_util("b", Some(95.0), Some(100.0)),
        ],
        "a",
    );
    config.state.switch_off_when_spent = true;
    config.state.burn_aware_switching = true;
    config.state.refresh_interval_ms = 90_000;
    assert_eq!(
        next_target(&config, Some(rate)),
        Some(SwitchAction::Off),
        "burn-aware agrees with static: 96% ≥ 95% threshold → Off"
    );

    let snap = snapshot_chain(&config).expect("snapshot");
    assert!(snap.switch_off_when_spent);
    assert!(snap.burn_aware);
    assert_eq!(snap.interval_ms, 90_000);
    assert_eq!(snap.burn_floor_pct, 98.0);
    let store = store_with_utils(&[("a", 96.0), ("b", 100.0)]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::Off),
        "scheduler-side walk agrees: burn-aware switches off at 96%, same as static"
    );
}

// ── weekly exhaustion (7d soft line, 98%) ─────────────────────────────────────
//
// A weekly-dead account's 5h window drains and then LAPSES (no live reset), so
// the 5h-only predicates read it as the freshest member in the chain — the
// observed 2026-07-08 bug: auto-fallback switched INTO a 7d=100 account, and
// recovery kept relinking it every time its 5h window rolled over. 2026-07-12
// lowered the line from the 100% hard cap to a 98% soft line: EITHER window
// crossing its line triggers the hop, while there is still room to land it.

/// UsageInfo with both windows populated explicitly.
fn usage_both(five_hour: Option<UsageWindow>, seven_day: Option<UsageWindow>) -> UsageInfo {
    UsageInfo {
        five_hour,
        seven_day,
        ..UsageInfo::default()
    }
}

/// A profile whose weekly window is spent to the cap and whose 5h window has
/// lapsed entirely — the live specimen's exact shape (5h resets_at: null).
fn weekly_dead_profile(name: &str) -> Profile {
    profile_with_usage(
        name,
        Some(95.0),
        Some(usage_both(None, Some(window(100.0, Some(live_reset()))))),
    )
}

#[test]
fn weekly_dead_member_is_never_a_fallback_target() {
    // a (active, 5h exhausted) → b (7d=100, 5h lapsed) → c (fresh): the walk
    // must land on c. Before the weekly gate, b's lapsed 5h read as headroom.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(97.0)),
            weekly_dead_profile("b"),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into()))
    );
}

#[test]
fn weekly_dead_active_is_exhausted_despite_idle_5h() {
    // The mirror direction: an ACTIVE account whose week is spent must count
    // exhausted — its 5h window never grows again (requests are refused), so
    // the 5h trigger alone would leave the daemon parked on a dead account.
    let p = weekly_dead_profile("a");
    assert!(is_exhausted(&p, 98.0));
    // Hard block trumps both burn-aware modes (nothing left to project).
    assert!(is_exhausted_active(
        &p,
        false,
        90_000,
        None,
        weekly_blocked(&p, 98.0),
        98.0,
        90_000
    ));
    assert!(is_exhausted_active(
        &p,
        true,
        90_000,
        Some(5.0),
        weekly_blocked(&p, 98.0),
        98.0,
        90_000
    ));
}

#[test]
fn weekly_soft_line_gates_below_it_and_lapsed_resets_renew() {
    // The weekly side is a soft line at 98%, not a 100% hard cap (2026-07-12):
    // an account riding 98%+ of its week bricks for DAYS the moment it tops
    // out, so the switch must happen while there is still room to land it —
    // waiting for the API to start refusing means dying mid-session.
    let below = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(None, Some(window(97.9, Some(live_reset()))))),
    );
    assert!(
        !is_exhausted(&below, 98.0),
        "97.9% weekly still has headroom"
    );
    let at_line = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(None, Some(window(98.0, Some(live_reset()))))),
    );
    assert!(
        is_exhausted(&at_line, 98.0),
        "98% weekly counts as exhausted"
    );
    // A 7d window whose reset has PASSED is a renewed quota, not a block.
    let renewed = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(None, Some(window(100.0, Some(expired_reset()))))),
    );
    assert!(!is_exhausted(&renewed, 98.0));
}

#[test]
fn weekly_soft_exhausted_active_triggers_a_switch_despite_5h_headroom() {
    // The user-reported gap (2026-07-12): active at 5h 40% / 7d 98.5% sat
    // unswitched until the weekly cap bricked it. EITHER window crossing its
    // line must trigger the hop.
    let active = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(
            Some(window(40.0, Some(live_reset()))),
            Some(window(98.5, Some(live_reset()))),
        )),
    );
    assert!(
        is_exhausted(&active, 98.0),
        "7d 98.5% triggers despite 5h 40%"
    );
    assert!(is_exhausted_active(
        &active,
        false,
        90_000,
        None,
        weekly_blocked(&active, 98.0),
        98.0,
        90_000
    ));
    let config = config_with_chain(
        vec![active, profile_with_util("b", Some(95.0), Some(10.0))],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".into()))
    );
}

#[test]
fn weekly_soft_member_is_not_a_target() {
    // Symmetric: hopping INTO a 98%+ weekly member just re-triggers next tick.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(97.0)),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_both(
                    Some(window(10.0, Some(live_reset()))),
                    Some(window(98.5, Some(live_reset()))),
                )),
            ),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into()))
    );
}

#[test]
fn weekly_dead_member_is_skipped_by_the_store_walk_too() {
    // Scheduler-side twin: next_auto_switch_target reads the UsageStore.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    let snapshot = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(97.0, Some(live_reset()))))),
        (
            "b",
            usage_both(None, Some(window(100.0, Some(live_reset())))),
        ),
        ("c", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    assert_eq!(
        next_auto_switch_target(&snapshot, &store),
        Some(SwitchAction::To("c".into()))
    );
}

#[test]
fn weekly_dead_member_never_recovers() {
    // Recovery's "5h lapsed = idle headroom" rule is exactly how a weekly-dead
    // member kept getting relinked every 5h rollover. It must stay out until
    // the WEEKLY reset passes.
    let members = vec![ChainMember {
        name: "b".into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
    }];
    let dead = store_with_infos(vec![(
        "b",
        usage_both(None, Some(window(100.0, Some(live_reset())))),
    )]);
    assert_eq!(find_recovered_member(&members, &dead, &[]), None);
    // Same member with the weekly reset in the past HAS recovered.
    let renewed = store_with_infos(vec![(
        "b",
        usage_both(None, Some(window(100.0, Some(expired_reset())))),
    )]);
    assert_eq!(
        find_recovered_member(&members, &renewed, &[]),
        Some("b".to_string())
    );
}

#[test]
fn soonest_resume_uses_the_weekly_reset_for_a_weekly_dead_member() {
    // The all-exhausted caption must not promise a 5h comeback for an account
    // that is blocked until its WEEKLY reset (nor bail to no caption at all
    // because the 5h window is absent).
    let weekly_reset = epoch_secs_to_iso(now_epoch_secs() + 48 * 3600);
    let five_hour_reset = epoch_secs_to_iso(now_epoch_secs() + 600);
    let config = config_with_chain(
        vec![
            profile_with_usage(
                "a",
                Some(95.0),
                Some(usage_both(Some(window(97.0, Some(five_hour_reset))), None)),
            ),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_both(None, Some(window(100.0, Some(weekly_reset))))),
            ),
        ],
        "a",
    );
    let (name, secs) = soonest_resume(&config).expect("caption data");
    // a's 10-minute 5h reset beats b's 48h weekly reset.
    assert_eq!(name, "a");
    assert!((500..700).contains(&secs), "got {secs}");
}

#[test]
fn weekly_line_is_configurable_chain_wide() {
    // Same roster, two configured lines: at 90 a member riding 7d 92% is
    // exhausted (excluded as a target, triggers as active); back at the 98
    // default it has headroom again. The accessor also rejects out-of-band
    // hand-edits rather than silently disabling the weekly gate.
    let mk = || {
        vec![
            profile_with_util("a", Some(95.0), Some(97.0)),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_both(
                    Some(window(10.0, Some(live_reset()))),
                    Some(window(92.0, Some(live_reset()))),
                )),
            ),
            profile_with_util("c", Some(95.0), Some(10.0)),
        ]
    };
    let mut config = config_with_chain(mk(), "a");
    config.state.weekly_switch_threshold = Some(90.0);
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".into())),
        "at a 90 line, b's 7d 92% is exhausted — walk lands on c"
    );
    let default_line = config_with_chain(mk(), "a");
    assert_eq!(
        next_target(&default_line, None),
        Some(SwitchAction::To("b".into())),
        "at the 98 default, b's 7d 92% is headroom"
    );
    let mut garbage = config_with_chain(mk(), "a");
    garbage.state.weekly_switch_threshold = Some(120.0);
    assert_eq!(garbage.state.weekly_switch_threshold_pct(), 98.0);
    assert_eq!(
        next_target(&garbage, None),
        Some(SwitchAction::To("b".into())),
        "an out-of-band hand-edit falls back to the default line"
    );
}

/// A soft-blocked (7d >= line, < 100) profile with fresh 5h headroom.
fn weekly_soft_profile(name: &str) -> Profile {
    profile_with_usage(
        name,
        Some(95.0),
        Some(usage_both(
            Some(window(40.0, Some(live_reset()))),
            Some(window(98.5, Some(live_reset()))),
        )),
    )
}

#[test]
fn wrap_off_keys_on_the_weekly_hard_cap_not_the_soft_line() {
    // Whole chain soft-blocked with fresh 5h headroom: switching is correctly
    // refused everywhere (the soft line's job), but wrap-off must NOT sign
    // everything out — the active still has real weekly room up to the API's
    // own cap, and Off would forfeit that tail for no gain.
    let mut config = config_with_chain(
        vec![weekly_soft_profile("a"), weekly_soft_profile("b")],
        "a",
    );
    config.state.switch_off_when_spent = true;
    assert_eq!(
        next_target(&config, None),
        None,
        "a soft-blocked active with weekly room left must stay put"
    );

    // Same chain with the ACTIVE at the hard cap: Off fires.
    let dead = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(
            Some(window(40.0, Some(live_reset()))),
            Some(window(100.0, Some(live_reset()))),
        )),
    );
    let mut config = config_with_chain(vec![dead, weekly_soft_profile("b")], "a");
    config.state.switch_off_when_spent = true;
    assert_eq!(next_target(&config, None), Some(SwitchAction::Off));
}

#[test]
fn wrap_off_keys_on_the_hard_cap_in_the_store_walk_too() {
    // Scheduler-side twin of the test above.
    let mk = || {
        config_with_chain(
            vec![
                profile_with_util("a", Some(95.0), None),
                profile_with_util("b", Some(95.0), None),
            ],
            "a",
        )
    };
    let soft = || {
        usage_both(
            Some(window(40.0, Some(live_reset()))),
            Some(window(98.5, Some(live_reset()))),
        )
    };
    let mut config = mk();
    config.state.switch_off_when_spent = true;
    let snapshot = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![("a", soft()), ("b", soft())]);
    assert_eq!(
        next_auto_switch_target(&snapshot, &store),
        None,
        "the soft-line trigger walks, finds nothing, and must NOT fall to Off"
    );

    let hard = usage_both(
        Some(window(40.0, Some(live_reset()))),
        Some(window(100.0, Some(live_reset()))),
    );
    let store = store_with_infos(vec![("a", hard), ("b", soft())]);
    assert_eq!(
        next_auto_switch_target(&snapshot, &store),
        Some(SwitchAction::Off),
        "the hard-capped active turns the chain off"
    );
}

#[test]
fn soonest_resume_keys_on_the_weekly_hard_cap_not_the_soft_line() {
    // A member past the SOFT line but under the API's cap still SERVES every
    // request its fresh 5h window allows. Keying the all-exhausted premise on
    // the soft line read the chain as dead and captioned a days-out weekly
    // reset over an account that works right now (2026-07-10 triage).
    let config = config_with_chain(vec![weekly_soft_profile("a")], "a");
    assert_eq!(
        soonest_resume(&config),
        None,
        "a soft-blocked member with fresh 5h headroom is not all-exhausted"
    );
}

#[test]
fn soonest_resume_all_exhausted_at_the_hard_cap_and_on_the_5h_line() {
    // The opposite direction, unmoved by the hard-cap fix: a weekly window AT
    // the cap is all-exhausted whatever its 5h window says (caption → the 7d
    // reset), and the 5h threshold leg still stands on its own.
    let capped = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_both(
            Some(window(40.0, Some(live_reset()))),
            Some(window(100.0, Some(reset_in(48 * 3600)))),
        )),
    );
    let (name, secs) =
        soonest_resume(&config_with_chain(vec![capped], "a")).expect("weekly cap is all-exhausted");
    assert_eq!(name, "a");
    assert!(
        (172_700..=172_800).contains(&secs),
        "the 7d reset drives the eta, got {secs}"
    );

    let five_hour = config_with_chain(vec![profile_with_util("b", Some(95.0), Some(97.0))], "b");
    let (name, secs) = soonest_resume(&five_hour).expect("5h past threshold is all-exhausted");
    assert_eq!(name, "b");
    assert!(
        (3500..=3600).contains(&secs),
        "the 5h reset drives the eta, got {secs}"
    );
}

#[test]
fn weekly_accessor_pins_the_band_and_resets_garbage_to_default() {
    let mut s = crate::profile::AppState {
        weekly_switch_threshold: Some(50.0),
        ..Default::default()
    };
    assert_eq!(s.weekly_switch_threshold_pct(), 50.0, "50.0 is in-band");
    s.weekly_switch_threshold = Some(100.0);
    assert_eq!(s.weekly_switch_threshold_pct(), 100.0, "100.0 is in-band");
    // Out-of-band RESETS to the default — never a clamp to the nearest bound.
    s.weekly_switch_threshold = Some(49.99);
    assert_eq!(s.weekly_switch_threshold_pct(), 98.0);
    s.weekly_switch_threshold = Some(f64::NAN);
    assert_eq!(s.weekly_switch_threshold_pct(), 98.0);
}

// ── blocked_reason: the Fallback tab's per-member chip (render-only) ──────────
//
// Dead-first precedence: auth broken › weekly hard (7d ≥ 100) › kick rejected ›
// budget spent › 5h over threshold › scoped spent (gated per-model week) ›
// weekly soft (soft ≤ 7d < 100) › stale. Each
// reason gets a positive test plus, where two can hold at once, a precedence test
// proving the worse one wins. Assertions match on the variant (not float `==`) so
// a wrong pct/countdown reds without tripping the float-cmp lint.

/// UsageInfo carrying only a live 7d window at `util`.
fn weekly_usage(util: f64) -> UsageInfo {
    UsageInfo {
        seven_day: Some(window(util, Some(live_reset()))),
        ..UsageInfo::default()
    }
}

/// UsageInfo with both a live 5h window at `five` and a live 7d at `seven`.
fn both_windows(five: f64, seven: f64) -> UsageInfo {
    UsageInfo {
        five_hour: Some(window(five, Some(live_reset()))),
        seven_day: Some(window(seven, Some(live_reset()))),
        ..UsageInfo::default()
    }
}

// `candidate_excluded` (the config-side skip every selection walk applies) and
// `blocked_reason`'s dead-first rungs (`Disabled`/`Canceled`/`AuthBroken`) are
// two hand-written ladders over one policy. A member the walk refuses as a
// candidate is exactly one the chip marks dead-first; a member blocked only by
// usage stays a walk candidate and carries a non-dead-first chip. This pins the
// biconditional for each of the three current dead-first terms, so dropping or
// re-gating one on either side reds here rather than drifting the chip off the
// behaviour it describes. A brand-new exclusion class needs its own fixture
// added below to be covered.
//
// Holds only for a NON-active member: `blocked_reason`'s `Disabled` rung and
// the walk's `== active` skip both special-case the active slot, so a healthy
// `keep` stays active and `cand` is judged as the candidate.
#[test]
fn candidate_exclusion_and_dead_first_chip_stay_coupled() {
    fn dead_first_chip(config: &AppConfig, name: &str) -> bool {
        let cand = config.find(name).expect("candidate is resolvable");
        matches!(
            blocked_reason(config, cand, None),
            Some(BlockedReason::Disabled | BlockedReason::Canceled | BlockedReason::AuthBroken)
        )
    }
    // The invariant, checked on every case: the walk skip and the chip's
    // dead-first verdict must agree for a resolvable non-active candidate.
    let assert_coupled = |config: &AppConfig, name: &str| {
        assert_eq!(
            candidate_excluded(config, name),
            dead_first_chip(config, name),
            "walk skip and dead-first chip disagree for {name}: {:?}",
            blocked_reason(config, config.find(name).expect("resolvable"), None)
        );
    };
    let keeper = || mark_fresh(profile_with_util("keep", Some(90.0), Some(10.0)));

    // disabled: the walk skips it, the chip is Disabled.
    let config = config_with_chain(
        vec![
            keeper(),
            mark_disabled(profile_with_util("cand", Some(90.0), Some(10.0))),
        ],
        "keep",
    );
    assert_coupled(&config, "cand");
    assert!(candidate_excluded(&config, "cand"));
    assert_eq!(
        blocked_reason(&config, config.find("cand").expect("cand"), None),
        Some(BlockedReason::Disabled)
    );

    // auth-broken: the walk skips it, the chip is AuthBroken.
    let mut config = config_with_chain(
        vec![keeper(), profile_with_util("cand", Some(90.0), Some(10.0))],
        "keep",
    );
    config.state.auth_broken.push("cand".into());
    assert_coupled(&config, "cand");
    assert_eq!(
        blocked_reason(&config, config.find("cand").expect("cand"), None),
        Some(BlockedReason::AuthBroken)
    );

    // canceled: the walk skips it, the chip is Canceled.
    let config = config_with_chain(
        vec![
            keeper(),
            profile_with_usage("cand", Some(90.0), Some(canceled_usage())),
        ],
        "keep",
    );
    assert_coupled(&config, "cand");
    assert_eq!(
        blocked_reason(&config, config.find("cand").expect("cand"), None),
        Some(BlockedReason::Canceled)
    );

    // live headroom: the walk keeps it, no chip at all.
    let config = config_with_chain(
        vec![
            keeper(),
            mark_fresh(profile_with_util("cand", Some(90.0), Some(10.0))),
        ],
        "keep",
    );
    assert_coupled(&config, "cand");
    assert!(!candidate_excluded(&config, "cand"));
    assert_eq!(
        blocked_reason(&config, config.find("cand").expect("cand"), None),
        None
    );

    // usage-only exhaustion: still a walk candidate (routed around by accept,
    // not skip), and the chip is present but NOT dead-first.
    let config = config_with_chain(
        vec![
            keeper(),
            mark_fresh(profile_with_util("cand", Some(90.0), Some(100.0))),
        ],
        "keep",
    );
    assert_coupled(&config, "cand");
    assert!(
        !candidate_excluded(&config, "cand"),
        "a member blocked only by usage stays a walk candidate"
    );
    assert!(
        matches!(
            blocked_reason(&config, config.find("cand").expect("cand"), None),
            Some(BlockedReason::FiveHour { .. })
        ),
        "usage exhaustion is a non-dead-first chip"
    );
}

#[test]
fn blocked_reason_none_for_a_live_member_with_headroom() {
    let p = mark_fresh(profile_with_util("a", Some(95.0), Some(40.0)));
    let cfg = config_with_chain(vec![p], "a");
    assert_eq!(blocked_reason(&cfg, &cfg.profiles[0], None), None);
}

#[test]
fn blocked_reason_auth_broken_outranks_every_other_block() {
    // 7d at the hard cap AND 5h over threshold — auth still wins.
    let p = profile_with_usage("a", Some(95.0), Some(both_windows(100.0, 100.0)));
    let mut cfg = config_with_chain(vec![p], "a");
    cfg.state.auth_broken.push("a".into());
    assert_eq!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::AuthBroken)
    );
}

#[test]
fn blocked_reason_weekly_hard_carries_the_7d_reset_countdown() {
    let p = profile_with_usage("a", Some(95.0), Some(weekly_usage(100.0)));
    let cfg = config_with_chain(vec![p], "a");
    assert!(
        matches!(
            blocked_reason(&cfg, &cfg.profiles[0], None),
            Some(BlockedReason::WeeklySpent { resets_in: Some(secs) })
                if (3500..=3600).contains(&secs)
        ),
        "got {:?}",
        blocked_reason(&cfg, &cfg.profiles[0], None)
    );
}

#[test]
fn blocked_reason_weekly_hard_outranks_a_5h_block() {
    // 7d at the hard cap AND 5h over threshold — weekly (dead for days) wins.
    let p = profile_with_usage("a", Some(95.0), Some(both_windows(99.0, 100.0)));
    let cfg = config_with_chain(vec![p], "a");
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::WeeklySpent { .. })
    ));
}

#[test]
fn blocked_reason_budget_spent_when_billing_and_over_ceiling() {
    // spend_member is 5h-spent + billing; ceiling 10, used 20 → over. Budget is
    // checked before the 5h block, so this also proves budget › 5h.
    let p = spend_member("a", 10.0, 20.0, None);
    let mut cfg = config_with_chain(vec![p], "a");
    cfg.state.spend_budget_switching = true;
    assert_eq!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::BudgetSpent)
    );
}

#[test]
fn blocked_reason_budget_spent_is_moot_with_free_5h_quota() {
    // Billing budget spent but 5h has headroom → the member serves for free and
    // the walk PREFERS it (headroom pass). No chip may claim a block here — the
    // invariant this whole fn promises. Reds against an ungated budget arm.
    let mut p = spend_member("a", 10.0, 20.0, None); // over ceiling → budget spent
    if let Some(u) = p.usage.as_mut() {
        u.five_hour = Some(window(30.0, Some(live_reset()))); // free 5h quota
    }
    let mut cfg = config_with_chain(vec![p], "a");
    cfg.state.spend_budget_switching = true;
    assert_eq!(blocked_reason(&cfg, &cfg.profiles[0], None), None);
}

#[test]
fn blocked_reason_five_hour_reports_utilization_and_reset() {
    let p = profile_with_util("a", Some(95.0), Some(97.0));
    let cfg = config_with_chain(vec![p], "a");
    assert!(
        matches!(
            blocked_reason(&cfg, &cfg.profiles[0], None),
            Some(BlockedReason::FiveHour { pct, resets_in: Some(secs) })
                if (pct - 97.0).abs() < f64::EPSILON && (3500..=3600).contains(&secs)
        ),
        "got {:?}",
        blocked_reason(&cfg, &cfg.profiles[0], None)
    );
}

#[test]
fn blocked_reason_weekly_soft_below_the_hard_cap_still_shows() {
    // 7d at 99 (≥ default soft 98, < 100) with 5h headroom → soft weekly block.
    let p = profile_with_usage("a", Some(95.0), Some(both_windows(40.0, 99.0)));
    let cfg = config_with_chain(vec![p], "a");
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::WeeklySoft { pct }) if (pct - 99.0).abs() < f64::EPSILON
    ));
}

#[test]
fn blocked_reason_five_hour_outranks_a_soft_weekly_block() {
    // Both hold: 5h fully stopped now beats the soft week (still serving).
    let p = profile_with_usage("a", Some(95.0), Some(both_windows(97.0, 99.0)));
    let cfg = config_with_chain(vec![p], "a");
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::FiveHour { .. })
    ));
}

#[test]
fn blocked_reason_scoped_spent_names_the_worst_gated_window() {
    // 5h + 7d headroom, but two per-model weeks over the line (gate on by
    // default): the chip names the worse one. Outranks the soft weekly band
    // (same still-serving band, but model-dead beats dispreferred).
    let p = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_with_scoped(
            10.0,
            99.0,
            vec![
                scoped_window("7d fable", 98.5, Some(week_reset())),
                scoped_window("7d opus", 100.0, Some(week_reset())),
            ],
        )),
    );
    let cfg = config_with_chain(vec![p], "a");
    assert!(
        matches!(
            blocked_reason(&cfg, &cfg.profiles[0], None),
            Some(BlockedReason::ScopedSpent { label, pct })
                if label == "7d opus" && (pct - 100.0).abs() < f64::EPSILON
        ),
        "got {:?}",
        blocked_reason(&cfg, &cfg.profiles[0], None)
    );
}

#[test]
fn blocked_reason_scoped_gate_off_never_claims_a_scoped_block() {
    // Same shape with the gate off: the walk keeps this member in rotation,
    // so the chip may not claim a scoped block — the soft week shows instead.
    let mut p = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_with_scoped(
            10.0,
            99.0,
            vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
        )),
    );
    p.check_scoped = false;
    let cfg = config_with_chain(vec![p], "a");
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::WeeklySoft { .. })
    ));
}

#[test]
fn blocked_reason_weekly_gate_off_drops_the_soft_chip() {
    // 7d at 99 with `check_weekly` off: the walk keeps rotating here, so no
    // soft-weekly chip either — chip and walk must not drift.
    let mut p = mark_fresh(profile_with_usage(
        "a",
        Some(95.0),
        Some(both_windows(40.0, 99.0)),
    ));
    p.check_weekly = false;
    let cfg = config_with_chain(vec![p], "a");
    assert_eq!(blocked_reason(&cfg, &cfg.profiles[0], None), None);
}

#[test]
fn blocked_reason_stale_when_the_last_read_was_cached() {
    let mut p = profile_with_util("a", Some(95.0), Some(40.0));
    p.fetch_status = Some(FetchStatus::Cached);
    let cfg = config_with_chain(vec![p], "a");
    assert_eq!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::Stale)
    );
}

#[test]
fn blocked_reason_a_real_block_outranks_stale_data() {
    // 5h over threshold AND cached — the block wins, stale is only the fallback.
    let mut p = profile_with_util("a", Some(95.0), Some(97.0));
    p.fetch_status = Some(FetchStatus::Cached);
    let cfg = config_with_chain(vec![p], "a");
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::FiveHour { .. })
    ));
}

#[test]
fn blocked_reason_failed_fetch_is_not_flagged_stale() {
    // `Failed` = no data at all (the card already says "no data"); only cached /
    // rate-limited numbers count as stale.
    let mut p = profile_with_util("a", Some(95.0), Some(40.0));
    p.fetch_status = Some(FetchStatus::Failed);
    let cfg = config_with_chain(vec![p], "a");
    assert_eq!(blocked_reason(&cfg, &cfg.profiles[0], None), None);
}

#[test]
fn blocked_reason_kick_rejected_when_switch_grade_with_headroom() {
    // Live member with 5h + 7d headroom — its ONLY block is the kick rejection
    // the walk routes around. `kick_lift` (Some(until)) carries the advertised
    // lift ceiling; the chip reports it as a countdown.
    let p = mark_fresh(profile_with_util("a", Some(95.0), Some(40.0)));
    let cfg = config_with_chain(vec![p], "a");
    let until = now_epoch_secs() + 3600;
    assert!(
        matches!(
            blocked_reason(&cfg, &cfg.profiles[0], Some(until)),
            Some(BlockedReason::KickRejected { lifts_in }) if (3500..=3600).contains(&lifts_in)
        ),
        "got {:?}",
        blocked_reason(&cfg, &cfg.profiles[0], Some(until))
    );
}

#[test]
fn blocked_reason_kick_rejected_outranks_a_5h_block() {
    // 5h over threshold AND kick-rejected: the limiter won't let clauth start the
    // member at all, so the kick block outranks the usage exhaustion. Reds if the
    // arm is placed below the 5h block.
    let p = profile_with_util("a", Some(95.0), Some(97.0));
    let cfg = config_with_chain(vec![p], "a");
    let until = now_epoch_secs() + 3600;
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], Some(until)),
        Some(BlockedReason::KickRejected { .. })
    ));
}

#[test]
fn blocked_reason_weekly_hard_outranks_a_kick_block() {
    // 7d at the hard cap (dead for days) beats a kick block that lifts in hours.
    let p = profile_with_usage("a", Some(95.0), Some(weekly_usage(100.0)));
    let cfg = config_with_chain(vec![p], "a");
    let until = now_epoch_secs() + 3600;
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], Some(until)),
        Some(BlockedReason::WeeklySpent { .. })
    ));
}

// ── weekly fallback §5: spend pass lock consolidation ─────────────────────────
//
// `next_auto_switch_target` takes ONE usage-store snapshot per evaluation and
// drives every pass off it, so a fetch landing mid-evaluation (between the
// headroom pass and the spend-armed pass) cannot flip a free-sibling decision
// into a paid one. The pre-snapshot shape locked the store per predicate, so
// each pass could observe a different state.
//
// The tests below pin the new contract:
//   * exactly one store lock per evaluation (the snapshot);
//   * the snapshot's content alone drives the decision (mutation after the
//     snapshot is invisible);
//   * a real `UsageStore` mutated between simulated passes flips the per-pass
//     read, but not the snapshot read.

/// Chain: A active + exhausted (the switch trigger), B no fresh + free 5h
/// headroom (pass-2 candidate; pass 1 skips it for lack of freshness), C
/// spend-armed (pass-4 candidate). With B viable the walk returns To(B) FREE;
/// with B exhausted it falls through to To(C) PAID. Three members, no
/// `last_resort`, no broken, no kick-rejected.
fn snapshot_for_lock_consolidation(spend_budget: bool) -> ChainSnapshot {
    ChainSnapshot {
        active: "a".into(),
        chain: vec![
            ChainMember {
                name: "a".into(),
                threshold: 95.0,
                last_resort: false,
                max_spend: 0.0,
                weekly_line: 98.0,
                scoped_line: 98.0,
                check_scoped: true,
            },
            ChainMember {
                name: "b".into(),
                threshold: 95.0,
                last_resort: false,
                max_spend: 0.0,
                weekly_line: 98.0,
                scoped_line: 98.0,
                check_scoped: true,
            },
            ChainMember {
                name: "c".into(),
                threshold: 95.0,
                last_resort: false,
                max_spend: 100.0,
                weekly_line: 98.0,
                scoped_line: 98.0,
                check_scoped: true,
            },
        ],
        switch_off_when_spent: true,
        broken: vec![],
        burn_aware: false,
        interval_ms: 60_000,
        burn_floor_pct: 80.0,
        burn_horizon_cap_ms: 3_600_000,
        spend_budget,
        switch_off_when_budget_spent: false,
        kick_rejected: vec![],
        fresh: vec![],
    }
}

/// Asserts [`next_auto_switch_target`] takes the `UsageStore` lock exactly once
/// per evaluation. The pre-snapshot shape took it per predicate (headroom walk
/// pass 1a + 1b, serving-sink active + sibling, spend-armed active + sibling,
/// budget-spent active, halt re-check) — six-plus lock windows an interleaved
/// `App::apply_usage` could mutate between. With the snapshot, the count is
/// exactly 1, which is the load-bearing guarantee: one lock window means no
/// mid-evaluation mutation can change the outcome.
///
/// This is the deterministic RED-against-old-shape test. Reverting the
/// snapshot refactor brings the count back above 1 and trips the assert.
#[test]
fn next_auto_switch_target_takes_store_lock_exactly_once() {
    super::NEXT_AUTO_SWITCH_TARGET_STORE_LOCKS.with(|c| c.set(0));
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0)]);
    let snapshot = snapshot_for_lock_consolidation(false);
    let _ = next_auto_switch_target(&snapshot, &store);
    let count = super::NEXT_AUTO_SWITCH_TARGET_STORE_LOCKS.with(|c| c.get());
    assert_eq!(
        count, 1,
        "snapshot mechanism must take the store lock exactly once per evaluation \
         (pre-snapshot shape locked per predicate — observed {count} > 1)"
    );
}

/// Real `UsageStore`, mutated between two simulated predicate reads, returns
/// different answers per read — reproducing the bug class the snapshot closes.
/// The snapshot read (a single clone at one instant) is invariant across the
/// same mutation. Uses the real predicates ([`is_exhausted_from_usage`]) and a
/// real `Arc<RankedMutex<HashMap<…>>>` store the test mutates by re-locking.
#[test]
fn snapshot_evaluation_isolates_predicate_reads_from_between_pass_mutation() {
    use super::is_exhausted_from_usage;

    // Pre-mutation store: B has live 5h headroom.
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
        (
            "c",
            usage_spent_with_spend(spend_block(true, 10.0, Some(100.0))),
        ),
    ]);

    let weekly_pct = 98.0;
    let member_b = ChainMember {
        name: "b".into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
    };

    // Per-predicate-lock simulation (the pre-snapshot shape): each call
    // re-locks the store, so a mutation between two calls IS observable.
    let read_pre_mutation = {
        let snap = store.lock().unwrap().clone();
        !is_exhausted_from_usage(&member_b, &snap, weekly_pct)
    };
    // A fetch lands between predicate calls — B's window crosses the line.
    {
        let mut s = store.lock().unwrap();
        s.insert(
            "b".into(),
            usage_info(Some(window(100.0, Some(live_reset())))),
        );
    }
    let read_post_mutation = {
        let snap = store.lock().unwrap().clone();
        !is_exhausted_from_usage(&member_b, &snap, weekly_pct)
    };
    assert!(
        read_pre_mutation && !read_post_mutation,
        "per-predicate reads must observe the mutation — pre={read_pre_mutation}, \
         post={read_post_mutation}. If both agree, the test never re-locked the store."
    );

    // Snapshot evaluation (the new shape): clone once, drive every read off
    // the same clone. The same mutation after the snapshot leaves the answer
    // unchanged.
    let store2 = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
        (
            "c",
            usage_spent_with_spend(spend_block(true, 10.0, Some(100.0))),
        ),
    ]);
    let snapshot = store2.lock().unwrap().clone();
    {
        let mut s = store2.lock().unwrap();
        s.insert(
            "b".into(),
            usage_info(Some(window(100.0, Some(live_reset())))),
        );
    }
    let snapshot_read = !is_exhausted_from_usage(&member_b, &snapshot, weekly_pct);
    assert!(
        snapshot_read,
        "snapshot evaluation must ignore the between-pass mutation — \
         the decision is locked to the snapshot's content"
    );
}

/// End-to-end: a single call to [`next_auto_switch_target`] returns To(B) FREE
/// when B has headroom at snapshot time, and To(C) PAID when B is exhausted at
/// snapshot time. The store's content at the snapshot instant drives the
/// decision — the load-bearing semantic the lock-count test above relies on.
#[test]
fn next_auto_switch_target_snapshot_content_drives_the_decision() {
    let snapshot = snapshot_for_lock_consolidation(true);

    // Snapshot state X: B has headroom → To(B) FREE.
    let store_with_b_headroom = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
        (
            "c",
            usage_spent_with_spend(spend_block(true, 10.0, Some(100.0))),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snapshot, &store_with_b_headroom),
        Some(SwitchAction::To("b".to_string())),
        "pass 2 must pick the free sibling when it has headroom at snapshot time"
    );

    // Snapshot state Y: B exhausted → To(C) PAID.
    let store_with_b_exhausted = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "c",
            usage_spent_with_spend(spend_block(true, 10.0, Some(100.0))),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snapshot, &store_with_b_exhausted),
        Some(SwitchAction::To("c".to_string())),
        "pass 4 must pick the spend-armed sibling when the free one is exhausted \
         at snapshot time — the bug direction: a fetch landing between the \
         headroom pass and the spend-armed pass could have caused this flip in \
         the pre-snapshot shape"
    );
}

/// End-to-end bug repro: with the snapshot, a store mutation AFTER the snapshot
/// instant cannot flip the result. The test takes the snapshot against
/// state X (B headroom), mutates the live store to state Y (B exhausted + C
/// spend-armed — the bug-direction flip), and asserts the snapshot-driven
/// evaluation still returns To(B) FREE. Pre-snapshot, the next predicate
/// re-lock would observe Y and return To(C) PAID.
///
/// Two evaluations run here:
///   * one with a snapshot taken AFTER the mutation — proves the same store
///     now yields To(C), so the assertion against the pre-mutation snapshot is
///     meaningful (not a tautology);
///   * one with the pre-mutation snapshot — the bug-direction check.
#[test]
fn next_auto_switch_target_ignores_store_mutation_after_snapshot() {
    // State X: B has headroom, C inert.
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(10.0, Some(live_reset()))))),
    ]);
    let snapshot = snapshot_for_lock_consolidation(true);

    // Pre-mutation snapshot (mimics what `next_auto_switch_target` clones as
    // its first and only store read).
    let pre_mutation: HashMap<String, UsageInfo> = store.lock().unwrap().clone();

    // The fetch lands: B crosses the line, C becomes spend-armed.
    {
        let mut s = store.lock().unwrap();
        s.insert(
            "b".into(),
            usage_info(Some(window(100.0, Some(live_reset())))),
        );
        s.insert(
            "c".into(),
            usage_spent_with_spend(spend_block(true, 10.0, Some(100.0))),
        );
    }

    // Snapshot evaluation against the PRE-mutation clone: To(B) FREE.
    let result_with_pre_snapshot =
        super::next_auto_switch_target_with_usage(&snapshot, &pre_mutation);
    assert_eq!(
        result_with_pre_snapshot,
        Some(SwitchAction::To("b".to_string())),
        "the snapshot is immutable — a post-snapshot mutation must not flip the \
         free-sibling decision into a paid one"
    );

    // Sanity: the live store HAS mutated, so a fresh snapshot returns To(C).
    // This proves the assertion above is not vacuous — the store genuinely
    // changed, the snapshot just refused to re-read it.
    assert_eq!(
        next_auto_switch_target(&snapshot, &store),
        Some(SwitchAction::To("c".to_string())),
        "a fresh snapshot taken AFTER the mutation must reflect it — To(C) PAID"
    );
}

// ---------------------------------------------------------------------------
// Per-model weekly windows ("7d fable") in the chain walk, gated by each
// member's `check_scoped` toggle (on by default).
// Live shape that motivated it (2026-07-18): a member at 7d 65% / 5h 0% but
// "7d fable" 100% — the aggregate-only walk called it the healthiest target
// and stranded every fable session landed on it.
// ---------------------------------------------------------------------------

/// A weekly reset days ahead — live for both 7d and per-model windows.
fn week_reset() -> String {
    epoch_secs_to_iso(now_epoch_secs() + 5 * 86_400)
}

fn scoped_window(
    label: &str,
    utilization: f64,
    resets_at: Option<String>,
) -> crate::usage::ScopedWindow {
    crate::usage::ScopedWindow {
        label: label.to_string(),
        window: window(utilization, resets_at),
    }
}

/// UsageInfo with a live 5h window, a live aggregate 7d window, and the given
/// per-model weekly windows.
fn usage_with_scoped(
    five_hour_pct: f64,
    seven_day_pct: f64,
    scoped: Vec<crate::usage::ScopedWindow>,
) -> UsageInfo {
    UsageInfo {
        five_hour: Some(window(five_hour_pct, Some(live_reset()))),
        seven_day: Some(window(seven_day_pct, Some(week_reset()))),
        weekly_scoped: scoped,
        ..UsageInfo::default()
    }
}

#[test]
fn auto_switch_prefers_member_clear_of_every_weekly_window() {
    // Active maxed. B is the ax-cl shape (agg clear, fable 100). C is clear
    // everywhere. Chain order favors B — the tier-1 pass must still pick C.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
            profile_with_util("c", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_with_scoped(
                0.0,
                65.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        (
            "c",
            usage_with_scoped(
                10.0,
                18.0,
                vec![scoped_window("7d fable", 12.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("c".to_string())),
        "fully-clear member beats an earlier model-blocked one"
    );
}

#[test]
fn auto_switch_never_lands_on_a_gate_on_model_blocked_member() {
    // Only sibling is model-blocked with its `check_scoped` gate on (the
    // default): it is out of rotation, so a spent active stays put rather
    // than stranding the next session of the capped model there.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_with_scoped(
                0.0,
                65.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn auto_switch_scoped_gate_off_keeps_a_model_blocked_member_in_rotation() {
    // Same shape, but the operator flipped b's `check_scoped` gate off ("I
    // run other models on it") — b rotates normally despite its fable week.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[1].check_scoped = false;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_with_scoped(
                0.0,
                65.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string()))
    );
}

#[test]
fn auto_switch_weekly_gate_off_ignores_the_soft_line_but_not_the_hard_cap() {
    // b rides the soft weekly band (99% > the default 98 line) with its
    // `check_weekly` gate off: still a rotation target. At the 100% hard cap
    // the gate no longer helps — the account cannot serve at all.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[1].check_weekly = false;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_with_scoped(0.0, 99.0, vec![])),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
        "gate off drops the soft weekly line"
    );

    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_with_scoped(0.0, 100.0, vec![])),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        None,
        "the 100% hard cap blocks regardless of the gate"
    );
}

#[test]
fn auto_switch_scoped_blocked_active_hops_to_fully_clear_member() {
    // Active is healthy by every aggregate gate but its fable window crossed
    // the line — with a fully-clear sibling, hop.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_with_scoped(
                20.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        (
            "b",
            usage_with_scoped(
                5.0,
                30.0,
                vec![scoped_window("7d fable", 10.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
    );
}

#[test]
fn auto_switch_scoped_blocked_active_stays_put_when_no_fully_clear_member() {
    // Every sibling is equally model-blocked: hopping buys nothing and would
    // ping-pong — stay put.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_with_scoped(
                20.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        (
            "b",
            usage_with_scoped(
                5.0,
                30.0,
                vec![scoped_window("7d fable", 99.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn scoped_active_trigger_stays_parked_on_a_pinned_sink() {
    // Active is a `last_resort` sink, healthy on every aggregate gate, fable
    // week spent, clear sibling waiting. A sink is parked ON PURPOSE ("serve
    // here for free until dead") — the scoped hop must not un-park it, or the
    // chain oscillates on the sibling's 5h period: hop off, sibling's 5h
    // spends, serving-sink pass returns, sibling resets, hop again.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[0].last_resort = true;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_with_scoped(
                20.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        ("b", usage_with_scoped(5.0, 30.0, vec![])),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        None,
        "a pinned sink must stay parked through a scoped block"
    );
}

// ── weekly gate off × non-empty weekly_scoped: the override and the line ─────
//
// `member_scoped_line` must ride the `weekly gate` (the `weekly at` row
// renders dimmed and refuses its editor while the gate is off, so a stored
// override must not keep judging) — but scoped judgment itself stays on at
// the CHAIN line (`check_scoped` is its gate). Both directions pinned, so
// neither "the override always applies" nor "scoped_line == weekly_line"
// (which goes to the hard cap on gate-off) survives the suite.

#[test]
fn scoped_line_ignores_the_override_while_the_weekly_gate_is_off() {
    // b: override 60, weekly gate OFF, fable at 70. The override is inert —
    // b's fable window sits under the chain line (98), so b is fully clear
    // and the walk leaves a spent active for it.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[1].weekly_threshold = Some(60.0);
    config.profiles[1].check_weekly = false;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_with_scoped(
                0.0,
                30.0,
                vec![scoped_window("7d fable", 70.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
        "a gated-off override must not judge the scoped windows"
    );
}

#[test]
fn scoped_windows_judge_the_chain_line_while_the_weekly_gate_is_off() {
    // b: weekly gate OFF, no override, fable at 99 — still judged at the
    // CHAIN line (98), so b stays out of rotation. If gate-off routed the
    // scoped line to the hard cap the way it does the aggregate one, 99
    // would pass and this would hop.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[1].check_weekly = false;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        (
            "b",
            usage_with_scoped(
                0.0,
                30.0,
                vec![scoped_window("7d fable", 99.0, Some(week_reset()))],
            ),
        ),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        None,
        "gate-off scoped judgment stays on the chain line, not the hard cap"
    );
}

#[test]
fn blocked_reason_scoped_override_is_inert_while_weekly_gate_is_off() {
    // Chip twin of the walk pair above: override 60 + gate off + fable at 70
    // → no chip (the walk keeps rotating here); fable at 99 → `ScopedSpent`
    // at the chain line (the walk skips it), even with the gate off.
    let mut under = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_with_scoped(
            10.0,
            30.0,
            vec![scoped_window("7d fable", 70.0, Some(week_reset()))],
        )),
    );
    under.weekly_threshold = Some(60.0);
    under.check_weekly = false;
    let cfg = config_with_chain(vec![under], "a");
    assert_eq!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        None,
        "a gated-off override must not put the chip on the card"
    );

    let mut over = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_with_scoped(
            10.0,
            30.0,
            vec![scoped_window("7d fable", 99.0, Some(week_reset()))],
        )),
    );
    over.check_weekly = false;
    let cfg = config_with_chain(vec![over], "a");
    assert!(
        matches!(
            blocked_reason(&cfg, &cfg.profiles[0], None),
            Some(BlockedReason::ScopedSpent { .. })
        ),
        "gate-off scoped judgment holds the chain line on the chip too"
    );
}

#[test]
fn blocked_reason_five_hour_outranks_scoped_spent() {
    // Both blocks live at once: the 5h window over its rotate threshold AND a
    // spent fable week. 5h ranks worse — it blocks every model right now,
    // while scoped still serves the others.
    let p = profile_with_usage(
        "a",
        Some(95.0),
        Some(usage_with_scoped(
            100.0,
            30.0,
            vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
        )),
    );
    let cfg = config_with_chain(vec![p], "a");
    assert!(matches!(
        blocked_reason(&cfg, &cfg.profiles[0], None),
        Some(BlockedReason::FiveHour { .. })
    ));
}

// ── fully_clear_target: the UI-thread twin of the scoped trigger's walk ──────

#[test]
fn fully_clear_target_skips_blocked_members_and_finds_the_clear_one() {
    // b: scoped-blocked (skipped), c: auth-broken (skipped), d: clear (the
    // pick). Exercised directly — this is the only walk `auto_switch_if_needed`
    // runs for the scoped trigger.
    let mut config = config_with_chain(
        vec![
            profile_with_usage("a", Some(95.0), Some(both_windows(20.0, 40.0))),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_with_scoped(
                    5.0,
                    30.0,
                    vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
                )),
            ),
            profile_with_usage("c", Some(95.0), Some(both_windows(5.0, 30.0))),
            profile_with_usage("d", Some(95.0), Some(both_windows(5.0, 30.0))),
        ],
        "a",
    );
    config.state.auth_broken.push("c".into());
    assert_eq!(
        fully_clear_target(&config, 98.0),
        Some("d".to_string()),
        "the walk must skip the scoped-blocked and broken members"
    );
}

#[test]
fn fully_clear_target_none_when_every_member_is_blocked() {
    let config = config_with_chain(
        vec![
            profile_with_usage("a", Some(95.0), Some(both_windows(20.0, 40.0))),
            profile_with_usage(
                "b",
                Some(95.0),
                Some(usage_with_scoped(
                    5.0,
                    30.0,
                    vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
                )),
            ),
            // c: 5h over its threshold — exhausted, not fully clear.
            profile_with_usage("c", Some(95.0), Some(both_windows(96.0, 30.0))),
        ],
        "a",
    );
    assert_eq!(fully_clear_target(&config, 98.0), None);
}

#[test]
fn fully_clear_target_skips_canceled_and_disabled_members() {
    // b: canceled (idle-looking usage, skipped), c: disabled (skipped),
    // d: clear (the pick). Its `skip` closure must mirror `next_target`'s —
    // a canceled or disabled member sorting first must not shadow a genuinely
    // clear one further down the chain.
    let config = config_with_chain(
        vec![
            profile_with_usage("a", Some(95.0), Some(both_windows(20.0, 40.0))),
            profile_with_usage("b", Some(95.0), Some(canceled_usage())),
            mark_disabled(profile_with_usage(
                "c",
                Some(95.0),
                Some(both_windows(5.0, 30.0)),
            )),
            profile_with_usage("d", Some(95.0), Some(both_windows(5.0, 30.0))),
        ],
        "a",
    );
    assert_eq!(
        fully_clear_target(&config, 98.0),
        Some("d".to_string()),
        "the walk must skip the canceled and disabled members"
    );
}

#[test]
fn scoped_gate_off_active_never_fires_the_scoped_hop() {
    // Active is model-blocked but its own `check_scoped` gate is off: no
    // scoped trigger, even with a clear sibling waiting.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[0].check_scoped = false;
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_with_scoped(
                20.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        ("b", usage_with_scoped(5.0, 30.0, vec![])),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn lapsed_scoped_window_never_blocks() {
    // A fable window at 100 whose reset has PASSED is stale — no hop.
    let config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_with_scoped(
                20.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(expired_reset()))],
            ),
        ),
        ("b", usage_with_scoped(5.0, 30.0, vec![])),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);
}

#[test]
fn next_target_prefers_fully_clear_member() {
    // UI-side twin of the tier-1 preference: B model-blocked, C clear.
    let mk = |name: &str, info: UsageInfo| profile_with_usage(name, Some(95.0), Some(info));
    let config = config_with_chain(
        vec![
            mk("a", usage_info(Some(window(100.0, Some(live_reset()))))),
            mk(
                "b",
                usage_with_scoped(
                    0.0,
                    65.0,
                    vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
                ),
            ),
            mk("c", usage_with_scoped(10.0, 18.0, vec![])),
        ],
        "a",
    );
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("c".to_string())),
    );
}

#[test]
fn find_recovered_prefers_member_clear_of_scoped_windows() {
    let chain = vec![
        ChainMember {
            name: "b".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
        ChainMember {
            name: "c".into(),
            threshold: 95.0,
            last_resort: false,
            max_spend: 0.0,
            weekly_line: 98.0,
            scoped_line: 98.0,
            check_scoped: true,
        },
    ];
    let store = store_with_infos(vec![
        (
            "b",
            usage_with_scoped(
                0.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        ("c", usage_with_scoped(5.0, 30.0, vec![])),
    ]);
    assert_eq!(
        find_recovered_member(&chain, &store, &[]),
        Some("c".to_string()),
        "recovery relinks the fully-clear member first"
    );
    // With ONLY the model-blocked member recovered, it is still taken.
    let store = store_with_infos(vec![(
        "b",
        usage_with_scoped(
            0.0,
            40.0,
            vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
        ),
    )]);
    let chain_b = vec![ChainMember {
        name: "b".into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
    }];
    assert_eq!(
        find_recovered_member(&chain_b, &store, &[]),
        Some("b".to_string()),
    );
}

#[test]
fn recovery_respects_each_members_gates() {
    // b is model-blocked but its scoped gate is off — it counts as clear in
    // the first recovery pass. c rides the soft weekly band with its weekly
    // gate off — clear too; chain order picks b.
    let member = |name: &str, check_weekly: bool, check_scoped: bool| ChainMember {
        name: name.into(),
        threshold: 95.0,
        last_resort: false,
        max_spend: 0.0,
        // Snapshot-folded shape: a gate-off weekly reads as the hard cap.
        weekly_line: if check_weekly { 98.0 } else { 100.0 },
        scoped_line: 98.0,
        check_scoped,
    };
    let chain = vec![member("b", true, false), member("c", false, true)];
    let store = store_with_infos(vec![
        (
            "b",
            usage_with_scoped(
                0.0,
                40.0,
                vec![scoped_window("7d fable", 100.0, Some(week_reset()))],
            ),
        ),
        ("c", usage_with_scoped(0.0, 99.0, vec![])),
    ]);
    assert_eq!(
        find_recovered_member(&chain, &store, &[]),
        Some("b".to_string()),
    );
    // c alone: its weekly-gate-off soft-band week recovers; at the hard cap
    // it never does.
    let chain_c = vec![member("c", false, true)];
    assert_eq!(
        find_recovered_member(&chain_c, &store, &[]),
        Some("c".to_string()),
    );
    let store = store_with_infos(vec![("c", usage_with_scoped(0.0, 100.0, vec![]))]);
    assert_eq!(find_recovered_member(&chain_c, &store, &[]), None);
}

#[test]
fn weekly_override_tightens_and_loosens_the_member_line() {
    // b overrides the chain-wide 98 line DOWN to 50: at weekly 60 it is out
    // of rotation even though the chain line would accept it.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[1].weekly_threshold = Some(50.0);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_with_scoped(0.0, 60.0, vec![])),
    ]);
    assert_eq!(next_auto_switch_target(&snap, &store), None);

    // Overridden UP to 100: b at weekly 99 keeps rotating where the chain
    // line (98) would have blocked it.
    config.profiles[1].weekly_threshold = Some(100.0);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_with_scoped(0.0, 99.0, vec![])),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string()))
    );
}

#[test]
fn weekly_override_governs_the_actives_scoped_windows_too() {
    // One per-account line for both judgments: active's fable week at 90 is
    // under the chain line (98) but over its own override (85) — the scoped
    // trigger fires and hops to the clear sibling.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[0].weekly_threshold = Some(85.0);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        (
            "a",
            usage_with_scoped(
                20.0,
                40.0,
                vec![scoped_window("7d fable", 90.0, Some(week_reset()))],
            ),
        ),
        ("b", usage_with_scoped(5.0, 30.0, vec![])),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string()))
    );
}

#[test]
fn weekly_override_never_softens_the_hard_sink_and_halt_judgments() {
    // A last-resort sink whose week is at 99 with an override of 50: the
    // serving-sink pass keys on the HARD cap, so the sink still parks —
    // a member's soft-line taste must not bench a literally-usable account.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), None),
            profile_with_util("b", Some(95.0), None),
        ],
        "a",
    );
    config.profiles[1].last_resort = true;
    config.profiles[1].weekly_threshold = Some(50.0);
    let snap = snapshot_chain(&config).expect("snapshot");
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_with_scoped(0.0, 99.0, vec![])),
    ]);
    assert_eq!(
        next_auto_switch_target(&snap, &store),
        Some(SwitchAction::To("b".to_string())),
        "the sink pass judges at the hard cap, not the member's soft line"
    );
}

#[test]
fn weekly_override_on_a_sink_never_makes_the_ui_twin_pay() {
    // The UI-thread twin of the case above: a `last_resort` sink carrying a
    // weekly override of 50 with its week at 98.5 (soft-blocked at its own
    // line, hard-clear, 5h idle) still serves every request for free. The
    // serving-sink pass must park there instead of paying the spend-armed
    // sibling — judging the sink at its member-resolved soft line would make
    // `next_target` spend real money while `next_auto_switch_target` parks.
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(100.0)),
            mark_last_resort(weekly_soft_profile("b")),
            spend_member("c", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.profiles[1].weekly_threshold = Some(50.0);
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        Some(SwitchAction::To("b".to_string())),
        "a hard-clear sink parks free regardless of its soft-line override"
    );
}

#[test]
fn weekly_override_on_a_sink_active_still_stays_put_over_paying() {
    // Active-side facet: the serving-sink ACTIVE carries the override. Its
    // own soft line says "exhausted", but it still answers for free — the
    // stay-put guard keys on the hard cap, so no switch (and no spend) fires.
    let mut config = config_with_chain(
        vec![
            mark_last_resort(weekly_soft_profile("a")),
            spend_member("b", 20.0, 0.0, Some(50.0)),
        ],
        "a",
    );
    config.profiles[0].weekly_threshold = Some(50.0);
    config.state.spend_budget_switching = true;
    assert_eq!(
        next_target(&config, None),
        None,
        "a serving-sink active stays parked free regardless of its override"
    );
}

// ── Fork-only tests (codex engine, RESCUE, CLA-FEED, forecast, email column) ──

// CDX-1 T1b tolerance layer: a stray codex member hand-edited into the
// persisted claude chain must never become a walk candidate. The edit
// surfaces reject NEW cross-harness members; this covers existing files —
// the walk would otherwise hand `switch_profile` a profile with no claude
// credentials.
#[test]
fn snapshot_chain_skips_stray_codex_members() {
    let mut config = config_with_chain(
        vec![
            profile_with_util("a", Some(95.0), Some(50.0)),
            profile_with_util("cdx", Some(95.0), Some(10.0)),
            profile_with_util("b", Some(95.0), Some(20.0)),
        ],
        "a",
    );
    config.find_mut("cdx").unwrap().harness = crate::profile::Harness::Codex;
    let snap = snapshot_chain(&config).expect("snapshot");
    let names: Vec<&str> = snap.chain.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        ["a", "b"],
        "the codex member must be invisible to the walk"
    );
}

// ---------------------------------------------------------------------------
// CDX-4: the codex chain walk (snapshot + next_codex_auto_switch_target)
// ---------------------------------------------------------------------------

fn codex_member(name: &str) -> ChainMember {
    ChainMember {
        name: name.to_string(),
        threshold: DEFAULT_THRESHOLD,
        last_resort: false,
        weekly_line: 98.0,
        scoped_line: 98.0,
        check_scoped: true,
        max_spend: 0.0,
    }
}

fn codex_snapshot(active: &str, members: &[&str]) -> CodexChainSnapshot {
    CodexChainSnapshot {
        active: active.to_string(),
        chain: members.iter().map(|n| codex_member(n)).collect(),
        broken: Vec::new(),
        weekly_pct: 98.0,
        leased: Vec::new(),
        loginless: Vec::new(),
    }
}

#[test]
fn codex_walk_fires_only_on_an_exhausted_active() {
    let snap = codex_snapshot("a", &["a", "b"]);
    // Healthy active → stay put.
    let store = store_with_utils(&[("a", 40.0), ("b", 10.0)]);
    assert_eq!(next_codex_auto_switch_target(&snap, &store), None);
    // Exhausted active + viable sibling → switch.
    let store = store_with_utils(&[("a", 100.0), ("b", 10.0)]);
    assert_eq!(
        next_codex_auto_switch_target(&snap, &store),
        Some("b".to_string())
    );
}

#[test]
fn codex_walk_treats_lapsed_windows_and_missing_data_as_viable() {
    let snap = codex_snapshot("a", &["a", "b", "c"]);
    // b: stale-high but LAPSED window (resets_at passed ⇒ reset ⇒ viable);
    // c: no store entry at all (never measured ⇒ viable). b wins by order.
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", usage_info(Some(window(100.0, Some(expired_reset()))))),
    ]);
    assert_eq!(
        next_codex_auto_switch_target(&snap, &store),
        Some("b".to_string())
    );
}

#[test]
fn codex_walk_skips_broken_leased_and_loginless_members() {
    let mut snap = codex_snapshot("a", &["a", "b", "c", "d", "e"]);
    snap.broken.push("b".to_string());
    snap.leased.push("c".to_string());
    snap.loginless.push("d".to_string());
    let store = store_with_utils(&[("a", 100.0)]);
    assert_eq!(
        next_codex_auto_switch_target(&snap, &store),
        Some("e".to_string()),
        "only the installable member is picked"
    );
}

#[test]
fn codex_limiter_verdict_drives_the_switch_and_clears_on_reset() {
    let snap = codex_snapshot("a", &["a", "b"]);
    // The limiter said "primary" and the 5h window hasn't reset: exhausted
    // even at a LOW percent (the verdict outranks the heuristic).
    let mut blocked = usage_info(Some(window(30.0, Some(live_reset()))));
    blocked.codex_rate_limit_reached = Some("primary".to_string());
    let store = store_with_infos(vec![("a", blocked), ("b", usage_info(None))]);
    assert_eq!(
        next_codex_auto_switch_target(&snap, &store),
        Some("b".to_string())
    );

    // Same verdict but the named window has lapsed → verdict expired.
    let mut lapsed = usage_info(Some(window(30.0, Some(expired_reset()))));
    lapsed.codex_rate_limit_reached = Some("primary".to_string());
    let store = store_with_infos(vec![("a", lapsed), ("b", usage_info(None))]);
    assert_eq!(next_codex_auto_switch_target(&snap, &store), None);
}

#[test]
fn codex_limiter_verdict_blocks_a_candidate_too() {
    let snap = codex_snapshot("a", &["a", "b", "c"]);
    let mut b_blocked = usage_info(Some(window(10.0, Some(live_reset()))));
    b_blocked.codex_rate_limit_reached = Some("primary".to_string());
    let store = store_with_infos(vec![
        ("a", usage_info(Some(window(100.0, Some(live_reset()))))),
        ("b", b_blocked),
    ]);
    assert_eq!(
        next_codex_auto_switch_target(&snap, &store),
        Some("c".to_string()),
        "a limiter-blocked sibling is walked around"
    );
}

#[test]
fn codex_walk_last_resort_pass_and_one_migration_rule() {
    // Everything exhausted; the last_resort member is still accepted…
    let mut snap = codex_snapshot("a", &["a", "b"]);
    snap.chain[1].last_resort = true;
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(
        next_codex_auto_switch_target(&snap, &store),
        Some("b".to_string())
    );
    // …but never FROM a last-resort active (no sink ping-pong).
    let mut snap = codex_snapshot("b", &["a", "b"]);
    snap.chain[0].last_resort = true;
    snap.chain[1].last_resort = true;
    let store = store_with_utils(&[("a", 100.0), ("b", 100.0)]);
    assert_eq!(next_codex_auto_switch_target(&snap, &store), None);
}

#[test]
fn codex_walk_degenerate_shapes_do_nothing() {
    // Chain of one: nowhere to go.
    let snap = codex_snapshot("a", &["a"]);
    let store = store_with_utils(&[("a", 100.0)]);
    assert_eq!(next_codex_auto_switch_target(&snap, &store), None);
    // Active outside the chain (post-snapshot mutation guard).
    let snap = codex_snapshot("zz", &["a", "b"]);
    assert_eq!(next_codex_auto_switch_target(&snap, &store), None);
}

#[test]
fn snapshot_codex_chain_rejects_degenerate_markers() {
    use crate::profile::{AppConfig, AppState, Harness};
    let mk = |names: &[(&str, Harness)], active: Option<&str>, chain: &[&str]| {
        let profiles = names
            .iter()
            .map(|(n, h)| {
                let mut p = crate::testutil::blank_profile(n);
                p.harness = *h;
                p
            })
            .collect();
        AppConfig {
            state: AppState {
                profiles: names.iter().map(|(n, _)| (*n).into()).collect(),
                active_codex_profile: active.map(Into::into),
                codex_fallback_chain: chain.iter().map(|n| (*n).into()).collect(),
                ..Default::default()
            },
            profiles,
        }
    };
    // Healthy shape snapshots.
    let ok = mk(
        &[("x", Harness::Codex), ("y", Harness::Codex)],
        Some("x"),
        &["x", "y"],
    );
    let snap = snapshot_codex_chain(&ok).expect("snapshot");
    assert_eq!(snap.active, "x");
    assert_eq!(snap.chain.len(), 2);

    // No active marker.
    assert!(snapshot_codex_chain(&mk(&[("x", Harness::Codex)], None, &["x"])).is_none());
    // Active names a CLAUDE profile (hand-edited state).
    assert!(snapshot_codex_chain(&mk(&[("x", Harness::Claude)], Some("x"), &["x"])).is_none());
    // Active names a deleted profile.
    assert!(snapshot_codex_chain(&mk(&[("y", Harness::Codex)], Some("gone"), &["gone"])).is_none());
    // Active not a chain member.
    assert!(
        snapshot_codex_chain(&mk(
            &[("x", Harness::Codex), ("y", Harness::Codex)],
            Some("x"),
            &["y"]
        ))
        .is_none()
    );
    // A stray claude member in the chain is skipped, never a candidate.
    let stray = mk(
        &[
            ("x", Harness::Codex),
            ("c", Harness::Claude),
            ("y", Harness::Codex),
        ],
        Some("x"),
        &["x", "c", "y"],
    );
    let snap = snapshot_codex_chain(&stray).expect("snapshot");
    assert_eq!(
        snap.chain
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        vec!["x", "y"]
    );
}

// ---------------------------------------------------------------------------
// SCW-1: per-model weekly windows ("7d fable") in the chain walk.
// Live shape that motivated it (2026-07-18): a member at 7d 65% / 5h 0% but
// "7d fable" 100% — the aggregate-only walk called it the healthiest target
// and stranded every fable session landed on it.
// ---------------------------------------------------------------------------
