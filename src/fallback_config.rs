//! Fallback-chain configuration edits — changing chain membership, order,
//! per-member thresholds, and wrap-off mode, together with the persistence each
//! edit requires (chain/order/wrap-off → `profiles.toml` via [`save_app_state`];
//! a threshold → that profile's `config.toml` via [`save_profile`]). One home
//! for "what an edit means" (seed a default threshold on add, clamp to 0..=100,
//! which file to write). Callers hold the config lock; these are pure edits over
//! `&mut AppConfig` plus their disk writes.
//!
//! Used by the daemon's control socket (`clauthd.sock`) so a menu-bar app can
//! configure the chain. The TUI's own fallback editor in `tui/app.rs` predates
//! this module and performs the equivalent mutations inline; migrating it to call
//! these primitives (so there is a single implementation) is a documented
//! follow-up, not done here to keep this change scoped to the socket path.

use anyhow::{Context, Result, bail};

use crate::fallback::DEFAULT_THRESHOLD;
use crate::profile::{AppConfig, profile_dir, save_profile, update_app_state};

/// Direction for [`move_member`] within the ordered chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveDir {
    Up,
    Down,
}

impl MoveDir {
    /// Parse the socket wire value (`"up"` / `"down"`), case-insensitively.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "up" => Some(MoveDir::Up),
            "down" => Some(MoveDir::Down),
            _ => None,
        }
    }
}

/// Each edit returns `Ok(true)` when it (re)wrote `profiles.toml` — i.e. changed
/// the persisted app state (chain membership/order, wrap-off) — and `Ok(false)`
/// when it made no such write (a no-op, or a threshold edit that touches only the
/// profile's own `config.toml`). The daemon uses this to bump its
/// `last_reload_fp` *only* after a real `profiles.toml` write, so an unrelated
/// external edit in the same tick isn't silently skipped. Every edit is also
/// transactional against its own write: on a save failure the in-memory mutation
/// is rolled back, so `AppConfig` never diverges from disk.
///
/// Add `name` to the end of the fallback chain, seeding a default threshold when
/// the profile has none. No-op if already a member. Errors when `name` resolves
/// to no known profile.
pub(crate) fn add(config: &mut AppConfig, name: &str) -> Result<bool> {
    let canonical = resolve(config, name)?;
    // CDX-4 C1: chains are per-harness (T1b invariant, now a ROUTE instead of
    // a refusal) — a codex profile joins `codex_fallback_chain`, a claude one
    // `fallback_chain`. Homogeneity holds by construction: the harness picks
    // the chain, so neither chain can hold the other kind.
    let codex = config.find(&canonical).is_some_and(|p| p.is_codex());
    if chain_of(config, codex)
        .iter()
        .any(|n| n.as_str() == canonical)
    {
        return Ok(false);
    }
    // Seed a default threshold if unset, persisting config.toml first; roll the
    // in-memory field back if that write fails.
    if let Some(profile) = config.find_mut(&canonical)
        && profile.fallback_threshold.is_none()
    {
        profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
        if let Err(e) = save_profile(profile) {
            profile.fallback_threshold = None;
            return Err(e);
        }
    }
    chain_of_mut(&mut config.state, codex).push(canonical.as_str().into());
    // TECH-7: merge the chain-append delta into the latest on-disk state so a
    // concurrent switch's `active_profile` (or a login's appended profile) is
    // preserved rather than clobbered by a blind rewrite.
    let canon = canonical.clone();
    if let Err(e) = update_app_state(move |s| {
        let chain = chain_of_mut(s, codex);
        if !chain.iter().any(|n| n.as_str() == canon) {
            chain.push(canon.as_str().into());
        }
    }) {
        chain_of_mut(&mut config.state, codex).pop();
        return Err(e);
    }
    Ok(true)
}

