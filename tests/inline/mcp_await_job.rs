#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The `mcp-await-job` wait, in process: the limits are injected, so a
//! 55 minute count and a day's deadline run in milliseconds. The process-ending
//! half lives in `tests/mcp_await_job_hook.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::hook_jobs::{mark_turn_end, note_turn_start, own_jobs};
use crate::hook_note::Payload;
use crate::mcp::jobs::{hold_server_marker, server_owner_pid, server_started_at};
use crate::testutil::HomeSandbox;

const SESSION: &str = "sess-1";
const MINUTE_MS: u64 = 60 * 1000;

fn payload(event: &str) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: SESSION.to_string(),
        agent_id: None,
        tool_name: None,
        source: None,
        transcript: None,
    }
}

fn write_job(job_id: &str, host_session: Option<&str>, state: &str, owner_pid: u32) {
    write_job_started(
        job_id,
        host_session,
        state,
        owner_pid,
        crate::usage::now_ms(),
    );
}

fn write_job_started(
    job_id: &str,
    host_session: Option<&str>,
    state: &str,
    owner_pid: u32,
    started_at: u64,
) {
    let dir = jobs::jobs_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": "work",
        "state": state,
        "started_at": started_at,
        "owner_pid": owner_pid,
        "envelope": {"profile": "work", "is_error": false, "result": job_id},
    });
    if let Some(host) = host_session {
        record["host_session"] = host.into();
    }
    let tmp = dir.join(format!("{job_id}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::rename(&tmp, dir.join(format!("{job_id}.json"))).unwrap();
}

fn running(job_id: &str) {
    write_job(job_id, Some(SESSION), "running", 0);
}

fn finish(job_id: &str) {
    write_job(job_id, Some(SESSION), "done", 0);
}

/// The hook's cadence shortened; the idle window stays the real one, so a
/// fixture's stamp alone decides whether the count has run out.
fn limits() -> AwaitLimits {
    AwaitLimits {
        deadline: Duration::from_millis(250),
        idle_wake_ms: AWAIT_IDLE_WAKE_SECS * 1000,
        poll: Duration::from_millis(5),
        rescan: Duration::from_millis(5),
    }
}

/// A watcher long enough for a helper thread to finish a job inside it.
fn patient() -> AwaitLimits {
    AwaitLimits {
        deadline: Duration::from_secs(60),
        ..limits()
    }
}

/// The session's watcher, its count started `idle_ms` ago.
fn watcher(held: &[&str], adopted: &[&str], idle_ms: u64) -> AwaitWatch {
    let lease = mark_turn_end(&payload("Stop"), crate::usage::now_ms() - idle_ms)
        .expect("no other watcher holds the session");
    AwaitWatch {
        held: held.iter().map(ToString::to_string).collect(),
        adopted: adopted.iter().map(ToString::to_string).collect(),
        lease: Some(lease),
        session: Some(SESSION.to_string()),
    }
}

fn counted(calls: &AtomicUsize, answer: bool) -> impl Fn() -> bool {
    move || {
        calls.fetch_add(1, Ordering::SeqCst);
        answer
    }
}

fn results(outcome: &AwaitOutcome) -> Vec<&str> {
    outcome
        .delivered
        .iter()
        .map(|envelope| envelope["result"].as_str().unwrap())
        .collect()
}

/// The print-mode gate reads the runner's own `CLAUDE_CODE_ENTRYPOINT`, which
/// the suite must not inherit: the turn-end unit tests pin the interactive
/// spelling and the print-mode test pins its own. The frozen process test
/// covers the same pair across the real env boundary.
fn entrypoint_pin<'a>(home: &'a HomeSandbox, value: &str) -> crate::testutil::EnvPin<'a> {
    crate::testutil::EnvPin::new(
        home,
        &[("CLAUDE_CODE_ENTRYPOINT", Some(std::ffi::OsStr::new(value)))],
    )
}

