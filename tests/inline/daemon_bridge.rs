#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The mod bridge: `~/.clauth/bridge.sock`'s bind rules, the session a socket
//! peer resolves to, and the control routes that queue commands for a mod.
//!
//! The peer resolver is a table the test owns (pid -> instance), so a test can
//! pose a second process on one id, a `/clear` moving an id, or an unknown
//! peer; the one end-to-end test resolves the test process's own pid as the
//! socket reports it.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, OTHER_TOKEN, TOKEN, body_json, call, req, seed_device};

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

fn instance(session_id: &str, proc_start: &str) -> Instance {
    Instance {
        session_id: session_id.to_string(),
        proc_start: proc_start.to_string(),
    }
}

/// A resolver over a table the test can rewrite mid-test.
fn table(rows: &[(u32, Instance)]) -> (Table, PeerResolver) {
    let table: Table = Arc::new(Mutex::new(rows.iter().cloned().collect()));
    let read = Arc::clone(&table);
    let resolver: PeerResolver = Box::new(move |pid| read.lock().unwrap().get(&pid).cloned());
    (table, resolver)
}

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

/// herdr absent: a session with no mod has nothing to fall back to.
fn no_herdr() -> PaneProbe {
    Box::new(|_, _| HerdrProbeOut::NotInstalled)
}

fn ctx_over(hub: &Arc<Hub>) -> Arc<ApiContext> {
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::for_tests_with_bridge(config(), status_path, no_herdr(), Arc::clone(hub))
}

fn start(hub: &Hub, pid: u32, session: &str) {
    let body = serde_json::json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "start for {session} from {pid}: {answer}");
}

fn commands(hub: &Hub, pid: u32, session: &str) -> serde_json::Value {
    let (status, answer) = hub.poll(pid, session, Duration::ZERO);
    assert_eq!(status, 200, "poll {session} from {pid}: {answer}");
    answer["commands"].clone()
}

fn open_ask(hub: &Hub, pid: u32, session: &str, seq: u64, ask: &str) -> serde_json::Value {
    let body = serde_json::json!({"session": session, "seq": seq, "kind": "ask_opened", "ask": ask, "tool": "Bash", "input": {"command": "ls"}});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "ask_opened {ask}: {answer}");
    answer
}

/// The socket is the daemon's own: 0600 in a 0700 dir.
#[test]
fn the_socket_is_owner_only_in_an_owner_only_dir() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    let _listener = bind(&path).expect("bind");
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    let sock_mode = std::fs::symlink_metadata(&path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
    assert_eq!(sock_mode & 0o777, 0o600);
}

/// A path a live listener holds is never unlinked: the bind refuses and the
/// incumbent keeps answering.
#[test]
fn a_live_listener_at_the_path_is_refused() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let squatter = UnixListener::bind(&path).unwrap();
    assert!(bind(&path).is_err(), "bound over a live listener");
    UnixStream::connect(&path).expect("the incumbent still answers");
    drop(squatter);
}

/// A crashed daemon's file (no listener behind it) is replaced.
#[test]
fn a_stale_socket_file_is_replaced() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists(), "the stale file is in place");
    let _listener = bind(&path).expect("a stale file is replaced");
    UnixStream::connect(&path).expect("the new listener answers");
}

/// The session id is resolved from the peer, never taken off the wire: an
/// event naming another session is refused and registers nothing.
#[test]
fn an_event_naming_another_session_is_refused() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 200, "s-two");

    let forged = serde_json::json!({"session": "s-two", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    let (status, answer) = hub.event(100, forged.to_string().as_bytes());
    assert_eq!(status, 403);
    assert_eq!(answer["error"], "session_mismatch");

    let (status, answer) = hub.poll(100, "s-two", Duration::ZERO);
    assert_eq!(status, 403);
    assert_eq!(answer["error"], "session_mismatch");
}

/// A peer the resolver knows nothing about is refused.
#[test]
fn an_unknown_peer_is_refused() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let body = serde_json::json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    let (status, answer) = hub.event(300, body.to_string().as_bytes());
    assert_eq!(status, 403);
    assert_eq!(answer["error"], "unknown_peer");
}

/// A prompt from a control device reaches the named session's poll once,
/// and no other session's.
#[test]
fn a_prompt_reaches_only_its_own_sessions_poll() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-two");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"fix it"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        body_json(&resp),
        serde_json::json!({"ok": true, "via": "mod"})
    );

    assert_eq!(commands(&hub, 200, "s-two"), serde_json::json!([]));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "prompt", "text": "fix it"}])
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([]),
        "delivered once"
    );
}

