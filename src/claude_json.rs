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
//! each file keeps as its own. Sync is "newest Claude Code write wins" at file
//! granularity: each member ranks by the mtime of the Claude Code write its
//! bytes carry (a non-echo by its own mtime, clauth's own unchanged echo by the
//! carried rank its record entry stores), and the highest rank is the source for
//! the shared fields, overlaid onto every other file while preserving that
//! file's per-profile fields. Atomic writes plus write-only-on-change make it
//! convergent and safe against Claude Code's concurrent in-place writes — a file
//! caught mid-write fails to parse and is simply skipped until the next tick.
//! The merge itself lives in [`crate::jsonsync`], shared with the `settings.json`
//! reconciler.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::jsonsync::{KeyPath, KeyRule, member_changed_since_read};
use crate::logline::logline;
use crate::profile::{
    AccountId, atomic_write_600, clauth_dir, home_dir, mkdir_700, open_state_file,
};

/// Account-specific keys that must never propagate between profiles.
const PER_PROFILE_FIELDS: &[&str] = &[
    "oauthAccount",
    "overageCreditGrantCache",
    "passesEligibilityCache",
    "passesLastSeenRemaining",
    "cachedExtraUsageDisabledReason",
    // The usage cache stamps `accountUuid` alone, the artifact roster also
    // stamps `organizationUuid`. Two organizations under one account share an
    // `accountUuid`, so syncing `cachedUsageUtilization` made every profile
    // read whichever copy was written last as its own 5h/7d usage.
    "cachedUsageUtilization",
    "cachedArtifactRoster",
    "artifactRosterDenied",
    // Org-scoped config and feature-flag caches Claude Code keeps beside them.
    "groveConfigCache",
    "promoStartupStatusCache",
    "metricsStatusCacheByPrincipal",
    "metricsStatusCache",
    "clientDataCacheSlots",
    "clientDataCache",
    "cachedGrowthBookFeatures",
    "cachedGrowthBookFeaturesAt",
    "cachedExperimentFeatures",
    "cachedExperimentData",
    // Account/org-scoped model caches Claude Code writes into `.claude.json`.
    // Syncing them would bleed one account's model access, org default, and
    // per-model cost/option tables into every other account. Each profile
    // re-fetches its own on first boot, so per-profile is lossless.
    "orgModelDefaultCache",
    "lastSeenOrgDefaultUpdatedAt",
    "modelAccessCache",
    "additionalModelCostsCache",
    "additionalModelOptionsCache",
    "additionalModelOptionsAnsweredAt",
    // The rest of what Claude Code clears on OAuth logout (verified on
    // 2.1.287): the GitHub connection status stamps both uuids, the
    // auto-compact windows come with the bootstrap's model caches, and a shared
    // prefetch stamp would make one profile skip its own startup prefetches.
    "githubWebConnectionStatusCache",
    "autoCompactWindowsCache",
    "startupPrefetchedAt",
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

/// The stat snapshot from the last [`sync_once`] that did work; the key for
/// [`crate::jsonsync::run_with_cache`]'s fast path.
static LAST_SYNCED: Mutex<Option<Vec<crate::jsonsync::MemberSnapshot>>> = Mutex::new(None);

/// The last good credit-billed family set per `.claude.json` path, so a file
/// CC is caught mid-writing (unparseable) keeps the previous successful parse
/// for that path — the sync engine's own "skip until the next tick" torn-read
/// rule. A `Mutex` leaf held for one map operation, never across the file read.
static CREDIT_BILLED_FAMILIES: LazyLock<Mutex<HashMap<PathBuf, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Every `.claude.json` copy on this host, live or dead: the global file plus
/// each SHARED per-session runtime copy that EXISTS, via
/// [`crate::jsonsync::runtime_files_under`]. The NAMING walk — the reconcile
/// passes and the wire take [`live_known_paths`], which gates this on liveness.
#[cfg(test)]
pub(crate) fn known_paths() -> Result<Vec<PathBuf>> {
    Ok(crate::jsonsync::runtime_files_under(
        &home_dir()?,
        ".claude.json",
    ))
}

/// [`known_paths`] over the LIVE shared runtime trees only — the member set a
/// reconcile pass may join, and the wire's enumeration. A tree no live session
/// holds and no live Claude Code job names is left out: its copy may be a stray
/// Claude Code's first-start file, and merging one would delete the operator's
/// own keys.
pub(crate) fn live_known_paths() -> Result<Vec<PathBuf>> {
    Ok(crate::jsonsync::live_runtime_files_under(
        &home_dir()?,
        ".claude.json",
    ))
}

/// Reconcile all known `.claude.json` files once, behind the `LAST_SYNCED`
/// stat fast path in [`crate::jsonsync::run_with_cache`]. Waits up to
/// [`json_sync_lock_timeout()`] for the sync lock: the
/// [`crate::runtime::ProfileRuntime`] drop's final flush, whose work has no
/// later event to retry it.
pub(crate) fn sync_once() -> Result<()> {
    let paths = live_known_paths()?;
    bypass_fast_path_for_pending_strip();
    crate::jsonsync::run_with_cache(&LAST_SYNCED, &paths, || {
        #[cfg(test)]
        run_pre_lock_hook();
        let _lock = JsonSyncLock::acquire()?;
        // The member list is re-read INSIDE the lock: a pass that listed it
        // before a just-started session's marker was held would prune that
        // session's record entry and let its first in-session edit be reverted.
        let paths = live_known_paths()?;
        sync_paths_locked(&paths)?;
        Ok(true)
    })
}

/// [`sync_once`] for the watchdog's config leg: try the sync lock ONCE and skip
/// (without advancing [`LAST_SYNCED`]) when another process or thread holds it,
/// so a busy or wedged peer never delays the credential leg and swap poll on the
/// same loop iteration. The 30 s fallback tick and the next filesystem event
/// retry a skipped pass.
pub(crate) fn sync_once_try() -> Result<()> {
    let paths = live_known_paths()?;
    bypass_fast_path_for_pending_strip();
    crate::jsonsync::run_with_cache(&LAST_SYNCED, &paths, || {
        #[cfg(test)]
        run_pre_lock_hook();
        match JsonSyncLock::try_once()? {
            Some(_lock) => {
                // Re-read the member list inside the lock, like [`sync_once`].
                let paths = live_known_paths()?;
                sync_paths_locked(&paths)?;
                Ok(true)
            }
            None => Ok(false),
        }
    })
}

/// A deferred switch strip left [`STRIP_PENDING_FILENAME`] under `~/.clauth`.
/// The marker is not a `.claude.json` member, so [`crate::jsonsync::run_with_cache`]'s
/// stat fast path cannot see it and would skip the pass that honours it. Clear
/// the cache so this pass reaches the body (which strips under the sync lock)
/// even when no member's stat moved.
///
/// The forced parse is bounded: after [`HONOUR_FAILURE_LIMIT`] consecutive
/// failures to honour the marker (a permanently unwritable home or `~/.clauth`),
/// this stops clearing the cache, so an unchanged member set skips the full parse
/// until a member's stat actually changes — at which point
/// [`crate::jsonsync::run_with_cache`] re-runs the body on its own and the honour
/// is retried.
fn bypass_fast_path_for_pending_strip() {
    if strip_pending_exists() && HONOUR_FAILURES.load(Ordering::Relaxed) < HONOUR_FAILURE_LIMIT {
        *LAST_SYNCED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Test-only interleave point at the top of the pass body of [`sync_once`] and
/// [`sync_once_try`], between the fast path's member list and the sync lock
/// ([`JsonSyncLock::acquire`] / [`JsonSyncLock::try_once`]): the span a
/// session starting between the list and the lock occupies. A test arms it to
/// start a session there and pin that the merge re-reads the list inside the
/// lock rather than pruning the fresh session's record entry. `None` is inert;
/// armed and disarmed only while the
/// arming test holds a `HomeSandbox`.
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

/// Reconcile the given `.claude.json` copies, waiting for [`JsonSyncLock`].
/// Every field is shared except [`PER_PROFILE_FIELDS`], which is a flat
/// top-level set — no `env`-style nested rule, unlike the `settings.json`
/// reconciler. Test-only: the production paths ([`sync_once`],
/// [`sync_once_try`]) go through [`sync_paths_locked`] directly; the N-wrapper
/// and holder-fd tests drive this waiting form to model a wrapper process.
///
/// The merge runs under [`JsonSyncLock`]: take the lock, THEN read the members,
/// merge, write, release. The stat-only [`LAST_SYNCED`] fast path in
/// [`sync_once`] stays outside the lock, so a quiet tick takes nothing.
#[cfg(test)]
fn sync_paths(paths: &[PathBuf]) -> Result<()> {
    let _lock = JsonSyncLock::acquire()?;
    sync_paths_locked(paths)
}

/// Merge, then honour a deferred switch strip, with [`JsonSyncLock`] already
/// held. The honour is best-effort: a strip that still cannot complete is
/// recorded (latch + failure count) and does NOT fail the merge pass, so a stuck
/// marker never blocks the merge or pins it to a full parse forever.
fn sync_paths_locked(paths: &[PathBuf]) -> Result<()> {
    merge_paths(paths)?;
    honor_pending_strip();
    Ok(())
}

/// The merge body, run with [`JsonSyncLock`] already held: read every member,
/// pick the winner whose bytes carry the newest Claude Code write, rewrite each
/// stale copy.
pub(crate) fn merge_paths(paths: &[PathBuf]) -> Result<()> {
    let operator_file = home_dir().ok().map(|h| h.join(".claude.json"));
    let record = written_record_path();
    crate::jsonsync::sync_paths(
        paths,
        operator_file.as_deref(),
        record.as_deref(),
        &[],
        |path| match path {
            KeyPath::Top(key) if PER_PROFILE_FIELDS.contains(&key) => KeyRule::PerProfile,
            _ => KeyRule::Shared,
        },
    )
}

/// The cross-process flock serializing the `.claude.json` merge, so N live
/// `clauth start` wrappers reconciling the same `$HOME/.claude.json` event do
/// not all read the stale members and each rewrite every copy (#137). A distinct
/// lock from the state flock: the hold does N reads, parses and writes of
/// MB-sized files, and that parse cost must never ride the box's busiest lock.
const JSON_SYNC_LOCK_FILENAME: &str = ".claude-json-sync.lock";

/// The written-copy record for `.claude.json`: clauth's own echoes of the
/// copies it wrote, each carrying the rank of the Claude Code write its bytes
/// were built from, persisted under [`JsonSyncLock`] so a member clauth wrote
/// ranks by that carried rank.
const CLAUDE_JSON_SYNC_WRITTEN_FILENAME: &str = ".claude-json-sync-written.json";

/// The written-copy record path for `.claude.json` — one derivation shared by
/// the merge, the strip, and the runtime seed — over [`clauth_dir`]. `None`
/// when the home cannot be resolved.
pub(crate) fn written_record_path() -> Option<PathBuf> {
    clauth_dir()
        .ok()
        .map(|d| d.join(CLAUDE_JSON_SYNC_WRITTEN_FILENAME))
}

/// Deadline for taking the [`JsonSyncLock`] flock before giving up with a
/// [`JsonSyncLockTimeout`]. Sized like the state flock's own
/// [`crate::lock::STATE_LOCK_TIMEOUT`]: the hold is local file IO with no
/// subprocesses, so only a genuine wedge (a peer hung mid-merge) reaches it.
pub(crate) const JSON_SYNC_LOCK_TIMEOUT: Duration = Duration::from_secs(25);

/// How long the runtime seed waits for [`JsonSyncLock`] under the rotation
/// guard, instead of [`JSON_SYNC_LOCK_TIMEOUT`]'s wedge wait. A merge pass
/// holds the lock for its own local file IO only: per member, one stat, one
/// read, one parse and one atomic write — a handful of MB-sized `.claude.json`
/// copies — plus one best-effort record write; no subprocess and no network.
/// Ten seconds is a chosen budget sized above that IO, never the 25 s a wedge
/// burns. A seed that cannot take the lock within this budget seeds unrecorded
/// and stamps its copy at the floor stamp (below every Claude Code write), so
/// it never parks the rotation guard on a wedged peer.
pub(crate) const SEED_SYNC_LOCK_BUDGET: Duration = Duration::from_secs(10);

/// The flock deadline [`JsonSyncLock::acquire`] waits out:
/// [`JSON_SYNC_LOCK_TIMEOUT`], or a shorter value a test poses a wedge under.
/// The one source of the deadline so a test can shrink the whole wait without
/// sleeping 25 s per acquisition. Production never sets the override.
pub(crate) fn json_sync_lock_timeout() -> Duration {
    #[cfg(test)]
    if let Some(t) = *JSON_SYNC_LOCK_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return t;
    }
    JSON_SYNC_LOCK_TIMEOUT
}

/// Set or clear the test-only deadline override. `None` restores
/// [`JSON_SYNC_LOCK_TIMEOUT`].
#[cfg(test)]
pub(crate) fn set_json_sync_lock_timeout_override(timeout: Option<Duration>) {
    *JSON_SYNC_LOCK_TIMEOUT_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = timeout;
}

#[cfg(test)]
static JSON_SYNC_LOCK_TIMEOUT_OVERRIDE: Mutex<Option<Duration>> = Mutex::new(None);

/// Reset the [`LAST_SYNCED`] stat cache. Test-only: the cache is
/// process-global, so a leak between sandboxes would let a later test's fixture
/// read as already-synced. `HomeSandbox` clears it alongside the lock-timeout
/// overrides.
#[cfg(test)]
pub(crate) fn reset_last_synced() {
    *LAST_SYNCED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// [`JsonSyncLock`] could not be taken within [`json_sync_lock_timeout()`]:
/// another clauth process or thread holds `~/.clauth/.claude-json-sync.lock`.
/// Surfaced through `anyhow`; the caller owns the recovery — the watchdog's
/// config leg never reaches it (it tries once and skips), the Drop final flush
/// logs and proceeds to teardown (a wedged peer can lose the final flush), and
/// the switch strip never reaches it (it tries once and defers the strip to the
/// next reconcile).
#[derive(Debug)]
struct JsonSyncLockTimeout {
    waited: Duration,
}

impl std::fmt::Display for JsonSyncLockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "timed out after {:.0}s acquiring the .claude.json sync lock; another clauth \
             process or thread holds ~/.clauth/.claude-json-sync.lock",
            self.waited.as_secs_f64()
        )
    }
}

