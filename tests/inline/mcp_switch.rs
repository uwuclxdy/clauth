#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `switch_profile_noninteractive` divergence-branch coverage for the MCP
//! `switch` tool. The four divergence outcomes are exercised at the home-derived
//! seam, no TTY: `Some(NewProfile)`/`None` must error, `Some(Overwrite)` captures
//! the diverged live login into the OUTGOING profile (the
//! `switch_profile_reconciled` path), and `Some(Discard)` force-relinks the
//! target's stored creds WITHOUT capturing the live login into any profile (the
//! `switch_profile_discard` path).

#![cfg(unix)]

use super::*;

use crate::claude::{LinkState, classify_credentials_link};
use crate::profile::{
    AppConfig, AppState, ClaudeCredentials, DivergenceChoice, OAuthToken, Profile, profile_dir,
    read_json_file, save_profile,
};
use crate::testutil::HomeSandbox;

fn creds(access: &str, refresh: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// Like [`creds`] but already past expiry, so `ensure_installable` classifies
/// the profile as expiring and exercises the injected refresher.
fn creds_expired(access: &str, refresh: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 - 60_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// Gate fixture: an offline stand-in for the injected refresher. Tests whose
/// profiles never route through the refresher ([`creds`]' `expires_at: None`
/// reads as not-expiring) pass it as a placeholder; the Transient-arm test
/// drives the gate through it on purpose via a [`creds_expired`] target.
fn no_network(
    _rt: &str,
    _scopes: Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    Err(crate::oauth::RefreshError::Transient(
        crate::oauth::TokenFailure::Transport,
    ))
}

fn handle(config: AppConfig) -> crate::profile::ConfigHandle {
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(config))
}

fn stored_profile(name: &str, c: Option<ClaudeCredentials>) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = c;
    save_profile(&p).expect("save profile");
    p
}

/// `profiles.toml` naming `active` active beside `target`, the state a switch
/// reads its outgoing account off.
fn persist_active_and_target() {
    crate::profile::save_app_state(&AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into(), "target".into()],
        ..Default::default()
    })
    .expect("persist state");
}

/// Active profile `active` has stored creds; CC re-logged into a different
/// account, so the live `~/.claude/.credentials.json` is a plain diverged file
/// carrying `live_bytes`. `target` is a second profile to switch to.
fn seed_diverged(
    active: &str,
    active_stored: ClaudeCredentials,
    live: &ClaudeCredentials,
    target: &str,
    target_stored: Option<ClaudeCredentials>,
) -> AppConfig {
    let active_profile = stored_profile(active, Some(active_stored));
    let target_profile = stored_profile(target, target_stored);

    let live_path = crate::profile::claude_dir()
        .expect("claude dir")
        .join(".credentials.json");
    std::fs::create_dir_all(live_path.parent().expect("parent")).expect("mkdir .claude");
    std::fs::write(&live_path, serde_json::to_vec(live).expect("ser live")).expect("write live");

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![active_profile, target_profile],
    };
    config.state.active_profile = Some(active.into());
    config.state.profiles = vec![active.into(), target.into()];
    crate::profile::save_app_state(&config.state).expect("persist state");
    config
}

#[test]
fn divergence_without_default_errors() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        no_network,
        None,
    );
    assert!(
        err.is_err(),
        "diverged active with no default must error, never prompt"
    );
}

#[test]
fn divergence_new_profile_default_errors() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::NewProfile),
        no_network,
        None,
    );
    assert!(
        err.is_err(),
        "'save as new profile' needs an interactive name prompt; headless must error",
    );
}

#[test]
fn divergence_overwrite_captures_relogin_into_outgoing() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Overwrite),
        no_network,
        None,
    )
    .expect("overwrite switch");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");

    // Overwrite = force-snapshot the live re-login into the OUTGOING profile.
    let on_disk: ClaudeCredentials = read_json_file(
        &profile_dir(&crate::profile::ProfileName::from("active"))
            .unwrap()
            .join("credentials.json"),
    )
    .expect("read outgoing creds");
    assert_eq!(
        on_disk.refresh_token(),
        Some("relogin-r"),
        "overwrite must capture the live re-login into the outgoing profile",
    );

    // …and the active link ends pointing at target's stored creds.
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live_now.refresh_token(),
        Some("target-r"),
        "overwrite must end with the active link pointing at target",
    );
}

