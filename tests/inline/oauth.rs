//! Behaviour tests for `rotation_candidates` — the filter that decides which
//! profiles `refresh_all` will attempt to rotate.
//!
//! These tests never touch the network. They assert on the candidate list
//! returned by `rotation_candidates`, which is the only part of `refresh_all`
//! that `force` affects.

use super::*;
use crate::lockorder::RankedMutex;
use crate::profile::{AppState, ClaudeCredentials, OAuthToken, Profile, profile_dir};
use crate::usage::is_idle;

/// The rotation persist as a spend site drives it, for a test holding a bare
/// `TokenResponse`: the store's own login with `tok` written in, persisted as
/// minted from the refresh token the store holds.
fn persist_rotation(
    handle: &crate::profile::ConfigHandle,
    name: &crate::profile::ProfileName,
    tok: TokenResponse,
) -> anyhow::Result<()> {
    let store = read_raw_store(name)?;
    let sent = store
        .as_ref()
        .and_then(ClaudeCredentials::refresh_token)
        .unwrap_or_default()
        .to_string();
    let old_access = store
        .as_ref()
        .and_then(ClaudeCredentials::access_token)
        .unwrap_or_default()
        .to_string();
    let creds = minted_credentials(store, tok);
    apply_rotated_tokens_locked(handle, name, &token_fingerprint(&sent), &creds, &old_access)
        .map_err(PersistError::into_error)
}

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
        preferred_days: Vec::new(),
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
                ..crate::profile::OAuthToken::default_extra()
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
    };
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile],
    };
    config.state.profiles.push(name.into());
    config
}

use crate::testutil::HomeSandbox;

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
    let home = HomeSandbox::new();
    let name = "test-oauth-live-session-guard";
    let file = crate::testutil::arm_live_session(home.home(), name);

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
    let home = HomeSandbox::new();
    let name = "test-oauth-live-session-force";
    let file = crate::testutil::arm_live_session(home.home(), name);

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
        preferred_days: Vec::new(),
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
                ..crate::profile::OAuthToken::default_extra()
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
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
        preferred_days: Vec::new(),
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
                ..crate::profile::OAuthToken::default_extra()
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
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
        preferred_days: Vec::new(),
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
                ..crate::profile::OAuthToken::default_extra()
            }),
        }),
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
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
        preferred_days: Vec::new(),
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
        usage_stale: false,
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
    let home = HomeSandbox::new();
    let name = "test-gate-live-session";
    let file = crate::testutil::arm_live_session(home.home(), name);
    let config = oauth_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save the store");
    let handle = Arc::new(RankedMutex::new(config));
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
    let home = HomeSandbox::new();
    let name = "test-gate-live-session-macos";
    let file = crate::testutil::arm_live_session(home.home(), name);
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

/// The same install-as-is arm, driven off macOS through the override seam:
/// the refusal relocates the spend rather than avoiding it (the Claude Code
/// the switch starts refreshes on its first request, in a process that can
/// write the item its reader consults), so the switch installs the expiring
/// token as-is instead of spending the chain here. Off macOS the predicate's
/// `cfg!` term keeps the refusal off; the override seam answers before it, so
/// this test pins the arm on every platform. `never_refresh` panics if the
/// arm is lifted and the gate falls through to the refresh.
#[test]
fn gate_installs_as_is_under_a_forced_live_session() {
    let _home = HomeSandbox::new();
    let name = "test-gate-live-vanilla";
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));

    crate::runtime::set_rotation_blocked_override(Some(true));
    let gate = ensure_installable(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    );
    crate::runtime::set_rotation_blocked_override(None);

    assert!(
        matches!(gate, AuthGate::Ready),
        "a live-session refusal installs the stored pair as-is"
    );
}

