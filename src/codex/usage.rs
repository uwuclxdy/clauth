//! Passive codex usage (CDX-2): the rate-limit snapshot codex embeds in its
//! own session logs (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` — every
//! `token_count` event carries a `rate_limits` block: 5h/7d windows with
//! `used_percent` + `resets_at`). Zero network, zero credentials. Originally
//! the ONLY sanctioned codex usage source; since 2026-07-22 the CDX-6 poll
//! leg (`codex::poll`) also asks `wham/usage` directly — this leg remains the
//! zero-network fallback and the in-session freshness source.
//!
//! Ported from ccu's proven reader (`~/projects/devtools/ccu/src/codex.rs`),
//! including its hard-won bound shape: session files are large (multi-MB is
//! normal) and the tree spans months, so the walk is capped, files are
//! ordered by mtime, and only a fixed-size tail of the newest few is read.
//! The walk covers the WHOLE capped date range, not just the newest days —
//! codex appends a resumed session into its START-date directory, so the
//! freshest snapshot can live in a weeks-old dir (ccu's 2026-07-12 catch).
//! `.jsonl.zst` files are recognized and skipped: 0 of 1136 local session
//! files are compressed at codex-cli 0.144.4, so the dependency waits until
//! reality produces one.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso};

/// What a passive read produced. `Missing` (no sessions tree — codex never
/// ran under this home) and `NoData` are quiet states; `Error` is loud at the
/// caller's dedup discretion.
pub(crate) enum SnapshotOutcome {
    // Boxed: UsageInfo is ~440 bytes vs the unit variants
    // (clippy::large_enum_variant); one read per interval, so the indirection
    // costs nothing.
    Snapshot(Box<CodexSnapshot>),
    Missing,
    NoData,
    Error(String),
}

pub(crate) struct CodexSnapshot {
    /// Mapped into clauth's shared shape by each window's OWN duration
    /// (`window_minutes` ≤ 24h → `five_hour`, longer → `seven_day`; see
    /// [`route_windows`]) — NOT by primary/secondary position, which OpenAI
    /// re-shapes (2026-07: primary became the weekly window, 5h gone).
    /// `plan` stays `None` — the codex plan tier renders from the stored
    /// auth.json lens (`tier_label`), not from `PlanTier` (Anthropic-shaped).
    pub(crate) info: UsageInfo,
    /// The JSONL line's own timestamp (epoch ms) — the attribution gate
    /// compares it against the live auth.json mtime.
    pub(crate) snapshot_at_ms: Option<u64>,
}

/// Hard cap on date directories walked (newest-first). Within the cap EVERY
/// day dir is walked — a resumed session's file lives in its start-date dir.
const MAX_DAY_DIRS_WALKED: usize = 400;
/// At most this many files get a tail read per refresh.
const MAX_FILES_SCANNED: usize = 20;
/// Tail sizes tried per file: `token_count` events are frequent, but a single
/// tool-output line can be huge, so one escalation step is kept.
const TAIL_SIZES: [u64; 2] = [256 * 1024, 4 * 1024 * 1024];

pub(crate) fn sessions_dir() -> anyhow::Result<PathBuf> {
    Ok(super::codex_dir()?.join("sessions"))
}

/// Extract the freshest rate-limit snapshot from the shared codex home.
pub(crate) fn read_latest_snapshot() -> SnapshotOutcome {
    let Ok(sessions) = sessions_dir() else {
        return SnapshotOutcome::Error("cannot resolve home directory".to_string());
    };
    read_latest_snapshot_in(&sessions)
}

/// Core, path-injected for tests.
pub(crate) fn read_latest_snapshot_in(sessions: &Path) -> SnapshotOutcome {
    if !sessions.is_dir() {
        return SnapshotOutcome::Missing;
    }
    let (files, walk_error) = candidate_files(sessions);
    // Annotate-and-continue at every level: a walk error or one unreadable
    // file must not blank a snapshot a readable sibling still carries. The
    // error only surfaces when NOTHING usable was found.
    let mut io_error: Option<String> = walk_error;
    for cand in files.iter().filter(|c| !c.zst).take(MAX_FILES_SCANNED) {
        match last_snapshot_in_file(&cand.path) {
            Ok(Some(snapshot)) => return SnapshotOutcome::Snapshot(snapshot),
            Ok(None) => {}
            Err(e) => {
                io_error.get_or_insert(e);
            }
        }
    }
    match io_error {
        Some(e) => SnapshotOutcome::Error(e),
        None => SnapshotOutcome::NoData,
    }
}

struct Candidate {
    path: PathBuf,
    mtime: SystemTime,
    zst: bool,
}

/// Every rollout file in the capped date range, sorted mtime-descending, plus
/// the first walk error seen.
fn candidate_files(sessions: &Path) -> (Vec<Candidate>, Option<String>) {
    let mut out: Vec<Candidate> = Vec::new();
    let mut err: Option<String> = None;
    let mut days_walked = 0usize;
    'outer: for year in numeric_dirs_desc(sessions, &mut err) {
        for month in numeric_dirs_desc(&year, &mut err) {
            for day in numeric_dirs_desc(&month, &mut err) {
                days_walked += 1;
                collect_rollouts(&day, &mut out, &mut err);
                if days_walked >= MAX_DAY_DIRS_WALKED {
                    break 'outer;
                }
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.mtime));
    (out, err)
}

