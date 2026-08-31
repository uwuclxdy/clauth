#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::BTreeMap;

use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName};
use crate::profile_cache::{USAGE_CACHE_FILE, write_profile_cache};
use crate::providers::Provider;
use crate::testutil::HomeSandbox;
use crate::usage::{PlanInfo, PlanTier, UsageInfo};

fn oauth_profile(name: &str, refresh: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
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
        name: name.into(),
        base_url: Some("https://example.test".to_string()),
        api_key: Some("sk-x".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn blank_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
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

/// The CLA-SPLIT lookup for a tree where no profile carries a sidecar — the
/// shape every pre-split test resolves under.
fn no_sidecars(_name: &crate::profile::ProfileName) -> Option<String> {
    None
}

/// A stub CLA-SPLIT lookup over `(profile, installed access token)` pairs,
/// standing in for `claude::installed_session_token`'s disk read. The real one
/// already filters mis-filled sidecars out, so anything listed here is a token a
/// switch would genuinely install.
fn sidecars(
    pairs: &'static [(&'static str, &'static str)],
) -> impl Fn(&crate::profile::ProfileName) -> Option<String> {
    move |name| {
        pairs
            .iter()
            .find(|(p, _)| *p == name)
            .map(|(_, token)| (*token).to_string())
    }
}

/// A login with no refresh token — what a `claude setup-token` mint installs.
fn live_session_token(access: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: None,
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
        Some(&crate::profile::ProfileName::from("personal"))
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
    assert_eq!(
        match_by_refresh_token(&config, "rt-shared"),
        Some(&crate::profile::ProfileName::from("second"))
    );
}

#[test]
fn endpoint_profiles_without_oauth_are_skipped() {
    let config = config_with(
        vec![endpoint_profile("api"), oauth_profile("work", "rt-work")],
        None,
    );
    assert_eq!(
        match_by_refresh_token(&config, "rt-work"),
        Some(&crate::profile::ProfileName::from("work"))
    );
}

#[test]
fn attributes_unmatched_login_to_credential_less_active() {
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            blank_profile(&crate::profile::ProfileName::from("new")),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("new"),
            Source::CredentialLessActive
        ))
    );
}

#[test]
fn token_match_wins_over_credential_less_active() {
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile(&crate::profile::ProfileName::from("new")),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("personal"),
            Source::RefreshMatch
        ))
    );
}

#[test]
fn no_attribution_when_active_profile_has_creds() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        None
    );
}

#[test]
fn no_attribution_when_no_active_profile() {
    let config = config_with(
        vec![blank_profile(&crate::profile::ProfileName::from("new"))],
        None,
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        None
    );
}

#[test]
fn attributes_credential_less_active_without_loaded_refresh_token() {
    // active credential-less profile owns the session even when the loaded
    // file carries no refresh token (API-key/endpoint auth carries none).
    let config = config_with(
        vec![blank_profile(&crate::profile::ProfileName::from("new"))],
        Some("new"),
    );
    let live = live_oauth(None);
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("new"),
            Source::CredentialLessActive
        ))
    );
}

#[test]
fn attributes_api_key_active_when_credentials_file_absent() {
    // switching to an API-key profile deletes ~/.claude/.credentials.json, so
    // the loaded creds are `None`. the active profile still owns the session.
    let config = config_with(vec![endpoint_profile("api")], Some("api"));
    assert_eq!(
        resolve_profile(&config, None, false, None, &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("api"),
            Source::CredentialLessActive
        ))
    );
}

#[test]
fn no_credential_less_attribution_inside_session() {
    // inside a session (CLAUDE_CONFIG_DIR set), creds belong to the runtime profile —
    // suppress attribution so a credential-less active isn't incorrectly credited
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            blank_profile(&crate::profile::ProfileName::from("active")),
        ],
        Some("active"),
    );
    let live = live_oauth(Some("rt-from-runtime"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, None, &no_sidecars),
        None
    );
}

