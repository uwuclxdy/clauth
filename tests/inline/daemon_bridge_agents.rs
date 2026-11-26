//! Subagent turns on the bridge: each turn start and end a mod reports for a
//! subagent reaches subscribers as its own `session.agent` frame, never as the
//! main loop's `session.status`, and the snapshot carries each instance's open
//! main turn and the subagents with an open turn, read from the instance's own
//! state, so a frame a stream missed never pins either.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::testutil::HomeSandbox;

/// A pid no process holds on linux or macOS: above linux's largest `pid_max`
/// (2^22) and macOS's (99999), so a resolve that misses reads it gone.
const GONE: u32 = 2_000_000_000;

const WAIT: Duration = Duration::from_secs(5);

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

fn instance(session_id: &str, proc_start: &str) -> Instance {
    Instance {
        session_id: session_id.to_string(),
        proc_start: proc_start.to_string(),
    }
}

fn hub_over(rows: &[(u32, Instance)]) -> (Table, Arc<Hub>) {
    let table: Table = Arc::new(Mutex::new(rows.iter().cloned().collect()));
    let read = Arc::clone(&table);
    let resolver: PeerResolver = Box::new(move |pid| read.lock().unwrap().get(&pid).cloned());
    (table, Hub::new(true, resolver))
}

fn event(hub: &Hub, pid: u32, body: &serde_json::Value) {
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "{body}: {answer}");
}

/// A start of a mod that reports each prompt's outcome.
fn start(hub: &Hub, pid: u32, session: &str, seq: u64) {
    event(
        hub,
        pid,
        &json!({"session": session, "seq": seq, "kind": "start", "cwd": "/w/app", "interactive": true, "prompt_reports": true}),
    );
}

fn main_turn(hub: &Hub, pid: u32, session: &str, seq: u64, kind: &str, turn: &str) {
    event(
        hub,
        pid,
        &json!({"session": session, "seq": seq, "kind": kind, "turn": turn}),
    );
}

fn agent(
    hub: &Hub,
    pid: u32,
    session: &str,
    seq: u64,
    (agent_id, state, turn): (&str, &str, &str),
) {
    event(
        hub,
        pid,
        &json!({"session": session, "seq": seq, "kind": "agent_turn", "agent_id": agent_id, "state": state, "turn": turn}),
    );
}

/// Every frame broadcast since the last call, in order.
fn frames(sub: &Subscription) -> Vec<(String, serde_json::Value)> {
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = sub.try_next() {
        frames.push(frame);
    }
    frames
}

/// One `GET /poll?<query>` as `serve` reads it, its drained commands consumed
/// as a written answer's are.
fn poll_query(hub: &Hub, pid: u32, query: &str) -> (u16, serde_json::Value) {
    let path = format!("/poll?{query}");
    let session = parse_poll_session(&path).unwrap_or_default();
    let PollAnswer {
        status,
        body,
        drained,
    } = hub.poll_announced(pid, session, Duration::ZERO, Announced::from_poll(&path));
    if let Some(drained) = drained {
        hub.consume(drained);
    }
    (status, body)
}

/// A poll of a reporting mod that reports its open turns, as the mod's own
/// query does: `turn` empty for no open main turn, `agents` the
/// `<agent_id>:<turn>` entries joined by commas (a caller percent-encodes each
/// side).
fn poll_turns(
    hub: &Hub,
    pid: u32,
    session: &str,
    turn: &str,
    agents: &str,
) -> (u16, serde_json::Value) {
    poll_query(
        hub,
        pid,
        &format!("session={session}&prompt_reports=1&caps=&turn={turn}&agents={agents}"),
    )
}

fn agent_frame(
    session: &str,
    agent_id: &str,
    state: &str,
    turn: &str,
) -> (String, serde_json::Value) {
    (
        "session.agent".to_string(),
        json!({"session": session, "agent_id": agent_id, "state": state, "turn": turn}),
    )
}

