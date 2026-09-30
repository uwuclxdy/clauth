#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `daemon::status_json::build_status` shape + field derivation.
//!
//! These exercise the single-shot path (`live = None`, freshness/next-refresh
//! from cache mtime) against a `HomeSandbox` so no real `~/.clauth` is touched.

use std::collections::{BTreeMap, HashMap};

use super::*;
use crate::profile::{
    AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName, save_profile,
};
use crate::profile_json::Window;
use crate::testutil::{HomeSandbox, schema_agrees_with_type, schema_deref};
use crate::usage::{FetchLeg, FetchStatus};
use utoipa::PartialSchema;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;

/// The typed body as a `Value`, for the tests that assert published values;
/// key order and byte shape are pinned by the `*_bytes` tests.
fn status_value(
    config: &AppConfig,
    interval_ms: u64,
    live: Option<&LiveSignals>,
    include_disabled: bool,
) -> serde_json::Value {
    serde_json::to_value(build_status(config, interval_ms, live, include_disabled)).unwrap()
}

fn oauth_profile(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    p
}

#[test]
fn build_status_top_level_shape_and_active() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), oauth_profile("home")],
    };
    config.state.active_profile = Some("work".into());
    config.state.refresh_interval_ms = 300_000;

    let v = status_value(&config, config.state.refresh_interval_ms, None, false);

    assert_eq!(v["schema"], SCHEMA_VERSION);
    assert_eq!(v["active_profile"], "work");
    assert_eq!(v["wrap_off"], false);
    assert_eq!(v["refresh_interval_ms"], 300_000);
    assert!(v["generated_at"].as_str().unwrap().contains('T'));
    // Exact key sets — a silent rename/removal anywhere in the contract fails
    // here rather than in a downstream reader.
    let mut top: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    top.sort_unstable();
    assert_eq!(
        top,
        [
            "active_codex_profile",
            "active_profile",
            "clauth_version",
            "codex_fallback_chain",
            "codex_wrap_off",
            "gateway",
            "generated_at",
            "pending_switch",
            "profiles",
            "proxies",
            "refresh_interval_ms",
            "schema",
            "wrap_off",
        ],
    );
    let profiles = v["profiles"].as_array().unwrap();
    let mut per: Vec<&str> = profiles[0]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    per.sort_unstable();
    assert_eq!(
        per,
        [
            "active",
            "auth_status",
            "auto_start",
            // Additive (interleaved auto-start queue): the
            // profile's queue slot and the queue's shared next-open estimate,
            // `null` for a profile that holds no slot.
            "auto_start_queue",
            "base_url",
            "bell_threshold",
            "fallback",
            "fetch_status",
            "fetched_at",
            "harness",
            "has_live_session",
            "name",
            "next_refresh_at",
            "provider",
            "rolling_token",
            "stale",
            "third_party",
            "tier",
            "windows",
        ],
    );
    assert_eq!(profiles.len(), 2);
    let work = profiles.iter().find(|p| p["name"] == "work").unwrap();
    assert_eq!(work["active"], true);
    assert_eq!(work["provider"], "anthropic");
    // No cache on disk → never-fetched profile reports nulls, not stale numbers.
    assert!(work["fetch_status"].is_null());
    assert!(work["fetched_at"].is_null());
    assert!(work["next_refresh_at"].is_null());
    assert!(work["windows"].as_array().unwrap().is_empty());
    let home = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "home")
        .unwrap()
        .clone();
    assert_eq!(home["active"], false);
}

#[test]
fn build_status_fallback_membership_and_armed() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("a"), oauth_profile("b"), oauth_profile("c")],
    };
    for p in &config.profiles {
        save_profile(p).unwrap();
    }
    config.state.active_profile = Some("a".into());
    config.state.fallback_chain = vec!["a".into(), "b".into()];

    let v = status_value(&config, 300_000, None, false);
    let profiles = v["profiles"].as_array().unwrap();

    let a = profiles.iter().find(|p| p["name"] == "a").unwrap();
    assert_eq!(a["fallback"]["position"], 1);
    assert_eq!(a["fallback"]["threshold"], 95.0); // DEFAULT_THRESHOLD
    assert_eq!(a["fallback"]["armed"], true, "active + in chain = armed");

    let b = profiles.iter().find(|p| p["name"] == "b").unwrap();
    assert_eq!(b["fallback"]["position"], 2);
    assert_eq!(b["fallback"]["armed"], false, "in chain but not active");

    let c = profiles.iter().find(|p| p["name"] == "c").unwrap();
    assert!(c["fallback"].is_null(), "not a chain member → null");
}

// ── disabled: hidden from the feed by default, surfaced via include_disabled ──

#[test]
fn build_status_hides_disabled_by_default_and_shows_with_include_disabled() {
    let _home = HomeSandbox::new();
    let mut off = oauth_profile("off");
    off.disabled = true;
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("on"), off],
    };

    let hidden = status_value(&config, 300_000, None, false);
    let hidden_names: Vec<&str> = hidden["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        hidden_names,
        ["on"],
        "a disabled account must not appear in the default feed"
    );

    let shown = status_value(&config, 300_000, None, true);
    let mut shown_names: Vec<&str> = shown["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    shown_names.sort_unstable();
    assert_eq!(
        shown_names,
        ["off", "on"],
        "include_disabled=true must surface the full set"
    );
}

// A disabled ACTIVE must stay visible even under the default hide, or the
// top-level `active_profile` field names an entry `profiles[]` doesn't carry —
// a reader following wiki/Daemon.md's contract (resolve `active_profile`
// against `profiles[]`) would find nothing.
#[test]
fn build_status_keeps_a_disabled_active_visible_so_active_profile_never_dangles() {
    let _home = HomeSandbox::new();
    let mut active_off = oauth_profile("active-off");
    active_off.disabled = true;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![active_off, oauth_profile("sibling")],
    };
    config.state.active_profile = Some("active-off".into());

    let v = status_value(&config, 300_000, None, false);
    let names: Vec<&str> = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"active-off"),
        "the disabled ACTIVE profile must stay visible in profiles[] even under the default hide"
    );
    let active_name = v["active_profile"].as_str().unwrap();
    assert!(
        names.contains(&active_name),
        "active_profile must always resolve against an entry in profiles[] — no dangling reference"
    );
}

// ── AUTH-2: auth_status + pending_switch contract ─────────────────────────────

fn set_expiry(p: &mut Profile, expires_at: i64) {
    p.credentials
        .as_mut()
        .unwrap()
        .claude_ai_oauth
        .as_mut()
        .unwrap()
        .expires_at = Some(expires_at);
}

#[test]
fn build_status_auth_status_ok_expired_broken() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;

    let mut ok = oauth_profile("ok");
    set_expiry(&mut ok, now + 3_600_000); // real life left → ok
    let mut expired = oauth_profile("expired");
    set_expiry(&mut expired, now - 1_000); // past due, not flagged → expired
    let mut broken = oauth_profile("broken");
    set_expiry(&mut broken, now - 1_000); // past due AND flagged → broken wins

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![ok, expired, broken],
    };
    config.set_auth_broken(&crate::profile::ProfileName::from("broken"), true);

    let v = status_value(&config, 300_000, None, false);
    let profiles = v["profiles"].as_array().unwrap();
    let get = |n: &str| profiles.iter().find(|p| p["name"] == n).unwrap();
    assert_eq!(get("ok")["auth_status"], "ok");
    assert_eq!(get("expired")["auth_status"], "expired");
    assert_eq!(
        get("broken")["auth_status"],
        "broken",
        "broken outranks expired"
    );
}

/// `auth_status` reports on the credential a profile STORES, not on where its
/// requests route: a hybrid (OAuth pair + `base_url`) with a dead access token
/// must publish `expired`, while an endpoint-only profile has no token to expire.
#[test]
fn build_status_auth_status_types_the_hybrid_on_its_credential() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;

    let mut hybrid = oauth_profile("hybrid");
    set_expiry(&mut hybrid, now - 1_000);
    hybrid.base_url = Some("https://api.z.ai/api/anthropic".to_string());

    let api_key_only = Profile::new(
        "apikey".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-test".to_string()),
    );

    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![hybrid, api_key_only],
    };

    let v = status_value(&config, 300_000, None, false);
    let profiles = v["profiles"].as_array().unwrap();
    let get = |n: &str| profiles.iter().find(|p| p["name"] == n).unwrap();
    assert_eq!(
        get("hybrid")["auth_status"],
        "expired",
        "a stored pair expires regardless of the endpoint it routes past"
    );
    assert_eq!(
        get("apikey")["auth_status"],
        "ok",
        "no stored pair → nothing to expire"
    );
}

#[test]
fn build_status_pending_switch_reflects_live_signal() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work")],
    };
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();

    // single-shot (no daemon) → pending_switch is present-but-null.
    let none = status_value(&config, 300_000, None, false);
    assert!(
        none.get("pending_switch").is_some(),
        "pending_switch key is always present"
    );
    assert!(none["pending_switch"].is_null());

    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: Some("home"),
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(v["pending_switch"], "home");
    assert_eq!(
        v["schema"], SCHEMA_VERSION,
        "pending_switch is additive — no bump of its own"
    );
}

