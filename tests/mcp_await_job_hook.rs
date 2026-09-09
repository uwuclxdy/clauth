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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
}
