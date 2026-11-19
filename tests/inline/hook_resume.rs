#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

//! Pins for the herdr resume-command report leg (`hook_resume`): the trigger
//! (`SessionStart`, a real attribution change, a re-send herdr's refusal
//! armed, nothing else), the report
//! argv, the no-op cases (no pane seat, no attributable profile, a subagent
//! fire, an unchanged account), the durable strictly-increasing seq, and the
//! swallow-don't-fail posture. The report argv keeps the session's launch
//! mode: a bare `claude` restores as `clauth __restore`, a pinned
//! `clauth start` as `clauth resume --profile`, a chain session with
//! `--with-fallback`. A shim herdr binary (via `HERDR_BIN_PATH`)
//! appends its argv to a log file, so every assertion reads the real spawned
//! argv. Nothing here touches a live herdr socket.
//!
//! unix-only: the shim is POSIX shell, which Windows cannot execute.

use std::time::Duration;

use super::*;
use crate::hook_note::{Payload, load_record, record_path, store_record};
use crate::testutil::{
    ConfigDirSandbox, EnvPin, HomeSandbox, echo_shim, exit1_shim, hang_shim, herdr_error_shim,
    herdr_pane_env, live_row, refuse_always_shim, refuse_once_shim, report_lines, seq_of,
    wait_for_lines,
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

/// The line one shim report records for a pinned `clauth start` session, with
/// its own seq spliced back in — the seq itself is pinned by the range and
/// monotonicity assertions. Built through the product's own builder, so a
/// builder that drops a word reds here as well as in the literal argv pins.
fn expected_line(pane: &str, sid: &str, profile: &str, seq: u64) -> String {
    mode_line(pane, sid, &RestoreMode::Pinned(profile.to_string()), seq)
}

fn mode_line(pane: &str, sid: &str, mode: &RestoreMode, seq: u64) -> String {
    build_report_args(pane, sid, mode, seq).join(" ")
}

/// A per-session runtime dir for clauth sid `runtime_sid`, as `clauth start`
/// builds one, with `CLAUDE_CONFIG_DIR` pinned at it — the hook child of a
/// `clauth start` session sees exactly this. No registry row is written here:
/// each caller decides whether one exists and what it says.
fn runtime_config_dir<'a>(home: &'a HomeSandbox, runtime_sid: &str) -> ConfigDirSandbox<'a> {
    let dir = home
        .home()
        .join(".clauth/profiles/prof")
        .join(format!("runtime-{runtime_sid}"));
    std::fs::create_dir_all(&dir).unwrap();
    ConfigDirSandbox::new(home, &dir)
}

/// The pinned-session context most fire tests run in: a runtime dir whose
/// registry row does not follow the chain.
fn pinned_session(home: &HomeSandbox) -> ConfigDirSandbox<'_> {
    let pin = runtime_config_dir(home, "7001-0");
    let mut row = live_row("7001-0", "prof");
    row.follows_chain = false;
    crate::live_sessions::register(&row).unwrap();
    pin
}

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_session_start_in_a_herdr_pane_reports_the_resume_argv() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _session = pinned_session(&home);
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
    let _session = pinned_session(&home);
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
    assert_eq!(
        build_report_args("w1:p1", "sid", &RestoreMode::Pinned("prof".into()), 42),
        expected
    );
}

/// The two other launch modes' argv tails, literal: a bare `claude` comes back
/// through `clauth __restore`, which runs a bare `claude --resume` (never
/// pinned to the account global at report time) and marks the launch as a
/// restore, and a chain session comes back still following the chain.
#[test]
fn the_report_argv_keeps_each_launch_mode() {
    let head = [
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
    ];
    let with_tail =
        |tail: &[&str]| -> Vec<String> { head.iter().chain(tail).map(|s| s.to_string()).collect() };
    assert_eq!(
        build_report_args("w1:p1", "sid", &RestoreMode::Bare, 42),
        with_tail(&["clauth", "__restore", "sid"])
    );
    assert_eq!(
        build_report_args("w1:p1", "sid", &RestoreMode::Chain("prof".into()), 42),
        with_tail(&[
            "clauth",
            "resume",
            "sid",
            "--profile",
            "prof",
            "--with-fallback"
        ])
    );
}

