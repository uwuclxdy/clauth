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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::StateLock;
use crate::profile::{ProfileName, atomic_write_600, clauth_dir, mkdir_700, read_toml_file};

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

    /// The active codex profile — the codex twin of `AppState.active_profile`,
    /// with no bearing on the claude slot.
    pub(crate) fn active_profile(&self) -> Option<&ProfileName> {
        self.active_profile.as_ref()
    }

    /// Exact-match membership, same semantics as `AppConfig::find` answering
    /// `is_some`.
    pub(crate) fn holds(&self, name: &str) -> bool {
        self.profiles.iter().any(|n| n.as_str() == name)
    }

    /// Case-insensitive lookup returning the canonical casing — the codex twin
    /// of `AppConfig::canonical_name`, for CLI name resolution.
    pub(crate) fn canonical_name(&self, query: &str) -> Option<String> {
        self.profiles
            .iter()
            .find(|n| n.eq_ignore_ascii_case(query))
            .map(|n| n.as_str().to_string())
    }

    /// Move (or clear) the active marker. Callers resolve membership first;
    /// under [`CodexState::update`] that resolution is re-made against the
    /// state this closure was handed, which was loaded under the lock.
    pub(crate) fn set_active(&mut self, name: Option<&str>) {
        self.active_profile = name.map(ProfileName::from);
    }

    /// Remove `name` from every slot — roster, fallback chain, and the active
    /// marker — the codex twin of `AppConfig::remove`.
    pub(crate) fn remove_profile(&mut self, name: &str) {
        self.profiles.retain(|n| n.as_str() != name);
        self.fallback_chain.retain(|n| n.as_str() != name);
        if self.active_profile.as_deref() == Some(name) {
            self.active_profile = None;
        }
    }

    /// The one mutation path: load under the state lock, hand the closure the
    /// on-disk state, persist what it left behind. Holding the lock across
    /// load → mutate → save is what makes a concurrent writer impossible to
    /// clobber — there is no way to persist a pre-acquisition snapshot,
    /// because [`CodexState::save`] is unreachable outside this function.
    /// Filesystem work that must land atomically with the state change (a dir
    /// removal, a rename) belongs inside the closure, which runs under the
    /// same hold.
    pub(crate) fn update<T>(f: impl FnOnce(&mut CodexState) -> Result<T>) -> Result<T> {
        let lock = StateLock::acquire()?;
        let mut state = Self::load()?;
        let out = f(&mut state)?;
        state.save(&lock)?;
        Ok(out)
    }

    /// Persist, witness-gated: the `StateLock` parameter is proof the caller
    /// holds the cross-process state lock, and the only caller is
    /// [`CodexState::update`], which acquired it around the load.
    fn save(&self, _witness: &StateLock) -> Result<()> {
        mkdir_700(&clauth_dir()?)?;
        atomic_write_600(&codex_state_path()?, toml::to_string_pretty(self)?)
            .context("failed to write codex-profiles.toml")
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
