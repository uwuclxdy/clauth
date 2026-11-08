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
//! runtime copy. The highest-ranked member — each ranks by the mtime of the
//! Claude Code write its bytes carry — wins for the shared fields; each member
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
//!   managed env keys, `apiKeyHelper`, `model`, and `advisorModel` into that
//!   very file on every switch. It is a member carrying its own per-profile
//!   fields, so those must neither propagate outward nor be overwritten by a
//!   sibling's — which the symmetric [`key_role`] rule gives for free.
//!
//! With no live `clauth start` session there is nothing to reconcile and nothing
//! to lose: teardown discards each session's runtime tree, so the base is the
//! only surviving member and the engine's `members.len() < 2` short-circuit makes
//! a sync a no-op. That is why this runs on the session watchdog and needs no
//! daemon tick — a headless box has no runtime copy that could diverge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// sit adjacent to `apiKeyHelper`): each names an executable CC runs to obtain a
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
    "advisorModel",
];

/// The stat snapshot from the last [`sync_once`] that did work; the key for
/// [`crate::jsonsync::run_with_cache`]'s fast path.
static LAST_SYNCED: Mutex<Option<Vec<crate::jsonsync::MemberSnapshot>>> = Mutex::new(None);

/// Latch for [`warn_paused`], so the pause is reported once rather than on every
/// watchdog tick. Cleared by the next clean [`per_profile_env_keys`] read.
static ENV_KEYS_WARNED: AtomicBool = AtomicBool::new(false);

/// The same latch for an unreadable codex roster, which does NOT pause: its
/// own line, reported once, cleared by the next clean roster read. Separate
/// from [`ENV_KEYS_WARNED`] because that one is cleared by the walk that
/// completes after the roster failed, which would re-arm this line every tick.
static CODEX_ROSTER_WARNED: AtomicBool = AtomicBool::new(false);

/// The single decision point for what a `settings.json` key belongs to. Every
/// per-profile key, top-level or nested, is named here and nowhere else.
///
/// **Criterion:** a key is per-profile when its value identifies, authenticates,
/// or routes ONE account — an endpoint, a credential (or the command that
/// obtains one), or a model choice. Copying such a key into a sibling member points that
/// account's session at the wrong endpoint, spends the wrong key, or bills the
/// wrong model. Everything else `settings.json` holds is operator preference
/// (`hooks`, `permissions`, `statusLine`, theme, non-clauth env vars) and is
/// shared.
///
/// The set has two halves:
/// - top level ([`PER_PROFILE_TOP_FIELDS`]): `apiKeyHelper` names the profile
///   whose `config.toml` holds the raw key, so propagating that one string hands
///   one account's key to another without any `env` entry moving; `model` and
///   `advisorModel` are routing. The top-level `effortLevel` stays shared;
///   clauth's account-wide effort carrier is the managed
///   `CLAUDE_CODE_EFFORT_LEVEL` env key below.
/// - inside `env`, key by key rather than skipping the whole object (the block
///   also carries plain shared vars): [`MANAGED_ENV_KEYS`] plus `custom_env`,
///   every key any profile declares in its own `config.toml` `[env]` and every
///   key `~/.clauth/base-env.json` records as covered.
/// - inside `modelSettings`, a model whose level any account pins
///   ([`crate::model_effort::resolved`]) or `~/.clauth/model-effort.json`
///   still covers, matched by its canonical name: that level is the
///   account's. Every other model's settings (a `/effort` on a model no
///   account pins) stay shared.
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
fn key_role(path: KeyPath<'_>, keys: &PerProfileKeys) -> KeyRule {
    match path {
        KeyPath::Top("env" | "modelSettings") => KeyRule::Nested,
        KeyPath::Top(key) if PER_PROFILE_TOP_FIELDS.contains(&key) => KeyRule::PerProfile,
        KeyPath::Top(_) => KeyRule::Shared,
        KeyPath::Nested {
            parent: "modelSettings",
            key,
        } => {
            if crate::model_effort::is_pinned(&keys.models, key) {
                KeyRule::PerProfile
            } else {
                KeyRule::Shared
            }
        }
        KeyPath::Nested { key, .. } => {
            if MANAGED_ENV_KEYS.contains(&key) || keys.env.contains(key) {
                KeyRule::PerProfile
            } else {
                KeyRule::Shared
            }
        }
    }
}

/// The nested keys every member keeps as its own: `env` keys any profile
/// declares or the base-env record covers, and `modelSettings` models (by
/// canonical name) any profile pins or the per-model record covers.
#[derive(Debug, Default)]
pub(crate) struct PerProfileKeys {
    pub(crate) env: BTreeSet<String>,
    pub(crate) models: BTreeSet<String>,
}

/// Every `settings.json` a reconcile pass may join: the operator's own
/// `~/.claude/settings.json` plus each LIVE shared per-session runtime copy,
/// via [`crate::jsonsync::live_runtime_files_under`].
fn known_paths() -> Result<Vec<PathBuf>> {
    Ok(crate::jsonsync::live_runtime_files_under(
        &claude_dir()?,
        "settings.json",
    ))
}