impl std::error::Error for JsonSyncLockTimeout {}

/// The `.claude.json` sync lock, held across the whole merge (take, THEN read,
/// merge, write, release), across the switch strip's read-modify-write, and
/// across the runtime seed's record write. A LEAF in the lock order: nothing
/// ranked is acquired while it is held. Its takers — the watchdog's config leg
/// (try-once, holds no ranked lock), [`crate::runtime::ProfileRuntime`]'s drop
/// (wait, holds no ranked lock), the switch strip (try-once, taken AFTER both
/// `Config` and `State` are released and gated on the switch actually moving the
/// active profile), the runtime seed (wait, taken OUTSIDE the state flock —
/// `JsonSync` (480) ranks OUTSIDE `State` (500), taken with no state flock
/// held, so the seed's record write cannot run under `with_state_lock`), and
/// the Services-tab wire (try-once, holds no ranked lock) — never hold `State`
/// when they reach it. Ranked OUTSIDE the state flock — taking `JsonSyncLock`
/// while holding `State` (or anything ranked inside it) trips the rank assert
/// instead of parking a peer's sync on the box's busiest lock.
pub(crate) struct JsonSyncLock {
    /// Held open for the guard's lifetime and never read: closing the fd is what
    /// releases the flock. Drops before `_rank`, so the flock releases before
    /// the `JsonSync` rank pops.
    _held: File,
    _rank: crate::lockorder::RankGuard,
}

