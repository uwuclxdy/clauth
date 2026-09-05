//! Shared engine for clauth's "newest parseable copy wins" JSON reconcilers.
//!
//! Two Claude Code files need the same treatment: `.claude.json`
//! ([`crate::claude_json`]) and `settings.json` ([`crate::settings_sync`]). Both
//! exist as one logical document spread across the operator's own copy plus one
//! copy per profile runtime; both must converge on the fields that are user
//! preference while every copy keeps the fields that name its own account; and
//! both are rewritten in place by a Claude Code that can be caught mid-write.
//!
//! Only two things differ between them: the file name, and which keys are
//! per-profile. So the read/parse/skip, newest-wins, merge, member
//! enumeration, mtime-cache fast path, and atomic-write-only-on-change
//! machinery lives here once, and each caller supplies its base file plus one
//! [`KeyRule`] function.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::profile::{atomic_write, atomic_write_600};

/// Where a key sits in a synced document. The walk is one level deep, so
/// [`KeyPath::Nested`] is only ever asked about keys of a top-level object whose
/// own rule was [`KeyRule::Nested`]. It carries no parent name because no spec
/// yet nests more than one object; a second one would need it back, or both
/// would silently share a rule.
#[derive(Debug, Clone, Copy)]
pub(crate) enum KeyPath<'a> {
    Top(&'a str),
    Nested(&'a str),
}

/// What the syncer does with one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyRule {
    /// The winner's value replaces every other member's, and a key the winner
    /// dropped is removed everywhere.
    Shared,
    /// Every member keeps its own value; the winner's never propagates.
    PerProfile,
    /// Object-valued: descend and rule its keys one by one. Meaningless on a
    /// [`KeyPath::Nested`] key and treated as [`KeyRule::Shared`] there.
    Nested,
}

/// Newest mtime across `paths`, or `None` when none of them exist. Stat-only —
/// the fast path callers use to skip a tick with no reads, parses, or writes.
pub(crate) fn newest_mtime(paths: &[PathBuf]) -> Option<SystemTime> {
    paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .max()
}

/// Every file clauth reconciles for one synced document: the operator's own
/// copy under `base` plus each SHARED per-session runtime copy of `file`.
/// Enumerated through [`crate::runtime::shared_runtime_dirs`] rather than a
/// fixed `<profile>/runtime` path — every session owns its own `runtime-<sid>`,
/// so a fixed path would reconcile nothing while every session ran. Isolated
/// copies stay out: an isolated runtime is built from an empty base, so it must
/// neither win (its empty copy would delete the operator's own fields) nor
/// receive (that would defeat the isolation it exists for).
pub(crate) fn runtime_files_under(base: &Path, file: &str) -> Vec<PathBuf> {
    let mut paths = vec![base.join(file)];
    paths.extend(
        crate::runtime::shared_runtime_dirs()
            .into_iter()
            .map(|dir| dir.join(file)),
    );
    paths
}

/// Stat-only mtime-cache fast path around `body`: skip the tick when no file in
/// `paths` is newer than what `cache` records from the last sync that did work
/// (no reads, parses, or writes), and advance `cache` only when `body` reports
/// it ran — so a paused (`Ok(false)`) or failed tick retries next time.
pub(crate) fn run_with_cache(
    cache: &Mutex<Option<SystemTime>>,
    paths: &[PathBuf],
    body: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    let Some(newest) = newest_mtime(paths) else {
        return Ok(());
    };
    {
        let last = cache.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|l| newest <= l) {
            return Ok(());
        }
    }
    if !body()? {
        return Ok(());
    }
    // `newest` was stated before the body, so it can only lag what the body
    // actually saw — costing at most one redundant tick, never a skipped one.
    *cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(newest);
    Ok(())
}

struct Member {
    path: PathBuf,
    mtime: SystemTime,
    obj: Map<String, Value>,
}

