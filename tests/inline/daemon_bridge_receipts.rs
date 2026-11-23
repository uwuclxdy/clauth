//! Answer receipts: a mod declaring `answer_receipts` reports what became of
//! each answer it is handed, and the asks route answers the device only once
//! that report or the ask's close settles it: 200 once applied, `409
//! ask_closed` once closed without it, `503 answer_withdrawn` when no poll
//! took it within the wait, `504 answer_unconfirmed` when one took it and no
//! receipt came. Answers go through the real route; the mod's side is its
//! events and its polls.

#![cfg(unix)]

use super::*;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, body_json, call, req, seed_device};

/// Long enough that a route still waiting at it has not answered.
const PARKED: Duration = Duration::from_millis(400);
/// Long enough that a route that was answered has surely returned by it.
const WOKEN: Duration = Duration::from_secs(5);
/// A receipt wait no test reaches: a route answering under it was woken.
const LONG_WAIT: Duration = Duration::from_secs(60);
/// A receipt wait a test reaches, still far above a test's own receipt post.
const SHORT_WAIT: Duration = Duration::from_secs(1);

/// What a receipt-declaring mod declares, on its start and on every poll.
const RECEIPTS: &[&str] = &["notes", "answer_receipts"];
const RECEIPT_CAPS: &[Cap] = &[Cap::Notes, Cap::AnswerReceipts];

const WITHDRAWN: &str =
    "the session's mod did not take the answer in time, so it was taken back; send it again.";
const UNCONFIRMED: &str = "the session's mod took the answer but did not confirm it in time; it may still apply, and the ask closes if it does.";

fn ok() -> (u16, Value) {
    (200, json!({"ok": true}))
}

fn refused(status: u16, code: &str, reason: &str) -> (u16, Value) {
    (
        status,
        json!({"ok": false, "error": code, "reason": reason}),
    )
}