/// The snapshot entry of an instance a reporting mod's start registered.
fn entry(session: &str, turn: Option<&str>, agents: &[(&str, &str)]) -> serde_json::Value {
    let agents: Vec<serde_json::Value> = agents
        .iter()
        .map(|(agent_id, turn)| json!({"agent_id": agent_id, "turn": turn}))
        .collect();
    json!({"session": session, "cwd": "/w/app", "interactive": true, "caps": [], "turn": turn, "agents": agents})
}

/// A subagent's turn start and end reach subscribers as one `session.agent`
/// frame each, while the main loop's turns around them keep their own
/// `session.status` frames: no subagent turn is relayed as the main loop's.
#[test]
fn a_subagent_turn_reaches_subscribers_as_its_own_frame() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    let sub = hub.subscribe();
    start(&hub, 100, "s-one", 1);
    main_turn(&hub, 100, "s-one", 2, "turn_start", "t-main");
    agent(
        &hub,
        100,
        "s-one",
        3,
        ("a50c94990f67d7980", "start", "t-sub"),
    );
    agent(&hub, 100, "s-one", 4, ("a50c94990f67d7980", "end", "t-sub"));
    main_turn(&hub, 100, "s-one", 5, "turn_end", "t-main");

    assert_eq!(
        frames(&sub),
        vec![
            (
                "session.status".to_string(),
                json!({"session": "s-one", "state": "start", "cwd": "/w/app", "interactive": true, "caps": []})
            ),
            (
                "session.status".to_string(),
                json!({"session": "s-one", "state": "turn_start", "turn": "t-main"})
            ),
            agent_frame("s-one", "a50c94990f67d7980", "start", "t-sub"),
            agent_frame("s-one", "a50c94990f67d7980", "end", "t-sub"),
            (
                "session.status".to_string(),
                json!({"session": "s-one", "state": "turn_end", "turn": "t-main"})
            ),
        ]
    );
}

/// A subagent resumed after its run ended gets a new start and end pair under
/// its first id, each naming the new turn; a start for a subagent still listed
/// replaces its turn rather than listing it twice.
#[test]
fn a_resumed_subagent_gets_a_new_pair_under_its_first_id() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    let sub = hub.subscribe();
    agent(&hub, 100, "s-one", 2, ("a1", "start", "t1"));
    agent(&hub, 100, "s-one", 3, ("a1", "end", "t1"));
    agent(&hub, 100, "s-one", 4, ("a1", "start", "t2"));
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a1", "t2")])])
    );
    agent(&hub, 100, "s-one", 5, ("a1", "end", "t2"));
    assert_eq!(
        frames(&sub),
        vec![
            agent_frame("s-one", "a1", "start", "t1"),
            agent_frame("s-one", "a1", "end", "t1"),
            agent_frame("s-one", "a1", "start", "t2"),
            agent_frame("s-one", "a1", "end", "t2"),
        ]
    );
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[])])
    );

    agent(&hub, 100, "s-one", 6, ("a1", "start", "t3"));
    agent(&hub, 100, "s-one", 7, ("a2", "start", "t4"));
    agent(&hub, 100, "s-one", 8, ("a1", "start", "t5"));
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a1", "t5"), ("a2", "t4")])])
    );
}

/// Every snapshot entry carries the open main turn's id and each subagent
/// with an open turn: an ended subagent leaves the next snapshot, and the main
/// turn's end reads `null` while a subagent still running past it stays
/// listed.
#[test]
fn the_snapshot_carries_the_open_main_turn_and_each_open_subagent_turn() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1")), (200, instance("s-two", "1"))]);
    start(&hub, 100, "s-one", 1);
    start(&hub, 200, "s-two", 1);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[]), entry("s-two", None, &[])])
    );

    main_turn(&hub, 100, "s-one", 2, "turn_start", "m1");
    agent(&hub, 100, "s-one", 3, ("a1", "start", "ta"));
    agent(&hub, 100, "s-one", 4, ("a2", "start", "tb"));
    main_turn(&hub, 200, "s-two", 2, "turn_start", "m2");
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            entry("s-one", Some("m1"), &[("a1", "ta"), ("a2", "tb")]),
            entry("s-two", Some("m2"), &[])
        ])
    );

    agent(&hub, 100, "s-one", 5, ("a1", "end", "ta"));
    main_turn(&hub, 100, "s-one", 6, "turn_end", "m1");
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            entry("s-one", None, &[("a2", "tb")]),
            entry("s-two", Some("m2"), &[])
        ])
    );

    agent(&hub, 100, "s-one", 7, ("a2", "end", "tb"));
    main_turn(&hub, 100, "s-one", 8, "turn_start", "m3");
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            entry("s-one", Some("m3"), &[]),
            entry("s-two", Some("m2"), &[])
        ])
    );
}