/// A logged-out shell (both tokens blanked) classifies Diverged but holds no
/// login: even the most dangerous default (`Overwrite`) must NOT capture its
/// blank tokens over the outgoing profile's stored login — the switch takes the
/// plain path and replaces the shell wholesale.
#[test]
fn divergence_over_a_shell_switches_without_capturing() {
    let _home = HomeSandbox::new();
    let shell = creds("", "");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &shell,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    // Positive control: the shell must be on the Diverged path the exemption
    // bypasses — otherwise this test would pass without exercising it.
    assert_eq!(
        classify_credentials_link(&crate::profile::ProfileName::from("active")).unwrap(),
        LinkState::Diverged,
        "a blank-token shell still classifies Diverged",
    );

    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Overwrite),
        no_network,
        None,
    )
    .expect("shell switch proceeds");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");

    // The shell's blanks were never captured over the outgoing store.
    let outgoing: ClaudeCredentials = read_json_file(
        &profile_dir(&crate::profile::ProfileName::from("active"))
            .unwrap()
            .join("credentials.json"),
    )
    .expect("read outgoing creds");
    assert_eq!(
        outgoing.refresh_token(),
        Some("stored-r"),
        "Overwrite over a shell must not destroy the outgoing profile's stored login",
    );

    // …and the live link ends pointing at target's stored creds.
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live_now.refresh_token(),
        Some("target-r"),
        "the switch replaces the shell with target's stored login",
    );
}

#[test]
fn divergence_discard_drops_relogin_and_relinks_target() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Discard),
        no_network,
        None,
    )
    .expect("discard switch succeeds — it force-relinks past the divergence guard");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");

    // The live file now resolves to target's stored creds (the foreign re-login
    // was dropped, not captured).
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live_now.refresh_token(),
        Some("target-r"),
        "discard relinks the live file to target's stored creds",
    );

    // The OUTGOING profile's stored chain is untouched — the foreign login was
    // never captured into it.
    let outgoing: ClaudeCredentials = read_json_file(
        &profile_dir(&crate::profile::ProfileName::from("active"))
            .unwrap()
            .join("credentials.json"),
    )
    .expect("read outgoing creds");
    assert_eq!(
        outgoing.refresh_token(),
        Some("stored-r"),
        "discard must not capture the live login into the outgoing profile",
    );
}

/// Diverged active + a vanished target through the Discard branch is the
/// worst-case ghost switch: `switch_profile_discard` force-links with no prior
/// snapshot, so pre-guard the uncaptured re-login was destroyed for good. The
/// existence bail must fire before any side effect.
#[test]
fn divergence_discard_to_a_ghost_target_bails_before_side_effects() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("ghost"),
        Some(DivergenceChoice::Discard),
        no_network,
        None,
    )
    .expect_err("a ghost target must bail");
    assert!(
        err.to_string().contains("not found"),
        "bail names the cause, got: {err}"
    );

    // The uncaptured re-login is still the live file, still diverged.
    let live_path = crate::profile::claude_dir()
        .unwrap()
        .join(".credentials.json");
    let live_now: ClaudeCredentials = read_json_file(&live_path).expect("live file survives");
    assert_eq!(
        live_now.refresh_token(),
        Some("relogin-r"),
        "the ghost bail must not touch the uncaptured live login",
    );
    assert!(matches!(
        classify_credentials_link(&crate::profile::ProfileName::from("active")).expect("classify"),
        LinkState::Diverged
    ));
    assert_eq!(
        config.lock().unwrap().state.active_profile.as_deref(),
        Some("active"),
        "active profile unchanged after the bail",
    );
}

/// Same ghost target through the Overwrite branch: the bail must fire before
/// `switch_profile_reconciled` snapshots the live login into the outgoing
/// profile (a half-applied capture would rewrite outgoing's stored chain for
/// a switch that then fails).
#[test]
fn divergence_overwrite_to_a_ghost_target_bails_before_capture() {
    let _home = HomeSandbox::new();
    let live = creds("relogin-a", "relogin-r");
    let config = handle(seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &live,
        "target",
        Some(creds("target-a", "target-r")),
    ));

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("ghost"),
        Some(DivergenceChoice::Overwrite),
        no_network,
        None,
    )
    .expect_err("a ghost target must bail");
    assert!(
        err.to_string().contains("not found"),
        "bail names the cause, got: {err}"
    );

    // Bail fires before the snapshot: outgoing's stored chain is untouched.
    let outgoing: ClaudeCredentials = read_json_file(
        &profile_dir(&crate::profile::ProfileName::from("active"))
            .unwrap()
            .join("credentials.json"),
    )
    .expect("read outgoing creds");
    assert_eq!(
        outgoing.refresh_token(),
        Some("stored-r"),
        "the ghost bail must not capture the live login into outgoing",
    );
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("live file survives");
    assert_eq!(live_now.refresh_token(), Some("relogin-r"));
}

