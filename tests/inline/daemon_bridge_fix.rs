#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The bridge's fix-round pins: the threat rows the first round left unpinned
//! (SB-3/SB-4/SB-10/SB-13 audits, poll replacement, silence, the read bound,
//! the hold flag's freshness) and the route-side gaps (herdr join, mod-first,
//! audit lines, the bind-failure wiring). Each reds on the pre-fix bytes or
//! under its plant — see the report.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
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

fn ncmds(answer: &(u16, serde_json::Value)) -> usize {
    answer.1["commands"].as_array().map_or(0, Vec::len)
}

fn start(hub: &Hub, pid: u32, session: &str) {
    let body = json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true});
    assert_eq!(ev(hub, pid, body).0, 200, "start for {session} from {pid}");
}

fn open_ask(hub: &Hub, pid: u32, session: &str, seq: u64, ask: &str) -> serde_json::Value {
    let body = json!({"session": session, "seq": seq, "kind": "ask_opened", "ask": ask, "tool": "Bash", "input": {"command": "ls"}});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "ask_opened {ask}: {answer}");
    answer
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

fn no_herdr() -> PaneProbe {
    Box::new(|_, _| HerdrProbeOut::NotInstalled)
}

fn ctx_over(hub: &Arc<Hub>) -> Arc<ApiContext> {
    ApiContext::for_tests_with_bridge(config(), status_path(), no_herdr(), Arc::clone(hub))
}

/// SB-4: a second poll from the same instance replaces the first; the replaced
/// poll must never take the instance's commands.
#[test]
fn a_replaced_poll_never_takes_commands() {
    let _home = HomeSandbox::new();
    let (mut a_won, mut b_won) = (0, 0);
    for _ in 0..20 {
        let (_t, r) = table(&[(100, instance("s1", "1"))]);
        let hub = Hub::new(true, r);
        assert_eq!(
            ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
            200
        );
        let ha = {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || hub.poll(100, "s1", Duration::from_secs(5)))
        };
        assert!(
            hub.wait_until_parked(100, 1, Duration::from_secs(5)),
            "the first poll parked"
        );
        let hb = {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || hub.poll(100, "s1", Duration::from_secs(5)))
        };
        assert!(
            hub.wait_until_parked(100, 2, Duration::from_secs(5)),
            "the second poll replaced the first"
        );
        assert!(matches!(
            hub.deliver("s1", json!({"kind":"prompt","text":"x"})),
            ModDelivery::Delivered
        ));
        let ra = ha.join().unwrap();
        let rb = hb.join().unwrap();
        a_won += usize::from(ncmds(&ra) > 0);
        b_won += usize::from(ncmds(&rb) > 0);
    }
    assert_eq!(
        a_won, 0,
        "the replaced poll took the command {a_won}/20 times"
    );
    assert_eq!(b_won, 20, "the live poll took the command {b_won}/20 times");
}

/// SB-4 "one live poll per instance": a replaced poll returns at once.
#[test]
fn a_replaced_poll_returns_at_once() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s1", "1"))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    let ha = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            let t = Instant::now();
            let _ = hub.poll(100, "s1", Duration::from_secs(3));
            t.elapsed()
        })
    };
    assert!(
        hub.wait_until_parked(100, 1, Duration::from_secs(5)),
        "the first poll parked"
    );
    let hb = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || hub.poll(100, "s1", Duration::from_secs(3)))
    };
    let a = ha.join().unwrap();
    let _ = hb.join().unwrap();
    assert!(
        a < Duration::from_secs(1),
        "the replaced poll stayed parked {a:?}"
    );
}

