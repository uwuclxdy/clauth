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
