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

/// Write an OAuth usage cache for `name` and backdate it by `age`. Both clocks
/// move together: a live fetch stamps the BODY, and the file's mtime follows it.
/// A test that needs them to disagree moves one afterwards.
fn seed_usage_cache(name: &str, usage: &UsageInfo, age: Duration) {
    // The cache write is gated on the on-disk record; seeding this cache is the
    // helper's whole job, and the mtime below panics over a skipped write.
    crate::testutil::register_names(&[name]);
    let mut usage = usage.clone();
    usage.fetched_at = Some(
        crate::usage::now_ms()
            .saturating_sub(u64::try_from(age.as_millis()).expect("fixture ages fit in u64")),
    );
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        USAGE_CACHE_FILE,
        &usage,
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
        ProfileWindows::Oauth { usage, age } => {
            assert_eq!(
                usage.and_then(|u| u.five_hour).map(|w| w.utilization),
                Some(12.0),
            );
            let secs = age.secs().expect("a stamped cache has an age");
            assert!((90..=200).contains(&secs), "age off its own body: {secs}s");
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

/// A third-party account's funded wallet carries its burn rate off the balance
/// series its own fetch leg records — the one figure every headroom surface
/// renders beside the balance.
#[test]
fn profile_windows_carries_the_funded_wallets_rate() {
    let _home = HomeSandbox::new();
    seed_provider_cache("vendor", Duration::from_secs(100));
    let name = crate::profile::ProfileName::from("vendor");
    let now = crate::usage::now_ms();
    // A dense hourly drain to the captured figure (the `total` wallet the
    // vendor cache carries): 85.45 → 31.45 CNY at 4.5 CNY/h ≈ 108/day,
    // seeded through the REAL writer so bridge pairs shape the series the
    // way a landing fetch does.
    for hours_ago in 1..=12u64 {
        let amount = 31.45 + 4.5 * hours_ago as f64;
        crate::profile::append_wallet_readings_at(
            &name,
            &wallet_rows_stats(amount),
            now - hours_ago * 3_600_000,
        );
    }

    match profile_windows(&vendor_profile("vendor")) {
        ProfileWindows::ThirdParty { wallet_rate, .. } => {
            let rate = wallet_rate.expect("a funded wallet with a live series carries a rate");
            assert_eq!(rate.currency, "CNY");
            assert_eq!(rate.label, "total");
            assert!((rate.amount - 31.45).abs() < 1e-9);
            assert!(
                (rate.per_day - 108.0).abs() < 8.0,
                "per_day={}",
                rate.per_day
            );
        }
        ProfileWindows::Oauth { .. } => {
            panic!("a third-party account has no 5h/7d window to report")
        }
    }
}

/// No balance series, no rate — the figure still renders; only its pace is
/// missing, which is the cold-start shape every surface already tolerates.
#[test]
fn profile_windows_carries_no_wallet_rate_without_a_series() {
    let _home = HomeSandbox::new();
    seed_provider_cache("vendor", Duration::from_secs(100));

    match profile_windows(&vendor_profile("vendor")) {
        ProfileWindows::ThirdParty {
            stats, wallet_rate, ..
        } => {
            assert!(stats.is_some(), "the cache is seeded");
            assert!(wallet_rate.is_none(), "no series, no rate");
        }
        ProfileWindows::Oauth { .. } => {
            panic!("a third-party account has no 5h/7d window to report")
        }
    }
}

fn wallet_rows_stats(amount: f64) -> crate::providers::ThirdPartyStats {
    crate::providers::ThirdPartyStats {
        is_available: true,
        rows: vec![crate::providers::StatRow {
            label: "total".to_string(),
            value: format!("{amount:.2} CNY"),
            kind: crate::providers::StatRowKind::Body,
        }],
        bars: vec![],
        plan: None,
        endpoint: None,
        best_effort: false,
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

    // The fresh direction at a DATED body: a live fetch younger than the
    // threshold reads not-stale on the same surface that publishes the marker,
    // so the flag cannot flip to "always stale once dated" and pass.
    seed_usage_cache("kerry", &five_hour_at(12.0), Duration::from_secs(60));
    let fresh = profile_windows(&blank_profile(&crate::profile::ProfileName::from("kerry")));
    assert!(!fresh.stale(), "a body fetched a minute ago is current");
    let age = fresh.age_secs().expect("a dated body publishes its age");
    assert!(
        (55..=65).contains(&age),
        "the fresh direction is exercised at a real dated age: {age}s"
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

/// `stale_after_ms` is the shared threshold derivation (#74): the status feed's
/// age arm and any other stale judgment derive their bound through it, so the
/// formula itself — floor at the degraded-fetch ceiling, scale with the
/// interval — is pinned exactly rather than through any one consumer.
#[test]
fn stale_after_ms_floors_at_the_degraded_ceiling_and_scales_with_interval() {
    let ceiling = crate::usage::DEGRADED_GAP_CEILING_MS;
    // Below the ceiling the floor binds: a tight cadence still gets the full
    // grace a degraded fetch can leave.
    assert_eq!(stale_after_ms(90_000), 2 * ceiling + 90_000);
    // At the ceiling the interval dominates; at the max interval the threshold
    // is 3h. Monotone: a slower cadence always means a wider grace. The max
    // interval is the config's own ceiling constant, not a restated number.
    assert_eq!(stale_after_ms(ceiling), 3 * ceiling);
    assert_eq!(
        stale_after_ms(crate::profile::MAX_REFRESH_INTERVAL_MS),
        2 * crate::profile::MAX_REFRESH_INTERVAL_MS + crate::profile::MAX_REFRESH_INTERVAL_MS
    );
    assert!(stale_after_ms(90_000) < stale_after_ms(ceiling));
    assert!(stale_after_ms(ceiling) < stale_after_ms(crate::profile::MAX_REFRESH_INTERVAL_MS));
}

/// An OAuth body clauth cannot date reads STALE with no age published. Both
/// undatable shapes take that arm: a stamp in the FUTURE, which proves the clock
/// moved rather than that the read is fresh, and a missing stamp, which is what
/// a plan-only cold fill and every pre-`fetched_at` cache carry. Publishing an
/// age would be maximum confidence in the one figure nothing can date.
#[test]
fn an_undatable_oauth_body_reads_stale_with_no_age() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("kerry");
    crate::testutil::register_names(&["kerry"]);

    for (case, fetched_at) in [
        ("future stamp", Some(crate::usage::now_ms() + 3_600_000)),
        ("no stamp at all", None),
    ] {
        let mut usage = five_hour_at(12.0);
        usage.fetched_at = fetched_at;
        write_profile_cache(&name, USAGE_CACHE_FILE, &usage);

        let windows = profile_windows(&blank_profile(&name));
        assert_eq!(
            windows.age_secs(),
            None,
            "{case}: clauth cannot date this figure, and says so by dating it not at all",
        );
        assert!(windows.stale(), "{case}: an undatable figure reads stale");
        match windows {
            ProfileWindows::Oauth { usage, .. } => assert_eq!(
                usage.and_then(|u| u.five_hour).map(|w| w.utilization),
                Some(12.0),
                "{case}: the figures stay visible",
            ),
            ProfileWindows::ThirdParty { .. } => panic!("{case}: an OAuth account"),
        }
    }
}

/// A staleness verdict qualifies a FIGURE. A body carrying only a plan has no
/// window to qualify, so it never reads stale however undatable it is: the
/// marker beside a dash would tell a reader that a number which does not exist
/// is old. Owner ruling 2026-09-09.
#[test]
fn a_body_with_no_window_is_never_stale() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("dead");
    crate::testutil::register_names(&["dead"]);

    let plan_only = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: None,
        }),
        ..Default::default()
    };
    for (case, fetched_at) in [
        ("undated", None),
        ("dated far past the threshold", Some(1_000_u64)),
    ] {
        let mut usage = plan_only.clone();
        usage.fetched_at = fetched_at;
        write_profile_cache(&name, USAGE_CACHE_FILE, &usage);
        assert!(
            !profile_windows(&blank_profile(&name)).stale(),
            "{case}: no figure, so nothing to discount",
        );
    }

    // Control: the same body carrying one window takes the verdict.
    let mut with_window = five_hour_at(12.0);
    with_window.fetched_at = None;
    write_profile_cache(&name, USAGE_CACHE_FILE, &with_window);
    assert!(
        profile_windows(&blank_profile(&name)).stale(),
        "one window is enough to qualify, and an undated one is stale",
    );
}