/// A main turn's end clears the snapshot's `turn` whatever id it names: the
/// `turn_start` a stream missed must not leave its id listed open. A
/// `turn_start` whose `turn` is not a string opens no turn while its frame
/// still relays the value it carried.
#[test]
fn a_main_turn_end_clears_the_turn_whatever_id_it_names() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    main_turn(&hub, 100, "s-one", 2, "turn_start", "m1");
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("m1"), &[])])
    );
    main_turn(&hub, 100, "s-one", 3, "turn_end", "other");
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[])])
    );

    let sub = hub.subscribe();
    event(
        &hub,
        100,
        &json!({"session": "s-one", "seq": 4, "kind": "turn_start", "turn": 7}),
    );
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[])])
    );
    assert_eq!(
        frames(&sub),
        vec![(
            "session.status".to_string(),
            json!({"session": "s-one", "state": "turn_start", "turn": 7})
        )]
    );
}

/// An end whose start the daemon never saw (a reconnect, a reload) still
/// relays its one frame, and lists nothing.
#[test]
fn an_end_whose_start_was_never_seen_relays_one_frame() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    agent(&hub, 100, "s-one", 2, ("a1", "start", "t1"));
    let sub = hub.subscribe();
    agent(&hub, 100, "s-one", 3, ("a9", "end", "t9"));

    assert_eq!(frames(&sub), vec![agent_frame("s-one", "a9", "end", "t9")]);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a1", "t1")])])
    );

    // A listed subagent's end removes it whatever turn it names: the end is
    // the run's close, and its turn id need not match the one it opened with.
    agent(&hub, 100, "s-one", 4, ("a1", "end", "other"));
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[])])
    );
}

/// Opens a main turn and one subagent turn on the instance at `pid`, from
/// `seq` on.
fn open_turns(hub: &Hub, pid: u32, session: &str, seq: u64) {
    main_turn(hub, pid, session, seq, "turn_start", "m1");
    agent(hub, pid, session, seq + 1, ("a1", "start", "ta"));
}

/// An instance whose `end` owns its final read lists no open turn and no
/// subagent while its entry stays listed; a live instance beside it keeps
/// its own.
#[test]
fn an_ending_instance_reads_no_turn_and_no_agents() {
    let sb = HomeSandbox::new();
    let transcript = sb.home().join(".claude/projects/-w-app/s-one.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, b"").unwrap();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    start(&hub, 100, "s-one", 1);
    start(&hub, 200, "s-two", 1);
    open_turns(&hub, 100, "s-one", 2);
    open_turns(&hub, 200, "s-two", 2);
    let generation = {
        let mut inner = hub.inner.lock().unwrap();
        let live = inner.instances.get_mut(&100).unwrap();
        live.tailing = true;
        live.generation
    };
    let (tx, ended) = std::sync::mpsc::channel();
    let ending = Arc::clone(&hub);
    std::thread::spawn(move || {
        let body = json!({"session": "s-one", "seq": 4, "kind": "end"});
        tx.send(ending.event(100, body.to_string().as_bytes()))
            .expect("the test holds the receiver until the end answers");
    });
    let deadline = std::time::Instant::now() + WAIT;
    while !hub
        .inner
        .lock()
        .unwrap()
        .instances
        .get(&100)
        .is_some_and(|live| live.ending)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the end never reached its final read"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            entry("s-one", None, &[]),
            entry("s-two", Some("m1"), &[("a1", "ta")])
        ])
    );

    hub.finish_tail(100, generation);
    assert_eq!(ended.recv_timeout(WAIT), Ok((200, json!({ "ok": true }))));
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-two", Some("m1"), &[("a1", "ta")])])
    );
}

