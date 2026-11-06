#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The bridge's live half: the frames a hub hands its subscribers (session
//! state, transcript records read from the transcript file a wake points at,
//! asks opened and closed) and the snapshot a (re)connecting stream opens with.
//!
//! Transcripts live in the global store under a [`HomeSandbox`]; byte offsets
//! in the expectations are computed here from the lines the test wrote, never
//! read back from the code under test.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::api::devices::Tier;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, seed_device};

const WAIT: Duration = Duration::from_secs(5);
const NONE: Duration = Duration::from_millis(300);

fn instance(session_id: &str, proc_start: &str) -> Instance {
    Instance {
        session_id: session_id.to_string(),
        proc_start: proc_start.to_string(),
    }
}

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

fn table(rows: &[(u32, Instance)]) -> (Table, PeerResolver) {
    let table: Table = Arc::new(Mutex::new(rows.iter().cloned().collect()));
    let read = Arc::clone(&table);
    let resolver: PeerResolver = Box::new(move |pid| read.lock().unwrap().get(&pid).cloned());
    (table, resolver)
}

fn event(hub: &Hub, pid: u32, body: serde_json::Value) {
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "{body}: {answer}");
}

fn start(hub: &Hub, pid: u32, session: &str) {
    event(
        hub,
        pid,
        serde_json::json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true}),
    );
}

/// The next frame a subscription yields within `within`, as (event, data).
fn next(sub: &Subscription, within: Duration) -> Option<(String, serde_json::Value)> {
    sub.recv_timeout(within)
}

fn transcript_path(sb: &HomeSandbox, session: &str) -> PathBuf {
    sb.home()
        .join(format!(".claude/projects/-w-app/{session}.jsonl"))
}

fn append_raw(path: &PathBuf, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

fn row(uuid: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": text}})
}

fn line(value: &serde_json::Value) -> Vec<u8> {
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// Start and turn events reach every subscriber as `session.status`.
#[test]
fn session_state_reaches_subscribers() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let sub = hub.subscribe();
    start(&hub, 100, "s-one");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "turn_start", "turn": "t1"}),
    );
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "turn_end", "turn": "t1"}),
    );

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "start", "cwd": "/w/app", "interactive": true})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "turn_start", "turn": "t1"})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "turn_end", "turn": "t1"})
        ))
    );
}

/// A wake relays the complete lines written since the instance registered,
/// each as the history route's `{offset, record}`: rows already in the file
/// at registration stay the history route's, a line with no `uuid` is never
/// relayed, and a torn last line waits until it is complete.
#[test]
fn a_wake_relays_new_complete_rows_from_the_transcript() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    let before = line(&row("u0", "already there"));
    append_raw(&path, &before);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let first = line(&row("u1", "hello"));
    let internal = line(&serde_json::json!({"type": "api-request", "bytes": 12}));
    let second = line(&row("u2", "world"));
    let torn = line(&row("u3", "late"));
    let (torn_head, torn_tail) = torn.split_at(10);
    append_raw(&path, &first);
    append_raw(&path, &internal);
    append_raw(&path, &second);
    append_raw(&path, torn_head);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "u2"}),
    );

    let off1 = before.len() as u64;
    let off2 = off1 + first.len() as u64 + internal.len() as u64;
    let off3 = off2 + second.len() as u64;
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": off1, "record": row("u1", "hello")})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": off2, "record": row("u2", "world")})
        ))
    );
    assert_eq!(next(&sub, NONE), None, "the torn line waits");

    append_raw(&path, torn_tail);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "append", "uuid": "u3"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": off3, "record": row("u3", "late")})
        ))
    );
}

/// The mod posts no wake for some rows, so a session can end on one no wake
/// reached. The end relays it, before the `end` status closes the stream's
/// view of the session.
#[test]
fn an_end_relays_the_rows_no_wake_reached() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    let before = line(&row("u0", "already there"));
    append_raw(&path, &before);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    append_raw(&path, &line(&row("u1", "compacted")));
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "end"}),
    );

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": before.len() as u64, "record": row("u1", "compacted")})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(next(&sub, NONE), None);
}

/// The end's final read covers every file the instance already holds a tail
/// for: the main transcript and each subagent file a wake has opened. A
/// subagent file no wake ever named stays out, its rows the history route's,
/// and every relayed record lands before the `end` frame.
#[test]
fn an_end_relays_the_rows_of_every_held_file() {
    let sb = HomeSandbox::new();
    let main = transcript_path(&sb, "s-one");
    append_raw(&main, b"");
    let woken = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-a1.jsonl");
    append_raw(&woken, b"");
    let quiet = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-a2.jsonl");
    append_raw(&quiet, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    // One wake opens a tail for a1; no wake ever names a2.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "x", "agent_id": "a1"}),
    );
    assert_eq!(next(&sub, NONE), None);

    append_raw(&main, &line(&row("u1", "main row")));
    append_raw(&woken, &line(&row("a1", "sub row")));
    append_raw(&quiet, &line(&row("a2", "quiet row")));
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "end"}),
    );

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "record": row("u1", "main row")})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": "a1", "offset": 0, "record": row("a1", "sub row")})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(
        next(&sub, NONE),
        None,
        "a file the hub never tailed relayed its rows"
    );
}

