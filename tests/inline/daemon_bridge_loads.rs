//! Module loads: a hot reload of the mod (upgrade or downgrade, numbered or
//! not) leaves a poll the torn-down module already sent in flight beside the
//! new module's `start`. Only the instance's current load sets its caps and
//! prompt tracking, drains its queue and parks its poll. Polls go through the
//! real `/poll` query parse ([`Announced::from_poll`]).

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, body_json, call, req, seed_device};

/// A poll's park bound in these tests: far above "at once".
const PARK: Duration = Duration::from_secs(5);
/// What "answers at once" means here.
const AT_ONCE: Duration = Duration::from_secs(2);

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

/// One module load of the mod, as its start and its polls both announce it.
#[derive(Clone, Copy)]
struct Load {
    number: Option<u64>,
    notes: bool,
    reports: bool,
}

impl Load {
    const fn numbered(number: u64) -> Self {
        Self {
            number: Some(number),
            notes: true,
            reports: true,
        }
    }

    /// A released mod: it sends no load number.
    const fn unnumbered() -> Self {
        Self {
            number: None,
            notes: true,
            reports: true,
        }
    }

    /// The same load declaring no capability and no prompt reports.
    const fn bare(self) -> Self {
        Self {
            notes: false,
            reports: false,
            ..self
        }
    }

    fn start_body(self, session: &str, seq: u64) -> serde_json::Value {
        let mut body = json!({"session": session, "seq": seq, "kind": "start", "cwd": "/w/app", "interactive": true});
        if self.notes {
            body["caps"] = json!(["notes"]);
        }
        if self.reports {
            body["prompt_reports"] = json!(true);
        }
        if let Some(n) = self.number {
            body["load"] = json!(n);
        }
        body
    }

    fn query(self, session: &str) -> String {
        let mut query = format!("session={session}");
        if self.reports {
            query.push_str("&prompt_reports=1");
        }
        if self.notes {
            query.push_str("&caps=notes");
        }
        if let Some(n) = self.number {
            let _ = write!(query, "&load={n}");
        }
        query
    }
}

fn start(hub: &Hub, pid: u32, session: &str, seq: u64, load: Load) {
    let body = load.start_body(session, seq);
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "start {body}: {answer}");
}

/// One `GET /poll?<query>` as `serve` reads it, its drained commands consumed
/// as a written answer's are.
fn poll_query(hub: &Hub, pid: u32, query: &str, wait: Duration) -> (u16, serde_json::Value) {
    let path = format!("/poll?{query}");
    let session = parse_poll_session(&path).unwrap_or_default();
    let PollAnswer {
        status,
        body,
        drained,
    } = hub.poll_announced(pid, session, wait, Announced::from_poll(&path));
    if let Some(drained) = drained {
        hub.consume(drained);
    }
    (status, body)
}

fn poll(
    hub: &Hub,
    pid: u32,
    session: &str,
    load: Load,
    wait: Duration,
) -> (u16, serde_json::Value) {
    poll_query(hub, pid, &load.query(session), wait)
}

/// The commands one zero-wait poll of `load` takes.
fn drain(hub: &Hub, pid: u32, session: &str, load: Load) -> serde_json::Value {
    let (status, answer) = poll(hub, pid, session, load, Duration::ZERO);
    assert_eq!(status, 200, "poll {session}: {answer}");
    answer["commands"].clone()
}

fn interrupt(hub: &Hub, session: &str) {
    assert!(
        matches!(
            hub.deliver(session, json!({"kind": "interrupt"})),
            ModDelivery::Delivered
        ),
        "the interrupt was queued"
    );
}

/// The id a phone prompt queued now gets: `None` while the instance does not
/// track prompt outcomes.
fn prompt_id(hub: &Hub, session: &str) -> Option<String> {
    match hub.deliver_prompt(session, "hello") {
        PromptDelivery::Queued { prompt } => prompt,
        _ => panic!("the prompt was not queued for {session}"),
    }
}

fn caps(hub: &Hub) -> serde_json::Value {
    hub.snapshot()["sessions"][0]["caps"].clone()
}

fn instance_caps(hub: &Hub, pid: u32) -> Vec<&'static str> {
    let inner = hub.inner.lock().unwrap();
    Cap::names(&inner.instances.get(&pid).expect("the live instance").caps)
}

fn queue_interrupt(hub: &Hub, pid: u32) {
    let mut inner = hub.inner.lock().unwrap();
    let live = inner.instances.get_mut(&pid).expect("the live instance");
    live.commands.push_back(json!({"kind": "interrupt"}));
}

