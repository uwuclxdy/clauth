//! Behaviour tests for `rotation_candidates` — the filter that decides which
//! profiles `refresh_all` will attempt to rotate.
//!
//! These tests never touch the network. They assert on the candidate list
//! returned by `rotation_candidates`, which is the only part of `refresh_all`
//! that `force` affects.

use super::*;
use crate::lockorder::RankedMutex;
use crate::profile::{AppState, ClaudeCredentials, OAuthToken, Profile, profile_dir};
use crate::runtime::open_pid_file;
use crate::usage::is_idle;

/// Read an ENTIRE HTTP request (headers + any `Content-Length` body) off a
/// loopback socket before the caller writes its response. On Windows a
/// `close()` with unread bytes still in the recv buffer emits an abortive RST
/// (WSAECONNABORTED 10053) that truncates the client's read of the response;
/// Linux closes gracefully, so an un-drained POST body only bites the Windows
/// CI leg. A bodyless GET drains in one read, which is why only the
/// POST-driven servers need this.
fn drain_http_request(sock: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    let mut req = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match sock.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                req.extend_from_slice(&tmp[..n]);
                if let Some(hlen) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let cl = req[..hlen]
                        .split(|&b| b == b'\n')
                        .find_map(|line| {
                            std::str::from_utf8(line)
                                .ok()?
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if req.len() >= hlen + 4 + cl {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    req
}

fn single_profile_config(name: &str, refresh_token: &str) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
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
                access_token: "at".to_string(),
                refresh_token: Some(refresh_token.to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    config
}

use crate::testutil::HomeSandbox;

/// Simulate a live `clauth start` session for `name`: a locked marker in its
/// sessions dir reads as alive via `has_live_session`. The caller must hold the
/// returned file for as long as the session should read as live — dropping it
/// releases the flock.
fn arm_live_session(name: &str) -> std::fs::File {
    let sessions = profile_dir(&crate::profile::ProfileName::from(name))
        .expect("profile_dir")
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let file = open_pid_file(&sessions.join("test-pid")).expect("open pid file");
    file.lock().expect("lock pid file");
    file
}

#[test]
fn no_live_session_included_with_force_false() {
    let config = single_profile_config("test-oauth-no-session-force-false", "rt-abc");
    let candidates = rotation_candidates(&config, false);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "test-oauth-no-session-force-false");
    assert_eq!(candidates[0].1, "rt-abc");
}

#[test]
fn no_live_session_included_with_force_true() {
    let config = single_profile_config("test-oauth-no-session-force-true", "rt-def");
    let candidates = rotation_candidates(&config, true);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "test-oauth-no-session-force-true");
}

#[test]
fn live_session_included_when_force_false() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-guard";
    let file = arm_live_session(name);

    let config = single_profile_config(name, "rt-ghi");
    let candidates = rotation_candidates(&config, false);
    assert_eq!(
        candidates,
        vec![(
            crate::profile::ProfileName::from(name),
            "rt-ghi".to_string()
        )],
        "a live session shares one credential file with clauth, so it follows a \
         rotation instead of being burned by one"
    );

    drop(file);
}

#[test]
fn live_session_included_with_force_true() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-force";
    let file = arm_live_session(name);

    let config = single_profile_config(name, "rt-jkl");
    let candidates = rotation_candidates(&config, true);
    assert_eq!(
        candidates,
        vec![(
            crate::profile::ProfileName::from(name),
            "rt-jkl".to_string()
        )],
        "liveness is not a rotation gate in either force mode"
    );

    drop(file);
}

#[test]
fn force_true_bypasses_diverged_active_when_no_active_profile() {
    // active_profile is None → active_link_diverged returns false → both force values include the
    // profile. The meaningful contract is `!force && active_link_diverged(config)` (was
    // `active_link_diverged(config)`, ignoring force); tested here without FS side effects.
    let config = single_profile_config("test-oauth-force-diverged", "rt-xyz");
    let force_false = rotation_candidates(&config, false);
    let force_true = rotation_candidates(&config, true);
    assert_eq!(force_false.len(), 1);
    assert_eq!(force_true.len(), 1);
    assert_eq!(force_true[0].0, "test-oauth-force-diverged");
}

/// `rotate_one_inner` must not stamp `Refreshing` when no refresh token —
/// the short-circuit runs before any HTTP, leaving the activity slot Idle.
#[test]
fn rotate_one_no_stamp_when_no_refresh_token() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    let _home = HomeSandbox::new();
    let profile = Profile {
        name: "test-rotate-one-no-rt".into(),
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
                access_token: "at".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push("test-rotate-one-no-rt".into());

    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one_inner(
        &config,
        &crate::profile::ProfileName::from("test-rotate-one-no-rt"),
        Some(&activity),
        &tx,
    );

    assert!(
        matches!(result, RotateOutcome::Persisted(false)),
        "rotate_one_inner should return Persisted(false) when no refresh token"
    );
    assert!(
        is_idle(
            &activity,
            &crate::profile::ProfileName::from("test-rotate-one-no-rt")
        ),
        "activity slot must remain Idle when rotation short-circuits at no-token"
    );
}

#[test]
fn profile_without_refresh_token_excluded() {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: "test-oauth-no-rt".into(),
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
                access_token: "at".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push("test-oauth-no-rt".into());
    assert!(rotation_candidates(&config, false).is_empty()); // no refresh token → excluded regardless of force
    assert!(rotation_candidates(&config, true).is_empty());
}

/// Per-profile rotation lock: acquiring for `b` must not block while `a` is held.
/// Without this, `refresh_all` workers would serialize behind the slowest profile.
/// `b` is acquired on a separate thread because the ROTATION rank forbids a single
/// thread from re-entering it — exactly the cross-thread guarantee needed.
#[test]
fn rotation_guard_is_independent_across_profiles() {
    use crate::runtime::RotationGuard;
    use std::sync::mpsc;
    use std::time::Duration;

    // HOME_OVERRIDE is process-global, so the worker thread's acquire also resolves into the sandbox.
    let _home = HomeSandbox::new();
    let a = "test-rotation-guard-indep-a";
    let b = "test-rotation-guard-indep-b";
    let held_a = RotationGuard::acquire(&crate::profile::ProfileName::from(a)).expect("acquire a");

    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let held_b = RotationGuard::acquire(&crate::profile::ProfileName::from(b))
            .expect("acquire b while a is held"); // distinct lock file → must not block
        tx.send(()).expect("signal acquired");
        drop(held_b);
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("acquiring b must not block on a (per-profile locks are independent)");
    worker.join().expect("join b worker");
    drop(held_a);
}

// `auto_start_kick` opens a window on the steady-state fetch leg now, not via a
// candidate scan; its kick/rotation legs hit the network, so the window-lapsed
// gate that decides whether to kick is unit-tested in `scheduler.rs`
// (`window_lapsed`), and the opt-in gate is `Profile::auto_start` threaded into
// `TokenEntry`.

// ── AUTH-1: pre-install auth gate (`ensure_installable`) ──────────────────────
//
// All offline: the HTTP refresh is injected as a closure, so these pin the
// gate's decision + persistence without touching the network or the real
// Keychain (Incident C guardrail).

/// Epoch-ms already in the past — an expired access token.
fn past_expiry() -> i64 {
    crate::usage::now_ms() as i64 - 60_000
}

/// Epoch-ms an hour ahead — a token with real life left.
fn future_expiry() -> i64 {
    crate::usage::now_ms() as i64 + 3_600_000
}

fn oauth_config(name: &str, refresh_token: Option<&str>, expires_at: Option<i64>) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
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
                access_token: "at-old".to_string(),
                refresh_token: refresh_token.map(String::from),
                expires_at,
                scopes: None,
                subscription_type: None,
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    crate::profile::save_app_state(&config.state).expect("persist state");
    config
}

fn third_party_config(name: &str) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
        name: name.into(),
        base_url: Some("https://api.deepseek.com/anthropic".to_string()),
        api_key: Some("sk-fixture".to_string()),
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
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    config
}

/// A refresher that must never run — bypass/valid-token paths take no refresh.
fn never_refresh(
    _rt: &str,
    _scopes: Option<&str>,
) -> std::result::Result<TokenResponse, RefreshError> {
    panic!("ensure_installable must not refresh in this scenario");
}

/// Third-party (api-key) profile → gate bypassed, no refresh attempted.
#[test]
fn gate_third_party_bypasses() {
    let _home = HomeSandbox::new();
    let name = "test-gate-third-party";
    let handle = Arc::new(RankedMutex::new(third_party_config(name)));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
}

/// The quarantine flag cannot route a third-party (custom-endpoint) profile
/// into the gate's `Broken` arm: non-OAuth targets pass through as `Ready`, so
/// no AUTH-1 surface (CLI/MCP switch, TUI toast, daemon tick) can hand a
/// quarantined keyless third-party target `login_expired`'s bare
/// `clauth login <name>` — for that state the fix is the `--api-key` command,
/// and the surface that refuses on it is the MCP pre-flight's keyless arm.
/// Reds if a gate change ever routes third-party targets into the OAuth arm
/// without revisiting that copy.
#[test]
fn gate_passes_a_quarantined_keyless_third_party_target_through() {
    let _home = HomeSandbox::new();
    let name = "test-gate-tp-quarantined-keyless";
    let target = crate::profile::ProfileName::from(name);
    let mut config = third_party_config(name);
    {
        #[allow(clippy::expect_used, reason = "test")]
        let p = config.profiles.get_mut(0).expect("fixture profile");
        p.api_key = None;
        p.provider = Some(crate::providers::Provider::DeepSeek);
    }
    config.set_auth_broken(&target, true);
    let handle = Arc::new(RankedMutex::new(config));
    {
        #[allow(clippy::expect_used, reason = "test")]
        let cfg = handle.lock().expect("lock");
        #[allow(clippy::expect_used, reason = "test")]
        let p = cfg.find(&target).expect("profile");
        assert!(
            p.is_third_party() && !crate::claude::has_inference_auth(p),
            "the fixture must be the keyless third-party shape"
        );
        assert!(cfg.is_auth_broken(&target), "the fixture must be flagged");
    }
    assert!(matches!(
        ensure_installable(&handle, &target, never_refresh),
        AuthGate::Ready
    ));
}

/// OAuth token with real life left → install as-is, no refresh.
#[test]
fn gate_valid_token_ready_without_refresh() {
    let _home = HomeSandbox::new();
    let name = "test-gate-valid";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-good"),
        Some(future_expiry()),
    )));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
}

/// Off macOS a live `clauth start` session no longer short-circuits the switch
/// gate to `Ready` (which installed the STALE token as-is): the session reads
/// the same credential file, so refreshing hands it a fresh pair. On macOS the
/// refusal stands — clauth can't write the Keychain item that session's CC
/// reads, so a refresh signs it out
/// (`runtime::rotation_blocked_by_live_session`). Offline: the refresher is
/// injected, and whether it runs at all is the assertion.
#[cfg(not(target_os = "macos"))]
#[test]
fn gate_refreshes_an_expiring_token_under_a_live_session() {
    let _home = HomeSandbox::new();
    let name = "test-gate-live-session";
    let file = arm_live_session(name);
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(
        matches!(
            ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
            AuthGate::Refreshed
        ),
        "a live session must not downgrade the gate to installing a spent token"
    );
    #[allow(clippy::expect_used, reason = "test")]
    let stored = handle
        .lock()
        .expect("config lock")
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(stored.as_deref(), Some("at-new"));

    drop(file);
}

/// The macOS arm of the above: the gate installs the expiring token as-is rather
/// than refreshing a chain it cannot hand to that session's Claude Code.
/// `never_refresh` panics if the refusal is ever lifted here.
#[cfg(target_os = "macos")]
#[test]
fn gate_installs_as_is_under_a_live_session_on_macos() {
    let _home = HomeSandbox::new();
    let name = "test-gate-live-session-macos";
    let file = arm_live_session(name);
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));
    assert!(
        matches!(
            ensure_installable(
                &handle,
                &crate::profile::ProfileName::from(name),
                never_refresh
            ),
            AuthGate::Ready
        ),
        "macOS must install as-is rather than sign the session out"
    );

    drop(file);
}

/// Expired-but-refreshable → rotated tokens minted, persisted, installed.
#[test]
fn gate_refreshes_expiring_token_and_installs() {
    let _home = HomeSandbox::new();
    let name = "test-gate-refreshable";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Refreshed
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    let p = cfg
        .find(&crate::profile::ProfileName::from(name))
        .expect("profile");
    assert_eq!(
        p.access_token(),
        Some("at-new"),
        "rotated access token stored"
    );
    assert_eq!(
        p.refresh_token(),
        Some("rt-new"),
        "rotated refresh token stored"
    );
    assert!(
        !cfg.is_auth_broken(&crate::profile::ProfileName::from(name)),
        "a successful refresh is not broken"
    );
}

/// Refresh rejected as invalid → switch refused + profile quarantined.
#[test]
fn gate_invalid_refresh_marks_broken_and_refuses() {
    let _home = HomeSandbox::new();
    let name = "test-gate-invalid";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-revoked"),
        Some(past_expiry()),
    )));
    let refresher =
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid(TokenFailure::Status(400)));
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        cfg.is_auth_broken(&crate::profile::ProfileName::from(name)),
        "a revoked refresh token quarantines the profile"
    );
}

/// A transient failure refuses the switch but does NOT quarantine the account.
#[test]
fn gate_transient_refresh_does_not_quarantine() {
    let _home = HomeSandbox::new();
    let name = "test-gate-transient";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-ok"),
        Some(past_expiry()),
    )));
    let refresher =
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Transient(TokenFailure::Transport));
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Transient(_)
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        !cfg.is_auth_broken(&crate::profile::ProfileName::from(name)),
        "a network blip must not quarantine a healthy account"
    );
}

/// An expiring OAuth token with no refresh token is unrecoverable → quarantined.
#[test]
fn gate_expiring_without_refresh_token_is_broken() {
    let _home = HomeSandbox::new();
    let name = "test-gate-no-rt";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        None,
        Some(past_expiry()),
    )));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(cfg.is_auth_broken(&crate::profile::ProfileName::from(name)));
}

/// A standing quarantine overrides a still-valid clock: the chain's last
/// refresh terminally failed, so a future `expires_at` proves nothing
/// (server-side revocation outlives the stored clock). The gate must route a
/// flagged profile through the refresher instead of installing the dead token
/// as `Ready` — the hole that let CLI/MCP disagree with the TUI's flag-only
/// refusal. A recovered chain (external re-login) lifts the flag on the way
/// through.
#[test]
fn gate_flagged_profile_refreshes_despite_a_valid_clock() {
    let _home = HomeSandbox::new();
    let name = "test-gate-flagged-recovers";
    let mut config = oauth_config(name, Some("rt-relogin"), Some(future_expiry()));
    config.set_auth_broken(&crate::profile::ProfileName::from(name), true);
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-recovered".to_string(),
            refresh_token: "rt-recovered".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Refreshed
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        !cfg.is_auth_broken(&crate::profile::ProfileName::from(name)),
        "a recovered chain lifts the quarantine on the way through the gate"
    );
    let p = cfg
        .find(&crate::profile::ProfileName::from(name))
        .unwrap_or_else(|| panic!("profile"));
    assert_eq!(p.access_token(), Some("at-recovered"));
}

