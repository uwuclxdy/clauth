use super::*;

// 2026-05-17T14:20:00 UTC == 1779027600 epoch seconds.
const BASE_UTC: i64 = 1_779_027_600;

// ── identity-anchor backfill (rides the hourly /profile tier fetch) ──────────
//
// A profile that predates login-time anchor seeding has no `account_id.json`;
// once its stored pair is fully dead, `oauth::try_adopt_live_rotation` cannot
// prove a diverged live login is the same account and the profile wedges in
// `auth_broken` (observed 2026-07-09). The backfill closes that hole while the
// stored token is still alive, at zero extra HTTP.

fn raw_profile(uuid: Option<&str>) -> RawProfile {
    let text = match uuid {
        Some(u) => format!(r#"{{"account":{{"uuid":"{u}"}}}}"#),
        None => r#"{"account":{}}"#.to_string(),
    };
    serde_json::from_str(&text).expect("fixture profile parses")
}

fn raw_profile_with_email(uuid: &str, email: &str) -> RawProfile {
    serde_json::from_str(&format!(
        r#"{{"account":{{"uuid":"{uuid}","email":"{email}"}}}}"#
    ))
    .expect("fixture profile parses")
}

// ── windows_maxed: the `refresh_spent_accounts` fetch-skip predicate ─────────

fn window(util: f64, resets_at: &str) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: Some(resets_at.to_string()),
    }
}

const FUTURE: &str = "2999-01-01T00:00:00+00:00";
const PAST: &str = "2020-01-01T00:00:00+00:00";

/// A live window at (or past) the 100% cap reads as maxed on either the 5h or
/// the 7d leg; a live 7d cap alone is enough.
#[test]
fn windows_maxed_true_for_a_live_capped_window() {
    let now = BASE_UTC;
    let five_capped = UsageInfo {
        five_hour: Some(window(100.0, FUTURE)),
        ..Default::default()
    };
    assert!(windows_maxed(&five_capped, now), "live 5h at 100% is maxed");

    let seven_capped = UsageInfo {
        seven_day: Some(window(100.0, FUTURE)),
        ..Default::default()
    };
    assert!(
        windows_maxed(&seven_capped, now),
        "live 7d at 100% is maxed"
    );
}

/// Not maxed when the window is below the cap, lapsed (reset already passed, so
/// quota is renewed), or absent — each is a case the fetch must keep polling.
#[test]
fn windows_maxed_false_when_below_cap_or_lapsed_or_absent() {
    let now = BASE_UTC;

    let below = UsageInfo {
        five_hour: Some(window(99.9, FUTURE)),
        ..Default::default()
    };
    assert!(
        !windows_maxed(&below, now),
        "99.9% is still moving, not maxed"
    );

    let lapsed = UsageInfo {
        five_hour: Some(window(100.0, PAST)),
        ..Default::default()
    };
    assert!(
        !windows_maxed(&lapsed, now),
        "a 100% window whose reset has passed has renewed quota",
    );

    assert!(
        !windows_maxed(&UsageInfo::default(), now),
        "a windowless snapshot is never maxed",
    );
}

/// `spent_resume_in_secs` names when a spent account resumes polling: the LATEST
/// reset among the maxed windows, so a maxed weekly (7d) dominates a maxed 5h —
/// the account stays blocked until every maxed window lapses. `None` when not
/// maxed (below-cap or lapsed windows never gate polling).
#[test]
fn spent_resume_in_secs_takes_the_latest_maxed_reset() {
    let now = BASE_UTC;
    // 5h resets sooner than the maxed weekly; the weekly gates, so it wins.
    let five_soon = "2026-05-17T15:20:00Z"; // now + 1h
    let seven_late = "2026-05-24T14:20:00Z"; // now + 7d
    let both = UsageInfo {
        five_hour: Some(window(100.0, five_soon)),
        seven_day: Some(window(100.0, seven_late)),
        ..Default::default()
    };
    assert_eq!(
        spent_resume_in_secs(&both, now),
        Some(7 * 24 * 3600),
        "the maxed weekly reset dominates the sooner maxed 5h reset",
    );

    // Only the 5h is maxed → its reset; a below-cap weekly never gates.
    let five_only = UsageInfo {
        five_hour: Some(window(100.0, five_soon)),
        seven_day: Some(window(40.0, seven_late)),
        ..Default::default()
    };
    assert_eq!(spent_resume_in_secs(&five_only, now), Some(3600));

    // Not maxed anywhere → no resume time (the account still polls normally).
    assert_eq!(spent_resume_in_secs(&UsageInfo::default(), now), None);
    let below = UsageInfo {
        five_hour: Some(window(99.9, FUTURE)),
        ..Default::default()
    };
    assert_eq!(spent_resume_in_secs(&below, now), None);
}

#[test]
fn identity_anchor_backfills_only_when_missing() {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache};
    let _home = crate::testutil::HomeSandbox::new();

    seed_identity_anchor("acme", &raw_profile(Some("uuid-live")));
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_ID_CACHE_FILE).as_deref(),
        Some("uuid-live"),
        "missing anchor is seeded from the parsed /profile response"
    );

    // An existing anchor is authoritative (login re-seeds it; the ride-along
    // must never churn it).
    seed_identity_anchor("acme", &raw_profile(Some("uuid-later")));
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_ID_CACHE_FILE).as_deref(),
        Some("uuid-live"),
        "a present anchor is never overwritten by the ride-along"
    );
}

/// The email half seeds only as a coherent PAIR with the uuid: fresh seeds
/// land both from one response; a disagreeing cached uuid blocks the email;
/// and a torn-drop survivor (email without uuid) is dropped and rebuilt whole.
#[test]
fn identity_email_seeds_stay_coherent_with_the_uuid() {
    use crate::profile_cache::{
        ACCOUNT_EMAIL_CACHE_FILE, ACCOUNT_ID_CACHE_FILE, load_profile_cache, write_profile_cache,
    };
    let _home = crate::testutil::HomeSandbox::new();

    // Fresh seed: both halves land together.
    seed_identity_anchor(
        "acme",
        &raw_profile_with_email("uuid-live", "live@example.com"),
    );
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_EMAIL_CACHE_FILE).as_deref(),
        Some("live@example.com"),
    );

    // A response for a DIFFERENT account (anchor deliberately kept — the CAP
    // refuse-and-heal state) must not swap the email under the old uuid.
    seed_identity_anchor(
        "acme",
        &raw_profile_with_email("uuid-other", "other@example.com"),
    );
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_EMAIL_CACHE_FILE).as_deref(),
        Some("live@example.com"),
        "a disagreeing uuid blocks the email ride-along",
    );

    // Torn-drop survivor: email present, uuid absent. The email predates the
    // anchor and may describe the previous account — the seed drops it and
    // rebuilds the pair whole from one response.
    let _ = std::fs::remove_file(
        crate::profile_cache::profile_cache_path("torn", ACCOUNT_ID_CACHE_FILE).unwrap(),
    );
    write_profile_cache(
        "torn",
        ACCOUNT_EMAIL_CACHE_FILE,
        &"stale@example.com".to_string(),
    );
    seed_identity_anchor(
        "torn",
        &raw_profile_with_email("uuid-new", "new@example.com"),
    );
    assert_eq!(
        load_profile_cache::<String>("torn", ACCOUNT_ID_CACHE_FILE).as_deref(),
        Some("uuid-new"),
    );
    assert_eq!(
        load_profile_cache::<String>("torn", ACCOUNT_EMAIL_CACHE_FILE).as_deref(),
        Some("new@example.com"),
        "a predating email never survives a fresh uuid seed",
    );
}

