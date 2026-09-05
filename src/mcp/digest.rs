//! The since-your-last-call change digest: what moved in clauth's state since
//! the last reply that reported a digest.
//!
//! A live Claude Code session has no push channel from clauth — MCP
//! `2026-07-28` defines no custom notifications and a server cannot send a
//! request between calls — so this is the pull-shaped answer: every reply that
//! already carries a live-usage footer (`profiles({scope:"session"})`,
//! `switch_profile`, `delegate`, `monitor`) names what moved since the last
//! digest-bearing reply, and `monitor` with no `job_ids` long-polls the same
//! comparison for a caller that wants to block until something moves.
//!
//! Three observables, all local disk, zero network and zero quota on every
//! path including the state-waiting loop:
//!
//! - the config's `active_profile` VALUE (content, not mtime — a rewrite that
//!   keeps the name is not news);
//! - the ACTIVE profile's usage-cache mtime, KEYED on the profile it was read
//!   from and read from whichever cache that profile's OWN fetch leg writes
//!   (`crate::profile_json::usage_cache_file_for`). Two profiles' caches are
//!   different files, so across a profile change the two mtimes are not
//!   comparable and there is no usage-cache event to report;
//! - `~/.claude/.credentials.json`'s mtime, read FOLLOWING symlinks: that
//!   reads the bytes' write time, which moves on both a rotation rewrite and a
//!   `switch` repoint (a different target file carries a different mtime),
//!   where the link's own mtime would move on the repoint only.
//!
//! ## The rules every caller is pinned to
//!
//! - **Shared, not per-clone.** rmcp clones the handler per request; the
//!   baseline lives behind an `Arc` so every clone compares against the same
//!   snapshot. A per-clone baseline would silently report nothing forever.
//! - **A first call reports nothing.** It establishes the baseline; claiming
//!   "nothing changed" would assert a comparison the server never made.
//! - **Reporting consumes; not reporting must not.** A call that reports a
//!   moved observable advances the baseline for exactly the observables it
//!   reported, and a surface that carries no digest at all never touches it
//!   (`profiles`' all-scope roster: it is already a fresh read of the same
//!   state). The usage cache re-keys silently alongside a reported profile
//!   change, because what it held is no longer comparable to anything.
//! - **`switch_profile` never reports its own write.** Its post-mutation arms
//!   reseed the baseline silently (the reply's `previous`/`active` IS the
//!   report); an arm that refused before any mutation reports like the
//!   session-scope roster does, because nothing of ours moved.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::lockorder::RankedMutex;
use crate::lockorder::rank::McpDigest;

/// Poll cadence for `monitor`'s state-waiting long-poll, mirroring the job
/// mode's `JOB_POLL_INTERVAL` so both modes answer on the same rhythm.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Which observables one call watches. There is no filtered subset anymore:
/// `monitor`'s state mode and every folded reply watch all three, so this is a
/// set type only because the delta computation branches per observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WatchSet {
    active_profile: bool,
    usage_cache: bool,
    credentials: bool,
}

impl WatchSet {
    pub(super) const ALL: Self = Self {
        active_profile: true,
        usage_cache: true,
        credentials: true,
    };
}

/// One read of the three observables. An absent file is `None` on that
/// observable, which is a value like any other: appearing and vanishing are
/// both changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DigestSample {
    active_profile: Option<String>,
    /// The usage cache of `active_profile` at sample time — that field is its
    /// key, and two profiles' caches are different files.
    usage_cache_mtime: Option<SystemTime>,
    credentials_mtime: Option<SystemTime>,
}

/// What moved between two samples, restricted to the watched set. Each field
/// carries news only when its observable moved AND was watched; the JSON
/// spelling keeps absent-that-carries-no-news keys out entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DigestDelta {
    /// `Some((from, to))` when the active-profile VALUE moved. The strings are
    /// the payload — a reader acts on the names, never on the mtime twin.
    active_profile: Option<(Option<String>, Option<String>)>,
    usage_cache: bool,
    credentials: bool,
}

impl DigestDelta {
    fn is_empty(&self) -> bool {
        self.active_profile.is_none() && !self.usage_cache && !self.credentials
    }

    /// The `since_your_last_call` object: one key per observable that carries
    /// news, `true` for the two whose only news is "it moved" (an mtime is not
    /// a figure a reader acts on, so it stays out).
    pub(super) fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some((from, to)) = &self.active_profile {
            map.insert(
                "active_profile".to_string(),
                serde_json::json!({ "from": from, "to": to }),
            );
        }
        if self.usage_cache {
            map.insert("usage_cache".to_string(), serde_json::Value::Bool(true));
        }
        if self.credentials {
            map.insert("credentials".to_string(), serde_json::Value::Bool(true));
        }
        serde_json::Value::Object(map)
    }
}