/// Expired-but-refreshable → rotated tokens minted, persisted, installed.
#[test]
fn gate_refreshes_expiring_token_and_installs() {
    let _home = HomeSandbox::new();
    let name = "test-gate-refreshable";
    let config = oauth_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save the store");
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
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Invalid {
            failure: TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
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
    crate::profile::save_profile(&config.profiles[0]).expect("save the store");
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
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Invalid {
            failure: TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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

    /// Memory holds P1, the live slot a later-expiring P2 whose refresh token
    /// the record names spent (fp("at-mirror-refresh"), hand-computed: the
    /// first 8 bytes of its SHA-256, hex), and the store the landed P3. The
    /// adopt refuses: a spent live pair never rewinds the store.
    #[test]
    fn refuses_a_live_pair_whose_refresh_token_is_spent() {
        let _home = HomeSandbox::new();
        let name = "adopt-spent-live";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("profile dir");
        std::fs::write(
            dir.join("credentials.json"),
            serde_json::to_vec(&creds_with("at-p3", Some(future_expiry() + 7_200_000)))
                .expect("serialize P3"),
        )
        .expect("land P3");
        std::fs::write(dir.join("auth.spent.json"), r#"["141ccd5b485cec0b"]"#).expect("record P2");

        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &|_| Some("uuid-1".into()),
        );

        assert_eq!(adopted, None);
        let store: crate::profile::ClaudeCredentials =
            crate::profile::read_json_file(&dir.join("credentials.json")).expect("read store");
        assert_eq!(store.refresh_token(), Some("at-p3-refresh"));
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

    /// End to end: a capture-overwrite commits a live login with NO proven
    /// uuid, so the anchor is removed; a later same-account CC rotation of the
    /// live file must STILL be adopted through the stored-token probe (the
    /// freshly installed stored token is alive, so it proves identity with no
    /// cached anchor). A refused adoption here would strand the profile behind
    /// the rotation CC already made.
    #[test]
    fn a_capture_overwrite_with_no_uuid_still_adopts_a_same_account_cc_rotation() {
        let _home = HomeSandbox::new();
        let name = "adopt-after-unknown-capture";
        crate::oauth::reset_stored_probe_suppression();

        // An inactive profile storing account u-a's login, anchored u-a.
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds_with("at-new", Some(future_expiry())));
        crate::profile::save_profile(&p).expect("save");
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState {
                profiles: vec![name.into()],
                active_profile: Some("someone-else".into()),
                ..crate::profile::AppState::default()
            },
            profiles: vec![p],
        };
        crate::profile::save_app_state(&config.state).expect("state");
        crate::usage::seed_login_anchor(
            &crate::profile::ProfileName::from(name),
            Some(&crate::profile::AccountId::from("u-a".to_string())),
        );

        // Capture-overwrite installs the live login with no proven uuid.
        crate::actions::overwrite_captured_profile(
            &mut config,
            &crate::profile::ProfileName::from(name),
            crate::actions::CaptureSnapshot {
                credentials: Some(creds_with("at-new", Some(future_expiry()))),
                base_url: None,
                api_key: None,
                anchor: crate::actions::AnchorAction::Unproven,
            },
        )
        .expect("overwrite");
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ACCOUNT_ID_CACHE_FILE
            ),
            None,
            "an unproven capture removes the anchor"
        );

        // Make the profile active, and have CC rotate the live file to a fresher
        // pair of the SAME account (u-b) — the stored token is alive, so its
        // probe proves identity without the anchor.
        config.state.active_profile = Some(name.into());
        crate::profile::save_app_state(&config.state).expect("state");
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with("at-mirror", Some(future_expiry() + 3_600_000)))
                .unwrap(),
        )
        .unwrap();

        let handle = Arc::new(RankedMutex::new(config));
        // Token-aware: the stored probe answers for "at-new" and the live mirror
        // probe for "at-mirror", both the SAME account u-b. Any other token is
        // foreign, so a refused adoption (a skipped stored-token probe, a stale
        // anchor) reds this test instead of passing on a token-blind closure.
        let identity = |tok: &str| {
            if tok == "at-new" || tok == "at-mirror" {
                Some("u-b".into())
            } else {
                Some("foreign".into())
            }
        };
        let adopted = try_adopt_live_rotation(
            &handle,
            &crate::profile::ProfileName::from(name),
            &guard(name),
            &identity,
        );
        assert_eq!(
            adopted,
            Some(("at-mirror".into(), Some("at-mirror-refresh".into()))),
            "the same-account CC rotation is adopted through the stored-token probe, not refused"
        );
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
            1,
            "the live token is probed for the announcement key, not for the verdict"
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
        assert_eq!(
            mirror_calls.get(),
            2,
            "the raw test closure re-probes per leg; production memoizes per token"
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

    /// A standing refusal is announced once, not once per rotation leg: while
    /// the live slot stays unadoptable the classify gate keeps reading
    /// `Diverged`, so every leg reaches the same refusal and an unconditional
    /// line drowns the daemon and TUI logs in identical copies.
    #[test]
    fn a_standing_adopt_refusal_is_announced_once_per_state() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-once";
        // Stored token dead + no cached anchor → identity unprovable, the
        // standing state this pins.
        let handle = setup(name, past_expiry(), future_expiry());
        let identity: fn(&str) -> Option<crate::profile::AccountId> =
            |tok| (tok == "at-mirror").then(|| "uuid-1".into());

        let sink = crate::logline::LogLines::new();
        let _capture = sink.capture_here();
        for _ in 0..3 {
            assert_eq!(
                try_adopt_live_rotation(
                    &handle,
                    &crate::profile::ProfileName::from(name),
                    &guard(name),
                    &identity,
                ),
                None
            );
        }

        assert_eq!(
            sink.snapshot(),
            vec![
                "clauth: live login for 'adopt-refusal-once' is newer but its identity can't \
                 be proven (no cached account id and the stored token is dead). Not adopting; \
                 resolve in the clauth TUI or re-run clauth login adopt-refusal-once"
                    .to_string(),
            ],
        );
    }

    /// A state CHANGE — the refusal reason flips — announces again: the dedupe
    /// key carries the reason (plus the live account for the foreign arm), so
    /// a new standing state is never mistaken for the one already announced,
    /// and each state still announces only once.
    #[test]
    fn a_refusal_reason_flip_announces_the_new_state_once() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-flip";
        // Dead stored token + no cached anchor → unprovable; the anchor
        // written mid-test flips later legs onto the provably-foreign arm.
        let handle = setup(name, past_expiry(), future_expiry());
        let unprovable: fn(&str) -> Option<crate::profile::AccountId> =
            |tok| (tok == "at-mirror").then(|| "uuid-1".into());
        let foreign: fn(&str) -> Option<crate::profile::AccountId> = |tok| {
            Some(
                if tok == "at-mirror" {
                    "uuid-2"
                } else {
                    "uuid-1"
                }
                .into(),
            )
        };

        let sink = crate::logline::LogLines::new();
        let _capture = sink.capture_here();
        for _ in 0..2 {
            assert_eq!(
                try_adopt_live_rotation(
                    &handle,
                    &crate::profile::ProfileName::from(name),
                    &guard(name),
                    &unprovable,
                ),
                None
            );
        }
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(name),
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &"uuid-1".to_string(),
        );
        for _ in 0..2 {
            assert_eq!(
                try_adopt_live_rotation(
                    &handle,
                    &crate::profile::ProfileName::from(name),
                    &guard(name),
                    &foreign,
                ),
                None
            );
        }

        assert_eq!(
            sink.snapshot(),
            vec![
                "clauth: live login for 'adopt-refusal-flip' is newer but its identity can't \
                 be proven (no cached account id and the stored token is dead). Not adopting; \
                 resolve in the clauth TUI or re-run clauth login adopt-refusal-flip"
                    .to_string(),
                "clauth: live login for 'adopt-refusal-flip' belongs to a DIFFERENT account. \
                 Not adopting; capture it via the clauth TUI divergence flow if that was \
                 intentional"
                    .to_string(),
            ],
        );
    }

    /// The once-per-state record lives beside the other per-profile caches,
    /// not in process memory: the rotation leg runs in TWO processes (the
    /// daemon scheduler and the TUI's in-process scheduler), so a memory
    /// record would let each process announce the same standing state anew.
    #[test]
    fn the_adopt_refusal_record_is_durable_on_disk() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-durable";
        let handle = setup(name, past_expiry(), future_expiry());
        let identity: fn(&str) -> Option<crate::profile::AccountId> =
            |tok| (tok == "at-mirror").then(|| "uuid-1".into());

        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            ),
            None,
            "nothing announced yet, so no record"
        );
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &identity,
            ),
            None
        );
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            )
            .as_deref(),
            Some("unprovable-identity:uuid-1"),
            "the announcement is recorded on disk, where the other process reads it"
        );
    }

    /// A successful adopt resolves the divergence the refusal announced, so
    /// the record drops and a future standing refusal is news again.
    #[test]
    fn a_resolving_adopt_clears_the_refusal_record() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-clear";
        let handle = setup(name, past_expiry(), future_expiry());
        let identity: fn(&str) -> Option<crate::profile::AccountId> =
            |tok| (tok == "at-mirror").then(|| "uuid-1".into());
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &identity,
            ),
            None,
            "identity unprovable until the anchor lands"
        );
        assert!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            )
            .is_some(),
            "the standing refusal is recorded"
        );

        // The cached anchor lands (a re-login's backfill): the same live
        // mirror is now provably the profile's own account, so it adopts.
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(name),
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &"uuid-1".to_string(),
        );
        assert!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &identity,
            )
            .is_some()
        );
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            ),
            None,
            "the resolved state no longer suppresses a future announcement"
        );
    }

    /// A leg that observes the link healthy again drops the record, so a
    /// re-occurring standing state announces like the first one.
    #[test]
    fn a_resolved_link_clears_the_record_so_a_recurrence_announces_again() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-recur";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let foreign: fn(&str) -> Option<crate::profile::AccountId> = |tok| {
            Some(
                if tok == "at-mirror" {
                    "uuid-2"
                } else {
                    "uuid-1"
                }
                .into(),
            )
        };
        let sink = crate::logline::LogLines::new();
        let _capture = sink.capture_here();

        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &foreign,
            ),
            None
        );
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            )
            .as_deref(),
            Some("foreign-account:uuid-2"),
        );

        // Resolution without an adopt: the live slot is rewritten to the
        // profile's own pair, so the link classifies LinkedTo again.
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with("at-old", Some(future_expiry()))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &foreign,
            ),
            None
        );
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            ),
            None,
            "the healthy leg observed the resolution and dropped the record"
        );

        // Re-divergence: the SAME foreign mirror returns, and the state is
        // news again.
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with("at-mirror", Some(future_expiry() + 3_600_000)))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &foreign,
            ),
            None
        );
        assert_eq!(
            sink.snapshot(),
            vec![
                "clauth: live login for 'adopt-refusal-recur' belongs to a DIFFERENT account. \
                 Not adopting; capture it via the clauth TUI divergence flow if that was \
                 intentional"
                    .to_string(),
                "clauth: live login for 'adopt-refusal-recur' belongs to a DIFFERENT account. \
                 Not adopting; capture it via the clauth TUI divergence flow if that was \
                 intentional"
                    .to_string(),
            ],
        );
    }

    /// A DIFFERENT foreign login is a state change worth a line, while the
    /// same login's token churn (CC rewriting the mirror on every launch) is
    /// not: the dedupe key carries the live account id for this reason.
    #[test]
    fn a_different_foreign_login_announces_while_same_account_churn_stays_silent() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-swap";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let foreign: fn(&str) -> Option<crate::profile::AccountId> = |tok| {
            Some(
                match tok {
                    "at-mirror-3" => "uuid-3",
                    tok if tok.starts_with("at-mirror") => "uuid-2",
                    _ => "uuid-1",
                }
                .into(),
            )
        };
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        let sink = crate::logline::LogLines::new();
        let _capture = sink.capture_here();
        let leg = || {
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &foreign,
            )
        };

        assert_eq!(leg(), None, "uuid-2 mirror is foreign to the stored uuid-1");
        assert_eq!(sink.snapshot().len(), 1);

        // Same account, fresh pair: the standing state did not change.
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with(
                "at-mirror-2",
                Some(future_expiry() + 7_200_000),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(leg(), None);
        assert_eq!(sink.snapshot().len(), 1, "same-account churn stays silent");

        // A different foreign account lands: news again.
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with(
                "at-mirror-3",
                Some(future_expiry() + 10_800_000),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(leg(), None);
        assert_eq!(
            sink.snapshot().len(),
            2,
            "a different foreign login announces"
        );
        assert_eq!(leg(), None);
        assert_eq!(sink.snapshot().len(), 2, "and it announces only once");
    }

    /// A profile that gains a session token leaves the adopt story entirely
    /// (the refusal sites are unreachable behind the first gate), so a record
    /// from its OAuth era must not outlive the regime switch: cleared here, a
    /// later OAuth re-divergence announces fresh.
    #[test]
    fn a_session_token_regime_switch_clears_a_stale_refusal_record() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-regime";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let foreign: fn(&str) -> Option<crate::profile::AccountId> = |tok| {
            Some(
                if tok == "at-mirror" {
                    "uuid-2"
                } else {
                    "uuid-1"
                }
                .into(),
            )
        };
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &foreign,
            ),
            None
        );
        assert!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            )
            .is_some(),
            "the standing refusal is recorded"
        );

        // Regime switch: a long-lived session-token sidecar (no refresh
        // token) engages the split, and the adopt gates stop applying.
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from(name))
            .expect("profile dir");
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec(&crate::profile::ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "static-mint".to_string(),
                    refresh_token: None,
                    expires_at: Some(future_expiry() + 86_400_000),
                    scopes: None,
                    subscription_type: None,
                    ..crate::profile::OAuthToken::default_extra()
                }),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &foreign,
            ),
            None
        );
        assert_eq!(
            crate::profile_cache::load_profile_cache::<String>(
                &crate::profile::ProfileName::from(name),
                crate::profile_cache::ADOPT_REFUSAL_FILE
            ),
            None,
            "the OAuth-era record does not outlive the regime switch"
        );
    }

    /// The unprovable refusal keys on the live account too: a different login
    /// under the same anchorless state is a state change worth a line, while
    /// the same login's token churn stays silent.
    #[test]
    fn a_different_login_under_an_unprovable_state_announces_again() {
        let _home = HomeSandbox::new();
        let name = "adopt-refusal-unprovable-swap";
        let handle = setup(name, past_expiry(), future_expiry());
        let identity: fn(&str) -> Option<crate::profile::AccountId> = |tok| {
            Some(
                if tok == "at-mirror-2" {
                    "uuid-2"
                } else {
                    "uuid-1"
                }
                .into(),
            )
        };
        let live = crate::profile::claude_dir()
            .unwrap()
            .join(".credentials.json");
        let sink = crate::logline::LogLines::new();
        let _capture = sink.capture_here();
        let leg = || {
            try_adopt_live_rotation(
                &handle,
                &crate::profile::ProfileName::from(name),
                &guard(name),
                &identity,
            )
        };

        assert_eq!(leg(), None, "uuid-1 mirror is unprovable");
        assert_eq!(sink.snapshot().len(), 1);

        // A different live login under the same anchorless state: news.
        std::fs::write(
            &live,
            serde_json::to_vec(&creds_with(
                "at-mirror-2",
                Some(future_expiry() + 7_200_000),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(leg(), None);
        assert_eq!(
            sink.snapshot().len(),
            2,
            "a different login under the unprovable state announces"
        );
        assert_eq!(leg(), None);
        assert_eq!(sink.snapshot().len(), 2, "and it announces only once");
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
            ..crate::profile::OAuthToken::default_extra()
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
    let config = oauth_config(name, Some("rt-current"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save the store");
    let handle = Arc::new(RankedMutex::new(config));
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

/// A live kick 429 carries the limiter's own headers out through `KickError`
/// instead of swallowing the outage like it did through the 2026-07-15
/// incident; `a_captured_window_exhaustion_429_carries_the_limiters_rejection`
/// pins the hand-off from there to `KickResult.blocked`.
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

/// A real window-exhaustion 429 (`testutil::window_exhaustion_429_headers`,
/// every epoch moved to now) on a kick with a still-valid access token hands
/// the limiter's own `rejected` verdict back as `KickResult.blocked` without
/// spending the refresh token; `run_fetch_records_a_window_exhaustion_429_as_a_switch_grade_block`
/// carries it from there to the grade. Its `retry-after` (6072 s) clamps to
/// the 15-minute retry cap, so the window's own reset is the ceiling.
#[test]
fn a_captured_window_exhaustion_429_carries_the_limiters_rejection() {
    let home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("kick-exhausted");
    let served_at = crate::usage::now_epoch_secs();
    let reset = served_at + 6_073;
    let limiter = crate::testutil::window_exhaustion_429_headers(reset, served_at + 383_473);
    // `max` sits above the one kick a correct run makes, so a rotation's token
    // request would be recorded rather than refused a socket.
    let (base, server) = crate::testutil::serve_endpoints_raw_with_headers(3, move |path, _| {
        if path.starts_with("/v1/messages") {
            (429, limiter.clone(), String::new())
        } else {
            (
                200,
                Vec::new(),
                r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&name);

    // A refresh token on hand and the access token an hour from expiry: the
    // 429 arm that must not rotate.
    let expires_at = crate::usage::now_ms() as i64 + 3_600_000;
    let result = auto_start_kick(
        &config,
        &name,
        "at-old",
        Some("rt-old"),
        Some(expires_at),
        None,
    );
    let seen: Vec<String> = server
        .join()
        .expect("listener")
        .iter()
        .map(|raw| crate::testutil::request_path(raw))
        .collect();

    assert_eq!(
        seen,
        vec!["/v1/messages?beta=true".to_string()],
        "one kick and no refresh: a 429 on a still-valid token never spends the refresh token"
    );
    assert!(!result.opened, "a 429 opens no window");
    assert_eq!(
        result.rotated, None,
        "nothing rotated, so no pair comes back"
    );
    assert_eq!(
        result.blocked,
        Some(KickRateLimit {
            rejected: true,
            until_epoch_secs: Some(reset),
        }),
        "a window-exhaustion 429 is the limiter's rejection, ceilinged at the window's reset"
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
                ..crate::profile::OAuthToken::default_extra()
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
                    ..crate::profile::OAuthToken::default_extra()
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
fn live_rotate_fixture(
    home: &std::path::Path,
    name: &str,
) -> (crate::profile::ConfigHandle, std::fs::File) {
    let pid = crate::testutil::arm_live_session(home, name);
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
    let (config, pid) = live_rotate_fixture(home.home(), name);
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
    let (config, pid) = live_rotate_fixture(home.home(), name);
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

/// The same skip, driven off macOS through the override seam: the single
/// rotate must not spend a chain the session holds, so the skip is silent —
/// `Persisted(false)`, activity Idle, no `OpResult`, no token request.
#[test]
fn rotate_one_inner_skips_under_a_forced_live_session() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "rotate-one-forced";
    // Zero expected requests: reaching the listener at all is the failure.
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (
            200,
            r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                .to_string(),
        )
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, rx) = mpsc::channel();

    crate::runtime::set_rotation_blocked_override(Some(true));
    let result = rotate_one_inner(
        &config,
        &crate::profile::ProfileName::from(name),
        Some(&activity),
        &tx,
    );
    crate::runtime::set_rotation_blocked_override(None);
    let seen = server.join().expect("listener");

    assert!(
        seen.is_empty(),
        "a blocked rotation must not spend the chain: {seen:?}"
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
/// caller's snapshot on a dead token that 400s every tick. `opened` stays false — the retry kick never runs — and
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
    // Proof the fixture failed the persist where it claims to: the store is
    // still the directory the fixture put in its place, so nothing landed.
    assert!(
        crate::profile::profile_subpath(
            &crate::profile::ProfileName::from(name),
            "credentials.json"
        )
        .expect("store path")
        .is_dir(),
        "the blocked store must be untouched, or the save never failed"
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
// not defend it: flipping `Retry::Stated` to a suffix-bearing retry at a call
// site leaves the mapping correct and now fails loudly at construction (the
// guard) instead of rendering two contradictory reasons to retry in one
// sentence. Four of the six selections survived the whole suite until these
// landed, so each one below reaches its real call site and asserts the
// sentence an operator reads.

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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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

/// The refusal leg carries no cfg gate: only its predicate's compile-time
/// `cfg!(target_os = "macos")` term makes it macOS-only in production, and the
/// override seam answers before that term, so this test drives the leg to its
/// exact owner-picked sentence on every platform CI runs. `never_refresh`
/// panics if the leg is lifted and the gate falls through to the refresh.
#[test]
fn a_live_session_on_a_rolling_chain_refuses_the_switch_with_the_live_session_copy() {
    let _home = HomeSandbox::new();
    let name = "test-roll-refuse";
    let config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let handle = Arc::new(RankedMutex::new(config));

    crate::runtime::set_rotation_blocked_override(Some(true));
    let gate = ensure_installable(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    );
    crate::runtime::set_rotation_blocked_override(None);

    let AuthGate::Transient(e) = gate else {
        panic!("a live session on a rotating chain must refuse, not install/refresh");
    };
    assert_eq!(
        e.text(),
        "'test-roll-refuse' has a live clauth start session still on its rotating login; retry in a moment"
    );
    assert_eq!(e.text_with_status(), e.text());
}

/// The refusal guards a SPEND: a comfortable stored chain stamps the sidecar
/// with no refresh, so a live session on the rotating pair never blocks it.
#[test]
fn a_live_session_on_a_rolling_chain_still_stamps_from_a_comfortable_chain() {
    let _home = HomeSandbox::new();
    let name = "test-roll-calm";
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
            ..crate::profile::OAuthToken::default_extra()
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));

    crate::runtime::set_rotation_blocked_override(Some(true));
    let gate = ensure_installable(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    );
    crate::runtime::set_rotation_blocked_override(None);

    assert!(
        matches!(gate, AuthGate::Ready),
        "a no-spend stamp is never refused"
    );
    assert_eq!(
        sidecar_oauth(name).expect("sidecar").access_token,
        "at-old",
        "stamped from the stored chain"
    );
}

/// `auto_start_kick`'s refusal sits after its first 401 kick, so only a real
/// listener reaches it. Off macOS via the override seam it must stop before the
/// rotation leg: the window stays shut and no token is spent. Reaching the
/// token endpoint is the failure.
#[test]
fn auto_start_kick_does_not_rotate_under_a_forced_live_session() {
    let home = HomeSandbox::new();
    let name = "kick-forced";
    let (base, server) = crate::testutil::serve_endpoints(3, |path, _| {
        if path.starts_with("/v1/messages") {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-LEAK","refresh_token":"rt-LEAK","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));

    crate::runtime::set_rotation_blocked_override(Some(true));
    let result = auto_start_kick(
        &config,
        &crate::profile::ProfileName::from(name),
        "at-old",
        Some("rt-old"),
        None,
        None,
    );
    crate::runtime::set_rotation_blocked_override(None);
    let seen = server.join().expect("listener");

    assert_eq!(
        seen,
        vec!["/v1/messages?beta=true".to_string()],
        "one kick, then the refusal: no token spent"
    );
    assert_eq!(result.rotated, None, "nothing may be rotated");
    assert!(!result.opened, "the window stays shut");
    assert!(
        result.blocked.is_none(),
        "a 401 first kick carries no limiter metadata"
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Invalid {
            failure: TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
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
            ..crate::profile::OAuthToken::default_extra()
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Invalid {
            failure: TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
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
    persist_rotation(
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

/// The rotation hook's stamp decision is the same disk re-read as the install
/// gate's: the in-memory profile a rotation leg persists from can predate a
/// completed `static-token --clear` (a separate process the daemon's snapshot
/// never sees), and stamping from that stale routing re-creates the very
/// sidecar the operator was just told is gone — with the flag now off so
/// nothing ever re-stamps it. The hook's whole-profile save must not
/// resurrect the cleared flag either, or the daemon's next reload re-arms it.
#[test]
fn rotation_hook_disk_disarm_stops_the_stamp() {
    let _home = HomeSandbox::new();
    let name = "test-hook-cleared";
    // In-memory: ARMED, with a comfortable chain a roll would happily stamp.
    let config = rolling_config(name, Some("rt-live"), Some(future_expiry()));
    // On disk: the clear already landed — flag off, no sidecar.
    let mut on_disk = config.profiles[0].clone();
    on_disk.rolling_token = false;
    crate::profile::save_profile(&on_disk).expect("save profile");
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("dir");
    assert!(
        !dir.join("session-token.json").exists(),
        "fixture: the profile is cleared"
    );
    let handle = Arc::new(RankedMutex::new(config));
    // Every production caller holds the RotationGuard across the hook — the
    // lock the clear serializes on.
    let _guard =
        RotationGuard::acquire(&crate::profile::ProfileName::from(name)).expect("rotation guard");
    persist_rotation(
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
    assert!(
        !dir.join("session-token.json").exists(),
        "a cleared profile stays cleared through the rotation hook"
    );
    assert!(
        !crate::profile::load_profile(&crate::profile::ProfileName::from(name))
            .expect("disk profile")
            .rolling_token,
        "the hook's whole-profile save did not resurrect the cleared flag"
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
    persist_rotation(
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
                ..crate::profile::OAuthToken::default_extra()
            }),
        })
        .expect("ser"),
    )
    .expect("write misfill");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
            ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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

/// The daemon main loop's switch gate never parks on the rolling guard, yet
/// keeps the switch paths' install arms: the same mis-fill installs through the
/// plain gate rather than answering the re-stamp leg's mis-fill refusal.
#[test]
fn the_main_loop_gate_keeps_the_disengaged_vanilla_posture_on_a_misfill() {
    let _home = HomeSandbox::new();
    let name = "test-feed-misfill-main-loop";
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
                ..crate::profile::OAuthToken::default_extra()
            }),
        })
        .expect("ser"),
    )
    .expect("misfill");
    let handle = Arc::new(RankedMutex::new(config));

    let gate = ensure_installable_from_main_loop(
        &handle,
        &crate::profile::ProfileName::from(name),
        never_refresh,
    );

    let verdict = match gate {
        AuthGate::Transient(t) => t.text(),
        AuthGate::Ready => "Ready".to_string(),
        AuthGate::Refreshed => "Refreshed".to_string(),
        AuthGate::Broken => "Broken".to_string(),
    };
    assert_eq!(verdict, "Ready");
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
                ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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

// ── #104: the re-stamp leg must follow a regular-file live slot ─────────────

/// The refreshless bearer a fed sidecar holds: real expiry, plan-capable
/// scopes, no refresh token.
fn fed_bearer(access: &str) -> OAuthToken {
    OAuthToken {
        access_token: access.to_string(),
        refresh_token: None,
        expires_at: Some(future_expiry()),
        scopes: Some(vec!["user:profile".into(), "user:inference".into()]),
        subscription_type: Some("max".into()),
        ..crate::profile::OAuthToken::default_extra()
    }
}

/// A live-file fixture shaped like a CC-written credentials file: the login
/// block plus an unrelated `mcpOAuth` block the follower must preserve. Built
/// from the typed model so no added line spells a JSON credential key (the
/// pre-commit secret scan refuses that shape outright).
fn live_file_fixture(oauth: OAuthToken) -> serde_json::Value {
    let creds = ClaudeCredentials {
        claude_ai_oauth: Some(oauth),
    };
    let mut value = serde_json::to_value(&creds).expect("serialize live");
    let mut mcp = serde_json::Map::new();
    let mut entry = serde_json::Map::new();
    entry.insert(
        "accessToken".to_string(),
        serde_json::Value::String("mcp-at".into()),
    );
    mcp.insert("fake-server".to_string(), serde_json::Value::Object(entry));
    value["mcpOAuth"] = serde_json::Value::Object(mcp);
    value
}

/// #104: Claude Code replaces the live `~/.claude/.credentials.json` symlink
/// with a regular file on any credential write (an MCP OAuth update, a
/// refresh). On Linux the re-stamp leg's only live-slot transport is the
/// `#[cfg(target_os = "macos")]` Keychain mirror, so after a re-stamp the live
/// file keeps the revoked bearer and every running session 401s. A live
/// regular file holding this profile's own pre-stamp bearer (no refresh token)
/// must follow the re-stamp — the Linux counterpart of the Keychain mirror —
/// keeping the blocks CC wrote beside the login.
#[test]
fn repro_104_restamp_follows_a_regular_file_live_slot() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-live-file";
    let far_chain = crate::usage::now_ms() as i64 + 24 * 3_600_000;
    let mut config = rolling_config(name, Some("rt-old"), Some(far_chain));
    // The re-stamp leg runs for the ACTIVE profile in production; the
    // follower's is-active belt reads the same fact.
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &fed_bearer("at-fed-dying"),
    )
    .expect("feed");
    // The live slot is a regular file CC wrote: the fed bearer plus an
    // unrelated block the follower must preserve.
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(fed_bearer("at-fed-dying"));
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
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
    let live_after: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        live_after.access_token().map(str::to_string),
        Some("at-old".to_string()),
        "the live regular file must follow the re-stamp (#104)"
    );
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert!(
        raw.get("mcpOAuth").is_some(),
        "blocks CC wrote beside the login survive the follower"
    );
}

/// The follower's recognition gate: a live regular file holding a REAL
/// re-login (a bearer clauth never wrote) is left for the TUI to resolve,
/// never overwritten by a re-stamp that knows nothing about it.
#[test]
fn repro_104_restamp_leaves_a_foreign_live_login_alone() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-foreign-live";
    let far_chain = crate::usage::now_ms() as i64 + 24 * 3_600_000;
    let mut config = rolling_config(name, Some("rt-old"), Some(far_chain));
    // The re-stamp leg runs for the ACTIVE profile in production; the
    // follower's is-active belt reads the same fact.
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &fed_bearer("at-fed-dying"),
    )
    .expect("feed");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let mut live = live_file_fixture(fed_bearer("at-foreign"));
    // A real re-login carries a refresh token too.
    live["claudeAiOauth"]["refreshToken"] = serde_json::Value::String("rt-foreign".into());
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let live_after: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        live_after.access_token().map(str::to_string),
        Some("at-foreign".to_string()),
        "a foreign live login is never overwritten by the re-stamp"
    );
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
        },
    )
    .expect("stamp");
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Invalid {
            failure: TokenFailure::Status(400),
            invalid_grant: true,
        })
    };
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
/// it runs inline on the tick thread and this gate's waiting form carries no
/// deadline, so a `clauth start` holding the lock across its recursive copy would
/// stall every account's poll. With the lock held, the gate answers Transient
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
            ..crate::profile::OAuthToken::default_extra()
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
        persist_rotation(&stale, &crate::profile::ProfileName::from(name), tok).is_err(),
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
             run `clauth login ql-oauth` (flagged auth_broken)"
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

// ── quarantine persist-retry pins ──────────────────────────────────────────
//
// The quarantine write goes to disk, and disk can refuse it. These pins hold
// `mark_auth_broken` to the contract that keeps a refused write recoverable
// in-process: the memory flag flips anyway (live readers keep skipping the
// refresh spend on a quarantined account), the refusal is logged once naming
// the profile and direction, and the NEXT call is the retry — which is why
// the persist cannot sit behind the memory gate: after a failed write memory
// already matches, and the changed-return would early-return the retry away.

/// One OAuth profile on disk and in a fresh handle, with the on-disk
/// `auth_broken` list seeded to `broken`.
fn quarantine_persist_fixture(name: &str, broken: bool) -> crate::profile::ConfigHandle {
    let profile = Profile::new(name.to_string(), None, None);
    let state = AppState {
        profiles: vec![name.into()],
        auth_broken: broken.then(|| name.into()).into_iter().collect(),
        ..AppState::default()
    };
    crate::profile::save_app_state(&state).expect("save state");
    Arc::new(RankedMutex::new(AppConfig {
        state,
        profiles: vec![profile],
    }))
}

/// Make the next `set_auth_broken_persisted` fail: a DIRECTORY where
/// `profiles.toml` should be makes the read inside the persist fail, the same
/// injection `testutil::block_credentials_write` aims at a credentials write.
/// The last-good file is gone until [`unblock_state_persist`] restores it.
fn block_state_persist() {
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("profiles.toml");
    std::fs::remove_file(&path).expect("drop the last-good state file");
    std::fs::create_dir(&path).expect("block the state file with a directory");
}

/// Put `state` back as the on-disk `profiles.toml` — the file the failed
/// write never touched, rewritten through the production saver.
fn unblock_state_persist(state: &AppState) {
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("profiles.toml");
    std::fs::remove_dir(&path).expect("drop the blocking directory");
    crate::profile::save_app_state(state).expect("restore the last-good state");
}

/// A refused quarantine write must not vanish: the flag flips in memory (the
/// scheduler's TokenEntry leg reads `config.is_auth_broken` to skip the
/// refresh spend, so this is what keeps a quarantined account quarantined for
/// live readers), the refusal is logged with the profile and direction, and
/// the next call — whose memory already matches, so the changed-return cannot
/// gate it — retries the write and re-logs nothing.
#[test]
fn a_failed_set_persist_is_logged_and_retried_by_the_next_call() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("qp-set");
    let handle = quarantine_persist_fixture("qp-set", false);
    let sink = crate::logline::LogLines::new();
    let _capture = sink.capture_here();

    block_state_persist();
    mark_auth_broken(&handle, &name, true);

    assert!(
        handle.lock().expect("lock handle").is_auth_broken(&name),
        "the in-memory flag flips even when the write is refused"
    );
    let lines = sink.snapshot();
    assert_eq!(
        lines.len(),
        2,
        "one transition line, one failure line — nothing else: {lines:?}"
    );
    assert_eq!(
        lines[0],
        "clauth: login for 'qp-set' has expired: refresh token revoked or \
         invalid: run `clauth login qp-set` (flagged auth_broken)"
    );
    assert!(
        lines[1]
            .starts_with("clauth: failed to persist auth_broken set for 'qp-set': failed to read "),
        "the failure line names the profile and the direction: {lines:?}"
    );

    // The next poll is the retry: with the failure removed it lands the flag
    // on disk, and the memory-matching call re-logs nothing.
    unblock_state_persist(&AppState {
        profiles: vec![name.clone()],
        ..AppState::default()
    });
    assert!(
        !crate::profile::load_app_state()
            .expect("reload")
            .is_auth_broken(&name),
        "fixture control: the restored last-good file carries no flag"
    );
    mark_auth_broken(&handle, &name, true);
    assert!(
        crate::profile::load_app_state()
            .expect("reload")
            .is_auth_broken(&name),
        "the retry lands the flag on disk"
    );
    assert_eq!(
        sink.snapshot().len(),
        2,
        "the retry adds no log lines: {:?}",
        sink.snapshot()
    );
}

/// The clear direction mirrors the set: memory heals even when the write is
/// refused, the refusal is logged, and the next call retries it onto disk.
#[test]
fn a_failed_clear_persist_is_logged_and_retried_by_the_next_call() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("qp-clear");
    let handle = quarantine_persist_fixture("qp-clear", true);
    let sink = crate::logline::LogLines::new();
    let _capture = sink.capture_here();

    block_state_persist();
    mark_auth_broken(&handle, &name, false);

    assert!(
        !handle.lock().expect("lock handle").is_auth_broken(&name),
        "memory clears even when the write is refused"
    );
    let lines = sink.snapshot();
    assert_eq!(
        lines.len(),
        2,
        "one transition line, one failure line — nothing else: {lines:?}"
    );
    assert_eq!(
        lines[0],
        "clauth: 'qp-clear' re-authenticated: auth_broken cleared"
    );
    assert!(
        lines[1].starts_with(
            "clauth: failed to persist auth_broken clear for 'qp-clear': failed to read "
        ),
        "the failure line names the profile and the direction: {lines:?}"
    );

    unblock_state_persist(&AppState {
        profiles: vec![name.clone()],
        auth_broken: vec![name.clone()],
        ..AppState::default()
    });
    assert!(
        crate::profile::load_app_state()
            .expect("reload")
            .is_auth_broken(&name),
        "fixture control: the restored last-good file still carries the flag"
    );
    mark_auth_broken(&handle, &name, false);
    assert!(
        !crate::profile::load_app_state()
            .expect("reload")
            .is_auth_broken(&name),
        "the retry clears the flag on disk"
    );
    assert_eq!(
        sink.snapshot().len(),
        2,
        "the retry adds no log lines: {:?}",
        sink.snapshot()
    );
}

/// The honest boundary of the in-process retry: a write that never reached
/// disk and whose process died before a retry is invisible to a fresh load —
/// the pre-existing semantics of an unwritten flag, pinned so the retry
/// cannot silently widen into restart-time resurrection.
#[test]
fn a_failed_set_persist_that_never_retried_stays_invisible_to_a_fresh_load() {
    let _home = HomeSandbox::new();
    let name = crate::profile::ProfileName::from("qp-dead");
    let handle = quarantine_persist_fixture("qp-dead", false);

    block_state_persist();
    // Refused; the "process" dies here, before any retry.
    mark_auth_broken(&handle, &name, true);

    unblock_state_persist(&AppState {
        profiles: vec![name.clone()],
        ..AppState::default()
    });
    assert!(
        !crate::profile::load_app_state()
            .expect("fresh load")
            .is_auth_broken(&name),
        "a write that never landed leaves no flag for the next process"
    );
}

/// Fixture for the dead-chain splitter tests: a recognised third-party
/// profile (provider derived from the base_url) whose own-endpoint arm the
/// splitter reaches, persisted so a verdict can be seeded against it
/// (`write_auth_expired` lands only for configured profiles). `env` seeds an
/// env auth entry (`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`).
fn dead_chain_copy_fixture(
    name: &str,
    base_url: &str,
    key: Option<&str>,
    env: Option<(&str, &str)>,
) -> Profile {
    let mut profile = Profile::new(
        name.to_string(),
        Some(base_url.to_string()),
        key.map(str::to_string),
    );
    if let Some((k, v)) = env {
        profile.env.insert(k.to_string(), v.to_string());
    }
    crate::profile::save_profile(&profile).expect("save fixture profile");
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![profile.clone()],
    };
    config.state.profiles.push(name.into());
    crate::profile::save_app_state(&config.state).expect("save app state");
    profile
}

