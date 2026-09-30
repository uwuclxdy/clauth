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
    AppConfig, AppState, ConfigHandle, DivergenceChoice, save_app_state, save_profile,
};
use crate::testutil::{
    DEVICE, HomeSandbox, OTHER_TOKEN, TOKEN, body_json, call, ctx_with, peer, req, seed_device,
    stored_profile, write_feed,
};

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

/// The same context a running daemon builds: one that can see the scheduler's
/// in-memory stores.
fn ctx_with_live(
    config: ConfigHandle,
    live: crate::daemon::LiveStores,
) -> std::sync::Arc<ApiContext> {
    seed_device(DEVICE, Tier::Control, TOKEN);
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::for_tests(config, status_path, Some(live), panes::absent_probe())
}

/// The id every templated session row is driven with: the transcript stem
/// `tests/fixtures/sessions/history.jsonl` is stored under, which is also the
/// `agent_session` id the `w1N:p19` fixture pane carries.
const FIXTURE_ID: &str = "1cb26556-3532-45e1-8b39-37f0b53a8e4f";
/// The id every templated pane row is driven with: that fixture pane, shaped
/// the way the agent routes require before they ask herdr.
const FIXTURE_PANE_ID: &str = "w1N:p19";

/// The captured transcript, so `GET /sessions/{id}` has a session to page.
fn seed_history_transcript() {
    let path = crate::profile::claude_dir()
        .expect("claude dir")
        .join(format!("projects/-home-user-repos-app/{FIXTURE_ID}.jsonl"));
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the slug dir");
    std::fs::write(&path, include_bytes!("../fixtures/sessions/history.jsonl"))
        .expect("write the transcript");
}

/// A documented path as a request path: under the prefix, a `{id}` filled
/// with [`FIXTURE_PANE_ID`] on a pane row and [`FIXTURE_ID`] elsewhere.
fn concrete(path: &str) -> String {
    let id = if path.starts_with("/panes/") {
        FIXTURE_PANE_ID
    } else {
        FIXTURE_ID
    };
    format!("{API_PREFIX}{}", path.replace("{id}", id))
}

/// A row's concrete request path.
fn route_path(route: &Route) -> String {
    concrete(route.path)
}

/// A conditional GET, for the feed's 304 and `?wait` paths.
fn req_tagged(path: &str, bearer: Option<&str>, etag: &str) -> Request {
    Request {
        if_none_match: Some(etag.to_string()),
        ..req("GET", path, bearer, "")
    }
}

/// Render a stream answer the way `serve_connection` writes it, with a short
/// deadline and the published feed already on disk. The head and every frame
/// land in the returned bytes.
fn render_stream(resp: Response, deadline: std::time::Instant) -> Vec<u8> {
    let mut out = Vec::new();
    super::super::http::write_response(
        &mut out,
        resp,
        &super::super::http::Disposition::Close,
        deadline,
    )
    .expect("write the stream");
    out
}

// ------------------------------------------------- status: waiting

/// A status feed body. `generated_at` is the field the daemon moves every tick
/// whether or not anything an operator can see has changed.
fn feed(active: &str, generated_at: &str) -> String {
    format!(
        r#"{{"schema":1,"generated_at":"{generated_at}","active_profile":"{active}","pending_switch":null,"wrap_off":false,"refresh_interval_ms":120000,"profiles":[]}}"#
    )
}

/// The tag the daemon would hand out for what is on disk right now.
fn current_tag(ctx: &ApiContext) -> String {
    let resp = call(ctx, &req("GET", "/api/v1/status", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    resp.etag.clone().expect("a 200 carries an entity tag")
}

/// The point of the whole thing: a reader parked on `?wait` is answered when
/// the accounts move, not when a timer fires. Without this the tray could only
/// be as current as its poll interval, which is what made a switch take
/// seconds to appear.
#[test]
fn a_wait_returns_as_soon_as_the_feed_changes() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let tag = current_tag(&ctx);

    let path = ctx.status_path.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(&path, feed("beta", "2026-09-02T06:00:05+00:00")).expect("republish");
    });

    let started = std::time::Instant::now();
    let resp = call(
        &ctx,
        &req_tagged("/api/v1/status?wait=10", Some(TOKEN), &tag),
    );
    let elapsed = started.elapsed();
    writer.join().expect("writer thread");

    assert_eq!(resp.status, 200, "the change is answered, not timed out");
    assert_eq!(body_json(&resp)["active_profile"], "beta");
    assert_ne!(
        resp.etag.as_deref(),
        Some(tag.as_str()),
        "and carries a new tag"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "answered on the change, not on the deadline: {elapsed:?}"
    );
}

/// The other half, and the reason the tag ignores `generated_at`: the daemon
/// rewrites the feed every tick, so on a quiet system that stamp is the only
/// thing moving. Waking a reader for it would turn every long poll into a
/// one-second one.
#[test]
fn a_wait_is_not_woken_by_the_timestamp_alone() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let tag = current_tag(&ctx);

    let path = ctx.status_path.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = std::sync::Arc::clone(&done);
    let ticker = std::thread::spawn(move || {
        let mut second = 0;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            second += 1;
            let stamp = format!("2026-09-02T06:00:{second:02}+00:00");
            // The production write path, not a truncate-in-place: the daemon's
            // tick republishes through `atomic_write_600`, and a plain
            // `fs::write` racing the wait loop's reads can hand it a torn body
            // the fixture never meant to model.
            let _ = crate::profile::atomic_write_600(&path, feed("alpha", &stamp).as_bytes());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    let resp = call(
        &ctx,
        &req_tagged("/api/v1/status?wait=2", Some(TOKEN), &tag),
    );
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    ticker.join().expect("ticker thread");

    assert_eq!(resp.status, 304, "a moving timestamp is not a change");
    assert_eq!(
        resp.etag.as_deref(),
        Some(tag.as_str()),
        "and the tag stands"
    );
}

/// A conditional read with no `wait` still answers at once — the first read of
/// a session has no tag, and a client that does have one must not be made to
/// block unless it asked to.
#[test]
fn a_conditional_read_without_wait_answers_immediately() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let tag = current_tag(&ctx);

    let started = std::time::Instant::now();
    let resp = call(&ctx, &req_tagged("/api/v1/status", Some(TOKEN), &tag));
    assert_eq!(resp.status, 304);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

/// A zero-wait conditional GET is a legitimate "has it moved?" probe: the feed
/// is read BEFORE the deadline is tested, so a feed that already differs from
/// the client's tag answers 200 at once. The loop used to test its deadline
/// first, which answered 304 without ever reading the file and delayed every
/// non-zero wait's first read by one poll interval.
#[test]
fn a_zero_wait_conditional_read_reads_before_deciding() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let held = current_tag(&ctx);

    // The feed moves; the reader holding the old tag must see it now, not on a
    // hypothetical next poll.
    write_feed(&ctx, &feed("beta", "2026-09-02T06:00:05+00:00"));
    let resp = call(
        &ctx,
        &req_tagged("/api/v1/status?wait=0", Some(TOKEN), &held),
    );
    assert_eq!(
        resp.status, 200,
        "a differing feed is a change, even at wait=0"
    );
    assert_eq!(body_json(&resp)["active_profile"], "beta");

    // A reader already current is told nothing changed.
    let now_tag = resp.etag.clone().expect("a 200 carries an entity tag");
    let resp = call(
        &ctx,
        &req_tagged("/api/v1/status?wait=0", Some(TOKEN), &now_tag),
    );
    assert_eq!(resp.status, 304);
}

/// The non-zero half of the same claim: a wait with a positive timeout serves
/// the FIRST read's freshness rather than blocking. The bound is one poll
/// interval, which is what separates the two failing shapes from the live
/// one: a loop that parks the caller for the whole wait answers at `wait`,
/// and one that delays its first read until after the first poll answers at
/// exactly [`WAIT_POLL`] — a sleep never undershoots — while the live loop
/// reads before its deadline test and answers in the time of one file read.
/// The zero-wait test above pins the same read-before-decide order at
/// `wait=0`, where the delayed shape answers 304 without ever reading.
#[test]
fn a_positive_wait_serves_the_first_reads_freshness_rather_than_parking() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let held = current_tag(&ctx);

    // The feed has already moved by the time the reader arrives.
    write_feed(&ctx, &feed("beta", "2026-09-02T06:00:05+00:00"));

    let started = std::time::Instant::now();
    let resp = call(
        &ctx,
        &req_tagged("/api/v1/status?wait=10", Some(TOKEN), &held),
    );
    let elapsed = started.elapsed();

    assert_eq!(resp.status, 200);
    assert_eq!(
        body_json(&resp)["active_profile"],
        serde_json::json!("beta")
    );
    assert!(
        elapsed < WAIT_POLL,
        "the first read is served before one poll interval could delay it, not \
         parked for the 10s wait: {elapsed:?}"
    );
}

/// A feed caught mid-replacement — a body that does not parse — is not a
/// change. The wait keeps going rather than answering the truncated bytes with
/// a tag fabricated from them, which is what the loop's own comment promises;
/// digesting raw bytes instead made a torn read answer 200 with the garbage.
#[test]
fn a_wait_skips_a_feed_that_does_not_parse() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let tag = current_tag(&ctx);

    // Half a feed: what a non-atomic writer leaves readable mid-write.
    write_feed(
        &ctx,
        r#"{"schema":1,"generated_at":"2026-09-02T06:00:00+00:00","active_prof"#,
    );

    let resp = call(
        &ctx,
        &req_tagged("/api/v1/status?wait=1", Some(TOKEN), &tag),
    );
    assert_eq!(resp.status, 304, "an unparseable body is not a change");
    assert!(
        resp.body.is_empty(),
        "the truncated bytes are never handed to the client"
    );
    assert_eq!(resp.etag.as_deref(), Some(tag.as_str()));
}

/// The plain conditional GET holds the same guard: a feed caught mid-replacement
/// is rebuilt from config rather than answered with the truncated bytes, so no
/// path serves a torn body. The 200 below is a REBUILT body (the seeded config's
/// `alpha` account), never the half-written file.
#[test]
fn a_plain_get_rebuilds_rather_than_serving_a_torn_feed() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let tag = current_tag(&ctx);

    // Half a feed: what a non-atomic writer leaves readable mid-write.
    write_feed(
        &ctx,
        r#"{"schema":1,"generated_at":"2026-09-02T06:00:00+00:00","active_prof"#,
    );

    let resp = call(&ctx, &req_tagged("/api/v1/status", Some(TOKEN), &tag));
    assert_eq!(resp.status, 200, "a torn feed is rebuilt, not skipped");
    // Parsed BEFORE the field asserts, so the pin's red under a regression is
    // the assertion on the answer, not a harness expect unwinding on garbage.
    let body: serde_json::Value =
        serde_json::from_slice(&resp.body).expect("the rebuilt body is whole, parseable JSON");
    assert_eq!(
        body["active_profile"], "alpha",
        "the answer is the rebuilt body, never the truncated file bytes"
    );
    assert_eq!(
        body["schema"],
        serde_json::json!(crate::daemon::SCHEMA_VERSION)
    );
}

// ---------------------------------------------------------------- auth

