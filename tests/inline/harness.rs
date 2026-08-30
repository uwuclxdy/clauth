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

#[test]
fn the_codex_engine_carries_its_own_spawn_facts() {
    let engine: &dyn HarnessEngine = &CodexEngine;
    assert_eq!(engine.home_env_key(), "CODEX_HOME");
    assert_eq!(
        engine.command().get_program(),
        crate::runtime::codex_command().get_program()
    );
    let err = engine
        .install_credentials("cx")
        .expect_err("a codex switch installs nothing — the seam refuses by contract");
    assert_eq!(
        err.to_string(),
        "codex profile 'cx' installs nothing at switch — sessions bind auth.json at start"
    );
}

/// The codex scrub drops its managed keys and the claude actives, and strips
/// an inherited CLAUDE_CONFIG_DIR only when it names a tree clauth built — an
/// operator's own custom dir is not clauth's to strip.
#[test]
fn the_codex_scrub_is_managed_keys_plus_clauth_runtime_hygiene() {
    let home = crate::testutil::HomeSandbox::new();
    let engine: &dyn HarnessEngine = &CodexEngine;

    let runtime_dir = home.home().join(".clauth/profiles/started/runtime-4242-0");
    {
        let _env = crate::testutil::ConfigDirSandbox::new(&home, &runtime_dir);
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &["A_KEY".to_string()]);
        let env = crate::testutil::env_overrides(&cmd);
        assert_eq!(env.get("CODEX_HOME"), Some(&None));
        assert_eq!(env.get("OPENAI_API_KEY"), Some(&None));
        assert_eq!(
            env.get("CODEX_SQLITE_HOME"),
            Some(&None),
            "an inherited state-DB home would pool every profile's DBs in one dir"
        );
        for carrier in ["CODEX_API_KEY", "CODEX_ACCESS_TOKEN"] {
            assert_eq!(
                env.get(carrier),
                Some(&None),
                "{carrier} outranks the linked auth.json in codex's own load_auth order"
            );
        }
        for endpoint in [
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
            "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
        ] {
            assert_eq!(
                env.get(endpoint),
                Some(&None),
                "{endpoint} would spend the profile's single-use chain elsewhere"
            );
        }
        assert_eq!(env.get("A_KEY"), Some(&None));
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR"),
            Some(&None),
            "an inherited clauth runtime claim is scrubbed from a codex spawn"
        );
    }
    {
        let _env = crate::testutil::ConfigDirSandbox::new(&home, &home.home().join("custom-dir"));
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &[]);
        let env = crate::testutil::env_overrides(&cmd);
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR"),
            None,
            "an operator's custom dir is left alone"
        );
    }
}

/// The claude engine's mirror hygiene: an inherited clauth codex home is
/// scrubbed from a claude spawn, a foreign CODEX_HOME is not.
#[test]
fn the_claude_scrub_strips_only_a_clauth_codex_home() {
    let home = crate::testutil::HomeSandbox::new();
    let engine: &dyn HarnessEngine = &ClaudeEngine;

    let codex_home = home.home().join(".clauth/profiles/cx/codex-home-4242-0");
    {
        let _env = crate::testutil::CodexHomeSandbox::new(&home, &codex_home);
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &[]);
        assert_eq!(
            crate::testutil::env_overrides(&cmd).get("CODEX_HOME"),
            Some(&None),
            "a clauth codex home claim is scrubbed from a claude spawn"
        );
    }
    {
        let _env = crate::testutil::CodexHomeSandbox::new(&home, &home.home().join(".codex"));
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &[]);
        assert_eq!(
            crate::testutil::env_overrides(&cmd).get("CODEX_HOME"),
            None,
            "the operator's own CODEX_HOME is not clauth's to strip"
        );
    }
}