/// The queue object is pinned by VALUE, not key presence: `position` is the
/// member's 1-based slot in the shared order, `next_open_at` round-trips to
/// anchor + gap, and every null shape is spelled out — toggle off, not a
/// member, and member-with-no-anchor. Publishing `null` for every position
/// would otherwise survive the key-list test.
#[test]
fn build_status_auto_start_queue_positions_and_null_cases() {
    let _home = HomeSandbox::new();
    let queued = |name: &str| {
        let mut p = oauth_profile(name);
        p.auto_start = true;
        p
    };
    let mut config = AppConfig {
        state: AppState {
            fallback_chain: vec!["a".into(), "b".into()],
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![queued("a"), queued("b"), oauth_profile("c")],
    };

    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();
    let anchor = 1_780_000_000i64;
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: Some(anchor),
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let queue_of = |v: &serde_json::Value, name: &str| -> serde_json::Value {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .expect("profile published")["auto_start_queue"]
            .clone()
    };

    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(queue_of(&v, "a")["position"], 1);
    assert_eq!(queue_of(&v, "b")["position"], 2);
    // Round-trip rather than a formatted literal, so the pin is on the
    // arithmetic (anchor + gap) and not on the ISO renderer.
    let published = queue_of(&v, "a")["next_open_at"]
        .as_str()
        .expect("an anchored queue publishes a next-open stamp")
        .to_string();
    assert_eq!(
        crate::usage::iso_to_epoch_secs(&published),
        Some(anchor + crate::usage::queue_gap_secs(2, 300_000)),
    );
    assert_eq!(
        queue_of(&v, "a")["next_open_at"],
        queue_of(&v, "b")["next_open_at"],
        "the estimate is the queue's, shared by every member"
    );
    // Null case 1: an OAuth profile that never opted into auto_start.
    assert!(queue_of(&v, "c").is_null());

    // Null case 2: a member with no anchor yet — the slot publishes, the
    // stamp is null (reads as "due now").
    let cold = LiveSignals {
        queue_anchor: None,
        ..live
    };
    let v = status_value(&config, 300_000, Some(&cold), false);
    assert_eq!(queue_of(&v, "a")["position"], 1);
    assert!(queue_of(&v, "a")["next_open_at"].is_null());

    // Null case 3: the toggle off is a real off switch on the feed too.
    config.state.auto_start_queue = false;
    let v = status_value(&config, 300_000, Some(&live), false);
    for name in ["a", "b", "c"] {
        assert!(queue_of(&v, name).is_null());
    }
}

/// An api-key profile's freshness derives from ITS cache
/// (`THIRD_PARTY_CACHE_FILE`), and a name the live stores don't carry falls
/// back to the same derivation — pre-fix both keyed on the OAuth
/// `USAGE_CACHE_FILE`/status store, so a healthy hourly-refreshed api-key
/// account rendered permanently as never-fetched (`fetch_status: null`).
#[test]
fn build_status_third_party_freshness_from_its_own_cache() {
    let _home = HomeSandbox::new();
    let mut api = Profile::new("zai".to_string(), None, None);
    api.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    api.api_key = Some("k".to_string());
    api.provider = crate::providers::Provider::from_base_url(api.base_url.as_deref().unwrap());
    assert!(api.is_third_party(), "fixture must be an api-key profile");
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![api],
    };

    // Warm third-party cache, no OAuth cache: the profile is fetched.
    crate::testutil::register_names(&["zai"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("zai"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );

    // Single-shot: freshness from the third-party cache mtime (just written).
    let v = status_value(&config, 300_000, None, false);
    let p = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(p["fetch_status"], "Fresh");
    assert!(!p["fetched_at"].is_null());
    assert!(!p["next_refresh_at"].is_null());
    assert_eq!(p["third_party"]["available"], true);

    // Live daemon whose stores don't carry the name (the OAuth-leg stores
    // never do for api-key profiles): same derivation, not null.
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    let p = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(
        p["fetch_status"], "Fresh",
        "a live daemon must not blank an api-key profile's freshness"
    );
    assert!(!p["next_refresh_at"].is_null());
}

/// `refresh_spent_accounts` OFF + a spent (100%-capped) OAuth window: the
/// account is skipped until reset, so it has no pending refresh — the feed nulls
/// `next_refresh_at` instead of the past mtime+interval stamp the derivation
/// would otherwise emit. With the toggle ON (default) the same account keeps its
/// derived countdown.
#[test]
fn build_status_nulls_next_refresh_for_a_spent_skipped_account() {
    let _home = HomeSandbox::new();
    let config = |refresh_spent: bool| AppConfig {
        state: AppState {
            refresh_spent_accounts: refresh_spent,
            ..AppState::default()
        },
        profiles: vec![oauth_profile("maxed")],
    };
    // Warm the OAuth usage cache with a live 100%-capped 5h window.
    crate::testutil::register_names(&["maxed"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("maxed"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );

    // Toggle OFF → skipped-spent → next_refresh_at nulled.
    let off = status_value(&config(false), 300_000, None, false);
    let p = &off["profiles"].as_array().unwrap()[0];
    assert!(
        p["next_refresh_at"].is_null(),
        "a spent skipped account has no pending refresh: {p}"
    );

    // Toggle ON (default) → still polled → derived countdown present.
    let on = status_value(&config(true), 300_000, None, false);
    let p = &on["profiles"].as_array().unwrap()[0];
    assert!(
        !p["next_refresh_at"].is_null(),
        "polling a spent account still schedules a refresh: {p}"
    );
}

/// A single-shot body derives `next_refresh_at` as mtime + interval. Once that
/// stamp is past (`now >= stamp`) no live countdown vouches for it, so the field
/// publishes `null` — pre-fix it published the overdue stamp, which reads as
/// perpetually overdue (#74). Live-store stamps stay verbatim.
#[test]
fn build_status_nulls_a_past_derived_next_refresh() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work")],
    };
    crate::testutil::register_names(&["work"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("work"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.0,
                resets_at: None,
            }),
            ..Default::default()
        },
    );
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("work"),
        crate::profile_cache::USAGE_CACHE_FILE,
    )
    .unwrap();
    // Back-date the cache by 2 × interval, so mtime + interval is a known
    // interval_ms in the past (mtime and interval are both ms, pinned here).
    let interval_ms = 300_000u64;
    crate::testutil::set_mtime(
        &path,
        std::time::SystemTime::now() - std::time::Duration::from_millis(2 * interval_ms),
    );

    let v = status_value(&config, interval_ms, None, false);
    let p = &v["profiles"].as_array().unwrap()[0];
    assert!(
        p["next_refresh_at"].is_null(),
        "a past derived stamp must publish null, got: {p}"
    );
}

/// A plan-only cache rewrite (`apply_outcome`'s `plan_refresh` write, the
/// hourly `/profile` ride on a 429'd `/usage`) moves the file's mtime to NOW
/// while the body's `fetched_at` still names the fetch that last read the
/// account. `fetch_status` and `next_refresh_at` derived off the mtime, so
/// that rewrite re-aged the account — Fresh again, countdown re-armed — the
/// #74 "stale reading served as live" shape reached through the plan leg
/// instead of a dead poller. Both fields derive off the body's own stamp
/// (R8): the one clock a plan-only write provably does not move.
#[test]
fn build_status_does_not_re_age_a_plan_only_rewrite() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work")],
    };
    let interval_ms = 300_000u64;
    crate::testutil::register_names(&["work"]);
    let body = |fetched_at: Option<u64>| {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from("work"),
            crate::profile_cache::USAGE_CACHE_FILE,
            &crate::usage::UsageInfo {
                five_hour: Some(crate::usage::UsageWindow {
                    utilization: 42.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                fetched_at,
                ..Default::default()
            },
        );
    };
    // The plan-only shape: a body 4 intervals old under a file rewritten now.
    // 20 min also clears this interval's staleness threshold
    // (2 × max(300s, 5min) + 300s = 15min), so the age arm pins on the same
    // body — it was already off the body (#74 R2); the re-age was these two
    // fields alone.
    let age_ms = 4 * interval_ms;
    body(Some(crate::usage::now_ms() - age_ms));

    let v = status_value(&config, interval_ms, None, false);
    let row = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(
        row["fetch_status"], "Cached",
        "a rewrite is not a fetch: the status follows the body's stamp"
    );
    assert_eq!(
        row["stale"], true,
        "20 min past fetch is past the threshold"
    );
    assert_eq!(
        row["next_refresh_at"],
        serde_json::Value::Null,
        "the last fetch's slot is 3 intervals past; a rewrite cannot re-arm it"
    );
    // The published stamp keeps naming the fetch, never the rewrite.
    let published = row["fetched_at"].as_str().expect("a dated body publishes");
    let published_ms = crate::usage::iso_to_epoch_secs(published).expect("ISO-8601") * 1000;
    assert!(
        crate::usage::now_ms().saturating_sub(u64::try_from(published_ms).expect("positive"))
            > age_ms / 2,
        "the published stamp must date the fetch, not the file: {published}"
    );

    // Control: same file, stamp moved to now — a real fetch. Fresh, countdown
    // armed: the derivation still reads a live fetch correctly.
    body(Some(crate::usage::now_ms()));
    let v = status_value(&config, interval_ms, None, false);
    let row = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(
        row["fetch_status"], "Fresh",
        "control: a live fetch is Fresh"
    );
    assert!(
        !row["next_refresh_at"].is_null(),
        "control: a live fetch has a pending refresh"
    );

    // An undatable body (a plan-only cold fill, a pre-`fetched_at` cache) has
    // no stamp; the file's own write is its only clock and stays the fallback.
    body(None);
    let path = crate::profile_cache::profile_cache_path(
        &crate::profile::ProfileName::from("work"),
        crate::profile_cache::USAGE_CACHE_FILE,
    )
    .unwrap();
    crate::testutil::set_mtime(
        &path,
        std::time::SystemTime::now() - std::time::Duration::from_millis(2 * interval_ms),
    );
    let v = status_value(&config, interval_ms, None, false);
    assert_eq!(
        v["profiles"].as_array().unwrap()[0]["fetch_status"],
        "Cached",
        "no stamp to trust, so the file's own write dates it"
    );
}

/// The half a spent-skip gate keyed on `is_third_party` gets wrong: a GENERIC
/// api-key endpoint (`provider` is `None`, so that predicate says false) is
/// fetched on the cadence by the third-party leg, and `drop_spent_oauth` blanks
/// the OAuth leg's countdown map alone — so a spent account is skipped on one
/// leg while the other still has a refresh pending, and the feed published
/// `null` over it.
///
/// The fixture is the HYBRID, which is the reachable shape: one Setup endpoint
/// edit on a spent OAuth account (`edit_profile_endpoint`) keeps the pair, the
/// key and the maxed `usage_cache.json`, so that reading is CURRENT rather than
/// leftover and the OAuth leg is genuinely mid-skip.
#[test]
fn build_status_keeps_a_generic_api_key_countdown_over_a_maxed_oauth_cache() {
    let _home = HomeSandbox::new();
    let mut api = oauth_profile("litellm");
    api.base_url = Some("http://127.0.0.1:4000".to_string());
    api.api_key = Some("k".to_string());
    api.provider = crate::providers::Provider::from_base_url(api.base_url.as_deref().unwrap());
    assert!(
        !api.is_third_party() && api.usage_cache_is_third_party(),
        "fixture must be the case the two predicates disagree on",
    );
    let config = AppConfig {
        state: AppState {
            refresh_spent_accounts: false,
            ..AppState::default()
        },
        profiles: vec![api],
    };
    crate::testutil::register_names(&["litellm"]);
    // Current, not stale: the OAuth leg still polls this pair, and this is the
    // reading `drop_spent_oauth` skips on.
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("litellm"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
            }),
            ..Default::default()
        },
    );
    // The cache this account's own leg writes.
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("litellm"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );

    // Single-shot: derived off the third-party cache's mtime, not suppressed.
    let single = status_value(&config, 300_000, None, false);
    let p = &single["profiles"].as_array().unwrap()[0];
    assert!(
        !p["next_refresh_at"].is_null(),
        "the third-party leg refreshes this account on the cadence: {p}"
    );

    // Live daemon: the countdown that leg published must reach the feed
    // verbatim — a stamp the mtime derivation could not have produced, so this
    // fails on suppression rather than on the two paths agreeing by accident.
    let next: std::collections::HashMap<crate::usage::LegKey, u64> = [(
        crate::usage::FetchLeg::ThirdParty.key(crate::profile::ProfileName::from("litellm")),
        4_102_444_800_000,
    )]
    .into_iter()
    .collect();
    let empty_status = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    let p = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(
        p["next_refresh_at"], "2100-01-01T00:00:00+00:00",
        "the live third-party countdown must reach the feed: {p}"
    );
}

