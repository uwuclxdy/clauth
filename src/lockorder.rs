//! Globally-ordered locks that enforce a single acquisition order in code.
//!
//! Every shared lock in clauth carries a *rank* — its position in one global
//! order. A thread may only acquire a lock whose rank is strictly greater than
//! the highest rank it already holds. Acquiring out of that order is the
//! classic lock-order-inversion that deadlocks, so we assert it the moment a
//! lock is taken. What used to be prose ("`usage_store` before `config`", "never
//! two leaf mutexes at once", "`RotationGuard` outermost") is now an executable
//! check that fails loudly in tests and dev runs.
//!
//! The assertion and its bookkeeping are `cfg(debug_assertions)`-only: release
//! builds compile the rank stack out entirely, so [`RankedMutex`] is a
//! zero-overhead wrapper around [`std::sync::Mutex`] in production.
//!
//! ## Deriving the order
//!
//! Only *nested* holdings constrain the order — a sequential acquire-then-drop
//! imposes nothing. The order below is the transitive closure of every nested
//! holding in the codebase:
//!
//! - `RotationGuard` is held across the OAuth HTTP round trip — outermost.
//! - `partition_due`: `last_fetched` → `activity`.
//! - `apply_usage`: `usage_store` → `usage_status` → `config`.
//! - rotation/save sites: `config` → state flock → `activity`.
//! - `/profile` TTL clock: `rotation` → `profile_ttl` (post-401 retry) and
//!   `config` → `profile_ttl` (the account-swap actions that expire it).
//!
//! Standalone leaves (`refetch_queue`, the `pending_*` sets) are never nested
//! with another tracked lock; they are ranked above the rest so that a future
//! accidental nesting under any held lock still *increases* the rank rather than
//! inverting it.
//!
//! Test builds prepend two `cfg(test)`-only ranks BELOW `Rotation` — the home
//! and tier scaffolding locks — so a sandbox held for a whole test is outer to
//! every production lock the code under test takes. See the `ranks!` block.

use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, PoisonError};

/// Sealed so only the rank markers defined in [`rank`] can implement [`Rank`].
/// Nothing outside this module can name a fresh `Rank` type, which is what makes
/// arbitrary-rank [`RankedMutex`] / [`RankGuard`] construction impossible.
mod sealed {
    pub(crate) trait Sealed {}
}

/// A position in the global lock order. Implemented only by the zero-sized
/// markers in [`rank`]; the sealed supertrait blocks any other implementation.
/// `VALUE` is the rank's u16 weight — lower = acquired earlier (outer).
pub(crate) trait Rank: sealed::Sealed {
    // Only read inside the `cfg(debug_assertions)` rank check in
    // `RankGuard::enter`; release builds compile that read out, leaving the
    // const unreferenced. The order it encodes is still load-bearing.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    const VALUE: u16;
}

/// Global lock order. Lower value = outer. Gaps leave room to insert future
/// locks without renumbering; only the relative order matters. Each rank is an
/// uninhabited marker implementing [`Rank`]; the raw u16 weights are private to
/// this module so the order can't be forged elsewhere.
pub(crate) mod rank {
    use super::{Rank, sealed::Sealed};

