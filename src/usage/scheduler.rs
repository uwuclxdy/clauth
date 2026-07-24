use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::oauth::RefreshError;
use crate::providers::ThirdPartyStats;

use super::fetch::{
    FetchError, PlanInfo, UsageInfo, UsageWindow, await_request_slot, epoch_secs_to_iso, fetch_raw,
    five_hour_live, humanize_duration, iso_to_epoch_secs, now_epoch_secs, now_ms, windows_maxed,
};
use crate::oauth::KickRateLimit;
use crate::profile_cache::{
    KICK_BLOCK_CACHE_FILE, THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache,
    profile_cache_mtime_ms, remove_profile_cache, write_profile_cache,
};
use serde::{Deserialize, Serialize};

/// Scheduler wake interval. Network work only fires for profiles whose cadence has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Hard ceiling on a server-provided `retry-after` so a bogus huge value
/// can't starve a profile's refresh slot.
const MAX_RETRY_AFTER_MS: u64 = 15 * 60 * 1000;

/// Widen-only poll deferral for an `auth_broken` profile. Each quarantined
/// poll spends a guaranteed-dead 401 → refresh → 400 pair against the token
/// endpoint, so the cadence stretches to the same ceiling the 429 ladder
/// converges to; the poll stays a (slow) recovery path rather than being
/// excluded outright. Applied at partition time from the live flag — never
/// baked into the `last_fetched` stamp — so a login/adopt/carry lifting the
/// flag snaps the cadence back on the very next tick.
const AUTH_BROKEN_BACKOFF_MS: u64 = MAX_RETRY_AFTER_MS;

/// Base extra backoff applied after a 429 that carries no usable `retry-after`:
/// the first such 429 lands the next slot one interval + this far out. Successive
/// 429s multiply it by [`RATE_LIMIT_BACKOFF_FACTOR`]; a server-provided
/// `retry-after` overrides the whole ladder.
const RATE_LIMIT_MIN_BACKOFF_MS: u64 = 10_000;

/// Per-consecutive-429 multiplier on [`RATE_LIMIT_MIN_BACKOFF_MS`] when the
/// server gives no usable `retry-after`: streak 1 → 10s, 2 → 30s, 3 → 90s,
/// each capped by [`MAX_RETRY_AFTER_MS`]. Stops a sustained rate limit from being
/// re-hit every cadence; the streak resets on the next live fetch.
const RATE_LIMIT_BACKOFF_FACTOR: u64 = 3;

/// Last streak level at which the ACTIVE profile's 429 ladder stays capped at
/// 2× cadence ([`next_slot_deferral`]); deeper streaks release to the full
/// drain ladder. The bound exists because the `/usage` throttle is per-account
/// on requests to `/usage` itself and counts REJECTED polls (the #30
/// learning) — a cap with no release would keep re-filling that window for as
/// long as a genuine storm lasts. At the default 90s cadence this bound buys
/// ~6 dense probes (≈3 min apart) over the storm's first quarter hour — enough
/// to re-discover a recovered endpoint fast — before conceding to the ladder.
pub(crate) const ACTIVE_CAP_MAX_STREAK: u32 = 6;

/// Grace added past a 5h window's `resets_at` before the anchored post-reset
/// poll fires. Anthropic's `/usage` can briefly lag the reset instant, so polling
/// exactly at the boundary risks reading the pre-reset (still-100%) body. A single
/// anchor, no retry loop — residual staleness beyond this grace is covered at
/// render time (the `(now)` reset chip), not here.
const RESET_ANCHOR_GRACE_MS: u64 = 15_000;

/// Wall-clock instant in epoch-milliseconds. Distinct from [`IntervalMs`] so
/// instants and spans can't be confused. `#[repr(transparent)]` keeps layout
/// identical to the persisted `u64` in any `HashMap<String, u64>`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct EpochMs(u64);

/// Span of time in milliseconds. Distinct from [`EpochMs`] so "instant" and
/// "span" can't be mixed up. `#[repr(transparent)]` for `u64` layout identity.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct IntervalMs(u64);

impl EpochMs {
    pub(crate) const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    pub(crate) const fn as_millis(self) -> u64 {
        self.0
    }

    /// Instant `interval` after this one, saturating.
    pub(crate) const fn saturating_add(self, interval: IntervalMs) -> EpochMs {
        EpochMs(self.0.saturating_add(interval.0))
    }
}

impl IntervalMs {
    pub(crate) const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }
}

pub(crate) type UsageStore = Arc<RankedMutex<HashMap<String, UsageInfo>, rank::UsageStore>>;
pub(crate) type StatusStore = Arc<RankedMutex<HashMap<String, FetchStatus>, rank::UsageStatus>>;
pub(crate) type TokenList = Arc<RankedMutex<Vec<TokenEntry>, rank::Tokens>>;

/// Per-profile epoch-ms of the last fetch attempt (cadence gating).
pub(crate) type LastFetchedAt = Arc<RankedMutex<HashMap<String, EpochMs>, rank::LastFetched>>;

/// One profile's consecutive-failure counters. Both ladder off
/// [`rate_limit_backoff_ms`] and both clear on the next live fetch, but they stay
/// separate counters because every other reader means only one of them: a 429
/// streak feeds [`is_stuck_rate_limited`], the auto-switch freshness bypass and
/// `status.json`'s `stale`, none of which a refresh failure may claim.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct StreakCounts {
    /// Consecutive 429s from `/usage`.
    pub(crate) rate_limit: u32,
    /// Consecutive token refreshes the endpoint rejected WITHOUT confirming the
    /// token is dead ([`crate::oauth::RefreshError::Transient`]). A confirmed
    /// dead token quarantines instead and carries its own, wider backoff.
    pub(crate) refresh_fail: u32,
}

/// Per-profile poll-health streaks, driving exponential backoff in
/// [`apply_outcome`] and [`partition_due`]. Reset on the next live fetch.
pub(crate) type PollStreaks = Arc<RankedMutex<HashMap<String, StreakCounts>, rank::PollStreak>>;

/// One profile's live kick-429 block: the messages endpoint is rejecting the
/// 5h auto-start kick. Deliberately NOT a [`StreakCounts`] axis — those clear
/// on the next live `/usage` body, but `/usage` stays 200 straight through a
/// messages-limiter outage (observed 2026-07-15), so only a kick outcome may
/// clear this. Persisted per profile ([`KICK_BLOCK_CACHE_FILE`]) so a standdown
/// TUI mirrors the fetching instance and a restart doesn't forget a live block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KickBlock {
    /// Consecutive kick 429s.
    pub(crate) streak: u32,
    /// The limiter said `unified-status: rejected` — the account-level hard
    /// rejection that also gates the fallback auto-switch, not a burst 429.
    pub(crate) rejected: bool,
    /// Advertised retry ceiling (epoch secs). Upper bound only — the limiter
    /// has relented 2.4h before its own reset — so retries decay toward it
    /// instead of sleeping until it.
    pub(crate) until: Option<i64>,
    /// Next allowed kick attempt (epoch secs): `min(now + ladder, until)`.
    pub(crate) next_retry: i64,
}

/// Per-profile kick-429 blocks. Same leaf discipline as [`PollStreaks`]:
/// read/copied alone, released before any other lock, file IO outside the guard.
pub(crate) type KickBlocks = Arc<RankedMutex<HashMap<String, KickBlock>, rank::KickBlockState>>;

/// Names pushed here after a successful token rotation bypass the cadence on the next tick.
pub(crate) type RefetchQueue = Arc<RankedMutex<HashSet<String>, rank::RefetchQueue>>;

/// Who queued a pending switch. Determines drain precedence: an operator's
/// explicit `User` choice outranks the scheduler's `Scheduler` auto-switch and
/// clears any queued auto-target — the user's tap must never be silently
/// overridden by a same-tick threshold crossing (TECH-6, finding #4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    User,
    Scheduler,
}

/// One queued switch request. Ordered (FIFO) in the [`PendingSwitch`] queue with
/// a per-entry retry deadline: a request that can't execute yet (target mid-fetch,
/// active diverged-unsaved, transient refresh failure) is RE-QUEUED until it lands
/// or `retry_until` passes — not dropped after one attempt (TECH-6, finding #4:
/// a user switch during a fetch window used to evaporate after the `{ok:true}` ack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSwitchEntry {
    pub(crate) target: String,
    pub(crate) origin: Origin,
    /// The target's harness at enqueue time (CDX-4 §0.15). The queue's gates
    /// are HARNESS-SCOPED — the two active slots are independent (§0.1), so a
    /// stuck claude switch must never block a codex rotation or vice versa:
    /// enqueue precedence, the scans' skip-while-pending, and the drain's
    /// winner selection each consider only same-harness entries.
    pub(crate) harness: crate::profile::Harness,
    /// Epoch-ms after which a still-unexecutable entry is dropped (with a
    /// `last_error`) instead of retried forever. Set once at first enqueue and
    /// preserved across re-queues, so the retry window is bounded from the ack.
    pub(crate) retry_until: u64,
}

/// Retry window for a queued switch that can't execute yet. Generous enough to
/// outlast a slow usage fetch (the common "target busy" cause), bounded so a
/// permanently-stuck target can't linger as a zombie retry forever.
pub(crate) const PENDING_SWITCH_RETRY_TTL_MS: u64 = 120_000;

/// Auto-switch / user-switch targets, drained by the daemon's `tick` (or the TUI's
/// `on_tick`). Ordered `VecDeque` (not a `Set`) so drain order is deterministic and
/// last-writer-wins is well-defined; [`enqueue_pending_switch`] enforces Origin
/// precedence and dedup on the way in.
pub(crate) type PendingSwitch = Arc<RankedMutex<VecDeque<PendingSwitchEntry>, rank::PendingSwitch>>;

/// Enqueue `target` with `origin`, honoring Origin precedence and deduping —
/// all HARNESS-SCOPED (CDX-4 §0.15: the two slots are independent):
/// - A `User` request supersedes the operator's prior intent AND any queued
///   `Scheduler` target OF THE SAME HARNESS — those entries are cleared first
///   (last-writer-wins), so a tap always wins a race with a same-tick
///   auto-switch, while the other harness's queued intent survives.
/// - A `Scheduler` request is a no-op when a same-harness entry is queued
///   (the level-triggered scan re-fires; it never stacks behind a choice).
/// - Either way, a target already queued is not doubled.
pub(crate) fn enqueue_pending_switch(
    queue: &mut VecDeque<PendingSwitchEntry>,
    target: String,
    harness: crate::profile::Harness,
    origin: Origin,
    now_ms: u64,
) {
    match origin {
        Origin::User => queue.retain(|e| e.harness != harness),
        Origin::Scheduler => {
            if queue.iter().any(|e| e.harness == harness) {
                return;
            }
        }
    }
    if queue.iter().any(|e| e.target == target) {
        return;
    }
    queue.push_back(PendingSwitchEntry {
        target,
        origin,
        harness,
        retry_until: now_ms.saturating_add(PENDING_SWITCH_RETRY_TTL_MS),
    });
}

/// Pick the single target to attempt from a drained queue: `User` outranks
/// `Scheduler`, and within an origin class the LAST enqueued wins (last-writer-
/// wins). Every other entry is superseded. `None` when the queue was empty. Pure
/// so the precedence is unit-testable without a live daemon. (Queue-wide — the
/// status.json `pending_switch` peek; the drain uses the per-harness form.)
pub(crate) fn select_switch_winner(
    entries: &VecDeque<PendingSwitchEntry>,
) -> Option<PendingSwitchEntry> {
    entries
        .iter()
        .rfind(|e| e.origin == Origin::User)
        .or_else(|| entries.iter().rfind(|e| e.origin == Origin::Scheduler))
        .cloned()
}

/// [`select_switch_winner`] restricted to one harness's entries — the drain
/// services (at most) one winner PER HARNESS per round, so a deferred claude
/// switch and a codex rotation proceed independently (CDX-4 §0.15).
pub(crate) fn select_switch_winner_for(
    entries: &VecDeque<PendingSwitchEntry>,
    harness: crate::profile::Harness,
) -> Option<PendingSwitchEntry> {
    let scoped: VecDeque<PendingSwitchEntry> = entries
        .iter()
        .filter(|e| e.harness == harness)
        .cloned()
        .collect();
    select_switch_winner(&scoped)
}

/// Whether any queued entry targets `harness` — the scans' skip-while-pending
/// gate, harness-scoped so one slot's pending decision never wedges the other.
pub(crate) fn pending_for_harness(
    entries: &VecDeque<PendingSwitchEntry>,
    harness: crate::profile::Harness,
) -> bool {
    entries.iter().any(|e| e.harness == harness)
}

/// Set true when wrap-off mode finds the entire chain exhausted (no sink below 100%).
/// Drained by `on_tick` to turn off all accounts. Bool because switch-off is a global act.
pub(crate) type PendingSwitchOff = Arc<RankedMutex<bool, rank::PendingSwitchOff>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    /// Opted into auto-start: the periodic tick opens a 5h window for this
    /// profile (kick) before fetching usage whenever its last-known window lapsed.
    pub(crate) auto_start: bool,
    /// Epoch-ms the access token expires at, when known. Gates the kick's
    /// rotate-on-429 to clock-expired tokens only.
    pub(crate) access_expires_at: Option<i64>,
    /// Persisted `auth_broken` quarantine at snapshot time; widens the poll
    /// cadence by [`AUTH_BROKEN_BACKOFF_MS`] while set.
    pub(crate) auth_broken: bool,
}

/// Snapshot of one third-party profile identity used by the refresher.
#[derive(Clone)]
pub(crate) struct ThirdPartyEntry {
    pub(crate) name: String,
    pub(crate) target: crate::providers::ThirdPartyTarget,
    pub(crate) api_key: String,
}

/// Profile-name accessor shared by the OAuth and third-party entry types so
/// `partition_due` / `merge_forced` run identically over both.
trait NamedEntry {
    fn name(&self) -> &str;
    /// Widen-only extra deferral added to the fixed cadence at partition time.
    /// Zero for everything but a quarantined or refresh-failing OAuth profile.
    fn poll_backoff_ms(&self, streaks: StreakCounts) -> u64 {
        let _ = streaks;
        0
    }
}

impl NamedEntry for TokenEntry {
    fn name(&self) -> &str {
        &self.name
    }

    fn poll_backoff_ms(&self, streaks: StreakCounts) -> u64 {
        if self.auth_broken {
            return AUTH_BROKEN_BACKOFF_MS;
        }
        // A run of transient refresh failures climbs the same curve a 429 run
        // does, capped at the same ceiling. Without it the one failure mode that
        // can hit EVERY profile at once — clauth's own request shape drifting,
        // which never quarantines because the endpoint never confirmed a dead
        // token — re-hits the token endpoint at full cadence indefinitely.
        if streaks.refresh_fail == 0 {
            return 0;
        }
        rate_limit_backoff_ms(streaks.refresh_fail).min(MAX_RETRY_AFTER_MS)
    }
}

impl NamedEntry for ThirdPartyEntry {
    fn name(&self) -> &str {
        &self.name
    }
}

pub(crate) type ThirdPartyList = Arc<RankedMutex<Vec<ThirdPartyEntry>, rank::ThirdParty>>;
pub(crate) type ThirdPartyUsageStore =
    Arc<RankedMutex<HashMap<String, ThirdPartyStats>, rank::ThirdPartyUsageStore>>;
pub(crate) type ThirdPartyStatusStore =
    Arc<RankedMutex<HashMap<String, FetchStatus>, rank::ThirdPartyStatus>>;
/// Session-scoped (in-memory) set of generic profiles whose last fetch yielded
/// no data, suppressed from the timer until a manual refresh clears them. Never
/// persisted — clears when the TUI process exits. Known providers and 429s are
/// never added (429 keeps the server-directed deferral).
pub(crate) type SuppressedGenericStore = Arc<RankedMutex<HashSet<String>, rank::SuppressedGeneric>>;

/// Per-profile next-fetch epoch-ms. Written after each `partition_due` run for
/// overview countdown display without re-running the partition math on the render thread.
pub(crate) type NextRefreshPerProfile = Arc<RankedMutex<HashMap<String, u64>, rank::NextRefresh>>;

/// In-flight op per profile. Overview shows a spinner instead of a countdown when non-`Idle`.
/// Map omits `Idle` entries — absent == `Idle`. Leaf-level: never held across HTTP.
pub(crate) type ActivityStore = Arc<RankedMutex<HashMap<String, ProfileActivity>, rank::Activity>>;

/// In-flight op for one profile. Non-`Idle` shows a spinner in the overview timer slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileActivity {
    Idle,
    /// Marked due this tick but still waiting behind the per-host request throttle
    /// (`REQUEST_SPACING_MS`) — not yet firing HTTP. Flips to `Fetching` the
    /// instant its request clears the gate. Distinguishing this from `Fetching`
    /// keeps a batch of due profiles from all reading as "fetching" while only one
    /// is actually in flight (the rest are queued behind the 5s spacing).
    Queued,
    /// `/usage` HTTP fetch in flight.
    Fetching,
    /// OAuth token rotation in flight.
    Refreshing,
    /// Off-thread AUTH-1 switch gate in flight for this profile (the switch
    /// target). Doubles as the pending-switch state: cleared when the gate's
    /// answer drains on the UI thread.
    Switching,
}

/// Result of one tracked operation. Drained by `on_tick`, which clears the `ActivityStore`
/// slot and surfaces any error as a toast.
#[derive(Debug)]
pub(crate) struct OpResult {
    pub(crate) name: String,
    pub(crate) outcome: anyhow::Result<()>,
}

pub(crate) type OpResultSender = Sender<OpResult>;
pub(crate) type OpResultReceiver = Receiver<OpResult>;

/// Startup phase transitions from background workers to the UI thread.
/// Drained in `on_tick` so the first paint never waits on network or FS.
#[derive(Debug)]
pub(crate) enum StartupSignal {
    /// Reconcile finished cleanly — credentials in sync or silent continuation.
    ReconcileDone,
    /// Reconcile found credentials diverged from the active profile's stored creds.
    /// UI pushes the Divergence prompt; bootstrap waits for user action.
    /// (No OAuth probe — would spend the single-use refresh token.)
    ReconcileNeedsPrompt { active: String },
    /// Bootstrap finished (refresh + initial fetch + auto-start kicks).
    /// UI rebuilds token snapshot, spawns scheduler, applies usage, runs startup auto-switch.
    BootstrapDone,
}

pub(crate) type StartupSender = Sender<StartupSignal>;
pub(crate) type StartupReceiver = Receiver<StartupSignal>;

/// Mark a profile's activity. Idempotent; pair with [`clear_activity`] on every exit path.
pub(crate) fn mark_activity(store: &ActivityStore, name: &str, activity: ProfileActivity) {
    if let Ok(mut g) = store.lock() {
        if matches!(activity, ProfileActivity::Idle) {
            g.remove(name);
        } else {
            g.insert(name.to_string(), activity);
        }
    }
}

/// Drop a profile to `Idle` (removes the entry; absent == `Idle`).
pub(crate) fn clear_activity(store: &ActivityStore, name: &str) {
    if let Ok(mut g) = store.lock() {
        g.remove(name);
    }
}

/// True iff the profile has no in-flight op. Poisoned mutex fails safe to "busy".
pub(crate) fn is_idle(store: &ActivityStore, name: &str) -> bool {
    match store.lock() {
        Ok(g) => !g.contains_key(name),
        Err(_) => false,
    }
}

/// True iff any profile has an in-flight op. Gates global actions like "rotate all".
pub(crate) fn any_busy(store: &ActivityStore) -> bool {
    match store.lock() {
        Ok(g) => !g.is_empty(),
        Err(_) => true,
    }
}

/// True iff any profile's switch gate is in flight. Poisoned mutex fails safe
/// to "busy". Switch entry points refuse while one is pending: a second switch
/// spawned mid-gate could land first and be overturned by the older gate's
/// completion.
pub(crate) fn switch_gate_in_flight(store: &ActivityStore) -> bool {
    match store.lock() {
        Ok(g) => g.values().any(|a| matches!(a, ProfileActivity::Switching)),
        Err(_) => true,
    }
}

/// Outcome of the most recent fetch attempt for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// Live API response.
    Fresh,
    /// API failed; numbers come from on-disk cache.
    Cached,
    /// API failed and no cache available.
    Failed,
    /// API returned 429 (endpoint-level rate limit); numbers come from on-disk cache.
    RateLimited,
}

