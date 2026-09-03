//! Disk-backed job store for background `delegate` calls.
//!
//! A background delegate returns a `job_id` at once and finishes on a detached
//! blocking task. The result must outlive the originating tool call AND be
//! readable by a separate process (the `mcp-await-job` PostToolUse hook), so it
//! lands on disk at `~/.clauth/jobs/<job_id>.json` rather than an in-memory
//! registry. Writes are atomic (tmp + rename) so a concurrent reader never sees
//! a torn file. No lock is taken: the path is keyed by a unique `job_id` and the
//! finalizing task is the sole writer for its own file — a leaf with no ordering
//! against the runtime/state locks.
//!
//! A BLOCKING delegate whose caller walks away also ends up here
//! (`Handoff::hand_off` promotes its record mid-run), and it writes the same
//! file through the same code — but it is NOT delivered the same way, and the
//! paragraph above does not reach it. Measured on Claude Code 2.1.233: a tool
//! call the client cancelled or timed out dispatches `PostToolUseFailure`, never
//! `PostToolUse`, so the bundled delivery hook never runs for it; and the reply carrying
//! the minted id is dropped by rmcp before it reaches the transport, so the
//! model never learns the id from the call it was minted for. So the record is
//! written to keep the spent window's result rather than to answer that caller,
//! and the id is recovered afterwards by ENUMERATION rather than by delivery:
//! `monitor` with no `job_ids` lists it, `clauth jobs` prints it, and the TUI's
//! delegates pane draws it. All three go through [`list_banded`], and so through
//! [`list`] beneath it.
//!
//! A blocking delegate that is STILL attached to its caller keeps a second
//! spelling here, `<job_id>.live.json` ([`RecordKind::Liveness`]) — the same
//! bytes, heartbeat and all, under a filename no reader can name. It exists so
//! an operator can see a run whose model-facing result is still travelling back
//! through the join. [`RecordKind`] documents why no id resolves that
//! spelling, so nothing collects the liveness file itself. It ends one of
//! three ways. Renamed to the collectable spelling when the caller walks away.
//! Deleted when the run finishes with its caller still there. Converted to a
//! tombstone when the server dies with the caller gone; the tombstone is a
//! collectable record `monitor` then answers and removes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::profile::clauth_dir;

/// Retain a `done` file this long AFTER IT FINISHES before GC reaps it: a day,
/// so a result survives a reboot and the overnight gap between sessions — the
/// slow poller the auto-delivery hook already served is the same one returning
/// the next morning. Measured from `done_at`, not from the mint: a
/// mint-anchored TTL would expire every long run's salvage envelope the instant
/// it finalizes, since the run's age is already whatever the run cost.
pub(super) const DONE_TTL_MS: u64 = 24 * 60 * 60 * 1000; // 24h
/// A `running` file SILENT this long is orphaned (its server died mid-job); reap
/// it.
///
/// Silence rather than age, because a streaming delegate has no wall clock and
/// so no maximum lifetime to sit above: a run still healthy at any age would
/// have had its file deleted under it, and answered `unknown job_id` while its
/// child kept spending the account.
///
/// The window is a day plus a 600 s grace, and the day is the point rather than
/// a deadline derivation: a record whose server died — crash, kill, reboot —
/// stays resolvable for a day, so the `session_id` it carries can still be
/// collected and resumed the next morning. Nothing a healthy run does comes
/// near it: whichever deadline a run's shape carries bounds that run's SILENCE,
/// and `resolve_deadlines` caps it at `mcp`'s `MAX_RUN_TIMEOUT_SECS` (3600 s),
/// so once a run has spawned, only a dead server keeps its record silent for
/// anything close to a day. The 600 s grace, carried over from the old
/// 3600+600 s window, covers the heartbeat throttle, the kill and the teardown
/// before `write_done` lands.
///
/// "Silent" is measured from the record's own mint (`recorded_at`), not the
/// run's birth. A blocking delegate handed off mid-flight keeps a `started_at`
/// from arbitrarily long before its file existed, so anchoring on that would
/// mint a long run already expired — and a pinned-format one, which never
/// heartbeats at all, would be reaped by every reader for the rest of its life.
///
/// What CAN sit silent that long under a server that is still alive is the
/// pre-spawn delay: `ProfileRuntime::acquire` waits out a same-profile rotation
/// or sibling session start, and the
/// reader thread that writes the beats has not spawned yet. Both background
/// shapes spend that delay silent-since-mint — a streaming run is still inside
/// the acquire with no child, while a pinned-format one can be well past it,
/// since the same wait plus its 3600 s wall already sits a long run past the
/// old window with the child spending. The day covers both where 3600+600 s
/// could not. The delay's two legs are the wait for another holder's rotation
/// lock, bounded by `runtime::ROTATION_LOCK_TIMEOUT` at tens of seconds, and this
/// acquire's OWN recursive `~/.claude` copy, which runs inside its own hold and is
/// bounded by nothing but the disk — so a wait past the day is no longer a
/// session's lifetime away, as this used to claim, but this run's own tree copy
/// taking a day, at which point a live run's record does read as a corpse.
/// A blocking run's
/// [`RecordKind::Liveness`] record is minted at
/// the spawn, so the delay is outside its clock entirely and its silence is
/// bounded by the run's own guards. A handed-off run adds no third exposure:
/// its clock starts at the crossing, which is strictly after the spawn, so it
/// is bounded by whichever of the two shapes it already is.
pub(crate) const RUNNING_TTL_MS: u64 = (24 * 60 * 60 + 600) * 1000;
/// The bound on one `monitor` `job_ids` list, keeping one response from growing
/// without limit.
///
/// It no longer caps the store itself: retention is the two TTLs' job alone, so
/// the store may exceed this by whatever a day produces. The count cap that
/// used to stand here evicted by the retention anchor and was removed for it: a
/// day's jobs can exceed 256 on a busy box, and an anchor-sorted eviction drops
/// the record a crashed run left behind — the one this file must outlive its
/// server for — while shorter, newer ones survive. Never re-add a count
/// eviction that can touch a record its TTL still protects.
pub(crate) const MAX_RETAINED: usize = 256;