/// Every path that removes an instance takes its open turn and subagents with
/// it: the instance the next contact on that pid makes, whatever the session,
/// lists none. A silence close is not the session's end, though: its kept
/// prompts come back on the mod's next poll with the open turns that poll
/// reports.
#[test]
fn every_instance_removal_takes_the_turn_and_agents_with_it() {
    let _home = HomeSandbox::new();
    let (records, hub) = hub_over(&[
        (100, instance("s-end", "1")),
        (200, instance("s-silent", "1")),
        (300, instance("s-clear", "1")),
        (400, instance("s-recycled", "1")),
        (GONE, instance("s-gone", "1")),
    ]);
    for (pid, session) in [
        (100, "s-end"),
        (200, "s-silent"),
        (300, "s-clear"),
        (400, "s-recycled"),
        (GONE, "s-gone"),
    ] {
        start(&hub, pid, session, 1);
        open_turns(&hub, pid, session, 2);
    }
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            entry("s-clear", Some("m1"), &[("a1", "ta")]),
            entry("s-end", Some("m1"), &[("a1", "ta")]),
            entry("s-gone", Some("m1"), &[("a1", "ta")]),
            entry("s-recycled", Some("m1"), &[("a1", "ta")]),
            entry("s-silent", Some("m1"), &[("a1", "ta")]),
        ])
    );

    // An `end`, then the same session registering again.
    event(
        &hub,
        100,
        &json!({"session": "s-end", "seq": 4, "kind": "end"}),
    );
    start(&hub, 100, "s-end", 5);
    // A `/clear`: the same process on a new session id.
    records
        .lock()
        .unwrap()
        .insert(300, instance("s-cleared", "1"));
    start(&hub, 300, "s-cleared", 1);
    // A recycled pid: another process on the same session id.
    records
        .lock()
        .unwrap()
        .insert(400, instance("s-recycled", "2"));
    start(&hub, 400, "s-recycled", 1);
    // A process gone: the next snapshot closes it.
    records.lock().unwrap().remove(&GONE);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            entry("s-cleared", None, &[]),
            entry("s-end", None, &[]),
            entry("s-recycled", None, &[]),
            entry("s-silent", Some("m1"), &[("a1", "ta")]),
        ])
    );

    // A silence close keeping a prompt, then the module's next poll, which
    // reports the turns the close dropped.
    let PromptDelivery::Queued { prompt: Some(kept) } = hub.deliver_prompt("s-silent", "fix it")
    else {
        panic!("no prompt id minted for s-silent");
    };
    hub.advance_clock(SILENCE_LIMIT + Duration::from_secs(1));
    hub.sweep();
    let (status, answer) = poll_turns(&hub, 200, "s-silent", "m1", "a1:ta");
    assert_eq!(status, 200);
    assert_eq!(
        answer["commands"],
        json!([{"kind": "prompt", "text": "fix it", "prompt": kept}])
    );
    let snapshot = hub.snapshot();
    assert_eq!(
        snapshot["sessions"],
        json!([
            {"session": "s-silent", "cwd": null, "interactive": null, "caps": [], "turn": "m1", "agents": [{"agent_id": "a1", "turn": "ta"}]},
        ])
    );
    assert_eq!(
        snapshot["prompts"],
        json!([{"session": "s-silent", "prompt": kept, "state": "queued"}])
    );
}