/// The table as shipped, row for row. A route's access is a security decision,
/// so a change to any row has to change this pin on purpose.
#[test]
fn the_route_table_is_exactly_this() {
    let rows: Vec<(&str, &str, Access)> = ROUTES
        .iter()
        .map(|route| (route.method, route.path, route.access))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("GET", "/health", Access::View),
            ("HEAD", "/health", Access::View),
            ("GET", "/status", Access::View),
            ("HEAD", "/status", Access::View),
            ("GET", "/events", Access::View),
            ("HEAD", "/events", Access::View),
            ("GET", "/openapi.json", Access::View),
            ("HEAD", "/openapi.json", Access::View),
            ("POST", "/switch", Access::Control),
            ("POST", "/chain/order", Access::Control),
            ("POST", "/chain/threshold", Access::Control),
            ("POST", "/chain/wrap-off", Access::Control),
            ("POST", "/pair", Access::None),
            ("GET", "/panes", Access::View),
            ("HEAD", "/panes", Access::View),
            ("GET", "/sessions", Access::View),
            ("HEAD", "/sessions", Access::View),
            ("POST", "/sessions", Access::Control),
            ("GET", "/sessions/{id}", Access::View),
            ("HEAD", "/sessions/{id}", Access::View),
            ("POST", "/panes/{id}/prompt", Access::Control),
            ("POST", "/panes/{id}/keys", Access::Control),
            ("GET", "/gateway", Access::View),
            ("HEAD", "/gateway", Access::View),
            ("GET", "/proxies", Access::View),
            ("HEAD", "/proxies", Access::View),
        ]
    );
}

/// The one path-parameter spelling: a `{id}` segment binds any non-empty
/// request segment, percent-decoded once, and nothing else; every other
/// segment matches exactly. A `%` not followed by two hex digits, or bytes
/// that are not UTF-8, match nothing; a `+` stays a `+`.
#[test]
fn the_template_matcher_binds_one_segment_decoded_and_nothing_else() {
    assert_eq!(
        match_template("/sessions/{id}", "/sessions/abc"),
        Some(Some("abc".to_string()))
    );
    assert_eq!(
        match_template("/panes/{id}/prompt", "/panes/w1N:p19/prompt"),
        Some(Some("w1N:p19".to_string()))
    );
    assert_eq!(
        match_template("/panes/{id}/prompt", "/panes/w1N%3Ap19/prompt"),
        Some(Some("w1N:p19".to_string()))
    );
    assert_eq!(
        match_template("/panes/{id}/prompt", "/panes/w1N%3ap19/prompt"),
        Some(Some("w1N:p19".to_string()))
    );
    assert_eq!(
        match_template("/sessions/{id}", "/sessions/a%2Fb%2E%2e+c%C3%A9"),
        Some(Some("a/b..+c\u{e9}".to_string()))
    );
    // Decoded once: a double-encoded `%25XX` yields the `%XX` text, never a
    // second pass over it.
    assert_eq!(
        match_template("/sessions/{id}", "/sessions/%2525"),
        Some(Some("%25".to_string()))
    );
    assert_eq!(
        match_template("/panes/{id}/prompt", "/panes/w1N%253Ap19/prompt"),
        Some(Some("w1N%3Ap19".to_string()))
    );
    assert_eq!(
        match_template("/panes/{id}/prompt", "/panes/../prompt"),
        Some(Some("..".to_string()))
    );
    assert_eq!(match_template("/sessions", "/sessions"), Some(None));
    for (template, path) in [
        ("/sessions/{id}", "/sessions/"),
        ("/sessions/{id}", "/sessions"),
        ("/sessions/{id}", "/sessions/a/b"),
        ("/sessions/{id}", "/sessions//"),
        ("/sessions/{id}", "/sessions/%"),
        ("/sessions/{id}", "/sessions/%4"),
        ("/sessions/{id}", "/sessions/%zz"),
        ("/sessions/{id}", "/sessions/%+1"),
        ("/sessions/{id}", "/sessions/%-1"),
        ("/sessions/{id}", "/sessions/%FF"),
        ("/sessions/{id}", "/sessions/a%C3"),
        ("/panes/{id}/prompt", "/panes/w1/keys"),
        ("/panes/{id}/prompt", "/panes//prompt"),
        ("/panes/{id}/prompt", "/panes/w1/prompt/"),
        ("/sessions", "/sessions/"),
        ("/sessions", "/sessions/x"),
    ] {
        assert_eq!(match_template(template, path), None, "{template} vs {path}");
    }
}

// -------------------------------------------------------------- openapi

/// The operation the document emits for one table row.
fn doc_operation<'a>(
    doc: &'a utoipa::openapi::OpenApi,
    method: &str,
    path: &str,
) -> Option<&'a utoipa::openapi::path::Operation> {
    let method = match method {
        "GET" => utoipa::openapi::path::HttpMethod::Get,
        "HEAD" => utoipa::openapi::path::HttpMethod::Head,
        "POST" => utoipa::openapi::path::HttpMethod::Post,
        _ => return None,
    };
    doc.paths
        .get_path_operation(format!("{API_PREFIX}{path}"), method)
}

/// Every operation the document carries, as `(method, path under API_PREFIX)`.
fn all_operations(
    doc: &utoipa::openapi::OpenApi,
) -> Vec<(&str, &str, &utoipa::openapi::path::Operation)> {
    let mut operations = Vec::new();
    for (path, item) in &doc.paths.paths {
        let Some(rel) = path.strip_prefix(API_PREFIX) else {
            panic!("documented path outside API_PREFIX: {path}");
        };
        for (method, op) in [
            ("GET", item.get.as_ref()),
            ("HEAD", item.head.as_ref()),
            ("POST", item.post.as_ref()),
            ("PUT", item.put.as_ref()),
            ("DELETE", item.delete.as_ref()),
            ("OPTIONS", item.options.as_ref()),
            ("PATCH", item.patch.as_ref()),
            ("TRACE", item.trace.as_ref()),
        ] {
            if let Some(op) = op {
                operations.push((method, rel, op));
            }
        }
    }
    operations
}

/// The document's operations, one per row, sorted.
fn document_rows(doc: &utoipa::openapi::OpenApi) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = all_operations(doc)
        .into_iter()
        .map(|(method, rel, _)| (method.to_string(), rel.to_string()))
        .collect();
    rows.sort();
    rows
}

/// One operation's EFFECTIVE security must state its row's access: `view` and
/// `control` ride the `bearer` scheme's roles, and `Access::None` carries none
/// at all. An operation with no `security` of its own inherits the document's
/// top-level requirement, so the test compares the requirement a client
/// actually faces, not the annotation alone.
fn assert_doc_security(
    operation: &utoipa::openapi::path::Operation,
    route: &Route,
    top_level: Option<&[utoipa::openapi::security::SecurityRequirement]>,
) {
    let effective = operation
        .security
        .as_deref()
        .unwrap_or_else(|| top_level.unwrap_or(&[]));
    match route.access {
        Access::View | Access::Control => {
            let role = if route.access == Access::View {
                "view"
            } else {
                "control"
            };
            assert_eq!(effective.len(), 1, "{} {}", route.method, route.path);
            let expected = serde_json::to_value(
                utoipa::openapi::security::SecurityRequirement::new("bearer", [role]),
            )
            .expect("security requirement serializes");
            assert_eq!(
                serde_json::to_value(&effective[0]).expect("security requirement serializes"),
                expected,
                "{} {} must require bearer {role}",
                route.method,
                route.path
            );
        }
        Access::None => assert!(
            effective.is_empty(),
            "{} {} must carry no security requirement",
            route.method,
            route.path
        ),
    }
}

/// The document and [`ROUTES`] name the same set in both directions, and each
/// operation's security states its row's access. An unannotated row, or an
/// annotation whose role disagrees with its row, reds this.
#[test]
fn the_openapi_document_matches_the_route_table_both_ways() {
    let doc = <ApiDoc as utoipa::OpenApi>::openapi();

    for route in ROUTES {
        // A HEAD row is documented by its GET operation: RFC 9110 §9.3 serves
        // the GET answer minus the body, so one `#[utoipa::path(get)]` covers
        // both table rows.
        let method = if route.method == "HEAD" {
            "GET"
        } else {
            route.method
        };
        let operation = doc_operation(&doc, method, route.path).unwrap_or_else(|| {
            panic!(
                "{} {} is missing from the OpenAPI document",
                route.method, route.path
            )
        });
        assert_doc_security(operation, route, doc.security.as_deref());
    }

    // The document names each GET operation once; its HEAD rows are those same
    // operations minus the body, so the reverse direction compares the
    // non-HEAD rows.
    let mut table_rows: Vec<(String, String)> = ROUTES
        .iter()
        .filter(|route| route.method != "HEAD")
        .map(|route| (route.method.to_string(), route.path.to_string()))
        .collect();
    table_rows.sort();
    assert_eq!(
        document_rows(&doc),
        table_rows,
        "every documented operation must be a route"
    );
}

/// The auth-layer answers one access implies: each status code and the error
/// code(s) its description must name, as [`handle`] produces them.
fn auth_answers(access: Access) -> Vec<(u16, Vec<&'static str>)> {
    match access {
        Access::None => Vec::new(),
        Access::View => vec![
            (401, vec!["unauthorized"]),
            (403, vec!["device_tier_unknown"]),
            (500, vec!["internal"]),
        ],
        Access::Control => vec![
            (401, vec!["unauthorized"]),
            (403, vec!["device_tier_unknown", "control_required"]),
            (500, vec!["internal"]),
        ],
    }
}

/// Every operation documents the auth answers its row's access implies, each
/// as an `ErrorBody` by `$ref`, with the 403 naming every refusal code the
/// router sends for that access. The expected set derives from
/// `ROUTES[].access`, the field the router decides on, so a new access level
/// cannot ship an undocumented answer.
#[test]
fn every_operation_documents_the_auth_answers_its_access_implies() {
    let doc = <ApiDoc as utoipa::OpenApi>::openapi();

    for route in ROUTES {
        let method = if route.method == "HEAD" {
            "GET"
        } else {
            route.method
        };
        let operation = doc_operation(&doc, method, route.path).unwrap_or_else(|| {
            panic!(
                "{} {} is missing from the OpenAPI document",
                route.method, route.path
            )
        });

        if route.access == Access::None {
            assert!(
                !operation.responses.responses.contains_key("401"),
                "{} {} must gain no auth 401",
                route.method,
                route.path
            );
        }

        for (status, codes) in auth_answers(route.access) {
            let response = operation
                .responses
                .responses
                .get(&status.to_string())
                .unwrap_or_else(|| {
                    panic!("{} {} must document {status}", route.method, route.path)
                });
            let utoipa::openapi::RefOr::T(response) = response else {
                panic!(
                    "{} {} status {status} must be an inline response",
                    route.method, route.path
                );
            };
            let content = response.content.get("application/json").unwrap_or_else(|| {
                panic!(
                    "{} {} status {status} must be json",
                    route.method, route.path
                )
            });
            let Some(utoipa::openapi::RefOr::Ref(schema)) = content.schema.as_ref() else {
                panic!(
                    "{} {} status {status} must reference ErrorBody",
                    route.method, route.path
                );
            };
            assert_eq!(
                schema.ref_location, "#/components/schemas/ErrorBody",
                "{} {} status {status}",
                route.method, route.path
            );
            for code in codes {
                assert!(
                    response.description.contains(code),
                    "{} {} status {status} must name {code}",
                    route.method,
                    route.path
                );
            }
        }
    }
}

/// OpenAPI 3.1 requires every `operationId` to be unique, and both client
/// generators key operations on it.
#[test]
fn every_operation_id_is_unique() {
    let doc = <ApiDoc as utoipa::OpenApi>::openapi();
    let mut seen: std::collections::BTreeMap<&str, (&str, &str)> =
        std::collections::BTreeMap::new();
    for (method, path, operation) in all_operations(&doc) {
        let Some(id) = operation.operation_id.as_deref() else {
            panic!("{method} {path} must name an operationId");
        };
        if let Some((first_method, first_path)) = seen.insert(id, (method, path)) {
            panic!(
                "operationId {id} is duplicated by {first_method} {first_path} and {method} {path}"
            );
        }
    }
}

/// One function is the whole source: two calls return byte-identical documents.
#[test]
fn the_openapi_document_is_identical_across_calls() {
    assert_eq!(
        openapi_document_bytes().expect("document serializes"),
        openapi_document_bytes().expect("document serializes")
    );
}

