#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

//! Pins for the herdr resume-command report leg (`hook_resume`): the trigger
//! (`SessionStart`, a real attribution change, a re-send herdr's refusal
//! armed, nothing else), the report
//! argv, the no-op cases (no pane seat, no attributable profile, a subagent
//! fire, an unchanged account), the durable strictly-increasing seq, and the
//! swallow-don't-fail posture. A shim herdr binary (via `HERDR_BIN_PATH`)
//! appends its argv to a log file, so every assertion reads the real spawned
//! argv. Nothing here touches a live herdr socket.
//!
//! unix-only: the shim is POSIX shell, which Windows cannot execute.

use std::time::Duration;

use super::*;
use crate::hook_note::{Payload, load_record, record_path, store_record};
use crate::testutil::{
    EnvPin, HomeSandbox, echo_shim, exit1_shim, hang_shim, herdr_error_shim, herdr_pane_env,
    refuse_always_shim, refuse_once_shim, report_lines, seq_of, wait_for_lines,
};

/// A main-scope payload carrying only what these tests vary.
fn payload(event: &str, session: &str) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: session.to_string(),
        agent_id: None,
        tool_name: None,
        source: None,
        transcript: None,
    }
}

/// The line one shim report records, with its own seq spliced back in — the
/// seq itself is pinned by the range and monotonicity assertions.
fn expected_line(pane: &str, sid: &str, profile: &str, seq: u64) -> String {
    format!(
        "pane report-agent-session {pane} --source clauth --agent claude \
         --seq {seq} -- clauth resume {sid} --profile {profile}"
    )
}

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_session_start_in_a_herdr_pane_reports_the_resume_argv() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sid123");
    let before = crate::usage::now_ms();
    report_for_fire(&fire, Some("prof"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
    let seq = seq_of(&lines[0]);
    assert!(seq >= before, "seq {seq} must sit on the wall clock");
    assert_eq!(lines[0], expected_line("w1:p1", "sid123", "prof", seq));
}

#[test]
fn an_attribution_change_reports_the_new_profile() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("PostToolUse", "sidmove");
    report_for_fire(&fire, Some("moved-to"), true);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        expected_line("w1:p1", "sidmove", "moved-to", seq_of(&lines[0]))
    );
}

