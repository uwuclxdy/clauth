//! A per-profile record of a limit-reset spend whose reply clauth could not
//! read, so a re-run within the server's window re-sends the same ids instead
//! of spending a second grant or credit. Both arms write their record through
//! one load/fresh/write/clear implementation: a claude spend records its grant
//! and request id, a codex consume its credit and redeem id, and each stamps
//! when the ids left.
//!
//! The record is 0o600 like every other file clauth writes, is written before
//! the POST, and is cleared on a settled reply. A re-run within 600 s of the
//! stamp re-sends the recorded ids; older or unparseable records are removed
//! and the normal fresh spend runs.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::codex_reset::{RESEND_WINDOW_MS, request_id_is_sendable};
use super::now_ms;
use crate::profile::{ProfileName, atomic_write_600, profile_subpath};

/// A claude spend whose reply was lost: the grant and request id that went
/// out, and the instant they did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudePending {
    pub(crate) grant_id: String,
    pub(crate) request_id: String,
    pub(crate) sent_at_ms: u64,
}

/// A codex consume whose reply was lost: the credit and redeem id that went
/// out, and the instant they did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexPending {
    pub(crate) credit_id: String,
    pub(crate) redeem_request_id: String,
    pub(crate) sent_at_ms: u64,
}

/// What the shared implementation needs from either record.
pub(crate) trait PendingSpend: Serialize + DeserializeOwned {
    fn sent_at_ms(&self) -> u64;
    fn file_name() -> &'static str;
    /// Whether the record's ids are ones clauth would send, so a record whose
    /// ids are off the arm's sendable shape was not written by clauth and
    /// reads as unreadable.
    fn ids_are_sendable(&self) -> bool;
}

impl PendingSpend for ClaudePending {
    fn sent_at_ms(&self) -> u64 {
        self.sent_at_ms
    }

    fn file_name() -> &'static str {
        "limit_reset_pending_claude.json"
    }

    fn ids_are_sendable(&self) -> bool {
        super::claude_reset::grant_id_is_sendable(&self.grant_id)
            && request_id_is_sendable(&self.request_id)
    }
}

impl PendingSpend for CodexPending {
    fn sent_at_ms(&self) -> u64 {
        self.sent_at_ms
    }

    fn file_name() -> &'static str {
        "limit_reset_pending_codex.json"
    }

    fn ids_are_sendable(&self) -> bool {
        request_id_is_sendable(&self.redeem_request_id)
    }
}

/// The record's path in `name`'s profile directory.
pub(crate) fn path<T: PendingSpend>(name: &ProfileName) -> anyhow::Result<PathBuf> {
    profile_subpath(name, T::file_name())
}

/// Whether a stamp is inside the re-send window: within 600 s of `now` on
/// either side, so a clock stepped back after the send reads the record stale
/// instead of re-sending the old request past its window.
fn fresh(now: u64, sent_at_ms: u64) -> bool {
    now.abs_diff(sent_at_ms) < RESEND_WINDOW_MS
}

/// Whether a loaded record is one the run may act on: inside the re-send
/// window and carrying ids the arm would send. Both the re-send read and
/// `--list`'s note ask this one question, so neither can outlive the other.
fn is_current<T: PendingSpend>(now: u64, record: &T) -> bool {
    fresh(now, record.sent_at_ms()) && record.ids_are_sendable()
}

/// Write the record through [`atomic_write_600`]. A failure names the file and
/// the operation, so the caller refuses the spend with nothing sent.
pub(crate) fn write<T: PendingSpend>(path: &Path, record: &T) -> anyhow::Result<()> {
    let body = serde_json::to_vec(record).map_err(|e| {
        anyhow::anyhow!(
            "could not encode the pending reset record for {}: {e}",
            path.display()
        )
    })?;
    atomic_write_600(path, body).map_err(|e| {
        anyhow::anyhow!(
            "could not record the pending reset in {} before sending: {e}",
            path.display()
        )
    })
}

/// Load a fresh record. A stale, unparseable, or unsendable one is removed
/// first so the normal fresh spend can write a new one; a removal that fails
/// refuses (the next write would overwrite it anyway, but the caller says what
/// failed). A file that cannot be read at all refuses rather than guessing
/// whether a spend is still pending.
pub(crate) fn take_fresh<T: PendingSpend>(path: &Path) -> anyhow::Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => anyhow::bail!(
            "could not read the pending reset record {}: {e}",
            path.display()
        ),
    };
    let record: T = match serde_json::from_str(&text) {
        Ok(record) => record,
        Err(_) => {
            clear(path)?;
            return Ok(None);
        }
    };
    if is_current(now_ms(), &record) {
        Ok(Some(record))
    } else {
        clear(path)?;
        Ok(None)
    }
}

/// The fresh record's stamp, for `--list`'s note: read-only, removes nothing,
/// so a list never mutates the store.
pub(crate) fn fresh_stamp<T: PendingSpend>(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let record: T = serde_json::from_str(&text).ok()?;
    is_current(now_ms(), &record).then_some(record.sent_at_ms())
}

/// Remove the record. An already-absent record is already gone; anything else
/// is an error naming the file.
pub(crate) fn clear(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => anyhow::bail!(
            "could not remove the pending reset record {}: {e}",
            path.display()
        ),
    }
}

// ── the spend lock ──────────────────────────────────────────────────────────

/// The per-account advisory flock serializing `limit-reset` spends: a run
/// holds it from before its first request through the settle, so a second run
/// on the same account waits and then re-decides on what the first left behind
/// (its pending record, or nothing). One lock per name, shared by the claude
/// and codex arms; `--list` never takes it. The lock file is never deleted
/// (the rotation-locks precedent).
#[must_use]
pub(crate) struct LimitResetLock {
    // Drops before `_rank` (declaration order): the flock releases, then the
    // LIMIT_RESET_SPEND rank pops — never the reverse.
    _file: std::fs::File,
    _rank: crate::lockorder::RankGuard,
}

impl LimitResetLock {
    /// The lock file: `~/.clauth/limit-reset-locks/<name>.lock`.
    fn path(name: &ProfileName) -> anyhow::Result<PathBuf> {
        Ok(crate::profile::clauth_dir()?
            .join("limit-reset-locks")
            .join(format!("{name}.lock")))
    }

    /// Acquire the per-account spend lock, blocking until any in-flight spend
    /// on the same account releases it. Creates the `limit-reset-locks`
    /// directory if missing. A held lock prints one stderr line, then waits;
    /// any other lock failure refuses the spend naming the lock file.
    pub(crate) fn acquire(name: &ProfileName) -> anyhow::Result<Self> {
        let path = Self::path(name)?;
        if let Some(parent) = path.parent() {
            crate::profile::mkdir_700(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = crate::profile::open_state_file(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                crate::out::errln!(
                    "clauth: another limit-reset on '{name}' is running; waiting for it."
                );
                file.lock()
                    .with_context(|| format!("failed to lock {}", path.display()))?;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
        let _rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::LimitResetSpend>();
        Ok(Self { _file: file, _rank })
    }
}

#[cfg(test)]
#[path = "../../tests/inline/limit_reset_pending.rs"]
mod tests;