/// Per-process counter making two job ids minted in the same millisecond differ.
static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JobState {
    Running,
    Done,
}

/// Which spelling of a record a path names. Not serialized: it is a property of
/// the FILENAME, so a record carries no flag saying which one holds it and there
/// is no flag to forget to set.
///
/// The split is what lets a blocking delegate be visible without being
/// collectable, and it closes structurally rather than by a refusal arm.
///
/// The property, stated so it survives the next reader being added: **no
/// CALLER-SUPPLIED id can resolve a `Liveness` record's content.** Two different
/// mechanisms hold it up, and both are needed because neither covers the other:
///
/// - An **id-keyed** reader returns content only through [`read`], which joins
///   `Collectable` and nothing else. `monitor`'s collect and wait paths and
///   `mcp::await_job` all go through it, and each also filters the id through
///   [`is_safe_job_id`], which refuses the `.` a `Liveness` name needs.
///   [`liveness_exists`] names the other spelling but answers a bool rather than
///   content, and guards its own id.
/// - [`list`] DOES return `Liveness` content — the pane draws that record's
///   tail. It is safe because it takes no id at all: it enumerates the directory
///   and returns what it finds, so nothing a caller spells selects a file.
///
/// So a new reader is safe iff it returns no `Liveness` content FOR AN ID. The
/// shape to refuse is an id-keyed wrapper over `list` — `list(now).find(|j|
/// j.record.job_id == id)` reads correct, returns a blocking run's record under
/// a caller's string, and reopens exactly what this type closes. An id-keyed
/// lookup belongs on `read`.
///
/// The sweep's conversion is the one place a caller's id resolves content that
/// started as a `Liveness` record, and it does not reopen this: [`sweep`]
/// rewrites the silent run onto the COLLECTABLE spelling first, so its
/// `session_id`, `isolated`, `profile` and `tail` then live in a `Collectable`
/// record, the spelling `read` resolves by design. The `.live.json` file itself
/// stays unreachable under any caller-supplied id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// `<id>.json` — a result a `monitor` call may collect.
    Collectable,
    /// `<id>.live.json` — liveness only, for a blocking run whose caller still
    /// holds the join and takes the envelope from there.
    Liveness,
}

/// `#[serde(skip_serializing_if)]` predicate for a numeric field at its default.
fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobRecord {
    pub(crate) job_id: String,
    pub(crate) profile: String,
    pub(crate) state: JobState,
    pub(crate) started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) envelope: Option<serde_json::Value>,
    /// Which endpoint this run's requests went to, in the roster's own host
    /// spelling, as `delegate_call_endpoint` resolved it once at the call:
    /// stored rather than re-derived because a caller `env` override retargets
    /// one run without touching the profile, and no later name-keyed read can
    /// recover it. `None` on a record an older server wrote and on one whose
    /// endpoint could not be resolved at the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
    /// Which provider actually served this run, resolved once at the call the
    /// same way and at the same precedence `endpoint` is: a caller `env`
    /// override first, then the profile's stored endpoint. The label, not the
    /// endpoint: `Provider::from_base_url`'s display name on a recognised
    /// third-party origin, `generic` on any unrecognised origin, `anthropic`
    /// for Anthropic's own origin and for an account with no endpoint of its
    /// own. `None` on a record an older server wrote and on one the resolver
    /// could not answer, where the fold omits the key like `endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    /// Whether this run launched isolated (`delegate({isolated: true})`): its
    /// transcript lived in a throwaway tree that dies with the run, so a
    /// `session_id` on such a record is NOT a handle `delegate({resume})`
    /// accepts — only `rescue_teardown` lifts an isolated store, and a crash
    /// skips it. `false` on a record an older server wrote: shared is the
    /// delegate default either way, and the serde default keeps those records
    /// parseable without a migration.
    #[serde(default)]
    pub(crate) isolated: bool,
    /// The child's own session id, off the first streamed event that carried
    /// one: the resume handle a crashed run's record must outlive its server
    /// for. The stdout reader captures it long before any crash and the
    /// heartbeat writes it, so a `running` record a killed server left behind
    /// carries the exact value a `delegate({resume})` accepts. `None` before
    /// the first event names one, on a record an older server wrote (the
    /// `default`), and on a `done` record — a killed run's salvage envelope
    /// carries the handle inside the envelope instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// The wall-clock ceiling this run actually launched under, resolved once by
    /// `resolve_deadlines`. `0` is never a run about to be killed: it means this
    /// run HAS no wall clock, which is the normal streaming case, or — paired
    /// with an absent `idle_secs` — that the server which wrote the record
    /// predates these fields. `idle_secs` is what tells those two apart.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) timeout_secs: u64,
    /// The idle ceiling, `None` when the idle leg is off entirely (a
    /// caller-pinned `--output-format` leaves silence carrying no information).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle_secs: Option<u64>,
    /// Epoch ms of the most recent stdout line — the same anchor `started_at`
    /// uses, so a reader subtracts them with no error term. `0` = nothing has
    /// arrived yet. (A run-relative stamp would be anchored at the child's
    /// spawn, which trails the mint by the config load, the pre-flight and the
    /// runtime acquire.)
    ///
    /// It is NOT the retention anchor's floor — see [`recorded_at`]. Stamping
    /// this at a mint to hold a record alive would buy that with a false
    /// liveness claim: it renders as `last_output_secs_ago` and it is what
    /// `idle_kill_in_secs` counts from, so a run silent for 280 s of its 300 s
    /// idle guard would report a full 300 s of headroom moments before the
    /// supervision loop killed it.
    ///
    /// [`recorded_at`]: Self::recorded_at
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) last_output_at: u64,
    /// Epoch ms this RECORD was written, which is not always when its RUN
    /// started: a blocking delegate handed off mid-flight (`Handoff::hand_off`)
    /// keeps the run's real `started_at`, because `elapsed_secs` and the job id
    /// are derived from it, while its file has existed only since the crossing.
    ///
    /// [`retention_anchor`] needs the later of the two, or a run handed off
    /// past the window is minted already expired and the very next `monitor`
    /// reaps the record it came to read. `0` on a file written before this
    /// field existed, where `started_at` was the mint and the fallback is
    /// exact.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) recorded_at: u64,
    /// A bounded single-line tail of the delegate's assistant text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) tail: String,
    /// Epoch ms the job finalized, which is what [`DONE_TTL_MS`] retains from.
    /// `0` on a `running` record and on a `done` file an older server wrote,
    /// where [`gc`] falls back to the mint and so keeps exactly its old
    /// behaviour.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) done_at: u64,
    /// Whether this `done` record is the sweep's tombstone for a blocking run
    /// whose server died without finishing it: `state` is `Done`, `envelope` is
    /// `None`, and the handle `session_id` kept is the only thing a shared run
    /// leaves to resume from. `false` on a normal finish and on a record an
    /// older server wrote, so the default keeps those parseable.
    #[serde(default)]
    pub(crate) crashed: bool,
}