#[test]
fn identity_anchor_refuses_blank_or_absent_uuid() {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache};
    let _home = crate::testutil::HomeSandbox::new();

    seed_identity_anchor("acme", &raw_profile(None));
    seed_identity_anchor("acme", &raw_profile(Some("  ")));
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_ID_CACHE_FILE),
        None,
        "no anchor may be minted from an absent or blank uuid"
    );
}

// ── login /profile probe (one request, both values) ──────────────────────────
//
// `clauth login` used to hit /profile TWICE ~5s apart with the same fresh token:
// once for the tier, once for the uuid, each discarding what the other wanted.

/// A `/profile` body carrying any combination of the two values the login reads.
fn login_body(uuid: Option<&str>, max: bool) -> RawProfile {
    let account = match uuid {
        Some(u) => format!(r#"{{"uuid":"{u}","has_claude_max":{max}}}"#),
        None => format!(r#"{{"has_claude_max":{max}}}"#),
    };
    serde_json::from_str(&format!(r#"{{"account":{account}}}"#)).expect("fixture profile parses")
}

#[test]
fn one_login_body_yields_both_the_tier_and_the_uuid() {
    let probe = login_profile_from_raw(login_body(Some("uuid-live"), true));
    assert_eq!(
        probe.subscription_type.as_deref(),
        Some("max"),
        "the tier the login stamps onto the mint"
    );
    assert_eq!(
        probe.account_uuid.as_deref(),
        Some("uuid-live"),
        "the identity the login anchors — from the SAME body, not a second request"
    );
}

/// Every tier the login can stamp maps to the exact string Claude Code stores in
/// `subscriptionType`. The arms are a hand-written match, so a table over all of
/// them is what keeps a future edit from silently renaming one.
#[test]
fn every_tier_maps_to_the_string_claude_code_stores() {
    let body = |json: &str| -> Option<String> {
        let raw: RawProfile = serde_json::from_str(json).expect("fixture profile parses");
        login_profile_from_raw(raw).subscription_type
    };
    assert_eq!(
        body(r#"{"account":{"has_claude_max":true}}"#).as_deref(),
        Some("max")
    );
    assert_eq!(
        body(r#"{"account":{"has_claude_pro":true}}"#).as_deref(),
        Some("pro")
    );
    assert_eq!(
        body(r#"{"organization":{"organization_type":"claude_team"}}"#).as_deref(),
        Some("team")
    );
    assert_eq!(
        body(r#"{"organization":{"organization_type":"claude_enterprise"}}"#).as_deref(),
        Some("enterprise")
    );
    assert_eq!(
        body(r#"{"organization":{"organization_type":"claude_free"}}"#).as_deref(),
        Some("free")
    );
    assert_eq!(
        body(r#"{"account":{}}"#),
        None,
        "an unrecognized tier stamps nothing — the usage poll re-derives it"
    );
}

#[test]
fn a_login_body_without_a_uuid_still_yields_the_tier() {
    let probe = login_profile_from_raw(login_body(None, true));
    assert_eq!(probe.subscription_type.as_deref(), Some("max"));
    assert_eq!(
        probe.account_uuid, None,
        "no identity to anchor, but the tier still stands"
    );
}

#[test]
fn a_login_body_without_a_tier_still_yields_the_uuid() {
    let probe = login_profile_from_raw(login_body(Some("uuid-live"), false));
    assert_eq!(
        probe.subscription_type, None,
        "an unrecognized tier is None, exactly as the old probe reported it"
    );
    assert_eq!(
        probe.account_uuid.as_deref(),
        Some("uuid-live"),
        "one absent value must not suppress the other"
    );
}

#[test]
fn a_login_bodys_blank_uuid_reads_as_no_identity() {
    // Same contract as `fetch_account_uuid`: two blanks comparing equal must
    // never prove two tokens are the same account.
    assert_eq!(
        login_profile_from_raw(login_body(Some("   "), true)).account_uuid,
        None
    );
    assert_eq!(
        login_profile_from_raw(login_body(Some(""), true)).account_uuid,
        None
    );
}

#[test]
fn a_login_anchor_overwrites_the_previous_account() {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache};
    let _home = crate::testutil::HomeSandbox::new();

    seed_login_anchor("acme", Some("uuid-first"));
    // The reauth-onto-a-DIFFERENT-account case: `clauth login` is the
    // authoritative (re)seeder, so unlike the ride-along backfill it must
    // replace the anchor rather than keep proving the old identity.
    seed_login_anchor("acme", Some("uuid-second"));
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_ID_CACHE_FILE).as_deref(),
        Some("uuid-second"),
        "a login re-seeds the anchor unconditionally"
    );
}

#[test]
fn a_login_anchor_write_ignores_an_absent_or_blank_uuid() {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache};
    let _home = crate::testutil::HomeSandbox::new();

    // A failed probe (`None`) or shape drift must never mint an anchor…
    seed_login_anchor("acme", None);
    seed_login_anchor("acme", Some("  "));
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_ID_CACHE_FILE),
        None,
        "no anchor may be minted from an absent or blank uuid"
    );

    // …and must never wipe a good one either.
    seed_login_anchor("acme", Some("uuid-good"));
    seed_login_anchor("acme", None);
    assert_eq!(
        load_profile_cache::<String>("acme", ACCOUNT_ID_CACHE_FILE).as_deref(),
        Some("uuid-good"),
        "a probe failure leaves the existing anchor intact"
    );
}

// ── canceled subscription: parse + the /usage-429 decouple ───────────────────
//
// Ground truth captured 2026-07-20 (CC 2.1.215) via mitmproxy: a canceled Pro
// account returns /profile 200 with `organization_type:"claude_free"` +
// `subscription_status:"canceled"` while /usage 429s on every tick. The active
// control (filip3) stays `claude_pro`/`trialing`. These pin the exact shapes.

/// `plan_from_profile` reads BOTH the tier (a canceled Pro drops to Free) and the
/// precise canceled status off the real captured bodies, and keeps a canceled
/// subscription distinct from a never-subscribed free account.
#[test]
fn plan_from_profile_reads_tier_and_canceled_status() {
    // canceled account (filip/filip2 on the wire).
    let canceled: RawProfile = serde_json::from_str(
        r#"{"account":{"has_claude_pro":false,"has_claude_max":false},
            "organization":{"organization_type":"claude_free","subscription_status":"canceled",
                            "billing_type":"none","rate_limit_tier":"default_claude_ai"}}"#,
    )
    .expect("canceled fixture parses");
    let plan = plan_from_profile(&canceled);
    assert_eq!(
        plan.tier,
        PlanTier::Free,
        "a canceled Pro reads as the Free tier"
    );
    assert!(
        plan.is_canceled(),
        "subscription_status canceled is observed"
    );

    // active control (filip3): claude_pro / trialing.
    let active: RawProfile = serde_json::from_str(
        r#"{"account":{"has_claude_pro":true,"has_claude_max":false},
            "organization":{"organization_type":"claude_pro","subscription_status":"trialing",
                            "billing_type":"stripe_subscription","rate_limit_tier":"default_claude_ai"}}"#,
    )
    .expect("active fixture parses");
    let plan = plan_from_profile(&active);
    assert_eq!(plan.tier, PlanTier::Pro);
    assert!(
        !plan.is_canceled(),
        "a trialing Pro account is not canceled"
    );

    // A genuine, never-subscribed free account (no status field) is Free but
    // NOT canceled — the two must stay distinguishable.
    let free: RawProfile =
        serde_json::from_str(r#"{"organization":{"organization_type":"claude_free"}}"#)
            .expect("free fixture parses");
    let plan = plan_from_profile(&free);
    assert_eq!(plan.tier, PlanTier::Free);
    assert!(
        !plan.is_canceled(),
        "an absent status is never read as canceled"
    );
}

/// The load-bearing decouple: when `/usage` 429s, `assemble_usage` STILL runs the
/// `/profile` leg and the error carries the FRESHLY observed plan — never the
/// stale prior one. Without this a canceled account (whose `/usage` 429s forever)
/// stays frozen on its cached Pro tier. A skipped leg (TTL) carries no plan, so
/// the scheduler doesn't re-persist on every masked tick.
#[test]
fn a_usage_429_still_surfaces_a_freshly_fetched_plan() {
    let prev_pro = PlanInfo {
        tier: PlanTier::Pro,
        subscription_status: None,
    };
    let canceled = PlanInfo {
        tier: PlanTier::Free,
        subscription_status: Some("canceled".to_string()),
    };

    // /profile observed canceled this tick → the 429 carries canceled, not Pro.
    let out = assemble_usage(
        Err(FetchError::RateLimited {
            retry_after: None,
            plan: None,
        }),
        Some(prev_pro.clone()),
        || Some(canceled.clone()),
    );
    match out {
        Err(FetchError::RateLimited { plan: Some(p), .. }) => {
            assert_eq!(p.tier, PlanTier::Free, "the 429 surfaces the Free tier");
            assert!(
                p.is_canceled(),
                "and the canceled state, not the stale prior Pro"
            );
        }
        _ => panic!("a 429 must carry the freshly fetched canceled plan"),
    }

    // /profile skipped this tick (hourly TTL) → the 429 carries NO plan; the
    // stale prior plan is never smuggled onto the error.
    let out = assemble_usage(
        Err(FetchError::RateLimited {
            retry_after: None,
            plan: None,
        }),
        Some(prev_pro.clone()),
        || None,
    );
    assert!(
        matches!(out, Err(FetchError::RateLimited { plan: None, .. })),
        "a skipped /profile leg carries no plan — the prior one is not smuggled in",
    );
}

#[test]
fn short_label_drops_claude_prefix_and_keeps_max_multiplier() {
    assert_eq!(
        PlanTier::Max(Some(5)).short_label().as_deref(),
        Some("Max 5x")
    );
    assert_eq!(PlanTier::Max(None).short_label().as_deref(), Some("Max"));
    assert_eq!(PlanTier::Pro.short_label().as_deref(), Some("Pro"));
    // an unknown tier carries no label so the MCP omits it entirely.
    assert_eq!(PlanTier::Unknown.short_label(), None);
}

/// A credential-less / no-claim profile must NOT fabricate a paid tier: `None`
/// (and any unrecognized value) map to `Unknown`, not `Pro`. A real claim still
/// resolves to its tier. Pins the "type Pro out of thin air" fix (issue #2's tell).
#[test]
fn from_subscription_type_defaults_to_unknown_not_pro() {
    assert_eq!(PlanTier::from_subscription_type(None), PlanTier::Unknown);
    assert_eq!(
        PlanTier::from_subscription_type(Some("nonsense")),
        PlanTier::Unknown
    );
    assert_eq!(PlanTier::from_subscription_type(Some("pro")), PlanTier::Pro);
    assert_eq!(
        PlanTier::from_subscription_type(Some("max")),
        PlanTier::Max(None)
    );
}

#[test]
fn parses_z_suffix() {
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00Z"), Some(BASE_UTC));
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00.121699Z"),
        Some(BASE_UTC)
    );
}

#[test]
fn parses_colon_offset() {
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+00:00"),
        Some(BASE_UTC)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00.121699+00:00"),
        Some(BASE_UTC)
    );
    // +05:30 is 5h30m ahead, so the UTC instant is earlier.
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+05:30"),
        Some(BASE_UTC - (5 * 3600 + 30 * 60))
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-05:30"),
        Some(BASE_UTC + (5 * 3600 + 30 * 60))
    );
}

