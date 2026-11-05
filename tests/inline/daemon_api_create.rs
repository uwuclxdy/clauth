#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `POST /api/v1/sessions`: the gate order, the fresh config read, the body
//! validation, the three agent forms, herdr's refusals mapped, the tab closed
//! on a refused start, and the audit line.
//!
//! herdr is a recorded table behind the context's seam, so no test runs a
//! real one; every call's argv and deadline are recorded for the pins.

#![cfg(unix)]

use super::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut, PaneProbe};
use crate::testutil::{DEVICE, HomeSandbox, OTHER_TOKEN, TOKEN, body_json, call, req, seed_device};

/// The measured `tab create` envelope (herdr 0.9.0), from the fixture.
const TAB_CREATED: &str = include_str!("../fixtures/sessions/tab-created.json");
/// herdr's `agent_not_ready` envelope as measured 2026-09-17: printed on
/// stderr, empty stdout, exit 1, while the agent is running but blocked.
const AGENT_NOT_READY: &str = r#"{"error":{"code":"agent_not_ready","message":"agent claude blocked during startup"},"id":"cli:agent:start"}"#;
/// `pane get`'s envelope, the captured shape with the status herdr reports for
/// a blocked and an idle agent.
const PANE_GET_BLOCKED: &str = r#"{"id":"cli:pane:get","result":{"pane":{"agent":"claude","agent_status":"blocked","cwd":"/tmp/work","focused":false,"pane_id":"w1:p2","tab_id":"w1:t2","workspace_id":"w1"},"type":"pane_info"}}"#;
const PANE_GET_IDLE: &str = r#"{"id":"cli:pane:get","result":{"pane":{"agent":"claude","agent_status":"idle","cwd":"/tmp/work","focused":false,"pane_id":"w1:p2","tab_id":"w1:t2","workspace_id":"w1"},"type":"pane_info"}}"#;
const WORKSPACE_NOT_FOUND_ERR: &str = r#"{"error":{"code":"workspace_not_found","message":"workspace w9:nope not found"},"id":"cli:tab:create"}"#;
const SERVER_NOT_RUNNING_ERR: &str = r#"{"id":"cli:tab:create","error":{"code":"server_not_running","message":"no herdr server is running at /nonexistent/herdr.sock; run `herdr` to start or attach it"}}"#;
const UNSUPPORTED_KIND: &str = "unsupported interactive agent kind: bogus";
const PANE_NOT_FOUND_ENVELOPE: &str =
    r#"{"error":{"code":"pane_not_found","message":"pane w1:p2 not found"},"id":"cli:pane:run"}"#;
const TAB_CLOSE_REFUSED: &str =
    r#"{"error":{"code":"tab_not_found","message":"tab w1:t2 not found"},"id":"cli:tab:close"}"#;

type Call = (Vec<String>, Duration);
type Calls = Arc<Mutex<Vec<Call>>>;

#[derive(Clone, Copy)]
enum Answer {
    NotInstalled,
    NeverRan,
    Ran {
        success: bool,
        stdout: &'static str,
        stderr: &'static str,
    },
}

fn ok(stdout: &'static str) -> Answer {
    Answer::Ran {
        success: true,
        stdout,
        stderr: "",
    }
}

fn failed(stderr: &'static str) -> Answer {
    Answer::Ran {
        success: false,
        stdout: "",
        stderr,
    }
}

/// A seam recording every `(argv, deadline)` and answering through `f`.
fn seam(f: impl Fn(&[&str]) -> Answer + Send + Sync + 'static) -> (PaneProbe, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    let probe: PaneProbe = Box::new(move |args, deadline| {
        recorded
            .lock()
            .unwrap()
            .push((args.iter().map(|arg| arg.to_string()).collect(), deadline));
        match f(args) {
            Answer::NotInstalled => HerdrProbeOut::NotInstalled,
            Answer::NeverRan => HerdrProbeOut::Ran(None),
            Answer::Ran {
                success,
                stdout,
                stderr,
            } => HerdrProbeOut::Ran(Some(HerdrOut {
                success,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            })),
        }
    });
    (probe, calls)
}

fn calls_of(calls: &Calls) -> Vec<Call> {
    calls.lock().unwrap().clone()
}

/// Every recorded argv in call order, for the pins that name the whole
/// sequence (a missing `tab close` then reads as a short list, never an index
/// panic).
fn argv_list(calls: &Calls) -> Vec<Vec<String>> {
    calls_of(calls).into_iter().map(|(argv, _)| argv).collect()
}

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

/// Persist the `[serve]` knob and a claude roster of one profile.
fn save_state(creation: bool) {
    let state = crate::profile::AppState {
        serve: crate::profile::ServeSettings {
            session_creation: creation,
        },
        profiles: vec!["acme".into()],
        ..Default::default()
    };
    crate::profile::save_app_state(&state).expect("save app state");
}

/// A control device with the sessions grant, and the config key on.
fn seed_control(creation: bool) {
    save_state(creation);
    seed_device(DEVICE, Tier::Control, TOKEN);
    crate::daemon::api::devices::allow_sessions(DEVICE).expect("grant sessions");
}

fn ctx_with_probe(probe: PaneProbe) -> Arc<ApiContext> {
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::for_tests(config(), status_path, None, probe)
}

/// An absolute existing cwd under the sandbox, and the body naming it.
fn cwd(sb: &HomeSandbox) -> String {
    let dir = sb.home().join("work");
    std::fs::create_dir_all(&dir).expect("create cwd");
    dir.to_string_lossy().into_owned()
}

fn body_with(cwd: &str, extra: &str) -> String {
    let mut body: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&format!(
        r#"{{"cwd":{}}}"#,
        serde_json::to_string(cwd).unwrap()
    ))
    .expect("cwd body parses");
    body.extend(
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(extra)
            .expect("extra parses"),
    );
    serde_json::to_string(&body).expect("body serializes")
}

fn path() -> &'static str {
    "/api/v1/sessions"
}

