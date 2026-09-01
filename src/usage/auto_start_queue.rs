//! Interleaved auto-start: space the 5h auto-start kick across accounts.
//!
//! `auto_start` ([`crate::oauth::auto_start_kick`]) opens one account's 5h
//! window the moment its last one lapsed. With several accounts opted in they
//! all reopen on lapse and stay in whatever phase they booted with, so every
//! window resets at the same instant and the accounts are collectively dark
//! between resets. This module spaces them: a queue member may open a window
//! only when no other PROFILE opened one within `5h / N` (the anchor replays
//! every profile's history, member or not — a window open is a window open,
//! whoever holds it). That converges an arbitrary starting configuration to
//! even spacing inside one cycle and holds it there — every window is exactly
//! 5h, so the spacing is self-preserving once established, and a lapsed
//! window can never be the gate (it was opened at least 5h ago, and
//! `gap <= 5h` always).
//!
//! ## Why the anchor is derived from the history SERIES, never one snapshot
//!
//! A real window's `resets_at` is a fixed wall-clock boundary held for the
//! window's whole 5h life, so an open proves itself by PERSISTING across
//! readings — one snapshot proves nothing, whatever its value: it can be a
//! torn reading, a window seen exactly once, or a stale line from a lapsed
//! window. clauth already persists that series per profile
//! (`usage_history.jsonl`, 2-day retention, [`crate::profile::load_usage_history`]),
//! so the anchor needs no state of its own: [`history_anchor`] replays it and
//! keeps the newest boundary CONFIRMED by persistence ([`series_open`]).
//!
//! The real idle shape, measured 2026-09-01 over a live profile's
//! `usage_history.jsonl`: an idle 5h window that never opened holds ONE
//! minute boundary for ~4.7 hours (same parsed value, band ≤ 2s, ±1s
//! sub-second oscillation around the minute boundary, ~80 samples, ts span
//! 280 min), then jumps forward ~4.7h when the window rolls (re-anchor
//! ε = 5h+ts−resets_at measured 5–26s). It does NOT slide with the clock the
//! way the first classifier assumed, so any equality-based rule under ~4.7h
//! reads an idle hold as a real boundary — the span pass below included. The
//! harm of that false anchor is bounded and accepted, not a reason to change
//! the design: the anchor is the hold's FIXED start (value − 5h ≈ the instant
//! the previous window lapsed), it ages with the clock, the queue reads due
//! within one gap, and the first elected kick re-anchors it exactly (its
//! marker below).
//!
//! Two passes read the series ([`series_open`]). OUR OWN kicked windows
//! confirm on their MARKER: `mark_window_open` stamps the synthetic store
//! entry it writes with `open_at`, the history writer bridges that stamp into
//! the file ahead of the next fresh body, and a marked line is confirmed by
//! the poll that reports its window back — normally the kick's own fetch
//! seconds later, or a later poll if that fetch 429s. The anchor is the
//! marker's exact `open_at`, never re-derived from the readings around it.
//! Everything ELSE confirms through the SPAN pass: an unmarked boundary held
//! within [`BOUNDARY_JITTER_SECS`] across readings at least [`SPAN_MIN_SECS`]
//! apart. That pass is what makes the gate's rule real — the rule above is
//! "no other PROFILE opened one", and an open needs no kick of ours behind
//! it: a live Claude Code session on any profile, the web app, or
//! the switch prime opens a window that only the series shows. Anchoring on our
//! own kicks alone would space the queue against half the openings it has to
//! space against, so the replay is the GATE's read, not just a startup seed
//! ([`queue_anchor`]).
//!
//! ## The one opening this queue does NOT gate
//!
//! The switch action primes its target's window through
//! [`crate::oauth::prime_window`] — the second caller of
//! [`crate::oauth::auto_start_kick`] — and that path checks neither the toggle
//! nor the anchor. It is an override on purpose: the operator switched because
//! they are about to work on that account, and a switch that silently declined
//! to prime would be a worse lie than a collapsed gap. It also cannot
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
//! old data with a new timestamp — exactly the held-still-across-time
//! evidence the span pass trusts — so the series reader drops any entry whose
//! 5h window repeats its predecessor's before classifying ([`member_series`];
//! review round 2, where idle histories self-confirmed through them). The
//! synthetic stamp is the one bridge that carries NEW window information, so
//! it survives the drop — and it is the line the kick marker rides in on.
//!
//! The trade, named: the kick marker lives only in the store until the next
//! fresh `/usage` body bridges it into the history file, and `/usage` can 429
//! for minutes right after a kick. A process death inside that gap forgets
//! the open — the restarted queue may elect immediately, open one window
//! early, and re-space within a cycle; a death after the body landed loses
//! nothing, because the marked bridge line replays the exact kick. Dropping
//! bridges is what keeps this trade honest in BOTH directions: a pre-crash
//! idle bridge can neither stand in for the forgotten open nor hold the queue
//! shut after a restart. One redundant ~1-token kick, self-healing, in
//! exchange for no file of its own (the marker is a field, not a stamp file),
//! no extra write path, and no trusting of single unconfirmable readings.

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