/// Rotated (access, refresh) pair from an in-fetch rotation. Propagated back into
/// `TokenList` so the next tick doesn't re-401 with the stale token and double-burn the chain.
pub(crate) type RotatedTokens = (String, Option<String>);

/// Load disk cache as `(Some, status)` or `(None, Failed)` for the rotation bail-out path.
fn load_cached_with_status(name: &str, status: FetchStatus) -> (Option<UsageInfo>, FetchStatus) {
    match load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE) {
        Some(info) => (Some(info), status),
        None => (None, FetchStatus::Failed),
    }
}

/// A poll-time refresh failure is terminal (the OAuth login dropped for good)
/// only for a revoked/invalid refresh token, not a transient network/5xx/parse
/// blip. Quarantining on a terminal failure surfaces "needs reauth" on this tick
/// instead of serving stale cached usage until the next switch trips
/// `ensure_installable`. Truth table pinned by the scheduler `*_terminal` tests.
fn refresh_failure_is_terminal(err: &RefreshError) -> bool {
    matches!(err, RefreshError::Invalid(_))
}

/// The benign face of a terminal 400: "refresh token not found or invalid" is
/// also the exact response after a single-use double-spend — Claude Code
/// refreshing the active profile's symlinked credentials mid-poll, or another
/// refresher that completed before this tick's guard was acquired (the
/// in-memory `TokenEntry` snapshot predates the guard). Re-read the profile's
/// on-disk credentials (call while STILL holding the rotation guard, so the
/// read is stable): a stored refresh token that DIFFERS from the one we just
/// spent means someone else advanced the chain (tokens are opaque, so this is
/// an inequality check, not an ordering — no writer rewinds the store, and a
/// wrong carry self-corrects next tick) — return that fresh pair for the
/// caller's `TokenList` sync instead of quarantining a healthy account.
/// `None` (unchanged, unreadable, or tokenless) means the 400 was a real
/// revocation.
fn fresher_disk_pair(name: &str, spent_refresh: &str) -> Option<RotatedTokens> {
    let profile = crate::profile::load_profile(name).ok()?;
    let access = profile.access_token()?.to_string();
    let refresh = profile.refresh_token()?.to_string();
    (refresh != spent_refresh).then_some((access, Some(refresh)))
}

/// The carry half of the double-spend guard: when [`fresher_disk_pair`] proves
/// someone else advanced the chain, clear any pre-existing quarantine (the
/// chain is alive, so a standing `auth_broken` is stale — without this, an
/// account recovered by an external re-login would stay excluded from the
/// fallback walk and refused by every switch gate forever), queue a refetch so
/// the next tick polls with the carried pair, and hand back the cached outcome
/// whose `rotated` syncs the caller's `TokenList`. A wrong clear self-corrects:
/// if the carried pair is itself dead, its refresh 400s next tick with the
/// store unchanged and the account re-quarantines.
fn carry_external_rotation(
    config: &crate::profile::ConfigHandle,
    name: &str,
    spent_refresh: &str,
    refetch: &RefetchQueue,
) -> Option<FetchOutcome> {
    let fresh = fresher_disk_pair(name, spent_refresh)?;
    crate::oauth::mark_auth_broken(config, name, false);
    if let Ok(mut q) = refetch.lock() {
        q.insert(name.to_string());
    }
    Some(FetchOutcome::cached(
        name,
        FetchStatus::Cached,
        Some(fresh),
        None,
    ))
}

/// Whether a 429 on the usage fetch is worth rotating for. Mirrors
/// `auth::auto_start_kick`'s 429 gate: a 429 on a still-valid token is a pure
/// endpoint rate limit a refresh can't fix, but a clock-expired token would 401
/// the moment the limit clears — so its 429 masks a token that MUST be refreshed,
/// and that refresh is exactly what surfaces `auth_broken` (AUTH-1) instead of the
/// account hiding behind `RateLimited` forever. Unknown expiry stays conservative
/// (never rotate). Truth table pinned by the scheduler `rate_limited_*` tests.
fn token_clock_expired(access_expires_at: Option<i64>, now_ms: i64) -> bool {
    access_expires_at.is_some_and(|exp| now_ms >= exp)
}

/// Status + server hint for a rotation-leg bail that couldn't complete a
/// refresh (busy guard, live session, missing refresh token, failed refresh).
/// A bail that entered the rotation leg through the clock-expired-429 unmask
/// (`unmask_429` = the 429's `retry-after`) keeps that endpoint-level context
/// — `RateLimited` plus the hint — so `apply_outcome`'s deferral and streak
/// accounting survive the failed attempt; dropping them re-polled a
/// rate-limited endpoint on the plain cadence. A 401-entered bail stays
/// `Cached`.
fn rotation_bail_context(unmask_429: Option<Option<Duration>>) -> (FetchStatus, Option<Duration>) {
    match unmask_429 {
        Some(retry_after) => (FetchStatus::RateLimited, retry_after),
        None => (FetchStatus::Cached, None),
    }
}

/// Floor (ms) for [`active_rotate_lead_ms`]: 15 minutes — deliberately larger
/// than upstream's 3 (fork divergence). Claude Code refreshes with its OWN
/// ahead-of-expiry margin (~5 min observed), so a lead inside that margin
/// still loses the race: on 2026-07-11T17:07Z the 4.5-min lead (interval*3 at
/// 90 s) fired inside CC's window, spent an already-superseded refresh token,
/// 400d with the store unchanged, and correctly-but-fatally quarantined the
/// active account — whose fresh chain then lived only in the unreadable
/// Keychain and was orphaned by the walk-away switch (= the "re-login every
/// other day" complaint). At 15 min clauth rotates decisively first and stays
/// the chain's last writer; adoption still backstops a CC that someday
/// refreshes even earlier.
const ACTIVE_ROTATE_LEAD_FLOOR_MS: i64 = 900_000;

/// How early the poller rotates the ACTIVE, Keychain-installed profile ahead
/// of its clock expiry — only with the opt-in `preemptive_rotation` toggle
/// (rotation coherence, #1).
///
/// The invariant this maintains is NOT "beat Claude Code's refresh schedule"
/// (any fixed margin would silently lose to a future CC that refreshes
/// earlier): it is **"the Keychain never holds an expired token while the
/// poller is alive."** CC re-reads the Keychain per request and refreshes
/// only when the token it just read looks spent; keep the item fresh and CC
/// has no reason to refresh at all. Three poll intervals (with a floor)
/// guarantees multiple rotation opportunities before expiry, whatever the
/// configured cadence. And correctness never depends on winning: if CC does
/// refresh first — schedule change, clauth downtime, lost race — the poller
/// ADOPTS CC's fresher pair from the live file mirror instead of fighting
/// for the chain (`oauth::try_adopt_live_rotation`).
fn active_rotate_lead_ms(interval_ms: u64) -> i64 {
    ((interval_ms as i64).saturating_mul(3)).max(ACTIVE_ROTATE_LEAD_FLOOR_MS)
}

/// Whether this poll should rotate ahead of expiry instead of waiting for a
/// 401: only with the opt-in `preemptive_rotation` toggle (`enabled`, off by
/// default — stock behavior stays strictly lazy; adoption + mirror-on-rotate
/// carry the correctness, this is an optimization), only for the ACTIVE
/// profile, only while its live login has NOT diverged (RESCUE-2c — the same
/// skip `oauth::rotation_candidates` applies: a preemptive spend is never
/// forced by a dead token, and rotating under divergence just leaks a refresh
/// chain nobody installs), only while the Keychain mirror is live (elsewhere
/// the live credential IS the profile file via the symlink, so there is no
/// second chain to race), and only inside the lead window. An unknown expiry
/// never rotates proactively (never spend a single-use refresh on a token
/// whose expiry we can't prove).
#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "a truth-table predicate; every input is a named row in its pinned tests"
)]
fn proactive_rotation_due(
    enabled: bool,
    feed: bool,
    active: bool,
    active_diverged: bool,
    keychain_live: bool,
    access_expires_at: Option<i64>,
    now_ms: i64,
    interval_ms: u64,
) -> bool {
    // CLA-FEED (`feed`): a feed-enabled profile forces the preemptive leg
    // regardless of the global toggle — its rotation re-stamps the fed
    // session token, and a stale one has a live claude behind it. The
    // Keychain-mirror condition doesn't apply either: off macOS the fed
    // sidecar IS the live credential via the symlink, so the re-stamp reaches
    // sessions on every platform.
    (enabled && keychain_live || feed)
        && active
        && !active_diverged
        && access_expires_at.is_some_and(|exp| now_ms + active_rotate_lead_ms(interval_ms) >= exp)
}

/// Whether the macOS Keychain mirror is live — `false` under `cfg(test)` and
/// on every other OS, where the symlinked profile file is the live credential.
#[cfg(target_os = "macos")]
fn keychain_live() -> bool {
    crate::keychain::enabled()
}

#[cfg(not(target_os = "macos"))]
fn keychain_live() -> bool {
    false
}

/// Wrap an identity probe so each access token is resolved at most once per
/// caller. An access token's account uuid is immutable, so a memo hit is exact
/// rather than merely fresh — which is what makes caching safe for a check whose
/// whole job is proving two tokens belong to the same account.
///
/// ONLY a `Some` is cached. A `None` means the probe failed (network, 401, shape
/// drift), and the adopt retry after a failed refresh exists precisely because the
/// live mirror may have surfaced a fresh pair since the first attempt — memoizing
/// the failure would quietly turn that second adopt into a no-op.
fn memoized_identity<'a>(
    probe: &'a dyn Fn(&str) -> Option<String>,
) -> impl Fn(&str) -> Option<String> + 'a {
    let seen: std::cell::RefCell<HashMap<String, String>> = std::cell::RefCell::new(HashMap::new());
    move |tok: &str| {
        if let Some(hit) = seen.borrow().get(tok).cloned() {
            return Some(hit);
        }
        let uuid = probe(tok)?;
        seen.borrow_mut().insert(tok.to_string(), uuid.clone());
        Some(uuid)
    }
}

/// What the plain (pre-rotation) fetch result means for `fetch_with_rotation`.
/// Produced by [`classify_pre_rotation`] — see its tests (`pre_rotation_*`) for
/// the truth table live HTTP can't drive.
#[derive(Debug)]
enum PreRotationDecision {
    /// Live API body — return straight from `fetch_with_rotation`. Boxed:
    /// `UsageInfo` dwarfs every other variant here (clippy::large_enum_variant).
    Serve(Box<UsageInfo>),
    /// 429 on a still-valid token: a pure endpoint rate limit, not a token
    /// problem — bail to cache. `plan` rides along: a canceled account's tier
    /// flip is only ever observed through the `/profile` reading the fetch
    /// took despite the 429.
    BailRateLimited {
        retry_after: Option<Duration>,
        plan: Option<PlanInfo>,
    },
    /// 401, or a 429 on a clock-expired token (AUTH-1 unmask): fall through to
    /// the rotation leg so a dead refresh token surfaces as `auth_broken`
    /// rather than staying masked as `RateLimited`. `unmask_429` is `None` for
    /// the 401 case; `Some` (the 429's retry-after) for the unmask case, so a
    /// failed unmask keeps the deferral + streak (see [`rotation_bail_context`]).
    /// The plan on the unmask arm rode a dead token and has no field here —
    /// deliberately discarded, unlike `BailRateLimited`'s.
    Rotate {
        unmask_429: Option<Option<Duration>>,
    },
    /// Any other error: bail straight to disk cache.
    BailCached,
}

/// Pure classification of the plain fetch's [`FetchError`] — no I/O, no clock
/// read of its own. `token_clock_expired` is passed in ALREADY COMPUTED by the
/// caller (see [`token_clock_expired`]) so pulling this branch logic out of
/// `fetch_with_rotation` can't introduce a second, differently-timed
/// `now_ms()` read.
fn classify_pre_rotation(
    result: Result<UsageInfo, FetchError>,
    token_clock_expired: bool,
) -> PreRotationDecision {
    match result {
        Ok(info) => PreRotationDecision::Serve(Box::new(info)),
        Err(FetchError::RateLimited { retry_after, plan }) if !token_clock_expired => {
            PreRotationDecision::BailRateLimited { retry_after, plan }
        }
        Err(FetchError::Status(401)) => PreRotationDecision::Rotate { unmask_429: None },
        Err(FetchError::RateLimited { retry_after, .. }) => PreRotationDecision::Rotate {
            unmask_429: Some(retry_after),
        },
        Err(_) => PreRotationDecision::BailCached,
    }
}

/// Fetch + rotate + retry for one profile. On 401 — or a 429 on a clock-expired
/// token (the AUTH-1 dead-login unmasking, see [`token_clock_expired`]) — refresh
/// the OAuth pair, persist, retry once. A 429 on a still-valid token bails to disk
/// cache as `RateLimited`; other errors bail as `Cached`. An unmask entry whose
/// refresh can't complete keeps the 429's status + `retry-after`
/// ([`rotation_bail_context`]). Pushes `name` onto
/// `refetch` when rotation succeeded but the follow-up fetch failed. Returns a
/// [`FetchOutcome`]: the rotated pair for the caller's `TokenList` sync, the
/// `from_fetch` provenance flag, and the 429 `retry-after` hint that
/// [`apply_outcome`] turns into a deferred next-fetch slot.
///
/// A second exception to "rotate only on a rejected token": with the opt-in
/// `preemptive_rotation` toggle, the ACTIVE, Keychain-installed profile
/// rotates ahead of expiry (see [`active_rotate_lead_ms`]) so the running
/// `claude` never spends the single-use chain.
fn fetch_with_rotation(
    config: &crate::profile::ConfigHandle,
    entry: &TokenEntry,
    prev_plan: Option<PlanInfo>,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) -> FetchOutcome {
    let name = entry.name.as_str();
    let access_token = entry.access_token.as_str();
    let refresh_token = entry.refresh_token.as_deref();
    // Rotation coherence (#1): read the active flag, stored expiry, and the
    // preemptive toggle in one short config-lock window; the poll itself must
    // never hold the lock. Config — not the `TokenEntry` snapshot — is the
    // expiry source: a kick that rotated earlier in this tick has already
    // persisted the fresh expiry there, while the entry still carries the
    // pre-kick one, and a stale past expiry here would read as clock-expired
    // and re-spend the just-minted single-use pair.
    let (is_active, access_expires_at, interval_ms, preemptive, session_feed) = config
        .lock()
        .map(|c| {
            (
                c.is_active(name),
                c.find(name).and_then(|p| p.access_token_expires_at()),
                c.state.refresh_interval_ms,
                c.state.preemptive_rotation,
                c.find(name).is_some_and(|p| p.session_feed),
            )
        })
        .unwrap_or((false, None, 90_000, false, false));
    // The divergence input is resolved outside the config-lock window — it
    // stats the live symlink, and the poll must never hold the lock.
    let active_diverged = is_active
        && matches!(
            crate::claude::classify_credentials_link(name).ok(),
            Some(crate::claude::LinkState::Diverged)
        );
    let proactive = proactive_rotation_due(
        preemptive,
        session_feed,
        is_active,
        active_diverged,
        keychain_live(),
        access_expires_at,
        now_ms() as i64,
        interval_ms,
    );
    let mut unmask_429: Option<Option<Duration>> = None;
    if !proactive {
        let result = fetch_raw(name, access_token, prev_plan.clone(), false, Some(activity));
        // Read the clock AFTER the fetch resolves — `fetch_raw` is a blocking
        // HTTP round-trip, so reading it earlier would classify a token whose
        // expiry falls inside that window against a stale `now`. Read once,
        // right here, and hand the bool to the pure classifier — never let it
        // re-read `now_ms()` on its own timing.
        let expired = token_clock_expired(access_expires_at, now_ms() as i64);
        match classify_pre_rotation(result, expired) {
            PreRotationDecision::Serve(info) => return FetchOutcome::live(name, *info, None),
            PreRotationDecision::BailRateLimited { retry_after, plan } => {
                return FetchOutcome::cached(name, FetchStatus::RateLimited, None, retry_after)
                    .with_plan(plan);
            }
            PreRotationDecision::Rotate { unmask_429: unmask } => unmask_429 = unmask,
            PreRotationDecision::BailCached => {
                return FetchOutcome::cached(name, FetchStatus::Cached, None, None);
            }
        }
    }

    let bail_to_cache = |rotated: Option<RotatedTokens>| {
        FetchOutcome::cached(name, FetchStatus::Cached, rotated, None)
    };
    // A rotation bail BEFORE any refresh was spent: reactively the token is
    // already dead, so disk cache is all there is; proactively the token still
    // has >= the lead window of life, so run the plain fetch instead — winning
    // the refresh race must never cost a live usage poll.
    let bail_unrotated = || {
        if proactive {
            match fetch_raw(name, access_token, prev_plan.clone(), false, Some(activity)) {
                Ok(info) => FetchOutcome::live(name, info, None),
                Err(FetchError::RateLimited { retry_after, plan }) => {
                    FetchOutcome::cached(name, FetchStatus::RateLimited, None, retry_after)
                        .with_plan(plan)
                }
                Err(_) => FetchOutcome::cached(name, FetchStatus::Cached, None, None),
            }
        } else {
            let (status, retry_after) = rotation_bail_context(unmask_429);
            FetchOutcome::cached(name, status, None, retry_after)
        }
    };

    // Per-profile rotation lock across the ENTIRE rotation leg — the adopt
    // below mutates the same stored credential fields as a refresh persist,
    // so both hold the same guard as `rotate_one_inner` (guard OUTERMOST,
    // then config mutex + state flock inside). Blocking acquire is safe: the
    // tick body holds no lock, so no deadlock risk. On acquire failure, fall
    // back rather than touching the credentials unguarded.
    let Ok(rotation_guard) = crate::runtime::RotationGuard::acquire(name) else {
        return bail_unrotated();
    };

    // Both adopts below resolve the same two tokens (ours + the live mirror's), so
    // they share one memo for this call rather than re-spending `/profile` on a
    // uuid already resolved seconds ago.
    let identity = memoized_identity(&|tok| crate::usage::fetch_account_uuid(tok));

    // Adopt before spending: when the ACTIVE Keychain profile's live file
    // mirror already holds a FRESHER same-account pair, the running claude
    // rotated first — adopt its pair (identity-guarded) instead of burning
    // OUR single-use refresh token against a family it just superseded.
    // Queue a refetch so the next tick polls with the adopted token; disk
    // cache serves this tick.
    if is_active
        && keychain_live()
        && let Some(adopted) =
            crate::oauth::try_adopt_live_rotation(config, name, &rotation_guard, &identity)
    {
        if let Ok(mut q) = refetch.lock() {
            q.insert(name.to_string());
        }
        // Carry the adopted pair as `rotated` so the caller syncs the
        // in-memory TokenList — otherwise the queued refetch would run on the
        // superseded entry and spend the revoked refresh token.
        return FetchOutcome::cached(name, FetchStatus::Cached, Some(adopted), None);
    }

    let Some(rt) = refresh_token else {
        return bail_unrotated();
    };
    // Re-check liveness under the guard: a live session owns the chain and will
    // refresh it itself — rotating here would double-spend the single-use token.
    // The guard makes this authoritative (winner stamped its PID file first).
    // `partition_due` excludes Refreshing/Switching but not live external sessions.
    // CLA-FEED exempts itself ONLY when the sidecar is genuinely armed
    // (LongLived): fed sessions run on a refresh-less bearer and never advance
    // the chain. The flag alone proves nothing — a session started inside an
    // arming window (sidecar absent/mis-filled) copied the ROTATING PAIR via
    // `install_source_path`, and rotating under it is the exact revoked-chain
    // death the split prevents (review round 1).
    if !(session_feed && crate::claude::has_session_token(name))
        && crate::runtime::has_live_session(name)
    {
        return bail_unrotated();
    }
    mark_activity(activity, name, ProfileActivity::Refreshing);
    // `refresh_result` (not `refresh`) so the RefreshError variant survives — the
    // poll needs to tell a dead token (quarantine) from a transient blip (retry).
    let rotation =
        crate::oauth::refresh_result(rt, crate::oauth::stored_scopes(config, name).as_deref());
    mark_activity(activity, name, ProfileActivity::Fetching);
    let tok = match rotation {
        Ok(t) => t,
        Err(e) => {
            // A failed refresh on the ACTIVE Keychain profile usually means
            // the live claude rotated first and revoked our copy — one more
            // adopt attempt (its mirror may have JUST surfaced the fresh
            // pair) before falling back. This same path re-runs every poll,
            // so a lagging store self-heals as soon as the mirror catches up
            // (at latest CC's next launch).
            if is_active
                && keychain_live()
                && let Some(adopted) =
                    crate::oauth::try_adopt_live_rotation(config, name, &rotation_guard, &identity)
            {
                if let Ok(mut q) = refetch.lock() {
                    q.insert(name.to_string());
                }
                // Sync the adopted pair into the TokenList (see the
                // rotation-leg adopt above).
                return FetchOutcome::cached(name, FetchStatus::Cached, Some(adopted), None);
            }
            if refresh_failure_is_terminal(&e) {
                // Double-spend guard before quarantining: if the on-disk pair
                // moved past the token we just spent, another refresher
                // already rotated the chain — carry the fresh pair into the
                // TokenList (clearing any stale quarantine) and retry next
                // tick (disk cache serves this one). Only an
                // unchanged-credentials 400 is a real revocation. The adopt
                // above is the identity-guarded fast path for the macOS
                // Keychain active; this re-read catches every other racer
                // (CC through the symlink, a sibling clauth process). See
                // `carry_external_rotation`.
                if let Some(outcome) = carry_external_rotation(config, name, rt, refetch) {
                    return outcome;
                }
                // A terminal failure (dead refresh token) quarantines the
                // account on this tick; a transient one leaves the flag and
                // retries. See `refresh_failure_is_terminal`.
                crate::oauth::mark_auth_broken(config, name, true);
                return bail_unrotated();
            }
            // Transient: the chain may still be good, so this account keeps
            // polling rather than quarantining. Count the failure — the streak
            // is the only thing that ladders the retry and names the state on
            // the row (`auth_broken` does neither for a profile it never flags).
            return bail_unrotated().with_refresh_failed();
        }
    };
    // Persist under the AppConfig mutex + state lock — matches every other rotation site
    // so a concurrent `rotate_one_inner` can't interleave, and keeps in-memory AppConfig in sync.
    let access = tok.access_token.clone();
    let refresh = tok.refresh_token.clone();
    if crate::oauth::apply_rotated_tokens_locked(config, name, tok).is_err() {
        return bail_to_cache(None);
    }
    // A successful refresh + persist clears any prior auth-broken quarantine
    // (mirrors `ensure_installable`); a no-op when the flag was already clear.
    crate::oauth::mark_auth_broken(config, name, false);
    let rotated: Option<RotatedTokens> = Some((access.clone(), Some(refresh)));
    // A refresh mints a new token for the SAME account, so no `/profile` field can
    // change because of it — the hourly TTL governs the plan here exactly as it
    // does on the plain leg above. Force a pull when holding NO plan (never
    // fetched, or an earlier `/profile` failed), OR when we rotated because
    // `/usage` 429'd on a clock-expired token: the pre-rotation attempt already
    // spent this tick's `/profile` slot on the now-dead token (a discarded read),
    // so without the force the just-written TTL stamp would skip the live-token
    // pull and a canceled account's flip would wait a full hour.
    let force_profile = prev_plan.is_none() || unmask_429.is_some();
    match fetch_raw(name, &access, prev_plan, force_profile, Some(activity)) {
        Ok(info) => FetchOutcome::live(name, info, rotated),
        Err(FetchError::RateLimited { retry_after, plan }) => {
            // Retry itself rate-limited. Don't push to RefetchQueue — that risks
            // a rotate→429→enqueue→rotate cycle. The retry-after deferral governs.
            // The fresh-token `/profile` reading still rides along (a canceled
            // account 429s `/usage` even on a freshly rotated token).
            FetchOutcome::cached(name, FetchStatus::RateLimited, rotated, retry_after)
                .with_plan(plan)
        }
        Err(_) => {
            // Rotation succeeded but a transient error stopped the retry.
            // Push to RefetchQueue so we retry with the new token next tick
            // rather than waiting the full refresh interval.
            if let Ok(mut q) = refetch.lock() {
                q.insert(name.to_string());
            }
            bail_to_cache(rotated)
        }
    }
}