/// Same flagged shape with a genuinely dead chain: the gate confirms `Broken`
/// (refusal + login hint), never a silent `Ready` install of the revoked pair.
#[test]
fn gate_flagged_profile_with_a_dead_chain_stays_broken() {
    let _home = HomeSandbox::new();
    let name = "test-gate-flagged-dead";
    let mut config = oauth_config(name, Some("rt-revoked"), Some(future_expiry()));
    config.set_auth_broken(&crate::profile::ProfileName::from(name), true);
    let handle = Arc::new(RankedMutex::new(config));
    let refresher =
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid(TokenFailure::Status(400)));
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        cfg.is_auth_broken(&crate::profile::ProfileName::from(name)),
        "a dead chain keeps the quarantine"
    );
}

/// A 2xx token-endpoint body that fails to deserialize still holds the live
/// access+refresh tokens, so `token_parse_error` must surface only the serde
/// error + HTTP status + body length — never the token values. Asserted against
/// `log_detail`, the WIDEST rendering: the user-facing one withholds strictly
/// more, so a leak that clears this clears the toast too.
#[test]
fn token_parse_error_redacts_the_2xx_body() {
    // Missing `expires_in` → fails to parse into TokenResponse, but the body
    // carries live-looking tokens.
    let body =
        r#"{"access_token":"sk-ant-oat01-SECRETLEAK","refresh_token":"sk-ant-ort01-SECRETLEAK"}"#;
    // Avoid `.expect_err` so `TokenResponse` need not derive `Debug` — a token-
    // bearing struct with a `Debug` impl is its own leak surface.
    let err = match serde_json::from_str::<TokenResponse>(body) {
        Ok(_) => panic!("2xx body without expires_in must fail to parse into TokenResponse"),
        Err(e) => e,
    };
    let msg = super::token_parse_error(&err, 200, body.len()).log_detail();

    assert!(
        !msg.contains("SECRETLEAK"),
        "no token value substring may appear in the error: {msg}"
    );
    assert!(
        !msg.contains("access_token\":\""),
        "raw body must not be echoed: {msg}"
    );
    assert!(msg.contains("200"), "HTTP status is reported: {msg}");
    assert!(
        msg.contains(&body.len().to_string()),
        "body length is reported: {msg}"
    );
    // Locks the value-free channel: the message reports the failure *position*,
    // never the serde Display `{e}` (which could echo an offending scalar).
    assert!(
        msg.contains("column"),
        "the parse position (not the serde value) is reported: {msg}"
    );
}

// ── refresh_rejection_is_terminal (the 400/401/403 truth table) ─────────────

/// 400/403 are terminal ONLY with a confirming `invalid_grant` body. The token
/// endpoint answers a dead refresh token with the flat OAuth2 envelope
/// (`{"error": "invalid_grant", …}`) but answers a request IT can't parse with
/// Anthropic's nested API envelope (`invalid_request_error`) under the same
/// 400 — and a WAF/geo/challenge block answers 403 with neither. Quarantining
/// without the confirmation takes a healthy account out of rotation, and when
/// the cause is our own request shape it takes EVERY account at once. 401 stays
/// terminal regardless of body (some proxies answer it for a dead token).
/// Bodies are real bytes captured from the live endpoint.
#[test]
fn refresh_rejection_terminal_truth_table() {
    // Dead refresh token — the flat OAuth2 envelope.
    assert!(refresh_rejection_is_terminal(
        400,
        r#"{"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}"#
    ));
    assert!(refresh_rejection_is_terminal(
        403,
        r#"{"error":"invalid_grant"}"#
    ));
    assert!(refresh_rejection_is_terminal(401, "unauthorized"));

    // Our request shape is wrong, not the token: Anthropic's API envelope under
    // a 400. Quarantining on these would flag every profile in the chain for a
    // client-side bug, each recoverable only by a manual re-login.
    for body in [
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Client with id 00000000-0000-0000-0000-000000000000 not found"},"request_id":"req_x"}"#,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid request format"},"request_id":"req_x"}"#,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unsupported grant_type: not_a_grant"},"request_id":"req_x"}"#,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid JSON body"},"request_id":"req_x"}"#,
    ] {
        assert!(
            !refresh_rejection_is_terminal(400, body),
            "a request-shape 400 must not quarantine: {body}"
        );
    }

    assert!(!refresh_rejection_is_terminal(
        403,
        "<html>Access denied by security policy</html>"
    ));
    assert!(!refresh_rejection_is_terminal(429, "rate limited"));
    assert!(!refresh_rejection_is_terminal(500, "internal error"));
}

// ── canonicalize_scopes (refresh `scope` byte-parity with Claude Code) ────────

/// CC emits the refresh `scope` in a fixed order regardless of how the
/// credential file stored the granted scopes. Reorder to
/// that canonical order, preserving the exact granted set.
#[test]
fn canonicalize_scopes_matches_claude_code_order() {
    // A credential's stored order (as seen on real Pro/Max profiles) reorders to
    // the canonical CC wire order.
    assert_eq!(
        canonicalize_scopes(
            "user:file_upload user:inference user:mcp_servers user:profile user:sessions:claude_code"
        ),
        "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
    );
    // The already-canonical fallback string is a no-op.
    assert_eq!(
        canonicalize_scopes(REFRESH_SCOPES_FALLBACK),
        REFRESH_SCOPES_FALLBACK
    );
    // `org:create_api_key` (present only on some credentials) sorts to the front.
    assert_eq!(
        canonicalize_scopes("user:profile org:create_api_key"),
        "org:create_api_key user:profile"
    );
    // An unrecognized scope is preserved (never dropped), appended after the
    // known ones, so the set is never altered — only the order.
    assert_eq!(
        canonicalize_scopes("user:weird_future_scope user:profile"),
        "user:profile user:weird_future_scope"
    );
    // Extra whitespace collapses to single spaces.
    assert_eq!(
        canonicalize_scopes("  user:profile   user:inference  "),
        "user:profile user:inference"
    );
}

// `live_login_is_foreign` gates the rotation→Keychain mirror (rotation
// coherence, #1): the mirror must still fire when the live `.credentials.json`
// is merely a stale regular-file copy of OUR OWN pre-rotation pair (Claude
// Code's Keychain mirror, one step behind), and must NOT fire over a login
// clauth doesn't own (a real CC re-login into some other account).
#[cfg(target_os = "macos")]
mod keychain_mirror_gate {
    use crate::testutil::HomeSandbox;

    fn creds(access: &str) -> crate::profile::ClaudeCredentials {
        crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn save_profile_with(name: &str, access: &str) {
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds(access));
        crate::profile::save_profile(&p).expect("save profile");
    }

    fn write_live_file(access: &str) {
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, serde_json::to_vec(&creds(access)).unwrap()).unwrap();
    }

    #[test]
    fn missing_live_file_is_not_foreign() {
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        assert!(!super::live_login_is_foreign(
            &crate::profile::ProfileName::from("alpha"),
            "old-token"
        ));
    }

    #[test]
    fn live_file_matching_stored_pair_is_not_foreign() {
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("new-token");
        assert!(!super::live_login_is_foreign(
            &crate::profile::ProfileName::from("alpha"),
            "old-token"
        ));
    }

    #[test]
    fn stale_mirror_of_own_pre_rotation_pair_is_not_foreign() {
        // The case the gate exists FOR: CC's regular-file mirror still holds the
        // pair this rotation just superseded. Diverged by classification, but it
        // is our own chain one step behind — the Keychain mirror must proceed.
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("old-token");
        assert!(!super::live_login_is_foreign(
            &crate::profile::ProfileName::from("alpha"),
            "old-token"
        ));
    }

    #[test]
    fn unrelated_live_login_is_foreign() {
        // A real CC re-login into some other account: matches neither the new
        // nor the pre-rotation pair — never overwrite it.
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("someone-elses-token");
        assert!(super::live_login_is_foreign(
            &crate::profile::ProfileName::from("alpha"),
            "old-token"
        ));
    }
}

// ── try_adopt_live_rotation (rotation coherence, &crate::profile::ProfileName::from(the adopt-don't-race half)) ──
//
// The running claude and clauth hold ONE single-use refresh family; when CC
// rotates first, its file mirror (~/.claude/.credentials.json) carries the
// fresher pair. Adopting it — identity-guarded — replaces racing for the
// chain. All offline: identity is injected, the "mirror" is a sandboxed file.
// NOT macOS-gated, and since the call sites lost their Keychain term there is no
// longer a platform-specific gate anywhere on this path: the identity gate and
// the expiry-monotonicity re-check must hold (and run in CI) on every OS.
mod adopt_live_rotation {
    use super::*;
    use crate::lockorder::RankedMutex;
    use crate::testutil::HomeSandbox;
    use std::sync::Arc;

    /// The per-profile rotation lock `try_adopt_live_rotation` demands proof
    /// of (production callers hold it across the whole rotation leg).
    fn guard(name: &str) -> crate::runtime::RotationGuard {
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
            .expect("rotation guard")
    }

