use std::collections::{HashMap, HashSet};
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
use crate::profile::ProfileName;
use crate::profile_cache::{
    KICK_BLOCK_CACHE_FILE, THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache,
    profile_cache_mtime_ms, remove_profile_cache, write_profile_cache,
};
use serde::{Deserialize, Serialize};

/// Scheduler wake interval. Network work only fires for profiles whose cadence has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How often the fetch-lease holder re-trims the usage-history logs to their
/// retention window. Coarse on purpose: the trim is a full read + rewrite per
/// profile, and the window it enforces is measured in days.
const HISTORY_PRUNE_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;

/// Hard ceiling on a server-provided `retry-after` so a bogus huge value
/// can't starve a profile's refresh slot. Also the ceiling on the widen-only
/// poll backoff `partition_due` adds on top of the interval, which is what
/// bounds the longest gap a live scheduler can leave between two cache writes
/// (`profile_json::MAX_LIVE_REFRESH_GAP_MS` reads it for exactly that).
pub(crate) const MAX_RETRY_AFTER_MS: u64 = 15 * 60 * 1000;

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

/// Profiles owed one weekly-reset re-test kick: their fresh usage body just
/// showed the aggregate 7d window roll over while it had been pinned at the
/// hard cap (see [`weekly_reset_pending`]). In-memory only — the mark rides
/// one tick, and the standing re-test leg (`has_block`) still covers the
/// account's recovery if the process dies between the rollover and the kick.
/// Leaf like [`KickBlocks`].
pub(crate) type WeeklyResetKicks = Arc<RankedMutex<HashSet<ProfileName>, rank::WeeklyResetKicks>>;

/// Names pushed here after a successful token rotation bypass the cadence on the next tick.
pub(crate) type RefetchQueue = Arc<RankedMutex<HashSet<String>, rank::RefetchQueue>>;

/// Auto-switch targets posted by the scheduler when the active profile crosses its threshold.
/// Set (not Vec) so duplicate enqueues collapse. Drained by `on_tick`, which dispatches a switch worker.
pub(crate) type PendingSwitch = Arc<RankedMutex<HashSet<String>, rank::PendingSwitch>>;

/// Set true when wrap-off mode finds the entire chain exhausted (no sink below 100%).
/// Drained by `on_tick` to turn off all accounts. Bool because switch-off is a global act.
pub(crate) type PendingSwitchOff = Arc<RankedMutex<bool, rank::PendingSwitchOff>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: ProfileName,
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
    /// Elected by [`tick`] before the fan-out: this profile is the one queue
    /// member allowed to OPEN a 5h window this tick (`usage::auto_start_queue`). Decided
    /// centrally because [`fetch_oauth_due_with`] runs one worker per profile,
    /// and two workers reading the queue anchor in the same tick would both kick.
    ///
    /// `true` from [`collect_tokens`], which builds the snapshot rather than the
    /// work-list: a caller with no queue (the single-shot paths) must not have
    /// its kick silently suppressed. Only `tick` narrows it.
    pub(crate) may_open_window: bool,
}

/// Snapshot of one third-party profile identity used by the refresher.
#[derive(Clone)]
pub(crate) struct ThirdPartyEntry {
    pub(crate) name: ProfileName,
    pub(crate) target: crate::providers::ThirdPartyTarget,
    /// Empty for a provider whose usage surface doesn't read one (Alibaba,
    /// whose quota runs on the console session in `target`).
    pub(crate) api_key: String,
}