/// One profile's fetch result, carried back to update shared state.
struct FetchOutcome {
    name: String,
    info: Option<UsageInfo>,
    status: FetchStatus,
    /// Rotated token pair when the fetch path rotated OAuth; propagated into `TokenList`.
    rotated: Option<RotatedTokens>,
    /// `info` is a live API body (not a disk-cache fallback). Only live bodies
    /// may overwrite the store / disk cache in [`apply_outcome`].
    from_fetch: bool,
    /// Server `retry-after` hint from a 429; [`apply_outcome`] turns it into a
    /// deferred next-fetch slot for this profile.
    retry_after: Option<Duration>,
    /// A token refresh failed WITHOUT the endpoint confirming the token is dead,
    /// so the chain is not quarantined and this profile keeps polling. Folded
    /// into the profile's `refresh_fail` streak by [`apply_outcome`], which is
    /// what ladders the cadence and names the state on the row.
    refresh_failed: bool,
    /// A `/profile` reading fetched DESPITE a `/usage` 429 (a canceled account
    /// 429s `/usage` forever). Overlaid onto the cached snapshot by
    /// [`apply_outcome`] so only the tier advances — windows stay cached — and
    /// persisted so the flip survives the next tick. `Some` only on the ~hourly
    /// tick `/profile` is actually re-pulled, never per masked tick.
    plan_override: Option<PlanInfo>,
}

impl FetchOutcome {
    /// A live API body — overwrites the store and disk cache.
    fn live(name: &str, info: UsageInfo, rotated: Option<RotatedTokens>) -> Self {
        Self {
            name: name.to_string(),
            info: Some(info),
            status: FetchStatus::Fresh,
            rotated,
            from_fetch: true,
            retry_after: None,
            refresh_failed: false,
            plan_override: None,
        }
    }

    /// Mark this outcome as following a transient refresh failure. A `Fresh`
    /// outcome is unaffected in practice — [`update_streaks`] clears the streak
    /// on a live body regardless.
    fn with_refresh_failed(mut self) -> Self {
        self.refresh_failed = true;
        self
    }

    /// A disk-cache fallback (`status` downgrades to `Failed` when no cache
    /// exists) — may only cold-fill an absent store entry.
    fn cached(
        name: &str,
        status: FetchStatus,
        rotated: Option<RotatedTokens>,
        retry_after: Option<Duration>,
    ) -> Self {
        let (info, status) = load_cached_with_status(name, status);
        Self {
            name: name.to_string(),
            info,
            status,
            rotated,
            from_fetch: false,
            retry_after,
            refresh_failed: false,
            plan_override: None,
        }
    }

    /// Carry a `/profile` plan fetched despite a `/usage` 429 into this cached
    /// bail. [`apply_outcome`] overlays it onto the cached windows so the tier
    /// advances (a canceled account flips Pro → Free/canceled) even though the
    /// windows stay stale. No-op when the plan is absent.
    fn with_plan(mut self, plan: Option<PlanInfo>) -> Self {
        self.plan_override = plan;
        self
    }
}

/// Patch a just-opened live 5h window back into a Fresh body that lags it. A
/// kick opens the window before `/usage` reflects it, so a Fresh body fetched in
/// the same tick can still report the window closed; writing it verbatim would
/// re-lapse the window and re-fire the kick. When `fresh` has no live 5h window
/// but `prev` does, keep `prev`'s window; every other field takes the fresh
/// value. A genuine new window (live in `fresh`) or a still-closed `prev` is left
/// untouched.
fn preserve_live_window(
    mut fresh: UsageInfo,
    prev: Option<&UsageInfo>,
    now_secs: i64,
) -> UsageInfo {
    if !five_hour_live(&fresh, now_secs)
        && let Some(prev) = prev
        && five_hour_live(prev, now_secs)
    {
        fresh.five_hour = prev.five_hour.clone();
    }
    fresh
}

/// True iff we hold a fetched usage entry for `name` whose 5h window is absent
/// or already past its reset — the signal to open a fresh window. An ABSENT
/// store entry (never fetched this run) returns false on purpose: fetch first,
/// kick next tick, so a cold cache never kicks blind on a window that may
/// already be live.
fn window_lapsed(store: &UsageStore, name: &str, now_secs: i64) -> bool {
    let Ok(s) = store.lock() else {
        return false;
    };
    let Some(info) = s.get(name) else {
        return false;
    };
    !five_hour_live(info, now_secs)
}

/// Current consecutive-429 streak for `name` (0 when absent or poisoned). Read
/// alone and released before any higher-ranked lock — POLL_STREAK(220)
/// sits below USAGE_STORE(300), so it must not be held across `window_lapsed`.
fn rate_limit_streak(streaks: &PollStreaks, name: &str) -> u32 {
    streaks
        .lock()
        .ok()
        .and_then(|m| m.get(name).copied())
        .unwrap_or_default()
        .rate_limit
}

/// Every profile's streak counts, copied out under one short lock. Taken at the
/// call site rather than inside [`partition_due`] so POLL_STREAK(220) is never
/// held under the `LastFetched`(200)/`Activity` locks that live there.
fn streak_snapshot(streaks: &PollStreaks) -> HashMap<String, StreakCounts> {
    streaks.lock().map(|m| m.clone()).unwrap_or_default()
}

/// Whether `run_fetch` should fire the auto-start kick. Never mid-`/usage`
/// 429-streak (`streak == 0`): the endpoint is already throttling and a kick on a
/// still-valid token can neither rotate nor open anything (see `auto_start_kick`).
/// Two firing modes:
///   * LAPSED window → open it, paced by the kick's own decaying retry clock
///     (`kick_due`, [`kick_retry_due`]) so a still-dead endpoint isn't re-hit
///     every due slot.
///   * LIVE window + a standing block → RE-TEST it, on the POLL cadence (ignoring
///     the deep `kick_due` backoff). Load-bearing: a live 5h window can be a
///     Claude-web open while Claude Code's `/v1/messages` stays 429'd for this
///     account, so window liveness does NOT clear the block — only a landed kick
///     does (`note_kick_outcome` `opened`). The lapsed-window backoff can ladder
///     to ~15min; honoring it here would leave the chain refusing to switch back
///     in long after the account recovered, so a reopened window re-tests every
///     poll (~one refresh interval) until a kick lands or 429s afresh.
fn should_open_window(streak: u32, window_lapsed: bool, kick_due: bool, has_block: bool) -> bool {
    if streak != 0 {
        return false;
    }
    if window_lapsed { kick_due } else { has_block }
}

/// The auto-start firing decision for `run_fetch`, factored out so it has a test
/// seam (`run_fetch` itself is HTTP-bound). Reads the streak, window, and kick
/// block for `name` and applies [`should_open_window`] — the `has_block` wiring
/// (`block.is_some()`) is the live-window re-test's load-bearing plumbing. Locks
/// are taken one at a time (never nested), so no rank-order constraint applies.
fn auto_start_should_kick(
    streaks: &PollStreaks,
    store: &UsageStore,
    kick_blocks: &KickBlocks,
    name: &str,
    now_secs: i64,
) -> bool {
    let block = kick_block(kick_blocks, name);
    should_open_window(
        rate_limit_streak(streaks, name),
        window_lapsed(store, name, now_secs),
        kick_retry_due(block.as_ref(), now_secs),
        block.is_some(),
    )
}

/// Copy of `name`'s kick block (`None` when absent or poisoned). Read alone and
/// released immediately — KickBlockState(230) is a leaf like PollStreak.
fn kick_block(blocks: &KickBlocks, name: &str) -> Option<KickBlock> {
    blocks.lock().ok().and_then(|m| m.get(name).copied())
}

/// Fold one more kick 429 into the block: the streak climbs the shared
/// [`rate_limit_backoff_ms`] ladder (10s ×3, 15min cap), clamped to the
/// limiter's advertised ceiling — once that passes, the next tick is always due.
fn kick_block_after_429(prev: Option<KickBlock>, rl: &KickRateLimit, now_secs: i64) -> KickBlock {
    let streak = prev.map_or(1, |b| b.streak.saturating_add(1));
    // The ladder fn itself is uncapped — every caller applies the shared cap.
    // Without it a header-less deep streak schedules hours out and wedges the
    // window closed long after the limiter relents.
    let ladder_secs = (rate_limit_backoff_ms(streak).min(MAX_RETRY_AFTER_MS) / 1000).max(1) as i64;
    let mut next_retry = now_secs.saturating_add(ladder_secs);
    if let Some(until) = rl.until_epoch_secs {
        next_retry = next_retry.min(until);
    }
    KickBlock {
        streak,
        rejected: rl.rejected,
        until: rl.until_epoch_secs,
        next_retry,
    }
}

/// Whether a blocked profile may kick again. `next_retry` is already clamped to
/// the advertised ceiling, so a passed ceiling is always due.
fn kick_retry_due(block: Option<&KickBlock>, now_secs: i64) -> bool {
    block.is_none_or(|b| now_secs >= b.next_retry)
}

/// Whether a kick block is switch-grade — strong enough to rotate the fallback
/// chain around its profile: the limiter's own `rejected` verdict, confirmed by
/// at least two consecutive kicks, with the advertised ceiling still ahead. A
/// single header-less burst 429 gets the pill and the backoff but never moves
/// the chain.
pub(crate) fn kick_block_switch_grade(block: &KickBlock, now_secs: i64) -> bool {
    block.rejected && block.streak >= 2 && block.until.is_some_and(|u| now_secs < u)
}