/// SB-4: a poll is served only its own instance's queue. After `/clear` the old
/// instance's parked poll must not take the new instance's command.
#[test]
fn a_cleared_instances_parked_poll_takes_nothing_new() {
    let _home = HomeSandbox::new();
    let (t, r) = table(&[(100, instance("s1", "1"))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    let ha = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || hub.poll(100, "s1", Duration::from_secs(3)))
    };
    assert!(
        hub.wait_until_parked(100, 1, Duration::from_secs(5)),
        "the poll parked"
    );
    t.lock().unwrap().insert(100, instance("s2", "1"));
    assert_eq!(
        ev(&hub, 100, json!({"session":"s2","seq":1,"kind":"start"})).0,
        200
    );
    assert!(matches!(
        hub.deliver("s2", json!({"kind":"prompt","text":"x"})),
        ModDelivery::Delivered
    ));
    let ra = ha.join().unwrap();
    let rb = hub.poll(100, "s2", Duration::ZERO);
    assert_eq!(ncmds(&ra), 0, "the cleared s1 poll took s2's command");
    assert_eq!(ncmds(&rb), 1, "the new s2 poll took its own command");
}

/// SB-13: only a parsed, different sessionId counts as moved; a transient
/// unreadable record (resolver None) must not close the instance and its asks
/// when the process is still alive and its start still matches (A#1).
#[test]
fn a_transient_unreadable_record_keeps_the_instance() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    // The test process's own pid and its real field-22 start, so a resolver
    // `None` keeps the instance: the process is alive and its start matches.
    // The macOS branch compares nothing (`procStart` there is unmeasured).
    let me = std::process::id();
    #[cfg(target_os = "linux")]
    let start = crate::testutil::proc_start_of(me).unwrap();
    #[cfg(target_os = "macos")]
    let start = "1".to_string();
    let (t, r) = table(&[(me, instance("s1", &start))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, me, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    let opened = ev(
        &hub,
        me,
        json!({"session":"s1","seq":2,"kind":"ask_opened","ask":"a1","tool":"Bash","input":{}}),
    );
    assert_eq!(opened.1["hold"], true);
    t.lock().unwrap().remove(&me);
    let _ = hub.live_session_ids();
    t.lock().unwrap().insert(me, instance("s1", &start));
    let _ = hub.poll(me, "s1", Duration::ZERO);
    let queued = matches!(
        hub.answer(
            "s1",
            "a1",
            json!({"kind":"answer","ask":"a1","decision":"allow"})
        ),
        AskOutcome::Queued
    );
    assert!(queued, "the ask survived an unreadable record");
}

/// The mod reads the line, so the daemon only carries what it posted: a number,
/// a null, or nothing at all from an older mod, which reads as null.
#[test]
fn an_ask_carries_the_start_line_its_mod_posted() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s1", "1"))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    for (seq, ask, posted) in [
        (2, "located", json!({"start_line": 42})),
        (3, "unlocated", json!({"start_line": null})),
        (4, "older-mod", json!({})),
    ] {
        let mut body = json!({"session":"s1","seq":seq,"kind":"ask_opened","ask":ask,"tool":"Edit","input":{}});
        body.as_object_mut()
            .expect("an object")
            .extend(posted.as_object().expect("an object").clone());
        assert_eq!(ev(&hub, 100, body).0, 200);
    }
    let snapshot = hub.snapshot();
    let lines: Vec<_> = snapshot["asks"]
        .as_array()
        .expect("the asks")
        .iter()
        .map(|ask| (ask["ask"].clone(), ask["start_line"].clone()))
        .collect();
    assert_eq!(
        lines,
        [
            (json!("located"), json!(42)),
            (json!("unlocated"), json!(null)),
            (json!("older-mod"), json!(null)),
        ]
    );
}

