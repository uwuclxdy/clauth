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
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