/// The reported case, through the fire: a plain `claude` in a herdr
/// pane (no `CLAUDE_CONFIG_DIR`, the global store) reports `clauth __restore`,
/// whose plain `claude --resume` keeps following the global account.
#[test]
fn a_bare_claude_session_reports_the_restore_command() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    report_for_fire(&payload("SessionStart", "sidbare"), Some("prof"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
    let seq = seq_of(&lines[0]);
    assert_eq!(
        lines[0],
        mode_line("w1:p1", "sidbare", &RestoreMode::Bare, seq)
    );
    assert!(
        lines[0].ends_with(" -- clauth __restore sidbare"),
        "{}",
        lines[0]
    );
}

/// `CLAUDE_CONFIG_DIR` exported at the global `~/.claude` is the same bare
/// session as no variable at all.
#[test]
fn a_session_on_the_global_config_dir_is_bare() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let global = home.home().join(".claude");
    std::fs::create_dir_all(&global).unwrap();
    let _dir = ConfigDirSandbox::new(&home, &global);
    report_for_fire(&payload("SessionStart", "sidglob"), Some("prof"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(
        lines[0],
        mode_line("w1:p1", "sidglob", &RestoreMode::Bare, seq_of(&lines[0]))
    );
}

/// A `clauth start --with-fallback` session's row follows the chain, so its
/// restore asks for the chain again, on the account it runs as now.
#[test]
fn a_chain_session_reports_resume_with_fallback() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _dir = runtime_config_dir(&home, "7002-0");
    crate::live_sessions::register(&live_row("7002-0", "prof")).unwrap();
    report_for_fire(&payload("SessionStart", "sidchain"), Some("member2"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(
        lines[0],
        mode_line(
            "w1:p1",
            "sidchain",
            &RestoreMode::Chain("member2".into()),
            seq_of(&lines[0])
        )
    );
}

/// A pinned `clauth start <name>` stays pinned: a row that does not follow the
/// chain, and a runtime dir with no row at all (a registry read that found
/// nothing is no evidence of a chain), both report `--profile` alone.
#[test]
fn a_pinned_or_unregistered_runtime_session_reports_its_profile() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    {
        let _session = pinned_session(&home);
        report_for_fire(&payload("SessionStart", "sidpin"), Some("prof"), false);
    }
    {
        let _dir = runtime_config_dir(&home, "7003-0");
        report_for_fire(&payload("SessionStart", "sidnorow"), Some("prof"), false);
    }
    let lines = wait_for_lines(home.home(), 2, WAIT);
    assert_eq!(
        lines[0],
        expected_line("w1:p1", "sidpin", "prof", seq_of(&lines[0]))
    );
    assert_eq!(
        lines[1],
        expected_line("w1:p1", "sidnorow", "prof", seq_of(&lines[1]))
    );
}

/// An isolated row never follows a chain (the chain gates refuse the pair), so
/// even a row carrying `follows_chain` reports pinned.
#[test]
fn an_isolated_session_reports_pinned() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _dir = runtime_config_dir(&home, "7004-0");
    let mut row = live_row("7004-0", "prof");
    row.isolated = true;
    crate::live_sessions::register(&row).unwrap();
    report_for_fire(&payload("SessionStart", "sidiso"), Some("prof"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(
        lines[0],
        expected_line("w1:p1", "sidiso", "prof", seq_of(&lines[0]))
    );
}

/// herdr 0.9 stores a `pane report-agent` state from a non-herdr source as the
/// pane's lifecycle authority, above its own screen detection. clauth never
/// reports `idle`, so a `--state working` report pinned the pane at `working`
/// for the rest of its life. The resume command needs no state: herdr records
/// it from a session report while no reporter holds the pane.
#[test]
fn the_report_never_claims_a_lifecycle_state() {
    let args = build_report_args("w1:p1", "sid", &RestoreMode::Pinned("prof".into()), 42);
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
    // A clauth runtime session: its argv names an account, so with none
    // attributable there is nothing to report.
    let _session = pinned_session(&home);
    let fire = payload("SessionStart", "sidnoacct");
    report_for_fire(&fire, None, false);
    report_for_fire(&fire, None, true);
    assert!(report_lines(home.home()).is_empty());
}

/// A bare `claude`'s restore argv names no account, so an account clauth
/// cannot attribute (a `claude login` it holds no copy of) costs neither the
/// report nor the launch-args record.
#[test]
fn an_unattributed_bare_session_still_reports_and_records() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _args = ClaudeArgsOverride::new(&["--model", "opus"]);
    report_for_fire(
        &sourced("SessionStart", "sidbarenoacct", "startup"),
        None,
        false,
    );
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(
        lines[0],
        mode_line(
            "w1:p1",
            "sidbarenoacct",
            &RestoreMode::Bare,
            seq_of(&lines[0])
        )
    );
    assert_eq!(
        record_of("sidbarenoacct").launch_options,
        Some(strings(&["--model"]))
    );
}

/// A pane whose herdr argv an older clauth reported pinned must not wait for
/// the next `SessionStart`: a herdr restore before it would turn the bare
/// conversation into a pinned clauth session for good. A record that holds a
/// past report (its seq) but not this argv re-reports on the next main-scope
/// fire, once.
#[test]
fn a_report_from_before_the_mode_split_is_corrected_on_the_next_fire() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let path = record_path("sidold", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.resume_seq = Some(5);
    store_record(&path, &record).unwrap();
    let prompt = payload("UserPromptSubmit", "sidold");
    report_for_fire(&prompt, Some("prof"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        mode_line("w1:p1", "sidold", &RestoreMode::Bare, seq_of(&lines[0]))
    );
    report_for_fire(&prompt, Some("prof"), false);
    assert_eq!(
        report_lines(home.home()).len(),
        1,
        "a corrected report is sent once"
    );
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
    let _session = pinned_session(&home);
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
/// every fire.
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
/// on every fire would buy nothing.
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
    let _session = pinned_session(&home);
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

// ── the restore notice ──────────────────────────────────────────────────────
//
// A bare restore relaunches `claude --resume <sid>` (through the hidden
// `clauth __restore <sid>`) with none of the options the conversation was
// started with. The hook records the launch's option NAMES at the
// conversation's first start, never a positional word nor a value unless it is
// shaped like an option, so no credential passed on the command line reaches
// the record or the notice. A resume arms the notice only when `__restore`
// launched it (its marker names this conversation and the pid it ran as,
// which `CLAUDE_PID` reports back), only while `[herdr] restore_notice` is
// on, and only when
// the resumed process lacks a launch option name; the next main-scope prompt
// is then refused with it, once. Every other resume clears a standing notice.

const SECRET: &str = "Bearer clauth-fixture-credential";

/// The restore marker `__restore` sets, pinned to `value` (`None` = absent)
/// for this test, whatever the runner's own env carries: a test run from a
/// restored pane inherits a real marker.
fn marker<'a>(home: &'a HomeSandbox, value: Option<&str>) -> EnvPin<'a> {
    EnvPin::new(
        home,
        &[(RESTORE_MARKER_ENV, value.map(std::ffi::OsStr::new))],
    )
}

/// A fire from the process `clauth __restore <session>` became: the marker
/// names the conversation and the pid `__restore` ran as, and `CLAUDE_PID`
/// names that same process (the unix `exec` keeps the pid). That pid is never
/// the test's own, as the hook's pid never is claude's.
fn restored<'a>(home: &'a HomeSandbox, session: &str) -> EnvPin<'a> {
    let pid = (std::process::id() + 1).to_string();
    let value = format!("{session}:{pid}");
    EnvPin::new(
        home,
        &[
            (RESTORE_MARKER_ENV, Some(std::ffi::OsStr::new(&value))),
            ("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid))),
        ],
    )
}

/// A record holding `options` as the launch's option names.
fn seed_launch(session: &str, options: &[&str], notice: Option<&str>) {
    let path = record_path(session, None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.launch_options = Some(strings(options));
    record.restore_notice = notice.map(str::to_string);
    store_record(&path, &record).unwrap();
}

/// `[herdr] restore_notice` written through the real profiles.toml load path.
fn write_restore_notice_knob(home: &HomeSandbox, on: bool) {
    let dir = home.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("profiles.toml"),
        format!(
            "active_profile = \"acct\"\nprofiles = [\"acct\"]\n[herdr]\nrestore_notice = {on}\n"
        ),
    )
    .unwrap();
}

/// The hidden `clauth __restore <sid>` herdr re-runs for a bare pane: the
/// `claude` CLI resuming that conversation, with the marker naming it and the
/// pid `__restore` runs as.
#[test]
fn the_restore_command_resumes_the_conversation_with_its_marker() {
    let cmd = restore_command("sid9");
    let program = std::path::Path::new(cmd.get_program());
    assert_eq!(
        program.file_stem().and_then(std::ffi::OsStr::to_str),
        Some("claude"),
        "{cmd:?}"
    );
    let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
    assert_eq!(args, ["--resume", "sid9"], "{cmd:?}");
    assert_eq!(
        crate::testutil::env_overrides(&cmd).get(RESTORE_MARKER_ENV),
        Some(&Some(format!("sid9:{}", std::process::id()))),
        "{cmd:?}"
    );
}

/// A `clauth __restore` run from inside a degraded session never hands the
/// claude it launches the degrade notice it inherited: that notice belongs to
/// the claude the degraded resume launched.
#[test]
fn the_restore_command_drops_an_inherited_degrade_notice() {
    let cmd = restore_command("sid9");
    let overrides = crate::testutil::env_overrides(&cmd);
    for key in [DEGRADE_NOTICE_ENV, DEGRADE_MARKER_ENV] {
        assert_eq!(overrides.get(key), Some(&None), "{key}: {cmd:?}");
    }
}

/// The launch record keeps option names only: a value (an `--mcp-config` JSON
/// carrying a bearer token) and a positional opening prompt never reach it.
#[test]
fn a_bare_startup_records_only_its_option_names() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let mcp = format!(r#"{{"headers":{{"Authorization":"{SECRET}"}}}}"#);
    let _args = ClaudeArgsOverride::new(&[
        "--model",
        "opus",
        "--mcp-config",
        &mcp,
        "fix the flaky test",
    ]);
    report_for_fire(
        &sourced("SessionStart", "sidlaunch", "startup"),
        Some("prof"),
        false,
    );
    assert_eq!(
        record_of("sidlaunch").launch_options,
        Some(strings(&["--model", "--mcp-config"]))
    );
    assert_eq!(record_of("sidlaunch").restore_notice, None);
    let bytes = std::fs::read_to_string(record_path("sidlaunch", None).unwrap()).unwrap();
    assert!(
        !bytes.contains("clauth-fixture-credential"),
        "a credential reached the record"
    );
    assert!(
        !bytes.contains("flaky"),
        "a positional word reached the record"
    );
}

/// The option-name reading, case by case: `--name=value` keeps its name, a
/// short flag stays whole, a repeat is named once in first-seen order, `--`
/// ends the options, and every spelling that names the conversation (the
/// resume and continue forms, `--session-id`, `--fork-session`) is dropped
/// with its value.
#[test]
fn option_names_keep_names_and_drop_everything_else() {
    for (argv, want) in [
        (vec!["--model=opus", "-v"], vec!["--model", "-v"]),
        (
            vec!["--add-dir", "/a", "--model", "x", "--add-dir", "/b"],
            vec!["--add-dir", "--model"],
        ),
        (
            vec!["--model", "x", "--", "--verbose", "prompt"],
            vec!["--model"],
        ),
        (vec!["hello world", "--model", "x"], vec!["--model"]),
        (vec!["--resume", "sid", "--model", "x"], vec!["--model"]),
        (
            vec!["-r", "sid", "-c", "--continue", "--resume=sid"],
            vec![],
        ),
        (vec!["-rsid", "-cv"], vec!["-v"]),
        (
            vec!["--session-id", "sid", "--fork-session", "--model", "x"],
            vec!["--model"],
        ),
        // A bare `-` (stdin) is a positional, never an option.
        (vec!["-", "--model", "x"], vec!["--model"]),
        // A short option with a glued value names the option alone: the
        // value never survives, whatever its bytes; a dash before no letter
        // names nothing.
        (
            vec![r#"-p"x""#, "-p=sk-1", "-1", "--model", "x"],
            vec!["-p", "--model"],
        ),
        // The residual the names-only rule accepts: a value shaped like an
        // option (letters after one dash) reads as one.
        (vec!["--model", "-x"], vec!["--model", "-x"]),
        // A short option's glued value, letters or not, never survives: a
        // single-dash word names its first letter alone.
        (vec!["-nPhoenix", "-wfeature"], vec!["-n", "-w"]),
        // A plain resume restores the agent a conversation was started
        // with, so `--agent` is never a lost option.
        (
            vec!["--agent", "pa", "--agent=pb", "--model", "x"],
            vec!["--model"],
        ),
    ] {
        assert_eq!(option_names(&strings(&argv)), strings(&want), "{argv:?}");
    }
}

/// The approved notice copy, pinned whole: the options the
/// restore dropped, by name only.
#[test]
fn the_restore_notice_names_the_dropped_options() {
    assert_eq!(
        restore_notice_text("sidrest", &strings(&["--model", "--mcp-config"])),
        "clauth: this conversation resumed as `claude --resume sidrest`, without the options \
         it was started with: --model, --mcp-config. This prompt was not sent: submit it \
         again to continue as restored, or restart the conversation with those options."
    );
}

/// A herdr restore (`__restore`'s marker names this conversation) that came
/// back without some launch options arms the notice naming exactly those. A
/// launcher that injects its own words on every run (a `claude` shim adding
/// `--system-prompt-file`) puts them on both sides, so they are never
/// reported as dropped.
#[test]
fn a_herdr_restore_that_dropped_options_arms_the_notice() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _marker = restored(&home, "sidrest");
    seed_launch(
        "sidrest",
        &["--system-prompt-file", "--model", "--mcp-config"],
        None,
    );
    let _args = ClaudeArgsOverride::new(&["--system-prompt-file", "/p", "--resume", "sidrest"]);
    report_for_fire(
        &sourced("SessionStart", "sidrest", "resume"),
        Some("prof"),
        false,
    );
    let after = record_of("sidrest");
    assert_eq!(
        after.launch_options,
        Some(strings(&[
            "--system-prompt-file",
            "--model",
            "--mcp-config"
        ])),
        "a resume never overwrites what it is compared against"
    );
    assert_eq!(
        after.restore_notice,
        Some(restore_notice_text(
            "sidrest",
            &strings(&["--model", "--mcp-config"])
        ))
    );
    assert_eq!(
        after.restore_notice_pid,
        Some(std::process::id() + 1),
        "the notice belongs to the restored process"
    );
}

/// A notice reaches only the process it was armed for. A restored pane quit
/// before its first prompt leaves its notice behind; a later launch of the
/// conversation (another account, another mode, outside herdr) is another
/// process, so its prompt is sent and the stale notice is dropped.
#[test]
fn a_notice_reaches_only_the_process_it_was_armed_for() {
    let home = HomeSandbox::new();
    let owner = std::process::id() + 1;
    let path = record_path("sidowned", None).unwrap();
    let seed = |path: &std::path::Path| {
        let mut record = load_record(path).unwrap_or_default();
        record.restore_notice = Some("owned".to_string());
        record.restore_notice_pid = Some(owner);
        store_record(path, &record).unwrap();
    };
    seed(&path);
    {
        let other = (owner + 1).to_string();
        let _pid = EnvPin::new(&home, &[("CLAUDE_PID", Some(std::ffi::OsStr::new(&other)))]);
        assert_eq!(
            take_restore_notice(&payload("UserPromptSubmit", "sidowned")),
            None
        );
        assert_eq!(
            record_of("sidowned").restore_notice,
            None,
            "a stale notice is dropped"
        );
    }
    seed(&path);
    let mine = owner.to_string();
    let _pid = EnvPin::new(&home, &[("CLAUDE_PID", Some(std::ffi::OsStr::new(&mine)))]);
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "sidowned")),
        Some("owned".to_string())
    );
    assert_eq!(record_of("sidowned").restore_notice, None);
}

