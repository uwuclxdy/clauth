//! Interleaved auto-start: space the 5h auto-start kick across accounts.
//!
//! `auto_start` ([`crate::oauth::auto_start_kick`]) opens one account's 5h
//! window the moment its last one lapsed. With several accounts opted in they
//! all reopen on lapse and stay in whatever phase they booted with, so every
//! window resets at the same instant and the accounts are collectively dark
//! between resets. This module spaces them: a queue member may open a window
//! only when no other member opened one within `5h / N`. That converges an
//! arbitrary starting configuration to even spacing inside one cycle and holds
//! it there — every window is exactly 5h, so the spacing is self-preserving
//! once established, and a lapsed window can never be the gate (it was opened
//! at least 5h ago, and `gap <= 5h` always).
//!
//! ## Why the anchor is derived from the history SERIES, never one snapshot
//!
//! The tempting anchor is the usage cache already on disk: `max(resets_at) -
//! 5h` over the queue. A single snapshot is NOT sound. `/usage` reports
//! `resets_at` a full duration out for an idle 5h window that never opened
//! ([`crate::usage::window_avg_pace_per_day`]'s contract), and the fetch layer
//! copies that through verbatim — value-identical to a genuine just-kicked
//! window (`utilization: 0`, `resets_at = open + 5h`). Anchoring on it would
//! imply an open at `now` on EVERY poll, pinning the gap shut and starving
//! the whole queue, silently and permanently.
//!
//! The SERIES separates what the snapshot cannot. Across two samples a real
//! window's `resets_at` is a fixed wall-clock boundary — measured 2026-08-20
//! over 2427 reported values, every one within a second of a minute boundary,
//! jittering sub-second between reads — while the idle shape tracks the
//! clock, sliding forward by exactly the time between the samples. clauth
//! already persists that series per profile (`usage_history.jsonl`, 2-day
//! retention, [`crate::profile::load_usage_history`]), so the anchor needs no
//! state of its own: [`history_anchor`] replays it and keeps the newest
//! boundary CONFIRMED by two agreeing samples.
//!
//! That replay is the GATE's read, not just a startup seed ([`queue_anchor`]).
//! It has to be: the rule above is "no other member OPENED one", and an open
//! needs no kick of ours behind it — a live Claude Code session on a member
//! account opens that account's window, and the series is the only place that
//! shows up. Anchoring on our own kicks alone would space the queue against
//! half the openings it has to space against.
//!
//! ## The one opening this queue does NOT gate
//!
//! `clauth use <name>` primes its target's window through
//! [`crate::oauth::prime_window`] — the second caller of
//! [`crate::oauth::auto_start_kick`] — and that path checks neither the toggle
//! nor the anchor. It is an override on purpose: the operator typed a switch
//! because they are about to work on that account, and a `use` that silently
//! declined to prime would be a worse lie than a collapsed gap. It also cannot
//! practically be gated, because the CLI is a one-shot process with no
//! scheduler and no share of this queue's memory.
//!
//! What the override costs is one interval of spacing, not the invariant: the
//! primed window lands in that profile's history like any other, so the next
//! daemon tick that consults [`queue_anchor`] confirms it, re-anchors on it,
//! and re-spaces the remaining members around it inside one cycle. Before the
//! anchor became a per-gate read this was NOT true — a manual prime was
//! invisible to the gate forever, and the member behind it could be kicked
//! seconds later.
//!
//! One thing in that file is NOT an observation: the writer re-stamps the
//! previous payload 1ms before each changed sample (a bridge, so idle
//! stretches keep their temporal density for the burn replay). A bridge pairs
//! old data with a new timestamp — exactly the held-still evidence the
//! classifier trusts — so the series reader drops any entry whose 5h window
//! repeats its predecessor's before classifying ([`member_series`]; review
//! round 2, where idle histories self-confirmed through them).
//!
//! The trade, named: a history line lands only with a fresh `/usage` body,
//! and `/usage` can 429 for minutes right after a kick. A process death
//! inside that gap forgets the open — the restarted queue may elect
//! immediately, open one window early, and re-space within a cycle. Dropping
//! bridges is what keeps this trade honest in BOTH directions: a pre-crash
//! idle bridge can neither stand in for the forgotten open nor hold the queue
//! shut after a restart. One redundant ~1-token kick, self-healing, in
//! exchange for no file of its own, no extra write path, and no trusting of
//! single unconfirmable readings.