/// The route serves exactly the one document's bytes, 200.
#[test]
fn the_openapi_route_serves_the_documents_bytes() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let resp = call(&ctx, &req("GET", "/api/v1/openapi.json", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body,
        openapi_document_bytes().expect("document serializes")
    );
}

/// Item 4: the document names the `bearer` http scheme, the status query
/// parameters and its 304, and every error answer is the `ErrorBody` schema.
#[test]
fn the_document_names_the_bearer_scheme_queries_304_and_errors() {
    let doc = <ApiDoc as utoipa::OpenApi>::openapi();

    let components = doc
        .components
        .as_ref()
        .expect("the document has components");
    let scheme = components
        .security_schemes
        .get("bearer")
        .expect("the bearer scheme exists");
    match scheme {
        utoipa::openapi::security::SecurityScheme::Http(http) => {
            assert!(http.scheme == utoipa::openapi::security::HttpAuthScheme::Bearer);
            assert_eq!(
                http.description.as_deref(),
                Some("Authorization: Bearer <device token>")
            );
        }
        _ => panic!("bearer must be an http scheme"),
    }

    let status = doc_operation(&doc, "GET", "/status").expect("status operation");
    let params = status
        .parameters
        .as_ref()
        .expect("status names its parameters");
    assert_eq!(params.len(), 3);
    assert_eq!(params[0].name, "all");
    assert_eq!(params[1].name, "wait");
    assert_eq!(params[2].name, "If-None-Match");
    assert!(
        params[..2]
            .iter()
            .all(|param| param.parameter_in == utoipa::openapi::path::ParameterIn::Query),
        "all and wait are query parameters"
    );
    assert!(
        params[2].parameter_in == utoipa::openapi::path::ParameterIn::Header,
        "If-None-Match is a header parameter"
    );
    assert!(
        status.responses.responses.contains_key("304"),
        "status documents its 304"
    );

    for (method, path, operation) in all_operations(&doc) {
        for (status, response) in &operation.responses.responses {
            let Ok(code) = status.parse::<u16>() else {
                continue;
            };
            if code < 400 {
                continue;
            }
            let utoipa::openapi::RefOr::T(response) = response else {
                panic!("{method} {path} status {status} must be an inline error");
            };
            let content = response
                .content
                .get("application/json")
                .unwrap_or_else(|| panic!("{method} {path} status {status} must be json"));
            let Some(utoipa::openapi::RefOr::Ref(schema)) = content.schema.as_ref() else {
                panic!("{method} {path} status {status} must reference ErrorBody");
            };
            assert_eq!(
                schema.ref_location, "#/components/schemas/ErrorBody",
                "{method} {path} status {status}"
            );
        }
    }
}

/// Run `req` through the router and walk the answer's body against the schema
/// the document names for `(method, path, status)`; a documented answer with no
/// body must have arrived empty. The driven `(method, path, status)` is
/// recorded, as is the `error` code of every `ErrorBody` answer keyed to that
/// (operation, status), and for an `ErrorBody` answer its `error` code must
/// appear inside backticks in the description the document gives that answer —
/// so a produced answer the document misdescribes fails here by derivation,
/// not by a copied string.
fn check_answer(
    doc: &utoipa::openapi::OpenApi,
    method: &str,
    path: &str,
    status: u16,
    resp: &Response,
    driven: &mut std::collections::HashSet<(String, String, u16)>,
    produced: &mut std::collections::HashSet<(String, String, u16, String)>,
) {
    assert_eq!(resp.status, status, "{method} {path}");
    driven.insert((method.to_string(), path.to_string(), status));
    let operation = doc_operation(doc, method, path)
        .unwrap_or_else(|| panic!("{method} {path} is missing from the document"));
    let response = operation
        .responses
        .responses
        .get(&status.to_string())
        .unwrap_or_else(|| panic!("{method} {path} must document {status}"));
    let utoipa::openapi::RefOr::T(response) = response else {
        panic!("{method} {path} {status} must be an inline response");
    };
    let Some(content) = response.content.get("application/json") else {
        assert!(
            resp.body.is_empty(),
            "{method} {path} {status} documents no body but the answer carries one"
        );
        return;
    };
    let Some(schema) = content.schema.as_ref() else {
        assert!(
            resp.body.is_empty(),
            "{method} {path} {status} documents no body but the answer carries one"
        );
        return;
    };
    let body = body_json(resp);
    crate::testutil::schema_agrees(
        &body,
        schema,
        doc.components
            .as_ref()
            .expect("the document has components"),
    );
    if let Some(code) = body.get("error").and_then(serde_json::Value::as_str) {
        produced.insert((
            method.to_string(),
            path.to_string(),
            status,
            code.to_string(),
        ));
        assert!(
            response.description.contains(&format!("`{code}`")),
            "{method} {path} {status} must name `{code}` inside backticks in its description"
        );
    }
}

/// Every backtick-quoted token in a description, in order. Error descriptions
/// name their `error` codes this way, so the reverse sweep can pin the named
/// set to exactly the produced set without a hand-typed list. A description
/// with an unmatched backtick is malformed and yields `Err`, never a truncated
/// token stream.
fn backtick_tokens(description: &str) -> Result<Vec<String>, ()> {
    let mut tokens = Vec::new();
    let mut rest = description;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            return Err(());
        };
        tokens.push(after_open[..close].to_string());
        rest = &after_open[close + 1..];
    }
    Ok(tokens)
}