/// The harness-matched chain, read side. CDX-4 C1: every membership edit
/// routes through this pair so the two chains cannot cross-contaminate.
fn chain_of(config: &AppConfig, codex: bool) -> &Vec<crate::profile::ProfileName> {
    if codex {
        &config.state.codex_fallback_chain
    } else {
        &config.state.fallback_chain
    }
}

/// The harness-matched chain, write side — usable both on the in-memory state
/// and inside the `update_app_state` merge closure.
fn chain_of_mut(
    state: &mut crate::profile::AppState,
    codex: bool,
) -> &mut Vec<crate::profile::ProfileName> {
    if codex {
        &mut state.codex_fallback_chain
    } else {
        &mut state.fallback_chain
    }
}

/// Rename a profile: canonical `old` → validated `new`. Renames the on-disk
/// profile directory, updates every in-memory + on-disk reference (name list,
/// fallback chain, active marker, auth-broken set) through the RMW delta so a
/// concurrent switch isn't clobbered (TECH-7), and re-links the credential mirror
/// when the renamed profile is active — same tokens, new dir, so the live session
/// is untouched (macOS reads the Keychain, not this file). `Ok(true)` on a real
/// rename, `Ok(false)` for a no-op (`new` == `old`), `Err` on an invalid/taken name
/// or a failed directory rename.
pub(crate) fn rename(config: &mut AppConfig, old: &str, new: &str) -> Result<bool> {
    let canonical = resolve(config, old)?;
    let new = new.trim().to_string();
    // Charset + collision (excluding the profile being renamed, so a case-only
    // self-rename is allowed). Belt-and-suspenders with the socket's own check.
    crate::actions::validate_profile_name(&new, &config.names(), Some(canonical.as_str()))?;
    if new == canonical {
        return Ok(false);
    }

    // Rename the directory first: if it fails, no state has changed yet.
    let old_dir = profile_dir(&canonical)?;
    if old_dir.exists() {
        std::fs::rename(&old_dir, profile_dir(&new)?)
            .with_context(|| format!("failed to rename profile directory to '{new}'"))?;
    }

    let was_active = config.is_active(&canonical);
    config.rename_all_occurrences(&canonical, &new);

    // Merge the same rename into the latest on-disk state (TECH-7 — never blind-write
    // a concurrent switch's active_profile).
    let (from, to) = (canonical.clone(), new.clone());
    if let Err(e) = update_app_state(move |s| {
        for slot in s
            .profiles
            .iter_mut()
            .chain(s.fallback_chain.iter_mut())
            .chain(s.codex_fallback_chain.iter_mut())
            .chain(s.auth_broken.iter_mut())
        {
            if slot.as_str() == from {
                *slot = to.as_str().into();
            }
        }
        if s.active_profile
            .as_ref()
            .is_some_and(|n| n.as_str() == from)
        {
            s.active_profile = Some(to.as_str().into());
        }
        // CDX slots (wave-2 fix — the in-memory rename covered these, the
        // disk merge didn't, stranding a stale name in profiles.toml).
        if s.active_codex_profile
            .as_ref()
            .is_some_and(|n| n.as_str() == from)
        {
            s.active_codex_profile = Some(to.as_str().into());
        }
    }) {
        // Roll back the directory rename + in-memory state so disk and memory agree.
        let _ = std::fs::rename(profile_dir(&new)?, profile_dir(&canonical)?);
        config.rename_all_occurrences(&new, &canonical);
        return Err(e);
    }

    // Re-link the credential mirror to the renamed (active) profile's new dir.
    if was_active {
        crate::claude::link_profile_credentials(&new)?;
    }
    Ok(true)
}