/// An interrupt reaches the mod as its own command.
#[test]
fn an_interrupt_reaches_the_mod() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/interrupt",
            Some(TOKEN),
            "{}",
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        body_json(&resp),
        serde_json::json!({"ok": true, "via": "mod"})
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "interrupt"}])
    );
}

/// The audit line names the device, the session and the text's length,
/// never the text.
#[test]
fn the_prompt_audit_line_carries_no_text() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"secret-canary"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device '{DEVICE}' prompted session 's-one' via mod text_len=13"
        )]
    );
}

/// Every command route is control tier; a view device queues nothing.
#[test]
fn a_view_device_cannot_drive_a_session() {
    let _home = HomeSandbox::new();
    seed_device("viewer", Tier::View, OTHER_TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);

    for (path, body) in [
        ("/api/v1/sessions/s-one/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-one/interrupt", "{}"),
        ("/api/v1/sessions/s-one/asks/a1", r#"{"decision":"allow"}"#),
    ] {
        let resp = call(&ctx, &req("POST", path, Some(OTHER_TOKEN), body));
        assert_eq!(resp.status, 403, "{path}");
        assert_eq!(body_json(&resp)["error"], "control_required", "{path}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// An ask is answered once: the answer reaches the mod, a second answer and
/// an unknown ask are refused by name.
#[test]
fn an_ask_is_answered_once() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    assert_eq!(open_ask(&hub, 100, "s-one", 2, "a1")["hold"], true);
    let ctx = ctx_over(&hub);

    let first = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/a1",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(first.status, 200);
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "a1", "decision": "allow"}])
    );

    let second = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/a1",
            Some(TOKEN),
            r#"{"decision":"deny"}"#,
        ),
    );
    assert_eq!(second.status, 409);
    assert_eq!(body_json(&second)["error"], "ask_closed");

    let unknown = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/zz",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(unknown.status, 404);
    assert_eq!(body_json(&unknown)["error"], "ask_not_found");
}

const LOGO: &str = "Which color do you want for the logo?";
const FONT: &str = "Which font?";

/// Open an `AskUserQuestion` ask the way the mod posts one: its `input` is the
/// tool's `questions` array, each entry naming its text under `question`.
fn open_question(hub: &Hub, pid: u32, session: &str, seq: u64, ask: &str) {
    let questions = serde_json::json!([
        {"question": LOGO, "header": "Logo", "multiSelect": false, "options": [{"label": "Red"}, {"label": "Blue"}]},
        {"question": FONT, "header": "Font", "multiSelect": true, "options": [{"label": "Sans"}, {"label": "Mono"}]}
    ]);
    let body = serde_json::json!({"session": session, "seq": seq, "kind": "ask_opened", "ask": ask, "tool": "AskUserQuestion", "input": questions});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "ask_opened {ask}: {answer}");
}

fn answer_question(
    ctx: &ApiContext,
    ask: &str,
    body: &serde_json::Value,
) -> crate::daemon::api::http::Response {
    call(
        ctx,
        &req(
            "POST",
            &format!("/api/v1/sessions/s-one/asks/{ask}"),
            Some(TOKEN),
            &body.to_string(),
        ),
    )
}

fn question_hub() -> (Arc<Hub>, Arc<ApiContext>, Table) {
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_question(&hub, 100, "s-one", 2, "q1");
    let ctx = ctx_over(&hub);
    (hub, ctx, t)
}

/// An `AskUserQuestion` ask takes its answers keyed by each question's exact
/// text; a question left unanswered is allowed, and no note means no `notes`.
#[test]
fn a_question_ask_takes_answers() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    let resp = answer_question(&ctx, "q1", &serde_json::json!({"answers": {LOGO: "Red"}}));
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q1", "answers": {LOGO: "Red"}}])
    );
}

