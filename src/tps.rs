//! Live token speed (tps) read off Claude Code transcripts.
//!
//! speed = a response's output tokens over its generation span; rate = output
//! tokens generated inside a wall-clock window over its length. The process
//! holding the single-fetcher lease produces a [`Snapshot`] into
//! `~/.clauth/tps_cache.json`; every other surface reads that cache.

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use crate::session_record::LenientString;

pub(crate) const TPS_CACHE_FILE: &str = "tps_cache.json";
/// The producer's per-session attribution timeline (0600, producer-only).
pub(crate) const TPS_ATTRIBUTION_FILE: &str = "tps_attribution.json";
/// The producer's owed-day list (0600, producer-only): every day not folded
/// yet, never its sums, so whichever life sees a day leave the window
/// rebuilds it from the transcripts and folds it.
pub(crate) const TPS_DAYS_FILE: &str = "tps_days.json";
/// Version 2: output only, never read back by this version. An older clauth
/// refuses it whole and writes its own version 1.
pub(crate) const CACHE_VERSION: u16 = 2;
const ATTRIBUTION_VERSION: u16 = 1;
const DAYS_VERSION: u16 = 1;
/// The attribution timeline's entry cap; the oldest-added entry goes first.
const ATTRIBUTION_CAP: usize = 4096;
/// The bytes before a file's read offset whose hash tells an append from a
/// rewrite.
const TAIL_BYTES: u64 = 256;
pub(crate) const LIVE_WINDOW_MS: u64 = 10_000;
pub(crate) const M5_WINDOW_MS: u64 = 5 * 60_000;
pub(crate) const H1_WINDOW_MS: u64 = 60 * 60_000;
/// How far the in-memory settle boundary trails the clock: rate windows and
/// speed responses older than this are settled once and re-derived only when a
/// line touches them, so a tick never re-sweeps a whole day.
const SETTLE_LAG_MS: u64 = 10 * 60_000;
/// A cache older than this reads as no producer.
pub(crate) const STALE_AFTER_MS: u64 = 30_000;

/// The reserved day-sum account for unattributed sessions: no profile name can
/// hold it, and per-account views skip it.
pub(crate) const UNATTRIBUTED: &str = "*";

/// How often the producer re-walks the transcript stores for new files. Between
/// walks it only tails the bytes appended to the files it already knows, so a
/// 1 s tick never pays a whole-tree walk.
const DISCOVER_INTERVAL_MS: u64 = 5_000;

/// Recursion depth of the store walk (`projects/<slug>/<session>/subagents/`).
const WALK_DEPTH: usize = 8;

const DAY_MS: u64 = 86_400_000;

/// How far back a start re-reads the transcripts for an owed day: an owed day
/// older than this many days gets no speed rows.
const OWED_DAYS_BACK: u64 = 14;

/// A response qualifies for a speed peak only at or above this output: the
/// floor keeps a 50-token burst (which reads thousands of t/s over its 100 ms
/// span) out of the "fastest response" record.
const SPEED_FLOOR: u64 = 100;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct Figure {
    pub(crate) rate: f64,
    pub(crate) speed: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct Figures {
    pub(crate) live: Figure,
    pub(crate) m5: Figure,
    pub(crate) h1: Figure,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionFigures {
    pub(crate) account: Option<String>,
    #[serde(flatten)]
    pub(crate) figures: Figures,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DaySum {
    pub(crate) output: u64,
    pub(crate) gen_ms: u64,
    /// The cell's peak speed: the fastest qualifying response's output ÷ span,
    /// with the epoch ms it ended. Recomputed for the live days (today,
    /// yesterday and any future day); kept as folded for older days. The
    /// cell's account and model are its `days` map keys, so the feed spells
    /// them at the boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) speed: Option<PeakAt>,
}

/// date (UTC `YYYY-MM-DD`, the ledger's day key) → account → model → sums.
pub(crate) type DaySums = BTreeMap<String, BTreeMap<String, BTreeMap<String, DaySum>>>;

/// date → account → model → the cell's settled speed peak.
pub(crate) type SpeedPeaks = BTreeMap<String, BTreeMap<String, BTreeMap<String, PeakAt>>>;

/// A peak's value and the epoch-ms instant it belongs to: the internal shape
/// both the cell speed peak and the per-day rate peaks store. The feed spells
/// `at` as `+00:00` RFC 3339 at the boundary ([`SpeedPeak`]/[`RatePeak`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeakAt {
    pub(crate) value: f64,
    pub(crate) at: u64,
}

/// The feed's speed peak: the fastest qualifying response, `at` spelled as
/// `+00:00` RFC 3339 with milliseconds, and the (account, model) cell that won.
/// `account` is omitted for an unattributed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct SpeedPeak {
    pub(crate) value: f64,
    pub(crate) at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    pub(crate) model: String,
}

/// The feed's rate peak: the busiest 10 s window's rate and its window end,
/// `at` spelled as `+00:00` RFC 3339 with milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct RatePeak {
    pub(crate) value: f64,
    pub(crate) at: String,
}

/// One grouping's peaks (an account's or a model's `{speed, rate}` in the
/// feed): each absent peak is omitted, never `0`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct GroupPeaks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) speed: Option<SpeedPeak>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rate: Option<RatePeak>,
}

/// One day's peaks across every grouping (the feed's `peaks.today` /
/// `peaks.ever` shape): `speed`/`rate` are the total's, `accounts` and
/// `models` their own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct PeaksHalf {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) speed: Option<SpeedPeak>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rate: Option<RatePeak>,
    #[serde(default)]
    pub(crate) accounts: BTreeMap<String, GroupPeaks>,
    #[serde(default)]
    pub(crate) models: BTreeMap<String, GroupPeaks>,
}

/// The feed's `peaks` object: today's and the all-time peaks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct Peaks {
    #[serde(default)]
    pub(crate) today: PeaksHalf,
    #[serde(default)]
    pub(crate) ever: PeaksHalf,
}

/// One day's rate peaks across groupings, the speed ledger's per-day half.
/// Rates never aggregate by max, so total, each account and each model keep
/// their own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DayRatePeaks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total: Option<PeakAt>,
    #[serde(default)]
    pub(crate) accounts: BTreeMap<String, PeakAt>,
    #[serde(default)]
    pub(crate) models: BTreeMap<String, PeakAt>,
}

/// The producer's cache (`~/.clauth/tps_cache.json`, version 2): output for
/// every reader, never read back by the producer (the live and owed days are
/// always rebuilt from the transcripts).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) version: u16,
    /// The time of the cache's last write, as `+00:00` RFC 3339 with
    /// milliseconds. (Not the producer's last tick: the write is throttled.)
    pub(crate) generated_at: String,
    pub(crate) total: Figures,
    pub(crate) accounts: BTreeMap<String, Figures>,
    pub(crate) sessions: BTreeMap<String, SessionFigures>,
    /// The live days' sums (today, yesterday and any future day), derived from
    /// the responses' current state, plus every older day whose fold has not
    /// landed yet.
    pub(crate) days: DaySums,
    /// Per-day rate peaks for the same days as `days`.
    #[serde(default)]
    pub(crate) rate_peaks: BTreeMap<String, DayRatePeaks>,
    /// The feed's peaks (today + ever) for consumers; the producer recomputes
    /// them from responses and the ledger, never reads them back.
    #[serde(default)]
    pub(crate) peaks: Peaks,
}

impl Snapshot {
    /// Milliseconds since `generated_at`; `None` when it does not parse or is
    /// in the future.
    pub(crate) fn age_ms(&self, now_ms: u64) -> Option<u64> {
        now_ms.checked_sub(parse_ts(&self.generated_at)?)
    }
}

/// One response the tail has seen, kept as its latest (largest-output) shape.
/// Keyed by message id (or `(session, preceding user ts)` for an id-less line),
/// so growing streamed partials collapse into one response and are never
/// counted twice.
#[derive(Debug, Clone)]
struct Resp {
    session: String,
    /// The member that spent this response: its session's attribution
    /// timeline entry in force at its span start (see
    /// [`Timeline::member_for`]), or `*` for an unattributed session.
    /// Re-derived whenever the timeline or the response's owning file
    /// changes, never frozen.
    account: String,
    model: String,
    /// Latest `user` line before this response's first line; `None` when there
    /// is none, which leaves the response without a measurable span.
    start_ms: Option<u64>,
    /// Timestamp of the last line carrying the largest output seen so far.
    end_ms: u64,
    /// Largest output seen so far.
    output: u64,
    /// The file the response belongs to: the earliest-created file whose lines
    /// carry its id (a fork copies ids into a second file).
    file: Arc<FileRank>,
}

impl Resp {
    /// `(start, generation span in ms)`, or `None` for a zero-length span.
    fn span(&self) -> Option<(u64, u64)> {
        let start = self.start_ms?;
        (self.end_ms > start).then(|| (start, self.end_ms - start))
    }
}

/// A transcript file's identity for the fork rule: its path and birth time.
#[derive(Debug, PartialEq, Eq)]
struct FileRank {
    path: PathBuf,
    created: Option<SystemTime>,
}

impl FileRank {
    /// The earlier-created file comes first; equal or unknown birth times fall
    /// back to the smaller path, so the order is the same on every restart.
    fn before(&self, other: &FileRank) -> bool {
        match (self.created, other.created) {
            (Some(a), Some(b)) if a != b => a < b,
            _ => self.path < other.path,
        }
    }
}

/// What a file is on disk, to tell an append from a replacement: (dev, ino)
/// on unix, the birth time elsewhere.
#[cfg(unix)]
type FileIdent = (u64, u64);
#[cfg(not(unix))]
type FileIdent = Option<SystemTime>;

#[cfg(unix)]
fn ident_of(meta: &std::fs::Metadata) -> FileIdent {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn ident_of(meta: &std::fs::Metadata) -> FileIdent {
    meta.created().ok()
}

/// A transcript file's read life: bumps whenever the producer cannot vouch
/// that the bytes before the old read offset are unchanged. Spend-cap positions
/// compare only within one life; a delivered 429 line re-read in a new life of
/// the same path carries its recorded position along, so a latch holding its
/// uuid is compared against where the line now lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileLife(u64);

/// Take the next read life: monotonic for the producer's whole life, so a life
/// id never needs comparing across files to answer "same life?". Every mint
/// goes through here, the single writer of the counter.
fn next_life(counter: &mut u64) -> FileLife {
    *counter += 1;
    FileLife(*counter)
}

/// One transcript file's read state.
#[derive(Debug, Clone)]
struct FileEntry {
    /// Bytes consumed (whole lines only).
    offset: u64,
    /// The read life [`offset`](Self::offset) belongs to.
    life: FileLife,
    /// The mtime the last store walk saw (the hot-set filter).
    mtime_ms: u64,
    /// The mtime, identity and tail hash at the last read: a file whose
    /// identity changed, that shrank, or whose bytes before `offset` no longer
    /// hash to `tail` is re-read from byte 0.
    read_mtime: u64,
    ident: Option<FileIdent>,
    tail: Option<[u8; 32]>,
    rank: Arc<FileRank>,
    session: String,
    /// The isolated runtime `(sid, profile)` the file's store path names;
    /// `None` for the global store.
    runtime: Option<(String, String)>,
    /// Read from byte 0 with the day cutoff at the next tail: a first read, a
    /// replacement, a day that turned live again, or a cold re-entry of a file
    /// holding a 429 this producer already delivered.
    reread: bool,
}

/// One pass over a file: where it starts, the tail hash that must vouch for the
/// bytes before the offset it carries, the spend-cap bound it folds with, and
/// the read life its facts belong to.
struct ReadPass {
    start: u64,
    verify: Option<(u64, [u8; 32])>,
    facts_through: u64,
    life: FileLife,
}

/// The file a read runs over, as its responses and spend-cap facts record it.
struct ReadCtx {
    rank: Arc<FileRank>,
    session: String,
    /// The read life this pass folds into: every spend-cap position it reports
    /// names it, so the scheduler compares positions within one life only.
    life: FileLife,
}

/// What [`Producer::read_append`] found.
enum ReadOutcome {
    /// The new offset and the hash of the bytes before it.
    Read((u64, Option<[u8; 32]>)),
    /// The bytes before the offset changed: the file was rewritten.
    Rewritten,
    /// The file could not be read; nothing changed.
    Failed,
}

/// One attribution timeline entry: the session spends `member` from `since`
/// (epoch ms) on, in every transcript of the session, subagent ones included;
/// entries order by `since` alone (clock order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TimelineEntry {
    session: String,
    since: u64,
    member: String,
}

/// `~/.clauth/tps_attribution.json`: every entry in the order it was added.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TimelineWire {
    #[serde(default)]
    version: u16,
    #[serde(default)]
    entries: Vec<TimelineEntry>,
}

/// [`TimelineWire`] borrowed, for the save.
#[derive(Serialize)]
struct TimelineWireRef<'a> {
    version: u16,
    entries: Vec<&'a TimelineEntry>,
}

/// How a producer-only file (the attribution timeline, the owed-day list) read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum DiskState {
    /// Absent or parsed: save freely.
    #[default]
    Usable,
    /// Did not parse: renamed aside before the first save.
    Corrupt,
    /// Could not be read: never saved over, read again before each save.
    Unreadable,
    /// From a newer clauth: never saved over, never read again.
    Newer,
}

/// The per-session attribution timeline: the producer appends an entry when
/// it observes a session's member change, and a response takes the entry
/// ordered before it. Capped by entry count,
/// evicting the oldest-added.
#[derive(Debug, Default)]
struct Timeline {
    by_session: HashMap<String, Vec<TimelineEntry>>,
    /// The session of every entry, in the order the entries were added.
    order: VecDeque<String>,
    disk: DiskState,
    /// An entry was added since the last save that landed.
    unsaved: bool,
    /// The "not saving" line was logged for the current failure state.
    save_failed_logged: bool,
}

impl Timeline {
    fn load(dir: &Path) -> Self {
        let mut timeline = Self::default();
        let bytes = match std::fs::read(dir.join(TPS_ATTRIBUTION_FILE)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return timeline,
            Err(_) => {
                timeline.disk = DiskState::Unreadable;
                return timeline;
            }
        };
        let Ok(wire) = serde_json::from_slice::<TimelineWire>(&bytes) else {
            timeline.disk = DiskState::Corrupt;
            return timeline;
        };
        if wire.version > ATTRIBUTION_VERSION {
            timeline.disk = DiskState::Newer;
            return timeline;
        }
        for entry in wire.entries {
            timeline.push(entry);
        }
        timeline.unsaved = false;
        timeline
    }

    fn push(&mut self, entry: TimelineEntry) {
        self.order.push_back(entry.session.clone());
        self.by_session
            .entry(entry.session.clone())
            .or_default()
            .push(entry);
        while self.order.len() > ATTRIBUTION_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entries) = self.by_session.get_mut(&oldest) {
                if !entries.is_empty() {
                    entries.remove(0);
                }
                if entries.is_empty() {
                    self.by_session.remove(&oldest);
                }
            }
        }
        self.unsaved = true;
    }

    /// The member a response of `session` starting at `start_ms` spent, in
    /// clock order: the newest entry whose `since` is at or before it, newest
    /// by `since`, so a change stamped before an earlier-added entry binds
    /// only up to that entry's instant. On equal stamps the earlier-added
    /// entry wins: a stale hook record stamped in a swap's own ms is read
    /// after the swap and must never take it over. Two hook records inside
    /// one ms tie the same way (stamps truncate to the ms): the earlier one
    /// wins, so the second change waits for the next entry. Before every
    /// entry, the earliest one's member; no entry at all, `*`. Across a clock
    /// step back a span start no longer orders a response against a swap, so
    /// a swapped session's responses may read under the wrong member; day and
    /// model totals stay exact.
    fn member_for(&self, session: &str, start_ms: u64) -> &str {
        let Some(entries) = self.by_session.get(session) else {
            return UNATTRIBUTED;
        };
        // `max_by_key` keeps the last of equal maxima; walking newest-added
        // first makes that the earliest-added one.
        entries
            .iter()
            .rev()
            .filter(|e| e.since <= start_ms)
            .max_by_key(|e| e.since)
            .or_else(|| entries.iter().min_by_key(|e| e.since))
            .map_or(UNATTRIBUTED, |e| e.member.as_str())
    }

    /// Persist the timeline (0600) when an entry was added since the last
    /// save. A file that did not parse is renamed aside first (0600); an
    /// unreadable or newer file is never saved over. Failures are logged once
    /// per failure state and retried on the next call; `unsaved` stays set
    /// until a save lands.
    fn save_if_changed(&mut self, dir: &Path) {
        if !self.unsaved {
            return;
        }
        if matches!(self.disk, DiskState::Unreadable | DiskState::Newer) {
            if !self.save_failed_logged {
                crate::logline::logline!(
                    "clauth: tps_attribution.json is newer or unreadable; not saving over it, holding speed folds"
                );
                self.save_failed_logged = true;
            }
            return;
        }
        let path = dir.join(TPS_ATTRIBUTION_FILE);
        if self.disk == DiskState::Corrupt {
            if !move_aside(dir, TPS_ATTRIBUTION_FILE) {
                if !self.save_failed_logged {
                    crate::logline::logline!(
                        "clauth: could not move the unreadable tps_attribution.json aside; not saving over it, holding speed folds"
                    );
                    self.save_failed_logged = true;
                }
                return;
            }
            self.disk = DiskState::Usable;
        }
        let mut cursor: HashMap<&str, usize> = HashMap::new();
        let entries: Vec<&TimelineEntry> = self
            .order
            .iter()
            .filter_map(|session| {
                let i = cursor.entry(session.as_str()).or_default();
                let entry = self.by_session.get(session)?.get(*i);
                *i += 1;
                entry
            })
            .collect();
        let wire = TimelineWireRef {
            version: ATTRIBUTION_VERSION,
            entries,
        };
        let written = serde_json::to_vec(&wire)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                crate::profile::atomic_write_600(&path, &bytes).map_err(|e| e.to_string())
            });
        match written {
            Ok(()) => {
                self.unsaved = false;
                if self.save_failed_logged {
                    crate::logline::logline!("clauth: tps_attribution.json saves recovered");
                    self.save_failed_logged = false;
                }
            }
            Err(e) => {
                if !self.save_failed_logged {
                    crate::logline::logline!(
                        "clauth: failed to save tps_attribution.json: {e}; holding speed folds"
                    );
                    self.save_failed_logged = true;
                }
            }
        }
    }
}

