//! The harness axis: which coding CLI a profile's credentials drive.
//!
//! A profile's harness is implied by WHICH STATE FILE it lives in — claude
//! profiles in `profiles.toml` ([`crate::profile::AppState`]), codex profiles
//! in `codex-profiles.toml` ([`crate::codex_profiles::CodexState`]) — never by
//! a field inside `AppState`, a dir-name suffix, or a load-order handshake.
//! File membership is what an old binary cannot misread: it never opens the
//! codex file, so it cannot drop or corrupt codex state, and `profiles.toml`
//! keeps its exact pre-codex meaning. This enum is only the in-memory answer
//! to "which file did this name come from"; converting a profile across
//! harnesses is delete + recreate, not a flag flip.

use serde::{Deserialize, Serialize};

/// The CLI a profile's stored credentials belong to. Carried on surfaces that
/// outlive one process (live-session rows) as a lowercase string, with
/// [`Harness::Claude`] the serde default so rows written before the axis
/// existed keep meaning what they meant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Harness {
    /// Claude Code — `profiles.toml`, `.credentials.json`, `CLAUDE_CONFIG_DIR`.
    #[default]
    Claude,
    /// OpenAI codex — `codex-profiles.toml`, `auth.json`, `CODEX_HOME`.
    Codex,
}

impl Harness {
    /// The user-facing spelling, for refusals that must name which harness
    /// holds a name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// The tag's behavior — the bridge every dispatch site crosses from
    /// "which file held the name" to "what runs for it".
    pub(crate) fn engine(self) -> &'static dyn HarnessEngine {
        match self {
            Harness::Claude => &ClaudeEngine,
            Harness::Codex => &CodexEngine,
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The two seams where harness-specific behavior sits behind one shape —
/// credential-install and runtime-spawn — per the codex plan's architecture
/// section. Everything else the plan touches stays an inline `match harness`
/// at its own site (lower claude-regression risk); only these two multiply
/// across every spawn and switch path, which is what earns them a trait.
///
/// Phase 2 puts claude behind the seams verbatim — every method delegates to
/// the fn that held the behavior before, and the callers bind through the
/// trait so the codex engine (arriving with the codex runtime) plugs into
/// call sites that already dispatch.
pub(crate) trait HarnessEngine {
    // ── credential-install ──
    /// Switch-time install of `name`'s stored credentials into this harness's
    /// live slot, with the refuse-guard the non-force path carries. Claude:
    /// the `.credentials.json` link machinery plus the macOS Keychain mirror.
    fn install_credentials(&self, name: &str) -> anyhow::Result<()>;
    /// The force flavor — same install, divergence guard bypassed.
    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()>;

    // ── runtime-spawn ──
    /// The resolved CLI command (Windows shim resolution included).
    fn command(&self) -> std::process::Command;
    /// The env var that pins a spawned session to its clauth-built home.
    fn home_env_key(&self) -> &'static str;
    /// Drop from `command`'s inherited env every key that must reach the
    /// session only through its own home — this harness's managed set plus
    /// the active profile's custom keys.
    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]);
}

/// Claude Code behind the seams: pure delegation, no behavior of its own.
pub(crate) struct ClaudeEngine;

impl HarnessEngine for ClaudeEngine {
    fn install_credentials(&self, name: &str) -> anyhow::Result<()> {
        crate::claude::link_profile_credentials(&crate::profile::ProfileName::from(name))
    }

    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()> {
        crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from(name))
    }

    fn command(&self) -> std::process::Command {
        crate::runtime::claude_command()
    }

    fn home_env_key(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]) {
        crate::runtime::scrub_profile_env(command, active_env_keys);
        // Cross-harness hygiene: a claude session started from inside a
        // clauth codex session inherits that session's CODEX_HOME, and a
        // `codex` run from the claude shell would then land in another
        // profile's home. Scrubbed only when the value names a home clauth
        // built — an operator's own CODEX_HOME is not clauth's to strip.
        if std::env::var_os("CODEX_HOME")
            .is_some_and(|v| crate::runtime::is_codex_home_path(std::path::Path::new(&v)))
        {
            command.env_remove("CODEX_HOME");
        }
    }
}

/// codex behind the seams. The spawn half delegates to the codex runtime; the
/// install half REFUSES by contract — a codex switch is a state slot and
/// sessions bind `auth.json` at start, so no flow may ask this engine to
/// install anything, and a caller that does has dispatched a claude flow onto
/// a codex tag.
pub(crate) struct CodexEngine;

/// The env keys a spawned codex session must receive only through its own
/// home, never inherited: `CODEX_HOME` itself (the parent's pin — exactly the
/// key this spawn is about to set right), and `OPENAI_API_KEY` (codex treats a
/// bare key in the env as an auth mode of its own, over the linked
/// `auth.json`).
const CODEX_MANAGED_ENV_KEYS: &[&str] = &["CODEX_HOME", "OPENAI_API_KEY"];

impl HarnessEngine for CodexEngine {
    fn install_credentials(&self, name: &str) -> anyhow::Result<()> {
        anyhow::bail!(
            "codex profile '{name}' installs nothing at switch — sessions bind auth.json at start"
        )
    }

    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()> {
        self.install_credentials(name)
    }

    fn command(&self) -> std::process::Command {
        crate::runtime::codex_command()
    }

    fn home_env_key(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]) {
        for key in CODEX_MANAGED_ENV_KEYS {
            command.env_remove(key);
        }
        for key in active_env_keys {
            command.env_remove(key);
        }
        // The mirror of ClaudeEngine's CODEX_HOME hygiene: a codex session
        // started from inside a clauth claude session inherits
        // CLAUDE_CONFIG_DIR, and `clauth which` run in the codex session
        // would answer as the ancestor claude session (the runtime claim
        // deliberately outranks the codex arm). Scrubbed only when it names a
        // tree clauth built — a custom operator dir is left alone.
        if std::env::var_os("CLAUDE_CONFIG_DIR")
            .is_some_and(|v| crate::runtime::is_clauth_runtime_path(std::path::Path::new(&v)))
        {
            command.env_remove("CLAUDE_CONFIG_DIR");
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/harness.rs"]
mod tests;