/// 61 seconds, in two roles, both pinned to the idle shape's minute
/// quantization.
///
/// As the span minimum: a boundary must hold across samples at least this far
/// apart, so the span outlasts the idle minute step of 60s — a minute-quantized
/// idle run steps +60 at every minute boundary, so two readings 61s apart in
/// one can never agree within [`BOUNDARY_JITTER_SECS`].
///
/// As the marker band's lower edge: the synthetic `mark_window_open` stamp is
/// `kick + 5h` exact, and the API reports the same window floored to the
/// minute — up to 59s earlier, plus one second of epoch truncation and one of
/// sub-second recompute jitter.
const SPAN_MIN_SECS: i64 = 61;

/// How far two resets may differ and still be one boundary: sub-second
/// recompute jitter plus a parse-truncation straddle. Measured 2026-09-01:
/// real boundaries hold exactly at parsed-second resolution, and idle values
/// oscillate ±1s around their minute boundary — so 2 admits both real reads,
/// and the span is the discriminator, not the jitter. Widening this toward
/// the 60s minute step would re-admit the quantized idle shape the span rule
/// exists to keep out.
const BOUNDARY_JITTER_SECS: i64 = 2;

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
    /// Memo for the history replay: the profiles' [`history_signature`] at the
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
///
/// At `n >= 60` the capped tolerance swallows the whole share
/// (`5h / 60 == 300s == MAX_GAP_TOLERANCE_SECS`), so the gap saturates at the
/// 1s floor: a 60+ member queue degenerates to back-to-back opens. Named, not
/// prevented — nobody runs 60 accounts, and the floor keeps the gate
/// arithmetic total.
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
/// series: the max over `profiles` of each profile's newest confirmed open
/// ([`series_open`] — marker-confirmed for our own kicks, span-confirmed for
/// everything else). `profiles` is the FULL config profile list, never the
/// queue members: a window open is a window open, whoever holds it, and an
/// open on a non-member gates the queue exactly like a member's. `None` when
/// no profile's history confirms one — cold start, which the gate reads as
/// "due".
pub(crate) fn history_anchor(profiles: &[ProfileName]) -> Option<i64> {
    profiles
        .iter()
        .filter_map(|name| series_open(&member_series(name)))
        .max()
}

/// A cheap fingerprint of every profile's history FILE, without reading one:
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
/// see [`elect_queue_member`]) and would otherwise re-parse every profile's file
/// on the refresh cadence for as long as the outage lasts. No fresh body lands
/// during one, so nothing is appended, so this is exactly the period the
/// fingerprint holds still (review round 4).
fn history_signature(profiles: &[ProfileName]) -> u64 {
    profiles.iter().fold(0u64, |acc, name| {
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
        // Order-sensitive fold: `profiles` is a stable declared order (the
        // config's file order), so two profiles swapping stamps cannot collide
        // back onto the same value.
        acc.rotate_left(7) ^ stamp
    })
}

