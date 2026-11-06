#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `GET /api/v1/panes`: the pid join, the absent shape, and the seam.
//!
//! Everything runs against a [`HomeSandbox`]; the herdr calls go through a
//! fixture-backed probe so no test runs a real herdr.

#![cfg(unix)]

use super::*;

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::daemon::api::devices::{self, Tier};
use crate::daemon::api::http::{Request, Response};
use crate::daemon::api::routes::{ApiContext, handle};
use crate::live_sessions::{self, LiveSession};
use crate::testutil::HomeSandbox;

/// The bearer of the control device every context below pairs.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The device [`TOKEN`] authenticates as.
const DEVICE: &str = "test";

const PANE_LIST: &str = include_str!("../fixtures/herdr/pane-list.json");
const PROCESS_INFO_WP: &str = include_str!("../fixtures/herdr/process-info-wP-pAA.json");
const PROCESS_INFO_W1N: &str = include_str!("../fixtures/herdr/process-info-w1N-p19.json");
const PROCESS_INFO_W0: &str = include_str!("../fixtures/herdr/process-info-w0-pK.json");
const PANES_ANSWER: &str = include_str!("../fixtures/herdr/panes-answer.json");

fn config() -> crate::profile::ConfigHandle {
    std::sync::Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn ctx(probe: PaneProbe) -> std::sync::Arc<ApiContext> {
    devices::seed_for_tests(DEVICE, Tier::Control, TOKEN).expect("seed the device");
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::for_tests(config(), status_path, None, probe)
}

fn peer() -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 7], 50_000))
}

fn call(ctx: &ApiContext, method: &str, path: &str) -> Response {
    handle(
        ctx,
        &Request {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            bearer: Some(TOKEN.to_string()),
            if_none_match: None,
            body: Vec::new(),
            keep_alive: true,
            ws: Default::default(),
            cross_origin: Default::default(),
            content_type: Default::default(),
        },
        peer(),
    )
    .response
}

fn body_json(resp: &Response) -> serde_json::Value {
    serde_json::from_slice(&resp.body).expect("a json body")
}

fn ran_ok(stdout: &'static str) -> HerdrProbeOut {
    HerdrProbeOut::Ran(Some(HerdrOut {
        success: true,
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }))
}

fn ran_failed() -> HerdrProbeOut {
    HerdrProbeOut::Ran(Some(HerdrOut {
        success: false,
        stdout: Vec::new(),
        stderr: Vec::new(),
    }))
}

fn ran_none() -> HerdrProbeOut {
    HerdrProbeOut::Ran(None)
}

fn clone_outcome(out: &HerdrProbeOut) -> HerdrProbeOut {
    match out {
        HerdrProbeOut::NotInstalled => HerdrProbeOut::NotInstalled,
        HerdrProbeOut::Ran(None) => HerdrProbeOut::Ran(None),
        HerdrProbeOut::Ran(Some(o)) => HerdrProbeOut::Ran(Some(HerdrOut {
            success: o.success,
            stdout: o.stdout.clone(),
            stderr: o.stderr.clone(),
        })),
    }
}

/// A probe that answers `list` for `pane list` and `infos` per pane id; any
/// other call (or an unknown pane) answers `Ran(None)`.
fn probe(list: HerdrProbeOut, infos: Vec<(String, HerdrProbeOut)>) -> PaneProbe {
    Box::new(move |args, _deadline| match args {
        ["pane", "list"] => clone_outcome(&list),
        ["pane", "process-info", "--pane", pane_id] => infos
            .iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, out)| clone_outcome(out))
            .unwrap_or_else(|| HerdrProbeOut::Ran(None)),
        _ => HerdrProbeOut::Ran(None),
    })
}

/// The fixture pane list plus the three captured process-info answers.
fn fixture_probe() -> PaneProbe {
    probe(
        ran_ok(PANE_LIST),
        vec![
            ("w1N:p19".to_string(), ran_ok(PROCESS_INFO_W1N)),
            ("w0:pK".to_string(), ran_ok(PROCESS_INFO_W0)),
            ("wP:pAA".to_string(), ran_ok(PROCESS_INFO_WP)),
        ],
    )
}