/// Move an unparseable producer file `dir/name` aside to
/// `name.corrupt-<epoch_ms>` (0600), so the next save never writes over it.
/// False when the rename failed.
pub(crate) fn move_aside(dir: &Path, name: &str) -> bool {
    let aside = dir.join(format!("{name}.corrupt-{}", crate::usage::now_ms()));
    if std::fs::rename(dir.join(name), &aside).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o600)) {
            crate::logline::logline!("clauth: failed to restrict {}: {e}", aside.display());
        }
    }
    crate::logline::logline!(
        "clauth: {name} was unreadable; moved aside to {}",
        aside.display()
    );
    true
}

/// `~/.clauth/tps_days.json`.
#[derive(Debug, Default, Deserialize)]
struct DaysWire {
    #[serde(default)]
    version: u16,
    #[serde(default)]
    owed: BTreeSet<String>,
}

/// [`DaysWire`] borrowed, for the save.
#[derive(Serialize)]
struct DaysWireRef<'a> {
    version: u16,
    owed: &'a BTreeSet<String>,
}

/// The owed-day list file: no file event
/// marks a turn finished, so a day folds only once it left the (today,
/// yesterday) window, and a day that leaves it while no producer runs must
/// still fold. The file names those days and never holds their sums: a stored
/// sum double counts a response that changes day or member after it was
/// summed. Written only when the list changed: a newly owed day before the
/// attribution timeline's save, a folded day's removal only after its fold
/// landed.
#[derive(Debug, Default)]
struct DaysFile {
    disk: DiskState,
    /// The bytes on disk as this life last read or wrote them.
    written: Option<Vec<u8>>,
    /// The "not saving" line was logged for the current failure state.
    save_failed_logged: bool,
}

impl DaysFile {
    /// The file's state and, when it parsed at this version or older, its
    /// owed days (only the strings that are `YYYY-MM-DD` dates).
    fn load(dir: &Path) -> (Self, Option<BTreeSet<String>>) {
        let mut file = Self::default();
        let bytes = match std::fs::read(dir.join(TPS_DAYS_FILE)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (file, None),
            Err(_) => {
                file.disk = DiskState::Unreadable;
                return (file, None);
            }
        };
        let Ok(wire) = serde_json::from_slice::<DaysWire>(&bytes) else {
            file.disk = DiskState::Corrupt;
            return (file, None);
        };
        if wire.version > DAYS_VERSION {
            file.disk = DiskState::Newer;
            return (file, None);
        }
        file.written = Some(bytes);
        let owed = wire.owed.into_iter().filter(|d| is_day(d)).collect();
        (file, Some(owed))
    }

    /// Persist `owed` (0600) when it differs from the bytes last read or
    /// written. A file that did not parse is moved aside first; an unreadable
    /// or newer one is never saved over. Failures log once per failure state
    /// and retry on the next call.
    fn save(&mut self, dir: &Path, owed: &BTreeSet<String>) {
        if matches!(self.disk, DiskState::Unreadable | DiskState::Newer) {
            self.log_failure(
                "clauth: tps_days.json is newer or unreadable; not saving over it, keeping the owed speed days in memory",
            );
            return;
        }
        let wire = DaysWireRef {
            version: DAYS_VERSION,
            owed,
        };
        let Ok(bytes) = serde_json::to_vec(&wire) else {
            self.log_failure("clauth: failed to serialize tps_days.json");
            return;
        };
        if self.written.as_deref() == Some(bytes.as_slice()) {
            return;
        }
        if self.disk == DiskState::Corrupt {
            if !move_aside(dir, TPS_DAYS_FILE) {
                self.log_failure(
                    "clauth: could not move the unreadable tps_days.json aside; not saving over it",
                );
                return;
            }
            self.disk = DiskState::Usable;
        }
        match crate::profile::atomic_write_600(&dir.join(TPS_DAYS_FILE), &bytes) {
            Ok(()) => {
                self.written = Some(bytes);
                if self.save_failed_logged {
                    crate::logline::logline!("clauth: tps_days.json saves recovered");
                    self.save_failed_logged = false;
                }
            }
            Err(e) => self.log_failure(&format!("clauth: failed to save tps_days.json: {e}")),
        }
    }

    fn log_failure(&mut self, line: &str) {
        if !self.save_failed_logged {
            crate::logline::logline!("{line}");
            self.save_failed_logged = true;
        }
    }
}

/// What the attribution sources say about one session right now.
struct Observed {
    member: Option<String>,
    /// The source's own stamp of the change: the live row's `last_swap_at`,
    /// else the hook's `resolved_at`; `None` for an unstamped source, whose
    /// change takes the observation instant.
    since: Option<u64>,
    /// A live row that swapped before this producer saw the session:
    /// `(start_profile, last_swap_at)`.
    swap_seed: Option<(String, u64)>,
}

/// The tail reader + attribution + windowing, owned by the lease holder.
#[derive(Debug, Default)]
pub(crate) struct Producer {
    /// Read state per transcript file; only appended bytes are re-read.
    files: HashMap<PathBuf, FileEntry>,
    /// Every response still relevant (the last hour + today + yesterday + any
    /// future day), keyed by message id or `(session, user ts)`: one response
    /// per id however many files or reads carry it.
    responses: HashMap<String, Resp>,
    /// Conversation id → transcript FILE → that file's later spend-cap 429 row
    /// since the last drain, carrying its timestamp, its position (the file's
    /// read life and the line's end offset) and the line's `uuid`. Per file
    /// first because line order decides within one file life only; the drain
    /// then hands over the highest stamp across files. The file matters: a
    /// subagent transcript shares the parent's conversation key (`session_key`
    /// folds `subagents/` into the parent dir), so a success in another file
    /// must not clear the parent's 429. An error row still folds no tokens;
    /// this map is its only effect.
    spend_cap_429: HashMap<String, HashMap<PathBuf, SpendCap429Fact>>,
    /// Conversation id → per transcript FILE, the latest successful turn of
    /// that FILE's current read life, as its position and stamp. Never drained:
    /// the scheduler pulls it while judging a latched 429, so a success read on
    /// a scan that could not judge the latch still clears it on the next scan.
    /// A new life of the file replaces the record, so positions from two lives
    /// never compare. Keyed by file, never conversation alone: a later success
    /// in another file (a subagent) must not clear the file that recovered.
    spend_cap_recoveries: HashMap<String, HashMap<PathBuf, SpendCapRecovery>>,
    /// The successful turns per model: conversation id → transcript FILE →
    /// model → its latest turn since the last drain, as its position in the
    /// file's read life and its stamp, passing the same position bound as the
    /// success record, so any later successful turn on a member clears its
    /// credit refusal, whatever model served it, and a turn in the refusal's
    /// own file is ordered against it by line. Drained, unlike the success
    /// record: each turn reaches one scan.
    spend_cap_turns_by_model: HashMap<String, HashMap<PathBuf, HashMap<String, SpendCapRecovery>>>,
    /// Conversation id → line `uuid` → where that line was last read, for
    /// every 429 line already folded into a 429 fact this producer life, so a
    /// 429 line that comes back — a byte-0 re-read, a rewrite, a shrink, a
    /// walk miss — never re-delivers. Its position follows every later read of
    /// the line in the SAME path, whatever the read's position bound, because
    /// a latch holding the uuid pulls it before judging a successful turn; a
    /// read at another path (a rescue copy) never moves it, because a latch
    /// never moves across transcript files. An undelivered line the bound
    /// skips is neither recorded nor delivered; a uuid-less 429 falls back to
    /// the position bound alone. Never pruned: the record lives for the
    /// producer's whole life.
    delivered_facts: HashMap<String, HashMap<String, SpendCapPos>>,
    /// The last [`FileLife`] handed out: producer-wide and monotonic, so a
    /// life id never needs comparing across files to answer "same life?".
    next_life: u64,
    /// The live days' sums (today, yesterday, future days), derived from
    /// `responses` whenever they change; never accumulated.
    days: DaySums,
    /// Days out of the window whose fold has not landed (a failed or refused
    /// save, or an owed day a start rebuilt): each day's cells as derived when
    /// it left the window, kept in memory until the ledger holds them.
    held: DaySums,
    /// The held days' rate peaks.
    held_rate_peaks: BTreeMap<String, DayRatePeaks>,
    /// Every day not folded yet: the live days, the held days, and a future
    /// day a previous life left, which waits until it enters and leaves the
    /// window. Persisted as `tps_days.json`.
    owed: BTreeSet<String>,
    /// `tps_days.json`'s list read at start (or once an unreadable one
    /// reads): the next tick merges it into `owed` and its past days into
    /// `unbuilt`.
    loaded_owed: Option<BTreeSet<String>>,
    /// Owed past days a read-back list named that no tick has rebuilt yet:
    /// the rebuild re-reads every transcript touched since the oldest of
    /// them, so it waits until a fold can land (the speed ledger usable and
    /// its saves landing, no attribution save pending), and these days stay
    /// owed until then.
    unbuilt: BTreeSet<String>,
    /// The owed-day list file's state.
    days_file: DaysFile,
    /// Latest user line per transcript PATH, for the next response's span start.
    last_user: HashMap<PathBuf, u64>,
    /// Batch-resolved shared-session owners, refreshed when the store mtime
    /// moves or a session is new to the batch.
    store_owners: HashMap<String, Option<String>>,
    /// The store mtime `store_owners` was refreshed at.
    store_owners_mtime: u64,
    /// `session_profiles.json` mtime, refreshed once per tick.
    store_mtime: u64,
    /// Session key → the isolated runtime (sid, profile) a file of it was read
    /// from this life: the live row and the path-profile fallback.
    session_isolated: HashMap<String, (String, String)>,
    /// Session key → the isolated runtime its earliest-created isolated-store
    /// file names (ties by the smaller path), over every file the last store
    /// walk found, a runtime with a live row first when the key sits in
    /// several: a session's first observation takes its runtime from here,
    /// whichever of its files a tick reads first.
    runtimes: HashMap<String, (Arc<FileRank>, (String, String))>,
    /// Each held session's current member, observed this tick (the session
    /// rows' `account`).
    current: HashMap<String, Option<String>>,
    /// Sessions this life has read a file of: a session's first read observes
    /// its member before any of its lines count.
    seen: HashSet<String>,
    /// The per-session attribution timeline, persisted in its own file.
    timeline: Timeline,
    /// The earliest end (epoch ms) from which every response is held: a tick
    /// whose day cutoff falls below it (a day turned live again under a clock
    /// step back) re-reads the hot files from byte 0. `None` before the first
    /// tick.
    whole_since: Option<u64>,
    /// The days the last tick derived as live.
    live_days: BTreeSet<String>,
    /// The speed ledger the producer folds each finalized day into.
    speed_ledger: crate::token_ledger::SpeedLedger,
    /// Per-day rate peaks for the live days; the recompute overwrites them.
    rate_peaks: BTreeMap<String, DayRatePeaks>,
    /// In-memory settled speed peaks for the live days: the responses ending
    /// before the settle boundary, advanced incrementally and re-derived from
    /// scratch when a line's support touches settled windows; the shown peak
    /// maxes with it. Never persisted: a restart derives everything afresh.
    settled_speed: SpeedPeaks,
    /// The settled rate peaks, likewise.
    settled_rate_peaks: BTreeMap<String, DayRatePeaks>,
    /// Days whose settled values owe a from-scratch re-derive: a line created
    /// or grew a response whose support touches already-settled windows, or a
    /// response changed account.
    settle_rederive: BTreeSet<String>,
    /// The advancing settle boundary: every settled value covers exactly the
    /// windows ending before it (rate) and the responses ending before
    /// `settled_through - LIVE_WINDOW_MS` (speed). 0 at start.
    settled_through: u64,
    /// The response set changed since the last recompute, so the peaks owe a
    /// recompute (never a running max: recomputing from the responses' current
    /// state is what keeps a still-streaming response self-correcting).
    peaks_dirty: bool,
    /// The (today, yesterday) pair the last recompute ran under; a day rollover
    /// forces a recompute even when no response changed.
    last_peaks_days: Option<(String, String)>,
    /// The spelled `ever` half, rebuilt only when `ever_dirty`.
    ever_cache: PeaksHalf,
    /// The `ever` cache is stale: a live or held day's cells or peaks
    /// changed, or the ledger did.
    ever_dirty: bool,
    /// The ledger's own half of `ever`; `None` until the ledger is walked,
    /// and again after the ledger changes (a reload, a fold that saved), so
    /// a tick walks the stored days only after one of those.
    ledger_ever: Option<EverBest>,
    /// The "newer/unreadable ledger" line has been logged once.
    unusable_logged: bool,
    /// The "save failed" line has been logged for the current failure state.
    save_failed_logged: bool,
    /// `token_speed.json`'s (ctime_secs, ctime_nsecs, mtime_ms, len) at the last
    /// reload check (ctime is 0 off unix).
    speed_ledger_sig: (u64, u64, u64, u64),
    /// The last time an unreadable speed ledger was reloaded (the 5 min timer).
    last_reload_ms: u64,
    /// When the last whole-tree walk ran.
    last_discover_ms: u64,
}

impl Producer {
    pub(crate) fn new() -> Self {
        let dir = crate::profile::clauth_dir().ok();
        let speed_ledger = dir
            .as_deref()
            .map(crate::token_ledger::SpeedLedger::load)
            .unwrap_or_default();
        let timeline = dir.as_deref().map(Timeline::load).unwrap_or_default();
        let (days_file, loaded_owed) = dir.as_deref().map(DaysFile::load).unwrap_or_default();
        Self {
            speed_ledger,
            timeline,
            days_file,
            loaded_owed,
            ever_dirty: true,
            ..Self::default()
        }
    }

    /// The spend-cap 429s since the last scan, consumed once by the scheduler's
    /// per-session scan: a line's fact mints at most once per scan, and a 429
    /// with a `uuid` at most once per producer life, whatever re-read brings it
    /// back, while that re-read moves the position a latch pulls. Each
    /// conversation's pending entry is the one 429 a scan latches: see
    /// [`pending_winner`]. The scheduler's latch is what makes a 429 survive a
    /// tick with no moveable target. A file's success record is NOT drained:
    /// the scheduler pulls [`spend_cap_recovery`](Self::spend_cap_recovery)
    /// while judging the latch, so no scan can consume one. The turns per model
    /// are, so each scan places a turn on its member once.
    pub(crate) fn drain_spend_cap(&mut self) -> SpendCapDrain {
        SpendCapDrain {
            pending: std::mem::take(&mut self.spend_cap_429)
                .into_iter()
                .filter_map(|(session, files)| {
                    pending_winner(&files).map(|fact| (session, fact.clone()))
                })
                .collect(),
            turns_by_model: std::mem::take(&mut self.spend_cap_turns_by_model),
        }
    }

    /// Where a delivered 429 line lies now, for a latch holding its `uuid`:
    /// `None` unless this conversation delivered the line and it still sits in
    /// `path`. The scheduler pulls this before judging a successful turn, so a
    /// scan that skipped the latch loses nothing.
    pub(crate) fn spend_cap_position(
        &self,
        session: &str,
        uuid: &str,
        path: &Path,
    ) -> Option<(FileLife, u64)> {
        let position = self.delivered_facts.get(session)?.get(uuid)?;
        (position.file.as_path() == path).then_some((position.life, position.end))
    }