/// Drive the real router through each documented success answer and every
/// documented error answer the fixtures reach in process, and walk each body
/// against the schema the document names for that operation and status. A
/// documented answer with no body (the status 304) must arrive empty, and so
/// must every HEAD answer, whose body the serve loop strips one layer above the
/// router. Every driven `ErrorBody` answer's `error` code must appear inside
/// backticks in the description the document gives that (operation, status), every code an
/// `ErrorBody` description names must be one a driven answer produced for that
/// (operation, status), and every documented (operation, status) must be
/// produced by at least one request below — so a produced answer the document
/// misdescribes, a code the document names nothing produces, and a documented
/// answer nothing produces all fail, each naming itself. The openapi document's own 200 is
/// the one free-form body: utoipa documents `serde_json::Value` as an empty
/// schema the walk refuses by design, so this test pins that contract and
/// checks the bytes instead.
#[test]
fn every_reachable_answer_matches_the_schema_the_document_names() {
    let _home = HomeSandbox::new();
    let doc = <ApiDoc as utoipa::OpenApi>::openapi();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));
    let mut driven = std::collections::HashSet::new();
    let mut produced = std::collections::HashSet::new();

    // A three-member chain plus one stored profile outside it, for the chain
    // mutation routes. Seeded before any arm so the chain routes can be driven
    // with the same fixture the rest of the test shares.
    {
        let mut cfg = config.lock().expect("config");
        let gamma = stored_profile("gamma");
        let delta = stored_profile("delta");
        cfg.profiles.push(gamma);
        cfg.profiles.push(delta);
        cfg.state.profiles.push("gamma".into());
        cfg.state.profiles.push("delta".into());
        cfg.state.fallback_chain = vec!["alpha".into(), "beta".into(), "gamma".into()];
        save_app_state(&cfg.state).expect("save chain");
    }

    // The switch_failed 500, posed the way the switch-failure route tests pose
    // it: a diverged-looking live slot and the Discard default reach the link
    // publish, and a directory at the live credentials path fails it. Driven
    // before the successful switch so the outgoing profile is still alpha.
    {
        let mut cfg = config.lock().expect("config");
        cfg.state.default_divergence = Some(DivergenceChoice::Discard);
    }
    let claude_dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(claude_dir.join(".credentials.json")).expect("pose the wedge");
    let switch_failed = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    std::fs::remove_dir_all(claude_dir.join(".credentials.json")).expect("clear the wedge");
    check_answer(
        &doc,
        "POST",
        "/switch",
        500,
        &switch_failed,
        &mut driven,
        &mut produced,
    );

    // Success answers, in an order that leaves the store and state lock free.
    let health = call(&ctx, &req("GET", "/api/v1/health", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/health",
        200,
        &health,
        &mut driven,
        &mut produced,
    );

    let panes = call(&ctx, &req("GET", "/api/v1/panes", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/panes",
        200,
        &panes,
        &mut driven,
        &mut produced,
    );

    let gateway = call(&ctx, &req("GET", "/api/v1/gateway", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/gateway",
        200,
        &gateway,
        &mut driven,
        &mut produced,
    );

    let proxies = call(&ctx, &req("GET", "/api/v1/proxies", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/proxies",
        200,
        &proxies,
        &mut driven,
        &mut produced,
    );

    // The sessions routes: the listing over the still-empty store, then the
    // captured transcript paged under the fixture id. A record is the one
    // free-form body besides the document's own: documented as a bare object
    // (pinned below), so the walk sees the envelope with each record blanked;
    // the records themselves are pinned byte for byte in
    // `daemon_api_sessions.rs`.
    let sessions = call(&ctx, &req("GET", "/api/v1/sessions", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/sessions",
        200,
        &sessions,
        &mut driven,
        &mut produced,
    );
    seed_history_transcript();
    let history = call(
        &ctx,
        &req("GET", &concrete("/sessions/{id}"), Some(TOKEN), ""),
    );
    assert_eq!(history.status, 200);
    let mut envelope = body_json(&history);
    let records = envelope["records"].as_array_mut().expect("records");
    assert!(!records.is_empty(), "the fixture transcript pages");
    for record in records.iter_mut() {
        record["record"] = serde_json::json!({});
    }
    check_answer(
        &doc,
        "GET",
        "/sessions/{id}",
        200,
        &Response::json(200, &envelope),
        &mut driven,
        &mut produced,
    );
    let record_schema = doc
        .components
        .as_ref()
        .expect("the document has components")
        .schemas
        .get("HistoryRecord")
        .expect("the record schema");
    let utoipa::openapi::RefOr::T(utoipa::openapi::Schema::Object(record_schema)) = record_schema
    else {
        panic!("HistoryRecord must be an inline object schema");
    };
    assert_eq!(
        serde_json::to_value(
            record_schema
                .properties
                .get("record")
                .expect("the record property")
        )
        .expect("schema serializes"),
        serde_json::json!({
            "type": "object",
            "description": "One Claude Code transcript record, verbatim.",
        }),
        "a transcript record must stay documented as a bare object"
    );
    let listing_bad = call(
        &ctx,
        &req("GET", "/api/v1/sessions?limit=0", Some(TOKEN), ""),
    );
    check_answer(
        &doc,
        "GET",
        "/sessions",
        400,
        &listing_bad,
        &mut driven,
        &mut produced,
    );
    let history_bad = call(
        &ctx,
        &req(
            "GET",
            &format!("{}?before=x", concrete("/sessions/{id}")),
            Some(TOKEN),
            "",
        ),
    );
    check_answer(
        &doc,
        "GET",
        "/sessions/{id}",
        400,
        &history_bad,
        &mut driven,
        &mut produced,
    );
    let history_missing = call(&ctx, &req("GET", "/api/v1/sessions/ghost", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/sessions/{id}",
        404,
        &history_missing,
        &mut driven,
        &mut produced,
    );

    // The agent routes, through a herdr table keyed on what is sent: the
    // text or first key `missing`, `blocked` and `boom` draw herdr's refusals
    // (its measured envelopes, on stderr with stdout empty as herdr prints
    // them), anything else succeeds. The plain context's absent probe is the
    // herdr-unavailable arm.
    let ctx_herdr = ApiContext::for_tests(
        std::sync::Arc::clone(&config),
        ctx.status_path.clone(),
        None,
        Box::new(|args, _deadline| {
            let sent = match args {
                ["agent", "prompt", _, text] => *text,
                ["pane", "send-keys", _, key, ..] => *key,
                _ => return panes::HerdrProbeOut::Ran(None),
            };
            let envelope = match sent {
                "missing" => {
                    r#"{"error":{"code":"agent_not_found","message":"agent target w9:p99 not found"},"id":"cli:agent:prompt"}"#
                }
                "blocked" => {
                    r#"{"error":{"code":"agent_blocked","message":"agent w9:p99 is blocked"},"id":"cli:agent:prompt"}"#
                }
                "boom" => {
                    r#"{"error":{"code":"timeout","message":"no matching state within 5000ms"},"id":"cli:agent:prompt"}"#
                }
                _ => {
                    return panes::HerdrProbeOut::Ran(Some(panes::HerdrOut {
                        success: true,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    }));
                }
            };
            panes::HerdrProbeOut::Ran(Some(panes::HerdrOut {
                success: false,
                stdout: Vec::new(),
                stderr: envelope.as_bytes().to_vec(),
            }))
        }),
    );
    for (path, body, status) in [
        ("/panes/{id}/prompt", r#"{"text":"fix the tests"}"#, 200),
        ("/panes/{id}/prompt", r#"{"text":"missing"}"#, 404),
        ("/panes/{id}/prompt", r#"{"text":"blocked"}"#, 409),
        ("/panes/{id}/prompt", r#"{"text":"boom"}"#, 502),
        ("/panes/{id}/prompt", "{}", 400),
        ("/panes/{id}/keys", r#"{"keys":["y","enter"]}"#, 200),
        ("/panes/{id}/keys", r#"{"keys":["missing"]}"#, 404),
        ("/panes/{id}/keys", r#"{"keys":["boom"]}"#, 502),
        ("/panes/{id}/keys", "{}", 400),
    ] {
        let resp = call(&ctx_herdr, &req("POST", &concrete(path), Some(TOKEN), body));
        check_answer(
            &doc,
            "POST",
            path,
            status,
            &resp,
            &mut driven,
            &mut produced,
        );
    }
    for (path, body) in [
        ("/panes/{id}/prompt", r#"{"text":"fix the tests"}"#),
        ("/panes/{id}/keys", r#"{"keys":["y","enter"]}"#),
    ] {
        let resp = call(&ctx, &req("POST", &concrete(path), Some(TOKEN), body));
        check_answer(&doc, "POST", path, 503, &resp, &mut driven, &mut produced);
    }

    // The session-creation route, through a herdr table answering tab create,
    // agent start and pane get. The config key and the grant are staged per arm.
    {
        let mut state = crate::profile::load_app_state().expect("load state");
        state.serve.session_creation = false;
        crate::profile::save_app_state(&state).expect("save session_creation off");
        crate::daemon::api::devices::allow_sessions(DEVICE).expect("grant sessions");

        let key_off = call(
            &ctx,
            &req("POST", "/api/v1/sessions", Some(TOKEN), r#"{"cwd":"/"}"#),
        );
        check_answer(
            &doc,
            "POST",
            "/sessions",
            403,
            &key_off,
            &mut driven,
            &mut produced,
        );

        state.serve.session_creation = true;
        crate::profile::save_app_state(&state).expect("save session_creation on");

        let ctx_create = ApiContext::for_tests(
            std::sync::Arc::clone(&config),
            ctx.status_path.clone(),
            None,
            Box::new(|args, _deadline| {
                let out = |success: bool, stdout: &str, stderr: &str| {
                    panes::HerdrProbeOut::Ran(Some(panes::HerdrOut {
                        success,
                        stdout: stdout.as_bytes().to_vec(),
                        stderr: stderr.as_bytes().to_vec(),
                    }))
                };
                match args {
                    [
                        "tab",
                        "create",
                        "--cwd",
                        "/",
                        "--no-focus",
                        "--workspace",
                        "w9",
                    ] => out(
                        false,
                        "",
                        r#"{"error":{"code":"workspace_not_found","message":"workspace w9:nope not found"},"id":"cli:tab:create"}"#,
                    ),
                    ["tab", "create", ..] => out(
                        true,
                        r#"{"id":"cli:tab:create","result":{"root_pane":{"pane_id":"w1:p2","tab_id":"w1:t2","workspace_id":"w1","agent_status":"unknown"},"tab":{"tab_id":"w1:t2","workspace_id":"w1"},"type":"tab_created"}}"#,
                        "",
                    ),
                    ["agent", "start", _, "--kind", "bogus", ..] => {
                        out(false, "", "unsupported interactive agent kind: bogus")
                    }
                    ["agent", "start", _, "--kind", "claude", ..] => out(
                        false,
                        "",
                        r#"{"error":{"code":"agent_not_ready","message":"agent claude blocked during startup"},"id":"cli:agent:start"}"#,
                    ),
                    ["pane", "run", "w1:p2", "clauth", "start", "alpha"] => out(
                        false,
                        "",
                        r#"{"error":{"code":"pane_not_found","message":"pane w1:p2 not found"},"id":"cli:pane:run"}"#,
                    ),
                    ["pane", "get", "w1:p2"] => out(
                        true,
                        r#"{"id":"cli:pane:get","result":{"pane":{"agent":"claude","agent_status":"blocked","cwd":"/","focused":false,"pane_id":"w1:p2","tab_id":"w1:t2","workspace_id":"w1"},"type":"pane_info"}}"#,
                        "",
                    ),
                    ["tab", "close", "w1:t2"] => out(true, "", ""),
                    _ => panes::HerdrProbeOut::Ran(None),
                }
            }),
        );

        for (body, status) in [
            (r#"{"cwd":"/"}"#, 200),
            (r#"{"cwd":"relative"}"#, 400),
            (r#"{"cwd":"/","profile":"ghost"}"#, 404),
            (r#"{"cwd":"/","profile":"alpha"}"#, 502),
            (r#"{"cwd":"/","workspace":"w9"}"#, 409),
            (r#"{"cwd":"/","kind":"bogus"}"#, 502),
        ] {
            let resp = call(
                &ctx_create,
                &req("POST", "/api/v1/sessions", Some(TOKEN), body),
            );
            check_answer(
                &doc,
                "POST",
                "/sessions",
                status,
                &resp,
                &mut driven,
                &mut produced,
            );
        }
        let absent = call(
            &ctx,
            &req("POST", "/api/v1/sessions", Some(TOKEN), r#"{"cwd":"/"}"#),
        );
        check_answer(
            &doc,
            "POST",
            "/sessions",
            503,
            &absent,
            &mut driven,
            &mut produced,
        );

        let nogrant_token = "c".repeat(64);
        seed_device("nogrunt", Tier::Control, &nogrant_token);
        let no_grant = call(
            &ctx,
            &req(
                "POST",
                "/api/v1/sessions",
                Some(&nogrant_token),
                r#"{"cwd":"/"}"#,
            ),
        );
        check_answer(
            &doc,
            "POST",
            "/sessions",
            403,
            &no_grant,
            &mut driven,
            &mut produced,
        );
    }

    let status = call(&ctx, &req("GET", "/api/v1/status", Some(TOKEN), ""));
    check_answer(
        &doc,
        "GET",
        "/status",
        200,
        &status,
        &mut driven,
        &mut produced,
    );
    let tag = status
        .etag
        .clone()
        .expect("a status 200 carries an entity tag");
    let not_modified = call(&ctx, &req_tagged("/api/v1/status", Some(TOKEN), &tag));
    check_answer(
        &doc,
        "GET",
        "/status",
        304,
        &not_modified,
        &mut driven,
        &mut produced,
    );

    // The events stream's 200 has no JSON body, so it is rendered against a
    // sink — with a feed on disk and a short deadline — and checked for its
    // head and frames rather than walked against a schema.
    write_feed(&ctx, &feed("alpha", "2026-09-02T06:00:00+00:00"));
    let events = call(&ctx, &req("GET", "/api/v1/events", Some(TOKEN), ""));
    assert_eq!(events.status, 200);
    let rendered = render_stream(
        events,
        std::time::Instant::now() + std::time::Duration::from_millis(300),
    );
    let rendered_text = String::from_utf8_lossy(&rendered);
    assert!(
        rendered_text.contains("Content-Type: text/event-stream\r\n"),
        "the stream head names its type: {rendered_text:?}"
    );
    assert!(
        rendered_text.contains("event: status\n"),
        "the stream carries the current feed: {rendered_text:?}"
    );
    driven.insert(("GET".to_string(), "/events".to_string(), 200));

    let switched = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/switch",
        200,
        &switched,
        &mut driven,
        &mut produced,
    );

    // Chain mutation success answers.
    let order_ok = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/chain/order",
            Some(TOKEN),
            r#"{"members":["BETA","alpha","gamma"]}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/order",
        200,
        &order_ok,
        &mut driven,
        &mut produced,
    );
    let threshold_ok = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/chain/threshold",
            Some(TOKEN),
            r#"{"profile":"alpha","threshold":90}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/threshold",
        200,
        &threshold_ok,
        &mut driven,
        &mut produced,
    );
    for wrap in [true, false] {
        let wrap_ok = call(
            &ctx,
            &req(
                "POST",
                "/api/v1/chain/wrap-off",
                Some(TOKEN),
                &serde_json::to_string(&serde_json::json!({"wrap_off": wrap})).expect("wrap body"),
            ),
        );
        check_answer(
            &doc,
            "POST",
            "/chain/wrap-off",
            200,
            &wrap_ok,
            &mut driven,
            &mut produced,
        );
    }

    let code = pairing::begin(
        &devices::DeviceName::parse("phone").expect("device name"),
        Tier::View,
        false,
    )
    .expect("mint a pairing code")
    .code()
    .to_string();
    let pair_body = serde_json::to_string(&serde_json::json!({"code": code})).expect("pair body");
    let paired = call(&ctx, &req("POST", "/api/v1/pair", None, &pair_body));
    check_answer(
        &doc,
        "POST",
        "/pair",
        201,
        &paired,
        &mut driven,
        &mut produced,
    );

    // The document's own 200 is `body = serde_json::Value`: utoipa renders it
    // as an empty schema, which the walk refuses by design.
    let openapi = call(&ctx, &req("GET", "/api/v1/openapi.json", Some(TOKEN), ""));
    assert_eq!(openapi.status, 200);
    assert_eq!(
        openapi.body,
        openapi_document_bytes().expect("document serializes")
    );
    let openapi_op = doc_operation(&doc, "GET", "/openapi.json").expect("openapi.json operation");
    let openapi_response = openapi_op
        .responses
        .responses
        .get("200")
        .expect("200 documented");
    let utoipa::openapi::RefOr::T(openapi_response) = openapi_response else {
        panic!("openapi.json 200 must be an inline response");
    };
    let openapi_content = openapi_response
        .content
        .get("application/json")
        .expect("json content");
    let openapi_schema = openapi_content.schema.as_ref().expect("a schema");
    assert_eq!(
        serde_json::to_value(openapi_schema).expect("schema serializes"),
        serde_json::json!({}),
        "openapi.json 200 must stay documented as the free-form serde_json::Value body"
    );
    driven.insert(("GET".to_string(), "/openapi.json".to_string(), 200));

    // A second live code, for the pair arms past the redemption.
    let code2 = pairing::begin(
        &devices::DeviceName::parse("tablet").expect("device name"),
        Tier::View,
        false,
    )
    .expect("mint a second pairing code")
    .code()
    .to_string();
    let pair_body2 = serde_json::to_string(&serde_json::json!({"code": code2})).expect("pair body");

    // The auth-layer answers: no bearer, then the two 403 arms.
    for (method, path) in [
        ("GET", "/health"),
        ("GET", "/status"),
        ("GET", "/events"),
        ("GET", "/openapi.json"),
        ("GET", "/panes"),
        ("GET", "/sessions"),
        ("GET", "/sessions/{id}"),
        ("POST", "/sessions"),
        ("POST", "/switch"),
        ("POST", "/chain/order"),
        ("POST", "/chain/threshold"),
        ("POST", "/chain/wrap-off"),
        ("POST", "/panes/{id}/prompt"),
        ("POST", "/panes/{id}/keys"),
        ("GET", "/gateway"),
        ("GET", "/proxies"),
    ] {
        let resp = call(&ctx, &req(method, &concrete(path), None, ""));
        check_answer(&doc, method, path, 401, &resp, &mut driven, &mut produced);
    }
    seed_device("viewer", Tier::View, OTHER_TOKEN);
    let control_required = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(OTHER_TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/switch",
        403,
        &control_required,
        &mut driven,
        &mut produced,
    );
    for (path, body) in [
        ("/chain/order", r#"{"members":["alpha","beta","gamma"]}"#),
        ("/chain/threshold", r#"{"profile":"alpha","threshold":90}"#),
        ("/chain/wrap-off", r#"{"wrap_off":true}"#),
        ("/panes/{id}/prompt", r#"{"text":"fix the tests"}"#),
        ("/panes/{id}/keys", r#"{"keys":["y","enter"]}"#),
        ("/sessions", r#"{"cwd":"/"}"#),
    ] {
        let resp = call(&ctx, &req("POST", &concrete(path), Some(OTHER_TOKEN), body));
        check_answer(&doc, "POST", path, 403, &resp, &mut driven, &mut produced);
    }
    let wall_token = "b".repeat(64);
    seed_device("wall", Tier::Unknown("readonly".to_string()), &wall_token);
    for (method, path) in [
        ("GET", "/health"),
        ("GET", "/status"),
        ("GET", "/events"),
        ("GET", "/openapi.json"),
        ("GET", "/panes"),
        ("GET", "/sessions"),
        ("GET", "/sessions/{id}"),
        ("POST", "/sessions"),
        ("POST", "/switch"),
        ("POST", "/chain/order"),
        ("POST", "/chain/threshold"),
        ("POST", "/chain/wrap-off"),
        ("POST", "/panes/{id}/prompt"),
        ("POST", "/panes/{id}/keys"),
        ("GET", "/gateway"),
        ("GET", "/proxies"),
    ] {
        let body = if method == "POST" {
            r#"{"profile":"beta"}"#
        } else {
            ""
        };
        let resp = call(&ctx, &req(method, &concrete(path), Some(&wall_token), body));
        check_answer(&doc, method, path, 403, &resp, &mut driven, &mut produced);
    }

    // The switch error answers the fixtures reach without extra staging.
    let bad = call(&ctx, &req("POST", "/api/v1/switch", Some(TOKEN), "{}"));
    check_answer(
        &doc,
        "POST",
        "/switch",
        400,
        &bad,
        &mut driven,
        &mut produced,
    );
    let unknown = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"ghost"}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/switch",
        404,
        &unknown,
        &mut driven,
        &mut produced,
    );

    // The pair error answers: a body with no code, and a wrong code.
    let pair_bad = call(&ctx, &req("POST", "/api/v1/pair", None, "{}"));
    check_answer(
        &doc,
        "POST",
        "/pair",
        400,
        &pair_bad,
        &mut driven,
        &mut produced,
    );
    let pair_refused = call(
        &ctx,
        &req("POST", "/api/v1/pair", None, r#"{"code":"ABCD-2345"}"#),
    );
    check_answer(
        &doc,
        "POST",
        "/pair",
        403,
        &pair_refused,
        &mut driven,
        &mut produced,
    );

    // The chain error answers: a malformed body, a non-permutation order, an
    // unknown threshold target, a non-member, and an out-of-band threshold.
    let order_bad = call(&ctx, &req("POST", "/api/v1/chain/order", Some(TOKEN), "{}"));
    check_answer(
        &doc,
        "POST",
        "/chain/order",
        400,
        &order_bad,
        &mut driven,
        &mut produced,
    );
    let order_invalid = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/chain/order",
            Some(TOKEN),
            r#"{"members":["alpha","beta"]}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/order",
        400,
        &order_invalid,
        &mut driven,
        &mut produced,
    );
    let threshold_bad = call(
        &ctx,
        &req("POST", "/api/v1/chain/threshold", Some(TOKEN), "{}"),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/threshold",
        400,
        &threshold_bad,
        &mut driven,
        &mut produced,
    );
    let threshold_range = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/chain/threshold",
            Some(TOKEN),
            r#"{"profile":"alpha","threshold":1e309}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/threshold",
        400,
        &threshold_range,
        &mut driven,
        &mut produced,
    );
    let threshold_unknown = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/chain/threshold",
            Some(TOKEN),
            r#"{"profile":"ghost","threshold":90}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/threshold",
        404,
        &threshold_unknown,
        &mut driven,
        &mut produced,
    );
    let threshold_not_member = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/chain/threshold",
            Some(TOKEN),
            r#"{"profile":"delta","threshold":90}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/threshold",
        409,
        &threshold_not_member,
        &mut driven,
        &mut produced,
    );
    let wrap_bad = call(
        &ctx,
        &req("POST", "/api/v1/chain/wrap-off", Some(TOKEN), "{}"),
    );
    check_answer(
        &doc,
        "POST",
        "/chain/wrap-off",
        400,
        &wrap_bad,
        &mut driven,
        &mut produced,
    );

    // HEAD routes like GET at the router and arrive bodyless on the wire, the
    // serve loop's strip the router never sees.
    for path in [
        "/health",
        "/status",
        "/events",
        "/openapi.json",
        "/sessions",
        "/sessions/{id}",
    ] {
        let resp = call(&ctx, &req("HEAD", &concrete(path), Some(TOKEN), ""));
        assert_eq!(resp.status, 200, "HEAD {path}");
        let head = resp.into_head();
        assert!(head.body.is_empty(), "HEAD {path} must arrive with no body");
    }

    // A held state flock is the one retryable refusal, for both routes that
    // take it. Seeded before the wedge, like the existing 503 pin.
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(100)));
    let switch_locked = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/switch",
        503,
        &switch_locked,
        &mut driven,
        &mut produced,
    );
    for (path, body) in [
        ("/chain/order", r#"{"members":["alpha","beta","gamma"]}"#),
        ("/chain/threshold", r#"{"profile":"alpha","threshold":90}"#),
        ("/chain/wrap-off", r#"{"wrap_off":true}"#),
    ] {
        let resp = call(
            &ctx,
            &req("POST", &format!("{API_PREFIX}{path}"), Some(TOKEN), body),
        );
        check_answer(&doc, "POST", path, 503, &resp, &mut driven, &mut produced);
    }
    let pair_locked = call(&ctx, &req("POST", "/api/v1/pair", None, &pair_body2));
    check_answer(
        &doc,
        "POST",
        "/pair",
        503,
        &pair_locked,
        &mut driven,
        &mut produced,
    );
    crate::lock::set_state_lock_timeout_override(None);
    drop(holder);

    // A second switch while one is in flight answers 409 switch_in_progress,
    // the gate held the way a_second_concurrent_switch_is_refused_immediately
    // holds it, so that description arm is driven rather than only documented.
    // The gate is released before the answer is walked: a failed assertion while
    // the holder still waited would deadlock the scope's join.
    let in_flight = {
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::sync::Arc::clone(&ctx);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _gate = holder.switch_gate.lock().expect("gate");
                held_tx.send(()).expect("signal held");
                let _ = release_rx.recv();
            });
            held_rx.recv().expect("gate taken");
            let in_flight = call(
                &ctx,
                &req(
                    "POST",
                    "/api/v1/switch",
                    Some(TOKEN),
                    r#"{"profile":"beta"}"#,
                ),
            );
            let _ = release_tx.send(());
            in_flight
        })
    };
    check_answer(
        &doc,
        "POST",
        "/switch",
        409,
        &in_flight,
        &mut driven,
        &mut produced,
    );

    // The same gate held for the chain routes answers their own 409 code.
    let chain_in_flight = {
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::sync::Arc::clone(&ctx);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _gate = holder.switch_gate.lock().expect("gate");
                held_tx.send(()).expect("signal held");
                let _ = release_rx.recv();
            });
            held_rx.recv().expect("gate taken");
            let mut answers = Vec::new();
            for (path, body) in [
                ("/chain/order", r#"{"members":["alpha","beta","gamma"]}"#),
                ("/chain/threshold", r#"{"profile":"alpha","threshold":90}"#),
                ("/chain/wrap-off", r#"{"wrap_off":true}"#),
            ] {
                answers.push((
                    path,
                    call(
                        &ctx,
                        &req("POST", &format!("{API_PREFIX}{path}"), Some(TOKEN), body),
                    ),
                ));
            }
            let _ = release_tx.send(());
            answers
        })
    };
    for (path, resp) in chain_in_flight {
        check_answer(&doc, "POST", path, 409, &resp, &mut driven, &mut produced);
    }

    // A refused switch (a disabled target) answers 409.
    {
        let mut cfg = config.lock().expect("config");
        let beta = cfg
            .find_mut(&crate::profile::ProfileName::from("beta"))
            .expect("beta");
        beta.disabled = true;
        save_profile(beta).expect("save");
    }
    let refused = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    check_answer(
        &doc,
        "POST",
        "/switch",
        409,
        &refused,
        &mut driven,
        &mut produced,
    );

    // A chain edit that fails on disk answers the route's own 500 edit_failed,
    // distinct from the router's device-list `internal` driven below. order and
    // wrap-off save whole state into ~/.clauth; threshold writes the member dir.
    {
        use std::os::unix::fs::PermissionsExt;
        let clauth_dir = crate::profile::clauth_dir().expect("clauth dir");
        std::fs::set_permissions(&clauth_dir, std::fs::Permissions::from_mode(0o500))
            .expect("chmod clauth dir read-only");
        let answers: Vec<(&str, Response)> = [
            ("/chain/order", r#"{"members":["beta","alpha","gamma"]}"#),
            ("/chain/wrap-off", r#"{"wrap_off":true}"#),
        ]
        .into_iter()
        .map(|(path, body)| {
            (
                path,
                call(
                    &ctx,
                    &req("POST", &format!("{API_PREFIX}{path}"), Some(TOKEN), body),
                ),
            )
        })
        .collect();
        // Restore before any assertion so a red still lets the sandbox clean up.
        std::fs::set_permissions(&clauth_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore clauth dir perms");
        for (path, resp) in answers {
            check_answer(&doc, "POST", path, 500, &resp, &mut driven, &mut produced);
        }

        let beta_dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("beta"))
            .expect("beta dir");
        std::fs::set_permissions(&beta_dir, std::fs::Permissions::from_mode(0o500))
            .expect("chmod beta dir read-only");
        let resp = call(
            &ctx,
            &req(
                "POST",
                "/api/v1/chain/threshold",
                Some(TOKEN),
                r#"{"profile":"beta","threshold":90}"#,
            ),
        );
        std::fs::set_permissions(&beta_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore beta dir perms");
        check_answer(
            &doc,
            "POST",
            "/chain/threshold",
            500,
            &resp,
            &mut driven,
            &mut produced,
        );
    }

    // An unreadable device list refuses every authenticated route with 500, and
    // the pairing redemption hits the same store when it mints the device.
    std::fs::write(
        crate::profile::clauth_dir()
            .expect("dir")
            .join("devices.json"),
        b"{ not json",
    )
    .expect("damage the device list");
    for (method, path) in [
        ("GET", "/health"),
        ("GET", "/status"),
        ("GET", "/events"),
        ("GET", "/openapi.json"),
        ("GET", "/panes"),
        ("GET", "/sessions"),
        ("GET", "/sessions/{id}"),
        ("POST", "/sessions"),
        ("POST", "/switch"),
        ("POST", "/chain/order"),
        ("POST", "/chain/threshold"),
        ("POST", "/chain/wrap-off"),
        ("POST", "/panes/{id}/prompt"),
        ("POST", "/panes/{id}/keys"),
        ("GET", "/gateway"),
        ("GET", "/proxies"),
    ] {
        let body = if method == "POST" {
            r#"{"profile":"beta"}"#
        } else {
            ""
        };
        let resp = call(&ctx, &req(method, &concrete(path), Some(TOKEN), body));
        check_answer(&doc, method, path, 500, &resp, &mut driven, &mut produced);
    }
    let pair_failed = call(&ctx, &req("POST", "/api/v1/pair", None, &pair_body2));
    check_answer(
        &doc,
        "POST",
        "/pair",
        500,
        &pair_failed,
        &mut driven,
        &mut produced,
    );

    // Every documented (operation, status) must have been produced by a request
    // above, so a documented answer nothing produces fails the test naming
    // itself.
    for (method, path, operation) in all_operations(&doc) {
        for status in operation.responses.responses.keys() {
            let Ok(code) = status.parse::<u16>() else {
                continue;
            };
            assert!(
                driven.contains(&(method.to_string(), path.to_string(), code)),
                "{method} {path} {code} is documented but no request above produced it"
            );
        }
    }

    // Every code an `ErrorBody` description names must be one a driven answer
    // produced for that (operation, status), so a ghost or misspelled code in a
    // description fails here, naming itself.
    for (method, path, operation) in all_operations(&doc) {
        for (status, response) in &operation.responses.responses {
            let Ok(code) = status.parse::<u16>() else {
                continue;
            };
            if code < 400 {
                continue;
            }
            let utoipa::openapi::RefOr::T(response) = response else {
                continue;
            };
            let Some(content) = response.content.get("application/json") else {
                continue;
            };
            let Some(utoipa::openapi::RefOr::Ref(schema)) = content.schema.as_ref() else {
                continue;
            };
            if schema.ref_location != "#/components/schemas/ErrorBody" {
                continue;
            }
            let tokens = backtick_tokens(&response.description).unwrap_or_else(|()| {
                panic!(
                    "{method} {path} {code} has an unmatched backtick in its description: {:?}",
                    response.description
                )
            });
            for token in tokens {
                let produced_key = (method.to_string(), path.to_string(), code, token.clone());
                assert!(
                    produced.contains(&produced_key),
                    "{method} {path} {code} backticks `{token}`: every backticked token in an ErrorBody description is a code claim, but no request above produced `{token}`; unbacktick the prose or make `{token}` a code the router produces"
                );
            }
        }
    }
}

/// AU-1: every route but the pairing redemption refuses an unpaired caller and
/// challenges it, and a path in no row is refused the same way, so an unpaired
/// caller learns nothing about the table. The rows are picked by identity
/// rather than by access, so a row that stops asking for a bearer fails here
/// instead of dropping out of the loop.
#[test]
fn every_route_but_pair_refuses_an_unpaired_caller() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let unpaired = "f".repeat(64);

    for route in ROUTES
        .iter()
        .filter(|route| (route.method, route.path) != ("POST", "/pair"))
    {
        for bearer in [None, Some(unpaired.as_str())] {
            let resp = call(&ctx, &req(route.method, &route_path(route), bearer, ""));
            assert_eq!(resp.status, 401, "{} {}", route.method, route.path);
            assert!(
                resp.challenge,
                "{} {} must challenge",
                route.method, route.path
            );
        }
    }
    for path in [
        "/api/v1/nope",
        "/api/v1/health/",
        "/api/v1//health",
        "/health",
        "/",
    ] {
        assert_eq!(
            call(&ctx, &req("GET", path, None, "")).status,
            401,
            "{path}"
        );
    }
}

/// The pairing redemption is the one route an unpaired caller reaches, and the
/// one row that reads no bearer; the same path under another method still
/// authenticates first.
#[test]
fn pair_is_the_one_route_an_unpaired_caller_reaches() {
    let open: Vec<(&str, &str)> = ROUTES
        .iter()
        .filter(|route| route.access == Access::None)
        .map(|route| (route.method, route.path))
        .collect();
    assert_eq!(open, vec![("POST", "/pair")]);

    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let resp = call(
        &ctx,
        &req("POST", "/api/v1/pair", None, r#"{"code":"ABCD-2345"}"#),
    );
    assert_eq!(resp.status, 403, "no code is live, and the route says so");
    assert_eq!(
        body_json(&resp)["error"],
        serde_json::json!("pairing_refused")
    );
    assert_eq!(
        call(&ctx, &req("GET", "/api/v1/pair", None, "")).status,
        401
    );
    assert_eq!(
        call(&ctx, &req("GET", "/api/v1/pair", Some(TOKEN), "")).status,
        405
    );
}

/// CT-1: each control route answers 403 to a view device. Derived from the
/// table, so a new mutating row is covered by being added; the refused switch
/// moved nothing, and the control device passes the same route.
#[test]
fn a_view_device_is_refused_every_control_route() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));
    seed_device("phone", Tier::View, OTHER_TOKEN);

    let control: Vec<&Route> = ROUTES
        .iter()
        .filter(|route| route.access == Access::Control)
        .collect();
    assert!(
        !control.is_empty(),
        "the table must hold a mutating route for this to guard"
    );
    for route in control {
        let resp = call(
            &ctx,
            &req(
                route.method,
                &route_path(route),
                Some(OTHER_TOKEN),
                r#"{"profile":"beta"}"#,
            ),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                403,
                serde_json::json!({
                    "ok": false,
                    "error": "control_required",
                    "reason": CONTROL_REQUIRED,
                })
            ),
            "{} {}",
            route.method,
            route.path
        );
    }
    assert_eq!(
        config
            .lock()
            .expect("config")
            .state
            .active_profile
            .as_deref(),
        Some("alpha"),
        "the refused switch moved nothing"
    );
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    assert_eq!(resp.status, 200, "the control device passes the same route");
}

/// Every view row answers 200 to a view device with an empty query: an empty
/// store lists zero sessions, and the one templated view row pages the
/// fixture transcript seeded under [`FIXTURE_ID`].
#[test]
fn a_view_device_reads_every_view_route() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    seed_device("phone", Tier::View, OTHER_TOKEN);
    seed_history_transcript();
    for route in ROUTES.iter().filter(|route| route.access == Access::View) {
        let resp = call(
            &ctx,
            &req(route.method, &route_path(route), Some(OTHER_TOKEN), ""),
        );
        assert_eq!(resp.status, 200, "{} {}", route.method, route.path);
    }
}

/// A stored tier this build does not know: that device authenticates, every
/// route refuses it with the fix named, the log says so once, and the other
/// devices keep working.
#[test]
fn an_unknown_tier_device_is_refused_while_the_others_work() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    seed_device("wall", Tier::Unknown("readonly".to_string()), OTHER_TOKEN);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    for route in ROUTES.iter().filter(|route| route.access != Access::None) {
        let handled = handle(
            &ctx,
            &req(
                route.method,
                &route_path(route),
                Some(OTHER_TOKEN),
                r#"{"profile":"beta"}"#,
            ),
            peer(),
        );
        assert_eq!(
            (handled.response.status, body_json(&handled.response)),
            (
                403,
                serde_json::json!({
                    "ok": false,
                    "error": "device_tier_unknown",
                    "reason": TIER_UNKNOWN,
                })
            ),
            "{} {}",
            route.method,
            route.path
        );
        assert_eq!(
            handled.device.as_deref(),
            Some("wall"),
            "it authenticated; authorization is what refuses it"
        );
    }
    assert_eq!(
        call(&ctx, &req("GET", "/api/v1/health", Some(TOKEN), "")).status,
        200,
        "the other devices are unaffected"
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("carries tier \"readonly\""))
            .count(),
        1
    );
}

/// TK-5: a revoked device is refused on its very next request, with no
/// restart, and the other devices keep working.
#[test]
fn a_revoked_device_is_refused_on_its_next_request() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    seed_device("phone", Tier::View, OTHER_TOKEN);
    let health = |bearer: &str| call(&ctx, &req("GET", "/api/v1/health", Some(bearer), "")).status;

    assert_eq!(health(OTHER_TOKEN), 200);
    devices::revoke("phone").expect("revoke");
    assert_eq!(health(OTHER_TOKEN), 401);
    assert_eq!(health(TOKEN), 200);
}

/// The list is read per request and fails closed: an unreadable one refuses
/// every request with a 500 said once in the log, never read as an empty list
/// and never as the last one that parsed.
#[test]
fn an_unreadable_device_list_refuses_every_request() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    std::fs::write(
        crate::profile::clauth_dir()
            .expect("dir")
            .join("devices.json"),
        b"{ not json",
    )
    .expect("damage the list");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    for _ in 0..2 {
        let resp = call(&ctx, &req("GET", "/api/v1/health", Some(TOKEN), ""));
        assert_eq!(
            (resp.status, body_json(&resp)),
            (500, serde_json::json!({"ok": false, "error": "internal"}))
        );
    }
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.contains("until the device list reads"))
            .count(),
        1
    );
}

/// The router tells the serve loop which device each answer went to, for the
/// per-request audit line.
#[test]
fn the_router_reports_the_device_it_answered() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let device = |bearer: Option<&str>, method: &str, path: &str| {
        handle(&ctx, &req(method, path, bearer, ""), peer()).device
    };
    assert_eq!(
        device(Some(TOKEN), "GET", "/api/v1/health").as_deref(),
        Some(DEVICE)
    );
    assert_eq!(
        device(Some(TOKEN), "GET", "/api/v1/nope").as_deref(),
        Some(DEVICE),
        "a 404 still went to the device"
    );
    assert_eq!(device(None, "GET", "/api/v1/health"), None);
    assert_eq!(
        device(None, "POST", "/api/v1/pair"),
        None,
        "the redemption reads no bearer"
    );
}

#[test]
fn a_wrong_token_is_rejected() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let wrong = "f".repeat(64);

    let resp = call(&ctx, &req("GET", "/api/v1/health", Some(&wrong), ""));
    assert_eq!(resp.status, 401);
}

#[test]
fn health_reports_the_feed_schema() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    let resp = call(&ctx, &req("GET", "/api/v1/health", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    let body = body_json(&resp);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(
        body["schema"],
        serde_json::json!(crate::daemon::SCHEMA_VERSION),
        "a client refuses a daemon newer than it knows off this number"
    );
}

/// The three answer bodies are typed structs now, and each serializes to
/// exactly the bytes the `json!` it replaced produced. Declaration order is the
/// wire key order and every value below is distinct, so a reordered or
/// mis-typed field changes the bytes.
#[test]
fn the_answer_bodies_serialize_byte_identically_to_the_json_they_replaced() {
    assert_eq!(
        serde_json::to_vec(&HealthBody {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            schema: crate::daemon::SCHEMA_VERSION,
        })
        .expect("HealthBody serializes"),
        serde_json::to_vec(&serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "schema": crate::daemon::SCHEMA_VERSION,
        }))
        .expect("json! serializes"),
        "the health body is byte-identical"
    );

    assert_eq!(
        serde_json::to_vec(&SwitchOk {
            ok: true,
            previous: Some("alpha".to_string()),
            active: "beta".to_string(),
        })
        .expect("SwitchOk serializes"),
        serde_json::to_vec(&serde_json::json!({
            "ok": true,
            "previous": "alpha",
            "active": "beta",
        }))
        .expect("json! serializes"),
        "the switch body is byte-identical"
    );

    // A switch from no active profile answers `previous: null` today, so the
    // `Option` is serialized, never skipped.
    assert_eq!(
        serde_json::to_vec(&SwitchOk {
            ok: true,
            previous: None,
            active: "beta".to_string(),
        })
        .expect("SwitchOk serializes"),
        serde_json::to_vec(&serde_json::json!({
            "ok": true,
            "previous": null,
            "active": "beta",
        }))
        .expect("json! serializes"),
        "a missing previous profile is `null`, not a dropped key"
    );

    assert_eq!(
        serde_json::to_vec(&PairOk {
            ok: true,
            name: "phone".to_string(),
            tier: "control".to_string(),
            token: "tok_0123456789".to_string(),
        })
        .expect("PairOk serializes"),
        serde_json::to_vec(&serde_json::json!({
            "ok": true,
            "name": "phone",
            "tier": "control",
            "token": "tok_0123456789",
        }))
        .expect("json! serializes"),
        "the pair body is byte-identical"
    );
}

#[test]
fn an_unknown_path_is_404_and_a_wrong_method_is_405() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    assert_eq!(
        call(&ctx, &req("GET", "/api/v1/nope", Some(TOKEN), "")).status,
        404
    );
    assert_eq!(
        call(&ctx, &req("POST", "/api/v1/status", Some(TOKEN), "")).status,
        405,
        "a known path with the wrong verb says which half is wrong"
    );
    assert_eq!(
        call(&ctx, &req("GET", "/api/v1/switch", Some(TOKEN), "")).status,
        405
    );
}

/// Methods are case-sensitive on the wire, so a lowercase verb is the wrong
/// verb: a plain client sending `get` at a known path is told which half is
/// wrong (405), never silently answered as `GET`. The parser-side half of this
/// pin lives in `daemon_api_http.rs` — the verb has to ARRIVE here lowercase
/// for this arm to be reachable from a real connection.
#[test]
fn a_lowercase_verb_is_a_method_error_not_a_silent_match() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    let resp = call(&ctx, &req("get", "/api/v1/status", Some(TOKEN), ""));
    assert_eq!(resp.status, 405);
    assert_eq!(
        body_json(&resp)["error"],
        serde_json::json!("method_not_allowed")
    );

    let control = call(&ctx, &req("GET", "/api/v1/status", Some(TOKEN), ""));
    assert_eq!(control.status, 200, "the uppercase spelling still serves");
}

/// HEAD routes like GET, at the router itself.
///
/// The route table maps HEAD onto its GET handlers (RFC 9110 §9.3), so this
/// seam answers a HEAD with the full GET response — status, body, and entity
/// tag. The body is stripped one layer up, in the connection loop, by the
/// method-level `Response::into_head` that covers the error arms too; the wire
/// half is pinned in the http tests. Pinning here that the ROUTER answers
/// HEAD as GET (and never 405) is what keeps the mapping from regressing.
#[test]
fn head_routes_like_get_at_the_router() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    let resp = call(&ctx, &req("HEAD", "/api/v1/health", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);

    let status = call(&ctx, &req("HEAD", "/api/v1/status", Some(TOKEN), ""));
    assert_eq!(status.status, 200);

    // The disabled-accounts query is a GET arm too: its HEAD keeps the ETag a
    // conditional client re-arms from, and a matching If-None-Match answers
    // 304 with the tag.
    let tagged = call(&ctx, &req("HEAD", "/api/v1/status?all=1", Some(TOKEN), ""));
    assert_eq!(tagged.status, 200);
    let etag = tagged
        .etag
        .as_deref()
        .expect("the HEAD arm keeps the entity tag")
        .to_string();
    let mut conditional = req_tagged("/api/v1/status?all=1", Some(TOKEN), &etag);
    conditional.method = "HEAD".to_string();
    let not_modified = call(&ctx, &conditional);
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.etag.is_some(), "the 304 repeats the tag");
}

/// The routes live under `/api/v1`, and nothing answers beside it.
///
/// Worth pinning because the prefix moved: an unversioned `/v1` was served
/// before, and a replica or script still dialling it must get a clean 404 rather
/// than a route that happens to still work. The token is valid in every case
/// here, so a 404 is the path being rejected and not the caller.
#[test]
fn only_the_api_v1_prefix_is_served() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    for path in ["/v1/health", "/v1/status", "/health", "/api/health"] {
        assert_eq!(
            call(&ctx, &req("GET", path, Some(TOKEN), "")).status,
            404,
            "{path} is outside the prefix and must not be served"
        );
    }

    assert_eq!(
        call(
            &ctx,
            &req(
                "GET",
                &format!("{}/health", crate::daemon::api::routes::API_PREFIX),
                Some(TOKEN),
                ""
            )
        )
        .status,
        200,
        "the prefix constant is what the table actually answers on"
    );
}

