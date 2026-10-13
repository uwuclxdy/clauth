#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use crate::hook_note::{NoteRecord, Payload, load_record, record_path, store_record};
use crate::testutil::{ConfigDirSandbox, HomeSandbox};
use std::path::Path;

/// A payload carrying only what these tests vary.
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

/// Write the threshold into the sandboxed `profiles.toml`.
fn set_threshold(th: Option<u64>) {
    let state = crate::profile::AppState {
        context_nudge_threshold_tokens: th,
        ..crate::profile::AppState::default()
    };
    crate::profile::save_app_state(&state).expect("save state");
}

/// One JSONL transcript line: an assistant message carrying usage and, when
/// `blocks` is non-empty, content blocks.
fn assistant_line(input: u64, cache_read: u64, cache_create: u64, blocks: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-09-14T10:00:00.000Z","message":{{"id":"m1","role":"assistant","usage":{{"input_tokens":{input},"output_tokens":100,"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create}}},"content":[{blocks}]}}}}"#
    )
}

/// A `Task` tool_use content block, spliced into an assistant line's content.
fn task_block(id: &str) -> String {
    format!(r#"{{"type":"tool_use","id":"{id}","name":"Task"}}"#)
}

/// One JSONL transcript line: a user message carrying tool results.
fn tool_result_line(ids: &[&str]) -> String {
    let blocks: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"type":"tool_result","tool_use_id":"{id}","content":"done"}}"#))
        .collect();
    format!(
        r#"{{"timestamp":"2026-09-14T10:00:01.000Z","message":{{"id":"m2","role":"user","content":[{}]}}}}"#,
        blocks.join(",")
    )
}

fn write_transcript(home: &Path, name: &str, lines: &[String]) -> std::path::PathBuf {
    let path = home.join(format!("{name}.jsonl"));
    std::fs::write(&path, lines.join("\n")).expect("write transcript");
    path
}

/// Write a job-store record into the sandboxed `~/.clauth/jobs`: a fresh
/// `running` one reads live, a `done` one does not. `host` and `host_pid` are
/// the session id and the process whose server minted it (`None`/`0` =
/// unstamped, an older server's record).
fn write_job(id: &str, live: bool, host: Option<&str>, host_pid: u32) {
    let dir = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("jobs");
    std::fs::create_dir_all(&dir).expect("create jobs dir");
    let now = crate::usage::now_ms();
    let record = crate::mcp::jobs::JobRecord {
        job_id: id.to_string(),
        profile: "p".to_string(),
        state: if live {
            crate::mcp::jobs::JobState::Running
        } else {
            crate::mcp::jobs::JobState::Done
        },
        started_at: now,
        envelope: None,
        endpoint: None,
        provider: None,
        isolated: false,
        cwd: None,
        spawned_by: None,
        host_session: host.map(str::to_string),
        host_pid,
        session_id: None,
        timeout_secs: 0,
        idle_secs: None,
        last_output_at: now,
        recorded_at: now,
        tail: String::new(),
        done_at: if live { 0 } else { now },
        crashed: false,
        owner_pid: 0,
        owner_started_at: 0,
    };
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec(&record).expect("encode job"),
    )
    .expect("write job");
}

fn write_settings(home: &Path, json: &str) {
    let dir = home.join(".claude");
    std::fs::create_dir_all(&dir).expect("create .claude");
    std::fs::write(dir.join("settings.json"), json).expect("write settings");
}

/// One fire through the leg's own entry: read, then decide-and-store.
fn fire(p: &Payload) -> Option<String> {
    note(p)
}

/// What threshold the main-scope record was last told, if any.
fn told(session: &str) -> Option<u64> {
    load_record(&record_path(session, None).expect("record path"))
        .and_then(|r| r.context)
        .and_then(|c| c.told_threshold)
}