#[test]
fn an_idle_session_wakes_once_the_count_runs_out() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 55 * MINUTE_MS);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(
        outcome,
        AwaitOutcome {
            delivered: Vec::new(),
            pending: vec!["d-a".to_string()],
            dead: Vec::new(),
            at_limit: false,
        },
    );
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-a` is still working. no action needed yet; \
                 clauth will wake this session when it finishes."
                    .to_string(),
            ),
        ),
    );
}

#[test]
fn a_session_idle_under_the_count_stays_asleep() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 54 * MINUTE_MS);
    let calls = AtomicUsize::new(0);

    let outcome = await_job_outcomes(&watch, &limits(), &counted(&calls, true), &|_| false);

    assert!(outcome.at_limit, "{outcome:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// The stamp a prompt clears is exactly what a running turn looks like to the
/// count, however old it was: a 3 h busy turn never reaches the wake.
#[test]
fn a_user_message_cancels_the_count_until_the_next_turn_end() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 180 * MINUTE_MS);
    note_turn_start(&payload("UserPromptSubmit"));
    let calls = AtomicUsize::new(0);

    let outcome = await_job_outcomes(&watch, &limits(), &counted(&calls, true), &|_| false);

    assert!(outcome.at_limit, "{outcome:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// The settings read runs at the boundary, not at every poll past it, and the
/// job's end still delivers.
#[test]
fn any_other_ttl_gets_no_wake_and_the_wait_runs_on_to_the_jobs_end() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 56 * MINUTE_MS);
    let calls = AtomicUsize::new(0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            finish("d-a");
        });
        await_job_outcomes(&watch, &patient(), &counted(&calls, false), &|_| false)
    });

    assert_eq!(results(&outcome), ["d-a"]);
    assert_eq!(outcome.pending, Vec::<String>::new());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// A turn end landing on a living watcher finds the lease held and leaves only
/// its stamp behind, so the watcher's count is whatever that stamp says now. The
/// stamp moves backwards here because that is the direction a timing-free test
/// can see: a watcher that read it once would wait out the deadline.
#[test]
fn a_turn_end_landing_on_a_living_watcher_moves_its_count() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            let stale = crate::usage::now_ms() - 56 * MINUTE_MS;
            assert!(mark_turn_end(&payload("Stop"), stale).is_none());
        });
        await_job_outcomes(&watch, &patient(), &|| true, &|_| false)
    });

    assert!(!outcome.at_limit, "{outcome:?}");
    assert_eq!(outcome.pending, ["d-a"]);
}

/// A later turn end with a stale stamp of its own is judged afresh, so a TTL
/// change between turns is honoured.
#[test]
fn a_new_turn_end_is_judged_afresh() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 56 * MINUTE_MS);
    let calls = AtomicUsize::new(0);
    let ttl = || {
        let second = calls.fetch_add(1, Ordering::SeqCst) == 1;
        if !second {
            mark_turn_end(&payload("Stop"), crate::usage::now_ms() - 57 * MINUTE_MS);
        }
        second
    };

    let outcome = await_job_outcomes(&watch, &patient(), &ttl, &|_| false);

    assert!(!outcome.at_limit, "{outcome:?}");
    assert_eq!(outcome.pending, ["d-a"]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// One call's fan-out arrives in one wake, and a finished member stays in the
/// store until then, so a long sibling cannot strand its claimed result in a
/// hook's memory.
#[test]
fn a_fan_out_waits_for_its_last_job_with_the_rest_unclaimed() {
    let _home = HomeSandbox::new();
    finish("d-a");
    running("d-b");
    let watch = AwaitWatch {
        held: vec!["d-a".to_string(), "d-b".to_string()],
        adopted: Vec::new(),
        lease: None,
        session: None,
    };

    let outcome = std::thread::scope(|scope| {
        let finisher = scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            let unclaimed = jobs::read("d-a").is_some();
            finish("d-b");
            unclaimed
        });
        let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
        assert!(finisher.join().unwrap(), "d-a was claimed early");
        outcome
    });

    assert_eq!(results(&outcome), ["d-a", "d-b"]);
    assert!(jobs::read("d-a").is_none(), "the claim consumed the record");
}

/// A `monitor` collect inside a long fan-out owns that job: the hook delivers
/// the rest and the collected one reaches the model once.
#[test]
fn a_job_collected_during_the_wait_is_never_delivered_again() {
    let _home = HomeSandbox::new();
    finish("d-a");
    running("d-b");
    let watch = AwaitWatch {
        held: vec!["d-a".to_string(), "d-b".to_string()],
        adopted: Vec::new(),
        lease: None,
        session: None,
    };

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            assert!(matches!(
                jobs::claim("d-a", jobs::Claimant::Monitor),
                jobs::Claim::Owned(_)
            ));
            finish("d-b");
        });
        await_job_outcomes(&watch, &patient(), &|| true, &|_| false)
    });

    assert_eq!(results(&outcome), ["d-b"]);
    assert_eq!(jobs::delivery_ledger("d-a").unwrap().by, "monitor");
    assert_eq!(jobs::delivery_ledger("d-b").unwrap().by, "hook");
}

/// The watched set outlives any one call, so holding a result back for another
/// call's job could cost it a day.
#[test]
fn an_adopted_job_delivers_at_its_own_end_with_the_others_pending() {
    let _home = HomeSandbox::new();
    finish("d-a");
    running("d-b");
    let watch = watcher(&[], &["d-a", "d-b"], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(results(&outcome), ["d-a"]);
    assert_eq!(outcome.pending, ["d-b"]);
    assert!(!outcome.at_limit);
    assert_eq!(
        await_job_wake(&outcome).0,
        2,
        "a delivery wakes the model whatever else runs",
    );
}

#[test]
fn a_watcher_finds_a_job_spawned_after_it_started() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&["d-a"], &[], 0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            finish("d-late");
        });
        await_job_outcomes(&watch, &patient(), &|| true, &|_| false)
    });

    assert_eq!(results(&outcome), ["d-late"]);
    assert_eq!(outcome.pending, ["d-a"]);
}

/// A hook that stepped aside trusts this watcher, and the timed re-list may
/// never come: the watcher's last act under the lease is one more look.
#[test]
fn the_last_job_ending_looks_once_more_before_the_lease_goes() {
    let _home = HomeSandbox::new();
    finish("d-handed");
    let watch = watcher(&["d-gone"], &[], 0);
    let long_rescan = AwaitLimits {
        rescan: Duration::from_secs(3600),
        ..patient()
    };

    let outcome = await_job_outcomes(&watch, &long_rescan, &|| true, &|_| false);

    assert_eq!(results(&outcome), ["d-handed"]);
}

/// A record whose stored id reads back as nothing is listed by the store and
/// gone to every read: the last look must end rather than spin on it.
#[test]
fn a_listed_record_no_read_can_reach_ends_the_wait() {
    let _home = HomeSandbox::new();
    finish("d-mislabelled");
    let dir = jobs::jobs_dir().unwrap();
    let stored = std::fs::read_to_string(dir.join("d-mislabelled.json"))
        .unwrap()
        .replace("d-mislabelled", "d-elsewhere");
    std::fs::write(dir.join("d-mislabelled.json"), stored).unwrap();
    let watch = watcher(&["d-gone"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(await_job_wake(&outcome), (0, None), "{outcome:?}");
}

/// A job whose server died wakes the session once, saying its result is lost:
/// claiming "still working" would promise a wake nothing can deliver, and
/// silence would leave the session waiting on it. A later watcher over the
/// same id says nothing again.
#[test]
fn a_job_whose_server_died_wakes_once_saying_its_result_is_lost() {
    let _home = HomeSandbox::new();
    write_job("d-dead", Some(SESSION), "running", 4242);
    let watch = watcher(&["d-dead"], &[], 180 * MINUTE_MS);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-dead` ended with its server; its result is lost. run the task again."
                    .to_string()
            )
        ),
        "{outcome:?}"
    );

    let again = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(await_job_wake(&again), (0, None), "{again:?}");
}

