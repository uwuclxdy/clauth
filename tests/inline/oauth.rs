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
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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
fn live_session_excluded_when_force_false() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-guard";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let config = single_profile_config(name, "rt-ghi");
    let candidates = rotation_candidates(&config, false);
    assert!(
        candidates.is_empty(),
        "force=false should exclude a profile with a live session"
    );

    drop(file);
}

#[test]
fn live_session_excluded_even_with_force_true() {
    let _home = HomeSandbox::new();
    let name = "test-oauth-live-session-force";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let config = single_profile_config(name, "rt-jkl");
    let candidates = rotation_candidates(&config, true);
    assert!(
        candidates.is_empty(),
        "a live session owns its single-use chain — never rotated, even under force"
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
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: "test-rotate-one-no-rt".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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

    let result = rotate_one_inner(&config, "test-rotate-one-no-rt", Some(&activity), &tx);

    assert!(
        matches!(result, RotateOutcome::Persisted(false)),
        "rotate_one_inner should return Persisted(false) when no refresh token"
    );
    assert!(
        is_idle(&activity, "test-rotate-one-no-rt"),
        "activity slot must remain Idle when rotation short-circuits at no-token"
    );
}

/// `rotate_one_inner` must never rotate a profile with a live `clauth start`
/// session: its single-use chain is owned by that session, so our stored token
/// is stale and a refresh would 400. It skips silently — `Persisted(false)`,
/// activity Idle, no `OpResult` (the single-rotate caller messages up front).
#[test]
fn rotate_one_inner_skips_live_session() {
    use std::collections::BTreeMap;
    use std::sync::mpsc;

    let _home = HomeSandbox::new();
    let name = "test-rotate-one-live-session";
    let sessions = profile_dir(name).expect("profile_dir").join("sessions");
    std::fs::create_dir_all(&sessions).expect("create sessions dir");
    let pid_file = sessions.join("test-pid");
    let file = open_pid_file(&pid_file).expect("open pid file");
    file.lock().expect("lock pid file");

    let profile = Profile {
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "at".to_string(),
                refresh_token: Some("rt-live".to_string()),
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

    let config = Arc::new(RankedMutex::new(config));
    let activity: ActivityStore = Arc::new(RankedMutex::new(std::collections::HashMap::new()));
    let (tx, rx) = mpsc::channel();

    let result = rotate_one_inner(&config, name, Some(&activity), &tx);

    assert!(
        matches!(result, RotateOutcome::Persisted(false)),
        "a live session must skip rotation (no stale-token refresh)"
    );
    assert!(
        is_idle(&activity, name),
        "a skipped rotation must leave the activity slot Idle"
    );
    assert!(
        rx.try_recv().is_err(),
        "the silent live-session skip must not emit an OpResult"
    );

    drop(file);
}

#[test]
fn profile_without_refresh_token_excluded() {
    use std::collections::BTreeMap;
    let profile = Profile {
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: "test-oauth-no-rt".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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
    let held_a = RotationGuard::acquire(a).expect("acquire a");

    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let held_b = RotationGuard::acquire(b).expect("acquire b while a is held"); // distinct lock file → must not block
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
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
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
    config
}

fn third_party_config(name: &str) -> AppConfig {
    use std::collections::BTreeMap;
    let profile = Profile {
        harness: crate::profile::Harness::Claude,
        session_feed: false,
        name: name.into(),
        base_url: Some("https://api.deepseek.com/anthropic".to_string()),
        api_key: Some("sk-fixture".to_string()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
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
        ensure_installable(&handle, name, never_refresh),
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
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
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
        ensure_installable(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    let p = cfg.find(name).expect("profile");
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
        !cfg.is_auth_broken(name),
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
        Err(RefreshError::Invalid("HTTP 400: invalid_grant".to_string()))
    };
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        cfg.is_auth_broken(name),
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
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Transient(anyhow::anyhow!("connection reset")))
    };
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Transient(_)
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        !cfg.is_auth_broken(name),
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
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(cfg.is_auth_broken(name));
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
    config.set_auth_broken(name, true);
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
        ensure_installable(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        !cfg.is_auth_broken(name),
        "a recovered chain lifts the quarantine on the way through the gate"
    );
    let p = cfg.find(name).unwrap_or_else(|| panic!("profile"));
    assert_eq!(p.access_token(), Some("at-recovered"));
}

/// Same flagged shape with a genuinely dead chain: the gate confirms `Broken`
/// (refusal + login hint), never a silent `Ready` install of the revoked pair.
#[test]
fn gate_flagged_profile_with_a_dead_chain_stays_broken() {
    let _home = HomeSandbox::new();
    let name = "test-gate-flagged-dead";
    let mut config = oauth_config(name, Some("rt-revoked"), Some(future_expiry()));
    config.set_auth_broken(name, true);
    let handle = Arc::new(RankedMutex::new(config));
    let refresher = |_rt: &str, _scopes: Option<&str>| {
        Err(RefreshError::Invalid("HTTP 400: invalid_grant".to_string()))
    };
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Broken
    ));
    #[allow(clippy::expect_used, reason = "test")]
    let cfg = handle.lock().expect("lock");
    assert!(
        cfg.is_auth_broken(name),
        "a dead chain keeps the quarantine"
    );
}

/// A 2xx token-endpoint body that fails to deserialize still holds the live
/// access+refresh tokens, so `token_parse_error` must surface only the serde
/// error + HTTP status + body length — never the token values.
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
    let msg = super::token_parse_error(err, 200, body.len()).to_string();

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
/// Bodies are real bytes captured from the live endpoint (`docs/wire-parity.md`).
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
/// credential file stored the granted scopes (`docs/wire-parity.md`). Reorder to
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
        assert!(!super::live_login_is_foreign("alpha", "old-token"));
    }

    #[test]
    fn live_file_matching_stored_pair_is_not_foreign() {
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("new-token");
        assert!(!super::live_login_is_foreign("alpha", "old-token"));
    }

    #[test]
    fn stale_mirror_of_own_pre_rotation_pair_is_not_foreign() {
        // The case the gate exists FOR: CC's regular-file mirror still holds the
        // pair this rotation just superseded. Diverged by classification, but it
        // is our own chain one step behind — the Keychain mirror must proceed.
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("old-token");
        assert!(!super::live_login_is_foreign("alpha", "old-token"));
    }

    #[test]
    fn unrelated_live_login_is_foreign() {
        // A real CC re-login into some other account: matches neither the new
        // nor the pre-rotation pair — never overwrite it.
        let _home = HomeSandbox::new();
        save_profile_with("alpha", "new-token");
        write_live_file("someone-elses-token");
        assert!(super::live_login_is_foreign("alpha", "old-token"));
    }
}