/// Every surface filters its rows through `window_row_is_live`, so a body whose
/// windows have ALL lapsed renders dashes exactly like one carrying none. The
/// verdict follows the figure a reader can see, not the field behind it.
#[test]
fn a_body_whose_windows_all_lapsed_is_never_stale() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("lapsed");
    crate::testutil::register_names(&["lapsed"]);

    let at = |offset: i64| UsageInfo {
        five_hour: Some(UsageWindow {
            utilization: 100.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(
                crate::usage::now_epoch_secs() + offset,
            )),
        }),
        // Undated, so only the window's liveness can decide the verdict.
        ..Default::default()
    };

    write_profile_cache(&name, USAGE_CACHE_FILE, &at(-3600));
    let windows = profile_windows(&blank_profile(&name));
    assert!(
        !windows.stale(),
        "the only row it could mark was dropped for having lapsed",
    );
    match &windows {
        ProfileWindows::Oauth { usage, .. } => assert!(
            crate::profile_json::usage_windows(usage.as_deref().expect("a cache")).is_empty(),
            "fixture control: the surfaces really do publish no row here",
        ),
        ProfileWindows::ThirdParty { .. } => panic!("an OAuth account"),
    }

    // Control: the same undated body with a LIVE window takes the verdict.
    write_profile_cache(&name, USAGE_CACHE_FILE, &at(3600));
    assert!(
        profile_windows(&blank_profile(&name)).stale(),
        "a live window is one a reader acts on, and an undated one is stale",
    );
}

