#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `daemon::status_json::build_status` shape + field derivation.
//!
//! These exercise the single-shot path (`live = None`, freshness/next-refresh
//! from cache mtime) against a `HomeSandbox` so no real `~/.clauth` is touched.

use super::*;
use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, save_profile};
use crate::testutil::HomeSandbox;

fn oauth_profile(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
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

    let v = build_status(&config, config.state.refresh_interval_ms, None, false);

    assert_eq!(v["schema"], SCHEMA_VERSION);
    assert_eq!(v["active_profile"], "work");
    assert_eq!(v["wrap_off"], false);
    assert_eq!(
        v["weekly_switch_threshold"], 98.0,
        "unset state publishes the default weekly line"
    );
    assert_eq!(v["burn_aware"], false);
    // Additive forecast object is always present; no chain here → "none".
    assert_eq!(v["forecast"]["action"], "none");
    assert!(v["forecast"]["to"].is_null());
    assert_eq!(v["refresh_interval_ms"], 300_000);
    assert!(v["generated_at"].as_str().unwrap().contains('T'));
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
            "account_email",
            "active",
            "auth_status",
            "auto_start",
            "base_url",
            "bell_threshold",
            "codex_rate_limit_reached",
            "codex_snapshot_at",
            "fallback",
            "fetch_status",
            "fetched_at",
            "harness",
            "has_live_session",
            "name",
            "next_refresh_at",
            "provider",
            "session_feed",
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
    // Additive account_email (schema stays 1): null until the identity
    // anchor's email half is cached, then the cached value verbatim.
    assert!(work["account_email"].is_null());
    crate::profile_cache::write_profile_cache(
        "work",
        crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE,
        &"work@example.com".to_string(),
    );
    let v = build_status(&config, config.state.refresh_interval_ms, None, false);
    let work = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "work")
        .unwrap()
        .clone();
    assert_eq!(work["account_email"], "work@example.com");
    let home = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "home")
        .unwrap()
        .clone();
    assert_eq!(home["active"], false);
    // OAuth-only gate: an API profile (OAuth→API conversion keeps the cached
    // anchor) must read null, matching the TUI's is_api gate.
    crate::profile_cache::write_profile_cache(
        "home",
        crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE,
        &"home@example.com".to_string(),
    );
    config.profiles[1].base_url = Some("https://api.example.com".to_string());
    let v = build_status(&config, config.state.refresh_interval_ms, None, false);
    let home = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "home")
        .unwrap()
        .clone();
    assert!(
        home["account_email"].is_null(),
        "an API profile never surfaces the stale OAuth email"
    );
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

    let v = build_status(&config, 300_000, None, false);
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

    // Top-level ordered chain mirrors the per-profile positions.
    assert_eq!(v["fallback_chain"], serde_json::json!(["a", "b"]));
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
fn build_status_auth_status_ok_expiring_broken() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;

    let mut ok = oauth_profile("ok");
    set_expiry(&mut ok, now + 3_600_000); // real life left → ok
    let mut expiring = oauth_profile("expiring");
    set_expiry(&mut expiring, now - 1_000); // past due, not flagged → expiring
    let mut broken = oauth_profile("broken");
    set_expiry(&mut broken, now - 1_000); // past due AND flagged → broken wins

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![ok, expiring, broken],
    };
    config.set_auth_broken("broken", true);

    let v = build_status(&config, 300_000, None, false);
    let profiles = v["profiles"].as_array().unwrap();
    let get = |n: &str| profiles.iter().find(|p| p["name"] == n).unwrap();
    assert_eq!(get("ok")["auth_status"], "ok");
    assert_eq!(get("expiring")["auth_status"], "expiring");
    assert_eq!(
        get("broken")["auth_status"],
        "broken",
        "broken outranks expiring"
    );
}