// ── try_adopt_live_rotation (rotation coherence, the adopt-don't-race half) ──
//
// The running claude and clauth hold ONE single-use refresh family; when CC
// rotates first, its file mirror (~/.claude/.credentials.json) carries the
// fresher pair. Adopting it — identity-guarded — replaces racing for the
// chain. All offline: identity is injected, the "mirror" is a sandboxed file.
// NOT macOS-gated: only the production call site's `keychain_live()` gate is
// platform-specific — the identity gate and the expiry-monotonicity re-check
// must hold (and run in CI) on every OS.
mod adopt_live_rotation {
    use super::*;
    use crate::lockorder::RankedMutex;
    use crate::testutil::HomeSandbox;
    use std::sync::Arc;

    /// The per-profile rotation lock `try_adopt_live_rotation` demands proof
    /// of (production callers hold it across the whole rotation leg).
    fn guard(name: &str) -> crate::runtime::RotationGuard {
        crate::runtime::RotationGuard::acquire(name).expect("rotation guard")
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
    /// mirror pair ("at-mirror").
    fn setup(name: &str, stored_expiry: i64, mirror_expiry: i64) -> crate::profile::ConfigHandle {
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(creds_with("at-old", Some(stored_expiry)));
        crate::profile::save_profile(&p).expect("save profile");
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: vec![p],
        };
        config.state.profiles = vec![name.into()];
        config.state.active_profile = Some(name.into());
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
            .find(name)
            .and_then(|p| p.access_token().map(str::to_string))
            .expect("stored access token")
    }