    fn creds_with(access: &str, expires_at: Option<i64>) -> crate::profile::ClaudeCredentials {
        crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: access.to_string(),
                refresh_token: Some(format!("{access}-refresh")),
                expires_at,
                scopes: None,
                subscription_type: None,
            }),
        }
    }

    fn past_expiry() -> i64 {
        crate::usage::now_ms() as i64 - 60_000
    }

    fn future_expiry() -> i64 {
        crate::usage::now_ms() as i64 + 3_600_000
    }

    /// Active profile persisted to disk (classify reads the file layer) with a
    /// stored pair ("at-old"), plus a DIVERGED live regular file holding the
    /// mirror pair ("at-mirror"). Resets the stored-token probe suppression:
    /// every test shares the "at-old" token, so a suppression recorded by one
    /// would bleed into the next.
    fn setup(name: &str, stored_expiry: i64, mirror_expiry: i64) -> crate::profile::ConfigHandle {
        crate::oauth::reset_stored_probe_suppression();
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds_with("at-old", Some(stored_expiry)));
        crate::profile::save_profile(&p).expect("save profile");
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: vec![p],
        };
        config.state.profiles = vec![name.into()];
        config.state.active_profile = Some(name.into());
        crate::profile::save_app_state(&config.state).expect("persist state");
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with("at-mirror", Some(mirror_expiry))).unwrap(),
        )
        .unwrap();
        Arc::new(RankedMutex::new(config))
    }

    fn stored_access(handle: &crate::profile::ConfigHandle, name: &str) -> String {
        handle
            .lock()
            .unwrap()
            .find(&crate::profile::ProfileName::from(name))
            .and_then(|p| p.access_token().map(str::to_string))
            .expect("stored access token")
    }

    #[test]
    fn adopts_a_fresher_same_account_pair() {
        let _home = HomeSandbox::new();
        let name = "adopt-ok";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        // The adopted pair is returned so the caller syncs its TokenList —
        // without it, the next poll runs on the superseded entry.
        assert_eq!(
            adopted,
            Some(("at-mirror".into(), Some("at-mirror-refresh".into())))
        );
        assert_eq!(stored_access(&handle, name), "at-mirror");
        // The identity anchor is cached for future dead-store adopts.
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE
            )
            .as_deref(),
            Some("uuid-1")
        );
    }

    #[test]
    fn refuses_a_live_login_from_a_different_account() {
        let _home = HomeSandbox::new();
        let name = "adopt-foreign";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        // Stored token answers uuid-1; the mirror token answers uuid-2 — a
        // manual CC /login into another account must never be captured.
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|tok| {
                Some(
                    if tok == "at-mirror" {
                        "uuid-2"
                    } else {
                        "uuid-1"
                    }
                    .into(),
                )
            },
        );
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_without_an_identity_anchor() {
        let _home = HomeSandbox::new();
        let name = "adopt-anchorless";
        // Stored token already expired → its own uuid can't be fetched, and no
        // cached anchor exists. Identity unprovable ⇒ refuse.
        let handle = setup(name, past_expiry(), future_expiry());
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|tok| (tok == "at-mirror").then(|| "uuid-1".into()),
        );
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn cached_anchor_allows_adopt_even_with_a_dead_stored_token() {
        let _home = HomeSandbox::new();
        let name = "adopt-cached-anchor";
        let handle = setup(name, past_expiry(), future_expiry());
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(name),
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &"uuid-1".to_string(),
        );
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|tok| (tok == "at-mirror").then(|| "uuid-1".into()),
        );
        assert!(adopted.is_some());
        assert_eq!(stored_access(&handle, name), "at-mirror");
    }

    /// A stored token that is CLOCK-valid but revoked upstream (its probe
    /// returns `None`) with no cached anchor is the per-leg waste the
    /// suppression exists for. The first leg probes and suppresses; a second
    /// leg must not re-spend a `/profile` on the same dead stored token.
    #[test]
    fn a_dead_stored_token_probe_is_suppressed_for_one_window() {
        let _home = HomeSandbox::new();
        let name = "adopt-dead-stored";
        // Stored token clock-valid (so the probe arm runs), mirror strictly
        // fresher (so the gate reaches the identity block).
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let stored_calls = std::cell::Cell::new(0usize);
        let mirror_calls = std::cell::Cell::new(0usize);
        let identity = |tok: &str| {
            if tok == "at-old" {
                stored_calls.set(stored_calls.get() + 1);
                None
            } else {
                mirror_calls.set(mirror_calls.get() + 1);
                Some("uuid-1".into())
            }
        };

        let first = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &identity,
        );
        assert_eq!(first, None, "identity unprovable → refuse");
        assert_eq!(
            stored_calls.get(),
            1,
            "the first leg probes the stored token"
        );
        assert_eq!(
            mirror_calls.get(),
            0,
            "a missing expected identity short-circuits before the mirror is probed"
        );

        let second = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &identity,
        );
        assert_eq!(second, None);
        assert_eq!(
            stored_calls.get(),
            1,
            "a second leg must not re-spend a /profile on the same dead stored token"
        );
    }

    /// The suppression is a TTL, never a permanent `None`. Once the window
    /// lapses, the stored token is probed again, so a transient failure stays
    /// retryable.
    #[test]
    fn a_suppressed_stored_probe_retries_after_the_ttl_lapses() {
        let _home = HomeSandbox::new();
        let name = "adopt-ttl-lapse";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let stored_calls = std::cell::Cell::new(0usize);
        let identity = |tok: &str| {
            if tok == "at-old" {
                stored_calls.set(stored_calls.get() + 1);
                None
            } else {
                Some("uuid-1".into())
            }
        };

        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &identity
            ),
            None
        );
        assert_eq!(stored_calls.get(), 1, "first leg probes and suppresses");

        set_stored_probe_not_before_for_test(
            &crate::usage::identity_key("at-old"),
            crate::usage::now_ms() - 1,
        );
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &identity
            ),
            None
        );
        assert_eq!(
            stored_calls.get(),
            2,
            "after the TTL lapses the stored token is probed again"
        );
    }

    #[test]
    fn refuses_a_stale_or_equal_mirror() {
        let _home = HomeSandbox::new();
        let name = "adopt-stale";
        // Mirror expiry equals the store's — nothing fresher to adopt.
        let expiry = future_expiry();
        let handle = setup(name, expiry, expiry);
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_when_not_the_active_profile() {
        let _home = HomeSandbox::new();
        let name = "adopt-inactive";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        handle.lock().unwrap().state.active_profile = None;
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_a_blank_identity() {
        // A present-but-blank uuid is shape drift, not an identity — two
        // blanks matching each other must never prove the tokens are the same
        // account.
        let _home = HomeSandbox::new();
        let name = "adopt-blank-id";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("  ".into()),
        );
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    /// An adopted pair proves the chain is alive, so a standing `auth_broken`
    /// is stale — same lift as the scheduler's `carry_external_rotation`.
    /// Without it, an active recovered by a CC-side re-login stays excluded
    /// from the fallback walk and refused as a switch target until a manual
    /// `clauth login` (the cross-PR seam the adopt PR deferred to the rebase).
    #[test]
    fn adopting_a_live_rotation_lifts_a_stale_quarantine() {
        let _home = HomeSandbox::new();
        let name = "adopt-quarantined";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        handle
            .lock()
            .unwrap()
            .set_auth_broken(&crate::profile::ProfileName::from(name), true);
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        assert!(adopted.is_some(), "the fresher same-account pair adopts");
        assert!(
            !handle
                .lock()
                .unwrap()
                .is_auth_broken(&crate::profile::ProfileName::from(name)),
            "an adopted (alive) chain lifts a stale quarantine"
        );
    }

    /// CLA-SPLIT: a session-token profile's live slot holds its STATIC token,
    /// so `classify_credentials_link` compares against `session-token.json`
    /// while the adopt's expiry gate and its write both target the
    /// clauth-private usage pair in `credentials.json` — two different
    /// surfaces. A live slot that stops holding the static token classifies
    /// `Diverged`, and without the guard the adopt overwrites the usage chain
    /// with the live login. Same invariant
    /// `snapshot_active_credentials_unchecked` carries for every other sink.
    #[test]
    fn refuses_a_session_token_profile() {
        let _home = HomeSandbox::new();
        let name = "adopt-session-token";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let mint = format!("sk-ant-oat01-{}", "x".repeat(40));
        crate::claude::write_session_token(
            &crate::profile::ProfileName::from(name),
            &mint,
            crate::usage::now_ms() as i64,
        )
        .expect("write session token");
        assert_eq!(
            crate::claude::classify_credentials_link(&crate::profile::ProfileName::from(name))
                .expect("classify"),
            crate::claude::LinkState::Diverged,
            "fixture precondition: the live slot no longer holds the static token"
        );

        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        assert_eq!(adopted, None);
        assert_eq!(
            stored_access(&handle, name),
            "at-old",
            "the clauth-private usage pair must survive"
        );
        let on_disk: crate::profile::ClaudeCredentials = crate::profile::read_json_file(
            &crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
                .expect("profile dir")
                .join("credentials.json"),
        )
        .expect("read stored");
        assert_eq!(on_disk.refresh_token(), Some("at-old-refresh"));
    }

    /// Off macOS the hand-back path is the SYMLINK, and CC's routine refresh
    /// renames a temp sibling over the live slot — `rename(2)` acts on the
    /// link, so the very divergence this adopt fires on has already destroyed
    /// it. Adopting the pair alone leaves a regular file that now classifies
    /// `LinkedTo` (the access tokens match), so nothing relinks it, and the
    /// next rotation writes only the store: the running claude never sees the
    /// pair and signs out at the stale token's expiry. The symlink assert is
    /// the discriminating one — `classify` reads `LinkedTo` either way.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn adopting_relinks_the_live_slot_so_the_next_rotation_reaches_claude() {
        let _home = HomeSandbox::new();
        let name = "adopt-relink";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let live = crate::profile::claude_dir()
            .expect("claude dir")
            .join(".credentials.json");
        assert!(
            !live
                .symlink_metadata()
                .expect("live slot")
                .file_type()
                .is_symlink(),
            "fixture precondition: CC's rename leaves a regular file, not our link"
        );

        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        assert!(adopted.is_some(), "the fresher same-account pair adopts");

        assert!(
            live.symlink_metadata()
                .expect("live slot")
                .file_type()
                .is_symlink(),
            "the adopt must restore the symlink, or clauth's next rotation never reaches CC"
        );
        assert_eq!(
            crate::claude::classify_credentials_link(&crate::profile::ProfileName::from(name))
                .expect("classify"),
            crate::claude::LinkState::LinkedTo,
        );
    }

    /// A live file carrying `mcpOAuth` survives the adopt with that key
    /// intact. Before the carry the relink pointed the live slot at a store
    /// holding the login alone, and Claude Code lost every MCP-server session on
    /// a leg that runs unattended roughly every 8 hours.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_adopt_keeps_the_live_files_mcp_oauth() {
        let _home = HomeSandbox::new();
        let name = "adopt-mcp";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);

        // Claude Code authenticated an MCP server before it rotated its login.
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        let mut body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&live).unwrap()).unwrap();
        body["mcpOAuth"] = serde_json::json!({ "linear": { "accessToken": "mock-linear" } });
        std::fs::write(&live, serde_json::to_vec(&body).unwrap()).unwrap();

        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );
        assert!(
            adopted.is_some(),
            "the same-account fresher pair is adopted"
        );

        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&live).unwrap()).unwrap();
        assert_eq!(
            after["claudeAiOauth"]["accessToken"], "at-mirror",
            "the adopted login is what the live slot serves"
        );
        assert_eq!(
            after["mcpOAuth"]["linear"]["accessToken"], "mock-linear",
            "the MCP-server login survives the post-adopt relink"
        );
    }

    /// The adopt persists a whole profile, so it must not recreate the directory
    /// of a profile deleted while the rotation guard was held across the identity
    /// HTTP window. `name` stays active on the stale handle but is already gone
    /// on disk; the fresh membership read refuses before `save_profile`.
    #[test]
    fn adopt_does_not_resurrect_a_deleted_profile() {
        let _home = HomeSandbox::new();
        let name = "adopt-resurrect";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);

        // CLI account mutation on a config where `name` is NOT active, so the
        // delete leaves the live mirror file alone (the adopt's classify gate
        // still reads it as Diverged rather than Missing).
        let mut disk = crate::profile::load_config().expect("load disk config");
        disk.state.active_profile = None;
        let rotation = guard(name);
        crate::actions::delete_profile(
            &mut disk,
            &crate::profile::ProfileName::from(name),
            false,
            &rotation,
        )
        .expect("delete");
        assert!(
            !crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
                .expect("dir")
                .exists(),
            "fixture precondition: the delete removed the directory"
        );

        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &rotation,
            &|_| Some("uuid-1".into()),
        );
        assert_eq!(adopted, None, "a deleted profile must not be adopted");
        assert!(
            !crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
                .expect("dir")
                .exists(),
            "the deleted profile's directory must stay deleted"
        );
    }
}

// ── post-guard re-read (the pre-RotationGuard token-snapshot race) ────────────
//
// Between the guard-less pre-check and RotationGuard acquisition a sibling
// rotation can spend the single-use refresh token and persist a new pair;
// refreshing from a pre-guard snapshot would 400 and wrongly quarantine a
// healthy login. `gate_under_guard` therefore takes NO token arguments — its
// decisions can only come from state read under the guard. These pin that
// boundary directly.

/// The rotation lock the guard leg demands proof of (production callers hold
/// it across the whole refresh window).
fn gate_guard(name: &str) -> crate::runtime::RotationGuard {
    crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("rotation guard")
}

/// Persist a peer's rotation to the on-disk profile store — the state a
/// cross-process rotation leaves behind for `adopt_disk_rotation` to find.
fn save_disk_profile(name: &str, refresh: &str, expires_at: Option<i64>) {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-disk".to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at,
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&p).expect("save disk profile");
}

/// Stored pair already fresh when the guard leg runs (the sibling-refreshed
/// interleave) → Ready, and the old chain is NOT double-spent (the refresher
/// panics if called).
#[test]
fn gate_under_guard_installs_a_sibling_refreshed_pair_as_is() {
    let _home = HomeSandbox::new();
    let name = "test-gate-sibling-refreshed";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-fresh"),
        Some(future_expiry()),
    )));
    assert!(matches!(
        gate_under_guard(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Ready
    ));
}

/// A sidecar repair that failed on the bounded state-flock wait is CONTENTION
/// — another clauth process is busy under ~/.clauth, on macOS possibly across
/// a 20-second Keychain shell-out — and must never render as
/// `SidecarWriteFailed`'s "check permissions", which sends the operator
/// hunting a fault that does not exist. Any other error keeps the fault copy.
#[test]
fn a_state_lock_timeout_reads_as_contention_not_permissions() {
    let busy = anyhow::Error::new(crate::lock::StateLockTimeout::stub())
        .context("quarantine session-token.json");
    let t = sidecar_repair_transient(&crate::profile::ProfileName::from("busy"), &busy);
    assert!(
        t.text().contains("another clauth process holds"),
        "contention names the holder, got: {}",
        t.text()
    );
    assert!(
        !t.text().contains("check permissions"),
        "contention must not prescribe a permissions hunt, got: {}",
        t.text()
    );

    let fault = anyhow::anyhow!("read-only file system").context("write session-token.json");
    let t = sidecar_repair_transient(&crate::profile::ProfileName::from("busy"), &fault);
    assert!(
        t.text().contains("check permissions on ~/.clauth"),
        "a genuine filesystem fault keeps the fault copy, got: {}",
        t.text()
    );
}

/// The install-gate grace IS Claude Code's refresh threshold, shared with the
/// backup-restore verdicts: identical bytes must never read as dead in the
/// backup slot ([`crate::claude::BACKUP_EXPIRY_GRACE_MS`]) and installable in
/// the live one. A mint with three minutes of life sits inside CC's own
/// five-minute refresh window — a refresh-less credential the client is
/// already trying to refresh — so every gate this constant feeds must treat
/// it as expiring.
#[test]
fn the_install_grace_is_ccs_refresh_window_shared_with_the_backup_verdicts() {
    assert_eq!(
        AUTH_GATE_GRACE_MS,
        crate::claude::BACKUP_EXPIRY_GRACE_MS,
        "one number, one home — the two slots must agree on what dead means"
    );
    let now = crate::usage::now_ms() as i64;
    assert!(
        expiring(Some(now + 3 * 60 * 1000), false),
        "three minutes of life is inside CC's five-minute refresh window"
    );
    assert!(
        !expiring(Some(now + 10 * 60 * 1000), false),
        "ten minutes clears the window"
    );
}

/// Still expiring under the guard → the refresher is fed the CURRENTLY stored
/// refresh token, never a caller-supplied snapshot.
#[test]
fn gate_under_guard_spends_the_currently_stored_refresh_token() {
    let _home = HomeSandbox::new();
    let name = "test-gate-current-rt";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-current"),
        Some(past_expiry()),
    )));
    let refresher = |rt: &str, _scopes: Option<&str>| {
        assert_eq!(
            rt, "rt-current",
            "must spend the token read under the guard"
        );
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-next".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        gate_under_guard(
            &handle,
            &crate::profile::ProfileName::from(name),
            refresher,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Refreshed
    ));
}

/// A cross-process peer rotated and persisted while this process held a stale
/// in-memory config snapshot (the CLI and MCP load config from disk once and
/// never reload): under the guard the DISK pair is authoritative. A live disk
/// pair installs as-is — the stale in-memory token is never spent (the
/// refresher panics if called) — and the handle carries the adopted pair.
#[test]
fn gate_under_guard_adopts_a_cross_process_rotation_from_disk() {
    let _home = HomeSandbox::new();
    let name = "test-gate-disk-adopt";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(past_expiry()),
    )));
    save_disk_profile(name, "rt-peer", Some(future_expiry()));
    assert!(matches!(
        gate_under_guard(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Ready
    ));
    assert_eq!(
        handle
            .lock()
            .unwrap()
            .find(&crate::profile::ProfileName::from(name))
            .unwrap()
            .refresh_token(),
        Some("rt-peer"),
        "the adopted disk pair must replace the stale in-memory snapshot"
    );
}

/// Peer-rotated pair that is ITSELF already expiring again: the refresher must
/// be fed the disk refresh token — spending the stale in-memory one would 400
/// (already spent by the peer) and wrongly quarantine a healthy login.
#[test]
fn gate_under_guard_spends_the_disk_pair_after_an_external_rotation() {
    let _home = HomeSandbox::new();
    let name = "test-gate-disk-spend";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(past_expiry()),
    )));
    save_disk_profile(name, "rt-peer", Some(past_expiry()));
    let refresher = |rt: &str, _scopes: Option<&str>| {
        assert_eq!(rt, "rt-peer", "must spend the disk pair, not the snapshot");
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-next".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        gate_under_guard(
            &handle,
            &crate::profile::ProfileName::from(name),
            refresher,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Refreshed
    ));
}

/// A wedged state flock during the adoption leg must not send the stale
/// in-memory pair into the refresh: the gate yields Transient with the
/// contention copy, the refresher never runs (the already-spent single-use
/// token stays unspent), and the disk pair is not half-adopted. The wedge is
/// a second open file description holding the flock — which conflicts exactly
/// as a second process would (same pose as the teardown wedge in
/// `runtime.rs`) — and the deadline override keeps the wait off the real 25 s.
#[test]
fn gate_under_guard_refuses_when_the_adoption_flock_is_wedged() {
    let _home = HomeSandbox::new();
    let name = "test-gate-flock-wedge";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(past_expiry()),
    )));
    save_disk_profile(name, "rt-peer", Some(future_expiry()));

    let lock_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join(crate::lock::LOCK_FILENAME);
    let holder = crate::profile::open_state_file(&lock_path).expect("open holder");
    holder.lock().expect("hold the flock");

    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(50)));
    let gate = gate_under_guard(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
        &gate_guard(name),
        AUTH_GATE_GRACE_MS,
    );
    crate::lock::set_state_lock_timeout_override(None);
    drop(holder);

    let AuthGate::Transient(t) = gate else {
        panic!("a wedged adoption flock must refuse the gate");
    };
    assert!(
        t.text().contains("another clauth process holds"),
        "the timeout reads as contention, got: {}",
        t.text()
    );
    assert_eq!(
        handle
            .lock()
            .unwrap()
            .find(&crate::profile::ProfileName::from(name))
            .unwrap()
            .refresh_token(),
        Some("rt-stale"),
        "a failed adoption must not half-land the disk pair"
    );
}