/// What one job's `running` record carries from its mint through every
/// heartbeat: identity, the spelling it lands under, and the deadlines the run
/// launched under. Grouped
/// so the reserve resolves them once and the heartbeat cannot re-derive them
/// differently — `resolve_deadlines` applies defaults, clamps and a streaming
/// fork, and a second derivation goes wrong the first time that fork changes.
#[derive(Debug, Clone)]
pub(crate) struct RunningSpec {
    pub(crate) job_id: String,
    pub(crate) profile: String,
    pub(crate) started_at: u64,
    /// When the record was minted; equal to `started_at` for a job that started
    /// out background, later for one handed off mid-run. Carried through every
    /// heartbeat, since a beat rewrites the whole record and would otherwise
    /// drop it back to the run's birth.
    pub(crate) recorded_at: u64,
    pub(crate) timeout_secs: u64,
    pub(crate) idle_secs: Option<u64>,
    /// The call's resolved endpoint, carried through every heartbeat so a
    /// hand-off and the final [`write_done`] record the same answer the mint
    /// resolved once.
    pub(crate) endpoint: Option<String>,
    /// The call's resolved serving provider, carried the same way and for the
    /// same reason: a heartbeat rewrites the whole record, and a hand-off must
    /// keep the label the mint resolved once.
    pub(crate) provider: Option<String>,
    /// Whether the run launched isolated, carried the same way and for the
    /// same reason: a heartbeat rewrites the whole record, and a hand-off
    /// must keep the answer the mint resolved once.
    pub(crate) isolated: bool,
    /// Which spelling every write of this record lands under. A background job
    /// is `Collectable` from its reserve; a blocking one is `Liveness` until its
    /// caller walks away and [`promote`] renames it.
    pub(crate) kind: RecordKind,
}

pub(crate) fn jobs_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("jobs"))
}

/// Lowercase base-36 digits for [`base36`], ordered by value so `n % 36`
/// indexes straight into it.
const B36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// The smallest base-36 spelling of `n`, `0` for zero. A `u64` needs at most 13
/// base-36 digits, so the fixed buffer never overruns.
fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut rev = [0u8; 13];
    let mut i = rev.len();
    while n > 0 {
        i -= 1;
        rev[i] = B36[(n % 36) as usize];
        n /= 36;
    }
    let mut out = String::with_capacity(rev.len() - i);
    for &b in &rev[i..] {
        out.push(char::from(b));
    }
    out
}

/// A fresh, process-unique, filesystem-safe job id: `started_at` (epoch ms) in
/// base-36, then a decimal monotonic counter. The stamp is encoded to keep the
/// id short; the counter stays decimal because a same-millisecond run count is
/// already tiny, and its only job is to differ.
pub(crate) fn new_job_id(started_at: u64) -> String {
    let n = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("d-{}-{n}", base36(started_at))
}

/// True iff `id` is safe as a single path component (no separators, no
/// traversal). Job ids reaching `monitor` / `mcp-await-job` come from
/// tool input, so this guards the path join.
pub(crate) fn is_safe_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The file one record lands in. `Collectable` is the only kind any reader of
/// caller-supplied ids ever asks for, and the `.` [`is_safe_job_id`] refuses is
/// what keeps its join off a `Liveness` file — see [`RecordKind`].
fn job_path(job_id: &str, kind: RecordKind) -> Result<PathBuf> {
    let name = match kind {
        RecordKind::Collectable => format!("{job_id}.json"),
        RecordKind::Liveness => format!("{job_id}{LIVE_SUFFIX}"),
    };
    Ok(jobs_dir()?.join(name))
}