#[test]
fn non_diverged_switch_takes_plain_path() {
    let _home = HomeSandbox::new();
    // Active profile is freshly linked (LinkedTo), not diverged: any
    // `on_divergence` is irrelevant and the switch just succeeds.
    let active_stored = creds("stored-a", "stored-r");
    let active_profile = stored_profile("active", Some(active_stored.clone()));
    let target_profile = stored_profile("target", Some(creds("target-a", "target-r")));

    // Link the live path to the active profile so it classifies as LinkedTo.
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");

    let config = AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let config = handle(config);

    assert_eq!(
        classify_credentials_link(&crate::profile::ProfileName::from("active")).expect("classify"),
        LinkState::LinkedTo,
        "precondition: active is cleanly linked, not diverged",
    );

    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        no_network,
        None,
    )
    .expect("plain switch");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "target");
}

/// The disabled gate lives in `ensure_switch_target_ok`, the chokepoint every
/// switch primitive calls first — so the MCP `switch` tool's own action
/// (`switch_profile_noninteractive`) refuses a disabled target even on the
/// plain, non-diverged path, with no CLI-side pre-check anywhere near it.
#[test]
fn non_diverged_switch_refuses_a_disabled_target() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let mut target_profile = stored_profile("target", Some(creds("target-a", "target-r")));
    target_profile.disabled = true;
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    persist_active_and_target();

    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        no_network,
        None,
    )
    .expect_err("a disabled target must be refused");
    assert_eq!(
        err.to_string(),
        "'target': account is disabled, run `clauth enable target`"
    );
    assert!(
        config
            .lock()
            .unwrap()
            .is_active(&crate::profile::ProfileName::from("active")),
        "a refused switch must leave the active profile unchanged"
    );
}

// ── AUTH-1 gate on the noninteractive path (Incident C, every entry point) ──

/// A refresher spy: records whether it ran and always succeeds when it does.
fn spy_refresher(
    called: &std::sync::atomic::AtomicBool,
) -> impl Fn(
    &str,
    Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError>
+ '_ {
    move |_rt: &str, _scopes: Option<&str>| {
        called.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::oauth::TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    }
}

/// Case A (the bug): a disabled AND clock-expired target must be refused with
/// the disabled message, and the refresher — which the AUTH-1 gate would
/// otherwise call for an expiring token — must never run. Pre-fix, the
/// disabled check lived only inside `switch_profile`, reached AFTER
/// `ensure_installable`, so this target's single-use refresh token got
/// rotated over HTTP before the refusal ever landed.
#[test]
fn non_diverged_switch_refuses_a_disabled_expired_target_before_any_refresh() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let mut target_profile = stored_profile("target", Some(creds_expired("dead-a", "dead-r")));
    target_profile.disabled = true;
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    persist_active_and_target();

    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });
    let called = std::sync::atomic::AtomicBool::new(false);

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        spy_refresher(&called),
        None,
    )
    .expect_err("a disabled target must be refused");
    assert_eq!(
        err.to_string(),
        "'target': account is disabled, run `clauth enable target`"
    );
    assert!(
        !called.load(std::sync::atomic::Ordering::SeqCst),
        "the refresher must never run for a disabled target"
    );
    assert!(
        config
            .lock()
            .unwrap()
            .is_active(&crate::profile::ProfileName::from("active")),
        "a refused switch must leave the active profile unchanged"
    );
}

/// Positive control: the SAME clock-expired shape, target NOT disabled, DOES
/// invoke the refresher — proving the spy actually detects a call (otherwise
/// the "never called" assertion above is vacuous).
#[test]
fn non_diverged_switch_refreshes_a_non_disabled_expired_target() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let target_profile = stored_profile("target", Some(creds_expired("live-a", "live-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");

    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });
    let called = std::sync::atomic::AtomicBool::new(false);

    let _ = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        spy_refresher(&called),
        None,
    );
    assert!(
        called.load(std::sync::atomic::Ordering::SeqCst),
        "positive control: a non-disabled expired target must invoke the refresher"
    );
}

/// A dead target must be refused BEFORE any relink — the MCP `switch` (and any
/// future headless caller) shares the CLI's `ensure_installable` gate, so a
/// quarantined token can never land in the Keychain through this path either.
#[test]
fn noninteractive_switch_refuses_a_dead_target_with_login_hint() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let target_profile = stored_profile("target", Some(creds_expired("dead-a", "dead-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    persist_active_and_target();

    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });

    let revoked = |_: &str, _: Option<&str>| {
        Err(crate::oauth::RefreshError::Invalid {
            failure: crate::oauth::TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        revoked,
        None,
    )
    .expect_err("a revoked target must refuse the switch");
    assert!(
        err.to_string().contains("clauth login target"),
        "the refusal names the recovery, got: {err}"
    );

    // The refusal quarantined the target and left the active link untouched.
    assert!(
        config
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from("target"))
    );
    assert!(
        config
            .lock()
            .unwrap()
            .is_active(&crate::profile::ProfileName::from("active"))
    );
    let live_now: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live_now.refresh_token(),
        Some("stored-r"),
        "the dead target's credentials must never reach the live link",
    );
}