/// One profile's `(sample_secs, resets_at_secs, open_at)` series for the 5h
/// window, chronological, entries without a parseable reset dropped — and
/// BRIDGE lines dropped first. `open_at` rides through so the marker pass can
/// see it; `None` on every line except the kick's synthetic stamp.
///
/// [`crate::profile::append_usage_sample`] re-stamps the store's previous
/// payload 1ms before each changed sample so an idle stretch keeps its
/// temporal density for the burn replay. A re-stamp is not an observation:
/// its 5h window was read at the ORIGINAL sample time, so counting its new
/// timestamp manufactures exactly the "boundary held still while the clock
/// advanced" evidence the span pass trusts — an idle profile bridges on
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
/// in `five_hour` and stays, so its `open_at` marker survives on the window
/// difference alone, never on the marker field. Dropping a real line that
/// changed only outside the 5h window can only under-confirm, which is the
/// safe direction (module docs: a skipped open makes the queue MORE willing to
/// kick, once).
fn member_series(name: &ProfileName) -> Vec<(i64, i64, Option<i64>)> {
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
            Some(((ts_ms / 1000) as i64, reset, info.open_at))
        })
        .collect()
}

/// Newest CONFIRMED open in one profile's series, as the max of two passes —
/// the marker pass over our own kicked windows and the span pass over every
/// unmarked boundary. `None` when neither confirms one — cold start, which the
/// gate reads as "due".
///
/// The MARKER pass confirms a landed kick on the poll that reports its window
/// back: the synthetic stamp `mark_window_open` writes carries
/// [`crate::usage::UsageInfo::open_at`], and a marked line is confirmed once
/// ANY later non-marked line reports a reset within [`SPAN_MIN_SECS`] below
/// (the API's minute floor) or 1s above (epoch truncation) its own. The
/// anchor is the marker's own `open_at` — exact by construction, never
/// re-derived from the pair. A marked line with no such successor is
/// UNCONFIRMED (the hedge: the kick confirms on its own fetch seconds later,
/// or a later poll if that fetch 429s — until one lands the open is not
/// provable, and the scan falls through to older marked lines).
///
/// The SPAN pass covers unmarked out-of-band opens — a live Claude Code
/// session, the web app, the switch prime. Over non-marked samples only, newest
/// first: a boundary counts when two samples at least [`SPAN_MIN_SECS`] apart
/// hold equal within [`BOUNDARY_JITTER_SECS`] with every intermediate equal
/// within the same jitter (zero intermediates is fine — two samples 90s apart
/// confirm); the anchor is the span's max reset less [`FIVE_HOUR_SECS`].
///
/// A trailing unconfirmed reading — a window seen exactly once, or a kick
/// whose marker no poll has vouched for yet — is skipped in favour of the
/// last boundary a pass DID confirm. Skipping a real-but-unconfirmed open is
/// the safe direction: it can only make the queue MORE willing to kick, once,
/// and the kick's own marker confirms it within a poll.
pub(crate) fn series_open(series: &[(i64, i64, Option<i64>)]) -> Option<i64> {
    // Marker pass: newest marked line first, confirmed by any later non-marked
    // report of the same window.
    let marker = series
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, &(_, reset, open_at))| {
            let open = open_at?;
            let confirmed = series[i + 1..].iter().any(|&(_, later, later_open)| {
                later_open.is_none() && (reset - SPAN_MIN_SECS..=reset + 1).contains(&later)
            });
            confirmed.then_some(open)
        });
    marker.into_iter().chain(span_open(series)).max()
}