/// The adoption flock's failure maps contention and fault to different
/// verdicts — the same split as the sidecar repair leg. A timeout is a busy
/// sibling (retry), never a "check permissions" hunt; a genuine IO fault
/// keeps the fault copy.
#[test]
fn an_adoption_flock_failure_splits_contention_from_fault() {
    let busy =
        anyhow::Error::new(crate::lock::StateLockTimeout::stub()).context("adopt disk rotation");
    let t = adopt_lock_transient(&crate::profile::ProfileName::from("busy"), &busy);
    assert!(
        t.text().contains("another clauth process holds"),
        "contention names the holder, got: {}",
        t.text()
    );
    assert!(
        !t.text().contains("check permissions"),
        "contention must not prescribe a permissions hunt, got: {}",
        t.text()
    );

    let fault = anyhow::anyhow!("read-only file system").context("open state lock");
    let t = adopt_lock_transient(&crate::profile::ProfileName::from("busy"), &fault);
    assert!(
        t.text().contains("check permissions on ~/.clauth"),
        "a genuine filesystem fault keeps the fault copy, got: {}",
        t.text()
    );
}

/// A differing disk pair proves the chain is alive, so a standing in-memory
/// quarantine is stale and lifts (same rationale as the scheduler's
/// `carry_external_rotation`): the gate proceeds from the adopted pair
/// instead of refusing a recovered login.
#[test]
fn gate_under_guard_disk_adoption_lifts_a_stale_quarantine() {
    let _home = HomeSandbox::new();
    let name = "test-gate-disk-quarantine";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-stale"),
        Some(future_expiry()),
    )));
    handle
        .lock()
        .unwrap()
        .set_auth_broken(&crate::profile::ProfileName::from(name), true);
    save_disk_profile(name, "rt-peer", Some(future_expiry()));
    assert!(matches!(
        gate_under_guard(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Ready
    ));
    assert!(
        !handle
            .lock()
            .unwrap()
            .is_auth_broken(&crate::profile::ProfileName::from(name)),
        "an adopted (alive) chain lifts a stale quarantine"
    );
}

// ── token-endpoint request bodies (platform.claude.com wire parity) ──────────
//
// The exact JSON body CC's axios client posts to platform.claude.com/v1/oauth/
// token, captured 2026-07-14 against CC 2.1.209. Field
// set is compared order-independently (a JSON body's key order is not a wire
// signal); `scope` value + canonical order carry their own assertions.

#[test]
fn refresh_body_matches_cc_field_set_and_scope() {
    // 5 granted scopes (no org:create_api_key, as every real Max/Pro login
    // grants) → the 5-scope canonical string CC echoed on the wire.
    let body = refresh_body(
        "RT",
        Some("user:file_upload user:inference user:mcp_servers user:profile user:sessions:claude_code"),
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["client_id", "grant_type", "refresh_token", "scope"]);
    assert_eq!(v["grant_type"], "refresh_token");
    assert_eq!(v["refresh_token"], "RT");
    assert_eq!(v["client_id"], CLIENT_ID);
    assert_eq!(
        v["scope"],
        "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
    );
}

#[test]
fn exchange_body_matches_cc_field_set() {
    let body = exchange_body(
        "CODE",
        "VERIFIER",
        "http://localhost:1234/callback",
        "STATE",
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "client_id",
            "code",
            "code_verifier",
            "grant_type",
            "redirect_uri",
            "state"
        ]
    );
    assert_eq!(v["grant_type"], "authorization_code");
    assert_eq!(v["code"], "CODE");
    assert_eq!(v["code_verifier"], "VERIFIER");
    assert_eq!(v["redirect_uri"], "http://localhost:1234/callback");
    assert_eq!(v["client_id"], CLIENT_ID);
}

#[test]
fn token_endpoint_constants_match_cc_wire() {
    // CC's axios client on platform.claude.com/v1/oauth/token, verified on the
    // wire 2026-07-14. If CC's bundle bumps axios, re-capture and update here.
    assert_eq!(TOKEN_USER_AGENT, "axios/1.15.2");
    assert_eq!(TOKEN_ACCEPT, "application/json, text/plain, */*");
    assert_eq!(TOKEN_ENDPOINT, "https://platform.claude.com/v1/oauth/token");
}

// ── kick emits Claude Code's /v1/messages client shape (wire parity) ─────────
//
// The window-priming POST carries CC's SDK instrumentation + full beta set
// (captured 2026-07-14, CC 2.1.209). Drives the REAL
// kick_to builder against a loopback listener and asserts the emitted bytes.
// Deliberately partial vs a real stainless client (no host-derived
// arch/os/runtime-version, no per-session ids) — asserted here so the boundary
// is explicit, not accidental.

fn kick_header<'a>(req: &'a str, name: &str) -> Option<&'a str> {
    let want = format!("{}:", name.to_ascii_lowercase());
    req.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&want))
        .and_then(|l| l.split_once(':').map(|x| x.1))
        .map(str::trim)
}

#[test]
fn kick_emits_cc_message_wire_shape() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let req = drain_http_request(&mut sock);
        let _ =
            sock.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}");
        let _ = sock.shutdown(std::net::Shutdown::Write);
        String::from_utf8_lossy(&req).into_owned()
    });

    crate::usage::reset_request_slots(); // don't sleep out the 5s host spacing
    let url = format!("http://127.0.0.1:{port}/v1/messages?beta=true");
    let _ = kick_to(&url, "TESTTOKEN");
    let req = server.join().unwrap();

    assert!(
        req.starts_with("POST /v1/messages?beta=true "),
        "kick keeps the ?beta=true query, got {:?}",
        req.lines().next()
    );
    assert_eq!(kick_header(&req, "content-type"), Some("application/json"));
    assert_eq!(kick_header(&req, "accept"), Some("application/json"));
    assert_eq!(kick_header(&req, "authorization"), Some("Bearer TESTTOKEN"));
    assert_eq!(kick_header(&req, "anthropic-version"), Some("2023-06-01"));
    // the fingerprint-critical header: the kick must identify as claude-cli, not
    // leak ureq's default UA (it silently did until 2026-07-14).
    let ua = kick_header(&req, "user-agent").unwrap_or("");
    assert!(
        ua.starts_with("claude-cli"),
        "kick UA must be claude-cli, got {ua:?}"
    );
    assert!(!ua.contains("ureq"), "kick must not leak ureq's default UA");
    assert_eq!(
        kick_header(&req, "anthropic-beta"),
        Some(KICK_ANTHROPIC_BETA)
    );
    assert_eq!(
        kick_header(&req, "anthropic-dangerous-direct-browser-access"),
        Some("true")
    );
    assert_eq!(kick_header(&req, "x-app"), Some("cli"));
    assert_eq!(kick_header(&req, "x-stainless-lang"), Some("js"));
    assert_eq!(kick_header(&req, "x-stainless-runtime"), Some("node"));
    assert_eq!(
        kick_header(&req, "x-stainless-package-version"),
        Some(KICK_STAINLESS_PACKAGE_VERSION)
    );
    // the partial-set boundary: these are intentionally NOT sent
    assert_eq!(kick_header(&req, "x-stainless-os"), None);
    assert_eq!(kick_header(&req, "x-stainless-arch"), None);
    assert_eq!(kick_header(&req, "x-claude-code-session-id"), None);
}

#[test]
fn kick_beta_is_ccs_full_six_value_list() {
    // Distinct from the single oauth-2025-04-20 on /usage; CC sends 6 on messages.
    assert_eq!(
        KICK_ANTHROPIC_BETA,
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
    );
    assert_eq!(KICK_ANTHROPIC_BETA.split(',').count(), 6);
    assert!(KICK_ANTHROPIC_BETA.starts_with("oauth-2025-04-20,"));
}

/// The pure header distillation behind a kick 429: `rejected` keys on
/// `unified-status`, and the ceiling is the LATER of `unified-reset` and
/// `retry-after`, with an already-past reset dropped. The ceiling is an upper
/// bound the scheduler decays toward, never a schedule (2026-07-15: the
/// limiter relented 2.4h before its own advertised reset).
#[test]
fn kick_rate_limit_distills_status_reset_and_retry_after() {
    let now = 1_784_000_000;

    let both = kick_rate_limit_at(
        Some("rejected"),
        Some(&(now + 9_000).to_string()),
        Some("120"),
        now,
    );
    assert!(both.rejected);
    assert_eq!(
        both.until_epoch_secs,
        Some(now + 9_000),
        "reset later than retry-after → reset wins"
    );

    let after_wins = kick_rate_limit_at(None, Some(&(now + 60).to_string()), Some("300"), now);
    assert!(!after_wins.rejected);
    assert_eq!(
        after_wins.until_epoch_secs,
        Some(now + 300),
        "retry-after later than reset → retry-after wins"
    );

    let past_reset = kick_rate_limit_at(Some("allowed"), Some(&(now - 5).to_string()), None, now);
    assert!(!past_reset.rejected, "non-rejected status stays false");
    assert_eq!(
        past_reset.until_epoch_secs, None,
        "an already-past reset is no ceiling"
    );

    let bare = kick_rate_limit_at(None, None, None, now);
    assert!(!bare.rejected);
    assert_eq!(bare.until_epoch_secs, None);

    // `retry-after: 0` (or any not-in-the-future hint) must yield NO ceiling,
    // not a ceiling of `now` — a now-ceiling collapses the backoff clamp to
    // "always due" and re-kicks every tick, the exact trap `next_slot_deferral`
    // already guards on the /usage side.
    let zero = kick_rate_limit_at(Some("rejected"), None, Some("0"), now);
    assert!(zero.rejected);
    assert_eq!(
        zero.until_epoch_secs, None,
        "retry-after: 0 is no usable ceiling"
    );
}

/// A live kick 429 carries the limiter's own headers out through `KickError`,
/// and `auto_start_kick` (no refresh token → no rotation attempt) surfaces them
/// as `KickResult.blocked` instead of swallowing the outage like it did through
/// the 2026-07-15 incident.
#[test]
fn kick_429_surfaces_limiter_metadata() {
    use std::io::Write;
    use std::net::TcpListener;

    let reset = crate::usage::now_epoch_secs() + 100_000;
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        drain_http_request(&mut sock);
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\n\
             retry-after: 120\r\n\
             anthropic-ratelimit-unified-status: rejected\r\n\
             anthropic-ratelimit-unified-reset: {reset}\r\n\
             connection: close\r\n\
             content-length: 0\r\n\r\n"
        );
        let _ = sock.write_all(response.as_bytes());
        let _ = sock.shutdown(std::net::Shutdown::Write);
    });

    crate::usage::reset_request_slots(); // don't sleep out the 5s host spacing
    let url = format!("http://127.0.0.1:{port}/v1/messages?beta=true");
    let err = kick_to(&url, "TESTTOKEN").expect_err("429 must error");
    server.join().unwrap();

    let KickError::Status(429, Some(rl)) = err else {
        panic!(
            "expected a 429 with limiter metadata, got {}",
            describe_kick_failure(&err)
        );
    };
    assert!(
        rl.rejected,
        "unified-status: rejected must survive the parse"
    );
    assert_eq!(
        rl.until_epoch_secs,
        Some(reset),
        "unified-reset (later than retry-after) is the ceiling"
    );
}

// ── ureq's non-2xx default on the token/kick agent ───────────────────────────

/// The sibling of `non_2xx_arrives_as_ok_so_the_status_branches_stay_reachable`
/// (fetch), for the agent that carries the token endpoint and the window kick.
/// Both agents need `http_status_as_error(false)` and for the same reason: with
/// ureq's default, `kick`'s 401 → rotate-and-retry becomes unreachable and
/// `refresh_result`'s explicit status check never runs, so a dead login is
/// reported as a transport error and never quarantined. Pinned per-agent because
/// the config is per-agent — fetch's flag says nothing about this one's.
#[test]
fn token_agent_surfaces_non_2xx_as_ok_not_a_transport_error() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local_addr").port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        drain_http_request(&mut sock);
        // The shape the real token endpoint answers a dead refresh token with.
        let body = br#"{"error": "invalid_grant"}"#;
        let _ = sock.write_all(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        let _ = sock.write_all(body);
        let _ = sock.shutdown(std::net::Shutdown::Write);
    });

    let url = format!("http://127.0.0.1:{port}/v1/oauth/token");
    let got = AGENT.post(&url).send("{}");
    let _ = server.join();

    let mut response = got.expect(
        "a 400 must arrive as Ok: refresh_result reads status + body to tell a dead token \
         (invalid_grant) from a rejected request shape, and neither is possible off an Err",
    );
    assert_eq!(response.status().as_u16(), 400);
    assert!(
        response
            .body_mut()
            .read_to_string()
            .expect("read body")
            .contains("invalid_grant"),
        "the body must be readable too — the terminal-vs-transient split keys on it",
    );
}

/// `describe_kick_failure` is the mapping behind the first-kick diagnostic
/// `logline!`: a dead ping (e.g. a rejecting 403) has to name its real
/// status/error so it stops vanishing silently. Pin the shape the log line
/// carries so a status stays "HTTP {status}" and a transport error keeps its
/// own text.
#[test]
fn describe_kick_failure_names_status_and_error() {
    assert_eq!(
        describe_kick_failure(&KickError::Status(403, None)),
        "HTTP 403",
    );
    assert_eq!(
        describe_kick_failure(&KickError::Status(500, None)),
        "HTTP 500",
    );
    let other = describe_kick_failure(&KickError::Other(anyhow::anyhow!("boom")));
    assert!(
        other.contains("boom"),
        "an Other must surface its transport text, got {other:?}",
    );
}

/// CLA-SPLIT: a session-token profile installs its static token as-is — even a
/// standing `auth_broken` quarantine (which now describes the USAGE chain)
/// must not bench the account or spend a refresh.
#[test]
fn gate_session_token_ready_even_when_auth_broken() {
    let _home = HomeSandbox::new();
    let name = "test-gate-session-token";
    let mut config = oauth_config(name, Some("rt-dead"), Some(past_expiry()));
    config.set_auth_broken(&crate::profile::ProfileName::from(name), true);
    // Materialize the profile dir, then the session token beside it.
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "oat-static".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("write session token");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
}

