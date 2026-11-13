//! The `hook-foreground-spawn` verdict, driven against the real binary.
//!
//! A delegate runs `claude -p`, which waits on the background agents and
//! shells its main thread starts, but not on a shell a subagent backgrounds:
//! once that subagent's turn ends, nothing reads the shell and its work never
//! reaches the delegate's result. The hook refuses that one spawn and names the
//! retry. Spawning is the only way to see what Claude Code reads off the
//! hook's stdout and exit code. The hook reads only its stdin, never a home,
//! so it runs on every platform.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

const SUBAGENT_SHELL_REASON: &str = "refused: a subagent's background shell is never waited on in a headless delegate (claude -p), so its output would be lost: re-send this call with run_in_background: false.";

const SUBAGENT: &str = "a35b6651539f5f17f";

/// Run the hook on `stdin`; returns (exit code, stdout, stderr).
fn run_hook(stdin: &str) -> (Option<i32>, String, String) {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .arg("hook-foreground-spawn")
        .env("HOME", home.path())
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env("CLAUTH_NO_UPDATE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clauth");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn payload(tool: &str, input: serde_json::Value, agent_id: Option<serde_json::Value>) -> String {
    let mut p = serde_json::json!({
        "session_id": "5d0c3f8e-0000-4000-8000-000000000001",
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": input,
    });
    if let Some(id) = agent_id {
        p["agent_id"] = id;
    }
    p.to_string()
}

fn subagent() -> Option<serde_json::Value> {
    Some(serde_json::json!(SUBAGENT))
}

#[track_caller]
fn assert_denies(stdin: &str) {
    let (code, stdout, stderr) = run_hook(stdin);
    assert_eq!(code, Some(0), "a deny exits 0; stderr: {stderr}");
    assert_eq!(stderr, "", "a deny writes nothing to stderr");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {stdout:?}"));
    assert_eq!(
        parsed,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": SUBAGENT_SHELL_REASON,
            }
        })
    );
}

#[track_caller]
fn assert_allows(stdin: &str) {
    let (code, stdout, stderr) = run_hook(stdin);
    assert_eq!(code, Some(0), "an allow exits 0; stderr: {stderr}");
    assert_eq!(stdout, "", "an allow prints nothing");
    assert_eq!(stderr, "", "an allow writes nothing to stderr");
}

#[test]
fn a_subagents_background_shell_is_refused_with_its_retry() {
    assert_denies(&payload(
        "Bash",
        serde_json::json!({"command": "cargo test", "run_in_background": true}),
        subagent(),
    ));
}

/// The main thread's background shell is one `claude -p` waits on.
#[test]
fn the_main_threads_background_shell_passes() {
    assert_allows(&payload(
        "Bash",
        serde_json::json!({"command": "cargo test", "run_in_background": true}),
        None,
    ));
}

/// An empty or non-string `agent_id` names no subagent: the call is the main
/// thread's own.
#[test]
fn an_empty_or_null_agent_id_is_the_main_thread() {
    for agent_id in [serde_json::json!(""), serde_json::Value::Null] {
        assert_allows(&payload(
            "Bash",
            serde_json::json!({"command": "cargo test", "run_in_background": true}),
            Some(agent_id),
        ));
    }
}

/// Only the JSON literal `true` backgrounds a shell; a subagent's other
/// spellings run in the foreground and pass.
#[test]
fn a_subagents_foreground_shell_passes() {
    for input in [
        serde_json::json!({"command": "cargo test"}),
        serde_json::json!({"command": "cargo test", "run_in_background": false}),
        serde_json::json!({"command": "cargo test", "run_in_background": "true"}),
    ] {
        assert_allows(&payload("Bash", input, subagent()));
    }
}

/// `claude -p` waits on a background agent, so no agent spawn is refused,
/// whoever makes it and whatever its flags.
#[test]
fn every_agent_spawn_passes() {
    for (tool, input, agent_id) in [
        (
            "Agent",
            serde_json::json!({"prompt": "review", "run_in_background": true}),
            None,
        ),
        ("Agent", serde_json::json!({"prompt": "review"}), None),
        (
            "Task",
            serde_json::json!({"prompt": "review", "run_in_background": true}),
            None,
        ),
        (
            "Agent",
            serde_json::json!({"prompt": "review", "isolation": "remote"}),
            None,
        ),
        (
            "Agent",
            serde_json::json!({"prompt": "review", "run_in_background": true}),
            subagent(),
        ),
    ] {
        assert_allows(&payload(tool, input, agent_id));
    }
}

#[test]
fn another_tool_passes() {
    assert_allows(&payload(
        "Read",
        serde_json::json!({"file_path": "/etc/hostname", "run_in_background": true}),
        subagent(),
    ));
}

/// The hook fails open: an input it cannot read never blocks the call.
#[test]
fn an_unreadable_payload_passes() {
    for stdin in [
        "not json".to_string(),
        String::new(),
        serde_json::json!({"tool_name": "Bash", "agent_id": SUBAGENT, "tool_input": "run_in_background"}).to_string(),
        serde_json::json!({"agent_id": SUBAGENT, "tool_input": {"run_in_background": true}}).to_string(),
    ] {
        assert_allows(&stdin);
    }
}