/// The three gates run key, then grant, then body: a device without the grant
/// still hears the key-off refusal, a granted device still hears the key-off
/// refusal before its relative cwd is judged, and an ungranted device hears
/// the grant refusal before its relative cwd is judged. A view device gets the
/// table's `control_required` before any of them. These inputs are what order
/// the gates.
#[test]
fn the_gate_order_is_key_then_grant_and_a_view_device_gets_the_table_refusal() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(false);
    let nogrant_token = "c".repeat(64);
    seed_device("nogrunt", Tier::Control, &nogrant_token);

    let (probe, calls) = seam(|_| Answer::Ran {
        success: true,
        stdout: TAB_CREATED,
        stderr: "",
    });
    let ctx = ctx_with_probe(probe);
    let off = serde_json::json!({
        "ok": false,
        "error": "session_creation_off",
        "reason": "session creation is off; set `session_creation = true` under `[serve]` in profiles.toml to enable it",
    });
    let no_grant = serde_json::json!({
        "ok": false,
        "error": "sessions_grant_required",
        "reason": "this device lacks the sessions grant; run `clauth devices allow-sessions <name>` on the host to grant it",
    });
    let relative = body_with("relative/dir", "{}");
    let valid = body_with(&cwd, "{}");

    // Key off + no grant + a valid body: the key is read before the grant.
    let resp = call(&ctx, &req("POST", path(), Some(&nogrant_token), &valid));
    assert_eq!((resp.status, body_json(&resp)), (403, off.clone()));

    // Key off + the grant + a relative cwd: the key is read before the body.
    let resp = call(&ctx, &req("POST", path(), Some(TOKEN), &relative));
    assert_eq!((resp.status, body_json(&resp)), (403, off.clone()));

    // Key on + no grant + a relative cwd: the grant is read before the body.
    save_state(true);
    let resp = call(&ctx, &req("POST", path(), Some(&nogrant_token), &relative));
    assert_eq!((resp.status, body_json(&resp)), (403, no_grant));

    // A view device hits the table's control_required before any gate.
    seed_device("phone", Tier::View, OTHER_TOKEN);
    let resp = call(&ctx, &req("POST", path(), Some(OTHER_TOKEN), &valid));
    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (403, serde_json::json!("control_required"))
    );
    assert!(calls_of(&calls).is_empty(), "herdr was never asked");
}

/// The key is read per request: flipping profiles.toml between two requests on
/// one context changes the answer (threat-model SC-3).
#[test]
fn the_config_key_is_read_fresh_per_request() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(false);
    let (probe, _) = seam(|_| ok(TAB_CREATED));
    let ctx = ctx_with_probe(probe);

    let off = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(off.status, 403, "the key starts off");

    save_state(true);
    let on = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(on.status, 200, "the same context now reads the key on");
}

/// Each validation refusal carries the field's sentence, and an unknown
/// profile is the one 404.
#[test]
fn validation_refusals_each_name_their_field() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|_| ok(TAB_CREATED));
    let ctx = ctx_with_probe(probe);

    let refuse = |body: &str, reason: &str| {
        let resp = call(&ctx, &req("POST", path(), Some(TOKEN), body));
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                400,
                serde_json::json!({ "ok": false, "error": "bad_request", "reason": reason })
            ),
            "{body}"
        );
    };

    refuse(
        &body_with("relative/dir", "{}"),
        "cwd must be an absolute path",
    );
    refuse(
        &body_with(&format!("{cwd}/missing"), "{}"),
        "cwd must name an existing directory",
    );
    refuse(
        &body_with(&cwd, r#"{"profile":"acme","kind":"codex"}"#),
        "name either a profile or a kind, never both",
    );
    refuse(
        &body_with(&cwd, r#"{"kind":"Bad Kind"}"#),
        "kind must be 1..=32 ascii lowercase letters or digits",
    );
    refuse(
        &body_with(&cwd, r#"{"workspace":"bad workspace!"}"#),
        "workspace must be 1..=32 chars of letters, digits or colons",
    );
    let long_kind = "a".repeat(33);
    let long_workspace = "w".repeat(33);
    for kind in ["", long_kind.as_str(), "UPPER"] {
        refuse(
            &serde_json::json!({ "cwd": cwd, "kind": kind }).to_string(),
            "kind must be 1..=32 ascii lowercase letters or digits",
        );
    }
    for workspace in ["", long_workspace.as_str()] {
        refuse(
            &serde_json::json!({ "cwd": cwd, "workspace": workspace }).to_string(),
            "workspace must be 1..=32 chars of letters, digits or colons",
        );
    }
    refuse(
        &body_with(&cwd, r#"{"profile":"../acme"}"#),
        "profile must be letters, digits and - _ . @ + only, and can't start with '.'",
    );

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"ghost"}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            404,
            serde_json::json!({
                "ok": false,
                "error": "profile_not_found",
                "reason": "no stored claude or codex profile has that name",
            })
        )
    );
    assert!(calls_of(&calls).is_empty(), "herdr was never asked");
}

/// Bare `claude` is `agent start --kind claude`, a `agent_not_ready` exit is a
/// created session, and the status is read back from `pane get`.
#[test]
fn bare_claude_starts_kind_claude_and_reads_back_blocked() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        [
            "agent",
            "start",
            "clauth-w1-p2",
            "--kind",
            "claude",
            "--pane",
            "w1:p2",
            "--timeout",
            "60000",
        ] => failed(AGENT_NOT_READY),
        ["pane", "get", "w1:p2"] => ok(PANE_GET_BLOCKED),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "workspace_id": "w1",
                "tab_id": "w1:t2",
                "pane_id": "w1:p2",
                "agent_status": "blocked",
            })
        )
    );
    assert_eq!(
        argv_list(&calls),
        vec![
            vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
            vec![
                "agent",
                "start",
                "clauth-w1-p2",
                "--kind",
                "claude",
                "--pane",
                "w1:p2",
                "--timeout",
                "60000"
            ],
            vec!["pane", "get", "w1:p2"],
        ]
    );
}

/// A named kind reaches `agent start --kind <kind>`, and a ready start answers
/// the status read back.
#[test]
fn a_kind_reaches_agent_start_with_that_kind() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        [
            "agent",
            "start",
            "clauth-w1-p2",
            "--kind",
            "codex",
            "--pane",
            "w1:p2",
            "--timeout",
            "60000",
        ] => ok(""),
        ["pane", "get", "w1:p2"] => ok(PANE_GET_IDLE),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"kind":"codex"}"#),
        ),
    );
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    assert_eq!(body_json(&resp)["agent_status"], serde_json::json!("idle"));
    assert_eq!(
        calls_of(&calls)[1].0,
        vec![
            "agent",
            "start",
            "clauth-w1-p2",
            "--kind",
            "codex",
            "--pane",
            "w1:p2",
            "--timeout",
            "60000"
        ]
    );
}

/// A clauth profile runs `pane run <pane> clauth start <profile>` and answers
/// `agent_status: null`, with no read-back.
#[test]
fn a_profile_runs_clauth_start_and_answers_null_status() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["pane", "run", "w1:p2", "clauth", "start", "acme"] => ok(""),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme"}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "workspace_id": "w1",
                "tab_id": "w1:t2",
                "pane_id": "w1:p2",
                "agent_status": null,
            })
        )
    );
    assert_eq!(
        argv_list(&calls),
        vec![
            vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
            vec!["pane", "run", "w1:p2", "clauth", "start", "acme"],
        ]
    );
    assert_eq!(
        calls_of(&calls)
            .into_iter()
            .map(|(_, deadline)| deadline)
            .collect::<Vec<_>>(),
        vec![crate::herdr::PROBE_TIMEOUT, crate::herdr::PROBE_TIMEOUT]
    );
}