/// Remove `name` from the chain. No-op (no write) if not a member. Errors when
/// `name` resolves to no known profile.
pub(crate) fn remove(config: &mut AppConfig, name: &str) -> Result<bool> {
    let canonical = resolve(config, name)?;
    let codex = config.find(&canonical).is_some_and(|p| p.is_codex());
    let Some(pos) = chain_of(config, codex)
        .iter()
        .position(|n| n.as_str() == canonical)
    else {
        return Ok(false);
    };
    let removed = chain_of_mut(&mut config.state, codex).remove(pos);
    // TECH-7: merge the removal delta into the latest on-disk state.
    let canon = canonical.clone();
    if let Err(e) = update_app_state(move |s| {
        chain_of_mut(s, codex).retain(|n| n.as_str() != canon);
    }) {
        chain_of_mut(&mut config.state, codex).insert(pos, removed);
        return Err(e);
    }
    Ok(true)
}

/// Move `name` one slot in `dir`. No-op (no write) at a boundary or when not a
/// member. Errors when `name` resolves to no known profile.
pub(crate) fn move_member(config: &mut AppConfig, name: &str, dir: MoveDir) -> Result<bool> {
    let canonical = resolve(config, name)?;
    let codex = config.find(&canonical).is_some_and(|p| p.is_codex());
    let Some(pos) = chain_of(config, codex)
        .iter()
        .position(|n| n.as_str() == canonical)
    else {
        return Ok(false);
    };
    let target = match dir {
        MoveDir::Up => pos.checked_sub(1),
        MoveDir::Down => Some(pos + 1).filter(|t| *t < chain_of(config, codex).len()),
    };
    let Some(target) = target else {
        return Ok(false);
    };
    chain_of_mut(&mut config.state, codex).swap(pos, target);
    // TECH-7: merge the move into the latest on-disk state, recomputing the
    // position on disk (its chain may differ from our snapshot) so we express the
    // intent "move `canonical` one slot in `dir`" rather than a stale positional swap.
    let canon = canonical.clone();
    if let Err(e) = update_app_state(move |s| {
        let chain = chain_of_mut(s, codex);
        if let Some(p) = chain.iter().position(|n| n.as_str() == canon) {
            let t = match dir {
                MoveDir::Up => p.checked_sub(1),
                MoveDir::Down => Some(p + 1).filter(|t| *t < chain.len()),
            };
            if let Some(t) = t {
                chain.swap(p, t);
            }
        }
    }) {
        chain_of_mut(&mut config.state, codex).swap(pos, target);
        return Err(e);
    }
    Ok(true)
}

/// Set `name`'s 5h auto-switch threshold, clamped to `0..=100`. Writes only the
/// profile's `config.toml`, so it returns `Ok(false)` (no `profiles.toml` write).
/// Errors when `name` resolves to no known profile.
pub(crate) fn set_threshold(config: &mut AppConfig, name: &str, value: f64) -> Result<bool> {
    let canonical = resolve(config, name)?;
    let clamped = value.clamp(0.0, 100.0);
    match config.find_mut(&canonical) {
        Some(profile) => {
            let previous = profile.fallback_threshold;
            profile.fallback_threshold = Some(clamped);
            if let Err(e) = save_profile(profile) {
                profile.fallback_threshold = previous;
                return Err(e);
            }
            Ok(false)
        }
        None => bail!("unknown profile '{name}'"),
    }
}

/// Set or clear `name`'s exclusive `last_resort` mark (the walk's sink pass
/// accepts a marked member even while exhausted; independent of `threshold` —
/// a member can switch away at 80% and still be the chain's last resort).
/// Writes only the profile's `config.toml`, so it returns `Ok(false)`.
/// Errors when `name` resolves to no known profile.
pub(crate) fn set_last_resort(config: &mut AppConfig, name: &str, on: bool) -> Result<bool> {
    let canonical = resolve(config, name)?;
    match config.find_mut(&canonical) {
        Some(profile) => {
            let previous = profile.last_resort;
            profile.last_resort = on;
            if let Err(e) = save_profile(profile) {
                profile.last_resort = previous;
                return Err(e);
            }
            Ok(false)
        }
        None => bail!("unknown profile '{name}'"),
    }
}