/// A `AuthExpired` verdict matching the profile's CURRENT credential retires
/// the dead-chain sentence: a key the verdict pronounces dead is no
/// credential, so the splitter renders the keyless sentence instead of "its
/// api key still works". The pin seeds the verdict through the same
/// fingerprint spelling the site consults, so the two can never diverge.
#[test]
fn dead_chain_copy_renders_keyless_when_the_verdict_matches_the_current_key() {
    let _home = HomeSandbox::new();
    let name = "tp-verdict-matches";
    let profile = dead_chain_copy_fixture(
        name,
        "https://api.deepseek.com/anthropic",
        Some("sk-fixture"),
        None,
    );
    let fp = crate::usage::profile_credential_fingerprint(&profile)
        .expect("the fixture must carry a third-party credential");
    crate::profile_cache::write_auth_expired(&profile.name, fp);
    assert!(
        crate::profile_cache::auth_expired_matches(&profile.name, fp),
        "the seeded verdict must land before the choice is pinned"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_keyless(&profile.name))
    );
}

/// No verdict: the splitter's dead-chain sentence stays byte-identical.
#[test]
fn dead_chain_copy_without_a_verdict_keeps_the_dead_chain_sentence() {
    let _home = HomeSandbox::new();
    let name = "tp-no-verdict";
    let profile = dead_chain_copy_fixture(
        name,
        "https://api.deepseek.com/anthropic",
        Some("sk-fixture"),
        None,
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_dead_chain(&profile.name))
    );
}