    /// The latest successful turn this producer read in `path`'s CURRENT read
    /// life, for judging a latch on this conversation's 429 in that file: the
    /// scheduler pulls it on every scan, so a success read on a scan that could
    /// not judge the latch stays available to the next one. `None` when this
    /// life of the path holds no success (a new life replaced the record) or
    /// the path has left the walk (its record is pruned with the entry).
    pub(crate) fn spend_cap_recovery(
        &self,
        session: &str,
        path: &Path,
    ) -> Option<SpendCapRecovery> {
        self.spend_cap_recoveries.get(session)?.get(path).copied()
    }

    /// One pass at `now_ms`: tail every transcript store under the home dir,
    /// attribute each session, save the owed-day list's additions, save the
    /// attribution timeline, fold the days that left the window, save the
    /// list's removals, and rebuild the snapshot from the responses' current
    /// state. The timeline lands before anything that reflects it: while a
    /// save is pending no fold runs. The snapshot is output only, so it is
    /// published either way.
    pub(crate) fn tick(&mut self, now_ms: u64) -> Snapshot {
        self.store_mtime = store_mtime_now();
        let cutoff = cutoff_ms(now_ms);
        let today = day_of_ms(now_ms);
        let yesterday = day_of_ms(now_ms - DAY_MS);
        let oldest_owed = oldest_owed_day(now_ms);
        let dir = crate::profile::clauth_dir().ok();
        if let Some(dir) = dir.as_deref() {
            if self.days_file.disk == DiskState::Unreadable {
                let (file, owed) = DaysFile::load(dir);
                if file.disk != DiskState::Unreadable {
                    let logged = self.days_file.save_failed_logged;
                    self.days_file = file;
                    self.days_file.save_failed_logged = logged;
                    self.loaded_owed = owed;
                }
            }
            if self.timeline.disk == DiskState::Unreadable {
                self.reload_timeline(dir, now_ms);
            }
            self.refresh_speed_ledger(dir, now_ms);
            self.speed_ledger.claim(dir);
        }
        // An owed-day list read back (at start, or once an unreadable one
        // reads): each past day it names, back to `OWED_DAYS_BACK`, is rebuilt
        // from every transcript touched since the oldest of them and folds;
        // an older one gets no rows. A future day waits in the list. The
        // rebuild waits while no fold could land, a ledger whose saves fail
        // included, so a start never pays it for nothing.
        if let Some(loaded) = self.loaded_owed.take() {
            self.owed.extend(loaded);
            self.unbuilt.extend(
                self.owed
                    .iter()
                    .filter(|d| d.as_str() < yesterday.as_str())
                    .cloned(),
            );
        }
        self.owed.retain(|d| d.as_str() >= oldest_owed.as_str());
        self.unbuilt.retain(|d| d.as_str() >= oldest_owed.as_str());
        let rebuilt = if !self.unbuilt.is_empty()
            && self.folds_can_land()
            && dir
                .as_deref()
                .is_some_and(|dir| self.speed_ledger.saves_land(dir))
        {
            std::mem::take(&mut self.unbuilt)
        } else {
            BTreeSet::new()
        };
        let floor = rebuilt
            .first()
            .map_or(cutoff, |d| day_start_ms(d).min(cutoff));
        // A day turned live again (a clock correction or a step back): the
        // responses it holds were dropped and the files' offsets sit past
        // them, so every file hot under the new cutoff re-reads from byte 0,
        // deduped by id. Once per step event: the re-read lowers
        // `whole_since` to the new cutoff. A rebuild of owed days re-reads
        // every file touched since its floor the same way.
        let reread = self.whole_since.is_some_and(|w| cutoff < w) || floor < cutoff;
        if reread
            || self.files.is_empty()
            || now_ms < self.last_discover_ms
            || now_ms - self.last_discover_ms >= DISCOVER_INTERVAL_MS
        {
            self.discover(now_ms);
            self.last_discover_ms = now_ms;
        }
        if reread {
            // Every append moves a file's mtime past its new lines, so only a
            // file whose mtime was set back by hand hides a line from this
            // floor; reading every file instead would cost the whole history.
            for e in self.files.values_mut() {
                if e.mtime_ms >= floor {
                    e.reread = true;
                }
            }
        }
        // Observe before reading, so every line this tick reads takes the
        // member in force at its span start.
        self.observe_sessions(now_ms);
        self.tail_all(now_ms, floor);
        self.whole_since = Some(self.whole_since.map_or(floor, |w| w.min(floor)));

        // Live: today, yesterday and every future day holding a response. A
        // day that left the window keeps its last derived state in `held`
        // until the fold lands.
        let tomorrow_start = day_start_ms(&today).saturating_add(DAY_MS);
        let mut live: BTreeSet<String> = BTreeSet::from([today.clone(), yesterday.clone()]);
        for r in self.responses.values() {
            if r.end_ms >= tomorrow_start {
                live.insert(day_of_ms(r.end_ms));
            }
        }
        // A newly owed day lands in the list before the timeline save, so a
        // life dying between the two writes still leaves it owed.
        self.owed.extend(live.iter().cloned());
        if let Some(dir) = dir.as_deref() {
            self.days_file.save(dir, &self.owed);
        }
        #[cfg(test)]
        if halt_seam::armed() {
            return Snapshot::default();
        }
        let attribution_saved = self.save_timeline(dir.as_deref());

        let leaving: BTreeSet<String> = self
            .live_days
            .iter()
            .filter(|d| !live.contains(*d) && d.as_str() < yesterday.as_str())
            .cloned()
            .chain(rebuilt)
            .collect();
        for day in &leaving {
            self.hold_leaving_day(day);
        }

        // Keep the day-before-yesterday's tail that still reaches yesterday's
        // first windows (they hold tokens from up to 10 s before midnight).
        let tail_cutoff = day_start_ms(&yesterday).saturating_sub(LIVE_WINDOW_MS);
        let before = self.responses.len();
        self.responses.retain(|_, r| r.end_ms >= tail_cutoff);
        if self.responses.len() != before {
            self.peaks_dirty = true;
        }
        self.whole_since = self.whole_since.map(|w| w.max(tail_cutoff));
        let held_sessions: HashSet<&str> = self
            .responses
            .values()
            .map(|r| r.session.as_str())
            .collect();
        self.current
            .retain(|k, _| held_sessions.contains(k.as_str()));
        self.session_isolated
            .retain(|k, _| held_sessions.contains(k.as_str()));
        self.seen.retain(|k| held_sessions.contains(k.as_str()));
        self.settled_speed.retain(|d, _| live.contains(d));
        self.settled_rate_peaks.retain(|d, _| live.contains(d));
        self.rate_peaks.retain(|d, _| live.contains(d));

        let mut snap = self.build_snapshot(now_ms, &live);
        self.live_days = live;
        let Some(dir) = dir.as_deref() else {
            return snap;
        };
        // A fold derived from an entry no file holds would disagree with what
        // a restart re-derives from the saved timeline.
        if attribution_saved {
            self.fold_days(&today, &yesterday, dir);
            // The fold may have landed held days: the snapshot holds what is
            // still held after it.
            snap.days
                .retain(|d, _| self.live_days.contains(d) || self.held.contains_key(d));
            snap.rate_peaks
                .retain(|d, _| self.live_days.contains(d) || self.held_rate_peaks.contains_key(d));
        }
        // The days a landed fold dropped leave the list only now.
        self.days_file.save(dir, &self.owed);
        snap
    }

    /// True when a fold of a rebuilt day could land this tick: the speed
    /// ledger may be saved over and no attribution save is pending or
    /// refused.
    fn folds_can_land(&self) -> bool {
        !self.speed_ledger.unusable()
            && !self.timeline.unsaved
            && !matches!(self.timeline.disk, DiskState::Unreadable | DiskState::Newer)
    }

    /// Save the attribution timeline when an entry was added since the last
    /// save landed; true when nothing is left unsaved.
    fn save_timeline(&mut self, dir: Option<&Path>) -> bool {
        if !self.timeline.unsaved {
            return true;
        }
        let Some(dir) = dir else {
            return false;
        };
        self.timeline.save_if_changed(dir);
        !self.timeline.unsaved
    }

    /// The attribution file could not be read at start: read it again (each
    /// tick, whether or not an entry waits to be saved, since an owed day's
    /// rebuild waits for it). Once it reads, it replaces this life's timeline,
    /// and every session this life holds is observed against it the way a
    /// restart's first read observes it (a change at its stamp, or a seed for
    /// a session it does not know); the responses take their members again.
    fn reload_timeline(&mut self, dir: &Path, now_ms: u64) {
        let fresh = Timeline::load(dir);
        if fresh.disk == DiskState::Unreadable {
            return;
        }
        let sessions: HashSet<String> = self.timeline.by_session.keys().cloned().collect();
        let logged = self.timeline.save_failed_logged;
        self.timeline = fresh;
        self.timeline.save_failed_logged = logged;
        for session in &sessions {
            let observed = self.observe_sources(session);
            self.observe_first(session, &observed, now_ms);
        }
        self.reattribute(&sessions, now_ms);
    }

    /// A day left the window, or a start rebuilt an owed day: derive its final
    /// cells, speed peaks and rate peaks from the responses still held (before
    /// the retain drops them) and hold them, by [`refold_day`], until the fold
    /// lands.
    fn hold_leaving_day(&mut self, day: &str) {
        let day_start = day_start_ms(day);
        let day_end = day_start.saturating_add(DAY_MS);
        let all: Vec<&Resp> = self.responses.values().collect();
        let only = BTreeSet::from([day.to_owned()]);
        let mut cells = sum_days(all.iter().copied(), &only)
            .remove(day)
            .unwrap_or_default();
        let speed = speed_peaks_over(&all, &only);
        for (account, models) in &mut cells {
            for (model, sum) in models.iter_mut() {
                sum.speed = speed
                    .get(day)
                    .and_then(|a| a.get(account))
                    .and_then(|m| m.get(model))
                    .copied();
            }
        }
        let (total, by_account, by_model) = group_responses(&all);
        let rate = day_rate_peaks(
            &total,
            &by_account,
            &by_model,
            day_start,
            day_end.saturating_sub(1),
        );
        if !cells.is_empty() {
            refold_day(self.held.entry(day.to_owned()).or_default(), &cells);
        }
        if !is_empty_rate_peaks(&rate) {
            let slot = self.held_rate_peaks.entry(day.to_owned()).or_default();
            *slot = merge_rate_peaks(std::mem::take(slot), rate);
        }
        self.days.remove(day);
        self.rate_peaks.remove(day);
        self.settled_speed.remove(day);
        self.settled_rate_peaks.remove(day);
        self.ever_dirty = true;
    }

    /// Re-load a speed ledger that was unreadable, whether or not a fold is
    /// pending, so `ever` regains its stored days as soon as the file reads.
    /// A newer file is never re-loaded. An unreadable one is
    /// re-loaded when its (ctime, mtime, len) moves — a chmod or a rewrite — or
    /// on a 5 min timer, because a cleared EIO moves no stamp. Both exist
    /// because a signature alone misses a permission fix (only ctime moves) and
    /// a permission fix alone misses a cleared transient I/O error (no stamp
    /// moves at all).
    fn refresh_speed_ledger(&mut self, dir: &Path, now_ms: u64) {
        if !self.speed_ledger.unusable() || self.speed_ledger.newer() {
            return;
        }
        let timer = self.last_reload_ms == 0
            || now_ms < self.last_reload_ms
            || now_ms - self.last_reload_ms >= 5 * 60_000;
        if timer || self.reload_speed_ledger_if_changed(dir) {
            self.speed_ledger = crate::token_ledger::SpeedLedger::load(dir);
            self.unusable_logged = false;
            self.last_reload_ms = now_ms;
            self.ledger_ever = None;
            self.ever_dirty = true;
        }
    }

    /// Fold into the speed ledger every held day older than yesterday: a day
    /// folds only once it left the (today, yesterday) window, since no file
    /// event marks a turn finished. A day the ledger holds refolds by
    /// [`refold_day`] and every peak takes the max, so a refold of an
    /// unchanged day is a no-op and a crash anywhere around the save needs no
    /// marker. A held day stays held until a save lands; a newer or
    /// unreadable ledger is never saved over.
    fn fold_days(&mut self, today: &str, yesterday: &str, dir: &Path) {
        // A held day back in the window (a clock step back) or ahead of it
        // waits until it leaves the window again.
        let mut days: DaySums = self
            .held
            .iter()
            .filter(|(d, _)| d.as_str() < yesterday)
            .map(|(d, a)| (d.clone(), a.clone()))
            .collect();
        let mut rate_peaks: BTreeMap<String, DayRatePeaks> = self
            .held_rate_peaks
            .iter()
            .filter(|(d, _)| d.as_str() < yesterday)
            .map(|(d, p)| (d.clone(), p.clone()))
            .collect();
        days.retain(|_, accounts| !accounts.is_empty());
        rate_peaks.retain(|_, p| !is_empty_rate_peaks(p));
        if days.is_empty() && rate_peaks.is_empty() {
            // A held past day with no cells and no peaks has nothing to land.
            self.drop_folded_held(yesterday);
            return;
        }
        if self.speed_ledger.unusable() {
            if !self.unusable_logged {
                crate::logline::logline!(
                    "clauth: token_speed.json is newer or unreadable; keeping the speed days owed"
                );
                self.unusable_logged = true;
            }
            return; // the days stay held, and owed in tps_days.json
        }
        match self.speed_ledger.fold(dir, &days, &rate_peaks, today) {
            crate::token_ledger::FoldOutcome::Unsaved => {
                if !self.save_failed_logged {
                    crate::logline::logline!(
                        "clauth: failed to save token_speed.json; retrying and keeping the speed days owed"
                    );
                    self.save_failed_logged = true;
                }
            }
            outcome @ (crate::token_ledger::FoldOutcome::Saved
            | crate::token_ledger::FoldOutcome::Unchanged) => {
                if self.save_failed_logged {
                    crate::logline::logline!("clauth: token_speed.json saves recovered");
                    self.save_failed_logged = false;
                }
                self.drop_folded_held(yesterday);
                if outcome == crate::token_ledger::FoldOutcome::Saved {
                    self.ledger_ever = None;
                    self.ever_dirty = true;
                }
            }
        }
    }

    /// Drop the held days a fold just landed, every one before `yesterday`,
    /// and every owed day before it: each was held, or had nothing to land.
    /// A day still waiting for its rebuild stays owed.
    fn drop_folded_held(&mut self, yesterday: &str) {
        let before = (self.held.len(), self.held_rate_peaks.len());
        self.held.retain(|d, _| d.as_str() >= yesterday);
        self.held_rate_peaks.retain(|d, _| d.as_str() >= yesterday);
        if (self.held.len(), self.held_rate_peaks.len()) != before {
            self.ever_dirty = true;
        }
        self.owed
            .retain(|d| d.as_str() >= yesterday || self.unbuilt.contains(d));
    }

    /// True when `token_speed.json`'s (ctime, mtime, len) changed since last
    /// seen. ctime is included because a chmod (the common permission-blip
    /// fix) moves only ctime.
    fn reload_speed_ledger_if_changed(&mut self, dir: &std::path::Path) -> bool {
        let path = dir.join("token_speed.json");
        let sig = std::fs::metadata(&path)
            .ok()
            .map(|m| {
                #[cfg(unix)]
                let ctime = {
                    use std::os::unix::fs::MetadataExt;
                    (m.ctime() as u64, m.ctime_nsec() as u64)
                };
                #[cfg(not(unix))]
                let ctime = (0, 0);
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                (ctime.0, ctime.1, mtime, m.len())
            })
            .unwrap_or((0, 0, 0, 0));
        if sig != self.speed_ledger_sig {
            self.speed_ledger_sig = sig;
            true
        } else {
            false
        }
    }