/// The age rides in the BODY, so a cache rewrite that produced no new reading
/// cannot re-age any surface. A plan-only refresh rewrites the file (moving its
/// mtime to now) while leaving `fetched_at` on the fetch that last read the
/// account, which is why every surface dates off the body.
#[test]
fn a_plan_only_rewrite_does_not_refresh_the_oauth_age() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("kerry");
    seed_usage_cache("kerry", &five_hour_at(12.0), Duration::from_secs(9_000));

    // What a plan-only rewrite does: the file is written again, now-stamped,
    // and the body's own fetch stamp is untouched.
    set_mtime(
        &profile_cache_path(&name, USAGE_CACHE_FILE).unwrap(),
        SystemTime::now(),
    );

    let windows = profile_windows(&blank_profile(&name));
    let age = windows.age_secs().expect("the body still dates itself");
    assert!(
        (8_900..=9_100).contains(&age),
        "the age follows the fetch, not the file: {age}s"
    );
    assert!(windows.stale(), "9000s is past the MCP staleness threshold");
}

/// The stale verdict flips on the EXACT threshold instant, not up to a second
/// late: `Dated` carries millis (R10) because the pre-R10 shape divided to
/// seconds in the age and multiplied back in the verdict, so a body at
/// `threshold + 999ms` still read fresh.
#[test]
fn the_stale_flip_is_exact_not_second_granular() {
    let threshold = crate::usage::now_ms();
    let is_stale_at = |offset_ms: u64| {
        let usage = crate::usage::UsageInfo {
            fetched_at: Some(threshold.saturating_sub(offset_ms)),
            ..five_hour_at(12.0)
        };
        oauth_age(Some(&usage), threshold).is_stale(690_000, true)
    };
    assert!(
        !is_stale_at(690_000 - 1),
        "1ms under the threshold reads fresh"
    );
    assert!(
        !is_stale_at(690_000),
        "the threshold instant itself still reads fresh: the verdict is strict"
    );
    assert!(
        is_stale_at(690_000 + 1),
        "1ms past the threshold flips (pre-R10: only +1000ms did)"
    );
    assert!(
        is_stale_at(690_000 + 999),
        "threshold + 999ms flips: the pre-R10 shape read it fresh"
    );
}

