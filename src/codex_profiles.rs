//! `~/.clauth/codex-profiles.toml` — the codex half of the profile roster.
//!
//! Codex profiles live in their own state file; claude profiles stay in
//! `profiles.toml` ([`crate::profile::AppState`]), which this module never
//! reads or writes. That file split IS the harness axis (see
//! [`crate::harness::Harness`]), and it is what makes back-compat trivial by
//! construction: `profiles.toml` keeps its exact pre-codex schema, an old
//! binary never opens this file, and a mixed-version window has no dual-write
//! or serde-drop hazard anywhere — the failure class is dissolved, not
//! handled.
//!
//! One namespace spans both files: a profile name held by either harness is
//! taken (enforced by `actions::validate_profile_name`, which reads both
//! rosters itself). Uniqueness is what lets every name-keyed subsystem — the
//! live-session tally, the pending-switch set, the per-profile cache files,
//! `profiles/<name>/` itself — carry codex members without learning a second
//! key, and it is why the CLI grammar needs no `--codex` on name-taking verbs:
//!
//! - `clauth login <name> --codex` — create or re-authenticate; the flag picks
//!   which file the profile lives in and appears ONLY here.
//! - `clauth <name>` / `clauth delete <name>` — the bare name resolves against
//!   both rosters; membership decides the harness, nothing else has to.
//! - rename follows the same rule when its surface (the TUI) gains codex rows.
//!
//! Dirs are bare for both harnesses: a codex profile stores under
//! `profiles/<name>/` exactly like a claude one, so the whole path layer
//! (`profile_dir`, `profile_subpath`, `rotation_lock_path`, the session
//! markers) needs no harness awareness.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::profile::{ProfileName, clauth_dir, read_toml_file};

/// The codex roster and its per-harness slots: the same four fields
/// `profiles.toml` holds for claude — active marker, ordering, fallback chain,
/// wrap-off — scoped to codex sessions alone. A codex switch writes this file
/// and never `profiles.toml`; chains are strictly per-harness.
///
/// Slots are PRIVATE, deliberately breaking with [`crate::profile::AppState`]'s
/// all-`pub(crate)` shape: every mutation goes through a writer on this type,
/// and persistence happens only inside [`CodexState::update`], which holds the
/// state lock across its load → mutate → save. That closes by construction the
/// stale-snapshot class `save_profile` callers had to dodge by convention
/// (load under the guard, mutate one field, never write a pre-prompt snapshot
/// back).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct CodexState {
    active_profile: Option<ProfileName>,
    #[serde(default)]
    profiles: Vec<ProfileName>,
    #[serde(default)]
    fallback_chain: Vec<ProfileName>,
    /// Same meaning as [`crate::profile::AppState::switch_off_when_spent`],
    /// scoped to the codex chain — and the same on-disk spelling, so the two
    /// files stay hand-editable by one rule.
    #[serde(rename = "wrap_off", default)]
    switch_off_when_spent: bool,
}

impl CodexState {
    /// Read the roster, lock-free — the reader's snapshot, same contract as
    /// `load_app_state`. A missing file is an empty roster: an install that
    /// has never created a codex profile has nothing to migrate and nothing
    /// to misread.
    pub(crate) fn load() -> Result<Self> {
        let path = codex_state_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        read_toml_file(&path)
    }

    /// Every codex profile, in roster order.
    pub(crate) fn profiles(&self) -> &[ProfileName] {
        &self.profiles
    }
}

fn codex_state_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("codex-profiles.toml"))
}

/// Mtime of `codex-profiles.toml`, `None` when absent — the reload
/// fingerprint's stat, mirroring `app_state_mtime`.
pub(crate) fn codex_state_mtime() -> Option<SystemTime> {
    let path = codex_state_path().ok()?;
    std::fs::metadata(&path).ok()?.modified().ok()
}

#[cfg(test)]
#[path = "../tests/inline/codex_profiles.rs"]
mod tests;