/// MB-7/SB-19: only a mod-HELD ask is answerable. An ask opened while
/// hold=false (no control device yet) must not take a 200 answer later.
#[test]
fn an_unheld_ask_is_not_answerable() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s1", "1"))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    let opened = ev(
        &hub,
        100,
        json!({"session":"s1","seq":2,"kind":"ask_opened","ask":"a1","tool":"Bash","input":{}}),
    );
    assert_eq!(opened.1["hold"], false);
    let question = ev(
        &hub,
        100,
        json!({"session":"s1","seq":3,"kind":"ask_opened","ask":"q1","tool":"AskUserQuestion","input":[{"question":"Which colour?"}]}),
    );
    assert_eq!(question.1["hold"], false);
    seed_device(DEVICE, Tier::Control, TOKEN);
    let queued = matches!(
        hub.answer(
            "s1",
            "a1",
            json!({"kind":"answer","ask":"a1","decision":"allow"})
        ),
        AskOutcome::Queued
    );
    assert!(!queued, "an ask the mod never held took an answer");
    // The held check comes before any form check: an unheld ask answers
    // ask_not_found to a wrong form and to an unknown question alike.
    for command in [
        json!({"kind":"answer","ask":"q1","decision":"allow"}),
        json!({"kind":"answer","ask":"q1","answers":{"Other?":"x"}}),
    ] {
        assert!(
            matches!(hub.answer("s1", "q1", command), AskOutcome::AskNotFound),
            "an unheld ask answers ask_not_found before any form check"
        );
    }
}

/// A 400 event (bad kind or a missing field) must not consume its sequence.
#[test]
fn a_refused_event_keeps_its_sequence() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s1", "1"))]);
    let hub = Hub::new(true, r);
    assert_eq!(
        ev(&hub, 100, json!({"session":"s1","seq":1,"kind":"start"})).0,
        200
    );
    assert_eq!(
        ev(
            &hub,
            100,
            json!({"session":"s1","seq":2,"kind":"ask_opened"})
        )
        .0,
        400
    );
    let retry = ev(
        &hub,
        100,
        json!({"session":"s1","seq":2,"kind":"ask_opened","ask":"a1","tool":"Bash","input":{}}),
    );
    assert_eq!(
        retry.0, 200,
        "the refused seq stayed spendable: {}",
        retry.1
    );
}

/// SB-10: a body over the 1 MiB limit is refused 413 and registers nothing.
fn ask_body(pad: usize) -> String {
    format!(
        r#"{{"session":"s-one","seq":1,"kind":"ask_opened","ask":"a1","tool":"Bash","input":{{"command":"{}"}}}}"#,
        "x".repeat(pad)
    )
}

#[test]
fn an_over_budget_event_body_is_refused() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let skeleton = ask_body(0).len();
    let at_limit = ask_body(BODY_LIMIT - skeleton);
    assert_eq!(at_limit.len(), BODY_LIMIT);
    assert_eq!(
        hub.event(100, at_limit.as_bytes()).0,
        200,
        "the limit is inclusive"
    );
    let over = ask_body(BODY_LIMIT + 1 - skeleton);
    assert_eq!(over.len(), BODY_LIMIT + 1);
    let (status, answer) = hub.event(100, over.as_bytes());
    assert_eq!(status, 413);
    assert_eq!(answer["error"], "body_too_large");
}

/// SB-6 "at or below": an equal sequence number is stale.
#[test]
fn an_equal_sequence_is_stale() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let turn =
        |seq: u64| json!({"session": "s-one", "seq": seq, "kind": "turn_start", "turn": "t"});
    assert_eq!(ev(&hub, 100, turn(3)).0, 200);
    let (status, answer) = ev(&hub, 100, turn(3));
    assert_eq!(status, 409);
    assert_eq!(answer["error"], "stale_sequence");
}

/// The per-instance high-water mark: a new `procStart` on the same pid is a new
/// instance whose fresh seq 1 is accepted.
#[test]
fn a_new_proc_start_restarts_the_sequence() {
    let _home = HomeSandbox::new();
    let (t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    t.lock().unwrap().insert(100, instance("s-one", "99"));
    let turn = json!({"session": "s-one", "seq": 1, "kind": "turn_start", "turn": "t"});
    assert_eq!(ev(&hub, 100, turn).0, 200, "a fresh instance accepts seq 1");
}

/// The `end` kind is sequence-checked like every other kind.
#[test]
fn the_end_kind_is_sequence_checked() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let (status, answer) = ev(
        &hub,
        100,
        json!({"session": "s-one", "seq": 1, "kind": "end"}),
    );
    assert_eq!(status, 409);
    assert_eq!(answer["error"], "stale_sequence");
}

