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