// -------------------------------------------------------------- status

/// `?all=1` answers from the scheduler's live stores, like the plain route.
///
/// It is the one status request that BUILDS a body instead of serving the file
/// the scheduler wrote, and it used to build with no live signals at all — so
/// `fetch_status`, `next_refresh_at`, `stale` and `pending_switch` came off a
/// file mtime while `GET /api/v1/status`, answered from the file, carried the
/// real ones. Same daemon, same second, four fields apart.
#[test]
fn all_equals_one_reads_the_live_stores_not_a_file_mtime() {
    let _home = HomeSandbox::new();
    let live = crate::daemon::LiveStores::default();
    live.usage_status
        .lock()
        .expect("status store")
        .insert("alpha".to_string(), crate::usage::FetchStatus::RateLimited);
    // The other store `LiveStores::snapshot`'s own comment names as the
    // regression: an `AuthExpired` third-party session writes no cache, so
    // before the snapshot carried this store the rebuild could only publish
    // the mtime derivation's `null` for it — indistinguishable from never
    // fetched. beta has no cache either, so only this store can answer.
    live.third_party_status
        .lock()
        .expect("third-party status store")
        .insert("beta".to_string(), crate::usage::FetchStatus::AuthExpired);
    let ctx = ctx_with_live(seeded_config(), live);

    let resp = call(&ctx, &req("GET", "/api/v1/status?all=1", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    let body = body_json(&resp);
    let entry = |name: &str| {
        body["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .find(|p| p["name"] == serde_json::json!(name))
            .unwrap_or_else(|| panic!("{name} is in the roster"))
            .clone()
    };
    let alpha = entry("alpha");
    assert_eq!(
        alpha["fetch_status"],
        serde_json::json!("RateLimited"),
        "the OAuth store's value has to win; nothing wrote a cache file for it to derive from"
    );
    let beta = entry("beta");
    assert_eq!(
        beta["fetch_status"],
        serde_json::json!("AuthExpired"),
        "the third-party store's verdict has to win; beta has no cache, so the \
         mtime derivation would publish null: {}",
        beta
    );
}

/// The published feed is passed through byte for byte: one writer, one shape.
#[test]
fn status_serves_the_on_disk_feed_verbatim() {
    let home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let feed = r#"{"schema":1,"active_profile":"alpha","profiles":[]}"#;
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    std::fs::write(&ctx.status_path, feed).expect("seed feed");
    let _ = home;

    let resp = call(&ctx, &req("GET", "/api/v1/status", Some(TOKEN), ""));
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

    let resp = call(&ctx, &req("GET", "/api/v1/status", Some(TOKEN), ""));
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

    let plain = body_json(&call(&ctx, &req("GET", "/api/v1/status", Some(TOKEN), "")));
    let all = body_json(&call(
        &ctx,
        &req("GET", "/api/v1/status?all=1", Some(TOKEN), ""),
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

/// `?all=1` is conditional like the plain route: its built body is tagged
/// through the same `etag_for`, a matching `If-None-Match` answered 304, a
/// moved feed answered 200 with a new tag. A client reusing its
/// conditional-request code against the query used to get the whole body
/// resent on every poll. `generated_at` moves on every rebuild and the tag
/// ignores it, so the 304 leg is not racing the clock.
#[test]
fn the_all_query_is_tagged_and_answers_304_to_a_matching_tag() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let first = call(&ctx, &req("GET", "/api/v1/status?all=1", Some(TOKEN), ""));
    assert_eq!(first.status, 200);
    let tag = first
        .etag
        .clone()
        .expect("the ?all body carries an entity tag");

    let second = call(&ctx, &req_tagged("/api/v1/status?all=1", Some(TOKEN), &tag));
    assert_eq!(second.status, 304, "an unchanged roster is not resent");
    assert_eq!(second.etag.as_deref(), Some(tag.as_str()));

    // The feed moves: the active account changes, so the body a reader could
    // act on changes with it.
    config.lock().expect("config").state.active_profile = Some("beta".into());
    let third = call(&ctx, &req_tagged("/api/v1/status?all=1", Some(TOKEN), &tag));
    assert_eq!(third.status, 200, "a moved feed is answered, not 304'd");
    let moved = third.etag.clone().expect("a 200 carries an entity tag");
    assert_ne!(moved, tag, "and the feed's move is visible in the tag");

    let fourth = call(
        &ctx,
        &req_tagged("/api/v1/status?all=1", Some(TOKEN), &moved),
    );
    assert_eq!(fourth.status, 304, "the new tag is now the current one");
}

// -------------------------------------------------------------- switch

#[test]
fn switch_relinks_and_reports_the_previous_account() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
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

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"BeTa"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(body_json(&resp)["active"], serde_json::json!("beta"));
}

#[test]
fn switch_to_an_unknown_profile_is_404_and_changes_nothing() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"ghost"}"#,
        ),
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

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
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
        let resp = call(&ctx, &req("POST", "/api/v1/switch", Some(TOKEN), body));
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

        let resp = call(
            &ctx,
            &req(
                "POST",
                "/api/v1/switch",
                Some(TOKEN),
                r#"{"profile":"beta"}"#,
            ),
        );
        assert_eq!(resp.status, 409);
        assert_eq!(
            body_json(&resp)["error"],
            serde_json::json!("switch_in_progress")
        );
        let _ = release_tx.send(());
    });
}

