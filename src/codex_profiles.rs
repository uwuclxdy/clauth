//! `~/.clauth/codex-profiles.toml` — the codex half of the profile roster.
//!
//! Codex profiles live in their own state file; claude profiles stay in
//! `profiles.toml` ([`crate::profile::AppState`]), which this module never
//! reads or writes. That file split IS the harness axis (see
//! [`crate::harness::Harness`]), and it is what makes back-compat trivial by
//! construction: `profiles.toml` keeps its exact pre-codex schema, and an OLD
//! binary never opens this file at all — so the mixed-version dual-write and
//! serde-drop hazards `profiles.toml` would have carried are dissolved, not
//! handled. (A NEWER binary's unknown keys are tolerated on load and dropped
//! on the next rewrite, exactly `profiles.toml`'s own contract.)
//!
//! One namespace spans both files: a profile name held by either harness is
//! taken (enforced by `actions::validate_profile_name`, which reads both
//! rosters itself). Uniqueness is what lets every name-keyed subsystem — the
//! live-session tally, the pending-switch set, the per-profile cache files,
//! `profiles/<name>/` itself — carry codex members without learning a second
//! key, and it is why the CLI grammar (pinned here for the whole series)
//! needs no `--codex` on name-taking verbs:
//!
//! - `clauth <name>` / `clauth delete <name>` — the bare name resolves against
//!   the claude roster first, then this one; membership decides the harness,
//!   nothing else has to. Claude-first also arbitrates the state uniqueness
//!   cannot promise: two hand-edited state files both claiming a name.
//! - `clauth login <name> --codex` — the create/re-auth verb (an adoption of
//!   the operator's own codex login); the flag picks which file the profile
//!   lives in and appears only there.
//! - rename follows the bare-name rule when its surface (the TUI) gains
//!   codex rows.
//!
//! Dirs are bare for both harnesses: a codex profile stores under
//! `profiles/<name>/` exactly like a claude one, so the whole path layer
//! (`profile_dir`, `profile_subpath`, `rotation_lock_path`, the session
//! markers) needs no harness awareness.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::StateLock;
use crate::profile::{
    DEFAULT_WEEKLY_SWITCH_PCT, MAX_WEEKLY_SWITCH_PCT, MIN_WEEKLY_SWITCH_PCT, ProfileId,
    ProfileName, atomic_write_600, clauth_dir, mkdir_700,
};

/// The codex roster and its per-harness slots: the same four fields
/// `profiles.toml` holds for claude — active marker, ordering, fallback chain,
/// wrap-off — plus the chain's own weekly line, scoped to codex sessions
/// alone. A codex switch writes this file and never `profiles.toml`; chains
/// are strictly per-harness.
///
/// Slots are PRIVATE, deliberately breaking with [`crate::profile::AppState`]'s
/// all-`pub(crate)` shape: every mutation goes through a writer on this type,
/// and persistence happens only inside [`CodexState::update`], which holds the
/// state lock across its load → mutate → save. That closes by construction the
/// stale-snapshot class the claude profile writers close with a fresh re-read
/// under the lock (load under the guard, mutate one field, never write a
/// pre-prompt snapshot back).
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
    /// The codex chain's own weekly (7d) exhaustion line, percent — the codex
    /// twin of [`crate::profile::AppState::weekly_switch_threshold`] under the
    /// same on-disk key, hand-editable only (the Fallback tab is claude-only).
    /// `None` = [`DEFAULT_WEEKLY_SWITCH_PCT`], and absent stays absent across a
    /// save: the key is never invented into a file that did not carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weekly_switch_threshold: Option<f64>,
    /// Each profile's [`ProfileId`], minted by [`CodexState::add_profile`] and
    /// dropped with the profile. A table beside the name list rather than
    /// records inside it, so an older binary still reads `profiles`; it drops
    /// this table on its own next save, and [`CodexState::claim_id`] mints
    /// afresh for the next login.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    ids: BTreeMap<String, ProfileId>,
}

/// Why a codex roster could not be loaded, with the parse position carried
/// apart from the io failure so a caller can render each by type. The parse
/// position is the one [`crate::format::toml_parse_position`] derives; a
/// non-parse failure carries the same anyhow chain [`CodexState::load`] would
/// surface.
#[derive(Debug)]
pub(crate) enum CodexRosterError {
    /// `codex_state_path()` failed: the roster path is unknowable.
    NoHome(anyhow::Error),
    /// The file exists but could not be read (an io failure).
    Read(anyhow::Error),
    /// The file read but is not valid toml; the position, when the error
    /// carries a span.
    Parse(Option<crate::format::ParsePosition>),
}