/// A transient refresh failure refuses THIS switch but never quarantines —
/// mirrors the CLI gate's Transient arm.
#[test]
fn noninteractive_switch_transient_failure_refuses_without_quarantine() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let target_profile = stored_profile("target", Some(creds_expired("t-a", "t-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    persist_active_and_target();

    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        no_network,
        None,
    )
    .expect_err("a transient refresh failure must refuse the switch");
    assert!(
        err.to_string().contains("could not refresh"),
        "the refusal explains the transient cause, got: {err}"
    );
    assert!(
        !config
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from("target")),
        "a transient blip must never quarantine"
    );
    assert!(
        config
            .lock()
            .unwrap()
            .is_active(&crate::profile::ProfileName::from("active"))
    );
}

/// Switching to the already-active profile must never run the AUTH-1 gate:
/// nothing new is installed (`switch_profile` no-ops on `is_active`), and the
/// active chain is the one a plain `claude` may be refreshing through the
/// symlink concurrently — a lost race would false-quarantine a healthy login.
/// Expired creds + a revoked refresher prove the gate is skipped: routing
/// through it would refuse the switch and quarantine.
#[test]
fn switch_to_the_active_profile_never_gates() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds_expired("live-a", "live-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");

    let config = AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into()],
            ..Default::default()
        },
        profiles: vec![active_profile],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let config = handle(config);

    let revoked = |_: &str, _: Option<&str>| {
        Err(crate::oauth::RefreshError::Invalid {
            failure: crate::oauth::TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("active"),
        None,
        revoked,
        None,
    )
    .expect("switch-to-active must no-op, never gate");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "active");
    assert!(
        !config
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from("active")),
        "the exempt path must never quarantine the active profile"
    );
}

/// A re-login landing while the gate refreshes the target, into a login that
/// is itself expiring: the switch waits for a retry, typed as a deferral (no
/// failure) whose sentence stands alone.
#[test]
fn a_login_change_during_the_gates_refresh_defers_the_switch() {
    let _home = HomeSandbox::new();
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let target_profile = stored_profile("target", Some(creds_expired("old-a", "old-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });
    crate::profile::save_app_state(&config.lock().unwrap().state).expect("persist state");

    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        |_: &str, _: Option<&str>| {
            let mut cfg = crate::profile::load_config().expect("load config");
            crate::actions::overwrite_captured_profile(
                &mut cfg,
                &crate::profile::ProfileName::from("target"),
                &crate::testutil::profile_id("target"),
                crate::actions::CaptureSnapshot {
                    credentials: Some(creds_expired("relogin-a", "relogin-r")),
                    base_url: None,
                    api_key: None,
                    anchor: crate::actions::AnchorAction::Unproven,
                    plan: None,
                },
            )
            .expect("install the re-login mid-refresh");
            Ok(crate::oauth::TokenResponse {
                access_token: "new-a".to_string(),
                refresh_token: "new-r".to_string(),
                expires_in: 3600,
                scope: None,
            })
        },
        None,
    )
    .expect_err("an expiring re-login defers the switch");

    match err {
        SwitchError::Deferred(sentence) => assert_eq!(
            sentence,
            "'target' login changed during its refresh and the new login expires soon. Retry in a moment"
        ),
        other => panic!("expected a deferral, got {other:?}"),
    }
}

/// `active` linked live, `peer` and an expiring `target` stored, all on disk;
/// the handle is the caller's read of that.
fn active_linked_with_a_peer_and_an_expiring_target() -> crate::profile::ConfigHandle {
    let active = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let peer = stored_profile("peer", Some(creds("peer-a", "peer-r")));
    let target = stored_profile("target", Some(creds_expired("dead-a", "dead-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");
    let config = AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "peer".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active, peer, target],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    handle(config)
}

/// A refresher that, inside the gate's refresh, lets a peer switch to `peer`
/// (then, with `relogs`, Claude Code re-logs it), and answers the refresh.
fn refresher_with_a_peer_switch(
    relogs: bool,
) -> impl Fn(
    &str,
    Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    let ran = std::cell::Cell::new(false);
    move |_: &str, _: Option<&str>| {
        if !ran.replace(true) {
            let mut peer = crate::profile::load_config().expect("peer read");
            assert!(
                crate::actions::switch_profile_locked(
                    &mut peer,
                    &crate::profile::ProfileName::from("peer")
                )
                .expect("peer switch"),
                "fixture: the peer switched"
            );
            if relogs {
                let live = crate::profile::claude_dir()
                    .expect("claude dir")
                    .join(".credentials.json");
                std::fs::remove_file(&live).expect("unlink");
                std::fs::write(
                    &live,
                    serde_json::to_vec(&creds("peer-relogin-a", "peer-relogin-r")).expect("ser"),
                )
                .expect("the peer's re-login");
            }
        }
        Ok(crate::oauth::TokenResponse {
            access_token: "new-a".to_string(),
            refresh_token: "new-r".to_string(),
            expires_in: 3600,
            scope: None,
        })
    }
}

fn stored_refresh_of(name: &str) -> Option<String> {
    read_json_file::<ClaudeCredentials>(
        &profile_dir(&crate::profile::ProfileName::from(name))
            .unwrap()
            .join("credentials.json"),
    )
    .ok()
    .and_then(|c| c.refresh_token().map(str::to_string))
}

/// A peer's switch landing in the gate's refresh window moves the outgoing
/// account: the reply's `previous` names the account disk names after the
/// gate, and each store keeps its own chain.
#[test]
fn a_peer_switch_inside_the_gate_is_the_previous_the_reply_names() {
    let _home = HomeSandbox::new();
    let config = active_linked_with_a_peer_and_an_expiring_target();
    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Overwrite),
        refresher_with_a_peer_switch(false),
        None,
    )
    .expect("the switch lands");
    assert_eq!(previous.as_deref(), Some("peer"));
    assert_eq!(active, "target");
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("peer").as_deref(), Some("peer-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("new-r"));
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("target"))
            .unwrap()
            .join("credentials.json"),
        "the live slot links the target"
    );
}

