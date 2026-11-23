//! The `mcp-await-job` hook's delivered line, driven against the real binary.
//!
//! The print leg lives in `await_job`, which ends the process with the wake
//! exit code, so nothing in-process can observe it: spawning is the only way
//! to see what the model's PostToolUse hook receives. Unix only, for the same
//! reason as `tests/closed_reader.rs`: the child resolves its home through
//! `dirs`, which on Windows reads `FOLDERID_Profile` and no environment
//! variable, so the run could not be pointed away from the operator's real
//! `~/.clauth`.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

/// One done job file under the sandbox home, in the shape the production
/// serializer writes: the record carries the call's `endpoint`, the envelope
/// the child's own output. The hook process is the production reader and
/// renderer, so the line it prints is the delivered artifact itself.
#[test]
fn the_hook_delivers_the_folded_envelopes_prose_with_its_account() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("d-hook-0.json"),
        serde_json::json!({
            "job_id": "d-hook-0",
            "profile": "work",
            "state": "done",
            "started_at": 1,
            "endpoint": "api.deepseek.com",
            "envelope": {
                "profile": "work",
                "is_error": false,
                "result": "ok",
                "total_cost_usd": 2.06,
            },
        })
        .to_string(),
    )
    .unwrap();

    // The host's documented mcp_result shape: the response envelope is
    // JSON-encoded as the content block's text.
    let payload = serde_json::json!({
        "tool_name": "mcp__plugin_clauth_clauth__delegate",
        "tool_response": {
            "type": "mcp_result",
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "job_id": "d-hook-0",
                    "profile": "work",
                    "status": "running",
                })
                .to_string(),
            }],
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["mcp-await-job"])
        .env("HOME", home.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the hook");
    // The taken handle drops at the end of the statement, closing the pipe so
    // the hook's stdin read hits EOF and it proceeds to the wait.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.trim(),
        "delegate to `work` finished: ok (equivalent Anthropic API rate cost: $2.06)",
        "the hook line names the account and carries the same qualified cost \
         clause the collect reply renders: {text}",
    );
    // A synchronous hook run builds its blocking error from stderr alone, so
    // a claimed result on stdout alone would reach nobody.
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        text.trim()
    );
}

/// The lost-job wake, driven against the real binary: a watched job whose
/// server died exits 2 with the lost line on both streams, and the record
/// stays put for `monitor` to answer as it always did.
#[test]
fn the_hook_wakes_once_with_the_lost_line() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("d-dead-0.json"),
        serde_json::json!({
            "job_id": "d-dead-0",
            "profile": "work",
            "state": "running",
            "started_at": now_ms(),
            "owner_pid": 4242,
            "host_session": "sess-1",
        })
        .to_string(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-1",
        "tool_name": "mcp__plugin_clauth_clauth__delegate",
        "tool_response": {
            "type": "mcp_result",
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "job_id": "d-dead-0",
                    "profile": "work",
                    "status": "running",
                })
                .to_string(),
            }],
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["mcp-await-job"])
        .env("HOME", home.path())
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a lost job wakes the model");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.trim(),
        "delegate job `d-dead-0` ended with its server; its result is lost. run the task again.",
        "the wake names the lost job: {text}",
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        text.trim(),
        "exit 2 carries the lost line to both streams"
    );
    assert!(
        jobs.join("d-dead-0.json").exists(),
        "the record is never claimed or deleted"
    );
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn write_job(jobs: &std::path::Path, id: &str, session: &str, state: &str) {
    let tmp = jobs.join(format!("{id}.tmp"));
    std::fs::write(
        &tmp,
        serde_json::json!({
            "job_id": id,
            "profile": "work",
            "state": state,
            "started_at": now_ms(),
            "host_session": session,
            "endpoint": "api.deepseek.com",
            "envelope": {
                "profile": "work",
                "is_error": false,
                "result": "ok",
                "total_cost_usd": 2.06,
            },
        })
        .to_string(),
    )
    .unwrap();
    std::fs::rename(&tmp, jobs.join(format!("{id}.json"))).unwrap();
}

fn turn_end_hook(home: &std::path::Path, payload: serde_json::Value) -> std::process::Child {
    hook_under(home, payload, "cli")
}

fn hook_under(
    home: &std::path::Path,
    payload: serde_json::Value,
    entrypoint: &str,
) -> std::process::Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .args(["mcp-await-job"])
        .env("HOME", home)
        .env("CLAUDE_CODE_ENTRYPOINT", entrypoint)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child
}