    /// Re-walk every transcript store, recording each file's mtime and len. A
    /// NEW file whose mtime predates the cutoff joins at `offset = len` (its
    /// lines cannot be newer than its mtime), so a cold start never reads the
    /// whole history; a cold re-entry whose path holds a 429 this producer
    /// already delivered does re-read from byte 0 on its next read, bound at
    /// that join length, so the line's position follows it into the new life
    /// and nothing below the join delivers. A hot new file joins at 0 and is
    /// read with the cutoff skip. A file that vanished is dropped.
    fn discover(&mut self, now_ms: u64) {
        let cutoff = cutoff_ms(now_ms);
        let mut found: HashMap<PathBuf, Walked> = HashMap::new();
        if let Ok(projects) = crate::profile::claude_dir().map(|d| d.join("projects")) {
            walk_jsonl(&projects, WALK_DEPTH, &mut found);
        }
        // Isolated runtimes keep their own store under `profiles/<name>/runtime-isolated-<sid>/projects`.
        if let Ok(profiles) = crate::profile::clauth_dir().map(|d| d.join("profiles"))
            && let Ok(entries) = std::fs::read_dir(&profiles)
        {
            for profile in entries.flatten() {
                let Ok(children) = std::fs::read_dir(profile.path()) else {
                    continue;
                };
                for child in children.flatten() {
                    let Some(name) = child.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if !name.starts_with("runtime-isolated") {
                        continue;
                    }
                    if crate::runtime::sid_of_runtime_dir_name(&name).is_none() {
                        continue;
                    }
                    let projects = child.path().join("projects");
                    if projects.is_dir() {
                        walk_jsonl(&projects, WALK_DEPTH, &mut found);
                    }
                }
            }
        }
        self.files.retain(|path, _| found.contains_key(path));
        self.last_user
            .retain(|path, _| self.files.contains_key(path));
        // A path that left the walk loses its success record with its
        // `FileEntry`: a re-entry is a new life, and positions from the old one
        // must never compare. A hot re-entry (offset 0) or a cold one whose
        // path holds a delivered 429 (a byte-0 re-read) rebuilds the record on
        // its next read; a cold entry with no delivered 429 joins at the file's
        // length and keeps none until a later success is appended.
        self.spend_cap_recoveries.retain(|_, by_path| {
            by_path.retain(|path, _| self.files.contains_key(path));
            !by_path.is_empty()
        });
        for (path, w) in found {
            match self.files.entry(path) {
                Entry::Occupied(mut e) => {
                    e.get_mut().mtime_ms = w.mtime_ms;
                }
                Entry::Vacant(v) => {
                    let hot = w.mtime_ms >= cutoff;
                    // A cold entry at a path holding a 429 this producer
                    // already delivered must read the line again in its new
                    // life, or a latch holding its uuid keeps comparing
                    // against the old life and falls back to stamps. The
                    // re-read starts at byte 0 with the join length as its
                    // position bound, so the delivered line's position moves
                    // and nothing below the join delivers; a cold entry with
                    // no delivered 429 keeps the join-at-length read.
                    let session = session_key(v.key()).unwrap_or_default();
                    let reread = hot
                        || self.delivered_facts.get(&session).is_some_and(|delivered| {
                            delivered.values().any(|pos| pos.file == *v.key())
                        });
                    let runtime = isolated_sid(v.key());
                    let rank = Arc::new(FileRank {
                        path: v.key().clone(),
                        created: w.created,
                    });
                    let life = next_life(&mut self.next_life);
                    v.insert(FileEntry {
                        offset: if hot { 0 } else { w.len },
                        life,
                        mtime_ms: w.mtime_ms,
                        read_mtime: w.mtime_ms,
                        ident: None,
                        tail: None,
                        rank,
                        session,
                        runtime,
                        reread,
                    });
                }
            }
        }
        // A key in several isolated runtimes takes one whose live row exists
        // (its `current_member` is who spends the session; a dead runtime
        // only offers its launch profile), else any; among equals the
        // earliest-created file, so the answer never depends on read order.
        let mut sids: HashMap<&str, HashSet<&str>> = HashMap::new();
        for e in self.files.values() {
            if let Some((sid, _)) = &e.runtime {
                sids.entry(e.session.as_str())
                    .or_default()
                    .insert(sid.as_str());
            }
        }
        let live: HashSet<String> = sids
            .values()
            .filter(|s| s.len() > 1)
            .flatten()
            .filter(|sid| crate::live_sessions::get(sid).is_some())
            .map(|sid| (*sid).to_owned())
            .collect();
        self.runtimes.clear();
        for e in self.files.values() {
            let Some(runtime) = &e.runtime else {
                continue;
            };
            match self.runtimes.entry(e.session.clone()) {
                Entry::Occupied(mut best) => {
                    let (rank, held) = best.get();
                    let is_live = live.contains(&runtime.0);
                    let held_live = live.contains(&held.0);
                    if (is_live && !held_live) || (is_live == held_live && e.rank.before(rank)) {
                        best.insert((Arc::clone(&e.rank), runtime.clone()));
                    }
                }
                Entry::Vacant(v) => {
                    v.insert((Arc::clone(&e.rank), runtime.clone()));
                }
            }
        }
    }

