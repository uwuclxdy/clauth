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
            "generated_at",
            "pending_switch",
            "profiles",
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
    config.set_auth_broken(&crate::profile::ProfileName::from("broken"), true);

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
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();

    // single-shot (no daemon) → pending_switch is present-but-null.
    let none = build_status(&config, 300_000, None, false);
    assert!(
        none.get("pending_switch").is_some(),
        "pending_switch key is always present"
    );
    assert!(none["pending_switch"].is_null());

    let live = LiveSignals {
        status: &empty_status,
        third_party_status: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: Some("home"),
    };
    let v = build_status(&config, 300_000, Some(&live), false);
    assert_eq!(v["pending_switch"], "home");
    assert_eq!(
        v["schema"], SCHEMA_VERSION,
        "pending_switch is part of schema 1 — no bump"
    );
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
        third_party_status: &Default::default(),
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
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
        third_party_status: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
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
        third_party_status: &Default::default(),
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
    assert_eq!(
        stale_of("work", &v),
        false,
        "a shallow RateLimited reading is not stale"
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
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
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
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
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
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live), false);
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
    let v = build_status(&dead, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["fetch_status"], "Fresh",
        "precondition: the mtime derivation alone calls a warm cache Fresh",
    );

    // Record the verdict against the credential the profile holds.
    let fp = crate::usage::profile_credential_fingerprint(&dead.profiles[0])
        .expect("a console-credentialed profile has a fingerprint");
    crate::profile_cache::write_auth_expired(&crate::profile::ProfileName::from("qwen"), fp);

    let v = build_status(&dead, 300_000, None, false);
    assert_eq!(
        v["profiles"][0]["fetch_status"], "AuthExpired",
        "no daemon, warm cache, dead session — must not read as a live measurement",
    );

    // A re-login changes the credential, so the record stops applying on its
    // own. THIS is what makes persisting it safe.
    let relogged = config_of(profile("fresh-token"));
    let v = build_status(&relogged, 300_000, None, false);
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
    let v = build_status(&config, 300_000, None, false);
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
    let v = build_status(&config_with_flag(true), 300_000, None, false);
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
    let v = build_status(&config_with_flag(false), 300_000, None, false);
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
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec_pretty(&misfill).unwrap(),
    )
    .unwrap();
    let v = build_status(&config, 300_000, None, false);
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

    let v = build_status(&config, 300_000, None, false);
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
    let v = build_status(&config, 300_000, None, false);

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