/// A peer's switch landing in the RETRY's gate (after the hold returned
/// `GateNeeded`) moves the outgoing account again: the reply's `previous`
/// names the account disk names after that gate, not the one read before it.
#[test]
fn a_peer_switch_inside_the_retry_gate_is_the_previous_the_reply_names() {
    let _home = HomeSandbox::new();
    let a = stored_profile("a", Some(creds_expired("a-old", "a-old-r")));
    let b = stored_profile("b", Some(creds("b-a", "b-r")));
    let c = stored_profile("c", Some(creds("c-a", "c-r")));
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("a"))
        .expect("link a's own login live");
    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("a".into()),
            profiles: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        },
        profiles: vec![a, b, c],
    });
    crate::profile::save_app_state(&config.lock().unwrap().state).expect("persist state");

    // The target is the disk-active account with its own login live, so the
    // gate is skipped. The seam moves disk-active to `b`, the hold returns
    // `GateNeeded`, and the retry's gate runs.
    crate::testutil::switch_race_seam::plant(|| {
        let mut peer = crate::profile::load_config().expect("peer read");
        crate::actions::switch_profile_locked(&mut peer, &crate::profile::ProfileName::from("b"))
            .expect("peer switch");
    });
    let (previous, active, _) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("a"),
        None,
        |_: &str, _: Option<&str>| {
            // The retry's gate refreshes `a` (expiring): a peer moves disk-active
            // to `c` inside that refresh, then the refresh lands.
            let mut peer = crate::profile::load_config().expect("peer read");
            crate::actions::switch_profile_locked(
                &mut peer,
                &crate::profile::ProfileName::from("c"),
            )
            .expect("peer switch");
            Ok(crate::oauth::TokenResponse {
                access_token: "new-a".to_string(),
                refresh_token: "new-r".to_string(),
                expires_in: 3600,
                scope: None,
            })
        },
        None,
    )
    .expect("the switch lands");
    assert_eq!(
        previous.as_deref(),
        Some("c"),
        "the reply names the account disk names after the retry's gate"
    );
    assert_eq!(active, "a");
    assert_eq!(stored_refresh_of("a").as_deref(), Some("new-r"));
    assert_eq!(stored_refresh_of("b").as_deref(), Some("b-r"));
    assert_eq!(stored_refresh_of("c").as_deref(), Some("c-r"));
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("a"))
            .unwrap()
            .join("credentials.json"),
        "the live slot links a"
    );
}

/// The peer's account re-logged inside the gate's window: the refusal names
/// that account, the one disk names active after the gate, and its re-login
/// stays live, unsaved and uncaptured.
#[test]
fn a_peer_switch_and_relogin_inside_the_gate_is_the_account_the_refusal_names() {
    let _home = HomeSandbox::new();
    let config = active_linked_with_a_peer_and_an_expiring_target();
    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        refresher_with_a_peer_switch(true),
        None,
    )
    .expect_err("no default resolves the peer's divergence");
    assert_eq!(
        err.to_string(),
        "'peer' has a login clauth hasn't saved, resolve the divergence in the `clauth` TUI"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("peer").as_deref(), Some("peer-r"));
    let live: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live");
    assert_eq!(
        live.refresh_token(),
        Some("peer-relogin-r"),
        "the re-login stays live"
    );
    assert_eq!(
        crate::profile::load_app_state()
            .expect("state")
            .active_profile,
        Some(crate::profile::ProfileName::from("peer")),
        "nothing switched past the peer"
    );
}