/// The `end` kind closes the instance: a prompt afterwards reads session_not_live.
#[test]
fn the_end_kind_closes_the_instance() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    assert_eq!(
        ev(
            &hub,
            100,
            json!({"session": "s-one", "seq": 2, "kind": "end"})
        )
        .0,
        200
    );
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

/// The `end` kind wakes the instance's parked poll rather than leaving it to
/// wait out its park.
#[test]
fn the_end_kind_wakes_a_parked_poll() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let ha = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            let t = Instant::now();
            let _ = hub.poll(100, "s-one", Duration::from_secs(3));
            t.elapsed()
        })
    };
    assert!(
        hub.wait_until_parked(100, 1, Duration::from_secs(5)),
        "the poll parked"
    );
    assert_eq!(
        ev(
            &hub,
            100,
            json!({"session": "s-one", "seq": 2, "kind": "end"})
        )
        .0,
        200
    );
    let elapsed = ha.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "the end must wake the parked poll, not {elapsed:?}"
    );
}

/// SB-13: an instance whose poll is silent past 40 s closes, driven by the
/// hub's test clock rather than a sleep.
#[test]
fn a_silent_instance_closes() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    hub.advance_clock(Duration::from_secs(41));
    assert!(matches!(
        hub.deliver("s-one", json!({"kind": "prompt", "text": "x"})),
        ModDelivery::NoMod
    ));
}

/// SB-3: a session_mismatch refusal is audited with the peer pid, the named id,
/// the resolved id, and the body size — never the body.
#[test]
fn a_session_mismatch_is_audited() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 200, "s-two");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let forged =
        json!({"session": "s-two", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    let body = forged.to_string();
    let (status, _) = hub.event(100, body.as_bytes());
    assert_eq!(status, 403);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth bridge: peer 100 refused session 's-two' (resolved 's-one') body_len={}",
            body.len()
        )]
    );
}

/// SB-3 "not registered, logged once": an unknown peer is audited once, not on
/// every event.
#[test]
fn an_unknown_peer_is_audited_once() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let body =
        json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    let body = body.to_string();
    assert_eq!(hub.event(300, body.as_bytes()).0, 403);
    assert_eq!(hub.event(300, body.as_bytes()).0, 403);
    assert_eq!(
        lines.snapshot(),
        vec!["clauth bridge: peer 300 not registered".to_string()]
    );
}

/// SB-4: a second live instance registering on an id another holds is audited
/// with both pids.
#[test]
fn a_second_live_instance_is_audited() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-one", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    start(&hub, 200, "s-one");
    assert_eq!(
        lines.snapshot(),
        vec!["clauth bridge: session 's-one' now has 2 live instances (pids 100, 200)".to_string()]
    );
}

/// SB-4: the command route's 409 refusal is audited on every arm — prompt,
/// interrupt and answer — beside the bridge's registration line (B#1/B#3).
#[test]
fn the_ambiguous_refusal_is_audited() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-one", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    for (path, body) in [
        ("/api/v1/sessions/s-one/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-one/interrupt", "{}"),
        ("/api/v1/sessions/s-one/asks/a1", r#"{"decision":"allow"}"#),
    ] {
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(resp.status, 409, "{path}");
    }
    assert_eq!(
        lines.snapshot(),
        vec![
            format!("clauth api: device '{DEVICE}' refused on session 's-one': ambiguous"),
            format!("clauth api: device '{DEVICE}' refused on session 's-one': ambiguous"),
            format!("clauth api: device '{DEVICE}' refused on session 's-one': ambiguous"),
        ]
    );
}

