#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! One content block per reply, every tool, refusals included. The old
//! `format` parameter's JSON arm is gone (prose is the only spelling a caller
//! sees; the JSON payload stays internal to the renderers), so what this file
//! still pins is the shape that survived: exactly one block, carrying prose.

use super::*;
use crate::testutil::HomeSandbox;

fn drive<F>(fut: F) -> CallToolResult
where
    F: std::future::Future<Output = Result<CallToolResult, ErrorData>>,
{
    // `monitor`'s wait loops sleep on tokio timers, which a bare current-thread
    // runtime does not arm.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    rt.block_on(fut)
        .expect("tool returns a tool result, never a transport error")
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

fn assert_one_prose_block(result: &CallToolResult) -> String {
    assert_eq!(result.content.len(), 1, "a reply is a single content block");
    let text = first_text(result);
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the reply must be prose, not a JSON blob: {text}",
    );
    text
}

#[test]
fn profiles_answers_prose_in_one_block() {
    let _home = HomeSandbox::new();

    let prose = drive(ClauthServer::new().profiles(Parameters(ProfilesArgs {
        names: None,
        scope: None,
    })));
    assert_eq!(assert_one_prose_block(&prose), "no profiles");
}

#[test]
fn session_scope_answers_prose_in_one_block() {
    let _home = HomeSandbox::new();

    let prose = drive(ClauthServer::new().profiles(Parameters(ProfilesArgs {
        names: None,
        scope: Some("session".to_string()),
    })));
    let text = assert_one_prose_block(&prose);
    assert!(
        text.starts_with("session profile unknown, source unknown"),
        "an unresolved session says so in prose: {text}",
    );
}

#[test]
fn switch_profile_refusal_answers_prose_in_one_block() {
    let _home = HomeSandbox::new();

    let prose = drive(ClauthServer::new().switch_profile(Parameters(SwitchArgs {
        name: "ghost".to_string(),
    })));
    assert_eq!(prose.is_error, Some(true));
    let text = assert_one_prose_block(&prose);
    assert_eq!(
        text.lines().next(),
        Some(
            "switch failed: profile not found: ghost; call `profiles` for valid names; active profile none"
        ),
    );
    // The session-effect note rides every arm of the reply, in the same shape
    // the init block carries it. Which variant this process earns depends on
    // the runner's own `CLAUDE_CONFIG_DIR`, so pin the lead, not the body.
    assert!(
        text.contains("\n\nswitch_profile & this session: "),
        "the reply names what a switch does to THIS session: {text}",
    );
}

#[test]
fn delegate_depth_refusal_answers_prose_in_one_block() {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the lock above, restored unconditionally.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, "1") };

    let prose = drive(ClauthServer::new().delegate_with(
        DelegateArgs {
            profiles: Some(vec!["any".to_string()]),
            prompt: Some("hi".to_string()),
            prompt_file: None,
            model: None,
            cwd: None,
            env: None,
            args: None,
            timeout_secs: None,
            idle_secs: None,
            resume: None,
            isolated: None,
            background: None,
        },
        ProgressSink::none(),
    ));
    assert_eq!(prose.is_error, Some(true));
    assert_eq!(
        assert_one_prose_block(&prose),
        "delegate to `any` failed: delegation depth exceeded (max 1)"
    );

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
}

#[test]
fn monitor_invalid_job_id_answers_prose_in_one_block() {
    let prose = drive(ClauthServer::new().monitor_with(
        MonitorArgs {
            job_ids: Some(vec!["../evil".to_string()]),
            wait_secs: None,
            return_on: None,
            cancel: None,
        },
        ProgressSink::none(),
    ));
    assert_eq!(prose.is_error, Some(true));
    assert_eq!(assert_one_prose_block(&prose), "error: invalid job_id");
}