/// Names whose live kick block is switch-grade ([`kick_block_switch_grade`]),
/// copied out under one short leaf lock for the auto-switch/recovery scans.
fn kick_rejected_names(blocks: &KickBlocks, now_secs: i64) -> Vec<String> {
    blocks
        .lock()
        .map(|m| {
            m.iter()
                .filter(|(_, b)| kick_block_switch_grade(b, now_secs))
                .map(|(n, _)| n.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Every switch-grade kick block's advertised lift (epoch secs), keyed by profile
/// name, read under one leaf lock. Same [`kick_block_switch_grade`] rule the
/// auto-switch walk routes around, so the Fallback tab's blocked-reason chip
/// flags a kick-rejected member without re-deriving the predicate. Read before
/// the Config lock (rank order: KickBlockState 230 < Config 400).
pub(crate) fn switch_grade_kick_lifts(blocks: &KickBlocks) -> HashMap<String, i64> {
    let now = now_epoch_secs();
    blocks
        .lock()
        .map(|m| {
            m.iter()
                .filter(|(_, b)| kick_block_switch_grade(b, now))
                .filter_map(|(n, b)| b.until.map(|u| (n.clone(), u)))
                .collect()
        })
        .unwrap_or_default()
}

/// Fold a kick's outcome into the block map + its per-profile cache file, and
/// logline the state TRANSITIONS only (silent while a streak merely grows).
fn note_kick_outcome(
    blocks: &KickBlocks,
    name: &str,
    opened: bool,
    blocked: Option<KickRateLimit>,
    now_secs: i64,
) {
    let prev = kick_block(blocks, name);
    if opened {
        if prev.is_some() {
            if let Ok(mut m) = blocks.lock() {
                m.remove(name);
            }
            remove_profile_cache(name, KICK_BLOCK_CACHE_FILE);
            logline!("{name}: 5h auto-start unblocked, kick accepted");
        }
        return;
    }
    let Some(rl) = blocked else {
        return;
    };
    let next = kick_block_after_429(prev, &rl, now_secs);
    if let Ok(mut m) = blocks.lock() {
        m.insert(name.to_string(), next);
    }
    write_profile_cache(name, KICK_BLOCK_CACHE_FILE, &next);
    if prev.is_none() {
        let ceiling = next
            .until
            .map(|u| {
                format!(
                    ", api ceiling in {}",
                    humanize_duration(u.saturating_sub(now_secs))
                )
            })
            .unwrap_or_default();
        logline!(
            "{name}: 5h auto-start kick rate-limited (rejected: {}){ceiling}; backing off",
            next.rejected
        );
    }
}

/// Overwrite the in-memory kick blocks with each profile's on-disk cache file —
/// the standdown mirror of the fetching instance's write-through, and the
/// bootstrap seed so a restart doesn't forget a live block. All file IO happens
/// before the single lock take.
fn sync_kick_blocks_from_cache(blocks: &KickBlocks, names: &[String]) {
    let loaded: Vec<(String, Option<KickBlock>)> = names
        .iter()
        .map(|n| (n.clone(), load_profile_cache(n, KICK_BLOCK_CACHE_FILE)))
        .collect();
    if let Ok(mut m) = blocks.lock() {
        for (name, block) in loaded {
            match block {
                Some(b) => {
                    m.insert(name, b);
                }
                None => {
                    m.remove(&name);
                }
            }
        }
    }
}

/// Fetch one profile's usage on the periodic tick. When the profile opted into
/// auto-start, fire the kick first whenever `should_open_window` says to — to OPEN
/// a lapsed window, or to RE-TEST a standing kick block on a now-live window (it
/// may have reopened via the web app while Claude Code stays 429'd) — rotating
/// once on 401 OR 429, mark the window open on success, then fetch with the
/// possibly-rotated token.
fn run_fetch(
    config: &crate::profile::ConfigHandle,
    mut entry: TokenEntry,
    store: &UsageStore,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    streaks: &PollStreaks,
    kick_blocks: &KickBlocks,
) -> FetchOutcome {
    // Auto-start leg: fire the kick before fetching when this profile opted in and
    // `should_open_window` says to — to open a lapsed window, or to re-test a
    // standing kick block on a live window (its two modes), as long as no 429
    // streak is in flight. The kick may rotate the chain (401 OR 429 in this
    // branch only); fold its rotated pair into both the local entry (so the
    // fetch below uses the fresh token, never re-spending) and the returned
    // outcome (so the tick syncs it into the live snapshot).
    let mut kick_rotated: Option<RotatedTokens> = None;
    if entry.auto_start {
        let now_secs = now_epoch_secs();
        if auto_start_should_kick(streaks, store, kick_blocks, &entry.name, now_secs) {
            let kicked = crate::oauth::auto_start_kick(
                config,
                &entry.name,
                &entry.access_token,
                entry.refresh_token.as_deref(),
                entry.access_expires_at,
                Some(activity),
            );
            note_kick_outcome(
                kick_blocks,
                &entry.name,
                kicked.opened,
                kicked.blocked,
                now_secs,
            );
            if let Some((access, refresh)) = kicked.rotated.clone() {
                entry.access_token = access;
                entry.refresh_token = refresh;
                kick_rotated = kicked.rotated;
            }
            if kicked.opened {
                mark_window_open(store, &entry.name, now_secs);
            }
        }
    }

    // Prior plan for the TTL'd `/profile` policy, read from the live store and
    // released before the fetch so no lock is held across HTTP.
    let prev_plan = store
        .lock()
        .ok()
        .and_then(|m| m.get(&entry.name).and_then(|i| i.plan.clone()));

    let mut outcome = fetch_with_rotation(config, &entry, prev_plan, refetch, activity);
    // The fetch's own rotation (if any) supersedes the kick's; otherwise carry
    // the kick's rotated pair back so the tick still syncs the spent chain.
    if outcome.rotated.is_none() {
        outcome.rotated = kick_rotated;
    }
    outcome
}

/// Extra backoff (ms) for the `streak`-th consecutive 429 with no usable hint:
/// `base * factor^(streak - 1)`, saturating. The ceiling is applied by
/// [`next_slot_deferral`].
fn rate_limit_backoff_ms(streak: u32) -> u64 {
    let exp = streak.saturating_sub(1);
    RATE_LIMIT_MIN_BACKOFF_MS.saturating_mul(RATE_LIMIT_BACKOFF_FACTOR.saturating_pow(exp))
}

/// Deferral added to a profile's `last_fetched` stamp so `partition_due`'s fixed
/// `stamp + interval` math lands the next slot correctly. On a 429 the slot is
/// `max(server retry-after, one interval + `[`rate_limit_backoff_ms`]`)` —
/// a REAL long hint is honored verbatim, but a `0` / sub-cadence hint can
/// never suppress the streak ladder. The usage endpoint answers EVERY 429
/// with `retry-after: 0` while its sliding window counts the rejected
/// requests too; taking that "retry now" at face value re-polls at cadence,
/// keeps the window pinned full, and the profile never leaves `RateLimited`
/// (observed 2026-07-11: hours of uninterrupted per-account 429s that only a
/// growing back-off can drain). Capped at [`MAX_RETRY_AFTER_MS`]. Non-429
/// outcomes: no defer.
///
/// The ACTIVE profile's ladder caps at one extra interval (2× cadence) while
/// the streak is shallow (≤ [`ACTIVE_CAP_MAX_STREAK`]): a deep slot on the row
/// the user is watching mostly buys staleness (observed 2026-07-12: the
/// endpoint recovered while the active account sat out a 14-minute slot as
/// `RateLimited`). The cap must NOT be unconditional: the `/usage` window is
/// filled only by clauth's own polls — the running claude's `/v1/messages`
/// traffic never touches it — so on a SUSTAINED storm capped ~2×-cadence
/// re-polls would keep the window pinned (the exact #30 failure); past the
/// bound the active row climbs the same drain ladder as everyone else. A REAL
/// server `retry-after` still wins (though `/usage` itself only ever sends 0).
fn next_slot_deferral(
    rate_limited: bool,
    retry_after: Option<Duration>,
    streak: u32,
    interval_ms: u64,
    active: bool,
) -> IntervalMs {
    let hint = retry_after.map(|ra| ra.as_millis() as u64);
    let target_ms = if rate_limited {
        let mut ladder = interval_ms.saturating_add(rate_limit_backoff_ms(streak));
        if active && streak <= ACTIVE_CAP_MAX_STREAK {
            ladder = ladder.min(interval_ms.saturating_mul(2));
        }
        hint.unwrap_or(0).max(ladder)
    } else {
        hint.unwrap_or(0)
    };
    IntervalMs::from_millis(
        target_ms
            .min(MAX_RETRY_AFTER_MS)
            .saturating_sub(interval_ms),
    )
}

/// Deterministic per-profile spread (phase offset + per-cycle jitter) added to a
/// live fetch's `last_fetched` stamp so distinct profiles don't fall due on the
/// same tick — avoiding a same-instant request burst against the shared host.
/// Range `[0, interval/4)`. Keyed by `(name, now)`: the name separates profiles,
/// `now` re-rolls the jitter each cycle; stable for a given stamp so the deadline
/// never moves earlier mid-wait. Only widens the gap, never shortens it.
fn deadline_spread(name: &str, now: EpochMs, interval_ms: u64) -> IntervalMs {
    use std::hash::{Hash, Hasher};
    let span = interval_ms / 4;
    if span == 0 {
        return IntervalMs::from_millis(0);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    now.as_millis().hash(&mut h);
    IntervalMs::from_millis(h.finish() % span)
}

/// Update `name`'s failure counters from a fetch `status` (plus whether its
/// refresh leg just failed transiently), returning the post-update counts.
/// `Fresh` clears both (a live body breaks the storm). Otherwise `RateLimited`
/// bumps the 429 axis and a transient refresh failure bumps its own; a status
/// that says nothing about either — a network-blip `Cached`/`Failed` mid-storm —
/// leaves both as is so the ramp is not reset. Leaf lock, taken and released
/// before the caller writes `last_fetched`/`status`.
fn update_streaks(
    streaks: &PollStreaks,
    name: &str,
    status: FetchStatus,
    refresh_failed: bool,
) -> StreakCounts {
    let Ok(mut m) = streaks.lock() else {
        return StreakCounts::default();
    };
    // A live body clears BOTH axes: whatever went wrong, this profile is serving
    // again. That also covers the preemptive-rotation case, where a refresh can
    // fail while the still-valid access token fetches fine — nothing is degraded
    // yet, so nothing should ladder or light up the row.
    if matches!(status, FetchStatus::Fresh) {
        m.remove(name);
        return StreakCounts::default();
    }
    let rate_limited = matches!(status, FetchStatus::RateLimited);
    if !rate_limited && !refresh_failed {
        // Says nothing about either axis — leave both counters (and, when this
        // profile has none, the empty map) untouched.
        return m.get(name).copied().unwrap_or_default();
    }
    let counts = m.entry(name.to_string()).or_default();
    if rate_limited {
        counts.rate_limit = counts.rate_limit.saturating_add(1);
    }
    if refresh_failed {
        counts.refresh_fail = counts.refresh_fail.saturating_add(1);
    }
    *counts
}

/// Write one outcome into the shared stores; returns the stamped next-fetch base
/// (`last_fetched`) so the caller republishes this profile's countdown the instant
/// it lands. Disk cache written on every live response.
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    streaks: &PollStreaks,
    interval_ms: u64,
    is_active: bool,
) -> EpochMs {
    let now = EpochMs::from_millis(now_ms());

    // Only a body that came off the live API may overwrite shared state. The
    // 429/cached fallback paths recycle the on-disk snapshot — stamping that
    // as fresh would clobber a newer store entry and re-write the disk cache
    // mtime, freezing the UI (and the auto-start scan) on stale numbers for as
    // long as the rate limit lasts. `status` still surfaces RateLimited/Cached
    // so the staleness stays visible.
    let is_fresh = outcome.from_fetch;

    // A `/profile` plan fetched despite a `/usage` 429 (a canceled account 429s
    // `/usage` forever): the ONLY fresh signal on an otherwise cached bail. The
    // fetch path carries it only on the ~hourly tick `/profile` is re-pulled, so
    // persisting it re-stamps the disk mtime at most once an hour — not the
    // per-tick storm the cached path guards against. Windows stay cached; only
    // the tier advances.
    let plan_refresh = outcome.plan_override.clone().filter(|_| !is_fresh);

    // For a Fresh body, keep any just-opened live 5h window we already hold so a
    // lagging `/usage` read can't re-close it (see `preserve_live_window`). The
    // prev window is read under a short lock, released before the disk write.
    let merged: Option<UsageInfo> = outcome.info.as_ref().map(|info| {
        if !is_fresh {
            let mut info = info.clone();
            if let Some(plan) = &plan_refresh {
                info.plan = Some(plan.clone());
            }
            return info;
        }
        let now_secs = now_epoch_secs();
        let prev = store
            .lock()
            .ok()
            .and_then(|s| s.get(&outcome.name).cloned());
        preserve_live_window(info.clone(), prev.as_ref(), now_secs)
    });

    // A profile added while ALREADY canceled 429s `/usage` from its first poll and
    // never gets a `usage_cache.json`, so `cached()` yields `info=None` and there
    // is nothing to overlay onto. Without recording the plan on a windowless entry
    // the cancellation is dropped every tick AND the fallback walk keeps treating
    // the dead account as selectable (`is_canceled_from_usage` reads no store
    // entry). Cold-fill a plan-only body so both surfaces see it. `filter` keeps
    // this to the same ~hourly cadence as `plan_refresh`, not a per-tick write.
    let cold_fill = plan_refresh
        .clone()
        .filter(|_| merged.is_none())
        .map(|plan| UsageInfo {
            plan: Some(plan),
            ..Default::default()
        });

    if (is_fresh || plan_refresh.is_some())
        && let Some(info) = merged.as_ref().or(cold_fill.as_ref())
    {
        write_profile_cache(&outcome.name, USAGE_CACHE_FILE, info);
    }

    if let Ok(mut s) = store.lock() {
        if let Some(info) = &merged {
            // Don't clobber newer Fresh data with a Cached fallback snapshot.
            // Cached only fills the store when no entry exists (cold start).
            if is_fresh || !s.contains_key(&outcome.name) {
                s.insert(outcome.name.clone(), info.clone());
            } else if let Some(plan) = &plan_refresh
                && let Some(existing) = s.get_mut(&outcome.name)
            {
                // Advance only the tier on the live entry; its windows stay cached.
                existing.plan = Some(plan.clone());
            }
        } else if let Some(plan) = &plan_refresh {
            // Cold-miss: advance the tier on any existing (windowless) entry, else
            // record a plan-only one so the walk can exclude the dead account.
            match s.get_mut(&outcome.name) {
                Some(existing) => existing.plan = Some(plan.clone()),
                None => {
                    s.insert(
                        outcome.name.clone(),
                        UsageInfo {
                            plan: Some(plan.clone()),
                            ..Default::default()
                        },
                    );
                }
            }
        }
    }

    // Server-directed deferral: a 429's `retry-after` lands the next slot on
    // `now + retry_after` (capped); a 429 with no hint backs off exponentially by
    // the consecutive-429 count; everything else keeps the cadence. Live fetches
    // also get a per-profile spread so two profiles don't fall due on the same tick.
    let rate_limited = matches!(outcome.status, FetchStatus::RateLimited);
    // Only the 429 axis feeds the deferral here; the refresh-fail axis widens at
    // partition time instead (`TokenEntry::poll_backoff_ms`) so a recovery snaps
    // the cadence back on the next tick rather than sitting out a baked-in stamp.
    let counts = update_streaks(
        streaks,
        &outcome.name,
        outcome.status,
        outcome.refresh_failed,
    );
    let defer = next_slot_deferral(
        rate_limited,
        outcome.retry_after,
        counts.rate_limit,
        interval_ms,
        is_active,
    );
    let spread = if outcome.from_fetch {
        deadline_spread(&outcome.name, now, interval_ms)
    } else {
        IntervalMs::from_millis(0)
    };
    let stamped = now.saturating_add(defer).saturating_add(spread);

    // Both in one critical section — ascending rank order: LAST_FETCHED(200) < USAGE_STATUS(350).
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), stamped);
        if let Ok(mut st) = status.lock() {
            st.insert(outcome.name.clone(), outcome.status);
        }
    }
    stamped
}

/// Optimistically mark a just-kicked profile's 5h window open in the store. A
/// 200 from the kick endpoint IS the window opening, but `/usage` can
/// rate-limit for minutes afterwards — until a live body lands, the usage tab
/// and the auto-start scan would keep seeing the stale windowless snapshot and
/// re-arm a profile whose window is already running. Utilization starts at 0
/// (the kick is ~1 token); the next live fetch overwrites the synthetic entry
/// with API truth. No-op while the stored window is still live.
fn mark_window_open(store: &UsageStore, name: &str, now_secs: i64) {
    let Ok(mut s) = store.lock() else {
        return;
    };
    let info = s.entry(name.to_string()).or_default();
    let live = info
        .five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_deref())
        .and_then(iso_to_epoch_secs)
        .is_some_and(|resets_at| now_secs < resets_at);
    if live {
        return;
    }
    info.five_hour = Some(UsageWindow {
        utilization: 0.0,
        resets_at: Some(epoch_secs_to_iso(now_secs + 5 * 3600)),
    });
}

/// Startup usage seed — never blocks on HTTP. Each profile with an on-disk cache is
/// seeded straight from disk so the UI shows last-known numbers instantly, with
/// `last_fetched` stamped at the cache mtime so the fixed cadence *resumes* across
/// the restart (see [`try_seed_cache`]) instead of resetting the countdown. A cache
/// older than one interval is seeded `Cached` and refreshed in the background on the
/// first tick; one younger is `Fresh` and left be. A profile with no cache at all is
/// left unseeded and unstamped, so the scheduler fetches it fresh on its first tick.
/// `seed_names` is the display superset ([`collect_oauth_seed_names`], disabled
/// included) — seeding a disabled profile's cache never adds it to the work-list.
pub(crate) fn bootstrap_fetch(
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    seed_names: &[String],
    interval_ms: u64,
) {
    let now = now_ms();
    for name in seed_names {
        try_seed_cache(store, status, last_fetched, name, now, interval_ms);
    }
}

/// Load gate shared by both startup seed sites. Takes no locks so each caller
/// stamps its own typed store and keeps its own lock rank (LAST_FETCHED then the
/// status store). Returns the loaded value, the cache mtime, and a freshness-derived
/// [`FetchStatus`] whenever a cache file exists AND is loadable; `None` only when
/// there is no cache. The cache is seeded as a starting point regardless of age:
/// `Fresh` when younger than one refresh interval (still in the fetch window — the
/// scheduler leaves it be), `Cached` when older (shown immediately while the
/// scheduler refreshes it in the background). See [`try_seed_cache`] /
/// [`bootstrap_third_party`] for why `last_fetched` is stamped at the mtime.
fn load_cache_seed<T>(
    name: &str,
    interval_ms: u64,
    now: u64,
    mtime_fn: impl Fn(&str) -> Option<u64>,
    load_fn: impl Fn(&str) -> Option<T>,
) -> Option<(T, u64, FetchStatus)> {
    let mtime = mtime_fn(name)?;
    let value = load_fn(name)?;
    let status = if now.saturating_sub(mtime) < interval_ms {
        FetchStatus::Fresh
    } else {
        FetchStatus::Cached
    };
    Some((value, mtime, status))
}

/// Seed `name` from its on-disk cache whenever one exists, returning `true`. The
/// cache is the startup starting point regardless of age: a cache younger than one
/// interval is `Fresh` (still in the fetch window — `partition_due` won't refetch
/// it), an older one is `Cached` (shown immediately while the scheduler refreshes it
/// in the background on the first tick). The `last_fetched` slot is stamped at the
/// cache **mtime**, so `partition_due` resumes the fixed cadence from the last real
/// write — the overview countdown continues where it left off across a restart
/// rather than resetting to a full interval, and a fresh cache never falls due on
/// the first tick (no startup refresh burst). A `Cached` seed may sit on a 5h window
/// that has since rolled over, so the startup auto-switch one-shot in
/// `finish_bootstrap` acts on `Fresh` data only; stale profiles auto-switch off the
/// corrected numbers on the scheduler's first tick.
fn try_seed_cache(
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    name: &str,
    now: u64,
    interval_ms: u64,
) -> bool {
    let Some((info, mtime, fetch_status)) = load_cache_seed(
        name,
        interval_ms,
        now,
        |n| profile_cache_mtime_ms(n, USAGE_CACHE_FILE),
        |n| load_profile_cache::<UsageInfo>(n, USAGE_CACHE_FILE),
    ) else {
        return false;
    };
    if let Ok(mut s) = store.lock() {
        s.insert(name.to_string(), info);
    }
    // Ascending rank order: LAST_FETCHED(200) < USAGE_STATUS(350) — matches `apply_outcome`.
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(name.to_string(), EpochMs::from_millis(mtime));
        if let Ok(mut st) = status.lock() {
            st.insert(name.to_string(), fetch_status);
        }
    }
    true
}

/// Startup third-party seed — the api-key/provider analogue of [`bootstrap_fetch`].
/// Each profile with a `third_party_cache.json` is seeded straight from disk
/// (`last_fetched` stamped at the cache mtime so the cadence resumes across the
/// restart) so the UI shows last-known numbers instantly: `Fresh` when younger than
/// one interval, `Cached` when older (refreshed in the background on the first tick).
/// A profile with no cache is left unstamped, so the scheduler fetches it fresh.
pub(crate) fn bootstrap_third_party(
    store: &ThirdPartyUsageStore,
    status: &ThirdPartyStatusStore,
    last_fetched: &LastFetchedAt,
    entries: &[ThirdPartyEntry],
    interval_ms: u64,
) {
    let now = now_ms();
    for entry in entries {
        let Some((stats, mtime, fetch_status)) = load_cache_seed(
            &entry.name,
            interval_ms,
            now,
            |n| profile_cache_mtime_ms(n, THIRD_PARTY_CACHE_FILE),
            |n| load_profile_cache::<ThirdPartyStats>(n, THIRD_PARTY_CACHE_FILE),
        ) else {
            continue;
        };
        if let Ok(mut s) = store.lock() {
            s.insert(entry.name.clone(), stats);
        }
        // Ascending rank order: LAST_FETCHED(200) < THIRD_PARTY_STATUS(280).
        if let Ok(mut lf) = last_fetched.lock() {
            lf.insert(entry.name.clone(), EpochMs::from_millis(mtime));
            if let Ok(mut st) = status.lock() {
                st.insert(entry.name.clone(), fetch_status);
            }
        }
    }
}

/// Collect api-key profiles for the third-party fetch leg: recognised providers
/// (typed fetch) plus unrecognised api-key endpoints (generic discovery + scan).
/// A disabled profile is excluded — it must never enter the scheduler's
/// per-profile work list (no polling, no rotation).
pub(crate) fn collect_third_party_entries(
    profiles: &[crate::profile::Profile],
) -> Vec<ThirdPartyEntry> {
    profiles
        .iter()
        .filter(|p| !p.is_disabled())
        .filter_map(|p| {
            // CDX-1 invariant: codex profiles never enter an Anthropic fetch
            // leg, even if one somehow carries an api_key (see the inline
            // regression test).
            if p.is_codex() {
                return None;
            }
            let api_key = p.api_key.clone()?;
            let target = if let Some(provider) = p.provider {
                crate::providers::ThirdPartyTarget::Known(provider)
            } else {
                let base_url = p.base_url.clone()?;
                crate::providers::ThirdPartyTarget::Generic { base_url }
            };
            Some(ThirdPartyEntry {
                name: p.name.to_string(),
                target,
                api_key,
            })
        })
        .collect()
}

/// Collect the OAuth profiles' token snapshots for the refresher's `TokenList`.
/// Skips api-key/credential-less profiles (no `claudeAiOauth`) and disabled
/// ones (`AppConfig::enabled_profiles`) — a disabled profile must never enter
/// the scheduler's per-profile work list (no poll, no rotate, no auto-start
/// kick). Snapshots the persisted quarantine flag so the poll partition can
/// widen a flagged profile's cadence without a config lock. Shared by the TUI
/// (`App::new` / `refresh_tokens`) and the headless `daemon`.
pub(crate) fn collect_tokens(config: &crate::profile::AppConfig) -> Vec<TokenEntry> {
    config
        .enabled_profiles()
        .filter_map(|p| {
            // CDX-1 invariant: codex profiles never enter the OAuth fetch leg
            // (their usage source is passive — CDX-2), even if one somehow
            // carries claude-shaped credentials.
            if p.is_codex() {
                return None;
            }
            let oauth = p.credentials.as_ref()?.claude_ai_oauth.as_ref()?;
            Some(TokenEntry {
                name: p.name.to_string(),
                access_token: oauth.access_token.clone(),
                refresh_token: oauth.refresh_token.clone(),
                auto_start: p.auto_start,
                access_expires_at: oauth.expires_at,
                auth_broken: config.is_auth_broken(&p.name),
            })
        })
        .collect()
}

/// Profile names to SEED from on-disk usage caches at startup / standby: every
/// OAuth-credentialed profile, **disabled ones included**. The startup seed is a
/// DISPLAY concern (last-known tier / windows for the UI), so it is deliberately
/// WIDER than [`collect_tokens`]'s work-list — a disabled profile is seeded so
/// its cached numbers render, but it never enters the poll / rotate / auto-start
/// work-list (that stays `collect_tokens`, enabled-only). This can't make a
/// disabled profile pollable: seeding only loads a cache that already exists
/// (`try_seed_cache` is a no-op otherwise), and no work-driving code enumerates
/// the store's keys — every poll/rotate/switch decision sources its candidates
/// from `state.tokens` or the disabled-filtered fallback chain, reading the
/// store only per known name.
pub(crate) fn collect_oauth_seed_names(config: &crate::profile::AppConfig) -> Vec<String> {
    config
        .profiles
        .iter()
        .filter(|p| {
            p.credentials
                .as_ref()
                .and_then(|c| c.claude_ai_oauth.as_ref())
                .is_some()
        })
        .map(|p| p.name.to_string())
        .collect()
}

/// Remove session-suppressed generic profiles from the third-party snapshot so
/// they aren't re-fetched on the timer. The set is cloned once (it is small) so
/// no lock is held across the filter; a poisoned lock passes the snapshot through.
fn filter_suppressed(
    suppressed: &SuppressedGenericStore,
    snapshot: Vec<ThirdPartyEntry>,
) -> Vec<ThirdPartyEntry> {
    let Some(sup) = suppressed.lock().ok() else {
        return snapshot;
    };
    if sup.is_empty() {
        return snapshot;
    }
    snapshot
        .into_iter()
        .filter(|e| !sup.contains(&e.name))
        .collect()
}