    /// Tail only the hot set — files whose recorded mtime is at or after
    /// `floor` (the retention cutoff, or an owed-day rebuild's lower floor) —
    /// so an unchanged tree costs one `stat` per hot file. A file whose
    /// identity changed or that shrank re-reads from byte 0; one that grew, or
    /// whose mtime moved at equal length, first checks the bytes before its
    /// offset and re-reads from byte 0 when they changed. The response dedupe
    /// by id makes every re-read count each response once.
    fn tail_all(&mut self, now_ms: u64, floor: u64) {
        let hot: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(_, e)| e.mtime_ms >= floor)
            .map(|(p, _)| p.clone())
            .collect();
        // One owner-store load for every session a read may meet first (a
        // restart meets them all at once), never one per session.
        let missing: HashSet<String> = hot
            .iter()
            .filter_map(|p| self.files.get(p))
            .filter(|e| !self.store_owners.contains_key(&e.session))
            .map(|e| e.session.clone())
            .collect();
        if !missing.is_empty() {
            self.store_owners
                .extend(crate::sessions::owners_of(missing));
        }
        for path in hot {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let (len, mtime, ident) = (meta.len(), mtime_of(&meta), ident_of(&meta));
            let Some(e) = self.files.get_mut(&path) else {
                continue;
            };
            let mut fresh = false;
            if e.ident.is_some_and(|known| known != ident) {
                e.rank = Arc::new(FileRank {
                    path: path.clone(),
                    created: meta.created().ok(),
                });
                e.reread = true;
                fresh = true;
            }
            if len < e.offset {
                e.reread = true;
                fresh = true;
            }
            if !e.reread && len == e.offset && mtime == e.read_mtime {
                continue;
            }
            // The spend-cap bound for this read: a fact line delivers only when
            // it ends past the bytes already consumed, so this is the old read
            // offset. A file that shrank re-reads from byte 0 and delivers
            // every fact line, hence 0. A cold-joined file's first read is
            // bound at its join length, so its history mints nothing on that
            // read; the same history later delivers through a shrink, a rescue
            // move (byte 0 as a hot file), or a walk miss after the file turned
            // hot (a re-entered hot entry at offset 0). The bound is the
            // position gate every fact line passes; a 429's line `uuid` is the
            // additional gate that folds a re-read 429 at most once this
            // producer life.
            let facts_through = if len < e.offset { 0 } else { e.offset };
            // A re-read from byte 0 keeps the entry's life only when the bytes
            // before the old offset still hash to the stored tail; the check
            // carries that offset beside the hash because the read itself
            // starts at 0. A missing tail vouches for nothing once the file
            // has consumed bytes.
            let old_life = e.life;
            let check = if fresh {
                None
            } else if e.reread {
                match e.tail {
                    Some(hash) if e.offset > 0 => Some((e.offset, hash)),
                    Some(_) => None,
                    None if e.offset > 0 => {
                        fresh = true;
                        None
                    }
                    None => None,
                }
            } else {
                e.tail.map(|hash| (e.offset, hash))
            };
            let start = if e.reread { 0 } else { e.offset };
            let mut life = if fresh {
                next_life(&mut self.next_life)
            } else {
                old_life
            };
            let pass = ReadPass {
                start,
                verify: check,
                facts_through,
                life,
            };
            let read = match self.read_append(&path, pass, now_ms, floor) {
                ReadOutcome::Read(read) => Some(read),
                ReadOutcome::Rewritten => {
                    life = next_life(&mut self.next_life);
                    let retry = ReadPass {
                        start: 0,
                        verify: None,
                        facts_through,
                        life,
                    };
                    match self.read_append(&path, retry, now_ms, floor) {
                        ReadOutcome::Read(read) => Some(read),
                        ReadOutcome::Rewritten | ReadOutcome::Failed => None,
                    }
                }
                ReadOutcome::Failed => None,
            };
            let Some((end, tail)) = read else {
                continue;
            };
            if let Some(e) = self.files.get_mut(&path) {
                e.offset = end;
                e.tail = tail;
                e.read_mtime = mtime;
                e.ident = Some(ident);
                e.life = life;
                e.reread = false;
            }
        }
    }

    /// Read complete lines from [`pass`](ReadPass)'s start and fold them into
    /// the response set. Its `verify` hash, with the offset it covers up to,
    /// must match the up-to-256 bytes before that offset (one read covers
    /// both), else the file was rewritten and nothing is consumed. A read from
    /// byte 0 drops assistant lines before `floor`. Only whole
    /// (newline-terminated) lines are consumed, so a line torn mid-write is
    /// re-read on the next tick; the result carries the new offset and the hash
    /// of the bytes before it.
    fn read_append(&mut self, path: &Path, pass: ReadPass, now_ms: u64, floor: u64) -> ReadOutcome {
        let ReadPass {
            start,
            verify,
            facts_through,
            life,
        } = pass;
        let from = start.saturating_sub(TAIL_BYTES);
        let Ok(mut file) = std::fs::File::open(path) else {
            return ReadOutcome::Failed;
        };
        if file.seek(SeekFrom::Start(from)).is_err() {
            return ReadOutcome::Failed;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return ReadOutcome::Failed;
        }
        // `start - from` never exceeds TAIL_BYTES, so it fits a usize.
        let skip = (start - from) as usize;
        let Some(body) = buf.get(skip..) else {
            return ReadOutcome::Rewritten;
        };
        if let Some((at, want)) = verify {
            // The checked bytes end at the offset the hash covers. An offset
            // before this read's own start vouches for nothing the buffer
            // holds, so the bytes cannot be confirmed unchanged.
            let (Some(lo), Some(hi)) = (
                at.saturating_sub(TAIL_BYTES).checked_sub(from),
                at.checked_sub(from),
            ) else {
                return ReadOutcome::Rewritten;
            };
            let Some(checked) = buf.get(lo as usize..hi as usize) else {
                return ReadOutcome::Rewritten;
            };
            if tail_hash(checked) != want {
                return ReadOutcome::Rewritten;
            }
        }
        let Some(file_ctx) = self.files.get(path).map(|e| ReadCtx {
            rank: Arc::clone(&e.rank),
            session: e.session.clone(),
            life,
        }) else {
            return ReadOutcome::Failed;
        };
        if start == 0 {
            self.last_user.remove(path);
        }
        self.see_session(&file_ctx.session, now_ms);
        let cutoff = if start == 0 { floor } else { 0 };
        let mut consumed = 0usize;
        // The latest-positioned successful turn in THIS file, folded here and
        // written once per file below: a success in a file is that file's
        // success record alone, and writing it line by line would allocate the
        // conversation key and the path for every assistant line of a cold
        // start's hot-set re-read.
        let mut max_success: Option<(u64, u64)> = None;
        let mut success_by_model: HashMap<String, SpendCapRecovery> = HashMap::new();
        for seg in body
            .split_inclusive(|&b| b == b'\n')
            .filter(|s| s.last() == Some(&b'\n'))
        {
            consumed += seg.len();
            let end_offset = start + consumed as u64;
            if let Some(ts) = self.process_line(
                &file_ctx,
                strip_crlf(seg),
                end_offset,
                cutoff,
                now_ms,
                facts_through,
                &mut success_by_model,
            ) {
                max_success = Some(max_success.map_or((end_offset, ts), |(end, latest)| {
                    (end.max(end_offset), latest.max(ts))
                }));
            }
        }
        if !success_by_model.is_empty() {
            let file_turns = self
                .spend_cap_turns_by_model
                .entry(file_ctx.session.clone())
                .or_default()
                .entry(path.to_path_buf())
                .or_default();
            for (model, turn) in success_by_model {
                note_turn(file_turns, turn, &model);
            }
        }
        if let Some((end, ts)) = max_success {
            let recovery = SpendCapRecovery { life, end, ts };
            self.spend_cap_recoveries
                .entry(file_ctx.session.clone())
                .or_default()
                .entry(path.to_path_buf())
                .and_modify(|held| {
                    if recovery.life != held.life {
                        *held = recovery;
                    } else {
                        held.end = held.end.max(recovery.end);
                        held.ts = held.ts.max(recovery.ts);
                    }
                })
                .or_insert(recovery);
        } else if let Some(by_path) = self.spend_cap_recoveries.get_mut(&file_ctx.session) {
            // This life of the path holds no success of its own, so a record
            // from an older life is replaced by that absence: only positions
            // from one life ever compare.
            let stale = by_path.get(path).is_some_and(|held| held.life != life);
            if stale {
                by_path.remove(path);
                if by_path.is_empty() {
                    self.spend_cap_recoveries.remove(&file_ctx.session);
                }
            }
        }
        let end_in_buf = skip + consumed;
        let tail = buf
            .get(end_in_buf.saturating_sub(TAIL_BYTES as usize)..end_in_buf)
            .map(tail_hash);
        ReadOutcome::Read((start + consumed as u64, tail))
    }

    /// One line's effect on the producer's per-line maps. Returns the successful
    /// assistant turn's timestamp (`Some`) so `read_append` can fold the file's
    /// success record once, `None` for every other line, a success this read's
    /// position bound skips included. A success the bound passes is noted in
    /// `success_by_model` too, under its model.
    #[allow(clippy::too_many_arguments)]
    fn process_line(
        &mut self,
        file: &ReadCtx,
        line: &str,
        end_offset: u64,
        cutoff_ms: u64,
        now_ms: u64,
        facts_through: u64,
        success_by_model: &mut HashMap<String, SpendCapRecovery>,
    ) -> Option<u64> {
        let parsed = parse_line(line)?;
        match parsed {
            Parsed::SpendCap429 { ts, uuid } => {
                let file_path = file.rank.path.clone();
                if !self.spend_cap_429_take(
                    &file.session,
                    uuid.as_deref(),
                    &file_path,
                    file.life,
                    end_offset,
                    spend_cap_take(facts_through, end_offset),
                ) {
                    return None;
                }
                let fact = SpendCap429Fact {
                    ts,
                    file: file_path,
                    life: file.life,
                    end: end_offset,
                    uuid,
                };
                let by_file = self.spend_cap_429.entry(file.session.clone()).or_default();
                if let Some(latest) = by_file.get_mut(&fact.file) {
                    if fact.supersedes(latest) {
                        *latest = fact;
                    }
                } else {
                    by_file.insert(fact.file.clone(), fact);
                }
                None
            }
            Parsed::User { ts } => {
                self.last_user.insert(file.rank.path.clone(), ts);
                None
            }
            Parsed::Assistant {
                ts,
                id,
                model,
                output,
            } => {
                // The turn on its model passes the same position bound as the
                // file's success record, so a re-read delivers it exactly when
                // the record takes it again (a shrink's byte-0 re-read does).
                if spend_cap_take(facts_through, end_offset) {
                    let turn = SpendCapRecovery {
                        life: file.life,
                        end: end_offset,
                        ts,
                    };
                    note_turn(success_by_model, turn, &model);
                }
                // A pre-cutoff user line still updates the span start; a pre-cutoff
                // response is simply not retained. A pre-cutoff success still
                // feeds its file's success record unless this read's position
                // bound already passed it.
                if ts < cutoff_ms {
                    return spend_cap_take(facts_through, end_offset).then_some(ts);
                }
                let start_ms = self.last_user.get(file.rank.path.as_path()).copied();
                let msg_key = response_key(&file.session, id.as_deref(), start_ms.unwrap_or(0));
                let existed = self.responses.contains_key(&msg_key);
                let prev_output = self.responses.get(&msg_key).map(|r| r.output).unwrap_or(0);
                let prev_end = self.responses.get(&msg_key).map(|r| r.end_ms).unwrap_or(0);
                // One id in two files (a fork's copy): the response belongs to
                // the earliest-created file, whatever order the reads ran in.
                let reown = self
                    .responses
                    .get(&msg_key)
                    .is_some_and(|r| r.file != file.rank && file.rank.before(&r.file));
                let account = self
                    .timeline
                    .member_for(&file.session, start_ms.unwrap_or(ts))
                    .to_owned();
                let resp = upsert_response(
                    &mut self.responses,
                    msg_key,
                    Resp {
                        session: file.session.clone(),
                        account,
                        model,
                        start_ms,
                        end_ms: ts,
                        output,
                        file: Arc::clone(&file.rank),
                    },
                );
                if reown {
                    resp.session.clone_from(&file.session);
                    resp.file = Arc::clone(&file.rank);
                    resp.start_ms = start_ms;
                    let account = self
                        .timeline
                        .member_for(&file.session, start_ms.unwrap_or(resp.end_ms));
                    account.clone_into(&mut resp.account);
                    // The owner's span start and member may differ from the
                    // copy's: the live days' settled values re-derive.
                    self.settle_rederive.insert(day_of_ms(now_ms));
                    self.settle_rederive
                        .insert(day_of_ms(now_ms.saturating_sub(DAY_MS)));
                    self.peaks_dirty = true;
                }
                // The recompute is owed only when the response's shape changed:
                // it is new, or its output or end moved. A smaller duplicate
                // partial changes nothing and owes nothing.
                let grew = output >= prev_output && (output != prev_output || ts > prev_end);
                if !existed || grew {
                    self.peaks_dirty = true;
                    // A line that created or grew a response whose support
                    // touches already-settled windows (its span start, or a
                    // span-less response's old end, below the boundary) owes a
                    // from-scratch re-derive of those days: the settled value
                    // may hold the old shape, or the late response was never
                    // settled.
                    {
                        // The response's support's leftmost point, read from the
                        // stored response (never the current `last_user`, which a
                        // user line between two block lines moves): its kept span
                        // start, or — span-less — the point its tokens landed at
                        // (its old end, or this line for a first-seen one).
                        let leftmost = match resp.start_ms {
                            Some(s) => s,
                            None => {
                                if existed && prev_end > 0 {
                                    prev_end
                                } else {
                                    ts
                                }
                            }
                        };
                        if leftmost < self.settled_through {
                            // The response's support before and after this
                            // line spans [lo, hi] (its kept span start or old
                            // point, its old end, and the new line). Re-derive
                            // each day the recompute keeps — today and
                            // yesterday — whose UTC range intersects the
                            // support's filled windows. A window belongs to
                            // the day it ENDS on, and a support [lo, hi] fills
                            // windows ending up to hi + LIVE_WINDOW_MS (the
                            // window is closed: a point at hi counts in the
                            // window ending at hi + LIVE_WINDOW_MS), so the
                            // intersection is [lo, hi + LIVE_WINDOW_MS]. Two
                            // bounded checks, never a walk whose end depends
                            // on the order of two timestamps.
                            let mut lo = leftmost.min(ts);
                            let mut hi = leftmost.max(ts);
                            if prev_end > 0 {
                                lo = lo.min(prev_end);
                                hi = hi.max(prev_end);
                            }
                            let hi = hi.saturating_add(LIVE_WINDOW_MS);
                            let today = day_of_ms(now_ms);
                            let yesterday = day_of_ms(now_ms - DAY_MS);
                            for day in [today, yesterday] {
                                let ds = day_start_ms(&day);
                                let de = ds.saturating_add(DAY_MS);
                                if lo < de && hi >= ds {
                                    self.settle_rederive.insert(day);
                                }
                            }
                        }
                    }
                }
                // A success line is never uuid-deduped, so a shrink or a walk
                // miss that re-reads it under a bound below its end folds it
                // into the path's success record again, at the position it now
                // holds in the file's new life. The scheduler pulls that record
                // on every scan and drops a latch on it only when it lies after
                // the latched 429 in that life; when the latched 429 holds no
                // position in the success's life (a uuid-less 429, or a new
                // life whose bytes no longer hold the 429's line) it falls back
                // to comparing stamps.
                if !spend_cap_take(facts_through, end_offset) {
                    return None;
                }
                Some(ts)
            }
        }
    }

    /// Whether a 429 line should fold a fact now, moving an already-delivered
    /// line's recorded position on the way. A uuid-less line folds whenever the
    /// read's position bound passes it. A line whose uuid this conversation
    /// already delivered never folds again and never re-latches: it only moves
    /// its recorded position, in the SAME path, whatever the bound, because a
    /// latch pulls that record before judging a successful turn. A read at
    /// another path (a rescue copy) leaves it alone: a latch is never moved to
    /// a different path. An undelivered line is recorded and folds only when
    /// the bound passes it, so a read that skips history neither delivers nor
    /// records it. The record is never pruned.
    fn spend_cap_429_take(
        &mut self,
        session: &str,
        uuid: Option<&str>,
        path: &Path,
        life: FileLife,
        end: u64,
        within_bound: bool,
    ) -> bool {
        let Some(uuid) = uuid else {
            return within_bound;
        };
        if let Some(position) = self
            .delivered_facts
            .get_mut(session)
            .and_then(|delivered| delivered.get_mut(uuid))
        {
            if position.file.as_path() == path {
                position.life = life;
                position.end = end;
            }
            return false;
        }
        if !within_bound {
            return false;
        }
        let positions = match self.delivered_facts.get_mut(session) {
            Some(positions) => positions,
            None => self.delivered_facts.entry(session.to_owned()).or_default(),
        };
        positions.insert(uuid.to_owned(), SpendCapPos::at(path, life, end));
        true
    }

    /// Note a session at the start of a read of one of its files: remember
    /// the isolated runtime it runs in (`runtimes`' pick, whichever of its
    /// files is read first), and seed its attribution timeline at first
    /// sight, so every response the read creates takes its member from the
    /// timeline.
    fn see_session(&mut self, session: &str, now_ms: u64) {
        if let Some((_, runtime)) = self.runtimes.get(session) {
            self.session_isolated
                .entry(session.to_owned())
                .or_insert_with(|| runtime.clone());
        }
        if self.seen.contains(session) {
            return;
        }
        self.seen.insert(session.to_owned());
        let observed = self.observe_sources(session);
        self.current
            .insert(session.to_owned(), observed.member.clone());
        self.observe_first(session, &observed, now_ms);
    }

    /// A session's first observation against the timeline this life: a
    /// session it knows appends a change made while no producer of this life
    /// watched (a swap during a restart); a session it does not know gets its
    /// seed.
    fn observe_first(&mut self, session: &str, observed: &Observed, now_ms: u64) {
        if self.timeline.by_session.contains_key(session) {
            self.record_change(session, observed, now_ms);
        } else {
            self.seed(session, observed);
        }
    }

    /// Observe every held session's current member and record each change in
    /// its timeline; responses of a session whose timeline changed take their
    /// member again.
    fn observe_sessions(&mut self, now_ms: u64) {
        let sessions: Vec<String> = self
            .responses
            .values()
            .map(|r| r.session.as_str())
            .collect::<HashSet<&str>>()
            .into_iter()
            .map(str::to_owned)
            .collect();
        if self.store_owners_mtime != self.store_mtime
            || sessions.iter().any(|s| !self.store_owners.contains_key(s))
        {
            self.store_owners = crate::sessions::owners_of(sessions.iter().cloned());
            self.store_owners_mtime = self.store_mtime;
        }
        let mut changed: HashSet<String> = HashSet::new();
        for session in &sessions {
            let observed = self.observe_sources(session);
            self.current
                .insert(session.clone(), observed.member.clone());
            if !self.timeline.by_session.contains_key(session) {
                self.seed(session, &observed);
                changed.insert(session.clone());
            } else if self.record_change(session, &observed, now_ms) {
                changed.insert(session.clone());
            }
        }
        self.reattribute(&changed, now_ms);
    }

    /// The responses of every session in `changed` take their member from
    /// the timeline again.
    fn reattribute(&mut self, changed: &HashSet<String>, now_ms: u64) {
        if changed.is_empty() {
            return;
        }
        let mut moved = false;
        for r in self.responses.values_mut() {
            if !changed.contains(&r.session) {
                continue;
            }
            let member = self
                .timeline
                .member_for(&r.session, r.start_ms.unwrap_or(r.end_ms));
            if r.account != member {
                member.clone_into(&mut r.account);
                moved = true;
            }
        }
        if moved {
            self.peaks_dirty = true;
            self.settle_rederive.insert(day_of_ms(now_ms));
            self.settle_rederive
                .insert(day_of_ms(now_ms.saturating_sub(DAY_MS)));
        }
    }

    /// Seed a session's timeline at first sight: a live row that swapped
    /// before this producer saw the session seeds its launch profile then the
    /// swap, else the observed member from the start.
    fn seed(&mut self, session: &str, observed: &Observed) {
        let entry = |since: u64, member: &str| TimelineEntry {
            session: session.to_owned(),
            since,
            member: member.to_owned(),
        };
        let member = observed.member.as_deref().unwrap_or(UNATTRIBUTED);
        match &observed.swap_seed {
            Some((start_profile, at)) => {
                self.timeline.push(entry(0, start_profile));
                self.timeline.push(entry(*at, member));
            }
            None => self.timeline.push(entry(0, member)),
        }
    }

    /// True when `observed` names a member other than the session's
    /// last-added entry's. An observation naming no member is never a change:
    /// no source answers any more (a session that ended).
    fn is_change(&self, session: &str, observed: &Observed) -> bool {
        let Some(member) = observed.member.as_deref() else {
            return false;
        };
        self.timeline
            .by_session
            .get(session)
            .and_then(|entries| entries.last())
            .is_none_or(|e| e.member != member)
    }

    /// Append an entry when the observed member differs from the session's
    /// last-added one, from the source's own stamp, else from now. Responses
    /// read it in clock order ([`Timeline::member_for`]): a stamp below the
    /// session's newest entry's (a hook record older than the last swap, read
    /// once the live row is gone) binds only up to that entry's instant, so it
    /// never moves a response spent after the newer stamp. Nothing bounds a
    /// real swap stamped under a clock stepped back: its reading is the
    /// step-back class `member_for` names, live, restarted and folded.
    /// Returns whether an entry was added.
    fn record_change(&mut self, session: &str, observed: &Observed, now_ms: u64) -> bool {
        if !self.is_change(session, observed) {
            return false;
        }
        let Some(member) = observed.member.clone() else {
            return false;
        };
        self.timeline.push(TimelineEntry {
            session: session.to_owned(),
            since: observed.since.unwrap_or(now_ms),
            member,
        });
        true
    }

    /// The member a session spends now, by the attribution precedence over its
    /// files: an isolated runtime's live row (its `current_member` follows
    /// swaps), then the hook's resolved account, then the owner store, then
    /// the isolated store's launch profile.
    fn observe_sources(&self, session: &str) -> Observed {
        let hook = || crate::hook_note::resolved_account_at(session);
        // A session new to the batch (its first read) loads the store alone.
        let owner = || match self.store_owners.get(session) {
            Some(owner) => owner.clone(),
            None => crate::sessions::owners_of([session.to_owned()])
                .remove(session)
                .flatten(),
        };
        let Some((sid, profile)) = self.session_isolated.get(session) else {
            return match hook() {
                Some((member, at)) => Observed {
                    member: Some(member),
                    since: at,
                    swap_seed: None,
                },
                None => Observed {
                    member: owner(),
                    since: None,
                    swap_seed: None,
                },
            };
        };
        if let Some(row) = crate::live_sessions::get(sid) {
            let swap_seed = match (&row.current_member, row.last_swap_at) {
                (Some(current), Some(at)) if *current != row.start_profile => {
                    Some((row.start_profile.clone(), at))
                }
                _ => None,
            };
            let since = row.last_swap_at.or_else(|| hook().and_then(|(_, at)| at));
            return Observed {
                member: Some(row.current_member.unwrap_or(row.start_profile)),
                since,
                swap_seed,
            };
        }
        match hook() {
            Some((member, at)) => Observed {
                member: Some(member),
                since: at,
                swap_seed: None,
            },
            None => Observed {
                member: owner().or_else(|| Some(profile.clone())),
                since: None,
                swap_seed: None,
            },
        }
    }

    fn build_snapshot(&mut self, now_ms: u64, live: &BTreeSet<String>) -> Snapshot {
        let today = day_of_ms(now_ms);
        let yesterday = day_of_ms(now_ms - DAY_MS);
        self.recompute_peaks(&today, &yesterday, live, now_ms);
        let peaks = self.build_peaks(&today);
        let h1_start = now_ms.saturating_sub(H1_WINDOW_MS);
        let mut by_account: BTreeMap<&str, Vec<&Resp>> = BTreeMap::new();
        let mut by_session: BTreeMap<&str, Vec<&Resp>> = BTreeMap::new();
        let mut all: Vec<&Resp> = Vec::new();
        for r in self.responses.values() {
            all.push(r);
            if r.end_ms >= h1_start {
                by_session.entry(r.session.as_str()).or_default().push(r);
                if r.account != UNATTRIBUTED {
                    by_account.entry(r.account.as_str()).or_default().push(r);
                }
            }
        }
        let sessions = by_session
            .into_iter()
            .map(|(sid, rs)| {
                let figures = SessionFigures {
                    account: self.current.get(sid).cloned().flatten(),
                    figures: figures_for(&rs, now_ms),
                };
                (sid.to_owned(), figures)
            })
            .collect();
        let accounts = by_account
            .into_iter()
            .map(|(name, rs)| (name.to_owned(), figures_for(&rs, now_ms)))
            .collect();
        let mut days = self.held.clone();
        days.extend(self.days.iter().map(|(d, a)| (d.clone(), a.clone())));
        let mut rate_peaks = self.held_rate_peaks.clone();
        rate_peaks.extend(self.rate_peaks.iter().map(|(d, p)| (d.clone(), p.clone())));
        Snapshot {
            version: CACHE_VERSION,
            generated_at: iso_ms_utc(now_ms),
            total: figures_for(&all, now_ms),
            accounts,
            sessions,
            days,
            rate_peaks,
            peaks,
        }
    }

    /// Recompute the live days' sums and peaks (today, yesterday and any future
    /// day holding a response, `live`; the settled values cover today and
    /// yesterday). Each cell and rate grouping keeps an in-memory settled value
    /// covering exactly the windows ending before the boundary
    /// `settled_through` (rate) and the responses ending before
    /// `settled_through - LIVE_WINDOW_MS` (speed); the shown peak is
    /// max(settled, the sweep over the later windows). The settled part is
    /// advanced every tick, the sums and the shown part when the response set
    /// or the day changed (a clock step's re-derive sets `peaks_dirty`, so its
    /// pre-step windows re-cover then). Every value derives from the
    /// responses' current state, never from a running max of samples.
    fn recompute_peaks(
        &mut self,
        today: &str,
        yesterday: &str,
        live_days: &BTreeSet<String>,
        now_ms: u64,
    ) {
        let days = (today.to_owned(), yesterday.to_owned());
        let frozen_cutoff = now_ms.saturating_sub(SETTLE_LAG_MS);
        let s = frozen_cutoff.saturating_add(LIVE_WINDOW_MS);
        // A tick whose boundary sits behind the clock is a clock step back:
        // lower the boundary to this tick's value and re-derive both live days
        // from scratch. The shown sweep runs uncapped to the day's end, so the
        // re-derive's own `peaks_dirty` re-shows the pre-step windows from
        // data; no timer is needed.
        if s < self.settled_through {
            self.settled_through = s;
            self.settle_rederive.insert(today.to_owned());
            self.settle_rederive.insert(yesterday.to_owned());
            self.peaks_dirty = true;
        }
        let dirty = self.peaks_dirty || self.last_peaks_days.as_ref() != Some(&days);
        if dirty {
            self.derive_days(live_days);
        }

        let attr_rs: Vec<&Resp> = self.responses.values().collect();
        let (attr_total, attr_by_account, attr_by_model) = group_responses(&attr_rs);
        let settle_days: BTreeSet<String> =
            BTreeSet::from([today.to_owned(), yesterday.to_owned()]);

        // Settled (every tick, incremental): only the windows that crossed the
        // boundary since the last settle, [prev, s), over the responses whose
        // support touches them (the sweep's own O(n) filter); speed over the
        // responses ending in [prev - LIVE, s - LIVE). Never re-sort over every
        // response.
        let prev = self.settled_through;
        let speed_lo = prev.saturating_sub(LIVE_WINDOW_MS);
        let settle_speed: Vec<&Resp> = attr_rs
            .iter()
            .copied()
            .filter(|r| r.end_ms >= speed_lo && r.end_ms < frozen_cutoff)
            .collect();
        let fresh_settled_speed = speed_peaks_over(&settle_speed, &settle_days);
        self.settled_speed = merge_speed_peaks(&self.settled_speed, &fresh_settled_speed);
        for (day, day_start) in [
            (today, day_start_ms(today)),
            (yesterday, day_start_ms(yesterday)),
        ] {
            let full_hi = day_start + DAY_MS - 1;
            let lo = prev.max(day_start);
            let hi = s.saturating_sub(1).min(full_hi);
            let settled_rp = day_rate_peaks(&attr_total, &attr_by_account, &attr_by_model, lo, hi);
            let merged = merge_rate_peaks(
                self.settled_rate_peaks
                    .get(day)
                    .cloned()
                    .unwrap_or_default(),
                settled_rp,
            );
            if is_empty_rate_peaks(&merged) {
                self.settled_rate_peaks.remove(day);
            } else {
                self.settled_rate_peaks.insert(day.to_owned(), merged);
            }
        }
        self.settled_through = self.settled_through.max(s);

        // A line grew or created a response touching settled windows: re-derive
        // those days' settled from scratch (never max'd with the advanced
        // values, which may hold the stale shape). The boundary is
        // `settled_through` (already advanced, or already lowered by a clock
        // step), so the re-derive covers exactly the settled range and the
        // shown sweep below re-covers everything from the boundary to the day's
        // end.
        if !self.settle_rederive.is_empty() {
            let rederive = std::mem::take(&mut self.settle_rederive);
            let speed_boundary = self.settled_through.saturating_sub(LIVE_WINDOW_MS);
            let frozen_rs: Vec<&Resp> = attr_rs
                .iter()
                .copied()
                .filter(|r| r.end_ms < speed_boundary)
                .collect();
            for day in &rederive {
                if day != today && day != yesterday {
                    continue;
                }
                let day_rs: Vec<&Resp> = frozen_rs
                    .iter()
                    .copied()
                    .filter(|r| day_of_ms(r.end_ms) == *day)
                    .collect();
                let scratch = speed_peaks_over(&day_rs, &settle_days);
                match scratch.get(day) {
                    Some(cells) if !cells.is_empty() => {
                        self.settled_speed.insert(day.clone(), cells.clone());
                    }
                    _ => {
                        self.settled_speed.remove(day);
                    }
                }
                let day_start = day_start_ms(day);
                let full_hi = day_start + DAY_MS - 1;
                let rp = day_rate_peaks(
                    &attr_total,
                    &attr_by_account,
                    &attr_by_model,
                    day_start,
                    self.settled_through.saturating_sub(1).min(full_hi),
                );
                if is_empty_rate_peaks(&rp) {
                    self.settled_rate_peaks.remove(day);
                } else {
                    self.settled_rate_peaks.insert(day.clone(), rp);
                }
            }
        }

        if !dirty {
            self.last_peaks_days = Some(days);
            return;
        }

        // Shown: max(settled, the sweep over windows ending in
        // [settled_through, the day's end]) — the settle already covered the
        // day up to `settled_through`, so the shown sweep starts at the
        // advancing boundary. Future days hold no settled values, so their
        // shown sweep runs from their own midnight.
        let shown_lo = self.settled_through.saturating_sub(LIVE_WINDOW_MS);
        let shown_speed_rs: Vec<&Resp> = attr_rs
            .iter()
            .copied()
            .filter(|r| r.end_ms >= shown_lo)
            .collect();
        let shown_speed = merge_speed_peaks(
            &self.settled_speed,
            &speed_peaks_over(&shown_speed_rs, live_days),
        );
        for day in live_days {
            if let Some(accounts) = self.days.get_mut(day) {
                for (account, models) in accounts.iter_mut() {
                    for (model, sum) in models.iter_mut() {
                        sum.speed = shown_speed
                            .get(day)
                            .and_then(|a| a.get(account))
                            .and_then(|m| m.get(model))
                            .copied();
                    }
                }
            }
        }
        for day in live_days {
            // Uncapped: the shown range runs to the day's end, today's and
            // yesterday's alike. Only responses stamped after `now` can lift a
            // window ending after `now` above the window ending at `now`, so a
            // cap would only ever hide read data; a peak read before a clock
            // step stays shown with its real `at`.
            let day_start = day_start_ms(day);
            let full_hi = day_start + DAY_MS - 1;
            let lo = self.settled_through.max(day_start);
            let shown_rp =
                day_rate_peaks(&attr_total, &attr_by_account, &attr_by_model, lo, full_hi);
            let merged = merge_rate_peaks(
                self.settled_rate_peaks
                    .get(day)
                    .cloned()
                    .unwrap_or_default(),
                shown_rp,
            );
            if is_empty_rate_peaks(&merged) {
                self.rate_peaks.remove(day);
            } else {
                self.rate_peaks.insert(day.to_owned(), merged);
            }
        }
        self.peaks_dirty = false;
        self.last_peaks_days = Some(days);
        self.ever_dirty = true;
    }

    /// The feed's `peaks` object: today from the just-recomputed cells and rate
    /// peaks, `ever` as the max over every live day (today, yesterday, and any
    /// future day holding a response), every held day and every ledger day,
    /// per grouping. `ever` is rebuilt only when `ever_dirty`: a walk of the
    /// live and held days, combined with the ledger's own half, which is
    /// walked only after the ledger changed (a reload, a fold that saved).
    fn build_peaks(&mut self, today: &str) -> Peaks {
        let day_sets = [&self.days, &self.held, &self.speed_ledger.days];
        let rate_sets = [
            &self.rate_peaks,
            &self.held_rate_peaks,
            &self.speed_ledger.rate_peaks,
        ];
        let today_half = build_peaks_half(today, &day_sets, &rate_sets);
        if self.ever_dirty {
            let ledger = self.ledger_ever.get_or_insert_with(|| {
                #[cfg(test)]
                ledger_walks::count();
                EverBest::over(&[&self.speed_ledger.days], &[&self.speed_ledger.rate_peaks])
            });
            let mut ever = EverBest::over(
                &[&self.days, &self.held],
                &[&self.rate_peaks, &self.held_rate_peaks],
            );
            ever.absorb_later(ledger);
            self.ever_cache = ever.spell();
            self.ever_dirty = false;
        }
        Peaks {
            today: today_half,
            ever: self.ever_cache.clone(),
        }
    }

    /// Re-derive the live days' sums from the responses' current state: per
    /// (day, account, model) the output and generation span of every span-ful
    /// response ending on a live day. The cell speed peaks are filled by the
    /// recompute that follows.
    fn derive_days(&mut self, live_days: &BTreeSet<String>) {
        self.days = sum_days(self.responses.values(), live_days);
    }
}