fn ctx_over(hub: &Arc<Hub>) -> Arc<ApiContext> {
    let config = Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ));
    let status_path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json");
    let no_herdr: PaneProbe = Box::new(|_, _| HerdrProbeOut::NotInstalled);
    ApiContext::for_tests_with_bridge(config, status_path, no_herdr, Arc::clone(hub))
}

const QUESTION: &str = "Which colour?";

fn open_question(hub: &Hub, pid: u32, session: &str, seq: u64, ask: &str) {
    let input = json!([{"question": QUESTION, "header": "Colour", "multiSelect": false, "options": [{"label": "Red"}, {"label": "Blue"}]}]);
    let body = json!({"session": session, "seq": seq, "kind": "ask_opened", "ask": ask, "tool": "AskUserQuestion", "input": input});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "ask_opened {ask}: {answer}");
}

/// After a numbered load's start that declares no caps, a poll the older load
/// sent with `caps=notes` leaves the instance without `notes`: the listing
/// shows none and a note is refused by name.
#[test]
fn an_older_loads_poll_never_declares_caps() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    let _stream = hub.subscribe_as(Some(&Tier::Control));
    start(&hub, 100, "s-one", 1, Load::numbered(2).bare());

    let (status, _) = poll(&hub, 100, "s-one", Load::numbered(1), Duration::ZERO);
    assert_eq!(status, 200);
    assert_eq!(caps(&hub), json!([]), "an older load's poll declared notes");

    open_question(&hub, 100, "s-one", 2, "q1");
    let resp = call(
        &ctx_over(&hub),
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/q1",
            Some(TOKEN),
            &json!({"answers": {QUESTION: "Red"}, "notes": {QUESTION: "darker"}}).to_string(),
        ),
    );
    assert_eq!(resp.status, 400, "{}", body_json(&resp));
    assert_eq!(body_json(&resp)["error"], "notes_unsupported");
}

/// After a numbered load's start without prompt reports, a poll the older
/// load sent announcing them leaves tracking off.
#[test]
fn an_older_loads_poll_never_turns_prompt_tracking_on() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(2).bare());

    poll(&hub, 100, "s-one", Load::numbered(1), Duration::ZERO);
    assert_eq!(
        prompt_id(&hub, "s-one"),
        None,
        "an older load's poll turned prompt tracking on"
    );
}

/// A poll from an older load answers empty at once, even with a command
/// queued, and the current load's next poll takes that command.
#[test]
fn an_older_loads_poll_drains_nothing() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(2));
    interrupt(&hub, "s-one");

    let asked = Instant::now();
    let (status, answer) = poll(&hub, 100, "s-one", Load::numbered(1), PARK);
    assert!(
        asked.elapsed() < AT_ONCE,
        "the stale poll waited {:?}",
        asked.elapsed()
    );
    assert_eq!(status, 200);
    assert_eq!(
        answer["commands"],
        json!([]),
        "an older load's poll drained the queue"
    );
    assert_eq!(
        drain(&hub, 100, "s-one", Load::numbered(2)),
        json!([{"kind": "interrupt"}]),
        "the current load's poll took the command"
    );
}

/// A start of an older load than the instance's current one changes nothing:
/// no caps, no prompt tracking, no drain.
#[test]
fn an_older_loads_start_changes_nothing() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(2));
    interrupt(&hub, "s-one");

    start(&hub, 100, "s-one", 2, Load::numbered(1).bare());

    assert_eq!(
        caps(&hub),
        json!(["notes"]),
        "an older load's start replaced the caps"
    );
    assert!(
        prompt_id(&hub, "s-one").is_some(),
        "an older load's start turned prompt tracking off"
    );
    let commands = drain(&hub, 100, "s-one", Load::numbered(2));
    assert_eq!(
        commands[0],
        json!({"kind": "interrupt"}),
        "the current load's poll took the queued interrupt"
    );
    assert_eq!(
        commands[1]["kind"], "prompt",
        "the phone prompt stayed queued for the current load"
    );
    assert_eq!(commands[1]["text"], "hello");
}