#[test]
fn token_match_still_works_inside_session() {
    // token-exact match is always valid, even inside a session
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            blank_profile(&crate::profile::ProfileName::from("active")),
        ],
        Some("active"),
    );
    let live = live_oauth(Some("rt-work"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, None, &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("work"),
            Source::RefreshMatch
        ))
    );
}

#[test]
fn resolves_started_profile_in_runtime_session() {
    // `clauth start <blank>`: credential-less started profile owns the runtime session
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            blank_profile(&crate::profile::ProfileName::from("new")),
        ],
        Some("work"),
    );
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("new"), &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("new"),
            Source::SessionDir
        ))
    );
}

#[test]
fn started_profile_resolves_with_no_loaded_creds() {
    // no creds yet (pre-first-login) — started profile still owns the session
    let config = config_with(
        vec![blank_profile(&crate::profile::ProfileName::from("new"))],
        Some("work"),
    );
    assert_eq!(
        resolve_profile(&config, None, true, Some("new"), &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("new"),
            Source::SessionDir
        ))
    );
}

#[test]
fn token_match_wins_over_started_profile() {
    // token match is more precise than path-derived profile
    let config = config_with(
        vec![
            oauth_profile("personal", "rt-personal"),
            blank_profile(&crate::profile::ProfileName::from("new")),
        ],
        Some("new"),
    );
    let live = live_oauth(Some("rt-personal"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("new"), &no_sidecars),
        Some((
            &crate::profile::ProfileName::from("personal"),
            Source::RefreshMatch
        ))
    );
}

#[test]
fn unknown_started_profile_is_not_resolved() {
    // profile no longer exists → falls through to in-session suppression, no invented match
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_oauth(Some("rt-fresh"));
    assert_eq!(
        resolve_profile(&config, Some(&live), true, Some("ghost"), &no_sidecars),
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
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        None
    );
}

#[test]
fn disabled_profile_is_never_resolved_as_credential_less_active() {
    // Belt-and-suspenders: even if a disabled profile were somehow still the
    // active one (a pre-existing on-disk state from before this gate
    // existed), `which` must not attribute the session to it.
    let mut disabled = blank_profile(&crate::profile::ProfileName::from("acme"));
    disabled.disabled = true;
    let config = config_with(vec![disabled], Some("acme"));
    let live = live_oauth(None);
    assert_eq!(
        resolve_profile(&config, Some(&live), false, None, &no_sidecars),
        None
    );
}

#[test]
fn session_token_install_is_attributed_to_its_profile() {
    // The regression: a switch installs `session-token.json` for a CLA-SPLIT
    // profile, and that mint carries no refresh token, so tier 1 cannot see it.
    // Before the sidecar tier the whole resolution fell through to `unknown`
    // and every statusline reading `clauth which` lost its account.
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_session_token("oat-work");
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            false,
            None,
            &sidecars(&[("work", "oat-work")])
        ),
        Some((
            &crate::profile::ProfileName::from("work"),
            Source::SessionTokenMatch
        ))
    );
}

#[test]
fn a_rotating_login_is_never_attributed_to_a_sidecar() {
    // The tier is gated on the loaded file carrying NO refresh token. A rotating
    // login whose refresh token matches nothing must stay unresolved even when a
    // sidecar happens to hold the same access token, or a stale live slot mid-
    // rotation would name a profile it is not running as.
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "oat-work".to_string(),
            refresh_token: Some("rt-elsewhere".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    };
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            false,
            None,
            &sidecars(&[("work", "oat-work")])
        ),
        None
    );
}

#[test]
fn session_token_match_wins_over_credential_less_active() {
    // Same precedence the refresh tier already has: an exact credential match is
    // more precise than "the active profile stores nothing".
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            blank_profile(&crate::profile::ProfileName::from("new")),
        ],
        Some("new"),
    );
    let live = live_session_token("oat-work");
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            false,
            None,
            &sidecars(&[("work", "oat-work")])
        ),
        Some((
            &crate::profile::ProfileName::from("work"),
            Source::SessionTokenMatch
        ))
    );
}