use std::collections::HashMap;
use std::sync::Arc;

use crate::lockorder::{RankedMutex, rank};
use crate::profile::{AppConfig, ProfileName};

/// The rolling window length the whole queue is phased against. Anthropic's 5h
/// limit window, the same span [`crate::usage::scheduler`] synthesizes when a
/// kick lands.
pub(crate) const FIVE_HOUR_SECS: i64 = 5 * 3600;

/// Ceiling on the jitter tolerance subtracted from the queue gap. The tolerance
/// exists so a tick landing a few seconds after a window lapses cannot ratchet
/// the whole queue later by one interval every cycle; one tick's worth is all
/// that needs forgiving. It is NOT `refresh_interval_ms` itself, which is user
/// settable up to an hour — a 30-minute interval would otherwise cut the N=3
/// floor from 1h40m to 1h10m, silently undoing most of the spacing.
const MAX_GAP_TOLERANCE_SECS: i64 = 300;

/// Two samples of one real window agree on its reset to within the API's own
/// quantization: sub-second recompute jitter, plus the one-way minute floor a
/// landed kick exposes — the synthetic `mark_window_open` stamp is `kick + 5h`
/// exact, and the next fresh body reports the same window floored to the
/// minute, up to 59s earlier.
const BOUNDARY_AGREE_SECS: i64 = 61;

/// How closely `resets_at` must track the clock across a pair to read as the
/// sliding idle shape (the recompute jitter is sub-second; a real boundary
/// does not track the clock at all).
const SLIDE_MATCH_SECS: i64 = 15;

/// Minimum spacing between two samples for the slide judgment to mean
/// anything: the kick path writes same-instant duplicate lines (the synthetic
/// stamp, then the fresh body), and below this spacing a slide is
/// indistinguishable from jitter anyway. Below it a pair confirms only
/// through the one-way kick band in [`confirmed_boundary`] — the refresh
/// interval is settable down to 10s, inside this baseline, and a symmetric
/// agreement test there would confirm the idle shape it cannot see slide.
const SLIDE_BASELINE_SECS: i64 = 30;

/// Consecutive failed elected kicks before the election skips a member.
///
/// Without a skip rule one permanently kick-incapable member starves every
/// member behind it: the macOS carve-out in [`crate::oauth::auto_start_kick`]
/// refuses to rotate while a live Claude Code session reads the profile's
/// Keychain item, and returns with no rate-limit metadata — so no `KickBlock`
/// is recorded, `auth_broken` is never set, and the member stays permanently
/// due and invisible to every other exclusion. Elected, it fails, every tick,
/// for as long as the session lives.
pub(crate) const MAX_ELECTION_FAILURES: u32 = 2;

/// How long a member's failure streak stands before the election forgives it.
///
/// Without a decay the skip is permanent for the process lifetime: a skipped
/// member is never elected, so it never lands a kick, so its streak never
/// clears — one transient blockage would eject an account from the queue for
/// good. The condition that causes the macOS carve-out (a live Claude Code
/// session holding the profile's Keychain item) ends on its own, so the streak
/// has to as well. An hour is long enough that a genuinely stuck member is not
/// retried on every gap, and short enough that a recovered one rejoins within
/// one window.
const FAILURE_DECAY_SECS: i64 = 3600;

/// In-memory queue state: the anchor plus the per-profile election health.
/// Neither survives a restart on its own — the anchor is re-derived from the
/// usage-history series ([`history_anchor`]), and a failing member re-proves
/// itself on the next run rather than inheriting a stale verdict.
#[derive(Debug, Default)]
pub(crate) struct AutoStartQueue {
    /// Epoch secs of the queue's most recent window open, `None` until a kick
    /// lands or the history seed supplies one.
    pub(crate) last_open_at: Option<i64>,
    /// Consecutive failed elected kicks, per profile, with the epoch secs of
    /// the most recent one. Cleared when a kick lands, and forgiven wholesale
    /// once [`FAILURE_DECAY_SECS`] has passed (see there for why a permanent
    /// streak would be a bug rather than a safeguard).
    pub(crate) failures: HashMap<ProfileName, (u32, i64)>,
    /// Memo for the history replay: the members' [`history_signature`] at the
    /// last derivation, and what that derivation produced. Holds the DERIVED
    /// value, never the composed anchor — `last_open_at` moves on its own when
    /// a kick lands, and folding the two into one memo would lose that.
    pub(crate) derived: Option<(u64, Option<i64>)>,
}