/// The bound the surfaces actually compare against: `STALE_AFTER_MS` is the
/// fixed threshold the MCP payloads read, and the strict `>` means the instant
/// a healthy ceiling-cadence fetch lands exactly on it still reads fresh. The
/// comparison is pinned against the constant itself so a threshold edit that
/// drops the strictness reds here, not only in the flip test's hand-picked
/// number (the threshold's margin is pinned by the value-arm test above).
#[test]
fn the_stale_threshold_boundary_is_strict_at_the_constant() {
    let threshold_ms = STALE_AFTER_MS;
    // One captured instant for both the stamp and the verdict — the flip
    // test's shape — so preemption between two `now_ms()` calls cannot drift
    // the boundary assertion.
    let now = crate::usage::now_ms();
    let at = |offset_ms: i64| {
        let usage = crate::usage::UsageInfo {
            fetched_at: Some((now as i64 - offset_ms).max(0) as u64),
            ..five_hour_at(12.0)
        };
        oauth_age(Some(&usage), now).is_stale(threshold_ms, true)
    };
    assert!(
        !at(threshold_ms as i64),
        "exactly at the threshold the verdict is strict: fresh"
    );
    assert!(
        at(threshold_ms as i64 + 1),
        "one millisecond past the constant flips"
    );
    assert!(
        !at(threshold_ms as i64 - 1),
        "one millisecond under the constant reads fresh"
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

/// `tier_label` reads the same OAuth cache `published_windows` does, so it
/// inherits the same leftover: a CONVERTED profile — a `base_url` + key saved on
/// disk, the `edit_profile_endpoint` shape — publishes no tier even with a
/// plan-bearing `usage_cache.json` from its earlier OAuth life still on disk:
/// publishing that leftover rendered `Max 5x` beside the account's new
/// `generic` label. The OAuth half is the guard's other direction: an account
/// whose figures do live in the OAuth cache keeps its tier.
#[test]
fn tier_label_is_none_for_a_converted_profile() {
    let _home = HomeSandbox::new();
    let converted = Profile::new(
        "litellm".to_string(),
        Some("http://127.0.0.1:4000".to_string()),
        Some("k".to_string()),
    );
    crate::profile::save_profile(&converted).expect("save the converted profile");
    let stale_plan = UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(5)),
            subscription_status: None,
        }),
        ..Default::default()
    };
    seed_usage_cache("litellm", &stale_plan, Duration::from_secs(100));

    assert_eq!(
        tier_label(&converted),
        None,
        "a converted account publishes no Anthropic tier"
    );

    seed_usage_cache("kerry", &stale_plan, Duration::from_secs(100));
    assert_eq!(
        tier_label(&blank_profile(&crate::profile::ProfileName::from("kerry"))),
        Some("Max 5x".to_string()),
        "an OAuth account keeps its tier"
    );
}

/// The published `provider` field names exactly one of three cases: the
/// recognised provider's display name, `"anthropic"` for a profile with no
/// managed endpoint of its own, and `"generic"` for every other endpoint.
/// The generic arm is what a litellm/LM Studio/ollama fleet reads: the pre-fix
/// answer published `"anthropic"` beside the account's own `base_url`, the
/// OAuth label contradicting the endpoint the account really calls. The
/// fourth arm is the endpoint-without-a-pair shape (a blank profile plus a
/// `base_url`, no key, no credentials): its endpoint is generic, so the label
/// follows the endpoint. A pair+endpoint-no-key hybrid never reaches this
/// arm — `effective_base_url` drops a managed `base_url` behind a stored pair
/// with no usable key and no env token at load (an env-token hybrid keeps the
/// endpoint), so it reads as an OAuth account.
#[test]
fn provider_label_names_all_three_cases() {
    let _home = HomeSandbox::new();

    let oauth = blank_profile(&crate::profile::ProfileName::from("kerry"));
    assert_eq!(provider_label(&oauth), "anthropic");

    assert_eq!(provider_label(&vendor_profile("vendor")), "DeepSeek");

    let mut generic = blank_profile(&crate::profile::ProfileName::from("litellm"));
    generic.base_url = Some("http://127.0.0.1:4000".to_string());
    generic.api_key = Some("k".to_string());
    assert_eq!(provider_label(&generic), "generic");

    let mut keyless = blank_profile(&crate::profile::ProfileName::from("keyless"));
    keyless.base_url = Some("https://proxy.example/anthropic".to_string());
    assert_eq!(provider_label(&keyless), "generic");
}