/// A turn that ends in an API error fires `StopFailure` and no `Stop`, so the
/// re-arm an idle wake relies on has to hang off both.
#[test]
fn a_failed_turn_end_watches_like_a_stop() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    write_job(&jobs, "d-failed-0", "sess-1", "running");

    let child = turn_end_hook(
        home.path(),
        serde_json::json!({
            "hook_event_name": "StopFailure",
            "session_id": "sess-1",
            "error": "rate_limit",
        }),
    );
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write_job(&jobs, "d-failed-0", "sess-1", "done");

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "delegate to `work` finished: ok (equivalent Anthropic API rate cost: $2.06)",
    );
}

/// `claude -p` runs an `asyncRewake` hook synchronously and sets this
/// entrypoint: a turn end there neither holds the run nor claims a result
/// nobody would read.
#[test]
fn a_print_mode_turn_end_leaves_the_session_and_its_results_alone() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    write_job(&jobs, "d-print-0", "sess-1", "done");
    write_job(&jobs, "d-print-1", "sess-1", "running");

    for entrypoint in ["sdk-cli", "sdk-ts", "sdk-py"] {
        let output = hook_under(
            home.path(),
            serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"}),
            entrypoint,
        )
        .wait_with_output()
        .unwrap();
        assert_eq!(output.status.code(), Some(0), "{entrypoint}");
        assert!(output.stdout.is_empty(), "{entrypoint}");
        assert!(jobs.join("d-print-0.json").exists(), "the result stays");
    }
}

/// A record whose server died is orphaned to the store: a turn end adopts it
/// and wakes once with the lost line, and a later turn end stays silent
/// about it.
#[test]
fn a_turn_end_wakes_once_for_an_orphan() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("d-silent-0.json"),
        serde_json::json!({
            "job_id": "d-silent-0",
            "profile": "work",
            "state": "running",
            "started_at": now_ms(),
            "owner_pid": 4242,
            "host_session": "sess-1",
        })
        .to_string(),
    )
    .unwrap();

    let turn_end = serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"});
    let output = turn_end_hook(home.path(), turn_end.clone())
        .wait_with_output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "the orphan wakes with the lost line"
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "delegate job `d-silent-0` ended with its server; its result is lost. run the task again.",
    );
    assert!(
        jobs.join("d-silent-0.json").exists(),
        "the record stays put for `monitor`"
    );

    let again = turn_end_hook(home.path(), turn_end)
        .wait_with_output()
        .unwrap();
    assert_eq!(
        again.status.code(),
        Some(0),
        "the once keeps the next turn end silent"
    );
    assert!(again.stdout.is_empty());
}

/// A turn end picks up the job a finished watcher left behind: it names no
/// job id, so the session's own running records are what it waits on.
#[test]
fn a_turn_end_delivers_a_running_job_of_its_own_session() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    write_job(&jobs, "d-turn-0", "sess-1", "running");

    let child = turn_end_hook(
        home.path(),
        serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"}),
    );
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write_job(&jobs, "d-turn-0", "sess-1", "done");

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "delegate to `work` finished: ok (equivalent Anthropic API rate cost: $2.06)",
    );
}

/// The hook fires at every turn end of every session, so the common case ends
/// at once and in silence: another session's job and a subagent's turn end are
/// nothing to wait on.
#[test]
fn a_turn_end_with_nothing_of_its_own_running_exits_silently() {
    let home = tempfile::tempdir().unwrap();
    let jobs = home.path().join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    write_job(&jobs, "d-peer-0", "sess-peer", "running");
    write_job(&jobs, "d-own-0", "sess-1", "running");

    for payload in [
        serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-2"}),
        serde_json::json!({
            "hook_event_name": "SubagentStop",
            "session_id": "sess-1",
            "agent_id": "agent-1",
        }),
    ] {
        let output = turn_end_hook(home.path(), payload.clone())
            .wait_with_output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{payload}");
        assert!(output.stdout.is_empty(), "{payload}");
    }
}

const DELIVERED: &str =
    "delegate to `work` finished: ok (equivalent Anthropic API rate cost: $2.06)";

fn jobs_dir(home: &std::path::Path) -> std::path::PathBuf {
    let jobs = home.join(".clauth").join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    jobs
}

