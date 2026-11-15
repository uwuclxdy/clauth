#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The bridge's round-2 pins: the dead-instance close (A#1), the connection
//! budgets and the whole-request deadline (A#2/A#3/A#9), the `last_poll`
//! refresh (A#4/n17), the herdr-path ambiguous refusal audit (B#1), the third
//! live instance's audit (B#9), and the production resolver's tree pick (A#8).

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, body_json, call, req, seed_device};

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

fn instance(session_id: &str, proc_start: &str) -> Instance {
    Instance {
        session_id: session_id.to_string(),
        proc_start: proc_start.to_string(),
    }
}

fn table(rows: &[(u32, Instance)]) -> (Table, PeerResolver) {
    let table: Table = Arc::new(Mutex::new(rows.iter().cloned().collect()));
    let read = Arc::clone(&table);
    let resolver: PeerResolver = Box::new(move |pid| read.lock().unwrap().get(&pid).cloned());
    (table, resolver)
}

fn ev(hub: &Hub, pid: u32, body: serde_json::Value) -> (u16, serde_json::Value) {
    hub.event(pid, body.to_string().as_bytes())
}

fn start(hub: &Hub, pid: u32, session: &str) {
    let body = json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true});
    assert_eq!(ev(hub, pid, body).0, 200, "start for {session} from {pid}");
}

/// Claim an event slot through [`claim_event`] — the accept loop's own gate —
/// and wrap the returned stream in the [`Slot`] the loop would hand to
/// [`serve`], so a direct unit `serve` runs through the real over-budget check
/// and releases the slot exactly as the loop does (B#2).
fn serve_claimed(
    stream: UnixStream,
    hub: &Arc<Hub>,
    events: &AtomicUsize,
    parked: &AtomicUsize,
    limits: Limits,
) {
    let Some(stream) = claim_event(stream, events, limits) else {
        return;
    };
    let event = Slot(events);
    serve(stream, hub, event, parked, limits);
}

/// Serve one `GET /poll?<query>` through the real `serve` path with the reader
/// kept alive, and return the response as a string.
fn serve_poll_once(
    hub: &Arc<Hub>,
    events: &AtomicUsize,
    parked: &AtomicUsize,
    query: &str,
) -> String {
    let (mut a, b) = UnixStream::pair().expect("pair");
    // Set before the peer end is served and dropped: macOS refuses
    // SO_RCVTIMEO (EINVAL) on a socket whose peer already closed.
    a.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    a.write_all(format!("GET /poll?{query} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .expect("write request");
    serve_claimed(b, hub, events, parked, Limits::default());
    let mut out = String::new();
    let _ = a.read_to_string(&mut out);
    out
}

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn status_path() -> std::path::PathBuf {
    crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json")
}

/// SB-19: a resolver `None` whose process is gone (linux: field 22 of
/// `/proc/<pid>/stat` does not match the recorded start) closes the instance,
/// and a resume in another pid then owns the id alone (A#1).
#[cfg(target_os = "linux")]
#[test]
fn a_dead_instance_is_closed_and_a_resume_takes_over() {
    let _home = HomeSandbox::new();
    let (t, r) = table(&[(100, instance("s1", "1"))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    t.lock().unwrap().remove(&100);
    assert!(
        matches!(
            hub.deliver("s1", json!({"kind":"prompt","text":"x"})),
            ModDelivery::NoMod
        ),
        "a gone process's instance must close"
    );
    t.lock().unwrap().insert(200, instance("s1", "2"));
    assert_eq!(
        ev(&hub, 200, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    assert!(
        matches!(
            hub.deliver("s1", json!({"kind":"prompt","text":"y"})),
            ModDelivery::Delivered
        ),
        "the resume owns the id alone"
    );
}

/// SB-13: `last_poll` moves on every poll, so an instance that polls inside the
/// 40 s window survives past its creation time (A#4; n17 removes the refresh).
#[test]
fn a_polling_instance_survives_the_silence_limit() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    hub.advance_clock(Duration::from_secs(30));
    let (status, _) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(status, 200);
    hub.advance_clock(Duration::from_secs(30));
    assert!(
        matches!(
            hub.deliver("s-one", json!({"kind":"prompt","text":"x"})),
            ModDelivery::Delivered
        ),
        "a poll refreshes last_poll, so the instance outlives a 60 s span split by a poll"
    );
}

/// SB-4: the herdr fallback's two-pane 409 is audited on both routes with the
/// exact refusal line (B#1).
#[test]
fn the_herdr_ambiguous_refusal_is_audited() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let probe: PaneProbe = Box::new(|args, _| {
        match args {
        ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}},{"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t2","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}}]}}"#.to_vec(),
            stderr: Vec::new(),
        })),
        _ => HerdrProbeOut::Ran(None),
    }
    });
    let ctx = ApiContext::for_tests_with_bridge(config(), status_path(), probe, Arc::clone(&hub));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    for (path, body) in [
        ("/api/v1/sessions/s-one/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-one/interrupt", "{}"),
    ] {
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(
            (resp.status, body_json(&resp)["error"].clone()),
            (409, json!("session_ambiguous")),
            "{path}"
        );
    }
    assert_eq!(
        lines.snapshot(),
        vec![
            format!("clauth api: device '{DEVICE}' refused on session 's-one': ambiguous"),
            format!("clauth api: device '{DEVICE}' refused on session 's-one': ambiguous"),
        ]
    );
}