/// A stale poll is no liveness: it never moves the instance's `last_poll`, so
/// the silence close still comes at its own deadline.
#[test]
fn a_stale_poll_never_refreshes_liveness() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(2));
    poll(&hub, 100, "s-one", Load::numbered(2), Duration::ZERO);
    let before = hub.inner.lock().unwrap().instances[&100].last_poll;

    hub.advance_clock(Duration::from_secs(30));
    poll(&hub, 100, "s-one", Load::numbered(1), Duration::ZERO);

    let after = hub.inner.lock().unwrap().instances[&100].last_poll;
    assert_eq!(
        after, before,
        "a stale poll refreshed the instance's liveness"
    );
}

/// With the current load's poll parked, an older load's poll answers empty at
/// once and leaves that park standing: a command queued afterwards reaches
/// the current load's poll.
#[test]
fn an_older_loads_poll_never_displaces_the_parked_poll() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(2));

    std::thread::scope(|scope| {
        let parked = scope.spawn(|| poll(&hub, 100, "s-one", Load::numbered(2), PARK));
        assert!(
            hub.wait_until_parked(100, 1, PARK),
            "the current load's poll parked"
        );
        let asked = Instant::now();
        let (status, answer) = poll(&hub, 100, "s-one", Load::numbered(1), PARK);
        assert!(
            asked.elapsed() < AT_ONCE,
            "the stale poll waited {:?}",
            asked.elapsed()
        );
        assert_eq!((status, answer["commands"].clone()), (200, json!([])));

        interrupt(&hub, "s-one");
        let (status, answer) = parked.join().unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            answer["commands"],
            json!([{"kind": "interrupt"}]),
            "the stale poll displaced the current load's parked poll"
        );
    });
}

/// Opens the FIFO's write end on drop: a reader blocked in `open` gets
/// through and reads EOF, so a failed assertion unwinds instead of leaving
/// the scope joining a reader no writer will ever release. Non-blocking, so
/// with no reader left it is a no-op.
struct ReleaseFifo<'a>(&'a std::path::Path);

impl Drop for ReleaseFifo<'_> {
    fn drop(&mut self) {
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(self.0),
        );
    }
}

/// A first-contact poll of an older load that pauses inside its registration
/// walk never displaces the parked poll of a newer load that started meanwhile:
/// it answers empty at once, and a command queued afterwards goes to the newer
/// load's poll. The FIFO transcript gives the pause deterministically.
#[test]
fn a_poll_paused_before_its_park_never_displaces_a_newer_loads_park() {
    let home = HomeSandbox::new();
    let fifo = home.home().join(".claude/projects/-w-app/s-one.jsonl");
    std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo failed: {made}");
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    std::thread::scope(|scope| {
        let _release = ReleaseFifo(&fifo);
        let older = scope.spawn(|| poll(&hub, 100, "s-one", Load::numbered(1), PARK));
        let deadline = Instant::now() + PARK;
        while !hub.inner.lock().unwrap().instances.contains_key(&100) {
            assert!(
                Instant::now() < deadline,
                "the older load's poll never drained"
            );
            std::thread::yield_now();
        }
        start(&hub, 100, "s-one", 1, Load::numbered(2));
        let newer = scope.spawn(|| poll(&hub, 100, "s-one", Load::numbered(2), PARK));
        assert!(
            hub.wait_until_parked(100, 1, PARK),
            "the newer load's poll parked"
        );
        let asked = Instant::now();
        drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        let (status, answer) = older.join().unwrap();
        assert!(
            asked.elapsed() < AT_ONCE,
            "the older poll waited {:?}",
            asked.elapsed()
        );
        assert_eq!((status, answer["commands"].clone()), (200, json!([])));
        interrupt(&hub, "s-one");
        let (status, answer) = newer.join().unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            answer["commands"],
            json!([{"kind": "interrupt"}]),
            "the paused older poll displaced the newer load's park"
        );
    });
}

/// A new load's start wakes the older load's parked poll empty at once, and a
/// command queued after that start reaches only a poll of the new load.
#[test]
fn a_new_loads_start_wakes_the_older_loads_parked_poll_empty() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(1));

    std::thread::scope(|scope| {
        let (done, woke) = mpsc::channel();
        let parked: &Hub = &hub;
        scope.spawn(move || {
            let answer = poll(parked, 100, "s-one", Load::numbered(1), PARK);
            done.send(answer).unwrap();
        });
        assert!(
            hub.wait_until_parked(100, 1, PARK),
            "the older load's poll parked"
        );

        start(&hub, 100, "s-one", 2, Load::numbered(2));
        let (status, answer) = woke
            .recv_timeout(AT_ONCE)
            .expect("the new load's start left the older load's poll parked");
        assert_eq!((status, answer["commands"].clone()), (200, json!([])));

        interrupt(&hub, "s-one");
        assert_eq!(drain(&hub, 100, "s-one", Load::numbered(1)), json!([]));
        assert_eq!(
            drain(&hub, 100, "s-one", Load::numbered(2)),
            json!([{"kind": "interrupt"}])
        );
    });
}

