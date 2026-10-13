#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

//! Guard coverage for the MCP `switch_profile` tool itself (the
//! `ClauthServer::switch_profile` seam, not the `switch_profile_noninteractive`
//! action it wraps). An unknown or
//! wrong-case profile name must be rejected BEFORE any credential mutation:
//! without the canonical-name guard the raw arg reaches `link_profile_credentials`,
//! which removes the live `~/.claude/.credentials.json` symlink and creates no
//! replacement, leaving the global session credential-less.

use super::*;

use crate::claude::force_link_profile_credentials;
use crate::profile::{
    AppState, ClaudeCredentials, OAuthToken, Profile, claude_dir, read_json_file, save_app_state,
    save_profile,
};
use crate::testutil::HomeSandbox;

/// Seed one cleanly-linked active profile on disk — profile creds, a symlinked
/// live `~/.claude/.credentials.json`, and persisted app state — so the tool's
/// own `load_config` sees a real active session.
fn seed_active_linked() {
    let mut p = Profile::new("active".to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "stored-a".to_string(),
            refresh_token: Some("stored-r".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    save_profile(&p).expect("save profile");
    force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");

    let state = AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into()],
        ..Default::default()
    };
    save_app_state(&state).expect("save state");
}

/// Drive the async `switch_profile` tool on a current-thread runtime.
fn call_switch(name: &str) -> CallToolResult {
    let server = ClauthServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .switch_profile(Parameters(SwitchArgs {
                name: name.to_string(),
            }))
            .await
    })
    .expect("switch_profile returns a tool result, never a transport error")
}

#[test]
fn unknown_target_is_rejected_without_stripping_live_creds() {
    let _home = HomeSandbox::new();
    seed_active_linked();

    let live = claude_dir().expect("claude dir").join(".credentials.json");
    assert!(
        live.symlink_metadata().is_ok(),
        "precondition: live credentials are linked",
    );

    let result = call_switch("ghost");
    assert_eq!(
        result.is_error,
        Some(true),
        "an unknown profile name must be a tool error",
    );
    assert!(
        live.symlink_metadata().is_ok(),
        "the live credentials symlink must survive a failed switch to an unknown name",
    );
}

/// Seed a cleanly-linked active profile plus a second stored `target`, both
/// registered — a non-diverged setup where switching to `target` succeeds.
fn seed_active_plus_target() {
    seed_active_linked();

    let mut target = Profile::new("target".to_string(), None, None);
    target.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "target-a".to_string(),
            refresh_token: Some("target-r".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    save_profile(&target).expect("save target");

    let state = AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into(), "target".into()],
        ..Default::default()
    };
    save_app_state(&state).expect("save state");
}

/// The reserved running record a switch test seeds a job from, in the shape a
/// real reserve writes. The owner fields default to the legacy ownerless shape;
/// a test that poses an owned job overrides them.
fn switch_running_spec(job_id: &str, profile: &str, started_at: u64) -> jobs::RunningSpec {
    jobs::RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        endpoint: None,
        provider: None,
        isolated: false,
        cwd: None,
        spawned_by: None,
        host_session: None,
        host_pid: 0,
        idle_secs: None,
        kind: jobs::RecordKind::Collectable,
        owner_pid: 0,
        owner_started_at: 0,
    }
}