/// The poisoned-gate half, pinned at the ROUTE rather than the primitive.
///
/// `RankedMutex::try_lock` recovers a poisoned mutex (its pin lives in
/// `tests/inline/lockorder.rs`), but only a test through `handle` reds on the
/// caller that matters: a future edit collapsing poison back into the lock
/// error would answer every later switch with a permanent `409
/// switch_in_progress`, and the primitive's pin would stay green while the
/// route wedged.
#[test]
fn a_poisoned_gate_does_not_wedge_the_switch_route() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());

    // Poison it the only way a mutex gets poisoned: panic while holding it.
    let holder = std::sync::Arc::clone(&ctx);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _gate = holder.switch_gate.lock().expect("gate");
        panic!("a switch panicked under the gate");
    }));
    assert!(poisoned.is_err(), "precondition: the closure panicked");

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    assert_eq!(
        resp.status,
        200,
        "a poisoned gate is recovered and the switch proceeds, not answered with a \
         permanent 409: {}",
        String::from_utf8_lossy(&resp.body)
    );
    assert_eq!(body_json(&resp)["active"], serde_json::json!("beta"));
    assert_eq!(
        ctx.config
            .lock()
            .expect("config")
            .state
            .active_profile
            .as_deref(),
        Some("beta"),
        "the switch must still land in state, not just in the response"
    );
}