#[test]
fn parses_colonless_offset() {
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+0000"),
        Some(BASE_UTC)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00.121699+0000"),
        Some(BASE_UTC)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+0530"),
        Some(BASE_UTC - (5 * 3600 + 30 * 60))
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-0530"),
        Some(BASE_UTC + (5 * 3600 + 30 * 60))
    );
}

#[test]
fn parses_bare_hour_offset() {
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+00"), Some(BASE_UTC));
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+05"),
        Some(BASE_UTC - 5 * 3600)
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-05"),
        Some(BASE_UTC + 5 * 3600)
    );
}

#[test]
fn colon_and_colonless_agree() {
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00+0530"),
        iso_to_epoch_secs("2026-05-17T14:20:00+05:30")
    );
    assert_eq!(
        iso_to_epoch_secs("2026-05-17T14:20:00-0800"),
        iso_to_epoch_secs("2026-05-17T14:20:00-08:00")
    );
}

#[test]
fn rejects_malformed() {
    // Too short to hold a date-time.
    assert_eq!(iso_to_epoch_secs("2026-05-17"), None);
    // Bad separators.
    assert_eq!(iso_to_epoch_secs("2026/05/17T14:20:00Z"), None);
    // Non-sign, non-Z trailing char.
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00X"), None);
    // Garbage in the offset.
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+ab:cd"), None);
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+5"), None);
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+12345"), None);
    assert_eq!(iso_to_epoch_secs("2026-05-17T14:20:00+05:3"), None);
}

