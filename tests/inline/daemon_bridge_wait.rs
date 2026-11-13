#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The per-ask wait: `GET /wait?session=<id>&ask=<ask>` parks until that one
//! ask is answered or closed, so every held check of a session waits on its own
//! ask while the session's single parked poll keeps carrying prompts and
//! interrupts. Two holds both parking `/poll` would wake each other forever:
//! a new park replaces the instance's parked poll.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, call, req, seed_device};

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

/// Long enough that a wait still parked at it has not been woken.
const PARKED: Duration = Duration::from_millis(400);
/// Long enough that a woken wait has surely answered by it.
const WOKEN: Duration = Duration::from_secs(5);

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

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn no_herdr() -> PaneProbe {
    Box::new(|_, _| HerdrProbeOut::NotInstalled)
}

fn ctx_over(hub: &Arc<Hub>) -> Arc<ApiContext> {
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    ApiContext::for_tests_with_bridge(config(), status_path, no_herdr(), Arc::clone(hub))
}

fn ev(hub: &Hub, pid: u32, body: serde_json::Value) -> (u16, serde_json::Value) {
    hub.event(pid, body.to_string().as_bytes())
}

/// The sandbox, and the control device's `/events` subscription held open
/// beside it for the test's duration; the subscription drops first.
struct HeldHome {
    _stream: Subscription,
    _home: HomeSandbox,
}

/// A control device paired with a live control stream, a live session `s-one`
/// at pid 100, and its asks `asks` open and held (seqs 2.., after `start` at
/// 1).
fn session_with_asks(asks: &[&str]) -> (HeldHome, Arc<Hub>, Arc<ApiContext>) {
    let home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let home = HeldHome {
        _stream: hub.subscribe_as(Some(&Tier::Control)),
        _home: home,
    };
    let start = json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true});
    assert_eq!(ev(&hub, 100, start).0, 200);
    for (i, ask) in asks.iter().enumerate() {
        // A `q` ask is a question the answers name; any other a permission ask.
        let (tool, input) = if ask.starts_with('q') {
            ("AskUserQuestion", json!([{"question": "Which colour?"}]))
        } else {
            ("Bash", json!({"command": "ls"}))
        };
        let opened = json!({"session": "s-one", "seq": 2 + i as u64, "kind": "ask_opened", "ask": ask, "tool": tool, "input": input});
        let (status, answer) = ev(&hub, 100, opened);
        assert_eq!(
            (status, answer["hold"].clone()),
            (200, json!(true)),
            "ask_opened {ask}"
        );
    }
    let ctx = ctx_over(&hub);
    (home, hub, ctx)
}

/// Park `wait_ask` for `ask` on its own thread; the receiver yields its answer.
fn park(hub: &Arc<Hub>, ask: &str, wait: Duration) -> mpsc::Receiver<(u16, serde_json::Value)> {
    let (tx, rx) = mpsc::channel();
    let hub = Arc::clone(hub);
    let ask = ask.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(hub.wait_ask(100, "s-one", &ask, wait));
    });
    rx
}

fn answer(ctx: &Arc<ApiContext>, ask: &str, body: &str) {
    let resp = call(
        ctx,
        &req(
            "POST",
            &format!("/api/v1/sessions/s-one/asks/{ask}"),
            Some(TOKEN),
            body,
        ),
    );
    assert_eq!(resp.status, 200, "answer {ask}");
}

