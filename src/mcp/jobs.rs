//! Disk-backed job store for background `delegate` calls.
//!
//! A background delegate returns a `job_id` at once and finishes on a detached
//! blocking task. The result must outlive the originating tool call AND be
//! collectable later, so it lands on disk at `~/.clauth/jobs/<job_id>.json`
//! rather than an in-memory registry. Writes are atomic (tmp + rename) so a
//! concurrent reader never sees
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
//! delegates detail draws it. All three go through [`list_banded`], and so through
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::lock::with_state_lock;
use crate::logline::logline;
use crate::profile::clauth_dir;

/// Retain a `done` file this long AFTER IT FINISHES before GC reaps it: a day,
/// so a result survives a reboot and the overnight gap between sessions — the
/// slow poller the auto-delivery hook already served is the same one returning
/// the next morning. Measured from `done_at`, not from the mint: a
/// mint-anchored TTL would expire every long run's salvage envelope the instant
/// it finalizes, since the run's age is already whatever the run cost.
pub(crate) const DONE_TTL_MS: u64 = 24 * 60 * 60 * 1000; // 24h
/// A `running` file SILENT this long is orphaned (its server died mid-job); reap
/// it.
///
/// Silence rather than age, because a delegate has no wall clock and so no
/// maximum lifetime to sit above: a run still healthy at any age would have had
/// its file deleted under it, and answered `unknown job_id` while its child kept
/// spending the account.
///
/// Since the owner marker landed, this window is the FALLBACK a record an older
/// server wrote keeps: an owned record's corpse verdict is its marker (see
/// [`owner_is_live`]), which reaps it at the owner's death, not a day later. The
/// day plus 600 s grace still buys the pre-marker records their old behaviour —
/// a crashed old server's record stays resolvable for a day, so the `session_id`
/// it carries can still be collected and resumed the next morning. Nothing a
/// healthy run does comes near the window: a delegate is unbounded, so once a
/// run has spawned, only a dead server keeps its record silent for anything
/// close to a day. The 600 s grace covers the heartbeat throttle and the
/// teardown before `write_done` lands.
///
/// "Silent" is measured from the record's own mint (`recorded_at`), not the
/// run's birth. A blocking delegate handed off mid-flight keeps a `started_at`
/// from arbitrarily long before its file existed, so anchoring on that would
/// mint a long run already expired — and a pinned-format one, which never
/// heartbeats at all, would be reaped by every reader for the rest of its life.
///
/// What CAN sit silent that long under a server that is still alive is the
/// pre-spawn delay: `ProfileRuntime::acquire` waits out a same-profile rotation
/// or sibling session start, and the reader thread that writes the beats has not
/// spawned yet. Both background shapes spend that delay silent-since-mint. The
/// delay's two legs are the wait for another holder's rotation lock, bounded by
/// `runtime::ROTATION_LOCK_TIMEOUT` at tens of seconds, and this acquire's OWN
/// recursive `~/.claude` copy, which runs inside its own hold and is bounded by
/// nothing but the disk — so a wait past the day reads a live run's record as a
/// corpse. A blocking run's [`RecordKind::Liveness`] record is minted at the
/// spawn, so the delay is outside its clock entirely. A handed-off run adds no
/// third exposure: its clock starts at the crossing, which is strictly after
/// the spawn.
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
///   `Collectable` and nothing else. `monitor`'s collect path goes through it,
///   and filters the id through [`is_safe_job_id`], which refuses the `.` a
///   `Liveness` name needs.
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