/// `epoch_secs_to_iso` is the exact inverse of `iso_to_epoch_secs`: a round
/// trip through the formatter lands on the same epoch, and the output uses the
/// `+00:00` shape the parser accepts.
#[test]
fn epoch_to_iso_round_trips() {
    assert_eq!(epoch_secs_to_iso(BASE_UTC), "2026-05-17T14:20:00+00:00");
    for secs in [0, BASE_UTC, 951_867_122, 4_102_444_799] {
        assert_eq!(
            iso_to_epoch_secs(&epoch_secs_to_iso(secs)),
            Some(secs),
            "round trip failed for {secs}"
        );
    }
    // Negative input clamps to epoch 0 instead of underflowing the calendar.
    assert_eq!(epoch_secs_to_iso(-1), "1970-01-01T00:00:00+00:00");
}

/// `retry-after` parsing accepts the delta-seconds form and the IMF-fixdate
/// HTTP-date form; a future date becomes the delay until it, a past date is
/// `Duration::ZERO`, and garbage is no-hint (`None`).
#[test]
fn retry_after_parses_delta_seconds_and_http_date() {
    assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
    assert_eq!(parse_retry_after(" 30 "), Some(Duration::from_secs(30)));
    assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    assert_eq!(parse_retry_after(""), None);
    assert_eq!(parse_retry_after("-5"), None);
    assert_eq!(parse_retry_after("1.5"), None);

    // HTTP-date form, evaluated against a fixed instant for determinism.
    // "Wed, 21 Oct 2015 07:28:00 GMT" == 1_445_412_480 epoch seconds.
    let date = "Wed, 21 Oct 2015 07:28:00 GMT";
    assert_eq!(
        parse_retry_after_at(date, 1_445_412_480),
        Some(Duration::ZERO)
    );
    assert_eq!(
        parse_retry_after_at(date, 1_445_412_480 - 90),
        Some(Duration::from_secs(90)),
        "a future HTTP-date yields the delay until it"
    );
    assert_eq!(
        parse_retry_after_at(date, 1_445_412_480 + 120),
        Some(Duration::ZERO),
        "a past HTTP-date saturates to zero"
    );
    // Malformed HTTP-dates are no-hint.
    assert_eq!(
        parse_retry_after_at("Wed, 21 Foo 2015 07:28:00 GMT", 0),
        None
    );
    assert_eq!(parse_retry_after_at("21 Oct 2015 07:28:00 GMT", 0), None);
    assert_eq!(
        parse_retry_after_at("Wed, 21 Oct 2015 07:28:00 PST", 0),
        None
    );
}

/// `reserve_slot` spaces same-host requests by `REQUEST_SPACING_MS`: a cold slot
/// (now already past it) fires immediately, and a slot still ahead of now waits
/// until it — each reservation advancing that host's slot by exactly one spacing.
#[test]
fn reserve_slot_spaces_requests() {
    let now = 1_000_000u64;
    // Cold slot in the past → fire now, reserve one spacing out.
    let (next, wait) = reserve_slot(0, now);
    assert_eq!(wait, 0);
    assert_eq!(next, now + REQUEST_SPACING_MS);
    // Slot already ahead of now → wait until it, advance by one more spacing.
    let (next2, wait2) = reserve_slot(next, now);
    assert_eq!(wait2, REQUEST_SPACING_MS);
    assert_eq!(next2, next + REQUEST_SPACING_MS);
}

/// The ideal-pace marker tracks the fraction of the window already elapsed:
/// half-elapsed → 50%, just-opened → 0%, lapsed → clamped to 100%. Windows with
/// no reset time or no fixed duration return `None` (no marker).
#[test]
fn ideal_pace_tracks_elapsed_window_fraction() {
    let win = |reset_secs: i64| UsageWindow {
        utilization: 0.0,
        resets_at: Some(epoch_secs_to_iso(reset_secs)),
    };

    // 5h window with 2.5h left → half elapsed → 50%.
    let p = ideal_pace_pct(LABEL_5H, &win(BASE_UTC + 9_000), BASE_UTC).unwrap();
    assert!(
        (p - 50.0).abs() < 1e-6,
        "half-elapsed 5h window paces at 50%, got {p}"
    );

    // 7d window with its full duration left → just opened → 0%.
    let p = ideal_pace_pct(LABEL_7D, &win(BASE_UTC + 7 * 86_400), BASE_UTC).unwrap();
    assert!(
        p.abs() < 1e-6,
        "a freshly-opened window paces at 0%, got {p}"
    );

    // Reset already in the past → clamped to 100%.
    let p = ideal_pace_pct(LABEL_5H, &win(BASE_UTC - 1), BASE_UTC).unwrap();
    assert!(
        (p - 100.0).abs() < 1e-6,
        "a lapsed window paces at 100%, got {p}"
    );

    // No reset time, or a window with no fixed duration → no marker.
    let no_reset = UsageWindow {
        utilization: 0.0,
        resets_at: None,
    };
    assert_eq!(ideal_pace_pct(LABEL_5H, &no_reset, BASE_UTC), None);
    assert_eq!(
        ideal_pace_pct("extra", &win(BASE_UTC + 100), BASE_UTC),
        None
    );
}

/// Window-anchored average pace = utilization spread over the time elapsed since
/// the window opened, in %/day — rotation-proof because it reads only `resets_at`
/// and the current utilization (no history). Gated below `min_elapsed_secs`, and
/// `None` without a reset time or fixed duration.
#[test]
fn window_avg_pace_is_util_over_elapsed_days() {
    let win = |util: f64, reset_secs: i64| UsageWindow {
        utilization: util,
        resets_at: Some(epoch_secs_to_iso(reset_secs)),
    };
    let duration = 7 * 86_400;

    // 7d window 12h into the week at 21% → 21 / 0.5d = 42 %/d.
    let reset = BASE_UTC + duration - 12 * 3600;
    let pace = window_avg_pace_per_day(LABEL_7D, &win(21.0, reset), BASE_UTC, 3600).unwrap();
    assert!((pace - 42.0).abs() < 1e-6, "12h/21% → 42 %/d, got {pace}");

    // Freshly opened (30 min elapsed) is below the 1h floor → None, no divide-by-~0.
    let reset = BASE_UTC + duration - 1800;
    assert_eq!(
        window_avg_pace_per_day(LABEL_7D, &win(5.0, reset), BASE_UTC, 3600),
        None
    );

    // No reset time, or a label with no fixed window → None.
    let no_reset = UsageWindow {
        utilization: 21.0,
        resets_at: None,
    };
    assert_eq!(
        window_avg_pace_per_day(LABEL_7D, &no_reset, BASE_UTC, 3600),
        None
    );
    assert_eq!(
        window_avg_pace_per_day("extra", &win(21.0, BASE_UTC + 100), BASE_UTC, 3600),
        None
    );
}