impl DigestSample {
    /// The watched-set delta from `self` (the stored baseline) to `next` (the
    /// fresh sample). Pure: the caller decides what to store.
    fn delta(&self, next: &DigestSample, watched: WatchSet) -> DigestDelta {
        // Across a profile change the two cache mtimes belong to different
        // files, so there is no refresh to report — including when the profile
        // change itself is unwatched, where reporting the incomparable pair
        // would put a false lesser event in its place.
        let same_profile = self.active_profile == next.active_profile;
        DigestDelta {
            active_profile: (watched.active_profile && !same_profile)
                .then(|| (self.active_profile.clone(), next.active_profile.clone())),
            usage_cache: watched.usage_cache
                && same_profile
                && self.usage_cache_mtime != next.usage_cache_mtime,
            credentials: watched.credentials && self.credentials_mtime != next.credentials_mtime,
        }
    }
}

/// A file's last write, following symlinks (`std::fs::metadata`): for the
/// credentials link that is the write time of the BYTES, which moves on both a
/// rotation rewrite and a repoint. Kept at whatever resolution the platform
/// reports (nanoseconds on Linux) — truncating to milliseconds would fold two
/// writes inside one millisecond into one and lose the second.
fn file_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Read the three observables off local disk. The reads are not one atomic
/// snapshot of the world (a writer can land between them), which is fine: the
/// digest is best-effort news, and a torn read surfaces as a delta the next
/// digest-bearing call reports.
fn sample_digest() -> DigestSample {
    let active = crate::profile::active_profile_name();
    // Whichever cache the ACTIVE profile's own fetch leg writes: the
    // third-party leg never touches `usage_cache.json`, so keying an api-key
    // profile on it renders a healthy hourly-refreshed account as never
    // refreshed. Resolving the profile costs one config read per sample, which
    // the state-wait loop pays 5x a second — the same order as the
    // `profiles.toml` read above it, and the price of naming the right file.
    let usage_cache_mtime = active
        .as_ref()
        .and_then(|name| {
            crate::profile_cache::profile_cache_path(
                name,
                crate::profile_json::usage_cache_file_for(name),
            )
        })
        .and_then(|path| file_mtime(&path));
    let credentials_mtime = crate::claude::claude_credentials_path()
        .ok()
        .and_then(|path| file_mtime(&path));
    DigestSample {
        active_profile: active.map(|n| n.to_string()),
        usage_cache_mtime,
        credentials_mtime,
    }
}

/// What one digest comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DigestVerdict {
    /// No baseline existed; this call established one and reports nothing.
    Seeded,
    /// Nothing in the watched set moved. The baseline is untouched — storing
    /// the fresh sample here would swallow an unwatched observable's change.
    Unchanged,
    /// Something in the watched set moved, and the baseline now reflects
    /// exactly the reported observables.
    Changed(DigestDelta),
}

impl DigestVerdict {
    fn delta(self) -> Option<DigestDelta> {
        match self {
            Self::Changed(d) => Some(d),
            Self::Seeded | Self::Unchanged => None,
        }
    }
}

/// The shared since-your-last-call baseline. Cheap to clone: every clone of
/// the server holds the same `Arc`, which is the whole feature — a per-clone
/// baseline never observes a change and reports nothing forever.
#[derive(Clone)]
pub(super) struct DigestTracker {
    shared: Arc<RankedMutex<Option<DigestSample>, McpDigest>>,
}