/// [`is_zero`] for the `u32` fields.
fn is_zero_u32(v: &u32) -> bool {
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
    /// `session_id` on such a record is NOT a handle `delegate({session_id})`
    /// accepts — only `rescue_teardown` lifts an isolated store, and a crash
    /// skips it. `false` on a record an older server wrote: shared is the
    /// delegate default either way, and the serde default keeps those records
    /// parseable without a migration.
    #[serde(default)]
    pub(crate) isolated: bool,
    /// The directory the delegate's `claude` runs in, resolved once at the
    /// call: a resume's recorded workspace, else the caller's `cwd`, else the
    /// server's own. `None` on a record an older server wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    /// The account of the session whose `clauth mcp` server spawned the
    /// delegate, resolved once at the call off the server's own credentials.
    /// `None` on a record an older server wrote and where no profile owns the
    /// session's credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spawned_by: Option<String>,
    /// The Claude Code conversation the spawning server's host was in when the
    /// job was minted: the host's session record id, which a `/clear` moves,
    /// else the server's startup `CLAUDE_CODE_SESSION_ID`. The host identity a
    /// reader matches when either side's pid is unknown. A record an older
    /// server wrote carries the startup id, valid until the host's first
    /// `/clear`. `None` on a record older still and on a server launched
    /// outside Claude Code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host_session: Option<String>,
    /// The `claude` process hosting the spawning conversation (the server's
    /// parent). A process outlives its session id: `/clear` hands the
    /// conversation a new id while the same process and server live on, and
    /// closing the process orphans every delegate it spawned under either id.
    /// `0` where unknown: a record an older server wrote, or (off unix) a host
    /// with no single session record naming its id at server start.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub(crate) host_pid: u32,
    /// The child's own session id, off the first streamed event that carried
    /// one: the resume handle a crashed run's record must outlive its server
    /// for. The stdout reader captures it long before any crash and the
    /// heartbeat writes it, so a `running` record a killed server left behind
    /// carries the exact value a `delegate({session_id})` accepts. `None` before
    /// the first event names one, on a record an older server wrote (the
    /// `default`), and on a `done` record whose envelope carried none (every
    /// completion arm stamps it — the id clauth pinned at the spawn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// Dead fields on new records: a delegate has no wall clock or idle ceiling
    /// anymore, so the producer writes `0`/`None` here. Kept with serde defaults
    /// because a record an OLDER server wrote still carries a real deadline pair,
    /// and [`running_liveness`] still reads that pair back for those records.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) timeout_secs: u64,
    /// See [`Self::timeout_secs`]: written `None` on new records, read back only
    /// from records an older server wrote.
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
    /// Which `clauth mcp` server process minted this record — its liveness
    /// marker pid, held by that server for its whole life, so a later server
    /// reads "owner alive" by probing one flock instead of waiting out the
    /// silence window. `0` on a record an older server wrote (which held no
    /// marker), where [`running_is_silent`] alone classifies it.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub(crate) owner_pid: u32,
    /// Epoch ms the owning server started, stamped with [`Self::owner_pid`] so
    /// a cancel that cannot reach the owner's registry can NAME the server by
    /// age. `0` where the owner is unknown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) owner_started_at: u64,
}