/// Provider window labels shaped `<n>h`/`<n>d` resolve to a duration so api-key
/// bars get the same pace + ideal-line as OAuth windows; named OAuth labels keep
/// their fixed durations and non-window/malformed labels stay `None`.
#[test]
fn window_duration_parses_provider_labels() {
    assert_eq!(window_duration_secs(LABEL_5H), Some(5 * 3600));
    assert_eq!(window_duration_secs(LABEL_7D), Some(7 * 86_400));
    // Dynamic per-model labels resolve to the 7-day window.
    assert_eq!(window_duration_secs("7d fable"), Some(7 * 86_400));
    assert_eq!(window_duration_secs("7d opus"), Some(7 * 86_400));
    assert_eq!(window_duration_secs("5h"), Some(5 * 3600));
    assert_eq!(window_duration_secs("30d"), Some(30 * 86_400));
    assert_eq!(window_duration_secs("14d"), Some(14 * 86_400));
    assert_eq!(window_duration_secs("balance"), None);
    assert_eq!(window_duration_secs("d"), None);
    assert_eq!(window_duration_secs("0d"), None);
    assert_eq!(window_duration_secs(""), None);
}

/// `/profile` re-fetch policy: fetches on first load (no stamp yet) and on a
/// `force` (401 retry), reuses the plan within the hourly TTL, re-pulls once it
/// lapses, and `expire_profile_ttl` (manual single refresh) re-arms it. A
/// persistently failing endpoint is still capped at one attempt per hour.
#[test]
fn take_profile_fetch_honors_ttl_force_and_expiry() {
    // Sandboxed: the decision now stamps a per-profile file, which must never
    // land in the real `~/.clauth`.
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;

    // First load (no stamp) → fetch; then the same name within the hour → reuse,
    // even though the prior attempt may have failed to yield a plan.
    assert!(
        take_profile_fetch("ttl-first", false, t0),
        "first load pulls /profile"
    );
    assert!(
        !take_profile_fetch("ttl-first", false, t0 + 60_000),
        "within the TTL the cached plan is reused — no per-tick re-pull"
    );

    // `force` overrides a fresh TTL (separate name to avoid cross-talk).
    assert!(take_profile_fetch("ttl-force", false, t0));
    assert!(
        take_profile_fetch("ttl-force", true, t0 + 60_000),
        "force (401 retry) re-pulls /profile despite a fresh TTL"
    );

    // Past the TTL → re-pull.
    assert!(take_profile_fetch("ttl-stale", false, t0));
    assert!(
        take_profile_fetch("ttl-stale", false, t0 + PROFILE_TTL_MS + 1),
        "a plan past the TTL is re-pulled"
    );

    // Manual single refresh expires the clock → re-pull even within the hour.
    assert!(take_profile_fetch("ttl-expire", false, t0));
    assert!(!take_profile_fetch("ttl-expire", false, t0 + 60_000));
    expire_profile_ttl("ttl-expire");
    assert!(
        take_profile_fetch("ttl-expire", false, t0 + 120_000),
        "expiring the TTL forces a re-pull"
    );
}

// ── durable /profile TTL (survives a restart) ────────────────────────────────
//
// The in-memory clock is empty at every process start, so the first tick after a
// launch used to pull `/profile` for EVERY profile at once — against a 429-prone
// host, and with the plan already sitting in `usage_cache.json`. The stamp is
// persisted per profile so the hour the policy claims is the hour it enforces.

/// Give `name` an identity anchor, the gate that lets its durable stamp count.
fn anchor(name: &str) {
    write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &"uuid-anchored".to_string());
}

/// The persisted `/profile` attempt stamp, as the next process would read it.
fn durable_stamp(name: &str) -> Option<u64> {
    load_profile_cache::<u64>(name, PROFILE_FETCHED_CACHE_FILE)
}

#[test]
fn a_relaunch_inside_the_hour_reuses_the_durable_stamp() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-relaunch");

    assert!(
        take_profile_fetch("ttl-relaunch", false, t0),
        "first ever load has no stamp anywhere → pull /profile"
    );
    assert_eq!(
        durable_stamp("ttl-relaunch"),
        Some(t0),
        "the attempt is stamped to disk, not just to the map"
    );

    forget_profile_memo("ttl-relaunch");
    assert!(
        !take_profile_fetch("ttl-relaunch", false, t0 + 60_000),
        "a relaunch inside the hour must reuse the cached plan — the whole point"
    );
}

#[test]
fn a_durable_stamp_past_the_ttl_still_re_pulls() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-durable-stale");

    assert!(take_profile_fetch("ttl-durable-stale", false, t0));
    forget_profile_memo("ttl-durable-stale");
    assert!(
        take_profile_fetch("ttl-durable-stale", false, t0 + PROFILE_TTL_MS + 1),
        "the hour lapses across a restart like any other — the stamp is a clock, not a mute"
    );
}

#[test]
fn an_unanchored_profile_ignores_the_durable_stamp() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    // No `anchor()` — `seed_identity_anchor`'s backfill rides the /profile body,
    // and deferring it by an hour is exactly what wedges an unanchored profile in
    // `auth_broken` once its stored pair dies.
    assert!(take_profile_fetch("ttl-unanchored", false, t0));
    forget_profile_memo("ttl-unanchored");
    assert!(
        take_profile_fetch("ttl-unanchored", false, t0 + 60_000),
        "an anchor-less profile pays one /profile per launch until the backfill lands"
    );
}

#[test]
fn a_blank_anchor_counts_as_absent_for_the_durable_stamp() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    // Shape drift, not an identity — same contract as `seed_identity_anchor`.
    write_profile_cache(
        "ttl-blank-anchor",
        ACCOUNT_ID_CACHE_FILE,
        &"   ".to_string(),
    );

    assert!(take_profile_fetch("ttl-blank-anchor", false, t0));
    forget_profile_memo("ttl-blank-anchor");
    assert!(
        take_profile_fetch("ttl-blank-anchor", false, t0 + 60_000),
        "a blank uuid is no anchor, so its durable stamp must not be honored"
    );
}

#[test]
fn expire_profile_ttl_clears_the_durable_stamp_too() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-expire-durable");

    assert!(take_profile_fetch("ttl-expire-durable", false, t0));
    assert!(!take_profile_fetch(
        "ttl-expire-durable",
        false,
        t0 + 60_000
    ));

    expire_profile_ttl("ttl-expire-durable");
    assert_eq!(
        durable_stamp("ttl-expire-durable"),
        None,
        "the manual single refresh must drop the durable stamp, not only the memo"
    );
    // Dropping only the memo would fall straight back to the fresh disk stamp and
    // silently reduce the manual refresh to a no-op for /profile.
    assert!(
        take_profile_fetch("ttl-expire-durable", false, t0 + 120_000),
        "expiring the TTL forces a re-pull"
    );
}

