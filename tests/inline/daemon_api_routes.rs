#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Route behavior: who is let in, what the feed serves, and which switches are
//! refused.
//!
//! Everything runs against a [`HomeSandbox`] tempdir and `keychain::enabled()`
//! is false under `cfg(test)`, so the switch paths exercise the file/symlink
//! model only and never touch the operator's real `~/.clauth`, `~/.claude`, or
//! the Keychain. No network: tokens are minted without an expiry, so the
//! pre-install auth gate returns `Ready` without calling the refresher.

#![cfg(unix)]

use super::*;

use crate::profile::{
    AppConfig, AppState, ClaudeCredentials, ConfigHandle, OAuthToken, Profile, save_app_state,
    save_profile,
};
use crate::testutil::HomeSandbox;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn creds(access: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("{access}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    }
}

fn stored_profile(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(creds(name));
    save_profile(&p).expect("save profile");
    p
}

/// Two profiles, the first active, both with usable stored credentials.
fn seeded_config() -> ConfigHandle {
    let profiles = vec![stored_profile("alpha"), stored_profile("beta")];
    let state = AppState {
        active_profile: Some("alpha".into()),
        profiles: vec!["alpha".into(), "beta".into()],
        ..Default::default()
    };
    // Persisted, not just built in memory: `ensure_installable` gates on
    // `profile::is_configured`, which reads the roster back off disk so a target
    // deleted by a concurrent CLI bounces before any relink tears the live slot
    // down. A fixture that only holds the roster in memory reads as "not found".
    save_app_state(&state).expect("save app state");
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(AppConfig {
        state,
        profiles,
    }))
}

fn ctx_with(config: ConfigHandle) -> std::sync::Arc<ApiContext> {
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::new(config, status_path, AuthToken::from_plaintext(TOKEN))
}

/// A request as the HTTP layer would hand it to the router.
fn req(method: &str, path: &str, bearer: Option<&str>, body: &str) -> Request {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        bearer: bearer.map(str::to_string),
        body: body.as_bytes().to_vec(),
        // Routing does not depend on this; the connection loop owns it.
        keep_alive: true,
    }
}

fn body_json(resp: &Response) -> serde_json::Value {
    serde_json::from_slice(&resp.body).expect("response body is json")
}

// ---------------------------------------------------------------- auth

/// Every route, health included. An unauthenticated caller learns only that
/// something is listening.
#[test]
fn every_route_requires_the_token() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    for (method, path) in [
        ("GET", "/v1/health"),
        ("GET", "/v1/status"),
        ("POST", "/v1/switch"),
    ] {
        let resp = handle(&ctx, &req(method, path, None, ""));
        assert_eq!(resp.status, 401, "{method} {path} without a token");
        assert!(resp.challenge, "{method} {path} must challenge");
    }
}

#[test]
fn a_wrong_token_is_rejected() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let wrong = "f".repeat(64);

    let resp = handle(&ctx, &req("GET", "/v1/health", Some(&wrong), ""));
    assert_eq!(resp.status, 401);
}

#[test]
fn health_reports_the_feed_schema() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    let resp = handle(&ctx, &req("GET", "/v1/health", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    let body = body_json(&resp);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(
        body["schema"],
        serde_json::json!(crate::daemon::SCHEMA_VERSION),
        "a client refuses a daemon newer than it knows off this number"
    );
}

#[test]
fn an_unknown_path_is_404_and_a_wrong_method_is_405() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    assert_eq!(
        handle(&ctx, &req("GET", "/v1/nope", Some(TOKEN), "")).status,
        404
    );
    assert_eq!(
        handle(&ctx, &req("POST", "/v1/status", Some(TOKEN), "")).status,
        405,
        "a known path with the wrong verb says which half is wrong"
    );
    assert_eq!(
        handle(&ctx, &req("GET", "/v1/switch", Some(TOKEN), "")).status,
        405
    );
}

// -------------------------------------------------------------- status

/// The published feed is passed through byte for byte: one writer, one shape.
#[test]
fn status_serves_the_on_disk_feed_verbatim() {
    let home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let feed = r#"{"schema":1,"active_profile":"alpha","profiles":[]}"#;
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    std::fs::write(&ctx.status_path, feed).expect("seed feed");
    let _ = home;

    let resp = handle(&ctx, &req("GET", "/v1/status", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&resp.body),
        feed,
        "the feed must not be re-serialized on the way out"
    );
}