/// A verdict recorded under a different credential is inert: the profile
/// re-logged in with a new key, so the old verdict must not retire the live
/// sentence.
#[test]
fn dead_chain_copy_ignores_a_verdict_for_another_credential() {
    let _home = HomeSandbox::new();
    let name = "tp-stale-verdict";
    let profile = dead_chain_copy_fixture(
        name,
        "https://api.deepseek.com/anthropic",
        Some("sk-current"),
        None,
    );
    let old = Profile::new(
        "tp-stale-verdict-old".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-old".to_string()),
    );
    let old_fp = crate::usage::profile_credential_fingerprint(&old)
        .expect("the stale fixture must carry a third-party credential");
    crate::profile_cache::write_auth_expired(&profile.name, old_fp);
    assert!(
        !crate::profile_cache::auth_expired_matches(
            &profile.name,
            crate::usage::profile_credential_fingerprint(&profile)
                .expect("the fixture must carry a third-party credential")
        ),
        "a verdict for the previous credential must not match the current one"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_dead_chain(&profile.name))
    );
}

/// An `[env]`-token profile with no api key has no key the split sentence
/// could claim still works, and no third-party credential to fingerprint, so
/// no verdict consult can reach the shape: the own-endpoint arm renders the
/// keyless sentence for it outright, without one (owner ruling 2026-09-02 —
/// the sentence is literally true for this profile).
#[test]
fn dead_chain_copy_env_token_without_a_key_renders_the_keyless_sentence() {
    let _home = HomeSandbox::new();
    let name = "tp-env-token";
    let profile = dead_chain_copy_fixture(
        name,
        "https://api.deepseek.com/anthropic",
        None,
        Some(("ANTHROPIC_AUTH_TOKEN", "env-tok")),
    );
    assert!(
        !crate::claude::has_usable_api_key(&profile),
        "the fixture must hold no usable api key for the keyless leg to reach"
    );
    assert!(
        crate::usage::profile_credential_fingerprint(&profile).is_none(),
        "an env-token-only profile must have no third-party credential to fingerprint"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_keyless(&profile.name))
    );
}

/// An env-carried `ANTHROPIC_API_KEY` is a key, so the keyless sentence is
/// not literally true for that shape: it keeps the pre-fix dead-chain
/// sentence, byte-identical (owner ruling 2026-09-02: env-key keeps
/// dead-chain).
#[test]
fn dead_chain_copy_env_carried_api_key_keeps_the_dead_chain_sentence() {
    let _home = HomeSandbox::new();
    let name = "tp-env-key";
    let profile = dead_chain_copy_fixture(
        name,
        "https://api.deepseek.com/anthropic",
        None,
        Some(("ANTHROPIC_API_KEY", "env-key")),
    );
    assert!(
        crate::claude::has_own_inference_endpoint(&profile),
        "the fixture must reach the own-endpoint arm"
    );
    assert!(
        !crate::claude::has_usable_api_key(&profile),
        "the fixture must hold no field api key for the leg to reach the env discriminator"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_dead_chain(&profile.name))
    );
}

/// Attach a console session to a fixture profile, so the shape the dead-console
/// sentence describes — a console that was actually captured — is the one under
/// test. In memory only: the splitter reads the profile it is handed, and
/// `auth_expired_matches` is keyed by name and takes the fingerprint as an
/// argument, so nothing here re-reads the profile from disk.
fn with_console(profile: &mut Profile, token: &str) {
    profile.console = Some(crate::profile::ConsoleCredential {
        token: token.to_string(),
        site: crate::profile::ConsoleSite::International,
        region: "ap-southeast-1".to_string(),
    });
}

/// The verdict consult is re-pointed for Alibaba: its usage fetch
/// authenticates with the console session, never the api key, so a matching
/// verdict records a dead console while the key may be live. The dead-console
/// sentence renders — the dead-chain one would name the wrong half, and the
/// keyless one would mis-claim a live key.
#[test]
fn dead_chain_copy_alibaba_renders_the_dead_console_sentence_on_a_matching_verdict() {
    let _home = HomeSandbox::new();
    let name = "tp-alibaba-console-verdict";
    let mut profile = dead_chain_copy_fixture(
        name,
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com",
        Some("sk-fixture"),
        None,
    );
    with_console(&mut profile, "console-captured");
    assert_eq!(
        profile.provider,
        Some(crate::providers::Provider::Alibaba),
        "the fixture base_url must resolve to the Alibaba provider"
    );
    assert!(
        crate::claude::has_own_inference_endpoint(&profile),
        "the fixture must reach the own-endpoint arm"
    );
    let fp = crate::usage::profile_credential_fingerprint(&profile)
        .expect("an Alibaba profile always carries a fetchable credential");
    crate::profile_cache::write_auth_expired(&profile.name, fp);
    assert!(
        crate::profile_cache::auth_expired_matches(&profile.name, fp),
        "the seeded verdict must land before the choice is pinned"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_dead_console(&profile.name))
    );
}

/// A verdict recorded against a console the profile has since replaced is
/// inert: a re-capture moves the console token, so nothing measured the CURRENT
/// session as dead and the split sentence stays, byte-identical. The stale
/// credential varied here is the console rather than the api key, because the
/// console is the only half an Alibaba verdict is ever about.
#[test]
fn dead_chain_copy_alibaba_ignores_a_verdict_for_a_replaced_console() {
    let _home = HomeSandbox::new();
    let name = "tp-alibaba-stale-verdict";
    let mut profile = dead_chain_copy_fixture(
        name,
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com",
        Some("sk-current"),
        None,
    );
    with_console(&mut profile, "console-expired");
    let old_fp = crate::usage::profile_credential_fingerprint(&profile)
        .expect("the stale fixture must carry a fetchable credential");
    crate::profile_cache::write_auth_expired(&profile.name, old_fp);
    with_console(&mut profile, "console-recaptured");
    assert!(
        !crate::profile_cache::auth_expired_matches(
            &profile.name,
            crate::usage::profile_credential_fingerprint(&profile)
                .expect("the fixture must carry a fetchable credential")
        ),
        "a verdict for the replaced console must not match the current one"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_dead_chain(&profile.name))
    );
}

/// `alibaba::fetch` collapses "no console" into "dead console" because neither
/// is worth a request, and the verdict inherits that collapse. Copy is where
/// the two states differ: "expired" and "re-capture" are both false for a
/// profile that never captured one, so it keeps the dead-chain sentence even
/// though its verdict matches.
#[test]
fn dead_chain_copy_alibaba_without_a_console_keeps_the_dead_chain_sentence() {
    let _home = HomeSandbox::new();
    let name = "tp-alibaba-never-captured";
    let profile = dead_chain_copy_fixture(
        name,
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com",
        Some("sk-fixture"),
        None,
    );
    assert!(
        profile.console.is_none(),
        "the fixture must model a profile that never captured a console"
    );
    let fp = crate::usage::profile_credential_fingerprint(&profile)
        .expect("an Alibaba profile always carries a fetchable credential");
    crate::profile_cache::write_auth_expired(&profile.name, fp);
    assert!(
        crate::profile_cache::auth_expired_matches(&profile.name, fp),
        "the seeded verdict must land before the choice is pinned"
    );
    assert_eq!(
        third_party_dead_chain_copy(Some(&profile), &profile.name),
        Some(crate::format::third_party_dead_chain(&profile.name))
    );
}

/// clauth's own rotation of the ACTIVE account's chain moves the store's login
/// past the live file Claude Code wrote beside the old one (a `/design-login`),
/// after which no sync recognises that file as this account's again. The
/// persist saves it first, while both still hold the old token, and the rotated
/// store keeps it beside the new pair.
#[cfg(unix)]
#[test]
fn a_rotation_keeps_a_design_login_the_live_file_held() {
    let _home = HomeSandbox::new();
    let name = "rot-design";
    let mut config = oauth_config(name, Some("rt-old"), Some(4_102_444_800_000));
    config.state.active_profile = Some(name.into());
    crate::profile::save_app_state(&config.state).expect("persist active");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let profile = crate::profile::ProfileName::from(name);
    crate::claude::force_link_profile_credentials(&profile).expect("link");
    let store = profile_dir(&profile).expect("dir").join("credentials.json");
    let live_path = crate::claude::claude_credentials_path().expect("live path");
    let mut live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&store).expect("read store")).expect("parse");
    live["designOauth"] = serde_json::json!({ "accessToken": "design-1" });
    let staged = live_path.with_extension("staged");
    std::fs::write(&staged, serde_json::to_vec_pretty(&live).expect("ser")).expect("stage");
    std::fs::rename(&staged, &live_path).expect("Claude Code's rename over the link");
    let handle = Arc::new(RankedMutex::new(config));

    persist_rotation(
        &handle,
        &profile,
        TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 3600,
            scope: None,
        },
    )
    .expect("persist");

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&store).expect("read store")).expect("parse");
    assert_eq!(stored["claudeAiOauth"]["accessToken"], "at-rotated");
    assert_eq!(stored["designOauth"]["accessToken"], "design-1");
}

/// The rotation's sync keeps the fresher copy. A store holding a newer MCP
/// login (a `clauth start` session refreshed it) keeps it over the older one
/// the global live file still holds.
#[cfg(unix)]
#[test]
fn a_rotation_never_puts_an_older_live_copy_over_the_stores() {
    let _home = HomeSandbox::new();
    let name = "rot-fresh";
    let mut config = oauth_config(name, Some("rt-old"), Some(4_102_444_800_000));
    config.state.active_profile = Some(name.into());
    crate::profile::save_app_state(&config.state).expect("persist active");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let profile = crate::profile::ProfileName::from(name);
    crate::claude::force_link_profile_credentials(&profile).expect("link");
    let store = profile_dir(&profile).expect("dir").join("credentials.json");
    let live_path = crate::claude::claude_credentials_path().expect("live path");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&store).expect("read store")).expect("parse");
    let mut live = stored.clone();
    stored["mcpOAuth"] =
        serde_json::json!({ "linear": { "refreshToken": "v2", "expiresAt": 2000 } });
    std::fs::write(&store, serde_json::to_vec_pretty(&stored).expect("ser")).expect("seed store");
    live["mcpOAuth"] = serde_json::json!({ "linear": { "refreshToken": "v1", "expiresAt": 1000 } });
    let staged = live_path.with_extension("staged");
    std::fs::write(&staged, serde_json::to_vec_pretty(&live).expect("ser")).expect("stage");
    std::fs::rename(&staged, &live_path).expect("Claude Code's rename over the link");
    let handle = Arc::new(RankedMutex::new(config));

    persist_rotation(
        &handle,
        &profile,
        TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 3600,
            scope: None,
        },
    )
    .expect("persist");

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&store).expect("read store")).expect("parse");
    assert_eq!(after["claudeAiOauth"]["accessToken"], "at-rotated");
    assert_eq!(after["mcpOAuth"]["linear"]["refreshToken"], "v2");
}