/// Downgrade to a released mod: after load n and then a start with no load
/// number, a poll numbered n or lower changes nothing and drains nothing,
/// while an unnumbered poll applies its caps and flag and drains.
#[test]
fn after_an_unnumbered_start_numbered_polls_are_stale() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(3));
    start(&hub, 100, "s-one", 2, Load::unnumbered().bare());
    interrupt(&hub, "s-one");

    for older in [3, 2] {
        assert_eq!(
            drain(&hub, 100, "s-one", Load::numbered(older)),
            json!([]),
            "load {older}'s poll drained the queue"
        );
        assert_eq!(caps(&hub), json!([]), "load {older}'s poll declared notes");
    }
    assert_eq!(
        prompt_id(&hub, "s-one"),
        None,
        "an older numbered poll turned prompt tracking on"
    );

    assert_eq!(
        drain(&hub, 100, "s-one", Load::unnumbered()),
        json!([{"kind": "interrupt"}, {"kind": "prompt", "text": "hello"}]),
        "the unnumbered load's poll drains"
    );
    assert_eq!(caps(&hub), json!(["notes"]));
    assert!(
        prompt_id(&hub, "s-one").is_some(),
        "the unnumbered poll's flag applies"
    );
}

/// After a downgrade to an unnumbered mod, a numbered poll above the highest
/// number seen is a newer load whose start never reached the hub: it becomes
/// current, and the unnumbered load's next poll is stale.
#[test]
fn a_numbered_poll_above_a_downgrades_high_becomes_current() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(3));
    start(&hub, 100, "s-one", 2, Load::unnumbered().bare());
    interrupt(&hub, "s-one");

    assert_eq!(
        drain(&hub, 100, "s-one", Load::numbered(4)),
        json!([{"kind": "interrupt"}]),
        "a numbered poll above the downgrade's high did not become current"
    );
    assert_eq!(caps(&hub), json!(["notes"]));

    interrupt(&hub, "s-one");
    assert_eq!(
        drain(&hub, 100, "s-one", Load::unnumbered()),
        json!([]),
        "the unnumbered load's poll drained after a newer numbered load polled"
    );
    assert_eq!(
        caps(&hub),
        json!(["notes"]),
        "the unnumbered load's poll replaced the caps"
    );
}

/// Upgrade from a released mod: after an unnumbered start and then a numbered
/// one, a poll with no load number changes nothing and drains nothing.
#[test]
fn after_a_numbered_start_unnumbered_polls_are_stale() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::unnumbered());
    start(&hub, 100, "s-one", 2, Load::numbered(1).bare());
    interrupt(&hub, "s-one");

    assert_eq!(
        drain(&hub, 100, "s-one", Load::unnumbered()),
        json!([]),
        "the unnumbered poll drained the queue"
    );
    assert_eq!(caps(&hub), json!([]), "the unnumbered poll declared notes");
    assert_eq!(
        prompt_id(&hub, "s-one"),
        None,
        "the unnumbered poll turned tracking on"
    );
}

/// A fresh hub (a restarted daemon) meeting a numbered poll first takes it as
/// current; a later poll with a lower number is stale.
#[test]
fn a_restarted_daemon_takes_the_first_numbered_poll_as_current() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);

    assert_eq!(drain(&hub, 100, "s-one", Load::numbered(4)), json!([]));
    assert_eq!(caps(&hub), json!(["notes"]), "the first poll's caps apply");
    interrupt(&hub, "s-one");
    assert_eq!(
        drain(&hub, 100, "s-one", Load::numbered(4)),
        json!([{"kind": "interrupt"}]),
        "the first poll's load drains"
    );

    interrupt(&hub, "s-one");
    assert_eq!(
        drain(&hub, 100, "s-one", Load::numbered(3).bare()),
        json!([]),
        "a lower load's poll drained the queue"
    );
    assert_eq!(
        caps(&hub),
        json!(["notes"]),
        "a lower load's poll replaced the caps"
    );
    assert!(
        prompt_id(&hub, "s-one").is_some(),
        "a lower load's poll turned prompt tracking off"
    );
}

