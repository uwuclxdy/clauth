#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The claude engine is delegation — behavior is gated by the flows that run
//! through it (the switch and spawn suites) — so what is pinned here are the
//! wire facts a seam must not let drift.

use super::*;

#[test]
fn the_claude_engine_carries_the_spawn_facts_unchanged() {
    let engine: &dyn HarnessEngine = &ClaudeEngine;
    assert_eq!(engine.home_env_key(), "CLAUDE_CONFIG_DIR");
    assert_eq!(
        engine.command().get_program(),
        crate::runtime::claude_command().get_program(),
        "one command resolution behind the seam, not a second spelling"
    );
}

/// The scrub through the seam is the shared scrub: managed keys and the
/// active profile's custom keys are dropped from the child env, everything
/// else inherits.
#[test]
fn the_claude_scrub_is_the_shared_scrub() {
    let engine: &dyn HarnessEngine = &ClaudeEngine;
    let mut cmd = std::process::Command::new("probe");
    cmd.env("ANTHROPIC_BASE_URL", "https://a")
        .env("UNRELATED", "1");
    engine.scrub_env(&mut cmd, &["MY_CUSTOM".to_string()]);

    let env = crate::testutil::env_overrides(&cmd);
    assert_eq!(
        env.get("ANTHROPIC_BASE_URL"),
        Some(&None),
        "a managed key is scrubbed even when explicitly set"
    );
    assert_eq!(
        env.get("MY_CUSTOM"),
        Some(&None),
        "the active profile's custom key is scrubbed from the inherited env"
    );
    assert_eq!(
        env.get("UNRELATED"),
        Some(&Some("1".to_string())),
        "an unmanaged key rides through untouched"
    );
}