/// Which [`RecordKind`] a store path names, off the filename tail. Shared by
/// [`list`] and [`sweep`] so the two derivations cannot drift.
fn record_kind(path: &Path) -> RecordKind {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) if name.ends_with(LIVE_SUFFIX) => RecordKind::Liveness,
        _ => RecordKind::Collectable,
    }
}

/// The filename tail marking a [`RecordKind::Liveness`] record. It still ends
/// `.json`, so [`gc`] and [`gc_running_corpses`] reach one on the same silence
/// rule as any other running record. A silent one is CONVERTED into a
/// tombstone instead of reaped (see [`sweep`]).
const LIVE_SUFFIX: &str = ".live.json";

/// Persist a record atomically (tmp + rename, so a reader sees either the old
/// file or the fully-written new one, never a torn write). Owner-only: a job
/// file carries the delegate's prompt and the account's full response, and lands
/// under `~/.clauth`, so it rides the 0o600 dir-0o700 invariant.
fn write_atomic(record: &JobRecord, kind: RecordKind) -> Result<()> {
    let bytes = serde_json::to_vec(record)?;
    crate::profile::atomic_write_600(&job_path(&record.job_id, kind)?, &bytes)?;
    Ok(())
}

/// Write the initial `running` record for a freshly-started job: the minted spec
/// with nothing observed yet, under whichever spelling `spec.kind` names.
/// `#[serde(default)]` on every later `JobRecord` field is what lets a job file
/// written by an older server still parse here.
pub(crate) fn write_running(spec: &RunningSpec) -> Result<()> {
    write_heartbeat(spec, 0, "")
}

/// Rewrite a running job's record with its freshest liveness: the epoch ms of
/// its last stdout line, and the bounded tail of what it has said.
///
/// Lock-free against [`write_done`] because the two cannot interleave: the
/// stdout reader thread is this function's only caller, and `run_delegate` joins
/// that thread on every exit path before it builds any envelope, while
/// `Handoff::finalize` — the sole `write_done` caller for a job — runs only
/// after `run_delegate` returns.
/// `run_delegate_never_returns_between_spawning_the_reader_and_joining_it`
/// is what holds the single-exit half of that up, since a `return` in between
/// would orphan a thread that then overwrites the finalized record.
///
/// A run handed off mid-flight does not widen that: the record it starts
/// heartbeating into is minted before its first beat resolves one, and the same
/// single reader thread does every beat either way.
pub(crate) fn write_heartbeat(spec: &RunningSpec, last_output_at: u64, tail: &str) -> Result<()> {
    write_heartbeat_with_session(spec, last_output_at, tail, None)
}

/// [`write_heartbeat`] plus the child's own session id, for the one beat caller
/// that has one: the streamed reader thread, which captured it off the first
/// event carrying one. The value rides the capture, so every beat after that
/// first event rewrites it back onto the record until the run ends; a beat
/// before it writes `None`. [`write_heartbeat`] is the spelling for every other
/// caller, and keeps `None`.
pub(crate) fn write_heartbeat_with_session(
    spec: &RunningSpec,
    last_output_at: u64,
    tail: &str,
    session_id: Option<&str>,
) -> Result<()> {
    write_atomic(
        &JobRecord {
            job_id: spec.job_id.clone(),
            profile: spec.profile.clone(),
            state: JobState::Running,
            started_at: spec.started_at,
            envelope: None,
            timeout_secs: spec.timeout_secs,
            idle_secs: spec.idle_secs,
            endpoint: spec.endpoint.clone(),
            provider: spec.provider.clone(),
            isolated: spec.isolated,
            session_id: session_id.map(str::to_string),
            last_output_at,
            recorded_at: spec.recorded_at,
            tail: tail.to_string(),
            done_at: 0,
            crashed: false,
        },
        spec.kind,
    )
}

/// Make `spec`'s COLLECTABLE record exist, keeping the run's id: what a blocking
/// delegate's hand-off crosses on.
///
/// A rename rather than a fresh mint, so one run keeps ONE identity across the
/// crossing — the id its own heartbeats already carry — and no reader ever sees
/// the id resolve to nothing. The write fallback covers the two ways the source
/// can be missing: the liveness write has not landed yet, or the run finished
/// and [`remove_liveness`] got there first. In the second case the caller's own
/// install hands the record straight back, so the fallback cannot strand one.
///
/// What this does NOT do is stop the liveness spelling coming back. The rename
/// is atomic; the concurrent writer is not bounded by it, and a heartbeat that
/// resolved its destination before the rename lands after it. `Handoff::finalize`
/// is what clears that, from a position where no writer is left to race — see
/// the comment there.
pub(crate) fn promote(spec: &RunningSpec) -> Result<()> {
    debug_assert_eq!(spec.kind, RecordKind::Collectable);
    let from = job_path(&spec.job_id, RecordKind::Liveness)?;
    let to = job_path(&spec.job_id, RecordKind::Collectable)?;
    if std::fs::rename(&from, &to).is_err() {
        write_running(spec)?;
    }
    Ok(())
}