/// Shared queue state. Leaf lock, same discipline as [`crate::usage::KickBlocks`]:
/// read or updated alone, released before any other lock, and its disk IO stays
/// outside the guard.
pub(crate) type AutoStartQueueState = Arc<RankedMutex<AutoStartQueue, rank::AutoStartQueue>>;

/// A fresh, empty queue state.
pub(crate) fn new_state() -> AutoStartQueueState {
    Arc::new(RankedMutex::new(AutoStartQueue::default()))
}

/// Even spacing for a queue of `n` kick-capable members, less a jitter
/// tolerance capped at [`MAX_GAP_TOLERANCE_SECS`].
///
/// `n <= 1` yields `FIVE_HOUR_SECS` less the tolerance, which is what makes the
/// whole feature a no-op for a single account: that member's own window start
/// IS the last queue open, so at lapse `now - last_open == 5h`, comfortably past
/// the gap. Nothing about a one-account setup changes.
pub(crate) fn queue_gap_secs(n: usize, interval_ms: u64) -> i64 {
    let members = n.max(1) as i64;
    let tolerance = ((interval_ms / 1000) as i64).min(MAX_GAP_TOLERANCE_SECS);
    (FIVE_HOUR_SECS / members).saturating_sub(tolerance).max(1)
}

/// The gate: may a queue member open a window now?
///
/// A queue with no anchor at all (cold start, nothing persisted, nothing
/// derivable) is due — otherwise the first ever auto-start could never happen.
pub(crate) fn queue_due(last_queue_open: Option<i64>, now_secs: i64, gap_secs: i64) -> bool {
    last_queue_open.is_none_or(|t| now_secs.saturating_sub(t) >= gap_secs)
}

/// The newest 5h window open the queue can PROVE from the usage-history
/// series: the max over `members` of each profile's newest two-sample-confirmed
/// boundary, less the window length. `None` when no member's history confirms
/// one — cold start, which the gate reads as "due".
pub(crate) fn history_anchor(members: &[ProfileName]) -> Option<i64> {
    members
        .iter()
        .filter_map(|name| series_open(&member_series(name)))
        .max()
}

/// A cheap fingerprint of every member's history FILE, without reading one:
/// each `usage_history.jsonl`'s mtime folded with its length.
///
/// The series cannot change while this does not, which is what lets
/// [`queue_anchor`] skip a replay it would only repeat the answer to. Length
/// rides along with mtime because the writer appends, so a torn same-millisecond
/// pair still moves the fingerprint; a missing file folds in a constant, so a
/// profile with no history yet is stable rather than absent.
///
/// Exists for one failure mode, which the due/lapsed bounds do NOT cover: while
/// a member's kick and its `/usage` refresh are both failing, that member stays
/// lapsed and the queue stays due, so the election keeps probing it (by design —
/// see [`elect_queue_member`]) and would otherwise re-parse every member's file
/// on the refresh cadence for as long as the outage lasts. No fresh body lands
/// during one, so nothing is appended, so this is exactly the period the
/// fingerprint holds still (review round 4).
fn history_signature(members: &[ProfileName]) -> u64 {
    members.iter().fold(0u64, |acc, name| {
        let stamp = crate::profile::profile_history_path(name)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                mtime.rotate_left(17) ^ m.len()
            })
            .unwrap_or(1);
        // Order-sensitive fold: `members` is a stable sorted order, so two
        // profiles swapping stamps cannot collide back onto the same value.
        acc.rotate_left(7) ^ stamp
    })
}