#[test]
fn session_token_ties_break_on_the_active_profile() {
    // One mint captured into two profiles (a duplicated account). The active one
    // is the honest answer for the live slot.
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            blank_profile(&crate::profile::ProfileName::from("copy")),
        ],
        Some("copy"),
    );
    let live = live_session_token("oat-shared");
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            false,
            None,
            &sidecars(&[("work", "oat-shared"), ("copy", "oat-shared")])
        ),
        Some((
            &crate::profile::ProfileName::from("copy"),
            Source::SessionTokenMatch
        ))
    );
}

#[test]
fn session_token_match_still_works_inside_a_session() {
    // An exact credential match is valid wherever it is asked from, so a session
    // on a custom `CLAUDE_CONFIG_DIR` resolves the same as a bare one.
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_session_token("oat-work");
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            true,
            None,
            &sidecars(&[("work", "oat-work")])
        ),
        Some((
            &crate::profile::ProfileName::from("work"),
            Source::SessionTokenMatch
        ))
    );
}

#[test]
fn disabled_profile_is_never_resolved_on_a_session_token_match() {
    // Disabling leaves the sidecar on disk, so the new tier needs the same gate
    // every other tier passes through.
    let mut disabled = oauth_profile("acme", "rt-acme");
    disabled.disabled = true;
    let config = config_with(vec![disabled], None);
    let live = live_session_token("oat-acme");
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            false,
            None,
            &sidecars(&[("acme", "oat-acme")])
        ),
        None
    );
}

#[test]
fn a_blank_access_token_never_matches_a_sidecar() {
    // Claude Code's logged-out shell blanks the tokens in place. A stub sidecar
    // that also read back blank would otherwise attribute the shell to it.
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let live = live_session_token("");
    assert_eq!(
        resolve_profile(
            &config,
            Some(&live),
            false,
            None,
            &sidecars(&[("work", "")])
        ),
        None
    );
}

/// An OAuth profile whose login token claims `sub`, the tier this field reported
/// forever before the cache became the first answer.
fn oauth_profile_claiming(name: &str, refresh: &str, sub: &str) -> Profile {
    let mut profile = oauth_profile(name, refresh);
    if let Some(oauth) = profile
        .credentials
        .as_mut()
        .and_then(|c| c.claude_ai_oauth.as_mut())
    {
        oauth.subscription_type = Some(sub.to_string());
    }
    profile
}

/// Persist a `/profile` plan for `name` — the on-disk cache every JSON surface
/// resolves a tier through. Needs a live [`HomeSandbox`].
fn cache_plan(name: &str, tier: PlanTier, status: Option<&str>) {
    // The cache write is gated on the on-disk record; persisting this plan is
    // the helper's whole job.
    crate::testutil::register_names(&[name]);
    let usage = UsageInfo {
        plan: Some(PlanInfo {
            tier,
            subscription_status: status.map(str::to_string),
        }),
        ..Default::default()
    };
    write_profile_cache(
        &crate::profile::ProfileName::from(name),
        USAGE_CACHE_FILE,
        &usage,
    );
}

/// `which --json`'s `tier` is `null` when nothing on disk claims a tier, which
/// is what `status.json` and the MCP tools already emit for the same account —
/// the bare "Claude" this field used to print was a plan the account never had,
/// and it made the three surfaces disagree.
#[test]
fn json_tier_is_null_when_no_tier_is_known() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let resolved = ("work".to_string(), Source::RefreshMatch);
    let value = json_view(&config, Some(&resolved));

    assert_eq!(
        value["profile"], "work",
        "fixture control: profile resolved"
    );
    assert!(
        value["tier"].is_null(),
        "tier must be null with no fetched plan and no token claim, got {}",
        value["tier"]
    );
}