/// The published `windows` array carries an account's OAuth windows only
/// while its figures live in the OAuth cache. A CONVERTED profile — a
/// `base_url` + key saved on disk, the `edit_profile_endpoint` shape —
/// publishes empty windows even with a maxed `usage_cache.json` from its
/// earlier OAuth life still on disk: publishing that leftover rendered a
/// stale 100% Anthropic window beside `"third_party":{"available":true}` for
/// an account with no Anthropic window.
#[test]
fn published_windows_is_empty_for_a_converted_profile() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&Profile::new(
        "litellm".to_string(),
        Some("http://127.0.0.1:4000".to_string()),
        Some("k".to_string()),
    ))
    .expect("save the converted profile");
    seed_usage_cache("litellm", &five_hour_at(100.0), Duration::from_secs(100));

    assert!(
        published_windows(&crate::profile::ProfileName::from("litellm")).is_empty(),
        "a converted account publishes no Anthropic window"
    );
}

/// The guard's other direction: an account whose figures do live in the OAuth
/// cache keeps publishing its real rows off that cache.
#[test]
fn published_windows_carries_an_oauth_accounts_windows() {
    let _home = HomeSandbox::new();
    seed_usage_cache("kerry", &five_hour_at(42.0), Duration::from_secs(100));

    let windows = published_windows(&crate::profile::ProfileName::from("kerry"));
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].label, "5h");
    assert_eq!(windows[0].utilization_pct, 42.0);
}