/// Fetch a pre-partitioned set of due OAuth profiles and apply outcomes to the
/// usage stores. Mirrors [`fetch_third_party_due`]: partitioning + countdown
/// publishing happen in `tick`; this leg only fetches. Each worker paces against
/// the shared `api.anthropic.com` host inside `get_json`.
fn fetch_oauth_due(state: &SchedulerState, due: Vec<TokenEntry>, interval_ms: u64) {
    fetch_oauth_due_with(state, due, interval_ms, |entry| {
        run_fetch(
            &state.config,
            entry,
            &state.store,
            &state.refetch_queue,
            &state.activity,
            &state.poll_streaks,
            &state.kick_blocks,
        )
    });
}

/// Fan out one worker per due profile and apply each outcome the instant its own
/// fetch resolves. Result processing is keyed on COMPLETION order — each worker
/// sends on an `mpsc` channel when `run` returns and the drain applies in arrival
/// order — so a slow account never stalls a faster one's spinner-clear and
/// countdown behind it in the `due` list (the join-order stall). `run` is the
/// per-profile fetch: real [`run_fetch`] in production, a deterministic fake in
/// tests. Marked `Queued`, not `Fetching`: the per-host throttle
/// (`REQUEST_SPACING_MS`) serializes the HTTP, so each worker flips itself to
/// `Fetching` (in `get_json`) only when its request clears the gate.
fn fetch_oauth_due_with<F>(state: &SchedulerState, due: Vec<TokenEntry>, interval_ms: u64, run: F)
where
    F: Fn(TokenEntry) -> FetchOutcome + Sync,
{
    for entry in &due {
        mark_activity(&state.activity, &entry.name, ProfileActivity::Queued);
    }
    let expected = due.len();
    let (tx, rx) = std::sync::mpsc::channel::<FetchOutcome>();
    let run = &run;
    std::thread::scope(|scope| {
        let handles: Vec<_> = due
            .into_iter()
            .map(|entry| {
                let name = entry.name.clone();
                let tx = tx.clone();
                let h = scope.spawn(move || {
                    let outcome = run(entry);
                    // A drained receiver (already got its `expected` count) drops
                    // this send; harmless. A panicking worker never reaches here.
                    let _ = tx.send(outcome);
                });
                (name, h)
            })
            .collect();
        // Drop the spare sender so the drain's `recv` unblocks once every worker's
        // clone is gone (a panicked worker drops its clone on unwind) — it then
        // never waits on a message that will never arrive.
        drop(tx);

        drain_oauth_completions(state, &rx, expected, interval_ms);

        // Reap the workers; a panicked worker sent nothing, so its slot may still
        // read `Queued` — clear it here so the spinner doesn't freeze.
        for (name, h) in handles {
            if h.join().is_err() {
                clear_activity(&state.activity, &name);
            }
        }
    });
}

/// Apply up to `expected` OAuth outcomes in the order their fetches COMPLETE
/// (each worker sends on `rx` when its fetch returns). Per outcome: clear the
/// spinner, propagate a rotated token pair into the live snapshot, read
/// `is_active` at apply time, write the outcome, republish the countdown.
/// Bounded by `expected`, and it bails the instant `rx` disconnects (every
/// sender dropped) so a panicked worker's missing message can never wedge it.
fn drain_oauth_completions(
    state: &SchedulerState,
    rx: &Receiver<FetchOutcome>,
    expected: usize,
    interval_ms: u64,
) {
    for _ in 0..expected {
        let Ok(outcome) = rx.recv() else { break };
        let name = outcome.name.clone();
        clear_activity(&state.activity, &outcome.name);
        // Propagate rotated tokens back into the live snapshot — otherwise
        // tick N+1 reuses the stale access token, 401s, and double-burns the chain.
        if let Some((new_access, new_refresh)) = &outcome.rotated
            && let Ok(mut t) = state.tokens.lock()
            && let Some(entry) = t.iter_mut().find(|e| e.name == outcome.name)
        {
            entry.access_token = new_access.clone();
            entry.refresh_token = new_refresh.clone();
        }
        // The active profile's 429 ladder caps low (see `next_slot_deferral`);
        // read the flag at apply time so a switch mid-flight lands the right cadence.
        let is_active = state
            .config
            .lock()
            .map(|c| c.is_active(&outcome.name))
            .unwrap_or(false);
        let stamped = apply_outcome(
            outcome,
            &state.store,
            &state.status,
            &state.last_fetched,
            &state.poll_streaks,
            interval_ms,
            is_active,
        );
        publish_one_countdown(&state.next_refresh_per_profile, name, stamped, interval_ms);
    }
}

/// Fetch a pre-partitioned set of due third-party entries and apply outcomes to
/// the third-party stores. Partitioning + countdown publishing happen in `tick`
/// so both legs share one publish window; this leg only fetches.
fn fetch_third_party_due(state: &SchedulerState, due: Vec<ThirdPartyEntry>) {
    let interval_ms = state.refresh_interval.load(Ordering::Relaxed);
    for entry in &due {
        // `Queued`, not `Fetching`: same-host accounts wait behind the per-host
        // spacing slot, so each worker flips itself to `Fetching` only once its
        // request clears the gate (mirrors the OAuth leg's `get_json` flip).
        mark_activity(&state.activity, &entry.name, ProfileActivity::Queued);
    }

    let handles: Vec<_> = due
        .into_iter()
        .map(|entry| {
            let name = entry.name.clone();
            // Only generic no-data outcomes get session-suppressed; known
            // providers keep retrying on their normal cadence.
            let is_generic = matches!(
                entry.target,
                crate::providers::ThirdPartyTarget::Generic { .. }
            );
            // Reuse the endpoint that last worked so steady state is one request.
            let hint = state
                .third_party_usage_store
                .lock()
                .ok()
                .and_then(|s| s.get(&entry.name).and_then(|st| st.endpoint.clone()));
            // Pace against this provider's host only: accounts on the same endpoint
            // serialize, distinct hosts (and the Anthropic OAuth leg) run in parallel.
            let host = entry.target.throttle_key();
            let activity = Arc::clone(&state.activity);
            let worker_name = entry.name.clone();
            let h = std::thread::spawn(move || {
                await_request_slot(&host);
                mark_activity(&activity, &worker_name, ProfileActivity::Fetching);
                crate::providers::fetch_third_party_usage(
                    &entry.target,
                    &entry.api_key,
                    hint.as_deref(),
                )
            });
            (name, is_generic, h)
        })
        .collect();

    for (name, is_generic, h) in handles {
        match h.join() {
            Ok(Ok(stats)) => {
                clear_activity(&state.activity, &name);
                write_profile_cache(&name, THIRD_PARTY_CACHE_FILE, &stats);
                if let Ok(mut store) = state.third_party_usage_store.lock() {
                    store.insert(name.clone(), stats);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.clone(), FetchStatus::Fresh);
                }
                stamp_last_fetched(
                    &state.last_fetched,
                    &state.next_refresh_per_profile,
                    name,
                    None,
                    false,
                    interval_ms,
                );
            }
            Ok(Err(err)) => {
                clear_activity(&state.activity, &name);
                // Cache cold-fills an absent entry only — never overwrites live
                // store data with disk state (same rule as the OAuth path).
                let cached = load_profile_cache::<ThirdPartyStats>(&name, THIRD_PARTY_CACHE_FILE);
                // A 429 carries the server's `retry-after` and defers the next
                // slot (same server-directed deferral as the OAuth 429 path);
                // any other error falls back to cache without deferring.
                let (status, retry_after) = match &err {
                    crate::providers::ThirdPartyError::RateLimited { retry_after } => {
                        (FetchStatus::RateLimited, *retry_after)
                    }
                    _ if cached.is_some() => (FetchStatus::Cached, None),
                    _ => (FetchStatus::Failed, None),
                };
                if let Some(c) = cached
                    && let Ok(mut store) = state.third_party_usage_store.lock()
                {
                    store.entry(name.clone()).or_insert(c);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.clone(), status);
                }
                // A generic profile that tried and found nothing (no cache, not a
                // 429) suppresses for the rest of the session — no timer retry,
                // only a manual refresh re-admits it for one retry. 429 keeps the
                // server-directed deferral; cached/known-provider legs are unaffected.
                if is_generic
                    && matches!(status, FetchStatus::Failed)
                    && let Ok(mut sup) = state.suppressed_generic.lock()
                {
                    sup.insert(name.clone());
                }
                stamp_last_fetched(
                    &state.last_fetched,
                    &state.next_refresh_per_profile,
                    name,
                    retry_after,
                    matches!(status, FetchStatus::RateLimited),
                    interval_ms,
                );
            }
            Err(_) => {
                // Worker panicked — clear slot so the spinner doesn't freeze.
                clear_activity(&state.activity, &name);
            }
        }
    }
}

/// Stamp a profile's fetch slot. Normally `now` (so the next deadline reflects
/// fetch duration, mirroring OAuth `apply_outcome`); a 429's `retry-after`
/// stamps `retry_after - interval` ahead so `partition_due`'s fixed
/// `stamp + interval_ms` math lands the next slot on `now + retry_after`
/// (capped by [`MAX_RETRY_AFTER_MS`]).
fn stamp_last_fetched(
    last_fetched: &LastFetchedAt,
    next_refresh: &NextRefreshPerProfile,
    name: String,
    retry_after: Option<Duration>,
    rate_limited: bool,
    interval_ms: u64,
) {
    // Third-party providers are independent hosts with their own limits; keep the
    // flat base backoff (streak 1) rather than the per-account exponential ramp.
    let defer = next_slot_deferral(rate_limited, retry_after, 1, interval_ms, false);
    let stamped = EpochMs::from_millis(now_ms()).saturating_add(defer);
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(name.clone(), stamped);
    }
    publish_one_countdown(next_refresh, name, stamped, interval_ms);
}

/// Partition a leg's snapshot into due entries + per-profile countdowns, with
/// forced (cadence-bypassing) names merged in. Empty snapshot → no work, no
/// lock traffic. Shared by both legs so they publish in one window.
fn partition_and_merge<T: NamedEntry + Clone>(
    snapshot: &[T],
    forced: &HashSet<String>,
    state: &SchedulerState,
    now: u64,
    interval_ms: u64,
) -> (Vec<T>, HashMap<String, u64>) {
    if snapshot.is_empty() {
        return (Vec::new(), HashMap::new());
    }
    let (mut due, mut next) = partition_due(
        snapshot,
        now,
        &state.last_fetched,
        &state.activity,
        interval_ms,
        &streak_snapshot(&state.poll_streaks),
    );
    merge_forced(snapshot, forced, &mut due, &mut next, &state.activity, now);
    (due, next)
}

/// Full-replace publish of both legs' countdowns in one lock window. `clear`
/// before `extend` drops any deleted profile's stale key and avoids the
/// mid-tick window where one leg's countdowns are momentarily missing.
fn publish_countdowns(
    nrpp: &NextRefreshPerProfile,
    oauth: HashMap<String, u64>,
    third_party: HashMap<String, u64>,
) {
    if let Ok(mut map) = nrpp.lock() {
        map.clear();
        map.extend(oauth);
        map.extend(third_party);
    }
}

/// Republish one profile's countdown (`stamped + interval`, mirroring
/// [`partition_due`]) the instant its fetch lands, so the timer jumps straight
/// from the fetch spinner to the real interval instead of holding the pre-fetch
/// `0s` until the whole batch finishes. Per-key insert (not the full clear+replace
/// of [`publish_countdowns`]) so it can't drop the other leg's keys. NEXT_REFRESH
/// (1100) is acquired alone, after the caller's lower-ranked locks — rank-safe.
fn publish_one_countdown(
    nrpp: &NextRefreshPerProfile,
    name: String,
    stamped: EpochMs,
    interval_ms: u64,
) {
    if let Ok(mut map) = nrpp.lock() {
        map.insert(name, stamped.as_millis().saturating_add(interval_ms));
    }
}

/// Background scheduler state. Holds **cloned `Arc`s only** — no live lock guards —
/// so the struct carries no lock rank. `tick` acquires individual mutexes in rank order.
pub(crate) struct SchedulerState {
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    poll_streaks: PollStreaks,
    kick_blocks: KickBlocks,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    suppressed_generic: SuppressedGenericStore,
    shutting_down: Arc<AtomicBool>,
    /// Single-fetcher lease (issue #27): `acquire()` reports whether THIS
    /// instance is the current usage fetcher. Won first-come, held for life; a
    /// non-holder hydrates from the shared disk cache each tick. `Arc`-shared
    /// with the TUI's bootstrap so its startup switch one-shot runs only for the
    /// fetcher, and with the tick thread so the flock stays held for the process
    /// lifetime.
    fetch_lease: Arc<crate::daemon::FetchLease>,
    /// Whether the previous tick stood down — transition edges get one log
    /// line each way, never a per-tick repeat.
    standdown_active: AtomicBool,
    /// CDX-3 standby pacing: next full-scan stamp + per-profile transient
    /// retry widening. Tick-thread-only state; a plain leaf mutex acquired
    /// while holding nothing else (never nested — outside `lockorder` ranks).
    codex_standby: std::sync::Mutex<CodexStandbyPacing>,
    /// CDX-6 poll pacing: per-profile next-due stamps + error-dedup memos.
    /// Same leaf-mutex discipline as `codex_standby`.
    codex_poll: std::sync::Mutex<CodexPollPacing>,
    /// EXP-1/F1: profiles whose stored codex token the CDX-6 poll saw a 401
    /// on — the poll's evidence that the token is dead server-side even when
    /// its clock looks fine. Drained by the CDX-3 standby tick, which
    /// attempts an immediate guarded refresh instead of waiting out the
    /// age-based `standby_due` schedule (a parked chain would otherwise sit
    /// dead for up to 48h/7d while every surface shows it healthy). Same
    /// leaf-mutex discipline as the pacing fields above.
    codex_auth_kicks: std::sync::Mutex<HashSet<String>>,
    /// EXP-1/F2 re-feed pacing: next-scan stamp + per-profile retry widening
    /// for the fed-sidecar freshness scan. Same leaf-mutex discipline.
    claude_feed: std::sync::Mutex<ClaudeFeedPacing>,
}

/// Pacing state for the CDX-3 standby scan — cheap in-memory throttles so the
/// scan re-reads stored auth files at most every [`CODEX_STANDBY_SCAN_GAP_MS`]
/// and a transiently-failing profile widens to [`CODEX_STANDBY_RETRY_MS`]
/// instead of retrying every scan. Deliberately NOT persisted: the due
/// predicate itself (`standby_due`, driven by the stored file's own exp +
/// last_refresh) is the durable truth; losing pacing on restart costs one
/// extra scan.
#[derive(Default)]
pub(super) struct CodexStandbyPacing {
    next_scan_ms: u64,
    retry_after_ms: HashMap<String, u64>,
}

/// One scheduler tick: drain forced refetches, partition both legs, publish
/// countdowns, fan out fetches (OAuth + third-party) that republish each
/// profile's countdown as it lands, propagate rotated tokens, evaluate
/// auto-switch chain.
fn tick(state: &SchedulerState) {
    let interval_ms = state.refresh_interval.load(Ordering::Relaxed);

    // Single-fetcher lease (#27): exactly one instance (the daemon or a TUI)
    // fetches usage at a time. This tick tries to hold `usage-fetch.lock`;
    // another holder means we stand down and hydrate from the shared disk cache
    // instead of competing (double HTTP polling drains the per-account quota, a
    // doubled rotation races the single-use refresh chain, a doubled auto-switch
    // scan is the #27 thrash). The lease is retried each tick until won, then
    // held for life — so a waiter re-arms within one tick of the current holder
    // exiting (flock auto-released on process death). An unreadable lock stands
    // down too: an io error is never a licence to dup-fetch.
    if !state.fetch_lease.acquire() {
        if !state.standdown_active.swap(true, Ordering::Relaxed) {
            standdown_transition_log(
                "clauth: another instance holds the usage-fetch lease: standing \
                 down (rendering from the shared cache)",
            );
        }
        standdown_tick(state, interval_ms);
        return;
    }
    if state.standdown_active.swap(false, Ordering::Relaxed) {
        standdown_transition_log("clauth: acquired the usage-fetch lease: fetching");
    }

    // Names pushed by rotation or manual refresh — bypass cadence this tick.
    // Drained once and handed to both legs; a forced name only matches the leg
    // whose snapshot owns it, so neither starves the other.
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    // A manual refresh (forced) clears session suppression so the profile
    // retries once this tick. If it still yields no data it re-suppresses when
    // the outcome lands. Done before the snapshot so the name survives the
    // suppressed-name filter below.
    if !forced.is_empty()
        && let Ok(mut sup) = state.suppressed_generic.lock()
    {
        for name in &forced {
            sup.remove(name);
        }
    }

    // Snapshot both legs. A poisoned OAuth lock yields an empty snapshot rather
    // than an early `return` — that would starve the third-party leg and drop
    // its already-drained forced names.
    let oauth_snapshot: Vec<TokenEntry> =
        state.tokens.lock().map(|t| t.clone()).unwrap_or_default();
    let tp_snapshot: Vec<ThirdPartyEntry> = state
        .third_party_tokens
        .lock()
        .map(|t| t.clone())
        .unwrap_or_default();
    // Drop generic profiles suppressed this session (no-data on the timer) from
    // the third-party leg so they aren't re-fetched every cadence. Only a manual
    // refresh (forced, cleared above) re-admits one for a single retry.
    let tp_snapshot = filter_suppressed(&state.suppressed_generic, tp_snapshot);

    // Partition both before either fetches, then publish in one window so the
    // countdown map never shows a leg as momentarily missing (and a deleted
    // profile's stale key is dropped by the full replace).
    let now = now_ms();
    let (mut oauth_due, mut oauth_next) =
        partition_and_merge(&oauth_snapshot, &forced, state, now, interval_ms);
    // Config toggle (`refresh_spent_accounts`): when off, drop accounts already
    // pinned at their 100% window cap from this tick's OAuth fetch — a spent
    // window can't change until it resets, so re-polling only burns quota +
    // poll load. Forced (`r`) and never-fetched accounts are never dropped (a
    // reset is only observed by polling). Also blanks a dropped account's
    // countdown + clears its Queued spinner (no pending fetch). Fetch-leg only;
    // switch/fallback predicates are untouched. Default-on keeps stock behavior.
    let refresh_spent = state
        .config
        .lock()
        .map(|c| c.state.refresh_spent_accounts)
        .unwrap_or(true);
    if !refresh_spent {
        drop_spent_oauth(
            state,
            &oauth_snapshot,
            &mut oauth_due,
            &mut oauth_next,
            &forced,
        );
    }
    // Fire-once post-reset poll: once a 5h window has reset (plus grace), force a
    // single fetch so the overview drops the stale pre-reset reading instead of
    // holding it until the natural cadence slot. After `drop_spent_oauth` so a
    // lapsed spent window is re-polled here. Store/last_fetched/activity each read
    // once, sequentially; a poisoned store or last_fetched skips anchoring, a
    // poisoned activity fails safe to "exclude all" (matching `partition_due`).
    let post_reset_resets: HashMap<String, u64> = match state.store.lock() {
        Ok(store) => oauth_snapshot
            .iter()
            .filter_map(|e| Some((e.name.clone(), five_hour_reset_ms(store.get(&e.name)?)?)))
            .collect(),
        Err(_) => HashMap::new(),
    };
    if !post_reset_resets.is_empty()
        && let Ok(last_guard) = state.last_fetched.lock()
    {
        let last_fetched = last_guard.clone();
        drop(last_guard);
        let excluded: HashSet<String> = match state.activity.lock() {
            Ok(a) => a
                .iter()
                .filter(|(_, v)| {
                    matches!(v, ProfileActivity::Refreshing | ProfileActivity::Switching)
                })
                .map(|(n, _)| n.clone())
                .collect(),
            Err(_) => oauth_snapshot.iter().map(|e| e.name.clone()).collect(),
        };
        anchor_post_reset_oauth(
            &oauth_snapshot,
            &post_reset_resets,
            &last_fetched,
            &excluded,
            &mut oauth_due,
            &mut oauth_next,
            now,
        );
    }
    let (tp_due, tp_next) = partition_and_merge(&tp_snapshot, &forced, state, now, interval_ms);
    publish_countdowns(&state.next_refresh_per_profile, oauth_next, tp_next);

    // CDX-2: the passive codex leg — a bounded local read of codex's own
    // session logs for the ACTIVE codex profile, publishing through the same
    // cache/store/status channels as a fetch outcome. No HTTP, no rotation,
    // no streaks (a local read cannot 429). Lease-holder only, like the legs
    // above; the standdown side hydrates from the cache this writes.
    codex_passive_tick(
        &state.config,
        &state.store,
        &state.status,
        &state.last_fetched,
        &forced,
        interval_ms,
    );

    // CDX-3: standby keep-alive for parked codex chains (lease-holder only,
    // like the legs above). Self-paced — the scan gate + due predicate make
    // this a no-op on almost every tick.
    codex_standby_tick(
        &state.config,
        &state.activity,
        &state.codex_standby,
        &state.codex_auth_kicks,
        now_ms(),
        &crate::codex::oauth::refresh,
    );

    // CDX-6: active usage polling for EVERY codex profile with a stored
    // token — the leg that sees parked accounts (passive/proxy only see
    // accounts that run). Read-only by invariant; 60s cadence per profile
    // (codex CLI's own); lease-holder only, like the legs above.
    codex_poll_tick(
        &state.config,
        &state.store,
        &state.status,
        &state.codex_poll,
        &state.codex_auth_kicks,
        now_ms(),
        &|token, account_id| crate::codex::poll::fetch_wham_usage(token, account_id),
    );

    // EXP-1/F2: fed-sidecar freshness scan — renew a feed-enabled profile's
    // session bearer hours ahead of its clock death instead of relying on
    // rotation side effects (lease-holder only, like the legs above).
    claude_feed_tick(&state.config, &state.claude_feed, now_ms(), &|name| {
        crate::oauth::refeed_session_token(&state.config, name, crate::oauth::refresh_result)
    });

    // Names actually scheduled this tick across both legs. A forced name absent
    // from both (e.g. a profile whose creds were removed between the UI `r` and
    // this tick) was marked Queued by `enqueue_refetch` but no worker owns it, so
    // the orphan sweep at the tick's end clears it — otherwise its spinner freezes.
    let scheduled: HashSet<String> = oauth_due
        .iter()
        .map(|e| e.name.clone())
        .chain(tp_due.iter().map(|e| e.name.clone()))
        .collect();

    // Both legs fan out concurrently so the third-party leg no longer waits behind
    // the OAuth join loop. Per-host pacing (`await_request_slot`) keeps accounts on
    // the same endpoint serialized while distinct hosts (the Anthropic OAuth host vs
    // each api-key provider) run in parallel. The scope joins the third-party leg
    // before the post-fetch scans below, preserving their "both legs done" ordering.
    std::thread::scope(|s| {
        let tp = (!tp_due.is_empty())
            .then(move || s.spawn(move || fetch_third_party_due(state, tp_due)));
        if !oauth_due.is_empty() {
            fetch_oauth_due(state, oauth_due, interval_ms);
        }
        if let Some(h) = tp {
            // Worker panics are already swallowed inside `fetch_third_party_due`;
            // this join only reaps the leg thread itself.
            let _ = h.join();
        }
    });

    // Orphan sweep: a forced name no leg scheduled keeps a stale Queued mark.
    clear_orphaned_forced(&state.activity, &forced, &scheduled);

    // Auto-switch: evaluate every tick (not only OAuth fetch ticks) so a
    // profile that crossed its threshold is switched immediately, without
    // waiting for the next scheduled fetch. Also checks recovery post-switch-off.
    scan_auto_switch(
        &state.config,
        &state.store,
        &state.status,
        &state.third_party_status,
        &state.poll_streaks,
        &state.kick_blocks,
        &state.activity,
        &state.pending_switch,
        &state.pending_switch_off,
    );
    scan_recovery(
        &state.config,
        &state.store,
        &state.status,
        &state.third_party_status,
        &state.kick_blocks,
        &state.pending_switch,
    );

    // CDX-4: the codex chain scan — session-boundary auto-switch off the
    // passive signal. Harness-scoped queue gates keep it independent of the
    // claude scans above (§0.15).
    scan_codex_auto_switch(&state.config, &state.store, &state.pending_switch);
}

