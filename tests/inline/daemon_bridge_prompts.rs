#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A phone prompt's life on the mod path: the texts the session prompt route
//! refuses because Claude Code always refuses them, the id the daemon mints for
//! each prompt it hands a mod, the `session.prompt` frames that follow it from
//! queued to delivered or dropped, and the `prompts` array of the snapshot a
//! (re)connecting stream opens with.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, TOKEN, body_json, call, req, seed_device};

const WAIT: Duration = Duration::from_secs(5);
const NONE: Duration = Duration::from_millis(300);

const PROMPT_COMMAND: &str = "Claude Code reads a prompt starting with / as a command and refuses it from the phone; type it at the session's terminal";
const PROMPT_BLANK: &str = "the prompt holds only whitespace";

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

fn event(hub: &Hub, pid: u32, body: serde_json::Value) -> (u16, serde_json::Value) {
    hub.event(pid, body.to_string().as_bytes())
}

/// A session whose mod reports each prompt's outcome.
fn start(hub: &Hub, pid: u32, session: &str) {
    let (status, answer) = event(
        hub,
        pid,
        json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true, "prompt_reports": true}),
    );
    assert_eq!(status, 200, "start {session}: {answer}");
}

/// A session whose mod predates outcome reports: its `start` carries no flag.
fn start_older_mod(hub: &Hub, pid: u32, session: &str) {
    let (status, answer) = event(
        hub,
        pid,
        json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true}),
    );
    assert_eq!(status, 200, "start {session}: {answer}");
}

fn commands(hub: &Hub, pid: u32, session: &str) -> serde_json::Value {
    let (status, answer) = hub.poll(pid, session, Duration::ZERO);
    assert_eq!(status, 200, "poll {session}: {answer}");
    answer["commands"].clone()
}

fn prompt(ctx: &ApiContext, session: &str, text: &str) -> (u16, serde_json::Value) {
    let resp = call(
        ctx,
        &req(
            "POST",
            &format!("/api/v1/sessions/{session}/prompt"),
            Some(TOKEN),
            &json!({ "text": text }).to_string(),
        ),
    );
    (resp.status, body_json(&resp))
}

/// The id a mod-path 200 carries, checked to be the body's only addition.
fn minted(answer: &serde_json::Value) -> String {
    let id = answer["prompt"]
        .as_str()
        .expect("a minted prompt id")
        .to_string();
    assert!(!id.is_empty(), "an empty prompt id");
    assert_eq!(answer, &json!({"ok": true, "via": "mod", "prompt": id}));
    id
}

/// The next `session.prompt` frame, skipping every other frame kind.
fn next_prompt_frame(sub: &Subscription, within: Duration) -> Option<serde_json::Value> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let (kind, data) = sub.recv_timeout(left)?;
        if kind == "session.prompt" {
            return Some(data);
        }
    }
}

fn report(hub: &Hub, pid: u32, seq: u64, body: serde_json::Value) -> (u16, serde_json::Value) {
    let mut full = json!({"session": "s-one", "seq": seq, "kind": "prompt"});
    full.as_object_mut()
        .unwrap()
        .extend(body.as_object().unwrap().clone());
    event(hub, pid, full)
}

/// Claude Code refuses a text whose JavaScript `trimStart()` begins with `/`,
/// and one whose `trim()` is empty (2.1.322): the mod path refuses both by
/// name and queues nothing. JavaScript's whitespace counts U+FEFF and not
/// U+0085, so a NEL before a `/` is no command and a lone NEL is no blank.
#[test]
fn the_mod_path_refuses_by_name_what_claude_code_always_refuses() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);

    for text in [
        "/compact",
        "  /model",
        "\t\n/x",
        "\u{FEFF}/x",
        "\u{3000}/x",
        "\u{2028}/x",
    ] {
        assert_eq!(
            prompt(&ctx, "s-one", text),
            (
                400,
                json!({"ok": false, "error": "prompt_command", "reason": PROMPT_COMMAND})
            ),
            "{text:?}"
        );
    }
    for text in ["   ", "\n\t", "\u{FEFF}", "\u{A0}\u{2029}", "\u{B}\u{C}"] {
        assert_eq!(
            prompt(&ctx, "s-one", text),
            (
                400,
                json!({"ok": false, "error": "prompt_blank", "reason": PROMPT_BLANK})
            ),
            "{text:?}"
        );
    }
    assert_eq!(
        prompt(&ctx, "s-one", ""),
        (400, json!({"ok": false, "error": "bad_request"}))
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        json!([]),
        "a refused text queues nothing"
    );

    let mut sent = Vec::new();
    for text in ["a/b", "\u{85}/x", "\u{85}", " fix it "] {
        let (status, answer) = prompt(&ctx, "s-one", text);
        assert_eq!(status, 200, "{text:?}: {answer}");
        sent.push(json!({"kind": "prompt", "text": text, "prompt": minted(&answer)}));
    }
    assert_eq!(commands(&hub, 100, "s-one"), json!(sent));
}