/// SB-3's once-memo is bounded: a pid that registers is forgotten, so the same
/// pid read as unknown again later is logged again (A#7).
#[test]
fn a_pid_is_logged_unknown_again_after_it_registers() {
    let _home = HomeSandbox::new();
    let (t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    t.lock().unwrap().remove(&100);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let body = json!({"session":"s-one","seq":1,"kind":"start","cwd":"/w","interactive":true});
    let body = body.to_string();
    assert_eq!(hub.event(100, body.as_bytes()).0, 403);
    assert_eq!(hub.event(100, body.as_bytes()).0, 403);
    t.lock().unwrap().insert(100, instance("s-one", "1"));
    assert_eq!(
        ev(&hub, 100, json!({"session":"s-one","seq":1,"kind":"start"})).0,
        200
    );
    t.lock().unwrap().remove(&100);
    assert_eq!(hub.event(100, body.as_bytes()).0, 403);
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth bridge: peer 100 not registered".to_string(),
            "clauth bridge: peer 100 not registered".to_string(),
        ]
    );
}

/// SB-4: a third (and later) live instance on an id is audited with the count
/// and every pid (B#9).
#[test]
fn a_third_live_instance_is_audited() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[
        (100, instance("s-one", "1")),
        (200, instance("s-one", "2")),
        (300, instance("s-one", "3")),
    ]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-one");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    start(&hub, 300, "s-one");
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth bridge: session 's-one' now has 3 live instances (pids 100, 200, 300)"
                .to_string()
        ]
    );
}