impl JsonSyncLock {
    /// Take the flock, waiting up to [`json_sync_lock_timeout()`]. Used by the
    /// Drop final flush ([`sync_once`]) — work with no later event to retry it.
    pub(crate) fn acquire() -> Result<Self> {
        Self::acquire_with_timeout(json_sync_lock_timeout())
    }

    /// Take the flock, waiting up to `timeout`. The runtime seed bounds its
    /// wait by [`SEED_SYNC_LOCK_BUDGET`] rather than [`json_sync_lock_timeout()`]:
    /// a seed that cannot take the lock within that budget seeds unrecorded
    /// instead of parking the rotation guard on a wedged peer.
    pub(crate) fn acquire_with_timeout(timeout: Duration) -> Result<Self> {
        let file = open_json_sync_lock_file()?;
        crate::lock::lock_file_with_timeout(
            &file,
            timeout,
            |waited| anyhow::Error::new(JsonSyncLockTimeout { waited }),
            "failed to acquire .claude.json sync lock",
        )?;
        Ok(Self::from_file(file))
    }

    /// Try the flock ONCE; `Ok(None)` when another process or thread holds it.
    /// Used by the watchdog's config leg ([`sync_once_try`]), which skips a held
    /// lock rather than parking the leg, by the switch strip
    /// ([`strip_home_oauth_account`]), which defers a held strip to the next
    /// reconcile instead of blocking a daemon tick or the TUI's UI thread, and
    /// by the Services-tab wire, which refuses a held lock as a retry.
    pub(crate) fn try_once() -> Result<Option<Self>> {
        let file = open_json_sync_lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self::from_file(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).context("failed to acquire .claude.json sync lock")
            }
        }
    }

    fn from_file(file: File) -> Self {
        Self {
            _held: file,
            _rank: crate::lockorder::RankGuard::enter::<crate::lockorder::rank::JsonSync>(),
        }
    }
}

