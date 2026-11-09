#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The pool-account admin client: every request's exact method, path, header
//! and JSON body; each status mapped to its typed error with the body and the
//! token kept off it; the 401 recovery (rewrite the config's unchanged bytes,
//! poll a read route, retry once). The recovery tests drive a `/bin/sh`
//! `shunt check` stub, so they are `#[cfg(unix)]`.

use std::fs;
#[cfg(unix)]
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
#[cfg(unix)]
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::gateway::{AdminMethod, AdminRequestFailure};
use crate::testutil::{
    HomeSandbox, assert_nothing_sent, deaf_listener, request_body, request_header, request_path,
    serve_endpoints_raw,
};

/// A test token long enough for shunt to accept (>= 32 characters).
const TOKEN: &str = "test-admin-token-0123456789abcdef0123456789abcdef";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn write_token(home: &HomeSandbox, token: &str) {
    write(
        &home.home().join(".clauth").join("gateway-admin-token"),
        token,
    );
}

fn listener_addr(base: &str) -> SocketAddr {
    base.trim_start_matches("http://").parse().expect("addr")
}

/// The HTTP method off a raw request text: the first token of its first line.
fn request_method(raw: &str) -> String {
    raw.lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("")
        .to_string()
}

/// A record whose config binds the gateway at `addr`.
fn record_at(home: &HomeSandbox, addr: SocketAddr) -> GatewayRecord {
    let config = home.home().join("etc").join("shunt.toml");
    write(&config, &format!("[server]\nbind = \"{addr}\"\n"));
    GatewayRecord::new(config).expect("adoptable")
}

/// A record whose config binds the gateway at `addr` AND carries clauth's
/// admin write key, so the 401 recovery's entry check passes.
#[cfg(unix)]
fn admin_record_at(home: &HomeSandbox, addr: SocketAddr) -> GatewayRecord {
    let config = home.home().join("etc").join("shunt.toml");
    let key_ref = format!(
        "${{file:{}}}",
        crate::gateway::admin_token_path()
            .expect("token path")
            .display()
    );
    write(
        &config,
        &format!(
            "[server]\nbind = \"{addr}\"\n\n[server.admin]\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"{key_ref}\"\n"
        ),
    );
    GatewayRecord::new(config).expect("adoptable")
}

fn name(text: &str) -> ShuntAccountName {
    ShuntAccountName::new(text).expect("valid name")
}

// ── the recovery stub (`shunt check`) ──────────────────────────────────────

#[cfg(unix)]
fn stub_dir(home: &HomeSandbox) -> PathBuf {
    let stub = home.home().join("stub");
    fs::create_dir_all(&stub).expect("stub dir");
    stub
}

/// A record bound at `addr` whose `shunt check` appends `call` to `calls` and
/// exits 0, so a test observes whether and how often [`write_checked`] ran.
#[cfg(unix)]
fn checked_record(home: &HomeSandbox, addr: SocketAddr, calls: &Path) -> GatewayRecord {
    let mut record = record_at(home, addr);
    let stub = calls.parent().expect("stub dir");
    let body = format!("echo call >> \"{}\"\nexit 0", calls.display());
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    record
}

/// [`checked_record`] over a config that already carries clauth's admin entry.
#[cfg(unix)]
fn checked_admin_record(home: &HomeSandbox, addr: SocketAddr, calls: &Path) -> GatewayRecord {
    let mut record = admin_record_at(home, addr);
    let stub = calls.parent().expect("stub dir");
    let body = format!("echo call >> \"{}\"\nexit 0", calls.display());
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    record
}

// ── request shapes ──────────────────────────────────────────────────────────

#[test]
fn a_claude_add_sends_the_oauth_mode_and_the_name() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (
            200,
            r#"{"name":"acct","authorize_url":"http://example/authorize"}"#.to_string(),
        )
    });
    let record = record_at(&home, listener_addr(&base));
    let outcome = add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect("add");
    assert_eq!(outcome.authorize_url, "http://example/authorize");
    let raw = seen.join().expect("listener");
    assert_eq!(request_method(&raw[0]), "POST");
    assert_eq!(request_path(&raw[0]), "/admin/api/accounts/claude");
    assert_eq!(request_header(&raw[0], "x-api-key").as_deref(), Some(TOKEN));
    assert_eq!(
        request_header(&raw[0], "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(request_body(&raw[0]), r#"{"name":"acct","mode":"oauth"}"#);
}

#[test]
fn codex_and_antigravity_adds_send_the_name_alone() {
    let home = HomeSandbox::new();
    for (family, segment) in [
        (PoolFamily::Codex, "codex"),
        (PoolFamily::Antigravity, "antigravity"),
    ] {
        write_token(&home, TOKEN);
        let (base, seen) = serve_endpoints_raw(1, |_, _| {
            (
                200,
                r#"{"name":"acct","authorize_url":"http://example/authorize"}"#.to_string(),
            )
        });
        let record = record_at(&home, listener_addr(&base));
        add_pool_account(&record, family, &name("acct")).expect("add");
        let raw = seen.join().expect("listener");
        assert_eq!(request_method(&raw[0]), "POST", "family {segment}");
        assert_eq!(
            request_path(&raw[0]),
            format!("/admin/api/accounts/{segment}"),
            "family {segment}"
        );
        assert_eq!(
            request_header(&raw[0], "content-type").as_deref(),
            Some("application/json"),
            "family {segment}"
        );
        assert_eq!(
            request_body(&raw[0]),
            r#"{"name":"acct"}"#,
            "family {segment}"
        );
    }
}

#[test]
fn complete_sends_the_pasted_code_verbatim() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    // The `#` and the space must ride through untouched: clauth relays the
    // pasted string, never splits or trims it.
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (
            200,
            r#"{"name":"acct","stored":true,"live":true,"message":"ignored"}"#.to_string(),
        )
    });
    let record = record_at(&home, listener_addr(&base));
    let outcome =
        complete_pool_account(&record, PoolFamily::Claude, &name("acct"), "a#b c/d").expect("ok");
    assert!(outcome.live);
    let raw = seen.join().expect("listener");
    assert_eq!(request_method(&raw[0]), "POST");
    assert_eq!(
        request_path(&raw[0]),
        "/admin/api/accounts/claude/acct/complete"
    );
    assert_eq!(request_header(&raw[0], "x-api-key").as_deref(), Some(TOKEN));
    assert_eq!(
        request_header(&raw[0], "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(request_body(&raw[0]), r#"{"code":"a#b c/d"}"#);
}

#[test]
fn remove_sends_a_delete_and_carries_removed() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (200, r#"{"name":"acct","removed":false}"#.to_string())
    });
    let record = record_at(&home, listener_addr(&base));
    let outcome = remove_pool_account(&record, PoolFamily::Claude, &name("acct")).expect("remove");
    assert!(
        !outcome.removed,
        "shunt's removed:false is carried, never read as gone"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(request_method(&raw[0]), "DELETE");
    assert_eq!(request_path(&raw[0]), "/admin/api/accounts/claude/acct");
    assert_eq!(request_header(&raw[0], "x-api-key").as_deref(), Some(TOKEN));
    assert_eq!(request_body(&raw[0]), "");
}