#[test]
fn a_crossing_emits_once_and_a_second_fire_stays_silent() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let path = write_transcript(
        home.home(),
        "above-150k",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-once");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
    assert_eq!(told("sess-once"), Some(100_000));
    assert_eq!(fire(&f), None);
    assert_eq!(told("sess-once"), Some(100_000));
}

#[test]
fn a_usage_below_the_threshold_stays_silent() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let path = write_transcript(
        home.home(),
        "below-50k",
        &[assistant_line(50_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-below");
    f.transcript = Some(path);
    assert_eq!(fire(&f), None);
    assert_eq!(told("sess-below"), None);
}

#[test]
fn a_whole_prompt_row_counts_input_alone() {
    let home = HomeSandbox::new();
    // Threshold sits between the input and the naive three-way sum: summing
    // would cross, the whole-prompt shape must not.
    set_threshold(Some(130_000));
    let path = write_transcript(
        home.home(),
        "whole-120k",
        &[assistant_line(120_000, 20_000, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-whole");
    f.transcript = Some(path);
    assert_eq!(fire(&f), None);
    assert_eq!(told("sess-whole"), None);
}

#[test]
fn an_input_equal_to_cache_read_counts_input_alone() {
    let home = HomeSandbox::new();
    // The shape boundary: input == cache_read is the whole-prompt shape, so
    // context tokens are input alone; the naive three-way sum would read the
    // row as anthropic.
    let path = write_transcript(
        home.home(),
        "shape-boundary",
        &[assistant_line(120_000, 120_000, 10_000, "")],
    );
    let tail = read_tail(&path, TAIL_BYTES).expect("tail read");
    assert_eq!(tail.usage, Some(120_000));
}

#[test]
fn an_anthropic_row_sums_all_three_fields() {
    let home = HomeSandbox::new();
    // Threshold sits between the input and the sum: input alone would stay
    // silent, the anthropic shape must cross.
    set_threshold(Some(100_000));
    let path = write_transcript(
        home.home(),
        "anthropic-130k",
        &[assistant_line(20_000, 100_000, 10_000, "")],
    );
    let mut f = payload("PostToolUse", "sess-anthropic");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
}

#[test]
fn the_later_of_two_usage_lines_wins() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let early = assistant_line(150_000, 0, 0, "");
    let late = assistant_line(50_000, 0, 0, "");
    let path = write_transcript(home.home(), "last-wins", &[early, late]);
    let tail = read_tail(&path, TAIL_BYTES).expect("tail read");
    assert_eq!(tail.usage, Some(50_000));
    // The later line decides the fire: its usage is below the threshold, so
    // the earlier above-threshold line must not leak through.
    let mut f = payload("PostToolUse", "sess-last");
    f.transcript = Some(path);
    assert_eq!(fire(&f), None);
    assert_eq!(told("sess-last"), None);
}

#[test]
fn an_open_task_counts_until_its_result_lands() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    let both_open = write_transcript(
        home.home(),
        "two-tasks",
        &[assistant_line(
            120_000,
            0,
            0,
            &format!("{},{}", task_block("tA"), task_block("tB")),
        )],
    );
    let mut f = payload("PostToolUse", "sess-open");
    f.transcript = Some(both_open);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 2 agents running. wait for them to return before closing the session."
        )
    );

    let one_closed = write_transcript(
        home.home(),
        "one-closed",
        &[
            assistant_line(
                120_000,
                0,
                0,
                &format!("{},{}", task_block("tA"), task_block("tB")),
            ),
            tool_result_line(&["tA"]),
        ],
    );
    let mut f = payload("PostToolUse", "sess-closed");
    f.transcript = Some(one_closed);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 1 agent running. wait for them to return before closing the session."
        )
    );
}

#[test]
fn repeated_partials_of_one_task_count_as_one_agent() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    // Two streaming partials of one turn, each carrying the same Task id.
    let path = write_transcript(
        home.home(),
        "partials",
        &[
            assistant_line(120_000, 0, 0, &task_block("tA")),
            assistant_line(120_000, 0, 0, &task_block("tA")),
        ],
    );
    let mut f = payload("PostToolUse", "sess-partials");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 1 agent running. wait for them to return before closing the session."
        )
    );
}