/// A reader that connects during the daemon's first tick still gets a coherent
/// body rather than a 404 or an empty file.
#[test]
fn status_falls_back_to_a_built_body_when_the_feed_is_missing() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    assert!(!ctx.status_path.exists());

    let resp = handle(&ctx, &req("GET", "/v1/status", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    let body = body_json(&resp);
    assert_eq!(body["active_profile"], serde_json::json!("alpha"));
    assert_eq!(
        body["schema"],
        serde_json::json!(crate::daemon::SCHEMA_VERSION)
    );
}

/// The published feed always hides disabled accounts, so `?all=1` has to bypass
/// the passthrough and rebuild.
#[test]
fn all_reveals_disabled_accounts_that_the_plain_feed_hides() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    {
        let mut cfg = config.lock().expect("config");
        let beta = cfg
            .find_mut(&crate::profile::ProfileName::from("beta"))
            .expect("beta");
        beta.disabled = true;
        save_profile(beta).expect("save");
    }
    let ctx = ctx_with(config);

    let plain = body_json(&handle(&ctx, &req("GET", "/v1/status", Some(TOKEN), "")));
    let all = body_json(&handle(
        &ctx,
        &req("GET", "/v1/status?all=1", Some(TOKEN), ""),
    ));

    let names = |v: &serde_json::Value| -> Vec<String> {
        v["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .map(|p| p["name"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert!(!names(&plain).contains(&"beta".to_string()), "{plain}");
    assert!(names(&all).contains(&"beta".to_string()), "{all}");
}

// -------------------------------------------------------------- switch

#[test]
fn switch_relinks_and_reports_the_previous_account() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = handle(
        &ctx,
        &req("POST", "/v1/switch", Some(TOKEN), r#"{"profile":"beta"}"#),
    );
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let body = body_json(&resp);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["previous"], serde_json::json!("alpha"));
    assert_eq!(body["active"], serde_json::json!("beta"));
    assert_eq!(
        config
            .lock()
            .expect("config")
            .state
            .active_profile
            .as_deref(),
        Some("beta"),
        "the switch must land in state, not just in the response"
    );
}

/// The same case-insensitive resolution the CLI and MCP paths apply, and for
/// the same reason: an unresolved name reaching the relink would strip the live
/// credential symlink and leave nothing in its place.
#[test]
fn switch_resolves_the_name_case_insensitively() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    let resp = handle(
        &ctx,
        &req("POST", "/v1/switch", Some(TOKEN), r#"{"profile":"BeTa"}"#),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(body_json(&resp)["active"], serde_json::json!("beta"));
}

#[test]
fn switch_to_an_unknown_profile_is_404_and_changes_nothing() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = handle(
        &ctx,
        &req("POST", "/v1/switch", Some(TOKEN), r#"{"profile":"ghost"}"#),
    );
    assert_eq!(resp.status, 404);
    assert_eq!(
        body_json(&resp)["error"],
        serde_json::json!("profile_not_found")
    );
    assert_eq!(
        config
            .lock()
            .expect("config")
            .state
            .active_profile
            .as_deref(),
        Some("alpha"),
    );
}

#[test]
fn switch_to_a_disabled_profile_is_refused() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    {
        let mut cfg = config.lock().expect("config");
        let beta = cfg
            .find_mut(&crate::profile::ProfileName::from("beta"))
            .expect("beta");
        beta.disabled = true;
        save_profile(beta).expect("save");
    }
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = handle(
        &ctx,
        &req("POST", "/v1/switch", Some(TOKEN), r#"{"profile":"beta"}"#),
    );
    assert_eq!(resp.status, 409);
    assert_eq!(
        config
            .lock()
            .expect("config")
            .state
            .active_profile
            .as_deref(),
        Some("alpha"),
    );
}

#[test]
fn a_malformed_switch_body_is_400() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    for body in ["", "{}", "not json", r#"{"profile":7}"#] {
        let resp = handle(&ctx, &req("POST", "/v1/switch", Some(TOKEN), body));
        assert_eq!(resp.status, 400, "body {body:?}");
    }
}

/// A switch already running means answer at once rather than parking the caller
/// on the cross-process flock for its full 25s deadline.
///
/// The gate is held on ANOTHER thread, which is the only shape that models the
/// real case: rank state is thread-local, so a same-thread re-entry would trip
/// the lock-order assertion instead of exercising this path.
#[test]
fn a_second_concurrent_switch_is_refused_immediately() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

    let holder = std::sync::Arc::clone(&ctx);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _gate = holder.switch_gate.lock().expect("gate");
            held_tx.send(()).expect("signal held");
            // Hold it until the assertion below has run.
            let _ = release_rx.recv();
        });
        held_rx.recv().expect("gate taken");

        let resp = handle(
            &ctx,
            &req("POST", "/v1/switch", Some(TOKEN), r#"{"profile":"beta"}"#),
        );
        assert_eq!(resp.status, 409);
        assert_eq!(
            body_json(&resp)["error"],
            serde_json::json!("switch_in_progress")
        );
        let _ = release_tx.send(());
    });
}