#[test]
fn force_bypasses_a_fresh_durable_stamp_and_restamps_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-force-durable");

    assert!(take_profile_fetch("ttl-force-durable", false, t0));
    forget_profile_memo("ttl-force-durable");
    assert!(
        take_profile_fetch("ttl-force-durable", true, t0 + 60_000),
        "a 401 retry re-pulls /profile despite a fresh durable stamp"
    );
    assert_eq!(
        durable_stamp("ttl-force-durable"),
        Some(t0 + 60_000),
        "the forced attempt re-stamps the durable clock"
    );
}

#[test]
fn a_failing_endpoint_stays_capped_across_restarts() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-storm");

    // The decision is taken BEFORE the request, so an attempt that yields no plan
    // (a persistently 500ing /profile) is stamped exactly like a successful one.
    assert!(take_profile_fetch("ttl-storm", false, t0));
    assert_eq!(
        durable_stamp("ttl-storm"),
        Some(t0),
        "a failure can't un-stamp an attempt"
    );

    forget_profile_memo("ttl-storm");
    assert!(
        !take_profile_fetch("ttl-storm", false, t0 + 60_000),
        "a failing endpoint must not become a per-launch storm either"
    );
}

/// The TUI holds the `Config` guard across its account swaps (`let mut cfg =
/// app.config(); overwrite_captured_profile(&mut cfg, ..)`), so the clock is taken
/// at rank `Config`. Rank `ProfileTtl` INSIDE it or every debug-build TUI
/// rename/delete/reauth/logout panics on the lock-order assert. The action tests
/// pass a bare `&mut AppConfig` and never enter the ranked guard, so this is the
/// only place that sees it.
#[test]
fn the_ttl_clock_is_reachable_under_the_config_guard() {
    let _home = crate::testutil::HomeSandbox::new();
    let _config_rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::Config>();

    // Both halves of the swap path: a lock-order violation panics here, it does
    // not merely return the wrong answer.
    expire_profile_ttl("ttl-under-config");
    assert!(
        take_profile_fetch("ttl-under-config", false, 1_000_000_000_000),
        "the clock stays usable while the guard the TUI swaps hold is held"
    );
}

#[test]
fn a_stamp_in_the_future_is_not_freshness() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-rollback");

    // Stamped against a wall clock running an hour fast, which NTP then corrects
    // back. Saturating the age to 0 would read as perpetually fresh — and, now
    // that the stamp is durable, would mute /profile across every restart until
    // real time caught up.
    assert!(take_profile_fetch(
        "ttl-rollback",
        false,
        t0 + PROFILE_TTL_MS
    ));
    forget_profile_memo("ttl-rollback");

    assert!(
        take_profile_fetch("ttl-rollback", false, t0),
        "a stamp in the future is not trustworthy freshness — fail toward fetching"
    );
    assert_eq!(
        durable_stamp("ttl-rollback"),
        Some(t0),
        "the corrected clock re-stamps the bogus one instead of preserving it"
    );
}

#[test]
fn the_durable_stamp_is_read_at_most_once_per_process() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0 = 1_000_000_000_000u64;
    anchor("ttl-memo");

    assert!(take_profile_fetch("ttl-memo", false, t0));
    forget_profile_memo("ttl-memo");
    assert!(
        !take_profile_fetch("ttl-memo", false, t0 + 60_000),
        "the cold map falls back to the durable stamp"
    );

    // Both inputs deleted: a per-tick disk read would now see an unanchored
    // profile with no stamp and fire.
    remove_profile_cache("ttl-memo", PROFILE_FETCHED_CACHE_FILE);
    remove_profile_cache("ttl-memo", ACCOUNT_ID_CACHE_FILE);
    assert!(
        !take_profile_fetch("ttl-memo", false, t0 + 120_000),
        "the memoized stamp answers every later tick — one disk read per profile per process"
    );
}

/// Windows are derived from the normalized `limits[]` array, not the legacy
/// per-model top-level fields: `session` → 5h, `weekly_all` → 7d, and each
/// `weekly_scoped` entry becomes a dynamic `"7d <model>"` window — so a model
/// the server adds later (here Fable) is picked up with zero code change, and
/// `limits[]` wins over any stale top-level `five_hour`/`seven_day`. `spend`
/// parses its minor-unit money into dollars.
#[test]
fn limits_drive_windows_and_pick_up_new_models() {
    let json = r#"{
        "five_hour": {"utilization": 99, "resets_at": "2026-07-02T14:50:00+00:00"},
        "seven_day": {"utilization": 99, "resets_at": "2026-07-06T23:59:59+00:00"},
        "seven_day_opus": null,
        "limits": [
            {"kind": "session", "percent": 3, "resets_at": "2026-07-02T14:50:00+00:00"},
            {"kind": "weekly_all", "percent": 9, "resets_at": "2026-07-06T23:59:59+00:00"},
            {"kind": "weekly_scoped", "percent": 14, "resets_at": "2026-07-07T00:00:00+00:00",
             "scope": {"model": {"display_name": "Fable"}}}
        ],
        "spend": {"enabled": true, "percent": 32,
                  "used": {"amount_minor": 320, "currency": "USD", "exponent": 2},
                  "limit": {"amount_minor": 1000, "currency": "USD", "exponent": 2}}
    }"#;
    let raw: RawUsage = serde_json::from_str(json).unwrap();

    let w = windows_from_raw(&raw);
    // limits[] wins over the stale 99% top-level fields.
    assert_eq!(w.five_hour.map(|w| w.utilization), Some(3.0));
    assert_eq!(w.seven_day.map(|w| w.utilization), Some(9.0));
    // The new model is recognized purely from its scope name.
    assert_eq!(w.weekly_scoped.len(), 1);
    assert_eq!(w.weekly_scoped[0].label, "7d fable");
    assert_eq!(w.weekly_scoped[0].window.utilization, 14.0);
    assert_eq!(
        window_duration_secs(&w.weekly_scoped[0].label),
        Some(7 * 86_400)
    );

    let spend = SpendInfo::from_raw(raw.spend.as_ref().unwrap());
    assert!(spend.is_visible());
    assert_eq!(spend.used, Some(3.20));
    assert_eq!(spend.limit, Some(10.0));
    assert_eq!(spend.percent, Some(32.0));
    assert_eq!(spend.currency.as_deref(), Some("USD"));
}

/// A missing `session` / `weekly_all` limit falls back to the legacy top-level
/// window so an unmigrated account still renders 5h/7d; a disabled `spend` block
/// stays hidden.
#[test]
fn windows_fall_back_to_legacy_fields_when_limits_absent() {
    let json = r#"{
        "five_hour": {"utilization": 7, "resets_at": "2026-07-02T14:50:00+00:00"},
        "seven_day": {"utilization": 11, "resets_at": "2026-07-06T23:59:59+00:00"},
        "limits": [],
        "spend": {"enabled": false, "limit": null}
    }"#;
    let raw: RawUsage = serde_json::from_str(json).unwrap();

    let w = windows_from_raw(&raw);
    assert_eq!(w.five_hour.map(|w| w.utilization), Some(7.0));
    assert_eq!(w.seven_day.map(|w| w.utilization), Some(11.0));
    assert!(w.weekly_scoped.is_empty());
    assert!(!SpendInfo::from_raw(raw.spend.as_ref().unwrap()).is_visible());
}

