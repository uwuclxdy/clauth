#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::BTreeMap;

use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName};

fn oauth_profile(name: &str, refresh: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: format!("at-{name}"),
                refresh_token: Some(refresh.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn endpoint_profile(name: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: Some("https://example.test".to_string()),
        api_key: Some("sk-x".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn blank_profile(name: &str) -> Profile {
    Profile {
        harness: Default::default(),
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        session_feed: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn live_oauth(refresh: Option<&str>) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-live".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            ..Default::default()
        },
        profiles,
    }
}

#[test]
fn matches_profile_by_refresh_token() {
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("personal", "rt-personal"),
        ],
        Some("work"),
    );
    assert_eq!(
        match_by_refresh_token(&config, "rt-personal"),
        Some("personal")
    );
}

#[test]
fn returns_none_when_no_profile_holds_token() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    assert_eq!(match_by_refresh_token(&config, "rt-stranger"), None);
}

#[test]
fn ties_break_on_active_profile() {
    // degenerate: duplicate profile dir gives two profiles the same token; active wins
    let config = config_with(
        vec![
            oauth_profile("first", "rt-shared"),
            oauth_profile("second", "rt-shared"),
        ],
        Some("second"),
    );
    assert_eq!(match_by_refresh_token(&config, "rt-shared"), Some("second"));
}

#[test]
fn endpoint_profiles_without_oauth_are_skipped() {
    let config = config_with(
        vec![endpoint_profile("api"), oauth_profile("work", "rt-work")],
        None,
    );
    assert_eq!(match_by_refresh_token(&config, "rt-work"), Some("work"));
}

#[test]
fn attributes_unmatched_login_to_credential_less_active() {
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("new")],
        Some("new"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None),
        Some(("new", Source::CredentialLessActive))
    );
}

#[test]
fn token_match_wins_over_credential_less_active() {
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile("new"),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None),
        Some(("personal", Source::RefreshMatch))
    );
}

#[test]
fn no_attribution_when_active_profile_has_creds() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn no_attribution_when_no_active_profile() {
    let config = config_with(vec![blank_profile("new")], None);
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn attributes_credential_less_active_without_loaded_refresh_token() {
    // active credential-less profile owns the session even when the loaded
    // file carries no refresh token (API-key/endpoint auth carries none).
    let config = config_with(vec![blank_profile("new")], Some("new"));
    let live = live_oauth(None);
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None),
        Some(("new", Source::CredentialLessActive))
    );
}

#[test]
fn attributes_api_key_active_when_credentials_file_absent() {
    // switching to an API-key profile deletes ~/.claude/.credentials.json, so
    // the loaded creds are `None`. the active profile still owns the session.
    let config = config_with(vec![endpoint_profile("api")], Some("api"));
    assert_eq!(
        resolve_profile(&config, None, false, None),
        Some(("api", Source::CredentialLessActive))
    );
}

#[test]
fn no_credential_less_attribution_inside_session() {
    // inside a session (CLAUDE_CONFIG_DIR set), creds belong to the runtime profile —
    // suppress attribution so a credential-less active isn't incorrectly credited
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("active")],
        Some("active"),
    );
    let live = live_oauth(Some("rt-from-runtime"));
    assert_eq!(resolve_profile(&config, Some(&live), true, None), None);
}

#[test]
fn token_match_still_works_inside_session() {
    // token-exact match is always valid, even inside a session
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("active")],
        Some("active"),
    );
    let live = live_oauth(Some("rt-work"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, None),
        Some(("work", Source::RefreshMatch))
    );
}

#[test]
fn resolves_started_profile_in_runtime_session() {
    // `clauth start <blank>`: credential-less started profile owns the runtime session
    let config = config_with(
        vec![oauth_profile("work", "rt-work"), blank_profile("new")],
        Some("work"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("new")),
        Some(("new", Source::SessionDir))
    );
}

#[test]
fn started_profile_resolves_with_no_loaded_creds() {
    // no creds yet (pre-first-login) — started profile still owns the session
    let config = config_with(vec![blank_profile("new")], Some("work"));
    assert_eq!(
        resolve_profile(&config, None, true, Some("new")),
        Some(("new", Source::SessionDir))
    );
}

#[test]
fn token_match_wins_over_started_profile() {
    // token match is more precise than path-derived profile
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile("new"),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("new")),
        Some(("personal", Source::RefreshMatch))
    );
}

#[test]
fn unknown_started_profile_is_not_resolved() {
    // profile no longer exists → falls through to in-session suppression, no invented match
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("ghost")),
        None
    );
}

#[test]
fn disabled_profile_is_never_resolved_even_on_a_stale_token_match() {
    // A disabled profile's stored creds are left on disk untouched (disable
    // only flips the flag), so a stale live file that still matches its
    // refresh token must NOT surface it — disabled accounts are invisible to
    // `which` regardless of which resolution tier would otherwise match.
    let mut disabled = oauth_profile("acme", "rt-acme");
    disabled.disabled = true;
    let config = config_with(vec![disabled], None);
    let live = live_oauth(Some("rt-acme"));
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn disabled_profile_is_never_resolved_as_credential_less_active() {
    // Belt-and-suspenders: even if a disabled profile were somehow still the
    // active one (a pre-existing on-disk state from before this gate
    // existed), `which` must not attribute the session to it.
    let mut disabled = blank_profile("acme");
    disabled.disabled = true;
    let config = config_with(vec![disabled], Some("acme"));
    let live = live_oauth(None);
    assert_eq!(resolve_profile(&config, Some(&live), false, None), None);
}

#[test]
fn source_maps_to_wire_strings() {
    assert_eq!(Source::RefreshMatch.as_str(), "refresh_match");
    assert_eq!(Source::SessionDir.as_str(), "session_dir");
    assert_eq!(
        Source::CredentialLessActive.as_str(),
        "credential_less_active"
    );
}

#[test]
fn session_profile_extracted_from_runtime_path() {
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new(
            "/home/u/.clauth/profiles/work/runtime"
        )),
        Some("work".to_string())
    );
}

#[test]
fn session_profile_none_for_non_runtime_path() {
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new("/home/u/.claude")),
        None
    );
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new("/home/u/.clauth/profiles/work")),
        None
    );
}