/// An `Overwrite` default over a live login another account already stores:
/// that login is saved, so the switch lands, captures it nowhere and forgets
/// nothing of the outgoing account's.
#[test]
fn an_overwrite_default_never_captures_a_login_another_account_stores() {
    let _home = HomeSandbox::new();
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &creds("owner-a", "owner-r"),
        "target",
        Some(creds("target-a", "target-r")),
    );
    config
        .profiles
        .push(stored_profile("owner", Some(creds("owner-a", "owner-r"))));
    config.state.profiles.push("owner".into());
    crate::profile::save_app_state(&config.state).expect("persist state");
    let active = crate::profile::ProfileName::from("active");
    let anchor = crate::profile::AccountId::from("uuid-active");
    crate::usage::seed_login_anchor(&active, Some(&anchor));
    let (previous, active_now, _) = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Overwrite),
        no_network,
        None,
    )
    .expect("the switch lands");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active_now, "target");
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("target"))
            .unwrap()
            .join("credentials.json"),
        "the live slot links the target"
    );
    assert_eq!(
        crate::profile_cache::load_profile_cache::<crate::profile::AccountId>(
            &active,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE
        ),
        Some(anchor),
        "the switch forgot nothing of active's"
    );
    assert_eq!(
        crate::profile::load_app_state()
            .expect("state")
            .active_profile,
        Some(crate::profile::ProfileName::from("target")),
        "target is active"
    );
}

/// The MCP tool or REST route switching onto the active account while the
/// live login is another account's saved one: that login stays in its store
/// and the active account's own login is relinked.
#[test]
fn a_headless_switch_onto_the_active_account_over_a_saved_login_relinks_it() {
    let _home = HomeSandbox::new();
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &creds("owner-a", "owner-r"),
        "target",
        Some(creds("target-a", "target-r")),
    );
    config
        .profiles
        .push(stored_profile("owner", Some(creds("owner-a", "owner-r"))));
    config.state.profiles.push("owner".into());
    crate::profile::save_app_state(&config.state).expect("persist state");
    let (previous, active_now, _) = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        None,
        no_network,
        None,
    )
    .expect("the switch lands");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active_now, "active");
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("active"))
            .unwrap()
            .join("credentials.json"),
        "the live slot links the active account"
    );
    assert_eq!(
        crate::profile::load_app_state()
            .expect("state")
            .active_profile,
        Some(crate::profile::ProfileName::from("active")),
        "active stays active"
    );
}

/// The MCP tool or REST route switching onto the active account while the live
/// login is another account's saved one AND the active account is flagged
/// `auth_broken`: the AUTH-1 gate now runs for that relink and refuses the
/// dead target, leaving the live slot and every store unchanged.
#[test]
fn a_headless_switch_refuses_a_flagged_active_account_over_another_accounts_saved_login() {
    let _home = HomeSandbox::new();
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &creds("owner-a", "owner-r"),
        "target",
        Some(creds("target-a", "target-r")),
    );
    config
        .profiles
        .push(stored_profile("owner", Some(creds("owner-a", "owner-r"))));
    config.state.profiles.push("owner".into());
    config.state.auth_broken = vec!["active".into()];
    crate::profile::save_app_state(&config.state).expect("persist state");

    let revoked = |_: &str, _: Option<&str>| {
        Err(crate::oauth::RefreshError::Invalid {
            failure: crate::oauth::TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
    let err = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        None,
        revoked,
        None,
    )
    .expect_err("a flagged active must refuse the switch");
    assert_eq!(
        err.to_string(),
        crate::format::login_expired(&crate::profile::ProfileName::from("active")).line()
    );

    let live: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live creds");
    assert_eq!(
        live.refresh_token(),
        Some("owner-r"),
        "the live slot still serves the owner's saved login"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("active")),
        "active stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
}

/// `active` flagged with its OWN login live (the live slot links `active`'s
/// store), `target` and `owner` beside it on the roster: a switch onto `active`
/// skips the AUTH-1 gate unless a peer write between the decision and the hold
/// changes what the hold reads.
fn active_flagged_with_own_login_live() -> AppConfig {
    let active_profile = stored_profile("active", Some(creds("stored-a", "stored-r")));
    let target_profile = stored_profile("target", Some(creds("target-a", "target-r")));
    let owner_profile = stored_profile("owner", Some(creds("owner-a", "owner-r")));
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![active_profile, target_profile, owner_profile],
    };
    config.state.active_profile = Some("active".into());
    config.state.profiles = vec!["active".into(), "target".into(), "owner".into()];
    config.state.auth_broken = vec!["active".into()];
    crate::profile::save_app_state(&config.state).expect("persist state");
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active's own login live");
    config
}