/// A second herdr restore of the conversation (a new process) that lost the
/// same options re-stamps the notice for itself, so its own first prompt is
/// refused and the earlier process's stamp never makes it stale.
#[test]
fn a_new_restore_restamps_an_identical_notice_for_its_own_process() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let path = record_path("sidagain", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.launch_options = Some(strings(&["--model"]));
    record.restore_notice = Some(restore_notice_text("sidagain", &strings(&["--model"])));
    record.restore_notice_pid = Some(std::process::id() + 9);
    store_record(&path, &record).unwrap();
    let _marker = restored(&home, "sidagain");
    let _args = ClaudeArgsOverride::new(&["--resume", "sidagain"]);
    report_for_fire(
        &sourced("SessionStart", "sidagain", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(
        record_of("sidagain").restore_notice_pid,
        Some(std::process::id() + 1)
    );
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "sidagain")),
        Some(restore_notice_text("sidagain", &strings(&["--model"])))
    );
}

/// A resume that arms nothing (a `claude --resume` typed in another pane, no
/// marker) leaves a notice stamped for another process standing: that notice
/// is the restored process's to spend, or the take's to drop once that process
/// is gone. A notice stamped for this very process, or one naming no process
/// at all, it clears.
#[test]
fn a_resume_that_arms_nothing_keeps_another_processs_notice() {
    let owner = std::process::id() + 1;
    for (stamp, claude_pid, kept) in [
        (Some(owner), owner + 1, true),
        (Some(owner), owner, false),
        (None, owner + 1, false),
    ] {
        let home = HomeSandbox::new();
        let shim = echo_shim(home.home(), "herdr");
        let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
        let pid = claude_pid.to_string();
        let _pins = EnvPin::new(
            &home,
            &[
                (RESTORE_MARKER_ENV, None),
                ("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid))),
            ],
        );
        let path = record_path("sidkeep", None).unwrap();
        let mut record = load_record(&path).unwrap_or_default();
        record.launch_options = Some(strings(&["--model"]));
        record.restore_notice = Some("herdr".to_string());
        record.restore_notice_pid = stamp;
        store_record(&path, &record).unwrap();
        let _args = ClaudeArgsOverride::new(&["--resume", "sidkeep"]);
        report_for_fire(
            &sourced("SessionStart", "sidkeep", "resume"),
            Some("prof"),
            false,
        );
        let after = record_of("sidkeep");
        if kept {
            assert_eq!(
                (after.restore_notice.as_deref(), after.restore_notice_pid),
                (Some("herdr"), stamp),
                "another process's notice survives a resume that arms nothing"
            );
        } else {
            assert_eq!(
                after.restore_notice, None,
                "stamp {stamp:?}, CLAUDE_PID {claude_pid}"
            );
        }
    }
}

/// A notice on the record that names no process refuses no prompt: the
/// degrade notice rides the resumed claude's own env, so an unowned record
/// notice is a leftover no live resume owns, and the take drops it.
#[test]
fn an_unowned_record_notice_refuses_no_prompt() {
    let home = HomeSandbox::new();
    let pid = (std::process::id() + 7).to_string();
    let _pins = EnvPin::new(
        &home,
        &[
            ("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid))),
            (DEGRADE_NOTICE_ENV, None),
            (DEGRADE_MARKER_ENV, None),
        ],
    );
    let path = record_path("sidleft", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.restore_notice = Some("left behind".to_string());
    record.restore_notice_pid = None;
    store_record(&path, &record).unwrap();
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "sidleft")),
        None
    );
    assert_eq!(record_of("sidleft").restore_notice, None, "dropped");
}