#[test]
fn refresh_sends_an_empty_post_and_carries_needs_relogin() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (
            200,
            r#"{"name":"acct","refreshed":true,"needs_relogin":true,"expires_at":null,"message":"ignored"}"#
                .to_string(),
        )
    });
    let record = record_at(&home, listener_addr(&base));
    let outcome =
        refresh_pool_account(&record, RefreshFamily::Claude, &name("acct")).expect("refresh");
    assert!(outcome.needs_relogin);
    let raw = seen.join().expect("listener");
    assert_eq!(request_method(&raw[0]), "POST");
    assert_eq!(
        request_path(&raw[0]),
        "/admin/api/accounts/claude/acct/refresh"
    );
    assert_eq!(request_header(&raw[0], "x-api-key").as_deref(), Some(TOKEN));
    assert_eq!(request_body(&raw[0]), "");
}

// ── the typed listing ───────────────────────────────────────────────────────

/// A pool body whose claude provider carries one full account and one with
/// every optional field absent; kimi and an unknown future mode ride their own
/// variants. Every field clauth does not read is ignored.
const POOL_BODY: &str = r#"{"providers":[
  {"provider":"claude","auth":"claude_oauth","accounts":[
    {"name":"a1","available":true,"near_quota":false,"cooldown_secs_remaining":42,"needs_relogin":false,"paused":false,"disabled":false,"utilization_5h":0.5,"reset_5h":1000,"utilization_7d":0.25,"reset_7d":2000,"plan":"max","status":"healthy","has_state":true,"priority":100,"requests_attempted":1,"requests_succeeded":1,"requests_failed":0,"requests_cancelled":0},
    {"name":"a2","available":false,"near_quota":true,"needs_relogin":true,"paused":true,"disabled":true}
  ]},
  {"provider":"codex","auth":"chatgpt_oauth","accounts":[]},
  {"provider":"kimi","auth":"kimi_oauth","accounts":[]},
  {"provider":"antigravity","auth":"antigravity_oauth","accounts":[]},
  {"provider":"future","auth":"future_oauth","accounts":[]}
]}"#;

#[test]
fn the_pool_listing_parses_providers_accounts_and_absent_optionals() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| (200, POOL_BODY.to_string()));
    let record = record_at(&home, listener_addr(&base));
    let providers = list_pool(&record).expect("listing");
    let raw = seen.join().expect("listener");
    assert_eq!(request_method(&raw[0]), "GET");
    assert_eq!(request_path(&raw[0]), "/admin/api/pool");
    assert_eq!(request_header(&raw[0], "x-api-key").as_deref(), Some(TOKEN));

    assert_eq!(providers.len(), 5);
    assert_eq!(providers[0].name, "claude");
    assert_eq!(providers[0].auth, PoolAuth::Claude);
    let accounts = &providers[0].accounts;
    assert_eq!(accounts.len(), 2);
    assert_eq!(
        accounts[0],
        PoolAccount {
            name: "a1".to_string(),
            available: true,
            near_quota: false,
            cooldown_secs_remaining: Some(42),
            needs_relogin: false,
            paused: false,
            disabled: false,
            utilization_5h: Some(0.5),
            reset_5h: Some(1000),
            utilization_7d: Some(0.25),
            reset_7d: Some(2000),
            utilization_7d_oi: None,
            reset_7d_oi: None,
            plan: Some("max".to_string()),
            status: Some("healthy".to_string()),
            requests_attempted: Some(1),
            requests_failed: Some(0),
            requests_cancelled: Some(0),
            mean_latency_ms: None,
        }
    );
    assert_eq!(
        accounts[1],
        PoolAccount {
            name: "a2".to_string(),
            available: false,
            near_quota: true,
            cooldown_secs_remaining: None,
            needs_relogin: true,
            paused: true,
            disabled: true,
            utilization_5h: None,
            reset_5h: None,
            utilization_7d: None,
            reset_7d: None,
            utilization_7d_oi: None,
            reset_7d_oi: None,
            plan: None,
            status: None,
            requests_attempted: None,
            requests_failed: None,
            requests_cancelled: None,
            mean_latency_ms: None,
        }
    );
    assert_eq!(providers[1].name, "codex");
    assert_eq!(providers[1].auth, PoolAuth::Codex);
    assert_eq!(providers[2].name, "kimi");
    assert_eq!(providers[2].auth, PoolAuth::Kimi);
    assert_eq!(providers[3].name, "antigravity");
    assert_eq!(providers[3].auth, PoolAuth::Antigravity);
    assert_eq!(providers[4].name, "future");
    assert_eq!(
        providers[4].auth,
        PoolAuth::Unknown("future_oauth".to_string())
    );
}

