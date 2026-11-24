//! The prompts a silence close keeps aside: a module that went 40 s without
//! polling may only be stopped (a desk `claude` suspended with Ctrl+Z), so its
//! queued prompts wait for it under the same ids, and drop `session_end` only
//! once nothing can still run them: the process gone or replaced, or an `end`
//! for their session. Each terminal-frame case counts the `session.prompt`
//! frames a stream sees, so each prompt's single terminal frame is pinned
//! whatever the path.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, seed_device};

/// A pid no process holds on linux or macOS: above linux's largest `pid_max`
/// (2^22) and macOS's (99999), so a resolve that misses reads it gone.
const GONE: u32 = 2_000_000_000;

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

fn hub_over(rows: &[(u32, Instance)]) -> (Table, Arc<Hub>) {
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (records, resolver) = table(rows);
    (records, Hub::new(true, resolver))
}

fn event(hub: &Hub, pid: u32, body: &serde_json::Value) -> (u16, serde_json::Value) {
    hub.event(pid, body.to_string().as_bytes())
}

/// A start of a mod that reports each prompt's outcome, under `load` when the
/// mod numbers its module loads.
fn start(hub: &Hub, pid: u32, session: &str, seq: u64, load: Option<u64>) {
    let mut body = json!({"session": session, "seq": seq, "kind": "start", "cwd": "/w/app", "interactive": true, "prompt_reports": true});
    if let Some(n) = load {
        body["load"] = json!(n);
    }
    let (status, answer) = event(hub, pid, &body);
    assert_eq!(status, 200, "start {session}: {answer}");
}

/// The commands one zero-wait poll of a reporting mod takes, under `load`,
/// consumed as a written answer's are.
fn poll(hub: &Hub, pid: u32, session: &str, load: Option<u64>) -> serde_json::Value {
    let announced = Announced {
        prompt_reports: true,
        caps: std::collections::BTreeSet::new(),
        load,
        turns: None,
    };
    let PollAnswer {
        status,
        body,
        drained,
    } = hub.poll_announced(pid, session, Duration::ZERO, announced);
    if let Some(drained) = drained {
        hub.consume(drained);
    }
    assert_eq!(status, 200, "poll {session}: {body}");
    body["commands"].clone()
}

fn mint(hub: &Hub, session: &str, text: &str) -> String {
    match hub.deliver_prompt(session, text) {
        PromptDelivery::Queued { prompt: Some(id) } => id,
        _ => panic!("no prompt id minted for {session}"),
    }
}

fn prompt_command(text: &str, id: &str) -> serde_json::Value {
    json!({"kind": "prompt", "text": text, "prompt": id})
}

fn report(hub: &Hub, pid: u32, session: &str, seq: u64, id: &str) -> u16 {
    let body = json!({"session": session, "seq": seq, "kind": "prompt", "prompt": id, "state": "delivered"});
    event(hub, pid, &body).0
}

fn end(hub: &Hub, pid: u32, session: &str, seq: u64) {
    let (status, answer) = event(
        hub,
        pid,
        &json!({"session": session, "seq": seq, "kind": "end"}),
    );
    assert_eq!(status, 200, "end {session}: {answer}");
}

/// Every `session.prompt` frame broadcast since the last call, in order.
fn prompt_frames(sub: &Subscription) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    while let Ok(Some((kind, data))) = sub.try_next() {
        if kind == "session.prompt" {
            frames.push(data);
        }
    }
    frames
}

/// 41 s with no poll, then the periodic sweep.
fn silence(hub: &Hub) {
    hub.advance_clock(SILENCE_LIMIT + Duration::from_secs(1));
    hub.sweep();
}

fn delivered(session: &str, id: &str) -> serde_json::Value {
    json!({"session": session, "prompt": id, "state": "delivered"})
}

fn dropped(session: &str, id: &str) -> serde_json::Value {
    json!({"session": session, "prompt": id, "state": "dropped", "reason": "session_end"})
}