/// Notes ride beside the answers, keyed like them, and reach the mod in the
/// command; an empty note counts as no note, so a body whose every note is
/// empty queues no `notes` at all.
#[test]
fn a_question_answer_carries_its_notes() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    let resp = answer_question(
        &ctx,
        "q1",
        &serde_json::json!({"answers": {LOGO: "Red", FONT: "Sans, Mono"}, "notes": {LOGO: "a darker red", FONT: ""}}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q1", "answers": {LOGO: "Red", FONT: "Sans, Mono"}, "notes": {LOGO: "a darker red"}}])
    );

    open_question(&hub, 100, "s-one", 3, "q2");
    let resp = answer_question(
        &ctx,
        "q2",
        &serde_json::json!({"answers": {LOGO: "Blue"}, "notes": {LOGO: ""}}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q2", "answers": {LOGO: "Blue"}}])
    );

    // An empty note keyed by no question is no note either: dropped, never
    // refused as an unknown question.
    open_question(&hub, 100, "s-one", 4, "q3");
    let resp = answer_question(
        &ctx,
        "q3",
        &serde_json::json!({"answers": {LOGO: "Blue"}, "notes": {"Logo": ""}}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q3", "answers": {LOGO: "Blue"}}])
    );
}

/// A note on a question left unanswered is allowed: Claude Code shows that
/// question to the model as `(no option selected)` with the note.
#[test]
fn a_note_may_stand_on_an_unanswered_question() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    let resp = answer_question(
        &ctx,
        "q1",
        &serde_json::json!({"answers": {LOGO: "Red"}, "notes": {FONT: "pick for me"}}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q1", "answers": {LOGO: "Red"}, "notes": {FONT: "pick for me"}}])
    );
}

const UNKNOWN_QUESTION: &str =
    "each answer and note must name one of the ask's questions by its exact text.";
const EMPTY_ANSWER: &str = "each answer must be a non-empty string.";

/// An answer or a note keyed by anything but a question's exact text is
/// refused by name before anything is queued, and the ask stays open: the
/// same device's corrected answer then goes through.
#[test]
fn an_answer_or_note_naming_no_question_is_refused() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    for body in [
        serde_json::json!({"answers": {"Which colour?": "Red"}}),
        serde_json::json!({"answers": {"which color do you want for the logo?": "Red"}}),
        serde_json::json!({"answers": {format!("{LOGO} "): "Red"}}),
        serde_json::json!({"answers": {LOGO: "Red", "Logo": "Blue"}}),
        serde_json::json!({"answers": {LOGO: "Red"}, "notes": {"Logo": "darker"}}),
    ] {
        let resp = answer_question(&ctx, "q1", &body);
        assert_eq!(resp.status, 400, "{body}");
        let got = body_json(&resp);
        assert_eq!(got["error"], "answer_unknown_question", "{body}");
        assert_eq!(got["reason"], UNKNOWN_QUESTION, "{body}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));

    let resp = answer_question(&ctx, "q1", &serde_json::json!({"answers": {LOGO: "Red"}}));
    assert_eq!(
        resp.status,
        200,
        "the ask stayed open: {}",
        body_json(&resp)
    );
}

/// An empty answer is refused by name, the ask kept open; a key naming no
/// question is the first refusal when both are wrong.
#[test]
fn an_empty_answer_is_refused() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    let resp = answer_question(&ctx, "q1", &serde_json::json!({"answers": {LOGO: ""}}));
    assert_eq!(resp.status, 400);
    let got = body_json(&resp);
    assert_eq!(got["error"], "answer_empty");
    assert_eq!(got["reason"], EMPTY_ANSWER);

    let resp = answer_question(
        &ctx,
        "q1",
        &serde_json::json!({"answers": {LOGO: "", "Which colour?": "Red"}}),
    );
    assert_eq!(resp.status, 400);
    assert_eq!(body_json(&resp)["error"], "answer_unknown_question");
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));

    let resp = answer_question(&ctx, "q1", &serde_json::json!({"answers": {LOGO: "Red"}}));
    assert_eq!(
        resp.status,
        200,
        "the ask stayed open: {}",
        body_json(&resp)
    );
}

/// A whitespace-only answer is a non-empty string, so it is taken like any
/// other: `answer_empty` is the empty string alone.
#[test]
fn a_whitespace_only_answer_is_taken() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    let resp = answer_question(&ctx, "q1", &serde_json::json!({"answers": {LOGO: " "}}));
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q1", "answers": {LOGO: " "}}])
    );
}