/// Two pool rows as shunt v0.61.0+ serializes them: one carrying the request
/// counters and a measured mean latency (the values shunt's own
/// `tests/admin_surface.rs` reads), one with zero counts and `null` latency.
const COUNTED_POOL_BODY: &str = r#"{"providers":[
  {"provider":"claude","auth":"claude_oauth","accounts":[
    {"name":"a1","available":true,"near_quota":false,"cooldown_secs_remaining":null,"needs_relogin":false,"paused":false,"disabled":false,"utilization_5h":null,"reset_5h":null,"utilization_7d":null,"reset_7d":null,"utilization_7d_oi":null,"reset_7d_oi":null,"status":null,"requests_attempted":4,"requests_succeeded":1,"requests_failed":2,"requests_cancelled":1,"mean_latency_ms":200.0},
    {"name":"a2","available":true,"near_quota":false,"cooldown_secs_remaining":null,"needs_relogin":false,"paused":false,"disabled":false,"utilization_5h":null,"reset_5h":null,"utilization_7d":null,"reset_7d":null,"utilization_7d_oi":null,"reset_7d_oi":null,"status":null,"requests_attempted":0,"requests_succeeded":0,"requests_failed":0,"requests_cancelled":0,"mean_latency_ms":null}
  ]}
]}"#;

#[test]
fn the_pool_listing_parses_request_counters_and_a_null_latency() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| (200, COUNTED_POOL_BODY.to_string()));
    let record = record_at(&home, listener_addr(&base));
    let providers = list_pool(&record).expect("a v0.61.0 pool row parses");
    seen.join().expect("listener");
    let counters = providers[0]
        .accounts
        .iter()
        .map(|a| {
            (
                a.requests_attempted,
                a.requests_failed,
                a.requests_cancelled,
                a.mean_latency_ms,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        counters,
        [
            (Some(4), Some(2), Some(1), Some(200.0)),
            (Some(0), Some(0), Some(0), None),
        ]
    );
}

/// One pool row as shunt `v0.48.0` (the version floor) serializes an account
/// the pool has never selected: `AccountSnapshot`'s field set at that tag
/// (`git show v0.48.0:src/accounts.rs`), every `Option` written as `null` (no
/// `skip_serializing_if` there), no `paused` (added in `v0.52.0`) and no
/// `plan` (the pool route inserts it only when one resolves).
const FLOOR_POOL_BODY: &str = r#"{"providers":[
  {"provider":"claude","auth":"claude_oauth","accounts":[
    {"name":"a1","has_state":false,"available":true,"near_quota":false,"cooldown_secs_remaining":null,"cooldown_fable_secs_remaining":null,"priority":100,"disabled":false,"headroom_secs":null,"utilization_5h":null,"reset_5h":null,"utilization_7d":null,"reset_7d":null,"utilization_7d_oi":null,"reset_7d_oi":null,"status":null,"needs_relogin":false}
  ]}
]}"#;

#[test]
fn a_floor_version_pool_row_without_paused_parses_as_unpaused() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let (base, seen) = serve_endpoints_raw(1, |_, _| (200, FLOOR_POOL_BODY.to_string()));
    let record = record_at(&home, listener_addr(&base));
    let providers = list_pool(&record).expect("a 0.48.0 pool row parses");
    seen.join().expect("listener");
    assert_eq!(providers.len(), 1);
    assert_eq!(
        providers[0].accounts,
        vec![PoolAccount {
            name: "a1".to_string(),
            available: true,
            near_quota: false,
            cooldown_secs_remaining: None,
            needs_relogin: false,
            paused: false,
            disabled: false,
            utilization_5h: None,
            reset_5h: None,
            utilization_7d: None,
            reset_7d: None,
            utilization_7d_oi: None,
            reset_7d_oi: None,
            plan: None,
            status: None,
            requests_attempted: None,
            requests_failed: None,
            requests_cancelled: None,
            mean_latency_ms: None,
        }]
    );
}

// ── the typed families ──────────────────────────────────────────────────────

#[test]
fn families_exclude_kimi_mutations_and_codex_refresh() {
    assert_eq!(PoolAuth::Claude.family(), Some(PoolFamily::Claude));
    assert_eq!(PoolAuth::Codex.family(), Some(PoolFamily::Codex));
    assert_eq!(
        PoolAuth::Antigravity.family(),
        Some(PoolFamily::Antigravity)
    );
    assert_eq!(PoolAuth::Kimi.family(), None);
    assert_eq!(PoolAuth::Unknown("future".to_string()).family(), None);

    assert_eq!(
        PoolAuth::Claude.refresh_family(),
        Some(RefreshFamily::Claude)
    );
    assert_eq!(PoolAuth::Codex.refresh_family(), None);
    assert_eq!(PoolAuth::Kimi.refresh_family(), None);
    assert_eq!(
        PoolAuth::Antigravity.refresh_family(),
        Some(RefreshFamily::Antigravity)
    );
    assert_eq!(
        PoolAuth::Unknown("future".to_string()).refresh_family(),
        None
    );
}