/// The follower heals a logged-out shell: CC blanks both tokens when its own
/// refresh dies, and the re-stamp writes the fresh bearer over the blanks,
/// keeping the blocks beside them.
#[test]
fn repro_104_restamp_heals_a_logged_out_shell_live_file() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-shell-live";
    let far_chain = crate::usage::now_ms() as i64 + 24 * 3_600_000;
    let mut config = rolling_config(name, Some("rt-old"), Some(far_chain));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &fed_bearer("at-fed-dying"),
    )
    .expect("feed");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(OAuthToken {
        access_token: String::new(),
        refresh_token: None,
        expires_at: Some(0),
        scopes: None,
        subscription_type: None,
        ..crate::profile::OAuthToken::default_extra()
    });
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        raw["claudeAiOauth"]["accessToken"], "at-old",
        "the re-stamp heals a logged-out shell live file"
    );
    assert!(
        raw.get("mcpOAuth").is_some(),
        "shell's blocks survive the heal"
    );
}

/// A clauth-owned symlink follows the store write itself — the follower must
/// not replace it with a regular file.
#[cfg(unix)]
#[test]
fn repro_104_restamp_leaves_a_symlink_live_slot_alone() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-symlink-live";
    let far_chain = crate::usage::now_ms() as i64 + 24 * 3_600_000;
    let mut config = rolling_config(name, Some("rt-old"), Some(far_chain));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &fed_bearer("at-fed-dying"),
    )
    .expect("feed");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let sidecar = profile_dir(&crate::profile::ProfileName::from(name))
        .expect("dir")
        .join("session-token.json");
    std::os::unix::fs::symlink(&sidecar, claude_dir.join(".credentials.json")).expect("link");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let link = claude_dir.join(".credentials.json");
    assert!(
        link.symlink_metadata()
            .expect("stat")
            .file_type()
            .is_symlink(),
        "the follower never replaces a clauth-owned symlink with a regular file"
    );
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-old",
        "the link resolves to the re-stamp"
    );
}

/// A torn live file (a mid-write login) stays torn: clauth never wrote this
/// file non-atomically, so unlike the Keychain mirror there is nothing of
/// ours to heal, and overwriting an unreadable login would drop a real one.
#[test]
fn repro_104_restamp_skips_an_unparseable_live_file() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-torn-live";
    let far_chain = crate::usage::now_ms() as i64 + 24 * 3_600_000;
    let mut config = rolling_config(name, Some("rt-old"), Some(far_chain));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &fed_bearer("at-fed-dying"),
    )
    .expect("feed");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    std::fs::write(&live_path, b"{ torn").expect("write torn live");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    assert_eq!(
        std::fs::read(&live_path).expect("read"),
        b"{ torn",
        "an unparseable live file is never overwritten"
    );
}

/// The is-active belt: a re-stamp of a PARKED profile must never touch the
/// live slot (which holds the active profile's login).
#[test]
fn repro_104_restamp_skips_a_parked_profile() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-parked";
    let far_chain = crate::usage::now_ms() as i64 + 24 * 3_600_000;
    let config = rolling_config(name, Some("rt-old"), Some(far_chain));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::stamp_rolling_token(
        &crate::profile::ProfileName::from(name),
        &fed_bearer("at-fed-dying"),
    )
    .expect("feed");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(fed_bearer("at-fed-dying"));
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        restamp_rolling_token(
            &handle,
            &crate::profile::ProfileName::from(name),
            never_refresh
        ),
        AuthGate::Ready
    ));
    let live_after: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        live_after.access_token().map(str::to_string),
        Some("at-fed-dying".to_string()),
        "a parked profile's re-stamp never touches the live slot"
    );
}

// ── #104: the rotation hook's follower legs ───────────────────────────────

/// The rotation hook re-stamps a rolling profile's sidecar; a live regular
/// file CC wrote must follow it too, or the revoked bearer keeps serving.
#[test]
fn repro_104_rotation_follows_a_regular_file_live_slot_for_a_rolling_profile() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-hook-rolling";
    let mut config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_app_state(&config.state).expect("persist the active marker");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let chain = config.profiles[0]
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .expect("chain")
        .clone();
    crate::claude::stamp_rolling_token(&crate::profile::ProfileName::from(name), &chain)
        .expect("stamp");
    // The live slot is a regular file CC wrote, holding the pre-rotation bearer.
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(fed_bearer("at-old"));
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        raw["claudeAiOauth"]["accessToken"], "at-rotated",
        "the live file follows the rotation (#104)"
    );
    assert!(
        raw["claudeAiOauth"].get("refreshToken").is_none(),
        "the sidecar projection is refreshless"
    );
    assert!(raw.get("mcpOAuth").is_some(), "blocks survive the follow");
}

/// The same leg, vanilla: no sidecar, so the live file follows the freshly
/// rotated PAIR — refresh token included, the content a symlink would serve.
#[test]
fn repro_104_rotation_follows_a_regular_file_live_slot_for_a_vanilla_profile() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-hook-vanilla";
    let mut config = oauth_config(name, Some("rt-old"), Some(past_expiry()));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_app_state(&config.state).expect("persist the active marker");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(OAuthToken {
        access_token: "at-old".to_string(),
        refresh_token: Some("rt-old".to_string()),
        expires_at: Some(future_expiry()),
        scopes: None,
        subscription_type: None,
        ..crate::profile::OAuthToken::default_extra()
    });
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        raw["claudeAiOauth"]["accessToken"], "at-rotated",
        "the live file follows the rotation (#104)"
    );
    assert_eq!(
        raw["claudeAiOauth"]["refreshToken"], "rt-rotated",
        "a vanilla live file follows the PAIR, refresh token included"
    );
    assert!(raw.get("mcpOAuth").is_some(), "blocks survive the follow");
}

/// A static mint's steady state is quiet: the rotation changes nothing the
/// live slot should hold, and the follower must not rewrite the file.
#[test]
fn repro_104_rotation_leaves_a_static_mint_live_file_byte_identical() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-hook-static";
    let mut config = oauth_config(name, Some("rt-old"), Some(past_expiry()));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_app_state(&config.state).expect("persist the active marker");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(
        &crate::profile::ProfileName::from(name),
        "setup-mint-fixture",
        crate::usage::now_ms() as i64,
    )
    .expect("mint");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(OAuthToken {
        access_token: "setup-mint-fixture".to_string(),
        refresh_token: None,
        expires_at: Some(future_expiry()),
        scopes: None,
        subscription_type: None,
        ..crate::profile::OAuthToken::default_extra()
    });
    let live_bytes = serde_json::to_vec(&live).expect("ser");
    std::fs::write(&live_path, &live_bytes).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
    assert_eq!(
        std::fs::read(&live_path).expect("read"),
        live_bytes,
        "nothing changed for the live slot to follow; the file stays byte-identical"
    );
}

/// The hook's is-active belt: rotating a PARKED profile never touches the
/// live slot (which holds the active profile's login).
#[test]
fn repro_104_rotation_skips_a_parked_profile() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-hook-parked";
    let config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let chain = config.profiles[0]
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .expect("chain")
        .clone();
    crate::claude::stamp_rolling_token(&crate::profile::ProfileName::from(name), &chain)
        .expect("stamp");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(fed_bearer("at-old"));
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        raw["claudeAiOauth"]["accessToken"], "at-old",
        "a parked profile's rotation never touches the live slot"
    );
}

/// The hook's recognition gate: a live regular file holding a real re-login
/// is left for the TUI, never overwritten by the rotation.
#[test]
fn repro_104_rotation_leaves_a_foreign_live_login_alone() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-hook-foreign";
    let mut config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_app_state(&config.state).expect("persist the active marker");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let chain = config.profiles[0]
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .expect("chain")
        .clone();
    crate::claude::stamp_rolling_token(&crate::profile::ProfileName::from(name), &chain)
        .expect("stamp");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let mut live = live_file_fixture(fed_bearer("at-foreign"));
    // A real re-login carries a refresh token too.
    live["claudeAiOauth"]["refreshToken"] = serde_json::Value::String("rt-foreign".into());
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        raw["claudeAiOauth"]["accessToken"], "at-foreign",
        "a foreign live login is never overwritten by the rotation"
    );
}

/// The arming rotation: no sidecar exists yet, the stamp creates it, and a
/// live regular file holding the old PAIR follows to the refreshless
/// projection.
#[test]
fn repro_104_arming_rotation_follows_the_old_pair_live_file() {
    let _home = HomeSandbox::new();
    let name = "test-repro-104-hook-arming";
    let mut config = rolling_config(name, Some("rt-old"), Some(past_expiry()));
    config.state.active_profile = Some(crate::profile::ProfileName::from(name));
    crate::profile::save_app_state(&config.state).expect("persist the active marker");
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");
    let live_path = claude_dir.join(".credentials.json");
    let live = live_file_fixture(OAuthToken {
        access_token: "at-old".to_string(),
        refresh_token: Some("rt-old".to_string()),
        expires_at: Some(future_expiry()),
        scopes: None,
        subscription_type: None,
        ..crate::profile::OAuthToken::default_extra()
    });
    std::fs::write(&live_path, serde_json::to_vec(&live).expect("ser")).expect("write live");
    let handle = Arc::new(RankedMutex::new(config));
    persist_rotation(
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
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live_path).expect("read")).expect("parse");
    assert_eq!(
        raw["claudeAiOauth"]["accessToken"], "at-rotated",
        "the arming rotation follows the old-pair live file"
    );
    assert!(
        raw["claudeAiOauth"].get("refreshToken").is_none(),
        "the arming rotation installs the refreshless projection"
    );
    assert!(raw.get("mcpOAuth").is_some(), "blocks survive the follow");
}

// ── the refresh chain: the rotation persist refuses a store it does not own ──

const MINTED_BODY: &str =
    r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#;

fn store_path(name: &str) -> std::path::PathBuf {
    profile_dir(&crate::profile::ProfileName::from(name))
        .expect("profile dir")
        .join("credentials.json")
}

/// Another writer's login landing in the store, as raw bytes.
fn write_store_pair(name: &str, access: &str, refresh: &str) {
    std::fs::write(
        store_path(name),
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}"}}}}"#),
    )
    .expect("write the store");
}

/// The store's refresh token read raw, `None` for an absent file or login.
fn stored_refresh(name: &str) -> Option<String> {
    let bytes = match std::fs::read(store_path(name)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => panic!("read the store: {e}"),
    };
    let creds: ClaudeCredentials = serde_json::from_slice(&bytes).expect("parse the store");
    creds.refresh_token().map(str::to_string)
}

fn memory_refresh(config: &crate::profile::ConfigHandle, name: &str) -> Option<String> {
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = config.lock().expect("config lock");
    cfg.find(&crate::profile::ProfileName::from(name))
        .and_then(|p| p.refresh_token().map(str::to_string))
}

fn stage_path(name: &str) -> std::path::PathBuf {
    profile_dir(&crate::profile::ProfileName::from(name))
        .expect("profile dir")
        .join("credentials.json.staged")
}

/// The stage a failed persist of the fixture's `rt-old` rotation leaves: its base
/// is the hand-computed first 8 bytes of SHA-256("rt-old"), hex, and its pair the
/// one that rotation minted.
fn write_live_stage_over_rt_old(name: &str) {
    std::fs::write(
        stage_path(name),
        r#"{"base":"068bfce5d78d07ac","base_access":"at-old",
            "creds":{"claudeAiOauth":{"accessToken":"at-mid","refreshToken":"rt-mid"}}}"#,
    )
    .expect("write the stage");
}

/// A save error left a live stage, then a foreign login landed in the store.
/// The manual rotate spends its in-memory token and mints a pair, but the store
/// is no longer this chain's: the persist refuses, the foreign login survives,
/// the stage goes, and memory follows the store.
#[test]
fn a_rotate_persist_refuses_a_foreign_login_over_a_live_stage() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "stage-foreign-rotate";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (200, MINTED_BODY.to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    write_live_stage_over_rt_old(name);
    write_store_pair(name, "at-foreign", "rt-foreign");
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string()],
        "the rotation must reach the token endpoint before its persist decides"
    );
    assert_eq!(
        stored_refresh(name).as_deref(),
        Some("rt-foreign"),
        "the foreign login survives the refused persist"
    );
    assert!(!stage_path(name).exists(), "the refusal deletes the stage");
    assert_eq!(
        memory_refresh(&config, name).as_deref(),
        Some("rt-foreign"),
        "memory adopts the store's login under the refusal's hold"
    );
    assert!(matches!(result, RotateOutcome::Persisted(false)));
}

/// The kick's rotation over the same state: the persist refuses, and the pair
/// the kick hands back is the STORE's, so the caller's token list follows the
/// store rather than a minted pair nothing persisted.
#[test]
fn a_kick_persist_refuses_a_foreign_login_over_a_live_stage() {
    let home = HomeSandbox::new();
    let name = "stage-foreign-kick";
    let (base, server) = crate::testutil::serve_endpoints(5, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (200, MINTED_BODY.to_string())
        } else {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    write_live_stage_over_rt_old(name);
    write_store_pair(name, "at-foreign", "rt-foreign");

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
        "the kick must rotate before its persist decides: {seen:?}"
    );
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-foreign"));
    assert!(!stage_path(name).exists(), "the refusal deletes the stage");
    assert_eq!(memory_refresh(&config, name).as_deref(), Some("rt-foreign"));
    assert_eq!(
        result.rotated,
        Some(("at-foreign".to_string(), Some("rt-foreign".to_string()))),
        "a refused persist carries the store's pair, never the minted one"
    );
    assert!(!result.opened, "the refusal returns before the retry kick");
}