/// An accepted poll that reports its open turns replaces both fields: the
/// subagent whose end post was lost drops out, a subagent the daemon never saw
/// start is listed, and a main turn id lands although no `turn_start` was
/// taken. No frame comes from a poll: each change shows in the next snapshot
/// alone.
#[test]
fn a_poll_replaces_the_open_turns_it_reports() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    main_turn(&hub, 100, "s-one", 2, "turn_start", "m1");
    agent(&hub, 100, "s-one", 3, ("a1", "start", "ta"));
    let sub = hub.subscribe();
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("m1"), &[("a1", "ta")])])
    );

    // The subagent's end never reached the daemon; the poll reports no open
    // subagent and a newer main turn.
    let (status, _) = poll_turns(&hub, 100, "s-one", "m2", "");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("m2"), &[])])
    );

    // A poll lists a subagent whose start the daemon never saw, and its empty
    // `turn` clears the main turn.
    let (status, _) = poll_turns(&hub, 100, "s-one", "", "a2:tb");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a2", "tb")])])
    );

    // The next poll replaces the whole list again, in its own order.
    let (status, _) = poll_turns(&hub, 100, "s-one", "m3", "a2:tc,a3:td");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("m3"), &[("a2", "tc"), ("a3", "td")])])
    );

    // A poll naming a main turn but carrying no `agents` key lists no open
    // subagent.
    let (status, _) = poll_query(&hub, 100, "session=s-one&prompt_reports=1&caps=&turn=m4");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("m4"), &[])])
    );

    // The main turn id is decoded once, like an `agents` side.
    let (status, _) = poll_turns(&hub, 100, "s-one", "t%3A1", "");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("t:1"), &[])])
    );

    // A main turn that does not decode reads as no open main turn while the
    // poll's `agents` still replace the list: an unreadable id never refuses
    // the poll's repair.
    let (status, _) = poll_turns(&hub, 100, "s-one", "%ZZ", "a6:t6");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a6", "t6")])])
    );

    // A repeated agent id replaces its turn where it first listed, listed once.
    let (status, _) = poll_turns(&hub, 100, "s-one", "", "a1:t1,a2:t2,a1:t3");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a1", "t3"), ("a2", "t2")])])
    );

    // A poll reporting no open turns at all (the mod's idle poll between
    // turns) still replaces both: the fields clear rather than keeping the
    // last report.
    let (status, _) = poll_turns(&hub, 100, "s-one", "", "");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[])])
    );

    let none = frames(&sub);
    assert!(none.is_empty(), "a poll reported a frame: {none:?}");
}

/// A poll of a mod that does not report its open turns (no `turn` key, as a
/// mod released before them polls) changes neither field, and a stale poll
/// that does report them changes neither either: only the current load's poll
/// replaces them.
#[test]
fn only_a_current_loads_poll_replaces_the_open_turns() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    event(
        &hub,
        100,
        &json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true, "prompt_reports": true, "load": 2}),
    );
    main_turn(&hub, 100, "s-one", 2, "turn_start", "m1");
    agent(&hub, 100, "s-one", 3, ("a1", "start", "ta"));

    // No `turn` key: a mod from before the poll's open-turn fields.
    let (status, _) = poll_query(
        &hub,
        100,
        "session=s-one&prompt_reports=1&caps=notes&load=2",
    );
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([{"session": "s-one", "cwd": "/w/app", "interactive": true, "caps": ["notes"], "turn": "m1", "agents": [{"agent_id": "a1", "turn": "ta"}]}])
    );

    // A poll of an older load reports other turns: stale, so nothing changes.
    let (status, _) = poll_query(
        &hub,
        100,
        "session=s-one&prompt_reports=1&caps=notes&load=1&turn=&agents=",
    );
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([{"session": "s-one", "cwd": "/w/app", "interactive": true, "caps": ["notes"], "turn": "m1", "agents": [{"agent_id": "a1", "turn": "ta"}]}])
    );

    // The current load's report replaces both.
    let (status, _) = poll_query(
        &hub,
        100,
        "session=s-one&prompt_reports=1&caps=notes&load=2&turn=m2&agents=a2:tb",
    );
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([{"session": "s-one", "cwd": "/w/app", "interactive": true, "caps": ["notes"], "turn": "m2", "agents": [{"agent_id": "a2", "turn": "tb"}]}])
    );
}

/// A silence close is not the session's end: with the main turn and a subagent
/// turn open across it, the mod's next poll reports them and the next snapshot
/// lists both again.
#[test]
fn a_silence_close_mid_turn_keeps_the_open_turns_on_the_next_poll() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(200, instance("s-silent", "1"))]);
    start(&hub, 200, "s-silent", 1);
    open_turns(&hub, 200, "s-silent", 2);
    hub.advance_clock(SILENCE_LIMIT + Duration::from_secs(1));
    hub.sweep();
    assert_eq!(hub.snapshot()["sessions"], json!([]));

    let (status, _) = poll_turns(&hub, 200, "s-silent", "m1", "a1:ta");
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([
            {"session": "s-silent", "cwd": null, "interactive": null, "caps": [], "turn": "m1", "agents": [{"agent_id": "a1", "turn": "ta"}]},
        ])
    );
}