/// One profile's `(sample_secs, resets_at_secs)` series for the 5h window,
/// chronological, entries without a parseable reset dropped — and BRIDGE lines
/// dropped first.
///
/// [`crate::profile::append_usage_sample`] re-stamps the store's previous
/// payload 1ms before each changed sample so an idle stretch keeps its
/// temporal density for the burn replay. A re-stamp is not an observation:
/// its 5h window was read at the ORIGINAL sample time, so counting its new
/// timestamp manufactures exactly the "boundary held still while the clock
/// advanced" evidence the classifier trusts — an idle profile bridges on
/// every poll, and the pair `(fresh, bridge)` has `dr = 0` (review round 2).
///
/// The drop is judged on the classifier's OWN projection — the `five_hour`
/// serialization — not whole-payload identity, because the store can drift in
/// non-window fields between appends: the `/profile`-despite-429 overlay
/// advances `plan` in place with no history line, so the next bridge differs
/// from its file predecessor in `plan` alone while re-stamping the same 5h
/// window. Whatever else drifted, an entry whose 5h window is byte-identical
/// to its predecessor's adds no window observation and is dropped. The one
/// bridge that carries NEW window information — the synthetic stamp
/// [`crate::usage::scheduler`]'s `mark_window_open` put in the store — differs
/// in `five_hour` and stays. Dropping a real line that changed only outside
/// the 5h window can only under-confirm, which is the safe direction (module
/// docs: a skipped open makes the queue MORE willing to kick, once).
fn member_series(name: &ProfileName) -> Vec<(i64, i64)> {
    let mut prev_window: Option<String> = None;
    crate::profile::load_usage_history(name)
        .into_iter()
        .filter(move |(_, info)| {
            let window = serde_json::to_string(&info.five_hour).ok();
            let bridge = window.is_some() && window == prev_window;
            prev_window = window;
            !bridge
        })
        .filter_map(|(ts_ms, info)| {
            let reset = info
                .five_hour
                .as_ref()?
                .resets_at
                .as_deref()
                .and_then(crate::usage::iso_to_epoch_secs)?;
            Some(((ts_ms / 1000) as i64, reset))
        })
        .collect()
}

/// Newest CONFIRMED open in one profile's series: scan the consecutive pairs
/// newest-first for the first that fixes a boundary, then boundary → open.
///
/// A trailing sliding run (the profile is idle NOW) or a trailing unconfirmed
/// jump (a window seen exactly once) is skipped in favour of the last boundary
/// two samples agree on — a single reading is exactly what cannot be trusted
/// (module docs). Skipping a real-but-unconfirmed open is the safe direction:
/// it can only make the queue MORE willing to kick, once, and the kick's own
/// history line confirms it within a poll.
pub(crate) fn series_open(series: &[(i64, i64)]) -> Option<i64> {
    series
        .windows(2)
        .rev()
        .find_map(|w| confirmed_boundary(w[0], w[1]))
        .map(|boundary| boundary - FIVE_HOUR_SECS)
}

/// The pair rule: `(ts, resets_at)` twice, chronological. A real boundary
/// holds still while the clock advances; the idle shape moves WITH it.
///
/// At or past [`SLIDE_BASELINE_SECS`] the slide is judged first, because the
/// two tests overlap when the samples sit less than one boundary-tolerance
/// apart: a slide across a 45s gap advances the reset by 45s, inside
/// [`BOUNDARY_AGREE_SECS`] — but it matches the clock, which no real window's
/// sub-second jitter does.
///
/// BELOW the baseline the clock has not moved enough to judge a slide at all,
/// so only the kick signature confirms: the exact synthetic stamp then the
/// API's minute-floored report of the same window, `dr` in
/// `[-BOUNDARY_AGREE_SECS, +1]` (the floor only rounds down; the +1 absorbs
/// epoch truncation of two floored reads). The band is one-way on purpose —
/// an idle pair this close has `dr = +dt`, at least `+10`
/// ([`crate::profile::MIN_REFRESH_INTERVAL_MS`]), and a symmetric band would
/// re-admit the very shape the slide test exists to reject on every cadence
/// the interval setting allows below the baseline.
///
/// Of an agreeing pair the LARGER value wins: when the exact synthetic stamp
/// and the minute-floored report disagree, the larger IS the exact one.
fn confirmed_boundary((ts1, r1): (i64, i64), (ts2, r2): (i64, i64)) -> Option<i64> {
    let (dr, dt) = (r2 - r1, ts2 - ts1);
    let confirmed = if dt >= SLIDE_BASELINE_SECS {
        (dr - dt).abs() > SLIDE_MATCH_SECS && dr.abs() <= BOUNDARY_AGREE_SECS
    } else {
        (-BOUNDARY_AGREE_SECS..=1).contains(&dr)
    };
    confirmed.then_some(r1.max(r2))
}