/// A stale `end` (a replayed sequence) relays nothing and answers 409: the
/// final tail must not run for an end the sequence judge refuses, and the
/// instance survives for the next wake.
#[test]
fn a_stale_end_relays_nothing_and_the_instance_survives() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    append_raw(&path, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();
    let first = line(&row("u1", "before the stale end"));
    append_raw(&path, &first);

    let (status, _) = hub.event(
        100,
        serde_json::json!({"session": "s-one", "seq": 1, "kind": "end"})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(status, 409, "the stale end was not refused");
    assert_eq!(next(&sub, NONE), None, "a stale end relayed or emitted");

    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "u1"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "record": row("u1", "before the stale end")})
        ))
    );
}

/// An end arriving while a tail is already running waits it out and relays
/// the rows the finished tail left behind: the removal never drops an
/// in-flight read's rows, and the record still lands before the `end` frame.
#[test]
fn an_end_waiting_out_a_tail_relays_the_rows_after_it() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    append_raw(&path, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let first = line(&row("u1", "during the tail"));
    append_raw(&path, &first);
    let generation = hold_tail(&hub, 100);
    let ended = post_end(&hub, 2);
    // The end parks waiting for the slot; the finishing tail ends it. The
    // bound below turns a never-woken end into a red, never a hang.
    wait_for_ending(&hub, 100);
    hub.finish_tail(100, generation);
    assert_eq!(
        ended.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true }))),
        "the end never answered"
    );

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "record": row("u1", "during the tail")})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(next(&sub, NONE), None);
}

/// Hold the tail slot as `relay_main` does while it reads, so an end parks in
/// its final read until `finish_tail` frees it.
fn hold_tail(hub: &Hub, pid: u32) -> u64 {
    let mut inner = hub.inner.lock().unwrap();
    let live = inner.instances.get_mut(&pid).unwrap();
    live.tailing = true;
    live.generation
}

/// Post an `end` from its own thread: it answers only once its final read ends.
fn post_end(hub: &Arc<Hub>, seq: u64) -> std::sync::mpsc::Receiver<(u16, serde_json::Value)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let hub = Arc::clone(hub);
    std::thread::spawn(move || {
        let body = serde_json::json!({"session": "s-one", "seq": seq, "kind": "end"});
        let _ = tx.send(hub.event(100, body.to_string().as_bytes()));
    });
    rx
}

/// Wait until an `end` owns the instance's final read.
fn wait_for_ending(hub: &Hub, pid: u32) {
    let deadline = std::time::Instant::now() + WAIT;
    while !hub
        .inner
        .lock()
        .unwrap()
        .instances
        .get(&pid)
        .is_some_and(|live| live.ending)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the end never reached its final read"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The mod stops polling before it posts its end, so a command queued during
/// the final read would be acknowledged and never run: the session reads as
/// gone, and a prompt falls back to herdr.
#[test]
fn a_command_during_the_final_read_finds_no_live_session() {
    let sb = HomeSandbox::new();
    append_raw(&transcript_path(&sb, "s-one"), b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    seed_device(DEVICE, Tier::Control, TOKEN);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );

    let generation = hold_tail(&hub, 100);
    let ended = post_end(&hub, 3);
    wait_for_ending(&hub, 100);

    assert!(matches!(
        hub.deliver("s-one", serde_json::json!({"kind": "prompt", "text": "hi"})),
        ModDelivery::NoMod
    ));
    assert!(matches!(
        hub.answer(
            "s-one",
            "a1",
            serde_json::json!({"kind": "answer", "ask": "a1", "decision": "deny"})
        ),
        AskOutcome::NoSession
    ));

    hub.finish_tail(100, generation);
    assert_eq!(
        ended.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true })))
    );
}

/// One end owns the final read, the removal and the end frames: a second end
/// answers without waiting on the read, and the session ends once.
#[test]
fn a_second_end_during_the_final_read_answers_at_once() {
    let sb = HomeSandbox::new();
    append_raw(&transcript_path(&sb, "s-one"), b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let generation = hold_tail(&hub, 100);
    let first = post_end(&hub, 2);
    wait_for_ending(&hub, 100);
    let second = post_end(&hub, 3);
    assert_eq!(
        second.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true }))),
        "the second end waited on the first end's read"
    );

    hub.finish_tail(100, generation);
    assert_eq!(
        first.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true })))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(next(&sub, NONE), None);
}

/// A session at its final read, a tail in the slot and a re-run queued behind
/// that tail, with an end parked on the slot.
fn ending_with_a_queued_re_run(
    hub: &Arc<Hub>,
) -> (u64, std::sync::mpsc::Receiver<(u16, serde_json::Value)>) {
    start(hub, 100, "s-one");
    let generation = hold_tail(hub, 100);
    let ended = post_end(hub, 2);
    wait_for_ending(hub, 100);
    hub.inner
        .lock()
        .unwrap()
        .instances
        .get_mut(&100)
        .unwrap()
        .rewake
        .insert(None);
    (generation, ended)
}

/// A re-run a finishing tail took could start after the end removed the
/// instance, and its rows would be gone: the end keeps the re-runs of an
/// ending instance for itself.
#[test]
fn a_tail_finishing_during_the_final_read_leaves_its_re_runs_to_the_end() {
    let sb = HomeSandbox::new();
    append_raw(&transcript_path(&sb, "s-one"), b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (generation, ended) = ending_with_a_queued_re_run(&hub);

    assert_eq!(hub.finish_tail(100, generation), HashSet::new());
    assert_eq!(
        ended.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true })))
    );
}