/// The backlog row's first verify: the module took the prompt, went silent
/// past the bound, then polled and reported it delivered. The stream sees that
/// delivery and never a drop, also when the report's connection reaches the
/// daemon before the module's next poll does.
#[test]
fn a_prompt_taken_before_a_silence_reports_delivered_once() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1")), (200, instance("s-two", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    start(&hub, 200, "s-two", 1, None);
    let id = mint(&hub, "s-one", "fix it");
    let reported_first = mint(&hub, "s-two", "fix it");
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([prompt_command("fix it", &id)])
    );
    assert_eq!(
        poll(&hub, 200, "s-two", None),
        json!([prompt_command("fix it", &reported_first)])
    );
    let sub = hub.subscribe();

    silence(&hub);
    assert_eq!(poll(&hub, 100, "s-one", None), json!([]));
    assert_eq!(report(&hub, 100, "s-one", 2, &id), 200);
    assert_eq!(report(&hub, 200, "s-two", 2, &reported_first), 200);
    assert_eq!(poll(&hub, 200, "s-two", None), json!([]));

    assert_eq!(
        prompt_frames(&sub),
        vec![delivered("s-one", &id), delivered("s-two", &reported_first)]
    );
}

/// A prompt no poll drained before the close comes back to the module's next
/// poll as the same command under the same id, behind nothing and in queue
/// order, and its outcome frame names that id.
#[test]
fn an_undrained_prompt_comes_back_to_the_next_poll_under_its_id() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    let taken = mint(&hub, "s-one", "one");
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([prompt_command("one", &taken)])
    );
    let second = mint(&hub, "s-one", "two");
    let third = mint(&hub, "s-one", "three");
    let sub = hub.subscribe();

    silence(&hub);
    assert_eq!(
        hub.snapshot()["prompts"],
        json!([
            {"session": "s-one", "prompt": taken, "state": "queued"},
            {"session": "s-one", "prompt": second, "state": "queued"},
            {"session": "s-one", "prompt": third, "state": "queued"},
        ])
    );
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([
            prompt_command("two", &second),
            prompt_command("three", &third)
        ])
    );
    assert_eq!(poll(&hub, 100, "s-one", None), json!([]));
    assert_eq!(report(&hub, 100, "s-one", 2, &third), 200);
    assert_eq!(report(&hub, 100, "s-one", 3, &second), 200);
    assert_eq!(report(&hub, 100, "s-one", 4, &taken), 200);

    assert_eq!(
        prompt_frames(&sub),
        vec![
            delivered("s-one", &third),
            delivered("s-one", &second),
            delivered("s-one", &taken),
        ]
    );
}

/// The backlog row's second verify: a kept prompt whose claude process exits
/// during the silence drops `session_end` once the sweep sees the exit, and
/// never before it, however many sweeps run while the process still lives.
#[test]
fn a_kept_prompt_drops_once_its_process_exits() {
    let _home = HomeSandbox::new();
    let (records, hub) = hub_over(&[(GONE, instance("s-gone", "1"))]);
    start(&hub, GONE, "s-gone", 1, None);
    let id = mint(&hub, "s-gone", "fix it");
    let sub = hub.subscribe();

    silence(&hub);
    silence(&hub);
    assert_eq!(
        prompt_frames(&sub),
        Vec::<serde_json::Value>::new(),
        "dropped while the process still lives"
    );

    records.lock().unwrap().remove(&GONE);
    hub.sweep();
    assert_eq!(prompt_frames(&sub), vec![dropped("s-gone", &id)]);
    hub.sweep();
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());
}