/// Finalize a job: overwrite its file with the completed envelope, stamped with
/// the moment it finished — which is what [`DONE_TTL_MS`] retains from. The
/// running-only fields default away: a finished job has no deadline left to
/// count down to and no tail worth keeping beside its whole result.
pub(crate) fn write_done(
    job_id: &str,
    profile: &str,
    started_at: u64,
    endpoint: Option<String>,
    provider: Option<String>,
    isolated: bool,
    envelope: serde_json::Value,
) -> Result<()> {
    write_atomic(
        &JobRecord {
            job_id: job_id.to_string(),
            profile: profile.to_string(),
            state: JobState::Done,
            started_at,
            envelope: Some(envelope),
            endpoint,
            provider,
            isolated,
            session_id: None,
            timeout_secs: 0,
            idle_secs: None,
            last_output_at: 0,
            recorded_at: 0,
            tail: String::new(),
            done_at: crate::usage::now_ms(),
            crashed: false,
        },
        // A result is always collectable: the one run that finalizes with a
        // liveness record still open is a blocking one, and its caller already
        // took the envelope from the join, so `Handoff::finalize` deletes that
        // record rather than writing a second delivery into it.
        RecordKind::Collectable,
    )
}

/// Read a job record, or `None` if the file is absent or unparseable. Resolves
/// the collectable spelling ONLY — see [`RecordKind`].
pub(crate) fn read(job_id: &str) -> Option<JobRecord> {
    let bytes = std::fs::read(job_path(job_id, RecordKind::Collectable).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The collectable record's mtime, in epoch ms: the moment that record was
/// finalized, since a Done file's only writer is [`write_atomic`]'s rename and
/// everything after it removes the file rather than rewriting it. The cancel
/// verdict dates a kill off this rather than the record's own `done_at`, which
/// a file written by an older server may not carry.
pub(crate) fn collectable_mtime_ms(job_id: &str) -> Option<u64> {
    let path = job_path(job_id, RecordKind::Collectable).ok()?;
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

/// Delete a job file (best-effort). No delivery path calls this any more:
/// a collect evicts through [`claim`]. The remaining caller gives a reserved
/// running job's record back on abandon.
pub(crate) fn remove(job_id: &str) {
    if let Ok(path) = job_path(job_id, RecordKind::Collectable) {
        let _ = std::fs::remove_file(path);
    }
}

/// Who owns the delivery after a [`claim`] attempt.
pub(crate) enum Claim {
    /// The rename won: this record is the one delivery of the job, and its
    /// file is consumed.
    Owned(JobRecord),
    /// The stored `job_id` disagrees with the path: the record was renamed
    /// back, so the caller may render it but nothing was evicted.
    Refused(JobRecord),
    /// The source was already gone: another claimant owns the delivery, and
    /// the caller answers the hedged unknown copy whose "already collected"
    /// clause names exactly this.
    Lost,
}

/// Claim the done record under `job_id` for exactly one delivery, whichever
/// process delivers it. The rename is the whole serialization: the `monitor`
/// wait and the auto-delivery hook both poll a finished record, and a
/// read-then-remove pair would let both read `Done`, both deliver, and both
/// evict — the double delivery this exists to end.
///
/// Contract: claim only a record a read just reported `Done`. A running
/// record is rewritten by its heartbeat, and renaming one would evict a live
/// job's file from under its waiter.
///
/// A record whose stored `job_id` disagrees with the path is renamed back
/// and refused, never claimed: eviction follows the stored id, so an id the
/// caller supplied must not collect a file another id's record owns. The
/// rename-back cannot clobber anything — ids mint exactly once, and a `Done`
/// file is never rewritten. The claimed spelling is invisible to [`list`]
/// (its extension is not `json`) and a leftover from a crash is removed by
/// the startup sweep's foreign-file arm; that crash also loses the record's
/// only copy, since nothing reads the claimed spelling — the accepted cost
/// of serializing before the render.
pub(crate) fn claim(job_id: &str) -> Claim {
    let from = job_path(job_id, RecordKind::Collectable).ok();
    let Some(from) = from else {
        return Claim::Lost;
    };
    let claimed = from.with_extension("json.claim");
    // The `from.exists()` gate is the exactly-once guard: a rename that failed
    // because the source is gone lost the race, and the claimed spelling then
    // holds the WINNER's bytes, never a stale file to unlink.
    //
    // The retry past it has no known trigger. It was written for a stale claimed
    // file left by a claimant that died mid-claim, on the premise that Windows
    // renames do not replace an existing destination; a real-box measurement has
    // since shown `std::fs::rename` replacing one there exactly as on unix.
    if std::fs::rename(&from, &claimed).is_err() && from.exists() {
        let _ = std::fs::remove_file(&claimed);
        if std::fs::rename(&from, &claimed).is_err() {
            return Claim::Lost;
        }
    }
    let record = std::fs::read(&claimed)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JobRecord>(&bytes).ok());
    let Some(record) = record else {
        let _ = std::fs::remove_file(&claimed);
        return Claim::Lost;
    };
    if record.job_id != job_id {
        let _ = std::fs::rename(&claimed, &from);
        return Claim::Refused(record);
    }
    let _ = std::fs::remove_file(&claimed);
    Claim::Owned(record)
}

/// Whether a blocking run's liveness record stands under this id.
///
/// The one fact that separates "clauth never minted this" from "clauth minted it
/// and its result is going back through the blocking call that owns it" — two
/// answers a caller acts on differently, and only this file tells them apart.
/// Guarded by [`is_safe_job_id`] here rather than at the caller, because it is
/// the only reader whose question is about an id that resolved to NOTHING, where
/// a caller has already stopped expecting the id to be well-formed.
pub(crate) fn liveness_exists(job_id: &str) -> bool {
    is_safe_job_id(job_id) && job_path(job_id, RecordKind::Liveness).is_ok_and(|path| path.exists())
}

/// Delete a blocking run's liveness record (best-effort).
///
/// Its own function rather than a `kind` argument on [`remove`], because the two
/// answer different questions: `remove` evicts a result a caller has just taken
/// delivery of, this one retracts an offer of visibility for a run whose result
/// went back through the join.
pub(crate) fn remove_liveness(job_id: &str) {
    if let Ok(path) = job_path(job_id, RecordKind::Liveness) {
        let _ = std::fs::remove_file(path);
    }
}

/// Best-effort GC at server startup: drop `done` files past their TTL and
/// `running` files silent past [`RUNNING_TTL_MS`] (orphaned by a dead server),
/// and sweep stray `.tmp` from a crash mid-write. Nothing is evicted by count:
/// the store is bounded by the two TTLs alone (see [`MAX_RETAINED`]).
pub(crate) fn gc(now: u64) {
    sweep(now, Scope::Everything);
}

/// The narrower sweep a `monitor` collect runs: reaps the corpses a dead server
/// orphaned, and touches nothing else.
///
/// A reader must never destroy what it came for. The Done TTL and the `.tmp`
/// sweep buy nothing before a read and can only delete a result the caller is
/// asking for, so they stay at startup. What DOES belong here is the corpse:
/// [`RUNNING_TTL_MS`] already knows a file whose server died mid-job is dead,
/// and until now `serve()` was the only place that knowledge was ever applied,
/// so a corpse polled `running` forever. One corpse shape is CONVERTED instead
/// of reaped: a silent blocking run's liveness record becomes the sweep's
/// tombstone, which keeps the handle for a later resume (see [`sweep`]).
pub(crate) fn gc_running_corpses(now: u64) {
    sweep(now, Scope::RunningCorpses);
}

/// How much of the store one sweep is allowed to touch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Everything,
    RunningCorpses,
}

/// The stamp both retention rules read: when this record last mattered. A `done`
/// record's finish, falling back to its mint for a file written before `done_at`
/// existed; a `running` record's freshest heartbeat, falling back to its mint
/// before the first line of output arrives.
///
/// One anchor for the TTLs, because they answer the same question — which
/// records are the stale ones — and mixing stamps is what retention got wrong
/// twice: the count cap this store once carried, sorted on the mint, evicted a
/// long delegate's fresh, never-read result ahead of a short run's older one,
/// and [`RUNNING_TTL_MS`] reaped a live long run for having started a while
/// ago.
///
/// A `Running` record takes the latest of its three stamps rather than the two
/// it used to, because a hand-off separated the run's birth from the record's:
/// on `started_at` alone, a delegate handed off past the window is minted
/// already expired and the next reader sweeps it. `recorded_at` is `0` on a file
/// written before that field, where the mint WAS `started_at` and the pair
/// collapses back to the old rule exactly.
fn retention_anchor(record: &JobRecord) -> u64 {
    match record.state {
        JobState::Done => {
            if record.done_at > 0 {
                record.done_at
            } else {
                record.started_at
            }
        }
        JobState::Running => record
            .last_output_at
            .max(record.recorded_at)
            .max(record.started_at),
    }
}

/// Whether a `running` record has been SILENT past [`RUNNING_TTL_MS`] — the one
/// question [`gc_running_corpses`] reaps on. [`list`] classifies with it too, so
/// a reader drawing a corpse and the sweep destroying one cannot disagree about
/// which records are dead, and the `monitor` arms read the SAME predicate on the
/// record they captured before the sweep, so the answer they give about it is
/// the sweep's own verdict rather than a re-derivation that can drift.
pub(crate) fn running_is_silent(record: &JobRecord, now: u64) -> bool {
    now.saturating_sub(retention_anchor(record)) > RUNNING_TTL_MS
}

/// How a reader sees one record: its own state, plus the corpse verdict a
/// `running` record earns once its server has stopped writing to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobLiveness {
    Running,
    Done,
    /// `running` on disk, silent past [`RUNNING_TTL_MS`]: the server that was
    /// writing it is gone. Drawn as such rather than as live.
    Corpse,
}