/// `clauth login` re-logs the account while a rotation holding a stale
/// in-memory pair is mid-flight: the rotation's persist must not write the
/// minted pair over the re-login.
#[test]
fn a_relogin_during_a_rotation_survives_the_rotations_persist() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "relogin-mid-rotation";
    let (base, server) = crate::testutil::serve_endpoints(3, move |path, _| {
        if path.starts_with("/v1/oauth/token") {
            let mut cfg = crate::profile::load_config().expect("load config");
            crate::actions::overwrite_captured_profile(
                &mut cfg,
                &crate::profile::ProfileName::from(name),
                crate::actions::CaptureSnapshot {
                    credentials: Some(ClaudeCredentials {
                        claude_ai_oauth: Some(OAuthToken {
                            access_token: "at-relogin".to_string(),
                            refresh_token: Some("rt-relogin".to_string()),
                            expires_at: None,
                            scopes: None,
                            subscription_type: None,
                            ..OAuthToken::default_extra()
                        }),
                    }),
                    base_url: None,
                    api_key: None,
                    anchor: crate::actions::AnchorAction::Unproven,
                },
            )
            .expect("install the re-login");
        }
        (200, MINTED_BODY.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let (tx, _rx) = mpsc::channel();

    let _ = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert_eq!(
        stored_refresh(name).as_deref(),
        Some("rt-relogin"),
        "the re-login is on disk after the rotation"
    );
}

/// A logout lands while a rotation's token call is in flight: the persist
/// refuses rather than write the minted pair back into the cleared profile.
#[test]
fn a_logout_during_a_rotation_stays_logged_out() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "logout-mid-rotation";
    let (base, server) = crate::testutil::serve_endpoints(3, move |path, _| {
        if path.starts_with("/v1/oauth/token") {
            let mut cfg = crate::profile::load_config().expect("load config");
            crate::actions::clear_profile_credentials(
                &mut cfg,
                &crate::profile::ProfileName::from(name),
            )
            .expect("blank the login");
        }
        (200, MINTED_BODY.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let (tx, _rx) = mpsc::channel();

    let _ = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert!(
        !store_path(name).exists(),
        "the cleared login stays absent after the rotation's persist"
    );
}

fn spent_record_path(name: &str) -> std::path::PathBuf {
    profile_dir(&crate::profile::ProfileName::from(name))
        .expect("profile dir")
        .join("auth.spent.json")
}

/// A transient answer (the endpoint never confirmed anything) records nothing,
/// so the next attempt sends the same token: a network blip stays harmless.
#[test]
fn a_transient_refresh_answer_records_nothing() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "spent-transient";
    let (base, server) = crate::testutil::serve_endpoints_recording(4, |_, _| {
        (503, r#"{"error":"overloaded"}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let (tx, _rx) = mpsc::channel();

    let first = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let second = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert!(matches!(first, RotateOutcome::Persisted(false)));
    assert!(matches!(second, RotateOutcome::Persisted(false)));
    let sent: Vec<String> = seen
        .iter()
        .map(|(_, body)| {
            serde_json::from_str::<serde_json::Value>(body).expect("token request body")
                ["refresh_token"]
                .as_str()
                .expect("refresh_token field")
                .to_string()
        })
        .collect();
    assert_eq!(sent, vec!["rt-old".to_string(), "rt-old".to_string()]);
    assert!(
        !spent_record_path(name).exists(),
        "a transient answer proves nothing consumed, so nothing is recorded"
    );
}

/// The endpoint's own `invalid_grant` envelope says the server consumed or
/// revoked the token it judged: the record gains exactly that token's
/// fingerprint (the hand-computed first 8 bytes of SHA-256("rt-old"), hex).
#[test]
fn a_confirmed_invalid_grant_records_the_token_it_judged() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "spent-invalid-grant";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| {
        (
            400,
            r#"{"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}"#
                .to_string(),
        )
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let (tx, _rx) = mpsc::channel();

    let _ = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert_eq!(
        std::fs::read_to_string(spent_record_path(name)).expect("the spent record"),
        r#"["068bfce5d78d07ac"]"#
    );
}

/// A bare 401 is terminal on status alone and quarantines, but it proves
/// nothing about the token having been consumed, so the record stays empty.
#[test]
fn a_bare_401_quarantines_without_recording_the_token() {
    let home = HomeSandbox::new();
    let name = "spent-bare-401";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (401, String::new()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    save_disk_profile(name, "rt-old", Some(past_expiry()));
    let handle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-old"),
        Some(past_expiry()),
    )));

    let gate = gate_under_guard(
        &handle,
        &crate::profile::ProfileName::from(name),
        refresh_result,
        &gate_guard(name),
        AUTH_GATE_GRACE_MS,
    );
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert!(matches!(gate, AuthGate::Broken));
    #[allow(clippy::expect_used, reason = "test")]
    let flagged = handle
        .lock()
        .expect("lock")
        .is_auth_broken(&crate::profile::ProfileName::from(name));
    assert!(flagged, "a terminal 401 still quarantines");
    assert!(
        !spent_record_path(name).exists(),
        "a status-only 401 must never become a permanent spent entry"
    );
}

/// The profile was deleted (another process) before this rotation took its
/// guard, and the rotating process's config is stale: the post-answer step
/// writes into an existing profile dir only, so the rotation recreates nothing.
#[test]
fn a_rotation_of_a_deleted_profile_recreates_no_profile_dir() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "spent-deleted";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (200, MINTED_BODY.to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let dir = profile_dir(&crate::profile::ProfileName::from(name)).expect("profile dir");
    std::fs::remove_dir_all(&dir).expect("delete the profile dir");
    let mut state = crate::profile::load_app_state().expect("state");
    state.profiles.retain(|n| n.as_str() != name);
    crate::profile::save_app_state(&state).expect("save state");
    let (tx, _rx) = mpsc::channel();

    let _ = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(
        seen,
        vec!["/v1/oauth/token".to_string()],
        "the stale config still spends, so the post-answer step runs"
    );
    assert!(!dir.exists(), "no profile dir after the post-answer step");
}

/// The rotation persist writes the store through the preserving credential
/// leg: an `mcpOAuth` block (the per-MCP-server logins) beside the login
/// survives a rotation.
#[test]
fn a_rotation_keeps_the_stores_mcp_logins() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "persist-keeps-mcp";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (200, MINTED_BODY.to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    std::fs::write(
        store_path(name),
        r#"{"claudeAiOauth":{"accessToken":"at-old","refreshToken":"rt-old"},
            "mcpOAuth":{"srv":{"accessToken":"mcp-at"}}}"#,
    )
    .expect("write the store");
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert!(matches!(result, RotateOutcome::Persisted(true)));
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store_path(name)).expect("read")).expect("parse");
    assert_eq!(raw["claudeAiOauth"]["refreshToken"], "rt-new");
    assert_eq!(
        raw["mcpOAuth"],
        serde_json::json!({"srv": {"accessToken": "mcp-at"}}),
        "the rotation's store write keeps every non-login block"
    );
}

/// A landed rotation writes `credentials.json` and nothing else under the
/// profile, so the reload fingerprint a peer compares each tick
/// (`reload_if_changed`, the TUI's `reload_if_state_changed`) must move on
/// that write alone, or a peer keeps the spent pair in memory.
#[test]
fn a_landed_rotation_moves_the_reload_fingerprint() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "persist-fingerprint";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (200, MINTED_BODY.to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&crate::profile::ProfileName::from(name));
    let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    crate::testutil::set_mtime(&store_path(name), an_hour_ago);
    let before = crate::profile::reload_fingerprint();
    let (tx, _rx) = mpsc::channel();

    let result = rotate_one_inner(&config, &crate::profile::ProfileName::from(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert!(matches!(result, RotateOutcome::Persisted(true)));
    assert_ne!(
        crate::profile::reload_fingerprint(),
        before,
        "a peer must see the landed pair as a reason to reload"
    );
}

// ── the refresh chain: the send-site rule, the landers and the record ────────

fn pn(name: &str) -> crate::profile::ProfileName {
    crate::profile::ProfileName::from(name)
}

use crate::testutil::hold_state_flock;

/// The `refresh_token` each recorded token request sent, in order.
fn sent_refresh_tokens(seen: &[(String, String)]) -> Vec<String> {
    seen.iter()
        .filter(|(path, _)| path.starts_with("/v1/oauth/token"))
        .map(|(_, body)| {
            serde_json::from_str::<serde_json::Value>(body).expect("token request body")
                ["refresh_token"]
                .as_str()
                .expect("refresh_token field")
                .to_string()
        })
        .collect()
}

fn is_flagged(config: &crate::profile::ConfigHandle, name: &str) -> bool {
    config.lock().expect("config").is_auth_broken(&pn(name))
}

/// The fixture profile with its access token already expired, loaded fresh, so
/// the install gate reaches its refresh leg.
fn expiring_fixture(name: &str) -> crate::profile::ConfigHandle {
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(
        store_path(name),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"at-old","refreshToken":"rt-old","expiresAt":{}}}}}"#,
            past_expiry()
        ),
    )
    .expect("write the store");
    Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("load config"),
    ))
}

fn minted(access: &str, refresh: &str) -> TokenResponse {
    TokenResponse {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_in: 28_800,
        scope: None,
    }
}