/// Ordered queue membership: chain position first, then display order.
/// Excludes members that cannot open a window.
///
/// ONE rule for the three surfaces that ask it — the scheduler's per-tick
/// election, the `status.json` feed, and the TUI's queue chips. They derived it
/// separately once and disagreed about a hybrid (an OAuth pair stored behind a
/// `base_url`): the scheduler kicks it, the feed published it as slotless. The
/// kick spends the stored OAuth access token and never routes through
/// `base_url` ([`crate::oauth::auto_start_kick`]), so holding that token is the
/// membership question, not where the account's `/v1/messages` go.
///
/// The exclusions are the ones that mean "cannot open a window", because a
/// member that can never kick would still take a slot and thin everyone else's
/// share of the 5h span: user-disabled, `auth_broken`, no stored OAuth pair,
/// a canceled subscription ([`crate::fallback::is_canceled`] — its windows are
/// gone, a kick cannot open one), and a switch-grade kick block. `blocked` is that last set, which only a
/// caller holding the kick blocks can supply
/// ([`crate::usage::switch_grade_kick_lifts`]); pass an empty slice where none
/// is available.
///
/// Empty when the queue toggle is off, which is what makes the toggle a real
/// off switch on every surface at once: no queue, so nothing to elect, publish,
/// or render.
pub(crate) fn auto_start_queue_members(
    config: &AppConfig,
    blocked: &[ProfileName],
) -> Vec<ProfileName> {
    if !config.state.auto_start_queue {
        return Vec::new();
    }
    // The user's own stated preference order first, then anything else in
    // display order — the sort key is `(rank, name)`, so every non-chain
    // member shares one rank and the name breaks the tie.
    let rank = |name: &ProfileName| {
        config
            .state
            .fallback_chain
            .iter()
            .position(|c| c == name)
            .unwrap_or(usize::MAX)
    };
    let mut members: Vec<&ProfileName> = config
        .profiles
        .iter()
        .filter(|p| {
            p.auto_start
                && !p.is_disabled()
                && p.access_token().is_some()
                && !config.is_auth_broken(&p.name)
                && !crate::fallback::is_canceled(p)
                && !blocked.contains(&p.name)
        })
        .map(|p| &p.name)
        .collect();
    members.sort_by_key(|n| (rank(n), n.as_str()));
    members.into_iter().cloned().collect()
}

/// One member's view of the queue, for a surface that renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueSlot {
    /// 1-based position in [`auto_start_queue_members`] order.
    pub(crate) position: usize,
    /// Queue size — the `N` the gap is `5h / N` of.
    pub(crate) total: usize,
    /// Secs until the gap clears, `None` once it has (the queue is due and the
    /// next tick elects). SHARED by every member, because the queue gates
    /// globally rather than per profile, and an ESTIMATE: any window opened out
    /// of band moves the anchor as soon as the scheduler notices.
    pub(crate) next_in: Option<i64>,
}

/// `name`'s slot in `members`, or `None` when it holds none.
///
/// Takes the anchor rather than reading it, so a render loop can take the
/// AutoStartQueue lock once per frame — and, where a Config guard is involved, take
/// it in rank order (AutoStartQueue 240 < Config 400).
pub(crate) fn queue_slot(
    members: &[ProfileName],
    name: &str,
    anchor: Option<i64>,
    interval_ms: u64,
    now_secs: i64,
) -> Option<QueueSlot> {
    let position = members.iter().position(|m| m.as_str() == name)? + 1;
    let gap = queue_gap_secs(members.len(), interval_ms);
    Some(QueueSlot {
        position,
        total: members.len(),
        next_in: anchor
            .map(|at| at.saturating_add(gap).saturating_sub(now_secs))
            .filter(|secs| *secs > 0),
    })
}

