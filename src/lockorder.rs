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
//! - `ApiSwitch` wraps a whole REST switch — feed, refresh, state flock — so it
//!   enters `ensure_installable` and everything below it: outermost.
//! - `RotationGuard` is held across the OAuth HTTP round trip.
//! - `partition_due`: `last_fetched` → `activity`.
//! - `apply_usage`: `usage_store` → `usage_status` → `config`.
//! - rotation/save sites: `config` → state flock → `activity`.
//! - `/profile` TTL clock: `rotation` → `profile_ttl` (post-401 retry) and
//!   `config` → `profile_ttl` (the account-swap actions that expire it).
//! - per-session swap: `rotation` → state flock → `swap_cell`.
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
        // Test-only scaffolding lock, ranked OUTERMOST (below every production
        // rank): the RAII sandboxes that hold it (`testutil::HomeSandbox`,
        // runtime's `with_fake_home`) wrap the whole test, so the code under
        // test still legally acquires any production lock inside it.
        /// `profile::HOME_TEST_LOCK` — serializes `home_dir()` redirects across
        /// the threads a `cargo test` fallback shares.
        #[cfg(test)]
        HomeTest = 20;
        /// The REST API's in-process switch gate (`daemon::api`). One
        /// `POST /api/v1/switch` at a time: a second concurrent request gets an
        /// immediate 409 instead of parking 25s on the cross-process state flock
        /// and then timing out.
        ///
        /// Outermost real rank, because it is held across the WHOLE of
        /// `switch_profile_noninteractive` — and that reaches further down than
        /// it looks. Besides `Config` (400) and `State` (500), it enters
        /// `ensure_installable` (`actions.rs`), which acquires a
        /// `RotationGuard` and so takes `Rotation` (100). At its old 380 that
        /// made every switch of a target holding a clock-expired access token a
        /// lock-order violation: rank 100 entered while holding 380. No cycle
        /// existed — nothing takes `ApiSwitch` while holding `Rotation` — but
        /// the assert is the enforcement, and it was firing on a false premise.
        ApiSwitch = 60;
        /// `RotationGuard` (per-profile rotation flock). Held across HTTP, and
        /// outermost of everything except [`ApiSwitch`], which wraps a whole
        /// REST switch and therefore wraps this too.
        Rotation = 100;
        /// Process-wide per-host request-spacing clock in `usage::fetch` (keyed by
        /// endpoint origin: the Anthropic OAuth host and each api-key provider host).
        /// Held only to reserve the next request slot, never across the sleep or
        /// the HTTP round trip; ranked just inside `Rotation` because the
        /// post-rotation retry reserves a slot while the rotation flock is held.
        UsageThrottle = 150;
        /// Per-process forget epoch (`(name, half)`, monotonic, never removed):
        /// the seeds hold it from compare through their store writes (ranks
        /// 200-350), so it must sit OUTERMOST of every reading lock it gates.
        Epoch = 160;
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
        /// Pending weekly-reset re-test marks
        /// (`usage::scheduler::WeeklyResetKicks`): the set of profiles owed one
        /// kick because their 7d window just rolled over from the hard cap.
        /// Leaf like `KickBlockState` — inserted/removed alone, released
        /// before any other lock.
        WeeklyResetKicks = 235;
        /// Interleaved auto-start queue state (`usage::auto_start_queue::AutoStartQueueState`): the
        /// anchor the 5h-window queue spaces against, plus per-profile
        /// election health. Leaf like `KickBlockState` — read/updated alone,
        /// released before any other lock, and the anchor's disk IO stays
        /// outside the guard.
        AutoStartQueue = 240;
        Tokens = 250;
        ThirdParty = 260;
        ThirdPartyUsageStore = 270;
        ThirdPartyStatus = 280;
        /// Per-profile consecutive provider-429 streak
        /// (`usage::scheduler::ThirdPartyStreaks`): the third-party analogue of
        /// [`PollStreak`]'s `rate_limit` axis, driving the same stuck judgment.
        /// Leaf like `ThirdPartyStatus` — bumped/reset in the third-party leg
        /// and read alone by the two auto-switch predicates, the status feed,
        /// and the TUI Usage row.
        ThirdPartyStreak = 290;
        /// Per-profile third-party key-rejection set
        /// (`usage::scheduler::ThirdPartyBroken`): members whose api KEY the
        /// provider rejected or whose setup token the usage probe found
        /// refused, treated as auth-broken by the chain walks. Live, with a
        /// durable twin (`profile_cache::DEAD_KEY_FILE` for a key,
        /// `profile_cache::THIRD_PARTY_AUTH_FILE` for a setup token); leaf —
        /// written/cleared in the third-party and probe legs, plus the durable
        /// twin's seed and re-sync (add-only at construction and reload,
        /// replacing on the first tick under a lease taken, and stat-gated on
        /// every later tick — the TUI once a second, the daemon every tick —
        /// fingerprint-intersected at every read), and read by the scans, the
        /// TUI/daemon key-rejected helpers (chain walks and day notices), and
        /// both switch drains, each comparing the recorded fingerprint with
        /// the current profile under one config snapshot.
        ThirdPartyBroken = 295;
        /// Members the auto-switch walk passes over because their stored login
        /// read [`crate::profile::StoredLogin::Unknown`] at the last drain
        /// (daemon: after give-up; TUI: at first skip). In-memory only, never
        /// persisted — written alone by the two drains, read alone by the scans
        /// (which re-read each name's `stored_login` lock-free and drop the
        /// healed ones) before `Config`. Leaf like [`ThirdPartyBroken`].
        SkippedUnreadable = 296;
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
        /// `claude_json::JsonSyncLock` (the `.claude.json` merge flock, at
        /// `~/.clauth/.claude-json-sync.lock`). Serializes the carried-rank
        /// merge across every wrapper process so N live `clauth start`
        /// sessions reconciling one `$HOME/.claude.json` event do not all read
        /// the stale members and each rewrite every copy. A true leaf — nothing
        /// is acquired while it is held — ranked OUTSIDE `State` (500) for the
        /// same reason `ProfileTtl` is: the hold does N reads, parses and
        /// writes of MB-sized files, and a cross-process flock is a
        /// serialization point nothing should hold across that IO. Its takers
        /// are the watchdog's config leg (try-once, no ranked lock held),
        /// `ProfileRuntime::drop` (wait, no ranked lock held), the switch
        /// paths' `strip_home_oauth_account` (try-once, taken AFTER both
        /// `Config` and `State` are released and gated on the switch actually
        /// moving the active profile, so the strip never runs under `State`),
        /// the Services-tab wire (try-once), and the runtime seed (wait, taken
        /// OUTSIDE the state flock: `JsonSync` (480) ranks OUTSIDE `State`
        /// (500), taken with no state flock held, so the seed's record write
        /// cannot run under `with_state_lock`).
        JsonSync = 480;
        /// `sessions::RescueFileLock` (`~/.clauth/rescue.lock`), held across
        /// one global file's rescue: the compare, the copy or append, and the
        /// verify. A true leaf — nothing is acquired while it is held — ranked
        /// OUTSIDE `State` (500) like `JsonSync`: the hold is file-sized IO,
        /// and a rescue never waits on it with the state flock held, which the
        /// rank turns into an assertion. Its takers are every rescue tail (the
        /// GC, `acquire`, `Drop`, a profile's delete, the isolated teardown),
        /// each after its state-flock hold has dropped.
        RescueFile = 490;
        /// `with_state_lock` (cross-process state flock). Inner of `config`.
        State = 500;
        /// `hook_note::ScopeLock` (the per-scope record flock). A true leaf —
        /// nothing is acquired while it is held — ranked INSIDE `State`, which is
        /// outer to it: `note_for` drops it before the exact-owner stamp reaches
        /// for the state flock, and the rank turns a future re-nesting into an
        /// assertion instead of a deadlock.
        Scope = 525;
        /// `runtime::SessionSwap`'s cell: the member a live session's credential
        /// link resolves to, plus the liveness markers of every member it has run
        /// on. A true leaf — take-read/take-publish-release, with the file IO the
        /// swap does outside it — ranked INSIDE `State` because both its writer
        /// (the swap's publish step) and its reader (the watchdog tick's
        /// credential leg) take it under `with_state_lock`, which is what makes
        /// the marker move and the link repoint one atomic step.
        SwapCell = 550;
        Activity = 600;
        // Standalone leaves — never nested with another tracked lock.
        NextRefresh = 1100;
        /// Codex poll-cadence memo (`usage::scheduler::CODEX_POLLED_AT`). Taken
        /// alone by `forget_codex_polled`, and under the epoch guard (160) by the
        /// codex landing's section 2, which also takes the usage stores (300/350)
        /// — so it ranks above them all as a leaf.
        CodexPolled = 1150;
        /// Claude banked-reset side-read memo (`usage::scheduler`'s
        /// `ClaudeResetsMemo`, one per scheduler state). The same leaf shape
        /// as `CodexPolled`: taken alone by the leg (under `Config`, 400), the
        /// forget and rename paths, and the landing's transition detector, so
        /// it ranks above everything any of those nests with.
        ClaudeResets = 1155;
        RefetchQueue = 1200;
        /// Process-lifetime `access token digest → account uuid` memo behind the
        /// adopt's identity gate (`usage::scheduler::memoized_identity`). A true
        /// leaf: every acquisition is get-or-insert and releases before the
        /// `/profile` probe runs, so nothing is acquired while it is held. Its
        /// one holder is `Rotation`, and the probe it guards itself takes
        /// `UsageThrottle` (150) — holding this across that probe inverts the
        /// order and asserts here rather than deadlocking in production.
        IdentityMemo = 1250;
        /// Session-scoped set of auth-expired profiles suppressed from the timer until
        /// a manual refresh (`usage::scheduler`). Leaf — acquired standalone in
        /// `tick`/`fetch_third_party_due`, never under another lock.
        SuppressedAuthExpired = 1300;
        /// CLA-ROLL re-stamp pacing (`usage::scheduler::ClaudeRollingPacing`).
        /// A true leaf: every acquisition — the scan gate up front, the
        /// departed-name retain sweep after candidates, the per-candidate hold
        /// check (and its release-on-changed-credentials remove), and the
        /// per-verdict bookkeeping inside the `match gate` arms — is
        /// take-mutate-release with no other lock and no IO under it (the due
        /// re-read before the Ready arm's insert and the credential
        /// fingerprint reads both run OUTSIDE the lock, for exactly that
        /// reason). A rankless `std::sync::Mutex` could not defend that
        /// shape: the ordering `debug_assert` is blind to it, so a future
        /// edit taking `Config` inside a pacing scope would sail past the one
        /// check built to catch it. Ranked as a standalone leaf like its
        /// neighbors.
        RollingPacing = 1400;
        PendingSwitch = 1500;
        PendingSwitchOff = 1700;
        /// MCP reply-digest snapshot (`mcp::digest::DigestTracker`): the
        /// since-your-last-call baseline every clone of the stdio server shares.
        /// A true leaf — each acquisition wraps one sample-compare-store step
        /// over a handful of small local-disk reads (the active profile's name
        /// and its stored endpoint, plus two stats), and `watch`'s poll loop
        /// drops it before every sleep slice, so no RANKED lock, HTTP,
        /// subprocess, or sleep runs under it. That endpoint read is why the
        /// sample calls `profile::stored_usage_cache_is_third_party` and never
        /// `load_profile`, whose staged-rotation recovery takes `State` (500)
        /// and would invert this order. Under `cfg(test)` the sample does nest one
        /// unranked mutex, `profile::HOME_OVERRIDE`, which every `home_dir()`
        /// takes and releases with nothing under it.
        McpDigest = 1800;
        /// Process-global queue of forgets a change site noted
        /// (`usage::forget::PENDING_FORGETS`): recorded inside the state flock
        /// (rank 500), drained with no ranked lock held.
        ForgetQueue = 1805;
        /// The per-process identity snapshot every reload compare reads
        /// (`usage::forget::MemoryStores::snapshot`). A leaf ABOVE
        /// [`ForgetQueue`]: the compare computes its diff under it, releases it,
        /// then forgets into the store locks (ranks 200-350), which would
        /// otherwise invert the order.
        Snapshot = 1810;
        /// The managed gateway's published slot (`daemon::gateway`): the
        /// supervisor thread replaces it, the status writer and
        /// `GET /api/v1/gateway` clone it. A true leaf: every acquisition is
        /// one clone or one assignment, with no IO, probe or child wait under
        /// it.
        GatewayPublished = 1900;
        /// A managed proxy's published slot (`daemon::proxies`): one shared map
        /// for the kind, same leaf discipline as the gateway's. One rank for
        /// the kind, so the ranks never nest.
        ProxyPublished = 1901;
        /// Who holds control of each pane, and the contention records `/events`
        /// relays (`daemon::api::terminal::TerminalHolders`). A true leaf: every
        /// acquisition is one bookkeeping step, and the warning line it decides
        /// on is written after the guard drops.
        TerminalHolders = 1902;
        /// The sessions whose per-session Keychain item a daemon-side reader
        /// gave up on (`runtime::ITEM_READ_GAVE_UP`), shared by the stale-copy
        /// sweep and the MCP login sync. A true leaf: each reader copies the
        /// set out, releases it, runs its `security` reads with nothing held,
        /// then re-takes it to record what it gave up and pruned.
        #[cfg(target_os = "macos")]
        ItemReadGaveUp = 1903;
        /// The per-account `limit-reset` spend flock
        /// (`usage::limit_reset_pending::LimitResetLock`,
        /// `~/.clauth/limit-reset-locks/<name>.lock`). A true leaf: the spend
        /// holds it across its reads, writes, prompts and HTTP round trip with
        /// no ranked lock acquired underneath, so nothing ever nests inside it.
        LimitResetSpend = 1950;
    }
}