/// The same for the path a real tail ends on: its write-back frees the slot,
/// wakes the parked end and leaves the re-runs to it.
#[test]
fn a_tail_writing_back_during_the_final_read_leaves_its_re_runs_to_the_end() {
    let sb = HomeSandbox::new();
    append_raw(&transcript_path(&sb, "s-one"), b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (generation, ended) = ending_with_a_queued_re_run(&hub);

    assert_eq!(
        hub.write_back_tail(100, generation, Vec::new(), |_| {}),
        HashSet::new()
    );
    assert_eq!(
        ended.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true })))
    );
}

/// An end parked on the slot belongs to the instance it was posted for: a new
/// session at the same pid survives it, and the old session ends once.
#[test]
fn an_end_leaves_the_session_that_replaced_its_instance() {
    let sb = HomeSandbox::new();
    append_raw(&transcript_path(&sb, "s-one"), b"");
    let (t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();
    hold_tail(&hub, 100);
    let ended = post_end(&hub, 2);
    wait_for_ending(&hub, 100);

    t.lock().unwrap().insert(100, instance("s-one", "2"));
    start(&hub, 100, "s-one");

    assert_eq!(
        ended.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true })))
    );
    assert!(hub.inner.lock().unwrap().instances.contains_key(&100));
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(
        next(&sub, WAIT).map(|(event, _)| event),
        Some("session.status".into())
    );
    assert_eq!(next(&sub, NONE), None);
}

/// A wake that bounced off the slot names a file the end's own queue does not
/// hold: the end relays it from `rewake`, before the `end` frame.
#[test]
fn an_end_relays_a_re_run_left_to_it() {
    let sb = HomeSandbox::new();
    append_raw(&transcript_path(&sb, "s-one"), b"");
    let agent = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-a2.jsonl");
    append_raw(&agent, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();
    append_raw(&agent, &line(&row("a2", "first seen")));

    let generation = hold_tail(&hub, 100);
    let ended = post_end(&hub, 2);
    wait_for_ending(&hub, 100);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "append", "uuid": "y", "agent_id": "a2"}),
    );
    hub.finish_tail(100, generation);
    assert_eq!(
        ended.recv_timeout(WAIT),
        Ok((200, serde_json::json!({ "ok": true })))
    );

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": "a2", "offset": 0, "record": row("a2", "first seen")})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(next(&sub, NONE), None);
}

/// A record past 1 MiB relays as a stub naming its offset and size; the
/// client reads it through the history route.
#[test]
fn a_record_past_the_bound_relays_as_a_stub() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    append_raw(&path, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let big = line(&row("u1", &"x".repeat(1024 * 1024 + 1)));
    append_raw(&path, &big);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "u1"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "size": big.len() - 1, "truncated": true})
        ))
    );
}

/// A subagent row is read from the subagent file the daemon finds itself; a
/// wake naming an id no listed file carries relays nothing.
#[test]
fn a_subagent_wake_reads_only_a_listed_stem() {
    let sb = HomeSandbox::new();
    let main = transcript_path(&sb, "s-one");
    append_raw(&main, b"");
    let agent_file = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-a1b2.jsonl");
    append_raw(&agent_file, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    // the first wake for a subagent file starts its offset at the file's
    // length, like registration does for the main file; the row after it relays.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "x0", "agent_id": "a1b2"}),
    );
    assert_eq!(next(&sub, NONE), None);
    let sub_row = line(&row("s1", "from the subagent"));
    append_raw(&agent_file, &sub_row);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "append", "uuid": "s1", "agent_id": "a1b2"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": "a1b2", "offset": 0, "record": row("s1", "from the subagent")})
        ))
    );

    // a row waits in the real subagent file: a wake whose id resolves to that
    // file only by path arithmetic must still relay nothing
    let waiting = line(&row("s2", "waiting"));
    append_raw(&agent_file, &waiting);
    // A fixture dir `agent-x/` makes `x/../agent-a1b2` resolve by a path join
    // (`agent-x/../agent-a1b2.jsonl` lands on the real file); a listing still
    // matches no stem, so the forged id relays nothing (SB-11).
    let fixture_dir = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-x");
    std::fs::create_dir_all(&fixture_dir).unwrap();
    for (seq, forged) in (4u64..).zip([
        "../../../../etc/passwd",
        "zz",
        "a1b2/../a1b2",
        "./a1b2",
        "x/../agent-a1b2",
    ]) {
        event(
            &hub,
            100,
            serde_json::json!({"session": "s-one", "seq": seq, "kind": "append", "uuid": "f", "agent_id": forged}),
        );
    }
    assert_eq!(next(&sub, NONE), None, "a forged agent id relays nothing");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 9, "kind": "append", "uuid": "s2", "agent_id": "a1b2"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": "a1b2", "offset": sub_row.len(), "record": row("s2", "waiting")})
        ))
    );
}