/// `workspace_not_found` is 409, `server_not_running` is 503, and a refused
/// `agent start` closes the tab before the 502; a failed close is logged and
/// the 502 stands.
#[test]
fn herdr_refusals_map_and_close_the_tab() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);

    // workspace_not_found.
    let (probe, _) = seam(|args| match args {
        [
            "tab",
            "create",
            "--cwd",
            _,
            "--no-focus",
            "--workspace",
            "w9",
        ] => failed(WORKSPACE_NOT_FOUND_ERR),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"workspace":"w9"}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            409,
            serde_json::json!({
                "ok": false,
                "error": "workspace_not_found",
                "reason": "no herdr workspace has that id",
            })
        )
    );

    // server_not_running on the tab create.
    let (probe, _) = seam(|args| match args {
        ["tab", "create", ..] => failed(SERVER_NOT_RUNNING_ERR),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (503, serde_json::json!("herdr_unavailable"))
    );

    // An unsupported kind closes the tab, logs the herdr line, and answers 502.
    {
        let (probe, calls) = seam(|args| match args {
            ["tab", "create", ..] => ok(TAB_CREATED),
            ["agent", "start", _, "--kind", "bogus", ..] => failed(UNSUPPORTED_KIND),
            ["tab", "close", "w1:t2"] => ok(""),
            _ => Answer::NeverRan,
        });
        let ctx = ctx_with_probe(probe);
        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        let resp = call(
            &ctx,
            &req(
                "POST",
                path(),
                Some(TOKEN),
                &body_with(&cwd, r#"{"kind":"bogus"}"#),
            ),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                502,
                serde_json::json!({
                    "ok": false,
                    "error": "herdr_refused",
                    "reason": "herdr refused the request; see daemon.log",
                })
            )
        );
        assert_eq!(
            argv_list(&calls),
            vec![
                vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
                vec![
                    "agent",
                    "start",
                    "clauth-w1-p2",
                    "--kind",
                    "bogus",
                    "--pane",
                    "w1:p2",
                    "--timeout",
                    "60000"
                ],
                vec!["tab", "close", "w1:t2"],
            ],
            "the created tab is closed"
        );
        assert_eq!(
            lines.snapshot(),
            vec![format!(
                "clauth api: device 'test' agent start refused by herdr: {UNSUPPORTED_KIND}"
            )]
        );
    }

    // A failed close is logged and the 502 still stands.
    {
        let (probe, calls) = seam(|args| match args {
            ["tab", "create", ..] => ok(TAB_CREATED),
            ["agent", "start", _, "--kind", "bogus", ..] => failed(UNSUPPORTED_KIND),
            ["tab", "close", "w1:t2"] => failed(TAB_CLOSE_REFUSED),
            _ => Answer::NeverRan,
        });
        let ctx = ctx_with_probe(probe);
        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        let resp = call(
            &ctx,
            &req(
                "POST",
                path(),
                Some(TOKEN),
                &body_with(&cwd, r#"{"kind":"bogus"}"#),
            ),
        );
        assert_eq!(resp.status, 502);
        assert_eq!(
            argv_list(&calls),
            vec![
                vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
                vec![
                    "agent",
                    "start",
                    "clauth-w1-p2",
                    "--kind",
                    "bogus",
                    "--pane",
                    "w1:p2",
                    "--timeout",
                    "60000"
                ],
                vec!["tab", "close", "w1:t2"],
            ]
        );
        assert_eq!(
            lines.snapshot(),
            vec![
                format!(
                    "clauth api: device 'test' agent start refused by herdr: {UNSUPPORTED_KIND}"
                ),
                format!("clauth api: device 'test' tab close 'w1:t2' failed: {TAB_CLOSE_REFUSED}"),
            ]
        );
    }
}

/// One audit line per creation, naming the device, the agent form, the cwd,
/// and the tab and pane ids.
#[test]
fn the_audit_line_names_device_form_cwd_tab_and_pane() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, _) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["agent", "start", _, "--kind", "claude", ..] => ok(""),
        ["pane", "get", "w1:p2"] => ok(PANE_GET_IDLE),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' created a session form='claude' cwd='{cwd}' tab='w1:t2' pane='w1:p2'"
        )]
    );
}

/// `agent start` gets the 65 s deadline while every other call keeps the
/// seam's 2 s probe timeout.
#[test]
fn agent_start_gets_the_long_deadline_while_other_calls_keep_the_short_one() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["agent", "start", _, "--kind", "claude", ..] => ok(""),
        ["pane", "get", "w1:p2"] => ok(PANE_GET_IDLE),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        calls_of(&calls)
            .into_iter()
            .map(|(_, deadline)| deadline)
            .collect::<Vec<_>>(),
        vec![
            crate::herdr::PROBE_TIMEOUT,
            AGENT_START_TIMEOUT,
            crate::herdr::PROBE_TIMEOUT,
        ]
    );
}

/// herdr absent at the first call is 503 with the fixed sentence, and no
/// further call is made.
#[test]
fn herdr_absent_is_503_and_stops() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|_| Answer::NotInstalled);
    let ctx = ctx_with_probe(probe);

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            503,
            serde_json::json!({
                "ok": false,
                "error": "herdr_unavailable",
                "reason": "herdr is not installed on this host",
            })
        )
    );
    assert_eq!(calls_of(&calls).len(), 1, "one call, then the refusal");
}

/// A `tab create` that never answers is 503, with one log line naming the
/// device and the cwd so a tab that outlived the flow is attributable.
#[test]
fn a_tab_create_that_does_not_answer_is_logged_and_503() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|_| Answer::NeverRan);
    let ctx = ctx_with_probe(probe);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (503, serde_json::json!("herdr_unavailable"))
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' tab create in cwd '{cwd}' did not answer (deadline or \
             spawn failure); a tab may exist unattributed"
        )]
    );
    assert_eq!(calls_of(&calls).len(), 1, "one call, then the refusal");
}

/// An unparseable `profiles.toml` answers 500, never the key-off refusal, and
/// logs the parse error once.
#[test]
fn an_unparseable_profiles_toml_answers_500_and_logs() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|_| ok(TAB_CREATED));
    let ctx = ctx_with_probe(probe);

    let state_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("profiles.toml");
    std::fs::write(&state_path, "[serve\nsession_creation = tru").expect("corrupt profiles.toml");

    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (500, serde_json::json!({ "ok": false, "error": "internal" }))
    );
    // The same error the handler saw, rendered the same way, so a `toml` bump
    // that rewords its diagnostics cannot red this pin.
    let unreadable = crate::profile::load_app_state().expect_err("the corrupt file must not read");
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: session creation refused, profiles.toml does not read: {unreadable:#}"
        )]
    );
    assert!(calls_of(&calls).is_empty(), "herdr was never asked");
}

/// An `agent start` that never answers is a 502 with the did-not-answer line,
/// and the created tab is closed last.
#[test]
fn a_hung_agent_start_times_out_closes_and_answers_502() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["agent", "start", _, "--kind", "claude", ..] => Answer::NeverRan,
        ["tab", "close", "w1:t2"] => ok(""),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            502,
            serde_json::json!({
                "ok": false,
                "error": "herdr_refused",
                "reason": "herdr refused the request; see daemon.log",
            })
        )
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth api: device 'test' agent start did not answer (deadline or spawn failure)"
                .to_string()
        ]
    );
    assert_eq!(
        argv_list(&calls),
        vec![
            vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
            vec![
                "agent",
                "start",
                "clauth-w1-p2",
                "--kind",
                "claude",
                "--pane",
                "w1:p2",
                "--timeout",
                "60000"
            ],
            vec!["tab", "close", "w1:t2"],
        ]
    );
}