// ── status mapping ──────────────────────────────────────────────────────────

#[test]
fn each_mutation_status_is_its_own_typed_error_without_the_body_or_token() {
    let home = HomeSandbox::new();
    for (status, message) in [
        (400u16, "the gateway refused the request (status 400)"),
        (404, "the gateway has no such route or account (status 404)"),
        (429, "the gateway rate-limited the request (status 429)"),
        (500, "the gateway failed the request (status 500)"),
        (502, "the gateway's upstream failed (status 502)"),
        (418, "the gateway answered status 418"),
    ] {
        write_token(&home, TOKEN);
        let marker = format!("marker-{status}-body");
        let (base, seen) = serve_endpoints_raw(1, move |_, _| (status, marker.clone()));
        let record = record_at(&home, listener_addr(&base));
        let err =
            remove_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("typed");
        let typed = err
            .downcast_ref::<ShuntAdminError>()
            .expect("a typed ShuntAdminError");
        assert_eq!(format!("{err:#}"), message, "status {status}");
        let debug = format!("{typed:?}");
        assert!(
            !format!("{err:#}").contains("marker"),
            "the body never reaches the error (status {status})"
        );
        assert!(
            !debug.contains("marker"),
            "the body never reaches the Debug (status {status})"
        );
        assert!(
            !format!("{err:#}").contains(TOKEN),
            "the token never reaches the error (status {status})"
        );
        assert!(
            !debug.contains(TOKEN),
            "the token never reaches the Debug (status {status})"
        );
        seen.join().expect("listener");
    }
}

#[test]
fn an_invalid_account_name_is_refused_at_construction() {
    for bad in ["", "Bad_Name", "a.b", "two words", "UPPER", "trail!"] {
        assert!(
            ShuntAccountName::new(bad).is_none(),
            "the name {bad:?} must be refused at construction"
        );
    }
    assert_eq!(ShuntAccountName::new("a-1").expect("valid").as_str(), "a-1");
}

// ── per-route bounds ────────────────────────────────────────────────────────

#[test]
fn each_operation_builds_its_agent_from_its_routes_bound() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);

    // complete: 40 s, above shunt's own 30 s code exchange.
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (
            200,
            r#"{"name":"acct","stored":true,"live":true,"message":"i"}"#.to_string(),
        )
    });
    let record = record_at(&home, listener_addr(&base));
    complete_pool_account(&record, PoolFamily::Claude, &name("acct"), "code").expect("ok");
    seen.join().expect("listener");
    let complete_bound = recorded_agent_bounds()
        .into_iter()
        .find(|(route, _)| *route == AdminRoute::Complete)
        .map(|(_, bound)| bound)
        .expect("complete recorded its bound");
    assert_eq!(complete_bound, Duration::from_secs(40));
    assert!(
        complete_bound > Duration::from_secs(COMPLETION_EXCHANGE_TIMEOUT_SECS),
        "the complete bound exceeds shunt's exchange window"
    );

    // add: 20 s, the pool read's bound.
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (200, r#"{"name":"acct","authorize_url":"u"}"#.to_string())
    });
    let record = record_at(&home, listener_addr(&base));
    add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect("ok");
    seen.join().expect("listener");
    let add_bound = recorded_agent_bounds()
        .into_iter()
        .find(|(route, _)| *route == AdminRoute::Add)
        .map(|(_, bound)| bound)
        .expect("add recorded its bound");
    assert_eq!(add_bound, Duration::from_secs(20));

    // refresh: 600 s, its own (past one whole antigravity refresh, discovery
    // and onboarding included; shunt bounds a claude refresh by nothing).
    let (base, seen) = serve_endpoints_raw(1, |_, _| {
        (
            200,
            r#"{"name":"acct","refreshed":true,"needs_relogin":false,"expires_at":null,"message":"i"}"#
                .to_string(),
        )
    });
    let record = record_at(&home, listener_addr(&base));
    refresh_pool_account(&record, RefreshFamily::Claude, &name("acct")).expect("ok");
    seen.join().expect("listener");
    let refresh_bound = recorded_agent_bounds()
        .into_iter()
        .find(|(route, _)| *route == AdminRoute::Refresh)
        .map(|(_, bound)| bound)
        .expect("refresh recorded its bound");
    assert_eq!(refresh_bound, Duration::from_secs(600));
}

// ── transport failures ──────────────────────────────────────────────────────

#[test]
fn a_missing_admin_token_is_named_before_any_request() {
    let home = HomeSandbox::new();
    let (listener, base) = deaf_listener();
    let record = record_at(&home, listener_addr(&base));
    let err = add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("no token");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::NoAdminToken),
        "a missing token file is named, got {typed:?}"
    );
    assert_nothing_sent(&listener);
}