/// The mod is the one that knows why its hold ended: an ask its own interrupt
/// aborted closes `by: interrupt`. The daemon relays only the closers a mod can
/// name (`desk`, `interrupt`); `device` and `session_end` are the daemon's own
/// stamps, so a mod naming one, any other value, or none, reads as `desk`.
#[test]
fn a_mod_close_names_the_interrupt_and_nothing_the_daemon_owns() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let cases: [(&str, Option<&str>, &str); 6] = [
        ("a1", Some("interrupt"), "interrupt"),
        ("a2", Some("desk"), "desk"),
        ("a3", None, "desk"),
        ("a4", Some("device"), "desk"),
        ("a5", Some("session_end"), "desk"),
        ("a6", Some("phone"), "desk"),
    ];
    let mut seq = 2;
    for (ask, sent, relayed) in cases {
        event(
            &hub,
            100,
            serde_json::json!({"session": "s-one", "seq": seq, "kind": "ask_opened", "ask": ask, "tool": "Bash", "input": {}}),
        );
        assert_eq!(
            next(&sub, WAIT).map(|(name, _)| name),
            Some("session.ask".into())
        );
        let mut closed = serde_json::json!({"session": "s-one", "seq": seq + 1, "kind": "ask_closed", "ask": ask});
        if let Some(by) = sent {
            closed["by"] = serde_json::json!(by);
        }
        event(&hub, 100, closed);
        assert_eq!(
            next(&sub, WAIT),
            Some((
                "session.ask".into(),
                serde_json::json!({"session": "s-one", "ask": ask, "state": "closed", "by": relayed})
            )),
            "a mod close sent with by {sent:?}"
        );
        seq += 2;
    }
}

/// An ask reaches subscribers when opened and when closed, with who closed
/// it: the desk through the mod's event, a device through the answer route.
#[test]
fn asks_reach_subscribers_opened_and_closed() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {"command": "ls"}}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.ask".into(),
            serde_json::json!({"session": "s-one", "ask": "a1", "state": "opened", "tool": "Bash", "input": {"command": "ls"}, "agent_id": null})
        ))
    );
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_closed", "ask": "a1", "by": "desk"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.ask".into(),
            serde_json::json!({"session": "s-one", "ask": "a1", "state": "closed", "by": "desk"})
        ))
    );

    // A control device is paired before `a2` opens, so `a2` is held and its
    // answer is accepted; an unheld ask is never answerable (SB-19).
    seed_device(DEVICE, Tier::Control, TOKEN);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 4, "kind": "ask_opened", "ask": "a2", "tool": "Bash", "input": {}}),
    );
    next(&sub, WAIT).expect("opened a2");
    assert!(matches!(
        hub.answer(
            "s-one",
            "a2",
            serde_json::json!({"kind": "answer", "ask": "a2", "decision": "deny"})
        ),
        AskOutcome::Queued
    ));
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.ask".into(),
            serde_json::json!({"session": "s-one", "ask": "a2", "state": "closed", "by": "device"})
        ))
    );
    // the mod's own close of an ask a device already answered says nothing new
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 5, "kind": "ask_closed", "ask": "a2", "by": "device"}),
    );
    assert_eq!(next(&sub, NONE), None);
}

/// The snapshot a stream opens with: every live session and every open ask.
#[test]
fn the_snapshot_lists_live_sessions_and_open_asks() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-two");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {"command": "ls"}}),
    );
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "a2", "tool": "Read", "input": {}}),
    );
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 4, "kind": "ask_closed", "ask": "a2", "by": "desk"}),
    );

    assert_eq!(
        hub.snapshot(),
        serde_json::json!({
            "sessions": [
                {"session": "s-one", "cwd": "/w/app", "interactive": true},
                {"session": "s-two", "cwd": "/w/app", "interactive": true}
            ],
            "asks": [
                {"session": "s-one", "ask": "a1", "tool": "Bash", "input": {"command": "ls"}, "agent_id": null}
            ]
        })
    );
}

/// A `/clear` (the record's id moved) closes the old session for subscribers:
/// its open asks close, then its end, and the snapshot drops it.
#[test]
fn a_cleared_session_ends_for_subscribers() {
    let _home = HomeSandbox::new();
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    let sub = hub.subscribe();
    table.lock().unwrap().insert(100, instance("s-three", "1"));
    let _ = hub.snapshot();

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.ask".into(),
            serde_json::json!({"session": "s-one", "ask": "a1", "state": "closed", "by": "session_end"})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
    assert_eq!(
        hub.snapshot(),
        serde_json::json!({"sessions": [], "asks": []})
    );
}

/// A slow `/events` subscriber never blocks the hub: its bounded channel fills,
/// the subscription is dropped, and the hub keeps broadcasting (SB-10).
#[test]
fn a_slow_subscriber_is_dropped_without_blocking() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let sub = hub.subscribe();
    start(&hub, 100, "s-one");
    // One start frame plus SUBSCRIBER_BOUND turn frames fill the channel; the
    // next broadcast drops the subscription. Every event returns, never blocks.
    for seq in 2..=(SUBSCRIBER_BOUND as u64 + 1) {
        event(
            &hub,
            100,
            serde_json::json!({"session": "s-one", "seq": seq, "kind": "turn_start", "turn": "t"}),
        );
    }
    let mut drained = 0;
    while sub.recv_timeout(WAIT).is_some() {
        drained += 1;
    }
    assert_eq!(drained, SUBSCRIBER_BOUND);
    assert!(matches!(sub.try_next(), Err(Dropped)));
}