/// The env a degraded `clauth resume` hands the claude it launches, pinned
/// for this test whatever the runner's own env carries: the notice, the
/// marker naming the conversation and the launcher's pid, and `CLAUDE_PID`.
fn degraded_env<'a>(
    home: &'a HomeSandbox,
    notice: Option<&str>,
    marker: Option<&str>,
    claude_pid: Option<&str>,
) -> EnvPin<'a> {
    EnvPin::new(
        home,
        &[
            (DEGRADE_NOTICE_ENV, notice.map(std::ffi::OsStr::new)),
            (DEGRADE_MARKER_ENV, marker.map(std::ffi::OsStr::new)),
            ("CLAUDE_PID", claude_pid.map(std::ffi::OsStr::new)),
        ],
    )
}

/// A live child of this test process stands in for the claude a degraded
/// resume launched: the test process is its launcher, its parent. Killed on
/// drop, so a failing test leaves no child behind.
struct LaunchedClaude(std::process::Child);

impl LaunchedClaude {
    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for LaunchedClaude {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn launched_claude() -> LaunchedClaude {
    LaunchedClaude(
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    )
}

/// The claude a degraded resume launched has its first main-scope prompt
/// refused with the notice from its own env, once: a tool call or a
/// subagent's prompt leaves it for that prompt, and nothing is written on the
/// record a later process of the conversation could be refused with.
#[test]
fn a_degraded_resume_refuses_its_own_claudes_first_prompt() {
    let home = HomeSandbox::new();
    let claude = launched_claude();
    let pid = claude.id().to_string();
    let marker = format!("siddeg:{}", std::process::id());
    let _env = degraded_env(&home, Some("the degrade notice"), Some(&marker), Some(&pid));
    assert_eq!(take_restore_notice(&payload("PostToolUse", "siddeg")), None);
    let mut sub = payload("UserPromptSubmit", "siddeg");
    sub.agent_id = Some("agent-1".to_string());
    assert_eq!(take_restore_notice(&sub), None);
    let prompt = payload("UserPromptSubmit", "siddeg");
    assert_eq!(
        take_restore_notice(&prompt),
        Some("the degrade notice".to_string())
    );
    assert_eq!(take_restore_notice(&prompt), None, "spent once");
    assert_eq!(record_of("siddeg").restore_notice, None);
}

/// The env notice reaches only the claude its resume launched for its
/// conversation: a claude under another launcher (one nested inside the
/// session inherits the env), another conversation, a fire with no
/// `CLAUDE_PID`, a marker without a readable pid, and either half alone are
/// all no prompt refused.
#[test]
fn a_degrade_notice_reaches_no_claude_its_resume_did_not_launch() {
    let claude = launched_claude();
    let pid = claude.id().to_string();
    let me = std::process::id();
    let ours = format!("siddeg:{me}");
    let other_launcher = format!("siddeg:{}", me + 1);
    let other_conversation = format!("sidother:{me}");
    for (notice, marker, claude_pid, why) in [
        (
            Some("n"),
            Some(other_launcher.as_str()),
            Some(pid.as_str()),
            "another launcher",
        ),
        (
            Some("n"),
            Some(other_conversation.as_str()),
            Some(pid.as_str()),
            "another conversation",
        ),
        (Some("n"), Some(ours.as_str()), None, "no CLAUDE_PID"),
        (
            Some("n"),
            Some("siddeg"),
            Some(pid.as_str()),
            "a marker missing its pid",
        ),
        (
            Some("n"),
            Some("siddeg:abc"),
            Some(pid.as_str()),
            "an unparsable pid",
        ),
        (
            Some("n"),
            None,
            Some(pid.as_str()),
            "a notice with no marker",
        ),
        (
            None,
            Some(ours.as_str()),
            Some(pid.as_str()),
            "a marker with no notice",
        ),
    ] {
        let home = HomeSandbox::new();
        let _env = degraded_env(&home, notice, marker, claude_pid);
        assert_eq!(
            take_restore_notice(&payload("UserPromptSubmit", "siddeg")),
            None,
            "{why}"
        );
    }
}

/// Two degraded resumes of one conversation run side by side (two panes):
/// each claude is told once, whatever the other one's prompts do in between.
#[test]
fn two_degraded_claudes_of_one_conversation_are_each_told_once() {
    let home = HomeSandbox::new();
    let first = launched_claude();
    let second = launched_claude();
    let marker = format!("siddeg:{}", std::process::id());
    let prompt = payload("UserPromptSubmit", "siddeg");
    let fire = |claude: &LaunchedClaude| {
        let pid = claude.id().to_string();
        let _env = degraded_env(&home, Some("the degrade notice"), Some(&marker), Some(&pid));
        take_restore_notice(&prompt)
    };
    let told = Some("the degrade notice".to_string());
    assert_eq!(fire(&first), told, "the first claude's first prompt");
    assert_eq!(fire(&second), told, "the second claude's first prompt");
    assert_eq!(fire(&first), None, "the first claude was already told");
    assert_eq!(fire(&second), None, "the second claude was already told");
}

/// Telling a degraded claude leaves a herdr notice stamped for another live
/// process of the conversation standing: that notice is the other process's
/// to spend.
#[test]
fn an_env_delivery_keeps_another_processs_record_notice() {
    let home = HomeSandbox::new();
    let claude = launched_claude();
    let owner = std::process::id() + 1;
    let path = record_path("siddeg", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.restore_notice = Some("herdr".to_string());
    record.restore_notice_pid = Some(owner);
    store_record(&path, &record).unwrap();
    let pid = claude.id().to_string();
    let marker = format!("siddeg:{}", std::process::id());
    let _env = degraded_env(&home, Some("the degrade notice"), Some(&marker), Some(&pid));
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "siddeg")),
        Some("the degrade notice".to_string())
    );
    let after = record_of("siddeg");
    assert_eq!(
        (after.restore_notice.as_deref(), after.restore_notice_pid),
        (Some("herdr"), Some(owner)),
        "the other process's notice and stamp survive"
    );
}