/// A server that dies between `tab create` and `agent start` is 503 with the
/// no-server sentence, and the tab is closed.
#[test]
fn agent_start_server_not_running_closes_and_answers_503() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["agent", "start", _, "--kind", "claude", ..] => failed(SERVER_NOT_RUNNING_ERR),
        ["tab", "close", "w1:t2"] => ok(""),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            503,
            serde_json::json!({
                "ok": false,
                "error": "herdr_unavailable",
                "reason": "herdr is installed but no server answered on its socket",
            })
        )
    );
    assert_eq!(
        argv_list(&calls),
        vec![
            vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
            vec![
                "agent",
                "start",
                "clauth-w1-p2",
                "--kind",
                "claude",
                "--pane",
                "w1:p2",
                "--timeout",
                "60000"
            ],
            vec!["tab", "close", "w1:t2"],
        ]
    );
}

/// A refused `pane run` is herdr refusing the daemon's own follow-up (the pane
/// is the daemon's intermediate, never something the client named), so it is
/// the logged 502 like any other refusal, and the tab is closed last.
#[test]
fn a_refused_pane_run_closes_and_answers_502() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["pane", "run", "w1:p2", "clauth", "start", "acme"] => failed(PANE_NOT_FOUND_ENVELOPE),
        ["tab", "close", "w1:t2"] => ok(""),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme"}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            502,
            serde_json::json!({
                "ok": false,
                "error": "herdr_refused",
                "reason": "herdr refused the request; see daemon.log",
            })
        )
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' pane run refused by herdr: {PANE_NOT_FOUND_ENVELOPE}"
        )]
    );
    assert_eq!(
        argv_list(&calls),
        vec![
            vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
            vec!["pane", "run", "w1:p2", "clauth", "start", "acme"],
            vec!["tab", "close", "w1:t2"],
        ]
    );
}

/// A `tab create` that exits 0 with stdout that is not the envelope answers
/// 502 with the unparseable-envelope line, and no agent call follows.
#[test]
fn a_tab_create_with_a_non_envelope_stdout_is_502() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_control(true);
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok("{}"),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            502,
            serde_json::json!({
                "ok": false,
                "error": "herdr_refused",
                "reason": "herdr refused the request; see daemon.log",
            })
        )
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth api: device 'test' tab create answered an unparseable envelope: {}".to_string()
        ]
    );
    assert_eq!(
        argv_list(&calls),
        vec![vec!["tab", "create", "--cwd", &cwd, "--no-focus"]],
        "the agent call never ran"
    );
}

/// A cwd whose name carries a newline reaches the audit line flattened, so a
/// forged second line cannot ride the directory name.
#[test]
fn the_cwd_is_sanitized_in_the_audit_line() {
    let sb = HomeSandbox::new();
    let dir = sb.home().join("lo\ncked");
    std::fs::create_dir_all(&dir).expect("create dir");
    let cwd = dir.to_string_lossy().into_owned();
    seed_control(true);
    let (probe, _) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        ["agent", "start", _, "--kind", "claude", ..] => ok(""),
        ["pane", "get", "w1:p2"] => ok(PANE_GET_IDLE),
        _ => Answer::NeverRan,
    });
    let ctx = ctx_with_probe(probe);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req("POST", path(), Some(TOKEN), &body_with(&cwd, "{}")),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' created a session form='claude' cwd='{}' tab='w1:t2' pane='w1:p2'",
            cwd.replace('\n', " ")
        )]
    );
}

#[test]
fn agent_name_fits_herdrs_rule_past_p9() {
    // herdr 0.9.1 (measured 2026-09-26) names the tenth pane `w1:pA`, and
    // `agent start` refuses any uppercase name with `invalid_agent_name`.
    assert_eq!(agent_name("w1:p2"), "clauth-w1-p2");
    assert_eq!(agent_name("w1:pD"), "clauth-w1-p_d");
    assert_ne!(agent_name("w1:pD"), agent_name("w1:pd"));
    let ok = |n: &str| {
        n.len() <= 32
            && n.starts_with(|c: char| c.is_ascii_lowercase())
            && n.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    for id in ["w1:p1", "w1:pD", "w1N:p19", "wZ:pAB"] {
        assert!(ok(&agent_name(id)), "{id} -> {}", agent_name(id));
    }
}

// ---------------------------------------------------------------------------
/// The 503 path records the supervisor stop's cause (the tree path) in
/// daemon.log only, never in the body.
#[test]
fn a_supervisor_stop_logs_the_stopped_tree() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((
        false,
        "",
        "Error: clauth is stopping Claude Code's background service in \
         /home/u/.clauth/profiles/acme/runtime-bg; retry in a moment\n",
    )));
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(resp.status, 503, "{:?}", body_json(&resp));
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth api: device 'test' background launch met a supervisor stop: Error: clauth is \
             stopping Claude Code's background service in /home/u/.clauth/profiles/acme/runtime-bg; \
             retry in a moment"
                .to_string(),
        ]
    );
}

/// A descendant holding the child's stdout after it exits delays the answer
/// only a short drain, not the run's bound.
#[test]
fn the_launch_runner_drains_a_held_pipe_shortly_after_exit() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let pid_file = scratch.path().join("holder.pid");
    let mut command = std::process::Command::new("sh");
    command.args([
        "-c",
        &format!(
            "sleep 30 & echo $! > '{}'; echo 'backgrounded · 3fde53e3'",
            pid_file.display()
        ),
    ]);
    let started = std::time::Instant::now();
    let out = run_launch(&mut command, Duration::from_secs(10));
    let elapsed = started.elapsed();
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
    assert!(
        elapsed < Duration::from_secs(4),
        "a held pipe drains shortly after exit, not at the bound: {elapsed:?}"
    );
    let out = out.expect("the child itself exited, so the run answers");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        "backgrounded · 3fde53e3"
    );
}

/// `background_id` keeps only the 8-hex lowercase job id, so an uppercase id
/// on the `backgrounded · <id>` line never reads.
#[test]
fn background_id_keeps_only_lowercase_hex() {
    assert_eq!(
        background_id("backgrounded · 3fde53e3\n".as_bytes()),
        Some("3fde53e3".to_string())
    );
    assert_eq!(background_id("backgrounded · 3FDE53E3\n".as_bytes()), None);
}