/// Question texts compare byte for byte, never trimmed or case-folded: a text
/// with surrounding whitespace, one with a newline and one outside ASCII each
/// take only their exact key, while a trimmed copy of the whitespace one and a
/// case-folded copy of the non-ASCII one are refused.
#[test]
fn a_question_text_compares_byte_for_byte() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let spaced = "  Which colour?  ";
    let multiline = "Which font?\nWhich weight?";
    let nonascii = "Quel est le nom du café ?";
    let questions = serde_json::json!([
        {"question": spaced},
        {"question": multiline},
        {"question": nonascii}
    ]);
    let opened = serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "q1", "tool": "AskUserQuestion", "input": questions});
    assert_eq!(hub.event(100, opened.to_string().as_bytes()).0, 200);
    let ctx = ctx_over(&hub);

    let trimmed = spaced.trim();
    let folded = nonascii.to_uppercase();
    for body in [
        serde_json::json!({"answers": {trimmed: "Red"}}),
        serde_json::json!({"answers": {folded: "Le Café"}}),
    ] {
        let resp = answer_question(&ctx, "q1", &body);
        assert_eq!(resp.status, 400, "{body}");
        assert_eq!(
            body_json(&resp)["error"],
            "answer_unknown_question",
            "{body}"
        );
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));

    let exact = serde_json::json!({"answers": {
        spaced: "Red",
        multiline: "Sans, Mono",
        nonascii: "Le Café"
    }});
    let resp = answer_question(&ctx, "q1", &exact);
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
}

/// A value that is not a string, a `notes` that is neither an object nor
/// `null`, `notes` beside a `decision` or alone, or an `answers` with no entry
/// at all is the wrong body shape: `bad_request`, nothing queued.
#[test]
fn a_misshapen_answer_or_note_is_a_bad_request() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    for body in [
        serde_json::json!({"answers": {LOGO: 3}}),
        serde_json::json!({"answers": {LOGO: ["Red"]}}),
        serde_json::json!({"answers": {LOGO: null}}),
        serde_json::json!({"answers": {}}),
        serde_json::json!({"answers": {LOGO: "Red"}, "notes": {LOGO: 3}}),
        serde_json::json!({"answers": {LOGO: "Red"}, "notes": "darker"}),
        serde_json::json!({"answers": {LOGO: "Red"}, "notes": [LOGO]}),
        serde_json::json!({"decision": "allow", "notes": {LOGO: "x"}}),
        serde_json::json!({"notes": {LOGO: "x"}}),
        serde_json::json!({"notes": null}),
        serde_json::json!({"answers": "Red"}),
    ] {
        let resp = answer_question(&ctx, "q1", &body);
        assert_eq!(resp.status, 400, "{body}");
        assert_eq!(body_json(&resp)["error"], "bad_request", "{body}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// A generated client writes every unset field as `null` (the app's Dart
/// serializer does), so a `null` `answers`, `notes` or `decision` reads as
/// absent: its decision and its answers both land.
#[test]
fn a_generated_clients_null_fields_read_as_absent() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();
    open_ask(&hub, 100, "s-one", 3, "a1");

    let resp = answer_question(
        &ctx,
        "a1",
        &serde_json::json!({"decision": "allow", "answers": null, "notes": null}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    let resp = answer_question(
        &ctx,
        "q1",
        &serde_json::json!({"decision": null, "answers": {LOGO: "Red"}, "notes": null}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([
            {"kind": "answer", "ask": "a1", "decision": "allow"},
            {"kind": "answer", "ask": "q1", "answers": {LOGO: "Red"}}
        ])
    );
}

/// `answers: null` beside a decision, with no `notes` field at all, reads as
/// absent on its own.
#[test]
fn a_null_answers_reads_as_absent() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);

    let resp = answer_question(
        &ctx,
        "a1",
        &serde_json::json!({"decision": "allow", "answers": null}),
    );
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "a1", "decision": "allow"}])
    );
}

/// A permission ask takes a decision, so `answers` sent to it are the wrong
/// form: refused by name, never queued for a mod that would drop them with the
/// ask closed, and the ask kept open for the decision it takes.
#[test]
fn answers_to_a_permission_ask_are_the_wrong_form() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);

    let resp = answer_question(&ctx, "a1", &serde_json::json!({"answers": {"ls": "yes"}}));
    assert_eq!(resp.status, 400);
    let got = body_json(&resp);
    assert_eq!(got["error"], "answer_wrong_form");
    assert_eq!(got["reason"], WRONG_FORM);
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));

    let resp = answer_question(&ctx, "a1", &serde_json::json!({"decision": "allow"}));
    assert_eq!(
        resp.status,
        200,
        "the ask stayed open: {}",
        body_json(&resp)
    );
}

const WRONG_FORM: &str = "a question takes answers, and a permission ask takes a decision.";