/// Just the `[env]` table of a profile's `config.toml`. A dedicated minimal
/// shape rather than `profile::load_profile`, which also reads credentials, runs
/// pending-rotation recovery, and normalizes fields — none of which this needs,
/// and all of which would run on every tick that finds work.
#[derive(Deserialize)]
struct EnvOnlyConfig {
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    models: crate::profile::ModelSettings,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    model_effort: BTreeMap<String, String>,
}

/// Union of every profile's custom `[env]` keys and every key
/// `~/.clauth/base-env.json` records as covered: a hand edit can drop a key
/// from the active account's `config.toml` while its value is still live. A
/// missing `config.toml` is a profile with no overrides and contributes
/// nothing.
///
/// `None` when a `config.toml` or the record cannot be read or parsed: the
/// affected env sets are then unknown, and [`sync_members`] skips the merge
/// rather than treat their keys as shared and leak them into a sibling.
/// Fail-closed, and self-healing — a file caught mid-edit reads cleanly on the
/// next tick. Because that pauses settings sync entirely, the first failure is
/// logged; the latch keeps the watchdog's ~10 Hz retry from flooding the log,
/// and clears on the next clean read so a recurrence is reported again.
///
/// An unreadable codex roster is the one read that does NOT pause: the codex
/// arm is treated as absent (an empty roster skips no dir), reported once
/// through its own latch, and the walk proceeds. A codex dir then reads as a
/// claude one, which is what claude-first means for a dual claim, and a
/// pure-codex dir's `config.toml` carries no `[env]`, so it contributes
/// nothing either way. The roster is a codex file, and the fix class this
/// walk's codex skip belongs to exists to keep a codex file from stalling a
/// claude subsystem.
#[cfg(test)]
fn per_profile_env_keys() -> Option<BTreeSet<String>> {
    per_profile_keys().map(|keys| keys.env)
}

/// [`per_profile_env_keys`] plus the `modelSettings` models every profile
/// pins and `~/.clauth/model-effort.json` covers, read in the same walk and
/// failing closed the same way.
fn per_profile_keys() -> Option<PerProfileKeys> {
    let mut keys = BTreeSet::new();
    let mut models = BTreeSet::new();
    // The union must cover every profile the MEMBER SET can contain, and the
    // members (`known_paths` → `live_shared_runtime_dirs`) are DIRECTORY-derived —
    // so the union walks the same directory listing. A roster-driven union
    // would let an off-roster dir (a profile dir written but its roster entry
    // never saved) keep merging its runtime
    // settings while its `[env]` keys read as shared: the exact leak this fn
    // fails closed against. What the codex split changes is only WHOSE dirs
    // belong to this subsystem: a dir the codex roster claims is skipped — it
    // can never become a member (a codex home matches no `runtime*` stem) —
    // so a codex `[env]` can neither join the union nor, unparseable, pause a
    // claude-only sync.
    let codex = match crate::codex_profiles::CodexState::load_with_position() {
        Ok(state) => {
            CODEX_ROSTER_WARNED.store(false, Ordering::Relaxed);
            state
        }
        Err(err) => {
            if !CODEX_ROSTER_WARNED.swap(true, Ordering::Relaxed) {
                let path = clauth_dir().ok()?.join("codex-profiles.toml");
                match err {
                    crate::codex_profiles::CodexRosterError::Parse(pos) => {
                        let head = crate::format::parse_refusal(
                            format!("clauth: {} did not yield the codex roster", path.display()),
                            pos,
                        );
                        logline!(
                            "{head}; settings.json sync reads every profile dir as claude until \
                             it reads cleanly"
                        );
                    }
                    crate::codex_profiles::CodexRosterError::Read(e) => {
                        logline!(
                            "clauth: {} did not yield the codex roster ({}); settings.json sync \
                             reads every profile dir as claude until it reads cleanly",
                            path.display(),
                            e.root_cause()
                        );
                    }
                    crate::codex_profiles::CodexRosterError::NoHome(_) => {}
                }
            }
            crate::codex_profiles::CodexState::default()
        }
    };
    let claude_roster: Vec<String> = crate::profile::claude_roster_names()
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.to_string())
        .collect();
    let profiles = clauth_dir().ok()?.join("profiles");
    // No profiles dir is no profile overrides; the record below still counts.
    for entry in std::fs::read_dir(&profiles).into_iter().flatten().flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        // Claude-FIRST for a name both files claim, the precedence pinned at
        // every other resolution site (`cmd_switch`, `cmd_start`). Skipping on
        // the codex roster alone inverted it here: a hand-edited dual claim
        // (uniqueness is enforced at creation, never against edited state) made
        // this arm treat the name as codex-only and skip the very dir whose
        // `[env]` leak it exists to catch.
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| codex.holds(name) && !claude_roster.iter().any(|p| p == name))
        {
            continue;
        }
        let path = entry.path().join("config.toml");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return warn_paused(&path, &format!("could not be read ({e})")),
        };
        match toml::from_str::<EnvOnlyConfig>(&raw) {
            Ok(config) => {
                models.extend(
                    crate::model_effort::resolved_parts(
                        &config.models,
                        config.effort.as_deref(),
                        &config.model_effort,
                    )
                    .into_keys(),
                );
                keys.extend(config.env.into_keys());
            }
            Err(e) => {
                let reason = crate::format::parse_refusal(
                    "is not valid TOML",
                    crate::format::toml_parse_position(&raw, &e),
                )
                .to_string();
                return warn_paused(&path, &reason);
            }
        }
    }
    // A key the active account covers stays per-profile while its record
    // entry lives, though a hand edit dropped it from that `config.toml`.
    match crate::claude::saved_base_env_keys() {
        Ok(covered) => keys.extend(covered),
        Err(_) => {
            let path = crate::claude::saved_base_env_path().ok()?;
            return warn_paused(&path, "does not parse");
        }
    }
    // Likewise a level the per-model record still covers in the live file.
    let record = crate::model_effort::record_path().ok()?;
    match crate::model_effort::read_record_at(&record) {
        Ok(found) => models.extend(found.into_iter().flat_map(|record| record.ids.into_keys())),
        Err(_) => return warn_paused(&record, "does not parse"),
    }
    ENV_KEYS_WARNED.store(false, Ordering::Relaxed);
    Some(PerProfileKeys { env: keys, models })
}