/// The refusal is the mod path's alone: the herdr fallback types into a real
/// terminal, where a `/` text runs as the command it is.
#[test]
fn the_herdr_fallback_passes_a_slash_prompt_through() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Arc::new(Hub::new(true, resolver));
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
            ["agent", "prompt", "w1:p1", _] => HerdrProbeOut::Ran(Some(HerdrOut {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })),
            _ => HerdrProbeOut::Ran(None),
        }
    });
    let ctx = ApiContext::for_tests_with_bridge(config(), status_path(), probe, Arc::clone(&hub));

    for text in ["/compact", "   "] {
        assert_eq!(
            prompt(&ctx, "s-one", text),
            (200, json!({"ok": true, "via": "herdr"})),
            "{text:?}"
        );
        assert!(
            seen.lock().unwrap().contains(&vec![
                "agent".to_string(),
                "prompt".to_string(),
                "w1:p1".to_string(),
                text.to_string()
            ]),
            "{text:?} reached herdr"
        );
    }
}

/// Each prompt handed to a mod gets its own id, carried in its command, and a
/// `queued` frame naming it, in the order the prompts were posted.
#[test]
fn each_mod_prompt_gets_its_own_id_and_a_queued_frame() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let sub = hub.subscribe();

    let (_, first) = prompt(&ctx, "s-one", "one");
    let (_, second) = prompt(&ctx, "s-one", "two");
    let (first, second) = (minted(&first), minted(&second));
    assert_ne!(first, second, "two prompts share an id");

    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(json!({"session": "s-one", "prompt": first, "state": "queued"}))
    );
    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(json!({"session": "s-one", "prompt": second, "state": "queued"}))
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        json!([
            {"kind": "prompt", "text": "one", "prompt": first},
            {"kind": "prompt", "text": "two", "prompt": second}
        ])
    );
}

/// A mod that never reports outcomes gets no id to report on: its prompts are
/// answered and queued as before, with no id, no frame and no snapshot entry,
/// so none sits `queued` forever or reads as dropped at its end. The texts
/// Claude Code always refuses are still refused.
#[test]
fn an_older_mod_gets_no_prompt_ids() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start_older_mod(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let sub = hub.subscribe();

    assert_eq!(
        prompt(&ctx, "s-one", "fix it"),
        (200, json!({"ok": true, "via": "mod"}))
    );
    assert_eq!(
        prompt(&ctx, "s-one", "/compact"),
        (
            400,
            json!({"ok": false, "error": "prompt_command", "reason": PROMPT_COMMAND})
        )
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        json!([{"kind": "prompt", "text": "fix it"}])
    );
    assert_eq!(hub.snapshot()["prompts"], json!([]));
    assert_eq!(
        event(
            &hub,
            100,
            json!({"session": "s-one", "seq": 2, "kind": "end"})
        )
        .0,
        200
    );
    assert_eq!(next_prompt_frame(&sub, NONE), None);
}

/// A refused prompt mints nothing: a full command queue answers 503 with no id
/// and no frame.
#[test]
fn a_busy_session_mints_no_prompt() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    for _ in 0..MAX_QUEUED_COMMANDS {
        assert!(matches!(
            hub.deliver("s-one", json!({"kind": "interrupt"})),
            ModDelivery::Delivered
        ));
    }
    let ctx = ctx_over(&hub);
    let sub = hub.subscribe();

    let (status, answer) = prompt(&ctx, "s-one", "late");
    assert_eq!(status, 503);
    assert_eq!(answer["error"], "session_busy");
    assert_eq!(
        answer,
        json!({"ok": false, "error": "session_busy", "reason": "the mod's command queue is full; retry after its next poll"})
    );
    assert!(
        answer.get("prompt").is_none(),
        "a busy answer carries an id: {answer}"
    );
    assert_eq!(next_prompt_frame(&sub, NONE), None);
}