/// A non-string `procStart` reads empty rather than failing every other
/// `SessionRecord` parse (A#16; A#9's lenient-parse pin).
#[test]
fn a_non_string_proc_start_parses_leniently() {
    let _home = HomeSandbox::new();
    let sessions = crate::profile::claude_dir()
        .expect("claude dir")
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    let pid = std::process::id();
    let path = sessions.join(format!("{pid}.json"));
    std::fs::write(
        &path,
        format!(r#"{{"sessionId":"s-one","pid":{pid},"procStart":"12345"}}"#),
    )
    .expect("write");
    let record = crate::session_record::session_record(&sessions, pid).expect("parses");
    assert_eq!(record.session_id, "s-one");
    assert_eq!(record.proc_start, "12345");
    std::fs::write(
        &path,
        format!(r#"{{"sessionId":"s-one","pid":{pid},"procStart":12345}}"#),
    )
    .expect("write");
    let record = crate::session_record::session_record(&sessions, pid).expect("parses");
    assert_eq!(record.session_id, "s-one");
    assert_eq!(
        record.proc_start, "",
        "a non-string procStart reads empty, never fails the parse"
    );
}

/// `startedAt` reads as the integer it is; any other shape, or none, reads
/// `None` rather than failing the parse of every other field.
#[test]
fn a_record_start_parses_leniently() {
    let _home = HomeSandbox::new();
    let sessions = crate::profile::claude_dir()
        .expect("claude dir")
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    let pid = std::process::id();
    let path = sessions.join(format!("{pid}.json"));
    for (raw, expected) in [
        (r#","startedAt":1700000000000"#, Some(1_700_000_000_000)),
        (r#","startedAt":"1700000000000""#, None),
        (r#","startedAt":-5"#, None),
        (r#","startedAt":[[1]]"#, None),
        (r#","startedAt":{"ms":1}"#, None),
        ("", None),
    ] {
        std::fs::write(
            &path,
            format!(r#"{{"sessionId":"s-one","pid":{pid}{raw}}}"#),
        )
        .expect("write");
        let record = crate::session_record::session_record(&sessions, pid).expect("parses");
        assert_eq!(record.session_id, "s-one");
        assert_eq!(record.started_at, expected, "startedAt {raw}");
    }
}

#[cfg(target_os = "linux")]
fn register_self_row(session_id: &str) {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        session_id: session_id.to_string(),
        start_profile: "alpha".to_string(),
        pid: std::process::id(),
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
    .expect("register");
}

/// The production resolver joins the test's own live-sessions row to a sandbox
/// tree, reads `sessions/<pid>.json`, and verifies field 22 of
/// `/proc/<pid>/stat` against the record's `procStart` (A#8).
#[cfg(target_os = "linux")]
#[test]
fn the_production_resolver_reads_a_sandbox_tree() {
    let _home = HomeSandbox::new();
    let pid = std::process::id();
    let sid = format!("{pid}-0");
    let start = crate::testutil::proc_start_of(pid).expect("field 22");
    register_self_row(&sid);
    let profile = crate::profile::ProfileName::from("alpha");
    let [real, _fake] =
        crate::runtime::session_runtime_trees(&profile, false, &sid).expect("trees");
    let sessions = real.join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        format!(r#"{{"sessionId":"{sid}","pid":{pid},"procStart":"{start}"}}"#),
    )
    .expect("record");
    let resolved = production_resolver()(pid).expect("resolve from the real tree");
    assert_eq!(resolved.session_id, sid);
    assert_eq!(resolved.proc_start, start);
}

/// The same resolver over the Fake-layout tree (a SHARED session's bare
/// `runtime` stem), so the `.find(is_dir)` pick is pinned (A#8/A#9).
#[cfg(target_os = "linux")]
#[test]
fn the_production_resolver_reads_the_fake_tree() {
    let _home = HomeSandbox::new();
    let pid = std::process::id();
    let sid = format!("{pid}-0");
    let start = crate::testutil::proc_start_of(pid).expect("field 22");
    register_self_row(&sid);
    let profile = crate::profile::ProfileName::from("alpha");
    let [_real, fake] =
        crate::runtime::session_runtime_trees(&profile, false, &sid).expect("trees");
    let sessions = fake.join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        format!(r#"{{"sessionId":"{sid}","pid":{pid},"procStart":"{start}"}}"#),
    )
    .expect("record");
    let resolved = production_resolver()(pid).expect("resolve from the fake tree");
    assert_eq!(resolved.session_id, sid);
    assert_eq!(resolved.proc_start, start);
}

/// A background row resolves to the account's fixed `runtime-bg`, never a
/// per-session tree: a `clauth start <p> -- --bg` launcher's session record and
/// the daemon watchdog's tree both live there, and no per-session tree exists
/// for the row to find.
/// A background row resolves to the account's fixed `runtime-bg` through the
/// SB-5 parent arm — the shape production has: a `claude --bg` child's parent is
/// the `clauth start` holding the launcher row, and that row is a background
/// row with no hold flag. The daemon's own hold is never a bridge peer, so it is
/// deliberately not posed here.
#[cfg(target_os = "linux")]
#[test]
fn the_production_resolver_reads_a_background_rows_tree() {
    let _home = HomeSandbox::new();
    let mut peer = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .expect("the peer child");
    let peer_pid = peer.id();
    let parent = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{peer_pid}/stat")).expect("stat");
    let start = stat
        .rsplit_once(')')
        .expect("comm")
        .1
        .split_whitespace()
        .nth(19)
        .expect("field 22")
        .to_string();
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        session_id: format!("{parent}-0"),
        start_profile: "alpha".to_string(),
        pid: parent,
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
        background: true,
        background_hold: false,
        visited_members: vec![],
    })
    .expect("register the launcher row");
    let _launch = crate::daemon::bridge::LaunchOverride::pose(parent);
    let profile = crate::profile::ProfileName::from("alpha");
    let tree = crate::runtime::background_runtime_tree(&profile).expect("tree");
    let sessions = tree.join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join(format!("{peer_pid}.json")),
        format!(r#"{{"sessionId":"4242-0","pid":{peer_pid},"procStart":"{start}"}}"#),
    )
    .expect("record");
    let resolved = production_resolver()(peer_pid).expect("resolve from the background tree");
    let _ = peer.kill();
    let _ = peer.wait();
    assert_eq!(resolved.session_id, "4242-0");
    assert_eq!(resolved.proc_start, start);
}

/// A peer no `live_sessions` row joins resolves from the global
/// `~/.claude/sessions` — the arm a shared tree on a box without the store
/// takes (SB-3).
#[cfg(target_os = "linux")]
#[test]
fn the_production_resolver_reads_the_global_store_for_an_unjoined_peer() {
    let _home = HomeSandbox::new();
    let mut peer = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .expect("the peer child");
    let peer_pid = peer.id();
    let start = crate::testutil::proc_start_of(peer_pid).expect("field 22");
    let sessions = crate::profile::claude_dir()
        .expect("claude dir")
        .join("sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join(format!("{peer_pid}.json")),
        format!(r#"{{"sessionId":"4242-0","pid":{peer_pid},"procStart":"{start}"}}"#),
    )
    .expect("record");
    let resolved = production_resolver()(peer_pid).expect("resolve from the global store");
    let _ = peer.kill();
    let _ = peer.wait();
    assert_eq!(resolved.session_id, "4242-0");
    assert_eq!(resolved.proc_start, start);
}

/// A peer trickling one byte at a time is cut off at the whole-request
/// deadline, not at any per-read bound (A#2/A#9).
#[test]
fn a_trickling_peer_is_cut_off_at_the_request_deadline() {
    let (mut a, mut b) = UnixStream::pair().expect("pair");
    let deadline = Instant::now() + Duration::from_millis(300);
    let _ = a.write_all(b"P");
    let start = Instant::now();
    let trickle = std::thread::spawn(move || {
        let mut a = a;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            if a.write_all(b"O").is_err() {
                break;
            }
        }
    });
    let result = read_request(&mut b, deadline);
    drop(b);
    let _ = trickle.join();
    assert!(
        matches!(result, Err(ReadError::Deadline)),
        "a trickling peer must be cut off at the whole-request deadline"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a per-read timeout would wait past the deadline: {:?}",
        start.elapsed()
    );
}

/// A connection over the event budget is answered 503 and logged, never
/// silently dropped, and refused in the accept loop before any thread spawns
/// (A#2/A#4/A#9).
#[test]
fn a_connection_over_the_event_budget_is_refused_with_a_503() {
    let _home = HomeSandbox::new();
    let (mut a, b) = UnixStream::pair().expect("pair");
    let events = AtomicUsize::new(MAX_EVENT_CONNECTIONS);
    let lines = crate::logline::LogLines::new();
    // Set before the peer end is served and dropped: macOS refuses
    // SO_RCVTIMEO (EINVAL) on a socket whose peer already closed.
    a.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let _capture = lines.capture_here();
    assert!(
        claim_event(b, &events, Limits::default()).is_none(),
        "a full event budget must refuse the connection"
    );
    assert_eq!(
        events.load(Ordering::Relaxed),
        MAX_EVENT_CONNECTIONS,
        "the refused claim must not leak a slot"
    );
    let mut out = String::new();
    let _ = a.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 503"), "got {out:?}");
    assert_eq!(
        lines.snapshot(),
        vec!["clauth bridge: refused a connection: 32 concurrent event connections".to_string()]
    );
}

/// A poll over the park budget is answered 503 and logged (A#2/A#9).
#[test]
fn a_poll_over_the_park_budget_is_refused_with_a_503() {
    let _home = HomeSandbox::new();
    let (mut a, b) = UnixStream::pair().expect("pair");
    let events = AtomicUsize::new(0);
    let parked = AtomicUsize::new(MAX_PARKED_POLLS);
    let (_t, r) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    // Set before the peer end is served and dropped: macOS refuses
    // SO_RCVTIMEO (EINVAL) on a socket whose peer already closed.
    a.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let _ = a.write_all(b"GET /poll?session=s-self HTTP/1.1\r\nHost: x\r\n\r\n");
    serve_claimed(b, &hub, &events, &parked, Limits::default());
    let mut out = String::new();
    let _ = a.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 503"), "got {out:?}");
    assert_eq!(
        lines.snapshot(),
        vec!["clauth bridge: refused a poll: 256 parked polls".to_string()]
    );
}

/// A parked poll leaves the event budget before it parks, so live sessions'
/// parked polls never eat the slot an event needs (P18/A#2/B#1). Deleting the
/// `drop(event)` before parking reds here: `events` stays 1 and `parked` reads
/// 0.
#[test]
fn a_parked_poll_leaves_the_event_budget() {
    let _home = HomeSandbox::new();
    let (mut a, b) = UnixStream::pair().expect("pair");
    let events = Arc::new(AtomicUsize::new(0));
    let parked = Arc::new(AtomicUsize::new(0));
    let (_t, r) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    let _ = a.write_all(b"GET /poll?session=s-self HTTP/1.1\r\nHost: x\r\n\r\n");
    let hub2 = Arc::clone(&hub);
    // Set before the peer end is served and dropped: macOS refuses
    // SO_RCVTIMEO (EINVAL) on a socket whose peer already closed.
    a.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let events2 = Arc::clone(&events);
    let parked2 = Arc::clone(&parked);
    let server = std::thread::spawn(move || {
        serve_claimed(b, &hub2, &events2, &parked2, Limits::default());
    });
    assert!(
        hub.wait_until_parked(std::process::id(), 1, Duration::from_secs(5)),
        "the poll must park"
    );
    assert_eq!(
        events.load(Ordering::Relaxed),
        0,
        "a parked poll leaves the event budget"
    );
    assert_eq!(
        parked.load(Ordering::Relaxed),
        1,
        "the parked poll holds the poll budget"
    );
    assert!(matches!(
        hub.deliver("s-self", json!({"kind": "prompt", "text": "hi"})),
        ModDelivery::Delivered
    ));
    let _ = server.join();
    let mut out = String::new();
    let _ = a.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 200"), "got {out:?}");
}

/// A peer that never reads its answer is cut off by the write timeout, so its
/// connection thread returns instead of blocking forever in `write!` (A#5).
#[test]
fn a_peer_that_never_reads_its_answer_is_cut_off_by_the_write_timeout() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, std::process::id(), "s-self");
    let big = "x".repeat(1024);
    for _ in 0..4096 {
        assert!(matches!(
            hub.deliver("s-self", json!({"kind": "prompt", "text": big})),
            ModDelivery::Delivered
        ));
    }
    let (mut a, b) = UnixStream::pair().expect("pair");
    let _ = a.write_all(b"GET /poll?session=s-self HTTP/1.1\r\nHost: x\r\n\r\n");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let events = AtomicUsize::new(0);
        let parked = AtomicUsize::new(0);
        serve_claimed(
            b,
            &hub,
            &events,
            &parked,
            Limits {
                request_deadline: Duration::from_secs(2),
                write_timeout: Duration::from_millis(200),
            },
        );
        let _ = done_tx.send(());
    });
    // The reader (`a`) never reads, so the ~4 MiB answer fills the pair's
    // buffer and the write times out instead of blocking the thread forever.
    assert!(
        done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "a peer that never reads must be cut off by the write timeout"
    );
    let _ = server.join();
}

/// A#1: a poll whose answer write fails (its reader is gone) puts the commands
/// it drained back at the front of its instance's queue, in order, so the next
/// poll gets them — on the pre-fix bytes they are drained and lost.
#[test]
fn a_poll_whose_answer_write_fails_requeues_its_commands() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, std::process::id(), "s-self");
    for command in [
        json!({"kind": "prompt", "text": "one"}),
        json!({"kind": "prompt", "text": "two"}),
        json!({"kind": "interrupt"}),
    ] {
        assert!(matches!(
            hub.deliver("s-self", command),
            ModDelivery::Delivered
        ));
    }
    let (mut a, b) = UnixStream::pair().expect("pair");
    let _ = a.write_all(b"GET /poll?session=s-self HTTP/1.1\r\nHost: x\r\n\r\n");
    drop(a); // the reader is gone: the answer write fails
    let events = AtomicUsize::new(0);
    let parked = AtomicUsize::new(0);
    serve_claimed(b, &hub, &events, &parked, Limits::default());
    let (status, answer) = hub.poll(std::process::id(), "s-self", Duration::ZERO);
    assert_eq!(status, 200);
    assert_eq!(
        answer["commands"],
        json!([
            {"kind": "prompt", "text": "one"},
            {"kind": "prompt", "text": "two"},
            {"kind": "interrupt"}
        ])
    );
}