/// Set or clear `name`'s per-account weekly-line override (`weekly at` on the
/// Fallback tab; the chain-wide `weekly_switch_threshold` stays the default).
/// `None` clears back to the default. Writes only the profile's `config.toml`,
/// so it returns `Ok(false)`. Errors when `name` resolves to no known profile.
pub(crate) fn set_member_weekly(
    config: &mut AppConfig,
    name: &str,
    value: Option<f64>,
) -> Result<bool> {
    let canonical = resolve(config, name)?;
    let clamped = value.map(|v| v.clamp(0.0, 100.0));
    match config.find_mut(&canonical) {
        Some(profile) => {
            let previous = profile.weekly_threshold;
            profile.weekly_threshold = clamped;
            if let Err(e) = save_profile(profile) {
                profile.weekly_threshold = previous;
                return Err(e);
            }
            Ok(false)
        }
        None => bail!("unknown profile '{name}'"),
    }
}

/// Flip one of `name`'s per-account usage gates (`check_weekly` /
/// `check_scoped` — the Fallback tab's `weekly gate` / `scoped gate`).
/// Writes only the profile's `config.toml`, so it returns `Ok(false)`.
/// Errors when `name` resolves to no known profile.
pub(crate) fn set_usage_gate(
    config: &mut AppConfig,
    name: &str,
    scoped: bool,
    on: bool,
) -> Result<bool> {
    let canonical = resolve(config, name)?;
    match config.find_mut(&canonical) {
        Some(profile) => {
            let field: &mut bool = if scoped {
                &mut profile.check_scoped
            } else {
                &mut profile.check_weekly
            };
            let previous = *field;
            *field = on;
            if let Err(e) = save_profile(profile) {
                let field: &mut bool = if scoped {
                    &mut profile.check_scoped
                } else {
                    &mut profile.check_weekly
                };
                *field = previous;
                return Err(e);
            }
            Ok(false)
        }
        None => bail!("unknown profile '{name}'"),
    }
}

/// Toggle wrap-off mode (switch every account off once the whole chain is spent,
/// rather than staying on the last one) and persist.
pub(crate) fn set_wrap_off(config: &mut AppConfig, on: bool) -> Result<bool> {
    let previous = config.state.switch_off_when_spent;
    config.state.switch_off_when_spent = on;
    // TECH-7: merge the wrap_off delta into the latest on-disk state.
    if let Err(e) = update_app_state(move |s| s.switch_off_when_spent = on) {
        config.state.switch_off_when_spent = previous;
        return Err(e);
    }
    Ok(true)
}

/// Set the chain-wide weekly (7d) exhaustion line and persist. Validated
/// against the legal band here (single write-side gate shared by the TUI and
/// the socket); `Ok(false)` = no-op, the value was already set.
pub(crate) fn set_weekly_threshold(config: &mut AppConfig, value: f64) -> Result<bool> {
    use crate::profile::{MAX_WEEKLY_SWITCH_PCT, MIN_WEEKLY_SWITCH_PCT};
    if !(MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT).contains(&value) {
        bail!(
            "weekly threshold must be within {MIN_WEEKLY_SWITCH_PCT}..={MAX_WEEKLY_SWITCH_PCT}, got {value}"
        );
    }
    let previous = config.state.weekly_switch_threshold;
    if previous == Some(value) {
        return Ok(false);
    }
    config.state.weekly_switch_threshold = Some(value);
    // TECH-7: merge the delta into the latest on-disk state.
    if let Err(e) = update_app_state(move |s| s.weekly_switch_threshold = Some(value)) {
        config.state.weekly_switch_threshold = previous;
        return Err(e);
    }
    Ok(true)
}

/// Resolve a raw/case-insensitive profile name to its canonical form, erroring
/// when it names no known profile.
fn resolve(config: &AppConfig, name: &str) -> Result<String> {
    config
        .canonical_name(name)
        .ok_or_else(|| anyhow::anyhow!("unknown profile '{name}'"))
}

#[cfg(test)]
#[path = "../tests/inline/fallback_config.rs"]
mod tests;