/// The told list is bounded by count on every target, newest kept: 20 told
/// pids keep the 16 told last, in the order they were told.
#[test]
fn the_told_list_keeps_the_newest_sixteen() {
    let mut told: Vec<u32> = (1..=20).collect();
    cap_degrade_told(&mut told);
    assert_eq!(told, (5..=20).collect::<Vec<u32>>());
    let mut short: Vec<u32> = vec![7, 3];
    cap_degrade_told(&mut short);
    assert_eq!(short, vec![7, 3], "a short list is kept whole");
}

/// The prune a delivery runs applies the count bound too: twenty told
/// entries of a pid that still runs (this test's own) keep sixteen.
#[test]
fn the_delivery_prune_applies_the_count_bound() {
    let mut told = vec![std::process::id(); 20];
    prune_degrade_told(&mut told);
    assert_eq!(told.len(), 16);
}

/// The conversation's records dir, made to exist with its scope lock file, so
/// a fault armed on it hits the record write, not a first-time create.
fn records_dir(session: &str) -> std::path::PathBuf {
    let path = record_path(session, None).unwrap();
    store_record(&path, &load_record(&path).unwrap_or_default()).unwrap();
    let dir = path.parent().unwrap().to_path_buf();
    drop(crate::hook_note::ScopeLock::acquire_within(
        std::time::Duration::from_secs(1),
    ));
    dir
}