/// A pid recycled to another process start during the silence drops the
/// kept prompts `session_end` once, whether the sweep or the new process's
/// first poll sees it first, and the new process's polls never receive them,
/// even on the same session id.
#[test]
fn a_recycled_pid_drops_the_kept_prompts_and_never_hands_them_over() {
    let _home = HomeSandbox::new();
    let (records, hub) = hub_over(&[(100, instance("s-one", "1")), (200, instance("s-two", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    start(&hub, 200, "s-two", 1, None);
    let by_sweep = mint(&hub, "s-one", "one");
    let by_poll = mint(&hub, "s-two", "two");
    let sub = hub.subscribe();

    silence(&hub);
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());

    records.lock().unwrap().insert(100, instance("s-one", "2"));
    hub.sweep();
    assert_eq!(prompt_frames(&sub), vec![dropped("s-one", &by_sweep)]);
    assert_eq!(poll(&hub, 100, "s-one", None), json!([]));

    records.lock().unwrap().insert(200, instance("s-two", "2"));
    assert_eq!(poll(&hub, 200, "s-two", None), json!([]));
    assert_eq!(prompt_frames(&sub), vec![dropped("s-two", &by_poll)]);
    hub.sweep();
    assert_eq!(poll(&hub, 200, "s-two", None), json!([]));
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());
}

/// A `/clear` after the silence close (the same process now on a new session
/// id), or an `end` the module posts for the kept prompts' session, drops them
/// `session_end` once: the mod empties its own queue at every session end, so
/// nothing can run them after it. The new conversation's polls never get them.
#[test]
fn a_clear_after_the_silence_drops_the_kept_prompts_once() {
    let _home = HomeSandbox::new();
    let (records, hub) = hub_over(&[(100, instance("s-one", "1")), (200, instance("s-end", "2"))]);
    start(&hub, 100, "s-one", 1, None);
    start(&hub, 200, "s-end", 1, None);
    let cleared = mint(&hub, "s-one", "one");
    let ended = mint(&hub, "s-end", "two");
    let sub = hub.subscribe();

    silence(&hub);
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());

    records.lock().unwrap().insert(100, instance("s-new", "1"));
    start(&hub, 100, "s-new", 1, None);
    assert_eq!(prompt_frames(&sub), vec![dropped("s-one", &cleared)]);
    assert_eq!(poll(&hub, 100, "s-new", None), json!([]));

    end(&hub, 200, "s-end", 2);
    assert_eq!(prompt_frames(&sub), vec![dropped("s-end", &ended)]);
    assert_eq!(poll(&hub, 200, "s-end", None), json!([]));

    hub.sweep();
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());
}

/// A downgrade whose start post failed: the unnumbered module's polls are
/// stale against the numbered load, so they drain nothing and keep no
/// liveness, and the instance closes silent. Its prompts are kept the same
/// way, and the module's next poll, accepted by the instance that close left
/// room for, gets them back.
#[test]
fn a_silence_close_during_stale_polls_keeps_the_prompts_for_the_next_accepted_poll() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, Some(2));
    let id = mint(&hub, "s-one", "fix it");
    let sub = hub.subscribe();

    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([]),
        "a stale poll drained"
    );
    hub.advance_clock(Duration::from_secs(30));
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([]),
        "a stale poll drained"
    );
    hub.advance_clock(Duration::from_secs(11));
    hub.sweep();
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());

    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([prompt_command("fix it", &id)])
    );
    assert_eq!(report(&hub, 100, "s-one", 5, &id), 200);
    assert_eq!(prompt_frames(&sub), vec![delivered("s-one", &id)]);
}