/// A peer relinking the live slot onto `owner`'s login: the live login is now
/// one another account stores, the write a switch's gate-decision/hold race
/// plants.
fn peer_saves_the_live_login_elsewhere() {
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("owner"))
        .expect("relink live onto owner's login");
}

/// A peer switching the active account away to `target`: the other write a
/// switch's gate-decision/hold race plants.
fn peer_switches_the_active_account_away() {
    let mut peer = crate::profile::load_config().expect("peer read");
    crate::actions::switch_profile_locked(&mut peer, &crate::profile::ProfileName::from("target"))
        .expect("peer switch");
}

/// A refresher that counts its calls and answers the terminal dead-token shape,
/// so the gate refuses a flagged target offline and the test can assert the
/// gate ran exactly the expected number of times.
fn counting_revoked(
    calls: &std::cell::Cell<usize>,
) -> impl Fn(
    &str,
    Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError>
+ '_ {
    |_: &str, _: Option<&str>| {
        calls.set(calls.get() + 1);
        Err(crate::oauth::RefreshError::Invalid {
            failure: crate::oauth::TokenFailure::Status(400),
            invalid_grant: true,
        })
    }
}

/// A peer saving the live login elsewhere after the gate decision must refuse
/// the flagged active account through the gate: the hold re-reads the predicate
/// and sends the switch back for the gate it skipped.
#[test]
fn a_peer_saving_the_live_login_elsewhere_gates_the_flagged_active_switch() {
    let _home = HomeSandbox::new();
    let config = active_flagged_with_own_login_live();
    let gate_calls = std::cell::Cell::new(0);
    crate::testutil::switch_race_seam::plant(peer_saves_the_live_login_elsewhere);

    let err = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        None,
        counting_revoked(&gate_calls),
        None,
    )
    .expect_err("a flagged active must refuse after the peer save");
    assert_eq!(
        err.to_string(),
        crate::format::login_expired(&crate::profile::ProfileName::from("active")).line()
    );
    assert_eq!(gate_calls.get(), 1, "the gate ran once, on the retry");
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("owner"))
            .unwrap()
            .join("credentials.json"),
        "the live slot still links the peer's relink target"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("active")),
        "active stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
    crate::testutil::switch_race_seam::assert_empty();
}

/// A peer switching away from the target after the gate decision must refuse
/// the flagged target through the gate: the hold re-reads the predicate and
/// finds the target no longer active on disk.
#[test]
fn a_peer_switching_away_gates_the_flagged_target_switch() {
    let _home = HomeSandbox::new();
    let config = active_flagged_with_own_login_live();
    let gate_calls = std::cell::Cell::new(0);
    crate::testutil::switch_race_seam::plant(peer_switches_the_active_account_away);

    let err = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        None,
        counting_revoked(&gate_calls),
        None,
    )
    .expect_err("a flagged target must refuse after the peer switch");
    assert_eq!(
        err.to_string(),
        crate::format::login_expired(&crate::profile::ProfileName::from("active")).line()
    );
    assert_eq!(gate_calls.get(), 1, "the gate ran once, on the retry");
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("target"))
            .unwrap()
            .join("credentials.json"),
        "the live slot still links the peer's switch target"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("target")),
        "the peer's switch stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
    crate::testutil::switch_race_seam::assert_empty();
}

/// `active` flagged with an UNSAVED live login (a diverged regular file) and a
/// `target` beside it: a switch onto `active` with a default, once a peer moves
/// disk-active away, must gate the target.
fn active_flagged_with_an_unsaved_live_login() -> AppConfig {
    let mut config = seed_diverged(
        "active",
        creds("stored-a", "stored-r"),
        &creds("x-a", "x-r"),
        "target",
        Some(creds("target-a", "target-r")),
    );
    let owner = stored_profile("owner", Some(creds("owner-a", "owner-r")));
    config.profiles.push(owner);
    config.state.profiles.push("owner".into());
    config.state.auth_broken = vec!["active".into()];
    crate::profile::save_app_state(&config.state).expect("persist state");
    config
}

/// A peer moving disk-active onto `target`: the other write an overwrite/discard
/// default's gate-decision/hold race plants.
fn peer_moves_disk_active_to_target() {
    let mut st = crate::profile::load_app_state().expect("peer state");
    st.active_profile = Some("target".into());
    crate::profile::save_app_state(&st).expect("peer persist");
}