/// Open the sync-lock file owner-only under `~/.clauth`, creating the dir on
/// first use.
fn open_json_sync_lock_file() -> Result<File> {
    let dir = clauth_dir()?;
    mkdir_700(&dir).context("failed to create ~/.clauth")?;
    open_state_file(&dir.join(JSON_SYNC_LOCK_FILENAME))
        .context("failed to open .claude.json sync lock file")
}

/// The durable marker a deferred switch strip leaves under `~/.clauth`: its
/// presence means the switch-time strip could not take the sync lock (or could
/// not complete once it did) and a later pass must strip home's `oauthAccount`
/// under the lock instead. Survives a process exit — the next reconcile or
/// daemon tick in ANY process honours it. Deliberately NOT a `.claude.json`
/// member, so it never participates in the carried-rank merge or the mtime fast
/// path. Its content is a `pid:nanos` stamp so a clear can compare-and-delete
/// only the marker it read, leaving a newer deferral's marker in place.
const STRIP_PENDING_FILENAME: &str = ".oauth-strip-pending";

/// Consecutive failures to honour the pending strip (a strip or marker clear
/// that could not complete — an unwritable home or `~/.clauth`). Once it reaches
/// [`HONOUR_FAILURE_LIMIT`], [`bypass_fast_path_for_pending_strip`] stops forcing
/// a full parse on every wake; the honour is then retried only when a member's
/// mtime moves. Reset to zero by a successful honour.
const HONOUR_FAILURE_LIMIT: usize = 3;