/// Across every path a kept prompt can take, each gets exactly one terminal
/// frame: back and reported, back and ended, its process gone, its pid's
/// record moved to a `/clear`, and kept through two silences with a refused
/// contact between. Later sweeps, snapshots, polls and ends add none.
#[test]
fn each_kept_prompt_gets_exactly_one_terminal_frame() {
    let _home = HomeSandbox::new();
    let (records, hub) = hub_over(&[
        (301, instance("s-a", "1")),
        (302, instance("s-b", "1")),
        (GONE, instance("s-c", "1")),
        (304, instance("s-d", "1")),
        (305, instance("s-e", "1")),
    ]);
    let pids = [
        (301, "s-a"),
        (302, "s-b"),
        (GONE, "s-c"),
        (304, "s-d"),
        (305, "s-e"),
    ];
    let mut ids = HashMap::new();
    for (pid, session) in pids {
        start(&hub, pid, session, 1, None);
        ids.insert(session, mint(&hub, session, "go"));
    }
    assert_eq!(
        poll(&hub, 301, "s-a", None),
        json!([prompt_command("go", &ids["s-a"])])
    );
    let sub = hub.subscribe();
    silence(&hub);

    poll(&hub, 301, "s-a", None);
    assert_eq!(report(&hub, 301, "s-a", 2, &ids["s-a"]), 200);
    end(&hub, 301, "s-a", 3);

    assert_eq!(
        poll(&hub, 302, "s-b", None),
        json!([prompt_command("go", &ids["s-b"])])
    );
    end(&hub, 302, "s-b", 2);

    records.lock().unwrap().remove(&GONE);
    records.lock().unwrap().insert(304, instance("s-d2", "1"));
    hub.sweep();
    hub.sweep();
    start(&hub, 304, "s-d2", 1, None);
    assert_eq!(poll(&hub, 304, "s-d2", None), json!([]));

    let refused = json!({"session": "s-e", "seq": 0, "kind": "prompt", "prompt": ids["s-e"], "state": "delivered"});
    assert_eq!(event(&hub, 305, &refused).0, 409);
    assert_eq!(
        poll(&hub, 305, "s-e", None),
        json!([prompt_command("go", &ids["s-e"])])
    );
    silence(&hub);
    assert_eq!(poll(&hub, 305, "s-e", None), json!([]));
    assert_eq!(report(&hub, 305, "s-e", 2, &ids["s-e"]), 200);
    hub.sweep();
    hub.snapshot();

    let mut terminal: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    for frame in prompt_frames(&sub) {
        let session = frame["session"].as_str().unwrap().to_string();
        terminal.entry(session).or_default().push(frame);
    }
    let expected: HashMap<String, Vec<serde_json::Value>> = [
        ("s-a", delivered("s-a", &ids["s-a"])),
        ("s-b", dropped("s-b", &ids["s-b"])),
        ("s-c", dropped("s-c", &ids["s-c"])),
        ("s-d", dropped("s-d", &ids["s-d"])),
        ("s-e", delivered("s-e", &ids["s-e"])),
    ]
    .into_iter()
    .map(|(session, frame)| (session.to_string(), vec![frame]))
    .collect();
    assert_eq!(terminal, expected);
}

/// An instance whose own `end` already landed keeps nothing when it goes
/// silent while that end's final tail still runs: the mod emptied its queue
/// at the end, so its prompts drop `session_end` at the close, and the same
/// process and session polling again gets none of them.
#[test]
fn an_ending_instance_closed_silent_drops_its_prompts_at_once() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    let id = mint(&hub, "s-one", "fix it");
    let sub = hub.subscribe();
    hub.inner
        .lock()
        .unwrap()
        .instances
        .get_mut(&100)
        .expect("the instance is live")
        .tailing = true;
    let ending = Arc::clone(&hub);
    let end = std::thread::spawn(move || {
        ending.event(
            100,
            json!({"session": "s-one", "seq": 2, "kind": "end"})
                .to_string()
                .as_bytes(),
        )
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
            "the end never began its final tail"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    silence(&hub);
    assert_eq!(
        end.join().expect("the end thread"),
        (200, json!({"ok": true}))
    );
    assert_eq!(prompt_frames(&sub), vec![dropped("s-one", &id)]);
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(poll(&hub, 100, "s-one", None), json!([]));
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());
}

/// A silence close keeps only prompts with no outcome yet: one reported
/// delivered before the close is not listed by the snapshot and never comes
/// back to the module's next poll.
#[test]
fn a_settled_prompt_is_not_kept() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    let id = mint(&hub, "s-one", "fix it");
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([prompt_command("fix it", &id)])
    );
    let sub = hub.subscribe();
    assert_eq!(report(&hub, 100, "s-one", 2, &id), 200);

    silence(&hub);
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(poll(&hub, 100, "s-one", None), json!([]));
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(prompt_frames(&sub), vec![delivered("s-one", &id)]);
}

/// A silence close keeps the undrained prompt commands and leaves every other
/// command behind: an interrupt queued for the closed instance never reaches
/// the module's next poll.
#[test]
fn a_close_keeps_prompt_commands_only() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    let id = mint(&hub, "s-one", "fix it");
    assert!(matches!(
        hub.deliver("s-one", json!({"kind": "interrupt"})),
        ModDelivery::Delivered
    ));
    let sub = hub.subscribe();

    silence(&hub);
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([prompt_command("fix it", &id)])
    );
    assert_eq!(report(&hub, 100, "s-one", 2, &id), 200);
    assert_eq!(prompt_frames(&sub), vec![delivered("s-one", &id)]);
}

