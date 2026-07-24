//! Cross-profile `settings.json` synchronizer.
//!
//! `runtime::write_merged_settings` computes each profile's runtime
//! `settings.json` once per `clauth start`, from the `~/.claude/settings.json`
//! base plus that profile's `config.toml` overrides. Without reconciliation a
//! setting the user changes inside a live Claude Code session lands only in that
//! profile's runtime copy: the next start rebuilds from the base and the change
//! is gone, and no sibling profile ever sees it.
//!
//! This module reconciles the same member set the credentials watchdog already
//! ticks: the operator's `~/.claude/settings.json` plus every SHARED profile
//! runtime copy. Newest parseable copy wins for the shared fields; each member
//! keeps its own per-profile fields ([`key_role`]). The merge machinery is
//! [`crate::jsonsync`], shared with the `.claude.json` reconciler.
//!
//! **The base is a sync member, and the write-back target.** Two consequences
//! drive the design:
//!
//! - Writing the winner's shared fields back into `~/.claude/settings.json` is
//!   what stops a thrash: the next `clauth start` recomputes from an
//!   already-updated base and reproduces the same bytes, so the recompute and
//!   the sync agree instead of overwriting each other. A runtimes-only sync
//!   would be undone on every start.
//! - `~/.claude/settings.json` is NOT a pristine operator base.
//!   `claude::apply_profile_to_claude_settings` rewrites the ACTIVE profile's
//!   managed env keys, `apiKeyHelper`, and `model` into that very file on every
//!   switch. It is a member carrying its own per-profile fields, so those must
//!   neither propagate outward nor be overwritten by a sibling's — which the
//!   symmetric [`key_role`] rule gives for free.
//!
//! With no live `clauth start` session there is nothing to reconcile and nothing
//! to lose: teardown removes `runtime/`, so the base is the only surviving
//! member and the engine's `members.len() < 2` short-circuit makes a sync a
//! no-op. That is why this runs on the session watchdog and needs no daemon
//! tick — a headless box has no runtime copy that could diverge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use anyhow::Result;
use serde::Deserialize;

use crate::jsonsync::{KeyPath, KeyRule};
use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::{claude_dir, clauth_dir};
use crate::runtime::MANAGED_ENV_KEYS;

/// Top-level `settings.json` keys every member keeps as its own. See
/// [`key_role`] for the criterion.
///
/// The helper/refresh/export commands are one family in Claude Code's own
/// settings schema (verified against the 2.1.215 binary's zod block, where they
/// sit adjacent to `apiKeyHelper`): each names an executable CC runs to MINT a
/// credential, so propagating one hands a sibling account the command that
/// prints another account's secret. The `forceLogin*` trio scopes an OAuth login
/// to one account — `forceLoginOrgUUID` in particular makes login FAIL when the
/// authenticated account is not the named org, so propagating it breaks every
/// sibling.
///
/// Two neighbours in that same schema block are deliberately NOT here.
/// `processWrapper` is a corporate launcher argv prefix (machine-wide, and CC
/// documents it as equivalent to a `CLAUDE_CODE_PROCESS_WRAPPER` env var), and
/// `policyHelper` computes managed settings and is "honored only from
/// admin-controlled policy sources" — org-scoped rather than account-scoped, so
/// they sync like any other operator preference.
const PER_PROFILE_TOP_FIELDS: &[&str] = &[
    "apiKeyHelper",
    "proxyAuthHelper",
    "awsCredentialExport",
    "awsAuthRefresh",
    "gcpAuthRefresh",
    "otelHeadersHelper",
    "forceLoginMethod",
    "forceLoginGatewayUrl",
    "forceLoginOrgUUID",
    "model",
];

/// Newest mtime from the last [`sync_once`] that did work. Short-circuits ticks
/// where no file is newer — no reads, parses, or writes.
static LAST_SYNCED: Mutex<Option<SystemTime>> = Mutex::new(None);

/// Latch for [`warn_paused`], so the pause is reported once rather than on every
/// watchdog tick. Cleared by the next clean [`per_profile_env_keys`] read.
static ENV_KEYS_WARNED: AtomicBool = AtomicBool::new(false);