impl ThirdPartyEntry {
    /// Fingerprint of the credential this entry would fetch with.
    ///
    /// Session suppression is keyed on it, which is what makes a re-login
    /// observable to a HEADLESS daemon: the daemon inserts nothing into
    /// `refetch_queue` (only the TUI's manual refresh does), so a suppressed
    /// name recorded by name alone stayed suppressed until the process
    /// restarted — even after `daemon::tick::rebuild_tokens` had already
    /// rebuilt this entry from the reloaded config with the new credential.
    /// Comparing fingerprints re-admits it the moment the credential on disk
    /// differs from the one the suppression was recorded under, and never on a
    /// schedule.
    ///
    /// A hash rather than the value, so no second copy of a live secret exists —
    /// which matters because this value is also PERSISTED, as the key of the
    /// dead-credential record the daemonless surfaces read
    /// (`profile_cache::THIRD_PARTY_AUTH_FILE`). Persistence is why it is
    /// SHA-256 over an explicit encoding rather than `DefaultHasher` over
    /// `Hash`: neither of those is stable across toolchain versions, and a
    /// fingerprint that silently changed under a rebuild would retire every
    /// record on disk. Changing this encoding has the same effect, so treat it
    /// as a format. A collision costs one profile one extra suppressed cadence.
    pub(crate) fn credential_fingerprint(&self) -> u64 {
        use sha2::{Digest as _, Sha256};
        /// Length-delimited so no two field splits can collide (`ab|c` vs `a|bc`).
        fn field(hasher: &mut Sha256, bytes: &[u8]) {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        let mut hasher = Sha256::new();
        field(&mut hasher, self.api_key.as_bytes());
        match &self.target {
            crate::providers::ThirdPartyTarget::Known { provider, console } => {
                field(&mut hasher, b"known");
                // A literal per variant, NOT `display_name`: that is user-facing
                // copy and may be reworded, which would silently reset every
                // recorded fingerprint.
                field(
                    &mut hasher,
                    match provider {
                        crate::providers::Provider::DeepSeek => b"deepseek".as_slice(),
                        crate::providers::Provider::Zai => b"zai".as_slice(),
                        crate::providers::Provider::Alibaba => b"alibaba".as_slice(),
                        crate::providers::Provider::OpenRouter => b"openrouter".as_slice(),
                    },
                );
                match console {
                    Some(c) => {
                        field(&mut hasher, b"console");
                        field(&mut hasher, c.token.as_bytes());
                        field(&mut hasher, c.site.as_str().as_bytes());
                        field(&mut hasher, c.region.as_bytes());
                    }
                    None => field(&mut hasher, b"no-console"),
                }
            }
            crate::providers::ThirdPartyTarget::Generic { base_url } => {
                field(&mut hasher, b"generic");
                field(&mut hasher, base_url.as_bytes());
            }
        }
        let digest = hasher.finalize();
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        u64::from_le_bytes(head)
    }
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
/// Session-scoped (in-memory) map of third-party profiles suppressed from the
/// timer, each recorded against the credential fingerprint it failed under
/// ([`ThirdPartyEntry::credential_fingerprint`]). Never persisted — clears when
/// the process exits.
///
/// Two admissions, and the type name records only the first: a GENERIC profile
/// whose last fetch yielded no data, and ANY profile whose usage credential is
/// dead ([`FetchStatus::AuthExpired`]: a dead api key or a dead console
/// session, either of which any third-party profile can hit).
/// 429s are never added; they keep the server-directed deferral instead.
///
/// Cleared by a manual refresh (the TUI's `refetch_queue`) OR by the credential
/// changing on disk, which is the only clearing path a headless daemon has.
/// A leftover row whose fingerprint no longer matches anything is inert: it
/// filters nothing and the next suppression for that name overwrites it.
pub(crate) type SuppressedGenericStore =
    Arc<RankedMutex<HashMap<String, u64>, rank::SuppressedGeneric>>;

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
pub(crate) fn mark_activity(store: &ActivityStore, name: &ProfileName, activity: ProfileActivity) {
    if let Ok(mut g) = store.lock() {
        if matches!(activity, ProfileActivity::Idle) {
            g.remove(name.as_str());
        } else {
            g.insert(name.to_string(), activity);
        }
    }
}

/// Drop a profile to `Idle` (removes the entry; absent == `Idle`).
pub(crate) fn clear_activity(store: &ActivityStore, name: &ProfileName) {
    if let Ok(mut g) = store.lock() {
        g.remove(name.as_str());
    }
}

/// True iff the profile has no in-flight op. Poisoned mutex fails safe to "busy".
pub(crate) fn is_idle(store: &ActivityStore, name: &ProfileName) -> bool {
    match store.lock() {
        Ok(g) => !g.contains_key(name.as_str()),
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
    /// The provider's usage credential is dead or absent and no refresh path
    /// exists — only an operator re-login clears it, so the profile is
    /// session-suppressed rather than re-polled, and re-admitted when the
    /// credential on disk changes. Third-party only, with two producers: a 401
    /// on any api-key fetch (a dead key), and Alibaba's console session, whose
    /// window is set by the operator's browser sign-in and cannot be extended
    /// from here. The OAuth leg has its own `auth_broken` quarantine for the
    /// analogous state.
    AuthExpired,
}

/// Rotated (access, refresh) pair from an in-fetch rotation. Propagated back into
/// `TokenList` so the next tick doesn't re-401 with the stale token and double-burn the chain.
pub(crate) type RotatedTokens = (String, Option<String>);

/// Load disk cache as `(Some, status)` or `(None, Failed)` for the rotation bail-out path.
fn load_cached_with_status(
    name: &ProfileName,
    status: FetchStatus,
) -> (Option<UsageInfo>, FetchStatus) {
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
fn fresher_disk_pair(name: &ProfileName, spent_refresh: &str) -> Option<RotatedTokens> {
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
    name: &ProfileName,
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
/// refresh (unwritable rotation lock, live session, missing refresh token,
/// failed refresh).
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

/// Floor (ms) for [`rotate_lead_ms`], and the term that actually carries the
/// margin at the shipped cadence: `3 × 90 s` is 4.5 min, which LOSES to Claude
/// Code's own 5-minute refresh threshold every time. 15 min is 3× that
/// threshold and still leaves ~10 min of slack for a daemon restart or a run
/// of missed polls.
const ROTATE_LEAD_FLOOR_MS: i64 = 900_000;

/// How early the poller rotates a profile ahead of its clock expiry, with the
/// `preemptive_rotation` toggle on (the default).
///
/// Claude Code refreshes its own OAuth token once it is within **5 minutes**
/// of expiry — one predicate gates its whole demand path, measured against its
/// shipped bundle. Rotating outside that window
/// means CC never has a reason to refresh, so clauth's stored pair stays the
/// live one instead of lagging a chain CC advanced. Three poll intervals give
/// multiple rotation opportunities before expiry whatever the cadence, and the
/// floor is what clears CC's threshold at the shipped 90 s rate.
///
/// Correctness still does not depend on winning that race: when CC refreshes
/// first — clauth downtime, a lost race — the poller ADOPTS CC's fresher pair
/// rather than fighting for the chain (`oauth::try_adopt_live_rotation`).
/// Losing is not free, though. Anthropic does not punish the double-spend — the
/// pair the winner minted keeps working — but
/// clauth answers the `invalid_grant` its own loser gets with a LOCAL
/// quarantine (`mark_auth_broken`), and only an adopt, a carry, or a
/// `clauth login` lifts that. Rotating early is what keeps the chain off that
/// path, not what makes the path harmless.
fn rotate_lead_ms(interval_ms: u64) -> i64 {
    ((interval_ms as i64).saturating_mul(3)).max(ROTATE_LEAD_FLOOR_MS)
}

/// Whether this poll should rotate ahead of expiry instead of waiting for a
/// 401: the `preemptive_rotation` toggle (`enabled`, on by default) and the
/// stored expiry sitting inside the lead window. Nothing else — a live session
/// and a bare `claude` both read the very credential file a rotation writes,
/// so neither is a reason to hold off. An unknown expiry never rotates
/// proactively (never spend a single-use refresh on a token whose expiry we
/// can't prove). That rule is unconditional: the CHAIN's own stored expiry is
/// what gates this leg, and the CLA-ROLL flag only widens WHEN a rotation is
/// due, never WHETHER a provable expiry is required first.
fn proactive_rotation_due(
    enabled: bool,
    rolling: bool,
    access_expires_at: Option<i64>,
    now_ms: i64,
    interval_ms: u64,
) -> bool {
    // CLA-ROLL: a rolling profile forces the preemptive leg regardless of the
    // global toggle, because here the rotation is not only about the chain —
    // its persist re-stamps the session token, and a stale sidecar has a live
    // claude reading it.
    (enabled || rolling)
        && access_expires_at.is_some_and(|exp| now_ms + rotate_lead_ms(interval_ms) >= exp)
}

/// Backing store for [`memoized_identity`], keyed by the SHA-256 of the access
/// token rather than the token: the map outlives every call now, and the tokens
/// reaching it include ones nothing else in the process retains (a superseded
/// pair, a foreign account's live mirror), so keeping the bytes would be a new
/// retention of key material for no gain. A digest collision is the only way a
/// hit can be wrong, which is the property SHA-256 sells.
static IDENTITY_MEMO: std::sync::LazyLock<
    RankedMutex<HashMap<[u8; 32], crate::profile::AccountId>, rank::IdentityMemo>,
> = std::sync::LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// SHA-256 of an access token, the shared key for both the positive memo and
/// the stored-token probe suppression in `oauth.rs`. The digest (never the
/// bytes) is what the maps retain.
pub(crate) fn identity_key(access_token: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(access_token.as_bytes()).into()
}

/// Test-only: forget every resolved identity, so one test's fake token cannot
/// answer another's probe out of a map that now outlives both. Wired into
/// `testutil::EndpointSandbox` alongside `reset_request_slots`.
#[cfg(test)]
pub(crate) fn reset_identity_memo() {
    if let Ok(mut m) = IDENTITY_MEMO.lock() {
        m.clear();
    }
}

/// Wrap an identity probe so each access token is resolved at most once per
/// PROCESS. An access token's account uuid is immutable, so a memo hit is exact
/// rather than merely fresh — which is what makes caching safe for a check whose
/// whole job is proving two tokens belong to the same account, and why this is a
/// memo rather than a TTL on the probe.
///
/// Per-call scope was enough while the adopt below was gated to macOS. It is not
/// now: a live mirror that is permanently fresher yet permanently FOREIGN (a
/// manual CC `/login` into another account) fails the identity gate on every
/// rotation leg forever, and a per-call memo re-spends a `/profile` request
/// against the rate-limited Anthropic host each time. Process lifetime prices
/// those probes by rotation events — one per token ever seen — instead of by leg
/// count.
///
/// ONLY a `Some` is cached. A `None` means the probe failed (network, 401, shape
/// drift), and the adopt retry after a failed refresh exists precisely because the
/// live mirror may have surfaced a fresh pair since the first attempt — memoizing
/// the failure would quietly turn that second adopt into a no-op.
fn memoized_identity<'a>(
    probe: &'a dyn Fn(&str) -> Option<crate::profile::AccountId>,
) -> impl Fn(&str) -> Option<crate::profile::AccountId> + 'a {
    move |tok: &str| {
        let key = identity_key(tok);
        // Released before the probe: it reserves the per-host request slot
        // (`UsageThrottle`), which ranks OUTSIDE this leaf.
        if let Some(hit) = IDENTITY_MEMO.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return Some(hit);
        }
        let uuid = probe(tok)?;
        if let Ok(mut m) = IDENTITY_MEMO.lock() {
            m.insert(key, uuid.clone());
        }
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
/// A second exception to "rotate only on a rejected token": with the
/// `preemptive_rotation` toggle on (the default), a profile rotates ahead of
/// expiry (see [`rotate_lead_ms`]) so the running `claude` reads a pair clauth
/// already refreshed rather than refreshing it itself.
fn fetch_with_rotation(
    config: &crate::profile::ConfigHandle,
    entry: &TokenEntry,
    prev_plan: Option<PlanInfo>,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
) -> FetchOutcome {
    let name = &entry.name;
    let access_token = entry.access_token.as_str();
    let refresh_token = entry.refresh_token.as_deref();
    // Rotation coherence (#1): read the active flag, stored expiry, and the
    // preemptive toggle in one short config-lock window; the poll itself must
    // never hold the lock. Config — not the `TokenEntry` snapshot — is the
    // expiry source: a kick that rotated earlier in this tick has already
    // persisted the fresh expiry there, while the entry still carries the
    // pre-kick one, and a stale past expiry here would read as clock-expired
    // and re-spend the just-minted single-use pair.
    let (is_active, access_expires_at, interval_ms, preemptive, rolling_token) = config
        .lock()
        .map(|c| {
            (
                c.is_active(name),
                c.find(name).and_then(|p| p.access_token_expires_at()),
                c.state.refresh_interval_ms,
                c.state.preemptive_rotation,
                c.find(name).is_some_and(|p| p.rolling_token),
            )
        })
        // Poisoned-lock fallback. The `None` expiry alone already forces the
        // lazy path, so the other four are unreachable rather than chosen —
        // they still mirror the shipped defaults so a future reader isn't told
        // preemptive rotation is off by default. `rolling_token` falls back to
        // OFF: the rolling-token axis only ever ADDS a rotation, so the safe unreachable
        // value is the one that adds none.
        .unwrap_or((
            false,
            None,
            crate::profile::DEFAULT_REFRESH_INTERVAL_MS,
            true,
            false,
        ));
    let proactive = proactive_rotation_due(
        preemptive,
        rolling_token,
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

    // Both adopts below resolve the same two tokens (ours + the live mirror's),
    // and every earlier leg's answers are still in the memo — an account uuid is
    // a property of the token, so `/profile` is asked once per token ever seen.
    let identity = memoized_identity(&|tok| crate::usage::fetch_account_uuid(tok));

    // Adopt before spending: when the ACTIVE profile's live file mirror already
    // holds a FRESHER same-account pair, the running claude rotated first —
    // adopt its pair (identity-guarded) instead of burning OUR single-use
    // refresh token against a family it just superseded.
    // Queue a refetch so the next tick polls with the adopted token; disk
    // cache serves this tick.
    // Ceiling: the adopt reaches exactly as far as `claude::read_claude_credentials`
    // does — the GLOBAL `~/.claude/.credentials.json`. A `LinkMode::Fake` host
    // (Windows without symlink privilege) keeps its second chain in the SESSION's
    // runtime copy, a different file, so that one is still unrecovered here.
    // Reaching it needs a profile → live-session lookup the scheduler does not
    // have; tracked on its own.
    if is_active
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
    // macOS only: clauth can't write the Keychain item this session's CC reads,
    // so rotating would sign it out (`runtime::rotation_blocked_by_live_session`).
    if crate::runtime::rotation_blocked_for(name) {
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
            // A failed refresh on the ACTIVE profile usually means the live
            // claude rotated first and revoked our copy — one more adopt
            // attempt (its mirror may have JUST surfaced the fresh pair)
            // before falling back. This same path re-runs every poll, so a
            // lagging store self-heals as soon as the mirror catches up (at
            // latest CC's next launch).
            if is_active
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
                // above is the identity-guarded fast path for the ACTIVE
                // profile's live mirror, on every platform since the
                // `keychain_live()` term went; this re-read catches every
                // other racer (CC writing THROUGH an intact symlink, a
                // sibling clauth process). See `carry_external_rotation`.
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
    // The refresh already spent the old single-use token, so this pair is now the
    // only usable one — carry it back even when the persist below fails, or the
    // caller's live snapshot keeps the dead token and 400s every tick until a
    // restart adopts the staged sidecar (`auto_start_kick` carries its pair back
    // for the same reason).
    let rotated: Option<RotatedTokens> = Some((access.clone(), Some(tok.refresh_token.clone())));
    if crate::oauth::apply_rotated_tokens_locked(config, name, tok).is_err() {
        return bail_to_cache(rotated);
    }
    // A successful refresh + persist clears any prior auth-broken quarantine
    // (mirrors `ensure_installable`); a no-op when the flag was already clear.
    crate::oauth::mark_auth_broken(config, name, false);
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
    name: ProfileName,
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
    /// has been observed to keep 429ing `/usage`). Overlaid onto the cached snapshot by
    /// [`apply_outcome`] so only the tier advances — windows stay cached — and
    /// persisted so the flip survives the next tick. `Some` only on the ~hourly
    /// tick `/profile` is actually re-pulled, never per masked tick.
    plan_override: Option<PlanInfo>,
}

impl FetchOutcome {
    /// A live API body — overwrites the store and disk cache.
    fn live(name: &ProfileName, info: UsageInfo, rotated: Option<RotatedTokens>) -> Self {
        Self {
            name: name.clone(),
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
        name: &ProfileName,
        status: FetchStatus,
        rotated: Option<RotatedTokens>,
        retry_after: Option<Duration>,
    ) -> Self {
        let (info, status) = load_cached_with_status(name, status);
        Self {
            name: name.clone(),
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
fn window_lapsed(store: &UsageStore, name: &ProfileName, now_secs: i64) -> bool {
    let Ok(s) = store.lock() else {
        return false;
    };
    let Some(info) = s.get(name.as_str()) else {
        return false;
    };
    !five_hour_live(info, now_secs)
}

/// A fresh body shows the aggregate 7d window rolled over from the hard cap
/// while the 5h window is live: the account was dead on weekly quota and is
/// fresh again, so one re-test kick is owed ([`should_open_window`]'s pending
/// arm). Pure — the caller owns the store reads and the flag write.
///
/// The HARD cap ([`crate::fallback::WEEKLY_HARD_BLOCK_PCT`]) is the gate, not
/// the soft switch line: below the cap the messages endpoint still serves, so
/// a kick re-tests nothing, and the recovery scans own the soft-line return.
/// The 5h liveness gate keeps the LAPSED case on the lapsed leg — there the
/// kick OPENS a window and belongs behind the queue's spacing.
fn weekly_reset_pending(prev: &UsageInfo, info: &UsageInfo, now_secs: i64) -> bool {
    let Some(prev_week) = prev
        .seven_day
        .as_ref()
        .filter(|w| w.utilization >= crate::fallback::WEEKLY_HARD_BLOCK_PCT)
    else {
        return false;
    };
    let (Some(prev_reset), Some(new_reset)) = (
        prev_week.resets_at.as_deref().and_then(iso_to_epoch_secs),
        info.seven_day
            .as_ref()
            .and_then(|w| w.resets_at.as_deref())
            .and_then(iso_to_epoch_secs),
    ) else {
        return false;
    };
    new_reset > prev_reset && five_hour_live(info, now_secs)
}

/// Current consecutive-429 streak for `name` (0 when absent or poisoned). Read
/// alone and released before any higher-ranked lock — POLL_STREAK(220)
/// sits below USAGE_STORE(300), so it must not be held across `window_lapsed`.
fn rate_limit_streak(streaks: &PollStreaks, name: &ProfileName) -> u32 {
    streaks
        .lock()
        .ok()
        .and_then(|m| m.get(name.as_str()).copied())
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
///
/// `queue_due` (the interleaved queue gate, `usage::auto_start_queue`) narrows the
/// LAPSED leg only. The re-test leg stays ungated on purpose: it is a health
/// probe on an already-open window whose verdict the fallback chain routes on
/// (`kick_block_switch_grade`), not a window open, so delaying it by up to
/// `5h / N` would leave the chain refusing to switch back to an account that
/// had already recovered.
///
/// The pending leg is the re-test's event-driven twin: a live window whose
/// weekly quota just reset gets ONE kick on the poll cadence (no backoff, no
/// queue — the kick opens nothing). A landed kick proves the account; a 429
/// records a block the `has_block` leg continues from.
fn should_open_window(
    streak: u32,
    window_lapsed: bool,
    kick_due: bool,
    has_block: bool,
    queue_due: bool,
    weekly_reset_pending: bool,
) -> bool {
    if streak != 0 {
        return false;
    }
    if window_lapsed {
        kick_due && queue_due
    } else if weekly_reset_pending {
        true
    } else {
        has_block
    }
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
    weekly_reset_kicks: &WeeklyResetKicks,
    name: &ProfileName,
    now_secs: i64,
    queue_due: bool,
) -> bool {
    let block = kick_block(kick_blocks, name);
    let weekly_reset_pending = weekly_reset_kicks
        .lock()
        .ok()
        .is_some_and(|m| m.contains(name));
    should_open_window(
        rate_limit_streak(streaks, name),
        window_lapsed(store, name, now_secs),
        kick_retry_due(block.as_ref(), now_secs),
        block.is_some(),
        queue_due,
        weekly_reset_pending,
    )
}

/// Copy of `name`'s kick block (`None` when absent or poisoned). Read alone and
/// released immediately — KickBlockState(230) is a leaf like PollStreak.
fn kick_block(blocks: &KickBlocks, name: &ProfileName) -> Option<KickBlock> {
    blocks
        .lock()
        .ok()
        .and_then(|m| m.get(name.as_str()).copied())
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
fn kick_rejected_names(blocks: &KickBlocks, now_secs: i64) -> Vec<ProfileName> {
    blocks
        .lock()
        .map(|m| {
            m.iter()
                .filter(|(_, b)| kick_block_switch_grade(b, now_secs))
                .map(|(n, _)| ProfileName::from(n.clone()))
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
    name: &ProfileName,
    opened: bool,
    blocked: Option<KickRateLimit>,
    now_secs: i64,
) {
    let prev = kick_block(blocks, name);
    if opened {
        if prev.is_some() {
            if let Ok(mut m) = blocks.lock() {
                m.remove(name.as_str());
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
        .map(|n| {
            (
                n.clone(),
                load_profile_cache(&ProfileName::from(n.clone()), KICK_BLOCK_CACHE_FILE),
            )
        })
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

/// The switch-grade kick blocks read off DISK rather than a live scheduler's
/// memory — the daemonless mirror of [`kick_rejected_names`], over the same
/// `kick_block.json` files [`sync_kick_blocks_from_cache`] bootstraps from and
/// the same [`kick_block_switch_grade`] predicate.
///
/// Exists for the auto-start queue's membership rule
/// ([`crate::usage::auto_start_queue_members`]), which every surface must answer
/// identically or publish a queue the election is not running. `clauth status
/// --json` has no scheduler to ask, and a blocked member left in would take a
/// position and inflate `N` for as long as the limiter's advertised ceiling
/// stands — hours — shortening every OTHER member's published `next_open_at`
/// against a gap the election never applies (review round 4).
pub(crate) fn switch_grade_kick_blocked_from_cache(
    names: &[ProfileName],
    now_secs: i64,
) -> Vec<ProfileName> {
    names
        .iter()
        .filter(|n| {
            load_profile_cache::<KickBlock>(n, KICK_BLOCK_CACHE_FILE)
                .is_some_and(|b| kick_block_switch_grade(&b, now_secs))
        })
        .cloned()
        .collect()
}

/// Elect this tick's single queue opener and stamp it onto `due`.
///
/// The interleaved auto-start queue (`usage::auto_start_queue`): with several accounts on
/// `auto_start` every window reopens the instant it lapses, so they stay in
/// whatever phase they booted with and all reset together. Spacing their opens
/// by `5h / N` instead puts a fresh window within reach every `5h / N`.
///
/// Two decisions live here rather than in the worker, and both need the whole
/// picture at once:
///
///   * **Queue SIZE** comes from [`crate::usage::auto_start_queue_members`] — every member
///     that participates over time — not from `due` (only those whose cadence
///     slot came up this tick). Sizing off `due` would swing `N`, and with it
///     the gap, every tick. That rule also drops the members that cannot open a
///     window at all, so the queue never reserves a slot for a corpse and
///     spreads the live members too thin. One exclusion comes free on top: a
///     spent account under `refresh_spent_accounts = false` was already dropped
///     from `due`, so it can be a member but never the winner.
///   * **The winner** is elected from the queue members due THIS tick, because
///     [`fetch_oauth_due_with`] fans out one worker per profile and two workers
///     consulting the queue anchor concurrently would both kick.
///
/// Candidates are built BEFORE the anchor is read, so a tick with nothing lapsed
/// answers without one. That ordering is what pays for
/// [`crate::usage::queue_anchor`]'s history replay: the two paths agree (an
/// election no one can win stamps every member shut, exactly as the gap branch
/// does), and the replay is left to the ticks where a window is actually wanted.
///
/// With the toggle off `auto_start_queue_members` is empty, so every entry keeps the
/// permissive `may_open_window` that [`collect_tokens`] set — exactly the pre-queue
/// behaviour.
fn elect_auto_start_queue(
    state: &SchedulerState,
    due: &mut [TokenEntry],
    interval_ms: u64,
    now_secs: i64,
) {
    // Kick blocks first (KickBlockState 230), then the config (400) — read and
    // released before the store below, since Config outranks UsageStore(300)
    // and the two must not nest.
    let blocked = kick_rejected_names(&state.kick_blocks, now_secs);
    // Two lists from the one config read: `queue` (members, minus the real
    // blocked set) sizes the gap and elects, `profiles` (the FULL list) is
    // the anchor input — a window open is a window open, whoever holds it.
    let (queue, profiles) = state
        .config
        .lock()
        .map(|c| {
            (
                crate::usage::auto_start_queue_members(&c, &blocked),
                c.profiles
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    if queue.is_empty() {
        return;
    }

    // Non-members are never stamped by any of the three loops below:
    // `auto_start_queue_members` excludes `auth_broken` and switch-grade-blocked
    // profiles, which can never be elected, so writing them `false` would deny
    // them the lapsed leg on every tick — and since only a landed kick clears a
    // block, permanently. They keep [`collect_tokens`]'s permissive default and
    // retry on the `kick_retry_due` ladder exactly as before the queue.
    let shut = |due: &mut [TokenEntry]| {
        for entry in due.iter_mut().filter(|e| queue.contains(&e.name)) {
            entry.may_open_window = false;
        }
    };

    let candidates: Vec<crate::usage::Candidate<'_>> = queue
        .iter()
        .filter(|name| due.iter().any(|d| &d.name == *name))
        .map(|name| crate::usage::Candidate {
            name: name.as_str(),
            lapsed: window_lapsed(&state.store, name, now_secs),
            failures: crate::usage::queue_failures(&state.auto_start_queue, name, now_secs),
        })
        .collect();
    // Nothing lapsed, so nothing wants a window: `elect_queue_member` elects on
    // `lapsed` alone, so the election below could only return `None` and stamp
    // every member shut — which is what the gap branch does too. Answering here
    // is the same outcome by both paths, and it is what keeps the anchor's
    // history replay off the ticks it could not change: this is the majority of
    // every cycle (all windows live, the gap long since elapsed), and an idle
    // window's boundary oscillates ±1s around its minute anchor on every
    // recompute, so a polled account appends history on every single poll.
    if !candidates.iter().any(|c| c.lapsed) {
        shut(due);
        return;
    }

    let gap = crate::usage::queue_gap_secs(queue.len(), interval_ms);
    // The anchor replays every profile's history (the full list), while the
    // member list keeps sizing the gap and electing — so an open on a
    // non-member still gates the queue.
    let anchor = crate::usage::queue_anchor(&state.auto_start_queue, &profiles, now_secs, gap);
    if !crate::usage::queue_due(anchor, now_secs, gap) {
        // Inside the gap: no MEMBER opens a window this tick. The re-test leg
        // is untouched — `should_open_window` only consults the queue on the
        // lapsed leg, so a standing kick block still gets probed on the poll
        // cadence.
        shut(due);
        return;
    }

    let elected = crate::usage::elect_queue_member(&candidates).map(str::to_string);
    // Members only, same as `shut` above: a non-member stamped by this loop
    // could never win it back.
    for entry in due.iter_mut().filter(|e| queue.contains(&e.name)) {
        entry.may_open_window = elected.as_deref() == Some(entry.name.as_str());
    }
}

/// The one line that says a queue open FIRED.
///
/// [`note_kick_outcome`] speaks on state TRANSITIONS only — it logs a kick that
/// cleared a standing block and stays silent on the happy path — so a landed
/// queue open was invisible on every surface: nothing in the daemon log,
/// nothing in the TUI's. This is the line that answers "did the queue
/// open fire?".
///
/// `next in` is the gap itself, because [`crate::usage::note_queue_open`] has
/// just moved the anchor to `now`: the queue's next opening is exactly one gap
/// out. Silent when the profile holds no queue slot, which is also how the
/// queue toggle turns this off — with it off there is no queue to report a
/// position in, and every lapsed window simply reopens as it did before the
/// feature existed.
///
/// Locks ascend and never nest: KickBlockState(230), then Config(400), each
/// released before the next.
fn log_queue_open(
    config: &crate::profile::ConfigHandle,
    kick_blocks: &KickBlocks,
    name: &str,
    interval_ms: u64,
    now_secs: i64,
) {
    let blocked = kick_rejected_names(kick_blocks, now_secs);
    let queue = config
        .lock()
        .map(|c| crate::usage::auto_start_queue_members(&c, &blocked))
        .unwrap_or_default();
    let Some(slot) = crate::usage::queue_slot(&queue, name, Some(now_secs), interval_ms, now_secs)
    else {
        return;
    };
    // `next in` is the gap itself: the anchor was just set to `now`, so the
    // recomputed slot's countdown is exactly one gap — no fallback exists.
    logline!(
        "{name}: 5h auto-start window opened (queue {}/{}, next in {})",
        slot.position,
        slot.total,
        humanize_duration(crate::usage::queue_gap_secs(slot.total, interval_ms))
    );
}

/// Fetch one profile's usage on the periodic tick. When the profile opted into
/// auto-start, fire the kick first whenever `should_open_window` says to — to OPEN
/// a lapsed window, or to RE-TEST a standing kick block on a now-live window (it
/// may have reopened via the web app while Claude Code stays 429'd) — rotating
/// once on 401 OR 429, mark the window open on success, then fetch with the
/// possibly-rotated token.
// One arg over the lint's bar, and every one of them is a distinct shared store
// this leg writes; bundling them into a struct would only rename the same
// coupling. `fetch_oauth_due` is the single caller.
#[allow(clippy::too_many_arguments)]
fn run_fetch(
    config: &crate::profile::ConfigHandle,
    mut entry: TokenEntry,
    store: &UsageStore,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    streaks: &PollStreaks,
    kick_blocks: &KickBlocks,
    weekly_reset_kicks: &WeeklyResetKicks,
    auto_start_queue: &crate::usage::AutoStartQueueState,
    interval_ms: u64,
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
        // WHICH leg would fire, read before the kick moves the window. The
        // queue gates the LAPSED leg only ([`should_open_window`]), so a lapsed
        // window plus an elected member is exactly a queue open; the
        // live-window re-test can also land a kick and it opens nothing.
        // Naming that a auto-start would be a lie in the one line whose whole job
        // is to say what happened.
        let lapsed = window_lapsed(store, &entry.name, now_secs);
        let queue_open = entry.may_open_window && lapsed;
        if auto_start_should_kick(
            streaks,
            store,
            kick_blocks,
            weekly_reset_kicks,
            &entry.name,
            now_secs,
            entry.may_open_window,
        ) {
            let kicked = crate::oauth::auto_start_kick(
                config,
                &entry.name,
                &entry.access_token,
                entry.refresh_token.as_deref(),
                entry.access_expires_at,
                Some(activity),
            );
            // Whatever leg fired it, the kick consumed the weekly-reset
            // re-test: a landed kick proved the account, a 429 recorded a
            // block the standing re-test leg continues from.
            if let Ok(mut pending) = weekly_reset_kicks.lock() {
                pending.remove(&entry.name);
            }
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
                // Anchor the queue on the kick, not on the window it produced —
                // but only when the kick actually PRODUCED one. `kicked.opened`
                // is a 2xx from `/v1/messages`, which the live-window re-test
                // leg also gets while opening nothing (`mark_window_open`
                // no-ops there for the same reason). Anchoring on that would
                // re-phase the whole queue for a non-event, and push a lone
                // account's own next auto-start out past its lapse. A kick that
                // found the window LAPSED is the one that opened it, elected or
                // not — an out-of-band open is still one the queue must space
                // against.
                if lapsed {
                    crate::usage::note_queue_open(auto_start_queue, &entry.name, now_secs);
                }
                if queue_open {
                    log_queue_open(config, kick_blocks, &entry.name, interval_ms, now_secs);
                }
            } else if entry.may_open_window {
                // An ELECTED kick that opened nothing. Step this member toward
                // the election's skip threshold so a permanently kick-incapable
                // account (the macOS `rotation_blocked_for` carve-out records no
                // `KickBlock` and never sets `auth_broken`, so nothing else sees
                // it) cannot head-of-line block the members behind it. The
                // anchor deliberately does NOT move: nothing opened, so the next
                // tick re-elects immediately.
                crate::usage::note_queue_kick_failed(auto_start_queue, &entry.name, now_secs);
            }
        } else if entry.may_open_window && lapsed {
            // Elected, lapsed, and REFUSED before the kick could fire — a 429
            // streak in flight, or a standing block whose retry clock hasn't
            // come due. The slot was still consumed with nothing opened, so it
            // steps toward the skip threshold exactly as a failed kick does;
            // otherwise a member stuck in refusal holds the slot on every tick,
            // which is the head-of-line case [`crate::usage::auto_start_queue`]'s
            // election-failure limit exists to prevent. (A lapsed NON-member
            // can reach here too while refused — harmless while it holds no
            // slot: its streak is never consulted. If it rejoins the queue
            // within the hour, the recorded refusal still counts, so a freshly
            // re-admitted member can carry one stale failure toward the skip
            // limit.)
            crate::usage::note_queue_kick_failed(auto_start_queue, &entry.name, now_secs);
        }
    }

    // Prior plan for the TTL'd `/profile` policy, read from the live store and
    // released before the fetch so no lock is held across HTTP.
    let prev_plan = store
        .lock()
        .ok()
        .and_then(|m| m.get(entry.name.as_str()).and_then(|i| i.plan.clone()));

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
fn deadline_spread(name: &ProfileName, now: EpochMs, interval_ms: u64) -> IntervalMs {
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
    name: &ProfileName,
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
        m.remove(name.as_str());
        return StreakCounts::default();
    }
    let rate_limited = matches!(status, FetchStatus::RateLimited);
    if !rate_limited && !refresh_failed {
        // Says nothing about either axis — leave both counters (and, when this
        // profile has none, the empty map) untouched.
        return m.get(name.as_str()).copied().unwrap_or_default();
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
// One arg over the lint's bar; each is a distinct shared store or decision input
// this leg reads alone, and bundling them would only rename the same coupling.
#[allow(clippy::too_many_arguments)]
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    streaks: &PollStreaks,
    interval_ms: u64,
    is_active: bool,
    auto_start: bool,
    weekly_reset_kicks: &WeeklyResetKicks,
) -> EpochMs {
    let now = EpochMs::from_millis(now_ms());

    // Only a body that came off the live API may overwrite shared state. The
    // 429/cached fallback paths recycle the on-disk snapshot — stamping that
    // as fresh would clobber a newer store entry and re-write the disk cache
    // mtime, freezing the UI (and the auto-start scan) on stale numbers for as
    // long as the rate limit lasts. `status` still surfaces RateLimited/Cached
    // so the staleness stays visible.
    let is_fresh = outcome.from_fetch;

    // A `/profile` plan fetched despite a `/usage` 429 (a canceled account has been
    // observed to keep 429ing `/usage`): the ONLY fresh signal on an otherwise cached bail. The
    // fetch path carries it only on the ~hourly tick `/profile` is re-pulled, so
    // persisting it re-stamps the disk mtime at most once an hour — not the
    // per-tick storm the cached path guards against. Windows stay cached; only
    // the tier advances.
    let plan_refresh = outcome.plan_override.clone().filter(|_| !is_fresh);

    // The sample this outcome replaces, cloned out under a short lock that is
    // released before either disk write below. Read once and shared by both
    // consumers — the live-window preservation and the history sample — so the
    // store is never locked twice for the same value. Only the Fresh-with-body
    // path has a consumer, so the lock is not taken at all otherwise.
    let prev: Option<UsageInfo> = if is_fresh && outcome.info.is_some() {
        store
            .lock()
            .ok()
            .and_then(|s| s.get(outcome.name.as_str()).cloned())
    } else {
        None
    };

    // For a Fresh body, keep any just-opened live 5h window we already hold so a
    // lagging `/usage` read can't re-close it (see `preserve_live_window`).
    let merged: Option<UsageInfo> = outcome.info.as_ref().map(|info| {
        if !is_fresh {
            let mut info = info.clone();
            if let Some(plan) = &plan_refresh {
                info.plan = Some(plan.clone());
            }
            return info;
        }
        preserve_live_window(info.clone(), prev.as_ref(), now_epoch_secs())
    });

    // The weekly-reset re-test mark: a fresh body whose aggregate 7d window
    // rolled over from the hard cap while the 5h window is live (see
    // [`weekly_reset_pending`]). Set for opted-in profiles only — run_fetch
    // consults the mark under `entry.auto_start`, and a mark on a profile
    // that never opts in would sit in the set for the process lifetime.
    if is_fresh
        && auto_start
        && let (Some(prev), Some(info)) = (prev.as_ref(), merged.as_ref())
        && weekly_reset_pending(prev, info, now_epoch_secs())
        && let Ok(mut pending) = weekly_reset_kicks.lock()
    {
        pending.insert(outcome.name.clone());
    }

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

    // Durable burn-rate series. It rides the fetch path, not a UI tick, so the
    // holder of the single-fetcher lease is its only writer: a headless daemon
    // keeps the log advancing with no TUI open, and no second process can
    // interleave a line. Same `Fresh` gate the sample-quality invariant wants —
    // a synthetic just-kicked 0% window or a recycled cached snapshot would
    // land a phantom reset that survives restart and skews the rate. Outside
    // every lock, like the cache write above.
    if is_fresh && let Some(info) = merged.as_ref() {
        crate::profile::append_usage_sample(&outcome.name, prev.as_ref(), info);
    }

    if let Ok(mut s) = store.lock() {
        if let Some(info) = &merged {
            // Don't clobber newer Fresh data with a Cached fallback snapshot.
            // Cached only fills the store when no entry exists (cold start).
            if is_fresh || !s.contains_key(outcome.name.as_str()) {
                s.insert(outcome.name.to_string(), info.clone());
            } else if let Some(plan) = &plan_refresh
                && let Some(existing) = s.get_mut(outcome.name.as_str())
            {
                // Advance only the tier on the live entry; its windows stay cached.
                existing.plan = Some(plan.clone());
            }
        } else if let Some(plan) = &plan_refresh {
            // Cold-miss: advance the tier on any existing (windowless) entry, else
            // record a plan-only one so the walk can exclude the dead account.
            match s.get_mut(outcome.name.as_str()) {
                Some(existing) => existing.plan = Some(plan.clone()),
                None => {
                    s.insert(
                        outcome.name.to_string(),
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
        lf.insert(outcome.name.to_string(), stamped);
        if let Ok(mut st) = status.lock() {
            st.insert(outcome.name.to_string(), outcome.status);
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
///
/// The `open_at` stamp is the kick's durable record: it rides this synthetic
/// entry into the history file when the next fresh body lands (the writer
/// bridges the value it replaces), and the auto-start queue's marker pass
/// confirms the kicked window on it. Stamped ONLY here — every other
/// `UsageInfo` (wire parses, `prime_window`'s out-of-band opens) carries
/// `None`, so a history line with a marker is provably a kick of ours.
fn mark_window_open(store: &UsageStore, name: &ProfileName, now_secs: i64) {
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
    info.open_at = Some(now_secs);
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
        try_seed_cache(
            store,
            status,
            last_fetched,
            &ProfileName::from(name.clone()),
            now,
            interval_ms,
        );
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
    name: &ProfileName,
    interval_ms: u64,
    now: u64,
    mtime_fn: impl Fn(&ProfileName) -> Option<u64>,
    load_fn: impl Fn(&ProfileName) -> Option<T>,
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
    name: &ProfileName,
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
            s.insert(entry.name.to_string(), stats);
        }
        // Ascending rank order: LAST_FETCHED(200) < THIRD_PARTY_STATUS(280).
        if let Ok(mut lf) = last_fetched.lock() {
            lf.insert(entry.name.to_string(), EpochMs::from_millis(mtime));
            if let Ok(mut st) = status.lock() {
                st.insert(entry.name.to_string(), fetch_status);
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
        .filter_map(third_party_entry_for)
        .collect()
}

/// One profile's third-party entry, or `None` when its provider has no usable
/// credential. The single construction site: the work list above filters it by
/// disabled-ness, and [`profile_credential_fingerprint`] takes its fingerprint,
/// so the identity the scheduler suppresses on and the identity a persisted
/// record is keyed by can never be two different things.
fn third_party_entry_for(p: &crate::profile::Profile) -> Option<ThirdPartyEntry> {
    if !third_party_credentialed(p) {
        return None;
    }
    let target = if let Some(provider) = p.provider {
        // The console credential rides the target because one provider's usage
        // surface can't read the api key at all (Alibaba); the others carry
        // `None` and never look.
        crate::providers::ThirdPartyTarget::Known {
            provider,
            console: p.console.clone(),
        }
    } else {
        crate::providers::ThirdPartyTarget::Generic {
            base_url: p.base_url.clone()?,
        }
    };
    Some(ThirdPartyEntry {
        name: p.name.clone(),
        target,
        api_key: p.api_key.clone().unwrap_or_default(),
    })
}

/// Fingerprint of the credential `p` would fetch with. Deliberately NOT gated on
/// disabled-ness: credential identity is a property of the credential, not of
/// whether the profile is currently scheduled.
pub(crate) fn profile_credential_fingerprint(p: &crate::profile::Profile) -> Option<u64> {
    third_party_entry_for(p).map(|e| e.credential_fingerprint())
}

/// Whether the third-party leg can fetch this profile at all — the credential
/// test [`collect_third_party_entries`] applies, hoisted so the RENDER layer
/// reads the same rule instead of restating it. A profile this returns `false`
/// for never gets a `fetch_status`, so the Usage tab must say so rather than
/// spin on "loading" forever.
///
/// Disabled-ness is deliberately not part of it: that is a separate axis, and
/// both callers already handle it themselves.
pub(crate) fn third_party_credentialed(p: &crate::profile::Profile) -> bool {
    match p.provider {
        // Alibaba's quota surface cannot read the api key (`providers::alibaba`),
        // so a console-only profile is fetchable and a keyless one must still be
        // scheduled — its fetch reports the missing session as `AuthExpired`,
        // which is an answer, where being dropped is a permanent "loading".
        Some(crate::providers::Provider::Alibaba) => true,
        // An empty or whitespace-only key is no credential (matches the load
        // boundary's `has_usable_key`): it authenticates nothing, so treating it
        // as `Some` would schedule a run that cannot work.
        _ => p
            .api_key
            .as_deref()
            .map(str::trim)
            .is_some_and(|k| !k.is_empty()),
    }
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
            let oauth = p.credentials.as_ref()?.claude_ai_oauth.as_ref()?;
            Some(TokenEntry {
                name: p.name.clone(),
                access_token: oauth.access_token.clone(),
                refresh_token: oauth.refresh_token.clone(),
                auto_start: p.auto_start,
                access_expires_at: oauth.expires_at,
                auth_broken: config.is_auth_broken(&p.name),
                // Permissive by default; `tick` is the only narrower.
                may_open_window: true,
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

/// Remove session-suppressed profiles from the third-party snapshot so they
/// aren't re-fetched on the timer. A poisoned lock passes the snapshot through.
///
/// An entry stays suppressed only while it still carries the credential it was
/// suppressed under. That comparison is the whole clearing path for a headless
/// daemon: nothing in `src/daemon/` writes `refetch_queue` (the TUI's manual
/// refresh is its only producer), so keying on the bare name pinned an
/// `AuthExpired` profile until the process restarted, even though the daemon
/// had already rebuilt the entry from the reloaded config with the new session.
///
/// The guard is held across the filter deliberately: the closure takes no other
/// lock, so there is no ordering hazard for `lockorder` to police, and cloning
/// the map to avoid it would copy state this loop is the only reader of.
fn filter_suppressed(
    suppressed: &SuppressedGenericStore,
    snapshot: Vec<ThirdPartyEntry>,
) -> Vec<ThirdPartyEntry> {
    let Ok(sup) = suppressed.lock() else {
        return snapshot;
    };
    if sup.is_empty() {
        return snapshot;
    }
    snapshot
        .into_iter()
        .filter(|e| {
            sup.get(e.name.as_str())
                .is_none_or(|recorded| *recorded != e.credential_fingerprint())
        })
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
            &state.weekly_reset_kicks,
            &state.auto_start_queue,
            interval_ms,
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
        // `auto_start` rides the same lock: the weekly-reset mark below only
        // matters for profiles run_fetch would kick.
        let (is_active, auto_start) = state
            .config
            .lock()
            .map(|c| {
                let profile = c
                    .profiles
                    .iter()
                    .find(|p| p.name.as_str() == outcome.name.as_str());
                (
                    c.is_active(&outcome.name),
                    profile.is_some_and(|p| p.auto_start),
                )
            })
            .unwrap_or((false, false));
        let name = outcome.name.clone();
        let stamped = apply_outcome(
            outcome,
            &state.store,
            &state.status,
            &state.last_fetched,
            &state.poll_streaks,
            interval_ms,
            is_active,
            auto_start,
            &state.weekly_reset_kicks,
        );
        publish_one_countdown(&state.next_refresh_per_profile, &name, stamped, interval_ms);
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
            // Generic-ness is only HALF the suppression gate (see the outcome
            // handler below): a generic profile suppresses on a no-data result,
            // while a dead usage credential suppresses whatever the provider.
            // On everything else a known provider keeps its normal cadence.
            let is_generic = matches!(
                entry.target,
                crate::providers::ThirdPartyTarget::Generic { .. }
            );
            // Captured before the entry moves into the worker: suppression is
            // recorded against the credential that failed, never the bare name.
            let fingerprint = entry.credential_fingerprint();
            // Reuse the endpoint that last worked so steady state is one request.
            let hint = state.third_party_usage_store.lock().ok().and_then(|s| {
                s.get(entry.name.as_str())
                    .and_then(|st| st.endpoint.clone())
            });
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
            (name, is_generic, fingerprint, h)
        })
        .collect();

    for (name, is_generic, fingerprint, h) in handles {
        match h.join() {
            Ok(Ok(stats)) => {
                clear_activity(&state.activity, &name);
                // A live body retires any dead-credential record: the surfaces
                // that read it have no other way to learn the session came back.
                crate::profile_cache::clear_auth_expired(&name);
                write_profile_cache(&name, THIRD_PARTY_CACHE_FILE, &stats);
                if let Ok(mut store) = state.third_party_usage_store.lock() {
                    store.insert(name.to_string(), stats);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.to_string(), FetchStatus::Fresh);
                }
                stamp_last_fetched(
                    &state.last_fetched,
                    &state.next_refresh_per_profile,
                    &name,
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
                    // Ahead of the cache arm on purpose: a cached copy still
                    // renders (the cold-fill below is unchanged), but the STATUS
                    // has to name the dead credential — `Cached` would read as a
                    // transient blip on a state only a re-login clears.
                    crate::providers::ThirdPartyError::AuthExpired => {
                        (FetchStatus::AuthExpired, None)
                    }
                    _ if cached.is_some() => (FetchStatus::Cached, None),
                    _ => (FetchStatus::Failed, None),
                };
                if let Some(c) = cached
                    && let Ok(mut store) = state.third_party_usage_store.lock()
                {
                    store.entry(name.to_string()).or_insert(c);
                }
                if let Ok(mut st) = state.third_party_status.lock() {
                    st.insert(name.to_string(), status);
                }
                // Two outcomes suppress for the rest of the session — no timer
                // retry, only a manual refresh re-admits one for a single try. A
                // generic profile that tried and found nothing (no cache, not a
                // 429), and ANY profile whose usage credential is dead: the
                // cadence can't fix either, and only the second can happen to a
                // known provider. 429 keeps the server-directed deferral;
                // cached legs are unaffected.
                if (matches!(status, FetchStatus::AuthExpired)
                    || (is_generic && matches!(status, FetchStatus::Failed)))
                    && let Ok(mut sup) = state.suppressed_generic.lock()
                {
                    sup.insert(name.to_string(), fingerprint);
                }
                // Durable twin of that suppression, for the surfaces with no
                // scheduler in the process (`clauth list`, `clauth status
                // --json`): without it they derive freshness from the usage
                // cache's mtime and publish a warm cache behind a dead session
                // as `Fresh`. Keyed by the same fingerprint, so a re-login
                // retires it. Cleared on every other outcome — the verdict is
                // "the last fetch under THIS credential was AuthExpired", so
                // one that isn't must not leave it standing.
                if matches!(status, FetchStatus::AuthExpired) {
                    crate::profile_cache::write_auth_expired(&name, fingerprint);
                } else {
                    crate::profile_cache::clear_auth_expired(&name);
                }
                stamp_last_fetched(
                    &state.last_fetched,
                    &state.next_refresh_per_profile,
                    &name,
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
    name: &ProfileName,
    retry_after: Option<Duration>,
    rate_limited: bool,
    interval_ms: u64,
) {
    // Third-party providers are independent hosts with their own limits; keep the
    // flat base backoff (streak 1) rather than the per-account exponential ramp.
    let defer = next_slot_deferral(rate_limited, retry_after, 1, interval_ms, false);
    let stamped = EpochMs::from_millis(now_ms()).saturating_add(defer);
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(name.to_string(), stamped);
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
    name: &ProfileName,
    stamped: EpochMs,
    interval_ms: u64,
) {
    if let Ok(mut map) = nrpp.lock() {
        map.insert(
            name.to_string(),
            stamped.as_millis().saturating_add(interval_ms),
        );
    }
}

/// Every profile that could own a `usage_history.jsonl`, disabled and
/// third-party included. Deliberately WIDER than [`collect_tokens`]'s work-list
/// and than [`collect_oauth_seed_names`]: retention is a property of the file on
/// disk, not of whether the profile is currently pollable. A disabled profile —
/// or one converted to an api-key base-url after its OAuth days — still holds
/// per-account utilization history that has to age out. Config(400) is acquired
/// alone and released here.
fn history_profile_names(config: &crate::profile::ConfigHandle) -> Vec<ProfileName> {
    config
        .lock()
        .map(|c| c.profiles.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default()
}

/// Re-trim every usage-history log once the cadence has elapsed; reports whether
/// it ran. The retention window itself is [`crate::profile::prune_usage_history`]'s
/// 2 days — this only bounds how far past it a file can drift.
///
/// A startup-only trim was enough while the TUI was the writer (a launch
/// re-pruned), but the appender is the fetch path now, and a daemon under
/// launchd/systemd is built never to restart: nothing would ever trim it, the
/// log would grow for the life of the process, and `burn_rate_for_profile`
/// re-parses the whole file from `scan_auto_switch` on EVERY tick.
///
/// Called only by the fetch-lease holder, which is also the only appender, so
/// this rewrite can never race an append of its own. The cadence check is an
/// atomic load, so the config lock and the name list are only reached on the
/// tick that actually prunes — not on the ~21 600 ticks between two of them.
fn prune_histories_if_due(
    last_prune: &AtomicU64,
    config: &crate::profile::ConfigHandle,
    now: u64,
) -> bool {
    let last = last_prune.load(Ordering::Relaxed);
    if now.saturating_sub(last) < HISTORY_PRUNE_INTERVAL_MS {
        return false;
    }
    // Claim the window before doing the work, so a second caller entering
    // concurrently backs off instead of pruning the same files alongside us.
    if last_prune
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    for name in history_profile_names(config) {
        crate::profile::prune_usage_history(&name);
    }
    true
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
    /// Pending weekly-reset re-test kicks (see [`weekly_reset_pending`]).
    weekly_reset_kicks: WeeklyResetKicks,
    /// Interleaved auto-start queue (`usage::auto_start_queue`): the anchor the 5h-window
    /// queue spaces against, plus per-profile election health.
    auto_start_queue: crate::usage::AutoStartQueueState,
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
    /// When the usage-history logs were last trimmed to their retention window,
    /// as epoch ms. Seeded at the startup pass in [`spawn_refresher`]; the tick
    /// re-runs the trim once [`HISTORY_PRUNE_INTERVAL_MS`] has elapsed.
    last_history_prune: AtomicU64,
    /// CLA-ROLL pacing for the rolling-sidecar freshness scan.
    claude_rolling:
        crate::lockorder::RankedMutex<ClaudeRollingPacing, crate::lockorder::rank::RollingPacing>,
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

    // Retention trim for the logs this process appends to. Under the lease, so
    // it never races its own appends, and ahead of the fetch legs so a long-run
    // process re-trims before adding to the file rather than after.
    prune_histories_if_due(&state.last_history_prune, &state.config, now_ms());

    // CLA-ROLL: rolling-sidecar freshness scan — renew a rolling-token profile's
    // session bearer hours ahead of its clock death instead of relying on
    // rotation side effects (lease-holder only, like every other leg).
    //
    // Inline and serial on the tick thread, ahead of the fanned-out fetch
    // legs, ON PURPOSE — a deliberate departure from the `fetch_oauth_due_with`
    // worker pattern, not an oversight of it. The steady state is one due
    // profile per chain lifetime (~hours), so the fan-out's reason to exist
    // (one slow account stalling every other account's poll, every tick) has
    // no analogue here. What makes inline SAFE is `LockWait::NoWait`: the
    // gate's rotation-lock acquisition is a try-lock on this leg, because its
    // waiting form carries no deadline and a `clauth start`
    // holding the lock across its recursive `~/.claude` copy would otherwise park
    // this thread — and with it every account's poll — while the heartbeat
    // (stamped in the main loop, not here) kept reading fresh. The session
    // start's own `runtime::ROTATION_LOCK_TIMEOUT` is no substitute: it bounds
    // that caller, not this one, and it waits tens of seconds — a tick budget many
    // times over. What remains
    // is one token round trip on a cold re-stamp, which delays a single tick.
    // Anyone adding per-tick work to this leg inherits the fan-out question.
    claude_rolling_tick(&state.config, &state.claude_rolling, now_ms(), &|name| {
        crate::oauth::restamp_rolling_token(&state.config, name, crate::oauth::refresh_result)
    });

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
            .filter_map(|e| {
                Some((
                    e.name.to_string(),
                    five_hour_reset_ms(store.get(e.name.as_str())?)?,
                ))
            })
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
            Err(_) => oauth_snapshot.iter().map(|e| e.name.to_string()).collect(),
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
    // Elect this tick's single queue opener now that `oauth_due` is final —
    // before the fan-out, which is what serialises the queue.
    elect_auto_start_queue(state, &mut oauth_due, interval_ms, now_epoch_secs());

    let (tp_due, tp_next) = partition_and_merge(&tp_snapshot, &forced, state, now, interval_ms);
    publish_countdowns(&state.next_refresh_per_profile, oauth_next, tp_next);

    // Names actually scheduled this tick across both legs. A forced name absent
    // from both (e.g. a profile whose creds were removed between the UI `r` and
    // this tick) was marked Queued by `enqueue_refetch` but no worker owns it, so
    // the orphan sweep at the tick's end clears it — otherwise its spinner freezes.
    let scheduled: HashSet<String> = oauth_due
        .iter()
        .map(|e| e.name.to_string())
        .chain(tp_due.iter().map(|e| e.name.to_string()))
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
    // Per-session fallback, off the SAME stores the global scan just read: one
    // fetcher, N decisions. Only from here, never from `standdown_tick` — a
    // non-lease-owner decides nothing, exactly as it skips both scans above.
    scan_session_switches(
        &state.config,
        &state.store,
        &state.status,
        &state.third_party_status,
        &state.poll_streaks,
        &state.kick_blocks,
    );
    scan_recovery(
        &state.config,
        &state.store,
        &state.status,
        &state.third_party_status,
        &state.kick_blocks,
        &state.pending_switch,
    );
}

/// Log a stand-down / lease-acquired transition. Either the TUI or the daemon
/// can stand down now (whichever didn't win the lease). `logline!` routes the
/// daemon's line to `daemon.log` and an interactive TUI's to
/// `~/.clauth/clauth.log`, so it is recorded without ever painting over the
/// accounts pane.
fn standdown_transition_log(msg: &str) {
    logline!("{msg}");
}

/// One scheduler tick while a live daemon owns the loop. The daemon
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
    let oauth_names: Vec<String> = oauth_snapshot.iter().map(|e| e.name.to_string()).collect();
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
    // does, and the row would spin forever where the daemon-fed countdown
    // belongs. Fetching/Refreshing/Switching stay — a worker from the last
    // armed tick may genuinely still be in flight and clears itself.
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
        try_seed_cache(
            store,
            status,
            last_fetched,
            &ProfileName::from(name.clone()),
            now,
            interval_ms,
        );
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
    auto_start_queue: crate::usage::AutoStartQueueState,
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
    // outlive a test's `HOME_OVERRIDE` and read the operator's real home.
    let names: Vec<String> = tokens
        .lock()
        .map(|t| t.iter().map(|e| e.name.to_string()).collect())
        .unwrap_or_default();
    sync_kick_blocks_from_cache(&kick_blocks, &names);
    // Interleaved auto-start queue. Constructed by the CALLER and passed in, the
    // same shape `kick_blocks` takes, because the TUI reads the anchor every
    // frame and a queue built in here would be unreachable from render (a
    // per-frame history replay is not an option in a render loop). Its
    // history-derived anchor is still seeded on THIS thread, for the same
    // home-path reason as the kick-block seed above — the derivation replays
    // per-profile `usage_history.jsonl` files.
    // The anchor replays EVERY profile's history — a window open is a window
    // open, whoever holds it — so the seed takes the full config profile list
    // and the per-tick gate does the same: the two agree on the anchor input
    // by construction (the blocked set and the auto-start toggle only ever
    // shaped the member list, which the anchor no longer takes).
    let anchor_seed: Vec<crate::profile::ProfileName> = config
        .lock()
        .map(|c| c.profiles.iter().map(|p| p.name.clone()).collect())
        .unwrap_or_default();
    crate::usage::seed_queue_anchor(&auto_start_queue, &anchor_seed);
    // Startup leg of the usage-history retention trim (the cadenced leg runs in
    // `tick`). Here rather than in the spawned closure for the same home-path
    // reason as the kick-block seed above.
    let history_names = history_profile_names(&config);
    for name in &history_names {
        crate::profile::prune_usage_history(name);
    }
    let last_history_prune = AtomicU64::new(now_ms());

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
        weekly_reset_kicks: Arc::new(RankedMutex::new(HashSet::new())),
        auto_start_queue,
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
        last_history_prune,
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
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
    name: &ProfileName,
) -> bool {
    matches!(
        status
            .lock()
            .ok()
            .and_then(|m| m.get(name.as_str()).copied()),
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
    name: &ProfileName,
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

/// Whether a member's last store reading may drive a switch decision.
///
/// Only a confirmed-live (`Fresh`) read qualifies — a stale or synthetic entry
/// would drive a false switch (see [`decision_fresh`]). TWO exceptions bypass it,
/// both because the member can never come back `Fresh` on its own, so requiring
/// one wedges the scan on it:
///   * an auth-broken member (AUTH-4) — its login is dead (observed 2026-07-09);
///     the walk never consults its usage.
///   * a deep-slot stuck `RateLimited` member (the `RateLimited` analogue) — the
///     `/usage` throttle stayed pinned past the active cap, so no `Fresh` read is
///     coming. Unlike auth-broken, this one still faces the walk's own last-known
///     exhaustion gate, so a throttle artifact with headroom stays put and only a
///     genuinely spent stuck member moves.
///
/// ONE predicate for the global active ([`scan_auto_switch`]) and for each
/// chain-following session's own member ([`scan_session_switches`]): the rule is
/// about what a reading is worth, which does not change with who is reading it.
fn reading_is_actionable(
    broken: bool,
    status: &StatusStore,
    streaks: &PollStreaks,
    name: &ProfileName,
) -> bool {
    if broken {
        return true;
    }
    let reading = status
        .lock()
        .ok()
        .and_then(|m| m.get(name.as_str()).copied());
    reading.is_some_and(|s| is_stuck_rate_limited(s, rate_limit_streak(streaks, name)))
        || matches!(reading, Some(FetchStatus::Fresh))
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
    // Skip when a previous decision is still pending. Each lock is acquired
    // and dropped before the next — never two leaf mutexes at once.
    {
        let Ok(p) = pending_switch.lock() else { return };
        if !p.is_empty() {
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

    // Only act on a reading worth acting on. The rule, and the two bypasses that
    // keep it from wedging the scan, live in `reading_is_actionable` — shared with
    // the per-session leg, which applies it to each session's own member.
    let active_broken = snapshot.broken.iter().any(|b| b == &snapshot.active);
    if !reading_is_actionable(active_broken, status, streaks, &snapshot.active.clone()) {
        return;
    }

    match crate::fallback::next_auto_switch_target(&snapshot, store) {
        Some(crate::fallback::SwitchAction::To(name)) => {
            if let Ok(mut p) = pending_switch.lock() {
                p.insert(name);
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

/// One live session's evaluation inputs, carried out of the `config` hold so the
/// walk itself runs lock-free.
struct SessionDecision {
    snapshot: crate::fallback::ChainSnapshot,
    /// The member this session's link resolves to — what the walk starts from.
    member: ProfileName,
    row: crate::live_sessions::LiveSession,
}

/// Per-session fallback: decide which chain member each chain-following live
/// session should be on, and write it into that session's registry row.
///
/// The DECISION half only. Nothing here touches a link, a marker, or a credential
/// file — the session's own watchdog executes the swap
/// ([`crate::runtime::SessionSwap::poll`]), and stays the safety gate for
/// everything this leg can only guess at from outside the child process.
///
/// **The retry is the recomputation.** A dropped write needs no queue and must not
/// be given one: the decision is re-derived from scratch every tick, so a failed
/// write self-heals within one interval, and `lock.rs` logs a state-lock timeout
/// once at the lock layer already.
///
/// Lock order is the shape of this function. Snapshot under `config` (400), DROP
/// it, evaluate lock-free, then take the state flock (500) ONCE for the whole
/// batch — `with_state_lock` is reentrant, so the `update_as_daemon` calls nested
/// inside it take no second flock, where per-session acquisition would expose one
/// tick to N × `STATE_LOCK_TIMEOUT`. Holding `config` across that flock would
/// lengthen contention for every other clauth process (the same reason
/// `ProfileTtl` ranks outside `State`), and no pending-switch lock is held
/// anywhere here: those rank OUTSIDE the flock (1500/1700 vs 500), so holding one
/// across the write inverts the order.
fn scan_session_switches(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
    status: &StatusStore,
    third_party_status: &ThirdPartyStatusStore,
    streaks: &PollStreaks,
    kick_blocks: &KickBlocks,
) {
    let rows: Vec<crate::live_sessions::LiveSession> = crate::live_sessions::list()
        .into_iter()
        .filter(|row| {
            // An isolated session runs a throwaway tree that is deliberately not
            // part of any chain, and the executor refuses it outright.
            row.follows_chain
                && !row.isolated
                // `gc_stale_runtimes` reaps rows at daemon STARTUP, not per tick,
                // so a SIGKILLed session's row outlives the whole daemon run and
                // would keep taking decisions nothing can execute.
                && {
                    let probe = ProfileName::from(
                        row.current_member.as_deref().unwrap_or(&row.start_profile),
                    );
                    crate::runtime::session_row_is_live(
                        &probe,
                        row.isolated,
                        &row.session_id,
                    )
                }
        })
        .collect();
    if rows.is_empty() {
        return;
    }

    // Snapshot under `config` only, exactly as `scan_auto_switch` does: holding it
    // while taking `usage_store` is the `App::apply_usage` inversion.
    let (chain, pending): (Vec<String>, Vec<SessionDecision>) = {
        let Ok(cfg) = config.lock() else { return };
        let chain = cfg
            .state
            .fallback_chain
            .iter()
            .map(|n| n.as_str().to_string())
            .collect();
        let pending = rows
            .into_iter()
            .filter_map(|row| {
                // `current_member` is `None` until a session's FIRST swap, which
                // is every session most of the time.
                let member = ProfileName::from(
                    row.current_member
                        .clone()
                        .unwrap_or_else(|| row.start_profile.clone()),
                );
                let launch = crate::runtime::LaunchTransport::of(
                    cfg.find(&ProfileName::from(row.start_profile.clone()))?,
                );
                let snapshot = crate::fallback::snapshot_session_chain(&cfg, &member, &launch)?;
                Some(SessionDecision {
                    snapshot,
                    member,
                    row,
                })
            })
            .collect();
        (chain, pending)
    };

    let kick_rejected = kick_rejected_names(kick_blocks, now_epoch_secs());
    let mut writes: Vec<(String, String, usize)> = Vec::new();
    for SessionDecision {
        mut snapshot,
        member,
        row,
    } in pending
    {
        // Neither of these is config state, so `snapshot_session_chain` cannot fill
        // them — same split `scan_auto_switch` works to.
        snapshot.kick_rejected = kick_rejected.clone();
        snapshot.fresh = snapshot
            .chain
            .iter()
            .filter(|m| decision_fresh_any(status, third_party_status, &m.name))
            .map(|m| m.name.clone())
            .collect();
        let member_broken = snapshot.broken.contains(&member);
        // The OAuth `StatusStore` alone, where the candidate fill above unions both
        // — and `decision_fresh_any` records why the twins must not disagree. Sound
        // here only because `swap_eligible`'s `is_oauth` arm leaves a
        // third-party-launched session a SINGLETON chain, which `walk_chain` cannot
        // move off whatever this gate says. Relaxing that arm (same-provider
        // swapping is the obvious next ask) closes such a session's gate
        // permanently with nothing saying so, so relax it and this gate goes
        // through the union too.
        if !reading_is_actionable(member_broken, status, streaks, &member) {
            continue;
        }
        // `Off` means "sign every account out", which has no per-session form: a
        // session cannot be left credential-less mid-flight and there is no member
        // name to write. It, and a stay-put `None`, leave the row as it stands.
        let Some(crate::fallback::SwitchAction::To(target)) =
            crate::fallback::next_auto_switch_target(&snapshot, store)
        else {
            continue;
        };
        // The cursor indexes the ON-DISK chain. `snapshot.chain` is filtered
        // (disabled members, and the swap-eligibility skip), so an index into it is
        // a different number the moment any member is dropped.
        let Some(cursor) = chain.iter().position(|n| n == &target) else {
            continue;
        };
        // Already what the row says: rewriting it every tick would take the
        // cross-process flock for nothing. A dropped write still self-heals, since
        // the row then differs from the decision this comparison is made against.
        if row.intended_member.as_deref() == Some(target.as_str())
            && row.chain_cursor == Some(cursor)
        {
            continue;
        }
        writes.push((row.session_id, target, cursor));
    }
    if writes.is_empty() {
        return;
    }

    let batch = crate::lock::with_state_lock(|_held| {
        for (session_id, target, cursor) in &writes {
            // A session that died between `list()` and this hold is skipped
            // silently — that is what keeps "no row for this id" from being this
            // leg's ordinary outcome rather than a real failure.
            if crate::live_sessions::get(session_id).is_none() {
                continue;
            }
            if let Err(e) = crate::live_sessions::update_as_daemon(session_id, |fields| {
                fields.set_intended_member(target.as_str());
                fields.set_chain_cursor(*cursor);
            }) {
                logline!("clauth: session {session_id} could not be pointed at {target}: {e:#}");
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    if let Err(e) = batch {
        logline!("clauth: per-session fallback decisions deferred to the next tick: {e:#}");
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
    // Skip when a previous switch is still pending.
    if let Ok(p) = pending_switch.lock()
        && !p.is_empty()
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
                    name: name.clone(),
                    threshold: profile
                        .map(crate::fallback::threshold_for)
                        .unwrap_or(crate::fallback::DEFAULT_THRESHOLD),
                    last_resort: profile.is_some_and(|p| p.last_resort),
                    preferred: profile.is_some_and(|p| p.preferred),
                    max_spend: profile.and_then(|p| p.max_auto_spend).unwrap_or(0.0),
                    weekly_line: profile
                        .map(|p| crate::fallback::member_weekly_line(p, weekly_pct))
                        .unwrap_or(weekly_pct),
                    scoped_line: profile
                        .map(|p| crate::fallback::member_scoped_line(p, weekly_pct))
                        .unwrap_or(weekly_pct),
                    check_scoped: profile.is_none_or(|p| p.check_scoped),
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
        p.insert(name);
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
    due.retain(|entry| !skip.contains(entry.name.as_str()));
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
            !forced.contains(entry.name.as_str())
                && store
                    .get(entry.name.as_str())
                    .is_some_and(|info| windows_maxed(info, now_secs))
        })
        .map(|entry| entry.name.to_string())
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
        if excluded.contains(entry.name.as_str()) || due.iter().any(|d| d.name == entry.name) {
            continue;
        }
        let last = last_fetched
            .get(entry.name.as_str())
            .map_or(0, |e| e.as_millis());
        if should_anchor_fetch(
            resets.get(entry.name.as_str()).copied(),
            last,
            now_ms,
            RESET_ANCHOR_GRACE_MS,
        ) {
            next.insert(entry.name.to_string(), now_ms);
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

/// CLA-ROLL cadence for the rolling-sidecar freshness scan. The due predicate is
/// stateless against the wall clock ([`crate::oauth::rolling_sidecar_restamp_due`]),
/// so a machine-sleep gap self-corrects on the first tick after wake — no
/// monotonic bookkeeping needed.
const ROLLING_SCAN_GAP_MS: u64 = 5 * 60 * 1000;
/// Widening after a transient re-stamp failure (network trouble, a busy
/// rotation lock) — the horizon is hours wide, so minutes-scale retries lose
/// nothing while avoiding per-scan log spam.
const ROLLING_RETRY_MS: u64 = 15 * 60 * 1000;
/// A Broken verdict (dead chain, no mint to degrade to) and the re-login-shaped
/// transients change only when the operator acts, so retrying them on the
/// minutes cadence is pure log noise — they pace on this long leash instead.
/// The leash is a NOISE gate, not the exit: a browser re-login writes
/// `credentials.json` and never touches the sidecar or the flag (and a
/// `--setup-token` re-mint writes a MINT, which disarms rather than re-arms),
/// so a hold only the clock releases would sit on the operator's fix for up to
/// six hours. Every hold this long therefore records a
/// [`crate::claude::credential_fingerprint`] of the profile, and a change to
/// any of the three files — the operator's fix, or clauth's own successful
/// rotation of the chain, either of which is exactly a reason to re-judge —
/// releases the hold on the next scan: the gate runs right after the write,
/// not six hours later. A self-release re-runs one gate and, if the verdict
/// stands, re-inserts the hold — bounded by the writes themselves.
const ROLLING_BROKEN_RETRY_MS: u64 = 6 * 60 * 60 * 1000;

/// One paced re-stamp hold. `watched` is `Some` exactly on the
/// [`ROLLING_BROKEN_RETRY_MS`]-length holds — the re-login-shaped ones, whose
/// real exit is a credential file changing (the operator's fix, or clauth's
/// own successful rotation — either one is reason to re-judge), not the
/// clock. The short transient cadence stays purely time-based, and the
/// backwards-clock clamp reads the kind off `watched`, so the coupling holds
/// under a clock step too.
pub(super) struct RetryHold {
    not_before: u64,
    watched: Option<[Option<(std::time::SystemTime, u64)>; 3]>,
}

/// Pacing for the re-stamp scan: an in-memory throttle only — the durable
/// truth is the sidecar's own expiry, which every leg re-reads.
#[derive(Default)]
pub(super) struct ClaudeRollingPacing {
    next_scan_ms: u64,
    retry_after_ms: HashMap<String, RetryHold>,
}

/// CLA-ROLL: rolling-sidecar freshness scan. For every rolling-token claude
/// profile whose armed sidecar is inside the re-stamp horizon of its clock
/// death, run the full feed decision table (no-spend re-stamp / guarded
/// refresh / mint degrade) NOW instead of waiting for a rotation side effect
/// — the failure this exists for was exactly a rolling bearer expiring under a running session
/// while rotations sat parked (spent-window poll parking, daemon idle,
/// machine sleep). Lease-holder tick only, like every other leg.
///
/// Deliberately scans every ENABLED rolling profile, not just the active one
/// (`AppConfig::enabled_profiles`): a parked profile's sessions may still be
/// running on its rolling bearer (sessions
/// survive switches by design), and a fresh sidecar makes the next switch-in
/// instant. The extra rotation pressure is nil — claude usage chains already
/// rotate on the ~8h access-token cadence for usage polling, and the daemon
/// is the single writer for parked chains either way.
///
/// `gate_fn` is injected (production: [`crate::oauth::restamp_rolling_token`])
/// so the orchestration — candidates → due → pace/widen — is testable
/// offline; only the injected closure ever touches locks or the network.
pub(super) fn claude_rolling_tick(
    config: &crate::profile::ConfigHandle,
    pacing: &crate::lockorder::RankedMutex<
        ClaudeRollingPacing,
        crate::lockorder::rank::RollingPacing,
    >,
    now: u64,
    gate_fn: &dyn Fn(&ProfileName) -> crate::oauth::AuthGate,
) {
    {
        let Ok(mut p) = pacing.lock() else { return };
        // Clamped, not just compared: these are WALL-CLOCK epoch-ms, and a
        // backwards NTP step of ΔT would otherwise suppress the entire scan
        // for ΔT while the sidecar this leg exists to renew keeps running its
        // clock down. A stamp further out than one full gap can only mean the
        // clock moved back under it, so it is pulled back into range.
        if p.next_scan_ms > now + ROLLING_SCAN_GAP_MS {
            p.next_scan_ms = now + ROLLING_SCAN_GAP_MS;
        }
        if now < p.next_scan_ms {
            return;
        }
        p.next_scan_ms = now + ROLLING_SCAN_GAP_MS;
    }
    let candidates: Vec<ProfileName> = {
        let Ok(cfg) = config.lock() else { return };
        // `enabled_profiles()`, not `profiles`: a disabled profile is off every
        // operational surface by definition, and this leg can reach a GUARDED
        // REFRESH, so sourcing the raw list spends single-use refresh tokens on
        // accounts the operator took out of service. Same view `collect_tokens`
        // uses one screen up.
        cfg.enabled_profiles()
            .filter(|p| p.rolling_token)
            .map(|p| p.name.clone())
            .collect()
    };
    // Names that leave the candidate set — profile deleted, disabled, or the
    // flag turned off — take their retry stamps with them: the map is
    // in-memory and small, but an entry nothing can ever clear is still a
    // leak, and a re-created profile of the same name would inherit a stale
    // leash it never earned.
    if let Ok(mut p) = pacing.lock() {
        p.retry_after_ms
            .retain(|held, _| candidates.iter().any(|c| c.as_str() == held));
    }
    for name in candidates {
        let held = pacing.lock().ok().and_then(|mut p| {
            // Same backwards-clock clamp as the scan stamp: a retry stamp
            // further out than its own leash can only mean the wall clock
            // moved back under it. The hold KNOWS its leash now — `watched`
            // rides exactly the long one — so each kind clamps to its own
            // bound, and a backwards step can no longer stretch a 15-minute
            // retry into an unwatched six-hour stall (the one combination
            // with no exit but the clock, which the watch exists to remove).
            let hold = p.retry_after_ms.get_mut(name.as_str())?;
            let bound = if hold.watched.is_some() {
                ROLLING_BROKEN_RETRY_MS
            } else {
                ROLLING_RETRY_MS
            };
            hold.not_before = hold.not_before.min(now + bound);
            Some((hold.not_before, hold.watched))
        });
        if let Some((not_before, watched)) = held
            && now < not_before
        {
            // A watched (re-login-shaped) hold releases the moment the
            // profile's credential files change: the operator just did the
            // thing the cause prescribed, and holding the gate shut for the
            // rest of the leash would delay the very recovery it named. The
            // fingerprint read is metadata-only IO, done OUTSIDE the pacing
            // lock — that rank is a leaf precisely because nothing else
            // happens under it.
            let released =
                watched.is_some_and(|w| w != crate::claude::credential_fingerprint(&name));
            if !released {
                continue;
            }
            if let Ok(mut p) = pacing.lock() {
                p.retry_after_ms.remove(name.as_str());
            }
            logline!(
                "clauth: '{name}' credentials changed under a re-stamp hold — re-checking now"
            );
        }
        if !crate::oauth::rolling_sidecar_restamp_due(&name, now as i64) {
            continue;
        }
        let gate = gate_fn(&name);
        // A standing `auth_broken` changes only via re-login, which re-arms
        // the rolling token anyway — so a quarantined chain takes the Broken
        // leash on EVERY verdict, not just a literal `Broken`. Without this, a
        // quarantined profile whose sidecar is running out its clock lands in
        // the Ready-still-due / Transient arms below and picks up a second
        // 15-minute cadence on top of the poll leg's own backoff, retrying a
        // roll that `roll_from_stored_chain` routes to `ChainStale` by flag.
        // Read AFTER the gate, which is what may have just raised it.
        let flagged = config.lock().ok().is_some_and(|c| c.is_auth_broken(&name));
        let kind = if flagged {
            HoldKind::ReloginShaped
        } else {
            HoldKind::Transient
        };
        match gate {
            // Ready = re-stamped no-spend or degraded to a serving fallback;
            // Refreshed = the rotation hook fed (and, active, mirrored). Both
            // logged at their source. A Ready that left the sidecar STILL due
            // is the degrade leg masking transient chain trouble behind a
            // live mint/bearer (review LOW) — pace it like a transient
            // instead of re-running the gate every scan.
            crate::oauth::AuthGate::Ready | crate::oauth::AuthGate::Refreshed => {
                // The due re-read (a file read) runs BEFORE the pacing lock is
                // taken — the rank is a leaf precisely because nothing else,
                // locks or IO, ever happens under it.
                let still_due = crate::oauth::rolling_sidecar_restamp_due(&name, now as i64);
                let hold = still_due.then(|| retry_hold(&name, now, kind));
                if let Ok(mut p) = pacing.lock() {
                    match hold {
                        Some(hold) => {
                            p.retry_after_ms.insert(name.to_string(), hold);
                        }
                        None => {
                            p.retry_after_ms.remove(name.as_str());
                        }
                    }
                }
            }
            crate::oauth::AuthGate::Transient(e) => {
                logline!(
                    "clauth: re-stamp for '{name}' failed (will retry): {}",
                    e.text_with_status()
                );
                // A transient whose cause only a re-login clears is not
                // transient for pacing purposes: the 15-minute cadence
                // against it re-logs the same refusal ~24 times a bearer
                // lifetime without one of them ever succeeding.
                let kind = if e.permanent_until_relogin() {
                    HoldKind::ReloginShaped
                } else {
                    kind
                };
                let hold = retry_hold(&name, now, kind);
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms.insert(name.to_string(), hold);
                }
            }
            crate::oauth::AuthGate::Broken => {
                let hold = retry_hold(&name, now, HoldKind::ReloginShaped);
                if let Ok(mut p) = pacing.lock() {
                    p.retry_after_ms.insert(name.to_string(), hold);
                }
            }
        }
    }
}

/// Which leash a paced re-stamp hold rides. Every call site KNOWS which one it
/// is minting (the quarantine read, `permanent_until_relogin`, the Broken
/// verdict), so the kind is passed rather than inferred back from the duration
/// — the same move `sidecar_kind_of` made for the other inference: a numeric
/// coincidence is correct at every site today and breaks silently the first
/// time a non-re-login verdict wants a long leash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HoldKind {
    /// The minutes cadence ([`ROLLING_RETRY_MS`]) — genuinely transient, and
    /// the clock is the honest exit.
    Transient,
    /// The noise leash ([`ROLLING_BROKEN_RETRY_MS`]) — re-login-shaped
    /// (permanent transient, quarantined chain, Broken verdict), whose real
    /// exit is a credential file changing, so the hold carries the watch.
    ReloginShaped,
}

/// Build the paced hold for one verdict. The duration and the watch both
/// derive from the KIND, so they cannot disagree. Computed before the pacing
/// lock is taken — the fingerprint is metadata IO, and that rank is a leaf.
fn retry_hold(name: &ProfileName, now: u64, kind: HoldKind) -> RetryHold {
    match kind {
        HoldKind::Transient => RetryHold {
            not_before: now + ROLLING_RETRY_MS,
            watched: None,
        },
        HoldKind::ReloginShaped => RetryHold {
            not_before: now + ROLLING_BROKEN_RETRY_MS,
            watched: Some(crate::claude::credential_fingerprint(name)),
        },
    }
}