/// SB-10: a body past the limit at the hub is audited (sizes only).
#[test]
fn an_over_budget_peer_is_audited() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let big = "a".repeat(BODY_LIMIT + 1);
    let (status, _) = hub.event(100, big.as_bytes());
    assert_eq!(status, 413);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth bridge: peer 100 refused: body_len={}",
            big.len()
        )]
    );
}

/// The read layer refuses a body past the limit from `Content-Length` alone,
/// before any body byte is read (SB-10).
#[test]
fn a_content_length_over_the_limit_is_refused_before_the_body() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    write!(
        a,
        "POST /event HTTP/1.1\r\nHost: x\r\nContent-Length: 2000000\r\n\r\npartial"
    )
    .unwrap();
    // Shut the writer half, so a build whose 413-from-Content-Length check is
    // dropped reads EOF and REDS rather than hanging on a body it never gets
    // (A#5).
    a.shutdown(std::net::Shutdown::Write).unwrap();
    assert!(matches!(
        read_request(&mut b, Instant::now() + Duration::from_secs(5)),
        Err(ReadError::BodyTooLarge(2_000_000))
    ));
}

/// Header field names are case-insensitive: a lowercase `content-length` still
/// frames the body.
#[test]
fn a_lowercase_content_length_is_parsed() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    write!(
        a,
        "POST /event HTTP/1.1\r\nHost: x\r\ncontent-length: 5\r\n\r\nhello"
    )
    .unwrap();
    let request = read_request(&mut b, Instant::now() + Duration::from_secs(5))
        .expect("a lowercase Content-Length parses");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/event");
    assert_eq!(request.body, b"hello");
}

/// Chunked transfer encoding is refused outright.
#[test]
fn chunked_transfer_is_refused() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    write!(
        a,
        "POST /event HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n"
    )
    .unwrap();
    assert!(matches!(
        read_request(&mut b, Instant::now() + Duration::from_secs(5)),
        Err(ReadError::Chunked)
    ));
}

/// The linux `procStart` compare reads field 22 of `/proc/<pid>/stat`: true for
/// the test process's own value, false for another.
#[cfg(target_os = "linux")]
#[test]
fn proc_start_matches_reads_the_tests_own_start_time() {
    let mine = crate::testutil::proc_start_of(std::process::id()).unwrap();
    assert!(crate::session_record::proc_start_matches(
        std::process::id(),
        &mine
    ));
    assert!(!crate::session_record::proc_start_matches(
        std::process::id(),
        "not-a-start"
    ));
}

/// MB-1: the hold flag reads the device store fresh per call — pairing a
/// control device after the hub starts flips the next ask and poll.
#[test]
fn the_hold_flag_reads_the_device_store_fresh() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    assert_eq!(open_ask(&hub, 100, "s-one", 2, "a1")["hold"], false);
    seed_device(DEVICE, Tier::Control, TOKEN);
    assert_eq!(open_ask(&hub, 100, "s-one", 3, "a2")["hold"], true);
    let (_, polled) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(polled["remote"], true);
}

/// A delivered command wakes a parked poll well under its wait.
#[test]
fn a_command_wakes_a_parked_poll() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let ha = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            let t = Instant::now();
            let (_, answer) = hub.poll(100, "s-one", Duration::from_secs(5));
            (t.elapsed(), answer)
        })
    };
    assert!(
        hub.wait_until_parked(100, 1, Duration::from_secs(5)),
        "the poll parked"
    );
    assert!(matches!(
        hub.deliver("s-one", json!({"kind": "prompt", "text": "x"})),
        ModDelivery::Delivered
    ));
    let (elapsed, answer) = ha.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "deliver must wake the poll, not {elapsed:?}"
    );
    assert_eq!(answer["commands"], json!([{"kind": "prompt", "text": "x"}]));
}