    #[test]
    fn adopts_a_fresher_same_account_pair() {
        let _home = HomeSandbox::new();
        let name = "adopt-ok";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
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
                name,
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|tok| {
            Some(
                if tok == "at-mirror" {
                    "uuid-2"
                } else {
                    "uuid-1"
                }
                .into(),
            )
        });
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|tok| {
            (tok == "at-mirror").then(|| "uuid-1".into())
        });
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn cached_anchor_allows_adopt_even_with_a_dead_stored_token() {
        let _home = HomeSandbox::new();
        let name = "adopt-cached-anchor";
        let handle = setup(name, past_expiry(), future_expiry());
        crate::profile_cache::write_profile_cache(
            name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &"uuid-1".to_string(),
        );
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|tok| {
            (tok == "at-mirror").then(|| "uuid-1".into())
        });
        assert!(adopted.is_some());
        assert_eq!(stored_access(&handle, name), "at-mirror");
    }

    #[test]
    fn refuses_a_stale_or_equal_mirror() {
        let _home = HomeSandbox::new();
        let name = "adopt-stale";
        // Mirror expiry equals the store's — nothing fresher to adopt.
        let expiry = future_expiry();
        let handle = setup(name, expiry, expiry);
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
        assert_eq!(adopted, None);
        assert_eq!(stored_access(&handle, name), "at-old");
    }

    #[test]
    fn refuses_when_not_the_active_profile() {
        let _home = HomeSandbox::new();
        let name = "adopt-inactive";
        let handle = setup(name, future_expiry(), future_expiry() + 3_600_000);
        handle.lock().unwrap().state.active_profile = None;
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
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
        let adopted = try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("  ".into()));
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
        handle.lock().unwrap().set_auth_broken(name, true);
        let adopted =
            try_adopt_live_rotation(&handle, name, &guard(name), &|_| Some("uuid-1".into()));
        assert!(adopted.is_some(), "the fresher same-account pair adopts");
        assert!(
            !handle.lock().unwrap().is_auth_broken(name),
            "an adopted (alive) chain lifts a stale quarantine"
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
    crate::runtime::RotationGuard::acquire(name).expect("rotation guard")
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
            name,
            never_refresh,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Ready
    ));
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
            name,
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
            name,
            never_refresh,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Ready
    ));
    assert_eq!(
        handle.lock().unwrap().find(name).unwrap().refresh_token(),
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
            name,
            refresher,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Refreshed
    ));
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
    handle.lock().unwrap().set_auth_broken(name, true);
    save_disk_profile(name, "rt-peer", Some(future_expiry()));
    assert!(matches!(
        gate_under_guard(
            &handle,
            name,
            never_refresh,
            &gate_guard(name),
            AUTH_GATE_GRACE_MS
        ),
        AuthGate::Ready
    ));
    assert!(
        !handle.lock().unwrap().is_auth_broken(name),
        "an adopted (alive) chain lifts a stale quarantine"
    );
}

// ── token-endpoint request bodies (platform.claude.com wire parity) ──────────
//
// The exact JSON body CC's axios client posts to platform.claude.com/v1/oauth/
// token, captured 2026-07-14 against CC 2.1.209 (docs/wire-parity.md). Field
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
// (captured 2026-07-14, CC 2.1.209, docs/wire-parity.md). Drives the REAL
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
            "expected a 429 with limiter metadata, got {:?}",
            anyhow::Error::from(err)
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
    config.set_auth_broken(name, true);
    // Materialize the profile dir, then the session token beside it.
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = crate::profile::profile_dir(name).expect("dir");
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
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
}

// ── Fork-only tests (codex engine, RESCUE, CLA-FEED, forecast, email column) ──