/// The switch gate must not invert the lock order when the target's token has
/// already expired.
///
/// Every other case in this file mints `expires_at: None`, so `expiring()` is
/// false and `ensure_installable` short-circuits before it acquires a
/// `RotationGuard` — which is why the whole suite stayed green over a live
/// lock-order bug. With an expiry in the past the guard IS taken, entering
/// `rank::Rotation` while `rank::ApiSwitch` is held; at ApiSwitch's old 380 that
/// tripped the ordering assert and panicked the request out on any debug build.
///
/// What is asserted is the absence of that panic: the switch is expected to
/// FAIL — the chain here is synthetic — but to fail as an answer rather than by
/// unwinding through the gate. The token endpoint is pointed at a closed
/// loopback port so the refresh leg fails as transport without reaching the
/// network; without that the test would post a synthetic refresh token to
/// Anthropic on every run.
#[test]
fn a_switch_to_a_clock_expired_target_does_not_invert_the_lock_order() {
    let home = HomeSandbox::new();

    // Bound then dropped: the port is now closed, so a connect fails at once.
    let dead = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = dead.local_addr().expect("addr").port();
    drop(dead);
    // The RAII sandbox the other endpoint-override tests use, not a bare
    // `set_endpoint_overrides`: this test exists to catch a PANIC (the
    // lock-order assert), and a panic unwinds past a manual clear, leaving the
    // dead-port redirect installed for every sibling test in the binary.
    let _endpoints =
        crate::testutil::EndpointSandbox::new(&home, &format!("http://127.0.0.1:{port}"));

    let config = seeded_config();
    {
        let mut cfg = config.lock().expect("config");
        let beta = cfg
            .find_mut(&crate::profile::ProfileName::from("beta"))
            .expect("beta");
        // An hour in the past, in epoch ms, so `expiring()` is true however the
        // staleness window is spelled.
        if let Some(oauth) = beta
            .credentials
            .as_mut()
            .and_then(|c| c.claude_ai_oauth.as_mut())
        {
            oauth.expires_at = Some(crate::usage::now_ms() as i64 - 3_600_000);
        }
        save_profile(beta).expect("save");
    }
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );

    assert_eq!(
        resp.status, 409,
        "an unrefreshable target is refused, not unwound through the gate"
    );
}