/// `SpendInfo` money conversion + visibility edge cases (the bar is built blind
/// against the schema since no reachable account enables it): `exponent` scales
/// the minor units and defaults to cents when absent, a missing `limit` yields a
/// used-only bar, currency passes through, and a disabled block with no cap stays
/// hidden.
#[test]
fn spend_money_conversion_and_visibility() {
    let parse = |json: &str| SpendInfo::from_raw(&serde_json::from_str::<RawSpend>(json).unwrap());

    // exponent 3 → thousandths; non-USD currency preserved.
    let s = parse(
        r#"{"enabled": true, "percent": 50,
            "used": {"amount_minor": 1500, "currency": "EUR", "exponent": 3},
            "limit": {"amount_minor": 3000, "currency": "EUR", "exponent": 3}}"#,
    );
    assert_eq!(s.used, Some(1.5));
    assert_eq!(s.limit, Some(3.0));
    assert_eq!(s.currency.as_deref(), Some("EUR"));
    assert!(s.is_visible());

    // Missing exponent defaults to cents (2); missing limit → used-only, still
    // visible because the cap is enabled.
    let s = parse(r#"{"enabled": true, "used": {"amount_minor": 250, "currency": "USD"}}"#);
    assert_eq!(s.used, Some(2.5));
    assert_eq!(s.limit, None);
    assert!(s.is_visible());

    // A cap present but currently disabled is still worth showing.
    assert!(parse(r#"{"enabled": false, "limit": {"amount_minor": 1000}}"#).is_visible());

    // Fully disabled, no cap → hidden.
    let s = parse(r#"{"enabled": false}"#);
    assert!(!s.is_visible());
    assert_eq!(s.used, None);
}

/// The speculative fields (per-window `*_dollars`, surface-scoped limits,
/// `extra_usage.daily/weekly`) are null on every reachable account and their
/// shapes are unconfirmed, so they parse defensively: money in either shape, a
/// surface-named scoped label, and a period breakdown — and an unexpected shape
/// is ignored, never a `/usage` parse failure.
#[test]
fn blind_fields_parse_defensively() {
    // Lenient money: bare number = dollars, `{amount_minor,exponent}` = minor
    // units, `{amount}` = dollars, anything else = None.
    assert_eq!(json_to_dollars(&serde_json::json!(12.5)), Some(12.5));
    assert_eq!(
        json_to_dollars(&serde_json::json!({"amount_minor": 1234, "exponent": 2})),
        Some(12.34)
    );
    assert_eq!(
        json_to_dollars(&serde_json::json!({"amount": "7.5"})),
        Some(7.5)
    );
    assert_eq!(json_to_dollars(&serde_json::json!("garbage")), None);
    assert_eq!(json_to_dollars(&serde_json::json!({})), None);

    let json = r#"{
        "five_hour": {"utilization": 40, "resets_at": "2026-07-02T14:50:00+00:00",
                      "used_dollars": {"amount_minor": 320, "exponent": 2},
                      "limit_dollars": 10},
        "limits": [
            {"kind": "session", "percent": 40, "resets_at": "2026-07-02T14:50:00+00:00"},
            {"kind": "weekly_scoped", "percent": 5, "resets_at": "2026-07-07T00:00:00+00:00",
             "scope": {"surface": {"display_name": "Code"}}}
        ],
        "extra_usage": {"is_enabled": true, "currency": "USD",
                        "daily": {"used_credits": 1.5, "utilization": 15},
                        "weekly": 999}
    }"#;
    let raw: RawUsage = serde_json::from_str(json).unwrap();
    let w = windows_from_raw(&raw);

    // #5: both dollar shapes land on the 5h window (from the top-level object).
    let d = w.window_dollars.iter().find(|d| d.label == "5h").unwrap();
    assert_eq!(d.used, Some(3.20));
    assert_eq!(d.limit, Some(10.0));

    // #6: a surface-scoped limit (no model) is labeled from the surface name.
    assert_eq!(w.weekly_scoped.len(), 1);
    assert_eq!(w.weekly_scoped[0].label, "7d code");

    // #7: an object daily breakdown extracts; a bare-number weekly is ignored,
    // not a parse error.
    let extra = raw.extra_usage.as_ref().unwrap();
    let daily = ExtraPeriod::from_value(extra.daily.as_ref().unwrap()).unwrap();
    assert_eq!(daily.utilization, Some(15.0));
    assert_eq!(daily.used_credits, Some(1.5));
    assert!(ExtraPeriod::from_value(extra.weekly.as_ref().unwrap()).is_none());
}

// ── get_json emits Claude Code's exact per-client header set (wire parity) ────
//
// Captured 2026-07-14 against CC 2.1.209 (docs/wire-parity.md): CC polls /usage
// with its claude-cli client (+anthropic-beta, no cache-control) and reads
// /profile with a plain axios client (axios UA, Cache-Control: no-cache, no
// beta). This drives the REAL get_json builder against a loopback listener and
// asserts the bytes it actually emits — a header drift off CC's shape fails here.

fn capture_get_json_headers(client: AuthClient, path: &str) -> String {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 512];
        while let Ok(n) = sock.read(&mut tmp) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // minimal 200 so get_json's read_to_string returns Ok
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        String::from_utf8_lossy(&buf).into_owned()
    });

    // bypass the 5s per-host request spacing so the test doesn't sleep
    if let Ok(mut m) = NEXT_REQUEST_SLOT.lock() {
        m.clear();
    }
    let url = format!("http://127.0.0.1:{port}{path}");
    let _ = get_json(&url, "TESTTOKEN", None, "wiretest", client);
    server.join().expect("listener thread")
}

fn header_value<'a>(req: &'a str, name: &str) -> Option<&'a str> {
    let want = format!("{}:", name.to_ascii_lowercase());
    req.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&want))
        .and_then(|l| l.split_once(':').map(|x| x.1))
        .map(str::trim)
}