/// A job orphaned while no watcher held the session: the next turn end adopts
/// it and wakes with the lost line once — the reported-lost set keeps every
/// later turn end silent about it.
#[test]
fn an_orphan_that_predates_the_watcher_gets_the_lost_line_once() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "cli");
    write_job("d-dead", Some(SESSION), "running", 4242);

    let watch = turn_end_watch(Some(&payload("Stop"))).expect("the orphan arms the watcher");
    assert_eq!(watch.adopted, ["d-dead"], "the orphan is adopted");
    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-dead` ended with its server; its result is lost. run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );

    let again = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(await_job_wake(&again), (0, None), "{again:?}");
}

/// Several lost jobs in one wake: one line, plural, like the limit line.
#[test]
fn several_lost_jobs_share_one_plural_line() {
    let _home = HomeSandbox::new();
    write_job("d-a", Some(SESSION), "running", 4242);
    write_job("d-b", Some(SESSION), "running", 4242);
    let watch = watcher(&["d-a", "d-b"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate jobs `d-a`, `d-b` ended with their server; their results are lost. \
                 resume each with `delegate` and its session id from `monitor`, or run them again."
                    .to_string(),
            ),
        ),
    );
}

/// A lost job beside a delivered result: one wake carries both — the folded
/// result and the lost line — and the lost job's record stays untouched for
/// `monitor` to answer exactly as it always did.
#[test]
fn a_lost_job_beside_a_delivered_result_shares_one_wake() {
    let _home = HomeSandbox::new();
    finish("d-done");
    write_job("d-dead", Some(SESSION), "running", 4242);
    let watch = watcher(&["d-dead"], &["d-done"], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(results(&outcome), ["d-done"], "the finished job delivers");
    assert_eq!(outcome.pending, Vec::<String>::new());
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-dead` ended with its server; its result is lost. run the task again."
                    .to_string(),
            ),
        ),
    );
    assert!(
        jobs::read("d-dead").is_some(),
        "the lost record is never claimed or deleted"
    );
}