/// One transcript's whole-life sums over every finished response (a finished
/// delegate job's own average); `None` when unreadable or holding none.
pub(crate) fn transcript_speed(path: &Path) -> Option<DaySum> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let key = session_key(path).unwrap_or_default();
    let rank = Arc::new(FileRank {
        path: path.to_path_buf(),
        created: None,
    });
    let mut responses: HashMap<String, Resp> = HashMap::new();
    let mut last_user: Option<u64> = None;
    for seg in buf
        .split_inclusive(|&b| b == b'\n')
        .filter(|s| s.last() == Some(&b'\n'))
    {
        let line = strip_crlf(seg);
        let Some(parsed) = parse_line(line) else {
            continue;
        };
        match parsed {
            Parsed::SpendCap429 { .. } => {}
            Parsed::User { ts } => last_user = Some(ts),
            Parsed::Assistant {
                ts,
                id,
                model,
                output,
            } => {
                let msg_key = response_key(&key, id.as_deref(), last_user.unwrap_or(0));
                upsert_response(
                    &mut responses,
                    msg_key,
                    Resp {
                        session: key.clone(),
                        account: UNATTRIBUTED.to_owned(),
                        model,
                        start_ms: last_user,
                        end_ms: ts,
                        output,
                        file: Arc::clone(&rank),
                    },
                );
            }
        }
    }
    let mut sum = DaySum::default();
    for r in responses.values() {
        if let Some((_, span)) = r.span() {
            sum.output = sum.output.saturating_add(r.output);
            sum.gen_ms = sum.gen_ms.saturating_add(span);
        }
    }
    (sum.output > 0 || sum.gen_ms > 0).then_some(sum)
}

/// The producer's cache, `None` when absent, unreadable, or written by a newer
/// version.
pub(crate) fn load_cache() -> Option<Snapshot> {
    let path = crate::profile::clauth_dir().ok()?.join(TPS_CACHE_FILE);
    let bytes = std::fs::read(path).ok()?;
    let snap: Snapshot = serde_json::from_slice(&bytes).ok()?;
    (snap.version <= CACHE_VERSION).then_some(snap)
}

/// `0` at zero, one decimal below 10, whole from 10. Branches on the rounded
/// value so `[9.95, 10)` reads "10", never "10.0".
pub(crate) fn format_figure(v: f64) -> String {
    let rounded = (v * 10.0).round() / 10.0;
    if rounded == 0.0 {
        "0".to_owned()
    } else if rounded < 10.0 {
        format!("{rounded:.1}")
    } else {
        format!("{rounded:.0}")
    }
}

/// A rate figure for a fixed-width cell: `format_figure` while it fits
/// `width`, a whole-`k` form (`13k`) once it would not, capped at `999k` for a
/// figure ≥ 999 500 so a number is never truncated into `…`. The overview's
/// t/s cell and the header's tok/s chip share this one formatter, so the chip's
/// figure stays within its four-cell ceiling past `9999`.
pub(crate) fn format_figure_capped(v: f64, width: usize) -> String {
    if v >= 999_500.0 {
        return "999k".to_owned();
    }
    let figure = format_figure(v);
    if figure.chars().count() > width {
        format!("{:.0}k", v / 1000.0)
    } else {
        figure
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

enum Parsed {
    User {
        ts: u64,
    },
    Assistant {
        ts: u64,
        /// The raw `message.id`, `None` when absent (keyed by path + user ts).
        id: Option<String>,
        model: String,
        output: u64,
    },
    /// A Claude Code API-error row naming the spend-cap 429: direct evidence
    /// the session runs a credit-billed model on a spent cap. Carries the
    /// row's timestamp so the scheduler can age a fact out, and no tokens — it
    /// never reaches a figure.
    SpendCap429 {
        ts: u64,
        /// The line's `uuid`, `None` when the row lacks one, holds a
        /// non-string or the empty string, or its re-parse failed; the
        /// spend-cap 429 dedupe falls back to the position bound on `None`.
        uuid: Option<String>,
    },
}

/// The `apiError` value the spend-cap 429 branch writes (`error: "rate_limit"`).
const SPEND_CAP_429_API_ERROR: &str = "model_requires_usage_credits";

/// A spend-cap 429 fact: its row timestamp, the transcript FILE it came from,
/// the file life and end offset of its line, and the line's `uuid`. The file is
/// what lets a later success in the SAME file clear it while a subagent's
/// success (a different file, the same conversation key) does not; the life
/// and offset are what let that success be compared to it by line order rather
/// than stamp; the uuid is what the producer's recorded position is pulled by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpendCap429Fact {
    pub(crate) ts: u64,
    pub(crate) file: PathBuf,
    pub(crate) life: FileLife,
    pub(crate) end: u64,
    pub(crate) uuid: Option<String>,
}

impl SpendCap429Fact {
    /// Whether this 429 is the later line of its file: the greater end offset
    /// when the two sit in one life, else the newer stamp (an equal stamp keeps
    /// the later read).
    fn supersedes(&self, other: &Self) -> bool {
        if self.life == other.life {
            self.end > other.end
        } else {
            self.ts >= other.ts
        }
    }
}

/// Total order over pending 429 facts: the higher stamp wins, ties broken by
/// path, so the pick never depends on `HashMap` iteration order.
fn pending_order(a: &SpendCap429Fact, b: &SpendCap429Fact) -> Ordering {
    a.ts.cmp(&b.ts).then_with(|| a.file.cmp(&b.file))
}

/// The 429 one conversation's scan latches: each file's later line, then the
/// highest stamp across files, ties broken by path. A total order, so the same
/// facts pick the same line whatever order the reads landed in.
fn pending_winner(facts: &HashMap<PathBuf, SpendCap429Fact>) -> Option<&SpendCap429Fact> {
    facts.values().max_by(|a, b| pending_order(a, b))
}

/// Where a delivered 429 line was last read: the transcript file, that file's
/// read life, and the line's end offset. A latch holding the line's `uuid`
/// pulls it before judging a successful turn, so the comparison always uses
/// where the line lies now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpendCapPos {
    file: PathBuf,
    life: FileLife,
    end: u64,
}

impl SpendCapPos {
    fn at(path: &Path, life: FileLife, end: u64) -> Self {
        Self {
            file: path.to_path_buf(),
            life,
            end,
        }
    }
}

/// A file's latest successful turn in its current read life (in
/// [`SpendCapDrain::turns_by_model`], one model's): where its line ends, and
/// its stamp for the fallback when a latch sits in another life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpendCapRecovery {
    pub(crate) life: FileLife,
    pub(crate) end: u64,
    pub(crate) ts: u64,
}

impl SpendCapRecovery {
    /// Whether this success lies after a 429 whose line ends at `end` in file
    /// life `life`, stamped `ts`: the greater end offset when the two sit in
    /// one life, else the strictly newer stamp, since positions from two lives
    /// never compare.
    pub(crate) fn lies_after(&self, life: FileLife, end: u64, ts: u64) -> bool {
        if self.life == life {
            self.end > end
        } else {
            self.ts > ts
        }
    }
}

/// One drain of the producer's pending spend-cap 429s, keyed by conversation id
/// (each conversation's one winner across its files), and of the successful
/// turns per model, keyed by conversation id then transcript FILE, both
/// consumed exactly once. A file's success record is not part of it: the
/// scheduler pulls that from the producer instead, so no scan consumes one.
#[derive(Debug, Default)]
pub(crate) struct SpendCapDrain {
    pub(crate) pending: HashMap<String, SpendCap429Fact>,
    pub(crate) turns_by_model: HashMap<String, HashMap<PathBuf, HashMap<String, SpendCapRecovery>>>,
}