#[test]
fn a_timeout_is_named_separately_from_a_refused_connection() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let token = crate::gateway::read_admin_token().expect("token");
    // A listener that takes the connection and never answers: the response
    // bound fires as a timeout, distinct from a refused connection.
    let (listener, base) = deaf_listener();
    let addr = listener_addr(&base);
    let agent = crate::gateway::admin_agent(Duration::from_millis(200));
    let failure =
        crate::gateway::admin_request(&agent, AdminMethod::Get, addr, "/admin/api/routes", &token)
            .expect_err("timeout");
    assert!(
        matches!(failure, AdminRequestFailure::TimedOut { addr: a } if a == addr),
        "a hung request is a timeout, not a refused connection"
    );
    drop(listener);

    let timed = transport_err(AdminRequestFailure::TimedOut { addr }, AdminRoute::Poll);
    assert_eq!(
        timed.to_string(),
        format!("the gateway at {addr} did not answer within 2 s")
    );
    assert!(
        matches!(timed, ShuntAdminError::TimedOut { addr: a, secs: 2 } if a == addr),
        "a timeout maps to TimedOut naming the poll's 2 s bound"
    );
    let refused = transport_err(AdminRequestFailure::Unreachable { addr }, AdminRoute::Poll);
    assert!(
        matches!(refused, ShuntAdminError::Unreachable { addr: a } if a == addr),
        "a refused connection stays Unreachable"
    );
}

// ── the 401 recovery ────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn a_401_mutation_rewrites_the_config_and_retries_once() {
    // The stub keys "reloaded" off the config's inode (the checked write
    // renames a staged file over the original), never off the `shunt check`
    // call record: the first two polls after the rename still answer 401
    // (shunt's 400 ms debounce), then the route clears.
    const POLLS_BEFORE_CLEAR: usize = 2;

    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let config = home.home().join("etc").join("shunt.toml");
    let original_ino = Arc::new(AtomicU64::new(0));
    let polls_since_reload = Arc::new(AtomicUsize::new(0));
    let (base, seen) = serve_endpoints_raw(6, {
        let config = config.clone();
        let original_ino = Arc::clone(&original_ino);
        let polls_since_reload = Arc::clone(&polls_since_reload);
        move |path, _| {
            let ino = fs::metadata(&config).map(|m| m.ino()).unwrap_or(0);
            let reloaded = ino != 0 && ino != original_ino.load(Ordering::SeqCst);
            match path {
                "/admin/api/routes" => {
                    if !reloaded {
                        (401, "unauthorized".to_string())
                    } else {
                        let n = polls_since_reload.fetch_add(1, Ordering::SeqCst);
                        if n < POLLS_BEFORE_CLEAR {
                            (401, "unauthorized".to_string())
                        } else {
                            (200, "{}".to_string())
                        }
                    }
                }
                _ if reloaded => (
                    200,
                    r#"{"name":"acct","authorize_url":"http://example/authorize"}"#.to_string(),
                ),
                _ => (401, "unauthorized".to_string()),
            }
        }
    });
    let record = checked_admin_record(&home, listener_addr(&base), &calls);
    original_ino.store(fs::metadata(&config).expect("meta").ino(), Ordering::SeqCst);
    let bytes_before = fs::read(&config).expect("read");

    let outcome = add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect("added");
    assert_eq!(outcome.authorize_url, "http://example/authorize");

    // The recovery ran the checked write exactly once...
    assert_eq!(fs::read_to_string(&calls).expect("calls"), "call\n");
    // ...renamed the config (a new inode is what the watcher sees)...
    assert_ne!(
        fs::metadata(&config).expect("meta").ino(),
        original_ino.load(Ordering::SeqCst),
        "the checked write renamed the config"
    );
    // ...and left its bytes unchanged.
    assert_eq!(
        fs::read(&config).expect("read"),
        bytes_before,
        "the config's bytes are unchanged"
    );

    let raw = seen.join().expect("listener");
    assert_eq!(
        raw.iter().map(|r| request_path(r)).collect::<Vec<_>>(),
        vec![
            "/admin/api/accounts/claude",
            "/admin/api/routes",
            "/admin/api/routes",
            "/admin/api/routes",
            "/admin/api/accounts/claude"
        ],
        "the first attempt, two 401 polls past the reload debounce, then exactly one retry"
    );
    // The retry carried the same request as the first attempt.
    assert_eq!(request_body(&raw[0]), request_body(&raw[4]));
    assert_eq!(request_header(&raw[0], "x-api-key").as_deref(), Some(TOKEN));
    assert_eq!(request_header(&raw[4], "x-api-key").as_deref(), Some(TOKEN));
}

#[cfg(unix)]
#[test]
fn a_poll_that_never_clears_names_the_restart() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(3, |path, _| {
        let _ = path;
        (401, "unauthorized".to_string())
    });
    let record = checked_admin_record(&home, listener_addr(&base), &calls);
    // A zero budget: the first non-clearing poll exhausts it, so the test
    // never waits out the real 5 s.
    let _budget = PollBudgetOverride::set(Duration::ZERO);
    let err = remove_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("restart");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::RestartRequired),
        "a poll that never clears is the restart variant, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        "the gateway still refuses clauth's admin key; restart the gateway: a → stop shunt, then start shunt"
    );
    assert_eq!(fs::read_to_string(&calls).expect("calls"), "call\n");
    let raw = seen.join().expect("listener");
    assert_eq!(
        raw.iter().map(|r| request_path(r)).collect::<Vec<_>>(),
        vec!["/admin/api/accounts/claude/acct", "/admin/api/routes"],
        "the mutation, one poll, then the budget gives up before any retry"
    );
}