/// `auth_status` reports on the credential a profile STORES, not on where its
/// requests route: a hybrid (OAuth pair + `base_url`) with a dead access token
/// must publish `expiring`, while an endpoint-only profile has no token to expire.
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

    let v = build_status(&config, 300_000, None, false);
    let profiles = v["profiles"].as_array().unwrap();
    let get = |n: &str| profiles.iter().find(|p| p["name"] == n).unwrap();
    assert_eq!(
        get("hybrid")["auth_status"],
        "expiring",
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

    // single-shot (no daemon) → pending_switch and last_error are present-but-null.
    let none = build_status(&config, 300_000, None, false);
    assert!(none["pending_switch"].is_null());
    assert!(
        none.get("last_error").is_some(),
        "last_error key is always present"
    );
    assert!(
        none["last_error"].is_null(),
        "single-shot has no drain history"
    );

    // live daemon with an accepted-not-yet-applied switch → the target name, plus a
    // recorded drain skip reason (TECH-6, additive — schema stays 1).
    let status: std::collections::HashMap<String, crate::usage::FetchStatus> =
        std::collections::HashMap::new();
    let next: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let streaks: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let last_switch = crate::daemon::LastSwitch {
        from: Some("home".to_string()),
        to: Some("work".to_string()),
        at_ms: 1_700_000_000_000,
        trigger: "user",
    };
    let live = LiveSignals {
        status: &status,
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: Some("work"),
        last_error: Some((
            1_700_000_000_000,
            "deferring switch to 'work': target is mid-fetch",
        )),
        last_switch: Some(&last_switch),
    };
    let v = build_status(&config, 300_000, Some(&live), false);
    assert_eq!(v["pending_switch"], "work");
    assert_eq!(
        v["schema"], 1,
        "last_error/last_switch/version are additive — schema must not bump"
    );
    // TECH-8: version always present; last_switch reflects the hero event.
    assert_eq!(v["clauth_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["last_switch"]["from"], "home");
    assert_eq!(v["last_switch"]["to"], "work");
    assert_eq!(v["last_switch"]["trigger"], "user");
    assert!(
        none["clauth_version"].is_string(),
        "version present in single-shot too"
    );
    assert!(
        none["last_switch"].is_null(),
        "single-shot has no switch history"
    );
    assert_eq!(
        v["last_error"]["message"],
        "deferring switch to 'work': target is mid-fetch"
    );
    assert!(
        v["last_error"]["at"].as_str().unwrap().contains('T'),
        "last_error.at is an ISO-8601 instant"
    );
}

// The daemon's published forecast is the same `fallback::next_target` walk the
// switch decision runs — the single source of truth for "would switch to X"
// (a client-side mirror of the walk is what drifted when upstream changed the
// walk semantics; readers should render THIS instead).
#[test]
fn build_status_forecast_publishes_next_target_and_last_resort() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), oauth_profile("home")],
    };
    config.state.active_profile = Some("work".into());
    config.state.profiles = vec!["work".into(), "home".into()];
    config.state.fallback_chain = vec!["work".into(), "home".into()];
    config.profiles[1].last_resort = true;

    let v = build_status(&config, 300_000, None, false);

    // `home` has no usage cache → headroom → it is the walk's pick.
    assert_eq!(v["forecast"]["action"], "switch");
    assert_eq!(v["forecast"]["to"], "home");

    // The exclusive last-resort mark rides the per-profile fallback object.
    let profiles = v["profiles"].as_array().unwrap();
    let home = profiles.iter().find(|p| p["name"] == "home").unwrap();
    assert_eq!(home["fallback"]["last_resort"], true);
    let work = profiles.iter().find(|p| p["name"] == "work").unwrap();
    assert_eq!(work["fallback"]["last_resort"], false);
}