/// SB-16: the herdr fallback joins on `agent_session_id` alone, so a prompt for
/// one id drives only the pane carrying that id, and an id no pane carries
/// answers 404 even when another id's pane is live.
#[test]
fn the_herdr_fallback_drives_only_the_joined_pane() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_probe = Arc::clone(&seen);
    let probe: PaneProbe = Box::new(move |args, _| {
        seen_probe
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).to_string()).collect());
        match args {
            ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
                success: true,
                stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-other"}},{"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t2","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}}]}}"#.to_vec(),
                stderr: Vec::new(),
            })),
            ["agent", "prompt", "w1:p2", _] | ["pane", "send-keys", "w1:p2", "esc"] => {
                HerdrProbeOut::Ran(Some(HerdrOut {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }))
            }
            _ => HerdrProbeOut::Ran(None),
        }
    });
    let ctx = ApiContext::for_tests_with_bridge(config(), status_path(), probe, Arc::clone(&hub));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"fix it"}"#,
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (200, json!({"ok": true, "via": "herdr"}))
    );
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-zzz/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)["error"].clone()),
        (404, json!("session_not_live"))
    );

    let seen = seen.lock().unwrap().clone();
    assert!(seen.contains(&vec![
        "agent".to_string(),
        "prompt".to_string(),
        "w1:p2".to_string(),
        "fix it".to_string()
    ]));
    assert!(
        !seen.iter().any(|args| {
            args.first().map(String::as_str) == Some("agent")
                && args.get(2).map(String::as_str) == Some("w1:p1")
        }),
        "the s-other pane must never be driven for s-one: {seen:?}"
    );
}

/// "to the mod first, else herdr": a live mod instance wins over a pane joined
/// on the same id, for both routes, and herdr is never asked.
#[test]
fn a_live_mod_wins_over_the_herdr_fallback() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_probe = Arc::clone(&seen);
    let probe: PaneProbe = Box::new(move |args, _| {
        seen_probe
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).to_string()).collect());
        match args {
            ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
                success: true,
                stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}}]}}"#.to_vec(),
                stderr: Vec::new(),
            })),
            _ => HerdrProbeOut::Ran(None),
        }
    });
    let ctx = ApiContext::for_tests_with_bridge(config(), status_path(), probe, Arc::clone(&hub));

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"fix it"}"#,
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (200, json!({"ok": true, "via": "mod"}))
    );
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/interrupt",
            Some(TOKEN),
            "{}",
        ),
    );
    assert_eq!(
        (resp.status, body_json(&resp)),
        (200, json!({"ok": true, "via": "mod"}))
    );

    let (_, polled) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(
        polled["commands"],
        json!([{"kind": "prompt", "text": "fix it"}, {"kind": "interrupt"}])
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "herdr must never be asked when a mod is live: {seen:?}"
    );
}