#[cfg(unix)]
#[test]
fn a_second_401_names_the_restart() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let config = home.home().join("etc").join("shunt.toml");
    let original_ino = Arc::new(AtomicU64::new(0));
    let (base, seen) = serve_endpoints_raw(4, {
        let config = config.clone();
        let original_ino = Arc::clone(&original_ino);
        move |path, _| {
            let ino = fs::metadata(&config).map(|m| m.ino()).unwrap_or(0);
            let reloaded = ino != 0 && ino != original_ino.load(Ordering::SeqCst);
            match path {
                // The read clears after the reload, but the mutation still
                // refuses the key: the retry answers 401 again.
                "/admin/api/routes" if reloaded => (200, "{}".to_string()),
                "/admin/api/routes" => (401, "unauthorized".to_string()),
                _ => (401, "unauthorized".to_string()),
            }
        }
    });
    let record = checked_admin_record(&home, listener_addr(&base), &calls);
    original_ino.store(fs::metadata(&config).expect("meta").ino(), Ordering::SeqCst);
    let err = remove_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("restart");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::RestartRequired),
        "a second 401 is the restart variant, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        "the gateway still refuses clauth's admin key; restart the gateway: a → stop shunt, then start shunt"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(
        raw.iter().map(|r| request_path(r)).collect::<Vec<_>>(),
        vec![
            "/admin/api/accounts/claude/acct",
            "/admin/api/routes",
            "/admin/api/accounts/claude/acct"
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_peer_edit_during_recovery_proceeds_to_the_poll() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let config = home.home().join("etc").join("shunt.toml");
    let (base, seen) = serve_endpoints_raw(4, move |path, i| match path {
        "/admin/api/routes" => (200, "{}".to_string()),
        _ if i == 0 => (401, "unauthorized".to_string()),
        _ => (
            200,
            r#"{"name":"acct","authorize_url":"http://example/authorize"}"#.to_string(),
        ),
    });
    let addr = listener_addr(&base);
    let mut record = admin_record_at(&home, addr);
    let stub = calls.parent().expect("stub dir");
    // The `edit` marker (tests/inline/gateway.rs's `gate()` shape) appends to
    // the ORIGINAL config mid-check, posing a peer write while clauth's check
    // runs. `write_checked` then refuses `ChangedDuringEdit`; the recovery
    // proceeds to the poll because the peer's write already re-armed shunt.
    let body = format!(
        "d=\"{}\"\nif [ -e \"$d/edit\" ]; then echo '# edited meanwhile' >> '{}'; fi\necho call >> \"{}\"\nexit 0",
        stub.display(),
        config.display(),
        calls.display()
    );
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    write(&stub.join("edit"), "");

    let outcome = add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect("added");
    assert_eq!(outcome.authorize_url, "http://example/authorize");
    assert_eq!(fs::read_to_string(&calls).expect("calls"), "call\n");
    let raw = seen.join().expect("listener");
    assert_eq!(
        raw.iter().map(|r| request_path(r)).collect::<Vec<_>>(),
        vec![
            "/admin/api/accounts/claude",
            "/admin/api/routes",
            "/admin/api/accounts/claude"
        ],
        "the mutation, one poll, then the retry — no rewrite landed, the peer's did"
    );
}

#[cfg(unix)]
#[test]
fn a_401_without_clauths_entry_names_the_missing_key_and_never_rewrites() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(1, |_, _| (401, "unauthorized".to_string()));
    let record = checked_record(&home, listener_addr(&base), &calls);
    let ino_before = fs::metadata(record.config()).expect("meta").ino();
    let err =
        add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("missing key");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(
            typed,
            ShuntAdminError::AdminKeyMissing(AdminNeed::AdminTable)
        ),
        "a missing [server.admin] table is named, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        "the shunt config has no [server.admin] table; add it from the shunt card (f  add admin table)"
    );
    assert_eq!(
        fs::metadata(record.config()).expect("meta").ino(),
        ino_before,
        "no rewrite: the config's inode is unchanged"
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap_or_default(),
        "",
        "no shunt check ran"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(raw.len(), 1, "the mutation alone, no poll");
    assert_eq!(request_path(&raw[0]), "/admin/api/accounts/claude");
}

#[cfg(unix)]
#[test]
fn a_401_with_another_write_key_but_no_clauth_entry_names_the_missing_key() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(1, |_, _| (401, "unauthorized".to_string()));
    let addr = listener_addr(&base);
    // A `[server.admin]` that holds another write key but no `id = "clauth"`:
    // the arm F7 was filed for (a hand-removed entry), distinct from a missing
    // table.
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        &format!(
            "[server]\nbind = \"{addr}\"\n\n[server.admin]\n[[server.admin.write_keys]]\nid = \"other\"\nkey = \"some-token\"\n"
        ),
    );
    let mut record = GatewayRecord::new(config).expect("adoptable");
    let stub = calls.parent().expect("stub dir");
    let body = format!("echo call >> \"{}\"\nexit 0", calls.display());
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    let ino_before = fs::metadata(record.config()).expect("meta").ino();
    let err =
        add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("missing key");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::AdminKeyMissing(AdminNeed::WriteKey)),
        "a write-key table without clauth's entry is named, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        "clauth's admin key is not in the shunt config; add it from the shunt card (f  add admin key)"
    );
    assert_eq!(
        fs::metadata(record.config()).expect("meta").ino(),
        ino_before,
        "no rewrite: the config's inode is unchanged"
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap_or_default(),
        "",
        "no shunt check ran"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(raw.len(), 1, "the mutation alone, no poll");
    assert_eq!(request_path(&raw[0]), "/admin/api/accounts/claude");
}