/// What one job's `running` record carries from its mint through every
/// heartbeat: identity, the spelling it lands under, and the record's
/// deadline pair. Grouped so the reserve resolves them once and the heartbeat
/// cannot re-derive them differently. New records write `0`/`None` here; the
/// fields stay so a test can still mint the shape an OLDER server wrote.
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
    /// The run's directory and its spawning account, carried the same way and
    /// for the same reason (see [`JobRecord::cwd`], [`JobRecord::spawned_by`]).
    pub(crate) cwd: Option<String>,
    pub(crate) spawned_by: Option<String>,
    /// The spawning conversation and its host process, carried the same way
    /// (see [`JobRecord::host_session`], [`JobRecord::host_pid`]).
    pub(crate) host_session: Option<String>,
    pub(crate) host_pid: u32,
    /// Which spelling every write of this record lands under. A background job
    /// is `Collectable` from its reserve; a blocking one is `Liveness` until its
    /// caller walks away and [`promote`] renames it.
    pub(crate) kind: RecordKind,
    /// The minting server's liveness marker pid and start stamp, resolved once
    /// at the reserve (see [`JobRecord::owner_pid`]). Carried through every
    /// heartbeat so a beat does not drop the record back to the ownerless
    /// legacy shape.
    pub(crate) owner_pid: u32,
    /// See [`Self::owner_pid`]: the owner's start stamp, carried the same way.
    pub(crate) owner_started_at: u64,
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
/// traversal). Job ids reaching `monitor` come from
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
/// Lock-free against [`write_done`] by `Handoff`'s in-flight beat counter:
/// every caller counts itself in flight under the state lock, in the same hold
/// that resolved its destination, and `Handoff::finalize` sets `Finished`
/// under that lock and then waits for the count to drain before any of its own
/// writes. A beat that resolved its destination after `Finished` writes
/// nothing; one that resolved before it lands before the finalize's first file
/// write — so the two cannot interleave, however the reader thread outlives
/// `run_delegate` (a grandchild holding the child's stdout pipe can park it in
/// `read` past the finalize).
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
            cwd: spec.cwd.clone(),
            spawned_by: spec.spawned_by.clone(),
            host_session: spec.host_session.clone(),
            host_pid: spec.host_pid,
            session_id: session_id.map(str::to_string),
            last_output_at,
            recorded_at: spec.recorded_at,
            tail: tail.to_string(),
            done_at: 0,
            crashed: false,
            owner_pid: spec.owner_pid,
            owner_started_at: spec.owner_started_at,
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
/// count down to and no tail worth keeping beside its whole result. The run's
/// session id rides the record off the envelope's own `session_id` key — every
/// completion arm stamps it (the id clauth pinned at the spawn), so a collected
/// completion is resumable, and the listing names the handle beside the job id.
/// Every call fact comes off `spec`, the one the heartbeats wrote from, so a
/// fact the mint resolved cannot drop off the record at the finish.
pub(crate) fn write_done(spec: &RunningSpec, envelope: serde_json::Value) -> Result<()> {
    let session_id = envelope
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    write_atomic(
        &JobRecord {
            job_id: spec.job_id.clone(),
            profile: spec.profile.clone(),
            state: JobState::Done,
            started_at: spec.started_at,
            envelope: Some(envelope),
            endpoint: spec.endpoint.clone(),
            provider: spec.provider.clone(),
            isolated: spec.isolated,
            cwd: spec.cwd.clone(),
            spawned_by: spec.spawned_by.clone(),
            host_session: spec.host_session.clone(),
            host_pid: spec.host_pid,
            session_id,
            timeout_secs: 0,
            idle_secs: None,
            last_output_at: 0,
            recorded_at: 0,
            tail: String::new(),
            done_at: crate::usage::now_ms(),
            crashed: false,
            // A finished record is final: no owner liveness is read from it.
            owner_pid: 0,
            owner_started_at: 0,
        },
        // A result is always collectable: the one run that finalizes with a
        // liveness record still open is a blocking one, and its caller already
        // took the envelope from the join, so `Handoff::finalize` deletes that
        // record rather than writing a second delivery into it.
        RecordKind::Collectable,
    )
}

/// [`write_done`] from loose parts, for tests that finalize a record with no
/// running spec of their own: no cwd, spawning account or host, the shape an
/// older server's finish wrote.
#[cfg(test)]
pub(crate) fn write_done_parts(
    job_id: &str,
    profile: &str,
    started_at: u64,
    endpoint: Option<String>,
    provider: Option<String>,
    isolated: bool,
    envelope: serde_json::Value,
) -> Result<()> {
    write_done(
        &RunningSpec {
            job_id: job_id.to_string(),
            profile: profile.to_string(),
            started_at,
            recorded_at: started_at,
            timeout_secs: 0,
            idle_secs: None,
            endpoint,
            provider,
            isolated,
            cwd: None,
            spawned_by: None,
            host_session: None,
            host_pid: 0,
            kind: RecordKind::Collectable,
            owner_pid: 0,
            owner_started_at: 0,
        },
        envelope,
    )
}

/// Read a job record, or `None` if the file is absent or unparseable. Resolves
/// the collectable spelling ONLY — see [`RecordKind`].
pub(crate) fn read(job_id: &str) -> Option<JobRecord> {
    let bytes = std::fs::read(job_path(job_id, RecordKind::Collectable).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
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

/// Which delivery path claimed a record. Recorded in the delivery ledger so a
/// later `monitor` naming the id can say what happened to it — by whom, when —
/// instead of hedging that clauth may never have minted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claimant {
    /// A `monitor` collect delivered the result in its own reply.
    Monitor,
    /// The bundled `mcp-await-job` auto-delivery hook pushed the result into
    /// the conversation.
    Hook,
}

impl Claimant {
    /// The one word the ledger stores. Shared so the record and the unknown-id
    /// copy cannot disagree about who delivered what.
    fn label(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Hook => "hook",
        }
    }
}

/// The durable record a claim leaves behind: which path delivered the job and
/// when. Lives at `<id>.json.delivered`, invisible to every reader (its
/// extension is not `json`) and reaped by the startup sweep once the unknown
/// answer it feeds stops mattering ([`DONE_TTL_MS`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryLedger {
    pub(crate) job_id: String,
    /// [`Claimant::label`] of the delivering path.
    pub(crate) by: String,
    /// Epoch ms the delivery happened.
    pub(crate) at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) profile: String,
}

