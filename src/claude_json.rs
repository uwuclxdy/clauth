//! Cross-profile `.claude.json` synchronizer.
//!
//! Claude Code keeps one large config file — `~/.claude.json` for normal use —
//! holding user-global state (`projects`, `mcpServers`, `tips`, `userID`)
//! alongside an account-specific `oauthAccount` block and a few billing/usage
//! caches. `clauth start <profile>` runs Claude Code against a per-profile
//! runtime tree with its OWN `.claude.json`, because a single shared file leaks
//! one account's identity into another: Claude Code trusts the cached
//! `oauthAccount` and does not re-derive it from the loaded token on a normal
//! startup (its bootstrap merge keeps the cached identity when the server
//! reports a different account).
//!
//! This module keeps every clauth-managed `.claude.json` (the global file plus
//! each profile runtime's copy) in sync EXCEPT for [`PER_PROFILE_FIELDS`], which
//! each file keeps as its own. Sync is "latest write wins" at file granularity:
//! each tick the newest parseable file is the source for the shared fields,
//! overlaid onto every other file while preserving that file's per-profile
//! fields. Atomic writes plus write-only-on-change make it convergent and safe
//! against Claude Code's concurrent in-place writes — a file caught mid-write
//! fails to parse and is simply skipped until the next tick. The merge itself
//! lives in [`crate::jsonsync`], shared with the `settings.json` reconciler.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::jsonsync::{KeyPath, KeyRule};
use crate::profile::{atomic_write, clauth_dir, home_dir};

/// Account-specific keys that must never propagate between profiles.
const PER_PROFILE_FIELDS: &[&str] = &[
    "oauthAccount",
    "overageCreditGrantCache",
    "passesEligibilityCache",
    "passesLastSeenRemaining",
    "cachedExtraUsageDisabledReason",
    // Account/org-scoped model caches Claude Code writes into `.claude.json`.
    // Syncing them would bleed one account's model access, org default, and
    // per-model cost/option tables into every other account. Each profile
    // re-fetches its own on first boot, so per-profile is lossless.
    "orgModelDefaultCache",
    "modelAccessCache",
    "additionalModelCostsCache",
    "additionalModelOptionsCache",
    // `/login`-managed API key and the per-key approval ledger. `primaryApiKey`
    // is a RAW KEY: Claude Code writes it into this file (verified on 2.1.215 —
    // the same accessor that reads `numStartups` and `oauthAccount`) and reads
    // it back as an auth source labelled "/login managed key". Propagating it
    // would hand one account's key to every other profile — the `.claude.json`
    // twin of the `apiKeyHelper` leak the settings syncer blocks.
    // `customApiKeyResponses` holds the approved/rejected hashes of those keys,
    // so it is scoped to the same account.
    "primaryApiKey",
    "customApiKeyResponses",
];

/// Newest mtime from the last [`sync_once`] that did work. Short-circuits ticks
/// where no file is newer — no reads, parses, or writes.
static LAST_SYNCED: Mutex<Option<SystemTime>> = Mutex::new(None);

fn known_paths() -> Result<Vec<PathBuf>> {
    let mut paths = vec![home_dir()?.join(".claude.json")];
    let profiles = clauth_dir()?.join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                paths.push(entry.path().join("runtime").join(".claude.json"));
            }
        }
    }
    Ok(paths)
}

/// Reconcile all known `.claude.json` files once. Stat-only fast path: skips
/// reads when no file is newer than `LAST_SYNCED`. Advances `LAST_SYNCED` only
/// on success, so transient errors retry next tick.
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
    sync_paths(&paths)?;
    *LAST_SYNCED.lock().unwrap_or_else(|p| p.into_inner()) = Some(newest);
    Ok(())
}

/// Reconcile the given `.claude.json` copies. Every field is shared except
/// [`PER_PROFILE_FIELDS`], which is a flat top-level set — no `env`-style nested
/// rule, unlike the `settings.json` reconciler.
fn sync_paths(paths: &[PathBuf]) -> Result<()> {
    let operator_file = home_dir().ok().map(|h| h.join(".claude.json"));
    crate::jsonsync::sync_paths(paths, operator_file.as_deref(), |path| match path {
        KeyPath::Top(key) if PER_PROFILE_FIELDS.contains(&key) => KeyRule::PerProfile,
        _ => KeyRule::Shared,
    })
}

/// CC's cached account uuid from `~/.claude.json`'s `oauthAccount.accountUuid`,
/// or `None` when the file, block, or value is absent/blank/unparseable.
/// Read-then-parse with the same discipline as [`strip_home_oauth_account`]: a
/// missing file is never created and a file caught mid-write by CC is left
/// untouched rather than clobbered. CC trusts this cached block and does not
/// re-derive it from a swapped credentials file — so a hit is "CC's last booted
/// identity", not fresh proof of the live token's account.
pub(crate) fn home_oauth_account_uuid() -> Option<String> {
    let path = home_dir().ok()?.join(".claude.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return None; // missing — never create
    };
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&bytes) else {
        return None; // unparseable (CC mid-write) — never clobber
    };
    obj.get("oauthAccount")
        .and_then(|a| a.get("accountUuid"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

/// Delete the stale `oauthAccount` identity block from the home
/// `~/.claude.json` on profile switch (issue #17). Claude Code trusts a
/// cached identity block and does not re-derive it from a relinked
/// credentials file on a normal startup; dropping the block instead lets it
/// self-heal — probed on CC 2.1.201 (`docs/issue-17-oauthaccount.md`): an
/// absent block re-derives the correct identity from the token within
/// seconds, a present-but-wrong one never self-corrects.
///
/// Read-then-parse first: a missing file is left uncreated, and a file that
/// fails to parse (CC mid-write) is left untouched rather than clobbered. A
/// write only happens when the key is actually present — an already-clean
/// file is never touched, because [`sync_once`] picks the newest-mtime member
/// as the sync winner; a pointless touch here would make home win the next
/// tick and stomp a runtime copy's own fields.
pub(crate) fn strip_home_oauth_account() -> Result<()> {
    let path = home_dir()?.join(".claude.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(()); // missing — never create
    };
    let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(()); // unparseable (CC mid-write) — never clobber
    };
    if obj.remove("oauthAccount").is_none() {
        return Ok(()); // already clean — avoid a pointless mtime bump
    }
    let bytes = serde_json::to_vec_pretty(&Value::Object(obj))
        .context("failed to serialize .claude.json after stripping oauthAccount")?;
    atomic_write(&path, &bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// Both identity halves — `(accountUuid, emailAddress)` — extracted from ONE
/// read of `~/.claude.json`, so a rewrite between two separate reads can never
/// pair one account's uuid with another's email. `None` when the file/block/
/// uuid is absent (same contract as [`home_oauth_account_uuid`]); the email is
/// independently optional.
pub(crate) fn live_oauth_account_pair() -> Option<(String, Option<String>)> {
    let path = home_dir().ok()?.join(".claude.json");
    let bytes = std::fs::read(&path).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let account = v.get("oauthAccount")?;
    let uuid = account.get("accountUuid")?.as_str()?.trim();
    if uuid.is_empty() {
        return None;
    }
    let email = account
        .get("emailAddress")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    Some((uuid.to_string(), email))
}

#[cfg(test)]
#[path = "../tests/inline/claude_json.rs"]
mod tests;