#[cfg(unix)]
#[test]
fn a_foreign_clauth_key_names_the_entry_as_unreadable() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(1, |_, _| (401, "unauthorized".to_string()));
    let addr = listener_addr(&base);
    // clauth's `id = "clauth"` entry points at another key file: the state
    // that makes shunt read another file and answer 401, which a restart
    // cannot fix.
    let config = home.home().join("etc").join("shunt.toml");
    write(
        &config,
        &format!(
            "[server]\nbind = \"{addr}\"\n\n[server.admin]\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"${{file:/some/other/path}}\"\n"
        ),
    );
    let mut record = GatewayRecord::new(config).expect("adoptable");
    let stub = calls.parent().expect("stub dir");
    let body = format!("echo call >> \"{}\"\nexit 0", calls.display());
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    let err = add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("unreadable");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::AdminEntryUnreadable(_)),
        "a foreign clauth key is unreadable, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        r#"clauth could not read its admin entry in the shunt config: [server.admin] already has a write key with id "clauth" holding another key; clauth adds none beside it; remove that entry, then run the edit again"#
    );
    assert!(
        !err.to_string().contains("restart"),
        "no restart advice: a restart cannot fix a foreign key"
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap_or_default(),
        "",
        "no shunt check ran"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(raw.len(), 1, "the mutation alone, no poll");
    assert_eq!(request_path(&raw[0]), "/admin/api/accounts/claude");
}

#[cfg(unix)]
#[test]
fn a_peer_edit_removing_clauths_entry_during_the_poll_names_the_missing_key() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(2, |_, _| (401, "unauthorized".to_string()));
    let addr = listener_addr(&base);
    let config = home.home().join("etc").join("shunt.toml");
    let mut record = admin_record_at(&home, addr);
    let stub = calls.parent().expect("stub dir");
    // The `edit` marker rewrites the ORIGINAL config to drop `[server.admin]`
    // entirely while `shunt check` runs, so `write_checked` refuses
    // `ChangedDuringEdit` and the poll finds clauth's entry gone.
    let body = format!(
        "d=\"{}\"\nif [ -e \"$d/edit\" ]; then printf '[server]\\nbind = \"{}\"\\n' > '{}'; fi\necho call >> \"{}\"\nexit 0",
        stub.display(),
        addr,
        config.display(),
        calls.display()
    );
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    write(&stub.join("edit"), "");

    let _budget = PollBudgetOverride::set(Duration::ZERO);
    let err =
        add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("missing key");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(
            typed,
            ShuntAdminError::AdminKeyMissing(AdminNeed::AdminTable)
        ),
        "an entry removed during the poll is the missing key, not the restart, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        "the shunt config has no [server.admin] table; add it from the shunt card (f  add admin table)"
    );
    assert_eq!(fs::read_to_string(&calls).expect("calls"), "call\n");
    let raw = seen.join().expect("listener");
    assert_eq!(
        raw.iter().map(|r| request_path(r)).collect::<Vec<_>>(),
        vec!["/admin/api/accounts/claude", "/admin/api/routes"],
        "the mutation, one poll, then the budget re-reads and names the missing key — no retry"
    );
}

#[cfg(unix)]
#[test]
fn a_poll_that_times_out_counts_as_not_yet_and_the_recovery_proceeds() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");

    // A hand-rolled listener: the mutation answers 401, the first poll is
    // accepted and then never answered (the per-poll bound fires as a
    // timeout), the second poll answers 200, and the retry answers 200.
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");

    let server = std::thread::spawn(move || {
        let mut paths = Vec::new();
        let mut held = Vec::new();
        for i in 0..4 {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut sock = loop {
                if std::time::Instant::now() > deadline {
                    panic!("stub saw {i} of 4 requests");
                }
                match listener.accept() {
                    Ok((sock, _)) => break sock,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("stub accept: {e}"),
                }
            };
            sock.set_nonblocking(false).ok();
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut req = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        req.extend_from_slice(&tmp[..n]);
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&req).into_owned();
            paths.push(request_path(&text));
            if i == 1 {
                // Stall: hold the connection open without answering, so the
                // client's response bound fires as a timeout.
                held.push(sock);
                continue;
            }
            let (status, body) = match i {
                0 => (401u16, "unauthorized".to_string()),
                2 => (200u16, "{}".to_string()),
                _ => (200u16, r#"{"name":"acct","removed":false}"#.to_string()),
            };
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = sock.shutdown(std::net::Shutdown::Write);
        }
        paths
    });

    let record = checked_admin_record(&home, addr, &calls);
    // 1 s per poll: the stalled poll still times out (it is never answered),
    // while the clearing poll keeps a margin far past the ~150 ms a CI runner
    // can oversleep the stub's 10 ms accept loop.
    let _bound = PollRequestOverride::set(Duration::from_secs(1));
    let outcome = remove_pool_account(&record, PoolFamily::Claude, &name("acct")).expect("removed");
    assert!(!outcome.removed);
    assert_eq!(fs::read_to_string(&calls).expect("calls"), "call\n");
    let paths = server.join().expect("server");
    assert_eq!(
        paths,
        vec![
            "/admin/api/accounts/claude/acct",
            "/admin/api/routes",
            "/admin/api/routes",
            "/admin/api/accounts/claude/acct"
        ],
        "the mutation, a stalled poll (timed out, counted as not-yet), a clearing poll, then the retry"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_rewrite_names_rewrite_failed() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(2, |_, _| (401, "unauthorized".to_string()));
    let mut record = admin_record_at(&home, listener_addr(&base));
    let stub = calls.parent().expect("stub dir");
    let body = format!("echo call >> \"{}\"\nexit 1", calls.display());
    record.binary = Some(crate::testutil::write_shim(stub, "shunt", &body));
    let err =
        add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("rewrite failed");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::RewriteFailed(_)),
        "a refused check is RewriteFailed, got {typed:?}"
    );
    assert_eq!(
        err.to_string(),
        "clauth could not rewrite the gateway config to reload its admin key; restart the gateway: a → stop shunt, then start shunt"
    );
    seen.join().expect("listener");
}