/// A never-fetched account has no cache to read, so the login token's claim is
/// still the answer rather than a `null` that would read as "no plan".
#[test]
fn json_tier_falls_back_to_the_token_claim_with_no_cache() {
    let _home = HomeSandbox::new();
    let config = config_with(
        vec![oauth_profile_claiming("work", "rt-work", "max")],
        Some("work"),
    );
    let resolved = ("work".to_string(), Source::RefreshMatch);

    assert_eq!(json_view(&config, Some(&resolved))["tier"], "Max");
}

/// A cached plan outranks the token claim. The multiplier is the discriminator:
/// the token carries a bare `max` and nothing else, so `Max 20x` is unreachable
/// by the token path and can only have come off the cache.
#[test]
fn json_tier_reports_the_cached_plan_over_the_token_claim() {
    let _home = HomeSandbox::new();
    let config = config_with(
        vec![oauth_profile_claiming("work", "rt-work", "max")],
        Some("work"),
    );
    cache_plan("work", PlanTier::Max(Some(20)), None);
    let resolved = ("work".to_string(), Source::RefreshMatch);

    assert_eq!(json_view(&config, Some(&resolved))["tier"], "Max 20x");
}

/// The defect this field carried: a Pro account canceled AFTER login kept
/// reporting `Claude Pro` here forever. `subscription_type` is written once at
/// login and no refresh response carries a replacement, so the token cannot
/// learn about the `claude_free` downgrade — while `status.json` and the MCP
/// tools, reading the cache, had been reporting `Free` all along.
#[test]
fn json_tier_reports_a_canceled_accounts_real_tier_not_its_login_claim() {
    let _home = HomeSandbox::new();
    let profile = oauth_profile_claiming("kerry", "rt-kerry", "pro");
    assert_eq!(
        profile
            .credentials
            .as_ref()
            .and_then(|c| c.claude_ai_oauth.as_ref())
            .and_then(|o| o.subscription_type.as_deref()),
        Some("pro"),
        "fixture control: the stale login claim the cache has to outrank"
    );
    let config = config_with(vec![profile], Some("kerry"));
    cache_plan("kerry", PlanTier::Free, Some("canceled"));
    let resolved = ("kerry".to_string(), Source::RefreshMatch);

    assert_eq!(json_view(&config, Some(&resolved))["tier"], "Free");
}

/// One account, one tier, on both surfaces reachable from here. `status.json` is
/// driven through its own builder, so this is a real cross-surface check.
///
/// The MCP `profiles` surface is NOT asserted here: it resolves tiers through
/// `which::resolve_active`, and its tier pins live in
/// `tests/inline/mcp_profiles_tool.rs`. Recomputing `tier_label` in this test
/// instead would re-evaluate the very expression `json_view` runs internally and
/// assert a value against itself.
#[test]
fn json_tier_agrees_with_the_status_json_surface() {
    let _home = HomeSandbox::new();
    let config = config_with(
        vec![oauth_profile_claiming("kerry", "rt-kerry", "pro")],
        Some("kerry"),
    );
    cache_plan("kerry", PlanTier::Free, Some("canceled"));
    let resolved = ("kerry".to_string(), Source::RefreshMatch);

    let which = json_view(&config, Some(&resolved));
    let status = crate::daemon::build_status(&config, 60_000, None, false);

    assert_eq!(which["tier"], "Free", "fixture control: the cached tier");
    assert_eq!(
        status["profiles"][0]["name"], "kerry",
        "fixture control: the status body's one row is this account"
    );
    assert_eq!(status["profiles"][0]["tier"], which["tier"]);
}