/// A no-prompt launch prints a note after the id on the same line (measured on
/// Claude Code 2.1.318), and the id is still the whole first word: a longer or
/// shorter word never reads as one.
#[test]
fn background_id_reads_the_first_word_after_the_dot() {
    assert_eq!(
        background_id(BG_IDLE_STDOUT.as_bytes()),
        Some(BG_ID.to_string())
    );
    assert_eq!(
        background_id("backgrounded · 3fde53e3 some other note\n".as_bytes()),
        Some(BG_ID.to_string())
    );
    for stdout in [
        "backgrounded · 3fde53e3\r\n",
        "backgrounded · 3fde53e3\t(idle)\n",
        "backgrounded · bad\nbackgrounded · 3fde53e3\n",
        "backgrounded · 3fde53e3\nbackgrounded · aaaaaaaa\n",
    ] {
        assert_eq!(
            background_id(stdout.as_bytes()),
            Some(BG_ID.to_string()),
            "{stdout}"
        );
    }
    for stdout in [
        "backgrounded · (idle) 3fde53e3\n",
        "backgrounded · 3fde53e3a (idle)\n",
        "backgrounded · 3fde53e (idle)\n",
        "backgrounded · 3FDE53E3 (idle)\n",
        "backgrounded · (idle)\n",
    ] {
        assert_eq!(background_id(stdout.as_bytes()), None, "{stdout}");
    }
}

/// A create with no prompt starts an idle session and answers both ids, never
/// a 502 over a session that is running.
#[test]
fn background_with_no_prompt_answers_both_ids() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _calls) = herdr_untouched();
    let (spawn, _launches) = launch_seam(Some((true, BG_IDLE_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "background_id": BG_ID,
                "session_id": BG_SESSION,
            })
        )
    );
}

/// A quarantined codex login on the pane arm answers 409 `login_quarantined`
/// with `clauth start`'s own sentence, before any herdr call.
#[test]
fn a_quarantined_codex_login_is_refused_before_a_tab_opens() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_codex_roster(&["cx"]);
    crate::testutil::write_codex_store("cx", &crate::testutil::codex_auth_body("at.cx", "rt.cx"));
    crate::codex_auth::quarantine_for_test("cx", "reused", "rt.cx");
    let (probe, calls) = herdr_untouched();
    let (spawn, launches) = launch_seam(None);
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"cx"}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            409,
            serde_json::json!({
                "ok": false,
                "error": "login_quarantined",
                "reason": "'cx': codex chain is broken (reused since 1970-01-01T00:00:00Z), \
                           run `clauth login cx --codex --browser`",
            })
        )
    );
    assert!(calls_of(&calls).is_empty(), "no tab was opened");
    assert!(launches_of(&launches).is_empty(), "nothing was launched");
}

// `"background": true`, `"with_fallback": true` and `"prompt"`.
//
// The background arm launches through the context's `session_spawn` seam
// (never a real `clauth` or `claude`); every launch is recorded as
// `(launcher, argv, cwd)` for the pins.
// ---------------------------------------------------------------------------

/// `claude --bg`'s stdout as captured on Claude Code 2.1.303.
const BG_STDOUT: &str = "Starting background service…\nbackgrounded · 3fde53e3\n  claude attach 3fde53e3\n  claude logs 3fde53e3\n  claude stop 3fde53e3\n";
/// The head of what a no-prompt `claude --bg` printed on 2.1.318.
const BG_IDLE_STDOUT: &str = "backgrounded · 3fde53e3 (idle — send a prompt to start)\n  claude agents             list sessions\n";
const BG_ID: &str = "3fde53e3";
/// The `sessionId` `testutil::write_claude_job` writes for [`BG_ID`].
const BG_SESSION: &str = "3fde53e3-0a6e-4b11-883e-fca3a218ae52";

type Launch = (Launcher, Vec<String>, std::path::PathBuf);
type Launches = Arc<Mutex<Vec<Launch>>>;
/// What a launch answers: `None` = never answered, else `(success, stdout, stderr)`.
type LaunchAnswer = Option<(bool, &'static str, &'static str)>;

/// A launch seam recording every call and answering `answer`.
fn launch_seam(answer: LaunchAnswer) -> (SessionSpawn, Launches) {
    let launches: Launches = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&launches);
    let spawn: SessionSpawn = Arc::new(move |launcher, args, cwd| {
        recorded.lock().unwrap().push((
            launcher,
            args.iter().map(|arg| arg.to_string()).collect(),
            cwd.to_path_buf(),
        ));
        answer.map(|(success, stdout, stderr)| LaunchOut {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    });
    (spawn, launches)
}

fn launches_of(launches: &Launches) -> Vec<Launch> {
    launches.lock().unwrap().clone()
}

/// A context whose herdr seam answers `herdr` and whose launch seam is `spawn`.
fn ctx_bg(probe: PaneProbe, spawn: SessionSpawn) -> Arc<ApiContext> {
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::for_tests_with_spawn(config(), status_path, probe, spawn)
}

/// Stored profiles: `oauth` without a base url, `api` with one, then the
/// `[serve]` key on, `chain` as the fallback chain, and a control device with
/// the sessions grant.
fn seed_accounts(oauth: &[&str], api: &[&str], chain: &[&str]) {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    for name in oauth {
        crate::actions::create_blank_profile(
            &mut config,
            (*name).to_string(),
            None,
            None,
            Default::default(),
            None,
        )
        .expect("create oauth profile");
    }
    for name in api {
        crate::actions::create_blank_profile(
            &mut config,
            (*name).to_string(),
            Some("https://api.example.test".to_string()),
            Some("fixture-key".to_string()),
            Default::default(),
            None,
        )
        .expect("create api profile");
    }
    let mut state = crate::profile::load_app_state().expect("load app state");
    state.serve.session_creation = true;
    state.fallback_chain = chain.iter().map(|n| (*n).into()).collect();
    crate::profile::save_app_state(&state).expect("save app state");
    seed_device(DEVICE, Tier::Control, TOKEN);
    crate::daemon::api::devices::allow_sessions(DEVICE).expect("grant sessions");
}

fn herdr_untouched() -> (PaneProbe, Calls) {
    seam(|_| Answer::NeverRan)
}

/// A background create on a profile runs `clauth start <p> -- --bg <prompt>`
/// in the body's cwd, reads the printed job id and the record's `sessionId`,
/// answers both, and never asks herdr.
#[test]
fn background_on_a_profile_launches_clauth_start_bg_and_answers_both_ids() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, calls) = herdr_untouched();
    let (spawn, launches) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(
                &cwd,
                r#"{"profile":"acme","background":true,"prompt":"hello there"}"#,
            ),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "background_id": BG_ID,
                "session_id": BG_SESSION,
            })
        )
    );
    assert_eq!(
        launches_of(&launches),
        vec![(
            Launcher::Clauth,
            vec!["start", "acme", "--", "--bg", "--", "hello there"]
                .into_iter()
                .map(String::from)
                .collect(),
            std::path::PathBuf::from(&cwd),
        )]
    );
    assert!(calls_of(&calls).is_empty(), "herdr was never asked");
}

/// No profile is a plain `claude --bg` on `~/.claude`, the linked account; no
/// prompt means none on the argv (an idle session).
#[test]
fn background_without_a_profile_runs_plain_claude_bg() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, launches) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"background":true}"#),
        ),
    );
    assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
    assert_eq!(body_json(&resp)["background_id"], serde_json::json!(BG_ID));
    assert_eq!(
        launches_of(&launches),
        vec![(
            Launcher::Claude,
            vec!["--bg".to_string()],
            std::path::PathBuf::from(&cwd),
        )]
    );
}