/// CDX-4 codex chain scan (PLAN.md §0.15): when the ACTIVE codex profile is
/// exhausted (percent shape or the limiter's own verdict), enqueue a
/// Scheduler-origin switch to the next viable codex chain member —
/// `drain_codex_switch` applies it with Refuse-foreign + backoff semantics.
/// The lease/loginless member facts are IO-derived, so they're filled AFTER
/// the config lock drops (the `kick_rejected` pattern).
pub(super) fn scan_codex_auto_switch(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    pending_switch: &PendingSwitch,
) {
    {
        // Skip while a CODEX decision is pending — a queued claude switch
        // must never wedge this scan (harness-scoped independence, §0.1).
        let Ok(p) = pending_switch.lock() else { return };
        if pending_for_harness(&p, crate::profile::Harness::Codex) {
            return;
        }
    }
    let snapshot = {
        let cfg = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        crate::fallback::snapshot_codex_chain(&cfg)
    };
    let Some(mut snapshot) = snapshot else {
        return;
    };
    for m in &snapshot.chain {
        if crate::runtime::has_live_codex_session(&m.name) {
            snapshot.leased.push(m.name.clone());
        }
        if !matches!(crate::codex::read_profile_auth(&m.name), Ok(Some(_))) {
            snapshot.loginless.push(m.name.clone());
        }
    }
    if let Some(name) = crate::fallback::next_codex_auto_switch_target(&snapshot, store)
        && let Ok(mut p) = pending_switch.lock()
    {
        enqueue_pending_switch(
            &mut p,
            name,
            crate::profile::Harness::Codex,
            Origin::Scheduler,
            now_ms(),
        );
    }
}

/// Log a stand-down / lease-acquired transition. Either the TUI or the daemon
/// can stand down now (whichever didn't win the lease). `logline!` routes the
/// daemon's line to `daemon.log` and an interactive TUI's to
/// `~/.clauth/clauth.log`, so it is recorded without ever painting over the
/// accounts pane.
fn standdown_transition_log(msg: &str) {
    logline!("{msg}");
}

/// CDX-2 passive codex leg. Reads the newest rate-limit snapshot from codex's
/// session logs and publishes it for the ACTIVE codex profile exactly like
/// `apply_outcome` publishes a fetch body: usage cache + store + status +
/// `last_fetched` cadence stamp — so TUI bars, status.json windows, and the
/// standdown hydrate light up with no extra plumbing. Inactive codex profiles
/// keep their last cached numbers (their logs stop moving the moment they
/// leave the live slot; `resets_at` lapse is the same display/walk logic the
/// claude side already applies).
///
/// Attribution (PLAN.md §0.8.2): a snapshot publishes only when its event
/// timestamp is not older than the live auth.json mtime — events from before
/// the last account change are never attributed to the current profile. The
/// snapshot is read BEFORE the mtime, so a concurrent switch can only reject
/// a valid event (conservative miss), never attribute a pre-switch one.
/// Known residual (accepted): an out-of-band `codex login` to a different
/// account can skew one interval's attribution until the daemon's follow
/// tick re-syncs the marker — inherent to identity-less session logs, and
/// self-healing within a tick.
pub(super) fn codex_passive_tick(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    forced: &HashSet<String>,
    interval_ms: u64,
) {
    let active = {
        let Ok(cfg) = config.lock() else { return };
        let Some(name) = cfg.state.active_codex_profile.as_deref() else {
            return;
        };
        // The marker names a claude/deleted profile only on hand-edited
        // state; treat that as "no active codex profile" rather than reading
        // logs on its behalf.
        if !cfg.find(name).is_some_and(|p| p.is_codex()) {
            return;
        }
        name.to_string()
    };

    // CDX-5 §1.7: while the proxy is actively serving, ITS per-account header
    // feed is the sole codex usage writer — the identity-less JSONL leg would
    // misattribute a post-rotation account's counters to the active profile.
    // Stand down until the heartbeat goes stale (proxy stopped).
    if crate::proxy::proxy_active(interval_ms) {
        return;
    }

    let now = EpochMs::from_millis(now_ms());
    let due = forced.contains(&active)
        || last_fetched
            .lock()
            .ok()
            .is_none_or(|lf| lf.get(&active).is_none_or(|t| now >= *t));
    if !due {
        return;
    }

    match crate::codex::usage::read_latest_snapshot() {
        crate::codex::usage::SnapshotOutcome::Snapshot(snap)
            if crate::codex::usage::attributable(
                snap.snapshot_at_ms,
                crate::codex::usage::live_auth_mtime_ms(),
            ) =>
        {
            write_profile_cache(&active, USAGE_CACHE_FILE, &snap.info);
            if let Ok(mut s) = store.lock() {
                s.insert(active.clone(), snap.info);
            }
            if let Ok(mut lf) = last_fetched.lock() {
                lf.insert(
                    active.clone(),
                    now.saturating_add(IntervalMs::from_millis(interval_ms)),
                );
                if let Ok(mut st) = status.lock() {
                    st.insert(active.clone(), FetchStatus::Fresh);
                }
            }
        }
        outcome => {
            // Missing/NoData/unattributable/error: keep whatever the cache
            // holds and retry next interval. Only a real IO error is worth a
            // line (a machine without codex sessions is a quiet state).
            if let crate::codex::usage::SnapshotOutcome::Error(e) = outcome {
                logline!("clauth: codex passive usage read failed: {e}");
            }
            if let Ok(mut lf) = last_fetched.lock() {
                lf.insert(
                    active.clone(),
                    now.saturating_add(IntervalMs::from_millis(interval_ms)),
                );
            }
        }
    }
}

/// CDX-6 per-profile poll cadence — codex CLI's own (~60s, openai/codex#10869),
/// per AX 2026-07-22 ("那我们也每分钟刷新"). Traffic-shaped like the official
/// client's own polling.
const CODEX_POLL_GAP_MS: u64 = 60 * 1000;
/// Widening after a failed poll: transient trouble retries in minutes, an
/// Unauthorized token waits for CDX-3's standby refresh (~scan cadence), and
/// a shape-drift error backs off hard — hammering a moved private API buys
/// nothing but log spam.
const CODEX_POLL_RETRY_MS: u64 = 5 * 60 * 1000;
const CODEX_POLL_UNAUTHORIZED_MS: u64 = 15 * 60 * 1000;
const CODEX_POLL_SHAPE_DRIFT_MS: u64 = 60 * 60 * 1000;
/// EXP-1/F1 circuit breaker: consecutive poll 401s (with no intervening poll
/// success) that may kick a forced standby refresh before the kick stands
/// down to the age schedule. Two attempts cover "genuinely revoked access
/// token" (first kick heals it) and "first kick raced a concurrent capture"
/// (second kick retries); beyond that the 401 evidently isn't curable by
/// rotation.
const CODEX_KICK_STREAK_MAX: u32 = 2;

/// Pacing for the CDX-6 poll: per-profile next-due stamps plus a last-error
/// memo so a persistently failing profile logs the SAME error once, not every
/// retry, plus the 401-kick streak for the F1 circuit breaker. In-memory only
/// — losing it on restart costs one immediate poll (and re-arms the breaker,
/// which is the right default for fresh evidence).
#[derive(Default)]
pub(super) struct CodexPollPacing {
    next_ms: HashMap<String, u64>,
    last_error: HashMap<String, String>,
    kick_streak: HashMap<String, u32>,
}

/// CDX-6 active poll leg. For each codex profile with a stored access token,
/// GET `wham/usage` with THAT profile's token — per-account exact attribution
/// (stronger than the identity-less JSONL leg), no live session required —
/// and publish through the same cache/store/status channels as every other
/// leg. Never refreshes anything: a clock-expired or rejected token stands
/// down and waits for CDX-3.
///
/// `poll_fn` is injected (production: `codex::poll::fetch_wham_usage`) so the
/// orchestration — toggle → candidates → due → publish/widen — is testable
/// offline; only the injected closure ever touches the network.
#[allow(
    clippy::type_complexity,
    reason = "injected poll seam, named at both call sites"
)]
pub(super) fn codex_poll_tick(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &StatusStore,
    pacing: &std::sync::Mutex<CodexPollPacing>,
    kicks: &std::sync::Mutex<HashSet<String>>,
    now: u64,
    poll_fn: &dyn Fn(
        &str,
        Option<&str>,
    ) -> std::result::Result<
        crate::codex::poll::PolledUsage,
        crate::codex::poll::PollError,
    >,
) {
    // Candidates under one short config-lock window; the polls themselves
    // must never hold the lock.
    let candidates: Vec<String> = {
        let Ok(cfg) = config.lock() else { return };
        if !cfg.state.codex_usage_poll {
            return;
        }
        cfg.profiles
            .iter()
            .filter(|p| p.is_codex())
            .map(|p| p.name.as_str().to_string())
            .collect()
    };
    for name in candidates {
        {
            let Ok(p) = pacing.lock() else { return };
            if p.next_ms.get(&name).is_some_and(|&t| now < t) {
                continue;
            }
        }
        let auth = match crate::codex::read_profile_auth(&name) {
            Ok(Some(bytes)) => match crate::codex::auth::CodexAuthFile::parse(&bytes) {
                Ok(a) => a,
                Err(_) => {
                    // Unparseable store — the capture/adopt paths own that
                    // problem; poll again at the normal cadence in case it heals.
                    defer_poll(pacing, &name, now + CODEX_POLL_RETRY_MS);
                    continue;
                }
            },
            // No codex login stored (or unreadable): nothing to poll with.
            _ => {
                defer_poll(pacing, &name, now + CODEX_POLL_RETRY_MS);
                continue;
            }
        };
        let Some(token) = auth.access_token().map(str::to_string) else {
            defer_poll(pacing, &name, now + CODEX_POLL_RETRY_MS);
            continue;
        };
        // A clock-expired token is a doomed request — stand down for CDX-3's
        // standby refresh instead of collecting a 401.
        if auth
            .access_token_exp_ms()
            .is_some_and(|exp| (now as i64) >= exp)
        {
            defer_poll(pacing, &name, now + CODEX_POLL_UNAUTHORIZED_MS);
            continue;
        }
        let account_id = auth.account_id();
        match poll_fn(&token, account_id.as_deref()) {
            Ok(polled) => {
                write_profile_cache(&name, USAGE_CACHE_FILE, &polled.info);
                // The live plan tier rides along — `tier_label` prefers it
                // over the stored id_token claim, which goes stale on a plan
                // change until codex re-mints. Written only on change (the
                // steady state must not churn a disk write per minute).
                if let Some(plan) = polled.plan_type
                    && crate::profile_cache::load_profile_cache::<String>(
                        &name,
                        crate::profile_cache::CODEX_PLAN_CACHE_FILE,
                    )
                    .as_deref()
                        != Some(plan.as_str())
                {
                    write_profile_cache(&name, crate::profile_cache::CODEX_PLAN_CACHE_FILE, &plan);
                }
                if let Ok(mut s) = store.lock() {
                    s.insert(name.clone(), polled.info);
                }
                if let Ok(mut st) = status.lock() {
                    st.insert(name.clone(), FetchStatus::Fresh);
                }
                if let Ok(mut p) = pacing.lock() {
                    p.last_error.remove(&name);
                    p.kick_streak.remove(&name);
                    p.next_ms.insert(name, now + CODEX_POLL_GAP_MS);
                }
            }
            Err(e) => {
                let (delay, msg) = match &e {
                    crate::codex::poll::PollError::Unauthorized => {
                        // EXP-1/F1: the server just proved this token dead —
                        // kick the standby leg for a guarded refresh on its
                        // next tick instead of waiting out the age schedule.
                        // Circuit breaker (review MED): wham/usage shares
                        // only the bearer with the coding API, so a
                        // wham-specific auth quirk could 401 forever while
                        // rotations keep succeeding — after
                        // [`CODEX_KICK_STREAK_MAX`] consecutive 401s without
                        // an intervening poll success, stop kicking (the age
                        // schedule resumes ownership) rather than rotating a
                        // healthy chain every poll interval indefinitely. A
                        // genuinely dead chain exits the loop on its own:
                        // the kicked refresh 400s permanent → auth_broken →
                        // out of the candidate set.
                        let streak = pacing
                            .lock()
                            .map(|mut p| {
                                let s = p.kick_streak.entry(name.clone()).or_insert(0);
                                *s += 1;
                                *s
                            })
                            .unwrap_or(u32::MAX);
                        if streak <= CODEX_KICK_STREAK_MAX {
                            if let Ok(mut k) = kicks.lock() {
                                k.insert(name.clone());
                            }
                            (
                                CODEX_POLL_UNAUTHORIZED_MS,
                                format!(
                                    "'{name}' wham/usage rejected the stored token — kicking a standby refresh"
                                ),
                            )
                        } else {
                            (
                                CODEX_POLL_UNAUTHORIZED_MS,
                                format!(
                                    "'{name}' wham/usage still rejects the token after {CODEX_KICK_STREAK_MAX} kicked refreshes — standing down to the age schedule"
                                ),
                            )
                        }
                    }
                    crate::codex::poll::PollError::RateLimited => (
                        CODEX_POLL_RETRY_MS,
                        format!("'{name}' wham/usage rate-limited"),
                    ),
                    crate::codex::poll::PollError::Other(detail) => {
                        let drift =
                            detail.contains("unrecognized") || detail.contains("no rate_limit");
                        (
                            if drift {
                                CODEX_POLL_SHAPE_DRIFT_MS
                            } else {
                                CODEX_POLL_RETRY_MS
                            },
                            format!("'{name}' codex usage poll failed: {detail}"),
                        )
                    }
                };
                if let Ok(mut p) = pacing.lock() {
                    // Same error as last time = already logged; new text = loud once.
                    if p.last_error.get(&name).map(String::as_str) != Some(msg.as_str()) {
                        logline!("clauth: {msg}");
                        p.last_error.insert(name.clone(), msg);
                    }
                    p.next_ms.insert(name, now + delay);
                }
            }
        }
    }
}

fn defer_poll(pacing: &std::sync::Mutex<CodexPollPacing>, name: &str, until: u64) {
    if let Ok(mut p) = pacing.lock() {
        p.next_ms.insert(name.to_string(), until);
    }
}

/// How often the CDX-3 standby scan re-reads stored codex auth files. The
/// real cadence is the due predicate (`standby_due` — ~weekly per profile);
/// this only bounds the file re-reads between events.
const CODEX_STANDBY_SCAN_GAP_MS: u64 = 10 * 60 * 1000;
/// Widening after a transient refresh failure — retry in hours, not per scan
/// (the deadline pressure is days away; hammering a flaky endpoint buys
/// nothing).
const CODEX_STANDBY_RETRY_MS: u64 = 6 * 60 * 60 * 1000;