/// Report the first file failure that pauses settings sync, then latch.
/// Always returns `None` so callers can `return warn_paused(..)` directly.
fn warn_paused<T>(path: &Path, reason: &str) -> Option<T> {
    if !ENV_KEYS_WARNED.swap(true, Ordering::Relaxed) {
        logline!(
            "clauth: settings.json sync paused: {} {reason}. \
             The affected profiles' custom [env] keys are unknown, so no \
             settings are synced until it reads cleanly",
            path.display()
        );
    }
    None
}

/// Reconcile every known `settings.json` once, behind the `LAST_SYNCED` stat
/// fast path in [`crate::jsonsync::run_with_cache`]. The member list is the
/// fast path's stat key only: the merge re-reads it inside the lock.
pub(crate) fn sync_once() -> Result<()> {
    let paths = known_paths()?;
    crate::jsonsync::run_with_cache(&LAST_SYNCED, &paths, sync_members)
}

/// The written-copy record for `settings.json`, persisted under the same state
/// flock the merge holds in [`sync_members`] — clauth's own echoes never
/// outrank a real Claude Code write.
const SETTINGS_SYNC_WRITTEN_FILENAME: &str = ".settings-sync-written.json";

/// The written-copy record path for `settings.json` — one derivation shared by
/// the merge and the runtime build, so a copy clauth builds is a copy the merge
/// knows clauth wrote.
pub(crate) fn written_record_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(SETTINGS_SYNC_WRITTEN_FILENAME))
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
/// The added cost is N small TOML parses plus the member-list walk — a read of
/// every profile dir, and of each shared runtime tree's sessions dir with one
/// open per marker until a held one, and a job-store read when some candidate's
/// marker probe found nothing — inside a hold that already does N
/// `settings.json` reads, parses, and writes.
fn sync_members() -> Result<bool> {
    let operator_file = claude_dir()?.join("settings.json");
    let record = written_record_path()?;
    #[cfg(test)]
    run_pre_lock_hook();
    with_state_lock(|_held| {
        // The member list is read INSIDE the hold: a pass that listed it before
        // a just-started session's marker was held would run with that session
        // missing, prune its record entry, and let its first in-session edit be
        // reverted. The list and `per_profile_env_keys` are read inside the
        // same hold, so neither predates the merge.
        let paths = known_paths()?;
        let Some(per_profile) = per_profile_keys() else {
            return Ok(false);
        };
        crate::jsonsync::sync_paths(
            &paths,
            Some(&operator_file),
            Some(&record),
            crate::jsonsync::SETTINGS_PROTECTED_KEYS,
            |path| key_role(path, &per_profile),
        )?;
        Ok(true)
    })
}

/// Test-only interleave point at the top of [`sync_members`], before the state
/// flock is taken: the span a session starting between the fast path's member
/// list and the lock occupies. A test arms it to start a session there and pin
/// that the merge re-reads the list inside the lock rather than pruning the
/// fresh session's record entry. `None` is inert; armed and disarmed only while
/// the arming test holds a `HomeSandbox`.
#[cfg(test)]
static PRE_LOCK_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`PRE_LOCK_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_pre_lock_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *PRE_LOCK_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`PRE_LOCK_HOOK`], if any. Clones the `Arc` so the hook
/// runs outside the slot lock.
#[cfg(test)]
fn run_pre_lock_hook() {
    let hook = PRE_LOCK_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
#[path = "../tests/inline/settings_sync.rs"]
mod tests;