/// `with_fallback` on the background arm passes `--with-fallback` to the
/// start, on a chain-eligible account with a daemon held.
#[test]
fn background_with_fallback_launches_the_chain_flag() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme", "beta"], &[], &["acme", "beta"]);
    let _daemon = crate::daemon::hold_daemon_lock();
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, launches) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(
                &cwd,
                r#"{"profile":"acme","background":true,"with_fallback":true}"#,
            ),
        ),
    );
    assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
    assert_eq!(
        launches_of(&launches)
            .into_iter()
            .map(|(launcher, argv, _)| (launcher, argv))
            .collect::<Vec<_>>(),
        vec![(
            Launcher::Clauth,
            vec!["start", "acme", "--with-fallback", "--", "--bg"]
                .into_iter()
                .map(String::from)
                .collect(),
        )]
    );
}

/// `with_fallback` on the pane arm runs `clauth start <p> --with-fallback` in
/// the pane, launches nothing in the background, and its
/// audit line says so.
#[test]
fn pane_with_fallback_runs_clauth_start_with_fallback() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme", "beta"], &[], &["acme", "beta"]);
    let _daemon = crate::daemon::hold_daemon_lock();
    let (probe, calls) = seam(|args| match args {
        ["tab", "create", ..] => ok(TAB_CREATED),
        [
            "pane",
            "run",
            "w1:p2",
            "clauth",
            "start",
            "acme",
            "--with-fallback",
        ] => ok(""),
        _ => Answer::NeverRan,
    });
    let (spawn, launches) = launch_seam(None);
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","with_fallback":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "workspace_id": "w1",
                "tab_id": "w1:t2",
                "pane_id": "w1:p2",
                "agent_status": null,
            })
        )
    );
    assert_eq!(
        argv_list(&calls),
        vec![
            vec!["tab", "create", "--cwd", &cwd, "--no-focus"],
            vec![
                "pane",
                "run",
                "w1:p2",
                "clauth",
                "start",
                "acme",
                "--with-fallback"
            ],
        ]
    );
    assert!(
        launches_of(&launches).is_empty(),
        "nothing launched in the background"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' created a session form='profile:acme' with_fallback=true \
             cwd='{cwd}' tab='w1:t2' pane='w1:p2'"
        )]
    );
}

/// Every new body refusal answers 400 with its own fixed sentence before herdr
/// or a launch is reached.
#[test]
fn background_body_refusals_each_name_their_field() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_codex_roster(&["cx"]);
    let (probe, calls) = herdr_untouched();
    let (spawn, launches) = launch_seam(None);
    let ctx = ctx_bg(probe, spawn);

    let cases = [
        (
            r#"{"background":true,"kind":"codex"}"#,
            "background sessions run Claude Code; optionally pick a profile, not a kind",
        ),
        (
            r#"{"background":true,"workspace":"w1"}"#,
            "background sessions do not open a herdr tab; drop the workspace",
        ),
        (
            r#"{"background":true,"profile":"cx"}"#,
            "background sessions run Claude Code; codex profiles are not supported",
        ),
        (
            r#"{"with_fallback":true,"kind":"codex"}"#,
            "with_fallback follows a clauth profile's chain; pick a profile, not a kind",
        ),
        (
            r#"{"with_fallback":true,"background":true}"#,
            "with_fallback needs a profile to start on",
        ),
        (
            r#"{"with_fallback":true}"#,
            "with_fallback needs a profile to start on",
        ),
        (
            r#"{"prompt":"hi"}"#,
            "prompt is for background sessions only; prompt a pane through its own prompt route",
        ),
        (
            r#"{"background":true,"prompt":""}"#,
            "prompt must be non-empty and hold no NUL byte",
        ),
        (
            r#"{"background":true,"prompt":"a\u0000b"}"#,
            "prompt must be non-empty and hold no NUL byte",
        ),
    ];
    for (extra, reason) in cases {
        let resp = call(
            &ctx,
            &req("POST", path(), Some(TOKEN), &body_with(&cwd, extra)),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                400,
                serde_json::json!({"ok": false, "error": "bad_request", "reason": reason})
            ),
            "{extra}"
        );
    }
    assert!(calls_of(&calls).is_empty(), "herdr was never asked");
    assert!(launches_of(&launches).is_empty(), "nothing was launched");
}

/// `clauth start`'s own refusals answer 409 with the sentence the CLI prints,
/// on both arms, before a tab opens or a launch runs.
#[test]
fn start_refusals_answer_409_before_anything_spawns() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme", "solo"], &["third"], &["acme"]);
    {
        let mut config = crate::profile::load_config().expect("load config");
        crate::actions::disable_profile(&mut config, &"acme".into()).expect("disable");
    }
    let _daemon = crate::daemon::hold_daemon_lock();
    let (probe, calls) = herdr_untouched();
    let (spawn, launches) = launch_seam(None);
    let ctx = ctx_bg(probe, spawn);

    let cases = [
        (
            r#"{"profile":"acme","background":true}"#,
            "profile_disabled",
            "'acme': account is disabled".to_string(),
        ),
        (
            r#"{"profile":"acme"}"#,
            "profile_disabled",
            "'acme': account is disabled".to_string(),
        ),
        (
            r#"{"profile":"third","background":true,"with_fallback":true}"#,
            "with_fallback_unavailable",
            "'third': --with-fallback needs an OAuth account, but this one has a custom base url"
                .to_string(),
        ),
        (
            r#"{"profile":"third","with_fallback":true}"#,
            "with_fallback_unavailable",
            "'third': --with-fallback needs an OAuth account, but this one has a custom base url"
                .to_string(),
        ),
        (
            r#"{"profile":"solo","with_fallback":true}"#,
            "with_fallback_unavailable",
            "'solo': --with-fallback needs a fallback-chain member; add 'solo' on the fallback \
             tab of the `clauth` TUI, or start without it"
                .to_string(),
        ),
    ];
    for (extra, code, reason) in cases {
        let resp = call(
            &ctx,
            &req("POST", path(), Some(TOKEN), &body_with(&cwd, extra)),
        );
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                409,
                serde_json::json!({"ok": false, "error": code, "reason": reason})
            ),
            "{extra}"
        );
    }
    assert!(calls_of(&calls).is_empty(), "no tab was opened");
    assert!(launches_of(&launches).is_empty(), "nothing was launched");
}

/// A copy host refuses a background create with `background_unavailable` and
/// the CLI's copy-host sentence.
#[test]
fn a_copy_host_refuses_a_background_create() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    let _fake = crate::runtime::ForcedFakeLinkMode::new();
    let (probe, _) = herdr_untouched();
    let (spawn, launches) = launch_seam(None);
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            409,
            serde_json::json!({
                "ok": false,
                "error": "background_unavailable",
                "reason": crate::runtime::copy_host_background_refusal("acme", "--bg"),
            })
        )
    );
    assert!(launches_of(&launches).is_empty(), "nothing was launched");
}