/// Row 4's demanded shape: a profile switch with live delegates under this
/// server refuses BEFORE the mutation, naming the jobs and the fix — a switch
/// re-pins the session's account, and the jobs' monitor handles die with this
/// server, parking them beyond the next session's monitor (the DS3→DS5 case).
#[test]
fn a_switch_with_a_live_delegate_refuses_before_the_mutation() {
    let _home = HomeSandbox::new();
    seed_active_plus_target();

    let _marker = jobs::hold_server_marker().expect("hold the server marker");
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: std::process::id(),
        owner_started_at: jobs::server_started_at(),
        ..switch_running_spec("d-switch-live-0", "work", crate::usage::now_ms())
    })
    .unwrap();

    let result = call_switch("target");
    assert_eq!(
        result.is_error,
        Some(true),
        "a live delegate must refuse the switch"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("switch refusal text");
    assert!(
        text.contains("d-switch-live-0") && text.contains("cancel or collect"),
        "the refusal names the held jobs and the fix: {text}"
    );

    // The refusal runs BEFORE the mutation: the live link still names the
    // original active profile, so nothing was half-switched.
    let live: ClaudeCredentials =
        read_json_file(&claude_dir().expect("claude dir").join(".credentials.json"))
            .expect("read live creds");
    assert_eq!(
        live.refresh_token(),
        Some("stored-r"),
        "a refused switch leaves the live link untouched"
    );
}

/// The guard scopes to THIS server's own live runs: a dead owner's parked job
/// and a foreign live server's job are not this switch's to protect — that
/// server's own monitor still reaches them. A done job is no guard either.
#[test]
fn the_live_jobs_guard_scopes_to_this_servers_own_runs() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();

    // A parked job (owner dead) is not protected by a refusal: the switch
    // cannot make its fate worse, and nothing here owns it to protect.
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: 42_424,
        ..switch_running_spec("d-switch-parked-0", "work", now)
    })
    .unwrap();
    assert!(
        super::live_jobs_guard(now).is_none(),
        "a dead owner's job is not this switch's to protect"
    );
    jobs::remove("d-switch-parked-0");

    // A foreign live server's job stays reachable through that server.
    let _foreign = jobs::hold_foreign_server_marker_for_test(999_999);
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: 999_999,
        owner_started_at: 1_700_000_000_000,
        ..switch_running_spec("d-switch-foreign-0", "work", now)
    })
    .unwrap();
    assert!(
        super::live_jobs_guard(now).is_none(),
        "a foreign live job is that server's, not this switch's"
    );
    jobs::remove("d-switch-foreign-0");

    // This server's own live run is the one the refusal protects.
    let _mine = jobs::hold_server_marker().expect("hold the server marker");
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: std::process::id(),
        owner_started_at: jobs::server_started_at(),
        ..switch_running_spec("d-switch-mine-0", "work", now)
    })
    .unwrap();
    let guard = super::live_jobs_guard(now).expect("this server's own run guards the switch");
    assert!(
        guard.contains("d-switch-mine-0"),
        "the guard names the held job: {guard}"
    );

    // A done job holds no run to strand.
    jobs::remove("d-switch-mine-0");
    jobs::write_done_parts(
        "d-switch-mine-0",
        "work",
        1,
        None,
        None,
        false,
        serde_json::json!({"is_error": false, "result": "ok"}),
    )
    .unwrap();
    assert!(
        super::live_jobs_guard(now).is_none(),
        "a done job is no guard"
    );
}

#[test]
fn valid_switch_repoints_active_through_the_blocking_task() {
    let _home = HomeSandbox::new();
    seed_active_plus_target();

    // Exercises the `spawn_blocking` wrap end-to-end (the reject test returns
    // before it). A clean switch must succeed and repoint the live link.
    let result = call_switch("target");
    assert_ne!(
        result.is_error,
        Some(true),
        "a clean switch to a known profile must succeed through the spawn_blocking wrap",
    );

    let live: ClaudeCredentials =
        read_json_file(&claude_dir().expect("claude dir").join(".credentials.json"))
            .expect("read live creds");
    assert_eq!(
        live.refresh_token(),
        Some("target-r"),
        "the switch ends with the active link pointing at target's stored creds",
    );

    // The description promises "the reply says which case this session is in":
    // the session-effect note rides the success arm through the same renderer
    // the init block uses. Only the lead is pinned — which variant this
    // process earns depends on the runner's own `CLAUDE_CONFIG_DIR`.
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("switch reply text");
    assert!(
        text.contains("\n\nswitch_profile & this session: "),
        "a successful switch names what it does to THIS session: {text}",
    );
}
