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

pub(crate) const LOCK_FILENAME: &str = ".lock";

/// Deadline for taking the cross-process state flock before giving up with a
/// [`StateLockTimeout`]. Sized to sit between two hard bounds: the macOS switch
/// path holds this flock across the `/usr/bin/security` shell-outs (`keychain.rs`),
/// so a shorter deadline would false-timeout a waiter during a legit slow switch;
/// the daemon's 30 s `WATCHDOG_DEADLINE` caps it from above, so a main-loop drain
/// waiting on the flock returns before the watchdog false-aborts. What keeps the
/// first bound from moving is [`SUBPROCESS_BUDGET`], which caps the shell-outs of
/// one hold in aggregate rather than one at a time. On Linux the flock is only
/// ever held across sub-millisecond disk writes, so only a genuine wedge ever
/// reaches this deadline.
const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(25);

/// The flock deadline [`StateLock::acquire`] waits out: [`STATE_LOCK_TIMEOUT`],
/// or a shorter value a test poses a wedge under. The one source of the deadline
/// so a test can shrink the whole wait — the pre-teardown sync legs included —
/// without sleeping 25 s per acquisition. Production never sets the override, so
/// the deadline is [`STATE_LOCK_TIMEOUT`] everywhere outside `cfg(test)`.
pub(crate) fn state_lock_timeout() -> Duration {
    #[cfg(test)]
    if let Some(t) = STATE_LOCK_TIMEOUT_OVERRIDE.with(|c| c.get()) {
        return t;
    }
    STATE_LOCK_TIMEOUT
}

/// Wall-clock ceiling on everything ONE state-lock hold may spend in
/// subprocesses, shared across every call it makes rather than granted per call.
///
/// Only macOS shells out under this lock (`keychain.rs` → `/usr/bin/security`),
/// and a per-call deadline cannot bound a hold: `switch_profile` adopting a first
/// login runs the Keychain mirror TWICE in one acquisition (its
/// `adopt_first_login` relinks the outgoing profile, then the switch relinks the
/// incoming one), each a read plus a write. At a 10 s per-call deadline that hold
/// reached 40 s against the 25 s [`STATE_LOCK_TIMEOUT`] a peer waits out, so a
/// legitimate switch false-timed-out the peer — the outcome that constant exists
/// to prevent. Bounding the shell-outs is what the daemon's own comment prescribed
/// over loosening the deadlines above it.
///
/// 20 s leaves 5 s of [`STATE_LOCK_TIMEOUT`] for the disk work around the
/// shell-outs, which is sub-millisecond. What a shared budget costs, and it is
/// real: the LAST call under a hold gets only what earlier calls left, so a switch
/// whose first mirror burned the budget on a stuck keychain fails loudly where it
/// used to get a fresh deadline and might have finished. That trade is deliberate.
/// A stuck keychain means an unanswered one-time ACL dialog or a locked keychain,
/// both of which fail the switch anyway; the write path is idempotent, so the
/// operator answers the dialog and retries.
///
/// It bounds ONE hold, not one tick: the daemon drains `pending_switch` and
/// `pending_switch_off` under two SEPARATE acquisitions, so a tick doing both can
/// still spend 2 × this against `WATCHDOG_DEADLINE`.
/// Arming a second budget for a wider scope needs an arm-if-not-armed rule that
/// [`StateLock::acquire_with_timeout`] does not have today, since it is the only
/// armer.
const SUBPROCESS_BUDGET: Duration = Duration::from_secs(20);

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

impl StateLockTimeout {
    /// A pre-built timeout for tests that pin how OTHER modules render this
    /// error (contention, never a permissions fault) without staging a real
    /// cross-process flock wait.
    #[cfg(test)]
    pub(crate) fn stub() -> Self {
        Self {
            waited: STATE_LOCK_TIMEOUT,
        }
    }
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

// When this thread's [`SUBPROCESS_BUDGET`] runs out, or None when the thread
// holds no state lock. Set by the OUTERMOST acquisition only, so a reentrant
// hold keeps spending the budget it entered rather than resetting it — which is
// the whole point, since the two mirrors of an adopting switch reach the
// keychain through nested `with_state_lock` frames.
thread_local! {
    static HOLD_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// `base`, clamped to what is left of this thread's [`SUBPROCESS_BUDGET`].
/// Returns `base` untouched when the thread holds no state lock, which is the
/// right answer rather than a fallback: a caller outside the lock (`oauth.rs`
/// mirrors a rotation after its lock closure ends) blocks no peer, so nothing is
/// waiting on it to finish. An exhausted budget clamps to zero, which the
/// subprocess layer refuses outright rather than spawning: nothing completes in
/// zero time, and the write path hands its payload to the child before the
/// deadline is ever checked.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only the macOS Keychain shells out under this lock; the budget is pinned on every platform"
    )
)]
pub(crate) fn clamp_to_hold_budget(base: Duration) -> Duration {
    match HOLD_DEADLINE.get() {
        Some(deadline) => base.min(deadline.saturating_duration_since(Instant::now())),
        None => base,
    }
}