/// A lost job tied to a different session is that session's to report: this
/// watcher drops it silently, so one conversation's wake never claims
/// another's.
#[test]
fn a_lost_job_tied_to_another_session_is_not_this_sessions_to_report() {
    let _home = HomeSandbox::new();
    write_job("d-other", Some("sess-peer"), "running", 4242);
    let watch = watcher(&["d-other"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(await_job_wake(&outcome), (0, None), "{outcome:?}");
    assert_eq!(outcome.dead, Vec::<LostJob>::new());
}

/// A `crashed` done record is an orphan too: the lost line, never a claim
/// that could not carry a result.
#[test]
fn a_crashed_done_record_wakes_with_the_lost_line() {
    let _home = HomeSandbox::new();
    let dir = jobs::jobs_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    std::fs::write(
        dir.join("d-crash.json"),
        serde_json::json!({
            "job_id": "d-crash",
            "profile": "work",
            "state": "done",
            "started_at": crate::usage::now_ms() - 2_000,
            "done_at": crate::usage::now_ms() - 1_000,
            "crashed": true,
            "host_session": SESSION,
        })
        .to_string(),
    )
    .unwrap();
    let watch = watcher(&["d-crash"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-crash` ended with its server; its result is lost. run the task again."
                    .to_string(),
            ),
        ),
    );
    assert!(jobs::read("d-crash").is_some(), "the tombstone stays put");
}

/// A watcher holding a job that turns into an orphan — a running record
/// silent past the store's corpse window, its server gone — wakes once with
/// the lost line, then the still-running reminder for the live rest, joined
/// by one space: the still-live job stays in the store for the next turn end.
/// A later turn end adopts the orphan too, and the reported-lost set keeps
/// that adoption from waking again.
#[test]
fn an_orphaned_job_wakes_with_the_lost_line_beside_the_still_running_rest() {
    let _home = HomeSandbox::new();
    running("d-a");
    running("d-b");
    let watch = watcher(&[], &["d-a", "d-b"], 0);

    let start = Instant::now();
    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            // A second older than `d-b`, so the store's newest-first listing
            // keeps the pinned order.
            write_job_started(
                "d-a",
                Some(SESSION),
                "running",
                4242,
                crate::usage::now_ms() - 1_000,
            );
        });
        await_job_outcomes(&watch, &patient(), &|| true, &|_| false)
    });
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "the orphan return ended the wait, not the deadline: {outcome:?}",
    );

    assert!(!outcome.at_limit, "{outcome:?}");
    assert_eq!(outcome.pending, ["d-b"], "the live job is still pending");
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-a` ended with its server; its result is lost. run the task again. \
                 delegate job `d-b` is still working. no action needed yet; clauth will wake this session when it finishes."
                    .to_string(),
            ),
        ),
    );
    assert!(
        jobs::read("d-b").is_some(),
        "the live job stays in the store, unclaimed"
    );
    assert_eq!(
        own_jobs(SESSION, crate::usage::now_ms()),
        ["d-b", "d-a"],
        "a later turn end adopts the orphan too; the reported set keeps it silent"
    );
}

/// The await hook's stdin read: capped at the payload ceiling and parsed once,
/// the raw value feeding the job-id scan and the typed payload everything else,
/// with `parse_payload`'s validation intact — a session id the gate refuses
/// still refuses.
#[test]
fn the_await_payload_read_is_capped_and_parsed_once() {
    let _home = HomeSandbox::new();
    let stop = r#"{"hook_event_name":"Stop","session_id":"sess-1"}"#;
    let (value, payload) = read_await_payload(&mut stop.as_bytes());
    assert!(value.is_some());
    let parsed = payload.expect("a valid payload parses");
    assert_eq!(parsed.event, "Stop");
    assert_eq!(parsed.session_id, "sess-1");
    assert_eq!(parsed.agent_id, None);

    let subagent = r#"{"hook_event_name":"Stop","session_id":"sess-1","agent_id":"agent-1"}"#;
    let (_, payload) = read_await_payload(&mut subagent.as_bytes());
    assert_eq!(
        payload.expect("parses").agent_id.as_deref(),
        Some("agent-1"),
    );

    let bad_session = r#"{"hook_event_name":"Stop","session_id":"a.b"}"#;
    let (value, payload) = read_await_payload(&mut bad_session.as_bytes());
    assert!(value.is_some(), "the raw value still parses");
    assert!(
        payload.is_none(),
        "the split parse keeps the session-id gate",
    );

    let over = format!(
        r#"{{"hook_event_name":"Stop","session_id":"sess-1","pad":"{}"}}"#,
        "x".repeat(crate::hook_note::MAX_PAYLOAD_BYTES as usize + 1)
    );
    let (value, payload) = read_await_payload(&mut over.as_bytes());
    assert!(value.is_none(), "the cap truncates the closing brace");
    assert!(payload.is_none());
}

/// No turn end will ever find a job naming no session, so nothing may end the
/// one hook holding its id: neither the idle count nor another job's delivery.
#[test]
fn an_untied_job_keeps_its_hook_past_the_count_and_a_delivery() {
    let _home = HomeSandbox::new();
    write_job("d-untied", None, "running", 0);
    finish("d-a");
    let watch = watcher(&["d-untied"], &["d-a"], 56 * MINUTE_MS);
    let calls = AtomicUsize::new(0);

    let outcome = await_job_outcomes(&watch, &limits(), &counted(&calls, true), &|_| false);

    assert!(outcome.at_limit, "{outcome:?}");
    assert_eq!(outcome.pending, ["d-untied"]);
    assert_eq!(results(&outcome), ["d-a"]);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_hook_with_no_lease_never_wakes_an_idle_session() {
    let _home = HomeSandbox::new();
    write_job("d-a", Some(SESSION), "running", 0);
    drop(watcher(&[], &[], 56 * MINUTE_MS));
    let watch = AwaitWatch {
        held: vec!["d-a".to_string()],
        adopted: Vec::new(),
        lease: None,
        session: None,
    };
    let calls = AtomicUsize::new(0);

    let outcome = await_job_outcomes(&watch, &limits(), &counted(&calls, true), &|_| false);

    assert!(outcome.at_limit, "{outcome:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn the_limit_line_alone_asks_for_monitor() {
    let limit = |pending: &[&str], delivered: Vec<serde_json::Value>| AwaitOutcome {
        delivered,
        pending: pending.iter().map(ToString::to_string).collect(),
        dead: Vec::new(),
        at_limit: true,
    };
    assert_eq!(
        await_job_wake(&limit(&["d-a"], Vec::new())),
        (
            2,
            Some(
                "24 hours have passed and delegate job `d-a` still running; call `monitor` to retrieve it — clauth will not wake this session when it finishes."
                    .to_string(),
            ),
        ),
    );
    assert_eq!(
        await_job_wake(&limit(&["d-a", "d-b"], vec![serde_json::json!({})])),
        (
            2,
            Some(
                "24 hours have passed and delegate jobs `d-a`, `d-b` still running; call `monitor` to retrieve them — clauth will not wake this session when they finish."
                    .to_string(),
            ),
        ),
    );
}

/// The wait ending on its limit, end to end: the deadline is what stops it,
/// and it hands the pending id to the line above.
#[test]
fn a_job_outliving_the_deadline_reaches_the_limit() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = AwaitWatch {
        held: vec!["d-a".to_string()],
        adopted: Vec::new(),
        lease: None,
        session: None,
    };

    let outcome = await_job_outcomes(&watch, &limits(), &|| true, &|_| false);

    assert_eq!(
        outcome,
        AwaitOutcome {
            delivered: Vec::new(),
            pending: vec!["d-a".to_string()],
            dead: Vec::new(),
            at_limit: true,
        },
    );
}

#[test]
fn a_subagents_turn_end_starts_no_watcher() {
    let _home = HomeSandbox::new();
    let _env = entrypoint_pin(&_home, "cli");
    running("d-a");
    let subagent = Payload {
        agent_id: Some("agent-1".to_string()),
        ..payload("Stop")
    };

    assert!(turn_end_watch(Some(&subagent)).is_none());
    assert_eq!(crate::hook_jobs::turn_ended_at(SESSION), None);
    assert!(turn_end_watch(Some(&payload("SubagentStop"))).is_none());
    assert_eq!(crate::hook_jobs::turn_ended_at(SESSION), None);
}

#[test]
fn a_quiet_turn_end_writes_nothing() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "cli");
    write_job("d-peer", Some("sess-peer"), "running", 0);

    assert!(turn_end_watch(Some(&payload("Stop"))).is_none());

    assert!(
        !home.home().join(".clauth").join("conversations").exists(),
        "the common turn end reads the store and nothing else",
    );
}