// RLS-1: the additive per-profile `stale` flag = the daemon distrusts this
// reading as a deep-slot stuck RateLimited (live status RateLimited AND the 429
// streak past the active cap) — the SAME predicate `scan_auto_switch` acts on,
// so the published cue and the switch decision cannot drift. Additive: the
// single-shot (no streaks) is always false.
#[test]
fn build_status_stale_flags_a_deep_slot_stuck_rate_limited_profile() {
    use crate::usage::FetchStatus;
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    // TWO profiles, so a "computed once and applied to every row" regression
    // (rather than keyed per profile name) is catchable.
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), oauth_profile("home")],
    };
    let next: HashMap<crate::usage::LegKey, u64> = HashMap::new();
    let deep = crate::usage::ACTIVE_CAP_MAX_STREAK + 1;
    let stale_of = |name: &str, v: &serde_json::Value| -> serde_json::Value {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap()["stale"]
            .clone()
    };

    // single-shot (no daemon / no streaks) → stale is present-and-false.
    let none = status_value(&config, 300_000, None, false);
    assert_eq!(
        none["schema"], SCHEMA_VERSION,
        "stale is additive — no bump of its own"
    );
    assert_eq!(
        stale_of("work", &none),
        false,
        "single-shot never publishes a distrusted reading"
    );

    // Two profiles in ONE body: `work` is a deep-slot stuck RateLimited (→ stale),
    // `home` is Fresh with an (irrelevant) equally-deep streak (→ NOT stale). This
    // one call proves the flag keys on the profile's OWN status+streak, is
    // per-profile (not one value smeared across the array), and that streak depth
    // alone never stales a live reading.
    let status = HashMap::from([
        ("work".to_string(), FetchStatus::RateLimited),
        ("home".to_string(), FetchStatus::Fresh),
    ]);
    let streaks = HashMap::from([("work".to_string(), deep), ("home".to_string(), deep)]);
    let live = LiveSignals {
        status: &status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("work", &v),
        true,
        "a deep-slot stuck RateLimited reading is published as stale"
    );
    assert_eq!(
        stale_of("home", &v),
        false,
        "a Fresh sibling is never stale however deep its streak — and stale is \
         per-profile, not computed once and applied to the whole array"
    );

    // Shallow RateLimited (≤ cap) → not yet distrusted.
    let status = HashMap::from([("work".to_string(), FetchStatus::RateLimited)]);
    let streaks = HashMap::from([("work".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK)]);
    let live = LiveSignals {
        status: &status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("work", &v),
        false,
        "a shallow RateLimited reading is not stale"
    );
}

/// The #74 age arm: past `2 × max(interval_ms, 5min) + interval` of cache age
/// the reading is stale on the single-shot path too — the exact surface that
/// reported `stale: false` at 22h. A live-maxed window under the spent-accounts
/// opt-out cannot change by polling, so its age arm stays silent: only the
/// opt-out skips it and the flag it would otherwise poll is still consulted.
#[test]
fn build_status_stale_flags_an_overdue_cache_on_the_single_shot_path() {
    use crate::usage::FetchStatus;
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work")],
    };
    config.state.refresh_interval_ms = 90_000;
    crate::testutil::register_names(&["work"]);
    let stale_of = |name: &str, v: &serde_json::Value| -> serde_json::Value {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap()["stale"]
            .clone()
    };
    // Threshold at this interval: 2 × max(90s, the degraded ceiling) + 90s.
    let ceiling_secs = crate::usage::DEGRADED_GAP_CEILING_MS / 1000;
    let threshold_secs = 2 * ceiling_secs + 90;
    let write = |utilization: f64, age_secs: u64| {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from("work"),
            crate::profile_cache::USAGE_CACHE_FILE,
            &crate::usage::UsageInfo {
                five_hour: Some(crate::usage::UsageWindow {
                    utilization,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                fetched_at: Some(crate::usage::now_ms() - age_secs * 1000),
                ..Default::default()
            },
        );
    };
    // Fresh and at-threshold → not stale; past it → stale on the single-shot.
    write(42.0, threshold_secs - 60);
    let v = status_value(&config, 90_000, None, false);
    assert_eq!(
        stale_of("work", &v),
        false,
        "a cache younger than the threshold is not stale"
    );
    write(42.0, threshold_secs + 60);
    let v = status_value(&config, 90_000, None, false);
    assert_eq!(
        stale_of("work", &v),
        true,
        "past 2 × max(interval, 5min) + interval the single-shot publishes stale — #74's 22h reading"
    );
    assert_eq!(
        v["schema"], SCHEMA_VERSION,
        "the age arm is additive — no bump of its own"
    );

    // A body this feed cannot date publishes `stale` and NO `fetched_at`: the
    // figures stay visible, and nothing claims to date them. Both undatable
    // shapes take the arm, and the file's mtime stays at now throughout, so a
    // regression back to mtime would read every case as fresh.
    for (case, fetched_at) in [
        ("no stamp", None),
        ("future stamp", Some(crate::usage::now_ms() + 3_600_000)),
    ] {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from("work"),
            crate::profile_cache::USAGE_CACHE_FILE,
            &crate::usage::UsageInfo {
                five_hour: Some(crate::usage::UsageWindow {
                    utilization: 42.0,
                    resets_at: Some("2999-01-01T00:00:00+00:00".to_string()),
                }),
                fetched_at,
                ..Default::default()
            },
        );
        let v = status_value(&config, 90_000, None, false);
        let row = v["profiles"].as_array().unwrap()[0].clone();
        assert_eq!(row["stale"], true, "{case}: an undatable body reads stale");
        assert_eq!(
            row["fetched_at"],
            serde_json::Value::Null,
            "{case}: the feed publishes no stamp it does not trust",
        );
        assert_eq!(
            row["windows"].as_array().map(Vec::len),
            Some(1),
            "{case}: the figure it dates stays visible",
        );
    }

    // The verdict qualifies a figure this feed publishes. An all-lapsed body
    // publishes an empty `windows[]`, so no age can make it stale — this is the
    // arm that separates the live-window predicate from a field count, which
    // answers `true` for the same body.
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("work"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.0,
                resets_at: Some("2000-01-01T00:00:00+00:00".to_string()),
            }),
            fetched_at: Some(crate::usage::now_ms() - (threshold_secs + 60) * 1000),
            ..Default::default()
        },
    );
    let v = status_value(&config, 90_000, None, false);
    let row = v["profiles"].as_array().unwrap()[0].clone();
    assert_eq!(
        row["windows"].as_array().map(Vec::len),
        Some(0),
        "fixture control: the lapsed row really is dropped from the feed",
    );
    assert_eq!(
        stale_of("work", &v),
        false,
        "no published figure, so nothing for the marker to qualify"
    );

    // A live-maxed window under the spent-accounts opt-out is exempt from the
    // age arm: its figure cannot change by polling, so age distrusts nothing.
    config.state.refresh_spent_accounts = false;
    write(100.0, threshold_secs + 60);
    let v = status_value(&config, 90_000, None, false);
    assert_eq!(
        stale_of("work", &v),
        false,
        "a live-maxed window the opt-out skips is never age-stale"
    );
    // The two quadrants the maxed arm does not cover: a NON-maxed window under
    // the opt-out still takes the age verdict (the exemption is keyed on the
    // opt-out AND the live-maxed shape together, so the flag alone exempts
    // nothing), and a live-maxed window with the opt-out ON takes the verdict
    // too — the source derives the exemption only under
    // `!refresh_spent_accounts && windows_maxed`.
    write(42.0, threshold_secs + 60);
    let v = status_value(&config, 90_000, None, false);
    assert_eq!(
        stale_of("work", &v),
        true,
        "a non-maxed window is age-stale even with spent accounts skipped"
    );
    config.state.refresh_spent_accounts = true;
    write(100.0, threshold_secs + 60);
    let v = status_value(&config, 90_000, None, false);
    assert_eq!(
        stale_of("work", &v),
        true,
        "the live-maxed exemption is inherited from the opt-out alone: with the \
         opt-out on, a maxed window is still age-stale"
    );
    // The stuck-429 arm is untouched: the exemption shares the OR, it does not
    // replace the flag. Pinned by its own test above.

    // The age arm holds on the DAEMON feed too: same cache, live signals
    // attached, no stuck-429 (Fresh store entry, no streaks) — an old body
    // must read stale on the more-used surface, not only the single-shot.
    config.state.refresh_spent_accounts = true;
    write(42.0, threshold_secs + 60);
    let live = LiveSignals {
        status: &HashMap::from([("work".to_string(), FetchStatus::Fresh)]),
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &HashMap::new(),
        streaks: &HashMap::new(),
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 90_000, Some(&live), false);
    assert_eq!(
        stale_of("work", &v),
        true,
        "an overdue cache reads stale on the live daemon feed too"
    );
}