/// The open subagent list is bounded at sixty-four from the event path too: a
/// sixty-fifth start lists nothing while its own frame still relays, and an
/// end frees the slot. A poll's list is held to the same bound and its
/// malformed entries are skipped, never a refused poll.
#[test]
fn a_sixty_fifth_open_subagent_stays_out_of_the_snapshot() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    for i in 0..65u64 {
        event(
            &hub,
            100,
            &json!({"session": "s-one", "seq": i + 2, "kind": "agent_turn", "agent_id": format!("a{i}"), "state": "start", "turn": format!("t{i}")}),
        );
    }
    let listed = hub.snapshot()["sessions"][0]["agents"].clone();
    let listed = listed.as_array().expect("agents").clone();
    assert_eq!(listed.len(), 64);
    assert_eq!(listed[0], json!({"agent_id": "a0", "turn": "t0"}));
    assert_eq!(listed[63], json!({"agent_id": "a63", "turn": "t63"}));

    // A start past the bound lists nothing, yet its own frame still relays.
    let sub = hub.subscribe();
    agent(&hub, 100, "s-one", 67, ("a65", "start", "t65"));
    assert_eq!(
        frames(&sub),
        vec![agent_frame("s-one", "a65", "start", "t65")]
    );
    assert_eq!(
        hub.snapshot()["sessions"][0]["agents"]
            .as_array()
            .expect("agents")
            .len(),
        64
    );

    // At the bound a listed id's re-start still replaces its turn in place.
    agent(&hub, 100, "s-one", 68, ("a0", "start", "t0b"));
    let listed = hub.snapshot()["sessions"][0]["agents"].clone();
    let listed = listed.as_array().expect("agents").clone();
    assert_eq!(listed.len(), 64);
    assert_eq!(listed[0], json!({"agent_id": "a0", "turn": "t0b"}));

    // An end frees a slot the next start fills.
    agent(&hub, 100, "s-one", 69, ("a0", "end", "t0b"));
    agent(&hub, 100, "s-one", 70, ("a64", "start", "t64"));
    let listed = hub.snapshot()["sessions"][0]["agents"].clone();
    let listed = listed.as_array().expect("agents").clone();
    assert_eq!(listed.len(), 64);
    assert_eq!(listed[0], json!({"agent_id": "a1", "turn": "t1"}));
    assert_eq!(listed[63], json!({"agent_id": "a64", "turn": "t64"}));

    // A poll of more than the bound keeps its first sixty-four entries; a
    // malformed entry (no `:`, an empty side, a bad percent escape, or bytes
    // that are not UTF-8) is skipped, and the poll still answers 200.
    let agents: Vec<String> = (0..65).map(|i| format!("a{i}:t{i}")).collect();
    let (status, _) = poll_turns(&hub, 100, "s-one", "", &agents.join(","));
    assert_eq!(status, 200);
    let listed = hub.snapshot()["sessions"][0]["agents"].clone();
    let listed = listed.as_array().expect("agents").clone();
    assert_eq!(listed.len(), 64);
    assert_eq!(listed[0], json!({"agent_id": "a0", "turn": "t0"}));
    assert_eq!(listed[63], json!({"agent_id": "a63", "turn": "t63"}));

    // At the poll's bound a repeat of a listed id replaces its turn in place,
    // consuming no slot.
    let mut repeated = agents[..64].to_vec();
    repeated.push("a0:t0c".to_owned());
    let (status, _) = poll_turns(&hub, 100, "s-one", "", &repeated.join(","));
    assert_eq!(status, 200);
    let listed = hub.snapshot()["sessions"][0]["agents"].clone();
    let listed = listed.as_array().expect("agents").clone();
    assert_eq!(listed.len(), 64);
    assert_eq!(listed[0], json!({"agent_id": "a0", "turn": "t0c"}));

    let (status, _) = poll_turns(
        &hub,
        100,
        "s-one",
        "",
        "a1:ta,bad,:,a2:,a3:%ZZ,%FF:x,a4:ta,a5:%2C",
    );
    assert_eq!(status, 200);
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry(
            "s-one",
            None,
            &[("a1", "ta"), ("a4", "ta"), ("a5", ",")]
        )])
    );
}