/// `mark_auth_broken` persists its ONE delta against the latest disk state.
/// A blind whole-state save from this process's snapshot would clobber a flag
/// a CONCURRENT process minted in between (the daemon marking one profile
/// while a CLI reauth clears another) — the TECH-7 lost-update surface.
#[test]
fn mark_auth_broken_merges_with_disk_instead_of_clobbering() {
    let _home = HomeSandbox::new();

    // Another process already quarantined "other" on disk; this process's
    // in-memory snapshot predates that and doesn't know.
    crate::profile::update_app_state(|s| s.auth_broken.push("other".into()))
        .expect("seed disk state");
    let handle: crate::profile::ConfigHandle =
        std::sync::Arc::new(crate::lockorder::RankedMutex::new(AppConfig {
            state: AppState::default(),
            profiles: vec![],
        }));

    mark_auth_broken(&handle, "mine", true);

    let disk = crate::profile::load_config().expect("reload").state;
    assert!(
        disk.auth_broken.iter().any(|n| n.as_str() == "mine"),
        "this process's mark lands"
    );
    assert!(
        disk.auth_broken.iter().any(|n| n.as_str() == "other"),
        "the concurrent writer's flag survives — no blind whole-state clobber"
    );

    mark_auth_broken(&handle, "mine", false);
    let disk = crate::profile::load_config().expect("reload").state;
    assert!(
        !disk.auth_broken.iter().any(|n| n.as_str() == "mine"),
        "the clear lands as a delta too"
    );
    assert!(
        disk.auth_broken.iter().any(|n| n.as_str() == "other"),
        "and still leaves the concurrent flag alone"
    );
}

/// `oauth_config` with the feed enabled and a Fable-capable chain (full
/// scopes + subscriptionType) — the shape `clauth feed <p> on` requires.
fn feed_config(name: &str, refresh_token: Option<&str>, expires_at: Option<i64>) -> AppConfig {
    let mut config = oauth_config(name, refresh_token, expires_at);
    let p = config.profiles.first_mut().expect("profile");
    p.session_feed = true;
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
    let dir = profile_dir(name).expect("dir");
    let creds: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(dir.join("session-token.json")).ok()?).ok()?;
    creds.claude_ai_oauth
}