    /// Defines a rank marker, its `Rank::VALUE`, and the `Sealed` impl in one
    /// shot so a new rank can never be added half-sealed.
    macro_rules! ranks {
        ($($(#[$m:meta])* $name:ident = $value:literal;)*) => {$(
            $(#[$m])*
            pub(crate) enum $name {}
            $(#[$m])*
            impl Sealed for $name {}
            $(#[$m])*
            impl Rank for $name {
                const VALUE: u16 = $value;
            }
        )*};
    }

    ranks! {
        // Test-only scaffolding locks, ranked OUTERMOST (below every production
        // rank): the RAII sandboxes that hold them (`testutil::HomeSandbox` /
        // `TierSandbox`, `showcase::ShowcaseHome`, runtime's `with_fake_home`)
        // wrap the whole test, so the code under test still legally acquires any
        // production lock inside them. Two ranks, not one, so a future test that
        // needs both cannot invert them into a deadlock: acquire `HomeTest`
        // before `TierTest`, both before any real lock.
        /// `profile::HOME_TEST_LOCK` — serializes `home_dir()` redirects across
        /// the threads a `cargo test` fallback shares.
        #[cfg(test)]
        HomeTest = 20;
        /// `theme::TIER_TEST_LOCK` — serializes color-tier pins across those
        /// same threads.
        #[cfg(test)]
        TierTest = 40;
        /// `RotationGuard` (per-profile rotation flock). Held across HTTP, outermost.
        Rotation = 100;
        /// Process-wide per-host request-spacing clock in `usage::fetch` (keyed by
        /// endpoint origin: the Anthropic OAuth host and each api-key provider host).
        /// Held only to reserve the next request slot, never across the sleep or
        /// the HTTP round trip; ranked just inside `Rotation` because the
        /// post-rotation retry reserves a slot while the rotation flock is held.
        UsageThrottle = 150;
        LastFetched = 200;
        /// Per-profile poll-health streaks (consecutive 429s, consecutive
        /// transient token-refresh failures) driving exponential backoff in
        /// `usage::scheduler`. Leaf — bumped and released before the
        /// `last_fetched`/`status` write in `apply_outcome`.
        PollStreak = 220;
        /// Per-profile kick-429 block state (`usage::scheduler::KickBlocks`):
        /// the messages endpoint is rejecting the 5h auto-start kick. Leaf like
        /// `PollStreak` — read/copied alone, released before any other lock,
        /// and its cache-file IO stays outside the guard.
        KickBlockState = 230;
        Tokens = 250;
        ThirdParty = 260;
        ThirdPartyUsageStore = 270;
        ThirdPartyStatus = 280;
        UsageStore = 300;
        UsageStatus = 350;
        Config = 400;
        /// `/profile` re-fetch TTL clock in `usage::fetch` (in-memory memo +
        /// durable stamp). A true leaf: every acquisition is take-read/insert-
        /// release, and the stamp's disk IO stays outside it, so nothing is ever
        /// acquired while it is held. That is what lets it rank as late as its
        /// HOLDERS need rather than as early as it is taken — ranking a true leaf
        /// later is monotonically safe, since a higher rank can only legalize call
        /// sites, never invert an existing one. Two real holders: `Rotation`, on
        /// every `fetch_with_rotation` → `fetch_raw` → take path that runs under
        /// the rotation guard, and `Config`, on the account-swap actions that
        /// expire it.
        ///
        /// Deliberately INSIDE `Config` (400) and OUTSIDE `State` (500). The rank
        /// is what enforces the latter: taking or expiring the clock does file IO,
        /// and the state flock is a CROSS-PROCESS serialization point, so holding
        /// it across that IO lengthens contention for every other clauth process.
        /// Move an `expire_profile_ttl` back inside a `with_state_lock` and this
        /// asserts.
        ProfileTtl = 450;
        /// `with_state_lock` (cross-process state flock). Inner of `config`.
        State = 500;
        Activity = 600;
        // Standalone leaves — never nested with another tracked lock.
        NextRefresh = 1100;
        RefetchQueue = 1200;
        /// Session-scoped set of generic profiles suppressed from the timer until
        /// a manual refresh (`usage::scheduler`). Leaf — acquired standalone in
        /// `tick`/`fetch_third_party_due`, never under another lock.
        SuppressedGeneric = 1300;
        PendingSwitch = 1500;
        /// Fallback-config edits queued by the daemon control socket for the main
        /// loop to apply. Leaf — the socket pushes standalone; the main loop
        /// drains into a `Vec` and releases before taking `config`.
        PendingConfigOps = 1600;
        PendingSwitchOff = 1700;
    }
}

#[cfg(debug_assertions)]
thread_local! {
    /// Ranks currently held by this thread, in acquisition order.
    static HELD: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tracks one held rank on the current thread; pops it on drop. Used directly by
/// the two file-lock guards ([`crate::lock`]'s state flock and
/// [`crate::runtime::RotationGuard`]) — not [`Mutex`]es but still in the order.
#[must_use]
pub(crate) struct RankGuard {
    #[cfg(debug_assertions)]
    rank: u16,
}

impl RankGuard {
    /// Enter rank `R`, asserting it is strictly greater than the highest rank the
    /// current thread already holds. No-op in release builds. `R` can only name a
    /// marker from [`rank`], so the rank entered is always a real position in the
    /// global order.
    #[inline]
    pub(crate) fn enter<R: Rank>() -> Self {
        #[cfg(debug_assertions)]
        {
            let rank = R::VALUE;
            HELD.with(|h| {
                let mut h = h.borrow_mut();
                debug_assert!(
                    h.last().is_none_or(|&top| rank > top),
                    "lock-order violation: acquiring rank {rank} while holding {:?} \
                     (would invert the global lock order and risk deadlock)",
                    h.as_slice(),
                );
                h.push(rank);
            });
            Self { rank }
        }
        #[cfg(not(debug_assertions))]
        {
            Self {}
        }
    }
}

impl Drop for RankGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        HELD.with(|h| {
            let mut h = h.borrow_mut();
            // Strict RAII makes this the stack top, but pop the last matching
            // entry defensively so a stray drop can't corrupt the stack.
            if let Some(pos) = h.iter().rposition(|&r| r == self.rank) {
                h.remove(pos);
            }
        });
    }
}

/// A [`Mutex`] carrying a compile-time rank in the global lock order. `lock()`
/// enters the rank (asserting order) before acquiring the inner mutex and holds
/// it for the guard's lifetime. Drop-in for [`std::sync::Mutex`]: `lock()`
/// returns a [`LockResult`] and the guard derefs to `T`.
pub(crate) struct RankedMutex<T, R: Rank> {
    inner: Mutex<T>,
    _rank: std::marker::PhantomData<R>,
}

impl<T, R: Rank> RankedMutex<T, R> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
            _rank: std::marker::PhantomData,
        }
    }

    /// Acquire the lock. Enters the rank first, so a misordered acquisition
    /// trips the debug assertion before it can block on the inner mutex.
    pub(crate) fn lock(&self) -> LockResult<RankedGuard<'_, T>> {
        let rank = RankGuard::enter::<R>();
        match self.inner.lock() {
            Ok(guard) => Ok(RankedGuard { guard, _rank: rank }),
            Err(poison) => Err(PoisonError::new(RankedGuard {
                guard: poison.into_inner(),
                _rank: rank,
            })),
        }
    }
}

/// Guard for a [`RankedMutex`]. Derefs to `T`. Releases the inner mutex first,
/// then the held rank (field declaration order), so the rank outlives the lock
/// it represents by an instant — never the reverse.
pub(crate) struct RankedGuard<'a, T> {
    guard: std::sync::MutexGuard<'a, T>,
    _rank: RankGuard,
}

impl<T> Deref for RankedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for RankedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(test)]
#[path = "../tests/inline/lockorder.rs"]
mod tests;