/// A poll from a load newer than the last start (that load's start never
/// reached the hub) becomes current; the last start's load is stale after it.
#[test]
fn a_newer_loads_poll_without_its_start_becomes_current() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(5).bare());

    assert_eq!(drain(&hub, 100, "s-one", Load::numbered(6)), json!([]));
    assert_eq!(caps(&hub), json!(["notes"]), "the newer load's caps apply");

    interrupt(&hub, "s-one");
    assert_eq!(
        drain(&hub, 100, "s-one", Load::numbered(5).bare()),
        json!([]),
        "the last start's load drained after a newer load polled"
    );
    assert_eq!(caps(&hub), json!(["notes"]));
    assert_eq!(
        drain(&hub, 100, "s-one", Load::numbered(6)),
        json!([{"kind": "interrupt"}])
    );
}

/// Prompt tracking follows the current load: a newer load's start without the
/// flag, after a load that reported, turns tracking off.
#[test]
fn a_newer_loads_start_without_reports_turns_tracking_off() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(1));
    assert!(
        prompt_id(&hub, "s-one").is_some(),
        "the reporting load tracks"
    );

    start(&hub, 100, "s-one", 2, Load::numbered(2).bare());
    assert_eq!(
        prompt_id(&hub, "s-one"),
        None,
        "tracking stayed on past a load that does not report"
    );
}

/// The load state is per instance: two instances on one session id never
/// make each other's polls stale, and the instance a `/clear` makes takes the
/// same load's next poll as current.
#[test]
fn load_numbers_are_per_instance() {
    let _home = HomeSandbox::new();
    let (t, r) = table(&[(100, instance("s-one", "1")), (200, instance("s-one", "2"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(7).bare());
    start(&hub, 200, "s-one", 1, Load::numbered(1).bare());

    queue_interrupt(&hub, 200);
    assert_eq!(
        drain(&hub, 200, "s-one", Load::numbered(1)),
        json!([{"kind": "interrupt"}]),
        "another instance's higher load made this one's poll stale"
    );
    assert_eq!(instance_caps(&hub, 200), ["notes"]);
    assert_eq!(instance_caps(&hub, 100), [] as [&str; 0]);

    // The `/clear`: the same module load re-registers under the new id.
    t.lock().unwrap().insert(200, instance("s-two", "2"));
    start(&hub, 200, "s-two", 1, Load::numbered(1).bare());
    queue_interrupt(&hub, 200);
    assert_eq!(
        drain(&hub, 200, "s-two", Load::numbered(1)),
        json!([{"kind": "interrupt"}]),
        "the cleared instance's load reached the new instance"
    );
    assert_eq!(instance_caps(&hub, 200), ["notes"]);
}

/// The instance a `/clear` makes starts with no load seen: a load number lower
/// than the cleared instance's load is current there, and its polls drain and
/// apply caps.
#[test]
fn a_cleared_instance_takes_a_lower_load_as_current() {
    let _home = HomeSandbox::new();
    let (t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(7).bare());
    t.lock().unwrap().insert(100, instance("s-two", "1"));
    start(&hub, 100, "s-two", 1, Load::numbered(1));
    queue_interrupt(&hub, 100);
    assert_eq!(
        drain(&hub, 100, "s-two", Load::numbered(1)),
        json!([{"kind": "interrupt"}]),
        "the cleared instance's load made the new instance's polls stale"
    );
    assert_eq!(instance_caps(&hub, 100), ["notes"]);
}

/// A `load` that is not plain decimal digits counts as absent: after a
/// numbered start it is an unnumbered poll, so it is stale.
#[test]
fn a_load_that_is_not_decimal_digits_counts_as_absent() {
    let _home = HomeSandbox::new();
    let (_t, r) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, r);
    start(&hub, 100, "s-one", 1, Load::numbered(3).bare());
    for load in ["+3", "3.0", "-3", "", "99999999999999999999"] {
        interrupt(&hub, "s-one");
        let (status, answer) = poll_query(
            &hub,
            100,
            &format!("session=s-one&caps=notes&load={load}"),
            Duration::ZERO,
        );
        assert_eq!(status, 200);
        assert_eq!(
            answer["commands"],
            json!([]),
            "load={load:?} read as a number"
        );
        assert_eq!(
            drain(&hub, 100, "s-one", Load::numbered(3).bare()),
            json!([{"kind": "interrupt"}])
        );
    }
}