/// Fresh fed sidecar → install as-is; neither the chain clock nor the
/// refresher is consulted (the chain here is stone dead).
#[test]
fn feed_gate_fresh_sidecar_ready_without_refresh() {
    let _home = HomeSandbox::new();
    let name = "test-feed-fresh";
    let config = feed_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::feed_session_token(
        name,
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
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-fed", "sidecar untouched");
}

/// Stale fed sidecar + comfortably live stored chain → re-stamped from the
/// store, no refresh spent, chain metadata (subscriptionType) carried.
#[test]
fn feed_gate_stale_sidecar_feeds_from_comfortable_chain_without_spend() {
    let _home = HomeSandbox::new();
    let name = "test-feed-nospend";
    let config = feed_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::feed_session_token(
        name,
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
        ensure_installable(&handle, name, never_refresh),
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
        .find(name)
        .and_then(|p| p.access_token_expires_at())
        .expect("chain expiry")
}

/// Stale sidecar + stale chain → guarded refresh; the rotation persist
/// re-feeds the sidecar with the freshly minted access token.
#[test]
fn feed_gate_stale_sidecar_stale_chain_refreshes_and_refeeds() {
    let _home = HomeSandbox::new();
    let name = "test-feed-refresh";
    let config = feed_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::feed_session_token(
        name,
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
        ensure_installable(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-new",
        "hook re-fed the rotated access token"
    );
    assert!(oauth.refresh_token.is_none());
}

/// Feed flag on but NO sidecar yet → the gate arms it through the refresh
/// leg instead of falling through to a vanilla pair install (which would put
/// the shared rotating chain in front of sessions).
#[test]
fn feed_gate_absent_sidecar_arms_instead_of_vanilla_install() {
    let _home = HomeSandbox::new();
    let name = "test-feed-arm";
    let config = feed_config(name, Some("rt-old"), Some(past_expiry()));
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
        ensure_installable(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    let oauth = sidecar_oauth(name).expect("sidecar armed");
    assert_eq!(oauth.access_token, "at-armed");
    assert!(oauth.refresh_token.is_none());
    let expected = crate::claude::install_source_path(name).expect("source");
    assert!(
        expected.ends_with("session-token.json"),
        "the armed sidecar is now the install source"
    );
}

/// Terminally dead chain + preserved static mint → degrade to the mint
/// (Ready) instead of benching the account; the backup is consumed.
#[test]
fn feed_gate_dead_chain_restores_static_mint() {
    let _home = HomeSandbox::new();
    let name = "test-feed-degrade";
    let config = feed_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    // A genuine mint first (1yr horizon, no subscriptionType)…
    crate::claude::write_session_token(name, "sk-ant-oat01-mint", crate::usage::now_ms() as i64)
        .expect("mint");
    // …then the feed takes over, preserving it…
    crate::claude::feed_session_token(
        name,
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
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid("revoked".to_string()));
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "sk-ant-oat01-mint", "the mint is back");
    let backup = profile_dir(name)
        .expect("dir")
        .join("session-token.static.json");
    assert!(!backup.exists(), "backup consumed by the restore");
}

/// Terminally dead chain and no mint to fall back to → Broken stands (the
/// pre-feed refusal), never a vanilla install of the dead pair.
#[test]
fn feed_gate_dead_chain_without_backup_stays_broken() {
    let _home = HomeSandbox::new();
    let name = "test-feed-broken";
    let config = feed_config(name, Some("rt-dead"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::feed_session_token(
        name,
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
        |_rt: &str, _scopes: Option<&str>| Err(RefreshError::Invalid("revoked".to_string()));
    assert!(matches!(
        ensure_installable(&handle, name, refresher),
        AuthGate::Broken
    ));
}

/// The rotation persist re-stamps a feed-enabled profile's sidecar (parked or
/// active) and preserves a genuine mint on first contact; a non-feed split
/// profile's sidecar stays untouched (the CLA-SPLIT quiet branch).
#[test]
fn rotation_hook_feeds_enabled_profiles_and_preserves_the_mint() {
    let _home = HomeSandbox::new();
    let name = "test-feed-hook";
    let config = feed_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(name, "sk-ant-oat01-mint", crate::usage::now_ms() as i64)
        .expect("mint");
    let handle = Arc::new(RankedMutex::new(config));
    apply_rotated_tokens_locked(
        &handle,
        name,
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
    let backup = profile_dir(name)
        .expect("dir")
        .join("session-token.static.json");
    let backed: ClaudeCredentials =
        serde_json::from_slice(&std::fs::read(&backup).expect("backup")).expect("parse");
    assert_eq!(
        backed.access_token(),
        Some("sk-ant-oat01-mint"),
        "first feed preserved the mint"
    );
}

/// Same rotation on a split profile WITHOUT the feed: the sidecar is the
/// static mint and stays byte-identical (the designed quiet steady state).
#[test]
fn rotation_hook_leaves_non_feed_split_sidecars_alone() {
    let _home = HomeSandbox::new();
    let name = "test-nofeed-hook";
    let config = oauth_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(name, "sk-ant-oat01-mint", crate::usage::now_ms() as i64)
        .expect("mint");
    let handle = Arc::new(RankedMutex::new(config));
    apply_rotated_tokens_locked(
        &handle,
        name,
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

/// A mis-filled sidecar (rotating pair) is never overwritten by the feed —
/// the DANGER evidence survives for the operator to see.
#[test]
fn rotation_hook_never_overwrites_a_misfilled_sidecar() {
    let _home = HomeSandbox::new();
    let name = "test-misfill-hook";
    let config = feed_config(name, Some("rt-old"), Some(past_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = profile_dir(name).expect("dir");
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
        name,
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
fn feed_gate_heals_a_misfilled_sidecar_when_a_backup_exists() {
    let _home = HomeSandbox::new();
    let name = "test-feed-heal";
    let config = feed_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(name, "sk-ant-oat01-mint", crate::usage::now_ms() as i64)
        .expect("mint");
    crate::claude::feed_session_token(
        name,
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed preserves mint");
    let dir = profile_dir(name).expect("dir");
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
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
    // Healed AND re-armed in one pass: the restored mint is immediately
    // superseded by a fed bearer from the comfortable chain (no spend).
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-old",
        "healed then re-fed from the chain"
    );
    assert!(oauth.refresh_token.is_none());
    let source = crate::claude::install_source_path(name).expect("source");
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
fn feed_gate_supersedes_a_static_mint_with_a_fed_bearer() {
    let _home = HomeSandbox::new();
    let name = "test-feed-mint-supersede";
    let config = feed_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::write_session_token(name, "sk-ant-oat01-mint", crate::usage::now_ms() as i64)
        .expect("mint");
    let handle = Arc::new(RankedMutex::new(config));
    assert!(matches!(
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-old", "fed from the stored chain");
    assert!(oauth.refresh_token.is_none());
    let backup: ClaudeCredentials = serde_json::from_slice(
        &std::fs::read(
            profile_dir(name)
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
fn feed_gate_misfill_without_backup_keeps_the_disengaged_vanilla_posture() {
    let _home = HomeSandbox::new();
    let name = "test-feed-misfill-vanilla";
    let config = feed_config(name, Some("rt-good"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    let dir = profile_dir(name).expect("dir");
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
        ensure_installable(&handle, name, never_refresh),
        AuthGate::Ready
    ));
    let source = crate::claude::install_source_path(name).expect("source");
    assert!(
        source.ends_with("credentials.json"),
        "no backup to degrade to — disengaged split behaves as vanilla"
    );
}

// ---------------------------------------------------------------------------
// EXP-1/F2: scheduler-leg re-feed (`refeed_session_token`) — renew the fed
// bearer HOURS before its clock death, not seconds (the RC-C fix).
// ---------------------------------------------------------------------------

/// Epoch-ms comfortably beyond the re-feed horizon — a chain the no-spend
/// re-stamp path may clone from.
fn beyond_horizon_expiry() -> i64 {
    crate::usage::now_ms() as i64 + 6 * 3_600_000
}

/// The due predicate fires only for an armed, exp-carrying sidecar inside the
/// horizon — absent sidecars and mis-fills belong to the switch-time gate.
#[test]
fn refeed_due_fires_inside_the_horizon_only() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;
    let name = "test-refeed-due";
    assert!(
        !fed_sidecar_refeed_due(name, now),
        "absent sidecar is not the timer's job"
    );
    crate::claude::feed_session_token(
        name,
        &OAuthToken {
            access_token: "at-fed".to_string(),
            refresh_token: None,
            expires_at: Some(beyond_horizon_expiry()),
            scopes: None,
            subscription_type: None,
        },
    )
    .expect("feed");
    assert!(
        !fed_sidecar_refeed_due(name, now),
        "a bearer clear of the horizon is left alone"
    );
    crate::claude::feed_session_token(
        name,
        &OAuthToken {
            access_token: "at-fed-dying".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()), // +1h, inside the 2h horizon
            scopes: None,
            subscription_type: None,
        },
    )
    .expect("feed");
    assert!(
        fed_sidecar_refeed_due(name, now),
        "a bearer inside the horizon is due"
    );
}

/// A dying fed bearer + a chain clear of the horizon → no-spend re-stamp,
/// exactly where the switch gate (seconds-tight grace) would have no-opped —
/// the contrast that pins the horizon semantics.
#[test]
fn refeed_restamps_a_dying_bearer_the_switch_gate_calls_fresh() {
    let _home = HomeSandbox::new();
    let name = "test-refeed-nospend";
    let config = feed_config(name, Some("rt-good"), Some(beyond_horizon_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::feed_session_token(
        name,
        &OAuthToken {
            access_token: "at-fed-dying".to_string(),
            refresh_token: None,
            expires_at: Some(future_expiry()), // +1h: dying, but "fresh" to the switch gate
            scopes: None,
            subscription_type: Some("max".into()),
        },
    )
    .expect("feed");
    let handle = Arc::new(RankedMutex::new(config));
    // Switch gate: +1h clears the 60s grace → install as-is, no re-stamp.
    assert!(matches!(
        ensure_installable(&handle, name, never_refresh),
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
        refeed_session_token(&handle, name, never_refresh),
        AuthGate::Ready
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(oauth.access_token, "at-old", "re-stamped from the chain");
    assert!(oauth.refresh_token.is_none(), "pair never leaves custody");
}

/// Dying bearer + chain itself inside the horizon (they usually share an
/// expiry — the fed token IS the chain's access token) → guarded refresh; the
/// rotation hook re-feeds the freshly minted token.
#[test]
fn refeed_rotates_when_the_chain_is_inside_the_horizon_too() {
    let _home = HomeSandbox::new();
    let name = "test-refeed-rotate";
    let config = feed_config(name, Some("rt-old"), Some(future_expiry()));
    crate::profile::save_profile(&config.profiles[0]).expect("save profile");
    crate::claude::feed_session_token(
        name,
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
        refeed_session_token(&handle, name, refresher),
        AuthGate::Refreshed
    ));
    let oauth = sidecar_oauth(name).expect("sidecar");
    assert_eq!(
        oauth.access_token, "at-new",
        "hook re-fed the rotated token"
    );
    assert!(oauth.refresh_token.is_none());
}