/// A reconnect's snapshot re-resolves a kept set's pid: once the process is
/// gone, the snapshot drops the prompt `session_end` and lists none, without
/// waiting for the next sweep.
#[test]
fn a_snapshot_drops_a_kept_set_whose_process_is_gone() {
    let _home = HomeSandbox::new();
    let (records, hub) = hub_over(&[(GONE, instance("s-gone", "1"))]);
    start(&hub, GONE, "s-gone", 1, None);
    let id = mint(&hub, "s-gone", "fix it");
    let sub = hub.subscribe();
    silence(&hub);
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());

    records.lock().unwrap().remove(&GONE);
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(prompt_frames(&sub), vec![dropped("s-gone", &id)]);
}

/// A snapshot lists a kept set's prompts in the same session-then-pid order as
/// live instances', also when the kept set's session id sorts before a live
/// instance's: the merge re-sorts both holders, not only appends the kept ones.
#[test]
fn a_snapshot_orders_kept_and_live_prompts_by_session() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-z", "1")), (200, instance("s-a", "1"))]);
    start(&hub, 100, "s-z", 1, None);
    start(&hub, 200, "s-a", 1, None);
    let live = mint(&hub, "s-z", "live");
    let kept = mint(&hub, "s-a", "kept");

    silence(&hub);
    assert_eq!(
        poll(&hub, 100, "s-z", None),
        json!([prompt_command("live", &live)])
    );
    assert_eq!(
        hub.snapshot()["prompts"],
        json!([
            {"session": "s-a", "prompt": kept, "state": "queued"},
            {"session": "s-z", "prompt": live, "state": "queued"},
        ])
    );
}

/// A kept set replaced by another process's while the sweep resolved the
/// predecessor's identity survives: the identity re-check drops only the set
/// the resolve compared against.
#[test]
fn a_replaced_kept_set_survives_the_resolve_that_saw_its_predecessor_move() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    mint(&hub, "s-one", "fix it");
    let sub = hub.subscribe();
    silence(&hub);
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());

    let mut inner = hub.inner.lock().unwrap();
    inner
        .kept
        .get_mut(&100)
        .expect("the silence close kept the prompt")
        .session_id = "s-two".to_string();
    let moved = HashMap::from([(100, ("1".to_string(), "s-one".to_string()))]);
    hub.close_moved(&mut inner, &moved);
    assert!(inner.kept.contains_key(&100));
    assert_eq!(prompt_frames(&sub), Vec::<serde_json::Value>::new());
}

/// A refused first-contact `end` for a silence-closed session changes nothing:
/// the prompts it restored are set aside again, and the module's next accepted
/// poll gets them back under their id.
#[test]
fn a_refused_first_contact_end_puts_the_kept_prompts_back() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-one", "1"))]);
    start(&hub, 100, "s-one", 1, None);
    let id = mint(&hub, "s-one", "fix it");
    let sub = hub.subscribe();
    silence(&hub);

    let refused = json!({"session": "s-one", "seq": 0, "kind": "end"});
    assert_eq!(event(&hub, 100, &refused).0, 409);
    assert_eq!(
        poll(&hub, 100, "s-one", None),
        json!([prompt_command("fix it", &id)])
    );
    assert_eq!(report(&hub, 100, "s-one", 2, &id), 200);
    assert_eq!(prompt_frames(&sub), vec![delivered("s-one", &id)]);
}

/// A snapshot orders two holders of one session id by pid: a kept set at the
/// lower pid lists before the live instance at the higher, the doc's
/// session-then-pid rule across both holder kinds.
#[test]
fn a_snapshot_orders_two_holders_of_one_session_by_pid() {
    let _home = HomeSandbox::new();
    let (_records, hub) = hub_over(&[(100, instance("s-x", "1")), (200, instance("s-x", "1"))]);
    start(&hub, 100, "s-x", 1, None);
    let kept = mint(&hub, "s-x", "kept");
    start(&hub, 200, "s-x", 1, None);

    silence(&hub);
    assert_eq!(poll(&hub, 200, "s-x", None), json!([]));
    let live = mint(&hub, "s-x", "live");
    assert_eq!(
        hub.snapshot()["prompts"],
        json!([
            {"session": "s-x", "prompt": kept, "state": "queued"},
            {"session": "s-x", "prompt": live, "state": "queued"},
        ])
    );
}