/// The third-party leg writes its outcomes to `third_party_status`, not the
/// OAuth `status` store the feed used to read alone. A name missing from that
/// one fell through to the mtime derivation — and an `AuthExpired` fetch writes
/// no cache, so the field came out `null`: a dead console session was
/// indistinguishable from a profile that had never been fetched. That is the
/// exact dishonesty "detect it, stop fetching, say why" was chosen to avoid.
#[test]
fn build_status_publishes_the_third_party_legs_own_status() {
    let _home = HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let mut qwen = Profile::new("qwen".to_string(), Some(base.to_string()), None);
    qwen.provider = crate::providers::Provider::from_base_url(base);
    let config = AppConfig {
        state: AppState {
            profiles: vec!["qwen".into()],
            ..AppState::default()
        },
        profiles: vec![qwen],
    };
    let empty: HashMap<String, FetchStatus> = HashMap::new();
    let next = HashMap::new();
    let streaks = HashMap::new();

    // No cache on disk (an AuthExpired fetch writes none), so the mtime
    // derivation has nothing — the live third-party store is the only source.
    let tp = HashMap::from([("qwen".to_string(), FetchStatus::AuthExpired)]);
    let live = LiveSignals {
        status: &empty,
        third_party_status: &tp,
        third_party_streaks: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(
        v["profiles"][0]["fetch_status"], "AuthExpired",
        "a dead console session must not read as never-fetched",
    );
    // `stale` is contracted as a stuck 429 off the OAuth store and must not
    // start following the third-party leg.
    assert_eq!(v["profiles"][0]["stale"], false);

    // A third-party 429 reaches the feed too — pre-fix it published whatever the
    // cache mtime said, which is a freshness claim about a rejected poll.
    let tp = HashMap::from([("qwen".to_string(), FetchStatus::RateLimited)]);
    let live = LiveSignals {
        status: &empty,
        third_party_status: &tp,
        third_party_streaks: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(v["profiles"][0]["fetch_status"], "RateLimited");
}

/// The OAuth leg keeps precedence, exactly as the TUI's own merge does: a
/// hybrid profile carrying both must not have its OAuth verdict overwritten.
#[test]
fn build_status_prefers_the_oauth_leg_when_both_stores_carry_a_name() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState {
            profiles: vec!["both".into()],
            ..AppState::default()
        },
        profiles: vec![Profile::new("both".to_string(), None, None)],
    };
    let status = HashMap::from([("both".to_string(), FetchStatus::Fresh)]);
    let tp = HashMap::from([("both".to_string(), FetchStatus::AuthExpired)]);
    let next = HashMap::new();
    let streaks = HashMap::new();
    let live = LiveSignals {
        status: &status,
        third_party_status: &tp,
        third_party_streaks: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(v["profiles"][0]["fetch_status"], "Fresh");
}

/// The daemonless surfaces (`clauth status --json`, `clauth list`) derive
/// freshness from the usage cache's mtime, so a warm cache behind a DEAD
/// console session published `fetch_status: "Fresh"` — a live measurement over
/// a credential that can never self-heal, which is the exact failure this
/// design was chosen over "keep last-known values" to avoid. The durable
/// verdict is keyed by credential fingerprint, so it can only ever say
/// "the last fetch under the credential you still hold died".
#[test]
fn build_status_reports_a_recorded_dead_credential_without_a_daemon() {
    let _home = HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let session = |token: &str| crate::profile::ConsoleCredential {
        token: token.to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    };
    let profile = |token: &str| {
        let mut p = Profile::new("qwen".to_string(), Some(base.to_string()), None);
        p.provider = crate::providers::Provider::from_base_url(base);
        p.console = Some(session(token));
        p
    };
    let config_of = |p: Profile| AppConfig {
        state: AppState {
            profiles: vec!["qwen".into()],
            ..AppState::default()
        },
        profiles: vec![p],
    };

    // A cache written just now: the mtime derivation calls this "Fresh".
    crate::testutil::register_names(&["qwen"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("qwen"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: Vec::new(),
            bars: Vec::new(),
            plan: Some("lite".to_string()),
            endpoint: None,
            best_effort: false,
        },
    );
    let dead = config_of(profile("dead-token"));
    let v = status_value(&dead, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["fetch_status"], "Fresh",
        "precondition: the mtime derivation alone calls a warm cache Fresh",
    );

    // Record the verdict against the credential the profile holds.
    let fp = crate::usage::profile_credential_fingerprint(&dead.profiles[0])
        .expect("a console-credentialed profile has a fingerprint");
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("qwen"), fp);

    let v = status_value(&dead, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["fetch_status"], "AuthExpired",
        "no daemon, warm cache, dead session — must not read as a live measurement",
    );

    // A re-login changes the credential, so the record stops applying on its
    // own. THIS is what makes persisting it safe.
    let relogged = config_of(profile("fresh-token"));
    let v = status_value(&relogged, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["fetch_status"], "Fresh",
        "a record for a credential the profile no longer holds is inert",
    );
}

/// The record must never invent a reading for a profile nothing has fetched.
#[test]
fn build_status_leaves_a_never_fetched_profile_unknown() {
    let _home = HomeSandbox::new();
    let base = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic";
    let mut p = Profile::new("cold".to_string(), Some(base.to_string()), None);
    p.provider = crate::providers::Provider::from_base_url(base);
    let config = AppConfig {
        state: AppState {
            profiles: vec!["cold".into()],
            ..AppState::default()
        },
        profiles: vec![p],
    };
    let v = status_value(&config, 300_000, None, false);
    assert!(
        v["profiles"][0]["fetch_status"].is_null(),
        "no cache and no verdict is unknown, not a status",
    );
}

/// The published `rolling_token` is what the sidecar HOLDS — the same content
/// classification the TUI renders — never the config flag. status.json is the
/// one surface where a reader has no second source to check against, so a
/// flag-driven value would tell external readers a degraded mint is routine
/// hours-scale maintenance (or hide a rolling bearer behind a mint's 30-day
/// warning ramp).
#[test]
fn build_status_rolling_token_is_the_sidecar_content_not_the_config_flag() {
    let _home = HomeSandbox::new();
    let name = "roll-truth";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let config_with_flag = |rolling_token: bool| {
        let mut p = oauth_profile(name);
        p.rolling_token = rolling_token;
        AppConfig {
            state: AppState::default(),
            profiles: vec![p],
        }
    };
    let sidecar = |scopes: Vec<&str>, plan: Option<&str>| ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-status-fixture".to_string(),
            refresh_token: None,
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: Some(scopes.into_iter().map(String::from).collect()),
            subscription_type: plan.map(String::from),
            ..crate::profile::OAuthToken::default_extra()
        }),
    };

    // Flag ON, sidecar degraded onto the mint: publish the mint.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&sidecar(
            vec!["user:inference", "user:sessions:claude_code"],
            None,
        ))
        .unwrap(),
    )
    .unwrap();
    let v = status_value(&config_with_flag(true), 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["rolling_token"], false,
        "a degraded profile must publish the mint it is actually on"
    );

    // Flag OFF, sidecar holding a rolling bearer: publish the bearer.
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&sidecar(
            vec!["user:inference", "user:profile"],
            Some("max"),
        ))
        .unwrap(),
    )
    .unwrap();
    let v = status_value(&config_with_flag(false), 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["rolling_token"], true,
        "what sessions actually hold outranks the flag in both directions"
    );
}

/// A mis-fill (rotating pair) publishes `rolling_token: false` even though its
/// chain-shaped scopes would scope-classify as rolling — the classifier's
/// refresh-token arm pre-empts the inference. Without it, status.json told
/// external readers "routine hours-scale maintenance" over the exact state the
/// TUI renders `[ mis-filled ]` for, on the same file, same frame.
#[test]
fn build_status_rolling_token_is_false_for_a_misfill() {
    let _home = HomeSandbox::new();
    let name = "roll-misfill";
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut p = oauth_profile(name);
    p.rolling_token = true;
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![p],
    };
    // What a mis-fill IS: a copy of credentials.json — refresh token, chain
    // scopes, plan stamp and all.
    let misfill = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-misfill".to_string(),
            refresh_token: Some("rt-misfill".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: Some("max".to_string()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&misfill).unwrap(),
    )
    .unwrap();
    let v = status_value(&config, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["rolling_token"], false,
        "a mis-fill is the state the split exists to detect, not a rolling token"
    );
}

/// The published `profiles[]` entries deserialize into [`ProfileEntry`] — the
/// typed spelling the reader (`clauth list`) derives its fields from. A field
/// the writer drops or renames reds here instead of in a reader's typed access.
#[test]
fn published_entries_deserialize_into_the_typed_contract() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work")],
    };
    config.state.active_profile = Some("work".into());
    // Warm the cache so the entry carries real window rows.
    crate::testutil::register_names(&["work"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("work"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 42.4,
                resets_at: None,
            }),
            ..Default::default()
        },
    );

    let v = status_value(&config, 300_000, None, false);
    let entries: Vec<ProfileEntry> = serde_json::from_value(v["profiles"].clone()).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.name.as_str(), "work");
    assert!(entry.active);
    assert_eq!(entry.windows.len(), 1);
    assert_eq!(entry.windows[0].label, "5h");
    assert_eq!(entry.windows[0].utilization_pct, 42.4);
    // The window row's published key set, pinned like the entry's above: both
    // sides derive from one struct, so a rename compiles clean and would
    // silently change the wire shape for every external reader.
    let mut win_keys: Vec<&str> = v["profiles"][0]["windows"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    win_keys.sort_unstable();
    assert_eq!(win_keys, ["label", "resets_at", "utilization_pct"]);
}

/// The codex half of the feed is ADDITIVE (decision 10): the top-level
/// `active_profile`/`wrap_off` stay the CLAUDE slots, the per-harness ones sit
/// beside them, and codex entries are APPENDED so a reader that predates codex
/// takes the prefix it always took.
#[test]
fn the_codex_surface_is_additive_and_appended() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = home.home().join(".clauth");
    crate::profile::mkdir_700(&dir).expect("mkdir .clauth");
    std::fs::write(
        dir.join("codex-profiles.toml"),
        "active_profile = \"cx1\"\nprofiles = [\"cx1\", \"cx2\"]\nfallback_chain = [\"cx1\", \"cx2\"]\nwrap_off = true\n",
    )
    .expect("write codex state");

    let config = crate::profile::AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["cl".into()],
            active_profile: Some("cl".into()),
            ..Default::default()
        },
        profiles: vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("cl"),
        )],
    };
    let v = serde_json::to_value(build_status(&config, 300_000, None, false))
        .expect("the status body serializes");

    assert_eq!(
        v["active_profile"], "cl",
        "the top-level slot stays CLAUDE's"
    );
    assert_eq!(v["wrap_off"], false, "…and so does the top-level wrap-off");
    assert_eq!(v["active_codex_profile"], "cx1");
    assert_eq!(v["codex_wrap_off"], true, "the codex slot carries its own");
    assert_eq!(
        v["codex_fallback_chain"].as_array().unwrap().len(),
        2,
        "the codex chain is published beside the claude one"
    );
    assert!(
        v["clauth_version"].as_str().is_some_and(|s| !s.is_empty()),
        "the writer names itself, so an old daemon is distinguishable from an empty roster"
    );

    let profiles = v["profiles"].as_array().unwrap();
    assert_eq!(profiles[0]["name"], "cl");
    assert_eq!(profiles[0]["harness"], "claude");
    assert_eq!(
        profiles.iter().filter(|p| p["harness"] == "codex").count(),
        2,
        "both codex accounts are entries, after the claude ones"
    );
    let cx1 = profiles.iter().find(|p| p["name"] == "cx1").expect("cx1");
    assert_eq!(
        cx1["active"], true,
        "the codex active marker is the codex slot's"
    );
    // One load feeds both halves: the entries' flags name exactly the profile
    // the top-level slot names, in one body.
    let flagged: Vec<&str> = profiles
        .iter()
        .filter(|p| p["harness"] == "codex" && p["active"] == true)
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(flagged, [v["active_codex_profile"].as_str().unwrap()]);
    assert_eq!(cx1["provider"], "openai");
    assert_eq!(
        cx1["rolling_token"], false,
        "a rolling sidecar is a claude mechanism; codex holds one chain in one auth.json"
    );
    assert!(
        cx1["tier"].is_null(),
        "no reading yet means no plan — never a fabricated Claude tier"
    );
}

/// The codex `tier`: the plan a poll cached is authoritative, the id_token's
/// `chatgpt_plan_type` claim stands in while no poll has answered (settled
/// question 5), and no claim plus no cache stays `null`. `auth_status` reads
/// `broken` off the quarantine record ahead of the cache-derived grades.
#[test]
fn codex_entries_fall_back_to_the_id_token_plan_and_publish_broken() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["claimed", "polled", "bare", "dead"]);
    let with_plan = |plan: &str| {
        let id_token = crate::testutil::codex_jwt(&format!(
            r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"acc","chatgpt_plan_type":"{plan}"}}}}"#
        ));
        format!(
            r#"{{"tokens":{{"id_token":"{id_token}","access_token":"at","refresh_token":"rt"}}}}"#
        )
    };
    // Captured, never polled: the claim (normalized like the live plan).
    crate::testutil::write_codex_store("claimed", &with_plan(" Plus "));
    // Polled: the cache wins over a claim that disagrees.
    crate::testutil::write_codex_store("polled", &with_plan("plus"));
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("polled"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &crate::usage::map_codex_usage(
            r#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":3,"limit_window_seconds":18000,"reset_after_seconds":3600}}}"#,
            crate::usage::now_epoch_secs(),
        )
        .expect("maps"),
    );
    // No claim, no cache.
    crate::testutil::write_codex_store(
        "bare",
        r#"{"tokens":{"access_token":"at","refresh_token":"rt"}}"#,
    );
    // A chain the server declared dead.
    crate::testutil::write_codex_store(
        "dead",
        &crate::testutil::codex_auth_body(&crate::testutil::jwt_with_exp(1_700_000_060), "rt.a"),
    );
    let invalidated = |_t: &str| -> Result<
        crate::codex_auth::CodexTokenResponse,
        crate::codex_auth::CodexRefreshError,
    > { Err(crate::codex_auth::CodexRefreshError::Dead("invalidated")) };
    assert_eq!(
        crate::codex_auth::standby_pass(
            "dead",
            1_700_000_000_000,
            "2026-08-13T00:00:00Z".into(),
            &invalidated
        ),
        crate::codex_auth::StandbyOutcome::Failed
    );

    let codex = crate::codex_profiles::CodexState::load().expect("load");
    let entries = build_codex_entries(&codex, 300_000);
    let by_name = |name: &str| {
        entries
            .iter()
            .find(|e| e.name.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is an entry"))
    };
    assert_eq!(by_name("claimed").tier.as_deref(), Some("plus"));
    assert_eq!(by_name("claimed").auth_status, "unknown");
    assert_eq!(by_name("polled").tier.as_deref(), Some("pro"));
    assert_eq!(by_name("polled").auth_status, "ok");
    assert_eq!(by_name("bare").tier, None);
    assert_eq!(by_name("dead").auth_status, "broken");
    assert_eq!(by_name("dead").tier, None);
}