/// The mod's outcome report relays as one frame per prompt: delivered, or
/// dropped with its reason. A second report for the same prompt and a report
/// naming an id the daemon never minted relay nothing.
#[test]
fn a_prompt_outcome_relays_once() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let ids: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|text| minted(&prompt(&ctx, "s-one", text).1))
        .collect();
    let sub = hub.subscribe();

    assert_eq!(
        report(
            &hub,
            100,
            2,
            json!({"prompt": ids[0], "state": "delivered"})
        )
        .0,
        200
    );
    assert_eq!(
        report(
            &hub,
            100,
            3,
            json!({"prompt": ids[1], "state": "dropped", "reason": "refused"})
        )
        .0,
        200
    );
    assert_eq!(
        report(
            &hub,
            100,
            4,
            json!({"prompt": ids[2], "state": "dropped", "reason": "removed"})
        )
        .0,
        200
    );
    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(json!({"session": "s-one", "prompt": ids[0], "state": "delivered"}))
    );
    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(
            json!({"session": "s-one", "prompt": ids[1], "state": "dropped", "reason": "refused"})
        )
    );
    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(
            json!({"session": "s-one", "prompt": ids[2], "state": "dropped", "reason": "removed"})
        )
    );

    assert_eq!(
        report(
            &hub,
            100,
            5,
            json!({"prompt": ids[0], "state": "delivered"})
        )
        .0,
        200
    );
    assert_eq!(
        report(
            &hub,
            100,
            6,
            json!({"prompt": ids[0], "state": "dropped", "reason": "removed"})
        )
        .0,
        200
    );
    assert_eq!(
        report(
            &hub,
            100,
            7,
            json!({"prompt": "p-never", "state": "delivered"})
        )
        .0,
        200
    );
    assert_eq!(
        next_prompt_frame(&sub, NONE),
        None,
        "a repeat or unknown report relayed"
    );
}

/// A report the daemon cannot read is refused and relays nothing: a missing id,
/// an unknown state, a drop with no reason or an unknown one, a delivery with a
/// reason, and `session_end`, which only the daemon itself reports.
#[test]
fn a_malformed_prompt_report_is_refused() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let id = minted(&prompt(&ctx, "s-one", "a").1);
    let sub = hub.subscribe();

    for (seq, body) in [
        json!({"state": "delivered"}),
        json!({"prompt": 7, "state": "delivered"}),
        json!({"prompt": id, "state": "queued"}),
        json!({"prompt": id, "state": "lost"}),
        json!({"prompt": id, "state": "dropped"}),
        json!({"prompt": id, "state": "dropped", "reason": "nope"}),
        json!({"prompt": id, "state": "dropped", "reason": "session_end"}),
        json!({"prompt": id, "state": "delivered", "reason": "refused"}),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, answer) = report(&hub, 100, 2, body.clone());
        assert_eq!(
            (status, answer["error"].clone()),
            (400, json!("bad_request")),
            "{seq}: {body}"
        );
    }
    assert_eq!(next_prompt_frame(&sub, NONE), None);
    // A refused report consumed no sequence number: the next good one lands.
    assert_eq!(
        report(&hub, 100, 2, json!({"prompt": id, "state": "delivered"})).0,
        200
    );
    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(json!({"session": "s-one", "prompt": id, "state": "delivered"}))
    );
}

/// A session's end drops every prompt still queued, each relayed as dropped
/// `session_end` before the `end` status, and leaves none in the snapshot.
#[test]
fn a_session_end_drops_every_queued_prompt() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let ids: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|text| minted(&prompt(&ctx, "s-one", text).1))
        .collect();
    assert_eq!(
        report(
            &hub,
            100,
            2,
            json!({"prompt": ids[1], "state": "delivered"})
        )
        .0,
        200
    );
    let sub = hub.subscribe();

    assert_eq!(
        event(
            &hub,
            100,
            json!({"session": "s-one", "seq": 3, "kind": "end"})
        )
        .0,
        200
    );
    let mut frames = Vec::new();
    while let Some(frame) = sub.recv_timeout(NONE) {
        frames.push(frame);
    }
    assert_eq!(
        frames,
        vec![
            (
                "session.prompt".to_string(),
                json!({"session": "s-one", "prompt": ids[0], "state": "dropped", "reason": "session_end"})
            ),
            (
                "session.prompt".to_string(),
                json!({"session": "s-one", "prompt": ids[2], "state": "dropped", "reason": "session_end"})
            ),
            (
                "session.status".to_string(),
                json!({"session": "s-one", "state": "end"})
            ),
        ]
    );
    assert_eq!(hub.snapshot()["prompts"], json!([]));
}