/// CLA-SPLIT: the install arm reads a mint with the SAME grace every other
/// verdict on these bytes uses (`AUTH_GATE_GRACE_MS` = CC's five-minute
/// refresh threshold = the backup-restore rule). Three minutes of life is
/// inside CC's own refresh window — a refresh-less credential the client
/// immediately tries to refresh, signing the session out — so the switch must
/// refuse it exactly where `clauth static-token` calls the identical bytes
/// EXPIRED; ten minutes clears the window and installs. Zero grace here was
/// the one arm that INSTALLS a mint while every other slot called it dead.
#[test]
fn gate_refuses_a_mint_inside_ccs_refresh_window() {
    let _home = HomeSandbox::new();
    let name = "test-gate-mint-window";
    let config = oauth_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let mint = |exp_in_ms: i64| {
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec(&ClaudeCredentials {
                claude_ai_oauth: Some(OAuthToken {
                    access_token: "oat-static".to_string(),
                    refresh_token: None,
                    expires_at: Some(crate::usage::now_ms() as i64 + exp_in_ms),
                    scopes: None,
                    subscription_type: None,
                }),
            })
            .expect("ser"),
        )
        .expect("write session token");
    };
    let handle = Arc::new(RankedMutex::new(config));

    mint(3 * 60 * 1000);
    assert!(
        matches!(
            ensure_installable(
                &handle,
                &crate::profile::ProfileName::from(name),
                never_refresh
            ),
            AuthGate::Broken
        ),
        "three minutes of life is inside CC's refresh window — refused"
    );

    mint(10 * 60 * 1000);
    assert!(
        matches!(
            ensure_installable(
                &handle,
                &crate::profile::ProfileName::from(name),
                never_refresh
            ),
            AuthGate::Ready
        ),
        "ten minutes clears the window — installs as-is"
    );
}

// ── rotate_one_inner, driven offline ─────────────────────────────────────────
//
// This leg feeds both the action-menu single rotate and every `refresh_all`
// worker, so it is the most direct user-reachable path to the macOS sign-out.
// It was unpinned in BOTH directions until now: its decision sits above the
// HTTP call, so nothing that stops short of answering that call can see it.

/// A live-session profile whose stored pair the leg would spend.
fn live_rotate_fixture(name: &str) -> (crate::profile::ConfigHandle, std::fs::File) {
    let pid = arm_live_session(name);
    (
        crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name)),
        pid,
    )
}

/// Off macOS the session shares the credential file, so the leg MUST rotate:
/// it reaches the token endpoint and persists the minted pair.
#[cfg(not(target_os = "macos"))]
#[test]
fn rotate_one_inner_rotates_under_a_live_session() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "rotate-one-live";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| {
        (
            200,
            r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#.to_string(),
        )
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let (config, pid) = live_rotate_fixture(name);
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, rx) = mpsc::channel();

    let result = rotate_one_inner(
        &config,
        &crate::profile::ProfileName::from(name),
        Some(&activity),
        &tx,
    );
    let seen = server.join().expect("listener");

    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string()],
        "the leg must reach the token endpoint off macOS"
    );
    assert!(matches!(result, RotateOutcome::Persisted(true)));
    assert!(
        rx.try_recv().is_ok(),
        "a rotation that ran emits an OpResult"
    );
    drop(pid);
}

/// On macOS it must NOT: spending the chain here strands the running session,
/// whose Claude Code reads a Keychain item clauth cannot write. A skip is
/// silent — `Persisted(false)`, activity Idle, no `OpResult`.
#[cfg(target_os = "macos")]
#[test]
fn rotate_one_inner_does_not_rotate_under_a_live_session_on_macos() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "rotate-one-live-mac";
    // Zero expected requests: reaching the listener at all is the failure.
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (
            200,
            r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                .to_string(),
        )
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let (config, pid) = live_rotate_fixture(name);
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, rx) = mpsc::channel();

    let result = rotate_one_inner(
        &config,
        &crate::profile::ProfileName::from(name),
        Some(&activity),
        &tx,
    );
    let seen = server.join().expect("listener");

    assert!(
        seen.is_empty(),
        "macOS must not spend the chain a live session holds: {seen:?}"
    );
    assert!(matches!(result, RotateOutcome::Persisted(false)));
    assert!(
        is_idle(&activity, &crate::profile::ProfileName::from(name)),
        "a skipped rotation stamps nothing"
    );
    assert!(rx.try_recv().is_err(), "the silent skip emits no OpResult");
    #[allow(clippy::expect_used, reason = "test")]
    let stored = config
        .lock()
        .expect("config lock")
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token().map(str::to_string));
    assert_eq!(stored.as_deref(), Some("at-old"), "the pair is untouched");
    drop(pid);
}

// ── the kick's two stuck-429 arms ────────────────────────────────────────────

/// The kick shares ONE `api.anthropic.com` spacing slot with `/usage` and
/// `/profile`, so a multi-profile window-reset fan-out can't burst
/// `/v1/messages`. The pacing key is the hardcoded origin, not the overridden
/// loopback URL, which is what makes the reservation observable through
/// `EndpointSandbox` at all.
///
/// Asserting the reservation the kick LEAVES, rather than timing a second
/// same-host request through it, is what keeps this free: observing the wait
/// costs a real `REQUEST_SPACING_MS` sleep per run.
#[test]
fn the_kick_reserves_the_shared_anthropic_request_slot() {
    let home = HomeSandbox::new();
    let name = "kick-paced";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (200, "{}".to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    // `EndpointSandbox` clears the slots, so the kick starts from an unreserved
    // host: it waits 0 ms and the reservation it leaves behind is the whole signal.
    assert_eq!(
        crate::usage::reserved_request_slot(crate::usage::ANTHROPIC_ORIGIN),
        None,
        "the probe must start unreserved, or a leftover slot would pass for the kick's"
    );
    let before = crate::usage::now_ms();
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));

    let result = auto_start_kick(
        &config,
        &crate::profile::ProfileName::from(name),
        "at-old",
        Some("rt-old"),
        None,
        None,
    );
    let seen = server.join().expect("listener");

    assert!(result.opened, "the 200 opens the window: {seen:?}");
    assert_eq!(seen.len(), 1, "one kick, nothing else: {seen:?}");
    let slot = crate::usage::reserved_request_slot(crate::usage::ANTHROPIC_ORIGIN);
    assert!(
        slot.is_some_and(|t| t > before),
        "the kick must reserve the shared slot so the next same-host request waits; got {slot:?}"
    );
}

/// A rotation whose persist fails still hands the minted pair back: the refresh
/// already spent the old single-use token, so dropping the new one leaves the
/// caller's snapshot on a dead token that 400s every tick until a restart adopts
/// the staged sidecar. `opened` stays false — the retry kick never runs — and
/// that is exactly why `KickResult` sets `rotated` independently of it.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_kick_rotation_carries_its_pair_back_when_the_persist_fails() {
    let home = HomeSandbox::new();
    let name = "kick-persist-fail";
    // messages 401 → token 200. `max` sits above that so a retry kick would be
    // recorded rather than silently refused a socket.
    let (base, server) = crate::testutil::serve_endpoints(5, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (
                200,
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        } else {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    crate::testutil::block_credentials_write(&crate::profile::ProfileName::from(name));

    let result = auto_start_kick(
        &config,
        &crate::profile::ProfileName::from(name),
        "at-old",
        Some("rt-old"),
        None,
        None,
    );
    let seen = server.join().expect("listener");

    assert!(
        seen.iter().any(|p| p.starts_with("/v1/oauth/token")),
        "the leg must actually rotate before the persist can fail: {seen:?}"
    );
    // Proof the fixture failed the persist where it claims to: the crash-durable
    // sidecar is only cleared after a committed save.
    let pending = crate::profile::profile_subpath(
        &crate::profile::ProfileName::from(name),
        "credentials.json.pending",
    )
    .expect("pending path");
    assert!(
        pending.is_file(),
        "the staged sidecar must survive, or the save never failed"
    );
    assert_eq!(
        result.rotated,
        Some(("at-new".to_string(), Some("rt-new".to_string()))),
        "a minted pair must propagate even when it could not be persisted"
    );
    assert!(!result.opened, "the retry kick never ran");
    assert_eq!(
        seen.iter()
            .filter(|p| p.starts_with("/v1/messages"))
            .count(),
        1,
        "the persist bail returns before the retry kick: {seen:?}"
    );
}

// ── containment: the endpoint's own bytes never reach a refusal ──────────────
//
// `TokenFailure` is the guard, and it is a TYPE rather than a convention on
// purpose: this codebase already broke the convention version. The manual
// rotate path printed `HTTP 400: {"error": "invalid_grant", …}` into a Danger
// toast from four hundred lines away from a sibling switch path that spelled
// the same condition through `format::refresh_transient`. With no `Display`,
// that bypass is a compile error instead of something review has to catch.

use crate::testutil::{NotDisplay as _, NotIntoAnyhow as _, Probe};

/// The structural half of the containment, covering BOTH escape hatches: a
/// `Display` impl makes `{e}` compile again, and an `Into<anyhow::Error>` makes
/// `?` do the same thing one layer up (which is exactly what the deleted
/// `From<RefreshError>`/`From<KickError>` impls were). Either one added back
/// reds this on its own, before any surface has to be re-audited — which is
/// what a string payload plus a naming convention could never do. Each leg has
/// its positive control; without them a probe that answered `false` for
/// everything would read as a pass.
#[test]
fn oauth_error_types_have_no_printable_escape_hatch() {
    assert!(
        Probe::<String>::is_display(),
        "positive control: the probe must detect a type that DOES implement Display"
    );
    assert!(
        Probe::<std::io::Error>::into_anyhow(),
        "positive control: the probe must detect a type that DOES convert into anyhow"
    );
    for (name, displays, converts) in [
        (
            "TokenFailure",
            Probe::<TokenFailure>::is_display(),
            Probe::<TokenFailure>::into_anyhow(),
        ),
        (
            "RefreshError",
            Probe::<RefreshError>::is_display(),
            Probe::<RefreshError>::into_anyhow(),
        ),
        (
            "KickError",
            Probe::<KickError>::is_display(),
            Probe::<KickError>::into_anyhow(),
        ),
    ] {
        assert!(
            !displays,
            "{name} must stay Display-free — with one, a bare {{e}} on a toast, a \
             bail!, or an MCP `reason` compiles again and the endpoint's own words \
             are one keystroke from a user"
        );
        assert!(
            !converts,
            "{name} must not convert into anyhow::Error — that conversion is what \
             smuggled the raw body past the terminal/transient classification into \
             whatever surface caught it"
        );
    }
}

// ── every Retry selection, pinned at the CALL SITE that chooses it ───────────
//
// `format.rs` pins the Retry→copy MAPPING. That is not the same thing and does
// not defend it: flipping `Retry::Stated` to `Retry::Connection` at a call site
// leaves the mapping correct and restores the exact defect the enum exists to
// remove — two contradictory reasons to retry in one sentence. Four of the six
// selections survived the whole suite until these landed, so each one below
// reaches its real call site and asserts the sentence an operator reads.

/// The guard-refusal copy must describe the condition that actually produces
/// it, and name one next step rather than two.
///
/// Reached by failing `RotationGuard::acquire`'s `mkdir_700`, NOT by holding the
/// lock: `acquire` ends in `File::lock()`, which BLOCKS. A second acquire in
/// this process waits instead of erroring, so a contention-based fixture
/// deadlocks the suite rather than exercising the arm (it did, once, here).
///
/// That mechanism is why the copy changed. Creating or opening the lock file is
/// the ONLY thing that lands here, so the previous wording — `rotation lock
/// busy; retry after the in-flight refresh` — described a condition that cannot
/// produce it and advised waiting for a refresh that is not running. An earlier
/// version of this very test pinned that sentence, which is worse than leaving
/// it unpinned: it would have handed a red suite to whoever noticed.
#[test]
fn guard_acquire_failure_names_the_filesystem_cause() {
    let _home = HomeSandbox::new();
    let name = "gate-retry-lock-busy";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-ok"),
        Some(past_expiry()),
    )));
    // A regular file where the lock's PARENT directory belongs, so `acquire`'s
    // `mkdir_700` of it fails. Aimed via `rotation_lock_path` rather than at the
    // profile dir: the lock no longer lives there, so occupying the profile dir
    // stopped denying the guard at all.
    #[allow(clippy::expect_used, reason = "test")]
    let lock = crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from(name))
        .expect("rotation lock path");
    #[allow(clippy::expect_used, reason = "test")]
    let locks_dir = lock.parent().expect("lock parent");
    #[allow(clippy::expect_used, reason = "test")]
    std::fs::create_dir_all(locks_dir.parent().expect("clauth dir")).expect("clauth dir");
    #[allow(clippy::expect_used, reason = "test")]
    std::fs::write(locks_dir, b"not a directory").expect("occupy the locks dir path");
    #[allow(clippy::expect_used, reason = "test")]
    let denied =
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name)).is_err();
    assert!(
        denied,
        "the fixture must actually deny the guard, or this proves nothing"
    );

    let AuthGate::Transient(t) = ensure_installable(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    ) else {
        panic!("an unacquirable rotation guard must refuse transiently");
    };
    assert_eq!(
        t.text(),
        format!("could not lock '{name}' for a token refresh; check permissions on ~/.clauth"),
        "the cause names its own next step; a second one contradicts it"
    );
    assert!(
        !t.text().contains("in-flight"),
        "contention cannot produce this arm, so the copy must not blame it: {}",
        t.text()
    );
}