/// One queue member's candidacy for this tick's election.
pub(crate) struct Candidate<'a> {
    pub(crate) name: &'a str,
    /// Its 5h window has lapsed, so it wants one opened.
    pub(crate) lapsed: bool,
    /// Consecutive failed elected kicks ([`MAX_ELECTION_FAILURES`]).
    pub(crate) failures: u32,
}

/// Elect at most ONE member to open a window this tick, in queue order.
///
/// Electing in the tick — rather than letting each fetch worker decide — is
/// what serialises the queue: [`crate::usage::scheduler`] fans out one worker
/// per due profile, and two workers reading the same anchor in the same tick
/// would both kick and land on top of each other.
///
/// Members past [`MAX_ELECTION_FAILURES`] are skipped so a permanently
/// kick-incapable account cannot head-of-line block the ones behind it. When
/// EVERY lapsed member is past the limit the first is elected anyway: a queue
/// where everything is failing must keep probing rather than go quietly silent,
/// and the failure path costs one request.
pub(crate) fn elect_queue_member<'a>(candidates: &[Candidate<'a>]) -> Option<&'a str> {
    candidates
        .iter()
        .find(|c| c.lapsed && c.failures < MAX_ELECTION_FAILURES)
        .or_else(|| candidates.iter().find(|c| c.lapsed))
        .map(|c| c.name)
}

/// Fold a landed kick into the queue: it becomes the new anchor and its
/// member's failure streak clears. In-memory only — durability is the history
/// line the kick's next fresh `/usage` body appends, which [`history_anchor`]
/// replays after a restart.
pub(crate) fn note_queue_open(state: &AutoStartQueueState, name: &ProfileName, now_secs: i64) {
    if let Ok(mut queue) = state.lock() {
        queue.last_open_at = Some(now_secs);
        queue.failures.remove(name);
    }
}

/// Fold a failed elected kick into the queue. The anchor does NOT move — nothing
/// opened — so the next tick re-elects, and this member steps toward the skip
/// threshold.
pub(crate) fn note_queue_kick_failed(
    state: &AutoStartQueueState,
    name: &ProfileName,
    now_secs: i64,
) {
    if let Ok(mut queue) = state.lock() {
        let entry = queue.failures.entry(name.clone()).or_insert((0, now_secs));
        // A streak already past its decay starts over rather than resuming.
        entry.0 = if decayed(entry.1, now_secs) {
            1
        } else {
            entry.0.saturating_add(1)
        };
        entry.1 = now_secs;
    }
}

/// Whether a failure stamped at `at` has aged out of the election's memory.
pub(crate) fn decayed(at: i64, now_secs: i64) -> bool {
    now_secs.saturating_sub(at) >= FAILURE_DECAY_SECS
}

/// This member's live elected-kick failure streak: 0 when absent, poisoned, or
/// decayed ([`FAILURE_DECAY_SECS`]).
pub(crate) fn queue_failures(
    state: &AutoStartQueueState,
    name: &ProfileName,
    now_secs: i64,
) -> u32 {
    state
        .lock()
        .ok()
        .and_then(|r| r.failures.get(name).copied())
        .filter(|(_, at)| !decayed(*at, now_secs))
        .map(|(n, _)| n)
        .unwrap_or(0)
}

/// Best-effort earliest moment the queue may open its next window: `anchor`
/// plus the gap for a queue of `n`. `None` without an anchor, which reads as
/// "due now".
///
/// An ESTIMATE, not a promise. A window opened out of band moves the anchor as
/// soon as two history samples confirm it ([`queue_anchor`]), so a published
/// value can shift out from under a reader. Three sources, none of them gated
/// by this queue: a live Claude Code session on a member account, the web app,
/// and `clauth use` — the CLI switch primes its target through
/// [`crate::oauth::prime_window`], which is the OTHER caller of
/// [`crate::oauth::auto_start_kick`] and deliberately consults neither the
/// toggle nor the anchor (see the module header's override paragraph).
pub(crate) fn next_queue_open_secs(anchor: Option<i64>, n: usize, interval_ms: u64) -> Option<i64> {
    anchor.map(|at| at.saturating_add(queue_gap_secs(n, interval_ms)))
}