/// The snapshot lists each live session's prompts in the order they were
/// minted: every one still queued, and the 16 most recent outcomes.
#[test]
fn the_snapshot_holds_queued_prompts_and_the_last_16_outcomes() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-two");
    let ctx = ctx_over(&hub);

    let other = minted(&prompt(&ctx, "s-two", "elsewhere").1);
    let mut ids = Vec::new();
    for n in 0..19 {
        ids.push(minted(&prompt(&ctx, "s-one", &format!("p{n}")).1));
    }
    // Prompts 0-16 settle, reported newest-minted first (17 outcomes, so the
    // oldest report falls out); 17 and 18 stay queued.
    for (seq, (n, id)) in (2..).zip(ids.iter().enumerate().take(17).rev()) {
        let body = if n % 2 == 0 {
            json!({"prompt": id, "state": "delivered"})
        } else {
            json!({"prompt": id, "state": "dropped", "reason": "refused"})
        };
        assert_eq!(report(&hub, 100, seq, body).0, 200);
    }

    // The oldest report is prompt 16's, so it is the one that fell out: the
    // 16 kept are prompts 0-15, listed in mint order, not report order.
    let mut expected = Vec::new();
    for (n, id) in ids.iter().enumerate().take(16) {
        expected.push(if n % 2 == 0 {
            json!({"session": "s-one", "prompt": id, "state": "delivered"})
        } else {
            json!({"session": "s-one", "prompt": id, "state": "dropped", "reason": "refused"})
        });
    }
    expected.push(json!({"session": "s-one", "prompt": ids[17], "state": "queued"}));
    expected.push(json!({"session": "s-one", "prompt": ids[18], "state": "queued"}));
    expected.push(json!({"session": "s-two", "prompt": other, "state": "queued"}));
    assert_eq!(hub.snapshot()["prompts"], json!(expected));
}

/// A mod that polls but never reports cannot grow the daemon's prompt state
/// without bound: once [`MAX_QUEUED_COMMANDS`] prompts sit queued, the next is
/// refused busy with no id and no frame, though every command was polled out;
/// one settled report makes room again.
#[test]
fn queued_prompts_are_capped_even_when_polled_out() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let mut first = None;
    for n in 0..MAX_QUEUED_COMMANDS {
        let PromptDelivery::Queued { prompt: Some(id) } = hub.deliver_prompt("s-one", "p") else {
            panic!("prompt {n} was not queued with an id");
        };
        first.get_or_insert(id);
        if n % 512 == 0 {
            let _ = commands(&hub, 100, "s-one");
        }
    }
    let _ = commands(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let sub = hub.subscribe();

    let (status, answer) = prompt(&ctx, "s-one", "one too many");
    assert_eq!(
        (status, answer["error"].clone()),
        (503, json!("session_busy"))
    );
    assert_eq!(
        answer,
        json!({"ok": false, "error": "session_busy", "reason": "the session holds too many prompts that have not run yet; retry once some of them run"})
    );
    assert!(
        answer.get("prompt").is_none(),
        "a busy answer carries an id: {answer}"
    );
    assert_eq!(next_prompt_frame(&sub, NONE), None);
    assert_eq!(
        hub.snapshot()["prompts"].as_array().unwrap().len(),
        MAX_QUEUED_COMMANDS
    );

    let first = first.unwrap();
    assert_eq!(
        report(&hub, 100, 2, json!({"prompt": first, "state": "delivered"})).0,
        200
    );
    let (status, answer) = prompt(&ctx, "s-one", "room again");
    assert_eq!(status, 200, "{answer}");
    minted(&answer);
}

/// The silence close is a removal path like an `end`: a prompt still queued
/// when the sweep closes the instance is reported dropped `session_end`
/// before the `end` status.
#[test]
fn a_silence_close_drops_every_queued_prompt() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let id = minted(&prompt(&ctx, "s-one", "a").1);
    let sub = hub.subscribe();

    hub.advance_clock(SILENCE_LIMIT + Duration::from_secs(1));
    hub.sweep();
    let mut frames = Vec::new();
    while let Some(frame) = sub.recv_timeout(NONE) {
        frames.push(frame);
    }
    assert_eq!(
        frames,
        vec![
            (
                "session.prompt".to_string(),
                json!({"session": "s-one", "prompt": id, "state": "dropped", "reason": "session_end"})
            ),
            (
                "session.status".to_string(),
                json!({"session": "s-one", "state": "end"})
            ),
        ]
    );
}