/// A lost job beside a delivered result: the lost line prints first, then the
/// delivered line, by equality on the whole output — the lost line never
/// follows the results it warns about.
#[test]
fn the_lost_line_prints_before_the_delivered_result() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    std::fs::write(
        jobs.join("d-done-0.json"),
        serde_json::json!({
            "job_id": "d-done-0",
            "profile": "work",
            "state": "done",
            "started_at": now_ms() - 60_000,
            "done_at": now_ms(),
            "host_session": "sess-1",
            "endpoint": "api.deepseek.com",
            "envelope": {
                "profile": "work",
                "is_error": false,
                "result": "ok",
                "total_cost_usd": 2.06,
            },
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        jobs.join("d-dead-0.json"),
        serde_json::json!({
            "job_id": "d-dead-0",
            "profile": "work",
            "state": "running",
            "started_at": now_ms(),
            "owner_pid": 4242,
            "host_session": "sess-1",
        })
        .to_string(),
    )
    .unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-1",
        "tool_name": "mcp__plugin_clauth_clauth__delegate",
        "tool_response": {
            "type": "mcp_result",
            "content": [
                {
                    "type": "text",
                    "text": serde_json::json!({"job_id": "d-dead-0", "status": "running"}).to_string(),
                },
                {
                    "type": "text",
                    "text": serde_json::json!({"job_id": "d-done-0", "status": "running"}).to_string(),
                },
            ],
        }
    });

    let output = hook_under(home.path(), payload, "cli")
        .wait_with_output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "a lost job wakes the model");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.trim(),
        format!(
            "delegate job `d-dead-0` ended with its server; its result is lost. run the task again.\n{DELIVERED}"
        ),
        "the lost line leads, then the delivered result: {text}",
    );
}

/// A lost job beside a live running job of the same session: the lost line
/// prints first, then the still-working reminder, by equality on the whole
/// output — the order production's line assembly prints, never the test-only
/// joined view.
#[test]
fn the_lost_line_prints_before_the_still_running_reminder() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-live-0", "sess-1", "running");
    std::fs::write(
        jobs.join("d-dead-0.json"),
        serde_json::json!({
            "job_id": "d-dead-0",
            "profile": "work",
            "state": "running",
            "started_at": now_ms(),
            "owner_pid": 4242,
            "host_session": "sess-1",
        })
        .to_string(),
    )
    .unwrap();

    let output = turn_end_hook(
        home.path(),
        serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"}),
    )
    .wait_with_output()
    .unwrap();
    assert_eq!(output.status.code(), Some(2), "a lost job wakes the model");
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.trim(),
        "delegate job `d-dead-0` ended with its server; its result is lost. run the task again.\n\
         delegate job `d-live-0` is still working. no action needed yet; clauth will wake this session when it finishes.",
        "the lost line leads, then the still-working reminder: {text}",
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        text.trim(),
        "exit 2 carries both lines to both streams"
    );
}

/// Blocks until the session's watcher holds its lease: the file exists from
/// inside the hold that takes the flock, so a later fire queues behind it.
fn wait_for_lease(home: &std::path::Path, session: &str) {
    let lease = home
        .join(".clauth")
        .join("conversations")
        .join(format!("{session}.watch"));
    let start = std::time::Instant::now();
    while !lease.exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "no watcher took the lease of {session}",
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn delegate_result(session: &str, job_id: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": session,
        "tool_name": "mcp__plugin_clauth_clauth__delegate",
        "tool_response": {
            "type": "mcp_result",
            "content": [{
                "type": "text",
                "text": serde_json::json!({"job_id": job_id, "status": "running"}).to_string(),
            }],
        }
    })
}

/// An idle wake ends its watcher, so a job finishing inside the wake's own turn
/// has nothing watching it: that turn's end is what delivers the result the
/// wake promised.
#[test]
fn a_turn_end_delivers_a_job_that_finished_with_no_watcher() {
    let home = tempfile::tempdir().unwrap();
    write_job(&jobs_dir(home.path()), "d-done-0", "sess-1", "done");

    let output = turn_end_hook(
        home.path(),
        serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"}),
    )
    .wait_with_output()
    .unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), DELIVERED);
}

/// A `Stop` payload carries the model's own prose, which can quote a job id: the
/// session's records decide what it waits on, never its text.
#[test]
fn a_turn_end_quoting_a_peers_job_leaves_it_alone() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-peer-1", "sess-peer", "done");

    let output = turn_end_hook(
        home.path(),
        serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "sess-1",
            "last_assistant_message": "{\"job_id\": \"d-peer-1\"}",
        }),
    )
    .wait_with_output()
    .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(jobs.join("d-peer-1.json").exists(), "its result stays put");
}

/// Every turn end of a long wait lands on a living watcher: it restarts that
/// watcher's idle count and leaves no second process polling the job.
#[test]
fn a_second_turn_end_leaves_the_wait_to_the_living_watcher() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-long-0", "sess-1", "running");
    let turn_end = serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"});

    let mut watcher = turn_end_hook(home.path(), turn_end.clone());
    wait_for_lease(home.path(), "sess-1");
    let second = turn_end_hook(home.path(), turn_end)
        .wait_with_output()
        .unwrap();
    assert_eq!(second.status.code(), Some(0), "the second fire steps aside");
    assert!(second.stdout.is_empty());
    assert!(
        watcher.try_wait().unwrap().is_none(),
        "the first watcher keeps the wait",
    );

    write_job(&jobs, "d-long-0", "sess-1", "done");
    let output = watcher.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), DELIVERED);
}