/// One condition, one sentence, across BOTH legs that report it. The rotate
/// leg's `OpResult` toast and the switch gate's refusal describe the identical
/// failure, and `5391a4c` reworded only the gate — leaving the toasts on
/// `failed to acquire rotation lock`, which is exactly the "one condition
/// printed four different sentences" that `format.rs` was created to end.
/// Compares the two renderings against each other rather than against a
/// literal, so it catches the divergence even if the copy changes again.
#[test]
fn the_unavailable_lock_has_one_spelling_across_both_legs() {
    use std::sync::mpsc;
    let _home = HomeSandbox::new();
    let name = "rotate-lock-unavailable";
    // Same fixture as the gate pin: a regular file where the lock's PARENT
    // directory belongs, so `RotationGuard::acquire`'s `mkdir_700` fails.
    #[allow(clippy::expect_used, reason = "test")]
    let lock = crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from(name))
        .expect("rotation lock path");
    #[allow(clippy::expect_used, reason = "test")]
    let locks_dir = lock.parent().expect("lock parent");
    #[allow(clippy::expect_used, reason = "test")]
    std::fs::create_dir_all(locks_dir.parent().expect("clauth dir")).expect("clauth dir");
    #[allow(clippy::expect_used, reason = "test")]
    std::fs::write(locks_dir, b"not a directory").expect("occupy the locks dir path");
    #[allow(clippy::expect_used, reason = "test")]
    let denied =
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name)).is_err();
    assert!(
        denied,
        "the fixture must actually deny the guard, or this proves nothing"
    );

    let config = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-ok"),
        Some(past_expiry()),
    )));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let refetch: crate::usage::RefetchQueue =
        Arc::new(RankedMutex::new(std::collections::HashSet::new()));
    let (tx, rx) = mpsc::channel();

    assert!(
        !rotate_one(
            &config,
            &crate::profile::ProfileName::from(name),
            &refetch,
            &activity,
            &tx
        ),
        "an unacquirable lock persists nothing"
    );
    #[allow(clippy::expect_used, reason = "test")]
    let OpResult { outcome, .. } = rx.try_recv().expect("the guard-fail leg emits an OpResult");
    let toast = match outcome {
        Ok(()) => panic!("an unacquirable lock is not a completed rotation"),
        Err(e) => e.to_string(),
    };

    let AuthGate::Transient(gate) = ensure_installable(
        &config,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    ) else {
        panic!("the gate must refuse the same condition transiently");
    };
    assert_eq!(
        toast,
        gate.text(),
        "both legs report the same failure and must say it the same way"
    );
}

/// A poisoned config mutex does not clear itself, so a retry hint would be a
/// lie. Reached by panicking a thread while it holds the lock — the only way
/// this arm happens in production too.
#[test]
fn poisoned_config_refusal_offers_no_retry() {
    let _home = HomeSandbox::new();
    let name = "gate-retry-poisoned";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-ok"),
        Some(past_expiry()),
    )));
    let poisoner = Arc::clone(&handle);
    let _ = std::thread::spawn(move || {
        let _held = poisoner.lock();
        panic!("deliberate: poison the config mutex");
    })
    .join();

    let AuthGate::Transient(t) = ensure_installable(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    ) else {
        panic!("a poisoned config mutex must refuse transiently");
    };
    assert_eq!(
        t.text(),
        "clauth hit an internal lock error, restart clauth"
    );
}

/// A 2xx whose body does not parse: the transport worked, so waiting is the
/// honest advice — and the body itself still never reaches the toast.
#[test]
fn an_unparseable_2xx_tells_the_operator_to_wait() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "rotate-unparseable-2xx";
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (200, format!(r#"{{"unexpected":"{WIRE_CANARY}"}}"#))
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let (tx, rx) = mpsc::channel();
    let cfg = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));

    rotate_one_inner(&cfg, &crate::profile::ProfileName::from(name), None, &tx);
    #[allow(clippy::expect_used, reason = "test")]
    let OpResult { outcome, .. } = rx.try_recv().expect("the HTTP leg emits an OpResult");
    let msg = match outcome {
        Ok(()) => panic!("an unparseable token body is not a completed rotation"),
        Err(e) => e.to_string(),
    };
    assert_eq!(msg, "anthropic's reply was unreadable: retry in a moment");
    assert!(
        !msg.contains(WIRE_CANARY),
        "the 2xx body holds live credentials and must never surface: {msg}"
    );

    #[allow(clippy::expect_used, reason = "test")]
    let seen = server.join().expect("listener");
    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
}

/// The refresh landed but its pair could not be written. The chain is fine and
/// the next attempt can succeed, so this one DOES get a retry hint — the arm
/// that must not collapse into `Stated`.
#[test]
fn a_failed_persist_after_a_good_refresh_offers_a_retry() {
    let _home = HomeSandbox::new();
    let name = "gate-retry-persist-fails";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));
    // A DIRECTORY where `credentials.json` belongs: the rotation guard still
    // acquires (its parent is a real dir), the refresh still lands, and only
    // `save_profile`'s credential write fails — the one reachable way to make
    // `apply_rotated_tokens_locked` err without breaking an earlier step.
    #[allow(clippy::expect_used, reason = "test")]
    let dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("profile dir");
    #[allow(clippy::expect_used, reason = "test")]
    std::fs::create_dir_all(dir.join("credentials.json")).expect("occupy the credentials path");

    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    let AuthGate::Transient(t) =
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher)
    else {
        panic!("a failed persist must refuse transiently, never install");
    };
    assert_eq!(
        t.text(),
        format!("refreshed '{name}' but failed to persist the rotated tokens: retry in a moment")
    );
}

/// Bytes only the wire could have written. Any of it in a refusal is the leak.
const WIRE_CANARY: &str = "WIRE-BYTES-CANARY";

/// The dead-token envelope and an upstream error page, both carrying the
/// canary. Indexed so one listener can answer both legs of a test.
fn canary_bodies(i: usize) -> (u16, String) {
    if i == 0 {
        (
            400,
            format!(r#"{{"error": "invalid_grant", "error_description": "{WIRE_CANARY}"}}"#),
        )
    } else {
        (503, format!("<html>{WIRE_CANARY}</html>"))
    }
}

/// The reported defect, driven through the real HTTP leg: the manual rotate
/// emits its failure as an `OpResult` the TUI renders as a Danger toast, and
/// that used to be `HTTP {status}: {body}` because the plain `refresh` wrapper
/// collapsed the Invalid/Transient split into one opaque error. Both arms are
/// asserted — a dead chain keeps the shared `clauth login` recovery step, a
/// blip gets the canned transient line — and neither carries a wire byte.
#[test]
fn rotate_refusal_carries_no_wire_bytes_in_either_direction() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    // `max` above the two requests a correct run makes, per `serve_endpoints`.
    let (base, server) = crate::testutil::serve_endpoints(3, |_, i| canary_bodies(i));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let (tx, rx) = mpsc::channel();

    let dead = "rotate-refusal-dead";
    let dead_cfg =
        crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(dead));
    rotate_one_inner(
        &dead_cfg,
        &crate::profile::ProfileName::from(dead),
        None,
        &tx,
    );
    #[allow(clippy::expect_used, reason = "test")]
    let OpResult { outcome, .. } = rx.try_recv().expect("the HTTP leg emits an OpResult");
    let msg = match outcome {
        Ok(()) => panic!("a 400 must never read as a completed rotation"),
        Err(e) => e.to_string(),
    };
    assert!(
        !msg.contains(WIRE_CANARY),
        "the endpoint's body reached the rotate toast: {msg}"
    );
    assert!(
        !msg.contains("HTTP"),
        "the endpoint's status reached the rotate toast: {msg}"
    );
    assert!(
        msg.contains(&format!("clauth login {dead}")),
        "the dead-chain arm must keep the one recovery step it shares with the \
         switch gate and the daemon, got: {msg}"
    );

    let flaky = "rotate-refusal-flaky";
    let flaky_cfg =
        crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(flaky));
    rotate_one_inner(
        &flaky_cfg,
        &crate::profile::ProfileName::from(flaky),
        None,
        &tx,
    );
    #[allow(clippy::expect_used, reason = "test")]
    let OpResult { outcome, .. } = rx.try_recv().expect("the HTTP leg emits an OpResult");
    let msg = match outcome {
        Ok(()) => panic!("a 503 must never read as a completed rotation"),
        Err(e) => e.to_string(),
    };
    assert!(
        !msg.contains(WIRE_CANARY) && !msg.contains("HTTP"),
        "the endpoint's page reached the rotate toast: {msg}"
    );
    // Copy pin, not the structural guard: this wording is what the toast's
    // "refresh for '<name>' failed" line completes. Both arms now end on a next
    // step — the dead one says re-login, this one says wait — because a toast
    // that only names a condition leaves the operator nothing to do.
    assert_eq!(msg, "anthropic is having trouble: retry in a moment");

    #[allow(clippy::expect_used, reason = "test")]
    let seen = server.join().expect("listener");
    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string(); 2],
        "proof of execution: both legs actually answered the endpoint"
    );
}

/// The dead-chain toast on a keyless third-party profile: `login_expired`'s
/// bare `clauth login <name>` runs the browser flow and leaves the missing key
/// missing, so the arm carries the pre-flight's keyless sentence instead — the
/// command that clears the state the profile is actually in. The OAuth shape
/// keeps the shared login hint, pinned one test up.
#[test]
fn rotate_names_the_api_key_command_for_a_keyless_third_party_profile() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "rotate-keyless-third-party";
    let (base, server) = crate::testutil::serve_endpoints(1, |_, _| {
        (400, r#"{"error": "invalid_grant"}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let (tx, rx) = mpsc::channel();

    // A hybrid the endpoint edit produced: browser-login credentials that
    // predate setting the endpoint (setting one never drops them), so the
    // rotate leg still spends the dying chain while inference has no key.
    let mut profile = crate::profile::Profile::new(
        name.to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        None,
    );
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-old".to_string(),
            refresh_token: Some("rt-old".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 86_400_000),
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    crate::profile::save_app_state(&config.state).expect("save app state");
    let cfg: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(config));

    rotate_one_inner(&cfg, &crate::profile::ProfileName::from(name), None, &tx);
    #[allow(clippy::expect_used, reason = "test")]
    let OpResult { outcome, .. } = rx.try_recv().expect("the HTTP leg emits an OpResult");
    let msg = match outcome {
        Ok(()) => panic!("a dead chain is never a completed rotation"),
        Err(e) => e.to_string(),
    };
    assert_eq!(
        msg,
        "profile has no api key: rotate-keyless-third-party (run \
         `clauth login rotate-keyless-third-party --api-key <key>`)"
    );

    #[allow(clippy::expect_used, reason = "test")]
    let seen = server.join().expect("listener");
    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string()],
        "proof of execution: the leg actually answered the endpoint"
    );
}

/// The same toast on a KEYED third-party hybrid: "name the split state" (owner
/// ruling, 2026-08-30) reaches this surface too — the rotate leg spends any
/// profile holding a refresh token, so a hybrid's dying chain lands here with
/// its api key still working.
#[test]
fn rotate_names_the_split_state_for_a_keyed_third_party_profile() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "rotate-keyed-third-party";
    let (base, server) = crate::testutil::serve_endpoints(1, |_, _| {
        (400, r#"{"error": "invalid_grant"}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let (tx, rx) = mpsc::channel();

    let mut profile = crate::profile::Profile::new(
        name.to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-live".to_string()),
    );
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-old".to_string(),
            refresh_token: Some("rt-old".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 86_400_000),
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    crate::profile::save_app_state(&config.state).expect("save app state");
    let cfg: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(config));

    rotate_one_inner(&cfg, &crate::profile::ProfileName::from(name), None, &tx);
    #[allow(clippy::expect_used, reason = "test")]
    let OpResult { outcome, .. } = rx.try_recv().expect("the HTTP leg emits an OpResult");
    let msg = match outcome {
        Ok(()) => panic!("a dead chain is never a completed rotation"),
        Err(e) => e.to_string(),
    };
    assert_eq!(
        msg,
        "stored OAuth chain is dead, its api key still works: rotate-keyed-third-party \
         (run `clauth login rotate-keyed-third-party --api-key <key>` to clear the quarantine)"
    );

    #[allow(clippy::expect_used, reason = "test")]
    let seen = server.join().expect("listener");
    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string()],
        "proof of execution: the leg actually answered the endpoint"
    );
}

/// The terminal-vs-transient split is what the `auth_broken` quarantine rests
/// on, and it now lives entirely inside `refresh_result` — the body that decides
/// it is dropped the instant it has decided. Driven over the real wire in BOTH
/// directions, because the injected-refresher gate tests construct the verdict
/// rather than derive it, and would stay green if the mapping inverted.
#[test]
fn refresh_classification_survives_the_real_wire_in_both_directions() {
    let home = HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints(3, |_, i| canary_bodies(i));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let dead = "gate-wire-dead";
    let dead_cfg = Arc::new(RankedMutex::new(oauth_config(
        dead,
        Some("rt-revoked"),
        Some(past_expiry()),
    )));
    assert!(
        matches!(
            ensure_installable(
                &dead_cfg,
                &crate::profile::ProfileName::from(dead),
                refresh_result
            ),
            AuthGate::Broken
        ),
        "a confirmed invalid_grant is still terminal"
    );
    #[allow(clippy::expect_used, reason = "test")]
    let dead_flagged = dead_cfg
        .lock()
        .expect("lock")
        .is_auth_broken(&crate::profile::ProfileName::from(dead));
    assert!(dead_flagged, "a dead refresh token still quarantines");

    let flaky = "gate-wire-flaky";
    let flaky_cfg = Arc::new(RankedMutex::new(oauth_config(
        flaky,
        Some("rt-ok"),
        Some(past_expiry()),
    )));
    let AuthGate::Transient(e) = ensure_installable(
        &flaky_cfg,
        &crate::profile::ProfileName::from(flaky),
        refresh_result,
    ) else {
        panic!("a 5xx the endpoint never confirmed must stay transient");
    };
    #[allow(clippy::expect_used, reason = "test")]
    let flaky_flagged = flaky_cfg
        .lock()
        .expect("lock")
        .is_auth_broken(&crate::profile::ProfileName::from(flaky));
    assert!(
        !flaky_flagged,
        "a 5xx must never quarantine — the retry is what recovers it"
    );
    // This value is what the CLI bail, the TUI toast, the MCP `reason` and the
    // daemon's deferral line are all built from. Both renderings are asserted
    // together so neither half drifts alone: the CLI form MUST name the status
    // (an operator at a terminal has no log open beside it) and MUST NOT carry a
    // byte of the body; the canned form carries neither.
    let cli = e.text_with_status();
    let canned = e.text();
    assert!(
        cli.contains("HTTP 503"),
        "CLI stderr must still name the status: {cli}"
    );
    assert!(
        !cli.contains(WIRE_CANARY) && !cli.contains("html"),
        "the endpoint's page reached CLI stderr: {cli}"
    );
    assert!(
        !canned.contains("503") && !canned.contains(WIRE_CANARY) && !canned.contains("html"),
        "the toast/MCP form must carry neither the status nor the body: {canned}"
    );
    // A 5xx is not Anthropic "rejecting" anything, and telling the operator to
    // check their connection over one is wrong advice.
    assert_eq!(canned, "anthropic is having trouble: retry in a moment");

    #[allow(clippy::expect_used, reason = "test")]
    let seen = server.join().expect("listener");
    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string(); 2],
        "proof of execution: both legs actually answered the endpoint"
    );
}