/// Subdirectories whose names are purely numeric (`2026`, `07`, `12`),
/// descending — numeric compare so zero-padding is irrelevant. A vanished dir
/// is empty; any other error is recorded and the walk continues.
fn numeric_dirs_desc(dir: &Path, err: &mut Option<String>) -> Vec<PathBuf> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            err.get_or_insert(format!("{}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut named: Vec<(u32, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(n) = entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            named.push((n, path));
        }
    }
    named.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    named.into_iter().map(|(_, p)| p).collect()
}

fn collect_rollouts(day: &Path, out: &mut Vec<Candidate>, err: &mut Option<String>) {
    let rd = match std::fs::read_dir(day) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            err.get_or_insert(format!("{}: {e}", day.display()));
            return;
        }
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("rollout-") {
            continue;
        }
        let zst = name.ends_with(".jsonl.zst");
        if !zst && !name.ends_with(".jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        out.push(Candidate {
            path: entry.path(),
            mtime,
            zst,
        });
    }
}

/// Read a bounded tail of one session file and return its NEWEST snapshot.
/// `Ok(None)` = file readable but no snapshot in the inspected tail.
fn last_snapshot_in_file(path: &Path) -> Result<Option<Box<CodexSnapshot>>, String> {
    use std::io::{Read, Seek, SeekFrom};
    for (i, tail) in TAIL_SIZES.iter().enumerate() {
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let len = f
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        let start = len.saturating_sub(*tail);
        f.seek(SeekFrom::Start(start))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        f.read_to_end(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&buf);
        let mut lines = text.lines();
        if start > 0 {
            // The window almost surely opened mid-line — drop the fragment.
            lines.next();
        }
        if let Some(snapshot) = snapshot_from_lines(lines) {
            return Ok(Some(snapshot));
        }
        let whole_file = start == 0;
        let last_try = i + 1 == TAIL_SIZES.len();
        if whole_file || last_try {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Newest snapshot among the given lines (scanned in reverse). Malformed lines
/// and `token_count` events without a `rate_limits` block are skipped — the
/// rollout schema drifts and one odd line must not hide an older good one.
fn snapshot_from_lines<'a>(
    lines: impl DoubleEndedIterator<Item = &'a str>,
) -> Option<Box<CodexSnapshot>> {
    for line in lines.rev() {
        // Cheap pre-filter: token_count events are rare among response items.
        if !line.contains("\"token_count\"") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<RolloutLine>(line) else {
            continue;
        };
        let Some(payload) = parsed.payload else {
            continue;
        };
        if payload.kind.as_deref() != Some("token_count") {
            continue;
        }
        let Some(rl) = payload.rate_limits else {
            continue;
        };
        // Codex now emits more than one independent limiter in the same
        // session log. `codex` is the account-wide quota shown by CCU;
        // model-specific buckets such as `codex_bengalfox` (Spark) must not
        // replace it merely because their token_count event is newer. Older
        // releases omitted limit_id, so absence remains the compatibility
        // path while an explicit non-codex id is skipped.
        if rl.limit_id.as_deref().is_some_and(|id| id != "codex") {
            continue;
        }
        let (five_hour, seven_day, verdict) =
            route_windows(rl.primary, rl.secondary, rl.rate_limit_reached_type);
        return Some(Box::new(CodexSnapshot {
            info: UsageInfo {
                five_hour,
                seven_day,
                // CDX-4 §0.16: the limiter's own verdict rides the shared
                // struct — stronger than the percent heuristic for the chain
                // scan, and the signal ccu's RATE-LIMITED badge keys on.
                codex_rate_limit_reached: verdict,
                ..UsageInfo::default()
            },
            snapshot_at_ms: parsed.timestamp.as_deref().and_then(rfc3339_to_ms),
        }));
    }
    None
}

/// Which shared-shape slot a raw limiter window belongs in.
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    /// The short/session window → `UsageInfo.five_hour` (published label "5h").
    Short,
    /// The weekly-ish window → `UsageInfo.seven_day` (published label "7d").
    Weekly,
}

/// Route the raw limiter windows into the shared `{five_hour, seven_day}` shape
/// by each window's OWN advertised duration, not its position — and remap the
/// limiter verdict to the slot its window landed in.
///
/// WHY: OpenAI re-shaped the limiter (observed 2026-07-16): `primary` is now a
/// `window_minutes: 10080` (7-day) window with `secondary` absent — the 5h limit
/// is temporarily gone. The old positional `primary → five_hour` mapping
/// published that weekly window under the "5h" label (a weekly reset rendered on
/// a "Session 5h" bar). Duration routing: ≤ 24h of minutes is the short/session
/// slot, longer is the weekly slot; a window with no `window_minutes` (very old
/// codex releases) falls back to its positional slot. If both windows classify
/// into the same slot, the second one takes the OTHER slot (positional
/// tiebreak) so no window is silently dropped.
///
/// VERDICT REMAP: `rate_limit_reached_type` names the RAW window ("primary"/
/// "secondary"). Every consumer of the published `codex_rate_limit_reached`
/// (`codex_limiter_blocked`, ccu's badge, ccsbar's strip) reads "primary" as
/// "the 5h/short window hit" and "secondary" as "the weekly window hit" — so
/// the verdict is republished as the SLOT name-equivalent: "primary" when the
/// named window routed to the short slot, "secondary" when it routed weekly.
/// Unknown verdict strings pass through (consumers degrade to either-window).
pub(crate) fn route_windows(
    primary: Option<LimiterWindow>,
    secondary: Option<LimiterWindow>,
    rate_limit_reached_type: Option<String>,
) -> (Option<UsageWindow>, Option<UsageWindow>, Option<String>) {
    fn classify(w: &LimiterWindow, positional: Slot) -> Slot {
        match w.window_minutes {
            Some(m) if m > 24 * 60 => Slot::Weekly,
            Some(_) => Slot::Short,
            None => positional,
        }
    }
    let map = |w: &LimiterWindow| UsageWindow {
        utilization: w.used_percent,
        resets_at: w.resets_at.map(epoch_secs_to_iso),
    };

    let primary_slot = primary.as_ref().map(|w| classify(w, Slot::Short));
    let mut secondary_slot = secondary.as_ref().map(|w| classify(w, Slot::Weekly));
    // Same-slot collision: the second window takes the other slot.
    if primary_slot.is_some() && secondary_slot == primary_slot {
        secondary_slot = Some(match primary_slot {
            Some(Slot::Short) => Slot::Weekly,
            _ => Slot::Short,
        });
    }

    let mut five_hour = None;
    let mut seven_day = None;
    for (raw, slot) in [(&primary, primary_slot), (&secondary, secondary_slot)] {
        let (Some(w), Some(slot)) = (raw, slot) else {
            continue;
        };
        match slot {
            Slot::Short => five_hour = Some(map(w)),
            Slot::Weekly => seven_day = Some(map(w)),
        }
    }

    let verdict = rate_limit_reached_type.filter(|s| !s.is_empty()).map(|t| {
        let named_slot = match t.as_str() {
            "primary" => primary_slot,
            "secondary" => secondary_slot,
            _ => return t, // unknown → pass through, consumers check both
        };
        match named_slot {
            Some(Slot::Weekly) => "secondary".to_string(),
            Some(Slot::Short) => "primary".to_string(),
            // Verdict names a window that isn't present — pass through.
            None => t,
        }
    });

    (five_hour, seven_day, verdict)
}

/// Lenient RFC 3339 → epoch ms via the tolerant second-level parser clauth
/// already ships (`iso_to_epoch_secs` handles `Z`, offsets, and fractions).
fn rfc3339_to_ms(s: &str) -> Option<u64> {
    let secs = crate::usage::iso_to_epoch_secs(s)?;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

/// Attribution gate (PLAN.md §0.8.2): a snapshot is attributed to the live
/// codex account only when its event timestamp is NOT OLDER than the live
/// auth.json's mtime — the account provably hasn't changed since the event
/// (every account change rewrites that file: switch install, `codex login`,
/// codex's own refresh). An event with no parseable timestamp is NOT
/// attributed — conservative staleness over misattribution.
pub(crate) fn attributable(snapshot_at_ms: Option<u64>, live_auth_mtime_ms: Option<u64>) -> bool {
    match (snapshot_at_ms, live_auth_mtime_ms) {
        (Some(event), Some(auth)) => event >= auth,
        _ => false,
    }
}

/// The live auth.json mtime in epoch ms, when the file exists.
pub(crate) fn live_auth_mtime_ms() -> Option<u64> {
    let path = super::live_auth_path().ok()?;
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(mtime.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

// ---------------------------------------------------------------------------
// Rollout line shape (lenient: every field optional, unknown fields ignored —
// the rollout schema drifts release to release)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RolloutLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct RateLimits {
    /// Limiter bucket identity. `codex` is the account-wide quota; newer
    /// clients also publish model-specific buckets (for example
    /// `codex_bengalfox`) that must not drive the shared usage display.
    #[serde(default)]
    limit_id: Option<String>,
    #[serde(default)]
    primary: Option<LimiterWindow>,
    #[serde(default)]
    secondary: Option<LimiterWindow>,
    /// Which window the limiter says is exhausted (`primary`/`secondary`),
    /// when codex recorded a limit rejection.
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct LimiterWindow {
    #[serde(default)]
    pub(crate) used_percent: f64,
    /// Unix seconds in current codex releases; absent in very old ones.
    #[serde(default)]
    pub(crate) resets_at: Option<i64>,
    /// The window's own duration — the slot-routing signal (see
    /// [`route_windows`]): OpenAI's limiter shape moves (2026-07: `primary`
    /// became the 10080-minute weekly window), so position can't name a slot.
    #[serde(default)]
    pub(crate) window_minutes: Option<i64>,
}

#[cfg(test)]
#[path = "../../tests/inline/codex_usage.rs"]
mod tests;