/// The span pass: the newest run of consecutive non-marked samples whose
/// resets all sit within [`BOUNDARY_JITTER_SECS`] of each other and whose
/// timestamps cover at least [`SPAN_MIN_SECS`], anchored at the run's max
/// reset less [`FIVE_HOUR_SECS`]. `None` when no run spans far enough.
fn span_open(series: &[(i64, i64, Option<i64>)]) -> Option<i64> {
    let non_marked: Vec<(i64, i64)> = series
        .iter()
        .filter_map(|&(ts, reset, open_at)| open_at.is_none().then_some((ts, reset)))
        .collect();
    let mut i = non_marked.len();
    while i > 0 {
        i -= 1;
        let (_, reset) = non_marked[i];
        let mut min = reset;
        let mut max = reset;
        // Extend backward while every added reset stays within jitter of the
        // run's extremes — that keeps max - min <= jitter for the whole run.
        let mut start = i;
        while start > 0 {
            let r = non_marked[start - 1].1;
            if (r - min).abs() > BOUNDARY_JITTER_SECS || (max - r).abs() > BOUNDARY_JITTER_SECS {
                break;
            }
            min = min.min(r);
            max = max.max(r);
            start -= 1;
        }
        // Newest run first. A run that fails on span proves nothing for any
        // pair reaching past its start — that pair crosses the jitter break —
        // so the scan continues below it.
        if non_marked[i].0 - non_marked[start].0 >= SPAN_MIN_SECS {
            return Some(max - FIVE_HOUR_SECS);
        }
        i = start;
    }
    None
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
/// member's failure streak clears. In-memory only — durability is the marked
/// bridge line the kick's next fresh `/usage` body writes into the history
/// file, which [`history_anchor`] replays after a restart.
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
/// by this queue: a live Claude Code session on any profile, the web app,
/// and the switch prime — the CLI switch primes its target through
/// [`crate::oauth::prime_window`], which is the OTHER caller of
/// [`crate::oauth::auto_start_kick`] and deliberately consults neither the
/// toggle nor the anchor (see the module header's override paragraph).
pub(crate) fn next_queue_open_secs(anchor: Option<i64>, n: usize, interval_ms: u64) -> Option<i64> {
    anchor.map(|at| at.saturating_add(queue_gap_secs(n, interval_ms)))
}

/// Load the history-derived anchor into memory.
///
/// `profiles` is the FULL config profile list, not the queue members — the
/// seed and the per-tick gate must agree on the anchor input, and
/// [`history_anchor`] replays every profile's history (a window open is a
/// window open, whoever holds it).
///
/// Called on the thread that SPAWNS the refresher, never inside it: nothing
/// joins that thread, so a home-derived path resolved on it could outlive a
/// test's `HOME_OVERRIDE` and read the operator's real home (the same
/// convention `sync_kick_blocks_from_cache` follows).
pub(crate) fn seed_queue_anchor(state: &AutoStartQueueState, profiles: &[ProfileName]) {
    let Some(open) = history_anchor(profiles) else {
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
/// `profiles` is the FULL config profile list, not the queue members — the
/// member list still sizes the gap and elects (the caller's job), but the
/// anchor replays every profile's history: a window open is a window open,
/// whoever holds it, and the queue spaces against an open on a non-member
/// exactly as it spaces against a member's. The memo and the short-circuit
/// bounds below are keyed on that same full list.
///
/// The derivation is re-run rather than short-circuited on the cached value,
/// because a window can open with no kick of ours behind it — a real Claude
/// Code session on any profile opens one, and the queue must space
/// against that exactly as it spaces against its own kicks (the whole rule is
/// "no OTHER PROFILE opened one within `5h / N`", not "no other member was
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
/// rare one: an idle 5h window's boundary oscillates ±1s around its minute
/// anchor on every recompute, so a polled account appends history on EVERY
/// poll, and the queue sits due with nothing lapsed for the whole stretch
/// between its last open and the next window to run out.
///
/// Past both, a third bound covers the case they miss — an outage, where a
/// member's kick AND its `/usage` refresh keep failing, so it stays lapsed, the
/// queue stays due, and the election keeps probing it on the refresh cadence.
/// The derivation is memoized against [`history_signature`], so the parse only
/// repeats once the files actually move; during an outage no fresh body lands,
/// so nothing is appended, and the memo answers instead.
pub(crate) fn queue_anchor(
    state: &AutoStartQueueState,
    profiles: &[ProfileName],
    now_secs: i64,
    gap_secs: i64,
) -> Option<i64> {
    let cached = queue_anchor_cached(state);
    if !queue_due(cached, now_secs, gap_secs) {
        return cached;
    }
    // A stat per profile, not a parse per profile: past this the replay only
    // runs when the files themselves moved ([`history_signature`]).
    let signature = history_signature(profiles);
    let memo = state
        .lock()
        .ok()
        .and_then(|queue| queue.derived)
        .and_then(|(seen, derived)| (seen == signature).then_some(derived));
    // Derived outside the guard — it replays a file per profile.
    let derived = match memo {
        Some(derived) => derived,
        None => history_anchor(profiles),
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