/// The two captured registry rows, written through the registry's own writer.
fn register_fixture_rows() {
    let rows = [
        LiveSession {
            session_id: "3208712-0".to_string(),
            start_profile: "uwuclxdy".to_string(),
            pid: 3208712,
            claude_pid: None,
            launch_families: None,
            started_at: 1789435569278,
            cwd: Some(PathBuf::from("/home/user/repos/rs/clauth")),
            isolated: false,
            follows_chain: false,
            intended_member: None,
            harness: crate::harness::Harness::Claude,
            chain_cursor: None,
            credit_leave: false,
            credit_leave_aware: false,
            current_member: None,
            last_swap_at: None,
            launch_store: Some(PathBuf::from(
                "/home/user/.clauth/profiles/uwuclxdy/credentials.json",
            )),
            boot_transport: None,
            is_delegate: false,
            background: false,
            background_hold: false,
            visited_members: vec![],
        },
        LiveSession {
            session_id: "1128637-0".to_string(),
            start_profile: "DS5".to_string(),
            pid: 1128637,
            claude_pid: None,
            launch_families: None,
            started_at: 1789415587819,
            cwd: Some(PathBuf::from("/home/user/repos/app")),
            isolated: false,
            follows_chain: false,
            intended_member: None,
            harness: crate::harness::Harness::Claude,
            chain_cursor: None,
            credit_leave: false,
            credit_leave_aware: false,
            current_member: None,
            last_swap_at: None,
            launch_store: Some(PathBuf::from(
                "/home/user/.clauth/profiles/DS5/credentials.json",
            )),
            boot_transport: None,
            is_delegate: false,
            background: false,
            background_hold: false,
            visited_members: vec![],
        },
    ];
    for row in rows {
        live_sessions::register(&row).expect("register the fixture row");
    }
}