/// An unresolved session emits every field as `null` rather than dropping them,
/// so a consumer's key lookup never has to branch on presence.
#[test]
fn json_tier_is_null_when_nothing_resolved() {
    let config = config_with(vec![oauth_profile("work", "rt-work")], Some("work"));
    let value = json_view(&config, None);

    assert!(value["profile"].is_null());
    assert!(value["tier"].is_null());
    // `get`, not `value["base_url"]`: indexing answers `Null` for a key that was
    // never emitted, so it cannot tell a null field from a dropped one — the
    // very distinction this test's contract rests on.
    assert_eq!(value.get("base_url"), Some(&serde_json::Value::Null));
}

/// A third-party profile publishes the endpoint its requests route to, matching
/// what `status.json` publishes for the same profile. `tier` answers only for an
/// Anthropic plan, so this field is the one thing on the surface that names where
/// a third-party session actually goes.
#[test]
fn json_base_url_carries_a_third_partys_endpoint() {
    let _home = HomeSandbox::new();
    let mut profile = endpoint_profile("deepseek");
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider = Some(Provider::DeepSeek);
    let config = config_with(vec![profile], Some("deepseek"));
    let resolved = ("deepseek".to_string(), Source::CredentialLessActive);

    let value = json_view(&config, Some(&resolved));
    let status = crate::daemon::build_status(&config, 60_000, None, false);

    assert_eq!(value["base_url"], "https://api.deepseek.com/anthropic");
    assert!(
        value["tier"].is_null(),
        "a third-party profile claims no Anthropic plan, got {}",
        value["tier"]
    );
    assert_eq!(
        status["profiles"][0]["base_url"], value["base_url"],
        "the endpoint reads the same on both JSON surfaces"
    );
}

/// The other direction: an Anthropic account routes nowhere special, so the
/// field is `null` while the tier still answers.
#[test]
fn json_base_url_is_null_for_an_anthropic_account() {
    let _home = HomeSandbox::new();
    let config = config_with(
        vec![oauth_profile_claiming("work", "rt-work", "max")],
        Some("work"),
    );
    let resolved = ("work".to_string(), Source::RefreshMatch);

    let value = json_view(&config, Some(&resolved));
    // Present-and-null, not absent: see `json_tier_is_null_when_nothing_resolved`.
    assert_eq!(value.get("base_url"), Some(&serde_json::Value::Null));
    assert_eq!(
        value["tier"], "Max",
        "fixture control: the account still reports its tier"
    );
}

/// The shape a reader gets wrong: a profile can hold a `base_url` AND stored
/// OAuth credentials, since setting an endpoint never drops them. With no api
/// key of its own the two fields are independent — `usage_cache_is_third_party`
/// stays false, its figures still live in the OAuth cache, so the stored pair's
/// tier still reports while requests route elsewhere.
#[test]
fn json_publishes_both_an_endpoint_and_a_tier_for_a_hybrid_profile() {
    let _home = HomeSandbox::new();
    let mut profile = oauth_profile_claiming("hybrid", "rt-hybrid", "max");
    profile.base_url = Some("https://example.test".to_string());
    let config = config_with(vec![profile], Some("hybrid"));
    let resolved = ("hybrid".to_string(), Source::RefreshMatch);

    let value = json_view(&config, Some(&resolved));
    assert_eq!(value["base_url"], "https://example.test");
    assert_eq!(value["tier"], "Max");
}

/// The arm the guard defends, and the limit of the independence above: give the
/// same hybrid a RECOGNISED provider and `tier_label`'s
/// `usage_cache_is_third_party` exit fires (its provider arm), so the
/// endpoint's presence does rule the tier out — `null` despite a stored pair
/// claiming `max`.
#[test]
fn json_tier_is_null_for_a_recognised_third_party_holding_oauth_creds() {
    let _home = HomeSandbox::new();
    let mut profile = oauth_profile_claiming("hybrid", "rt-hybrid", "max");
    profile.base_url = Some("https://api.deepseek.com/anthropic".to_string());
    profile.provider = Some(Provider::DeepSeek);
    let config = config_with(vec![profile], Some("hybrid"));
    let resolved = ("hybrid".to_string(), Source::RefreshMatch);

    let value = json_view(&config, Some(&resolved));
    assert_eq!(value["base_url"], "https://api.deepseek.com/anthropic");
    assert!(
        value["tier"].is_null(),
        "a recognised provider outranks the stored pair's claim, got {}",
        value["tier"]
    );
}