/// A flock dies with its holder, so a watcher killed mid-wait leaves a file
/// behind and no claim on it.
#[test]
fn a_killed_watcher_does_not_block_the_next_turn_end() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-orphan-0", "sess-1", "running");
    let turn_end = serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"});

    let mut dead = turn_end_hook(home.path(), turn_end.clone());
    wait_for_lease(home.path(), "sess-1");
    dead.kill().unwrap();
    dead.wait().unwrap();

    let next = turn_end_hook(home.path(), turn_end);
    std::thread::sleep(std::time::Duration::from_millis(500));
    write_job(&jobs, "d-orphan-0", "sess-1", "done");
    let output = next.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), DELIVERED);
}

/// A delegate spawned under a living watcher needs no process of its own: the
/// watcher finds the session's job in the store, and delivers it at its own
/// end whatever else still runs.
#[test]
fn a_second_delegate_hands_its_job_to_the_sessions_watcher() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-first-0", "sess-1", "running");
    write_job(&jobs, "d-second-0", "sess-1", "running");

    let watcher = turn_end_hook(
        home.path(),
        serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"}),
    );
    wait_for_lease(home.path(), "sess-1");
    let handed = turn_end_hook(home.path(), delegate_result("sess-1", "d-second-0"))
        .wait_with_output()
        .unwrap();
    assert_eq!(handed.status.code(), Some(0), "the watcher takes the job");
    assert!(handed.stdout.is_empty());

    write_job(&jobs, "d-second-0", "sess-1", "done");
    let output = watcher.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    let expected = format!(
        "{DELIVERED}\ndelegate job `d-first-0` is still working. no action needed yet; \
         clauth will wake this session when it finishes."
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), expected);
}

/// A record naming no session is invisible to the session's watcher, so the
/// hook holding its id is the only thing that will ever deliver it.
#[test]
fn a_job_no_session_claims_stays_with_the_hook_holding_its_id() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-own-0", "sess-1", "running");
    let untied = |state: &str| {
        let tmp = jobs.join("d-untied-0.tmp");
        std::fs::write(
            &tmp,
            serde_json::json!({
                "job_id": "d-untied-0",
                "profile": "work",
                "state": state,
                "started_at": now_ms(),
                "endpoint": "api.deepseek.com",
                "envelope": {
                    "profile": "work",
                    "is_error": false,
                    "result": "ok",
                    "total_cost_usd": 2.06,
                },
            })
            .to_string(),
        )
        .unwrap();
        std::fs::rename(&tmp, jobs.join("d-untied-0.json")).unwrap();
    };
    untied("running");

    let watcher = turn_end_hook(
        home.path(),
        serde_json::json!({"hook_event_name": "Stop", "session_id": "sess-1"}),
    );
    wait_for_lease(home.path(), "sess-1");
    let mut holder = turn_end_hook(home.path(), delegate_result("sess-1", "d-untied-0"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        holder.try_wait().unwrap().is_none(),
        "the hook keeps the id nothing else can find",
    );

    untied("done");
    let output = holder.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "a delivery wakes the model");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), DELIVERED);

    write_job(&jobs, "d-own-0", "sess-1", "done");
    assert_eq!(
        watcher.wait_with_output().unwrap().status.code(),
        Some(2),
        "the session's watcher still delivers its own job",
    );
}

/// Under a print-mode entrypoint the `PostToolUse` hook is the only watcher
/// its own job has: two overlapping delegates each keep their own id, and both
/// deliveries reach their hooks.
#[test]
fn two_print_mode_delegates_each_wait_on_their_own_job() {
    let home = tempfile::tempdir().unwrap();
    let jobs = jobs_dir(home.path());
    write_job(&jobs, "d-sdk-a", "sess-1", "running");
    write_job(&jobs, "d-sdk-b", "sess-1", "running");

    let mut first = hook_under(home.path(), delegate_result("sess-1", "d-sdk-a"), "sdk-ts");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let second = hook_under(home.path(), delegate_result("sess-1", "d-sdk-b"), "sdk-ts");

    write_job(&jobs, "d-sdk-b", "sess-1", "done");
    let second = second.wait_with_output().unwrap();
    assert_eq!(
        second.status.code(),
        Some(2),
        "the second hook delivers its own job",
    );
    assert_eq!(String::from_utf8(second.stdout).unwrap().trim(), DELIVERED);
    assert!(
        first.try_wait().unwrap().is_none(),
        "the first hook still waits on its own",
    );

    write_job(&jobs, "d-sdk-a", "sess-1", "done");
    let first = first.wait_with_output().unwrap();
    assert_eq!(
        first.status.code(),
        Some(2),
        "the first hook delivers its own job",
    );
    assert_eq!(String::from_utf8(first.stdout).unwrap().trim(), DELIVERED);
}