/// A#1: a command route over the queue cap is refused 503 `session_busy` with
/// one audit line (ids only), never queued. Prompt, interrupt and answer share
/// the one line shape, pinned by equality for all three.
#[test]
fn a_full_queue_refuses_a_command_with_a_503() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let me = std::process::id();
    let (_t, r) = table(&[(me, instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    let _stream = hub.subscribe_as(Some(&Tier::Control));
    start(&hub, me, "s-self");
    // An open ask, so the answer route reaches its Busy arm (a Busy leaves the
    // ask open, so the poll below still sees only the queued interrupts).
    assert_eq!(
        ev(
            &hub,
            me,
            json!({"session": "s-self", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {"command": "ls"}})
        )
        .0,
        200
    );
    for _ in 0..MAX_QUEUED_COMMANDS {
        assert!(matches!(
            hub.deliver("s-self", json!({"kind": "interrupt"})),
            ModDelivery::Delivered
        ));
    }
    let ctx = ApiContext::for_tests_with_bridge(
        config(),
        status_path(),
        Box::new(|_, _| HerdrProbeOut::NotInstalled),
        Arc::clone(&hub),
    );
    let busy_line = format!("clauth api: device '{DEVICE}' refused on session 's-self': busy");
    for (path, body) in [
        ("/api/v1/sessions/s-self/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-self/interrupt", "{}"),
        ("/api/v1/sessions/s-self/asks/a1", r#"{"decision":"allow"}"#),
    ] {
        let lines = crate::logline::LogLines::new();
        let _capture = lines.capture_here();
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(resp.status, 503, "{path}");
        assert_eq!(body_json(&resp)["error"], "session_busy", "{path}");
        assert_eq!(lines.snapshot(), vec![busy_line.clone()], "{path}");
    }
    let (status, answer) = hub.poll(me, "s-self", Duration::ZERO);
    assert_eq!(status, 200);
    assert_eq!(
        answer["commands"].as_array().map_or(0, Vec::len),
        MAX_QUEUED_COMMANDS,
        "the refused command must never be queued"
    );
}

/// A long session: every command is delivered by a successful poll over the
/// real `serve` path, so the cap must never bite a mod that keeps polling
/// (R3: making `consume` a no-op leaks each poll's count into `inflight`, and
/// the cap bites once 8192 commands have ever been delivered).
#[test]
fn successful_polls_never_fill_the_cap() {
    let _home = HomeSandbox::new();
    let me = std::process::id();
    let (_t, r) = table(&[(me, instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, me, "s-self");
    let events = AtomicUsize::new(0);
    let parked = AtomicUsize::new(0);
    for i in 0..(MAX_QUEUED_COMMANDS + 1) {
        assert!(
            matches!(
                hub.deliver("s-self", json!({"kind": "interrupt"})),
                ModDelivery::Delivered
            ),
            "command {i} refused: a polling mod must never hit the cap"
        );
        let out = serve_poll_once(&hub, &events, &parked, "session=s-self");
        assert!(out.starts_with("HTTP/1.1 200"), "poll {i}: {out:?}");
    }
}

/// SB-4/A#1: a failed write whose instance was replaced (`/clear`) must not
/// leak its commands into the successor (R2: dropping the generation check
/// hands the old session's commands to the new one).
#[test]
fn a_replaced_instance_drops_a_failed_writes_commands() {
    let _home = HomeSandbox::new();
    let me = std::process::id();
    let (t, r) = table(&[(me, instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, me, "s-self");
    assert!(matches!(
        hub.deliver("s-self", json!({"kind": "prompt", "text": "old"})),
        ModDelivery::Delivered
    ));
    let PollAnswer { drained, .. } = hub.poll_answer(me, "s-self", Duration::ZERO, false);
    t.lock().unwrap().insert(me, instance("s-new", "1"));
    start(&hub, me, "s-new");
    hub.requeue(drained.expect("the poll drained the delivered command"));
    let (status, answer) = hub.poll(me, "s-new", Duration::ZERO);
    assert_eq!(status, 200);
    assert_eq!(
        answer["commands"],
        json!([]),
        "a replaced instance must drop the old session's commands"
    );
}

/// A#1 order: a command queued while a poll's write is in flight lands after
/// the requeued ones (R1: requeueing at the back runs a requeued prompt after
/// a command queued later).
#[test]
fn requeued_commands_go_ahead_of_newer_ones() {
    let _home = HomeSandbox::new();
    let me = std::process::id();
    let (_t, r) = table(&[(me, instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, me, "s-self");
    assert!(matches!(
        hub.deliver("s-self", json!({"kind": "prompt", "text": "one"})),
        ModDelivery::Delivered
    ));
    let PollAnswer { drained, .. } = hub.poll_answer(me, "s-self", Duration::ZERO, false);
    assert!(matches!(
        hub.deliver("s-self", json!({"kind": "prompt", "text": "two"})),
        ModDelivery::Delivered
    ));
    hub.requeue(drained.expect("the poll drained the delivered command"));
    let (status, answer) = hub.poll(me, "s-self", Duration::ZERO);
    assert_eq!(status, 200);
    assert_eq!(
        answer["commands"],
        json!([
            {"kind": "prompt", "text": "one"},
            {"kind": "prompt", "text": "two"}
        ])
    );
}

/// A live listener whose socket is mode 000 makes `bind`'s connect probe fail
/// with something other than refused/not-found, so the file is refused rather
/// than unlinked (A#11's connect-error refusal, A#9).
#[test]
fn a_mode_000_socket_is_refused_not_unlinked() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    let squatter = UnixListener::bind(&path).expect("bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    assert!(
        bind(&path).is_err(),
        "a connect error other than refused/not-found must refuse"
    );
    assert!(
        path.exists(),
        "the file is never unlinked on a non-refused connect error"
    );
    drop(squatter);
}

/// Idle peers hold the event budget only until their request deadline: a real
/// event is answered 503 while the budget is full, then served once the peers'
/// 3 s deadline frees their slots (probe_h's shape).
#[test]
fn idle_peers_hold_the_event_budget_only_until_their_deadline() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".clauth/bridge.sock");
    let (_t, r) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    let listener = bind(&path).expect("bind");
    let server = spawn_server_with_limits(
        listener,
        Arc::clone(&hub),
        Limits {
            request_deadline: Duration::from_secs(3),
            write_timeout: Duration::from_secs(3),
        },
    )
    .expect("spawn");
    let mut idle = Vec::new();
    for _ in 0..MAX_EVENT_CONNECTIONS {
        let mut s = UnixStream::connect(&path).expect("connect");
        s.write_all(b"P").expect("write");
        idle.push(s);
    }
    let post = || -> String {
        let body = json!({"session":"s-self","seq":1,"kind":"start"}).to_string();
        let mut s = UnixStream::connect(&path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let _ = write!(
            s,
            "POST /event HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out.lines().next().unwrap_or("<no response>").to_string()
    };
    let mut saw_503 = false;
    let started = Instant::now();
    while !saw_503 && started.elapsed() < Duration::from_secs(2) {
        let line = post();
        assert!(
            line.starts_with("HTTP/1.1"),
            "never a silent drop: {line:?}"
        );
        saw_503 = line.starts_with("HTTP/1.1 503");
    }
    assert!(saw_503, "the full event budget must answer 503");
    let mut got_200 = false;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let line = post();
        assert!(
            line.starts_with("HTTP/1.1"),
            "never a silent drop: {line:?}"
        );
        if line.starts_with("HTTP/1.1 200") {
            got_200 = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        got_200,
        "the idled-out peers must free the budget so the event is served"
    );
    server.stop();
}

/// A restarted daemon first meets a live mod by its poll: the query's
/// `prompt_reports=1`, read through the real `serve` path, turns prompt ids on
/// for that instance; a poll without it leaves them off.
#[test]
fn a_flagged_poll_over_the_socket_turns_prompt_ids_on() {
    let _home = HomeSandbox::new();
    let me = std::process::id();
    let events = AtomicUsize::new(0);
    let parked = AtomicUsize::new(0);

    let (_t, r) = table(&[(me, instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    let out = serve_poll_once(&hub, &events, &parked, "session=s-self");
    assert!(out.starts_with("HTTP/1.1 200"), "{out:?}");
    assert!(
        matches!(
            hub.deliver_prompt("s-self", "a"),
            PromptDelivery::Queued { prompt: None }
        ),
        "a flagless poll turned ids on"
    );

    let (_t, r) = table(&[(me, instance("s-self", "1"))]);
    let hub = Hub::new(true, r);
    let out = serve_poll_once(&hub, &events, &parked, "session=s-self&prompt_reports=1");
    assert!(out.starts_with("HTTP/1.1 200"), "{out:?}");
    assert!(
        matches!(
            hub.deliver_prompt("s-self", "a"),
            PromptDelivery::Queued { prompt: Some(_) }
        ),
        "a flagged poll left ids off"
    );
}