#[test]
fn a_task_tool_use_on_a_user_role_message_counts() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    // The binding is open Task tool_use ids in the tail, any role: a
    // user-role message carrying one still counts as a running agent.
    let line = format!(
        r#"{{"timestamp":"2026-09-14T10:00:01.000Z","message":{{"id":"m2","role":"user","usage":{{"input_tokens":120000,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{}]}}}}"#,
        task_block("tU")
    );
    let path = write_transcript(home.home(), "user-task", &[line]);
    let mut f = payload("PostToolUse", "sess-user-task");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 1 agent running. wait for them to return before closing the session."
        )
    );
}

#[test]
fn a_non_task_tool_use_is_not_counted_as_an_agent() {
    let home = HomeSandbox::new();
    let bash = assistant_line(
        120_000,
        0,
        0,
        r#"{"type":"tool_use","id":"tB","name":"Bash"}"#,
    );
    let path = write_transcript(home.home(), "bash-tool", &[bash]);
    let tail = read_tail(&path, TAIL_BYTES).expect("tail read");
    assert_eq!(tail.open_tasks, 0);
}

#[test]
fn auto_compact_on_uses_the_on_arm_and_an_absent_key_defaults_true() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let path = write_transcript(home.home(), "on-120k", &[assistant_line(120_000, 0, 0, "")]);
    write_settings(home.home(), r#"{"autoCompactEnabled":true}"#);
    let mut f = payload("PostToolUse", "sess-on");
    f.transcript = Some(path.clone());
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
    // No settings file at all: the binding default is true, so the ON arm.
    // NB the task brief's test list said "absent → OFF arm copy"; the binding
    // design says "default true when the key is absent". Implemented per the
    // binding; the conflict is flagged in the lane report.
    std::fs::remove_file(home.home().join(".claude").join("settings.json"))
        .expect("remove settings");
    let mut f = payload("PostToolUse", "sess-absent");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
}

#[test]
fn auto_compact_off_uses_the_off_arm_with_the_handoff_sentence() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    let path = write_transcript(
        home.home(),
        "off-120k",
        &[assistant_line(120_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-off");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over."
        )
    );
}

#[test]
fn the_config_dir_settings_win_over_the_home_fallback() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    // The fallback path says OFF, the config-dir path says ON: the env wins.
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    let cfg = home.home().join("cfgdir");
    std::fs::create_dir_all(&cfg).expect("create cfg dir");
    std::fs::write(cfg.join("settings.json"), r#"{"autoCompactEnabled":true}"#)
        .expect("write cfg settings");
    let _pin = ConfigDirSandbox::new(&home, &cfg);
    let path = write_transcript(
        home.home(),
        "env-120k",
        &[assistant_line(120_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-env");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
}

#[test]
fn the_running_clause_lists_only_what_is_running() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    let two = write_transcript(
        home.home(),
        "clause-two",
        &[assistant_line(
            120_000,
            0,
            0,
            &format!("{},{}", task_block("tA"), task_block("tB")),
        )],
    );
    write_job("d-run", true, Some("sess-2a1d"), 0);
    write_job("d-done", false, Some("sess-2a1d"), 0);
    write_job("d-run-1d", true, Some("sess-1d"), 0);
    let mut f = payload("PostToolUse", "sess-2a1d");
    f.transcript = Some(two);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 2 agents and 1 delegate running. wait for them to return before closing the session."
        )
    );

    // One delegate alone: a transcript with no open tasks; the other
    // session's done job is not counted.
    let none = write_transcript(
        home.home(),
        "clause-none",
        &[assistant_line(120_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-1d");
    f.transcript = Some(none.clone());
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 1 delegate running. wait for them to return before closing the session."
        )
    );

    // Nothing running here: this session's delegate finished, and a peer's
    // live one adds no clause.
    write_job("d-0", false, Some("sess-0"), 0);
    let mut f = payload("PostToolUse", "sess-0");
    f.transcript = Some(none);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over."
        )
    );
}