/// A decision sent to a question ask is the wrong form: refused by name,
/// nothing queued, the ask kept open for the answers it takes. It is checked
/// before the answer rules, so it wins over them.
#[test]
fn a_decision_to_a_question_ask_is_the_wrong_form() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();

    for body in [
        serde_json::json!({"decision": "allow"}),
        serde_json::json!({"decision": "deny"}),
    ] {
        let resp = answer_question(&ctx, "q1", &body);
        assert_eq!(resp.status, 400, "{body}");
        let got = body_json(&resp);
        assert_eq!(got["error"], "answer_wrong_form", "{body}");
        assert_eq!(got["reason"], WRONG_FORM, "{body}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));

    let resp = answer_question(&ctx, "q1", &serde_json::json!({"answers": {LOGO: "Red"}}));
    assert_eq!(
        resp.status,
        200,
        "the ask stayed open: {}",
        body_json(&resp)
    );
}

/// A refused answer is refused before the queue cap is consulted: a full queue
/// answers the body's own 400, not the 503 a taken command would get, and the
/// ask stays open.
#[test]
fn a_refused_answer_is_refused_before_a_full_queue() {
    let _home = HomeSandbox::new();
    let (hub, ctx, _t) = question_hub();
    {
        let mut inner = hub.inner.lock().unwrap();
        let live = inner.instances.get_mut(&100).expect("the live instance");
        for _ in 0..MAX_QUEUED_COMMANDS {
            live.commands
                .push_back(serde_json::json!({"kind": "prompt", "text": "x"}));
        }
    }

    for (body, code) in [
        (
            serde_json::json!({"decision": "allow"}),
            "answer_wrong_form",
        ),
        (
            serde_json::json!({"answers": {"Which colour?": "Red"}}),
            "answer_unknown_question",
        ),
        (serde_json::json!({"answers": {LOGO: ""}}), "answer_empty"),
    ] {
        let resp = answer_question(&ctx, "q1", &body);
        assert_eq!(resp.status, 400, "{body}");
        assert_eq!(body_json(&resp)["error"], code, "{body}");
    }

    let inner = hub.inner.lock().unwrap();
    let live = inner.instances.get(&100).expect("the live instance");
    assert_eq!(
        live.commands.len(),
        MAX_QUEUED_COMMANDS,
        "a refusal queues nothing"
    );
    assert!(
        live.open_asks.iter().any(|(id, _)| id == "q1"),
        "a refusal leaves the ask open"
    );
}

/// An ask's kind comes from its tool, never its input's shape: a permission
/// ask whose tool input happens to be an array of `question` objects (an MCP
/// tool's own input) still takes a decision and refuses answers keyed by
/// those texts, and a question ask whose input carries no question text
/// still refuses a decision.
#[test]
fn an_asks_kind_comes_from_its_tool() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let lookalike = serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "mcp__forms__ask", "input": [{"question": LOGO}]});
    assert_eq!(hub.event(100, lookalike.to_string().as_bytes()).0, 200);
    let bare = serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "q1", "tool": "AskUserQuestion", "input": {}});
    assert_eq!(hub.event(100, bare.to_string().as_bytes()).0, 200);
    let ctx = ctx_over(&hub);

    let resp = answer_question(&ctx, "a1", &serde_json::json!({"answers": {LOGO: "Red"}}));
    assert_eq!(resp.status, 400);
    assert_eq!(body_json(&resp)["error"], "answer_wrong_form");
    let resp = answer_question(&ctx, "q1", &serde_json::json!({"decision": "allow"}));
    assert_eq!(resp.status, 400);
    assert_eq!(body_json(&resp)["error"], "answer_wrong_form");
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));

    let resp = answer_question(&ctx, "a1", &serde_json::json!({"decision": "deny"}));
    assert_eq!(resp.status, 200, "{}", body_json(&resp));
}