/// The feed must publish the queue the ELECTION is running, not a wider one.
/// `auto_start_queue_members` drops switch-grade kick-blocked profiles, and the
/// scheduler and the TUI both supply that set — the feed used to pass an empty
/// one, so a blocked account kept a position and inflated `N` for as long as
/// the limiter's advertised ceiling stood (hours, not the "one poll" the code
/// claimed). Every OTHER member's `next_open_at` is then computed off the wrong
/// `5h / N` and reads earlier than the gap actually applied (review round 4).
///
/// Both legs, because the two surfaces read the set from different places: a
/// live daemon passes its in-memory blocks through `LiveSignals`, and the
/// daemonless `status --json` re-derives them from the same `kick_block.json`
/// caches the scheduler writes through — on the same `kick_block_switch_grade`
/// predicate, which the third profile below pins by NOT being excluded.
#[test]
fn build_status_auto_start_queue_drops_switch_grade_kick_blocked_members() {
    use crate::profile_cache::{KICK_BLOCK_CACHE_FILE, write_profile_cache};
    use crate::usage::KickBlock;
    let _home = HomeSandbox::new();
    let queued = |name: &str| {
        let mut p = oauth_profile(name);
        p.auto_start = true;
        p
    };
    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["a".into(), "b".into(), "c".into()],
            auto_start_queue: true,
            ..AppState::default()
        },
        profiles: vec![queued("a"), queued("b"), queued("c")],
    };
    let queue_of = |v: &serde_json::Value, name: &str| -> serde_json::Value {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .expect("profile published")["auto_start_queue"]
            .clone()
    };

    // Live leg: the scheduler's own blocked set, handed over.
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();
    let anchor = 1_780_000_000i64;
    let blocked = [crate::profile::ProfileName::from("b")];
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: Some(anchor),
        queue_blocked: &blocked,
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert!(
        queue_of(&v, "b").is_null(),
        "a kick-blocked member holds no published slot, as it holds none in the election"
    );
    assert_eq!(queue_of(&v, "a")["position"], 1);
    assert_eq!(
        queue_of(&v, "c")["position"],
        2,
        "the members behind it close up rather than leaving a hole"
    );
    // The N the estimate is sized from, which is the half of this that a
    // position assertion alone would miss.
    let published = queue_of(&v, "a")["next_open_at"]
        .as_str()
        .expect("an anchored queue publishes a next-open stamp")
        .to_string();
    assert_eq!(
        crate::usage::iso_to_epoch_secs(&published),
        Some(anchor + crate::usage::queue_gap_secs(2, 300_000)),
        "the gap is 5h/2, not the 5h/3 an un-excluded member would publish"
    );

    // Daemonless leg: the same verdict re-derived from disk. `c` gets a block
    // that is NOT switch-grade (one 429, no `rejected`), so the predicate is
    // pinned in both directions by the same run.
    crate::testutil::register_names(&["a", "b", "c"]);
    let far_ahead = crate::usage::now_epoch_secs() + 3600;
    write_profile_cache(
        &crate::profile::ProfileName::from("b"),
        KICK_BLOCK_CACHE_FILE,
        &KickBlock {
            streak: 2,
            rejected: true,
            until: Some(far_ahead),
            next_retry: far_ahead,
        },
    );
    write_profile_cache(
        &crate::profile::ProfileName::from("c"),
        KICK_BLOCK_CACHE_FILE,
        &KickBlock {
            streak: 1,
            rejected: false,
            until: Some(far_ahead),
            next_retry: far_ahead,
        },
    );
    let v = status_value(&config, 300_000, None, false);
    assert!(
        queue_of(&v, "b").is_null(),
        "`status --json` reads the same block off `kick_block.json`"
    );
    assert_eq!(queue_of(&v, "a")["position"], 1);
    assert_eq!(
        queue_of(&v, "c")["position"],
        2,
        "a burst 429 is not switch-grade and never costs a queue slot"
    );
}

/// Byte-parity pin for the `fallback` object: key order, `None` → `null`,
/// `Some` → the object, and a threshold with more significant digits than `f32`
/// carries so a narrowing of the field changes the bytes.
#[test]
fn fallback_object_matches_legacy_json_bytes() {
    let typed = Some(Fallback {
        position: 1,
        threshold: 92.345678901,
        armed: true,
    });
    assert_eq!(
        serde_json::to_string(&typed).unwrap(),
        r#"{"position":1,"threshold":92.345678901,"armed":true}"#,
    );

    let none: Option<Fallback> = None;
    assert_eq!(serde_json::to_string(&none).unwrap(), "null");
}

/// Byte-parity pin for the third-party availability object, `Some` and `None`.
#[test]
fn third_party_availability_matches_legacy_json_bytes() {
    let typed = Some(ThirdPartyAvailability { available: true });
    let legacy = serde_json::json!({"available": true});
    assert_eq!(
        serde_json::to_string(&typed).unwrap(),
        serde_json::to_string(&legacy).unwrap(),
    );

    let none: Option<ThirdPartyAvailability> = None;
    assert_eq!(serde_json::to_string(&none).unwrap(), "null");
}

/// Byte-parity pin for the full body including `profiles[]`: field order equals
/// the published key order, `Option` fields emit a present `null` when absent,
/// and one entry pins every optional field `Some` while another pins them
/// `None`/empty.
#[test]
fn status_body_matches_legacy_json_bytes() {
    let body = StatusBody {
        schema: 2,
        generated_at: "2026-09-13T00:00:00Z".to_string(),
        active_profile: Some("work".to_string()),
        pending_switch: Some("later".to_string()),
        wrap_off: true,
        active_codex_profile: Some("cx".to_string()),
        codex_fallback_chain: vec!["cx".into()],
        codex_wrap_off: true,
        refresh_interval_ms: 300_000,
        clauth_version: "9.9.9".to_string(),
        gateway: None,
        proxies: Vec::new(),
        profiles: vec![
            ProfileEntry {
                name: "all-some".into(),
                active: true,
                rolling_token: true,
                provider: "anthropic".to_string(),
                base_url: Some("https://api.anthropic.com".to_string()),
                tier: Some("Max 5x".to_string()),
                harness: "claude".to_string(),
                has_live_session: true,
                auth_status: "ok".to_string(),
                fetch_status: Some("Fresh".to_string()),
                stale: true,
                fetched_at: Some("2026-09-13T00:00:00Z".to_string()),
                next_refresh_at: Some("2026-09-13T00:05:00Z".to_string()),
                auto_start: true,
                auto_start_queue: Some(QueueEntry {
                    position: 1,
                    next_open_at: Some("2026-09-13T00:05:00Z".to_string()),
                }),
                bell_threshold: Some(92.345678901),
                fallback: Some(Fallback {
                    position: 1,
                    threshold: 92.345678901,
                    armed: true,
                }),
                windows: vec![
                    Window {
                        label: "5h".to_string(),
                        utilization_pct: 42.123456789,
                        resets_at: Some("2026-09-13T05:00:00Z".to_string()),
                    },
                    Window {
                        label: "7d".to_string(),
                        utilization_pct: 13.123456789,
                        resets_at: None,
                    },
                ],
                third_party: Some(ThirdPartyAvailability { available: true }),
            },
            ProfileEntry {
                name: "all-none".into(),
                active: false,
                rolling_token: false,
                provider: "anthropic".to_string(),
                base_url: None,
                tier: None,
                harness: "claude".to_string(),
                has_live_session: false,
                auth_status: "ok".to_string(),
                fetch_status: None,
                stale: false,
                fetched_at: None,
                next_refresh_at: None,
                auto_start: false,
                auto_start_queue: None,
                bell_threshold: None,
                fallback: None,
                windows: vec![],
                third_party: None,
            },
            ProfileEntry {
                name: "null-stamp".into(),
                active: false,
                rolling_token: false,
                provider: "anthropic".to_string(),
                base_url: None,
                tier: None,
                harness: "codex".to_string(),
                has_live_session: false,
                auth_status: "ok".to_string(),
                fetch_status: None,
                stale: false,
                fetched_at: None,
                next_refresh_at: None,
                auto_start: true,
                auto_start_queue: Some(QueueEntry {
                    position: 2,
                    next_open_at: None,
                }),
                bell_threshold: None,
                fallback: None,
                windows: vec![],
                third_party: None,
            },
        ],
    };
    let expected = concat!(
        r#"{"schema":2,"generated_at":"2026-09-13T00:00:00Z","active_profile":"work","#,
        r#""pending_switch":"later","wrap_off":true,"active_codex_profile":"cx","#,
        r#""codex_fallback_chain":["cx"],"codex_wrap_off":true,"refresh_interval_ms":300000,"#,
        r#""clauth_version":"9.9.9","gateway":null,"proxies":[],"profiles":["#,
        r#"{"name":"all-some","active":true,"rolling_token":true,"provider":"anthropic","#,
        r#""base_url":"https://api.anthropic.com","tier":"Max 5x","harness":"claude","has_live_session":true,"#,
        r#""auth_status":"ok","fetch_status":"Fresh","stale":true,"fetched_at":"2026-09-13T00:00:00Z","#,
        r#""next_refresh_at":"2026-09-13T00:05:00Z","auto_start":true,"#,
        r#""auto_start_queue":{"position":1,"next_open_at":"2026-09-13T00:05:00Z"},"#,
        r#""bell_threshold":92.345678901,"fallback":{"position":1,"threshold":92.345678901,"armed":true},"#,
        r#""windows":[{"label":"5h","utilization_pct":42.123456789,"resets_at":"2026-09-13T05:00:00Z"},"#,
        r#"{"label":"7d","utilization_pct":13.123456789,"resets_at":null}],"#,
        r#""third_party":{"available":true}},"#,
        r#"{"name":"all-none","active":false,"rolling_token":false,"provider":"anthropic","#,
        r#""base_url":null,"tier":null,"harness":"claude","has_live_session":false,"auth_status":"ok","#,
        r#""fetch_status":null,"stale":false,"fetched_at":null,"next_refresh_at":null,"#,
        r#""auto_start":false,"auto_start_queue":null,"bell_threshold":null,"fallback":null,"#,
        r#""windows":[],"third_party":null},"#,
        r#"{"name":"null-stamp","active":false,"rolling_token":false,"provider":"anthropic","#,
        r#""base_url":null,"tier":null,"harness":"codex","has_live_session":false,"auth_status":"ok","#,
        r#""fetch_status":null,"stale":false,"fetched_at":null,"next_refresh_at":null,"#,
        r#""auto_start":true,"auto_start_queue":{"position":2,"next_open_at":null},"#,
        r#""bell_threshold":null,"fallback":null,"windows":[],"third_party":null}]}"#,
    );
    assert_eq!(serde_json::to_string(&body).unwrap(), expected);

    let body = StatusBody {
        schema: 2,
        generated_at: "2026-09-13T00:00:00Z".to_string(),
        active_profile: None,
        pending_switch: None,
        wrap_off: false,
        active_codex_profile: None,
        codex_fallback_chain: vec![],
        codex_wrap_off: false,
        refresh_interval_ms: 60_000,
        clauth_version: "9.9.9".to_string(),
        gateway: None,
        proxies: Vec::new(),
        profiles: vec![],
    };
    let expected = concat!(
        r#"{"schema":2,"generated_at":"2026-09-13T00:00:00Z","active_profile":null,"#,
        r#""pending_switch":null,"wrap_off":false,"active_codex_profile":null,"#,
        r#""codex_fallback_chain":[],"codex_wrap_off":false,"refresh_interval_ms":60000,"#,
        r#""clauth_version":"9.9.9","gateway":null,"proxies":[],"profiles":[]}"#,
    );
    assert_eq!(serde_json::to_string(&body).unwrap(), expected);
}