/// The print-mode gate fires before any store read or write: a `Stop` under an
/// `sdk-` entrypoint starts no watcher, stamps nothing and takes no lease, so
/// the `claude -p` run is neither held nor robbed of a result. The interactive
/// half of the pair is the test above this one.
#[test]
fn a_print_mode_turn_end_starts_no_watcher_and_stamps_nothing() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "sdk-cli");
    running("d-a");

    assert!(turn_end_watch(Some(&payload("Stop"))).is_none());
    assert_eq!(crate::hook_jobs::turn_ended_at(SESSION), None, "no stamp");
    assert!(
        !home.home().join(".clauth").join("conversations").exists(),
        "no lease, no record",
    );
}

#[test]
fn a_turn_end_with_work_out_starts_the_count_and_holds_the_session() {
    let _home = HomeSandbox::new();
    let _env = entrypoint_pin(&_home, "cli");
    running("d-a");
    let before = crate::usage::now_ms();

    let watch = turn_end_watch(Some(&payload("Stop"))).expect("a watcher");

    assert_eq!(watch.adopted, ["d-a"]);
    assert!(crate::hook_jobs::turn_ended_at(SESSION).is_some_and(|at| at >= before));
    assert!(
        turn_end_watch(Some(&payload("Stop"))).is_none(),
        "the second fire leaves the wait to the first",
    );
}

#[test]
fn a_delegate_result_inside_a_turn_stops_a_stale_count() {
    let _home = HomeSandbox::new();
    let _env = entrypoint_pin(&_home, "cli");
    running("d-a");
    drop(watcher(&[], &[], 56 * MINUTE_MS));
    let result = serde_json::json!({"tool_response": {"job_id": "d-a"}});

    let watch = tool_result_watch(Some(&result), Some(&payload("PostToolUse")));

    assert_eq!(watch.expect("a watcher").held, ["d-a"]);
    assert_eq!(crate::hook_jobs::turn_ended_at(SESSION), None);
}

#[test]
fn a_subagents_delegate_result_leaves_the_count_running() {
    let _home = HomeSandbox::new();
    running("d-a");
    let stamped = crate::usage::now_ms() - 10 * MINUTE_MS;
    drop(mark_turn_end(&payload("Stop"), stamped));
    let result = serde_json::json!({"tool_response": {"job_id": "d-a"}});
    let subagent = Payload {
        agent_id: Some("agent-1".to_string()),
        ..payload("PostToolUse")
    };

    assert!(tool_result_watch(Some(&result), Some(&subagent)).is_some());
    assert_eq!(crate::hook_jobs::turn_ended_at(SESSION), Some(stamped));
}