/// The single decision point for what a `settings.json` key belongs to. Every
/// per-profile key, top-level or nested, is named here and nowhere else.
///
/// **Criterion:** a key is per-profile when its value identifies, authenticates,
/// or routes ONE account — an endpoint, a credential (or the command that mints
/// one), or a model choice. Copying such a key into a sibling member points that
/// account's session at the wrong endpoint, spends the wrong key, or bills the
/// wrong model. Everything else `settings.json` holds is operator preference
/// (`hooks`, `permissions`, `statusLine`, theme, non-clauth env vars) and is
/// shared.
///
/// The set has two halves:
/// - top level ([`PER_PROFILE_TOP_FIELDS`]): `apiKeyHelper` names the profile
///   whose `config.toml` holds the raw key, so propagating that one string hands
///   one account's key to another without any `env` entry moving; `model` is
///   routing.
/// - inside `env`, key by key rather than skipping the whole object (the block
///   also carries plain shared vars): [`MANAGED_ENV_KEYS`] plus `custom_env`,
///   every key any profile declares in its own `config.toml` `[env]`.
///
/// `custom_env` is the union across ALL profiles, not just the member's own
/// owner, and that is REQUIRED — not merely the conservative choice.
/// `~/.claude/settings.json` carries the ACTIVE profile's custom env
/// (`claude::apply_profile_to_claude_settings` writes it there on every switch),
/// and `claude::build_claude_settings_json` strips exactly those keys back out
/// of a sibling's runtime copy via `prev_env_keys`. Scoping the set to a
/// member's own owner would rule the base's active-profile keys `Shared` and
/// propagate them into every sibling runtime — precisely the leak that strip
/// exists to prevent. The cost of the union is nil: a key one account declares
/// simply stays put in each member, and the next start re-derives it.
fn key_role(path: KeyPath<'_>, custom_env: &BTreeSet<String>) -> KeyRule {
    match path {
        KeyPath::Top("env") => KeyRule::Nested,
        KeyPath::Top(key) if PER_PROFILE_TOP_FIELDS.contains(&key) => KeyRule::PerProfile,
        KeyPath::Top(_) => KeyRule::Shared,
        KeyPath::Nested(key) => {
            if MANAGED_ENV_KEYS.contains(&key) || custom_env.contains(key) {
                KeyRule::PerProfile
            } else {
                KeyRule::Shared
            }
        }
    }
}

/// Every `settings.json` clauth reconciles: the operator's own
/// `~/.claude/settings.json` plus each profile's SHARED runtime copy.
///
/// `runtime-isolated/settings.json` is deliberately absent. An isolated runtime
/// is built from an EMPTY base (`runtime::write_merged_settings`), so it carries
/// none of the operator's hooks, permissions, or statusline; letting it win a
/// newest-wins race would delete all of them from the base and from every shared
/// copy, and letting it receive would defeat the isolation it exists for. The
/// two flavors live in separate directories (`runtime/` vs `runtime-isolated/`),
/// so excluding it is an exact path check, not a heuristic.
fn known_paths() -> Result<Vec<PathBuf>> {
    let mut paths = vec![claude_dir()?.join("settings.json")];
    let profiles = clauth_dir()?.join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) && !is_codex_profile_dir(&entry.path()) {
                paths.push(entry.path().join("runtime").join("settings.json"));
            }
        }
    }
    Ok(paths)
}

/// Harness gate (fork): a codex profile runs `codex` against its own isolated
/// CODEX_HOME — it has no claude `runtime/settings.json` to sync, and its
/// `config.toml` `[env]` belongs to the codex side, never to the managed key
/// set written into `~/.claude/settings.json`. Judged from the same
/// `harness = "codex"` line `profile::render_config_toml` writes; an unreadable
/// config reads as claude here — per-profile-env safety is separately handled
/// by [`per_profile_env_keys`]'s own fail-closed read of the same file.
fn is_codex_profile_dir(dir: &Path) -> bool {
    #[derive(Deserialize)]
    struct HarnessOnly {
        harness: Option<String>,
    }
    std::fs::read_to_string(dir.join("config.toml"))
        .ok()
        .and_then(|raw| toml::from_str::<HarnessOnly>(&raw).ok())
        .and_then(|c| c.harness)
        .is_some_and(|h| h == "codex")
}