/// The single-shot `next_open_at` must be the history-derived anchor's stamp,
/// not a `null` and not a live-anchor leftover. Its own test rather than the
/// canary's presence guard, so a `None => None` regression at the single-shot
/// match arm fails as a value mismatch, not a fixture message.
#[test]
fn status_body_derives_the_single_shot_queue_anchor_from_usage_history() {
    let _home = HomeSandbox::new();
    let now_secs = crate::usage::now_epoch_secs();
    let mut p = Profile::new("anchor-history".to_string(), None, None);
    p.auto_start = true;
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "anchor-history-access".to_string(),
            refresh_token: Some("anchor-history-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
            ..OAuthToken::default_extra()
        }),
    });
    save_profile(&p).unwrap();
    crate::profile::save_app_state(&AppState {
        profiles: vec![p.name.clone()],
        auto_start_queue: true,
        ..AppState::default()
    })
    .unwrap();

    // Two polls 90s apart over one unchanged 5h window, written through the
    // real history writer: the span pass confirms an open, so the anchor
    // derives from the series rather than reading as cold history.
    let window = |utilization: f64, hours: i64| crate::usage::UsageWindow {
        utilization,
        resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + hours * 3600)),
    };
    let reading = |utilization: f64| crate::usage::UsageInfo {
        five_hour: Some(window(utilization, 3)),
        ..Default::default()
    };
    let first = reading(40.0);
    crate::profile::append_usage_sample_at(&p.name, None, &first, (now_secs - 150) as u64 * 1000);
    crate::profile::append_usage_sample_at(
        &p.name,
        Some(&first),
        &reading(42.0),
        (now_secs - 60) as u64 * 1000,
    );

    let config = crate::profile::load_config().unwrap();
    let interval_ms = 300_000u64;
    let members = crate::usage::auto_start_queue_members(&config, &[]);
    let anchor = crate::usage::history_anchor(
        &config
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>(),
    );
    let expected = crate::usage::next_queue_open_secs(anchor, members.len(), interval_ms)
        .map(crate::usage::epoch_secs_to_iso);

    let body = build_status(&config, interval_ms, None, false);
    let queue = body
        .profiles
        .iter()
        .find(|e| e.name.as_str() == "anchor-history")
        .and_then(|e| e.auto_start_queue.as_ref())
        .expect("queue member publishes its slot");
    assert_eq!(
        queue.next_open_at, expected,
        "the single-shot queue stamp must be the history anchor plus the queue gap"
    );
    assert!(
        expected.is_some(),
        "history never anchored the queue (test would be vacuous)"
    );
}

/// Threat-model design defect 4: "`status.json` carries no secret" was asserted,
/// never enforced against the serialized body. Plant a unique marker in every
/// credential slot a real writer fills and write every cache the builder reads,
/// then prove no marker reaches the typed body (single-shot or live-signal), the
/// feed, or the file the writer publishes.
#[test]
fn status_body_never_leaks_a_credential() {
    let home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;

    // One marker per slot, so an assertion message names the slot a leak came
    // from instead of blaming a shared string.
    let oauth_access = "clauth-canary-oauth-access-7f3a";
    let oauth_refresh = "clauth-canary-oauth-refresh-7f3a";
    let oauth_extra = "clauth-canary-oauth-extra-7f3a";
    let api_key = "clauth-canary-api-key-7f3a";
    let console = "clauth-canary-console-7f3a";
    let env_auth = "clauth-canary-env-auth-7f3a";
    let env_second = "clauth-canary-env-second-7f3a";
    let session = "clauth-canary-session-7f3a";
    let mcp = "clauth-canary-mcp-7f3a";
    let kick_access = "clauth-canary-kick-access-7f3a";
    let kick_refresh = "clauth-canary-kick-refresh-7f3a";

    let mut oauth = Profile::new("canary-oauth".to_string(), None, None);
    oauth.auto_start = true;
    let mut oauth_token = OAuthToken {
        access_token: oauth_access.to_string(),
        refresh_token: Some(oauth_refresh.to_string()),
        expires_at: Some(now),
        scopes: None,
        subscription_type: Some("max".to_string()),
        extra: serde_json::Map::new(),
    };
    oauth_token
        .extra
        .insert("canaryExtraKey".to_string(), serde_json::json!(oauth_extra));
    oauth.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(oauth_token),
    });
    save_profile(&oauth).unwrap();

    let mut api = Profile::new(
        "canary-api".to_string(),
        Some("https://api.anthropic.com".to_string()),
        Some(api_key.to_string()),
    );
    api.console = Some(crate::profile::ConsoleCredential {
        token: console.to_string(),
        site: crate::profile::ConsoleSite::Domestic,
        region: "cn-beijing".to_string(),
    });
    api.env
        .insert("ANTHROPIC_AUTH_TOKEN".to_string(), env_auth.to_string());
    api.env
        .insert("CLAUTH_CANARY_ENV".to_string(), env_second.to_string());
    save_profile(&api).unwrap();

    // A second queue member, so the kick block below excludes a planted name
    // from the single-shot queue while `canary-oauth` keeps its slot.
    let mut kick = Profile::new("canary-kick".to_string(), None, None);
    kick.auto_start = true;
    kick.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: kick_access.to_string(),
            refresh_token: Some(kick_refresh.to_string()),
            expires_at: Some(now),
            scopes: None,
            subscription_type: Some("max".to_string()),
            ..OAuthToken::default_extra()
        }),
    });
    save_profile(&kick).unwrap();

    // The setup-token capture slot, written through its own persistence fn.
    crate::claude::write_session_token(&oauth.name, session, now).unwrap();

    // The MCP park writer persists only for a profile the on-disk record
    // carries (`write_profile_cache` checks profiles.toml), so register both
    // profiles before planting. The active profile, the fallback chain and the
    // auto-start queue put a planted name on the builder's active, chain and
    // queue arms.
    crate::profile::save_app_state(&AppState {
        active_profile: Some(oauth.name.clone()),
        profiles: vec![oauth.name.clone(), api.name.clone(), kick.name.clone()],
        fallback_chain: vec![oauth.name.clone(), api.name.clone()],
        auto_start_queue: true,
        ..AppState::default()
    })
    .unwrap();

    // The durable dead-credential verdict and a live session for the planted
    // api-key profile, so the builder's `recorded_expired` and
    // `has_live_session` arms run over a planted name (each carries its own
    // presence guard below). Both land after `save_app_state` — the record's
    // writer checks `profiles.toml`, and the session dir resolves under this
    // sandbox.
    let api_fp = crate::usage::profile_credential_fingerprint(&api).expect("api fingerprint");
    crate::profile_cache::write_auth_expired(&api.name, api_fp);
    let _api_live = crate::testutil::arm_live_session(home.home(), "canary-api");

    // The parked MCP logins slot, written through its real park writer.
    let mcp_store = crate::profile::clauth_dir()
        .unwrap()
        .join("canary-mcp-store.json");
    std::fs::write(
        &mcp_store,
        serde_json::json!({ "mcpOAuth": { "linear": { "accessToken": mcp } } }).to_string(),
    )
    .unwrap();
    crate::claude::park_mcp_logins_from_store(&oauth.name, &mcp_store);
    let parked = crate::profile_cache::load_profile_cache::<serde_json::Value>(
        &oauth.name,
        crate::profile_cache::MCP_LOGINS_FILE,
    )
    .expect("park writer landed the mcpOAuth block");

    // Every cache the builder reads, written through the crate's real writers
    // with ordinary content, so each cache-gated branch runs over a planted
    // profile. Two history polls 90s apart over one 5h window are what the
    // single-shot queue anchor derives from.
    let now_secs = crate::usage::now_epoch_secs();
    let window = |utilization: f64, hours: i64| crate::usage::UsageWindow {
        utilization,
        resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + hours * 3600)),
    };
    let reading = |utilization: f64| crate::usage::UsageInfo {
        plan: Some(crate::usage::PlanInfo {
            tier: crate::usage::PlanTier::Pro,
            subscription_status: Some("active".to_string()),
            codex_plan: None,
        }),
        five_hour: Some(window(utilization, 3)),
        seven_day: Some(window(13.0, 72)),
        fetched_at: Some((now - 60_000) as u64),
        ..Default::default()
    };
    crate::profile_cache::write_profile_cache(
        &oauth.name,
        crate::profile_cache::USAGE_CACHE_FILE,
        &reading(42.0),
    );
    let first = reading(40.0);
    crate::profile::append_usage_sample_at(
        &oauth.name,
        None,
        &first,
        (now_secs - 150) as u64 * 1000,
    );
    crate::profile::append_usage_sample_at(
        &oauth.name,
        Some(&first),
        &reading(42.0),
        (now_secs - 60) as u64 * 1000,
    );
    crate::profile_cache::write_profile_cache(
        &api.name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![crate::providers::UsageBar {
                label: crate::usage::LABEL_5H.to_string(),
                pct: 25.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + 2 * 3600)),
                used: None,
                total: None,
            }],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
    crate::profile_cache::write_profile_cache(
        &kick.name,
        crate::profile_cache::KICK_BLOCK_CACHE_FILE,
        &crate::usage::KickBlock {
            streak: 2,
            rejected: true,
            until: Some(now_secs + 3600),
            next_retry: now_secs + 3600,
        },
    );

    // Reload through the crate's own config loader, so the builder reads what the
    // loader fills from disk rather than the hand-built profiles.
    let config = crate::profile::load_config().unwrap();

    let body_none = build_status(&config, 300_000, None, true);
    let body_none_str =
        String::from_utf8_lossy(&serde_json::to_vec_pretty(&body_none).unwrap()).into_owned();

    // Live-signal path: non-empty maps for both planted names, a stuck 429
    // streak, a pending switch naming one, a queue anchor and a blocked list,
    // so a leak that fires only for the active profile or only under live
    // signals is caught.
    let oauth_p = config
        .profiles
        .iter()
        .find(|p| p.name.as_str() == "canary-oauth")
        .unwrap();
    let api_p = config
        .profiles
        .iter()
        .find(|p| p.name.as_str() == "canary-api")
        .unwrap();
    let status_map = HashMap::from([("canary-oauth".to_string(), FetchStatus::RateLimited)]);
    let third_party_map = HashMap::from([("canary-api".to_string(), FetchStatus::Cached)]);
    let next_refresh_map = HashMap::from([
        (
            FetchLeg::for_profile(oauth_p).key(oauth_p.name.clone()),
            now as u64,
        ),
        (
            FetchLeg::for_profile(api_p).key(api_p.name.clone()),
            now as u64,
        ),
    ]);
    let streaks_map = HashMap::from([("canary-oauth".to_string(), u32::MAX)]);
    let blocked: Vec<ProfileName> = vec!["canary-api".into()];
    let live = LiveSignals {
        status: &status_map,
        third_party_status: &third_party_map,
        third_party_streaks: &Default::default(),
        next_refresh: &next_refresh_map,
        streaks: &streaks_map,
        pending_switch: Some("canary-api"),
        queue_anchor: Some(now / 1000),
        queue_blocked: &blocked,
        gateway: None,
        proxies: None,
    };
    let body_live = build_status(&config, 300_000, Some(&live), true);
    let body_live_str =
        String::from_utf8_lossy(&serde_json::to_vec_pretty(&body_live).unwrap()).into_owned();

    let feed = crate::daemon::status_feed_json(&config, Some(&live), Some("2026-09-13T00:00:00Z"))
        .unwrap();
    let feed_str = String::from_utf8_lossy(&feed).into_owned();

    // And the literal file that writer publishes.
    crate::daemon::write_status_json(&feed);
    let published =
        std::fs::read(crate::profile::clauth_dir().unwrap().join("status.json")).unwrap();
    let published_str = String::from_utf8_lossy(&published).into_owned();

    // Each cache must reach its branch, or its part of the canary proves nothing.
    let entry = |body: &StatusBody, name: &str| {
        body.profiles
            .iter()
            .find(|e| e.name.as_str() == name)
            .cloned()
            .expect("planted profile published")
    };
    let oauth_entry = entry(&body_none, "canary-oauth");
    assert!(
        oauth_entry.fetched_at.is_some() && !oauth_entry.windows.is_empty(),
        "usage cache never reached the build (canary would be vacuous)"
    );
    assert_eq!(
        oauth_entry.tier,
        crate::usage::PlanTier::Pro.short_label(),
        "cached plan never reached the build (canary would be vacuous)"
    );
    assert!(
        oauth_entry
            .auto_start_queue
            .is_some_and(|q| q.next_open_at.is_some()),
        "usage history never anchored the single-shot queue (canary would be vacuous)"
    );
    let api_entry = entry(&body_none, "canary-api");
    assert!(
        api_entry.third_party.is_some() && !api_entry.windows.is_empty(),
        "third-party cache never reached the build (canary would be vacuous)"
    );
    assert_eq!(
        api_entry.fetch_status.as_deref(),
        Some("AuthExpired"),
        "recorded dead-credential verdict never reached the build (canary would be vacuous)"
    );
    assert!(
        api_entry.has_live_session,
        "live session never reached the build (canary would be vacuous)"
    );
    assert!(
        entry(&body_none, "canary-kick").auto_start_queue.is_none()
            && entry(&body_live, "canary-kick").auto_start_queue.is_some(),
        "kick block never reached the single-shot build (canary would be vacuous)"
    );

    // Each slot's marker as the reloaded state carries it: a marker missing
    // here would make its absence from the outputs prove nothing.
    let login_of = |name: &str| {
        let p = config
            .profiles
            .iter()
            .find(|p| p.name.as_str() == name)
            .unwrap();
        let login = p
            .credentials
            .as_ref()
            .and_then(|c| c.claude_ai_oauth.as_ref());
        serde_json::to_string(login.unwrap()).unwrap()
    };
    let planted = [
        login_of("canary-oauth"),
        login_of("canary-kick"),
        format!(
            "{:?} {:?} {:?}",
            api_p.api_key,
            api_p.console.as_ref().map(|c| &c.token),
            api_p.env
        ),
        crate::claude::sidecar_summary(&oauth_p.name)
            .map(|(_, t)| t.access_token)
            .unwrap_or_default(),
        parked.to_string(),
    ]
    .join("\n");

    let markers: [(&str, &str); 11] = [
        ("oauth access_token", oauth_access),
        ("oauth refresh_token", oauth_refresh),
        ("oauth extra", oauth_extra),
        ("kick oauth access_token", kick_access),
        ("kick oauth refresh_token", kick_refresh),
        ("api_key", api_key),
        ("console token", console),
        ("env ANTHROPIC_AUTH_TOKEN", env_auth),
        ("env second key", env_second),
        ("session token", session),
        ("parked mcp login", mcp),
    ];
    for (slot, marker) in markers {
        assert!(
            planted.contains(marker),
            "{slot} marker never reached its slot (canary would be vacuous)"
        );
        assert!(
            !body_none_str.contains(marker),
            "{slot} marker leaked into the single-shot body"
        );
        assert!(
            !body_live_str.contains(marker),
            "{slot} marker leaked into the live-signal body"
        );
        assert!(
            !feed_str.contains(marker),
            "{slot} marker leaked into the feed"
        );
        assert!(
            !published_str.contains(marker),
            "{slot} marker leaked into the published status.json"
        );
    }

    // The apiKeyHelper command CC runs per request, built through the crate's
    // real settings builder for a planted api-key profile, then asserted absent
    // from every output.
    let settings = crate::claude::build_claude_settings_json(None, api_p, &[]).unwrap();
    let helper = serde_json::from_str::<serde_json::Value>(&settings).unwrap()["apiKeyHelper"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !helper.is_empty(),
        "apiKeyHelper was not built (canary would be vacuous)"
    );
    // Every output is JSON, so a leaked helper appears escaped: a Windows exe
    // path carries backslashes and may carry quotes.
    let escaped = serde_json::to_string(&helper).unwrap();
    let helper = &escaped[1..escaped.len() - 1];
    assert!(
        !body_none_str.contains(helper),
        "apiKeyHelper leaked into the single-shot body"
    );
    assert!(
        !body_live_str.contains(helper),
        "apiKeyHelper leaked into the live-signal body"
    );
    assert!(
        !feed_str.contains(helper),
        "apiKeyHelper leaked into the feed"
    );
    assert!(
        !published_str.contains(helper),
        "apiKeyHelper leaked into the published status.json"
    );

    // Re-publish with the api-key profile ACTIVE: the api-key slots (api key,
    // console bearer, env) ride the active-only branch, so the oauth-active
    // bodies above prove nothing about a leak gated on `config.is_active`.
    crate::profile::save_app_state(&AppState {
        active_profile: Some(api.name.clone()),
        profiles: vec![oauth.name.clone(), api.name.clone(), kick.name.clone()],
        fallback_chain: vec![oauth.name.clone(), api.name.clone()],
        auto_start_queue: true,
        ..AppState::default()
    })
    .unwrap();
    let config_api_active = crate::profile::load_config().unwrap();
    let feed_api_active = crate::daemon::status_feed_json(
        &config_api_active,
        Some(&live),
        Some("2026-09-13T00:00:00Z"),
    )
    .unwrap();
    crate::daemon::write_status_json(&feed_api_active);
    let published_api_active =
        std::fs::read(crate::profile::clauth_dir().unwrap().join("status.json")).unwrap();
    let api_active_surfaces = [
        (
            "api-active single-shot body",
            String::from_utf8_lossy(
                &serde_json::to_vec_pretty(&build_status(&config_api_active, 300_000, None, true))
                    .unwrap(),
            )
            .into_owned(),
        ),
        (
            "api-active live-signal body",
            String::from_utf8_lossy(
                &serde_json::to_vec_pretty(&build_status(
                    &config_api_active,
                    300_000,
                    Some(&live),
                    true,
                ))
                .unwrap(),
            )
            .into_owned(),
        ),
        (
            "api-active feed",
            String::from_utf8_lossy(&feed_api_active).into_owned(),
        ),
        (
            "api-active published status.json",
            String::from_utf8_lossy(&published_api_active).into_owned(),
        ),
    ];
    for (slot, marker) in markers {
        for (surface, text) in &api_active_surfaces {
            assert!(
                !text.contains(marker),
                "{slot} marker leaked into the {surface}"
            );
        }
    }
}