/// A launch that fails, never answers, or prints no job id answers 502 with
/// the fixed sentence; the cause reaches daemon.log only.
#[test]
fn a_failed_launch_answers_502_and_logs_the_cause() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    let refused = serde_json::json!({
        "ok": false,
        "error": "background_refused",
        "reason": "the background session did not start; see daemon.log",
    });
    let body = body_with(&cwd, r#"{"profile":"acme","background":true}"#);

    let cases: [(LaunchAnswer, String); 3] = [
        (
            Some((
                false,
                "",
                "'--bg' is disabled by the 'disableAgentView' setting.",
            )),
            "clauth api: device 'test' background launch refused: '--bg' is disabled by the \
             'disableAgentView' setting."
                .to_string(),
        ),
        (
            None,
            "clauth api: device 'test' background launch did not answer (deadline or spawn \
             failure)"
                .to_string(),
        ),
        (
            Some((true, "Starting background service…\n", "")),
            "clauth api: device 'test' background launch printed no job id: Starting background \
             service…"
                .to_string(),
        ),
    ];
    for (answer, logged) in cases {
        let (probe, _) = herdr_untouched();
        let (spawn, _) = launch_seam(answer);
        let ctx = ctx_bg(probe, spawn);
        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        let resp = call(&ctx, &req("POST", path(), Some(TOKEN), &body));
        assert_eq!((resp.status, body_json(&resp)), (502, refused.clone()));
        assert_eq!(lines.snapshot(), vec![logged]);
    }
}

/// A launch refused while clauth stops that tree's background service answers
/// 503 with the path-free sentence.
#[test]
fn a_launch_meeting_a_supervisor_stop_answers_503() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((
        false,
        "",
        "Error: clauth is stopping Claude Code's background service in /home/u/.clauth/profiles/acme/runtime-bg; retry in a moment\n",
    )));
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            503,
            serde_json::json!({
                "ok": false,
                "error": "background_busy",
                "reason": "clauth is stopping this account's background service; retry in a moment",
            })
        )
    );
}

/// A record that never reads within the bound still answers 200: the session
/// started, so `session_id` is `null` rather than a failure. The bound stays
/// short.
#[test]
fn a_record_that_never_reads_answers_a_null_session_id() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    let started = std::time::Instant::now();
    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({"ok": true, "background_id": BG_ID, "session_id": null})
        )
    );
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "the record wait is bounded: {:?}",
        started.elapsed()
    );
}

/// One audit line per background creation: device, form, with_fallback, cwd,
/// both ids and the prompt's length, never its text.
#[test]
fn the_background_audit_line_names_ids_and_prompt_length_only() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(
                &cwd,
                r#"{"profile":"acme","background":true,"prompt":"secret plan"}"#,
            ),
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' created a background session form='profile:acme' \
             with_fallback=false cwd='{cwd}' background_id='{BG_ID}' session_id='{BG_SESSION}' \
             prompt_len=11"
        )]
    );
}

/// A successful launch's stderr — where `clauth start` names each store it
/// could not create — reaches the daemon log one line per line of stderr, so
/// no note hides another behind the per-line bound, and the 200 answer carries
/// no new field.
#[test]
fn a_successful_background_launch_logs_each_stderr_line() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((
        true,
        BG_IDLE_STDOUT,
        "clauth: could not create /home/u/.claude/jobs: Permission denied (os error 13); this session's background jobs stay in its own tree\n\
         clauth: could not create /home/u/.claude/projects: Permission denied (os error 13); this session's transcripts stay in its own tree\n\
         \n\
         clauth: could not create /home/u/.claude/sessions: Permission denied (os error 13); this session's background sessions stay there\n\
         a\x07b\n",
    )));
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "background_id": BG_ID,
                "session_id": BG_SESSION,
            })
        )
    );
    let said = "clauth api: device 'test' background launch said:";
    assert_eq!(
        lines.snapshot(),
        vec![
            format!(
                "{said} clauth: could not create /home/u/.claude/jobs: Permission denied (os \
                 error 13); this session's background jobs stay in its own t..."
            ),
            format!(
                "{said} clauth: could not create /home/u/.claude/projects: Permission denied (os \
                 error 13); this session's transcripts stay in its own t..."
            ),
            format!(
                "{said} clauth: could not create /home/u/.claude/sessions: Permission denied (os \
                 error 13); this session's background sessions stay ther..."
            ),
            format!("{said} a b"),
            format!(
                "clauth api: device 'test' created a background session form='profile:acme' \
                 with_fallback=false cwd='{cwd}' background_id='{BG_ID}' \
                 session_id='{BG_SESSION}' prompt_len=0"
            ),
        ]
    );
}

/// A child printing more than the cap costs a bounded number of log lines.
#[test]
fn a_chatty_background_launch_logs_a_bounded_number_of_lines() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((
        true,
        BG_IDLE_STDOUT,
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n",
    )));
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "background_id": BG_ID,
                "session_id": BG_SESSION,
            })
        )
    );
    let mut expected: Vec<String> = (1..=8)
        .map(|n| format!("clauth api: device 'test' background launch said: {n}"))
        .collect();
    expected.push(format!(
        "clauth api: device 'test' created a background session form='profile:acme' \
         with_fallback=false cwd='{cwd}' background_id='{BG_ID}' \
         session_id='{BG_SESSION}' prompt_len=0"
    ));
    assert_eq!(lines.snapshot(), expected,);
}

/// A launch that prints nothing on stderr adds no such line: the audit line is
/// the only one.
#[test]
fn a_silent_background_launch_logs_no_stderr_line() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((true, BG_IDLE_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({
                "ok": true,
                "background_id": BG_ID,
                "session_id": BG_SESSION,
            })
        )
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' created a background session form='profile:acme' \
             with_fallback=false cwd='{cwd}' background_id='{BG_ID}' \
             session_id='{BG_SESSION}' prompt_len=0"
        )]
    );
}