/// The clause speaks for THIS conversation: a peer session's live delegate is
/// not its to wait on, while a record no server stamped (an older server still
/// running across a self-update) counts, since missing it orphans a paid run.
#[test]
fn the_running_clause_counts_only_this_sessions_delegates() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    let path = write_transcript(
        home.home(),
        "own-delegates",
        &[assistant_line(120_000, 0, 0, "")],
    );
    write_job("d-mine", true, Some("sess-me"), 0);
    write_job("d-peer", true, Some("sess-peer"), 0);
    write_job("d-legacy", true, None, 0);
    let mut f = payload("PostToolUse", "sess-me");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 2 delegates running. wait for them to return before closing the session."
        )
    );
}

/// `/clear` hands the conversation a new session id while the same `claude`
/// process and its `clauth mcp` server live on, so the server keeps stamping
/// the id it started under. The hook knows its own process (`CLAUDE_PID`) and
/// matches on it; a peer process stays out even when its server started under
/// the same id. A record with no host pid (an older server, a host whose pid
/// was unknown) still falls back to the session id, and an unstamped one counts.
#[test]
fn the_running_clause_keeps_counting_its_own_delegates_after_a_clear() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":false}"#);
    let _pid =
        crate::testutil::EnvPin::new(&home, &[("CLAUDE_PID", Some(std::ffi::OsStr::new("4242")))]);
    let path = write_transcript(
        home.home(),
        "after-clear",
        &[assistant_line(120_000, 0, 0, "")],
    );
    write_job("d-before-clear", true, Some("sess-before-clear"), 4242);
    write_job("d-after-clear", true, Some("sess-before-clear"), 4242);
    write_job("d-peer", true, Some("sess-before-clear"), 5151);
    write_job("d-pidless", true, Some("sess-after-clear"), 0);
    write_job("d-pidless-peer", true, Some("sess-peer"), 0);
    write_job("d-legacy", true, None, 0);
    let mut f = payload("PostToolUse", "sess-after-clear");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. consider writing or updating a handoff prompt so a fresh session can take over. there are still 4 delegates running. wait for them to return before closing the session."
        )
    );
}

#[test]
fn the_running_clause_stays_off_the_auto_compact_on_arm() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    write_settings(home.home(), r#"{"autoCompactEnabled":true}"#);
    let path = write_transcript(
        home.home(),
        "on-running",
        &[assistant_line(120_000, 0, 0, &task_block("tA"))],
    );
    let mut f = payload("PostToolUse", "sess-on-running");
    f.transcript = Some(path);
    // An open agent with auto-compact on: the ON arm carries no running
    // clause, however many things are still running.
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
}