/// The `ToSchema`-derived schema and a real `build_status` body agree key for
/// key: every schema property is a present, required body key, every body key
/// is a schema property, and the walk reaches every nested type through the
/// registered components.
#[test]
fn status_schema_agrees_with_the_serialized_body() {
    let _home = HomeSandbox::new();
    let now_secs = crate::usage::now_epoch_secs();

    // One OAuth profile in the fallback chain and the auto-start queue, a
    // second chain member, and a third-party profile, with caches written
    // through the crate's real writers so `windows` is non-empty.
    let mut oauth = oauth_profile("schema-oauth");
    oauth.auto_start = true;
    save_profile(&oauth).unwrap();

    let mut chain = oauth_profile("schema-chain");
    chain.auto_start = true;
    save_profile(&chain).unwrap();

    let mut api = Profile::new(
        "schema-api".to_string(),
        Some("https://api.anthropic.com".to_string()),
        Some("schema-api-key".to_string()),
    );
    api.auto_start = true;
    save_profile(&api).unwrap();

    crate::profile::save_app_state(&AppState {
        active_profile: Some(oauth.name.clone()),
        profiles: vec![oauth.name.clone(), chain.name.clone(), api.name.clone()],
        fallback_chain: vec![oauth.name.clone(), chain.name.clone()],
        auto_start_queue: true,
        ..AppState::default()
    })
    .unwrap();

    let window = |utilization: f64, hours: i64| crate::usage::UsageWindow {
        utilization,
        resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + hours * 3600)),
    };
    let oauth_reading = crate::usage::UsageInfo {
        plan: Some(crate::usage::PlanInfo {
            tier: crate::usage::PlanTier::Pro,
            subscription_status: Some("active".to_string()),
            codex_plan: None,
        }),
        five_hour: Some(window(42.0, 3)),
        seven_day: Some(window(13.0, 72)),
        fetched_at: Some(crate::usage::now_ms() - 60_000),
        ..Default::default()
    };
    crate::profile_cache::write_profile_cache(
        &oauth.name,
        crate::profile_cache::USAGE_CACHE_FILE,
        &oauth_reading,
    );
    crate::profile_cache::write_profile_cache(
        &chain.name,
        crate::profile_cache::USAGE_CACHE_FILE,
        &oauth_reading,
    );
    crate::profile_cache::write_profile_cache(
        &api.name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![crate::providers::UsageBar {
                label: crate::usage::LABEL_5H.to_string(),
                pct: 25.0,
                resets_at: Some(crate::usage::epoch_secs_to_iso(now_secs + 2 * 3600)),
                used: None,
                total: None,
            }],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );

    let config = crate::profile::load_config().unwrap();
    let body = build_status(&config, 300_000, None, true);

    // The fixture must exercise every nested schema, or the walk proves
    // nothing about it.
    assert!(
        body.profiles.iter().any(|e| e.fallback.is_some()),
        "fixture never populates a fallback object"
    );
    assert!(
        body.profiles.iter().any(|e| e.auto_start_queue.is_some()),
        "fixture never populates a queue entry"
    );
    assert!(
        body.profiles.iter().any(|e| e.third_party.is_some()),
        "fixture never populates a third-party object"
    );
    assert!(
        body.profiles.iter().any(|e| !e.windows.is_empty()),
        "fixture never populates a window"
    );

    let value = serde_json::to_value(&body).unwrap();
    schema_agrees_with_type::<StatusBody>(&value);
}