/// How one stored record reads to a reader ENUMERATING the store — four
/// situations where [`JobLiveness`] carries three, because a `Running` record
/// means two different things depending on which spelling holds it and only the
/// pair answers "is anything already waiting on this".
///
/// One derivation for every surface that names a record's situation — `clauth
/// jobs`, `monitor`'s listing and the TUI's delegates pane — so none of them can
/// give one record a different name, a different band, or a different word.
/// `src/tui/render/plugin.rs` keeps only what a TERMINAL adds on top: the glyph
/// and the hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobPhase {
    /// A background job still going. Its result waits in the store until a
    /// `monitor` call collects it.
    Running,
    /// A blocking run whose caller still holds the join. Nothing can collect it:
    /// the envelope goes back through the call that started it.
    Blocking,
    /// Finished, with its envelope on disk, until someone collects it or the
    /// Done TTL reaps it.
    Done,
    /// `running` on disk and silent past [`RUNNING_TTL_MS`]: the `clauth mcp`
    /// server writing it is gone, and so is the result.
    Orphaned,
}

impl JobPhase {
    /// The one word every text surface names this phase by. Shared so a row
    /// cannot read `blocking` in one place and `attached` in another.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Blocking => "blocking",
            Self::Done => "done",
            Self::Orphaned => "orphaned",
        }
    }

    /// Whether a `monitor` call naming this record's id could collect a RESULT
    /// from it. False for a blocking run by construction (see [`RecordKind`])
    /// and for an orphan, whose result died with its server. A tombstone, an
    /// orphan whose collectable record still sits on disk, is the exception:
    /// `monitor` naming its id answers it with the crash copy and then removes
    /// it. `collectable: false` there names the absence of a result, never the
    /// absence of an answer.
    pub(crate) fn is_collectable(self) -> bool {
        matches!(self, Self::Running | Self::Done)
    }

    /// Whether something is still spending an account under this record.
    ///
    /// A DIFFERENT question from [`is_collectable`], and the pair splits the
    /// four phases two ways that do not line up: a blocking run is live and not
    /// collectable, a done one is collectable and not live. Naming both keeps a
    /// later caller from reaching for whichever predicate happens to be there.
    ///
    /// [`is_collectable`]: Self::is_collectable
    pub(crate) fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Blocking)
    }

    /// Which band a row sits in when a reader has to drop some: live first.
    ///
    /// Derived from [`is_live`] rather than matched again, so the band split is
    /// decided in exactly one place.
    ///
    /// [`is_live`]: Self::is_live
    pub(crate) fn rank(self) -> u8 {
        u8::from(!self.is_live())
    }
}