/// Running count of consecutive honour failures (see [`HONOUR_FAILURE_LIMIT`]).
static HONOUR_FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Latch: the honour failure line is logged once per process, not once per
/// failed pass. Cleared by the next successful honour.
static HONOUR_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Home `.claude.json`'s mtime+size at the daemon tick's last honour attempt.
/// The tick's honour is bounded like the wrappers' ([`HONOUR_FAILURE_LIMIT`]):
/// once the bound is reached it re-arms only when home moves. The wrappers'
/// stat fast path re-arms on any member change (set, size or mtime); this
/// honour bound watches home alone. A `Mutex` leaf held for one stat record,
/// never across the file stat or the strip.
static LAST_HONOUR_STAT: Mutex<Option<(SystemTime, u64)>> = Mutex::new(None);

fn strip_pending_path() -> Option<PathBuf> {
    clauth_dir().ok().map(|d| d.join(STRIP_PENDING_FILENAME))
}

fn strip_pending_exists() -> bool {
    strip_pending_path().is_some_and(|p| p.exists())
}

/// Record the owed strip in the marker with a unique `pid:nanos` stamp. Rewrites
/// any marker already there, which is what makes a concurrent deferral's marker
/// distinguishable from the one an honouring holder read.
fn mark_strip_pending() -> Result<()> {
    let dir = clauth_dir()?;
    mkdir_700(&dir).context("failed to create ~/.clauth")?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let stamp = format!("{}:{nanos}", std::process::id());
    atomic_write_600(&dir.join(STRIP_PENDING_FILENAME), stamp.as_bytes())
        .context("failed to record the pending .claude.json identity strip")
}

/// The marker's stamp bytes, read BEFORE the strip so the clear afterwards can
/// compare-and-delete only this marker.
fn read_strip_pending_stamp() -> Option<Vec<u8>> {
    std::fs::read(strip_pending_path()?).ok()
}

/// Remove the marker only when its content still equals `expected` — a newer
/// deferral that rewrote the marker between the holder's read and this clear
/// survives. A vanished or unreadable marker is not an error (there is nothing
/// safely removable to report); only a `remove_file` failure that is not
/// absence is.
///
/// The compare read and the `remove_file` are still two steps, so a deferral
/// that rewrites the marker in the µs window between them is unlinked with the
/// old marker. Accepted: the holder reaches this clear only after a verified
/// strip (`StripOutcome::Done`), so home has no `oauthAccount` at that instant;
/// a block a concurrent switch writes after that verified read is stripped (or
/// re-marks) by that switch's own strip path, and the next tick or reconcile
/// honours a marker that does land.
fn clear_strip_pending(expected: &[u8]) -> Result<()> {
    let Some(path) = strip_pending_path() else {
        return Ok(());
    };
    let Ok(current) = std::fs::read(&path) else {
        return Ok(());
    };
    if current != expected {
        return Ok(());
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("failed to clear the pending .claude.json identity strip"),
    }
}