#[test]
fn a_payload_naming_no_session_gets_the_wait_alone() {
    let _home = HomeSandbox::new();
    running("d-a");
    let result = serde_json::json!({"tool_response": {"job_id": "d-a"}});

    let watch = tool_result_watch(Some(&result), None).expect("a watch");

    assert_eq!(watch.held, ["d-a"]);
    assert!(watch.lease.is_none());
}

/// The turn end an API error leaves behind (`StopFailure`, where the host
/// fires no `Stop`) arms the watcher exactly like a `Stop` does.
#[test]
fn a_failed_turn_end_starts_a_watcher_like_a_stop() {
    let _home = HomeSandbox::new();
    let _env = entrypoint_pin(&_home, "cli");
    running("d-a");
    let failed = Payload {
        event: "StopFailure".to_string(),
        ..payload("Stop")
    };

    let watch = turn_end_watch(Some(&failed)).expect("a watcher");

    assert_eq!(watch.adopted, ["d-a"]);
}

#[test]
fn a_print_mode_stop_failure_starts_no_watcher_like_a_stop() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "sdk-cli");
    running("d-a");
    let failed = Payload {
        event: "StopFailure".to_string(),
        ..payload("Stop")
    };

    assert!(turn_end_watch(Some(&failed)).is_none());
    assert_eq!(crate::hook_jobs::turn_ended_at(SESSION), None, "no stamp");
}

/// A print-mode `PostToolUse` takes no lease: the session has no turn-end
/// watcher to hand the job to, so the hook keeps every id it was handed, and
/// two overlapping delegates each wait on their own.
#[test]
fn a_print_mode_tool_result_keeps_its_own_job_and_takes_no_lease() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "sdk-cli");
    running("d-a");
    let result = serde_json::json!({"tool_response": {"job_id": "d-a"}});

    let watch = tool_result_watch(Some(&result), Some(&payload("PostToolUse"))).expect("a watch");

    assert_eq!(watch.held, ["d-a"], "the hook keeps its own job");
    assert!(watch.lease.is_none(), "no lease under print mode");
}

/// Print mode takes no lease, so the once keys on the payload's session id: a
/// tied orphan wakes once, and a later hook of the same session stays silent.
#[test]
fn a_print_mode_tied_orphan_wakes_once() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "sdk-ts");
    write_job("d-dead", Some(SESSION), "running", 4242);
    let result = serde_json::json!({"tool_response": {"job_id": "d-dead"}});
    let watch = tool_result_watch(Some(&result), Some(&payload("PostToolUse"))).expect("a watch");

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-dead` ended with its server; its result is lost. run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );

    let later = tool_result_watch(Some(&result), Some(&payload("PostToolUse"))).expect("a watch");
    let again = await_job_outcomes(&later, &patient(), &|| true, &|_| false);
    assert_eq!(await_job_wake(&again), (0, None), "{again:?}");
}

/// The same print-mode once for an untied orphan: the payload's session keys
/// the record, so the second call of the same session stays silent.
#[test]
fn a_print_mode_untied_orphan_wakes_once() {
    let home = HomeSandbox::new();
    let _env = entrypoint_pin(&home, "sdk-ts");
    write_job("d-dead", None, "running", 4242);
    let result = serde_json::json!({"tool_response": {"job_id": "d-dead"}});
    let watch = tool_result_watch(Some(&result), Some(&payload("PostToolUse"))).expect("a watch");

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-dead` ended with its server; its result is lost. run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );

    let later = tool_result_watch(Some(&result), Some(&payload("PostToolUse"))).expect("a watch");
    let again = await_job_outcomes(&later, &patient(), &|| true, &|_| false);
    assert_eq!(await_job_wake(&again), (0, None), "{again:?}");
}