/// Load the history-derived anchor into memory.
///
/// Called on the thread that SPAWNS the refresher, never inside it: nothing
/// joins that thread, so a home-derived path resolved on it could outlive a
/// test's `HOME_OVERRIDE` and read the operator's real home (the same
/// convention `sync_kick_blocks_from_cache` follows).
pub(crate) fn seed_queue_anchor(state: &AutoStartQueueState, members: &[ProfileName]) {
    let Some(open) = history_anchor(members) else {
        return;
    };
    if let Ok(mut queue) = state.lock() {
        queue.last_open_at = Some(open);
    }
}

/// The queue's in-memory anchor, no disk read. `None` when nothing has opened.
///
/// The RENDER read. [`queue_anchor`] falls through to the history derivation —
/// a per-profile file replay, right for a tick, wrong for a frame. The
/// scheduler seeds this value on the thread that spawns it
/// ([`seed_queue_anchor`]) and refreshes it on every landed kick, so a surface
/// sharing the same [`AutoStartQueueState`] reads the same anchor the gate does.
pub(crate) fn queue_anchor_cached(state: &AutoStartQueueState) -> Option<i64> {
    state.lock().ok()?.last_open_at
}

/// The GATE's anchor: the newer of the in-memory value and the history
/// derivation ([`history_anchor`]), folded back into memory.
///
/// The derivation is re-run rather than short-circuited on the cached value,
/// because a window can open with no kick of ours behind it — a real Claude
/// Code session on a member account opens one, and the queue must space
/// against that exactly as it spaces against its own kicks (the whole rule is
/// "no OTHER MEMBER opened one within `5h / N`", not "no other member was
/// kicked by us"). Only [`note_queue_open`] writes the in-memory anchor, and
/// only for a kick this process fired, so nothing else would ever notice: the
/// startup seed would be the last word for the life of the process, and the
/// member behind an out-of-band open could be elected seconds after it —
/// re-collapsing the two windows the queue had just spaced (review round 4).
///
/// Re-run only where it can change an outcome, which is what keeps the disk
/// replay off the common tick. Two gates, both exact rather than approximate:
///
///   * The cached anchor already says NOT due. The two values compose by
///     `max`, so a replay can only move the anchor FORWARD, and forward is
///     deeper into the gap — a cached anchor inside it cannot be flipped to
///     due by one.
///   * No member is lapsed, so no member wants a window. The caller
///     ([`crate::usage::scheduler`]'s election) returns before asking, since
///     an election no one can win stamps every member shut whichever way the
///     gate answers.
///
/// Without those the replay would run on most ticks of every cycle, not the
/// rare one: an idle 5h window's `resets_at` slides with the clock, so a
/// polled account appends history on EVERY poll, and the queue sits due with
/// nothing lapsed for the whole stretch between its last open and the next
/// window to run out.
///
/// Past both, a third bound covers the case they miss — an outage, where a
/// member's kick AND its `/usage` refresh keep failing, so it stays lapsed, the
/// queue stays due, and the election keeps probing it on the refresh cadence.
/// The derivation is memoized against [`history_signature`], so the parse only
/// repeats once the files actually move; during an outage no fresh body lands,
/// so nothing is appended, and the memo answers instead.
pub(crate) fn queue_anchor(
    state: &AutoStartQueueState,
    members: &[ProfileName],
    now_secs: i64,
    gap_secs: i64,
) -> Option<i64> {
    let cached = queue_anchor_cached(state);
    if !queue_due(cached, now_secs, gap_secs) {
        return cached;
    }
    // A stat per member, not a parse per member: past this the replay only runs
    // when the files themselves moved ([`history_signature`]).
    let signature = history_signature(members);
    let memo = state
        .lock()
        .ok()
        .and_then(|queue| queue.derived)
        .and_then(|(seen, derived)| (seen == signature).then_some(derived));
    // Derived outside the guard — it replays a file per member.
    let derived = match memo {
        Some(derived) => derived,
        None => history_anchor(members),
    };
    let anchor = derived.max(cached);
    if let Ok(mut queue) = state.lock() {
        queue.derived = Some((signature, derived));
        if anchor != cached {
            queue.last_open_at = anchor;
        }
    }
    anchor
}

#[cfg(test)]
#[path = "../../tests/inline/auto_start_queue.rs"]
mod tests;