/// The captured `clauth mcp` pid inside `wP:pAA`, as a delegate stand-in.
fn register_delegate_row() {
    live_sessions::register(&LiveSession {
        session_id: "3210136-0".to_string(),
        start_profile: "uwuclxdy".to_string(),
        pid: 3210136,
        claude_pid: None,
        launch_families: None,
        started_at: 0,
        cwd: Some(PathBuf::from("/home/user/repos/rs/clauth")),
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the delegate row");
}

/// A row whose pid matches no pane, for the lingering-row test.
fn register_orphan_row() {
    live_sessions::register(&LiveSession {
        session_id: "999999-0".to_string(),
        start_profile: "ghost".to_string(),
        pid: 999999,
        claude_pid: None,
        launch_families: None,
        started_at: 0,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the orphan row");
}

/// A lingering row whose profile equals `w1N:p19`'s `tokens.clauth` tag but
/// whose pid is in no pane, so a join on the tag would attach it where the pid
/// join does not.
fn register_stale_tag_row() {
    live_sessions::register(&LiveSession {
        session_id: "888888-0".to_string(),
        start_profile: "DS5".to_string(),
        pid: 888888,
        claude_pid: None,
        launch_families: None,
        started_at: 0,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the stale-tag row");
}

/// The three fixture panes joined to the two registry rows serialize to the
/// pinned answer, and that answer agrees with the documented schema. The stale
/// tag row joins nothing by pid, so a tag-join is what would make this red.
#[test]
fn the_fixture_panes_join_to_the_pinned_answer() {
    let _home = HomeSandbox::new();
    register_fixture_rows();
    register_stale_tag_row();
    let ctx = ctx(fixture_probe());

    let resp = call(&ctx, "GET", "/api/v1/panes");
    assert_eq!(resp.status, 200);
    let body = body_json(&resp);
    let expected: serde_json::Value = serde_json::from_str(PANES_ANSWER).expect("pin parses");
    assert_eq!(body, expected);
    crate::testutil::schema_agrees_with_type::<PanesBody>(&body);
}

/// `agent_session_id` is herdr's `agent_session.value` exactly when its
/// `kind` is `id` — the transcript stem the sessions routes page — and `null`
/// for a pane with no agent session, one of another kind, or one with no
/// `kind` at all (which parses rather than failing the whole list).
#[test]
fn agent_session_id_is_the_id_kind_value_or_null() {
    let _home = HomeSandbox::new();
    let by_pane = |body: &serde_json::Value| -> Vec<(String, serde_json::Value)> {
        body["panes"]
            .as_array()
            .expect("panes")
            .iter()
            .map(|pane| {
                (
                    pane["pane_id"].as_str().expect("pane id").to_string(),
                    pane["agent_session_id"].clone(),
                )
            })
            .collect()
    };

    let body = body_json(&call(&ctx(fixture_probe()), "GET", "/api/v1/panes"));
    assert_eq!(
        by_pane(&body),
        vec![
            (
                "w1N:p19".to_string(),
                serde_json::json!("1cb26556-3532-45e1-8b39-37f0b53a8e4f")
            ),
            ("w0:pK".to_string(), serde_json::Value::Null),
            (
                "wP:pAA".to_string(),
                serde_json::json!("cd1d7f14-4d0a-4616-b6e7-bed7b2feaea8")
            ),
        ]
    );

    // The same list with `w1N:p19`'s session detected by another kind,
    // `w0:pK` carrying an `agent_session` with no `kind` at all, and
    // `wP:pAA`'s `agent_session` key absent altogether: every row still
    // parses, and each projects `null`.
    let mut list: serde_json::Value = serde_json::from_str(PANE_LIST).expect("fixture parses");
    let panes = list["result"]["panes"].as_array_mut().expect("panes");
    panes[0]["agent_session"] = serde_json::json!({
        "agent": "claude",
        "kind": "title",
        "source": "herdr:claude",
        "value": "Refactor the parser",
    });
    panes[1]["agent_session"] = serde_json::json!({
        "agent": "claude",
        "value": "1cb26556-3532-45e1-8b39-37f0b53a8e4f",
    });
    panes[2]
        .as_object_mut()
        .expect("a pane object")
        .remove("agent_session");
    let list = serde_json::to_vec(&list).expect("serializes");
    let probe: PaneProbe = Box::new(move |args, _deadline| match args {
        ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout: list.clone(),
            stderr: Vec::new(),
        })),
        _ => HerdrProbeOut::Ran(None),
    });
    let body = body_json(&call(&ctx(probe), "GET", "/api/v1/panes"));
    assert_eq!(
        by_pane(&body),
        vec![
            ("w1N:p19".to_string(), serde_json::Value::Null),
            ("w0:pK".to_string(), serde_json::Value::Null),
            ("wP:pAA".to_string(), serde_json::Value::Null),
        ]
    );
}

/// A row whose pid is a listed `clauth mcp` (never the group leader, never a
/// `clauth start`/`resume`) is the pane's delegate, listed after its own
/// session.
#[test]
fn a_delegate_pid_joins_as_delegate_after_the_own_session() {
    let _home = HomeSandbox::new();
    register_fixture_rows();
    register_delegate_row();
    let ctx = ctx(fixture_probe());

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    let pane = body["panes"]
        .as_array()
        .expect("panes")
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wP:pAA"))
        .expect("the wP:pAA pane");
    let sessions = pane["sessions"].as_array().expect("sessions");
    let expected = serde_json::json!([
        {
            "session_id": "3208712-0",
            "profile": "uwuclxdy",
            "kind": "session",
            "follows_chain": false,
            "isolated": false,
            "cwd": "/home/user/repos/rs/clauth",
        },
        {
            "session_id": "3210136-0",
            "profile": "uwuclxdy",
            "kind": "delegate",
            "follows_chain": false,
            "isolated": false,
            "cwd": "/home/user/repos/rs/clauth",
        },
    ]);
    assert_eq!(
        sessions,
        expected.as_array().expect("the expected sessions")
    );
}

/// A session that moved its working dir after launch (`--worktree`) reports
/// the dir its Claude Code record names, the same cwd `clauth sessions --live`
/// lists, never the launcher's.
#[test]
fn a_session_lists_the_cwd_its_record_names() {
    let home = HomeSandbox::new();
    register_fixture_rows();
    let mut row = live_sessions::list()
        .into_iter()
        .find(|row| row.session_id == "3208712-0")
        .expect("the fixture row");
    row.claude_pid = Some(4242);
    live_sessions::register(&row).expect("re-register with its claude pid");
    let sessions_dir = home.home().join(".claude").join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    std::fs::write(
        sessions_dir.join("4242.json"),
        serde_json::json!({
            "sessionId": "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa",
            "pid": 4242,
            "cwd": "/home/user/repos/rs/clauth/.claude/worktrees/x",
        })
        .to_string(),
    )
    .expect("record");
    let ctx = ctx(fixture_probe());

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    let pane = body["panes"]
        .as_array()
        .expect("panes")
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wP:pAA"))
        .expect("the wP:pAA pane");
    assert_eq!(
        pane["sessions"][0]["cwd"],
        serde_json::json!("/home/user/repos/rs/clauth/.claude/worktrees/x")
    );
}

/// Herdr absent answers 200 with the one fixed sentence per state, never an
/// error.
#[test]
fn herdr_absent_answers_200_with_the_reason_for_each_state() {
    let _home = HomeSandbox::new();

    let not_installed = body_json(&call(
        &ctx(probe(HerdrProbeOut::NotInstalled, Vec::new())),
        "GET",
        "/api/v1/panes",
    ));
    assert_eq!(
        not_installed,
        serde_json::json!({
            "ok": true,
            "herdr": {"present": false, "reason": "herdr is not installed on this host"},
            "panes": [],
        })
    );

    let failed = body_json(&call(
        &ctx(probe(ran_failed(), Vec::new())),
        "GET",
        "/api/v1/panes",
    ));
    assert_eq!(
        failed,
        serde_json::json!({
            "ok": true,
            "herdr": {"present": false, "reason": "herdr is installed but no server answered on its socket"},
            "panes": [],
        })
    );

    let none = body_json(&call(
        &ctx(probe(ran_none(), Vec::new())),
        "GET",
        "/api/v1/panes",
    ));
    assert_eq!(none, failed);
}

/// One pane whose `process-info` no longer answers keeps its row with a null
/// process group and no sessions; the other panes join normally.
#[test]
fn a_pane_whose_process_info_failed_keeps_its_row_empty() {
    let _home = HomeSandbox::new();
    register_fixture_rows();
    let probe = probe(
        ran_ok(PANE_LIST),
        vec![
            ("w1N:p19".to_string(), ran_failed()),
            ("w0:pK".to_string(), ran_ok(PROCESS_INFO_W0)),
            ("wP:pAA".to_string(), ran_ok(PROCESS_INFO_WP)),
        ],
    );
    let ctx = ctx(probe);

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    assert_eq!(body["herdr"], serde_json::json!({"present": true}));
    let panes = body["panes"].as_array().expect("panes");
    assert_eq!(panes.len(), 3);

    let failed = panes
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("w1N:p19"))
        .expect("w1N:p19");
    assert_eq!(
        failed["foreground_process_group_id"],
        serde_json::Value::Null
    );
    assert_eq!(failed["sessions"], serde_json::json!([]));

    let joined = panes
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wP:pAA"))
        .expect("wP:pAA");
    assert_eq!(
        joined["foreground_process_group_id"],
        serde_json::json!(3208712)
    );
    assert_eq!(joined["sessions"].as_array().expect("sessions").len(), 1);
}

/// A registry row whose pid matches no pane appears in no pane's sessions,
/// and the two fixture rows still join theirs.
#[test]
fn a_lingering_row_joins_no_pane() {
    let _home = HomeSandbox::new();
    register_fixture_rows();
    register_orphan_row();
    let ctx = ctx(fixture_probe());

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    let mut joined_ids: Vec<String> = body["panes"]
        .as_array()
        .expect("panes")
        .iter()
        .flat_map(|pane| pane["sessions"].as_array().expect("sessions"))
        .map(|session| {
            session["session_id"]
                .as_str()
                .expect("session id")
                .to_string()
        })
        .collect();
    joined_ids.sort();
    assert_eq!(
        joined_ids,
        vec!["1128637-0".to_string(), "3208712-0".to_string()]
    );
}

/// Two rows sharing one pid (a recycled pid matching a stale row and the live
/// one at once) join as one: the newest `started_at` wins.
#[test]
fn a_recycled_pid_keeps_only_the_newest_row() {
    let _home = HomeSandbox::new();
    register_fixture_rows();
    live_sessions::register(&LiveSession {
        session_id: "3208712-9".to_string(),
        start_profile: "ghost".to_string(),
        pid: 3208712,
        claude_pid: None,
        launch_families: None,
        started_at: 1789435569000,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the stale pid row");
    let ctx = ctx(fixture_probe());

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    let pane = body["panes"]
        .as_array()
        .expect("panes")
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wP:pAA"))
        .expect("the wP:pAA pane");
    assert_eq!(
        pane["sessions"],
        serde_json::json!([{
            "session_id": "3208712-0",
            "profile": "uwuclxdy",
            "kind": "session",
            "follows_chain": false,
            "isolated": false,
            "cwd": "/home/user/repos/rs/clauth",
        }])
    );
}

/// A `clauth start` running under a wrapper (its pid is a listed process, not
/// the group leader) reads as the pane's own session, not a delegate.
#[test]
fn a_wrapper_launched_clauth_start_is_the_panes_session() {
    let _home = HomeSandbox::new();
    live_sessions::register(&LiveSession {
        session_id: "1002-0".to_string(),
        start_profile: "DS5".to_string(),
        pid: 1002,
        claude_pid: None,
        launch_families: None,
        started_at: 1,
        cwd: Some(PathBuf::from("/home/user/repos/app")),
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the wrapper session");
    let list = r#"{"id":"cli:pane:list","result":{"panes":[{"agent_status":"idle","cwd":"/home/user/repos/app","focused":false,"pane_id":"wX:pX","tab_id":"wX:tX","workspace_id":"wX"}]}}"#;
    let info = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":1001,"foreground_processes":[{"name":"bash","pid":1001,"argv":["/usr/bin/bash"]},{"name":"clauth","pid":1002,"argv":["clauth","start","DS5"]}],"pane_id":"wX:pX","shell_pid":1001}}}"#;
    let ctx = ctx(probe(
        ran_ok(list),
        vec![("wX:pX".to_string(), ran_ok(info))],
    ));

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    let pane = body["panes"]
        .as_array()
        .expect("panes")
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wX:pX"))
        .expect("the wX:pX pane");
    assert_eq!(
        pane["sessions"],
        serde_json::json!([{
            "session_id": "1002-0",
            "profile": "DS5",
            "kind": "session",
            "follows_chain": false,
            "isolated": false,
            "cwd": "/home/user/repos/app",
        }])
    );
}

/// A pane with no foreground job (herdr omits both keys) keeps its row with a
/// null group and no sessions, beside a pane whose job lists a delegate child.
#[test]
fn a_pane_with_no_foreground_job_keeps_its_row_beside_a_joined_pane() {
    let _home = HomeSandbox::new();
    live_sessions::register(&LiveSession {
        session_id: "2002-0".to_string(),
        start_profile: "uwuclxdy".to_string(),
        pid: 2002,
        claude_pid: None,
        launch_families: None,
        started_at: 1,
        cwd: Some(PathBuf::from("/home/user/repos/rs/clauth")),
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the member row");
    let list = r#"{"id":"cli:pane:list","result":{"panes":[{"agent_status":"idle","cwd":"/home/user/repos/rs/clauth","focused":false,"pane_id":"wM:pM","tab_id":"wM:tM","workspace_id":"wM"},{"agent_status":"idle","cwd":"/home/user/repos/shell","focused":false,"pane_id":"wE:pE","tab_id":"wE:tE","workspace_id":"wE"}]}}"#;
    let member_info = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":2001,"foreground_processes":[{"name":"bash","pid":2001,"argv":["/usr/bin/bash"]},{"name":"claude","pid":2002,"argv":["claude","--effort","max"]}],"pane_id":"wM:pM","shell_pid":2001}}}"#;
    // The real no-job wire: herdr omits both keys.
    let empty_info = r#"{"id":"cli:pane:process_info","result":{"process_info":{"pane_id":"wE:pE","shell_pid":2003}}}"#;
    let ctx = ctx(probe(
        ran_ok(list),
        vec![
            ("wM:pM".to_string(), ran_ok(member_info)),
            ("wE:pE".to_string(), ran_ok(empty_info)),
        ],
    ));

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    assert_eq!(body["herdr"], serde_json::json!({"present": true}));
    let panes = body["panes"].as_array().expect("panes");

    let member = panes
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wM:pM"))
        .expect("wM:pM");
    assert_eq!(
        member["foreground_process_group_id"],
        serde_json::json!(2001)
    );
    assert_eq!(
        member["sessions"],
        serde_json::json!([{
            "session_id": "2002-0",
            "profile": "uwuclxdy",
            "kind": "delegate",
            "follows_chain": false,
            "isolated": false,
            "cwd": "/home/user/repos/rs/clauth",
        }])
    );

    let empty = panes
        .iter()
        .find(|pane| pane["pane_id"] == serde_json::json!("wE:pE"))
        .expect("wE:pE");
    assert_eq!(
        empty["foreground_process_group_id"],
        serde_json::Value::Null
    );
    assert_eq!(empty["sessions"], serde_json::json!([]));
}

/// A pane whose list entry omits `cwd` (herdr skips it when unknown) still
/// lists: the pane row carries `cwd: null` rather than collapsing the whole
/// answer into the absent sentence.
#[test]
fn a_pane_without_cwd_still_lists_with_a_null_cwd() {
    let _home = HomeSandbox::new();
    let list = r#"{"id":"cli:pane:list","result":{"panes":[{"agent_status":"idle","focused":false,"pane_id":"wN:pN","tab_id":"wN:tN","workspace_id":"wN"}]}}"#;
    let ctx = ctx(probe(
        ran_ok(list),
        vec![("wN:pN".to_string(), ran_failed())],
    ));

    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    assert_eq!(body["herdr"], serde_json::json!({"present": true}));
    let panes = body["panes"].as_array().expect("panes");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0]["pane_id"], serde_json::json!("wN:pN"));
    assert_eq!(panes[0]["cwd"], serde_json::Value::Null);
}

/// `HEAD /panes` answers the head only: the route serves the GET answer, and
/// `Response::into_head` strips its body.
#[test]
fn head_panes_answers_the_head_only() {
    let _home = HomeSandbox::new();
    let ctx = ctx(fixture_probe());

    let resp = call(&ctx, "HEAD", "/api/v1/panes");
    assert_eq!(resp.status, 200);
    assert!(resp.into_head().body.is_empty());
}

/// A helper for the kind pins: one row, one pane, one process-info envelope,
/// the resulting `sessions` array.
fn sessions_for(row_pid: u32, info: &'static str) -> serde_json::Value {
    live_sessions::register(&LiveSession {
        session_id: format!("{row_pid}-0"),
        start_profile: "DS5".to_string(),
        pid: row_pid,
        claude_pid: None,
        launch_families: None,
        started_at: 1,
        cwd: None,
        isolated: false,
        follows_chain: false,
        intended_member: None,
        harness: crate::harness::Harness::Claude,
        chain_cursor: None,
        credit_leave: false,
        credit_leave_aware: false,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        boot_transport: None,
        is_delegate: false,
        background: false,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the row");
    let list = r#"{"id":"cli:pane:list","result":{"panes":[{"agent_status":"idle","cwd":"/home/user/repos/app","focused":false,"pane_id":"wK:pK","tab_id":"wK:tK","workspace_id":"wK"}]}}"#;
    let ctx = ctx(probe(
        ran_ok(list),
        vec![("wK:pK".to_string(), ran_ok(info))],
    ));
    let body = body_json(&call(&ctx, "GET", "/api/v1/panes"));
    body["panes"][0]["sessions"].clone()
}

fn kinds(sessions: &serde_json::Value) -> Vec<String> {
    sessions
        .as_array()
        .expect("sessions")
        .iter()
        .map(|s| s["kind"].as_str().expect("kind").to_string())
        .collect()
}

/// The group leader is the pane's own session whatever its argv says (here
/// unreadable, as under `hidepid` or an elevated Windows process): a delegate
/// child never leads the pane's group, so the leader needs no argv test.
#[test]
fn a_group_leader_is_the_panes_session_whatever_its_argv() {
    let _home = HomeSandbox::new();
    let info: &'static str = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":3001,"foreground_processes":[{"name":"clauth","pid":3001},{"name":"claude","pid":3002,"argv":["claude","--effort","max"]}],"pane_id":"wK:pK","shell_pid":3000}}}"#;
    assert_eq!(kinds(&sessions_for(3001, info)), vec!["session"]);
}

/// `clauth resume` registers its row like `clauth start`; under a wrapper it is
/// a listed member and its verb is what makes it the pane's session.
#[test]
fn a_wrapper_launched_clauth_resume_is_the_panes_session() {
    let _home = HomeSandbox::new();
    let info: &'static str = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":3100,"foreground_processes":[{"name":"bash","pid":3100,"argv":["/usr/bin/bash"]},{"name":"clauth","pid":3101,"argv":["/home/user/.cargo/bin/clauth","resume","latest"]}],"pane_id":"wK:pK","shell_pid":3100}}}"#;
    assert_eq!(kinds(&sessions_for(3101, info)), vec!["session"]);
}

/// herdr's Windows `process-info` carries one entry, the pane's agent root
/// (`claude.exe`), as both group id and sole process; the `clauth.exe`
/// supervisor is never listed, so no session joins on Windows and the answer
/// says so by an empty `sessions`, never by a wrong one.
#[test]
fn a_windows_pane_lists_only_its_agent_root_so_no_session_joins() {
    let _home = HomeSandbox::new();
    let info: &'static str = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":4003,"foreground_processes":[{"name":"claude.exe","pid":4003,"argv":["C:\\Users\\user\\AppData\\Local\\claude.exe","--effort","max"]}],"pane_id":"wK:pK","shell_pid":4001}}}"#;
    assert_eq!(kinds(&sessions_for(4002, info)), Vec::<String>::new());
}

/// A member that is not a `clauth` reads as a delegate even when its second
/// argument is `start`: the name conjunct is what keeps a recycled pid on an
/// `npm start` from posing as the pane's session.
#[test]
fn a_non_clauth_member_running_start_is_a_delegate() {
    let _home = HomeSandbox::new();
    let info: &'static str = r#"{"id":"cli:pane:process_info","result":{"process_info":{"foreground_process_group_id":5001,"foreground_processes":[{"name":"bash","pid":5001,"argv":["/usr/bin/bash"]},{"name":"node","pid":5002,"argv":["npm","start"]}],"pane_id":"wK:pK","shell_pid":5001}}}"#;
    assert_eq!(kinds(&sessions_for(5002, info)), vec!["delegate"]);
}

/// The real no-foreground-job wire omits both keys; the envelope still parses,
/// so the pane keeps its row through the parsed arm, never the failure arm.
#[test]
fn the_real_no_job_wire_parses_with_no_group_and_no_processes() {
    let info = parse_process_info(
        br#"{"id":"cli:pane:process_info","result":{"process_info":{"pane_id":"wE:pE","shell_pid":2003}}}"#,
    )
    .expect("the no-job envelope parses");
    assert_eq!(info.foreground_process_group_id, None);
    assert!(info.foreground_processes.is_empty());
}

/// F9: the daemon's background hold carries the DAEMON's pid, so it never joins
/// a pane as that pane's Claude session (wrong account on the row and in the
/// push). A launcher's own background row (no hold flag) still joins — it IS a
/// `clauth start`.
#[test]
fn a_background_hold_row_never_joins_a_pane_pid() {
    let hold = LiveSession {
        pid: 7001,
        background: true,
        background_hold: true,
        ..crate::testutil::live_row("7200-0", "bghold")
    };
    let launcher = LiveSession {
        pid: 7002,
        background: true,
        ..crate::testutil::live_row("7201-0", "bglaunch")
    };
    let rows = [hold, launcher];

    let joined = newest_per_pid(&rows);

    assert_eq!(joined.len(), 1, "only the launcher joins its pid");
    assert_eq!(joined[0].session_id, "7201-0");
}