/// The ledger path for `job_id`. Guarded by [`is_safe_job_id`] at every caller
/// that derives one from input; [`read_delivery_ledger`] is the one a caller
/// reaches through.
fn ledger_path(job_id: &str) -> Result<PathBuf> {
    Ok(jobs_dir()?.join(format!("{job_id}.json.delivered")))
}

/// The ledger one claim left for `job_id`, or `None` when absent or
/// unparseable — an unreadable ledger is no ledger, and the hedged unknown copy
/// answers instead.
pub(crate) fn delivery_ledger(job_id: &str) -> Option<DeliveryLedger> {
    if !is_safe_job_id(job_id) {
        return None;
    }
    let bytes = std::fs::read(ledger_path(job_id).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Test seam: plant a delivery ledger the way the production claim writes one,
/// so the unknown-id copy can be pinned at a fixed instant without driving a
/// claim through the store.
#[cfg(test)]
pub(crate) fn write_delivery_ledger_for_test(job_id: &str, claimant: Claimant, at: u64) {
    write_delivery_ledger_for_test_with_by(job_id, claimant.label(), at);
}

/// [`write_delivery_ledger_for_test`] with an arbitrary `by` value — a ledger
/// written by a build this one has never heard of.
#[cfg(test)]
pub(crate) fn write_delivery_ledger_for_test_with_by(job_id: &str, by: &str, at: u64) {
    let Ok(path) = ledger_path(job_id) else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&DeliveryLedger {
        job_id: job_id.to_string(),
        by: by.to_string(),
        at,
        profile: "work".to_string(),
    }) else {
        return;
    };
    let _ = crate::profile::atomic_write_600(&path, &bytes);
}

/// Test seam: hold a server marker for an arbitrary pid — a live "foreign"
/// server this process does not own — so a cancel can be pinned against a
/// record another live server owns. The flock is what reads alive; the fake
/// pid cannot collide with a real server's marker, since the sandbox home
/// holds this store's own `mcp_live` dir.
#[cfg(test)]
pub(crate) fn hold_foreign_server_marker_for_test(pid: u32) -> std::fs::File {
    let dir = mcp_live_dir().expect("mcp_live dir");
    crate::profile::mkdir_700(&dir).expect("mkdir mcp_live");
    let file = crate::runtime::open_pid_file(&dir.join(pid.to_string())).expect("open marker");
    file.lock().expect("lock marker");
    file
}

/// Best-effort: the delivery is the point, the ledger the nicety. A failed
/// write degrades the unknown answer back to the hedge, never the delivery.
fn write_delivery_ledger(record: &JobRecord, claimant: Claimant) {
    let Ok(path) = ledger_path(&record.job_id) else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&DeliveryLedger {
        job_id: record.job_id.clone(),
        by: claimant.label().to_string(),
        at: crate::usage::now_ms(),
        profile: record.profile.clone(),
    }) else {
        return;
    };
    if let Err(e) = crate::profile::atomic_write_600(&path, &bytes) {
        logline!("clauth: delivery ledger {} failed: {e}", path.display());
    }
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
/// The winning claim leaves a delivery ledger (see [`DeliveryLedger`]) naming
/// `claimant` and the delivery instant, written BEFORE the claimed spelling is
/// consumed so a crash between cannot lose the record AND its ledger.
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
pub(crate) fn claim(job_id: &str, claimant: Claimant) -> Claim {
    let from = job_path(job_id, RecordKind::Collectable).ok();
    let Some(from) = from else {
        return Claim::Lost;
    };
    let claimed = from.with_extension("json.claim");
    // The `from.exists()` gate is the exactly-once guard: a rename that failed
    // because the source is gone lost the race, and the claimed spelling then
    // holds the WINNER's bytes, never a stale file to unlink.
    //
    // The retry past it covers one measured state: a stale claimed file
    // carrying the read-only attribute refuses the rename on Windows with os
    // error 5, and `remove_file` clears that attribute on its way past, so the
    // second rename lands. Against a peer holding the file open the remove
    // fails 32 and both renames lose, which is the `Lost` this returns anyway.
    // A Windows rename does replace an existing destination, so an ordinary
    // closed leftover is handled by the first rename and never reaches here.
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
    write_delivery_ledger(&record, claimant);
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

// ── server owner liveness ────────────────────────────────────────────────────

/// Where a live `clauth mcp` server's liveness marker stands: one flock-held
/// `<pid>` file per server process, the same discipline as the bare-session
/// markers in `runtime::live_bare`. The flock is released by the kernel on ANY
/// death — crash, kill, SIGKILL — so a record's owner liveness is readable by a
/// later server with no teardown path to run: a row whose server is gone is
/// dead AT THE NEXT READ, not after the silence window.
///
/// Deliberately a namespace of its own rather than `live_bare`'s: that dir
/// counts BARE `claude` sessions into the fleet tally, and a server running
/// under a `clauth start` session is not one of them.
fn mcp_live_dir() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("mcp_live"))
}