/// Read and parse each file (skipping missing ones and partial writes), pick the
/// newest as winner, rewrite every other as `winner.shared ∪ target.per_profile`
/// — atomically, only on change. Idempotent after convergence.
///
/// `operator_file` is the one member Claude Code owns (the copy under the
/// operator's home rather than under `~/.clauth`); see [`write_member`] for why
/// it is written differently.
pub(crate) fn sync_paths(
    paths: &[PathBuf],
    operator_file: Option<&Path>,
    rule: impl Fn(KeyPath<'_>) -> KeyRule,
) -> Result<()> {
    let mut members: Vec<Member> = Vec::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // Partial CC in-place write fails to parse — skip until next tick.
        let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        members.push(Member {
            path: path.clone(),
            mtime,
            obj,
        });
    }
    if members.len() < 2 {
        return Ok(());
    }

    // Newest mtime wins; path breaks ties for determinism.
    // Non-empty invariant: the `members.len() < 2` guard above.
    #[allow(clippy::expect_used, reason = "non-empty invariant guaranteed above")]
    let winner = members
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)))
        .map(|(i, _)| i)
        .expect("members is non-empty");

    for i in 0..members.len() {
        if i == winner {
            continue;
        }
        let merged = merge_member(&members[winner].obj, &members[i].obj, &rule);
        if merged == members[i].obj {
            continue;
        }
        let path = &members[i].path;
        let bytes = serde_json::to_vec_pretty(&Value::Object(merged))
            .with_context(|| format!("failed to serialize merged {}", path.display()))?;
        write_member(path, &bytes, operator_file)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Overlay the winner's shared state onto one target: start from the target so
/// its key order and per-profile fields survive, drop the shared keys the winner
/// no longer carries, then upsert the winner's.
fn merge_member(
    winner: &Map<String, Value>,
    target: &Map<String, Value>,
    rule: &impl Fn(KeyPath<'_>) -> KeyRule,
) -> Map<String, Value> {
    let mut merged = target.clone();
    // Per-profile keys are the target's own; nested ones survive the top-level
    // pass and are filtered key-by-key below.
    merged.retain(|k, _| rule(KeyPath::Top(k)) != KeyRule::Shared || winner.contains_key(k));
    for (k, v) in winner {
        if rule(KeyPath::Top(k)) == KeyRule::Shared {
            merged.insert(k.clone(), v.clone());
        }
    }

    // Nested objects, over the union of both sides' keys so the winner can add
    // and drop shared entries without disturbing the target's own.
    let mut nested_keys: Vec<String> = Vec::new();
    for k in merged.keys().chain(winner.keys()) {
        if rule(KeyPath::Top(k)) == KeyRule::Nested && !nested_keys.iter().any(|n| n == k) {
            nested_keys.push(k.clone());
        }
    }
    for key in nested_keys {
        let winner_value = winner.get(&key);
        // A winner whose value is not an object cannot be merged key-by-key, and
        // copying it wholesale would erase every per-profile key the target
        // holds under it — the exact thing this rule exists to protect. Leave
        // the target alone; the writers that produce these files reject the
        // shape outright (`build_claude_settings_json`), so it is a hand-edit or
        // a truncation that still parsed, and the next tick self-heals.
        if winner_value.is_some_and(|v| !v.is_object()) {
            continue;
        }
        let winner_obj = winner_value.and_then(Value::as_object);
        let target_obj = merged.get(&key).and_then(Value::as_object).cloned();
        if winner_obj.is_none() && target_obj.is_none() {
            continue; // neither side holds an object here — leave the target as is
        }
        let mut nested = target_obj.unwrap_or_default();
        nested.retain(|nk, _| {
            rule(KeyPath::Nested(nk)) == KeyRule::PerProfile
                || winner_obj.is_some_and(|w| w.contains_key(nk))
        });
        if let Some(winner_obj) = winner_obj {
            for (nk, nv) in winner_obj {
                if rule(KeyPath::Nested(nk)) != KeyRule::PerProfile {
                    nested.insert(nk.clone(), nv.clone());
                }
            }
        }
        merged.insert(key, Value::Object(nested));
    }
    merged
}

/// Write one synced member. The rename swaps the inode, so the mode is the
/// writer's, not the file's: a clauth-owned copy (under `~/.clauth`) gets 0o600
/// so the syncer can't silently revert the seed's owner-only mode, while
/// `operator_file` — Claude Code's own copy under the operator's home — lands at
/// CC's own 0o644 posture, matching what `claude::apply_profile_to_claude_settings`
/// already does to that file. Note this is not preservation: `atomic_write`
/// renames a fresh umask-moded temp over the path, so a hand-tightened operator
/// file is widened. Deliberate — clauth does not own that file and does not
/// restyle it either way. Any path that is not `operator_file` is treated as
/// clauth-owned, the stricter default.
fn write_member(path: &Path, bytes: &[u8], operator_file: Option<&Path>) -> std::io::Result<()> {
    if operator_file == Some(path) {
        atomic_write(path, bytes)
    } else {
        atomic_write_600(path, bytes)
    }
}