/// RE-1: the answer, interrupt, and herdr-path audit lines carry ids and sizes,
/// never prompt or answer text.
#[test]
fn the_answer_and_interrupt_audit_lines_carry_no_text() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    // mod side: two answer forms and an interrupt.
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let question = json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "q1", "tool": "AskUserQuestion", "input": [{"question": "Which colour?"}, {"question": "Other?"}]});
    assert_eq!(hub.event(100, question.to_string().as_bytes()).0, 200);
    open_ask(&hub, 100, "s-one", 3, "q2");
    let ctx = ctx_over(&hub);
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/q1",
            Some(TOKEN),
            r#"{"answers":{"Which colour?":"secret-canary","Other?":"x"},"notes":{"Other?":"note-canary"}}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/q2",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
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

    // herdr side: a prompt and an interrupt fall through to the joined pane.
    let (_t2, resolver2) = table(&[]);
    let hub2 = Hub::new(true, resolver2);
    let probe: PaneProbe = Box::new(|args, _| {
        match args {
        ["pane", "list"] => HerdrProbeOut::Ran(Some(HerdrOut {
            success: true,
            stdout: br#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent_status":"ok","focused":false,"agent_session":{"kind":"id","value":"s-one"}}]}}"#.to_vec(),
            stderr: Vec::new(),
        })),
        ["agent", "prompt", ..] | ["pane", "send-keys", ..] => {
            HerdrProbeOut::Ran(Some(HerdrOut {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            }))
        }
        _ => HerdrProbeOut::Ran(None),
    }
    });
    let ctx2 = ApiContext::for_tests_with_bridge(config(), status_path(), probe, Arc::clone(&hub2));
    let resp = call(
        &ctx2,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"secret-canary"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    let resp = call(
        &ctx2,
        &req(
            "POST",
            "/api/v1/sessions/s-one/interrupt",
            Some(TOKEN),
            "{}",
        ),
    );
    assert_eq!(resp.status, 200);

    assert_eq!(
        lines.snapshot(),
        vec![
            format!(
                "clauth api: device '{DEVICE}' answered ask 'q1' on session 's-one' answers=2 notes=1"
            ),
            format!(
                "clauth api: device '{DEVICE}' answered ask 'q2' on session 's-one' decision=allow"
            ),
            format!("clauth api: device '{DEVICE}' interrupted session 's-one' via mod"),
            format!("clauth api: device '{DEVICE}' prompted session 's-one' via herdr text_len=13"),
            format!("clauth api: device '{DEVICE}' interrupted session 's-one' via herdr"),
        ]
    );
}

/// The audit line's no-notes branch counts the answers alone: a note count of
/// zero is never appended.
#[test]
fn the_answer_audit_line_carries_no_note_count_without_a_note() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let question = json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "q1", "tool": "AskUserQuestion", "input": [{"question": "Which colour?"}]});
    assert_eq!(hub.event(100, question.to_string().as_bytes()).0, 200);
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/q1",
            Some(TOKEN),
            r#"{"answers":{"Which colour?":"red"}}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: device '{DEVICE}' answered ask 'q1' on session 's-one' answers=1"
        )]
    );
}

/// SB-1: a live squatter on the bridge socket leaves the daemon serving with no
/// bridge — the seam `serve_prepared` drives — and the bind failure is audited
/// with the path.
#[test]
fn a_squatter_on_the_bridge_socket_leaves_the_daemon_serving() {
    let sb = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let path = sb.home().join(".clauth/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _squatter = UnixListener::bind(&path).unwrap();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let ctx = crate::daemon::api::build_context(
        config(),
        status_path(),
        crate::daemon::LiveStores::default(),
        crate::usage::MemoryStores::empty_for_test(),
    );
    assert!(
        ctx.bridge.is_none(),
        "the daemon serves on without a bridge"
    );
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth daemon: no mod bridge on {}: a live listener holds the bridge socket",
            path.display()
        )]
    );
}

/// The runtime-tree helper derives both layouts from `paired_dir_names`: a
/// SHARED session under `LinkMode::Fake` shares the bare `runtime` stem, while
/// every other shape is keyed `-<sid>` (A#9, B#24).
#[test]
fn session_runtime_trees_covers_the_fake_layout() {
    let _home = HomeSandbox::new();
    let profile = crate::profile::ProfileName::from("alpha");
    let [real, fake] =
        crate::runtime::session_runtime_trees(&profile, false, "123-4").expect("trees");
    assert_eq!(real.file_name().unwrap().to_str().unwrap(), "runtime-123-4");
    assert_eq!(
        fake.file_name().unwrap().to_str().unwrap(),
        "runtime",
        "a shared Fake session is the bare stem"
    );
    let [real, fake] =
        crate::runtime::session_runtime_trees(&profile, true, "123-4").expect("trees");
    assert_eq!(
        real.file_name().unwrap().to_str().unwrap(),
        "runtime-isolated-123-4"
    );
    assert_eq!(
        fake.file_name().unwrap().to_str().unwrap(),
        "runtime-isolated-123-4"
    );
}