#[test]
fn the_report_argv_is_pinned() {
    let expected: Vec<String> = [
        "pane",
        "report-agent-session",
        "w1:p1",
        "--source",
        "clauth",
        "--agent",
        "claude",
        "--seq",
        "42",
        "--",
        "clauth",
        "resume",
        "sid",
        "--profile",
        "prof",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(build_report_args("w1:p1", "sid", "prof", 42), expected);
}

/// herdr 0.9 stores a `pane report-agent` state from a non-herdr source as the
/// pane's lifecycle authority, above its own screen detection. clauth never
/// reports `idle`, so a `--state working` report pinned the pane at `working`
/// for the rest of its life. The resume command needs no state: herdr records
/// it from a session report while no reporter holds the pane.
#[test]
fn the_report_never_claims_a_lifecycle_state() {
    let args = build_report_args("w1:p1", "sid", "prof", 42);
    assert_eq!(args[1], "report-agent-session");
    let before_resume = &args[..args.iter().position(|a| a == "--").unwrap()];
    assert!(!before_resume.iter().any(|a| a == "--state"), "{args:?}");
    assert!(
        !before_resume.iter().any(|a| a == "--agent-session-id"),
        "{args:?}"
    );
}

#[test]
fn seqs_strictly_increase_and_beat_any_stored_value() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidseq");
    report_for_fire(&fire, Some("prof"), false);
    let first = seq_of(&wait_for_lines(home.home(), 1, WAIT)[0]);
    // A stored seq in the future forces the mint past it: herdr silently keeps
    // the stored argv for any report whose seq is not strictly newer, so the
    // mint must be per-session monotone, not just wall-time-fresh.
    let path = record_path("sidseq", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    let stored: u64 = 1 << 62;
    record.resume_seq = Some(stored);
    store_record(&path, &record).unwrap();
    report_for_fire(&fire, Some("other"), true);
    let lines = wait_for_lines(home.home(), 2, WAIT);
    let second = seq_of(&lines[1]);
    assert!(
        second > first,
        "seqs must strictly increase: {first} then {second}"
    );
    assert_eq!(second, stored + 1, "the mint must beat the stored value");
}

#[test]
fn a_fire_outside_a_herdr_pane_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, None, Some(&shim));
    let fire = payload("SessionStart", "sidnopane");
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn a_pane_id_without_a_resolvable_herdr_bin_reports_nothing() {
    let home = HomeSandbox::new();
    // An empty PATH: the bin gate must not fall through to the operator's
    // real herdr.
    let empty = tempfile::tempdir_in(home.home()).unwrap();
    let _env = EnvPin::new(
        &home,
        &[
            ("HERDR_PANE_ID", Some(std::ffi::OsStr::new("w1:p1"))),
            ("PATH", Some(empty.path().as_os_str())),
        ],
    );
    let fire = payload("SessionStart", "sidnobin");
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn an_unattributed_fire_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidnoacct");
    report_for_fire(&fire, None, false);
    report_for_fire(&fire, None, true);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn an_unchanged_account_tool_call_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("PostToolUse", "sidstay");
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn a_subagent_fire_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let mut fire = payload("SessionStart", "sidsub");
    fire.agent_id = Some("agent-1".to_string());
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn a_failing_herdr_report_is_swallowed() {
    let home = HomeSandbox::new();
    let shim = exit1_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidfail");
    report_for_fire(&fire, Some("prof"), false);
    // The log line proves the spawn was attempted; reaching this assert at
    // all proves the failing status was swallowed.
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
}

/// A `SessionStart` can reach herdr before its detector has seen Claude in the
/// pane; herdr then answers `resume_not_accepted` and records nothing. The
/// hook never waits for detection: the next main-scope fire re-sends, and an
/// accepted re-send disarms it.
#[test]
fn a_refused_report_is_resent_on_the_next_fire_until_accepted() {
    let home = HomeSandbox::new();
    let shim = refuse_once_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    report_for_fire(&payload("SessionStart", "sidrace"), Some("prof"), false);
    assert_eq!(
        report_lines(home.home()).len(),
        1,
        "the refused fire must not wait for herdr's detector"
    );
    let prompt = payload("UserPromptSubmit", "sidrace");
    report_for_fire(&prompt, Some("prof"), false);
    let lines = report_lines(home.home());
    assert_eq!(lines.len(), 2, "{lines:?}");
    let (first, second) = (seq_of(&lines[0]), seq_of(&lines[1]));
    assert!(
        second > first,
        "the re-send mints a newer seq: {first} then {second}"
    );
    assert_eq!(lines[1], expected_line("w1:p1", "sidrace", "prof", second));
    report_for_fire(&prompt, Some("prof"), false);
    assert_eq!(
        report_lines(home.home()).len(),
        2,
        "an accepted re-send disarms the next fire"
    );
}

/// Every fire sends at most once, so a pane where herdr never accepts costs
/// each fire one report and never a wait.
#[test]
fn a_report_herdr_keeps_refusing_is_sent_once_per_fire() {
    let home = HomeSandbox::new();
    let shim = refuse_always_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    report_for_fire(&payload("SessionStart", "sidnever"), Some("prof"), false);
    assert_eq!(report_lines(home.home()).len(), 1);
    let tool = payload("PostToolUse", "sidnever");
    report_for_fire(&tool, Some("prof"), false);
    report_for_fire(&tool, Some("prof"), false);
    assert_eq!(report_lines(home.home()).len(), 3);
}

/// Only herdr's not-yet-accepted refusal arms a re-send: any other failure
/// keeps the logged-silence posture, so a broken herdr is never re-spawned on
/// every tool call.
#[test]
fn a_failure_other_than_a_refusal_arms_no_resend() {
    let home = HomeSandbox::new();
    let shim = exit1_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    report_for_fire(&payload("SessionStart", "sidbroken"), Some("prof"), false);
    report_for_fire(
        &payload("UserPromptSubmit", "sidbroken"),
        Some("prof"),
        false,
    );
    assert_eq!(report_lines(home.home()).len(), 1);
}

/// The arm reads herdr's error code, not the bare fact of an error envelope:
/// a stale pane id answers `pane_not_found` on every call, and re-sending it
/// per tool call would buy nothing.
#[test]
fn another_herdr_error_code_arms_no_resend() {
    let home = HomeSandbox::new();
    let shim = herdr_error_shim(home.home(), "herdr", "pane_not_found");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    report_for_fire(&payload("SessionStart", "sidgone"), Some("prof"), false);
    report_for_fire(&payload("UserPromptSubmit", "sidgone"), Some("prof"), false);
    assert_eq!(report_lines(home.home()).len(), 1);
}

/// An armed re-send waits for a fire that can name the account and is the
/// conversation's own: an unattributed fire and a subagent's fire send
/// nothing and leave it armed.
#[test]
fn an_armed_resend_skips_unattributed_and_subagent_fires() {
    let home = HomeSandbox::new();
    let shim = refuse_once_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    report_for_fire(&payload("SessionStart", "sidwait"), Some("prof"), false);
    let prompt = payload("UserPromptSubmit", "sidwait");
    report_for_fire(&prompt, None, false);
    let mut sub = payload("PostToolUse", "sidwait");
    sub.agent_id = Some("agent-1".to_string());
    report_for_fire(&sub, Some("prof"), false);
    assert_eq!(report_lines(home.home()).len(), 1);
    report_for_fire(&prompt, Some("prof"), false);
    let lines = report_lines(home.home());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(
        lines[1],
        expected_line("w1:p1", "sidwait", "prof", seq_of(&lines[1]))
    );
}

#[test]
fn a_hung_herdr_report_is_killed_within_its_deadline() {
    let home = HomeSandbox::new();
    let shim = hang_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidhang");
    let started = std::time::Instant::now();
    report_for_fire(&fire, Some("prof"), false);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the report must stay bounded, took {:?}",
        started.elapsed()
    );
}