/// A refusal whose "already told" mark cannot be stored (a full disk, a
/// read-only records dir) is not shown: the record is what stops the next
/// prompt being refused again, so a mark that cannot be kept means staying
/// silent. Once the dir is writable again the claude is told, once.
#[test]
fn a_degrade_notice_whose_told_mark_cannot_be_stored_is_not_shown() {
    let home = HomeSandbox::new();
    let claude = launched_claude();
    let dir = records_dir("siddeg");
    let pid = claude.id().to_string();
    let marker = format!("siddeg:{}", std::process::id());
    let _env = degraded_env(&home, Some("the degrade notice"), Some(&marker), Some(&pid));
    let prompt = payload("UserPromptSubmit", "siddeg");
    {
        let _fault = crate::testutil::UnwritableDir::new(&dir);
        assert_eq!(take_restore_notice(&prompt), None, "first prompt");
        assert_eq!(take_restore_notice(&prompt), None, "second prompt");
    }
    assert_eq!(
        take_restore_notice(&prompt),
        Some("the degrade notice".to_string())
    );
    assert_eq!(take_restore_notice(&prompt), None, "told once");
}

/// The herdr notice obeys the same rule: a delivery whose spending cannot be
/// stored is not shown and the notice stays armed for its process; once the
/// dir is writable again it is shown, once.
#[test]
fn a_herdr_notice_whose_spending_cannot_be_stored_is_not_shown() {
    let home = HomeSandbox::new();
    let owner = std::process::id() + 1;
    let pid = owner.to_string();
    let _pid = EnvPin::new(
        &home,
        &[
            ("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid))),
            (DEGRADE_NOTICE_ENV, None),
            (DEGRADE_MARKER_ENV, None),
        ],
    );
    let dir = records_dir("sidherdr");
    let path = record_path("sidherdr", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.restore_notice = Some("herdr".to_string());
    record.restore_notice_pid = Some(owner);
    store_record(&path, &record).unwrap();
    let prompt = payload("UserPromptSubmit", "sidherdr");
    {
        let _fault = crate::testutil::UnwritableDir::new(&dir);
        assert_eq!(take_restore_notice(&prompt), None, "first prompt");
        assert_eq!(take_restore_notice(&prompt), None, "second prompt");
    }
    assert_eq!(
        record_of("sidherdr").restore_notice.as_deref(),
        Some("herdr")
    );
    assert_eq!(take_restore_notice(&prompt), Some("herdr".to_string()));
    assert_eq!(take_restore_notice(&prompt), None, "spent once");
}

/// The told list holds only claudes that still run: a delivery drops a pid
/// whose process is gone (a later claude reusing it must not count as told)
/// and keeps one still running.
#[test]
fn a_delivery_forgets_told_claudes_that_are_gone() {
    let home = HomeSandbox::new();
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    let gone = gone.id();
    let other = launched_claude();
    let claude = launched_claude();
    let path = record_path("siddeg", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.degrade_told = vec![gone, other.id()];
    store_record(&path, &record).unwrap();
    let pid = claude.id().to_string();
    let marker = format!("siddeg:{}", std::process::id());
    let _env = degraded_env(&home, Some("the degrade notice"), Some(&marker), Some(&pid));
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "siddeg")),
        Some("the degrade notice".to_string())
    );
    let mut told = record_of("siddeg").degrade_told;
    told.sort_unstable();
    let mut want = vec![other.id(), claude.id()];
    want.sort_unstable();
    assert_eq!(
        told, want,
        "the gone pid {gone} is forgotten, the live ones kept"
    );
}

/// The user is told once per restored process: after the notice is spent, an
/// in-process `/resume` back into the restored conversation (same pid, same
/// inherited marker) arms nothing.
#[test]
fn a_spent_notice_is_never_rearmed_in_the_same_process() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _marker = restored(&home, "sidonce");
    seed_launch("sidonce", &["--model"], None);
    let _args = ClaudeArgsOverride::new(&["--resume", "sidonce"]);
    let fire = sourced("SessionStart", "sidonce", "resume");
    report_for_fire(&fire, Some("prof"), false);
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "sidonce")),
        Some(restore_notice_text("sidonce", &strings(&["--model"])))
    );
    report_for_fire(&fire, Some("prof"), false);
    assert_eq!(record_of("sidonce").restore_notice, None);
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "sidonce")),
        None
    );
}

/// Only `__restore` earns the notice. A resume the user typed in the pane
/// (no marker), or a nested one whose marker names another conversation,
/// arms nothing even when it lost options, and clears a notice left standing
/// by an earlier restore that was never prompted.
#[test]
fn a_resume_without_this_conversations_marker_arms_nothing() {
    for (sid, value) in [("sidmanual", None), ("sidnested", Some("sidother"))] {
        let home = HomeSandbox::new();
        let shim = echo_shim(home.home(), "herdr");
        let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
        let _marker = marker(&home, value);
        seed_launch(sid, &["--model"], Some("stale"));
        let _args = ClaudeArgsOverride::new(&["--resume", sid]);
        report_for_fire(&sourced("SessionStart", sid, "resume"), Some("prof"), false);
        assert_eq!(record_of(sid).restore_notice, None, "{sid}");
    }
}