/// The production launch command: `current_exe()` for the clauth arm, the
/// body's cwd, stdin closed, and the session env a daemon may carry removed
/// (herdr's five pane vars, the Claude Code session and config vars, codex's
/// home), so the child targets the user's own home and no herdr pane.
#[test]
fn the_launch_command_strips_the_session_env_and_runs_in_the_cwd() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    let cwd = std::path::Path::new(&cwd);

    let clauth = launch_command(Launcher::Clauth, &["start", "acme", "--", "--bg"], cwd);
    assert_eq!(
        clauth.get_program(),
        std::env::current_exe().expect("current exe").as_os_str()
    );
    assert_eq!(
        clauth.get_args().collect::<Vec<_>>(),
        vec!["start", "acme", "--", "--bg"]
    );
    for command in [clauth, launch_command(Launcher::Claude, &["--bg"], cwd)] {
        assert_eq!(command.get_current_dir(), Some(cwd));
        let removed: std::collections::BTreeSet<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in [
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_PANE_ID",
            "HERDR_TAB_ID",
            "HERDR_WORKSPACE_ID",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_PID",
            "CODEX_HOME",
        ] {
            assert!(removed.contains(key), "{key} is removed: {removed:?}");
        }
        // The restore marker and the degrade notice pair belong to the one
        // claude a restore or a degraded resume launched, never a session the
        // daemon starts.
        for key in [
            crate::hook_resume::RESTORE_MARKER_ENV,
            crate::hook_resume::DEGRADE_NOTICE_ENV,
            crate::hook_resume::DEGRADE_MARKER_ENV,
        ] {
            assert!(removed.contains(key), "{key} is removed: {removed:?}");
        }
    }
    let claude = launch_command(Launcher::Claude, &["--bg"], cwd);
    let removed: std::collections::BTreeSet<String> = claude
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    for key in crate::runtime::MANAGED_ENV_KEYS {
        assert!(
            removed.contains(*key),
            "the plain-claude arm drops the managed {key}: {removed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Prompt placement, roster resolution, the relayed session id and the launch
// runner's pipe handling.
// ---------------------------------------------------------------------------

/// A prompt always follows a `--`, so one starting with `-` is a prompt and
/// never a `claude` option (`claude -p -- --help` takes `--help` as the
/// prompt, measured 2026-10-27), on both launch arms.
#[test]
fn a_prompt_always_follows_a_separator() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, launches) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    for extra in [
        r#"{"profile":"acme","background":true,"prompt":"--dangerously-skip-permissions"}"#,
        r#"{"background":true,"prompt":"-v flag is broken"}"#,
    ] {
        let resp = call(
            &ctx,
            &req("POST", path(), Some(TOKEN), &body_with(&cwd, extra)),
        );
        assert_eq!(resp.status, 200, "{extra}: {:?}", body_json(&resp));
    }
    assert_eq!(
        launches_of(&launches)
            .into_iter()
            .map(|(launcher, argv, _)| (launcher, argv))
            .collect::<Vec<_>>(),
        vec![
            (
                Launcher::Clauth,
                vec![
                    "start",
                    "acme",
                    "--",
                    "--bg",
                    "--",
                    "--dangerously-skip-permissions"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
            ),
            (
                Launcher::Claude,
                vec!["--bg", "--", "-v flag is broken"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
        ]
    );
}

/// A codex profile with `with_fallback` on the pane arm is refused before a
/// tab opens, with the sentence `clauth start` prints for it.
#[test]
fn a_codex_profile_with_fallback_is_refused_before_a_tab_opens() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    crate::testutil::write_codex_roster(&["cx"]);
    let (probe, calls) = herdr_untouched();
    let (spawn, launches) = launch_seam(None);
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"cx","with_fallback":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            409,
            serde_json::json!({
                "ok": false,
                "error": "with_fallback_unavailable",
                "reason": "--with-fallback is not available on a codex profile: codex reads \
                           auth.json once at start, so a chain lands at the NEXT start, not \
                           mid-session. Start without the flag",
            })
        )
    );
    assert!(calls_of(&calls).is_empty(), "no tab was opened");
    assert!(launches_of(&launches).is_empty(), "nothing was launched");
}

/// A name in both rosters resolves to the claude profile first, as
/// `clauth start` does, so a background create launches it.
#[test]
fn a_name_in_both_rosters_resolves_to_the_claude_profile() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["dup"], &[], &[]);
    crate::testutil::write_codex_roster(&["dup"]);
    crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let (probe, _) = herdr_untouched();
    let (spawn, launches) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"dup","background":true}"#),
        ),
    );
    assert_eq!(resp.status, 200, "{:?}", body_json(&resp));
    assert_eq!(
        launches_of(&launches)
            .into_iter()
            .map(|(launcher, argv, _)| (launcher, argv))
            .collect::<Vec<_>>(),
        vec![(
            Launcher::Clauth,
            vec!["start", "dup", "--", "--bg"]
                .into_iter()
                .map(String::from)
                .collect(),
        )]
    );
}

/// A job record whose `sessionId` is not a Claude Code session id (a uuid:
/// 8-4-4-4-12 lowercase hex) is never relayed: the answer carries `null` and
/// the audit line `session_id=null`, so a forged record cannot write a line
/// into daemon.log or a value into the app.
#[test]
fn a_session_id_that_is_not_a_uuid_is_never_relayed() {
    let sb = HomeSandbox::new();
    let cwd = cwd(&sb);
    seed_accounts(&["acme"], &[], &[]);
    let dir = crate::testutil::write_claude_job(sb.home(), BG_ID, &sb.home().join(".claude"));
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    record["sessionId"] = serde_json::json!("x\nclauth api: device 'forged' created a session");
    std::fs::write(dir.join("state.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    let (probe, _) = herdr_untouched();
    let (spawn, _) = launch_seam(Some((true, BG_STDOUT, "")));
    let ctx = ctx_bg(probe, spawn);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            path(),
            Some(TOKEN),
            &body_with(&cwd, r#"{"profile":"acme","background":true}"#),
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (
            200,
            serde_json::json!({"ok": true, "background_id": BG_ID, "session_id": null})
        )
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device 'test' created a background session form='profile:acme' \
             with_fallback=false cwd='{cwd}' background_id='{BG_ID}' session_id=null \
             prompt_len=0"
        )]
    );
}

/// The production launch runner drains both pipes while the child runs: a
/// child writing far past one pipe buffer to stderr still answers at once,
/// with its stdout and the start of its stderr.
#[test]
fn the_launch_runner_drains_a_flooding_child() {
    let mut command = std::process::Command::new("sh");
    command.args([
        "-c",
        "head -c 300000 /dev/zero | tr '\\0' x >&2; echo 'backgrounded · 3fde53e3'",
    ]);
    let started = std::time::Instant::now();
    let out = run_launch(&mut command, Duration::from_secs(20)).expect("the run answers");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a flood never stalls the run: {:?}",
        started.elapsed()
    );
    assert!(out.success);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        "backgrounded · 3fde53e3"
    );
    assert!(
        out.stderr.starts_with(b"xxxx"),
        "stderr's start is kept for daemon.log"
    );
}

/// A descendant that keeps the child's stdout open (a daemonizing supervisor
/// that did not close it) cannot hold the request past the run's bound.
#[test]
fn the_launch_runner_answers_within_its_bound_when_a_descendant_holds_a_pipe() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let pid_file = scratch.path().join("holder.pid");
    let mut command = std::process::Command::new("sh");
    command.args([
        "-c",
        &format!(
            "sleep 30 & echo $! > '{}'; echo 'backgrounded · 3fde53e3'",
            pid_file.display()
        ),
    ]);
    let bound = Duration::from_secs(3);
    let started = std::time::Instant::now();
    let out = run_launch(&mut command, bound);
    let elapsed = started.elapsed();
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
    assert!(
        elapsed < bound + Duration::from_secs(2),
        "a held pipe never outlasts the bound: {elapsed:?}"
    );
    let out = out.expect("the child itself exited, so the run answers");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        "backgrounded · 3fde53e3"
    );
}