// Test-only per-thread counter: increments once per OUTERMOST acquisition, i.e.
// once per real flock wait. What it makes falsifiable is that a batching caller
// takes the flock once for N items rather than N times — the difference between
// one tick risking `STATE_LOCK_TIMEOUT` and one risking N × it. Thread-local, so
// parallel tests cannot pollute each other's count.
#[cfg(test)]
thread_local! {
    pub(crate) static OUTERMOST_ACQUISITIONS: Cell<u64> = const { Cell::new(0) };
}

// Test seam shortening `state_lock_timeout` so a wedge can be posed without a
// real 25 s wait. `None` is the production deadline. Thread-local, so a test
// that shortens it only ever affects the thread it drops the runtime on.
#[cfg(test)]
thread_local! {
    static STATE_LOCK_TIMEOUT_OVERRIDE: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// Set or clear the test-only deadline override. `None` restores
/// [`STATE_LOCK_TIMEOUT`].
#[cfg(test)]
pub(crate) fn set_state_lock_timeout_override(timeout: Option<Duration>) {
    STATE_LOCK_TIMEOUT_OVERRIDE.with(|c| c.set(timeout));
}

/// Zero-sized proof that the current thread holds the cross-process state
/// flock. Only [`with_state_lock`] mints it, handing one to its closure, so a
/// writer of shared state ([`crate::profile::AppState::set_active`],
/// [`crate::profile::Profile::set_credentials`]) can require it in its
/// signature instead of leaving the hold to a comment.
///
/// Deliberately not `Copy` or `Clone`: a copied witness would outlive the
/// hold it proves, keeping the compile-time contract bypassable.
#[derive(Debug)]
pub(crate) struct StateLockHeld(());

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
        Self::acquire_with_timeout(state_lock_timeout())
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
            crate::profile::mkdir_700(&dir).context("failed to create ~/.clauth")?;
            let file = crate::profile::open_state_file(&dir.join(LOCK_FILENAME))
                .context("failed to open clauth state lock file")?;
            // On timeout `guard` drops here, releasing THREAD_LOCK with the slot
            // still `None` and DEPTH still 0 — a clean unwind, no rank entered.
            lock_file_with_timeout(&file, timeout)?;
            *guard = Some(file);
        }

        DEPTH.set(1);
        #[cfg(test)]
        OUTERMOST_ACQUISITIONS.with(|c| c.set(c.get() + 1));

        // Enter the STATE rank on the outermost hold. `config` (rank CONFIG) may
        // already be held — STATE sits inside it; `RankGuard::enter` asserts it.
        let rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::State>();

        // Arm the shared subprocess budget for this hold. After the rank guard,
        // which panics on a rank violation: an arm before it would outlive the
        // unwind with no `StateLock` left to disarm it.
        HOLD_DEADLINE.set(Some(Instant::now() + SUBPROCESS_BUDGET));

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
pub(crate) fn lock_file_with_timeout(file: &File, timeout: Duration) -> Result<()> {
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
            // Outermost unwind. Clearing the File from the guard closes the fd (releasing the flock for other processes); the thread mutex releases when _thread_guard drops at the end of this fn.
            if let Some(ref mut g) = self._thread_guard {
                **g = None; // close the File → flock released
            }
            // Nobody waits on this thread once the flock is free, so the next
            // hold starts on a full budget. Cleared on a panic unwind too, since
            // Drop runs there — a poisoned lock must not leave a spent budget
            // behind to strangle the recovery path.
            HOLD_DEADLINE.set(None);
        }
        // Reentrant calls have _thread_guard = None; nothing extra to do.
    }
}

/// Run `f` while holding the cross-process state lock, handing it the
/// [`StateLockHeld`] witness. Re-entrant within the same thread; serializes
/// concurrent calls from different threads.
pub(crate) fn with_state_lock<T>(f: impl FnOnce(&StateLockHeld) -> Result<T>) -> Result<T> {
    let _guard = StateLock::acquire()?;
    f(&StateLockHeld(()))
}

#[cfg(test)]
#[path = "../tests/inline/lock.rs"]
mod tests;