#[cfg(debug_assertions)]
thread_local! {
    /// Ranks currently held by this thread, in acquisition order.
    static HELD: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Tracks one held rank on the current thread; pops it on drop. Used directly by
/// the file-lock guards ([`crate::lock`]'s state flock,
/// [`crate::runtime::RotationGuard`], the limit-reset spend lock and the other
/// flock guards) — not [`Mutex`]es but still in the order.
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

/// Whether the current thread holds rank `R`. Lets a function whose correctness
/// depends on a lock its own signature cannot express assert that rather than say
/// it in a comment — the state flock's holders are the case that needs it.
/// Writers of shared state prefer the [`crate::lock::StateLockHeld`] witness
/// `with_state_lock` hands its closure; this is the fallback for sites that only
/// assert (e.g. a cache-consistency check), which have nothing to hand a witness
/// to.
///
/// Always `true` in release: the rank stack is `cfg(debug_assertions)`-only, so a
/// caller must use this inside a `debug_assert!` and never as real control flow.
#[inline]
pub(crate) fn holds<R: Rank>() -> bool {
    #[cfg(debug_assertions)]
    {
        HELD.with(|h| h.borrow().contains(&R::VALUE))
    }
    #[cfg(not(debug_assertions))]
    {
        true
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

    /// Acquire the lock if it is free, else return `Err` at once.
    ///
    /// Ranks like [`lock`](Self::lock) rather than skipping the check: a
    /// try-lock that SUCCEEDS holds the rank for exactly as long, so an
    /// out-of-order try is the same latent deadlock as an out-of-order lock and
    /// has to assert the same way. The rank is entered before the attempt and
    /// dropped again when the attempt fails, so a contended try leaves nothing
    /// behind.
    ///
    /// `daemon::api`'s switch gate is still the only caller, on every platform
    /// clauth targets.
    pub(crate) fn try_lock(&self) -> Result<RankedGuard<'_, T>, TryLockError> {
        let rank = RankGuard::enter::<R>();
        match self.inner.try_lock() {
            Ok(guard) => Ok(RankedGuard { guard, _rank: rank }),
            // Poisoned is NOT busy. `lock` already recovers through
            // `into_inner`, and collapsing the two here made a single panic
            // under the gate permanent: every later caller was told "busy, try
            // again" for the rest of the process, with nothing in flight to
            // wait for. Recovered the same way `lock` recovers, and reported
            // separately so a caller can say which it was.
            Err(std::sync::TryLockError::Poisoned(poison)) => Ok(RankedGuard {
                guard: poison.into_inner(),
                _rank: rank,
            }),
            Err(std::sync::TryLockError::WouldBlock) => Err(TryLockError),
        }
    }
}

/// [`RankedMutex::try_lock`] found the lock genuinely held by someone else.
///
/// Held ONLY — a poisoned mutex is recovered rather than reported here. That
/// distinction is the point: "busy" invites a retry, and a poisoned lock never
/// stops being poisoned, so answering it with "busy" is an instruction to retry
/// forever.
#[derive(Debug)]
pub(crate) struct TryLockError;

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
