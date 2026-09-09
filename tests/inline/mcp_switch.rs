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

    let (previous, active) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Overwrite),
        no_network,
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

    let (previous, active) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Overwrite),
        no_network,
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

    let (previous, active) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        Some(DivergenceChoice::Discard),
        no_network,
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

    let (previous, active) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        no_network,
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

    let config = handle(AppConfig {
        state: AppState {
            active_profile: Some("active".into()),
            profiles: vec!["active".into(), "target".into()],
            ..Default::default()
        },
        profiles: vec![active_profile, target_profile],
    });

    let revoked = |_: &str, _: Option<&str>| {
        Err(crate::oauth::RefreshError::Invalid(
            crate::oauth::TokenFailure::Status(400),
        ))
    };
    let err = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("target"),
        None,
        revoked,
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
        Err(crate::oauth::RefreshError::Invalid(
            crate::oauth::TokenFailure::Status(400),
        ))
    };
    let (previous, active) = switch_profile_noninteractive(
        &config,
        &crate::profile::ProfileName::from("active"),
        None,
        revoked,
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