/// A peer switching away from the flagged active after the gate decision must
/// refuse it through the gate on the `Discard` default: the discard hold
/// re-reads the predicate and sends the switch back for the gate it skipped.
#[test]
fn a_discard_default_after_a_peer_switches_away_gates_the_flagged_active() {
    let _home = HomeSandbox::new();
    let config = active_flagged_with_an_unsaved_live_login();
    let gate_calls = std::cell::Cell::new(0);
    crate::testutil::switch_race_seam::plant(peer_moves_disk_active_to_target);

    let err = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        Some(DivergenceChoice::Discard),
        counting_revoked(&gate_calls),
        None,
    )
    .expect_err("a flagged active must be refused through the gate");
    assert_eq!(
        err.to_string(),
        crate::format::login_expired(&crate::profile::ProfileName::from("active")).line()
    );
    assert_eq!(gate_calls.get(), 1, "the gate ran once, on the retry");
    let live: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live");
    assert_eq!(
        live.refresh_token(),
        Some("x-r"),
        "the live slot still serves the unsaved login, unrelinked"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("target")),
        "the peer's switch stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
    crate::testutil::switch_race_seam::assert_empty();
}

/// A peer switching away from the flagged active after the gate decision must
/// refuse it through the gate on the `Overwrite` default: the reconcile hold
/// re-reads the predicate and sends the switch back for the gate it skipped.
#[test]
fn an_overwrite_default_after_a_peer_switches_away_gates_the_flagged_active() {
    let _home = HomeSandbox::new();
    let config = active_flagged_with_an_unsaved_live_login();
    let gate_calls = std::cell::Cell::new(0);
    crate::testutil::switch_race_seam::plant(peer_moves_disk_active_to_target);

    let err = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        Some(DivergenceChoice::Overwrite),
        counting_revoked(&gate_calls),
        None,
    )
    .expect_err("a flagged active must be refused through the gate");
    assert_eq!(
        err.to_string(),
        crate::format::login_expired(&crate::profile::ProfileName::from("active")).line()
    );
    assert_eq!(gate_calls.get(), 1, "the gate ran once, on the retry");
    let live: ClaudeCredentials = read_json_file(
        &crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json"),
    )
    .expect("read live");
    assert_eq!(
        live.refresh_token(),
        Some("x-r"),
        "the live slot still serves the unsaved login, unrelinked"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("target")),
        "the peer's switch stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
    crate::testutil::switch_race_seam::assert_empty();
}

/// A discard onto the active account over another account's saved login, with
/// no gate run, must refuse with [`crate::actions::GateNeeded`] and change
/// nothing. The headless seam cannot reach this arm (the divergence read runs
/// after the seam), so the re-check is driven directly.
#[test]
fn a_discard_switch_onto_the_active_account_over_another_accounts_saved_login_needs_the_gate() {
    let _home = HomeSandbox::new();
    let config = active_flagged_with_own_login_live();
    peer_saves_the_live_login_elsewhere();
    let a = crate::profile::ProfileName::from("active");

    let err = switch_profile_discard_gated(&handle(config), &a, Some(&a), false)
        .expect_err("a gated:false discard must refuse through GateNeeded");
    assert!(
        err.downcast_ref::<crate::actions::GateNeeded>().is_some(),
        "the refusal is the gate-needed signal"
    );
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("owner"))
            .unwrap()
            .join("credentials.json"),
        "the live slot still links the owner's login"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("owner").as_deref(), Some("owner-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("active")),
        "active stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
}

/// With no peer write the exemption holds: the live slot serving the active
/// account's own login still skips the gate, so nothing is gated or changed.
#[test]
fn the_flagged_active_switch_over_its_own_login_keeps_the_exemption() {
    let _home = HomeSandbox::new();
    let config = active_flagged_with_own_login_live();
    let gate_calls = std::cell::Cell::new(0);
    let (previous, active, _) = switch_profile_noninteractive(
        &handle(config),
        &crate::profile::ProfileName::from("active"),
        None,
        counting_revoked(&gate_calls),
        None,
    )
    .expect("the switch stays the exemption's no-op");
    assert_eq!(previous.as_deref(), Some("active"));
    assert_eq!(active, "active");
    assert_eq!(gate_calls.get(), 0, "the gate never ran");
    assert_eq!(
        std::fs::read_link(
            crate::profile::claude_dir()
                .unwrap()
                .join(".credentials.json")
        )
        .expect("linked"),
        profile_dir(&crate::profile::ProfileName::from("active"))
            .unwrap()
            .join("credentials.json"),
        "the live slot still links the active account"
    );
    assert_eq!(stored_refresh_of("active").as_deref(), Some("stored-r"));
    assert_eq!(stored_refresh_of("target").as_deref(), Some("target-r"));
    let state = crate::profile::load_app_state().expect("state");
    assert_eq!(
        state.active_profile,
        Some(crate::profile::ProfileName::from("active")),
        "active stays active"
    );
    assert!(
        state.auth_broken.iter().any(|n| n.as_str() == "active"),
        "active stays flagged"
    );
}