impl CodexState {
    /// Read the roster, lock-free — the reader's snapshot, same contract as
    /// `load_app_state`. A missing file is an empty roster: an install that
    /// has never created a codex profile has nothing to migrate and nothing
    /// to misread.
    pub(crate) fn load() -> Result<Self> {
        let path = codex_state_path()?;
        Self::load_with_position().map_err(|e| match e {
            CodexRosterError::Parse(pos) => crate::format::parse_error(path.display(), pos),
            CodexRosterError::Read(e) | CodexRosterError::NoHome(e) => e,
        })
    }

    /// The single read+normalize body, split by failure type so the roster
    /// logline can render a parse position and an io failure differently.
    /// [`CodexState::load`] maps this back to the anyhow shape every other
    /// caller wants.
    pub(crate) fn load_with_position() -> Result<Self, CodexRosterError> {
        let path = codex_state_path().map_err(CodexRosterError::NoHome)?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .map_err(CodexRosterError::Read)?;
        let mut state: Self = toml::from_str(&content).map_err(|e| {
            CodexRosterError::Parse(crate::format::toml_parse_position(&content, &e))
        })?;
        // Normalized at load the way `load_app_state` normalizes the claude
        // line: left raw, an out-of-band hand-edit would survive every save.
        // Through the accessor, so the band and its reset-to-default stay
        // defined in one place; `None` stays `None`, so `skip_serializing_if`
        // keeps omitting a key the file never carried.
        if state.weekly_switch_threshold.is_some() {
            state.weekly_switch_threshold = Some(state.weekly_switch_threshold_pct());
        }
        Ok(state)
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

    /// The codex fallback chain, in walk order — the codex twin of
    /// `AppState.fallback_chain`, read by [`crate::fallback::snapshot_codex_chain`].
    pub(crate) fn fallback_chain(&self) -> &[ProfileName] {
        &self.fallback_chain
    }

    /// Whether the codex chain switches OFF rather than wrapping when every
    /// member is spent — the codex twin of `AppState.switch_off_when_spent`.
    pub(crate) fn switch_off_when_spent(&self) -> bool {
        self.switch_off_when_spent
    }

    /// The effective codex weekly line: the configured value inside
    /// [`MIN_WEEKLY_SWITCH_PCT`]`..=`[`MAX_WEEKLY_SWITCH_PCT`], else the
    /// default — the same reset-not-clamp
    /// [`crate::profile::AppState::weekly_switch_threshold_pct`] applies, since
    /// a hand-edited `0.98` or `nan` must not silently disable the gate.
    pub(crate) fn weekly_switch_threshold_pct(&self) -> f64 {
        self.weekly_switch_threshold
            .filter(|v| (MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT).contains(v))
            .unwrap_or(DEFAULT_WEEKLY_SWITCH_PCT)
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

    /// Append `name` to the roster with a fresh [`ProfileId`]. Idempotent — a
    /// re-capture of an existing profile must not double its entry.
    pub(crate) fn add_profile(&mut self, name: &str) -> Result<()> {
        if !self.holds(name) {
            self.profiles.push(ProfileName::from(name));
            self.ids.insert(name.to_string(), ProfileId::mint()?);
        }
        Ok(())
    }

    /// The [`ProfileId`] of the profile `name` holds exactly, minted when an
    /// older binary left it without one; `None` when the roster lacks `name`.
    pub(crate) fn claim_id(&mut self, name: &str) -> Result<Option<ProfileId>> {
        if !self.holds(name) {
            return Ok(None);
        }
        if let Some(id) = self.ids.get(name) {
            return Ok(Some(id.clone()));
        }
        let id = ProfileId::mint()?;
        self.ids.insert(name.to_string(), id.clone());
        Ok(Some(id))
    }

    /// The [`ProfileId`] `name` holds exactly, if any.
    pub(crate) fn id(&self, name: &str) -> Option<&ProfileId> {
        self.ids.get(name)
    }

    /// Remove `name` from every slot — roster, fallback chain, the active
    /// marker and its id — the codex twin of `AppConfig::remove`.
    pub(crate) fn remove_profile(&mut self, name: &str) {
        self.ids.remove(name);
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
    ///
    /// Saves only when the closure changed something: an untouched state
    /// leaves the file's bytes AND mtime alone, so a no-op verb (switching to
    /// the already-active profile) neither rewrites a hand-edited file
    /// through this binary's serializer nor bumps the reload fingerprint.
    pub(crate) fn update<T>(f: impl FnOnce(&mut CodexState) -> Result<T>) -> Result<T> {
        let lock = StateLock::acquire()?;
        let mut state = Self::load()?;
        let before = state.clone();
        let out = f(&mut state)?;
        if state != before {
            state.save(&lock)?;
        }
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