/// An instance silent past the bound closes for subscribers: its open asks
/// close `by: session_end`, then its `end`, driven by the sweep on the hub's
/// test clock (SB-13).
#[test]
fn a_silent_instance_ends_for_subscribers() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    let sub = hub.subscribe();
    hub.advance_clock(SILENCE_LIMIT + Duration::from_secs(1));
    hub.sweep();

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.ask".into(),
            serde_json::json!({"session": "s-one", "ask": "a1", "state": "closed", "by": "session_end"})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
}

/// A mod `end` event closes the session for subscribers: open asks close
/// `by: session_end`, then the `end` status (SB-13).
#[test]
fn a_mod_end_closes_asks_and_ends() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    let sub = hub.subscribe();
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "end"}),
    );

    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.ask".into(),
            serde_json::json!({"session": "s-one", "ask": "a1", "state": "closed", "by": "session_end"})
        ))
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.status".into(),
            serde_json::json!({"session": "s-one", "state": "end"})
        ))
    );
}

/// An `end` from a pid whose instance this event just minted emits nothing: no
/// `start` was ever broadcast for it, so no `end` frame may appear either.
#[test]
fn an_end_from_an_unknown_pid_emits_nothing() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let sub = hub.subscribe();
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 1, "kind": "end"}),
    );
    assert_eq!(next(&sub, NONE), None);
}

/// A subagent file created after the instance registered starts at 0: the row
/// that woke the daemon relays live, unlike a file present at registration
/// whose first rows stay the history route's (SB-11).
#[test]
fn a_subagent_created_after_registration_relays_from_zero() {
    let sb = HomeSandbox::new();
    let main = transcript_path(&sb, "s-one");
    append_raw(&main, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let agent_file = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-b2.jsonl");
    let first = line(&row("b1", "created later"));
    append_raw(&agent_file, &first);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "b1", "agent_id": "b2"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": "b2", "offset": 0, "record": row("b1", "created later")})
        ))
    );
}

/// A mod ask opening a new wait fires one `blocked` push for the session's
/// account (WP-8): a second ask in the same wait does not, and a later wait —
/// even back to back — pushes again. The mod is the only push source for its
/// session, so no pane probe gates it.
#[test]
fn a_mod_ask_pushes_blocked_once_per_wait() {
    let _home = HomeSandbox::new();
    // The row the hub joins by pid (100) names the account the push carries.
    let mut row = crate::testutil::live_row("100-0", "alpha");
    row.pid = 100;
    crate::live_sessions::register(&row).expect("register the session's row");

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (queue, rx) = crate::push::queue_for_tests();
    hub.wire_push(queue);
    start(&hub, 100, "s-one");

    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        })
    );

    // A second ask in the same wait does not push again.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "a2", "tool": "Bash", "input": {}}),
    );
    assert!(
        rx.try_recv().is_err(),
        "a second ask in the same wait pushed again"
    );

    // Close both asks, so the wait ends and the next one announces.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 4, "kind": "ask_closed", "ask": "a1", "by": "desk"}),
    );
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 5, "kind": "ask_closed", "ask": "a2", "by": "desk"}),
    );

    // The latch cleared: the next wait announces again.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 6, "kind": "ask_opened", "ask": "a3", "tool": "Bash", "input": {}}),
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        }),
        "the next wait after the last ask closed did not announce"
    );
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 7, "kind": "ask_closed", "ask": "a3", "by": "desk"}),
    );

    // A back-to-back wait (PROBE-L's shape) announces too: the wait closed and
    // a new one opened at once, so it is a new wait, not the old one.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 8, "kind": "ask_opened", "ask": "a4", "tool": "Bash", "input": {}}),
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        }),
        "a back-to-back wait did not announce"
    );
}

/// A mod ask opening before the push queue is wired does not spend its wait:
/// a second ask in the same wait, after `wire_push`, announces — exactly once
/// (WP-8, PROBE-N's shape).
#[test]
fn an_ask_opened_before_the_push_queue_is_wired_still_announces() {
    let _home = HomeSandbox::new();
    // The row the hub joins by pid (100) names the account the push carries.
    let mut row = crate::testutil::live_row("100-0", "alpha");
    row.pid = 100;
    crate::live_sessions::register(&row).expect("register the session's row");

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");

    // The first ask opens before the push queue exists, so no push is queued.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );

    let (queue, rx) = crate::push::queue_for_tests();
    hub.wire_push(queue);

    // A second ask in the same wait announces: the wait was never announced.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "a2", "tool": "Bash", "input": {}}),
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        }),
        "the wait opened before the queue was wired never announced"
    );
    // Exactly one push: the two asks in one wait do not double.
    assert!(
        rx.try_recv().is_err(),
        "the pre-wire wait pushed more than once"
    );
}