/// A body naming both, or neither, of `decision` and `answers` is refused
/// before anything is queued.
#[test]
fn an_ask_answer_names_exactly_one_form() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);

    for body in [
        "{}",
        r#"{"decision":"allow","answers":{"q":"a"}}"#,
        r#"{"decision":"maybe"}"#,
        r#"{"answers":"x"}"#,
        r#"{"answers":[]}"#,
    ] {
        let resp = call(
            &ctx,
            &req("POST", "/api/v1/sessions/s-one/asks/a1", Some(TOKEN), body),
        );
        assert_eq!(resp.status, 400, "{body}");
        assert_eq!(body_json(&resp)["error"], "bad_request", "{body}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// The desk answering first closes the ask for every device.
#[test]
fn a_desk_answer_closes_the_ask() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let closed = serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_closed", "ask": "a1", "by": "desk"});
    assert_eq!(hub.event(100, closed.to_string().as_bytes()).0, 200);
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/a1",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(resp.status, 409);
    assert_eq!(body_json(&resp)["error"], "ask_closed");
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// The mod holds only when a remote answer is possible: a bound listener
/// and a paired control device. Each half alone answers `hold: false`, and
/// the poll carries the same flag as `remote`.
#[test]
fn the_hold_flag_needs_a_listener_and_a_control_device() {
    for (listening, tier, want) in [
        (true, Some(Tier::Control), true),
        (true, Some(Tier::View), false),
        (true, None, false),
        (false, Some(Tier::Control), false),
    ] {
        let _home = HomeSandbox::new();
        if let Some(ref tier) = tier {
            seed_device(DEVICE, tier.clone(), TOKEN);
        }
        let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
        let hub = Hub::new(listening, resolver);
        start(&hub, 100, "s-one");
        let answer = open_ask(&hub, 100, "s-one", 2, "a1");
        assert_eq!(answer["hold"], want, "listening={listening} tier={tier:?}");
        let (_, polled) = hub.poll(100, "s-one", Duration::ZERO);
        assert_eq!(
            polled["remote"], want,
            "listening={listening} tier={tier:?}"
        );
    }
}

/// `/clear` keeps the pid and moves the record's id: the old session closes,
/// so a command for it answers 404 and its poll is refused.
#[test]
fn a_cleared_session_closes() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    table.lock().unwrap().insert(100, instance("s-three", "1"));
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(resp.status, 404);
    assert_eq!(body_json(&resp)["error"], "session_not_live");
    let (status, _) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(status, 403);
}

/// A recycled pid (same pid, another process start) is a new instance: the
/// old session closes rather than passing to the newcomer.
#[test]
fn a_recycled_pid_does_not_inherit_a_session() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    table.lock().unwrap().insert(100, instance("s-one", "99"));
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(resp.status, 404);
    assert_eq!(body_json(&resp)["error"], "session_not_live");
}

/// Two live processes resolving to one session id make it ambiguous: a
/// command for it is refused rather than handed to either.
#[test]
fn two_instances_on_one_id_are_ambiguous() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-one", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-one");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(resp.status, 409);
    assert_eq!(body_json(&resp)["error"], "session_ambiguous");
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
    assert_eq!(commands(&hub, 200, "s-one"), serde_json::json!([]));
}

/// An event at or below the instance's high-water mark is dropped by name.
#[test]
fn a_stale_sequence_is_dropped() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let turn = |seq: u64| serde_json::json!({"session": "s-one", "seq": seq, "kind": "turn_start", "turn": format!("t{seq}")});
    assert_eq!(hub.event(100, turn(3).to_string().as_bytes()).0, 200);
    let (status, answer) = hub.event(100, turn(2).to_string().as_bytes());
    assert_eq!(status, 409);
    assert_eq!(answer["error"], "stale_sequence");
}

/// A session with no mod and no herdr pane to fall back to has no reach.
#[test]
fn a_session_with_neither_mod_nor_pane_is_not_live() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let ctx = ctx_over(&hub);
    for (path, body) in [
        ("/api/v1/sessions/s-nine/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-nine/interrupt", "{}"),
    ] {
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(resp.status, 404, "{path}");
        assert_eq!(body_json(&resp)["error"], "session_not_live", "{path}");
    }
}

/// End to end over the real socket: the server reads the peer pid off the
/// connection (the test process itself), so an event naming the resolved
/// session is taken and one naming another is refused.
#[test]
fn the_socket_resolves_the_connecting_process() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    let (_t, resolver) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, resolver);
    let listener = bind(&path).expect("bind");
    let server = spawn_server(listener, Arc::clone(&hub)).expect("spawn");

    let post = |session: &str| -> String {
        let body = serde_json::json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w", "interactive": true}).to_string();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST /event HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    };
    assert!(
        post("s-self").starts_with("HTTP/1.1 200"),
        "the resolved session is taken"
    );
    assert!(
        post("s-other").starts_with("HTTP/1.1 403"),
        "another session is refused"
    );
    server.stop();
}