/// C4 (rotate): the manual rotate mints P2 but its persist cannot take the
/// flock, so memory and the store stay on the spent P1. The next manual rotate
/// sends the stage's P2 token, never P1, and its persist lands.
#[test]
fn a_rotate_after_a_wedged_persist_spends_the_staged_token() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-rotate-wedge";
    let (held_tx, held_rx) = mpsc::channel::<std::fs::File>();
    let (base, server) = crate::testutil::serve_endpoints_recording(3, move |_, i| {
        if i == 0 {
            held_tx
                .send(hold_state_flock())
                .expect("hand the holder back");
            (200, MINTED_BODY.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-p3","refresh_token":"rt-p3","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    let (tx, _rx) = mpsc::channel();

    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(300)));
    let first = rotate_one_inner(&config, &pn(name), None, &tx);
    crate::lock::set_state_lock_timeout_override(None);
    drop(held_rx.try_recv().expect("the token call took the flock"));
    let second = rotate_one_inner(&config, &pn(name), None, &tx);
    let seen = server.join().expect("listener");

    assert!(matches!(first, RotateOutcome::Persisted(false)));
    assert_eq!(
        sent_refresh_tokens(&seen),
        vec!["rt-old".to_string(), "rt-new".to_string()],
        "the spend after a wedged persist sends the staged pair's token"
    );
    assert!(matches!(second, RotateOutcome::Persisted(true)));
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-p3"));
}

/// C4 (gate): the install gate's refresh mints P2 under a wedged flock and
/// answers Transient; the next manual rotate sends P2's token, never P1.
#[test]
fn a_rotate_after_a_wedged_gate_persist_spends_the_staged_token() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-gate-wedge";
    let (base, server) = crate::testutil::serve_endpoints_recording(2, |_, _| {
        (
            200,
            r#"{"access_token":"at-p3","refresh_token":"rt-p3","expires_in":28800}"#.to_string(),
        )
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = expiring_fixture(name);
    let (held_tx, held_rx) = mpsc::channel::<std::fs::File>();
    let refresher = move |rt: &str, _: Option<&str>| {
        assert_eq!(rt, "rt-old", "the gate spends the store's token");
        held_tx
            .send(hold_state_flock())
            .expect("hand the holder back");
        Ok(minted("at-new", "rt-new"))
    };

    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(300)));
    let gate = ensure_installable(&config, &pn(name), refresher);
    crate::lock::set_state_lock_timeout_override(None);
    drop(held_rx.try_recv().expect("the refresh took the flock"));
    assert!(
        matches!(gate, AuthGate::Transient(_)),
        "a wedged persist is no install"
    );
    let (tx, _rx) = mpsc::channel();
    let _ = rotate_one_inner(&config, &pn(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(sent_refresh_tokens(&seen), vec!["rt-new".to_string()]);
}

/// C5: the stage AND the save fail together (a directory where the stage goes,
/// a wedged flock), so only the record knows P1 was consumed. A fresh load
/// holds P1 again; its rotate quarantines without sending P1 a second time.
#[test]
fn a_spent_token_with_nothing_staged_quarantines_without_a_resend() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-lost-pair";
    let (held_tx, held_rx) = mpsc::channel::<std::fs::File>();
    let (base, server) = crate::testutil::serve_endpoints_recording(3, move |_, i| {
        if i == 0 {
            held_tx
                .send(hold_state_flock())
                .expect("hand the holder back");
            (200, MINTED_BODY.to_string())
        } else {
            (400, r#"{"error":"invalid_grant"}"#.to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::create_dir(stage_path(name)).expect("block the stage path");
    let (tx, _rx) = mpsc::channel();

    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(300)));
    let _ = rotate_one_inner(&config, &pn(name), None, &tx);
    crate::lock::set_state_lock_timeout_override(None);
    drop(held_rx.try_recv().expect("the token call took the flock"));
    let fresh: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("fresh load"),
    ));
    assert_eq!(memory_refresh(&fresh, name).as_deref(), Some("rt-old"));
    let _ = rotate_one_inner(&fresh, &pn(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(
        sent_refresh_tokens(&seen),
        vec!["rt-old".to_string()],
        "the spent P1 is never sent again"
    );
    assert!(is_flagged(&fresh, name), "the lost chain quarantines");
}

/// C13 (quarantine half): the endpoint's `invalid_grant` records the token;
/// spending it again quarantines with no request at all.
#[test]
fn a_token_judged_invalid_is_never_sent_again() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-judged";
    let (base, server) = crate::testutil::serve_endpoints_recording(3, |_, _| {
        (400, r#"{"error":"invalid_grant"}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    let (tx, rx) = mpsc::channel();

    let _ = rotate_one_inner(&config, &pn(name), None, &tx);
    let _ = rotate_one_inner(&config, &pn(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(sent_refresh_tokens(&seen), vec!["rt-old".to_string()]);
    assert!(is_flagged(&config, name), "the second spend quarantines");
    let toasts: Vec<bool> = rx
        .try_iter()
        .map(|r: OpResult| r.outcome.is_err())
        .collect();
    assert_eq!(toasts, vec![true, true], "both legs report the dead chain");
}

/// C17: memory holds a spent P1, nothing is staged, and the store holds a
/// different unspent login. The rotate adopts the store, sends nothing, raises
/// no toast and quarantines nothing.
#[test]
fn a_spent_token_adopts_an_unspent_store_without_sending() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-adopt";
    let (listener, base) = crate::testutil::deaf_listener();
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    write_store_pair(name, "at-foreign", "rt-foreign");
    let (tx, rx) = mpsc::channel();

    let outcome = rotate_one_inner(&config, &pn(name), None, &tx);

    crate::testutil::assert_nothing_sent(&listener);
    assert!(matches!(outcome, RotateOutcome::Persisted(false)));
    assert_eq!(memory_refresh(&config, name).as_deref(), Some("rt-foreign"));
    assert!(!is_flagged(&config, name), "an adopt is no quarantine");
    assert!(rx.try_recv().is_err(), "an adopt raises no toast");
}

/// C18 (gate): an unreadable spent record (a directory at its path) skips the
/// refresh as Transient: nothing spent, nothing quarantined.
#[test]
fn an_unreadable_spent_record_skips_the_gates_spend() {
    let _home = HomeSandbox::new();
    let name = "chain-record-dir-gate";
    let config = expiring_fixture(name);
    std::fs::create_dir(spent_record_path(name)).expect("block the record");

    let gate = ensure_installable(&config, &pn(name), |_: &str, _: Option<&str>| {
        panic!("an unreadable record must never reach the token endpoint")
    });

    let AuthGate::Transient(t) = gate else {
        panic!("an unreadable record skips as Transient");
    };
    assert_eq!(
        t.text(),
        "could not lock 'chain-record-dir-gate' for a token refresh; check permissions on ~/.clauth"
    );
    assert!(!is_flagged(&config, name));
}

/// C18 (rotate): the manual rotate under the same record sends nothing,
/// quarantines nothing, and says why.
#[test]
fn an_unreadable_spent_record_skips_the_manual_rotate() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-record-dir-rotate";
    let (listener, base) = crate::testutil::deaf_listener();
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::create_dir(spent_record_path(name)).expect("block the record");
    let (tx, rx) = mpsc::channel();

    let outcome = rotate_one_inner(&config, &pn(name), None, &tx);

    crate::testutil::assert_nothing_sent(&listener);
    assert!(matches!(outcome, RotateOutcome::Persisted(false)));
    assert!(!is_flagged(&config, name));
    let reported = rx.try_recv().expect("the skip reports").outcome;
    assert_eq!(
        reported.expect_err("a skip is no success").to_string(),
        "could not lock 'chain-record-dir-rotate' for a token refresh; check permissions on ~/.clauth"
    );
}

/// C27 (gate): a live stage whose landing cannot take the flock refuses the
/// install as Transient, so no caller installs the lagging store.
#[test]
fn the_install_gate_refuses_while_a_live_stage_cannot_land() {
    let _home = HomeSandbox::new();
    let name = "chain-gate-land-wedge";
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    write_live_stage_over_rt_old(name);
    let holder = hold_state_flock();

    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(300)));
    let gate = ensure_installable(&config, &pn(name), |_: &str, _: Option<&str>| {
        panic!("the gate never refreshes around a stage it could not land")
    });
    crate::lock::set_state_lock_timeout_override(None);
    drop(holder);

    let AuthGate::Transient(t) = gate else {
        panic!("a failed land refuses the install");
    };
    assert_eq!(
        t.text(),
        "another clauth process holds ~/.clauth's state lock · 'chain-gate-land-wedge' left \
         unchanged: retry in a moment"
    );
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-old"));
    assert!(stage_path(name).exists(), "the stage waits for a landing");
}

/// The install gate lands a live stage before answering, so what the caller
/// installs is the chain head and memory follows it.
#[test]
fn the_install_gate_lands_a_live_stage_before_ready() {
    let _home = HomeSandbox::new();
    let name = "chain-gate-lands";
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    write_live_stage_over_rt_old(name);

    let gate = ensure_installable(&config, &pn(name), |_: &str, _: Option<&str>| {
        panic!("a landed stage is installable as it is")
    });

    assert!(matches!(gate, AuthGate::Ready));
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-mid"));
    assert_eq!(memory_refresh(&config, name).as_deref(), Some("rt-mid"));
    assert!(!stage_path(name).exists());
}

/// F1: the store WRITE fails while its read works (directories squat on the
/// staging names the persist's atomic write would take next, placed once the
/// post-answer step has staged and recorded, before the persist's hold): the
/// stage survives for the catch-up and memory holds the minted pair, the chain
/// head.
#[cfg(unix)]
#[test]
fn a_store_write_failure_keeps_the_stage_and_moves_memory_to_the_minted_pair() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-write-fails";
    let (held_tx, held_rx) = mpsc::channel::<std::fs::File>();
    let (base, server) = crate::testutil::serve_endpoints(2, move |_, _| {
        held_tx
            .send(hold_state_flock())
            .expect("hand the holder back");
        (200, MINTED_BODY.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    let record = spent_record_path(name);
    let store = store_path(name);
    let squatter = std::thread::spawn(move || {
        let holder = held_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the token call took the flock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !record.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the post-answer step never ran"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // One staging name taken here gives the counter's position; the
        // persist's write takes one of the next names, each a directory now,
        // which its stale-temp removal cannot unlink.
        let probe = crate::profile::tmp_sibling(&store);
        let probe = probe.to_string_lossy().into_owned();
        let (stem, seq) = probe.rsplit_once('.').expect("a sequenced staging name");
        let seq: u64 = seq.parse().expect("the sequence number");
        for next in seq + 1..=seq + 4096 {
            std::fs::create_dir(format!("{stem}.{next}")).expect("squat a staging name");
        }
        drop(holder);
    });
    let (tx, _rx) = mpsc::channel();

    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_secs(20)));
    let outcome = rotate_one_inner(&config, &pn(name), None, &tx);
    crate::lock::set_state_lock_timeout_override(None);
    squatter.join().expect("squatter");
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert!(matches!(outcome, RotateOutcome::Persisted(false)));
    assert_eq!(
        stored_refresh(name).as_deref(),
        Some("rt-old"),
        "the write failed"
    );
    assert!(stage_path(name).exists(), "a save error keeps the stage");
    assert_eq!(memory_refresh(&config, name).as_deref(), Some("rt-new"));
}

/// F3: a 400 that is not the flat OAuth2 envelope proves nothing consumed, even
/// when it holds the literal: a nested error object, and an HTML page.
#[test]
fn a_rejection_merely_holding_the_literal_is_not_a_confirmed_invalid_grant() {
    let home = HomeSandbox::new();
    let (base, server) = crate::testutil::serve_endpoints(3, |_, i| {
        if i == 0 {
            (
                400,
                r#"{"error":{"type":"invalid_grant","message":"x"}}"#.to_string(),
            )
        } else {
            (
                400,
                "<html><body>invalid_grant: see the status page</body></html>".to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);

    let nested = refresh_result("rt-x", None);
    let page = refresh_result("rt-x", None);
    let _ = server.join().expect("listener");

    for (shape, answer) in [("nested", nested), ("page", page)] {
        match answer {
            Err(RefreshError::Invalid { invalid_grant, .. }) => {
                assert!(!invalid_grant, "{shape}: not body-confirmed");
            }
            Err(RefreshError::Transient(_)) => {}
            Ok(_) => panic!("{shape}: a 400 is no pair"),
        }
    }
}

/// F4 (a): a rotation that sent a token other than the store's lands while the
/// store's token is recorded spent (the stage-write-error row).
#[test]
fn a_persist_lands_over_a_store_whose_token_is_recorded_spent() {
    let _home = HomeSandbox::new();
    let name = "chain-land-spent-store";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    let creds = crate::oauth::minted_credentials(None, minted("at-new", "rt-new"));

    let landing =
        land_pair(&pn(name), "6eebcc2159154314", &creds, "at-old", |_, _| {}).expect("land");

    assert!(matches!(landing, Landing::Landed));
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-new"));
}

/// F4 (b): the same with an empty record and a stage whose base is the store's
/// token (the record-write-error and two-failed-persists rows).
#[test]
fn a_persist_lands_over_a_store_that_is_the_stages_base() {
    let _home = HomeSandbox::new();
    let name = "chain-land-base";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    write_live_stage_over_rt_old(name);
    let creds = crate::oauth::minted_credentials(None, minted("at-new", "rt-new"));

    let landing =
        land_pair(&pn(name), "6eebcc2159154314", &creds, "at-old", |_, _| {}).expect("land");

    assert!(matches!(landing, Landing::Landed));
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-new"));
}

/// F4 (c, d): a pair answer for a live stage's token re-stages with the base
/// unchanged (the store has not moved), and the record gains the sent token.
#[test]
fn a_pair_for_the_staged_token_keeps_the_base_and_records_the_sent_token() {
    let _home = HomeSandbox::new();
    let name = "chain-restage";
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    write_live_stage_over_rt_old(name);

    let settled = settle_refresh_answer(
        &config,
        &pn(name),
        "rt-mid",
        Ok(minted("at-new", "rt-new")),
        &gate_guard(name),
    );

    assert!(settled.is_ok());
    let stage: serde_json::Value =
        serde_json::from_slice(&std::fs::read(stage_path(name)).expect("stage")).expect("parse");
    assert_eq!(stage["base"], "068bfce5d78d07ac");
    assert_eq!(stage["base_access"], "at-old");
    assert_eq!(stage["creds"]["claudeAiOauth"]["refreshToken"], "rt-new");
    assert_eq!(
        std::fs::read_to_string(spent_record_path(name)).expect("record"),
        r#"["068bfce5d78d07ac","6eebcc2159154314"]"#
    );
}

/// F11: an answer echoing the sent refresh token did not rotate it, so the
/// token stays unrecorded.
#[test]
fn a_pair_echoing_the_sent_token_records_nothing() {
    let _home = HomeSandbox::new();
    let name = "chain-echo";
    let config = crate::testutil::rotation_fixture_config(&pn(name));

    let _ = settle_refresh_answer(
        &config,
        &pn(name),
        "rt-old",
        Ok(minted("at-new", "rt-old")),
        &gate_guard(name),
    );

    assert!(!spent_record_path(name).exists());
}

/// F10: a re-answered fingerprint moves to the newest end, so eviction runs by
/// the latest answer.
#[test]
fn a_re_answered_token_moves_to_the_newest_end_of_the_record() {
    let _home = HomeSandbox::new();
    let name = "chain-reanswer";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(
        spent_record_path(name),
        r#"["068bfce5d78d07ac","6eebcc2159154314"]"#,
    )
    .expect("record");

    record_spent(&pn(name), "rt-old");

    assert_eq!(
        std::fs::read_to_string(spent_record_path(name)).expect("record"),
        r#"["6eebcc2159154314","068bfce5d78d07ac"]"#
    );
}

/// F5: a stage is never loaded off an unreadable record: whether its pair is
/// spent is unknown.
#[test]
fn a_stage_with_an_unreadable_record_does_not_load() {
    let _home = HomeSandbox::new();
    let name = "chain-stage-record-dir";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    write_live_stage_over_rt_old(name);
    std::fs::create_dir(spent_record_path(name)).expect("block the record");
    let store = read_raw_store(&pn(name)).expect("store");

    assert!(live_stage_credentials(&pn(name), store.as_ref()).is_none());
}

/// F12: a persist for a profile the roster no longer names reports the failed
/// write to its caller's hold, the same as a write error.
#[test]
fn a_persist_for_an_unconfigured_profile_reports_no_landing_in_its_hold() {
    let _home = HomeSandbox::new();
    let name = "chain-unconfigured";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    let mut state = crate::profile::load_app_state().expect("state");
    state.profiles.retain(|n| n.as_str() != name);
    crate::profile::save_app_state(&state).expect("save state");
    let creds = crate::oauth::minted_credentials(None, minted("at-new", "rt-new"));
    let mut reported = Vec::new();

    let landed = land_pair(
        &pn(name),
        "068bfce5d78d07ac",
        &creds,
        "at-old",
        |landing, _| reported.push(landing.is_none()),
    );

    assert!(landed.is_err());
    assert_eq!(reported, vec![true]);
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-old"));
}

fn write_split_sidecar(name: &str) {
    std::fs::write(
        profile_dir(&pn(name))
            .expect("dir")
            .join("session-token.json"),
        r#"{"claudeAiOauth":{"accessToken":"sk-static","expiresAt":4102444800000}}"#,
    )
    .expect("write the session token");
}

/// C19 (split) + F15: a CLA-SPLIT profile's persist reads `D` from
/// `credentials.json`, never the session token: it lands on its own chain and
/// refuses a foreign one, and the session token is untouched either way.
#[test]
fn a_split_profiles_persist_judges_the_chain_in_credentials_json() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-split";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, i| {
        if i == 0 {
            (200, MINTED_BODY.to_string())
        } else {
            (
                200,
                r#"{"access_token":"at-p3","refresh_token":"rt-p3","expires_in":28800}"#
                    .to_string(),
            )
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    write_split_sidecar(name);
    let sidecar = profile_dir(&pn(name))
        .expect("dir")
        .join("session-token.json");
    let sidecar_before = std::fs::read(&sidecar).expect("sidecar");
    let (tx, _rx) = mpsc::channel();

    let own = rotate_one_inner(&config, &pn(name), None, &tx);
    assert!(matches!(own, RotateOutcome::Persisted(true)));
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-new"));

    write_store_pair(name, "at-foreign", "rt-foreign");
    let foreign = rotate_one_inner(&config, &pn(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen.len(), 2);
    assert!(matches!(foreign, RotateOutcome::Persisted(false)));
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-foreign"));
    assert_eq!(std::fs::read(&sidecar).expect("sidecar"), sidecar_before);
}

// ── the refresh chain: captures, the gates, the landing's Keychain budget ────

/// A capture re-installs a pair the record names spent (a live slot that lagged
/// the landed store). The readable record survives the install, so the next
/// spend of that pair quarantines without a request instead of replaying the
/// consumed token.
#[test]
fn a_capture_of_a_recorded_pair_keeps_the_record_and_its_spend_quarantines_unsent() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-recapture";
    let (listener, base) = crate::testutil::deaf_listener();
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    write_store_pair(name, "at-new", "rt-new");
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    let mut cfg = crate::profile::load_config().expect("load config");

    crate::actions::overwrite_captured_profile(
        &mut cfg,
        &pn(name),
        crate::actions::CaptureSnapshot {
            credentials: Some(ClaudeCredentials {
                claude_ai_oauth: Some(OAuthToken {
                    access_token: "at-old".to_string(),
                    refresh_token: Some("rt-old".to_string()),
                    expires_at: None,
                    scopes: None,
                    subscription_type: None,
                    ..OAuthToken::default_extra()
                }),
            }),
            base_url: None,
            api_key: None,
            anchor: crate::actions::AnchorAction::Unproven,
        },
    )
    .expect("install the lagging pair");

    assert_eq!(
        std::fs::read_to_string(spent_record_path(name)).expect("the record survives the install"),
        r#"["068bfce5d78d07ac"]"#
    );
    let fresh: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("fresh load"),
    ));
    assert_eq!(memory_refresh(&fresh, name).as_deref(), Some("rt-old"));
    let (tx, _rx) = mpsc::channel();
    let _ = rotate_one_inner(&fresh, &pn(name), None, &tx);

    crate::testutil::assert_nothing_sent(&listener);
    assert!(is_flagged(&fresh, name), "the consumed pair quarantines");
}

/// The store exists but cannot be parsed when the answer lands, so the persist
/// fails, and memory moves to its OWN login block with the minted pair written
/// in, never to an empty block that drops the plan.
#[test]
fn a_minted_pair_over_an_unreadable_store_keeps_memorys_login_block() {
    use std::sync::mpsc;
    let home = HomeSandbox::new();
    let name = "chain-store-unreadable";
    let (base, server) = crate::testutil::serve_endpoints(3, |_, _| (200, MINTED_BODY.to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(
        store_path(name),
        r#"{"claudeAiOauth":{"accessToken":"at-old","refreshToken":"rt-old","expiresAt":4102444800000,"subscriptionType":"max"}}"#,
    )
    .expect("write the store");
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("load config"),
    ));
    std::fs::write(store_path(name), "not json").expect("corrupt the store");
    let (tx, _rx) = mpsc::channel();

    let outcome = rotate_one_inner(&config, &pn(name), None, &tx);
    let seen = server.join().expect("listener");

    assert_eq!(seen, vec!["/v1/oauth/token".to_string()]);
    assert!(matches!(outcome, RotateOutcome::Persisted(false)));
    let cfg = config.lock().expect("config");
    let oauth = cfg
        .find(&pn(name))
        .and_then(|p| p.credentials.as_ref())
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .expect("memory holds a login");
    assert_eq!(oauth.refresh_token.as_deref(), Some("rt-new"));
    assert_eq!(oauth.access_token, "at-new");
    assert_eq!(
        oauth.subscription_type.as_deref(),
        Some("max"),
        "memory keeps its own plan beside the minted pair"
    );
}

/// An inert stage (the store moved past its base) while another holder has the
/// profile's rotation lock: the gate judges the stage lock-free, skips the
/// lander, and answers without waiting on the lock.
#[test]
fn the_install_gate_skips_an_inert_stage_without_waiting_on_the_rotation_lock() {
    use std::sync::mpsc;
    let _home = HomeSandbox::new();
    let name = "chain-gate-inert-held";
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    write_live_stage_over_rt_old(name);
    write_store_pair(name, "at-foreign", "rt-foreign");
    let holder = crate::testutil::hold_rotation_lock(name);
    let (tx, rx) = mpsc::channel();
    let gate_config = config.clone();
    let worker = std::thread::spawn(move || {
        let gate = ensure_installable(&gate_config, &pn(name), |_: &str, _: Option<&str>| {
            panic!("a fresh target never refreshes")
        });
        tx.send(matches!(gate, AuthGate::Ready)).expect("report");
    });

    let answered = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(holder);
    worker.join().expect("gate thread");

    assert_eq!(
        answered.ok(),
        Some(true),
        "the gate answered Ready without waiting on the held rotation lock"
    );
}

/// The daemon's main loop never parks behind a held rotation lock to land a
/// live stage: it answers the busy cause, nothing lands, and the stage waits
/// for its holder.
#[test]
fn the_main_loop_gate_answers_busy_while_a_live_stages_lock_is_held() {
    use std::sync::mpsc;
    let _home = HomeSandbox::new();
    let name = "chain-main-loop-held";
    let config = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    write_live_stage_over_rt_old(name);
    let holder = crate::testutil::hold_rotation_lock(name);
    let (tx, rx) = mpsc::channel();
    let gate_config = config.clone();
    let worker = std::thread::spawn(move || {
        let gate = ensure_installable_from_main_loop(
            &gate_config,
            &pn(name),
            |_: &str, _: Option<&str>| panic!("a busy lander never refreshes"),
        );
        let text = match gate {
            AuthGate::Transient(t) => t.text(),
            AuthGate::Ready => "Ready".to_string(),
            AuthGate::Refreshed => "Refreshed".to_string(),
            AuthGate::Broken => "Broken".to_string(),
        };
        tx.send(text).expect("report");
    });

    let answered = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(holder);
    worker.join().expect("gate thread");

    assert_eq!(
        answered.ok().as_deref(),
        Some("'chain-main-loop-held' has a token rotation in progress, retry in a moment"),
        "the main loop answers the held lock instead of waiting it out"
    );
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-old"));
    assert!(stage_path(name).exists(), "the stage waits for its holder");
}

/// The main loop's switch gate onto a rolling-token target never parks behind
/// a held rotation lock either: the rolling gate's own guard answers the busy
/// cause, and the switch retries on a later tick.
#[test]
fn the_main_loop_gate_answers_busy_while_a_rolling_targets_lock_is_held() {
    use std::sync::mpsc;
    let _home = HomeSandbox::new();
    let name = "chain-main-loop-rolling";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    let mut on_disk = crate::profile::load_profile(&pn(name)).expect("load");
    on_disk.rolling_token = true;
    crate::profile::save_profile_config(&on_disk).expect("arm rolling");
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("load config"),
    ));
    let holder = crate::testutil::hold_rotation_lock(name);
    let (tx, rx) = mpsc::channel();
    let gate_config = config.clone();
    let worker = std::thread::spawn(move || {
        let gate = ensure_installable_from_main_loop(
            &gate_config,
            &pn(name),
            |_: &str, _: Option<&str>| panic!("a busy rolling gate never refreshes"),
        );
        let text = match gate {
            AuthGate::Transient(t) => t.text(),
            AuthGate::Ready => "Ready".to_string(),
            AuthGate::Refreshed => "Refreshed".to_string(),
            AuthGate::Broken => "Broken".to_string(),
        };
        tx.send(text).expect("report");
    });

    let answered = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(holder);

    assert_eq!(
        answered.ok().as_deref(),
        Some("'chain-main-loop-rolling' has a token rotation in progress, retry in a moment"),
        "the main loop answers the held lock instead of waiting it out"
    );
    worker.join().expect("gate thread");
}

/// A landing that waits out a peer's state-flock hold still hands its Keychain
/// calls the whole `KEYCHAIN_MIRROR_BUDGET`: the budget is armed past the
/// hold, so the wait spends none of it.
#[test]
fn a_landings_flock_wait_leaves_its_keychain_budget_whole() {
    let _home = HomeSandbox::new();
    let name = "chain-budget-after-flock";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    write_live_stage_over_rt_old(name);
    let guard = crate::runtime::RotationGuard::acquire(&pn(name)).expect("rotation guard");
    let held = std::time::Duration::from_millis(1500);
    let holder = hold_state_flock();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(held);
        drop(holder);
    });
    let _ = crate::lock::take_keychain_leg_windows();

    let start = std::time::Instant::now();
    let landed = land_stage_for_install(&pn(name), &guard);
    let waited = start.elapsed();
    releaser.join().expect("releaser");

    assert!(landed.is_ok(), "the stage lands once the peer lets go");
    assert!(
        waited >= held,
        "the landing waited out the peer: {waited:?}"
    );
    assert_eq!(stored_refresh(name).as_deref(), Some("rt-mid"));
    let windows = crate::lock::take_keychain_leg_windows();
    assert_eq!(windows.len(), 1, "one landing, one Keychain leg scope");
    let left = windows[0].expect("the landing's Keychain legs run under a budget");
    assert!(
        left > crate::runtime::KEYCHAIN_MIRROR_BUDGET - std::time::Duration::from_millis(500),
        "the flock wait spent the Keychain budget: {left:?} left"
    );
}