/// PROBE-O's shape (WP-8): the decision to announce and the reservation share
/// one lock hold, so a second ask of the same wait handled between the decision
/// and the queueing pushes nothing. The push lock is held to park the first
/// ask's `note_ask_opened` at its send (the queueing), then the second ask runs:
/// on the pre-fix bytes it re-decides and the wait pushes twice.
#[test]
fn a_second_ask_between_the_decision_and_the_queueing_pushes_nothing() {
    let _home = HomeSandbox::new();
    // The row the hub joins by pid (100) names the account the push carries.
    let mut row = crate::testutil::live_row("100-0", "alpha");
    row.pid = 100;
    crate::live_sessions::register(&row).expect("register the session's row");

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    let (queue, rx) = crate::push::queue_for_tests();
    hub.wire_push(queue);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    // Park the first ask's queueing: it reserves (or, pre-fix, only decides)
    // under the hub lock, then blocks here before its push is sent.
    let parked = hub.push.lock().unwrap_or_else(|p| p.into_inner());
    let first = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            event(
                &hub,
                100,
                serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
            )
        })
    };
    // The opened frame lands only after the arm ran, so the second ask reads
    // the post-decision state (`announced` reserved, or not yet, pre-fix).
    let frame = next(&sub, WAIT);
    assert!(
        frame
            .as_ref()
            .is_some_and(|(event, _)| event == "session.ask"),
        "the first ask never opened: {frame:?}"
    );
    let second = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            event(
                &hub,
                100,
                serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "a2", "tool": "Bash", "input": {}}),
            )
        })
    };
    // The second ask's opened frame is sent after its decision, so waiting
    // for it before releasing the first ask's queueing makes the race
    // deterministic on the split-latch code as well as the fixed one.
    let frame = next(&sub, WAIT);
    assert!(
        frame
            .as_ref()
            .is_some_and(|(event, data)| event == "session.ask" && data["ask"] == "a2"),
        "the second ask never opened: {frame:?}"
    );
    drop(parked);
    first.join().expect("the first ask panicked");
    second.join().expect("the second ask panicked");

    assert_eq!(
        rx.try_recv(),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        }),
        "the first ask never pushed"
    );
    assert!(
        rx.try_recv().is_err(),
        "a second ask of the same wait pushed again (the decision and the reservation are not one hold)"
    );
}

/// P22: a device answer closing the last ask ends the wait, so the next wait
/// pushes again (WP-8, PROBE-P's shape). Breaking the answer's clear
/// (`announced = false` on the last ask) must red this.
#[test]
fn a_device_answer_ends_the_wait_so_the_next_wait_pushes() {
    let _home = HomeSandbox::new();
    crate::testutil::seed_device(DEVICE, Tier::Control, TOKEN);
    // The row the hub joins by pid (100) names the account the push carries.
    let mut row = crate::testutil::live_row("100-0", "alpha");
    row.pid = 100;
    crate::live_sessions::register(&row).expect("register the session's row");

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (queue, rx) = crate::push::queue_for_tests();
    hub.wire_push(queue);
    start(&hub, 100, "s-one");

    // A control device is paired, so the ask opens held and is answerable.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        })
    );

    // The device answers the held ask, closing the wait's last ask.
    assert!(
        matches!(
            hub.answer(
                "s-one",
                "a1",
                serde_json::json!({"kind": "answer", "ask": "a1", "decision": "allow"})
            ),
            AskOutcome::Queued
        ),
        "the held ask was not answerable"
    );

    // The next wait announces again.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "a2", "tool": "Bash", "input": {}}),
    );
    assert_eq!(
        rx.recv_timeout(WAIT),
        Ok(crate::push::Trigger::SessionBlocked {
            account: Some("alpha".to_string())
        }),
        "the wait after a device answer never pushed"
    );
}

/// P23: the rollback's generation guard. A reservation on a replaced instance
/// must not roll the new instance's latch back. Driven without threads: the
/// queue is unwired, then `note_ask_opened` is called with the old generation
/// directly, standing in for the stale rollback. Breaking the generation check
/// (`|| true`) must red this.
#[test]
fn a_stale_rollback_with_a_moved_generation_keeps_the_new_wait_announced() {
    let _home = HomeSandbox::new();
    // The row the hub joins by pid (100) names the account the push carries.
    let mut row = crate::testutil::live_row("100-0", "alpha");
    row.pid = 100;
    crate::live_sessions::register(&row).expect("register the session's row");

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (queue, rx) = crate::push::queue_for_tests();
    hub.wire_push(queue.clone());
    start(&hub, 100, "s-one");

    let old_generation = hub
        .inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .instances
        .get(&100)
        .expect("the instance was minted by start")
        .generation;

    // The old instance's wait announces, then the instance is replaced.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    assert!(rx.recv_timeout(WAIT).is_ok(), "a1 never pushed");
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "end"}),
    );
    start(&hub, 100, "s-one");

    // The new instance's first wait announces.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "b1", "tool": "Bash", "input": {}}),
    );
    assert!(rx.recv_timeout(WAIT).is_ok(), "b1 never pushed");

    // Unwire the queue, then deliver the stale rollback from the old instance:
    // the generation guard must leave the new instance's latch alone.
    *hub.push.lock().unwrap_or_else(|p| p.into_inner()) = None;
    hub.note_ask_opened(100, old_generation);
    hub.wire_push(queue);

    // A second ask in the new instance's wait pushes nothing: the latch is
    // still reserved.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_opened", "ask": "b2", "tool": "Bash", "input": {}}),
    );
    assert!(
        rx.try_recv().is_err(),
        "the stale rollback cleared the new instance's latch and the wait pushed twice"
    );
}