/// The switch is the same action `clauth <name>` and the MCP tool perform, so
/// it inherits their refusal sentences — not their anyhow chains. The IO arms
/// of a switch carry context strings that name absolute paths under the
/// operator's home (`failed to publish /home/…/credentials.json`); a reason
/// that reflects the open chain puts the operator's home layout in an HTTP
/// body, and the body is the one surface the daemon hands to a remote reader.
/// The failure is posed by making the live `.credentials.json` a directory:
/// the relink stages its symlink fine, and the rename onto a directory fails
/// `EISDIR` — the `failed to publish <home path>` arm itself.
#[test]
fn a_failed_switch_reflects_no_home_path() {
    let home = HomeSandbox::new();
    let config = seeded_config();
    {
        let mut cfg = config.lock().expect("config");
        // A diverged-looking live slot routes to the divergence machinery, so
        // the Discard default is what lets a headless switch reach the link
        // publish this fixture is built to fail.
        cfg.state.default_divergence = Some(DivergenceChoice::Discard);
    }
    std::fs::create_dir_all(
        crate::profile::claude_dir()
            .expect("claude dir")
            .join(".credentials.json"),
    )
    .expect("pose the wedge");
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    let body = body_json(&resp);
    let reason = body["reason"].as_str().expect("reason is a string");
    let sandbox = home.home().display().to_string();
    assert_eq!(
        resp.status,
        500,
        "an IO failure answers switch_failed, body: {}",
        String::from_utf8_lossy(&resp.body)
    );
    assert_eq!(body["error"], serde_json::json!("switch_failed"));
    assert!(
        !reason.contains(&sandbox) && !reason.contains("/home/"),
        "the reflected reason must carry no home path, got: {reason}"
    );
    assert_eq!(
        config
            .lock()
            .expect("config")
            .state
            .active_profile
            .as_deref(),
        Some("alpha"),
        "a failed switch must leave the active profile unchanged"
    );
}

/// The 500 the failed switch answers is a fixed literal, so the context that
/// would name the home path has to live somewhere the operator can read: the
/// refusal logline, and only it. `Refused` renders a bare sentence, `Failed`
/// the full anyhow chain — a head-line-only copy would name neither the
/// failed operation nor its cause, and the body's "see daemon.log" would
/// point at a log that cannot answer.
#[test]
fn a_failed_switchs_context_reaches_the_log_but_not_the_body() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    {
        let mut cfg = config.lock().expect("config");
        cfg.state.default_divergence = Some(DivergenceChoice::Discard);
    }
    std::fs::create_dir_all(
        crate::profile::claude_dir()
            .expect("claude dir")
            .join(".credentials.json"),
    )
    .expect("pose the wedge");

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let ctx = ctx_with(std::sync::Arc::clone(&config));
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    assert_eq!(resp.status, 500);

    let body = body_json(&resp);
    assert_eq!(
        body["reason"],
        serde_json::json!("the switch failed; see daemon.log"),
        "the body is the fixed literal, nothing else"
    );
    let joined = lines.snapshot().join("\n");
    let logged = joined
        .lines()
        .find(|line| line.contains("refused"))
        .expect("the refusal reached the log");
    assert!(
        logged.contains("failed to publish"),
        "the logline names the operation that failed: {logged}"
    );
    assert!(
        logged.contains("Is a directory"),
        "the logline carries the underlying cause, not the head line alone: {logged}"
    );
    let home = _home.home().display().to_string();
    assert!(
        !body["reason"].as_str().unwrap_or_default().contains(&home),
        "the home path lives in the log, never the body"
    );
}

/// The authored refusals — the closed diagnostic set `src/format.rs` renders
/// — reflect verbatim, so a remote reader gets the same actionable sentence
/// the CLI and MCP surfaces do, name and fix included.
#[test]
fn a_refused_switch_reflects_the_authored_sentence() {
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

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    let body = body_json(&resp);
    assert_eq!(resp.status, 409);
    assert_eq!(body["error"], serde_json::json!("switch_refused"));
    assert_eq!(
        body["reason"],
        serde_json::json!("'beta': account is disabled, run `clauth enable beta`")
    );
}

/// A config snapshot a tick old must not misfile a genuine refusal as an
/// unexpected failure. `ensure_switch_target_ok` re-reads the roster off disk
/// under the state flock precisely because the daemon holds a handle a
/// concurrent `clauth delete` can leave behind; the refusal it raises there is
/// still authored, still carries the fix, and must still answer 409 — the
/// route's own membership check passed a moment earlier, so answering 500 for
/// what the disk says a tick later mislabels a refusal the operator can act
/// on. Posed by the divergence itself: the handle still lists beta while the
/// saved roster does not.
#[test]
fn a_target_vanishing_mid_switch_answers_refused_not_failed() {
    let _home = HomeSandbox::new();
    let config = seeded_config();
    // seeded_config saved a roster carrying both profiles; take beta back off
    // the disk while the in-memory handle keeps it, which is exactly what a
    // delete racing the route's pre-check leaves behind.
    {
        let mut cfg = config.lock().expect("config");
        cfg.state.profiles.retain(|n| n != "beta");
        save_app_state(&cfg.state).expect("save roster");
    }
    let ctx = ctx_with(std::sync::Arc::clone(&config));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    let body = body_json(&resp);
    assert_eq!(
        resp.status,
        409,
        "a vanished target is a refusal, not an unexpected failure: {}",
        String::from_utf8_lossy(&resp.body)
    );
    assert_eq!(body["error"], serde_json::json!("switch_refused"));
    assert_eq!(
        body["reason"],
        serde_json::json!("profile 'beta' not found")
    );
}

/// A held state flock is the one retryable refusal, and its reason is the
/// closed `StateLockTimeout` Display — `~/.clauth/.lock` spelled as a literal,
/// never the sandbox's absolute home. Posed with the same independent open
/// file description `tests/inline/lock.rs` uses to stand in for a second
/// clauth process, plus the thread-local deadline override so the wait is
/// milliseconds, not 25 s.
#[test]
fn a_state_lock_timeout_is_503_with_a_path_free_reason() {
    let _home = HomeSandbox::new();
    // Seeded BEFORE the wedge: `seeded_config` takes the state flock itself.
    let ctx = ctx_with(seeded_config());
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");
    crate::lock::set_state_lock_timeout_override(Some(std::time::Duration::from_millis(100)));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    crate::lock::set_state_lock_timeout_override(None);
    drop(holder);

    let body = body_json(&resp);
    assert_eq!(resp.status, 503, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(body["error"], serde_json::json!("state_locked"));
    let reason = body["reason"].as_str().expect("reason is a string");
    assert!(
        !reason.contains("/home/") && !reason.contains("home"),
        "the reflected reason must carry no home path, got: {reason}"
    );
    assert!(
        reason.contains("state lock"),
        "the reason names the condition, got: {reason}"
    );
}

// --------------------------------------------------------------- audit

/// RE-1: the switch's own line names the device that asked.
#[test]
fn a_switch_line_names_the_device() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with(seeded_config());
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert!(
        lines
            .snapshot()
            .contains(&"clauth api: device 'test' switched to 'beta'".to_string()),
        "{:#?}",
        lines.snapshot()
    );
}

#[test]
fn a_refused_switch_line_names_the_device() {
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
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/switch",
            Some(TOKEN),
            r#"{"profile":"beta"}"#,
        ),
    );
    assert_eq!(resp.status, 409);
    assert!(
        lines
            .snapshot()
            .iter()
            .any(|line| line.starts_with("clauth api: device 'test' switch to 'beta' refused: ")),
        "{:#?}",
        lines.snapshot()
    );
}

/// `GET /proxies` answers the supervisors' live slots over the record-only
/// verdict: a published slot wins for its row, not the record alone.
#[test]
fn get_proxies_publishes_the_live_slot_over_the_record() {
    use crate::daemon::proxies::{ProxySlot, ProxyState};
    use crate::proxy::Service;
    let _home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("proxies.toml"),
        "[zcode]\nport = 9101\nenabled = true\n",
    )
    .expect("registry");
    let live = crate::daemon::LiveStores::default();
    let slot = ProxySlot {
        service: "zcode".to_string(),
        state: ProxyState::Healthy,
        binary: Some("/usr/local/bin/clauth-zcode-proxy".to_string()),
        port: Some(9101),
        pid: Some(4242),
        version: Some("1.2.0".to_string()),
        contract: Some("1.0".to_string()),
        answerer: None,
        restarts: 0,
        last_exit: None,
        reason: None,
        since: Some("2026-09-29T00:00:00Z".to_string()),
    };
    live.proxies
        .lock()
        .expect("slots")
        .insert(Service::parse("zcode").expect("a service"), slot);
    let ctx = ctx_with_live(seeded_config(), live);

    let resp = call(&ctx, &req("GET", "/api/v1/proxies", Some(TOKEN), ""));
    assert_eq!(resp.status, 200);
    let body = body_json(&resp);
    assert_eq!(body[0]["service"], "zcode");
    assert_eq!(body[0]["state"], "healthy", "the live slot wins");
    assert_eq!(body[0]["version"], "1.2.0");
    assert_eq!(body[0]["pid"], 4242);
}

/// `ProxySlot` documents every field as required, like `GatewaySlot`: the
/// served OpenAPI schema's `required` list names all 12 fields, so a client
/// generated from it never types a proxy field as optional by accident.
#[test]
fn proxy_slot_requires_every_field() {
    let doc = openapi_document_bytes().expect("document serializes");
    let value: serde_json::Value = serde_json::from_slice(&doc).expect("json");
    let required = value["components"]["schemas"]["ProxySlot"]["required"]
        .as_array()
        .unwrap_or_else(|| panic!("ProxySlot documents a required list"));
    let mut required: Vec<&str> = required
        .iter()
        .map(|field| field.as_str().expect("a field name"))
        .collect();
    required.sort_unstable();
    assert_eq!(
        required,
        [
            "answerer",
            "binary",
            "contract",
            "last_exit",
            "pid",
            "port",
            "reason",
            "restarts",
            "service",
            "since",
            "state",
            "version",
        ],
        "ProxySlot's required set"
    );
}