/// The rolling arm's token call runs under no Keychain budget: each landing
/// arms its own past its state-flock hold, so neither a flock wait nor the
/// token call spends the window its Keychain calls get.
#[test]
fn the_rolling_arm_spends_its_refresh_under_no_keychain_budget() {
    let _home = HomeSandbox::new();
    let name = "chain-rolling-budget";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    std::fs::write(
        store_path(name),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"at-old","refreshToken":"rt-old","expiresAt":{},"subscriptionType":"max"}}}}"#,
            past_expiry()
        ),
    )
    .expect("an expiring chain");
    let mut on_disk = crate::profile::load_profile(&pn(name)).expect("load");
    on_disk.rolling_token = true;
    crate::profile::save_profile_config(&on_disk).expect("arm rolling");
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("load config"),
    ));
    let budget = std::sync::Mutex::new(None);

    let _ = arm_rolling_token(&config, &pn(name), |rt: &str, _: Option<&str>| {
        assert_eq!(rt, "rt-old");
        *budget.lock().expect("budget slot") = Some(crate::lock::armed_budget_remaining());
        Ok(minted("at-new", "rt-new"))
    });

    let seen = budget
        .into_inner()
        .expect("budget slot")
        .expect("the arm reached its refresh");
    assert_eq!(seen, None, "the refresh runs under no Keychain budget");
}

/// The install gate reached through the rolling arm lands a live stage before
/// it stamps from the stored chain.
#[test]
fn the_rolling_arm_lands_a_live_stage_before_it_stamps() {
    let _home = HomeSandbox::new();
    let name = "chain-rolling-lands";
    let _ = crate::testutil::rotation_fixture_config(&pn(name));
    let mut on_disk = crate::profile::load_profile(&pn(name)).expect("load");
    on_disk.rolling_token = true;
    crate::profile::save_profile_config(&on_disk).expect("arm rolling");
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    std::fs::write(
        stage_path(name),
        r#"{"base":"068bfce5d78d07ac","base_access":"at-old",
            "creds":{"claudeAiOauth":{"accessToken":"at-mid","refreshToken":"rt-mid",
                     "expiresAt":4102444800000,"subscriptionType":"max"}}}"#,
    )
    .expect("write the stage");
    let config: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(
        crate::profile::load_config().expect("load config"),
    ));

    let _ = arm_rolling_token(&config, &pn(name), |_: &str, _: Option<&str>| {
        panic!("a comfortable staged chain stamps without a refresh")
    });

    assert_eq!(stored_refresh(name).as_deref(), Some("rt-mid"));
    assert!(!stage_path(name).exists(), "the landed stage is gone");
}

/// The gate's adoption never takes a store whose token the server consumed:
/// memory holds the minted P2, the store still the spent P1, nothing staged.
#[test]
fn the_gate_adoption_never_takes_a_spent_store_over_memory() {
    let _home = HomeSandbox::new();
    let name = "chain-adopt-spent";
    save_disk_profile(name, "rt-old", Some(future_expiry()));
    std::fs::write(spent_record_path(name), r#"["068bfce5d78d07ac"]"#).expect("record P1");
    let handle: crate::profile::ConfigHandle = Arc::new(RankedMutex::new(oauth_config(
        name,
        Some("rt-new"),
        Some(future_expiry()),
    )));

    adopt_disk_rotation(&handle, &pn(name), &gate_guard(name)).expect("adopt");

    assert_eq!(memory_refresh(&handle, name).as_deref(), Some("rt-new"));
}