/// The forecast walk must see usage the DAEMON way — hydrated from the
/// per-profile disk caches — because `Profile.usage` is only ever populated by
/// the TUI thread. Regression: an un-hydrated walk read universal headroom and
/// forecast a weekly-dead (7d=100) member as the next switch target.
#[test]
fn forecast_hydrates_usage_from_disk_and_skips_a_weekly_dead_member() {
    use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
    let _home = HomeSandbox::new();

    let win = |util: f64| {
        Some(UsageWindow {
            utilization: util,
            resets_at: Some(epoch_secs_to_iso(now_epoch_secs() + 3600)),
        })
    };
    // active a: 5h exhausted · b: weekly-dead (7d=100, no 5h) · c: fresh.
    let caches = [
        (
            "a",
            UsageInfo {
                five_hour: win(97.0),
                ..UsageInfo::default()
            },
        ),
        (
            "b",
            UsageInfo {
                seven_day: win(100.0),
                ..UsageInfo::default()
            },
        ),
        (
            "c",
            UsageInfo {
                five_hour: win(10.0),
                ..UsageInfo::default()
            },
        ),
    ];
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: caches
            .iter()
            .map(|(n, _)| {
                let mut p = oauth_profile(n);
                save_profile(&p).expect("save profile");
                p.fallback_threshold = Some(95.0);
                p
            })
            .collect(),
    };
    for (name, info) in &caches {
        crate::profile_cache::write_profile_cache(
            name,
            crate::profile_cache::USAGE_CACHE_FILE,
            info,
        );
    }
    config.state.active_profile = Some("a".into());
    config.state.profiles = vec!["a".into(), "b".into(), "c".into()];
    config.state.fallback_chain = vec!["a".into(), "b".into(), "c".into()];

    // Note: none of the in-memory profiles carry `usage` — exactly the daemon's
    // shape. The forecast must still route around b to c.
    assert!(config.profiles.iter().all(|p| p.usage.is_none()));
    let forecast = super::forecast_json(&config);
    assert_eq!(forecast["action"], "switch");
    assert_eq!(forecast["to"], "c");
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
    crate::profile_cache::write_profile_cache(
        "maxed",
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
    let off = build_status(&config(false), 300_000, None, false);
    let p = &off["profiles"].as_array().unwrap()[0];
    assert!(
        p["next_refresh_at"].is_null(),
        "a spent skipped account has no pending refresh: {p}"
    );

    // Toggle ON (default) → still polled → derived countdown present.
    let on = build_status(&config(true), 300_000, None, false);
    let p = &on["profiles"].as_array().unwrap()[0];
    assert!(
        !p["next_refresh_at"].is_null(),
        "polling a spent account still schedules a refresh: {p}"
    );
}

// RLS-1: the additive per-profile `stale` flag = the daemon distrusts this
// reading as a deep-slot stuck RateLimited (live status RateLimited AND the 429
// streak past the active cap) — the SAME predicate `scan_auto_switch` acts on,
// so the published cue and the switch decision cannot drift. Additive: schema
// stays 1; the single-shot (no streaks) is always false.
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
    let next: HashMap<String, u64> = HashMap::new();
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
    let none = build_status(&config, 300_000, None, false);
    assert_eq!(
        none["schema"], 1,
        "stale is additive — schema must not bump"
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
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        last_error: None,
        last_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
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
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
        last_error: None,
        last_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("work", &v),
        false,
        "a shallow RateLimited reading is not stale"
    );
}

// ---- CDX-1 T7: codex fields (all additive — schema stays 1) ----

// A mixed claude+codex config publishes per-profile harness, per-slot active
// truth, codex identity from the stored JWTs, the pinned codex_snapshot_at
// contract, and the top-level active_codex_profile — while every claude field
// keeps its exact prior meaning.
#[test]
fn build_status_publishes_codex_fields() {
    let _home = HomeSandbox::new();

    let id_token = crate::testutil::fake_jwt(&serde_json::json!({
        "email": "cdx@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": "acct-cdx",
        },
    }));
    let bytes = serde_json::json!({
        "tokens": {
            "id_token": id_token,
            "access_token": "at-cdx",
            "refresh_token": "rt-cdx",
            "account_id": "acct-cdx",
        },
    })
    .to_string()
    .into_bytes();

    let mut cdx = Profile::new("cdx-a".to_string(), None, None);
    cdx.harness = crate::profile::Harness::Codex;
    save_profile(&cdx).unwrap();
    crate::codex::write_profile_auth("cdx-a", &bytes).unwrap();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), cdx],
    };
    config.state.active_profile = Some("work".into());
    config.state.active_codex_profile = Some("cdx-a".into());

    let v = build_status(&config, 300_000, None, false);
    assert_eq!(v["active_profile"], "work");
    assert_eq!(v["active_codex_profile"], "cdx-a");

    let profiles = v["profiles"].as_array().unwrap();
    let by_name = |n: &str| {
        profiles
            .iter()
            .find(|p| p["name"] == n)
            .unwrap_or_else(|| panic!("profile {n} missing"))
    };

    let work = by_name("work");
    assert_eq!(work["harness"], "claude");
    assert_eq!(
        work["active"], true,
        "claude slot truth for claude profiles"
    );
    assert!(work["codex_snapshot_at"].is_null());

    let cdx = by_name("cdx-a");
    assert_eq!(cdx["harness"], "codex");
    assert_eq!(cdx["active"], true, "codex slot truth for codex profiles");
    assert_eq!(cdx["account_email"], "cdx@example.com");
    assert_eq!(cdx["tier"], "pro");
    assert_eq!(cdx["auth_status"], "ok");
    assert!(
        cdx["codex_snapshot_at"].as_str().unwrap().contains('T'),
        "snapshot stamp is ISO 8601"
    );
}