/// One record as a reader finds it: the parsed record, which spelling held it,
/// and how it reads right now.
#[derive(Debug, Clone)]
pub(crate) struct StoredJob {
    pub(crate) record: JobRecord,
    pub(crate) kind: RecordKind,
    pub(crate) liveness: JobLiveness,
    /// The stamp this listing sorted on and both retention rules read: a `done`
    /// record's finish, a `running` one's freshest sign of life, each with its
    /// own fallback for a file an older server wrote. Carried so a reader dates
    /// a row from the same stamp the store keeps it by, rather than picking a
    /// field per state and drifting from [`retention_anchor`].
    pub(crate) anchor: u64,
}

impl StoredJob {
    /// How long since this record last mattered, in seconds: the same
    /// [`retention_anchor`] the store keeps it by, so a reader dates a row from
    /// the stamp that decides how long it survives.
    pub(crate) fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.anchor) / 1000
    }

    /// Which of the four situations this record is in.
    ///
    /// The one classification in the crate: `clauth jobs`, `monitor`'s listing
    /// and the TUI's delegates pane all read a record's situation from here, so
    /// none of them can answer differently about one file.
    ///
    /// The spelling on disk is the whole difference between the two live ones:
    /// a [`RecordKind::Liveness`] file exists only while its caller holds the
    /// join, so no second field is needed to say which situation a `running`
    /// record is in.
    pub(crate) fn phase(&self) -> JobPhase {
        match self.liveness {
            // A crashed blocking run's record is `Done` on disk but carries no
            // envelope; it is the sweep's tombstone, not a result to collect.
            JobLiveness::Done if self.record.crashed => JobPhase::Orphaned,
            JobLiveness::Done => JobPhase::Done,
            JobLiveness::Corpse => JobPhase::Orphaned,
            JobLiveness::Running => match self.kind {
                RecordKind::Collectable => JobPhase::Running,
                RecordKind::Liveness => JobPhase::Blocking,
            },
        }
    }
}

/// Every record in the store, newest-mattering first.
///
/// READ-ONLY, and that is the contract rather than an implementation detail: no
/// Done TTL, no `.tmp` sweep, no corpse reap. A reader that
/// destroys what it came for is the defect this store has shipped twice, so
/// every destructive rule stays in [`gc`] / [`gc_running_corpses`] where a
/// caller asks for it by name. An unreadable file is skipped, never deleted.
///
/// Ordered on [`retention_anchor`] — the same stamp both retention rules read —
/// so the record a sweep would drop last is the one this lists first, then on
/// `job_id` DESCENDING where two records share an anchor.
///
/// The tiebreak REFINES that contract rather than changing it: it only orders
/// what the anchor left unordered. Without it a tie falls through to `read_dir`
/// order, which is arbitrary and not stable across two calls on an unchanged
/// store — a fan-out whose members land inside one millisecond enumerated
/// differently every time, so a model diffing two replies saw changes that had
/// not happened and an operator watching `clauth jobs` saw rows swap under a
/// still store.
///
/// **`job_id` is not an arbitrary string here**, which is why it is the
/// tiebreak: [`new_job_id`] mints `d-<base36 started_at>-<counter>`, so the
/// comparison is over a mint stamp followed by a per-process sequence, and
/// descending order puts the newest mint first — the same direction the anchor
/// sorts. Two bounds worth stating rather than discovering: base-36 stamps
/// compare as numbers only while they are the same width (the next width change
/// is decades out), and the counter is decimal, so `-9` sorts above `-10` within
/// one millisecond. Neither can reorder records with different anchors, and both
/// are stable — which is the property this is for.
pub(crate) fn list(now: u64) -> Vec<StoredJob> {
    let Ok(dir) = jobs_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, StoredJob)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let kind = record_kind(&path);
        let record = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<JobRecord>(&b).ok());
        let Some(record) = record else {
            continue;
        };
        let liveness = match record.state {
            JobState::Done => JobLiveness::Done,
            JobState::Running if running_is_silent(&record, now) => JobLiveness::Corpse,
            JobState::Running => JobLiveness::Running,
        };
        let anchor = retention_anchor(&record);
        found.push((
            anchor,
            StoredJob {
                record,
                kind,
                liveness,
                anchor,
            },
        ));
    }
    // Anchor descending, then id descending. `sort_by` rather than
    // `sort_by_key` so the id is compared in place instead of cloned into a key
    // for every record.
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.record.job_id.cmp(&a.1.record.job_id))
    });
    found.into_iter().map(|(_, job)| job).collect()
}