/// P24: the rollback's open-wait guard. A rollback arriving after the wait it
/// reserved for has closed must not clear the latch — the close already reset
/// it, so re-clearing would clobber nothing the rollback owns. Driven without
/// threads: the reservation is re-armed after the close, then `note_ask_opened`
/// is called directly with the wait closed. Breaking the open-wait check
/// (`|| true`) must red this.
#[test]
fn a_stale_rollback_with_the_wait_already_closed_keeps_the_latch() {
    let _home = HomeSandbox::new();
    // The row the hub joins by pid (100) names the account the push carries.
    let mut row = crate::testutil::live_row("100-0", "alpha");
    row.pid = 100;
    crate::live_sessions::register(&row).expect("register the session's row");

    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (queue, rx) = crate::push::queue_for_tests();
    hub.wire_push(queue);
    start(&hub, 100, "s-one");

    let generation = hub
        .inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .instances
        .get(&100)
        .expect("the instance was minted by start")
        .generation;

    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "ask_opened", "ask": "a1", "tool": "Bash", "input": {}}),
    );
    assert!(rx.recv_timeout(WAIT).is_ok(), "a1 never pushed");

    // The desk closes the wait, then the reservation is re-armed: the stale
    // rollback arrives for a reservation whose wait has since closed.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_closed", "ask": "a1", "by": "desk"}),
    );
    *hub.push.lock().unwrap_or_else(|p| p.into_inner()) = None;
    hub.inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .instances
        .get_mut(&100)
        .expect("the instance survived the close")
        .announced = true;

    hub.note_ask_opened(100, generation);

    let announced = hub
        .inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .instances
        .get(&100)
        .expect("the instance survived the rollback")
        .announced;
    assert!(
        announced,
        "the stale rollback cleared a latch the closed wait already owned"
    );
}

/// A poll as an instance's first contact registers its transcript like an
/// event does: rows already in the file stay the history route's, and the
/// first wake relays only rows written after the poll (SB-11).
#[test]
fn a_poll_first_never_replays_history() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    let before = line(&row("u0", "already there"));
    append_raw(&path, &before);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let (status, _) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(status, 200);
    let sub = hub.subscribe();

    let first = line(&row("u1", "hello"));
    append_raw(&path, &first);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 1, "kind": "append", "uuid": "u1"}),
    );
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": before.len(), "record": row("u1", "hello")})
        ))
    );
    assert_eq!(next(&sub, NONE), None, "a pre-registration row replayed");
}

/// A first contact refused as stale (a `seq: 0` before any event advanced the
/// sequence) leaves no instance behind, so the next valid start registers
/// cleanly and its first wake relays the row (A#13, SB-11). Both the `start`
/// and the `end` path mint an instance before refusing, so both must remove it.
#[test]
fn a_first_contact_refused_as_stale_leaves_no_instance() {
    let sb = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (101, instance("s-two", "1"))]);
    let hub = Hub::new(true, resolver);

    // The refused contact must not have wedged the tail slot: the valid start
    // registers and the first append relays the row.
    let relays_after = |hub: &Hub, pid: u32, session: &str, path: &PathBuf, uuid: &str| {
        start(hub, pid, session);
        let sub = hub.subscribe();
        let first = line(&row(uuid, "hello"));
        append_raw(path, &first);
        event(
            hub,
            pid,
            serde_json::json!({"session": session, "seq": 2, "kind": "append", "uuid": uuid}),
        );
        assert_eq!(
            next(&sub, WAIT),
            Some((
                "session.record".into(),
                serde_json::json!({"session": session, "agent_id": null, "offset": 0, "record": row(uuid, "hello")})
            ))
        );
    };

    // The `start` path mints an instance before refusing.
    let path = transcript_path(&sb, "s-one");
    append_raw(&path, b"");
    let (status, _) = hub.event(
        100,
        serde_json::json!({"session": "s-one", "seq": 0, "kind": "start", "cwd": "/w/app", "interactive": true})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(status, 409, "the first stale start was not refused");
    relays_after(&hub, 100, "s-one", &path, "u1");

    // The `end` path mints an instance the same way before refusing.
    let path2 = transcript_path(&sb, "s-two");
    append_raw(&path2, b"");
    let (status, _) = hub.event(
        101,
        serde_json::json!({"session": "s-two", "seq": 0, "kind": "end"})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(status, 409, "the first stale end was not refused");
    relays_after(&hub, 101, "s-two", &path2, "u2");
}

/// A >1 MiB row torn across wakes, with `uuid` after the bulky `message`
/// content (Claude Code's own key order), still relays a stub naming its full
/// size (SB-10).
#[test]
fn a_torn_oversize_row_with_a_late_uuid_relays_a_stub() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    append_raw(&path, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let big = "x".repeat(1024 * 1024 + 10);
    let head = format!("{{\"parentUuid\":null,\"type\":\"user\",\"message\":{{\"content\":\"{big}");
    let tail = "\"},\"uuid\":\"u1\",\"timestamp\":\"t\"}\n";
    append_raw(&path, head.as_bytes());
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "u0"}),
    );
    append_raw(&path, tail.as_bytes());
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 3, "kind": "append", "uuid": "u1"}),
    );
    let size = (head.len() + tail.len() - 1) as u64;
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "size": size, "truncated": true})
        ))
    );
}