fn ask_closed() -> (u16, Value) {
    refused(409, "ask_closed", "that ask is already answered or closed")
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

/// One live mod instance at pid 100 on `s-one` behind the real asks route,
/// its asks open and held for a connected control device.
struct World {
    hub: Arc<Hub>,
    ctx: Arc<ApiContext>,
    /// A frame tap opened before the asks: every frame the hub sends.
    frames: Subscription,
    /// The frames the tap read but no accessor took yet.
    seen: Mutex<VecDeque<(String, Value)>>,
    seq: AtomicU64,
    /// The control-tier subscription that makes the daemon hold the asks: its
    /// frames are drained and dropped, so a full channel never lapses it.
    control: Subscription,
    _home: HomeSandbox,
}

impl World {
    /// The hub waits `wait` on a receipt; the mod's start declares `caps`.
    fn new(wait: Duration, caps: &[&str], asks: &[&str]) -> Self {
        let home = HomeSandbox::new();
        seed_device(DEVICE, Tier::Control, TOKEN);
        let rows: HashMap<u32, Instance> = [(
            100,
            Instance {
                session_id: "s-one".to_string(),
                proc_start: "1".to_string(),
            },
        )]
        .into_iter()
        .collect();
        let table = Mutex::new(rows);
        let resolver: PeerResolver = Box::new(move |pid| table.lock().unwrap().get(&pid).cloned());
        let hub = Hub::with_receipt_wait(true, resolver, wait);
        let control = hub.subscribe_as(Some(&Tier::Control));
        let frames = hub.subscribe();
        let status_path = crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("status.json");
        let ctx =
            ApiContext::for_tests_with_bridge(config(), status_path, no_herdr(), Arc::clone(&hub));
        let world = Self {
            hub,
            ctx,
            frames,
            seen: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
            control,
            _home: home,
        };
        // A receipt-declaring mod reports prompt outcomes and numbers its load
        // like every 0.19.0 mod: the fixture announces what a shipped load
        // sends, on its start and on every poll.
        assert_eq!(
            world.post(json!({
                "kind": "start",
                "cwd": "/w",
                "interactive": true,
                "prompt_reports": true,
                "load": 1,
                "caps": caps,
            })),
            200
        );
        for ask in asks {
            let opened = json!({"kind": "ask_opened", "ask": ask, "tool": "Bash", "input": {"command": "ls"}});
            let body = world.post_for(opened);
            assert_eq!(
                body,
                (200, json!({"ok": true, "remote": true, "hold": true}))
            );
        }
        world
    }

    /// Post one mod event under the session and the next seq.
    fn post(&self, body: Value) -> u16 {
        self.post_for(body).0
    }

    fn post_for(&self, mut body: Value) -> (u16, Value) {
        body["session"] = json!("s-one");
        body["seq"] = json!(self.seq.fetch_add(1, Ordering::Relaxed) + 1);
        self.hub.event(100, body.to_string().as_bytes())
    }

    fn receipt(&self, ask: &str, state: &str) -> u16 {
        self.post(json!({"kind": "answer_receipt", "ask": ask, "state": state}))
    }

    /// A device's `allow` to `ask` through the route, on its own thread.
    fn answer(&self, ask: &str) -> mpsc::Receiver<(u16, Value)> {
        let (tx, rx) = mpsc::channel();
        let ctx = Arc::clone(&self.ctx);
        let path = format!("/api/v1/sessions/s-one/asks/{ask}");
        std::thread::spawn(move || {
            let resp = call(
                &ctx,
                &req("POST", &path, Some(TOKEN), r#"{"decision":"allow"}"#),
            );
            // A test that stopped reading has failed already.
            drop(tx.send((resp.status, body_json(&resp))));
        });
        rx
    }

    /// One written poll of the receipt-declaring mod: its commands.
    fn poll(&self, wait: Duration) -> Vec<Value> {
        let announced = Announced {
            prompt_reports: true,
            caps: RECEIPT_CAPS.iter().copied().collect(),
            load: Some(1),
        };
        let PollAnswer {
            status,
            body,
            drained,
        } = self.hub.poll_announced(100, "s-one", wait, announced);
        assert_eq!(status, 200);
        if let Some(drained) = drained {
            self.hub.consume(drained);
        }
        body["commands"].as_array().cloned().unwrap_or_default()
    }

    /// Every frame the hub sent so far, moved into the buffer; the control
    /// subscription's are dropped, keeping its channel from filling.
    fn pump(&self) {
        let mut seen = self.seen.lock().unwrap();
        while let Ok(Some(frame)) = self.frames.try_next() {
            seen.push_back(frame);
        }
        while let Ok(Some(_)) = self.control.try_next() {}
    }

    /// The `session.ask` frames in `state` sent since the last call, leaving
    /// every other frame buffered for the next accessor.
    fn ask_frames(&self, state: &str) -> Vec<Value> {
        self.pump();
        let mut seen = self.seen.lock().unwrap();
        let mut taken = Vec::new();
        let mut kept = VecDeque::new();
        while let Some((event, data)) = seen.pop_front() {
            if event == "session.ask" && data["state"] == state {
                taken.push(data);
            } else {
                kept.push_back((event, data));
            }
        }
        *seen = kept;
        taken
    }

    /// The `session.ask` close frames sent since the last call, as `(ask, by)`.
    fn closes(&self) -> Vec<(String, Value)> {
        self.ask_frames("closed")
            .into_iter()
            .map(|data| {
                (
                    data["ask"].as_str().unwrap_or_default().to_string(),
                    data["by"].clone(),
                )
            })
            .collect()
    }

    /// The `session.ask` opened frames sent since the last call.
    fn opened(&self) -> Vec<Value> {
        self.ask_frames("opened")
    }
}

fn allow(ask: &str) -> Value {
    json!({"kind": "answer", "ask": ask, "decision": "allow"})
}

fn close(ask: &str, by: &str) -> (String, Value) {
    (ask.to_string(), json!(by))
}

/// The route answers only once the mod reports it applied the answer, and the
/// receipt itself wakes it: under a wait no test reaches, the route is still
/// waiting after the mod took the answer, and answers 200 on the receipt rather
/// than at the wait's end.
#[test]
fn an_applied_receipt_wakes_the_waiting_route_with_200() {
    let world = World::new(LONG_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    assert_eq!(world.poll(WOKEN), vec![allow("a1")]);
    assert!(
        route.recv_timeout(PARKED).is_err(),
        "the route answered before the mod's receipt"
    );

    assert_eq!(world.receipt("a1", "applied"), 200);
    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the receipt answers the route"),
        ok()
    );
    assert_eq!(world.closes(), vec![close("a1", "device")]);
}

/// The desk resolved the mod's hold before the device's answer reached the
/// mod, whose `ask_closed` has not landed yet. The mod reports it held nothing
/// for the answer: the route answers `409 ask_closed`, never 200, and the ask's
/// one close frame is the desk's, sent when that close lands.
#[test]
fn an_answer_reaching_a_mod_whose_hold_already_resolved_answers_409() {
    let world = World::new(LONG_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    assert_eq!(world.poll(WOKEN), vec![allow("a1")]);

    assert_eq!(world.receipt("a1", "not_held"), 200);

    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the receipt answers the route"),
        ask_closed()
    );
    assert_eq!(world.closes(), Vec::new(), "no close landed yet");
    assert_eq!(
        world.post(json!({"kind": "ask_closed", "ask": "a1", "by": "desk"})),
        200
    );
    assert_eq!(world.closes(), vec![close("a1", "desk")]);
}

/// No poll took the answer within the wait: the route answers `503
/// answer_withdrawn`, no later poll gets it, the ask is announced open again
/// with the fields its first `opened` frame carried, is listed open again in
/// its place and still held, and a second answer to it is taken.
#[test]
fn an_answer_no_poll_took_in_time_is_withdrawn_and_its_ask_reopens() {
    let world = World::new(SHORT_WAIT, RECEIPTS, &["a0", "a1", "a2"]);

    assert_eq!(
        world
            .answer("a1")
            .recv_timeout(WOKEN)
            .expect("the route answers at its wait"),
        refused(503, "answer_withdrawn", WITHDRAWN)
    );
    let reopened: Vec<Value> = world
        .opened()
        .into_iter()
        .filter(|frame| frame["ask"] == json!("a1"))
        .collect();
    assert_eq!(reopened.len(), 2, "the reopened ask is announced again");
    assert_eq!(
        reopened[1], reopened[0],
        "the reopened frame carries the same fields as the first"
    );

    assert_eq!(
        world.poll(Duration::ZERO),
        Vec::<Value>::new(),
        "a withdrawn answer reached a poll"
    );
    let listed: Vec<(Value, Value)> = world.hub.snapshot()["asks"]
        .as_array()
        .expect("the snapshot lists asks")
        .iter()
        .map(|ask| (ask["ask"].clone(), ask["held"].clone()))
        .collect();
    assert_eq!(
        listed,
        vec![
            (json!("a0"), json!(true)),
            (json!("a1"), json!(true)),
            (json!("a2"), json!(true)),
        ]
    );
    assert_eq!(
        world.closes(),
        Vec::new(),
        "a withdrawn answer closed its ask"
    );

    let again = world.answer("a1");
    assert_eq!(
        world.poll(WOKEN),
        vec![allow("a1")],
        "the second answer was not queued"
    );
    assert_eq!(world.receipt("a1", "applied"), 200);
    assert_eq!(
        again
            .recv_timeout(WOKEN)
            .expect("the receipt answers the route"),
        ok()
    );
}

/// A poll took the answer and no receipt came within the wait: the route
/// answers `504 answer_unconfirmed`, never 200; the ask stays closed to a
/// second answer, and a late `applied` receipt closes it `by: device`.
#[test]
fn a_taken_answer_with_no_receipt_is_unconfirmed_until_its_late_receipt() {
    let world = World::new(SHORT_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    assert_eq!(world.poll(WOKEN), vec![allow("a1")]);

    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the route answers at its wait"),
        refused(504, "answer_unconfirmed", UNCONFIRMED)
    );
    assert_eq!(
        world.closes(),
        Vec::new(),
        "an unconfirmed answer closed its ask"
    );
    assert_eq!(
        world
            .answer("a1")
            .recv_timeout(WOKEN)
            .expect("a second answer is refused at once"),
        ask_closed()
    );

    assert_eq!(world.receipt("a1", "applied"), 200);
    assert_eq!(world.closes(), vec![close("a1", "device")]);
}

/// Drain and withdraw never both happen: a poll racing the deadline either
/// takes the answer (the route then answers 504, no receipt coming) or finds
/// nothing (the route answers 503), and no later poll gets it either way. Each
/// round lands its poll at another point around the deadline.
#[test]
fn a_poll_racing_the_deadline_either_takes_the_answer_or_finds_it_withdrawn() {
    const WAIT: Duration = Duration::from_millis(30);
    let names: Vec<String> = (0..40).map(|i| format!("a{i}")).collect();
    let asks: Vec<&str> = names.iter().map(String::as_str).collect();
    let world = World::new(WAIT, RECEIPTS, &asks);
    for (round, ask) in (0u64..).zip(&asks) {
        let route = world.answer(ask);
        std::thread::sleep(Duration::from_millis(round * 2 % 60));
        let took = world.poll(Duration::ZERO).contains(&allow(ask));
        let (status, _) = route.recv_timeout(WOKEN).expect("the route answers");
        let later = world.poll(Duration::ZERO).contains(&allow(ask));
        match status {
            503 => assert!(
                !took && !later,
                "round {round}: a withdrawn answer reached a poll"
            ),
            504 => assert!(
                took && !later,
                "round {round}: an unconfirmed answer reached no poll, or two"
            ),
            other => panic!("round {round}: the route answered {other}"),
        }
    }
}

/// A poll whose answer write failed put the answer back before the deadline:
/// it counts as still queued, so the route withdraws it and no poll gets it.
#[test]
fn an_answer_requeued_by_a_failed_write_counts_as_still_queued() {
    let world = World::new(SHORT_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    let announced = Announced {
        prompt_reports: true,
        caps: RECEIPT_CAPS.iter().copied().collect(),
        load: Some(1),
    };
    let PollAnswer { body, drained, .. } = world.hub.poll_announced(100, "s-one", WOKEN, announced);
    assert_eq!(body["commands"], json!([allow("a1")]));
    world
        .hub
        .requeue(drained.expect("the poll drained the answer"));

    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the route answers at its wait"),
        refused(503, "answer_withdrawn", WITHDRAWN)
    );
    assert_eq!(world.poll(Duration::ZERO), Vec::<Value>::new());
}

/// The current load decides: after a reload onto a mod that does not declare
/// `answer_receipts`, an answer gets the immediate 200 and its `closed` `by:
/// device` frame at once, whatever the load before it declared.
#[test]
fn a_load_without_answer_receipts_gets_the_immediate_200() {
    let world = World::new(LONG_WAIT, RECEIPTS, &["a1"]);
    assert_eq!(
        world.post(json!({
            "kind": "start",
            "cwd": "/w",
            "interactive": true,
            "prompt_reports": true,
            "load": 2,
            "caps": ["notes"],
        })),
        200
    );

    assert_eq!(
        world
            .answer("a1")
            .recv_timeout(WOKEN)
            .expect("the route answers at once"),
        ok()
    );
    assert_eq!(world.closes(), vec![close("a1", "device")]);
}

/// The ask closing while the route waits answers `409 ask_closed` at once,
/// never after the wait: the desk's close, an interrupt's, and the session's
/// end, each with its own close frame.
#[test]
fn a_close_while_the_route_waits_answers_409_at_once() {
    let world = World::new(LONG_WAIT, RECEIPTS, &["a1", "a2", "a3"]);
    for (ask, by) in [("a1", "desk"), ("a2", "interrupt")] {
        let route = world.answer(ask);
        assert_eq!(world.poll(WOKEN), vec![allow(ask)]);
        assert_eq!(
            world.post(json!({"kind": "ask_closed", "ask": ask, "by": by})),
            200
        );
        assert_eq!(
            route
                .recv_timeout(WOKEN)
                .expect("the close answers the route"),
            ask_closed(),
            "{by}"
        );
    }
    let route = world.answer("a3");
    assert_eq!(world.poll(WOKEN), vec![allow("a3")]);
    assert_eq!(world.post(json!({"kind": "end"})), 200);
    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the end answers the route"),
        ask_closed()
    );

    assert_eq!(
        world.closes(),
        vec![
            close("a1", "desk"),
            close("a2", "interrupt"),
            close("a3", "session_end"),
        ]
    );
}

/// A malformed `answer_receipt` is refused before the sequence advances: a
/// missing or non-string `ask`, or a state other than `applied`/`not_held`.
#[test]
fn a_malformed_answer_receipt_is_refused() {
    let world = World::new(Duration::from_secs(1), RECEIPTS, &["a1"]);
    for body in [
        json!({"kind": "answer_receipt", "state": "applied"}),
        json!({"kind": "answer_receipt", "ask": 7, "state": "applied"}),
        json!({"kind": "answer_receipt", "ask": "a1", "state": "maybe"}),
        json!({"kind": "answer_receipt", "ask": "a1"}),
    ] {
        assert_eq!(world.post(body.clone()), 400, "{body}");
    }
}

/// A parked `/wait` took the answer before the receipt wait ended, so no poll
/// can get it back: the route answers `504 answer_unconfirmed` and never
/// withdraws the answer, which the mod may still apply. The command stays
/// queued for the poll the mod runs it from.
#[test]
fn a_wait_taken_answer_with_no_receipt_is_unconfirmed() {
    let world = World::new(SHORT_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    let deadline = Instant::now() + WOKEN;
    let taken = loop {
        let (status, body) = world
            .hub
            .wait_ask(100, "s-one", "a1", Duration::from_millis(50));
        assert_eq!(status, 200);
        if body["closed"] == json!(true) {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "the answer never reached the wait"
        );
    };
    assert_eq!(taken["answer"]["decision"], json!("allow"));
    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the route answers at its wait"),
        refused(504, "answer_unconfirmed", UNCONFIRMED)
    );
    assert_eq!(world.poll(Duration::ZERO), vec![allow("a1")]);
}

/// An `applied` after a `not_held` still closes the ask `by: device`, once, and
/// leaves nothing for the instance's end to close again: two module loads can
/// see one answer, the one holding nothing reporting first.
#[test]
fn an_applied_after_not_held_closes_the_ask_by_device() {
    let world = World::new(LONG_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    assert_eq!(world.poll(WOKEN), vec![allow("a1")]);
    assert_eq!(world.receipt("a1", "not_held"), 200);
    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the receipt answers the route"),
        ask_closed()
    );
    assert_eq!(world.closes(), Vec::new(), "a not_held close is the mod's");

    assert_eq!(world.receipt("a1", "applied"), 200);
    assert_eq!(world.closes(), vec![close("a1", "device")]);
    assert_eq!(world.post(json!({"kind": "end"})), 200);
    assert_eq!(
        world.closes(),
        Vec::new(),
        "the ended instance closed the ask a second time"
    );
}

/// An `ask_opened` re-posting the id of an ask whose answer waits is refused
/// `409 ask_awaited`: it neither lists the ask open beside its awaited answer
/// nor drops the payload a withdraw needs, so the answer is still taken back
/// and the ask reopens.
#[test]
fn a_repost_of_an_awaited_ask_is_refused() {
    let world = World::new(SHORT_WAIT, RECEIPTS, &["a1"]);
    let route = world.answer("a1");
    // The route queues the answer (taking the ask out of the open asks) under
    // the same lock hold that records it as awaited: until the snapshot drops
    // it, there is nothing for a re-post to collide with.
    let deadline = Instant::now() + WOKEN;
    while world.hub.snapshot()["asks"]
        .as_array()
        .is_some_and(|asks| asks.iter().any(|ask| ask["ask"] == json!("a1")))
    {
        assert!(
            Instant::now() < deadline,
            "the answer never left the open asks"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        world.post_for(
            json!({"kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {"command": "ls"}})
        ),
        (409, json!({"error": "ask_awaited"}))
    );
    assert_eq!(
        route
            .recv_timeout(WOKEN)
            .expect("the route answers at its wait"),
        refused(503, "answer_withdrawn", WITHDRAWN)
    );
}

/// An `end` whose final transcript tail outlasts the receipt wait answers the
/// route `409 ask_closed` while that tail runs: the phone is never told to send
/// the answer again for a session that is ending, and its ask is not announced
/// open again. Under a wait no test reaches, the end itself is what answers.
#[test]
fn an_end_whose_final_tail_outlasts_the_wait_answers_closed() {
    for wait in [SHORT_WAIT, LONG_WAIT] {
        let world = World::new(wait, RECEIPTS, &["a1"]);
        let route = world.answer("a1");
        // The route takes the ask out of the open asks under the same lock
        // hold that parks it on its receipt.
        let deadline = Instant::now() + WOKEN;
        while world.hub.snapshot()["asks"]
            .as_array()
            .is_some_and(|asks| asks.iter().any(|ask| ask["ask"] == json!("a1")))
        {
            assert!(
                Instant::now() < deadline,
                "{wait:?}: the answer never left the open asks"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // A tail the instance already runs when the end lands parks the end's
        // removal until the tail settles; the test holds it past the receipt
        // wait.
        world
            .hub
            .inner
            .lock()
            .unwrap()
            .instances
            .get_mut(&100)
            .expect("the instance is live")
            .tailing = true;
        let seq = world.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let hub = Arc::clone(&world.hub);
        let end = std::thread::spawn(move || {
            hub.event(
                100,
                format!(r#"{{"session":"s-one","seq":{seq},"kind":"end"}}"#).as_bytes(),
            )
        });
        let answered = route
            .recv_timeout(WOKEN)
            .unwrap_or_else(|_| panic!("{wait:?}: the end answers the route"));
        assert_eq!(answered, ask_closed(), "{wait:?}");
        {
            let inner = world.hub.inner.lock().unwrap();
            let live = inner.instances.get(&100).expect("the end still waits");
            assert!(
                live.ending && live.tailing,
                "{wait:?}: the wait ended for another reason"
            );
        }
        assert_eq!(
            world
                .opened()
                .iter()
                .filter(|frame| frame["ask"] == json!("a1"))
                .count(),
            1,
            "{wait:?}: an ending session's ask was announced open again"
        );
        world
            .hub
            .inner
            .lock()
            .unwrap()
            .instances
            .get_mut(&100)
            .expect("the instance is live")
            .tailing = false;
        world.hub.changed.notify_all();
        assert_eq!(
            end.join().expect("the end thread"),
            (200, json!({"ok": true})),
            "{wait:?}"
        );
        assert_eq!(world.closes(), vec![close("a1", "session_end")], "{wait:?}");
    }
}

/// An `applied` receipt answers 200 whatever ends the instance after it: the
/// receipt's wake races a forced silence close that removes the instance, at
/// every point the route's thread can lose that race.
#[test]
fn an_applied_receipt_answers_200_however_the_instance_ends_after_it() {
    let world = World::new(LONG_WAIT, RECEIPTS, &[]);
    for round in 0..40 {
        let ask = format!("a{round}");
        // A fresh instance: the round before ended it by silence.
        assert_eq!(world.poll(Duration::ZERO), Vec::<Value>::new());
        assert_eq!(
            world.post_for(json!({"kind": "ask_opened", "ask": ask, "tool": "Bash", "input": {"command": "ls"}})),
            (200, json!({"ok": true, "remote": true, "hold": true}))
        );
        let route = world.answer(&ask);
        assert_eq!(world.poll(WOKEN), vec![allow(&ask)]);
        assert_eq!(world.receipt(&ask, "applied"), 200);

        // The instance goes silent right after the receipt: the snapshot
        // closes it, racing the route's wake for the hub lock.
        world
            .hub
            .advance_clock(SILENCE_LIMIT + Duration::from_secs(1));
        drop(world.hub.snapshot());

        assert_eq!(
            route.recv_timeout(WOKEN).expect("the route answers"),
            ok(),
            "round {round}: an answer the mod applied read as closed"
        );
        assert_eq!(world.closes(), vec![close(&ask, "device")], "round {round}");
    }
}