/// One orphaned running record with the fields these lost-copy tests vary:
/// the record's own session id (the resume handle) and its last-seen stamp,
/// which the wake's age clause reads. The owner pid names a server that is
/// gone, so the record reads as a corpse at the next poll.
fn write_lost_record(
    job_id: &str,
    host_session: Option<&str>,
    session_id: Option<&str>,
    started_at: u64,
) {
    let dir = jobs::jobs_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    let mut record = serde_json::json!({
        "job_id": job_id,
        "profile": "work",
        "state": "running",
        "started_at": started_at,
        "owner_pid": 4242,
    });
    if let Some(host) = host_session {
        record["host_session"] = host.into();
    }
    if let Some(sid) = session_id {
        record["session_id"] = sid.into();
    }
    std::fs::write(
        dir.join(format!("{job_id}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
}

/// A lost job whose record carries the delegate's own session id names the
/// resume handle — `delegate({session_id})` reaches the transcript — and a
/// wake within five minutes of the record's last move names no age.
#[test]
fn a_lost_job_whose_record_names_its_session_offers_the_resume() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-sid",
        Some(SESSION),
        Some("d-sid-1"),
        crate::usage::now_ms(),
    );
    let watch = watcher(&["d-sid"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-sid` ended with its server; its result is lost. \
                 resume it with `delegate` `session_id: d-sid-1`, or run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}

/// A lost job found after the fact — its record last moved hours before the
/// wake — names how long ago, in the listing's "last seen" spelling, then the
/// same resume sentence.
#[test]
fn a_lost_job_found_late_names_how_long_ago() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-late",
        Some(SESSION),
        Some("d-late-1"),
        crate::usage::now_ms() - 130 * MINUTE_MS,
    );
    let watch = watcher(&["d-late"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-late` ended with its server 2h 10m ago; its result is lost. \
                 resume it with `delegate` `session_id: d-late-1`, or run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}

/// A lost job whose run was isolated carries a session id no `delegate`
/// resume accepts (its transcript left with the throwaway tree), so its wake
/// is the no-session-id line: it never offers a handle `delegate` refuses.
#[test]
fn a_lost_isolated_job_offers_no_resume() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-iso",
        Some(SESSION),
        Some("d-iso-1"),
        crate::usage::now_ms(),
    );
    let path = jobs::jobs_dir().unwrap().join("d-iso.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["isolated"] = true.into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let watch = watcher(&["d-iso"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-iso` ended with its server; its result is lost. \
                 run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}

/// An isolated record found late: the age clause still names when, and the
/// resume sentence is the no-session-id one — an isolated run's session id is
/// no handle `delegate` accepts, however old the record.
#[test]
fn a_lost_isolated_job_found_late_offers_no_resume() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-iso-late",
        Some(SESSION),
        Some("d-iso-late-1"),
        crate::usage::now_ms() - 130 * MINUTE_MS,
    );
    let path = jobs::jobs_dir().unwrap().join("d-iso-late.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["isolated"] = true.into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let watch = watcher(&["d-iso-late"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-iso-late` ended with its server 2h 10m ago; its result is lost. \
                 run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}

/// A live-owner job silent past the store's corpse window is hung, not lost:
/// the wait ends with the `monitor` limit line naming it alone — the silent
/// guard, not the orphan wake — and the live rest stays for the next turn
/// end.
#[test]
fn a_live_owner_job_silent_past_the_ttl_ends_the_wait_naming_only_it() {
    let _home = HomeSandbox::new();
    let _marker = hold_server_marker().expect("hold the marker");
    let dir = jobs::jobs_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    std::fs::write(
        dir.join("d-hung.json"),
        serde_json::json!({
            "job_id": "d-hung",
            "profile": "work",
            "state": "running",
            "started_at": crate::usage::now_ms() - jobs::RUNNING_TTL_MS - 60_000,
            "owner_pid": server_owner_pid(),
            "owner_started_at": server_started_at(),
            "host_session": SESSION,
        })
        .to_string(),
    )
    .unwrap();
    running("d-b");
    let watch = watcher(&["d-hung"], &["d-b"], 0);

    let outcome = await_job_outcomes(&watch, &limits(), &|| true, &|_| false);

    assert_eq!(
        outcome.pending,
        ["d-hung"],
        "the limit names the hung job alone: {outcome:?}"
    );
    assert_eq!(outcome.dead, Vec::<LostJob>::new(), "{outcome:?}");
    assert!(outcome.at_limit, "{outcome:?}");
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "24 hours have passed and delegate job `d-hung` still running; call `monitor` to retrieve it — clauth will not wake this session when it finishes."
                    .to_string(),
            ),
        ),
    );
}

/// The session-move judge, counted so the skip is proven to go through it.
fn moved(calls: &AtomicUsize, answer: bool) -> impl Fn(&str) -> bool + '_ {
    move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        answer
    }
}

/// `/clear` moves the session id while the watcher lives: the old id's count
/// may run out, but the host's current id no longer matches, so the wake is
/// skipped and the job's own end still delivers.
#[test]
fn a_moved_session_gets_no_idle_wake_and_delivers_at_the_jobs_end() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 56 * MINUTE_MS);
    let calls = AtomicUsize::new(0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            finish("d-a");
        });
        await_job_outcomes(&watch, &patient(), &|| true, &moved(&calls, true))
    });

    assert_eq!(results(&outcome), ["d-a"], "the job's own end delivers");
    assert_eq!(outcome.pending, Vec::<String>::new());
    assert!(!outcome.at_limit, "{outcome:?}");
    assert!(calls.load(Ordering::SeqCst) >= 1, "the move was judged");
}

/// A moved id has no later turn end to find a job this hook leaves behind, so
/// the first job's end does not end the wait while another still runs.
#[test]
fn a_moved_session_keeps_the_wait_until_its_last_job_ends() {
    let _home = HomeSandbox::new();
    running("d-a");
    running("d-b");
    let watch = watcher(&[], &["d-a", "d-b"], 0);
    let calls = AtomicUsize::new(0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            finish("d-a");
            std::thread::sleep(Duration::from_millis(600));
            finish("d-b");
        });
        await_job_outcomes(&watch, &patient(), &|| true, &moved(&calls, true))
    });

    assert_eq!(results(&outcome), ["d-a", "d-b"], "both reach the session");
    assert_eq!(outcome.pending, Vec::<String>::new(), "nothing is promised");
    assert!(!outcome.at_limit, "{outcome:?}");
}