/// `oauth_config` with the rolling token enabled and a plan-capable chain
/// (full scopes + subscriptionType) — the shape `clauth rolling-token <p>`
/// requires.
fn rolling_config(name: &str, refresh_token: Option<&str>, expires_at: Option<i64>) -> AppConfig {
    let mut config = oauth_config(name, refresh_token, expires_at);
    let p = config.profiles.first_mut().expect("profile");
    p.rolling_token = true;
    if let Some(oauth) = p
        .credentials
        .as_mut()
        .and_then(|c| c.claude_ai_oauth.as_mut())
    {
        oauth.scopes = Some(vec!["user:profile".into(), "user:inference".into()]);
        oauth.subscription_type = Some("max".into());
    }
    config
}

/// Read the sidecar's OAuth block back for assertions.
fn sidecar_oauth(name: &str) -> Option<OAuthToken> {
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    let creds: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).ok()?).ok()?;
    creds.claude_ai_oauth
}

/// Fresh fed sidecar → install as-is; neither the chain clock nor the
/// refresher is consulted (the chain here is stone dead).
#[test]
fn rolling_gate_fresh_sidecar_ready_without_refresh() {
    let _home = HomeSandbox::new();
    let name = "test-feed-fresh";
    let config = rolling_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-fed", "sidecar untouched");
}

/// The flag is re-read from DISK under the guard, on both wait arms: the
/// in-memory config that routed here can predate a completed
/// `static-token --clear` (a separate process the daemon's snapshot never
/// sees), and stamping from that stale routing would re-create the very
/// sidecar the operator was just told is gone — with the flag now off so
/// nothing ever re-stamps it. The scheduler leg reports Ready (its still-due
/// re-read then drops the pacing hold); the switch-in leg drops the guard and
/// falls to the vanilla gate, which installs the stored login.
#[test]
fn rolling_gate_disk_disarm_under_the_guard_stops_the_stamp() {
    let _home = HomeSandbox::new();
    let name = "test-feed-cleared";
    // In-memory: ARMED, with a comfortable chain a roll would happily stamp.
    let config = rolling_config(name, Some("rt-live"), Some(future_expiry()));
    // On disk: the clear already landed — flag off, no sidecar, no backup.
    let mut on_disk = config.profiles[0].clone();
    on_disk.rolling_token = false;
    crate::profile::save_profile(&on_disk).expect("save profile");
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    assert!(
        !dir.join("session-token.json").exists(),
        "fixture: the profile is cleared"
    );
    let handle = Arc::new(RankedMutex::new(config));

    // The scheduler leg (NoWait).
    let gate = restamp_rolling_token(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    );
    assert!(
        matches!(gate, AuthGate::Ready),
        "the scheduler leg has nothing to re-stamp"
    );
    assert!(
        !dir.join("session-token.json").exists(),
        "a cleared profile stays cleared on the scheduler leg"
    );

    // The switch-in leg (Block) — the vanilla fallback serves the login.
    let gate = ensure_installable(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    );
    assert!(
        matches!(gate, AuthGate::Ready),
        "the vanilla fallback serves the login"
    );
    assert!(
        !dir.join("session-token.json").exists(),
        "a cleared profile stays cleared on the switch-in leg"
    );
}

/// Stale fed sidecar + comfortably live stored chain → re-stamped from the
/// store, no refresh spent, chain metadata (subscriptionType) carried.
#[test]
fn rolling_gate_stale_sidecar_feeds_from_comfortable_chain_without_spend() {
    let _home = HomeSandbox::new();
    let name = "test-feed-nospend";
    let config = rolling_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed-stale".to_string(),
            refresh_token: None,
            expires_at: Some(past_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-old",
        "re-stamped from the stored chain"
    );
    assert!(
        oauth.refresh_token.is_none(),
        "the pair never leaves clauth custody"
    );
    assert_eq!(oauth.subscription_type.as_deref(), Some("max"));
    assert_eq!(oauth.expires_at, Some(future_expiry_of(&handle, name)));
}

/// The chain expiry the gate fed from — read back from the handle so the
/// assertion tracks the fixture rather than re-deriving clock math.
fn future_expiry_of(handle: &crate::profile::ConfigHandle, name: &str) -> i64 {
    handle
        .lock()
        .expect("config")
        .find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.access_token_expires_at())
        .expect("chain expiry")
}

/// Stale sidecar + stale chain → guarded refresh; the rotation persist
/// re-stamps the sidecar with the freshly minted access token.
#[test]
fn rolling_gate_stale_sidecar_stale_chain_refreshes_and_restamps() {
    let _home = HomeSandbox::new();
    let name = "test-feed-refresh";
    let config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed-stale".to_string(),
            refresh_token: None,
            expires_at: Some(past_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Refreshed
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-new",
        "hook re-stamped the rotated access token"
    );
    assert!(oauth.refresh_token.is_none());
}

/// Feed flag on but NO sidecar yet → the gate arms it through the refresh
/// leg instead of falling through to a vanilla pair install (which would put
/// the shared rotating chain in front of sessions).
#[test]
fn rolling_gate_absent_sidecar_arms_instead_of_vanilla_install() {
    let _home = HomeSandbox::new();
    let name = "test-feed-arm";
    let config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-armed".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 3600,
            scope: None,
        })
    };
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Refreshed
    ));
    let oauth = sidecar_oauth(name).expect("sidecar armed");
    assert_eq!(oauth.access_token, "at-armed");
    assert!(oauth.refresh_token.is_none());
    let expected = crate::claude::install_source_path(&crate::profile::ProfileName::from(name))
        .expect("source");
    assert!(
        expected.ends_with("session-token.json"),
        "the armed sidecar is now the install source"
    );
}

/// Terminally dead chain + preserved static mint → degrade to the mint
/// (Ready) instead of benching the account; the backup is consumed.
#[test]
fn rolling_gate_dead_chain_restores_static_mint() {
    let _home = HomeSandbox::new();
    let name = "test-feed-degrade";
    let config = rolling_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    // A genuine mint first (1yr horizon, no subscriptionType)…
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-mint",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    // …then the roll takes over, preserving it…
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed-stale".to_string(),
            refresh_token: None,
            expires_at: Some(past_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher =
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid(TokenFailure::Status(400)));
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "sk-ant-oat01-mint", "the mint is back");
    let backup = profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("session-token.static.json");
    assert!(!backup.exists(), "backup consumed by the restore");
}

/// Terminally dead chain and no mint to fall back to → Broken stands (the
/// pre-stamp refusal), never a vanilla install of the dead pair.
#[test]
fn rolling_gate_dead_chain_without_backup_stays_broken() {
    let _home = HomeSandbox::new();
    let name = "test-feed-broken";
    let config = rolling_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed-stale".to_string(),
            refresh_token: None,
            expires_at: Some(past_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher =
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid(TokenFailure::Status(400)));
    assert!(matches!(
        ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Broken
    ));
}

/// The rotation persist re-stamps a feed-enabled profile's sidecar (parked or
/// active) and preserves a genuine mint on first contact; a non-feed split
/// profile's sidecar stays untouched (the CLA-SPLIT quiet branch).
#[test]
fn rotation_hook_stamps_enabled_profiles_and_preserves_the_mint() {
    let _home = HomeSandbox::new();
    let name = "test-feed-hook";
    let config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-mint",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    let handle = Arc::new(RankedMutex::new(config));
    apply_rotated_tokens_locked(
        &handle,
        &crate::profile::ProfileName::from(name),
        TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 3600,
            scope: None,
        },
    )
    .expect("persist");
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-rotated", "rotation fed the sidecar");
    assert!(oauth.refresh_token.is_none());
    let backup = profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("session-token.static.json");
    let backed: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(&backup).expect("backup")).expect("parse");
    assert_eq!(
        backed.access_token(),
        Some("sk-ant-oat01-mint"),
        "first roll preserved the mint"
    );
}

/// Same rotation on a split profile WITHOUT the rolling token: the sidecar is the
/// static mint and stays byte-identical (the designed quiet steady state).
#[test]
fn rotation_hook_leaves_non_rolling_split_sidecars_alone() {
    let _home = HomeSandbox::new();
    let name = "test-nofeed-hook";
    let config = oauth_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-mint",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    let handle = Arc::new(RankedMutex::new(config));
    apply_rotated_tokens_locked(
        &handle,
        &crate::profile::ProfileName::from(name),
        TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 3600,
            scope: None,
        },
    )
    .expect("persist");
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "sk-ant-oat01-mint", "mint untouched");
}

/// A mis-filled sidecar (rotating pair) is never overwritten by the roll —
/// the DANGER evidence survives for the operator to see.
#[test]
fn rotation_hook_never_overwrites_a_misfilled_sidecar() {
    let _home = HomeSandbox::new();
    let name = "test-misfill-hook";
    let config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at-misfill".to_string(),
                refresh_token: Some("rt-misfill".to_string()),
                expires_at: Some(future_expiry()),
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("write misfill");
    let handle = Arc::new(RankedMutex::new(config));
    apply_rotated_tokens_locked(
        &handle,
        &crate::profile::ProfileName::from(name),
        TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 3600,
            scope: None,
        },
    )
    .expect("persist");
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-misfill",
        "mis-fill evidence survives"
    );
    assert_eq!(oauth.refresh_token.as_deref(), Some("rt-misfill"));
}

/// Feed profile + mis-filled sidecar + preserved mint → the gate heals
/// (quarantine + restore) and installs the mint; the pair never fronts CC.
#[test]
fn rolling_gate_heals_a_misfilled_sidecar_when_a_backup_exists() {
    let _home = HomeSandbox::new();
    let name = "test-feed-heal";
    let config = rolling_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-mint",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed preserves mint");
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at-misfill".to_string(),
                refresh_token: Some("rt-misfill".to_string()),
                expires_at: Some(future_expiry()),
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("misfill");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    // Healed AND re-armed in one pass: the restored mint is immediately
    // superseded by a fed bearer from the comfortable chain (no spend).
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-old",
        "healed then re-stamped from the chain"
    );
    assert!(oauth.refresh_token.is_none());
    let source = crate::claude::install_source_path(&crate::profile::ProfileName::from(name))
        .expect("source");
    assert!(
        source.ends_with("session-token.json"),
        "the pair is never the install source after a heal"
    );
}

/// A static MINT on a feed profile is a fallback, not a fresh fed token — the
/// gate supersedes it from the comfortable chain (no spend) and the mint
/// lands in the degrade backup. Regression for the deploy-day bug where the
/// mint's ~1yr stamp read as "fresh" and arming never fed anything.
#[test]
fn rolling_gate_supersedes_a_static_mint_with_a_fed_bearer() {
    let _home = HomeSandbox::new();
    let name = "test-feed-mint-supersede";
    let config = rolling_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-mint",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-old", "fed from the stored chain");
    assert!(oauth.refresh_token.is_none());
    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(
            profile_dir(&crate::profile::ProfileName::from(name))
                .expect("dir")
                .join("session-token.static.json"),
        )
        .expect("backup"),
    )
    .expect("parse");
    assert_eq!(
        backup.access_token(),
        Some("sk-ant-oat01-mint"),
        "the superseded mint became the degrade backup"
    );
}

/// Feed profile + mis-filled sidecar + NO backup → the disengaged-vanilla
/// posture stands (documented CLA-SPLIT-3 semantics): the plain gate runs and
/// a comfortable chain installs credentials.json, loudly.
#[test]
fn rolling_gate_misfill_without_backup_keeps_the_disengaged_vanilla_posture() {
    let _home = HomeSandbox::new();
    let name = "test-feed-misfill-vanilla";
    let config = rolling_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at-misfill".to_string(),
                refresh_token: Some("rt-misfill".to_string()),
                expires_at: Some(future_expiry()),
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("misfill");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let source = crate::claude::install_source_path(&crate::profile::ProfileName::from(name))
        .expect("source");
    assert!(
        source.ends_with("credentials.json"),
        "no backup to degrade to — disengaged split behaves as vanilla"
    );
}

// ---------------------------------------------------------------------------
// CLA-ROLL: scheduler-leg re-stamp (`restamp_rolling_token`) — renew the fed
// bearer HOURS before its clock death, not seconds.
// ---------------------------------------------------------------------------

/// Epoch-ms comfortably beyond the re-stamp horizon — a chain the no-spend
/// re-stamp path may clone from.
fn beyond_horizon_expiry() -> i64 {
    crate::usage::now_ms() as i64 + 6 * 3_600_000
}

/// The due predicate fires for an armed, exp-carrying sidecar inside the
/// horizon — and for a MIS-FILL at any clock, because the content is the
/// defect and this leg is the only one a running daemon has that can repair
/// it. Absent sidecars stay the switch-time gate's job.
#[test]
fn restamp_due_fires_inside_the_horizon_or_on_a_misfill() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;
    let name = "test-restamp-due";
    assert!(
        !rolling_sidecar_restamp_due(&crate::profile::ProfileName::from(name), now),
        "absent sidecar is not the timer's job"
    );
    // A rotating pair with 8h of clock — comfortably OUTSIDE the horizon, so
    // only the content classification can make it due. This is the arm whose
    // deletion turns the daemon-side heal back into dead code.
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at-misfill".to_string(),
                refresh_token: Some("rt-misfill".to_string()),
                expires_at: Some(beyond_horizon_expiry()),
                scopes: None,
                subscription_type: None,
            }),
        })
        .expect("ser"),
    )
    .expect("write misfill");
    assert!(
        rolling_sidecar_restamp_due(&crate::profile::ProfileName::from(name), now),
        "a mis-fill is due NOW, whatever its clock says"
    );
    std::fs::remove_file(dir.join("session-token.json")).expect("clean the misfill fixture");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolled".to_string(),
            refresh_token: None,
            expires_at: Some(beyond_horizon_expiry()),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: None,
        },
    )
    .expect("stamp");
    assert!(
        !rolling_sidecar_restamp_due(&crate::profile::ProfileName::from(name), now),
        "a bearer clear of the horizon is left alone"
    );
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolled-dying".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()), // +1h,  inside the 2h horizon
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: None,
        },
    )
    .expect("stamp");
    assert!(
        rolling_sidecar_restamp_due(&crate::profile::ProfileName::from(name), now),
        "a bearer inside the horizon is due"
    );
}