/// A restore that kept every launch option name (a value may differ: that is
/// the user's later choice, never a loss) arms nothing and clears a stale
/// notice; so does one with no recorded launch at all.
#[test]
fn a_restore_that_lost_nothing_clears_a_stale_notice() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    {
        let _marker = restored(&home, "sidkept");
        seed_launch("sidkept", &["--model"], Some("stale"));
        let _args = ClaudeArgsOverride::new(&["--model", "sonnet", "--resume", "sidkept"]);
        report_for_fire(
            &sourced("SessionStart", "sidkept", "resume"),
            Some("prof"),
            false,
        );
        assert_eq!(record_of("sidkept").restore_notice, None);
    }
    let _marker = restored(&home, "sidnone");
    let _args = ClaudeArgsOverride::new(&["--resume", "sidnone"]);
    report_for_fire(
        &sourced("SessionStart", "sidnone", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(record_of("sidnone").restore_notice, None);
}

/// A restore whose process argv cannot be read cannot name a loss: it arms
/// nothing and clears a notice left standing by an earlier restore. The
/// marker and `CLAUDE_PID` agree on a pid that no longer runs, so the marker
/// matches and only the unreadable argv stops the arm.
#[test]
fn a_restore_with_an_unreadable_argv_clears_a_stale_notice() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    let pid = gone.id().to_string();
    let value = format!("sidblind:{pid}");
    let _pins = EnvPin::new(
        &home,
        &[
            (RESTORE_MARKER_ENV, Some(std::ffi::OsStr::new(&value))),
            ("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid))),
        ],
    );
    seed_launch("sidblind", &["--model"], Some("stale"));
    report_for_fire(
        &sourced("SessionStart", "sidblind", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(record_of("sidblind").restore_notice, None);
}

/// `[herdr] restore_notice = false` turns the notice off: a restore that
/// dropped options arms nothing.
#[test]
fn the_restore_notice_switch_off_arms_nothing() {
    let home = HomeSandbox::new();
    write_restore_notice_knob(&home, false);
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _marker = restored(&home, "sidoff");
    seed_launch("sidoff", &["--model"], None);
    let _args = ClaudeArgsOverride::new(&["--resume", "sidoff"]);
    report_for_fire(
        &sourced("SessionStart", "sidoff", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(record_of("sidoff").restore_notice, None);

    // The same restore with the switch written on arms it: the knob, not the
    // fixture, is what silenced it.
    write_restore_notice_knob(&home, true);
    report_for_fire(
        &sourced("SessionStart", "sidoff", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(
        record_of("sidoff").restore_notice,
        Some(restore_notice_text("sidoff", &strings(&["--model"])))
    );
}

/// The marker pins the process `__restore` became, not only the conversation:
/// a nested `claude --resume <sid>` run from inside the restored conversation
/// inherits the marker, but runs as another pid, so it arms nothing and
/// clears a standing notice. So does a marker missing its pid (an older
/// clauth's `<sid>` alone) or carrying one that does not parse, and a fire
/// with no `CLAUDE_PID` to compare against.
#[test]
fn a_nested_resume_of_the_restored_conversation_arms_nothing() {
    let restored_pid = (std::process::id() + 1).to_string();
    let nested_pid = (std::process::id() + 2).to_string();
    for (value, claude_pid) in [
        (format!("sidnest:{restored_pid}"), Some(&nested_pid)),
        (format!("sidnest:{restored_pid}"), None),
        (format!("sidnest:{restored_pid}"), Some(&"abc".to_string())),
        (format!("sidother:{restored_pid}"), Some(&restored_pid)),
        ("sidnest".to_string(), Some(&restored_pid)),
        ("sidnest:".to_string(), Some(&restored_pid)),
        ("sidnest:abc".to_string(), Some(&restored_pid)),
    ] {
        let home = HomeSandbox::new();
        let shim = echo_shim(home.home(), "herdr");
        let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
        let _pins = EnvPin::new(
            &home,
            &[
                (RESTORE_MARKER_ENV, Some(std::ffi::OsStr::new(&value))),
                ("CLAUDE_PID", claude_pid.map(std::ffi::OsStr::new)),
            ],
        );
        seed_launch("sidnest", &["--model"], Some("stale"));
        let _args = ClaudeArgsOverride::new(&["--resume", "sidnest"]);
        report_for_fire(
            &sourced("SessionStart", "sidnest", "resume"),
            Some("prof"),
            false,
        );
        assert_eq!(
            record_of("sidnest").restore_notice,
            None,
            "{value} {claude_pid:?}"
        );
    }
}

/// The marker's pid is what follows its LAST `:`, so a session id holding a
/// colon still matches its own restore.
#[test]
fn a_session_id_holding_a_colon_still_matches_its_marker() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _marker = restored(&home, "sid:colon");
    seed_launch("sid:colon", &["--model"], None);
    let _args = ClaudeArgsOverride::new(&["--resume", "sid:colon"]);
    report_for_fire(
        &sourced("SessionStart", "sid:colon", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(
        record_of("sid:colon").restore_notice,
        Some(restore_notice_text("sid:colon", &strings(&["--model"])))
    );
}

/// `[herdr] restore_notice` gates the herdr restore notice's arm only: the
/// `clauth resume --with-fallback` degrade refusal is shown at the next
/// prompt with the switch off.
#[test]
fn the_switch_off_never_hides_a_degraded_resume_refusal() {
    let home = HomeSandbox::new();
    write_restore_notice_knob(&home, false);
    let claude = launched_claude();
    let pid = claude.id().to_string();
    let marker = format!("siddeg:{}", std::process::id());
    let _env = degraded_env(
        &home,
        Some("clauth: resumed pinned to 'p'"),
        Some(&marker),
        Some(&pid),
    );
    assert_eq!(
        take_restore_notice(&payload("UserPromptSubmit", "siddeg")),
        Some("clauth: resumed pinned to 'p'".to_string())
    );
}

/// A `profiles.toml` that does not parse reads the switch at its default,
/// on: a restore that dropped options still arms the notice.
#[test]
fn an_unreadable_switch_keeps_the_notice_on() {
    let home = HomeSandbox::new();
    let dir = home.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("profiles.toml"), "[herdr\nrestore_notice = ").unwrap();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _marker = restored(&home, "sidtorn");
    seed_launch("sidtorn", &["--model"], None);
    let _args = ClaudeArgsOverride::new(&["--resume", "sidtorn"]);
    report_for_fire(
        &sourced("SessionStart", "sidtorn", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(
        record_of("sidtorn").restore_notice,
        Some(restore_notice_text("sidtorn", &strings(&["--model"])))
    );
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn sourced(event: &str, session: &str, source: &str) -> Payload {
    let mut fire = payload(event, session);
    fire.source = Some(source.to_string());
    fire
}

fn record_of(session: &str) -> crate::hook_note::NoteRecord {
    load_record(&record_path(session, None).unwrap()).unwrap_or_default()
}

/// A fork (`--fork-session`, `/fork`, `/branch`) starts a new conversation id
/// like `/clear`, so its `SessionStart` (`source: "fork"`) records the launch
/// options. `--fork-session` and `--session-id <id>` name the conversation,
/// never an option a restore could drop, so neither is recorded.
#[test]
fn a_fork_records_the_launch_options_without_conversation_spellings() {
    for (sid, source, argv) in [
        (
            "sidfork",
            "fork",
            ["--resume", "sidprev", "--fork-session", "--model", "opus"].as_slice(),
        ),
        (
            "sidpinned",
            "startup",
            ["--session-id", "sidpinned", "--model", "opus"].as_slice(),
        ),
    ] {
        let home = HomeSandbox::new();
        let shim = echo_shim(home.home(), "herdr");
        let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
        let _args = ClaudeArgsOverride::new(argv);
        report_for_fire(&sourced("SessionStart", sid, source), Some("prof"), false);
        assert_eq!(
            record_of(sid).launch_options,
            Some(strings(&["--model"])),
            "{argv:?}"
        );
    }
}

/// Every resume spelling Claude Code's parser accepts, the same set
/// `start::resume_forms` models: a glued `-r<id>`, a `c` or `r` inside a short
/// cluster (the cluster's other flags survive), and a bare `--resume` whose
/// neighbour is a flag, which that flag never feeds as a value.
#[test]
fn every_resume_spelling_is_dropped_from_the_option_names() {
    for argv in [
        vec!["-v", "--model", "opus", "-rsidk"],
        vec!["-cv", "--model", "opus"],
        vec!["-vrsidk", "--model", "opus"],
        vec!["-vr", "sidk", "--model", "opus"],
        vec!["-v", "--resume", "--model", "opus"],
        vec!["-v", "--continue", "--model", "opus"],
    ] {
        assert_eq!(
            option_names(&strings(&argv)),
            strings(&["-v", "--model"]),
            "{argv:?}"
        );
    }
}

/// The launch record drops the same spellings: a `/clear` in a process started
/// with a glued resume or a bare `--resume` beside a flag records the options
/// it was started with, never the resume.
#[test]
fn a_clear_records_no_glued_or_bare_resume_spelling() {
    for (sid, argv) in [
        ("sidglued", ["-rsidprev", "--model", "opus"]),
        ("sidbare", ["--resume", "--model", "opus"]),
    ] {
        let home = HomeSandbox::new();
        let shim = echo_shim(home.home(), "herdr");
        let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
        let _args = ClaudeArgsOverride::new(&argv);
        report_for_fire(&sourced("SessionStart", sid, "clear"), Some("prof"), false);
        assert_eq!(
            record_of(sid).launch_options,
            Some(strings(&["--model"])),
            "{argv:?}"
        );
    }
}

/// `/clear` starts a new conversation id in the same process, so its first
/// `SessionStart` (`source: "clear"`) records the launch options like a
/// startup. The process argv can still carry the resume that started it; that
/// spelling is never part of what a later restore is compared against.
#[test]
fn a_clear_records_the_launch_options_without_resume_spellings() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _args = ClaudeArgsOverride::new(&["--resume", "sidprev", "--model", "opus"]);
    report_for_fire(
        &sourced("SessionStart", "sidclear", "clear"),
        Some("prof"),
        false,
    );
    assert_eq!(
        record_of("sidclear").launch_options,
        Some(strings(&["--model"]))
    );
}

/// The notice covers bare restores only: a `clauth start` session's claude argv
/// carries clauth's own per-spawn flags, so comparing it would always differ.
/// Even a marker naming the conversation arms nothing on a pinned resume.
#[test]
fn a_clauth_session_records_and_arms_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let _session = pinned_session(&home);
    let _args = ClaudeArgsOverride::new(&["--settings", "/x/settings.json"]);
    report_for_fire(
        &sourced("SessionStart", "sidcl", "startup"),
        Some("prof"),
        false,
    );
    assert_eq!(record_of("sidcl").launch_options, None);

    let _marker = restored(&home, "sidcl");
    seed_launch("sidcl", &["--model"], None);
    let _resumed = ClaudeArgsOverride::new(&["--resume", "sidcl"]);
    report_for_fire(
        &sourced("SessionStart", "sidcl", "resume"),
        Some("prof"),
        false,
    );
    assert_eq!(record_of("sidcl").restore_notice, None);
}

/// Armed once, spent once: only the conversation's own prompt takes it, and a
/// tool call or a subagent's fire leaves it armed for that prompt.
#[test]
fn the_restore_notice_blocks_one_main_prompt() {
    let home = HomeSandbox::new();
    let owner = std::process::id() + 1;
    let pid = owner.to_string();
    let _pid = EnvPin::new(&home, &[("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid)))]);
    let path = record_path("sidtake", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    record.restore_notice = Some("the notice".to_string());
    record.restore_notice_pid = Some(owner);
    store_record(&path, &record).unwrap();
    assert_eq!(
        take_restore_notice(&payload("PostToolUse", "sidtake")),
        None
    );
    let mut sub = payload("UserPromptSubmit", "sidtake");
    sub.agent_id = Some("agent-1".to_string());
    assert_eq!(take_restore_notice(&sub), None);
    let prompt = payload("UserPromptSubmit", "sidtake");
    assert_eq!(take_restore_notice(&prompt), Some("the notice".to_string()));
    assert_eq!(take_restore_notice(&prompt), None);
}

/// Claude Code's documented block shape for `UserPromptSubmit`: the reason is
/// shown to the user and never reaches the model; the original prompt stays in
/// the block message, so the user can submit it again.
#[test]
fn the_block_envelope_is_claude_codes_decision_shape() {
    assert_eq!(
        crate::hook_note::block_envelope("why"),
        serde_json::json!({"decision": "block", "reason": "why"})
    );
}

/// The real argv reader, against a live child whose argv is known: the hook
/// reads its `claude` through `CLAUDE_PID`, argv0 excluded.
#[test]
fn claude_args_reads_the_claude_pid_argv() {
    let home = HomeSandbox::new();
    {
        let _pid = EnvPin::new(&home, &[("CLAUDE_PID", None)]);
        assert_eq!(claude_args(), None);
    }
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let read = {
        let _pid = EnvPin::new(&home, &[("CLAUDE_PID", Some(std::ffi::OsStr::new(&pid)))]);
        claude_args()
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(read, Some(strings(&["30"])));
}