#[cfg(unix)]
#[test]
fn a_read_only_key_is_named_and_no_recovery_runs() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(2, |_, _| (403, "forbidden".to_string()));
    let record = checked_record(&home, listener_addr(&base), &calls);
    let err = add_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("read-only");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::ReadOnlyKey),
        "a 403 is the read-only variant, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        "clauth's gateway admin token is read-only, so it cannot change pool accounts"
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap_or_default(),
        "",
        "no shunt check ran: a read-only refusal never recovers"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(raw.len(), 1, "the mutation alone, no recovery poll");
    assert_eq!(request_path(&raw[0]), "/admin/api/accounts/claude");
}

#[cfg(unix)]
#[test]
fn a_retrys_403_is_the_read_only_key() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let config = home.home().join("etc").join("shunt.toml");
    let original_ino = Arc::new(AtomicU64::new(0));
    let (base, seen) = serve_endpoints_raw(4, {
        let config = config.clone();
        let original_ino = Arc::clone(&original_ino);
        move |path, _| {
            let ino = fs::metadata(&config).map(|m| m.ino()).unwrap_or(0);
            let reloaded = ino != 0 && ino != original_ino.load(Ordering::SeqCst);
            match path {
                // The reload lands, the read clears, and the mutation's
                // retry then meets a read-only key.
                "/admin/api/routes" if reloaded => (200, "{}".to_string()),
                "/admin/api/routes" => (401, "unauthorized".to_string()),
                _ if reloaded => (403, "forbidden".to_string()),
                _ => (401, "unauthorized".to_string()),
            }
        }
    });
    let record = checked_admin_record(&home, listener_addr(&base), &calls);
    original_ino.store(fs::metadata(&config).expect("meta").ino(), Ordering::SeqCst);
    let err =
        remove_pool_account(&record, PoolFamily::Claude, &name("acct")).expect_err("read-only");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::ReadOnlyKey),
        "a retry's 403 is the read-only variant like the first send's, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        "clauth's gateway admin token is read-only, so it cannot change pool accounts"
    );
    assert_eq!(fs::read_to_string(&calls).expect("calls"), "call\n");
    let raw = seen.join().expect("listener");
    assert_eq!(
        raw.iter().map(|r| request_path(r)).collect::<Vec<_>>(),
        vec![
            "/admin/api/accounts/claude/acct",
            "/admin/api/routes",
            "/admin/api/accounts/claude/acct"
        ],
        "the mutation, one clearing poll, then exactly one retry"
    );
}

#[cfg(unix)]
#[test]
fn the_pool_listing_never_recovers_a_401() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let calls = stub_dir(&home).join("calls");
    let (base, seen) = serve_endpoints_raw(2, |_, _| (401, "unauthorized".to_string()));
    let record = checked_record(&home, listener_addr(&base), &calls);
    let err = list_pool(&record).expect_err("unauthorized");
    let typed = err
        .downcast_ref::<ShuntAdminError>()
        .expect("a typed ShuntAdminError");
    assert!(
        matches!(typed, ShuntAdminError::Unauthorized { status: 401 }),
        "a read's 401 is the Unauthorized variant, got {typed:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        "the gateway did not accept clauth's admin token (status 401)"
    );
    assert_eq!(
        fs::read_to_string(&calls).unwrap_or_default(),
        "",
        "the card's read never writes"
    );
    let raw = seen.join().expect("listener");
    assert_eq!(raw.len(), 1, "the read alone, no recovery");
    assert_eq!(request_path(&raw[0]), "/admin/api/pool");
}

/// shunt's pool row carries the fable-scoped weekly window beside 5h and 7d
/// (`AccountSnapshot`'s `utilization_7d_oi` / `reset_7d_oi`, present since the
/// 0.48.0 floor): the listing keeps both, and an absent pair reads `None`.
#[test]
fn the_pool_listing_keeps_the_fable_weekly_window() {
    let home = HomeSandbox::new();
    write_token(&home, TOKEN);
    let body = r#"{"providers":[
  {"provider":"claude","auth":"claude_oauth","accounts":[
    {"name":"a1","available":true,"near_quota":false,"needs_relogin":false,"paused":false,"disabled":false,"utilization_5h":0.5,"reset_5h":1000,"utilization_7d":0.25,"reset_7d":2000,"utilization_7d_oi":0.125,"reset_7d_oi":3000},
    {"name":"a2","available":true,"near_quota":false,"needs_relogin":false,"paused":false,"disabled":false}
  ]}
]}"#;
    let (base, seen) = serve_endpoints_raw(1, move |_, _| (200, body.to_string()));
    let record = record_at(&home, listener_addr(&base));
    let providers = list_pool(&record).expect("listing");
    seen.join().expect("listener");
    let accounts = &providers[0].accounts;
    assert_eq!(
        (accounts[0].utilization_7d_oi, accounts[0].reset_7d_oi),
        (Some(0.125), Some(3000)),
        "the fable weekly window rides the row"
    );
    assert_eq!(
        (accounts[1].utilization_7d_oi, accounts[1].reset_7d_oi),
        (None, None),
        "an absent pair reads as no window"
    );
}