/// A dying fed bearer + a chain clear of the horizon → no-spend re-stamp,
/// exactly where the switch gate (minutes-tight grace) would have no-opped —
/// the contrast that pins the horizon semantics.
#[test]
fn restamp_restamps_a_dying_bearer_the_switch_gate_calls_fresh() {
    let _home = HomeSandbox::new();
    let name = "test-restamp-nospend";
    let config = rolling_config(name, Some("rt-good"), Some(beyond_horizon_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed-dying".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()), // +1h: dying,  but "fresh" to the switch gate
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    // Switch gate: +1h clears the five-minute grace → install as-is, no re-stamp.
    assert!(matches!(
        ensure_installable(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    assert_eq!(
        sidecar_oauth(name).expect("sidecar").access_token,
        "at-fed-dying",
        "the switch gate leaves a +1h bearer alone"
    );
    // Re-feed leg: +1h is inside the 2h horizon → re-stamped from the chain,
    // still without spending a refresh.
    assert!(matches!(
        restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-old", "re-stamped from the chain");
    assert!(oauth.refresh_token.is_none(), "pair never leaves custody");
}

/// Dying bearer + chain itself inside the horizon (they usually share an
/// expiry — the fed token IS the chain's access token) → guarded refresh; the
/// rotation hook re-stamps the freshly minted token.
#[test]
fn restamp_rotates_when_the_chain_is_inside_the_horizon_too() {
    let _home = HomeSandbox::new();
    let name = "test-restamp-rotate";
    let config = rolling_config(name, Some("rt-old"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-fed-dying".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Ok(TokenResponse {
            access_token: "at-new".to_string(),
            refresh_token: "rt-new".to_string(),
            expires_in: 8 * 3600,
            scope: None,
        })
    };
    assert!(matches!(
        restamp_rolling_token(&handle, &crate::profile::ProfileName::from(name), refresher),
        AuthGate::Refreshed
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-new",
        "hook re-stamped the rotated token"
    );
    assert!(oauth.refresh_token.is_none());
}

/// The missing arm of the dead-chain degrade: a preserved mint whose stamped
/// `expiresAt` has PASSED. Restoring it would install a credential that signs
/// every session out on first use (the Incident C shape the vanilla gate's
/// clock check guards), so the restore refuses, the backup survives as
/// evidence, and with nothing live to serve the verdict is Broken — never
/// Ready-on-a-dead-mint.
#[test]
fn rolling_gate_dead_chain_with_expired_backup_stays_broken() {
    let _home = HomeSandbox::new();
    let name = "test-expired-backup";
    let config = rolling_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A backup that aged out on the shelf: mint-scoped, stamped in the past.
    let expired_mint = crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-aged-out".to_string(),
            refresh_token: None,
            expires_at: Some(past_expiry()),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec_pretty(&expired_mint).expect("ser"),
    )
    .expect("write backup");
    // The sidecar holds the last rolling bearer, itself past its clock.
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-rolled-dead".to_string(),
            refresh_token: None,
            expires_at: Some(past_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("stamp");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher =
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid(TokenFailure::Status(400)));
    assert!(
        matches!(
            ensure_installable(&handle, &crate::profile::ProfileName::from(name), refresher),
            AuthGate::Broken
        ),
        "an expired backup must not launder a dead chain into Ready"
    );
    assert!(
        dir.join("session-token.static.json").exists(),
        "the refused backup stays on disk instead of being consumed"
    );
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-rolled-dead",
        "and the expired mint never reached the sidecar"
    );
}

/// The scheduler's re-stamp leg must never park behind a held rotation lock:
/// it runs inline on the tick thread and the rotation lock has no timeout, so a
/// `clauth start` holding the lock across its recursive copy would stall
/// every account's poll. With the lock held, the gate answers Transient
/// promptly (the NoWait path) instead of blocking until release.
#[test]
fn restamp_never_parks_behind_a_held_rotation_lock() {
    let _home = HomeSandbox::new();
    let name = "test-noblock";
    let config = rolling_config(name, Some("rt-live"), Some(beyond_horizon_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    // A rolling sidecar inside the re-stamp horizon, so the gate has work
    // that reaches the lock.
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &OAuthToken {
            access_token: "at-dying".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()), // +1h,  inside the 2h horizon
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: None,
        },
    )
    .expect("stamp");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("hold the lock");
    let handle = Arc::new(RankedMutex::new(config));
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let refresher =
            |_rt: &str, _scopes: Option<&str>| panic!("a held lock must never reach the refresher");
        let gate = restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from("test-noblock"),
            refresher,
        );
        let text = match gate {
            AuthGate::Transient(e) => Some(e.text()),
            _ => None,
        };
        tx.send(text).expect("send");
    });
    // Generous for a loaded runner, tiny next to the block it guards against
    // (the lock is held for the whole wait, so a parking implementation can
    // only fail this by timeout).
    let verdict = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(guard);
    worker.join().expect("worker");
    let text = verdict
        .expect("the re-stamp leg parked behind a held rotation lock")
        .expect("a held lock answers Transient, never Ready and never a wait");
    // The HELD copy, not the UNAVAILABLE one: this is genuine contention, and
    // the arm whose copy upstream corrected in round 1 describes a filesystem
    // fault that is not what happened here. Derived from the renderer rather
    // than re-spelled: the two arms render different sentences, so a swap still
    // reds, and the literal stays pinned once, in `format`'s copy table.
    assert_eq!(
        text,
        crate::format::Transient::new(
            crate::format::Cause::RotationLockHeld(name.to_string()),
            crate::format::Retry::Stated,
        )
        .text(),
        "a held lock renders as contention, not as the filesystem-fault arm"
    );
}

/// The mis-fill arm is the one path that leaves [`rolling_install_gate`] for
/// the vanilla gate — whose own acquire BLOCKS — so the NoWait axis must
/// cover it too, or the non-parking property holds everywhere except on the
/// arm that widened last. The scenario that defeats it: a mis-filled sidecar,
/// an EXPIRED preserved mint (so the heal has nothing live to restore), an
/// expiring chain (so the vanilla gate would proceed to its blocking
/// acquire), and a `clauth start` holding the rotation lock. The tick's leg
/// must answer Transient with the mis-fill's own cause, promptly, touching
/// neither the refresher nor the evidence on disk.
#[test]
fn restamp_on_a_misfill_with_no_live_backup_never_takes_the_vanilla_gate() {
    let _home = HomeSandbox::new();
    let name = "test-misfill-nowait";
    // Chain inside the vanilla gate's own grace: the fall-through would not
    // stop at the pre-check but go on to acquire the held lock and refresh.
    let config = rolling_config(name, Some("rt-live"), Some(now_ms() as i64 + 10_000));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    // The mis-fill: a rotating pair in the sidecar.
    let pair = crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-misfill".to_string(),
            refresh_token: Some("rt-misfill".to_string()),
            expires_at: Some(now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.json"),
        serde_json::to_vec(&pair).expect("ser"),
    )
    .expect("misfill");
    // The preserved mint: genuine, but past its clock — nothing live to heal
    // with, and refused without being consumed.
    let dead_mint = crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "sk-ant-oat01-dead-mint".to_string(),
            refresh_token: None,
            expires_at: Some(now_ms() as i64 - 1_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:sessions:claude_code".to_string(),
            ]),
            subscription_type: None,
        }),
    };
    std::fs::write(
        dir.join("session-token.static.json"),
        serde_json::to_vec(&dead_mint).expect("ser"),
    )
    .expect("expired backup");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("hold the lock");
    let handle = Arc::new(RankedMutex::new(config));
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let refresher = |_rt: &str, _scopes: Option<&str>| {
            panic!("the NoWait leg must never do the vanilla gate's refresh work")
        };
        let gate = restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from("test-misfill-nowait"),
            refresher,
        );
        let text = match gate {
            AuthGate::Transient(e) => Some(e.text()),
            _ => None,
        };
        tx.send(text).expect("send");
    });
    let verdict = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(guard);
    worker.join().expect("worker");
    let text = verdict
        .expect("the re-stamp leg parked behind the vanilla gate's blocking acquire")
        .expect("a disengaged mis-fill answers Transient on the NoWait leg, never vanilla-Ready");
    assert!(
        text.contains("rotating pair and no live mint backup"),
        "the mis-fill's own cause, not a lock or write flavor: {text}"
    );
    assert!(
        dir.join("session-token.static.json").exists(),
        "the expired mint stays on disk as evidence"
    );
    let sidecar: crate::profile::ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).expect("read"))
            .expect("parse");
    assert_eq!(
        sidecar.access_token(),
        Some("at-misfill"),
        "the sidecar is untouched — the evidence quarantine is the CLI's explicit-intent path"
    );
}

/// A profile whose chain grant is UNRECORDED (no scopes, no plan stamp — the
/// shape `stamp_rolling_token` refuses) but whose sidecar holds a live,
/// installable mint must still switch in on that mint. The GrantUnusable
/// verdict's first cut returned Transient unconditionally and turned this
/// profile into a hard switch refusal — losing the live-sidecar fallback the
/// old WriteFailed path had (verification fleet, round 3).
#[test]
fn rolling_gate_unrecorded_grant_still_installs_a_live_mint() {
    let _home = HomeSandbox::new();
    let name = "test-grant-mint";
    // A LIVE chain with no recorded grant at all.
    let config = rolling_config(name, Some("rt-live"), Some(beyond_horizon_expiry()));
    let mut profile = config.profiles[0].clone();
    if let Some(oauth) = profile
        .credentials
        .as_mut()
        .and_then(|c| c.claude_ai_oauth.as_mut())
    {
        oauth.scopes = None;
        oauth.subscription_type = None;
    }
    crate::profile::save_profile(&profile).expect("save profile");
    let config = Arc::new(RankedMutex::new(crate::profile::AppConfig {
        state: config.state.clone(),
        profiles: vec![profile],
    }));
    // The sidecar: a genuine live mint.
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "sk-ant-oat01-live-mint",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        panic!("a live chain with an unusable grant must not spend a refresh")
    };
    assert!(
        matches!(
            ensure_installable(&config, &crate::profile::ProfileName::from(name), refresher),
            AuthGate::Ready
        ),
        "the live mint installs; the unusable grant only stops the ROLL"
    );
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "sk-ant-oat01-live-mint",
        "mint untouched"
    );
}

// ── stale-config persist gates (lock-race row 3) ─────────────────────────────
//
// A daemon leg persists a whole profile/state from an in-memory config that a
// concurrent CLI delete/rename can have already moved past. Each test deletes a
// profile through the CLI action while a separate handle holds the pre-delete
// snapshot, then drives the persist leg and asserts the deleted profile stays
// gone.

/// A rotation that lands after the profile was deleted must not recreate its
/// directory: `save_profile` opens with `mkdir_700`, so the pre-fix leg
/// resurrected the account's whole directory tree.
#[test]
fn rotated_tokens_do_not_resurrect_a_deleted_profile() {
    let _home = HomeSandbox::new();
    let name = "rotate-resurrect";
    let mut profile = Profile::new(name.to_string(), None, None);
    profile.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "at-old".to_string(),
            refresh_token: Some("rt-old".to_string()),
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: None,
        }),
    });
    crate::profile::save_profile(&profile).expect("save profile");
    let mut config = AppConfig {
        state: AppState {
            profiles: vec![name.into()],
            ..AppState::default()
        },
        profiles: vec![profile],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");

    // The leg's handle is a snapshot taken BEFORE the delete.
    let stale = Arc::new(RankedMutex::new(config.clone()));

    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(name))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut config,
        &crate::profile::ProfileName::from(name),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);
    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("dir")
            .exists(),
        "fixture precondition: the delete removed the directory"
    );

    let tok = TokenResponse {
        access_token: "at-new".to_string(),
        refresh_token: "rt-new".to_string(),
        expires_in: 3600,
        scope: None,
    };
    assert!(
        apply_rotated_tokens_locked(&stale, &crate::profile::ProfileName::from(name), tok).is_err(),
        "a persist to a deleted profile must refuse"
    );
    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("dir")
            .exists(),
        "the deleted profile's directory must stay deleted"
    );
}

/// The durable record of a quarantine names the same recovery the live
/// surfaces do, split the same three ways: this leg fires for a third-party
/// hybrid (the scheduler spends any profile holding a refresh token), and a
/// log that prescribes the bare browser login there contradicts the toast the
/// same event raises.
#[test]
fn the_quarantine_logline_splits_the_recovery_like_every_other_surface() {
    let _home = HomeSandbox::new();
    let mk = |name: &str, base_url: Option<&str>, api_key: Option<&str>| {
        let p = Profile::new(
            name.to_string(),
            base_url.map(str::to_string),
            api_key.map(str::to_string),
        );
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    let oauth = mk("ql-oauth", None, None);
    let keyed = mk(
        "ql-keyed",
        Some("https://api.deepseek.com/anthropic"),
        Some("sk-live"),
    );
    let keyless = mk(
        "ql-keyless",
        Some("https://api.deepseek.com/anthropic"),
        None,
    );
    let config = AppConfig {
        state: AppState {
            profiles: vec!["ql-oauth".into(), "ql-keyed".into(), "ql-keyless".into()],
            ..AppState::default()
        },
        profiles: vec![oauth, keyed, keyless],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let handle = Arc::new(RankedMutex::new(config));

    let sink = crate::logline::LogLines::new();
    let _capture = sink.capture_here();
    for name in ["ql-oauth", "ql-keyed", "ql-keyless"] {
        mark_auth_broken(&handle, &crate::profile::ProfileName::from(name), true);
    }

    assert_eq!(
        sink.snapshot(),
        vec![
            "clauth: login for 'ql-oauth' has expired: refresh token revoked or invalid: \
             run clauth login ql-oauth (flagged auth_broken)"
                .to_string(),
            "clauth: stored OAuth chain is dead, its api key still works: ql-keyed (run \
             `clauth login ql-keyed --api-key <key>` to clear the quarantine) (flagged auth_broken)"
                .to_string(),
            "clauth: profile has no api key: ql-keyless (run `clauth login ql-keyless \
             --api-key <key>`) (flagged auth_broken)"
                .to_string(),
        ],
    );
}

/// The quarantine write re-serializes the whole profile list, so it used to
/// restore a deleted profile's ROW in `profiles.toml` (not only its directory).
/// It must write only the flag onto the current on-disk state.
#[test]
fn mark_auth_broken_does_not_resurrect_a_deleted_profiles_row() {
    let _home = HomeSandbox::new();
    let mk = |n: &str| {
        let p = Profile::new(n.to_string(), None, None);
        crate::profile::save_profile(&p).expect("save profile");
        p
    };
    let kept = mk("kept-row");
    let victim = mk("victim-row");
    let mut config = AppConfig {
        state: AppState {
            profiles: vec!["kept-row".into(), "victim-row".into()],
            ..AppState::default()
        },
        profiles: vec![kept, victim],
    };
    crate::profile::save_app_state(&config.state).expect("persist state");
    let stale = Arc::new(RankedMutex::new(config.clone()));

    let guard =
        crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("victim-row"))
            .expect("rotation guard");
    crate::actions::delete_profile(
        &mut config,
        &crate::profile::ProfileName::from("victim-row"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    mark_auth_broken(
        &stale,
        &crate::profile::ProfileName::from("victim-row"),
        true,
    );

    let reloaded = crate::profile::load_config().expect("reload");
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("victim-row"))
            .is_none(),
        "a deleted profile's row must not come back through the quarantine write"
    );
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("kept-row"))
            .is_some(),
        "the surviving profile's row is untouched"
    );
}