/// An append wake that races the registering call's locate (simulated: the
/// instance exists in its pre-locate state, the tail slot held and `main_file`
/// unwritten) queues a re-run instead of locating at offset 0, so it never
/// replays the pre-registration rows (SB-11).
#[test]
fn a_wake_during_registration_does_not_replay_history() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    let before = line(&row("u0", "already there"));
    append_raw(&path, &before);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    // Create the instance exactly as `event` does before its registration
    // locate runs: the tail slot is held from the start and `main_file` is not
    // yet written.
    {
        let mut inner = hub.inner.lock().unwrap();
        let now = hub.now();
        let resolved = instance("s-one", "1");
        let (_live, created) = hub.reconcile(&mut inner, 100, &resolved, now);
        assert!(created);
    }
    let sub = hub.subscribe();
    // The mod's next row lands while the locate is still walking the stores.
    let first = line(&row("u1", "hello"));
    append_raw(&path, &first);
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 1, "kind": "append", "uuid": "u1"}),
    );
    // The wake is deferred by the held tail slot: it must not locate at offset
    // 0 and replay the pre-registration row.
    assert_eq!(
        sub.try_next(),
        Ok(None),
        "a wake during registration replayed history"
    );
}

/// A row landing after the registration locate read the file's length, but
/// before the slot ends, relays once the slot ends: the re-run queued by the
/// racing wake reads the fresh row (SB-11).
#[test]
fn a_row_landing_after_the_length_read_relays_once_the_slot_ends() {
    let sb = HomeSandbox::new();
    let path = transcript_path(&sb, "s-one");
    append_raw(&path, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    let generation = {
        let mut inner = hub.inner.lock().unwrap();
        let now = hub.now();
        let resolved = instance("s-one", "1");
        let (_live, created) = hub.reconcile(&mut inner, 100, &resolved, now);
        assert!(created);
        inner.instances.get(&100).unwrap().generation
    };
    let sub = hub.subscribe();
    // The wake lands during the registration slot: deferred, never relayed.
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 1, "kind": "append", "uuid": "u1"}),
    );
    assert_eq!(
        sub.try_next(),
        Ok(None),
        "the wake relayed during registration"
    );
    // The locate reads the length now — the file is still empty.
    let (main, snapshot) = hub
        .registration_tail("s-one")
        .expect("the locate found the file");
    // A row lands after that read, before the slot ends.
    let first = line(&row("u1", "after the length read"));
    append_raw(&path, &first);
    // The slot ends: the queued wake re-runs and relays the row.
    hub.finish_registration(100, "s-one", generation, Some((main, snapshot)));
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "record": row("u1", "after the length read")})
        ))
    );
}

/// A main-file wake that lands during a subagent's tail relays through the
/// MAIN file, not the subagent's: the pending re-run is keyed per file, so the
/// finishing tail re-runs exactly the file the wake named (SB-11).
#[test]
fn a_main_wake_during_a_subagent_tail_relays_through_main() {
    let sb = HomeSandbox::new();
    let main = transcript_path(&sb, "s-one");
    append_raw(&main, b"");
    let agent = sb
        .home()
        .join(".claude/projects/-w-app/s-one/subagents/agent-a1.jsonl");
    append_raw(&agent, b"");
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let sub = hub.subscribe();

    let first = line(&row("m1", "main"));
    append_raw(&main, &first);
    let generation = {
        let mut inner = hub.inner.lock().unwrap();
        let live = inner.instances.get_mut(&100).unwrap();
        live.tailing = true; // a subagent tail for a1 is in flight
        live.generation
    };
    event(
        &hub,
        100,
        serde_json::json!({"session": "s-one", "seq": 2, "kind": "append", "uuid": "m1"}),
    );
    assert_eq!(
        sub.try_next(),
        Ok(None),
        "the main wake relayed during the subagent tail"
    );
    // The in-flight subagent tail finishes and drains the queued re-run.
    let rewakes = hub.write_back_tail(100, generation, Vec::new(), |_| {});
    assert!(
        rewakes.contains(&None),
        "the main wake queued the main file"
    );
    hub.run_rewakes(100, "s-one", rewakes);
    assert_eq!(
        next(&sub, WAIT),
        Some((
            "session.record".into(),
            serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "record": row("m1", "main")})
        ))
    );
}

/// A tail whose write-back lands after its instance was replaced (a `/clear`
/// or recycled pid) writes nothing: the generation gate drops the stale tail,
/// so the new instance's main file is untouched and no record follows the old
/// instance's `end` (SB-11).
#[test]
fn a_write_back_after_the_instance_was_replaced_touches_nothing() {
    let sb = HomeSandbox::new();
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let old_generation = hub
        .inner
        .lock()
        .unwrap()
        .instances
        .get(&100)
        .unwrap()
        .generation;
    // The record for pid 100 moves to a new session, and the new instance
    // registers with its own located main file.
    append_raw(&transcript_path(&sb, "s-two"), b"");
    table.lock().unwrap().insert(100, instance("s-two", "2"));
    let _ = hub.snapshot();
    start(&hub, 100, "s-two");
    assert!(
        hub.inner
            .lock()
            .unwrap()
            .instances
            .get(&100)
            .unwrap()
            .main_file
            .is_some(),
        "the new instance has a located main file"
    );
    let sub = hub.subscribe();
    // A stale tail that copied the old instance's state writes back late.
    let _ = hub.write_back_tail(
        100,
        old_generation,
        vec![serde_json::json!({"session": "s-one", "agent_id": null, "offset": 0, "record": row("u9", "stale")})],
        |live| {
            live.main_file = None;
        },
    );
    assert_eq!(
        sub.try_next(),
        Ok(None),
        "a stale write-back broadcast a record"
    );
    assert!(
        hub.inner
            .lock()
            .unwrap()
            .instances
            .get(&100)
            .unwrap()
            .main_file
            .is_some(),
        "a stale write-back touched the new instance's main file"
    );
}