/// The always-serialized `Option` fields in the answer bodies are required in
/// their schemas (`SwitchOk.previous`, `PaneEntry.title`/`agent`/`tag`/
/// `foreground_process_group_id`/`cwd`/`agent_session_id`, `PaneSession.cwd`,
/// `SessionsBody.next_before`, `SessionRow.last_ran_profile`/`first_message`/
/// `last_message` and `HistoryBody.next_before` answer `null`, never a dropped
/// key), and the skip-when-absent `Option`s (`ErrorBody.reason`,
/// `HerdrState.reason`) stay optional.
#[test]
fn always_serialized_option_fields_are_required_and_skipped_ones_are_not() {
    use crate::daemon::api::panes::{HerdrState, PaneEntry, PaneSession};
    use crate::daemon::api::routes::{ErrorBody, SwitchOk};
    use crate::daemon::api::sessions::{HistoryBody, SessionRow, SessionsBody};

    let no_components: BTreeMap<String, RefOr<Schema>> = BTreeMap::new();

    let required = |schema: &RefOr<Schema>, name: &str| -> Vec<String> {
        match schema_deref(schema, &no_components) {
            Schema::Object(object) => object.required.clone(),
            _ => panic!("{name} must be an object"),
        }
    };

    let switch_ok_required = required(&SwitchOk::schema(), "SwitchOk");
    assert!(
        switch_ok_required.iter().any(|name| name == "previous"),
        "SwitchOk.previous is serialized on every answer, so its schema requires it"
    );

    let pane_entry_required = required(&PaneEntry::schema(), "PaneEntry");
    for field in [
        "title",
        "agent",
        "tag",
        "foreground_process_group_id",
        "cwd",
        "agent_session_id",
    ] {
        assert!(
            pane_entry_required.iter().any(|name| name == field),
            "PaneEntry.{field} is serialized on every answer, so its schema requires it"
        );
    }

    let pane_session_required = required(&PaneSession::schema(), "PaneSession");
    assert!(
        pane_session_required.iter().any(|name| name == "cwd"),
        "PaneSession.cwd is serialized on every answer, so its schema requires it"
    );

    let sessions_body_required = required(&SessionsBody::schema(), "SessionsBody");
    assert!(
        sessions_body_required
            .iter()
            .any(|name| name == "next_before"),
        "SessionsBody.next_before is serialized on every answer, so its schema requires it"
    );
    let session_row_required = required(&SessionRow::schema(), "SessionRow");
    for field in ["last_ran_profile", "first_message", "last_message"] {
        assert!(
            session_row_required.iter().any(|name| name == field),
            "SessionRow.{field} is serialized on every answer, so its schema requires it"
        );
    }
    let history_body_required = required(&HistoryBody::schema(), "HistoryBody");
    assert!(
        history_body_required
            .iter()
            .any(|name| name == "next_before"),
        "HistoryBody.next_before is serialized on every answer, so its schema requires it"
    );

    let herdr_state_required = required(&HerdrState::schema(), "HerdrState");
    assert!(
        !herdr_state_required.iter().any(|name| name == "reason"),
        "HerdrState.reason is skipped when absent, so its schema leaves it optional"
    );

    let error_body_required = required(&ErrorBody::schema(), "ErrorBody");
    assert!(
        !error_body_required.iter().any(|name| name == "reason"),
        "ErrorBody.reason is skipped when absent, so its schema leaves it optional"
    );
}

/// The health, switch, pair, error and panes bodies' `ToSchema`-derived
/// schemas agree with their wire shapes: each required property present, each
/// body key a schema property, required-ness matching presence (the sessions,
/// history and agent bodies are walked in their own modules and the
/// router-wide pin). Request bodies are pinned by their literal JSON because
/// they only deserialize.
#[test]
fn every_rest_body_schema_agrees_with_its_wire_shape() {
    use crate::daemon::api::panes::PanesBody;
    use crate::daemon::api::routes::{
        ErrorBody, HealthBody, PairBody, PairOk, SwitchBody, SwitchOk,
    };

    schema_agrees_with_type::<HealthBody>(&serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "schema": crate::daemon::SCHEMA_VERSION,
    }));

    schema_agrees_with_type::<SwitchOk>(&serde_json::json!({
        "ok": true,
        "previous": "alpha",
        "active": "beta",
    }));
    schema_agrees_with_type::<SwitchOk>(&serde_json::json!({
        "ok": true,
        "previous": null,
        "active": "beta",
    }));

    schema_agrees_with_type::<PairOk>(&serde_json::json!({
        "ok": true,
        "name": "phone",
        "tier": "control",
        "token": "tok_0123456789",
    }));

    schema_agrees_with_type::<ErrorBody>(&serde_json::json!({
        "ok": false,
        "error": "bad_request",
        "reason": "the fix",
    }));
    schema_agrees_with_type::<ErrorBody>(&serde_json::json!({
        "ok": false,
        "error": "bad_request",
    }));

    schema_agrees_with_type::<PanesBody>(&serde_json::json!({
        "ok": true,
        "herdr": {"present": false, "reason": "herdr is not installed on this host"},
        "panes": [],
    }));
    schema_agrees_with_type::<PanesBody>(&serde_json::json!({
        "ok": true,
        "herdr": {"present": true},
        "panes": [{
            "pane_id": "w0:pK",
            "workspace_id": "w0",
            "tab_id": "w0:tH",
            "title": null,
            "agent": null,
            "agent_status": "unknown",
            "cwd": null,
            "focused": false,
            "tag": null,
            "foreground_process_group_id": null,
            "sessions": [{
                "session_id": "1128637-0",
                "profile": "DS5",
                "kind": "session",
                "follows_chain": false,
                "isolated": false,
                "cwd": null,
            }],
            "agent_session_id": null,
        }],
    }));

    schema_agrees_with_type::<SwitchBody>(&serde_json::json!({"profile": "alpha"}));
    schema_agrees_with_type::<PairBody>(&serde_json::json!({"code": "01234567"}));
}

/// Ruling 1 (display parity): `stale`'s stuck arm is OAuth-first with a
/// third-party fallback. A pure third-party member (no OAuth status entry) with
/// a `RateLimited` reading and a provider-429 streak past the cap publishes
/// `stale: true`; at exactly the cap it stays false; a hybrid follows its OAuth
/// reading (a `Cached` OAuth entry keeps it false however deep the third-party
/// streak is).
#[test]
fn build_status_stale_reads_the_third_party_streak_for_a_member_without_oauth() {
    use crate::usage::FetchStatus;
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    let mut api = Profile::new("zai".to_string(), None, None);
    api.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    api.api_key = Some("k".to_string());
    api.provider = crate::providers::Provider::from_base_url(api.base_url.as_deref().unwrap());
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![api],
    };
    let stale_of = |name: &str, v: &serde_json::Value| -> serde_json::Value {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap()["stale"]
            .clone()
    };
    let next: HashMap<crate::usage::LegKey, u64> = HashMap::new();
    let deep = crate::usage::ACTIVE_CAP_MAX_STREAK + 1;

    // Pure third-party, deep streak, no OAuth status entry → stale.
    let tp_status = HashMap::from([("zai".to_string(), FetchStatus::RateLimited)]);
    let tp_streaks = HashMap::from([("zai".to_string(), deep)]);
    let empty_status = HashMap::new();
    let empty_streaks = HashMap::new();
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &tp_status,
        third_party_streaks: &tp_streaks,
        next_refresh: &next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("zai", &v),
        true,
        "a deep provider-429 streak must publish stale for a pure third-party member"
    );

    // Exactly the cap → not yet distrusted.
    let tp_streaks = HashMap::from([("zai".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK)]);
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &tp_status,
        third_party_streaks: &tp_streaks,
        next_refresh: &next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("zai", &v),
        false,
        "a shallow provider-429 streak is not stale for a pure third-party member"
    );

    // Hybrid: the OAuth reading stays authoritative — a `Cached` OAuth entry
    // keeps it false even with a deep third-party streak.
    let oauth_status = HashMap::from([("zai".to_string(), FetchStatus::Cached)]);
    let tp_streaks = HashMap::from([("zai".to_string(), deep)]);
    let live = LiveSignals {
        status: &oauth_status,
        third_party_status: &tp_status,
        third_party_streaks: &tp_streaks,
        next_refresh: &next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: None,
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("zai", &v),
        false,
        "a hybrid's OAuth Cached reading outranks its third-party stuck reading"
    );
}

/// The `proxies` array is additive: one record-only entry per registry row in
/// service order, the supervisor's slot where one is published. A row whose
/// recorded binary is gone reads `binary_missing` naming the recorded path.
#[test]
fn the_proxies_array_publishes_one_record_only_entry_per_row() {
    let _home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    std::fs::create_dir_all(&dir).expect("dir");
    // An absolute path to a binary that does not exist, derived from the
    // sandbox dir and written with forward slashes, so it is absolute on every
    // platform and the hand-written TOML stays free of `\` escapes.
    let gone = dir
        .join("gone")
        .join("clauth-zcode-proxy")
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::write(
        dir.join("proxies.toml"),
        format!(
            "[zcode]\nport = 9101\nenabled = true\nbinary = \"{gone}\"\n\n[qwen]\nport = 9102\nenabled = false\n"
        ),
    )
    .expect("registry");
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let v = status_value(&config, 300_000, None, false);
    let proxies = v["proxies"].as_array().expect("a proxies array");
    assert_eq!(proxies.len(), 2, "one entry per row, in service order");
    assert_eq!(proxies[0]["service"], "qwen");
    assert_eq!(proxies[0]["state"], "disabled");
    assert_eq!(proxies[0]["port"], 9102);
    assert_eq!(proxies[1]["service"], "zcode");
    assert_eq!(proxies[1]["state"], "binary_missing");
    assert_eq!(proxies[1]["port"], 9101);
    assert_eq!(proxies[1]["binary"], gone);
}

/// A live supervisor slot wins over the record-only verdict for its row, and
/// a sibling row without one reads its record-only verdict.
#[test]
fn the_proxies_array_merges_a_live_slot_over_the_record() {
    use crate::daemon::proxies::{ProxySlot, ProxyState};
    let _home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("proxies.toml"),
        "[zcode]\nport = 9101\nenabled = true\n\n[qwen]\nport = 9102\nenabled = false\n",
    )
    .expect("registry");
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let slot = ProxySlot {
        service: "zcode".to_string(),
        state: ProxyState::Healthy,
        binary: Some("/usr/local/bin/clauth-zcode-proxy".to_string()),
        port: Some(9101),
        pid: Some(4242),
        version: Some("1.2.0".to_string()),
        contract: Some("1.0".to_string()),
        answerer: None,
        restarts: 0,
        last_exit: None,
        reason: None,
        since: Some("2026-09-29T00:00:00Z".to_string()),
    };
    let proxies = [slot];
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();
    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        third_party_streaks: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
        queue_anchor: None,
        queue_blocked: &[],
        gateway: None,
        proxies: Some(&proxies),
    };
    let v = status_value(&config, 300_000, Some(&live), false);
    let proxies = v["proxies"].as_array().expect("a proxies array");
    assert_eq!(proxies.len(), 2, "one entry per row, in service order");
    assert_eq!(proxies[0]["service"], "qwen");
    assert_eq!(proxies[0]["state"], "disabled", "qwen has no live slot");
    assert_eq!(proxies[1]["service"], "zcode");
    assert_eq!(proxies[1]["state"], "healthy", "the live slot wins");
    assert_eq!(proxies[1]["version"], "1.2.0");
    assert_eq!(proxies[1]["contract"], "1.0");
    assert_eq!(proxies[1]["pid"], 4242);
}