/// Whether THIS process holds a server marker: the fail-safe that keeps a
/// record minted by a marker-less server (its [`hold_server_marker`] failed)
/// in the ownerless legacy shape, where the silence window alone classifies it
/// — a record stamped with an owner no marker backs would read dead everywhere
/// and be reaped from under its live run.
static SERVER_MARKER_HELD: AtomicBool = AtomicBool::new(false);

/// Epoch ms THIS server started, set beside [`SERVER_MARKER_HELD`] so a record
/// and the stamp the cancel reply names by cannot disagree. `0` when no marker
/// is held.
static SERVER_STARTED_AT: AtomicU64 = AtomicU64::new(0);

/// The owner stamp the mint writes. [`SERVER_MARKER_HELD`] is the whole gate:
/// a server whose marker registration failed mints ownerless records, which is
/// the graceful half of the fail-safe above.
pub(crate) fn server_owner_pid() -> u32 {
    if SERVER_MARKER_HELD.load(Ordering::Relaxed) {
        std::process::id()
    } else {
        0
    }
}

/// The owner start stamp the mint writes, `0` without a marker (see
/// [`server_owner_pid`]).
pub(crate) fn server_started_at() -> u64 {
    SERVER_STARTED_AT.load(Ordering::Relaxed)
}

/// The held marker: the returned guard carries the flock for exactly as long
/// as the server lives, and clears the owner stamp on drop so a test process —
/// or a second server boot in one process — never mints records against a
/// marker it no longer holds.
pub(crate) struct ServerMarkerGuard {
    _file: std::fs::File,
}

impl Drop for ServerMarkerGuard {
    fn drop(&mut self) {
        SERVER_MARKER_HELD.store(false, Ordering::Relaxed);
        SERVER_STARTED_AT.store(0, Ordering::Relaxed);
    }
}

/// Stamp and hold THIS server's marker. The state lock is what separates the
/// create-then-lock from [`gc_server_markers`]'s prune, which unlinks whatever
/// it reads as unlocked — a marker pruned in that window would leave a live
/// server holding an unlinked file that nothing can count (the same discipline
/// the bare-session registration documents).
///
/// Called once per server at startup; a failure is the caller's to log and
/// step over — it costs the owner-liveness fast path, never the server.
pub(crate) fn hold_server_marker() -> Result<ServerMarkerGuard> {
    let dir = mcp_live_dir()?;
    let guard = with_state_lock(|_held| {
        crate::profile::mkdir_700(&dir)
            .map_err(|e| anyhow::anyhow!("failed to create {}: {e}", dir.display()))?;
        let path = dir.join(std::process::id().to_string());
        // Open without truncation, keyed by pid the way the bare markers are:
        // a pid the OS reused re-locks the dead server's file rather than
        // minting a second one, and the flock (not the name) is what reads
        // alive.
        let file = crate::runtime::open_pid_file(&path)
            .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", path.display()))?;
        file.try_lock()
            .map_err(|e| anyhow::anyhow!("marker {} not lockable: {e}", path.display()))?;
        SERVER_MARKER_HELD.store(true, Ordering::Relaxed);
        SERVER_STARTED_AT.store(crate::usage::now_ms(), Ordering::Relaxed);
        Ok(ServerMarkerGuard { _file: file })
    })?;
    Ok(guard)
}