/// Just the `[env]` table of a profile's `config.toml`. A dedicated minimal
/// shape rather than `profile::load_profile`, which also reads credentials, runs
/// pending-rotation recovery, and normalizes fields — none of which this needs,
/// and all of which would run on every tick that finds work.
#[derive(Deserialize)]
struct EnvOnlyConfig {
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Union of every profile's custom `[env]` keys. A missing `config.toml` is a
/// profile with no overrides and contributes nothing.
///
/// `None` when a `config.toml` cannot be read or parsed: that profile's
/// per-profile env set is then unknown, and [`sync_members`] skips the merge
/// rather than treat its keys as shared and leak them into a sibling.
/// Fail-closed, and self-healing — a config caught mid-edit reads cleanly on the
/// next tick. Because that pauses settings sync entirely, the first failure is
/// logged; the latch keeps the watchdog's ~10 Hz retry from flooding the log,
/// and clears on the next clean read so a recurrence is reported again.
fn per_profile_env_keys() -> Option<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    let profiles = clauth_dir().ok()?.join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else {
        return Some(keys);
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || is_codex_profile_dir(&entry.path()) {
            continue;
        }
        let path = entry.path().join("config.toml");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return warn_paused(&path, &format!("could not be read ({e})")),
        };
        match toml::from_str::<EnvOnlyConfig>(&raw) {
            Ok(config) => keys.extend(config.env.into_keys()),
            Err(e) => return warn_paused(&path, &format!("is not valid TOML ({e})")),
        }
    }
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
    Some(keys)
}

/// Report the first `config.toml` failure that pauses settings sync, then latch.
/// Always returns `None` so callers can `return warn_paused(..)` directly.
fn warn_paused(path: &Path, reason: &str) -> Option<BTreeSet<String>> {
    if !ENV_KEYS_WARNED.swap(true, Ordering::Relaxed) {
        logline!(
            "clauth: settings.json sync paused — {} {reason}; \
             that profile's custom [env] keys are unknown, so no settings are \
             synced until it reads cleanly",
            path.display()
        );
    }
    None
}

/// Reconcile every known `settings.json` once. Stat-only fast path: skips reads
/// when no file is newer than `LAST_SYNCED`. Advances `LAST_SYNCED` only when
/// the merge actually ran, so a paused or failed tick retries.
pub(crate) fn sync_once() -> Result<()> {
    let paths = known_paths()?;
    let Some(newest) = crate::jsonsync::newest_mtime(&paths) else {
        return Ok(());
    };
    {
        let last = LAST_SYNCED.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|l| newest <= l) {
            return Ok(());
        }
    }
    if !sync_members(&paths)? {
        return Ok(());
    }
    // `newest` was stated before the lock, so it can only lag what the merge
    // actually saw — costing at most one redundant tick, never a skipped one.
    *LAST_SYNCED.lock().unwrap_or_else(|p| p.into_inner()) = Some(newest);
    Ok(())
}

/// Merge the members under the cross-process state flock. Returns whether the
/// merge ran (`false` = paused on an unreadable `config.toml`).
///
/// The lock is what makes this safe against the switch writer.
/// `claude::apply_profile_to_claude_settings` rewrites the ACTIVE profile's
/// managed env keys, `apiKeyHelper`, and `model` into `~/.claude/settings.json`,
/// and `runtime::build_runtime_dir_with_active_env` rewrites a runtime copy —
/// both under this same lock. A switch landing between our read and our write
/// would otherwise be silently reverted to the value we read. Re-stat-then-write
/// would only narrow that window; taking the writers' own lock closes it, and
/// everything inside is local file IO, so the hold is short. The `sync_once`
/// fast path keeps a quiet tick from taking the lock at all.
///
/// The `[env]` classification is read INSIDE the same hold, and must stay there:
/// it has to describe the same instant as the member bytes the merge reads. A
/// set gathered before the lock can be stale by the time the merge runs — a
/// switch to a profile whose `[env]` just gained a key writes that key into
/// `~/.claude/settings.json` and makes it the newest member, and a classifier
/// that predates the edit rules the key `Shared` and copies it into every
/// sibling runtime. That leak does not self-heal: from the next tick the key IS
/// per-profile, so each sibling keeps the copy it should never have received.
/// The added cost is N small TOML parses inside a hold that already does N
/// `settings.json` reads, parses, and writes.
fn sync_members(paths: &[PathBuf]) -> Result<bool> {
    let operator_file = claude_dir()?.join("settings.json");
    with_state_lock(|| {
        let Some(custom_env) = per_profile_env_keys() else {
            return Ok(false);
        };
        crate::jsonsync::sync_paths(paths, Some(&operator_file), |path| {
            key_role(path, &custom_env)
        })?;
        Ok(true)
    })
}

#[cfg(test)]
#[path = "../tests/inline/settings_sync.rs"]
mod tests;