// The two active slots are independent in the published truth: a codex switch
// must never flip a claude profile's `active` and vice versa.
#[test]
fn build_status_keeps_the_two_active_slots_independent() {
    let _home = HomeSandbox::new();
    let mut cdx = Profile::new("cdx-a".to_string(), None, None);
    cdx.harness = crate::profile::Harness::Codex;
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), cdx],
    };
    // Only the codex slot is set: the claude profile must NOT report active.
    config.state.active_codex_profile = Some("cdx-a".into());

    let v = build_status(&config, 300_000, None, false);
    let profiles = v["profiles"].as_array().unwrap();
    assert_eq!(profiles[0]["name"], "work");
    assert_eq!(profiles[0]["active"], false);
    assert_eq!(profiles[1]["name"], "cdx-a");
    assert_eq!(profiles[1]["active"], true);
    assert!(v["active_profile"].is_null());
}

// The codex auth_status arms beyond "ok": a stored access token past its JWT
// exp reports "expiring"; a quarantined profile reports "broken" (and broken
// outranks expiring) — the same value set + precedence as the claude leg.
#[test]
fn build_status_codex_auth_status_expiring_and_broken() {
    let _home = HomeSandbox::new();
    let expired_jwt = crate::testutil::fake_jwt(&serde_json::json!({ "exp": 1_000_000 }));
    let bytes = serde_json::json!({
        "tokens": {
            "access_token": expired_jwt,
            "refresh_token": "rt-old",
            "account_id": "acct-old",
        },
    })
    .to_string()
    .into_bytes();

    let mut cdx = Profile::new("cdx-a".to_string(), None, None);
    cdx.harness = crate::profile::Harness::Codex;
    save_profile(&cdx).unwrap();
    crate::codex::write_profile_auth("cdx-a", &bytes).unwrap();

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![cdx],
    };

    let v = build_status(&config, 300_000, None, false);
    assert_eq!(v["profiles"][0]["auth_status"], "expiring");

    config.set_auth_broken("cdx-a", true);
    let v = build_status(&config, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["auth_status"], "broken",
        "broken outranks expiring"
    );
}

// ── Upstream v0.13 tests (daemon singleton modes, disable, #58 final) ──────

#[test]
fn build_status_hides_disabled_by_default_and_shows_with_include_disabled() {
    let _home = HomeSandbox::new();
    let mut off = oauth_profile("off");
    off.disabled = true;
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("on"), off],
    };

    let hidden = build_status(&config, 300_000, None, false);
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

    let shown = build_status(&config, 300_000, None, true);
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
// a reader following wiki/daemon.md's contract (resolve `active_profile`
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

    let v = build_status(&config, 300_000, None, false);
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
    crate::profile_cache::write_profile_cache(
        "zai",
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
    let v = build_status(&config, 300_000, None, false);
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
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
        last_error: None,
        last_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
    let p = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(
        p["fetch_status"], "Fresh",
        "a live daemon must not blank an api-key profile's freshness"
    );
    assert!(!p["next_refresh_at"].is_null());
}