/// Fold one successful turn on `model` into a latest-turn-per-model map, as a
/// file's success record folds: a turn from another read life replaces the one
/// held, since positions from two lives never compare, and within one life the
/// later line end and the newer stamp are kept. The model's name is allocated
/// only the first time the map sees it.
fn note_turn(
    by_model: &mut HashMap<String, SpendCapRecovery>,
    turn: SpendCapRecovery,
    model: &str,
) {
    match by_model.get_mut(model) {
        Some(held) if held.life == turn.life => {
            held.end = held.end.max(turn.end);
            held.ts = held.ts.max(turn.ts);
        }
        Some(held) => *held = turn,
        None => {
            by_model.insert(model.to_owned(), turn);
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireLine {
    timestamp: Option<String>,
    #[serde(rename = "isApiErrorMessage")]
    is_api_error_message: Option<bool>,
    #[serde(rename = "apiError")]
    api_error: Option<String>,
    message: Option<WireMsg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireMsg {
    id: Option<String>,
    model: Option<String>,
    role: Option<String>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireUsage {
    output_tokens: u64,
}

/// The `uuid` field of a 429 row, read back off the same line through the
/// shared lenient element reader. A re-parse failure (a token serde_json
/// refuses, like an out-of-range number) reads as absent and falls back to
/// the position bound, so no odd `uuid` shape can lose the 429.
#[derive(Deserialize, Default)]
#[serde(default)]
struct UuidOnly {
    uuid: Option<LenientString>,
}

/// Parse one JSONL line into a user line or an assistant usage line. `None`
/// for a malformed/timestamp-less/message-less line, a placeholder model, or a
/// zero-output line (the synthetic API-error shape).
fn parse_line(line: &str) -> Option<Parsed> {
    let wire: WireLine = serde_json::from_str(line).ok()?;
    let ts = parse_ts(wire.timestamp.as_deref()?)?;
    // The spend-cap 429 row is recognized off its own fields, before the
    // usage filters below: it carries no usable usage and must still add no
    // tokens, so it cannot ride the assistant path. Its timestamp is kept so
    // the scheduler can ignore a stale fact. Only this arm reads the uuid, so
    // a success line pays no uuid parse or allocation.
    if wire.is_api_error_message == Some(true)
        && wire.api_error.as_deref() == Some(SPEND_CAP_429_API_ERROR)
    {
        let uuid = serde_json::from_str::<UuidOnly>(line)
            .ok()
            .and_then(|w| w.uuid)
            .and_then(|u| u.0)
            .filter(|s| !s.is_empty());
        return Some(Parsed::SpendCap429 { ts, uuid });
    }
    let msg = wire.message.as_ref()?;
    if msg.role.as_deref() == Some("user") {
        return Some(Parsed::User { ts });
    }
    let usage = msg.usage.as_ref()?;
    if usage.output_tokens == 0 {
        return None;
    }
    let raw = msg.model.as_deref().unwrap_or("unknown");
    let model = crate::providers::deepseek_served_model(raw, msg.id.as_deref().unwrap_or_default())
        .unwrap_or(raw)
        .to_owned();
    if crate::tokens::is_placeholder_model(&model) {
        return None;
    }
    Some(Parsed::Assistant {
        ts,
        id: msg
            .id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        model,
        output: usage.output_tokens,
    })
}

/// The dedup key for a response: the message id, or `(session, preceding user
/// ts)` for an id-less line so its growing partials share one key.
fn response_key(session: &str, id: Option<&str>, user_ts: u64) -> String {
    match id {
        Some(id) => id.to_owned(),
        None => format!("{session}@{user_ts}"),
    }
}

/// The shared fold both [`process_line`] and [`transcript_speed`] drive, so the
/// span/attribution/partial rules cannot drift between them. A line whose output
/// is at least the largest seen so far updates `output` and moves `end_ms` to the
/// later of the stored end and this line's timestamp; a smaller output changes
/// nothing.
fn upsert_response(
    responses: &mut HashMap<String, Resp>,
    key: String,
    candidate: Resp,
) -> &mut Resp {
    let output = candidate.output;
    let end_ms = candidate.end_ms;
    let resp = responses.entry(key).or_insert(candidate);
    if output >= resp.output {
        resp.output = output;
        resp.end_ms = resp.end_ms.max(end_ms);
    }
    resp
}

/// RFC 3339 (with or without a `Z`/offset, milliseconds) → epoch milliseconds.
fn parse_ts(ts: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some(dt.timestamp_millis().max(0) as u64)
}

fn day_of_ms(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// The epoch ms of `day`'s UTC midnight (`YYYY-MM-DD`).
fn day_start_ms(day: &str) -> u64 {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

fn iso_ms_utc(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3f+00:00").to_string())
        .unwrap_or_default()
}

/// The oldest timestamp a cold-start read retains: yesterday's UTC midnight.
/// It is always below `now - 1 h`, so it covers every line the live/5m/1h
/// windows or today's and yesterday's day sums need.
fn cutoff_ms(now_ms: u64) -> u64 {
    now_ms - (now_ms % DAY_MS) - DAY_MS
}

/// The oldest day an owed day may be and still be rebuilt: `OWED_DAYS_BACK`
/// days before today.
fn oldest_owed_day(now_ms: u64) -> String {
    day_of_ms((now_ms - now_ms % DAY_MS).saturating_sub(OWED_DAYS_BACK * DAY_MS))
}

/// `day` is a `YYYY-MM-DD` date as [`day_of_ms`] spells it, so it orders as
/// a day and has a midnight.
fn is_day(day: &str) -> bool {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .is_ok_and(|d| d.format("%Y-%m-%d").to_string() == day)
}

fn mtime_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn path_mtime_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| mtime_of(&m))
}

/// `~/.clauth/session_profiles.json`'s mtime; the same path
/// `sessions::store_path` uses.
fn store_mtime_now() -> u64 {
    crate::profile::clauth_dir()
        .ok()
        .map(|d| d.join("session_profiles.json"))
        .and_then(|p| path_mtime_ms(&p))
        .unwrap_or(0)
}

/// One transcript file a store walk found.
struct Walked {
    mtime_ms: u64,
    len: u64,
    created: Option<SystemTime>,
}

/// Recursively collect `*.jsonl` paths with their mtime (epoch ms), length and
/// birth time.
fn walk_jsonl(dir: &Path, depth: usize, out: &mut HashMap<PathBuf, Walked>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            walk_jsonl(&path, depth - 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let walked = Walked {
                mtime_ms: mtime_of(&meta),
                len: meta.len(),
                created: meta.created().ok(),
            };
            out.insert(path, walked);
        }
    }
}

/// How one transcript ends, for an isolated run's teardown
/// ([`transcript_ends`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptEnd {
    pub(crate) file: PathBuf,
    /// The file's last spend-cap 429, stamp and `uuid`, unless a successful
    /// turn follows it in the file.
    pub(crate) refusal: Option<(u64, Option<String>)>,
    /// The file's last spend-cap 429 when a successful turn follows it in the
    /// file, stamp and `uuid`, with the stamp of the latest such turn: what a
    /// drain reading the file whole records and then clears. Only the lift's
    /// note reads it ([`crate::usage::note_isolated_credit_refusal`]).
    pub(crate) refusal_served_after: Option<(u64, Option<String>, u64)>,
    /// The stamp of the file's latest successful turn.
    pub(crate) last_success: Option<u64>,
}

/// How every transcript under `projects` ends, read whole with the producer's
/// own line parser: a spend-cap 429 and a successful turn are what the drain
/// reads them as. Unreadable files are skipped.
pub(crate) fn transcript_ends(projects: &Path) -> Vec<TranscriptEnd> {
    let mut found = HashMap::new();
    walk_jsonl(projects, WALK_DEPTH, &mut found);
    found
        .into_keys()
        .filter_map(|file| {
            let bytes = std::fs::read(&file).ok()?;
            let mut refusal = None;
            let mut refusal_served_after: Option<(u64, Option<String>, u64)> = None;
            let mut last_success: Option<u64> = None;
            for seg in bytes.split_inclusive(|&b| b == b'\n') {
                match parse_line(strip_crlf(seg)) {
                    Some(Parsed::SpendCap429 { ts, uuid }) => {
                        refusal = Some((ts, uuid));
                        refusal_served_after = None;
                    }
                    Some(Parsed::Assistant { ts, .. }) => {
                        if let Some((at_ms, uuid)) = refusal.take() {
                            refusal_served_after = Some((at_ms, uuid, ts));
                        } else if let Some((_, _, served)) = refusal_served_after.as_mut() {
                            *served = (*served).max(ts);
                        }
                        last_success = last_success.max(Some(ts));
                    }
                    _ => {}
                }
            }
            Some(TranscriptEnd {
                file,
                refusal,
                refusal_served_after,
                last_success,
            })
        })
        .collect()
}

/// The fingerprint of the bytes before a read offset: their sha256.
fn tail_hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Whether a spend-cap fact line ending at `end` passes the position bound:
/// yes when it ends past `facts_through`, the byte offset this read's bound
/// skips up to. End offsets strictly increase within one read, so the bound
/// never needs to advance. The uuid gate in `process_line` is a separate,
/// additional filter for a 429 line that carries one; a line that gate dedupes
/// still moves its recorded position when the bound skips it.
fn spend_cap_take(facts_through: u64, end: u64) -> bool {
    end > facts_through
}

/// The sums of the span-ful responses in `rs` ending on one of `days`, per
/// (day, account, model); speed peaks unset.
fn sum_days<'a>(rs: impl IntoIterator<Item = &'a Resp>, days: &BTreeSet<String>) -> DaySums {
    let ranges: Vec<(&str, u64, u64)> = days
        .iter()
        .map(|d| {
            let start = day_start_ms(d);
            (d.as_str(), start, start.saturating_add(DAY_MS))
        })
        .collect();
    let mut sums: HashMap<(&str, &str, &str), (u64, u64)> = HashMap::new();
    for r in rs {
        let Some((_, span)) = r.span() else {
            continue;
        };
        let Some(&(day, _, _)) = ranges
            .iter()
            .find(|(_, start, end)| r.end_ms >= *start && r.end_ms < *end)
        else {
            continue;
        };
        let cell = sums
            .entry((day, r.account.as_str(), r.model.as_str()))
            .or_default();
        cell.0 = cell.0.saturating_add(r.output);
        cell.1 = cell.1.saturating_add(span);
    }
    let mut out = DaySums::new();
    for ((day, account, model), (output, gen_ms)) in sums {
        out.entry(day.to_owned())
            .or_default()
            .entry(account.to_owned())
            .or_default()
            .insert(
                model.to_owned(),
                DaySum {
                    output,
                    gen_ms,
                    speed: None,
                },
            );
    }
    out
}

/// The refold rule over one day's cells, per model `add` holds: the model's
/// total across accounts, `(output, gen_ms)` summed and compared
/// lexicographically, decides. When `add`'s total is at least `into`'s, `add`'s
/// whole account split replaces `into`'s for that model; else `into`'s model
/// stays. A move between accounts keeps a model's total, so a day re-derived
/// after a swap refolds without counting the moved responses under both
/// accounts (the split may still name the wrong one, see
/// [`Timeline::member_for`]); a day that lost a transcript keeps what it
/// stored. Sums alone cannot tell a lost response from a new one, so a day
/// whose transcript was deleted after it folded and which then gains new spend
/// on that model (a clock step back of more than a day into it) keeps only the
/// larger side, the stored total or the re-derived one, never both: the other
/// side's tokens are lost. Every cell's speed peak takes the max either way, a
/// cell that kept only its peak holding no sums. Returns whether `into`
/// changed.
pub(crate) fn refold_day(
    into: &mut BTreeMap<String, BTreeMap<String, DaySum>>,
    add: &BTreeMap<String, BTreeMap<String, DaySum>>,
) -> bool {
    let total = |cells: &BTreeMap<String, BTreeMap<String, DaySum>>, model: &str| {
        cells.values().filter_map(|models| models.get(model)).fold(
            (0u64, 0u64),
            |(output, gen_ms), s| {
                (
                    output.saturating_add(s.output),
                    gen_ms.saturating_add(s.gen_ms),
                )
            },
        )
    };
    let models: BTreeSet<&String> = add.values().flat_map(BTreeMap::keys).collect();
    let mut out = into.clone();
    for model in models {
        let take_add = total(add, model) >= total(into, model);
        let accounts: BTreeSet<&String> = into
            .iter()
            .chain(add)
            .filter(|(_, models)| models.contains_key(model))
            .map(|(account, _)| account)
            .collect();
        for account in accounts {
            let stored = into.get(account).and_then(|m| m.get(model));
            let added = add.get(account).and_then(|m| m.get(model));
            let speed = match (stored.and_then(|s| s.speed), added.and_then(|s| s.speed)) {
                (Some(s), Some(a)) => Some(if peak_better(&a, &s) { a } else { s }),
                (s, a) => s.or(a),
            };
            let sums = if take_add { added } else { stored };
            let cell = match sums {
                Some(s) => Some(DaySum { speed, ..*s }),
                None => speed.map(|p| DaySum {
                    speed: Some(p),
                    ..DaySum::default()
                }),
            };
            match cell {
                Some(cell) => {
                    out.entry(account.clone())
                        .or_default()
                        .insert(model.clone(), cell);
                }
                None => {
                    if let Some(models) = out.get_mut(account) {
                        models.remove(model);
                        if models.is_empty() {
                            out.remove(account);
                        }
                    }
                }
            }
        }
    }
    let changed = out != *into;
    *into = out;
    changed
}

/// Strip a trailing `\r\n` or `\n` off a raw line; non-UTF-8 reads as empty.
fn strip_crlf(seg: &[u8]) -> &str {
    let mut s = seg;
    if s.last() == Some(&b'\n') {
        s = &s[..s.len() - 1];
    }
    if s.last() == Some(&b'\r') {
        s = &s[..s.len() - 1];
    }
    std::str::from_utf8(s).unwrap_or("")
}

/// The session a transcript belongs to: a subagent file's parent dir, else the
/// file stem, a rescue's `.rescued-<n>` sibling stripped back to its source's
/// stem (the same conversation, moved). Mirrors the path → session
/// attribution of the stores.
fn session_key(path: &Path) -> Option<String> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if let Some(pos) = comps.iter().rposition(|c| *c == "subagents")
        && pos >= 1
    {
        return Some(comps[pos - 1].to_owned());
    }
    let name = path.file_name()?.to_str()?;
    let source = crate::sessions::rescued_source_name(name);
    Path::new(source.as_deref().unwrap_or(name))
        .file_stem()?
        .to_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The profile and runtime sid an isolated store's path names, `None` for the
/// global store or a path outside `~/.clauth/profiles`.
fn isolated_sid(path: &Path) -> Option<(String, String)> {
    let profiles_root = crate::profile::clauth_dir().ok()?.join("profiles");
    let rel = path.strip_prefix(&profiles_root).ok()?;
    let mut comps = rel.components();
    let name = comps.next()?.as_os_str().to_str()?.to_owned();
    let runtime = comps.next()?.as_os_str().to_str()?;
    let sid = crate::runtime::sid_of_runtime_dir_name(runtime)?;
    Some((sid, name))
}

/// Windowed figures over a set of responses: speed = output ÷ generation
/// seconds of the responses whose span touches the window (absent when none);
/// rate = output generated inside the window ÷ its length.
fn figures_for(rs: &[&Resp], now_ms: u64) -> Figures {
    Figures {
        live: window_figure(rs, now_ms, LIVE_WINDOW_MS),
        m5: window_figure(rs, now_ms, M5_WINDOW_MS),
        h1: window_figure(rs, now_ms, H1_WINDOW_MS),
    }
}

fn window_figure(rs: &[&Resp], now_ms: u64, len_ms: u64) -> Figure {
    let win_start = now_ms.saturating_sub(len_ms);
    let mut out = 0u64;
    let mut gen_ms = 0u64;
    let mut tok_ms = 0.0f64;
    for r in rs {
        match r.span() {
            Some((start, span)) => {
                if r.end_ms < win_start || start > now_ms {
                    continue;
                }
                out += r.output;
                gen_ms += span;
                let ov_start = start.max(win_start);
                let ov_end = r.end_ms.min(now_ms);
                if ov_end > ov_start {
                    tok_ms += r.output as f64 * (ov_end - ov_start) as f64 / span as f64;
                }
            }
            None => {
                // No span: its tokens land as a point at `end_ms` and count
                // whole when that instant is inside the window.
                if r.end_ms >= win_start && r.end_ms <= now_ms {
                    tok_ms += r.output as f64;
                }
            }
        }
    }
    Figure {
        speed: (gen_ms > 0).then(|| out as f64 / (gen_ms as f64 / 1000.0)),
        rate: tok_ms / (len_ms as f64 / 1000.0),
    }
}

// ── peaks ─────────────────────────────────────────────────────────────────────

/// Keep-larger over one sum: the larger state wins, comparing `(output,
/// gen_ms)` lexicographically (a response's later state never has a shorter
/// span), and the speed peak takes the max. Two states of one day never add,
/// so merging a day again is a no-op or a growth. Returns whether `into`
/// changed.
pub(crate) fn keep_larger_cell(into: &mut DaySum, add: &DaySum) -> bool {
    let mut changed = false;
    if (add.output, add.gen_ms) > (into.output, into.gen_ms) {
        into.output = add.output;
        into.gen_ms = add.gen_ms;
        changed = true;
    }
    if let Some(peak) = add.speed
        && into.speed.is_none_or(|s| peak_better(&peak, &s))
    {
        changed |= into.speed != Some(peak);
        into.speed = Some(peak);
    }
    changed
}

/// Peak ordering, shared by speed and rate alike: a higher value wins; a tie
/// goes to the earlier instant, so a span-less rate winner's `at` is its own
/// instant (never a window end that has not happened yet) and a speed tie
/// names the response that ended first.
pub(crate) fn peak_better(a: &PeakAt, b: &PeakAt) -> bool {
    a.value > b.value || (a.value == b.value && a.at < b.at)
}

fn spell_speed(peak: &PeakAt, account: Option<&str>, model: &str) -> SpeedPeak {
    SpeedPeak {
        value: peak.value,
        at: iso_ms_utc(peak.at),
        account: account.map(str::to_owned),
        model: model.to_owned(),
    }
}

/// An account name spelled for the feed: `*` (unattributed) omits the name.
fn spell_account(account: &str) -> Option<&str> {
    (account != UNATTRIBUTED).then_some(account)
}

fn spell_rate(peak: &PeakAt) -> RatePeak {
    RatePeak {
        value: peak.value,
        at: iso_ms_utc(peak.at),
    }
}

/// One day's peaks across groupings, read off the day's cells and rate peaks.
/// `speed`/`rate` are the total's; `*` never enters `accounts`.
fn build_peaks_half(
    day: &str,
    day_sets: &[&DaySums],
    rate_sets: &[&BTreeMap<String, DayRatePeaks>],
) -> PeaksHalf {
    let mut half = PeaksHalf::default();
    let mut best_total: Option<(PeakAt, &str, &str)> = None;
    let mut by_account: BTreeMap<&str, (PeakAt, &str)> = BTreeMap::new();
    let mut by_model: BTreeMap<&str, (PeakAt, &str)> = BTreeMap::new();
    for days in day_sets {
        let Some(accounts) = days.get(day) else {
            continue;
        };
        for (account, models) in accounts {
            for (model, sum) in models {
                let Some(peak) = &sum.speed else {
                    continue;
                };
                if best_total.is_none_or(|(b, _, _)| peak_better(peak, &b)) {
                    best_total = Some((*peak, account.as_str(), model.as_str()));
                }
                if account.as_str() != UNATTRIBUTED {
                    let slot = by_account
                        .entry(account.as_str())
                        .or_insert((*peak, model.as_str()));
                    if peak_better(peak, &slot.0) {
                        *slot = (*peak, model.as_str());
                    }
                }
                let slot = by_model
                    .entry(model.as_str())
                    .or_insert((*peak, account.as_str()));
                if peak_better(peak, &slot.0) {
                    *slot = (*peak, account.as_str());
                }
            }
        }
    }
    if let Some((p, a, m)) = best_total {
        half.speed = Some(spell_speed(&p, spell_account(a), m));
    }
    for (account, (p, m)) in by_account {
        half.accounts.entry(account.to_owned()).or_default().speed =
            Some(spell_speed(&p, Some(account), m));
    }
    for (model, (p, a)) in by_model {
        half.models.entry(model.to_owned()).or_default().speed =
            Some(spell_speed(&p, spell_account(a), model));
    }
    // Rate: each grouping's max across the sets (the producer's and the
    // ledger's copy of a reopened day), as speed takes it above.
    let rate = rate_sets
        .iter()
        .filter_map(|set| set.get(day))
        .cloned()
        .fold(DayRatePeaks::default(), merge_rate_peaks);
    if let Some(t) = &rate.total {
        half.rate = Some(spell_rate(t));
    }
    for (account, p) in &rate.accounts {
        half.accounts.entry(account.clone()).or_default().rate = Some(spell_rate(p));
    }
    for (model, p) in &rate.models {
        half.models.entry(model.clone()).or_default().rate = Some(spell_rate(p));
    }
    half
}

/// One grouping's all-time best, unspelled so two halves combine by
/// [`peak_better`]: the fastest cell with the (account, model) it sits in,
/// and the busiest window.
#[derive(Debug, Clone, Default)]
struct GroupBest {
    speed: Option<(PeakAt, String, String)>,
    rate: Option<PeakAt>,
}

impl GroupBest {
    /// Take `peak` when it beats the current best: an equal one keeps the one
    /// offered first.
    fn offer_speed(&mut self, peak: &PeakAt, account: &str, model: &str) {
        if self
            .speed
            .as_ref()
            .is_none_or(|(b, _, _)| peak_better(peak, b))
        {
            self.speed = Some((*peak, account.to_owned(), model.to_owned()));
        }
    }

    fn offer_rate(&mut self, peak: &PeakAt) {
        if self.rate.is_none_or(|b| peak_better(peak, &b)) {
            self.rate = Some(*peak);
        }
    }

    fn spell(&self) -> GroupPeaks {
        GroupPeaks {
            speed: self
                .speed
                .as_ref()
                .map(|(p, a, m)| spell_speed(p, spell_account(a), m)),
            rate: self.rate.as_ref().map(spell_rate),
        }
    }
}

/// The all-time peaks over some days, per grouping (the total, each account
/// but `*`, each model), before spelling.
#[derive(Debug, Clone, Default)]
struct EverBest {
    total: GroupBest,
    accounts: BTreeMap<String, GroupBest>,
    models: BTreeMap<String, GroupBest>,
}

impl EverBest {
    /// One walk over every day in `day_sets` / `rate_sets`, in their order:
    /// on an equal (value, at) the first offered stays.
    fn over(day_sets: &[&DaySums], rate_sets: &[&BTreeMap<String, DayRatePeaks>]) -> Self {
        let mut best = Self::default();
        for days in day_sets {
            for accounts in days.values() {
                for (account, models) in accounts {
                    for (model, sum) in models {
                        let Some(peak) = &sum.speed else {
                            continue;
                        };
                        best.total.offer_speed(peak, account, model);
                        if account.as_str() != UNATTRIBUTED {
                            best.accounts
                                .entry(account.clone())
                                .or_default()
                                .offer_speed(peak, account, model);
                        }
                        best.models
                            .entry(model.clone())
                            .or_default()
                            .offer_speed(peak, account, model);
                    }
                }
            }
        }
        for set in rate_sets {
            for day in set.values() {
                if let Some(p) = &day.total {
                    best.total.offer_rate(p);
                }
                for (account, p) in &day.accounts {
                    if account.as_str() != UNATTRIBUTED {
                        best.accounts
                            .entry(account.clone())
                            .or_default()
                            .offer_rate(p);
                    }
                }
                for (model, p) in &day.models {
                    best.models.entry(model.clone()).or_default().offer_rate(p);
                }
            }
        }
        best
    }

    /// Combine with the best over days walked after these: `later`'s peak
    /// wins a grouping only when it beats this one, so an equal one keeps
    /// this one, as one walk over both in order would.
    fn absorb_later(&mut self, later: &EverBest) {
        fn absorb(into: &mut GroupBest, later: &GroupBest) {
            if let Some((p, a, m)) = &later.speed {
                into.offer_speed(p, a, m);
            }
            if let Some(p) = &later.rate {
                into.offer_rate(p);
            }
        }
        absorb(&mut self.total, &later.total);
        for (account, g) in &later.accounts {
            absorb(self.accounts.entry(account.clone()).or_default(), g);
        }
        for (model, g) in &later.models {
            absorb(self.models.entry(model.clone()).or_default(), g);
        }
    }

    fn spell(&self) -> PeaksHalf {
        let total = self.total.spell();
        PeaksHalf {
            speed: total.speed,
            rate: total.rate,
            accounts: self
                .accounts
                .iter()
                .map(|(k, g)| (k.clone(), g.spell()))
                .collect(),
            models: self
                .models
                .iter()
                .map(|(k, g)| (k.clone(), g.spell()))
                .collect(),
        }
    }
}

/// The fastest qualifying response per (day, account, model) over `rs`, for the
/// days in `days` only.
fn speed_peaks_over(rs: &[&Resp], days: &BTreeSet<String>) -> SpeedPeaks {
    let mut fresh: SpeedPeaks = BTreeMap::new();
    for r in rs {
        let Some((_, span)) = r.span() else {
            continue;
        };
        if r.output < SPEED_FLOOR {
            continue;
        }
        let day = day_of_ms(r.end_ms);
        if !days.contains(&day) {
            continue;
        }
        let peak = PeakAt {
            value: r.output as f64 / (span as f64 / 1000.0),
            at: r.end_ms,
        };
        let slot = fresh
            .entry(day)
            .or_default()
            .entry(r.account.clone())
            .or_default()
            .entry(r.model.clone())
            .or_default();
        if peak_better(&peak, slot) {
            *slot = peak;
        }
    }
    fresh
}

/// Element-wise max of two cell speed-peak maps.
fn merge_speed_peaks(a: &SpeedPeaks, b: &SpeedPeaks) -> SpeedPeaks {
    let mut out = a.clone();
    for (day, accounts) in b {
        for (account, models) in accounts {
            for (model, peak) in models {
                let slot = out
                    .entry(day.clone())
                    .or_default()
                    .entry(account.clone())
                    .or_default()
                    .entry(model.clone())
                    .or_default();
                if peak_better(peak, slot) {
                    *slot = *peak;
                }
            }
        }
    }
    out
}

/// Element-wise max of two per-day rate-peak maps, per grouping.
pub(crate) fn merge_rate_peaks(a: DayRatePeaks, b: DayRatePeaks) -> DayRatePeaks {
    let mut out = a;
    if let Some(p) = b.total {
        out.total = Some(match out.total {
            Some(t) if !peak_better(&p, &t) => t,
            _ => p,
        });
    }
    for (account, p) in b.accounts {
        out.accounts
            .entry(account)
            .and_modify(|t| {
                if peak_better(&p, t) {
                    *t = p;
                }
            })
            .or_insert(p);
    }
    for (model, p) in b.models {
        out.models
            .entry(model)
            .and_modify(|t| {
                if peak_better(&p, t) {
                    *t = p;
                }
            })
            .or_insert(p);
    }
    out
}

/// True when a rate-peak map holds no peak at all.
fn is_empty_rate_peaks(d: &DayRatePeaks) -> bool {
    d.total.is_none() && d.accounts.is_empty() && d.models.is_empty()
}

/// Responses partitioned into the total and the per-account / per-model groups.
type GroupedResponses<'a> = (
    Vec<&'a Resp>,
    BTreeMap<&'a str, Vec<&'a Resp>>,
    BTreeMap<&'a str, Vec<&'a Resp>>,
);

/// Partition responses into the total and the per-account / per-model groups.
fn group_responses<'a>(rs: &[&'a Resp]) -> GroupedResponses<'a> {
    let mut total = Vec::new();
    let mut by_account: BTreeMap<&str, Vec<&Resp>> = BTreeMap::new();
    let mut by_model: BTreeMap<&str, Vec<&Resp>> = BTreeMap::new();
    for r in rs {
        total.push(*r);
        if r.account != UNATTRIBUTED {
            by_account.entry(r.account.as_str()).or_default().push(*r);
        }
        by_model.entry(r.model.as_str()).or_default().push(*r);
    }
    (total, by_account, by_model)
}