/// CDX-3 standby keep-alive: refresh parked codex chains clauth exclusively
/// holds, so an inactive profile's refresh token never ages out server-side
/// (PLAN.md §0.9–0.12). Runs on the lease-holder tick only, like every other
/// leg. Sequential by design — standby refreshes are ~weekly per profile, so
/// fan-out buys nothing and one-at-a-time keeps the guard story simple.
///
/// `refresh_fn` is injected (production: `crate::codex::oauth::refresh`) so
/// the orchestration — candidates → due → guard → re-check → spend → apply —
/// is testable offline; only the injected closure ever touches the network.
pub(super) fn codex_standby_tick(
    config: &crate::profile::ConfigHandle,
    activity: &ActivityStore,
    pacing: &std::sync::Mutex<CodexStandbyPacing>,
    kicks: &std::sync::Mutex<HashSet<String>>,
    now: u64,
    refresh_fn: &dyn Fn(
        &str,
    ) -> std::result::Result<
        crate::codex::oauth::CodexRefreshResponse,
        crate::codex::oauth::CodexRefreshError,
    >,
) {
    // EXP-1/F1: process usage-poll 401 kicks ahead of the scan gate — a kick
    // lands on the tick after the poll saw the 401 (this leg runs before the
    // poll leg within a tick), far faster than the age schedule. A kick
    // carries server-side proof the stored token is dead, so it bypasses both
    // the scan cadence and `standby_due` (forced refresh) — but keeps every
    // other discipline: candidates membership (parked, clauth-held), the
    // RotationGuard, and the apply-time chain-identity re-check. It also
    // bypasses the transient-retry widening: a 401 is fresh, stronger
    // evidence than whatever aged the widening in, and the poll's own
    // 15-minute unauthorized cadence (plus its kick-streak breaker)
    // rate-limits re-kicks. The pacing gate is checked FIRST so a poisoned
    // pacing mutex returns with the kick set untouched — drained kicks are
    // never dropped unprocessed.
    let scan_due = {
        let Ok(mut p) = pacing.lock() else { return };
        if now < p.next_scan_ms {
            false
        } else {
            p.next_scan_ms = now + CODEX_STANDBY_SCAN_GAP_MS;
            true
        }
    };
    let kicked: Vec<String> = kicks
        .lock()
        .map(|mut k| std::mem::take(&mut *k).into_iter().collect())
        .unwrap_or_default();
    if kicked.is_empty() && !scan_due {
        return;
    }
    let candidates = {
        let Ok(cfg) = config.lock() else { return };
        crate::actions::codex_standby_candidates(&cfg)
    };
    for name in &kicked {
        if !candidates.iter().any(|(n, _)| n == name) {
            // Not a parked clauth-held chain (live owner, leased, logged out,
            // or auth_broken) — the kick is moot; codex or a re-login owns it.
            continue;
        }
        match codex_refresh_parked(config, name, Some(activity), refresh_fn, true) {
            CodexStandbyOutcome::Refreshed => {
                logline!("clauth: revived codex profile '{name}' after a usage-poll 401");
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms.remove(name);
                }
            }
            CodexStandbyOutcome::Skipped => {}
            CodexStandbyOutcome::Permanent => {}
            CodexStandbyOutcome::Transient(e) => {
                logline!(
                    "clauth: kicked codex standby refresh for '{name}' failed (the poll re-kicks): {e}"
                );
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms
                        .insert(name.clone(), now + CODEX_STANDBY_RETRY_MS);
                }
            }
        }
    }
    if !scan_due {
        return;
    }
    for (name, bytes) in candidates {
        if kicked.contains(&name) {
            // Just force-refreshed (or deliberately skipped) above.
            continue;
        }
        let widened = pacing
            .lock()
            .ok()
            .and_then(|p| p.retry_after_ms.get(&name).copied())
            .is_some_and(|at| now < at);
        if widened {
            continue;
        }
        let due = crate::codex::CodexAuthFile::parse(&bytes)
            .is_ok_and(|a| crate::codex::oauth::standby_due(&a, now));
        if !due {
            continue;
        }
        match codex_standby_refresh_one(config, activity, &name, refresh_fn) {
            CodexStandbyOutcome::Refreshed => {
                logline!("clauth: standby-refreshed codex profile '{name}'");
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms.remove(&name);
                }
            }
            CodexStandbyOutcome::Skipped => {}
            CodexStandbyOutcome::Permanent => {
                // mark_auth_broken already logged the transition; broken
                // profiles drop out of the candidate set until re-login.
            }
            CodexStandbyOutcome::Transient(e) => {
                logline!("clauth: codex standby refresh for '{name}' failed (will retry): {e}");
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms
                        .insert(name.clone(), now + CODEX_STANDBY_RETRY_MS);
                }
            }
        }
    }
}

/// EXP-1/F2 cadence for the fed-sidecar freshness scan. The due predicate is
/// stateless against the wall clock ([`crate::oauth::fed_sidecar_refeed_due`]),
/// so a machine-sleep gap self-corrects on the first tick after wake — no
/// monotonic bookkeeping needed.
const FEED_SCAN_GAP_MS: u64 = 5 * 60 * 1000;
/// Widening after a transient re-feed failure (network trouble, a busy
/// rotation lock) — the horizon is hours wide, so minutes-scale retries lose
/// nothing while avoiding per-scan log spam.
const FEED_RETRY_MS: u64 = 15 * 60 * 1000;
/// A Broken verdict (dead chain, no mint to degrade to) only changes via
/// re-login — which re-arms the feed anyway — so retry on a long leash.
const FEED_BROKEN_RETRY_MS: u64 = 6 * 60 * 60 * 1000;

/// Pacing for the EXP-1/F2 re-feed scan — same in-memory throttle shape as
/// [`CodexStandbyPacing`]; the durable truth is the sidecar's own expiry.
#[derive(Default)]
pub(super) struct ClaudeFeedPacing {
    next_scan_ms: u64,
    retry_after_ms: HashMap<String, u64>,
}

/// EXP-1/F2: fed-sidecar freshness scan. For every feed-enabled claude
/// profile whose armed sidecar is inside the re-feed horizon of its clock
/// death, run the full feed decision table (no-spend re-stamp / guarded
/// refresh / mint degrade) NOW instead of waiting for a rotation side effect
/// — the RC-C death was exactly a fed bearer expiring under a running session
/// while rotations sat parked (spent-window poll parking, daemon idle,
/// machine sleep). Lease-holder tick only, like every other leg.
///
/// Deliberately scans ALL feed profiles, not just the active one: a parked
/// profile's sessions may still be running on its fed bearer (sessions
/// survive switches by design), and a fresh sidecar makes the next switch-in
/// instant. The extra rotation pressure is nil — claude usage chains already
/// rotate on the ~8h access-token cadence for usage polling, and the daemon
/// is the single writer for parked chains either way.
///
/// `gate_fn` is injected (production: [`crate::oauth::refeed_session_token`])
/// so the orchestration — candidates → due → pace/widen — is testable
/// offline; only the injected closure ever touches locks or the network.
pub(super) fn claude_feed_tick(
    config: &crate::profile::ConfigHandle,
    pacing: &std::sync::Mutex<ClaudeFeedPacing>,
    now: u64,
    gate_fn: &dyn Fn(&str) -> crate::oauth::AuthGate,
) {
    {
        let Ok(mut p) = pacing.lock() else { return };
        if now < p.next_scan_ms {
            return;
        }
        p.next_scan_ms = now + FEED_SCAN_GAP_MS;
    }
    let candidates: Vec<String> = {
        let Ok(cfg) = config.lock() else { return };
        cfg.profiles
            .iter()
            .filter(|p| p.session_feed)
            .map(|p| p.name.as_str().to_string())
            .collect()
    };
    for name in candidates {
        let widened = pacing
            .lock()
            .ok()
            .and_then(|p| p.retry_after_ms.get(&name).copied())
            .is_some_and(|at| now < at);
        if widened {
            continue;
        }
        if !crate::oauth::fed_sidecar_refeed_due(&name, now as i64) {
            continue;
        }
        match gate_fn(&name) {
            // Ready = re-stamped no-spend or degraded to a serving fallback;
            // Refreshed = the rotation hook fed (and, active, mirrored). Both
            // logged at their source. A Ready that left the sidecar STILL due
            // is the degrade leg masking transient chain trouble behind a
            // live mint/bearer (review LOW) — pace it like a transient
            // instead of re-running the gate every scan.
            crate::oauth::AuthGate::Ready | crate::oauth::AuthGate::Refreshed => {
                if let Ok(mut p) = pacing.lock() {
                    if crate::oauth::fed_sidecar_refeed_due(&name, now as i64) {
                        p.retry_after_ms.insert(name.clone(), now + FEED_RETRY_MS);
                    } else {
                        p.retry_after_ms.remove(&name);
                    }
                }
            }
            crate::oauth::AuthGate::Transient(e) => {
                logline!("clauth: re-feed for '{name}' failed (will retry): {e:#}");
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms.insert(name.clone(), now + FEED_RETRY_MS);
                }
            }
            crate::oauth::AuthGate::Broken => {
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms
                        .insert(name.clone(), now + FEED_BROKEN_RETRY_MS);
                }
            }
        }
    }
}

pub(crate) enum CodexStandbyOutcome {
    Refreshed,
    /// The in-guard re-check disqualified the profile (became live owner /
    /// leased / logged out / no longer due) or the apply-time chain check
    /// found the store moved under us — nothing spent or nothing persisted,
    /// both quiet.
    Skipped,
    Permanent,
    Transient(anyhow::Error),
}

/// Daemon standby refresh: the UI-spinner wrapper around the shared guarded
/// core. Marks the row `Refreshing` for the HTTP window, then delegates.
fn codex_standby_refresh_one(
    config: &crate::profile::ConfigHandle,
    activity: &ActivityStore,
    name: &str,
    refresh_fn: &dyn Fn(
        &str,
    ) -> std::result::Result<
        crate::codex::oauth::CodexRefreshResponse,
        crate::codex::oauth::CodexRefreshError,
    >,
) -> CodexStandbyOutcome {
    codex_refresh_parked(config, name, Some(activity), refresh_fn, false)
}

/// Refresh ONE parked codex chain clauth exclusively holds, holding the
/// per-profile `RotationGuard` across the whole spend window (the claude
/// `rotate_one_inner` discipline). THE single-writer entry point — both the
/// daemon standby scan and the CDX-5 proxy call this, so a parked chain is
/// never double-spent (the review-confirmed CRIT was a second, guardless
/// implementation in the proxy; there is now exactly one).
///
/// Discipline (each step load-bearing):
/// 1. **Adopt-back first** (`gc_one_codex_runtime`): a SIGKILLed isolated
///    `clauth start` session may have left a chain in its `codex-home` up to
///    60 s fresher than the store; without adopting it back first we would
///    read the store's STALE token and spend it — but codex already rotated
///    past it in the isolated home, so the server would reuse-kill the chain
///    (review MED). Adopting first makes the store the freshest truth, so
///    `standby_due` then reads a fresh token and skips.
/// 2. **Re-derive eligibility INSIDE the guard** (`codex_standby_candidates` +
///    `standby_due`): a switch/capture/adopt that landed since the caller's
///    snapshot flips the answer; grab the CURRENT token, not a pre-guard one.
/// 3. Spend via `refresh_fn`.
/// 4. **Re-check the stored chain before persisting**: if its refresh token is
///    no longer the one we spent (a concurrent capture/adopt-back installed a
///    fresh independent chain during the HTTP window), discard — the store's
///    newer chain is the truth, ours is a dead branch (review CRIT + MED).
///
/// `force` (EXP-1/F1) skips ONLY the in-guard `standby_due` check — for the
/// usage-poll 401 kick, where the server already proved the access token dead
/// while its clock still looks comfortable. Candidates membership and the
/// chain-identity re-check are never skipped.
pub(crate) fn codex_refresh_parked(
    config: &crate::profile::ConfigHandle,
    name: &str,
    activity: Option<&ActivityStore>,
    refresh_fn: &dyn Fn(
        &str,
    ) -> std::result::Result<
        crate::codex::oauth::CodexRefreshResponse,
        crate::codex::oauth::CodexRefreshError,
    >,
    force: bool,
) -> CodexStandbyOutcome {
    // (1) Adopt back a crashed isolated session's fresher chain BEFORE the
    // guard, so the in-guard read below sees the freshest truth.
    crate::runtime::gc_one_codex_runtime_public(name);

    let Ok(_rotation_guard) = crate::runtime::RotationGuard::acquire(name) else {
        return CodexStandbyOutcome::Transient(anyhow::anyhow!(
            "failed to acquire the rotation lock"
        ));
    };
    // (2) In-guard re-check + token grab. Config mutex (400) then state flock
    // (500) — the rotate_one_inner order; both dropped before HTTP.
    let spent: Option<String> = {
        let Ok(cfg) = config.lock() else {
            return CodexStandbyOutcome::Skipped;
        };
        crate::lock::with_state_lock(|| {
            let still = crate::actions::codex_standby_candidates(&cfg)
                .into_iter()
                .find(|(n, _)| n == name);
            let Some((_, bytes)) = still else {
                return Ok::<_, anyhow::Error>(None);
            };
            let auth = crate::codex::CodexAuthFile::parse(&bytes)?;
            if !force && !crate::codex::oauth::standby_due(&auth, crate::usage::now_ms()) {
                return Ok(None);
            }
            let rt = auth.refresh_token().map(str::to_string);
            if rt.is_some()
                && let Some(activity) = activity
            {
                mark_activity(activity, name, ProfileActivity::Refreshing);
            }
            Ok(rt)
        })
        .ok()
        .flatten()
    };
    let Some(spent) = spent else {
        return CodexStandbyOutcome::Skipped;
    };

    // (3) Spend.
    let outcome = refresh_fn(&spent);

    let result = match outcome {
        Ok(resp) => {
            // (4) Re-read + token identity check before writing.
            let applied = crate::lock::with_state_lock(|| {
                let Some(bytes) = crate::codex::read_profile_auth(name)? else {
                    return Ok::<_, anyhow::Error>(false); // logged out mid-flight
                };
                let auth = crate::codex::CodexAuthFile::parse(&bytes)?;
                if auth.refresh_token() != Some(spent.as_str()) {
                    return Ok(false); // chain moved under us — discard
                }
                let now_iso = crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs());
                let updated = crate::codex::oauth::apply_refresh(&bytes, &resp, &now_iso)?;
                crate::codex::write_profile_auth(name, &updated)?;
                Ok(true)
            });
            match applied {
                Ok(true) => CodexStandbyOutcome::Refreshed,
                Ok(false) => CodexStandbyOutcome::Skipped,
                Err(e) => CodexStandbyOutcome::Transient(e),
            }
        }
        Err(crate::codex::oauth::CodexRefreshError::Permanent(msg)) => {
            logline!("clauth: codex standby refresh for '{name}' rejected permanently: {msg}");
            crate::oauth::mark_auth_broken(config, name, true);
            CodexStandbyOutcome::Permanent
        }
        Err(crate::codex::oauth::CodexRefreshError::Transient(e)) => {
            CodexStandbyOutcome::Transient(e)
        }
    };
    if let Some(activity) = activity {
        clear_activity(activity, name);
    }
    result
}

/// One scheduler tick while a live daemon owns the loop (R3). The daemon
/// fetches, rotates, and decides switches; this side only re-reads its work
/// product so the UI stays current:
///   * re-seed the usage / third-party stores from the disk caches the daemon
///     keeps fresh ([`try_seed_cache`] stamps status Fresh/Cached off the cache
///     mtime, and `last_fetched` AT the mtime — so the countdowns below track
///     the daemon's real cadence);
///   * republish countdowns from those stamps (partition is reused for its
///     timing math only; the due list is deliberately discarded — nothing
///     fetches here);
///   * drain forced names (a manual `r`) and clear their Queued marks — the
///     daemon can't be asked to fetch early from here, and a stranded mark
///     would freeze the row's spinner;
///   * skip rotation and both auto-switch scans entirely.
fn standdown_tick(state: &SchedulerState, interval_ms: u64) {
    let forced: HashSet<String> = state
        .refetch_queue
        .lock()
        .ok()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();

    let oauth_snapshot: Vec<TokenEntry> =
        state.tokens.lock().map(|t| t.clone()).unwrap_or_default();
    let tp_snapshot: Vec<ThirdPartyEntry> = state
        .third_party_tokens
        .lock()
        .map(|t| t.clone())
        .unwrap_or_default();
    // Seed the DISPLAY superset (disabled included) so a stood-down TUI shows a
    // disabled account's cached numbers; the work-list stays `oauth_snapshot`.
    let oauth_seed_names = state
        .config
        .lock()
        .map(|cfg| collect_oauth_seed_names(&cfg))
        .unwrap_or_default();

    hydrate_from_daemon_caches(
        &state.store,
        &state.status,
        &state.third_party_usage_store,
        &state.third_party_status,
        &state.last_fetched,
        &oauth_seed_names,
        &tp_snapshot,
        interval_ms,
    );
    // Mirror the fetching instance's kick blocks (write-through cache files) so
    // a stood-down TUI still shows the blocked pill for an outage it can't see.
    let oauth_names: Vec<String> = oauth_snapshot.iter().map(|e| e.name.clone()).collect();
    sync_kick_blocks_from_cache(&state.kick_blocks, &oauth_names);

    let now = now_ms();
    let streaks = streak_snapshot(&state.poll_streaks);
    let (_, mut oauth_next) = partition_due(
        &oauth_snapshot,
        now,
        &state.last_fetched,
        &state.activity,
        interval_ms,
        &streaks,
    );
    // Mirror the fetch tick's `refresh_spent_accounts` OFF handling: the daemon
    // skips spent accounts, so their disk cache stops advancing and the derived
    // countdown would freeze at `0s`. Blank it here too (the Queued sweep below
    // already clears any stranded spinner) so a stood-down TUI shows a spent row
    // the same as an armed one.
    let refresh_spent = state
        .config
        .lock()
        .map(|c| c.state.refresh_spent_accounts)
        .unwrap_or(true);
    if !refresh_spent && let Ok(store) = state.store.lock() {
        let skip = spent_skip_set(&oauth_snapshot, &forced, &store, now_epoch_secs());
        oauth_next.retain(|name, _| !skip.contains(name));
    }
    let (_, tp_next) = partition_due(
        &tp_snapshot,
        now,
        &state.last_fetched,
        &state.activity,
        interval_ms,
        &streaks,
    );
    publish_countdowns(&state.next_refresh_per_profile, oauth_next, tp_next);

    clear_orphaned_forced(&state.activity, &forced, &HashSet::new());
    // With no worker running, EVERY Queued mark is an orphan — not only forced
    // ones. The bootstrap pre-marks cache-due profiles Queued so the first
    // paint shows a spinner instead of a stale countdown, expecting the first
    // tick's worker to take over and clear it; standing down, nothing ever
    // does (observed 2026-07-12: a perpetual spinner where the daemon-fed
    // timer belongs). Fetching/Refreshing/Switching stay — a worker from the
    // last armed tick may genuinely still be in flight and clears itself.
    if let Ok(mut a) = state.activity.lock() {
        a.retain(|_, act| !matches!(act, ProfileActivity::Queued));
    }
}