impl DigestTracker {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(RankedMutex::new(None)),
        }
    }

    /// Sample, compare against the baseline, and consume what this call
    /// reports. The disk sampling runs under the leaf lock so two concurrent
    /// calls cannot interleave sample and compare (and double-report); the
    /// lock never outlives the method, so the state-wait loop's sleep slices
    /// run outside it. A poisoned lock keeps digesting: nothing under it can
    /// panic, and a lost baseline costs one silent reseed, not a dead tool.
    pub(super) fn report(&self, watched: WatchSet) -> DigestVerdict {
        let mut baseline = self.lock();
        let sample = sample_digest();
        let Some(prev) = baseline.as_ref() else {
            *baseline = Some(sample);
            return DigestVerdict::Seeded;
        };
        let delta = prev.delta(&sample, watched);
        if delta.is_empty() {
            return DigestVerdict::Unchanged;
        }
        // Consume exactly the reported observables. An unwatched one stays on
        // its old baseline value, so the next surface watching it still has
        // the change to report.
        if let Some(slot) = baseline.as_mut() {
            if delta.active_profile.is_some() {
                slot.active_profile = sample.active_profile.clone();
                // Re-key the cache baseline in the same step, silently: left on
                // the old profile's file it would read as a refresh on the next
                // call, which is the false report this consume step exists to
                // avoid. Nothing comparable survives the profile move anyway.
                slot.usage_cache_mtime = sample.usage_cache_mtime;
            }
            if delta.usage_cache {
                slot.usage_cache_mtime = sample.usage_cache_mtime;
            }
            if delta.credentials {
                slot.credentials_mtime = sample.credentials_mtime;
            }
        }
        DigestVerdict::Changed(delta)
    }

    /// Replace the baseline with the current state without reporting anything:
    /// `switch_profile`'s post-mutation arms, whose own write must never echo
    /// as news from elsewhere. The whole sample is stored — a switch watches all
    /// three, so nothing it moved survives as a later delta. The accepted cost
    /// is that an external change landing inside the switch window is swallowed
    /// with it; the alternative is the switch reporting its own write.
    pub(super) fn reseed(&self) {
        *self.lock() = Some(sample_digest());
    }

    /// Long-poll for a change in the watched set: check, sleep one
    /// [`WATCH_POLL_INTERVAL`] slice, repeat, until something moves or
    /// `wait_secs` elapses. Mirrors the job mode's `wait_for_done` cadence;
    /// the baseline lock is taken and dropped inside [`report`], never held
    /// across a sleep OR an await. `wait_secs` 0 samples exactly once. Each
    /// slice re-reads `profiles.toml` and two file stats: small local reads
    /// whose total is bounded by `wait_secs`, and a cached value could not see
    /// the writer this loop exists to catch.
    ///
    /// It ticks the same progress sink the job mode does, on the same throttle:
    /// one tool cannot hold two ceilings, and the raised ceiling is only safe on
    /// a peer that receives progress. It races the same cancellation token too,
    /// so a client abandoning the call ends the loop instead of leaving it to
    /// run out an hour against a request id that no longer exists.
    pub(super) async fn watch(
        &self,
        watched: WatchSet,
        wait_secs: u64,
        progress: &mut super::ProgressSink,
    ) -> WatchOutcome {
        let start = Instant::now();
        let deadline = Duration::from_secs(wait_secs);
        let mut cancelled = false;
        loop {
            match self.report(watched) {
                DigestVerdict::Changed(delta) => return WatchOutcome::Changed(delta),
                // A first call with no wait arms the baseline and answers at
                // once; with a wait it keeps polling against the baseline it
                // just established, which is a real comparison from here on.
                DigestVerdict::Seeded if wait_secs == 0 => return WatchOutcome::Armed,
                DigestVerdict::Seeded | DigestVerdict::Unchanged
                    if cancelled || start.elapsed() >= deadline =>
                {
                    return WatchOutcome::Unchanged {
                        waited_secs: start.elapsed().as_secs(),
                    };
                }
                DigestVerdict::Seeded | DigestVerdict::Unchanged => {}
            }
            progress
                .tick(|| {
                    format!(
                        "waiting on clauth's state, {}s of {wait_secs}s",
                        start.elapsed().as_secs()
                    )
                })
                .await;
            cancelled = progress.sleep_or_cancelled(WATCH_POLL_INTERVAL).await;
        }
    }

    fn lock(&self) -> crate::lockorder::RankedGuard<'_, Option<DigestSample>> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Result of `monitor`'s state-waiting long-poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatchOutcome {
    /// No baseline existed and there was no wait: this call set it.
    Armed,
    /// The wait elapsed with nothing in the watched set having moved.
    Unchanged { waited_secs: u64 },
    /// Something moved; the delta is carried (and consumed).
    Changed(DigestDelta),
}

/// How a folded reply treats the digest baseline alongside its live-usage
/// fold. Only [`DigestMode::Report`] can put `since_your_last_call` into the
/// payload.
pub(super) enum DigestMode<'a> {
    /// Report the delta over all three observables and CONSUME it: whatever
    /// this reply names, the next reporting reply no longer carries.
    Report(&'a DigestTracker),
    /// Reseed the baseline silently, for a reply whose own body is the report
    /// of what it did: its write must not echo back as news from elsewhere,
    /// and leaving the baseline stale would echo it on the next call instead.
    Reseed(&'a DigestTracker),
    /// Neither report nor touch the baseline, for a fold that is not where this
    /// call's news belongs. The delta survives for whichever reply does report
    /// it.
    Skip,
}

impl DigestMode<'_> {
    /// The delta to fold into this reply, if any.
    fn delta(self) -> Option<DigestDelta> {
        match self {
            Self::Report(tracker) => tracker.report(WatchSet::ALL).delta(),
            Self::Reseed(tracker) => {
                tracker.reseed();
                None
            }
            Self::Skip => None,
        }
    }

    /// The payload key this mode contributes, when it carries news.
    pub(super) fn folded(self) -> Option<serde_json::Value> {
        self.delta().map(|d| d.to_json())
    }
}