/// Every code point of JavaScript's whitespace (Unicode `Zs`, TAB, VT, FF, LF,
/// CR, U+FEFF, U+2028, U+2029), alone and before a `/`, is refused as Claude
/// Code refuses it; U+0085 and U+180E, which `trim()` keeps, are not.
#[test]
fn every_javascript_whitespace_code_point_is_trimmed_like_the_engine() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);

    let mut whitespace: Vec<char> = vec![
        '\t', '\u{B}', '\u{C}', '\n', '\r', '\u{FEFF}', '\u{2028}', '\u{2029}', ' ', '\u{A0}',
        '\u{1680}', '\u{202F}', '\u{205F}', '\u{3000}',
    ];
    whitespace.extend('\u{2000}'..='\u{200A}');
    assert_eq!(whitespace.len(), 25);
    for ws in whitespace {
        let blank = ws.to_string();
        assert_eq!(
            prompt(&ctx, "s-one", &blank).1["error"],
            "prompt_blank",
            "U+{:04X} alone",
            u32::from(ws)
        );
        assert_eq!(
            prompt(&ctx, "s-one", &format!("{ws}/x")).1["error"],
            "prompt_command",
            "U+{:04X} before a slash",
            u32::from(ws)
        );
    }
    for kept in ['\u{85}', '\u{180E}'] {
        assert_eq!(
            prompt(&ctx, "s-one", &kept.to_string()).0,
            200,
            "U+{:04X} alone",
            u32::from(kept)
        );
        assert_eq!(
            prompt(&ctx, "s-one", &format!("{kept}/x")).0,
            200,
            "U+{:04X} before a slash",
            u32::from(kept)
        );
    }
}

/// A restarted daemon meets a live mod by its poll first, with no `start` to
/// read: a poll announcing reports turns tracking on for that instance, and a
/// poll that does not leaves it off.
#[test]
fn a_mod_first_met_by_a_reporting_poll_gets_prompt_ids() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    assert_eq!(hub.poll_as(100, "s-one", Duration::ZERO, true).0, 200);
    assert_eq!(hub.poll_as(200, "s-two", Duration::ZERO, false).0, 200);
    let ctx = ctx_over(&hub);

    let (status, answer) = prompt(&ctx, "s-one", "fix it");
    assert_eq!(status, 200, "{answer}");
    minted(&answer);
    assert_eq!(
        prompt(&ctx, "s-two", "fix it"),
        (200, json!({"ok": true, "via": "mod"}))
    );
}

/// A prompt the mod cannot account for (its submit was out when a fresh load
/// of the module found it) is dropped `unknown`: it may have run.
#[test]
fn a_prompt_of_unknown_fate_relays_as_dropped_unknown() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Arc::new(Hub::new(true, resolver));
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let id = minted(&prompt(&ctx, "s-one", "a").1);
    let sub = hub.subscribe();

    assert_eq!(
        report(
            &hub,
            100,
            2,
            json!({"prompt": id, "state": "dropped", "reason": "unknown"})
        )
        .0,
        200
    );
    assert_eq!(
        next_prompt_frame(&sub, WAIT),
        Some(json!({"session": "s-one", "prompt": id, "state": "dropped", "reason": "unknown"}))
    );
    assert_eq!(
        hub.snapshot()["prompts"],
        json!([{"session": "s-one", "prompt": id, "state": "dropped", "reason": "unknown"}])
    );
}

/// An id outlives the daemon run that minted it in a live mod's queue, so a
/// restarted daemon never mints one an earlier run already handed out.
#[test]
fn prompt_ids_differ_across_daemon_runs() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let mut ids = Vec::new();
    for _run in 0..2 {
        let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
        let hub = Arc::new(Hub::new(true, resolver));
        start(&hub, 100, "s-one");
        let ctx = ctx_over(&hub);
        ids.push(minted(&prompt(&ctx, "s-one", "a").1));
    }
    assert_ne!(ids[0], ids[1], "two daemon runs minted the same first id");
}