#[test]
fn get_json_emits_cc_per_client_wire_headers() {
    // /usage: the claude-cli client.
    let usage = capture_get_json_headers(AuthClient::Usage, "/api/oauth/usage");
    assert!(
        usage.starts_with("GET /api/oauth/usage "),
        "targets the usage path"
    );
    // version resolves from the locally-installed CC; bare `claude-cli` in a
    // no-CC environment (CI). Either way it's the claude-cli client prefix.
    let ua = header_value(&usage, "user-agent").unwrap_or("");
    assert!(
        ua.starts_with("claude-cli"),
        "usage UA is claude-cli, got {ua:?}"
    );
    assert_eq!(
        header_value(&usage, "accept"),
        Some("application/json, text/plain, */*")
    );
    assert_eq!(
        header_value(&usage, "content-type"),
        Some("application/json")
    );
    assert_eq!(
        header_value(&usage, "anthropic-beta"),
        Some("oauth-2025-04-20")
    );
    assert_eq!(
        header_value(&usage, "cache-control"),
        None,
        "usage sends no cache-control"
    );
    assert_eq!(
        header_value(&usage, "authorization"),
        Some("Bearer TESTTOKEN")
    );

    // /profile: the axios client — deterministic UA constant, cache-control, no beta.
    let profile = capture_get_json_headers(AuthClient::Profile, "/api/oauth/profile");
    assert!(profile.starts_with("GET /api/oauth/profile "));
    assert_eq!(header_value(&profile, "user-agent"), Some("axios/1.15.2"));
    assert_eq!(
        header_value(&profile, "accept"),
        Some("application/json, text/plain, */*")
    );
    assert_eq!(
        header_value(&profile, "content-type"),
        Some("application/json")
    );
    assert_eq!(header_value(&profile, "cache-control"), Some("no-cache"));
    assert_eq!(
        header_value(&profile, "anthropic-beta"),
        None,
        "profile sends no beta"
    );
}

// ── ureq's non-2xx default (the flag that keeps the rotate leg reachable) ────

/// ureq 3 answers a non-2xx with `Err(Error::StatusCode)` by DEFAULT, which once
/// turned every `status >= 400` branch on the `Ok` response into dead code here:
/// the 401 → rotate leg and the 429 retry-after read never fired, and every HTTP
/// error collapsed into `Network` (`docs/internals.md`, 2026-06-07). The whole
/// fix is one builder flag on this agent, and nothing pinned it — dropping the
/// flag, or rebuilding the agent without it, would strand rotation exactly the
/// same way while every offline test in the tree kept passing, because no offline
/// test ever gives this agent a real non-2xx to answer. So this one does.
#[test]
fn non_2xx_arrives_as_ok_so_the_status_branches_stay_reachable() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut tmp = [0u8; 1024];
        let _ = sock.read(&mut tmp);
        let _ = sock.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
    });

    let url = format!("http://127.0.0.1:{port}/unauthorized");
    let got = AGENT.get(&url).call();
    let _ = server.join();

    let response = got.expect(
        "a 401 must arrive as Ok: the rotate leg reads the status off the Ok response, and \
         ureq's default would collapse it into Err(StatusCode) -> Network",
    );
    assert_eq!(response.status().as_u16(), 401);
}

// ── RESCUE-1: identity-probe classification truth table ──────────────────────
//
// `classify_identity_response` is the reclaim-vs-retry safety hinge: ONLY a
// 401 may read as "the token itself is dead" (eligible for the destructive
// live-slot reclaim); every transient/ambiguous outcome must stay
// Indeterminate (retry, touch nothing). A drift that widened Rejected would
// point the reclaim at logins that are merely unreachable.

#[test]
fn identity_probe_classification_truth_table() {
    use super::IdentityProbe as P;
    let ok = |body: &str| classify_identity_response(Ok(body.to_string()));

    // The one and only Rejected: an endpoint 401.
    assert!(matches!(
        classify_identity_response(Err(FetchError::Status(401))),
        P::Rejected
    ));

    // Everything else that can go wrong proves nothing.
    assert!(
        matches!(
            classify_identity_response(Err(FetchError::Status(403))),
            P::Indeterminate
        ),
        "403 is the WAF/geo shape — must never read as a dead token"
    );
    for status in [400, 404, 429, 500, 503] {
        assert!(matches!(
            classify_identity_response(Err(FetchError::Status(status))),
            P::Indeterminate
        ));
    }
    assert!(matches!(
        classify_identity_response(Err(FetchError::Network)),
        P::Indeterminate
    ));
    assert!(matches!(
        classify_identity_response(Err(FetchError::RateLimited {
            retry_after: None,
            plan: None
        })),
        P::Indeterminate
    ));
    assert!(
        matches!(
            classify_identity_response(Err(FetchError::Parse)),
            P::Indeterminate
        ),
        "a transport-level parse failure proves nothing about the token"
    );

    // Body-shape drift proves nothing either.
    assert!(matches!(ok("not json"), P::Indeterminate));
    assert!(matches!(ok("{}"), P::Indeterminate), "no account block");
    assert!(matches!(ok(r#"{"account":{}}"#), P::Indeterminate));
    assert!(
        matches!(ok(r#"{"account":{"uuid":"   "}}"#), P::Indeterminate),
        "a blank uuid is shape drift, never an identity"
    );

    // The happy shape proves the account, email riding along when present.
    match ok(r#"{"account":{"uuid":" uuid-1 ","email":" a@b.c "}}"#) {
        P::Proven(id) => {
            assert_eq!(id.uuid, "uuid-1", "uuid is trimmed");
            assert_eq!(id.email.as_deref(), Some("a@b.c"), "email is trimmed");
        }
        other => panic!("expected Proven, got {other:?}"),
    }
    match ok(r#"{"account":{"uuid":"uuid-2"}}"#) {
        P::Proven(id) => assert_eq!(id.email, None, "email independently optional"),
        other => panic!("expected Proven, got {other:?}"),
    }
    match ok(r#"{"account":{"uuid":"uuid-3","email":"   "}}"#) {
        P::Proven(id) => assert_eq!(
            id.email, None,
            "a whitespace-only email is blank-filtered, not surfaced"
        ),
        other => panic!("expected Proven, got {other:?}"),
    }
}

// ── humanize_duration: seconds under 5 min ───────────────────────────────────

#[test]
fn humanize_duration_shows_seconds_under_five_minutes() {
    assert_eq!(humanize_duration(0), "now");
    assert_eq!(humanize_duration(-5), "now");
    assert_eq!(humanize_duration(1), "1s");
    assert_eq!(humanize_duration(45), "45s");
    assert_eq!(humanize_duration(59), "59s");
    assert_eq!(humanize_duration(60), "1m");
    assert_eq!(humanize_duration(90), "1m 30s");
    assert_eq!(humanize_duration(120), "2m");
    assert_eq!(humanize_duration(299), "4m 59s");
}

#[test]
fn humanize_duration_five_minutes_and_up_stays_coarse() {
    // At 5 min the seconds drop: minute granularity is enough that far out, and
    // the old rounding stays for every longer span.
    assert_eq!(humanize_duration(300), "5m");
    assert_eq!(humanize_duration(59 * 60), "59m");
    assert_eq!(humanize_duration(3600), "1h 0m");
    assert_eq!(humanize_duration(90 * 60), "1h 30m");
    assert_eq!(humanize_duration(25 * 3600), "1d 1h");
}