#[test]
fn source_maps_to_wire_strings() {
    assert_eq!(Source::RefreshMatch.as_str(), "refresh_match");
    assert_eq!(Source::SessionTokenMatch.as_str(), "session_token_match");
    assert_eq!(Source::SessionDir.as_str(), "session_dir");
    assert_eq!(
        Source::CredentialLessActive.as_str(),
        "credential_less_active"
    );
}

/// Tier 2 keys on the config dir's NAME, so per-session runtime dirs have to
/// resolve too — otherwise `clauth which` and `session_auth` stop recognizing
/// every `clauth start` session, with nothing failing loudly. The legacy
/// unsuffixed path must keep resolving alongside it.
#[test]
fn session_profile_extracted_from_runtime_path() {
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new(
            "/home/u/.clauth/profiles/work/runtime"
        )),
        Some("work".to_string())
    );
    assert_eq!(
        session_profile_from_config_dir(std::path::Path::new(
            "/home/u/.clauth/profiles/work/runtime-4242-0"
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
    // The isolated flavor was never attributable through this tier; widening the
    // name check must not start attributing it.
    for isolated in [
        "/home/u/.clauth/profiles/work/runtime-isolated",
        "/home/u/.clauth/profiles/work/runtime-isolated-4242-0",
    ] {
        assert_eq!(
            session_profile_from_config_dir(std::path::Path::new(isolated)),
            None,
            "{isolated} must not resolve to a profile"
        );
    }
}

/// `CLAUDE_CONFIG_DIR` describes the process ASKING, so it is the wrong input for
/// attributing another process's credentials. A TUI running inside a `clauth
/// start` session would otherwise claim every bare `claude` on the box for its
/// own runtime profile.
#[test]
fn resolve_global_ignores_claude_config_dir_in_the_readers_env() {
    let home = crate::testutil::HomeSandbox::new();
    let config = config_with(
        vec![
            blank_profile(&crate::profile::ProfileName::from("global")),
            blank_profile(&crate::profile::ProfileName::from("started")),
        ],
        Some("global"),
    );
    let runtime_dir = home
        .home()
        .join(".clauth")
        .join("profiles")
        .join("started")
        .join("runtime-4242-0");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&home, &runtime_dir);

    assert_eq!(
        resolve_active(&config),
        Some(("started".to_string(), Source::SessionDir)),
        "fixture control: the reader's own env attributes it to its runtime profile"
    );
    assert_eq!(
        resolve_global(&config),
        Some(("global".to_string(), Source::CredentialLessActive)),
        "the global credential link's owner does not depend on who is asking"
    );
}

/// The `--json` doc names which half of the endpoint question its fields
/// answer. `oauth` and `base_url` read the MANAGED field alone; a doc that
/// reads as the whole routing rule is what routed readers to the wrong
/// answer, so this pins the wording in source.
#[test]
fn json_view_doc_names_the_managed_half_and_points_at_the_routing_answer() {
    let src = include_str!("../../src/which.rs");
    let doc = &src[..src.find("fn json_view(").expect("json_view is defined")];
    let doc = &doc[doc
        .rfind("/// The `--json` payload")
        .expect("the doc opens with its subject")..];
    assert!(
        doc.contains("MANAGED half of routing"),
        "the doc names the half: {doc}"
    );
    assert!(
        doc.contains("crate::profile::stored_endpoint"),
        "the doc points at the reader that answers both halves: {doc}"
    );
}