/// The store-refresh half of [`standdown_tick`], extracted store-narrow so the
/// hydrate contract is testable without a full `SchedulerState`: every profile
/// with an on-disk cache lands in its store with a freshness-derived status and
/// `last_fetched` stamped at the cache mtime; cacheless profiles are left
/// untouched (the daemon will publish them shortly).
#[allow(clippy::too_many_arguments)]
fn hydrate_from_daemon_caches(
    store: &UsageStore,
    status: &StatusStore,
    tp_store: &ThirdPartyUsageStore,
    tp_status: &ThirdPartyStatusStore,
    last_fetched: &LastFetchedAt,
    oauth_seed_names: &[String],
    third_party: &[ThirdPartyEntry],
    interval_ms: u64,
) {
    let now = now_ms();
    for name in oauth_seed_names {
        try_seed_cache(store, status, last_fetched, name, now, interval_ms);
    }
    bootstrap_third_party(tp_store, tp_status, last_fetched, third_party, interval_ms);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_refresher(
    config: crate::profile::ConfigHandle,
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    poll_streaks: PollStreaks,
    kick_blocks: KickBlocks,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    suppressed_generic: SuppressedGenericStore,
    shutting_down: Arc<AtomicBool>,
    fetch_lease: Arc<crate::daemon::FetchLease>,
) {
    // Seed kick blocks from the per-profile cache files on the CALLING thread
    // so a restart mid-outage resumes the decayed retry clock instead of
    // hammering. Must happen here, not inside the spawned closure below:
    // nothing joins that thread, so a home-derived path resolved on it could
    // outlive a test's `HOME_OVERRIDE` and read the operator's real home. See
    // the 2026-06-06 convention in docs/internals.md.
    let names: Vec<String> = tokens
        .lock()
        .map(|t| t.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default();
    sync_kick_blocks_from_cache(&kick_blocks, &names);

    let state = SchedulerState {
        config,
        tokens,
        store,
        status,
        refresh_interval,
        next_refresh_per_profile,
        activity,
        last_fetched,
        poll_streaks,
        kick_blocks,
        pending_switch,
        pending_switch_off,
        refetch_queue,
        third_party_tokens,
        third_party_usage_store,
        third_party_status,
        suppressed_generic,
        shutting_down,
        fetch_lease,
        standdown_active: AtomicBool::new(false),
        codex_standby: std::sync::Mutex::new(CodexStandbyPacing::default()),
        codex_poll: std::sync::Mutex::new(CodexPollPacing::default()),
        codex_auth_kicks: std::sync::Mutex::new(HashSet::new()),
        claude_feed: std::sync::Mutex::new(ClaudeFeedPacing::default()),
    };
    // Same test-skip rationale as the status/tokens/pricing workers in
    // `tui/app.rs`: a detached tick thread is never joined, so it could run
    // `tick()` — which itself resolves home-derived paths and makes network
    // calls — after a test's `HOME_OVERRIDE` sandbox has already unwound.
    if cfg!(test) {
        return;
    }
    #[allow(clippy::expect_used, reason = "thread spawn failure is unrecoverable")]
    std::thread::Builder::new()
        .name("clauth-tick".into())
        .spawn(move || {
            loop {
                if state.shutting_down.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(TICK_INTERVAL);
                if state.shutting_down.load(Ordering::SeqCst) {
                    break;
                }
                tick(&state);
            }
        })
        .expect("failed to spawn scheduler tick thread");
}

/// Evaluate the fallback chain and queue an auto-switch target.
///
/// Snapshots the chain under `config` mutex (dropped before taking `usage_store`).
/// This split is load-bearing: `App::apply_usage` takes `usage_store` then `config`,
/// so the scheduler must never hold `config` while taking `usage_store`.
/// A profile's store entry is trustworthy for an auto-switch / recovery decision
/// only when its last fetch was live (`Fresh`). A `Cached` entry may be a 5h
/// window that has since rolled over (its stale-high utilization would drive a
/// false switch-away) and a `RateLimited` one may be the synthetic just-kicked
/// 0% placeholder (which would never switch away, or switch toward a spent
/// account) — the startup one-shot gates on `Fresh` for the same reason.
fn decision_fresh<R: crate::lockorder::Rank>(
    status: &Arc<RankedMutex<HashMap<String, FetchStatus>, R>>,
    name: &str,
) -> bool {
    matches!(
        status.lock().ok().and_then(|m| m.get(name).copied()),
        Some(FetchStatus::Fresh)
    )
}

/// [`decision_fresh`] against EITHER store — the OAuth `StatusStore` or the
/// third-party one. `Profile.fetch_status` (the UI twin's freshness input) is
/// filled from both in `App::apply_usage`, so the scheduler twin must read both
/// too or a fresh third-party member looks stale to it alone, inverting the
/// fresh-preference on a mixed OAuth+third-party chain (2026-07-17). Consulted
/// by both `scan_auto_switch` (the walk's fresh-preference) and `scan_recovery`
/// (the relink gate) so both stay in lockstep with the UI twin.
fn decision_fresh_any(
    status: &StatusStore,
    third_party_status: &ThirdPartyStatusStore,
    name: &str,
) -> bool {
    decision_fresh(status, name) || decision_fresh(third_party_status, name)
}

/// True when `name`'s last reading is a **deep-slot stuck** `RateLimited`: the
/// status is `RateLimited` AND its consecutive-429 streak has passed
/// [`ACTIVE_CAP_MAX_STREAK`] — the boundary where the active cap stops holding
/// retries frequent. Past it, a still-`RateLimited` read is genuinely stuck (the
/// `/usage` throttle window never drained), not a transient blip.
///
/// ONE predicate, two consumers, so display and decision cannot drift:
///   * `scan_auto_switch` distrusts a stuck-RateLimited active — it bypasses the
///     [`decision_fresh`] gate exactly like an auth-broken active (AUTH-4) so the
///     walk can rotate away instead of wedging on an account that can never
///     return `Fresh`. The switch still requires the walk's own last-known
///     exhaustion gate ([`crate::fallback::next_auto_switch_target`]), so a
///     throttle artifact with real headroom stays put — only a genuinely spent
///     stuck active moves.
///   * `status.json`'s per-profile `stale` flag publishes the same judgment so a
///     menu-bar reader renders the reading as distrusted, not current truth.
pub(crate) fn is_stuck_rate_limited(status: FetchStatus, streak: u32) -> bool {
    matches!(status, FetchStatus::RateLimited) && is_stuck_streak(streak)
}

/// Whether a consecutive-failure streak has run deeper than the active row's
/// retry cap ([`ACTIVE_CAP_MAX_STREAK`]) — the point past which whatever we are
/// waiting out is not draining on its own. One home for the boundary, so the
/// daemon's `stale` judgment and the row's red pill can't drift apart.
pub(crate) fn is_stuck_streak(streak: u32) -> bool {
    streak > ACTIVE_CAP_MAX_STREAK
}

#[allow(clippy::too_many_arguments)]
fn scan_auto_switch(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &StatusStore,
    third_party_status: &ThirdPartyStatusStore,
    streaks: &PollStreaks,
    kick_blocks: &KickBlocks,
    _activity: &ActivityStore,
    pending_switch: &PendingSwitch,
    pending_switch_off: &PendingSwitchOff,
) {
    // Skip when a previous CLAUDE decision is still pending — harness-scoped
    // (CDX-4 §0.15): a queued codex rotation must not wedge this scan. Each
    // lock is acquired and dropped before the next — never two leaf mutexes
    // at once.
    {
        let Ok(p) = pending_switch.lock() else { return };
        if pending_for_harness(&p, crate::profile::Harness::Claude) {
            return;
        }
    }
    {
        // Pending switch-off not yet applied — skip until UI drains it.
        let Ok(off) = pending_switch_off.lock() else {
            return;
        };
        if *off {
            return;
        }
    }
    // Snapshot under `config` only — drop guard before taking `usage_store`.
    let snapshot = {
        let cfg = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        crate::fallback::snapshot_chain(&cfg)
    };
    let Some(mut snapshot) = snapshot else {
        return;
    };
    // Not config state, so `snapshot_chain` can't fill it: the walk skips
    // switch-grade kick-rejected members and a rejected ACTIVE bypasses the
    // exhaustion gate (its usage reads idle while inference is refused).
    snapshot.kick_rejected = kick_rejected_names(kick_blocks, now_epoch_secs());
    // Same reason: freshness lives in the status stores, not in config, and
    // `Profile.fetch_status` (what the UI twin reads) is written only by the UI
    // thread. Unions BOTH stores (OAuth + third-party) via `decision_fresh_any`,
    // exactly as the UI twin's `apply_usage` fills `fetch_status`, so a fresh
    // third-party member isn't invisible to this walk alone. A PREFERENCE for
    // the headroom walk, never a gate — see `ChainSnapshot::fresh`.
    snapshot.fresh = snapshot
        .chain
        .iter()
        .filter(|m| decision_fresh_any(status, third_party_status, &m.name))
        .map(|m| m.name.clone())
        .collect();

    // Only act on a confirmed-live read of the active profile — a stale or
    // synthetic store entry would drive a false switch (see `decision_fresh`).
    // TWO exceptions bypass the freshness gate, both because the active can
    // never come back Fresh on its own so requiring one wedges the scan on it:
    //   * an auth-broken active (AUTH-4) — its login is dead (observed
    //     2026-07-09); the walk never consults its usage.
    //   * a deep-slot stuck RateLimited active (the RateLimited analogue) — the
    //     `/usage` throttle stayed pinned past the active cap, so no Fresh read
    //     is coming. Unlike auth-broken, this one still faces the walk's
    //     last-known exhaustion gate, so a throttle artifact with headroom stays
    //     put and only a genuinely spent stuck active rotates away.
    let active_broken = snapshot.broken.iter().any(|b| b == &snapshot.active);
    let active_status = status
        .lock()
        .ok()
        .and_then(|m| m.get(&snapshot.active).copied());
    let active_stuck_rl = active_status
        .is_some_and(|s| is_stuck_rate_limited(s, rate_limit_streak(streaks, &snapshot.active)));
    if !active_broken && !active_stuck_rl && !matches!(active_status, Some(FetchStatus::Fresh)) {
        return;
    }

    match crate::fallback::next_auto_switch_target(&snapshot, store) {
        Some(crate::fallback::SwitchAction::To(name)) => {
            if let Ok(mut p) = pending_switch.lock() {
                enqueue_pending_switch(
                    &mut p,
                    name,
                    crate::profile::Harness::Claude,
                    Origin::Scheduler,
                    now_ms(),
                );
            }
        }
        Some(crate::fallback::SwitchAction::Off) => {
            if let Ok(mut off) = pending_switch_off.lock() {
                *off = true;
            }
        }
        None => {}
    }
}

/// Evaluate recovery after switch-off-all: when no active profile is set,
/// scan the fallback chain for any member whose utilization has dropped
/// below its threshold and queue a switch to the first one found.
///
/// Lock-safe: acquires `config` (rank 400) then drops before `store` (300)
/// and `pending_switch` (1500) — never two tracked locks at once.
fn scan_recovery(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &StatusStore,
    third_party_status: &ThirdPartyStatusStore,
    kick_blocks: &KickBlocks,
    pending_switch: &PendingSwitch,
) {
    // Skip when a previous CLAUDE switch is still pending (harness-scoped).
    if let Ok(p) = pending_switch.lock()
        && pending_for_harness(&p, crate::profile::Harness::Claude)
    {
        return;
    }

    // Build chain-member snapshot under config lock, then drop before
    // touching store (avoids the config↔store inversion that
    // `next_auto_switch_target` avoids via ChainSnapshot).
    let members: Vec<crate::fallback::ChainMember> = {
        let cfg = match config.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let weekly_pct = cfg.state.weekly_switch_threshold_pct();
        // Only scan for recovery after switch-off-all (no active profile).
        if cfg.state.active_profile.is_some() {
            return;
        }
        if cfg.state.fallback_chain.is_empty() {
            return;
        }
        cfg.state
            .fallback_chain
            .iter()
            // A disabled or auth-broken member is not a recovery target. Shares
            // `fallback::walk_excluded` with `next_target`/`fully_clear_target`
            // so the skip list can't drift; canceled is caught store-side inside
            // `find_recovered_member` (this walk's `Profile.usage` is stale
            // headless, so the config-plan `is_canceled` the selection walks use
            // would read empty here).
            .filter(|name| !crate::fallback::walk_excluded(&cfg, name))
            .map(|name| {
                let profile = cfg.find(name);
                crate::fallback::ChainMember {
                    name: name.to_string(),
                    threshold: profile
                        .map(crate::fallback::threshold_for)
                        .unwrap_or(crate::fallback::DEFAULT_THRESHOLD),
                    last_resort: profile.is_some_and(|p| p.last_resort),
                    weekly_line: profile
                        .map(|p| crate::fallback::member_weekly_line(p, weekly_pct))
                        .unwrap_or(weekly_pct),
                    scoped_line: profile
                        .map(|p| crate::fallback::member_scoped_line(p, weekly_pct))
                        .unwrap_or(weekly_pct),
                    check_scoped: profile.is_none_or(|p| p.check_scoped),
                    max_spend: profile.and_then(|p| p.max_auto_spend).unwrap_or(0.0),
                }
            })
            .collect()
    };

    // Relink only to a member with a confirmed-live read in EITHER store; a
    // synthetic/stale 0% entry would relink to an unverified placeholder (see
    // `decision_fresh`). Third-party-fresh members count too, so a third-party
    // fallback is recoverable after switch-off-all instead of frozen out.
    let members: Vec<crate::fallback::ChainMember> = members
        .into_iter()
        .filter(|m| decision_fresh_any(status, third_party_status, &m.name))
        .collect();

    // A switch-grade kick-rejected member is not "recovered" — its idle-looking
    // usage is exactly what the messages-limiter rejection freezes it in.
    let kick_rejected = kick_rejected_names(kick_blocks, now_epoch_secs());
    if let Some(name) = crate::fallback::find_recovered_member(&members, store, &kick_rejected)
        && let Ok(mut p) = pending_switch.lock()
    {
        enqueue_pending_switch(
            &mut p,
            name,
            crate::profile::Harness::Claude,
            Origin::Scheduler,
            now_ms(),
        );
    }
}

/// Split `snapshot` into the due set and a per-profile next-fetch map.
///
/// Poisoned `last_fetched` returns empty rather than `last=0` (which would mark
/// all profiles due — fetch storm). Profiles currently `Switching` or `Refreshing`
/// are excluded to avoid racing the switch worker on `TokenList` or `rotate_one_inner`
/// on the single-use refresh token. Poisoned activity mutex fails safe to excluded.
/// A quarantined entry's deadline widens by its `poll_backoff_ms` — read from the
/// snapshot each partition, so the widening vanishes the tick the flag lifts.
fn partition_due<T: NamedEntry + Clone>(
    snapshot: &[T],
    now: u64,
    last_fetched: &LastFetchedAt,
    activity: &ActivityStore,
    interval_ms: u64,
    streaks: &HashMap<String, StreakCounts>,
) -> (Vec<T>, HashMap<String, u64>) {
    let now = EpochMs::from_millis(now);
    let Ok(lf) = last_fetched.lock() else {
        return (Vec::new(), HashMap::new());
    };
    let act = activity.lock();

    let interval = IntervalMs::from_millis(interval_ms);
    let mut due = Vec::new();
    let mut per_profile = HashMap::with_capacity(snapshot.len());
    for entry in snapshot {
        let last = lf
            .get(entry.name())
            .copied()
            .unwrap_or(EpochMs::from_millis(0));
        let backoff = entry.poll_backoff_ms(streaks.get(entry.name()).copied().unwrap_or_default());
        let next = last
            .saturating_add(interval)
            .saturating_add(IntervalMs::from_millis(backoff));
        per_profile.insert(entry.name().to_string(), next.as_millis());
        let excluded = match act.as_ref() {
            Ok(a) => matches!(
                a.get(entry.name()),
                Some(ProfileActivity::Refreshing | ProfileActivity::Switching)
            ),
            Err(_) => true, // Poisoned: fail safe to excluded.
        };
        if excluded {
            continue;
        }
        if now >= next {
            due.push(entry.clone());
        }
    }
    (due, per_profile)
}

/// Merge forced (cadence-bypassing) entries into `due`. Skips profiles that are
/// `Refreshing`/`Switching` — `rotate_one_inner` or the switch gate owns the
/// activity slot — and entries already due.
fn merge_forced<T: NamedEntry + Clone>(
    snapshot: &[T],
    forced: &HashSet<String>,
    due: &mut Vec<T>,
    per_profile_next: &mut HashMap<String, u64>,
    activity: &ActivityStore,
    now: u64,
) {
    if forced.is_empty() {
        return;
    }
    let switching: HashSet<String> = match activity.lock() {
        Ok(a) => a
            .iter()
            .filter(|(_, v)| matches!(v, ProfileActivity::Refreshing | ProfileActivity::Switching))
            .map(|(n, _)| n.clone())
            .collect(),
        Err(_) => snapshot.iter().map(|e| e.name().to_string()).collect(),
    };
    let mut extras: Vec<T> = Vec::with_capacity(forced.len());
    for entry in snapshot.iter().filter(|e| {
        forced.contains(e.name())
            && !switching.contains(e.name())
            && !due.iter().any(|d| d.name() == e.name())
    }) {
        per_profile_next.insert(entry.name().to_string(), now);
        extras.push(entry.clone());
    }
    due.extend(extras);
}

/// Apply `refresh_spent_accounts` OFF to this tick: drop spent accounts from the
/// due set, blank their published countdown, and clear any bootstrap `Queued`
/// mark. A skipped account has no pending fetch, so a countdown frozen at `0s`
/// (its `last_fetched + interval` is already past — that's why it was due) and a
/// `Queued` spinner that no worker will ever clear are both stale UI. The
/// overview timer renders blank and the usage tab reads "up to date"/"spent"
/// instead. Reads the usage store once. Fetch-leg only; switch/fallback
/// predicates are untouched.
fn drop_spent_oauth(
    state: &SchedulerState,
    snapshot: &[TokenEntry],
    due: &mut Vec<TokenEntry>,
    next: &mut HashMap<String, u64>,
    forced: &HashSet<String>,
) {
    let now_secs = now_epoch_secs();
    let skip = {
        let Ok(store) = state.store.lock() else {
            return; // can't read usage → fail safe to polling everything
        };
        spent_skip_set(snapshot, forced, &store, now_secs)
    };
    if skip.is_empty() {
        return;
    }
    due.retain(|entry| !skip.contains(&entry.name));
    next.retain(|name, _| !skip.contains(name));
    // Clear a stranded bootstrap `Queued` mark so the row stops spinning on a
    // fetch that never runs. `Fetching`/`Refreshing`/`Switching` are worker-owned
    // and left alone — one may still be in flight and clears itself on landing.
    if let Ok(mut act) = state.activity.lock() {
        for name in &skip {
            if matches!(act.get(name), Some(ProfileActivity::Queued)) {
                act.remove(name);
            }
        }
    }
}

/// Names `refresh_spent_accounts` OFF skips this tick: an unforced, already-
/// fetched account whose windows are maxed (spent). A forced (`r`) name, a
/// never-fetched one (no store entry — a reset is only seen by polling), and a
/// below-cap or lapsed one are all absent (they still poll). Pure over the store
/// map so it tests without a full `SchedulerState`.
fn spent_skip_set(
    snapshot: &[TokenEntry],
    forced: &HashSet<String>,
    store: &HashMap<String, UsageInfo>,
    now_secs: i64,
) -> HashSet<String> {
    snapshot
        .iter()
        .filter(|entry| {
            !forced.contains(&entry.name)
                && store
                    .get(&entry.name)
                    .is_some_and(|info| windows_maxed(info, now_secs))
        })
        .map(|entry| entry.name.clone())
        .collect()
}

/// Epoch-ms of a profile's 5h window reset, when it carries a parseable
/// `resets_at`. `None` for a windowless or unparseable snapshot.
fn five_hour_reset_ms(info: &UsageInfo) -> Option<u64> {
    let secs = info
        .five_hour
        .as_ref()?
        .resets_at
        .as_deref()
        .and_then(iso_to_epoch_secs)?;
    u64::try_from(secs).ok().and_then(|s| s.checked_mul(1000))
}

/// Fire-once post-reset predicate: `true` when this profile's latest fetch
/// predates its 5h window reset (`last_fetched_ms < resets_at_ms`, so the
/// utilization we hold is the stale pre-reset reading) AND the reset has since
/// passed with `grace_ms` to spare. Self-limiting: the forced fetch stamps
/// `last_fetched` past the reset, flipping `last < resets` false until the NEXT
/// reset, so it never doubles the natural cadence. `grace_ms` covers `/usage`
/// briefly lagging the reset instant.
fn should_anchor_fetch(
    resets_at_ms: Option<u64>,
    last_fetched_ms: u64,
    now_ms: u64,
    grace_ms: u64,
) -> bool {
    resets_at_ms.is_some_and(|r| last_fetched_ms < r && now_ms >= r.saturating_add(grace_ms))
}

/// Force a single post-reset poll for every OAuth profile whose 5h window reset
/// has passed since its last fetch (see [`should_anchor_fetch`]), so the overview
/// leaves the stale pre-reset reading instead of holding it until the natural
/// cadence slot. Skips `Refreshing`/`Switching` (`excluded`, the same set
/// [`partition_due`] drops) and any name already `due`. A scheduled name's
/// countdown is stamped to `now_ms` (fetching now), mirroring [`merge_forced`].
/// Decomposed args (no [`SchedulerState`]) so the due decision tests directly.
fn anchor_post_reset_oauth(
    snapshot: &[TokenEntry],
    resets: &HashMap<String, u64>,
    last_fetched: &HashMap<String, EpochMs>,
    excluded: &HashSet<String>,
    due: &mut Vec<TokenEntry>,
    next: &mut HashMap<String, u64>,
    now_ms: u64,
) {
    for entry in snapshot {
        if excluded.contains(&entry.name) || due.iter().any(|d| d.name == entry.name) {
            continue;
        }
        let last = last_fetched.get(&entry.name).map_or(0, |e| e.as_millis());
        if should_anchor_fetch(
            resets.get(&entry.name).copied(),
            last,
            now_ms,
            RESET_ANCHOR_GRACE_MS,
        ) {
            next.insert(entry.name.clone(), now_ms);
            due.push(entry.clone());
        }
    }
}

/// Clear any forced name that no leg scheduled this tick — its profile vanished
/// from both snapshots between the UI `r` and now, leaving a `Queued` mark that no
/// worker owns and would otherwise spin forever. `Refreshing`/`Switching` names
/// are owned by a rotate / switch-gate worker, so they are left in place.
fn clear_orphaned_forced(
    activity: &ActivityStore,
    forced: &HashSet<String>,
    scheduled: &HashSet<String>,
) {
    if forced.is_empty() {
        return;
    }
    if let Ok(mut a) = activity.lock() {
        for name in forced {
            if !scheduled.contains(name)
                && !matches!(
                    a.get(name),
                    Some(ProfileActivity::Refreshing | ProfileActivity::Switching)
                )
            {
                a.remove(name);
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/scheduler.rs"]
mod tests;