/// The whole request head at the sixty-four-subagent cap, with the longest ids
/// the engine mints on Claude Code 2.1.330 (17-character agent ids,
/// 36-character turn ids), fits far under the bridge's 16 KiB head bound, and
/// the daemon reads every entry of it.
#[test]
fn a_full_poll_query_stays_under_the_head_cap() {
    const SESSION: &str = "99f41e9b-89e4-446f-a494-e8a99501635e";
    const TURN: &str = "f4b0ae65-076f-456a-b50b-d9cc36aa0e6f";
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance(SESSION, "1"))]);
    event(
        &hub,
        100,
        &json!({"session": SESSION, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true, "prompt_reports": true, "load": 12345}),
    );
    let agents: Vec<String> = (0..64).map(|i| format!("a{i:016x}:{TURN}")).collect();
    let query = format!(
        "session={SESSION}&prompt_reports=1&caps=notes,answer_receipts&load=12345&turn={TURN}&agents={}",
        agents.join(",")
    );
    let head = format!("GET /poll?{query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    println!("poll head at the 64-agent cap: {} bytes", head.len());
    assert!(
        head.len() < MAX_HEAD_BYTES,
        "the poll head is {} bytes, the bound {}",
        head.len(),
        MAX_HEAD_BYTES
    );

    let (status, _) = poll_query(&hub, 100, &query);
    assert_eq!(status, 200);
    let listed = hub.snapshot()["sessions"][0]["agents"].clone();
    let listed = listed.as_array().expect("agents").clone();
    assert_eq!(listed.len(), 64);
    assert_eq!(
        listed[0],
        json!({"agent_id": "a0000000000000000", "turn": TURN})
    );
    assert_eq!(
        listed[63],
        json!({"agent_id": "a000000000000003f", "turn": TURN})
    );
}

/// A mod that posts no subagent turn events (one released before them) lists
/// no subagent, whatever its subagents' transcript wakes and asks carry, and
/// its `turn` follows the main loop's turn events alone.
#[test]
fn a_mod_without_subagent_events_lists_no_agents() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    main_turn(&hub, 100, "s-one", 2, "turn_start", "m1");
    event(
        &hub,
        100,
        &json!({"session": "s-one", "seq": 3, "kind": "append", "uuid": "u1", "agent_id": "a1"}),
    );
    event(
        &hub,
        100,
        &json!({"session": "s-one", "seq": 4, "kind": "ask_opened", "ask": "k1", "tool": "Bash", "input": {"command": "ls"}, "agent_id": "a1"}),
    );
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", Some("m1"), &[])])
    );

    main_turn(&hub, 100, "s-one", 5, "turn_end", "m1");
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[])])
    );
}

/// A subagent turn event the daemon cannot read is refused before its
/// sequence advances, so the next event under that seq still lands.
#[test]
fn a_malformed_agent_turn_is_refused_before_its_seq() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1);
    let sub = hub.subscribe();
    for body in [
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "state": "start", "turn": "t1"}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": "", "state": "start", "turn": "t1"}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": 7, "state": "start", "turn": "t1"}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": "a1", "state": "running", "turn": "t1"}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": "a1", "turn": "t1"}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": "a1", "state": "start"}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": "a1", "state": "start", "turn": null}),
        json!({"session": "s-one", "seq": 2, "kind": "agent_turn", "agent_id": "a1", "state": "start", "turn": ""}),
    ] {
        assert_eq!(
            hub.event(100, body.to_string().as_bytes()),
            (400, json!({"error": "bad_request"})),
            "{body}"
        );
    }
    agent(&hub, 100, "s-one", 2, ("a1", "start", "t1"));

    assert_eq!(
        frames(&sub),
        vec![agent_frame("s-one", "a1", "start", "t1")]
    );
    assert_eq!(
        hub.snapshot()["sessions"],
        json!([entry("s-one", None, &[("a1", "t1")])])
    );
}