#[test]
fn an_off_threshold_stays_silent_and_a_turned_back_on_threshold_re_arms() {
    let home = HomeSandbox::new();
    let path = write_transcript(
        home.home(),
        "above-150k",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-rearm");
    f.transcript = Some(path);
    // Off: silent, nothing recorded.
    set_threshold(None);
    assert_eq!(fire(&f), None);
    assert_eq!(told("sess-rearm"), None);
    // On: crossing emits and records.
    set_threshold(Some(100_000));
    assert!(fire(&f).is_some());
    assert_eq!(told("sess-rearm"), Some(100_000));
    // Off again: silent, and the told stamp is cleared so a later on re-arms.
    set_threshold(None);
    assert_eq!(fire(&f), None);
    assert_eq!(told("sess-rearm"), None);
    // Back on: the cleared stamp lets it emit again.
    set_threshold(Some(100_000));
    assert!(fire(&f).is_some());
}

#[test]
fn a_threshold_change_re_emits_while_the_usage_stays_above() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let path = write_transcript(
        home.home(),
        "th-change-200k",
        &[assistant_line(200_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-th-change");
    f.transcript = Some(path);
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
    // The told stamp names the threshold it last told: moving the config to a
    // threshold the same usage still crosses announces the new one.
    set_threshold(Some(150_000));
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 150k tokens. auto-compaction is turned on."
        )
    );
    assert_eq!(told("sess-th-change"), Some(150_000));
}

#[test]
fn a_compaction_re_emits_while_the_usage_is_still_above() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let above = write_transcript(
        home.home(),
        "compact-above",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let below = write_transcript(
        home.home(),
        "compact-below",
        &[assistant_line(50_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-compact");
    f.transcript = Some(above.clone());
    assert!(fire(&f).is_some());
    // A compaction dropped the note from context: SessionStart compact with
    // usage still above re-announces.
    let mut compact = payload("SessionStart", "sess-compact");
    compact.source = Some("compact".to_string());
    compact.transcript = Some(above.clone());
    assert_eq!(
        fire(&compact).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
    // A plain startup is a fresh context boundary: no re-announcement.
    let mut startup = payload("SessionStart", "sess-compact");
    startup.source = Some("startup".to_string());
    startup.transcript = Some(above);
    assert_eq!(fire(&startup), None);
    // A compaction with usage no longer above stays silent.
    let mut compact = payload("SessionStart", "sess-compact");
    compact.source = Some("compact".to_string());
    compact.transcript = Some(below);
    assert_eq!(fire(&compact), None);
}

#[test]
fn an_agent_scope_fire_stays_silent() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let path = write_transcript(
        home.home(),
        "above-150k",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-agent");
    f.agent_id = Some("agent1".to_string());
    f.transcript = Some(path);
    assert_eq!(fire(&f), None);
    assert_eq!(
        load_record(&record_path("sess-agent", None).expect("record path")),
        None
    );
}

#[test]
fn an_out_of_range_threshold_reads_as_off() {
    let home = HomeSandbox::new();
    let path = write_transcript(
        home.home(),
        "above-150k",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let mut f = payload("PostToolUse", "sess-range");
    f.transcript = Some(path.clone());
    for bad in [Some(10_000_u64), Some(3_000_000_u64)] {
        set_threshold(bad);
        // The load-time normalization resets the hand-edit to off.
        assert_eq!(
            crate::profile::load_app_state()
                .expect("state")
                .context_nudge_threshold_tokens(),
            None
        );
        assert_eq!(fire(&f), None);
    }
    // The band edges themselves are on.
    set_threshold(Some(50_000));
    let mut edge = payload("PostToolUse", "sess-min");
    edge.transcript = Some(path);
    assert_eq!(
        fire(&edge).as_deref(),
        Some("clauth: context window usage has exceeded 50k tokens. auto-compaction is turned on.")
    );
}

#[test]
fn a_divisible_threshold_renders_as_k_and_an_indivisible_one_as_a_plain_number() {
    assert_eq!(
        render_context_note(600_000, true, 0, 0),
        "clauth: context window usage has exceeded 600k tokens. auto-compaction is turned on."
    );
    assert_eq!(
        render_context_note(123_456, true, 0, 0),
        "clauth: context window usage has exceeded 123456 tokens. auto-compaction is turned on."
    );
}

#[test]
fn an_exact_million_threshold_renders_as_m() {
    assert_eq!(
        render_context_note(1_000_000, true, 0, 0),
        "clauth: context window usage has exceeded 1M tokens. auto-compaction is turned on."
    );
    assert_eq!(
        render_context_note(2_000_000, true, 0, 0),
        "clauth: context window usage has exceeded 2M tokens. auto-compaction is turned on."
    );
    // Above 1M but not an exact million: plain, never a k form.
    assert_eq!(
        render_context_note(1_500_000, true, 0, 0),
        "clauth: context window usage has exceeded 1500000 tokens. auto-compaction is turned on."
    );
}

#[test]
fn the_transcript_falls_back_to_the_records_stored_path() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let stored = write_transcript(
        home.home(),
        "stored-150k",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let path = record_path("sess-fallback", None).expect("record path");
    std::fs::create_dir_all(path.parent().expect("records dir")).expect("create records dir");
    let mut record = NoteRecord::default();
    record.transcript = Some(stored);
    store_record(&path, &record).expect("store record");
    // No transcript in the payload: the stored one is read.
    let f = payload("PostToolUse", "sess-fallback");
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
}

#[test]
fn a_legacy_record_without_the_context_field_reads_as_untold() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let stored = write_transcript(
        home.home(),
        "legacy-150k",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let path = record_path("sess-legacy", None).expect("record path");
    std::fs::create_dir_all(path.parent().expect("records dir")).expect("create records dir");
    // Pre-`context` record bytes: everything the account leg wrote, minus the
    // field this leg added later. The Option implicit default must parse it
    // and read it as untold, so the crossing still announces.
    let mut record = NoteRecord::default();
    record.transcript = Some(stored);
    let mut json = serde_json::to_value(&record).expect("encode record");
    json.as_object_mut()
        .expect("record object")
        .remove("context")
        .expect("record context key");
    std::fs::write(&path, serde_json::to_vec(&json).expect("encode legacy"))
        .expect("write legacy record");
    // No payload transcript: the leg reads the record's stored one, which
    // only works if the legacy bytes parse.
    let f = payload("PostToolUse", "sess-legacy");
    assert_eq!(
        fire(&f).as_deref(),
        Some(
            "clauth: context window usage has exceeded 100k tokens. auto-compaction is turned on."
        )
    );
    assert_eq!(told("sess-legacy"), Some(100_000));
}

#[test]
fn a_payload_transcript_wins_over_the_record_and_a_missing_transcript_stays_silent() {
    let home = HomeSandbox::new();
    set_threshold(Some(100_000));
    let above = write_transcript(
        home.home(),
        "wins-above",
        &[assistant_line(150_000, 0, 0, "")],
    );
    let below = write_transcript(
        home.home(),
        "wins-below",
        &[assistant_line(50_000, 0, 0, "")],
    );
    // The record points above; the payload's below path wins: silent.
    let path = record_path("sess-wins", None).expect("record path");
    std::fs::create_dir_all(path.parent().expect("records dir")).expect("create records dir");
    let mut record = NoteRecord::default();
    record.transcript = Some(above);
    store_record(&path, &record).expect("store record");
    let mut f = payload("PostToolUse", "sess-wins");
    f.transcript = Some(below);
    assert_eq!(fire(&f), None);
    // A payload path that does not exist: silent, no crash.
    let mut g = payload("PostToolUse", "sess-missing");
    g.transcript = Some(home.home().join("nope.jsonl"));
    assert_eq!(fire(&g), None);
}

#[test]
fn the_tail_window_bounds_what_a_fire_counts() {
    let home = HomeSandbox::new();
    let early = assistant_line(120_000, 0, 0, &task_block("tA"));
    let padding = format!(r#"{{"padding":"{}"}}"#, "x".repeat(400));
    let late = assistant_line(130_000, 0, 0, &task_block("tB"));
    let path = write_transcript(
        home.home(),
        "tail-late",
        &[early.clone(), padding.clone(), late],
    );
    // A 300-byte window opens inside the padding: the late usage line and its
    // Task block are in, the early ones are out.
    let tail = read_tail(&path, 300).expect("tail read");
    assert_eq!(tail.usage, Some(130_000));
    assert_eq!(tail.open_tasks, 1);
    // With the padding LAST, the early usage line and task sit outside the
    // window: nothing is counted.
    let path = write_transcript(home.home(), "tail-early", &[early, padding]);
    let tail = read_tail(&path, 300).expect("tail read");
    assert_eq!(tail.usage, None);
    assert_eq!(tail.open_tasks, 0);
}