/// Whether the server that minted a record under `owner_pid` is still alive.
///
/// Mirrors `runtime::is_session_alive`'s discipline exactly, because the two
/// answer one question over one flock shape: open WITHOUT `O_CREAT` (creating
/// the file would race a server that just created it but has not locked it yet
/// into a false dead reading), only a genuinely absent file reads dead, and
/// every other `open` failure — EMFILE, ESTALE, EACCES — reads ALIVE, because a
/// false dead verdict is what the corpse sweep reaps on, and a live run's
/// record destroyed by an unreadable marker is the one failure this store
/// exists to prevent.
pub(crate) fn owner_is_live(owner_pid: u32) -> bool {
    let Ok(dir) = mcp_live_dir() else {
        return true;
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(owner_pid.to_string()))
    {
        Ok(file) => file,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    // Held (or unreadable) = alive; an acquired lock = the owner is gone.
    file.try_lock().is_err()
}

/// Prune the marker files whose server is gone: every file this can lock is
/// unlocked, which means its holder died, so it is unlinked. The full startup
/// sweep only — like every destructive rule, it never rides a reader. Held
/// files are kept; the state lock brackets the whole prune against
/// [`hold_server_marker`]'s create-then-lock.
fn gc_server_markers() {
    let Ok(dir) = mcp_live_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    // The prune is best-effort like every GC arm: a failure to prune leaves a
    // dead marker file, which only costs one open on a later probe.
    let _ = with_state_lock(|_held| {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            else {
                continue;
            };
            if file.try_lock().is_ok() {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    });
}

/// Best-effort GC at server startup: drop `done` files past their TTL and
/// `running` files whose server is gone ([`running_is_corpse`]), sweep stray
/// `.tmp` from a crash mid-write, reap stale delivery ledgers, and prune the
/// owner markers no server holds any more. Nothing is evicted by count: the
/// store is bounded by the two TTLs alone (see [`MAX_RETAINED`]).
pub(crate) fn gc(now: u64) {
    sweep(now, Scope::Everything);
    gc_server_markers();
}

/// The narrower sweep a `monitor` collect runs: reaps the corpses a dead server
/// orphaned, and touches nothing else.
///
/// A reader must never destroy what it came for. The Done TTL, the `.tmp`
/// sweep and the ledger reap buy nothing before a read and can only delete a
/// result the caller is asking for, so they stay at startup. What DOES belong
/// here is the corpse: [`running_is_corpse`] already knows a record whose
/// server died mid-job is dead — the moment its owner marker drops, not just
/// after the silence window — and until now `serve()` was the only place that
/// knowledge was ever applied, so a corpse polled `running` for hours. One
/// corpse shape is CONVERTED instead of reaped: a dead blocking run's liveness
/// record becomes the sweep's tombstone, which keeps the handle for a later
/// resume (see [`sweep`]).
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

/// Whether a `running` record has been SILENT past [`RUNNING_TTL_MS`] — the
/// one question [`gc_running_corpses`] reaps on, and the only one a record an
/// older server wrote can answer: silence is its sole corpse rule.
pub(crate) fn running_is_silent(record: &JobRecord, now: u64) -> bool {
    now.saturating_sub(retention_anchor(record)) > RUNNING_TTL_MS
}

/// Whether a `done` record is past [`DONE_TTL_MS`]: the startup sweep's reap
/// rule, shared with any reader that must not offer a result that sweep is
/// about to delete.
pub(crate) fn done_is_expired(record: &JobRecord, now: u64) -> bool {
    now.saturating_sub(retention_anchor(record)) > DONE_TTL_MS
}

/// Whether the server that minted `record` is gone: its marker released, or —
/// the pid-reuse arm — this process now holds the flock under a record minted
/// by a DIFFERENT server epoch that once owned our pid. The start stamp is what
/// tells the two epochs apart: a record whose owner pid is ours but whose
/// `owner_started_at` is not our [`SERVER_STARTED_AT`] predates this server, so
/// its owner is dead by construction — two servers never share one pid while
/// alive. `false` on an ownerless record, which only the silence window can
/// judge.
pub(crate) fn owner_is_gone(record: &JobRecord) -> bool {
    if record.owner_pid == 0 {
        return false;
    }
    if !owner_is_live(record.owner_pid) {
        return true;
    }
    record.owner_pid == std::process::id()
        && SERVER_STARTED_AT.load(Ordering::Relaxed) != record.owner_started_at
}

/// Whether a `running` record is a corpse — the one question
/// [`gc_running_corpses`] reaps on. A record whose owner is gone is a corpse AT
/// THE NEXT READ, never only after the silence window: the marker flock drops
/// with the owning server however it dies, so a killed session's rows read dead
/// within one poll of its death. The silence window is the OWNERLESS rule — a
/// record an older server wrote, which nothing can attribute — so the two legs
/// never overlap: an owned record's verdict is its marker alone, and a live
/// owner's record is never reaped however silent it sits.
///
/// [`list`] classifies with it and the `monitor` arms read the SAME predicate
/// on the record they captured before the sweep, so a reader drawing a corpse
/// and the sweep destroying one cannot disagree about which records are dead —
/// and the answer the arms give is the sweep's own verdict rather than a
/// re-derivation that can drift.
pub(crate) fn running_is_corpse(record: &JobRecord, now: u64) -> bool {
    (record.owner_pid == 0 && running_is_silent(record, now)) || owner_is_gone(record)
}

/// How a reader sees one record: its own state, plus the corpse verdict a
/// `running` record earns once its server has stopped writing to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobLiveness {
    Running,
    Done,
    /// `running` on disk, but its server is gone: silent past
    /// [`RUNNING_TTL_MS`] (an ownerless record an older server wrote), or
    /// minted by a server whose liveness marker is released. Drawn as such
    /// rather than as live.
    Corpse,
}

/// How one stored record reads to a reader ENUMERATING the store — four
/// situations where [`JobLiveness`] carries three, because a `Running` record
/// means two different things depending on which spelling holds it and only the
/// pair answers "is anything already waiting on this".
///
/// One derivation for every surface that names a record's situation — `clauth
/// jobs`, `monitor`'s listing and the TUI's delegates detail — so none of them can
/// give one record a different name, a different band, or a different word.
/// `src/tui/render/services.rs` keeps only what a TERMINAL adds on top: the glyph
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
    /// `running` on disk whose server is gone — silent past
    /// [`RUNNING_TTL_MS`] (an ownerless record an older server wrote), or owned
    /// by a server whose marker is released — and so is the result.
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
    /// and the TUI's delegates detail all read a record's situation from here, so
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
            JobState::Running if running_is_corpse(&record, now) => JobLiveness::Corpse,
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
/// `src/tui/render/services.rs` renders the delegates detail straight from this
/// order — its `delegate_cells` sorts nothing — so the banding lives here alone.
pub(crate) fn list_banded(now: u64) -> Vec<StoredJob> {
    let mut jobs = list(now);
    jobs.sort_by_key(|job| job.phase().rank());
    jobs
}

/// Every liveness figure a `running` record yields at one instant.
///
/// ONE derivation for two surfaces: `monitor`'s running payload renders it for
/// the calling model, and the TUI's delegates detail draws it for the operator, so
/// neither can answer differently about the same file. A `None` is a figure the
/// record structurally does not have, never an unknown one — the same rule the
/// payload's absent keys already render by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningLiveness {
    pub(crate) elapsed_secs: u64,
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
    // A run that has said nothing has been idle for its whole life, which is
    // also how the kill path counts it.
    let idle_for_secs = if record.last_output_at == 0 {
        elapsed_secs
    } else {
        now.saturating_sub(record.last_output_at) / 1000
    };
    RunningLiveness {
        elapsed_secs,
        last_output_secs_ago: (record.last_output_at > 0).then_some(idle_for_secs),
        idle_kill_in_secs: record.idle_secs.map(|i| i.saturating_sub(idle_for_secs)),
        wall_kill_in_secs: (record.timeout_secs > 0)
            .then(|| record.timeout_secs.saturating_sub(elapsed_secs)),
    }
}

fn is_ledger_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".json.delivered"))
}

/// Drop a delivery ledger past [`DONE_TTL_MS`] from its `at` stamp — the same
/// horizon the unknown answer it feeds keeps. An unparseable ledger is garbage
/// and goes with the sweep.
fn reap_stale_ledger(path: &Path, now: u64) {
    let expired = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<DeliveryLedger>(&b).ok())
        .map(|ledger| now.saturating_sub(ledger.at) > DONE_TTL_MS)
        .unwrap_or(true);
    if expired {
        let _ = std::fs::remove_file(path);
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
                // A delivery ledger is kept while the unknown answer it feeds
                // matters and reaped with the done TTL; everything else here is
                // a stray tmp / foreign file.
                if is_ledger_path(&path) {
                    reap_stale_ledger(&path, now);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
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
            JobState::Done => full && done_is_expired(&record, now),
            JobState::Running => running_is_corpse(&record, now),
        };
        if !expired {
            continue;
        }
        // A dead blocking run's liveness record is CONVERTED rather than
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
