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
fn await_job_verdict_wakes_for_pending_jobs_without_a_delivery() {
    assert_eq!(
        await_job_verdict(false, &["d-pending-0".into()]),
        (
            2,
            Some(
                "delegate job `d-pending-0` is still working. no action needed yet; \
                 clauth will wake this session when it finishes."
                    .into()
            ),
        ),
    );
}

#[test]
fn await_job_verdict_lists_all_pending_jobs_with_or_without_a_delivery() {
    for delivered in [false, true] {
        assert_eq!(
            await_job_verdict(delivered, &["d-pending-0".into(), "d-pending-1".into()]),
            (
                2,
                Some(
                    "delegate jobs `d-pending-0`, `d-pending-1` are still working. \
                     no action needed yet; clauth will wake this session when they finish."
                        .into(),
                ),
            ),
        );
    }
}

/// The hook process outlives its own wait: the host kills it at the manifest's
/// `timeout`, and a kill delivers nothing.
#[test]
fn the_wait_runs_a_day_inside_the_hook_manifests_bound() {
    assert_eq!(AWAIT_JOB_DEADLINE_SECS, 24 * 60 * 60);
    assert_eq!(AWAIT_IDLE_WAKE_SECS, 55 * 60);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../plugins/hooks/hooks.json")).expect("manifest");
    for event in ["PostToolUse", "Stop", "StopFailure"] {
        let hook = manifest["hooks"][event]
            .as_array()
            .expect("the event is registered")
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().expect("hooks"))
            .find(|hook| hook["command"] == "clauth mcp-await-job")
            .unwrap_or_else(|| panic!("{event} runs the await hook"));
        assert_eq!(hook["asyncRewake"], true, "{event} can wake the session");
        assert!(
            hook["timeout"].as_u64().expect("timeout") > AWAIT_JOB_DEADLINE_SECS,
            "{event}'s bound outlasts the wait",
        );
        assert_eq!(
            hook["rewakeMessage"], "A clauth delegate result or status update follows.",
            "{event}'s wake names what follows",
        );
        assert_eq!(
            hook["rewakeSummary"], "clauth delegate update",
            "{event}'s wake never shows the host's placeholder",
        );
    }
}

#[test]
fn await_job_verdict_wakes_for_a_delivery_without_pending_jobs() {
    assert_eq!(await_job_verdict(true, &[]), (2, None));
}

#[test]
fn await_job_verdict_stays_silent_when_every_job_is_gone() {
    assert_eq!(await_job_verdict(false, &[]), (0, None));
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
            model: None,
            cwd: None,
            env: None,
            args: None,
            session_id: None,
            subagent_type: None,
            allowed_tools: None,
            permission_mode: None,
            result: None,
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
    let prose = drive(ClauthServer::new().monitor_with(MonitorArgs {
        job_ids: Some(vec!["../evil".to_string()]),
        cancel: None,
    }));
    assert_eq!(prose.is_error, Some(true));
    assert_eq!(assert_one_prose_block(&prose), "error: invalid job_id");
}