/// Record one failed honour: latch the one-line log and bump the failure count.
/// The line names the ACTUAL failure (a torn/unparseable home, a write error, a
/// home rewritten mid-strip), never a guessed permission cause.
fn record_honour_failure(reason: &'static str) {
    if !HONOUR_FAILURE_LOGGED.swap(true, Ordering::Relaxed) {
        logline!(
            "clauth: pending .claude.json identity strip could not complete ({reason}); \
             it will be retried when a member changes"
        );
    }
    HONOUR_FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Clear the failure count and latch after a successful honour, so a later
/// failure is reported again and the bypass re-arms.
fn record_honour_success() {
    HONOUR_FAILURES.store(0, Ordering::Relaxed);
    HONOUR_FAILURE_LOGGED.store(false, Ordering::Relaxed);
}

/// Test-only interleave point inside [`honor_pending_strip`], after the strip and
/// before the compare-and-delete clear. A test arms it to write a newer marker in
/// that window, proving the clear removes only the marker it read. `None` is
/// inert; armed and disarmed only while the arming test holds a `HomeSandbox`.
#[cfg(test)]
static STRIP_HONOUR_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`STRIP_HONOUR_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_strip_honour_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *STRIP_HONOUR_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`STRIP_HONOUR_HOOK`], if any.
#[cfg(test)]
fn run_strip_honour_hook() {
    let hook = STRIP_HONOUR_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Test-only interleave point inside [`strip_home_oauth_account_unlocked`],
/// after home is read and parsed and before the strip's write. A test arms it
/// to park a Claude Code write on home inside the strip's read-modify-write
/// window, making the "write lands after the parse, before the write" race
/// deterministic. `None` is inert; armed and disarmed only while the arming
/// test holds a `HomeSandbox`, so `HOME_TEST_LOCK` already serializes it.
#[cfg(test)]
static STRIP_RMW_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`STRIP_RMW_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_strip_rmw_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *STRIP_RMW_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`STRIP_RMW_HOOK`], if any.
#[cfg(test)]
fn run_strip_rmw_hook() {
    let hook = STRIP_RMW_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Reset the process-global honour-failure count and latch. Test-only: the count
/// is process-global, so a leak between sandboxes would carry a previous test's
/// failures into the next. `HomeSandbox` clears it alongside
/// [`reset_last_synced`].
#[cfg(test)]
pub(crate) fn reset_pending_strip_state() {
    HONOUR_FAILURES.store(0, Ordering::Relaxed);
    HONOUR_FAILURE_LOGGED.store(false, Ordering::Relaxed);
    *LAST_HONOUR_STAT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Honour a switch strip the try-once strip deferred: strip home's
/// `oauthAccount` and clear the marker, under the sync lock the caller already
/// holds. The strip records its own home rewrite with the rank of the Claude
/// Code write its bytes carry (home's own rank as the strip read it), so a
/// runtime copy's newer Claude Code write outranks the strip's echo and home's
/// own newer write is not reverted. The strip and
/// the merge serialize — both run under [`JsonSyncLock`], and the strip never
/// runs under the state flock.
///
/// Best-effort and infallible: a strip that still cannot complete keeps the
/// marker and is recorded via [`record_honour_failure`] (latch + bound) rather
/// than failing the merge pass. The clear is compare-and-delete on the stamp read
/// BEFORE the strip, so a marker a concurrent deferral wrote during the
/// strip-to-clear window survives.
fn honor_pending_strip() {
    if !strip_pending_exists() {
        return;
    }
    let stamp = read_strip_pending_stamp();
    match strip_home_oauth_account_unlocked() {
        StripOutcome::Done => {
            #[cfg(test)]
            run_strip_honour_hook();
            // No stamp read (the marker vanished or is unreadable): leave
            // whatever is there rather than delete a marker we cannot compare.
            if let Some(s) = stamp {
                if clear_strip_pending(&s).is_err() {
                    record_honour_failure("the pending marker could not be cleared");
                } else {
                    record_honour_success();
                }
            }
        }
        StripOutcome::Deferred(reason) => record_honour_failure(reason),
    }
}

/// Whether the daemon tick's honour should attempt this tick: within the
/// failure bound it always does; once bound, only when home's mtime or size
/// moved since the last attempt — a retry can only newly succeed once the file
/// changed. The wrappers' stat fast path re-arms on any member change (set,
/// size or mtime); this honour bound watches home alone. Records the stat each
/// call so the next bound check compares against the latest attempt.
fn honour_within_bound() -> bool {
    let within = HONOUR_FAILURES.load(Ordering::Relaxed) < HONOUR_FAILURE_LIMIT;
    let Some((mtime, size)) = home_claude_json_stat() else {
        return true; // cannot stat home — attempt rather than skip a possibly-healed file
    };
    let mut slot = LAST_HONOUR_STAT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let changed = slot.as_ref().is_none_or(|(m, s)| *m != mtime || *s != size);
    *slot = Some((mtime, size));
    within || changed
}

/// Home `.claude.json`'s current mtime and size, or `None` when the file (or
/// its mtime) cannot be statted.
fn home_claude_json_stat() -> Option<(SystemTime, u64)> {
    let path = home_dir().ok()?.join(".claude.json");
    let meta = std::fs::metadata(&path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Honour a pending strip from the daemon tick: try the sync lock ONCE (never
/// waits, so the tick thread is never parked behind a held lock), and when taken,
/// honour the marker under it. Called every tick so a deferral with no live
/// `clauth start` wrapper still lands while the daemon runs. Bounded by
/// [`honour_within_bound`]: after [`HONOUR_FAILURE_LIMIT`] consecutive failures
/// the tick stops the full read-modify-write until home moves, instead of
/// retrying every tick forever while the strip cannot complete.
pub(crate) fn try_honor_pending_strip() {
    if !strip_pending_exists() {
        return;
    }
    if !honour_within_bound() {
        return;
    }
    if let Ok(Some(_lock)) = JsonSyncLock::try_once() {
        honor_pending_strip();
    }
}

/// CC's cached account uuid from `~/.claude.json`'s `oauthAccount.accountUuid`,
/// or `None` when the file, block, or value is absent/blank/unparseable.
/// Read-then-parse with the same discipline as [`strip_home_oauth_account`]: a
/// missing file is never created and a file caught mid-write by CC is left
/// untouched rather than clobbered. CC trusts this cached block and does not
/// re-derive it from a swapped credentials file — so a hit is "CC's last booted
/// identity", not fresh proof of the live token's account.
pub(crate) fn home_oauth_account_uuid() -> Option<AccountId> {
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
        .map(AccountId::from)
}

/// Delete the stale `oauthAccount` identity block from the home
/// `~/.claude.json` on profile switch (issue #17). Claude Code trusts a
/// cached identity block and does not re-derive it from a relinked
/// credentials file on a normal startup; dropping the block instead lets it
/// self-heal — probed on CC 2.1.201: an
/// absent block re-derives the correct identity from the token within
/// seconds, a present-but-wrong one never self-corrects.
///
/// Best-effort: never fails a committed switch. The strip TRY-ONCEs the
/// [`JsonSyncLock`] so a daemon tick or the TUI's UI thread never blocks past a
/// short bound, and when it cannot complete it records the strip as owed — the
/// next reconcile (or daemon tick), which holds the lock across its whole pass,
/// strips home's `oauthAccount` under that same lock (see
/// [`honor_pending_strip`]) and clears the marker. The marker is durable, so a
/// strip deferred across a process exit still lands. Every caller takes this
/// AFTER the switch's `Config` and `State` holds are released (gated on the
/// switch actually moving the active profile), never under `with_state_lock` —
/// the strip and the reconcile serialize on the sync lock alone.
///
/// Logs "deferred" only when it really deferred — a held lock, an unparseable
/// home (CC mid-write), a home that moved between the read and the write, a
/// read/write failure, or a re-read that still shows the block — never on a
/// strip that landed. A missing file is left uncreated, and a file that fails to
/// parse is left untouched rather than clobbered. A write only happens when the
/// key is actually present: an already-clean file is never touched. The home
/// rewrite drops only `oauthAccount` and records the rank of the Claude Code
/// write its bytes carry (home's own rank as the strip read it), so a runtime
/// copy's newer Claude Code write outranks the strip's echo and home's own newer
/// write is not reverted.
pub(crate) fn strip_home_oauth_account() {
    if try_strip_home_oauth_account() {
        logline!("clauth: .claude.json identity strip deferred; it will be retried");
    }
}

/// [`strip_home_oauth_account`]'s body: try the sync lock ONCE, strip, and clear
/// (or record) the owed marker. Returns `true` when the strip was deferred — a
/// held lock, an unparseable home (CC mid-write), a home that moved between the
/// read and the write, a read/write failure, or a re-read that still shows the
/// block — the owed strip surviving via the marker.
fn try_strip_home_oauth_account() -> bool {
    match JsonSyncLock::try_once() {
        Ok(Some(_lock)) => {
            let stamp = read_strip_pending_stamp();
            match strip_home_oauth_account_unlocked() {
                StripOutcome::Done => {
                    // Clear only the marker this strip set out to clear (if any):
                    // a marker a concurrent deferral wrote during the strip-to-
                    // clear window survives.
                    if let Some(s) = stamp {
                        let _ = clear_strip_pending(&s);
                    }
                    false
                }
                StripOutcome::Deferred(_) => {
                    let _ = mark_strip_pending();
                    true
                }
            }
        }
        Ok(None) | Err(_) => {
            let _ = mark_strip_pending();
            true
        }
    }
}

/// What an unlocked strip attempt achieved.
enum StripOutcome {
    /// The home file was written without an `oauthAccount` (verified by re-read),
    /// or had none to begin with, or is missing — nothing to strip. The marker
    /// the caller read may clear.
    Done,
    /// The strip could not complete now — an unparseable home (CC mid-write), a
    /// home that moved between the read and the write, a read/write failure, or
    /// a re-read that still shows the block. The owed strip must survive to a
    /// later pass. The `&'static str` names the actual cause for
    /// [`record_honour_failure`]'s latched log line.
    Deferred(&'static str),
}

/// [`strip_home_oauth_account`]'s body, with [`JsonSyncLock`] already held.
/// Infallible: every way the strip cannot complete folds into
/// [`StripOutcome::Deferred`], so a caller keeps (or writes) the marker instead
/// of clearing it. Rewrites home without `oauthAccount` and records the rank of
/// the Claude Code write its bytes carry, so an older runtime copy never
/// outranks home's own newer Claude Code write the strip copied forward.
fn strip_home_oauth_account_unlocked() -> StripOutcome {
    let Ok(home) = home_dir() else {
        return StripOutcome::Deferred("no home directory");
    };
    let path = home.join(".claude.json");
    // Capture the stat BEFORE the read so the re-stat just before the write can
    // tell a concurrent Claude Code write from an unchanged file.
    let (read_mtime, read_size) = match std::fs::metadata(&path) {
        Ok(meta) => match meta.modified() {
            Ok(mtime) => (mtime, meta.len()),
            Err(_) => return StripOutcome::Deferred("home mtime could not be read"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StripOutcome::Done; // missing — never create
        }
        Err(_) => return StripOutcome::Deferred("home could not be statted"),
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StripOutcome::Done; // missing — never create
        }
        Err(_) => return StripOutcome::Deferred("home could not be read"),
    };
    let parsed = match serde_json::from_slice::<Value>(&bytes) {
        Ok(parsed) => parsed,
        Err(_) => return StripOutcome::Deferred("home is unparseable (Claude Code mid-write)"),
    };
    let Value::Object(mut obj) = parsed else {
        return StripOutcome::Deferred("home is not a JSON object");
    };
    // Test-only interleave: after the read+parse, before the write — the span a
    // Claude Code write landing in is reverted unless the re-stat below sees it.
    #[cfg(test)]
    run_strip_rmw_hook();
    if obj.remove("oauthAccount").is_none() {
        return StripOutcome::Done; // already clean — avoid a pointless mtime bump
    }
    let Ok(rewritten) = serde_json::to_vec_pretty(&Value::Object(obj)) else {
        return StripOutcome::Deferred("home could not be re-serialized");
    };
    // Compute the carried rank BEFORE the re-stat, so the re-stat-to-rename
    // window holds no record read and no hash. Home's own rank as the strip read
    // it (its mtime when home is not an echo, its entry's carried rank when it
    // is) keeps an older runtime copy from outranking home's own newer Claude
    // Code write the strip copied forward, while a runtime write newer than the
    // strip's source still outranks the strip's echo. The record path shares
    // [`written_record_path`] with the merge; `home_dir()` succeeded above, so
    // this is always `Some`.
    let Some(record) = written_record_path() else {
        return StripOutcome::Deferred("no home directory");
    };
    let carried_rank =
        crate::jsonsync::carried_rank_of(&record, &path, read_mtime, read_size, &bytes);
    // Re-stat immediately before the write: a Claude Code write landing inside
    // this read-modify-write window would be reverted by the strip's stale
    // parse, so defer and let the marker retry.
    if member_changed_since_read(&path, read_mtime, read_size) {
        return StripOutcome::Deferred("home was rewritten during the strip");
    }
    // A failed home write defers with its own cause, distinct from a home that
    // moved under the strip.
    if crate::jsonsync::write_copy_and_record(
        Some(&record),
        &path,
        &rewritten,
        carried_rank,
        false,
        None,
    )
    .is_err()
    {
        return StripOutcome::Deferred("the home write failed");
    }
    // Verified write: only a re-read showing no `oauthAccount` counts the strip
    // done; anything else defers so the marker survives.
    match std::fs::read(&path)
        .ok()
        .and_then(|re| serde_json::from_slice::<Value>(&re).ok())
    {
        Some(Value::Object(re)) if re.get("oauthAccount").is_none() => StripOutcome::Done,
        _ => StripOutcome::Deferred("home was rewritten during the strip"),
    }
}

/// The model families Claude Code bills against usage credits, derived from
/// the cached `tengu_calm_seal` flag: `fable` always, then every family the
/// flag's array lists, trimmed + lowercased, empty and non-string entries
/// skipped, deduped, order-stable. A non-array flag adds nothing.
pub(crate) fn credit_billed_families_from(flag: Option<&Value>) -> Vec<String> {
    let mut families = vec!["fable".to_string()];
    let Some(Value::Array(items)) = flag else {
        return families;
    };
    for item in items {
        let Some(s) = item.as_str() else {
            continue;
        };
        let s = s.trim().to_lowercase();
        if s.is_empty() || families.contains(&s) {
            continue;
        }
        families.push(s);
    }
    families
}

/// The credit-billed families off `home_dir()/.claude.json`'s
/// `cachedGrowthBookFeatures.tengu_calm_seal`, read with the same discipline
/// as [`home_oauth_account_uuid`]: a missing file is never created and reads
/// no flag. Reads ONLY that key — `oauthAccount` beside it is never logged,
/// printed or copied. A fresh read per call (the file is ~100 KB), so a flag
/// CC just rewrote is seen on the next call; a file caught mid-write
/// (unparseable) answers with this path's last good parse instead of flapping,
/// and a path never parsed yet answers `fable` alone. A read that fails for
/// any reason but absence (a directory, EACCES, ...) is no evidence the flag
/// changed, so it answers like the torn parse; absence itself answers `fable`
/// alone and never creates the file.
pub(crate) fn credit_billed_families() -> Vec<String> {
    let Some(path) = home_dir().ok().map(|h| h.join(".claude.json")) else {
        return credit_billed_families_from(None);
    };
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(obj)) => {
                let flag = obj
                    .get("cachedGrowthBookFeatures")
                    .and_then(|f| f.get("tengu_calm_seal"));
                let families = credit_billed_families_from(flag);
                CREDIT_BILLED_FAMILIES
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(path, families.clone());
                families
            }
            // Unparseable (CC mid-write): keep the last good parse for this
            // path, exactly as the sync engine skips the torn file until the
            // next tick.
            _ => last_good_families(&path),
        },
        // Absence is a real answer: no file, no flag, never create it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => credit_billed_families_from(None),
        // Any other read failure is no evidence the flag changed, so it answers
        // exactly like the torn parse: the last good set for this path.
        Err(_) => last_good_families(&path),
    }
}

/// The last good credit-billed family set for `path`, else `fable` alone when
/// this path has never parsed yet. One lock, one map lookup — a leaf.
fn last_good_families(path: &Path) -> Vec<String> {
    CREDIT_BILLED_FAMILIES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(path)
        .cloned()
        .unwrap_or_else(|| credit_billed_families_from(None))
}

#[cfg(test)]
#[path = "../tests/inline/claude_json.rs"]
mod tests;
