#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::profile::Profile;
use crate::profile_cache::{profile_cache_path, write_profile_cache};
use crate::testutil::{HomeSandbox, THIRD_PARTY_CACHE_BYTES, blank_profile, set_mtime};
use crate::usage::{PlanInfo, PlanTier, UsageWindow};

use std::time::{Duration, SystemTime};

/// A third-party profile as `Profile::new` derives one, endpoint and all.
fn vendor_profile(name: &str) -> Profile {
    Profile::new(
        name.to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-fixture".to_string()),
    )
}

/// Write real captured provider-cache bytes for `name` and backdate them by
/// `age`. Bytes rather than a serialized struct: every consumer reaches this
/// file through the production reader, so the fixture must too.
fn seed_provider_cache(name: &str, age: Duration) {
    let path = profile_cache_path(
        &crate::profile::ProfileName::from(name),
        THIRD_PARTY_CACHE_FILE,
    )
    .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, THIRD_PARTY_CACHE_BYTES).unwrap();
    set_mtime(&path, SystemTime::now() - age);
}

/// Write an OAuth usage cache for `name` and backdate it by `age`.
fn seed_usage_cache(name: &str, usage: &UsageInfo, age: Duration) {
    // The cache write is gated on the on-disk record; seeding this cache is the
    // helper's whole job, and the mtime below panics over a skipped write.
    crate::testutil::register_names(&[name]);
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        USAGE_CACHE_FILE,
        usage,
    );
    let path =
        profile_cache_path(&crate::profile::ProfileName::from(name), USAGE_CACHE_FILE).unwrap();
    set_mtime(&path, SystemTime::now() - age);
}

fn five_hour_at(pct: f64) -> UsageInfo {
    UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: pct,
            resets_at: None,
        }),
        ..Default::default()
    }
}

/// The OAuth arm reads the account's own `/usage` cache, and dates the figures
/// off that same file.
#[test]
fn profile_windows_reads_an_oauth_accounts_own_cache() {
    let _home = HomeSandbox::new();
    seed_usage_cache("kerry", &five_hour_at(12.0), Duration::from_secs(100));

    match profile_windows(&blank_profile(&crate::profile::ProfileName::from("kerry"))) {
        ProfileWindows::Oauth { usage, age_secs } => {
            assert_eq!(
                usage.and_then(|u| u.five_hour).map(|w| w.utilization),
                Some(12.0),
            );
            let age = age_secs.expect("a cache on disk has an age");
            assert!((90..=200).contains(&age), "age off its own file: {age}s");
        }
        ProfileWindows::ThirdParty { .. } => panic!("an OAuth account has OAuth windows"),
    }
}

/// A third-party account's figures come from the cache ITS OWN leg writes, and
/// so does their age. The fixture holds BOTH caches at very different stamps —
/// a third-party profile really can carry a leftover `usage_cache.json` from an
/// earlier OAuth life — so reading either half off the wrong file is visible.
#[test]
fn profile_windows_reads_a_third_party_accounts_own_cache() {
    let _home = HomeSandbox::new();
    seed_provider_cache("vendor", Duration::from_secs(100));
    seed_usage_cache("vendor", &five_hour_at(99.0), Duration::from_secs(10_000));

    match profile_windows(&vendor_profile("vendor")) {
        ProfileWindows::ThirdParty {
            stats, age_secs, ..
        } => {
            let stats = stats.expect("the provider cache on disk parses");
            assert_eq!(
                stats
                    .rows
                    .iter()
                    .find(|r| r.label == "total")
                    .map(|r| r.value.as_str()),
                Some("31.45 CNY"),
                "the real captured bytes reach the consumer",
            );
            let age = age_secs.expect("a cache on disk has an age");
            assert!(
                (90..=200).contains(&age),
                "the provider cache dates the provider figures, not the stale OAuth one: {age}s",
            );
        }
        ProfileWindows::Oauth { .. } => {
            panic!("a third-party account has no 5h/7d window to report")
        }
    }
}

/// Before its first provider fetch there is still no 5h/7d window — that half
/// is structurally none — and no balance either, which is a genuine unknown.
#[test]
fn profile_windows_leaves_an_unfetched_third_party_account_without_stats() {
    let _home = HomeSandbox::new();

    match profile_windows(&vendor_profile("vendor")) {
        ProfileWindows::ThirdParty {
            stats, age_secs, ..
        } => {
            assert!(stats.is_none(), "nothing has been fetched yet");
            assert!(age_secs.is_none(), "no cache, so no age to report");
        }
        ProfileWindows::Oauth { .. } => {
            panic!("a third-party account has no 5h/7d window to report")
        }
    }
}