/// A device's answer wakes the wait parked on that ask, carrying the answer.
#[test]
fn a_device_answer_wakes_its_asks_wait() {
    let (_home, hub, ctx) = session_with_asks(&["a1"]);
    let waiting = park(&hub, "a1", Duration::from_secs(20));
    assert!(
        waiting.recv_timeout(PARKED).is_err(),
        "the wait must park until the ask is answered"
    );

    answer(&ctx, "a1", r#"{"decision":"allow"}"#);

    assert_eq!(
        waiting
            .recv_timeout(WOKEN)
            .expect("the answer wakes the wait"),
        (
            200,
            json!({"remote": true, "answer": {"decision": "allow"}, "closed": true})
        )
    );
}

/// The device can answer between `ask_opened` and the mod's first wait: that
/// wait answers at once with the answer, never parks past it.
#[test]
fn an_answer_landing_before_the_wait_is_returned_at_once() {
    let (_home, hub, ctx) = session_with_asks(&["q1"]);
    answer(&ctx, "q1", r#"{"answers":{"Which colour?":"blue"}}"#);

    assert_eq!(
        park(&hub, "q1", Duration::from_secs(20))
            .recv_timeout(WOKEN)
            .expect("an answered ask's wait answers at once"),
        (
            200,
            json!({"remote": true, "answer": {"answers": {"Which colour?": "blue"}}, "closed": true})
        )
    );
}

/// A note rides the wait reply's answer beside the answers it was typed with:
/// the parked wait is the mod's usual delivery path, so a note that survived
/// only the poll command would be lost.
#[test]
fn the_wait_reply_carries_the_answers_notes() {
    let (_home, hub, ctx) = session_with_asks(&["q1"]);
    answer(
        &ctx,
        "q1",
        r#"{"answers":{"Which colour?":"blue"},"notes":{"Which colour?":"a deeper blue"}}"#,
    );

    assert_eq!(
        park(&hub, "q1", Duration::from_secs(20))
            .recv_timeout(WOKEN)
            .expect("an answered ask's wait answers at once"),
        (
            200,
            json!({"remote": true, "answer": {"answers": {"Which colour?": "blue"}, "notes": {"Which colour?": "a deeper blue"}}, "closed": true})
        )
    );
}

/// Two held asks of one session wait side by side, and beside the session's
/// own parked poll: answering one wakes its wait alone, and neither wait
/// replaces the poll nor the other wait.
#[test]
fn two_asks_wait_side_by_side_without_waking_each_other() {
    let (_home, hub, ctx) = session_with_asks(&["a1", "a2"]);
    let (poll_tx, poll_rx) = mpsc::channel();
    {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            let _ = poll_tx.send(hub.poll(100, "s-one", Duration::from_secs(3)));
        });
    }
    std::thread::sleep(Duration::from_millis(100));
    let first = park(&hub, "a1", Duration::from_secs(20));
    let second = park(&hub, "a2", Duration::from_secs(20));
    assert!(
        first.recv_timeout(PARKED).is_err(),
        "a1's wait woke with nothing answered"
    );
    assert!(
        second.recv_timeout(PARKED).is_err(),
        "a2's wait woke with nothing answered"
    );

    answer(&ctx, "a2", r#"{"decision":"deny"}"#);

    assert_eq!(
        second
            .recv_timeout(WOKEN)
            .expect("a2's answer wakes a2's wait"),
        (
            200,
            json!({"remote": true, "answer": {"decision": "deny"}, "closed": true})
        )
    );
    assert!(
        first.recv_timeout(PARKED).is_err(),
        "a2's answer must not wake a1's wait"
    );

    answer(&ctx, "a1", r#"{"decision":"allow"}"#);

    assert_eq!(
        first
            .recv_timeout(WOKEN)
            .expect("a1's answer wakes a1's wait"),
        (
            200,
            json!({"remote": true, "answer": {"decision": "allow"}, "closed": true})
        )
    );
    let (status, polled) = poll_rx.recv_timeout(WOKEN).expect("the poll answers");
    assert_eq!(status, 200);
    assert_ne!(
        polled["commands"],
        json!([]),
        "the session's poll was woken empty: a wait replaced it"
    );
}

/// The desk answering closes the ask: its wait wakes closed, with no answer.
#[test]
fn a_desk_close_wakes_the_wait_closed_with_no_answer() {
    let (_home, hub, _ctx) = session_with_asks(&["a1"]);
    let waiting = park(&hub, "a1", Duration::from_secs(20));
    assert!(waiting.recv_timeout(PARKED).is_err());

    let closed =
        json!({"session": "s-one", "seq": 3, "kind": "ask_closed", "ask": "a1", "by": "desk"});
    assert_eq!(ev(&hub, 100, closed).0, 200);

    assert_eq!(
        waiting
            .recv_timeout(WOKEN)
            .expect("the close wakes the wait"),
        (200, json!({"remote": true, "answer": null, "closed": true}))
    );
}

/// The session ending closes every open ask: their waits wake closed.
#[test]
fn the_sessions_end_wakes_its_waits_closed() {
    let (_home, hub, _ctx) = session_with_asks(&["a1"]);
    let waiting = park(&hub, "a1", Duration::from_secs(20));
    assert!(waiting.recv_timeout(PARKED).is_err());

    let end = json!({"session": "s-one", "seq": 3, "kind": "end"});
    assert_eq!(ev(&hub, 100, end).0, 200);

    let (status, body) = waiting.recv_timeout(WOKEN).expect("the end wakes the wait");
    assert_eq!(status, 200);
    assert_eq!(
        (body["answer"].clone(), body["closed"].clone()),
        (json!(null), json!(true))
    );
}

/// A wait that reaches its bound answers with the ask still open, so the mod
/// waits again.
#[test]
fn a_wait_that_times_out_says_the_ask_is_still_open() {
    let (_home, hub, _ctx) = session_with_asks(&["a1"]);
    assert_eq!(
        hub.wait_ask(100, "s-one", "a1", Duration::from_millis(50)),
        (
            200,
            json!({"remote": true, "answer": null, "closed": false})
        )
    );
}

/// A wait names an ask its own session opened: an ask never opened is 404,
/// and a peer naming another session's ask is refused like every bridge call.
#[test]
fn a_wait_needs_an_ask_its_own_session_opened() {
    let (_home, hub, _ctx) = session_with_asks(&["a1"]);
    let (status, body) = hub.wait_ask(100, "s-one", "never", Duration::from_millis(50));
    assert_eq!(
        (status, body["error"].clone()),
        (404, json!("ask_not_found"))
    );

    let (status, body) = hub.wait_ask(100, "s-two", "a1", Duration::from_millis(50));
    assert_eq!(
        (status, body["error"].clone()),
        (403, json!("session_mismatch"))
    );
}

/// One instance holds at most `MAX_WAITS_PER_INSTANCE` parked waits; one more
/// is refused `503 busy` at once, so the mod hands that check to the stock
/// dialog instead of parking a connection without bound.
#[test]
fn waits_past_the_per_instance_cap_are_refused_busy() {
    let names: Vec<String> = (0..=MAX_WAITS_PER_INSTANCE)
        .map(|i| format!("a{i}"))
        .collect();
    let asks: Vec<&str> = names.iter().map(String::as_str).collect();
    let (_home, hub, _ctx) = session_with_asks(&asks);
    let parked: Vec<_> = asks[..MAX_WAITS_PER_INSTANCE]
        .iter()
        .map(|ask| park(&hub, ask, Duration::from_secs(5)))
        .collect();
    std::thread::sleep(PARKED);

    let (status, body) = hub.wait_ask(
        100,
        "s-one",
        asks[MAX_WAITS_PER_INSTANCE],
        Duration::from_secs(5),
    );
    assert_eq!((status, body["error"].clone()), (503, json!("busy")));
    for waiting in &parked {
        assert!(
            waiting.try_recv().is_err(),
            "the refusal must not wake a parked wait"
        );
    }
}

/// The route over the socket: `GET /wait?session=&ask=` answers the same body.
#[test]
fn the_wait_route_serves_over_the_socket() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let pid = std::process::id();
    let (_t, resolver) = table(&[(pid, instance("s-self", "1"))]);
    let hub = Hub::new(true, resolver);
    let _stream = hub.subscribe_as(Some(&Tier::Control));
    let start =
        json!({"session": "s-self", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    assert_eq!(ev(&hub, pid, start).0, 200);
    let opened = json!({"session": "s-self", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}});
    assert_eq!(ev(&hub, pid, opened).0, 200);
    let ctx = ctx_over(&hub);
    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-self/asks/a1",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(resp.status, 200);

    let (mut a, b) = UnixStream::pair().expect("pair");
    // Set before the peer end is served and dropped: macOS refuses
    // SO_RCVTIMEO (EINVAL) on a socket whose peer already closed.
    a.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    a.write_all(b"GET /wait?session=s-self&ask=a1 HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write request");
    let events = AtomicUsize::new(0);
    let parked = AtomicUsize::new(0);
    let Some(stream) = claim_event(b, &events, Limits::default()) else {
        panic!("the event slot was refused");
    };
    serve(stream, &hub, Slot(&events), &parked, Limits::default());
    let mut out = String::new();
    let _ = a.read_to_string(&mut out);

    assert!(out.starts_with("HTTP/1.1 200"), "got {out:?}");
    let body = out.split("\r\n\r\n").nth(1).expect("a body");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("json body"),
        json!({"remote": true, "answer": {"decision": "allow"}, "closed": true})
    );
}

/// A wait that times out releases its slot: after the cap's worth of waits end
/// on their own, one more parks instead of being refused `503 busy`.
#[test]
fn a_timed_out_wait_releases_its_slot() {
    let names: Vec<String> = (0..=MAX_WAITS_PER_INSTANCE)
        .map(|i| format!("a{i}"))
        .collect();
    let asks: Vec<&str> = names.iter().map(String::as_str).collect();
    let (_home, hub, _ctx) = session_with_asks(&asks);
    let parked: Vec<_> = asks[..MAX_WAITS_PER_INSTANCE]
        .iter()
        .map(|ask| park(&hub, ask, Duration::from_millis(50)))
        .collect();
    for waiting in &parked {
        assert_eq!(
            waiting.recv_timeout(WOKEN).expect("the wait times out"),
            (
                200,
                json!({"remote": true, "answer": null, "closed": false})
            )
        );
    }
    assert_eq!(
        hub.wait_ask(
            100,
            "s-one",
            asks[MAX_WAITS_PER_INSTANCE],
            Duration::from_millis(50)
        ),
        (
            200,
            json!({"remote": true, "answer": null, "closed": false})
        )
    );
}

/// A wait woken by a device answer releases its slot: after the cap's worth of
/// waits settle, one more parks instead of being refused `503 busy`.
#[test]
fn an_answered_wait_releases_its_slot() {
    let names: Vec<String> = (0..=MAX_WAITS_PER_INSTANCE)
        .map(|i| format!("a{i}"))
        .collect();
    let asks: Vec<&str> = names.iter().map(String::as_str).collect();
    let (_home, hub, ctx) = session_with_asks(&asks);
    let parked: Vec<_> = asks[..MAX_WAITS_PER_INSTANCE]
        .iter()
        .map(|ask| park(&hub, ask, Duration::from_secs(20)))
        .collect();
    std::thread::sleep(PARKED);
    for ask in &asks[..MAX_WAITS_PER_INSTANCE] {
        answer(&ctx, ask, r#"{"decision":"allow"}"#);
    }
    for waiting in &parked {
        assert_eq!(
            waiting
                .recv_timeout(WOKEN)
                .expect("the answer wakes the wait"),
            (
                200,
                json!({"remote": true, "answer": {"decision": "allow"}, "closed": true})
            )
        );
    }
    assert_eq!(
        hub.wait_ask(
            100,
            "s-one",
            asks[MAX_WAITS_PER_INSTANCE],
            Duration::from_millis(50)
        ),
        (
            200,
            json!({"remote": true, "answer": null, "closed": false})
        )
    );
}

/// A wait parked when the instance dies silent answers at once, not at its
/// 20 s bound: the silence close wakes it through the same removal path as
/// `end`.
#[test]
fn a_silent_close_wakes_its_wait_closed() {
    let (_home, hub, _ctx) = session_with_asks(&["a1"]);
    let waiting = park(&hub, "a1", Duration::from_secs(20));
    assert!(waiting.recv_timeout(PARKED).is_err());

    hub.advance_clock(Duration::from_secs(41));
    hub.sweep();

    let (status, body) = waiting
        .recv_timeout(WOKEN)
        .expect("the silent close wakes the wait");
    assert_eq!(status, 200);
    assert_eq!(
        (body["answer"].clone(), body["closed"].clone()),
        (json!(null), json!(true))
    );
}

/// A wait parked when the record moves (`/clear`, a recycled pid) answers at
/// once, not at its 20 s bound: the moved record's removal wakes it.
#[test]
fn a_moved_record_wakes_its_wait_closed() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let _stream = hub.subscribe_as(Some(&Tier::Control));
    let start =
        json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    assert_eq!(ev(&hub, 100, start).0, 200);
    let opened = json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}});
    assert_eq!(ev(&hub, 100, opened).0, 200);
    let waiting = park(&hub, "a1", Duration::from_secs(20));
    assert!(waiting.recv_timeout(PARKED).is_err());

    table.lock().unwrap().insert(100, instance("s-three", "1"));
    let moved =
        json!({"session": "s-three", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    assert_eq!(ev(&hub, 100, moved).0, 200);

    let (status, body) = waiting
        .recv_timeout(WOKEN)
        .expect("the moved record wakes the wait");
    assert_eq!(status, 200);
    assert_eq!(
        (body["answer"].clone(), body["closed"].clone()),
        (json!(null), json!(true))
    );
}

/// A wait that delivered an answer drops the payload: the ask still reads as
/// closed, but a second wait for it gets no answer, so a live instance never
/// accumulates one answer payload per closed ask.
#[test]
fn a_delivered_answers_payload_is_dropped_while_the_ask_stays_closed() {
    let (_home, hub, ctx) = session_with_asks(&["q1"]);
    answer(&ctx, "q1", r#"{"answers":{"Which colour?":"blue"}}"#);
    assert_eq!(
        hub.wait_ask(100, "s-one", "q1", Duration::from_secs(5)),
        (
            200,
            json!({"remote": true, "answer": {"answers": {"Which colour?": "blue"}}, "closed": true})
        )
    );
    assert_eq!(
        hub.wait_ask(100, "s-one", "q1", Duration::from_secs(5)),
        (200, json!({"remote": true, "answer": null, "closed": true}))
    );
}

/// A hold already lost (its check handed to the stock dialog, a `503 busy`, or
/// a rejected fetch) posts `ask_closed` after the device answer already closed
/// the ask: the late close is the ask's last consumer and must drop the kept
/// payload, keeping only the id.
#[test]
fn a_late_close_after_a_device_answer_drops_the_kept_payload() {
    let (_home, hub, ctx) = session_with_asks(&["q1"]);
    answer(&ctx, "q1", r#"{"answers":{"Which colour?":"blue"}}"#);
    let closed =
        json!({"session": "s-one", "seq": 3, "kind": "ask_closed", "ask": "q1", "by": "desk"});
    assert_eq!(ev(&hub, 100, closed).0, 200);

    let kept = hub
        .inner
        .lock()
        .unwrap()
        .instances
        .get(&100)
        .unwrap()
        .closed_asks
        .get("q1")
        .cloned();
    assert_eq!(kept, Some(json!(null)));
}

/// A poll delivering the device's answer command is that ask's last consumer via
/// the poll path: after delivery the payload is dropped, keeping only the id.
#[test]
fn a_polled_answer_drops_its_payload_with_no_wait_parked() {
    let (_home, hub, ctx) = session_with_asks(&["q1"]);
    answer(&ctx, "q1", r#"{"answers":{"Which colour?":"blue"}}"#);
    let (status, polled) = hub.poll(100, "s-one", Duration::from_millis(50));
    assert_eq!(status, 200);
    assert_eq!(
        polled["commands"],
        json!([{"kind": "answer", "ask": "q1", "answers": {"Which colour?": "blue"}}])
    );

    let kept = hub
        .inner
        .lock()
        .unwrap()
        .instances
        .get(&100)
        .unwrap()
        .closed_asks
        .get("q1")
        .cloned();
    assert_eq!(kept, Some(json!(null)));
}

/// A poll delivering an answer keeps the payload while a wait is parked on
/// that ask (the wait still owes it to the mod) and drops it once none is.
/// Posed through the hub's own records, so no thread timing decides it.
#[test]
fn a_polled_answer_keeps_its_payload_for_a_parked_wait_only() {
    let (_home, hub, ctx) = session_with_asks(&["q1", "a2"]);
    hub.inner
        .lock()
        .unwrap()
        .instances
        .get_mut(&100)
        .expect("the live instance")
        .parked_waits
        .insert("q1".to_string(), 1);

    answer(&ctx, "q1", r#"{"answers":{"Which colour?":"blue"}}"#);
    answer(&ctx, "a2", r#"{"decision":"allow"}"#);
    let (status, _) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(status, 200);

    let inner = hub.inner.lock().unwrap();
    let live = inner.instances.get(&100).expect("the live instance");
    assert_eq!(
        live.closed_asks.get("q1"),
        Some(&json!({"answers": {"Which colour?": "blue"}})),
        "a wait parked on q1 still owes its payload"
    );
    assert_eq!(
        live.closed_asks.get("a2"),
        Some(&serde_json::Value::Null),
        "no wait is parked on a2, so the poll was its last consumer"
    );
}

/// The parked path drops the payload too: a wait parked when the device answers
/// delivers the body, and a second wait for the same ask gets no answer — the
/// ask still reads as closed, but the payload is gone.
#[test]
fn a_parked_wait_that_delivered_its_answer_drops_the_payload() {
    let (_home, hub, ctx) = session_with_asks(&["q1"]);
    let waiting = park(&hub, "q1", Duration::from_secs(20));
    assert!(
        waiting.recv_timeout(PARKED).is_err(),
        "the wait must park until the ask is answered"
    );

    answer(&ctx, "q1", r#"{"answers":{"Which colour?":"blue"}}"#);

    assert_eq!(
        waiting
            .recv_timeout(WOKEN)
            .expect("the answer wakes the wait"),
        (
            200,
            json!({"remote": true, "answer": {"answers": {"Which colour?": "blue"}}, "closed": true})
        )
    );
    assert_eq!(
        hub.wait_ask(100, "s-one", "q1", Duration::from_secs(5)),
        (200, json!({"remote": true, "answer": null, "closed": true}))
    );
}