/// One day's rate peaks per grouping: total over every response, each account
/// over its own, each model over its own (rates never aggregate by max). `t_lo`
/// and `t_hi` are the inclusive window-end bounds.
fn day_rate_peaks(
    total: &[&Resp],
    by_account: &BTreeMap<&str, Vec<&Resp>>,
    by_model: &BTreeMap<&str, Vec<&Resp>>,
    t_lo: u64,
    t_hi: u64,
) -> DayRatePeaks {
    let mut out = DayRatePeaks::default();
    if let Some(p) = rate_peak_sweep(total, t_lo, t_hi) {
        out.total = Some(p);
    }
    for (account, rs) in by_account {
        if let Some(p) = rate_peak_sweep(rs, t_lo, t_hi) {
            out.accounts.insert((*account).to_owned(), p);
        }
    }
    for (model, rs) in by_model {
        if let Some(p) = rate_peak_sweep(rs, t_lo, t_hi) {
            out.models.insert((*model).to_owned(), p);
        }
    }
    out
}

/// Fixed-point scale for [`rate_peak_sweep`]: a span's density is floored to
/// `output × 2^K / span_ms` and a point mass is `output × 2^K`, so equal
/// densities compare bit-equal, adding and later subtracting the same integer
/// leaves exactly zero, and an empty window is exactly zero by construction.
///
/// Headroom: at clauth's magnitudes (output ≤ ~10^6, ≤ ~10^5 responses over
/// two days) the running mass and any window's tokens stay below
/// 10^11 × 2^40 ≈ 2^77, leaving ~50 bits of i128 headroom; even a single
/// u64::MAX-output span over 1 ms keeps its density (`2^64 × 2^40 = 2^104`)
/// and its 10 s product below 2^118.
const RATE_SCALE_BITS: u32 = 40;

/// The busiest 10 s window over `rs` whose window end lands in `[t_lo, t_hi]`
/// (inclusive). A sweep, not a scan: sort the spans' density edges and the
/// point masses once, walk the candidate instants in order keeping the
/// cumulative generated-token function F (a span adds output/span density
/// between its start and end; a span-less response adds its whole output as a
/// point mass at its end), and read each window's mass as F(t) - F(t - 10 s)
/// with a second pointer. The candidates are the response breakpoints plus the
/// range's own bounds, so a peak sitting on a day edge is found; a window
/// holding no tokens emits nothing. The response filter keeps only the
/// responses whose support touches `[t_lo - LIVE_WINDOW_MS, t_hi]`, an O(n)
/// pass, so a small range pays only a small sweep.
///
/// The winner is the first (earliest) window whose floored integer mass is the
/// grouping's maximum; its value is `window_figure` at its own instant, so a
/// plateau reads its exact rate. A rate peak names a window at the max; which
/// of several equal windows it names is not fixed and may move on a restart
/// — here it is the earliest window at that integer max. Residual, outside
/// that tie rule (two windows below the max by a margin are not a tie): the
/// floor can misorder two windows whose exact
/// masses differ by less than n × 9.09e-10 tok/s (one ulp of the fixed-point
/// scale at a mass near n), so the named window, and the value read at it,
/// can sit below the true max by that sub-1e-9 tok/s margin. The sweep accepts
/// it; the full form it stands in for compares exact rational masses.
fn rate_peak_sweep(rs: &[&Resp], t_lo: u64, t_hi: u64) -> Option<PeakAt> {
    if t_lo > t_hi {
        return None;
    }
    let lo = t_lo.saturating_sub(LIVE_WINDOW_MS);
    // Only responses whose support can touch a window ending in the range take
    // part; a filtered-out response's breakpoints all sit outside the range,
    // so the peak is unchanged.
    let rs: Vec<&Resp> = rs
        .iter()
        .copied()
        .filter(|r| match r.span() {
            Some((start, _)) => r.end_ms >= lo && start <= t_hi,
            None => r.end_ms >= lo && r.end_ms <= t_hi,
        })
        .collect();
    let mut slope: Vec<(u64, i128)> = Vec::new();
    let mut starts: Vec<(u64, i128)> = Vec::new();
    let mut ends: Vec<(u64, i128)> = Vec::new();
    for r in &rs {
        match r.span() {
            Some((start, span)) => {
                let d = ((r.output as i128) << RATE_SCALE_BITS) / span as i128;
                slope.push((start, d));
                slope.push((r.end_ms, -d));
            }
            None => {
                let o = (r.output as i128) << RATE_SCALE_BITS;
                starts.push((r.end_ms, o));
                ends.push((r.end_ms + LIVE_WINDOW_MS, o));
            }
        }
    }
    slope.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    starts.sort_unstable_by_key(|e| e.0);
    ends.sort_unstable_by_key(|e| e.0);
    let mut candidates: Vec<u64> = rate_breakpoints(&rs)
        .filter(|&t| t >= t_lo && t <= t_hi)
        .collect();
    candidates.push(t_lo);
    candidates.push(t_hi);
    candidates.sort_unstable();
    candidates.dedup();
    // (window end, integer mass), in ascending `t`.
    let mut masses: Vec<(u64, i128)> = Vec::new();
    let mut si = 0usize;
    let mut rtime = 0u64;
    let mut rval = 0i128;
    let mut rslope = 0i128;
    let mut li = 0usize;
    let mut ltime = 0u64;
    let mut lval = 0i128;
    let mut lslope = 0i128;
    let mut psi = 0usize;
    let mut pei = 0usize;
    let mut point = 0i128;
    for t in candidates {
        let tw = t.saturating_sub(LIVE_WINDOW_MS);
        while si < slope.len() && slope[si].0 <= t {
            let (et, d) = slope[si];
            rval += rslope * (et - rtime) as i128;
            rslope += d;
            rtime = et;
            si += 1;
        }
        let ft = rval + rslope * (t - rtime) as i128;
        while li < slope.len() && slope[li].0 <= tw {
            let (et, d) = slope[li];
            lval += lslope * (et - ltime) as i128;
            lslope += d;
            ltime = et;
            li += 1;
        }
        let ftw = lval + lslope * (tw - ltime) as i128;
        while psi < starts.len() && starts[psi].0 <= t {
            point += starts[psi].1;
            psi += 1;
        }
        while pei < ends.len() && ends[pei].0 < t {
            point -= ends[pei].1;
            pei += 1;
        }
        // `ft - ftw + point` is exact integer arithmetic: equal windows
        // compare bit-equal and an empty window is exactly zero — never a
        // float residue.
        let tok = ft - ftw + point;
        if tok > 0 {
            masses.push((t, tok));
        }
    }
    // A rate peak names a window at the max; which of several equal windows
    // it names is not fixed. First strict
    // integer max — the earliest window whose floored mass equals the
    // grouping's maximum, `masses` being ascending in `t`. The winner's value
    // is `window_figure` at its own instant, the peak by definition.
    let max_tok = masses.iter().map(|&(_, tok)| tok).max()?;
    let &(t, _) = masses.iter().find(|&&(_, tok)| tok == max_tok)?;
    let value = window_figure(&rs, t, LIVE_WINDOW_MS).rate;
    Some(PeakAt { value, at: t })
}

/// Every window end at which the 10 s window rate over `rs` can change.
fn rate_breakpoints(rs: &[&Resp]) -> impl Iterator<Item = u64> {
    let mut set = BTreeSet::new();
    for r in rs {
        match r.span() {
            Some((start, _)) => {
                set.insert(start);
                set.insert(start + LIVE_WINDOW_MS);
                set.insert(r.end_ms);
                set.insert(r.end_ms + LIVE_WINDOW_MS);
            }
            None => {
                set.insert(r.end_ms);
                set.insert(r.end_ms + LIVE_WINDOW_MS);
            }
        }
    }
    set.into_iter()
}

/// Test seam: a crash between a tick's owed-day list save and its attribution
/// save. While any [`halt_seam::Arm`] this thread holds lives, every
/// [`Producer::tick`] on the thread returns right after the list save, as a
/// process dying there would.
#[cfg(test)]
pub(crate) mod halt_seam {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<u32> = const { Cell::new(0) };
    }

    /// Disarms on drop, so a panicking test never leaks the halt; nested arms
    /// count, so an inner one dropping leaves the outer one armed.
    #[must_use]
    pub(crate) struct Arm(());

    pub(crate) fn arm() -> Arm {
        ARMED.with(|a| a.set(a.get() + 1));
        Arm(())
    }

    impl Drop for Arm {
        fn drop(&mut self) {
            ARMED.with(|a| a.set(a.get().saturating_sub(1)));
        }
    }

    pub(super) fn armed() -> bool {
        ARMED.with(|a| a.get() > 0)
    }
}

/// Test seam: how many times this thread walked a speed ledger's days for
/// the `ever` half, so a test can tell a tick that reused the ledger's half
/// from one that walked the ledger again.
#[cfg(test)]
pub(crate) mod ledger_walks {
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn count() {
        COUNT.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn taken() -> u64 {
        COUNT.with(Cell::get)
    }
}

#[cfg(test)]
#[path = "../tests/inline/tps.rs"]
mod tests;