/// A window whose `resets_at` has passed renders unknown, never as its last
/// utilization (#74): a 19h-lapsed 5h window publishing `100%` is a spent
/// account reading as fully used, and `clauth list`'s `-` is the honest
/// answer. A live window beside it stays; a live-maxed one stays too — a
/// window pinned at the API's cap is live by definition, and the T1 stale
/// exemption reasons about it elsewhere rather than dropping it here.
#[test]
fn published_windows_drops_rows_whose_reset_has_passed() {
    let _home = HomeSandbox::new();
    let lapsed = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() - 3600);
    let live = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs() + 3600);
    seed_usage_cache(
        "kerry",
        &UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 100.0,
                resets_at: Some(lapsed.clone()),
            }),
            seven_day: Some(UsageWindow {
                utilization: 17.0,
                resets_at: Some(live.clone()),
            }),
            weekly_scoped: vec![crate::usage::ScopedWindow {
                label: "7d Opus".to_string(),
                window: UsageWindow {
                    utilization: 100.0,
                    resets_at: Some(live.clone()),
                },
            }],
            ..Default::default()
        },
        Duration::from_secs(100),
    );

    let windows = published_windows(&crate::profile::ProfileName::from("kerry"));
    assert_eq!(
        windows.len(),
        2,
        "the lapsed 5h row drops; the live 7d and the live-maxed weekly row stay: {windows:?}"
    );
    assert_eq!(windows[0].label, "7d");
    assert_eq!(windows[0].utilization_pct, 17.0);
    assert_eq!(windows[0].resets_at, Some(live.clone()));
    assert_eq!(windows[1].label, "7d Opus");
    assert_eq!(
        windows[1].utilization_pct, 100.0,
        "a window pinned at the cap with a future reset is live, not lapsed"
    );
    assert_eq!(windows[1].resets_at, Some(live));

    // A row whose `resets_at` is present but UNPARSEABLE stays — absence of a
    // usable stamp is missing data, not a lapsed window — and its stamp rides
    // through verbatim, the same missing-data shape an unstamped row carries.
    seed_usage_cache(
        "kerry",
        &UsageInfo {
            seven_day: Some(UsageWindow {
                utilization: 33.0,
                resets_at: Some("not a timestamp".to_string()),
            }),
            ..Default::default()
        },
        Duration::from_secs(100),
    );
    let windows = published_windows(&crate::profile::ProfileName::from("kerry"));
    assert_eq!(
        windows.len(),
        1,
        "an unparseable stamp keeps its row: {windows:?}"
    );
    assert_eq!(
        windows[0].resets_at,
        Some("not a timestamp".to_string()),
        "the unparsed stamp rides through, never normalized"
    );

    // A lapsed stamp on the weekly row too proves the per-model window drops
    // by the same rule and not because the weekly leg was skipped.
    seed_usage_cache(
        "kerry",
        &UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 50.0,
                resets_at: Some(lapsed.clone()),
            }),
            seven_day: Some(UsageWindow {
                utilization: 100.0,
                resets_at: Some(lapsed.clone()),
            }),
            weekly_scoped: vec![crate::usage::ScopedWindow {
                label: "7d Opus".to_string(),
                window: UsageWindow {
                    utilization: 100.0,
                    resets_at: Some(lapsed),
                },
            }],
            ..Default::default()
        },
        Duration::from_secs(100),
    );
    let windows = published_windows(&crate::profile::ProfileName::from("kerry"));
    assert!(
        windows.is_empty(),
        "every lapsed row drops, 7d and weekly and 5h alike: {windows:?}"
    );
    // And the 7d leg drops ON ITS OWN, not only in the all-lapsed set: the
    // first fixture's live 7d proved the row exists on this body, so the drop
    // here is the 7d filter's own verdict.
}

/// The other direction of the converted-profile guard: an api-key account
/// whose provider publishes windows carries them in `windows[]`, read from its
/// own third-party cache through the same derivation the walk reads.
#[test]
fn published_windows_carries_a_third_party_accounts_provider_windows() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&Profile::new(
        "zai-keyed".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("k".to_string()),
    ))
    .expect("save the profile");
    crate::testutil::register_names(&["zai-keyed"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("zai-keyed"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::testutil::stats_with_bars(vec![
            crate::testutil::bar("5h", 62.0),
            crate::testutil::bar("7d", 31.0),
        ]),
    );

    let windows = published_windows(&crate::profile::ProfileName::from("zai-keyed"));
    assert_eq!(windows.len(), 2, "both provider windows publish");
    assert_eq!(windows[0].label, "5h");
    assert_eq!(windows[0].utilization_pct, 62.0);
    assert_eq!(windows[1].label, "7d");
    assert_eq!(windows[1].utilization_pct, 31.0);
}

/// A provider bar whose `resets_at` has passed derives a row that then DROPS:
/// the derived window passes through the same liveness filter the OAuth row
/// does, so a lapsed provider window publishes no stale figure while its
/// OAuth sibling renders dashes for the same shape.
#[test]
fn published_windows_drops_a_lapsed_provider_bar() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&Profile::new(
        "zai-old".to_string(),
        Some("https://api.z.ai/api/anthropic".to_string()),
        Some("k".to_string()),
    ))
    .expect("save the profile");
    crate::testutil::register_names(&["zai-old"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("zai-old"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::testutil::stats_with_bars(vec![
            crate::testutil::bar_reset_in("5h", 62.0, -3_600),
            crate::testutil::bar_reset_in("7d", 31.0, 86_400),
        ]),
    );

    let windows = published_windows(&crate::profile::ProfileName::from("zai-old"));
    assert_eq!(
        windows.len(),
        1,
        "only the live 7d row survives: {windows:?}"
    );
    assert_eq!(windows[0].label, "7d");
}
