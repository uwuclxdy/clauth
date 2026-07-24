//! Cross-process serialization of state mutations.
//!
//! All disk writes that touch shared clauth state (profiles.toml, per-profile
//! config/credentials, ~/.claude/settings.json, .credentials.json symlink) run
//! under an exclusive advisory file lock on ~/.clauth/.lock. This stops two
//! concurrent clauth instances from interleaving read-modify-write cycles and
//! losing each other's changes, racing OAuth refresh-token rotations, or
//! clobbering the active-profile symlink.
//!
//! The lock is re-entrant within the same thread so high-level actions
//! (e.g. `switch_profile`) can take the lock and still call helpers that take
//! it themselves without deadlocking. Two different threads of the same process
//! calling `with_state_lock` concurrently are fully serialized — only one
//! executes its closure at a time.
//!
//! Acquiring the cross-process flock is bounded by [`STATE_LOCK_TIMEOUT`]. A
//! blocking flock has no deadline, so a lease-holding fetcher whose rotation path
//! waits on a lock another clauth process holds forever would pin the usage-fetch
//! lease and stand every TUI down permanently (no watchdog covers the scheduler
//! thread). A bounded wait turns that silent wedge into a [`StateLockTimeout`] the
//! caller retries instead of a hang.

use std::cell::Cell;
use std::fs::File;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::logline::logline;
use crate::profile::clauth_dir;

const LOCK_FILENAME: &str = ".lock";

/// Deadline for taking the cross-process state flock before giving up with a
/// [`StateLockTimeout`]. Sized to sit between two hard bounds: the macOS switch
/// path holds this flock across the `/usr/bin/security` shell-out (up to its 20 s
/// kill deadline, `keychain.rs`), so a shorter deadline would false-timeout a
/// waiter during a legit slow switch; the daemon's 30 s `WATCHDOG_DEADLINE` caps
/// it from above, so a main-loop drain waiting on the flock returns before the
/// watchdog false-aborts.
/// On Linux the flock is only ever held across sub-millisecond disk writes, so
/// only a genuine wedge ever reaches this deadline.
const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(25);

/// How often [`StateLock::acquire`] re-polls the flock while waiting. Small enough
/// that a freed lock is taken promptly, large enough that the busy-wait costs
/// nothing over a multi-second deadline.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The state lock could not be taken within [`STATE_LOCK_TIMEOUT`]: another clauth
/// process is holding `~/.clauth/.lock`. A recoverable, retry-later condition kept
/// as a distinct type (surfaced through `anyhow`) so a caller can `downcast_ref`
/// and retry rather than treat it as a hard error. The scheduler's fetch tick
/// falls back to the disk cache and retries next tick without dropping its
/// usage-fetch lease.
#[derive(Debug)]
pub(crate) struct StateLockTimeout {
    waited: Duration,
}

impl std::fmt::Display for StateLockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "timed out after {:.0}s acquiring the state lock; another clauth process holds ~/.clauth/.lock",
            self.waited.as_secs_f64()
        )
    }
}

impl std::error::Error for StateLockTimeout {}

// Serializes all threads of this process across the full closure duration.
// The guard is stored in the outermost StateLock and dropped only when that
// StateLock drops, so no second thread can enter while any thread is inside.
static THREAD_LOCK: Mutex<Option<File>> = Mutex::new(None);

// Per-thread reentrancy depth. Non-zero means this thread already holds
// THREAD_LOCK and must not try to re-acquire it (non-reentrant Mutex).
thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[must_use]
pub(crate) struct StateLock {
    // Non-None only for the outermost acquisition on this thread.
    // Holds THREAD_LOCK for the full closure lifetime; None for reentrant calls.
    _thread_guard: Option<std::sync::MutexGuard<'static, Option<File>>>,
    // Holds the STATE rank in the global lock order — pushed once on the
    // outermost acquisition, popped on its drop. None for reentrant calls so
    // the rank is not double-pushed (it is already held by the outer frame).
    _rank: Option<crate::lockorder::RankGuard>,
}