/// The staleness verdict, pinned at BOTH boundaries the arithmetic sets, because
/// only the tightening direction can fail: a figure at the longest gap a live
/// scheduler can legally leave must NOT read stale, and one past the threshold
/// must — while still carrying its number, since suppressing it reads as clauth
/// losing the account.
#[test]
fn a_figure_older_than_any_refresh_cadence_reads_stale() {
    let _home = HomeSandbox::new();

    // `partition_due` schedules at `last + interval + backoff`, so this age is
    // one a healthy account at the ceiling interval genuinely produces.
    seed_usage_cache(
        "kerry",
        &five_hour_at(12.0),
        Duration::from_millis(MAX_LIVE_REFRESH_GAP_MS),
    );
    assert!(
        !profile_windows(&blank_profile(&crate::profile::ProfileName::from("kerry"))).stale(),
        "an account still on the slowest legal cadence is not one nobody refreshes",
    );

    seed_usage_cache(
        "kerry",
        &five_hour_at(12.0),
        Duration::from_millis(STALE_AFTER_MS) + Duration::from_secs(60),
    );
    let windows = profile_windows(&blank_profile(&crate::profile::ProfileName::from("kerry")));
    assert!(
        windows.stale(),
        "past the threshold nothing is refreshing it"
    );
    assert!(
        windows.age_secs().is_some(),
        "a stale figure keeps its age: suppressing it reads as clauth losing the account",
    );
}

/// A cache stamp in the FUTURE is a clock that moved, not a fresh read. A
/// saturating subtraction renders it as `cached just now` with `stale` false —
/// maximum confidence for the one stamp that proves the age cannot be trusted.
#[test]
fn a_future_cache_stamp_carries_no_age_at_all() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["kerry"]);
    write_profile_cache(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
        &five_hour_at(12.0),
    );
    let path = profile_cache_path(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
    )
    .unwrap();
    set_mtime(&path, SystemTime::now() + Duration::from_secs(3600));

    let windows = profile_windows(&blank_profile(&crate::profile::ProfileName::from("kerry")));
    assert_eq!(
        windows.age_secs(),
        None,
        "clauth cannot date this figure, and says so by dating it not at all",
    );
    assert!(
        !windows.stale(),
        "an undatable figure is not a stale verdict"
    );
}

/// `tier_label` feeds the MCP `profiles` rows (roster and session scope), and
/// reads straight off `usage_cache.json` — never a live fetch. A canceled
/// subscription reports its TIER here like every other account: the org drops to
/// `claude_free` on cancellation, so `Free` already carries the fact, and the
/// canceled marker belongs on the status line (the `⊖` pill), not in a field
/// every other path fills with a tier.
#[test]
fn tier_label_reports_the_tier_of_a_canceled_account() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["kerry"]);
    let profile = blank_profile(&crate::profile::ProfileName::from("kerry"));
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
            codex_plan: None,
        }),
        ..Default::default()
    };
    write_profile_cache(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
        &usage,
    );

    assert_eq!(tier_label(&profile), Some("Free".to_string()));
}

/// Code invariant, not a claim about any observed account: whatever tier the
/// cache holds is what this reports, `subscription_status` notwithstanding. A
/// paid tier is the fixture that can tell the two apart — `Free` alone cannot
/// prove the status was not substituted, since the canceled arm returned a
/// different string but the free one returns the same tier either way.
#[test]
fn tier_label_never_substitutes_canceled_for_a_paid_tier() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["kerry"]);
    let profile = blank_profile(&crate::profile::ProfileName::from("kerry"));
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(20)),
            subscription_status: Some("canceled".to_string()),
            codex_plan: None,
        }),
        ..Default::default()
    };
    write_profile_cache(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
        &usage,
    );

    assert_eq!(tier_label(&profile), Some("Max 20x".to_string()));
}

/// Regression guard the other direction: an un-canceled cached plan still
/// reports its real tier, not a false "canceled".
#[test]
fn tier_label_reports_the_real_tier_when_not_canceled() {
    let _home = HomeSandbox::new();
    crate::testutil::register_names(&["kerry"]);
    let profile = blank_profile(&crate::profile::ProfileName::from("kerry"));
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(5)),
            subscription_status: None,
            codex_plan: None,
        }),
        ..Default::default()
    };
    write_profile_cache(
        &crate::profile::ProfileName::from("kerry"),
        USAGE_CACHE_FILE,
        &usage,
    );

    assert_eq!(tier_label(&profile), Some("Max 5x".to_string()));
}