/// [`list`] banded for a READER: live rows first, each band still in `list`'s
/// own newest-mattering order.
///
/// **A retention order is not a display order**, and this is the whole reason
/// this function exists rather than a `.take()` over `list`. `retention_anchor`
/// dates a `done` record by its FINISH, so ten delegates that landed seconds ago
/// outrank one that has been running quietly for five minutes — and any reader
/// that caps its rows then drops the live one, which is the row every one of
/// these surfaces was built to show. `list`'s own order stays exactly as
/// documented; the banding happens here, where a reader asks for it.
///
/// The sort is STABLE, so the band is the only thing that moves and `list`'s
/// within-band order survives untouched.
///
/// `src/tui/render/plugin.rs` bands its own rows the same way for the same
/// reason, one layer later (it sorts already-rendered cells). Folding the two
/// onto this one is owed.
pub(crate) fn list_banded(now: u64) -> Vec<StoredJob> {
    let mut jobs = list(now);
    jobs.sort_by_key(|job| job.phase().rank());
    jobs
}

/// Every liveness figure a `running` record yields at one instant.
///
/// ONE derivation for two surfaces: `monitor`'s running payload renders it for
/// the calling model, and the TUI's delegates pane draws it for the operator, so
/// neither can answer differently about the same file. A `None` is a figure the
/// record structurally does not have, never an unknown one — the same rule the
/// payload's absent keys already render by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningLiveness {
    pub(crate) elapsed_secs: u64,
    /// `false` on a record written before these fields existed, where every
    /// figure below is absent rather than zero. A wall-less streaming run is the
    /// other zero-`timeout_secs` shape, and `idle_secs` is what tells them apart.
    pub(crate) recorded: bool,
    pub(crate) last_output_secs_ago: Option<u64>,
    pub(crate) idle_kill_in_secs: Option<u64>,
    pub(crate) wall_kill_in_secs: Option<u64>,
}

/// Derive [`RunningLiveness`] from a record at epoch-ms `now`.
///
/// Every figure is one epoch-ms subtraction; the only inaccuracy is the
/// heartbeat throttle, which can over-report silence and under-report each
/// countdown by up to one beat. The kill path reads an in-process atomic rather
/// than this file, so the two never have to agree exactly.
pub(crate) fn running_liveness(record: &JobRecord, now: u64) -> RunningLiveness {
    let elapsed_secs = now.saturating_sub(record.started_at) / 1000;
    if record.timeout_secs == 0 && record.idle_secs.is_none() {
        return RunningLiveness {
            elapsed_secs,
            recorded: false,
            last_output_secs_ago: None,
            idle_kill_in_secs: None,
            wall_kill_in_secs: None,
        };
    }
    // A run that has said nothing has been idle for its whole life, which is
    // also how the kill path counts it.
    let idle_for_secs = if record.last_output_at == 0 {
        elapsed_secs
    } else {
        now.saturating_sub(record.last_output_at) / 1000
    };
    RunningLiveness {
        elapsed_secs,
        recorded: true,
        last_output_secs_ago: (record.last_output_at > 0).then_some(idle_for_secs),
        idle_kill_in_secs: record.idle_secs.map(|i| i.saturating_sub(idle_for_secs)),
        wall_kill_in_secs: (record.timeout_secs > 0)
            .then(|| record.timeout_secs.saturating_sub(elapsed_secs)),
    }
}

fn sweep(now: u64, scope: Scope) {
    let full = scope == Scope::Everything;
    let Ok(dir) = jobs_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            if full {
                let _ = std::fs::remove_file(&path); // stray tmp / foreign file
            }
            continue;
        }
        let record = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<JobRecord>(&b).ok());
        let Some(record) = record else {
            // A file this sweep cannot read might still be a result: only the
            // startup sweep, which owns the store, discards one.
            if full {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        };
        let kind = record_kind(&path);
        let expired = match record.state {
            JobState::Done => full && now.saturating_sub(retention_anchor(&record)) > DONE_TTL_MS,
            JobState::Running => running_is_silent(&record, now),
        };
        if !expired {
            continue;
        }
        // A silent blocking run's liveness record is CONVERTED rather than
        // deleted: the caller holding the join is gone, and the run's handle is
        // the only thing it left to resume from. The collectable spelling keeps
        // being deleted, since its server dying means its result died with it.
        if record.state == JobState::Running && kind == RecordKind::Liveness {
            // The conversion writes to the COLLECTABLE spelling, a file this
            // sweep has not read. Never overwrite a record that carries an
            // envelope: a finish whose liveness leftover is the stale file here
            // must keep its result.
            if read(&record.job_id).is_some_and(|existing| existing.envelope.is_some()) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let mut crashed = record;
            crashed.state = JobState::Done;
            crashed.done_at = now;
            crashed.envelope = None;
            crashed.crashed = true;
            // Drop the source only once the tombstone landed: a failed write
            // (ENOSPC, read-only dir) leaves the liveness record as the
            // surviving carrier of the handle.
            if write_atomic(&crashed, RecordKind::Collectable).is_ok() {
                let _ = std::fs::remove_file(&path);
            }
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_jobs.rs"]
mod tests;