/// With the session id moved, the idle wake never fires whatever the TTL: the
/// wait runs on to the deadline instead of waking a session with nothing of
/// its own.
#[test]
fn a_moved_session_runs_the_wait_to_the_deadline_without_a_wake() {
    let _home = HomeSandbox::new();
    running("d-a");
    let watch = watcher(&[], &["d-a"], 56 * MINUTE_MS);

    let outcome = await_job_outcomes(&watch, &limits(), &|| true, &|_| true);

    assert!(
        outcome.at_limit,
        "the deadline, not the wake, ended it: {outcome:?}",
    );
    assert_eq!(outcome.pending, ["d-a"]);
}

/// A hook holding an untied job is the only thing that will ever deliver it,
/// so a tied job it adopted turning orphaned must not end its wait: the
/// untied job's own end still delivers, and the one wake then carries the
/// lost line beside the delivery.
#[test]
fn an_orphaned_tied_job_does_not_strand_the_hook_holding_an_untied_one() {
    let _home = HomeSandbox::new();
    write_job("d-u", None, "running", 0);
    running("d-t");
    let watch = watcher(&["d-u"], &["d-t"], 0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            write_job("d-t", Some(SESSION), "running", 4242);
            std::thread::sleep(Duration::from_millis(60));
            write_job("d-u", None, "done", 0);
        });
        await_job_outcomes(&watch, &patient(), &|| true, &|_| false)
    });

    assert_eq!(results(&outcome), ["d-u"], "the untied job delivers");
    assert!(!outcome.at_limit, "{outcome:?}");
    assert_eq!(outcome.pending, Vec::<String>::new());
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-t` ended with its server; its result is lost. run the task again."
                    .to_string(),
            ),
        ),
    );
}

/// At the deadline the wake names the lost tied job with the lost line and
/// the untied rest with the limit line — never the lost one as still running
/// — since nothing else will ever print the untied one again.
#[test]
fn the_deadline_names_the_untied_rest_beside_the_lost_job() {
    let _home = HomeSandbox::new();
    write_job("d-u", None, "running", 0);
    running("d-t");
    let watch = watcher(&["d-u"], &["d-t"], 0);

    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(60));
            write_job("d-t", Some(SESSION), "running", 4242);
        });
        await_job_outcomes(&watch, &limits(), &|| true, &|_| false)
    });

    assert!(outcome.at_limit, "{outcome:?}");
    assert_eq!(
        outcome.pending,
        ["d-u"],
        "the untied rest, never the lost job"
    );
    assert_eq!(
        await_job_wake(&outcome).1.as_deref(),
        Some(
            "delegate job `d-t` ended with its server; its result is lost. run the task again. \
             24 hours have passed and delegate job `d-u` still running; call `monitor` to retrieve it — clauth will not wake this session when it finishes."
        ),
    );
}

/// The five-minute boundary, from the inside: a record last seen exactly five
/// minutes before the wake names no age. The wake runs within a fraction of a
/// second of the fixture's stamp, inside the second that would otherwise carry
/// the age to five minutes and one.
#[test]
fn a_lost_job_on_the_five_minute_edge_names_no_age() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-edge",
        Some(SESSION),
        Some("d-edge-1"),
        crate::usage::now_ms() - 5 * MINUTE_MS,
    );
    let watch = watcher(&["d-edge"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-edge` ended with its server; its result is lost. \
                 resume it with `delegate` `session_id: d-edge-1`, or run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}

/// A record last seen a second and a half past five minutes names the age, in
/// the listing's duration spelling: the threshold sits at five minutes, not
/// at the job store's day.
#[test]
fn a_lost_job_past_the_five_minute_edge_names_the_age() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-past",
        Some(SESSION),
        Some("d-past-1"),
        crate::usage::now_ms() - 5 * MINUTE_MS - 1_500,
    );
    let watch = watcher(&["d-past"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);
    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate job `d-past` ended with its server 5m ago; its result is lost. \
                 resume it with `delegate` `session_id: d-past-1`, or run the task again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}

/// The plural line names no ages, however old the records: only the single
/// form says how long ago. Two records last seen 130 minutes before the wake
/// still fill exactly the plural sentence, with no age clause anywhere in it.
#[test]
fn several_old_lost_jobs_share_one_plural_line_with_no_ages() {
    let _home = HomeSandbox::new();
    write_lost_record(
        "d-old-a",
        Some(SESSION),
        Some("d-old-a-1"),
        crate::usage::now_ms() - 130 * MINUTE_MS,
    );
    write_lost_record(
        "d-old-b",
        Some(SESSION),
        Some("d-old-b-1"),
        crate::usage::now_ms() - 130 * MINUTE_MS,
    );
    let watch = watcher(&["d-old-a", "d-old-b"], &[], 0);

    let outcome = await_job_outcomes(&watch, &patient(), &|| true, &|_| false);

    assert_eq!(
        await_job_wake(&outcome),
        (
            2,
            Some(
                "delegate jobs `d-old-a`, `d-old-b` ended with their server; their results are lost. \
                 resume each with `delegate` and its session id from `monitor`, or run them again."
                    .to_string(),
            ),
        ),
        "{outcome:?}"
    );
}