impl StateLock {
    /// Acquire the state lock, bounding the cross-process flock wait by
    /// [`STATE_LOCK_TIMEOUT`]. A timeout surfaces as a [`StateLockTimeout`].
    pub(crate) fn acquire() -> Result<Self> {
        Self::acquire_with_timeout(STATE_LOCK_TIMEOUT)
    }

    /// [`acquire`](Self::acquire) with an explicit flock deadline. Split out so
    /// tests drive the timeout path with a short deadline; production always uses
    /// [`STATE_LOCK_TIMEOUT`].
    pub(crate) fn acquire_with_timeout(timeout: Duration) -> Result<Self> {
        let depth = DEPTH.get();
        if depth > 0 {
            // This thread already holds the mutex — increment depth. A reentrant
            // call never re-touches the flock, so the deadline does not apply.
            #[allow(
                clippy::expect_used,
                reason = "lock depth overflow is a programming error, unrecoverable"
            )]
            DEPTH.set(
                depth
                    .checked_add(1)
                    .expect("clauth state lock depth overflow"),
            );
            return Ok(Self {
                _thread_guard: None,
                _rank: None,
            });
        }

        // Outermost acquisition: block until we own the thread mutex.
        // `THREAD_LOCK` is `static`, so `.lock()` yields `MutexGuard<'static, _>`
        // directly — storable in `StateLock` with no lifetime laundering.
        // Poison recovery: proceed even if a previous holder panicked. This wait
        // is itself unbounded, but its holder is not: the mutex is held only across
        // the bounded flock acquisition below plus the closure (fast disk writes,
        // or a keychain shell-out with its own kill deadline), so a waiter here can
        // never block longer than that.
        let mut guard: std::sync::MutexGuard<'static, Option<File>> = match THREAD_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        if guard.is_none() {
            let dir = clauth_dir()?;
            // TECH-9 #13: 0o700 (was create_dir_all → umask 0o755).
            crate::profile::mkdir_700(&dir).context("failed to create ~/.clauth")?;
            // upstream 0.12.0: owner-only (0o600) lock file via open_state_file,
            // which subsumes the fork's read/write/create/truncate(false) open.
            let file = crate::profile::open_state_file(&dir.join(LOCK_FILENAME))
                .context("failed to open clauth state lock file")?;
            // On timeout `guard` drops here, releasing THREAD_LOCK with the slot
            // still `None` and DEPTH still 0 — a clean unwind, no rank entered.
            lock_file_with_timeout(&file, timeout)?;
            *guard = Some(file);
        }

        DEPTH.set(1);

        // Enter the STATE rank on the outermost hold. `config` (rank CONFIG) may
        // already be held — STATE sits inside it; `RankGuard::enter` asserts it.
        let rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::State>();

        Ok(Self {
            _thread_guard: Some(guard),
            _rank: Some(rank),
        })
    }
}

/// Take the exclusive advisory flock on `file`, re-polling every
/// [`LOCK_POLL_INTERVAL`] until it is free or `timeout` elapses. A `WouldBlock`
/// past the deadline returns [`StateLockTimeout`] (logged once so a wedge is
/// diagnosable); a real IO error propagates as-is.
fn lock_file_with_timeout(file: &File, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    let timed_out = StateLockTimeout { waited: timeout };
                    logline!("clauth: {timed_out}");
                    return Err(anyhow::Error::new(timed_out));
                }
                std::thread::sleep(LOCK_POLL_INTERVAL.min(deadline - now));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).context("failed to acquire clauth state lock");
            }
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let depth = DEPTH.get();
        let new_depth = depth.saturating_sub(1);
        DEPTH.set(new_depth);

        if new_depth == 0 {
            // Outermost unwind. Close the flock file so other processes can
            // acquire it, then release the thread mutex so other threads of
            // this process can enter. Both happen when _thread_guard drops.
            if let Some(ref mut g) = self._thread_guard {
                **g = None; // close the File → flock released
            }
        }
        // Reentrant calls have _thread_guard = None; nothing extra to do.
    }
}

/// Run `f` while holding the cross-process state lock. Re-entrant within the
/// same thread; serializes concurrent calls from different threads.
pub(crate) fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = StateLock::acquire()?;
    f()
}

#[cfg(test)]
#[path = "../tests/inline/lock.rs"]
mod tests;
