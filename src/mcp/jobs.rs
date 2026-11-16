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
//! [`list_readable`], the one store walk both listings share.
//!
//! A blocking delegate that is STILL attached to its caller keeps a second
//! spelling here, `<job_id>.live.json` ([`RecordKind::Liveness`]) — the same
//! bytes, heartbeat and all, under a filename no reader can name. It exists so
//! an operator can see a run whose model-facing result is still travelling back
//! through the join. [`RecordKind`] documents why no id resolves that
//! spelling, so nothing collects the liveness file itself. It ends one of
//! four ways. Renamed to the collectable spelling when the caller walks away.
//! Settled in place when the run finishes with its caller still attached:
//! `done`, no envelope — the join was the one delivery — an `outcome` for the
//! TUI mascot's cue and a done state for `monitor`'s reason arm; the startup
//! sweep reaps it past [`DONE_TTL_MS`] like every other done record. Converted
//! to a tombstone when the server dies with the caller gone; the tombstone is
//! a collectable record `monitor` then answers and removes. Deleted when a
//! leftover stands beside a finished record: the finalize clears the spelling
//! a racing beat recreated after a hand-off, and the sweep drops one beside a
//! collectable record that already carries its envelope.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::lock::{state_lock_timeout, with_state_lock};
use crate::logline::logline;
use crate::profile::clauth_dir;

/// Retain a `done` file this long AFTER IT FINISHES before GC reaps it: a day,
/// so a result survives a reboot and the overnight gap between sessions — the
/// slow poller the auto-delivery hook already served is the same one returning
/// the next morning. Measured from `done_at`, not from the mint: a
/// mint-anchored TTL would expire every long run's salvage envelope the instant
/// it finalizes, since the run's age is already whatever the run cost.
pub(crate) const DONE_TTL_MS: u64 = 24 * 60 * 60 * 1000; // 24h

/// How often a corpse's recorded child may be probed while it lives: each
/// probe forks `ps` on macOS/BSD, so the poll's probe ledger caps the
/// aggregate at one per second. The store re-scan forks none of its own: it
/// skips every id the watch already holds, and the one classification that
/// would probe — a corpse carrying a session id whose owner is gone — is the
/// resume note's shape, which the scan leaves alone.
pub(crate) const CORPSE_CHILD_PROBE_INTERVAL_MS: u64 = 1000;
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
/// [`owner_is_live`]), read dead at the owner's death, not a day later. The
/// day plus 600 s grace still buys the pre-marker records their old behaviour —
/// a crashed old server's record stays resolvable for a day, so the `session_id`
/// it carries can still be collected and resumed the next morning — and it is
/// how long a collectable corpse tied to a session is kept until it has been
/// silent past this window (see [`sweep`]), giving the session's watcher its
/// chance to report the lost job. The keep runs from the record's silence
/// anchor, not the owner's death, so a record silent before its server died
/// is kept only for the rest of the window. Nothing a
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
    /// holds the join and takes the envelope from there. The same file settles
    /// at the run's finish: `done` with the run's `outcome` and no envelope,
    /// still unreachable by id, until the Done TTL reaps it (see
    /// [`write_done_liveness`]).
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
    /// Whether this `done` record is the sweep's tombstone for a run whose
    /// server died without finishing it: `state` is `Done`, `envelope` is
    /// `None`, and the handle `session_id` kept is the only thing a shared run
    /// leaves to resume from. `false` on a normal finish and on a record an
    /// older server wrote, so the default keeps those parseable. A stop-at-exit
    /// tombstone keeps it beside [`Self::stopped`] — the server DID die — and
    /// [`StoredJob::phase`] reads the stop first.
    #[serde(default)]
    pub(crate) crashed: bool,
    /// Whether a `done` record's run finished ok. The settled `Liveness`
    /// record of an attached run is the one shape that writes it — its
    /// envelope went back through the join, so the outcome is all the finish
    /// left to read, and `done_job_failed` reads it exactly there. `false` on
    /// a record an older server wrote and on a collectable finish, whose
    /// envelope's `is_error` is the verdict.
    #[serde(default)]
    pub(crate) outcome: bool,
    /// Whether this `done` record is a stop-at-`/exit` record: the in-process
    /// stop's finalize (its salvage envelope still rides the record, and an
    /// isolated run's transcript was rescued) or the sweep's tombstone for a
    /// run the exit killed before its server could write one (no envelope,
    /// nothing rescued; the tombstone also carries [`Self::crashed`]).
    /// `false` on a normal finish, a cancel, and a record an older server
    /// wrote, so the default keeps those parseable.
    #[serde(default)]
    pub(crate) stopped: bool,
    /// The delegate child's pid, recorded at the spawn so a later server's
    /// sweep can stop the process tree a killed server left spending. `0`
    /// before the spawn, on a record an older server wrote, and on Windows,
    /// where the stop keeps today's behaviour.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub(crate) child_pid: u32,
    /// The token [`crate::daemon::gateway::process_start_time`] returned for
    /// [`Self::child_pid`] at the spawn, so a sweep about to kill a recorded
    /// child can tell the run's own process from a pid the OS reused. `None`
    /// where the pid is unknown or the token could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) child_start_token: Option<String>,
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

// The recorded-child probes this thread has run: the test seam a poll-rate
// test reads, so it can bound how often the watcher re-probes a child-alive
// corpse. Per-thread so a concurrent test's probes never count against this
// one.
#[cfg(test)]
thread_local! {
    static RECORDED_CHILD_PROBE_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

// The readers are unix-only tests, so the accessors take their gate: a
// Windows build has no caller and `-D dead_code` reds it.
#[cfg(all(test, unix))]
pub(crate) fn reset_recorded_child_probe_count() {
    RECORDED_CHILD_PROBE_COUNT.with(|count| count.set(0));
}

#[cfg(all(test, unix))]
pub(crate) fn recorded_child_probe_count() -> u64 {
    RECORDED_CHILD_PROBE_COUNT.with(|count| count.get())
}

impl JobRecord {
    /// Whether a `stopped` record's resume handle is real: a shared run's
    /// transcript is already in the global store, an isolated one's only when
    /// the stop's own rescue moved it there (the envelope's
    /// `transcript_rescued` verdict, written either way). The ONE resume
    /// verdict — the note, the listing and the collect copy all read it, so
    /// no surface offers a resume the record cannot honour.
    pub(crate) fn stop_is_resumable(&self) -> bool {
        !self.isolated
            || self
                .envelope
                .as_ref()
                .and_then(|e| e.get("transcript_rescued"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
    }

    /// Whether the child pid this record stamped at the spawn still names
    /// that child: the pid is non-zero and the process's start token still
    /// matches the recorded one, which is what tells the run's own process
    /// from a pid the OS reused. `false` on Windows, which records no child.
    pub(crate) fn recorded_child_alive(&self) -> bool {
        #[cfg(test)]
        RECORDED_CHILD_PROBE_COUNT.with(|count| count.set(count.get() + 1));
        self.child_pid != 0
            && self.child_start_token.as_deref().is_some_and(|token| {
                crate::daemon::gateway::process_start_time(self.child_pid).as_deref() == Some(token)
            })
    }
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

/// A fresh, filesystem-safe job id: `started_at` (epoch ms) in base-36, the
/// minting server's decimal pid, then a decimal monotonic counter. The stamp is
/// encoded to keep the id short; the pid keeps the id unique across `clauth mcp`
/// processes on one box, since the counter restarts at 0 in each. The counter
/// stays decimal because a same-millisecond run count is already tiny, and its
/// only job is to differ within one process.
pub(crate) fn new_job_id(started_at: u64) -> String {
    let n = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("d-{}-{}-{n}", base36(started_at), std::process::id())
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
    write_heartbeat_with_spawn_facts(spec, last_output_at, tail, session_id, 0, None)
}

/// [`write_heartbeat_with_session`] plus the spawn facts: the child's pid and
/// its start token, recorded at the spawn so a later server's sweep can stop
/// the tree a killed server left behind, and carried forward by every beat so
/// the record keeps them however it is rewritten.
pub(crate) fn write_heartbeat_with_spawn_facts(
    spec: &RunningSpec,
    last_output_at: u64,
    tail: &str,
    session_id: Option<&str>,
    child_pid: u32,
    child_start_token: Option<&str>,
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
            outcome: false,
            stopped: false,
            owner_pid: spec.owner_pid,
            owner_started_at: spec.owner_started_at,
            child_pid,
            child_start_token: child_start_token.map(str::to_string),
        },
        spec.kind,
    )
}

/// Make `spec`'s COLLECTABLE record exist, keeping the run's id: what a blocking
/// delegate's hand-off crosses on.
///
/// A rename rather than a fresh mint, so one run keeps ONE identity across the
/// crossing — the id its own heartbeats already carry — and no reader ever sees
/// the id resolve to nothing. The write fallback covers the one way the source
/// can be missing: the spawn's own liveness write has not landed yet. A run
/// that already finished is `Finished`, so `hand_off` never reaches the
/// rename — its settled record stays put.
/// A collectable record already on disk (a racing hand-off promoted first) is
/// kept: a fresh mint would drop the session id and child facts it carries,
/// leaving the sweep a record it reaps instead of converts.
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
    if std::fs::rename(&from, &to).is_err() && !to.exists() {
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
    // The one completion envelope a host exit produces carries the marker; the
    // record's own flag is what every reader classifies on.
    let stopped = envelope.get("stopped").and_then(serde_json::Value::as_str) == Some("host_exit");
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
            outcome: false,
            stopped,
            // A finished record is final: no owner liveness is read from it,
            // and its child is gone.
            owner_pid: 0,
            owner_started_at: 0,
            child_pid: 0,
            child_start_token: None,
        },
        // A result is always collectable: the one run that finalizes with a
        // liveness record still open is a blocking one, and its caller already
        // took the envelope from the join, so `Handoff::finalize` settles that
        // record ([`write_done_liveness`]: done + outcome, no envelope) rather
        // than writing a second delivery into it — except for a stop at the
        // host's exit, which has no caller left and lands here.
        RecordKind::Collectable,
    )
}

/// Settle a blocking run's liveness record at its finish: the same spelling,
/// `done` state, no envelope — the caller's join was the one delivery — and
/// the run's outcome off the envelope's error flag, which the mascot's cue
/// reads; `monitor`'s reason arm reads the done state alone. The `done_at`
/// stamp is what the startup sweep reaps by [`DONE_TTL_MS`], exactly like
/// every other done record, so no new sweep arm exists for it.
/// The record stays under the `Liveness` spelling, so no caller-supplied id
/// can resolve it (see [`RecordKind`]) and the result is never delivered
/// twice. Mirrors [`write_done`]'s field choices: the run's session id rides
/// the record off the envelope's own `session_id` key, the running-only
/// fields default away and the owner marker zeroes, since no owner liveness
/// is read from a finished record.
pub(crate) fn write_done_liveness(spec: &RunningSpec, envelope: &serde_json::Value) -> Result<()> {
    write_atomic(
        &JobRecord {
            job_id: spec.job_id.clone(),
            profile: spec.profile.clone(),
            state: JobState::Done,
            started_at: spec.started_at,
            envelope: None,
            endpoint: spec.endpoint.clone(),
            provider: spec.provider.clone(),
            isolated: spec.isolated,
            cwd: spec.cwd.clone(),
            spawned_by: spec.spawned_by.clone(),
            host_session: spec.host_session.clone(),
            host_pid: spec.host_pid,
            session_id: envelope
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            timeout_secs: 0,
            idle_secs: None,
            last_output_at: 0,
            recorded_at: 0,
            tail: String::new(),
            done_at: crate::usage::now_ms(),
            crashed: false,
            outcome: !crate::mcp::envelope_reports_failure(envelope),
            stopped: false,
            owner_pid: 0,
            owner_started_at: 0,
            child_pid: 0,
            child_start_token: None,
        },
        RecordKind::Liveness,
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

/// Read a blocking run's liveness record, or `None` if the file is absent or
/// unparseable. Guarded by [`is_safe_job_id`] the way [`liveness_exists`] is:
/// it answers about an id that resolved to NOTHING, where the caller has
/// already stopped expecting the id to be well-formed.
pub(crate) fn read_liveness(job_id: &str) -> Option<JobRecord> {
    if !is_safe_job_id(job_id) {
        return None;
    }
    let bytes = std::fs::read(job_path(job_id, RecordKind::Liveness).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Delete a job file (best-effort). A delivered RESULT still evicts through
/// [`claim`], never here: the claim's rename is what hands one delivery to
/// exactly one claimant. The two callers left delete a record no envelope
/// has to serialize — a monitor collect consuming a kept corpse, and an
/// abandon giving a reserved running job's record back.
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
/// when. Lives at `<id>.json.delivered`, outside the store listing (its
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
    /// Whether the delivered job had failed, judged at the claim by the same
    /// rule every stored-record delivery folds. `#[serde(default)]`, so a
    /// ledger an older build wrote reads as a successful finish.
    #[serde(default)]
    pub(crate) failed: bool,
    /// Whether the delivered record was a stop, judged at the claim by the
    /// one phase classification: a stop cues no finish, whichever spelling a
    /// reader first sees its id as. `#[serde(default)]`, so a ledger an older
    /// build wrote reads as no stop.
    #[serde(default)]
    pub(crate) stopped: bool,
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
        failed: false,
        stopped: false,
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
    let at = crate::usage::now_ms();
    let Ok(bytes) = serde_json::to_vec(&DeliveryLedger {
        job_id: record.job_id.clone(),
        by: claimant.label().to_string(),
        at,
        profile: record.profile.clone(),
        failed: crate::mcp::done_job_failed(record),
        stopped: phase_of(record, RecordKind::Collectable, at) == JobPhase::Stopped,
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

/// Whether a blocking run's liveness record reads `done`: the run finished
/// with its caller still attached, so the record settled
/// ([`write_done_liveness`]) and the result went back through the join. Still
/// a bool, never the record's result for an id — the same [`RecordKind`]
/// contract [`liveness_exists`] holds; `state` alone is read, to tell the
/// live shapes apart. `false` on an absent or unreadable record, which the
/// caller's live-shape answer still covers.
pub(crate) fn liveness_is_done(job_id: &str) -> bool {
    is_safe_job_id(job_id)
        && job_path(job_id, RecordKind::Liveness)
            .ok()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<JobRecord>(&bytes).ok())
            .is_some_and(|record| record.state == JobState::Done)
}

/// The store entry a failed tag walk parks the tag's only name in:
/// `<job id>.pending` beside the records, deliberately NOT a `.json` — no
/// reader enumerates it ([`list`], [`read`], `clauth jobs`, the stopped note),
/// and the startup sweep's foreign-file arm is its only consumer. It outlives
/// the record: a record's own reap, collect or Done TTL can run while the
/// marker still names the tag, and a completed walk is the one thing that
/// removes it. A box whose enumeration never works keeps the marker and pays
/// one logged failure per startup sweep until it can — the alternative is
/// retiring the tag's only name on a walk that never proved the tag gone.
#[cfg(unix)]
const PENDING_SUFFIX: &str = ".pending";

/// The job id a pending-marker path names, `None` for any other file.
#[cfg(unix)]
fn pending_marker_job_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(PENDING_SUFFIX)?;
    is_safe_job_id(id).then(|| id.to_string())
}

/// Park the tag's only name in the pending marker. The content repeats the id
/// for an operator reading the store; the sweep takes the tag from the file
/// name. Validated the way [`pending_marker_job_id`] validates the read, so a
/// corrupted record's internal id cannot steer the write outside the store.
#[cfg(unix)]
pub(crate) fn write_pending_marker(job_id: &str) -> Result<()> {
    if !is_safe_job_id(job_id) {
        logline!("clauth: refusing the pending tag-sweep marker for unsafe job id {job_id:?}");
        anyhow::bail!("job id {job_id:?} is not a safe path component");
    }
    let path = jobs_dir()?.join(format!("{job_id}{PENDING_SUFFIX}"));
    if let Err(e) = crate::profile::atomic_write_600(&path, job_id.as_bytes()) {
        logline!("clauth: writing the pending tag-sweep marker for `{job_id}` failed: {e}");
        return Err(e.into());
    }
    Ok(())
}

/// The startup sweep's marker consumer: re-run the tag sweep the failed walk
/// owed, and remove the marker only once a walk completed — a failed
/// enumeration (already logged) keeps the marker, which is the tag's only
/// remaining name.
#[cfg(unix)]
fn sweep_pending_marker(path: &Path, job_id: &str) {
    if stop_tagged(job_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Delete a blocking run's liveness record (best-effort).
///
/// Its own function rather than a `kind` argument on [`remove`], because the two
/// answer different questions: `remove` evicts a result a caller has just taken
/// delivery of, this one clears the liveness spelling a racing beat recreated
/// after a hand-off, so no orphan stands beside the collectable finish.
pub(crate) fn remove_liveness(job_id: &str) {
    if let Ok(path) = job_path(job_id, RecordKind::Liveness) {
        let _ = std::fs::remove_file(path);
    }
}

/// The filename tail a resumed-session marker carries: `<session_id>.resumed`
/// under the jobs dir. Deliberately not `.json`, so no store reader parses
/// it as a record — the sweep's foreign-file arm owns it.
const RESUMED_SUFFIX: &str = ".resumed";

/// Where the resumed marker for `session_id` stands.
fn resumed_marker_path(session_id: &str) -> Result<PathBuf> {
    Ok(jobs_dir()?.join(format!("{session_id}{RESUMED_SUFFIX}")))
}

/// One resumed marker's content, as the fence reads it.
///
/// `resumed_at` is the start of the run its resume launched — the instant the
/// fence compares another record's start against — and `live` holds the ids of
/// the session's records that read live when that resume listed them: the ones
/// the resume did not consume, whose later stops it must never fence.
struct ResumedMarker {
    resumed_at: u64,
    live: Vec<String>,
}

impl ResumedMarker {
    /// The instant on the first line, then one live job id per line. A marker
    /// written without a live set carries the bare instant and reads back with
    /// no ids; an unparsable first line is no marker at all.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let resumed_at = lines.next()?.trim().parse::<u64>().ok()?;
        let live = lines
            .map(str::trim)
            .filter(|id| is_safe_job_id(id))
            .map(str::to_string)
            .collect();
        Some(Self { resumed_at, live })
    }

    /// The content [`Self::parse`] reads back, ids one per line: every job id
    /// is path-safe, so none can carry the separator.
    fn render(&self) -> String {
        let mut text = format!("{}\n", self.resumed_at);
        for id in &self.live {
            text.push_str(id);
            text.push('\n');
        }
        text
    }
}

/// Whether the resumed marker for `session_id` fences a run of `job_id` that
/// started at `run_started_at`. The marker carries the start of the run its
/// resume launched and the ids that resume found live, so only a run already
/// under way before that one, and not one it left running, is a record the
/// resume consumed: the resumed run's own stop and any stop of a run live at
/// the resume are told and kept like any other. The note filters on this and
/// the sweep's post-write check retires a tombstone on it. The session id is
/// validated before the join: it reaches here from a stored record the user's
/// uid can edit.
pub(crate) fn resumed_marker_fences(session_id: &str, job_id: &str, run_started_at: u64) -> bool {
    if !is_safe_job_id(session_id) {
        return false;
    }
    let Ok(path) = resumed_marker_path(session_id) else {
        return false;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // Absent is the only read that fences nothing: a marker that exists
        // but cannot be read is still a resume's fence, dated only by its TTL.
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    // A marker whose content is the session id itself predates the instant
    // content, and one whose first line is no instant is corrupt: both fence
    // everything, because reading either as absent (a numeric session id would
    // read as a centuries-old instant) would re-open a re-tell the file was
    // written to close. Each ages out on the same TTL.
    if text.trim() == session_id {
        return true;
    }
    let Some(marker) = ResumedMarker::parse(&text) else {
        return true;
    };
    run_started_at < marker.resumed_at && !marker.live.iter().any(|id| id == job_id)
}

/// The session id a resumed-marker path names, `None` for any other file.
fn resumed_marker_session_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(RESUMED_SUFFIX)?;
    is_safe_job_id(id).then(|| id.to_string())
}

/// Whether a resumed marker is past [`DONE_TTL_MS`], read off its mtime —
/// the instant the write landed. An unreadable stamp reads as fresh: the
/// marker is the fence that keeps a resumed run from being re-told, so a
/// sweep spares what it cannot date.
fn resumed_marker_expired(path: &Path, now: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .is_ok_and(|age| now.saturating_sub(age.as_millis() as u64) > DONE_TTL_MS)
}

/// Park the resumed session's marker in the jobs store: the start of the run
/// the resume launched, then the ids of the session's records that read live
/// when the resume listed them — the pair [`resumed_marker_fences`] compares
/// another record's start and id against, so neither the resumed run's own
/// stop nor a stop of a run the resume left running is fenced as one it
/// consumed. The session itself is read from the file name. The instant is
/// the run's, not this write's: a background resume mints that record before
/// this runs, and a write-time instant would fence it. Content that already
/// carries a later instant keeps it, so a backward wall-clock step between two
/// resumes can only over-fence, never un-fence a record the earlier resume
/// consumed. Validated the way [`resumed_marker_session_id`] validates the
/// read, so a corrupted record's internal id cannot steer the write outside
/// the store. Written through the 0o600 atomic writer the tree invariant
/// requires, like every other file under `~/.clauth`.
///
/// The write takes the state lock the store's TTL reap takes: the reap stats
/// and unlinks in one hold, so a resume's fresh marker can never land between
/// that stat and that unlink and be the file the reap deletes. The read the
/// monotone instant needs rides inside the same hold, against the same reap.
pub(crate) fn write_resumed_marker(
    session_id: &str,
    resumed_at: u64,
    live: &[String],
) -> Result<()> {
    if !is_safe_job_id(session_id) {
        logline!(
            "clauth: refusing the resumed-session marker for unsafe session id {session_id:?}"
        );
        anyhow::bail!("session id {session_id:?} is not a safe path component");
    }
    let path = resumed_marker_path(session_id)?;
    let live: Vec<String> = live
        .iter()
        .filter(|id| is_safe_job_id(id))
        .cloned()
        .collect();
    let written = with_state_lock(|_held| {
        let resumed_at = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| ResumedMarker::parse(&text))
            .map_or(resumed_at, |existing| existing.resumed_at.max(resumed_at));
        let marker = ResumedMarker { resumed_at, live };
        crate::profile::atomic_write_600(&path, marker.render().as_bytes())
            .map_err(anyhow::Error::from)
    });
    if let Err(e) = written {
        logline!("clauth: writing the resumed-session marker for `{session_id}` failed: {e}");
        return Err(e);
    }
    Ok(())
}

/// Remove the `stopped` record(s) whose resume handle is `session_id`: a
/// `delegate` resume by that id has taken the session, so the stopped note
/// must not offer it again on the next compaction. Only a record that reads
/// [`JobPhase::Stopped`] is removed — a corpse whose recorded child still
/// runs, a blocking run its caller still holds, and every other record keep
/// whatever they are. The spelling removed is the one the record actually
/// holds: a stopped record can still sit in the liveness spelling until the
/// next sweep converts it, and that is the file the note read.
///
/// The resumed marker lands FIRST, before any removal: the sweep's
/// post-write check keys on it, so a tombstone the sweep writes while this
/// removal runs — from a claim this list never saw — is retired by the
/// sweep itself, whichever racer lands second. A marker that fails to write
/// stops the removal loudly: without it no order is airtight, so the record
/// stays.
///
/// A stopped record still `running` on disk is a corpse no sweep has
/// converted yet: its escaped processes carry the run's job-id tag, and once
/// this removal retires the record no later sweep targets that id — so the
/// tag sweep runs here, before the remove. The resumed run's own tag is a
/// NEW job id, so the sweep never reaches the new child. The sweep spends up
/// to the group grace, which is why the one caller sits after its own
/// child's pipe drains have started. Unix-only: Windows has no tag sweep.
///
/// A failed walk does not undo the removal — the resumed session must not
/// read stopped anywhere after it — it parks the tag's name in the pending
/// marker, where a later startup sweep retries the stop. A failed marker
/// write keeps the record instead: it is the tag's only other name.
///
/// Each spelling is claimed the way the sweep's conversion is: renamed to
/// the claim name first, so a conversion racing this removal finds no
/// source to claim and writes nothing at all. A spelling holding a finished
/// result is kept: a job's collectable `Done` beside a stopped liveness
/// leftover is the result a `monitor` collect still owes, never a record this
/// resume took.
///
/// The marker fences by the record, not by its presence alone: it carries the
/// ids of this session's records that read live when this removal listed them,
/// and a stop is a new record — told and kept like any other — when its run
/// started at or after `resumed_at`, the resumed run's own start, or when that
/// run was one this resume left live.
pub(crate) fn remove_stopped_with_session(session_id: &str, now: u64, resumed_at: u64) {
    // Read the store BEFORE the marker lands: these are the ids whose runs the
    // resume left going, and one that stops between this list and the marker
    // write still counts as live at the resume. Reading the store after it
    // would file such a stop as consumed, and the record would never be told.
    // The claim list below stays after the marker, where every tombstone a
    // racing sweep writes is either visible to it or retired by the sweep's
    // own post-write check.
    //
    // The verdict is the ID's, over every spelling it holds: a finished
    // blocking run leaves its liveness file behind, still reading as a live
    // hold, while the collectable spelling is the stop. A record whose stop
    // any spelling carries is this resume's to take.
    let jobs = list(now);
    let stopped: HashSet<&str> = jobs
        .iter()
        .filter(|job| job.phase() == JobPhase::Stopped)
        .map(|job| job.record.job_id.as_str())
        .collect();
    let live: Vec<String> = jobs
        .iter()
        .filter(|job| {
            job.record.session_id.as_deref() == Some(session_id)
                && !stopped.contains(job.record.job_id.as_str())
        })
        .map(|job| job.record.job_id.clone())
        .collect();
    if let Err(e) = write_resumed_marker(session_id, resumed_at, &live) {
        logline!(
            "clauth: writing the resumed-session marker for `{session_id}` failed: {e}; the stopped record stays"
        );
        return;
    }
    #[cfg(test)]
    if HOLD_RESUME_MID_REMOVAL.load(std::sync::atomic::Ordering::Relaxed) {
        hold_resume_park();
    }
    for job in list(now) {
        if job.phase() == JobPhase::Stopped
            && job.record.session_id.as_deref() == Some(session_id)
            && job.record.started_at < resumed_at
            && !live.iter().any(|id| id == &job.record.job_id)
        {
            #[cfg(unix)]
            if job.record.state == JobState::Running
                && !stop_tagged(&job.record.job_id)
                && write_pending_marker(&job.record.job_id).is_err()
            {
                continue;
            }
            claim_job_spelling(&job.record.job_id, RecordKind::Liveness);
            claim_job_spelling(&job.record.job_id, RecordKind::Collectable);
        }
    }
}

/// Claim one record spelling by renaming it to the claim name and deleting
/// the claim: the rename fails exactly when the spelling is already gone,
/// and the claim name is the sweep's own, so a conversion racing this
/// removal finds no source to claim and writes nothing.
///
/// A spelling that carries a finished result and is not a stop is kept: the
/// same state the sweep's conversion refuses to overwrite (a finish whose
/// liveness leftover still stands) reaches here as two spellings of one job,
/// and the collectable one is the result a `monitor` collect still owes.
fn claim_job_spelling(job_id: &str, kind: RecordKind) {
    let Ok(path) = job_path(job_id, kind) else {
        return;
    };
    let finished = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JobRecord>(&bytes).ok())
        .is_some_and(|record| record.envelope.is_some() && !record.stopped);
    if finished {
        return;
    }
    let claimed = path.with_extension("json.claim");
    if std::fs::rename(&path, &claimed).is_ok() {
        let _ = std::fs::remove_file(&claimed);
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

/// The server-marker registration lifecycle. `hold_server_marker` runs on the
/// serve background thread, and a delegate mint must not stamp an owner while
/// that registration is still deciding: it would either mint ownerless the
/// instant before the marker lands, or stamp an owner no marker backs. Three
/// states: never started (no registration began → the mint proceeds ownerless at
/// once, the pre-marker test shape), in flight, and settled (success or failure).
static MARKER_REGISTRATION: AtomicU8 = AtomicU8::new(REG_NEVER);
const REG_NEVER: u8 = 0;
const REG_IN_FLIGHT: u8 = 1;
const REG_SETTLED: u8 = 2;

/// Mark the marker registration in flight. Called by `serve` before it spawns
/// the startup thread, and by a test posing a registration that has not settled.
pub(crate) fn mark_server_registration_in_flight() {
    MARKER_REGISTRATION.store(REG_IN_FLIGHT, Ordering::Release);
}

/// Settle the registration without a registration ever running: the serve
/// thread's spawn was refused, so no later mint waits the full bound on a
/// registration that can never settle itself.
pub(crate) fn settle_marker_registration() {
    MARKER_REGISTRATION.store(REG_SETTLED, Ordering::Release);
}

/// Reset the registration lifecycle and owner stamp to the never-started shape.
/// A test posing an in-flight registration that never settles calls this at its
/// end so no later mint in the process waits on the dead pose.
#[cfg(test)]
pub(crate) fn reset_marker_registration_for_test() {
    MARKER_REGISTRATION.store(REG_NEVER, Ordering::Release);
    SERVER_MARKER_HELD.store(false, Ordering::Relaxed);
    SERVER_STARTED_AT.store(0, Ordering::Relaxed);
}

/// Test seam: the armed sweep removes a corpse's source file once it has read
/// the record and reached the conversion's write point — the instant a
/// concurrent resume's removal acts — so the conversion's source guard is
/// driven on real store bytes rather than a hand-built interleaving.
#[cfg(test)]
static REMOVE_CONVERSION_SOURCE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only RAII: arm [`REMOVE_CONVERSION_SOURCE`] for the guard's lifetime,
/// clearing it on drop even if the test panics. It borrows the
/// [`HomeSandbox`](crate::testutil::HomeSandbox) for the reason
/// `profile::FailAppStateSave` does: the flag is a process-global serialized
/// by `HOME_TEST_LOCK`, which the home sandbox holds.
#[cfg(test)]
pub(crate) struct RemoveConversionSource<'a>(
    std::marker::PhantomData<&'a crate::testutil::HomeSandbox>,
);

#[cfg(test)]
impl<'a> RemoveConversionSource<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox) -> Self {
        REMOVE_CONVERSION_SOURCE.store(true, std::sync::atomic::Ordering::Relaxed);
        Self(std::marker::PhantomData)
    }
}

#[cfg(test)]
impl Drop for RemoveConversionSource<'_> {
    fn drop(&mut self) {
        REMOVE_CONVERSION_SOURCE.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Test seam: the armed sweep parks between its claim rename and its
/// tombstone write, so a test runs a resume inside the claim window on real
/// store bytes rather than a hand-built interleaving. The park is bounded, so
/// a hold a test forgot to release cannot wedge the sweep forever.
#[cfg(test)]
static HOLD_CONVERSION_AFTER_CLAIM: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The park's state: 0 idle, 1 parked at the claim, 2 released.
#[cfg(test)]
static CONVERSION_HOLD: (std::sync::Mutex<u8>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// Test-only RAII: arm [`HOLD_CONVERSION_AFTER_CLAIM`] for the seam's
/// lifetime, releasing the park and clearing the flag on drop even if the
/// test panics. It borrows the [`HomeSandbox`](crate::testutil::HomeSandbox)
/// the way [`RemoveConversionSource`] does: the flag is a process-global
/// serialized by `HOME_TEST_LOCK`, which the home sandbox holds.
#[cfg(test)]
pub(crate) struct HoldConversionAfterClaim<'a>(
    std::marker::PhantomData<&'a crate::testutil::HomeSandbox>,
);

#[cfg(test)]
impl<'a> HoldConversionAfterClaim<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox) -> Self {
        *CONVERSION_HOLD.0.lock().unwrap() = 0;
        HOLD_CONVERSION_AFTER_CLAIM.store(true, std::sync::atomic::Ordering::Relaxed);
        Self(std::marker::PhantomData)
    }

    /// Block until the sweep's conversion parks at the claim, or fail the
    /// test: the seam's whole point is the resume running inside the claim
    /// window.
    pub(crate) fn wait_parked(&self) {
        let (lock, cond) = &CONVERSION_HOLD;
        let mut state = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while *state != 1 {
            assert!(
                *state != 2,
                "the conversion ran past the seam without parking"
            );
            let (guard, _) = cond
                .wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap();
            state = guard;
            assert!(
                std::time::Instant::now() < deadline,
                "the conversion never parked at the claim"
            );
        }
    }
}

#[cfg(test)]
impl Drop for HoldConversionAfterClaim<'_> {
    fn drop(&mut self) {
        let (lock, cond) = &CONVERSION_HOLD;
        *lock.lock().unwrap() = 2;
        cond.notify_all();
        HOLD_CONVERSION_AFTER_CLAIM.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The seam's park: block the sweep's own thread between the claim and the
/// write until the test releases the hold, bounded so a forgotten release
/// cannot wedge the sweep.
#[cfg(test)]
fn hold_conversion_park() {
    let (lock, cond) = &CONVERSION_HOLD;
    let mut state = lock.lock().unwrap();
    *state = 1;
    cond.notify_all();
    let _ = cond
        .wait_timeout_while(state, std::time::Duration::from_secs(5), |state| {
            *state == 1
        })
        .unwrap();
}

/// Arms the resume-side park in [`remove_stopped_with_session`]: the resume
/// parks between writing the resumed marker and listing the store, so a test
/// can run the sweep's conversion to completion inside that window.
#[cfg(test)]
static HOLD_RESUME_MID_REMOVAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The park's state: 0 idle, 1 parked, 2 released.
#[cfg(test)]
static RESUME_HOLD: (std::sync::Mutex<u8>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// Test-only RAII: arm [`HOLD_RESUME_MID_REMOVAL`] for the seam's lifetime,
/// releasing the park and clearing the flag on drop even if the test panics.
/// It borrows the [`HomeSandbox`](crate::testutil::HomeSandbox) the way
/// [`HoldConversionAfterClaim`] does: the flag is a process-global serialized
/// by `HOME_TEST_LOCK`, which the home sandbox holds.
#[cfg(test)]
pub(crate) struct HoldResumeMidRemoval<'a>(
    std::marker::PhantomData<&'a crate::testutil::HomeSandbox>,
);

#[cfg(test)]
impl<'a> HoldResumeMidRemoval<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox) -> Self {
        *RESUME_HOLD.0.lock().unwrap() = 0;
        HOLD_RESUME_MID_REMOVAL.store(true, std::sync::atomic::Ordering::Relaxed);
        Self(std::marker::PhantomData)
    }

    /// Block until the resume parks at its mid-removal point, or fail the
    /// test: the seam's whole point is the sweep's conversion completing
    /// inside the resume's window.
    pub(crate) fn wait_parked(&self) {
        let (lock, cond) = &RESUME_HOLD;
        let mut state = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while *state != 1 {
            assert!(*state != 2, "the resume ran past the seam without parking");
            let (guard, _) = cond
                .wait_timeout(state, std::time::Duration::from_millis(50))
                .unwrap();
            state = guard;
            assert!(
                std::time::Instant::now() < deadline,
                "the resume never parked at its mid-removal point"
            );
        }
    }
}

#[cfg(test)]
impl Drop for HoldResumeMidRemoval<'_> {
    fn drop(&mut self) {
        let (lock, cond) = &RESUME_HOLD;
        *lock.lock().unwrap() = 2;
        cond.notify_all();
        HOLD_RESUME_MID_REMOVAL.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The seam's park: block the resume's own thread at its mid-removal point
/// until the test releases the hold, bounded so a forgotten release cannot
/// wedge the removal.
#[cfg(test)]
fn hold_resume_park() {
    let (lock, cond) = &RESUME_HOLD;
    let mut state = lock.lock().unwrap();
    *state = 1;
    cond.notify_all();
    let _ = cond
        .wait_timeout_while(state, std::time::Duration::from_secs(5), |state| {
            *state == 1
        })
        .unwrap();
}

/// How long a delegate mint waits for an in-flight marker registration to settle
/// before minting the ownerless legacy shape, derived from the flock acquisition
/// the registration itself waits behind ([`state_lock_timeout`]) plus a poll
/// margin that lets a registration which just timed the flock out land its
/// settled signal.
///
/// Residual, deliberate: `with_state_lock` first queues on `lock`'s in-process
/// `THREAD_LOCK`, an unbounded wait behind any other holder in this process, and
/// that holder may itself spend a `SUBPROCESS_BUDGET` under the flock. A bound
/// covering those would be `2 × state_lock_timeout + SUBPROCESS_BUDGET` (~70 s),
/// which delays every delegate call in the boot window by more than the stall it
/// removes. The mint's contract is bounded, never ownerless-by-surprise: past
/// this bound it mints ownerless and logs, and the wait only decides whether a
/// real owner stamp can be carried.
pub(crate) fn server_marker_wait() -> Duration {
    state_lock_timeout() + Duration::from_millis(250)
}

/// The deadline one mint's marker wait arms: [`server_marker_wait`] from now.
pub(crate) fn marker_wait_deadline() -> Instant {
    Instant::now() + server_marker_wait()
}

/// The ONE production marker wait, taken at `delegate_with` (the single entry
/// point every mint is reached from): yields to the runtime between polls
/// (`tokio::time::sleep`), so a mint waiting on the registration never stalls
/// the current-thread runtime that answers every other MCP request.
pub(crate) async fn wait_for_server_marker_async(deadline: Instant) {
    if MARKER_REGISTRATION.load(Ordering::Acquire) != REG_IN_FLIGHT {
        return;
    }
    while MARKER_REGISTRATION.load(Ordering::Acquire) == REG_IN_FLIGHT {
        if Instant::now() >= deadline {
            logline!(
                "clauth: server marker not registered within {:.0}s; the job record mints ownerless",
                server_marker_wait().as_secs_f64()
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether the registration lifecycle has settled, success or failure. The pin
/// that a failed registration still wakes its waiters.
#[cfg(test)]
pub(crate) fn marker_registration_settled() -> bool {
    MARKER_REGISTRATION.load(Ordering::Acquire) == REG_SETTLED
}

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

/// Settles [`MARKER_REGISTRATION`] however the registration ends, a panic
/// included. The explicit store this replaces ran only on a normal return, so an
/// unwinding registration left the state IN_FLIGHT and every later mint waiting
/// out the full bound.
struct MarkerRegistrationSettled;

impl Drop for MarkerRegistrationSettled {
    fn drop(&mut self) {
        MARKER_REGISTRATION.store(REG_SETTLED, Ordering::Release);
    }
}

/// Stamp and hold THIS server's marker. The state lock is what separates the
/// create-then-lock from [`gc_server_markers`]'s prune, which unlinks whatever
/// it reads as unlocked — a marker pruned in that window would leave a live
/// server holding an unlinked file that nothing can count (the same discipline
/// the bare-session registration documents).
///
/// Called once per server at startup; a failure is the caller's to log and
/// step over — it costs the owner-liveness fast path, never the server. The
/// registration settles ([`MARKER_REGISTRATION`]) on every exit, success, failure
/// or panic, so a mint waiting on it wakes either way.
pub(crate) fn hold_server_marker() -> Result<ServerMarkerGuard> {
    let _settled = MarkerRegistrationSettled;
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
/// `running` files whose server is gone ([`running_is_corpse`], save a
/// collectable corpse tied to a session, kept until it has been silent past the
/// window so its session's watcher can report it lost), sweep stray
/// `.tmp` from a crash mid-write, reap stale delivery ledgers, reap the result
/// files whose job the same rules retired, and prune the owner markers no
/// server holds any more. Nothing is evicted by count: the store is bounded by
/// the two TTLs alone (see [`MAX_RETAINED`]).
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
/// knowledge was ever applied, so a corpse polled `running` for hours. Two
/// corpse shapes survive it. One is CONVERTED: a dead blocking run's liveness
/// record becomes the sweep's tombstone, which keeps the handle for a later
/// resume (see [`sweep`]). The other is KEPT: a collectable corpse tied to a
/// session, until it has been silent past the window — the session's watcher
/// still owes it one lost-job wake (see [`sweep`]).
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
/// [`gc_running_corpses`] reaps on. The verdict is unchanged by the keep: a
/// tied collectable corpse inside the silence window still reads a corpse
/// here, and the sweep's keep arm adds a second condition beside the verdict
/// (see [`sweep`]). A record whose owner is gone is a corpse AT
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

/// How one stored record reads to a reader ENUMERATING the store — five
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
    /// Finished. A background run carries its envelope on disk until someone
    /// collects it or the Done TTL reaps it; an attached run's settled
    /// liveness record reads the same phase with no envelope — its caller took
    /// it — and no collect, reaped by the same TTL.
    Done,
    /// `running` on disk whose server is gone — silent past
    /// [`RUNNING_TTL_MS`] (an ownerless record an older server wrote), or owned
    /// by a server whose marker is released — and so is the result.
    Orphaned,
    /// A delegate the host session's exit stopped, or killed before its server
    /// could write the stop: the resume handle survives, and the record says
    /// the run did not finish.
    Stopped,
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
            Self::Stopped => "stopped",
        }
    }

    /// Whether a `monitor` call naming this record's id could collect a RESULT
    /// from it. False for a blocking run by construction (see [`RecordKind`]),
    /// for an orphan, whose result died with its server, and for a stopped
    /// run, whose result never existed. A tombstone (an orphan's or a stop's
    /// collectable record still on disk) is the exception: `monitor` naming
    /// its id answers it with the tombstone copy and then removes it.
    /// `collectable: false` there names the absence of a result, never the
    /// absence of an answer. A settled liveness record (the `Done` a blocking
    /// run's finish leaves) answers true here but no id reaches the spelling —
    /// the row flag in `jobs_cli` consults the record's kind for it.
    pub(crate) fn is_collectable(self) -> bool {
        matches!(self, Self::Running | Self::Done)
    }

    /// Whether something is still spending an account under this record.
    ///
    /// A DIFFERENT question from [`is_collectable`], and the pair splits the
    /// five phases two ways that do not line up: a blocking run is live and not
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

    /// Which of the five situations this record is in.
    ///
    /// The one classification in the crate: `clauth jobs`, `monitor`'s listing
    /// and the TUI's delegates detail all read a record's situation from here, so
    /// none of them can answer differently about one file.
    ///
    /// The spelling on disk is the whole difference between the two live ones:
    /// a [`RecordKind::Liveness`] file that is still `running` exists only
    /// while its caller holds the join (a finished one is the settled record,
    /// read as `Done` above), so no second field is needed to say which
    /// situation a `running` record is in.
    pub(crate) fn phase(&self) -> JobPhase {
        match self.liveness {
            // A stop's `Done` record (the in-process finalize, or the sweep's
            // tombstone) reads `stopped`, never `done` or the crash copy.
            JobLiveness::Done if self.record.stopped => JobPhase::Stopped,
            // A crashed blocking run's record is `Done` on disk but carries no
            // envelope; it is the sweep's tombstone, not a result to collect.
            // The settled liveness record is the same shape with `crashed`
            // false and the run's `outcome` on it — a done row, not an orphan.
            JobLiveness::Done if self.record.crashed => JobPhase::Orphaned,
            JobLiveness::Done => JobPhase::Done,
            // An owner-gone corpse that still carries its handle reads
            // `stopped` at once: the hook and the resumed server's own sweep
            // race, and the note must not need the sweep to have won. An
            // ownerless silent record is not proven to be a `/exit` — silence
            // alone is its corpse rule — so it stays `orphaned`. A corpse
            // whose recorded child is STILL RUNNING stays `orphaned` too: its
            // transcript is being written right now, so naming it `stopped`
            // would offer a resume while the original run is live. It reads
            // `stopped` once a sweep stops the child.
            JobLiveness::Corpse
                if self.record.session_id.is_some()
                    && owner_is_gone(&self.record)
                    && !self.record.recorded_child_alive() =>
            {
                JobPhase::Stopped
            }
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
/// tiebreak: [`new_job_id`] mints `d-<base36 started_at>-<pid>-<counter>`, so
/// the comparison is over a mint stamp, then the minting process's pid, then a
/// per-process sequence. Descending order puts a later stamp first — the same
/// direction the anchor sorts — while a same-millisecond tie across two pids
/// falls to the pid's decimal TEXT: `d-<ms>-9-0` sorts above `d-<ms>-10-0`,
/// stable and deterministic but neither chronological nor numeric. Two more
/// bounds worth stating rather than discovering: base-36 stamps compare as
/// numbers only while they are the same width (the next width change is decades
/// out), and the counter is decimal, so `-9` sorts above `-10` within one
/// millisecond in one process. None can reorder records with different anchors,
/// and all are stable — which is the property this is for.
pub(crate) fn list(now: u64) -> Vec<StoredJob> {
    list_readable(now).unwrap_or_default()
}

/// [`list`] for a caller that must tell an empty store from one it could not
/// read: `None` when the jobs directory cannot be resolved or read (or its
/// path is not a directory), `Some` — possibly empty — otherwise. A missing
/// directory is an empty store; a record file that will not read is still
/// skipped, never an unreadable store.
pub(crate) fn list_readable(now: u64) -> Option<Vec<StoredJob>> {
    let dir = jobs_dir().ok()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
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
    Some(found.into_iter().map(|(_, job)| job).collect())
}

/// Every delivery ledger the claims left behind, newest delivery first — the
/// record a finish keeps once its done file is consumed, so a reader that
/// polls the store still sees finishes claimed between two of its reads. The
/// same unreadable-vs-empty rule as [`list_readable`]: `None` when the jobs
/// directory cannot be resolved or read, `Some` — possibly empty — otherwise;
/// a ledger that will not read is still skipped, never an unreadable store.
pub(crate) fn list_ledgers_readable() -> Option<Vec<DeliveryLedger>> {
    let dir = jobs_dir().ok()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut ledgers: Vec<DeliveryLedger> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_ledger_path(&path) {
            continue;
        }
        let Some(ledger) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<DeliveryLedger>(&b).ok())
        else {
            continue;
        };
        ledgers.push(ledger);
    }
    ledgers.sort_by_key(|ledger| std::cmp::Reverse(ledger.at));
    Some(ledgers)
}

/// The phase a record read from `kind`'s spelling reads as, classified the way
/// [`list`] classifies it: a collect arm and `monitor`'s unknown arm route a
/// caller-supplied id by the SAME verdict every other surface uses, so a
/// collect, a listing and a `monitor` cannot answer differently about one
/// file. [`read`] resolves the collectable spelling and [`read_liveness`] the
/// liveness one, so each caller pins the kind it read.
pub(crate) fn phase_of(record: &JobRecord, kind: RecordKind, now: u64) -> JobPhase {
    let liveness = match record.state {
        JobState::Done => JobLiveness::Done,
        JobState::Running if running_is_corpse(record, now) => JobLiveness::Corpse,
        JobState::Running => JobLiveness::Running,
    };
    StoredJob {
        record: record.clone(),
        kind,
        liveness,
        anchor: retention_anchor(record),
    }
    .phase()
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
    list_banded_readable(now).unwrap_or_default()
}

/// [`list_banded`] keeping the readability answer: `None` when the store
/// could not be read, so a consumer seeding off it never mistakes an
/// unreadable read for an empty one.
pub(crate) fn list_banded_readable(now: u64) -> Option<Vec<StoredJob>> {
    let mut jobs = list_readable(now)?;
    jobs.sort_by_key(|job| job.phase().rank());
    Some(jobs)
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

/// How long a stop waits between its SIGTERM and its SIGKILL: the window the
/// delegate's `claude` gets to flush its transcript and its MCP servers and
/// shell children get to wind down. A real `claude -p` exits within ~185 ms of
/// a group SIGTERM (measured 2026-11-07, Claude Code 2.1.319), so 300 ms gives
/// a loaded box 1.6x that worst case; the poll loop still ends the grace early
/// once no tagged pid is alive. Shared by the supervision loop's stop and the
/// sweep's backstop; the exit handler's own fast SIGTERM is not followed by
/// one — the loop owns the escalation.
#[cfg(unix)]
pub(crate) const GROUP_STOP_GRACE: Duration = Duration::from_millis(300);

/// How long the sweep's stop arm settles a KILLed child's death before
/// judging it: the window is a settle heuristic, not a measured death bound
/// — a child still alive past it is judged the survivor the KILL cannot
/// reach (an uninterruptible state), whose record the sweep keeps rather
/// than naming a live transcript stopped. The only miss is a slow-dying
/// child, converted by a later sweep once it is gone. A child already gone
/// before the stop costs nothing — the settle's first read ends it.
#[cfg(unix)]
const STOP_SURVIVOR_SETTLE: Duration = Duration::from_millis(100);

/// Send `signal` to the delegate child's whole process group. The spawn sets
/// `process_group(0)`, so the child's pid IS the group id. Signalling a group
/// that is already gone fails silently (ESRCH), which is what every stop path
/// wants from its second or late signal.
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: i32) {
    #[expect(
        unsafe_code,
        reason = "killpg has no safe std wrapper; the pid is a recorded child's"
    )]
    // SAFETY: killpg takes a group id and a signal, and the pid came from a
    // child this process recorded at its spawn.
    unsafe {
        libc::killpg(pid as i32, signal);
    }
}

/// Whether the delegate child's process group still exists. `killpg` with
/// signal 0 probes without delivering anything: 0 means members live, ESRCH
/// means none. A stop reads this after its SIGTERM to skip the grace and the
/// SIGKILL once nothing survived, and to know that the group KILL it is about
/// to send still has a group to land on.
#[cfg(unix)]
pub(crate) fn group_alive(pgid: u32) -> bool {
    #[expect(
        unsafe_code,
        reason = "killpg has no safe std wrapper; the pgid is a recorded child's"
    )]
    // SAFETY: killpg takes a group id and a signal; 0 probes without
    // delivering anything.
    unsafe {
        libc::killpg(pgid as i32, 0) == 0
    }
}

/// Send `signal` to one pid, the per-pid twin of [`signal_group`]. A pid that
/// is already gone fails silently (ESRCH), which is what the sweep wants from
/// its second or late signal.
#[cfg(unix)]
fn signal_pid(pid: u32, signal: i32) {
    #[expect(
        unsafe_code,
        reason = "libc::kill has no safe std wrapper; the pid was verified by its exact clauth tag"
    )]
    // SAFETY: kill takes a pid and a signal; the pid's environment was read
    // right before this call and carried this run's exact tag.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// This process and its ancestors, which the escaped-process sweep must never
/// signal: the server can itself be a delegate, and its ancestors may carry a
/// PARENT run's tag. The walk is bounded so a wedged proc table cannot loop
/// it.
#[cfg(unix)]
fn self_and_ancestors() -> HashSet<u32> {
    let mut skip = HashSet::new();
    let mut pid = std::process::id();
    for _ in 0..64 {
        if !skip.insert(pid) {
            break;
        }
        match crate::session_record::parent_pid(pid) {
            Some(parent) if parent != 0 => pid = parent,
            _ => break,
        }
    }
    skip
}

/// Whether `pid`'s environment holds one `CLAUTH_DELEGATE_JOB_ID=<job id>`
/// entry exactly.
#[cfg(unix)]
fn env_carries_tag(pid: u32, job_id: &str) -> bool {
    crate::runtime::process_environ(pid).is_some_and(|env| {
        crate::runtime::environ_entry_equals(&env, super::DELEGATE_JOB_ENV, job_id)
    })
}

/// Whether a sweep target still lives: the liveness read treats a zombie as
/// gone, which is what the sweep wants — a zombie cannot keep running and a
/// SIGKILL cannot end it. macOS reads the no-spawn `proc_bsdinfo`
/// (`proc_pidinfo` answers 0 for a zombie), so one iteration of the 20 ms
/// poll never spends a `ps` spawn; linux keeps the start-time read.
#[cfg(unix)]
fn tagged_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::daemon::gateway::process_start_time(pid).is_some()
    }
    #[cfg(target_os = "macos")]
    {
        crate::runtime::proc_bsdinfo(pid).is_some()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        false
    }
}

/// Test seam: counts the tag sweeps' process enumerations
/// ([`find_tagged_pids`] calls), so a test can pin that the reader's
/// child-alive arm short-circuits before its walk. A test asserts a delta
/// from a baseline read, never an absolute: the threaded fallback runs many
/// tests in one process.
#[cfg(all(test, unix))]
static TAG_WALK_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, unix))]
pub(crate) fn tag_walk_count() -> usize {
    TAG_WALK_COUNT.load(Ordering::Relaxed)
}

/// Every live pid whose environment carries `CLAUTH_DELEGATE_JOB_ID=<job id>`
/// exactly, minus this process and its ancestors. The one enumeration the
/// stop sweep and the reader's tag probe share, so the two paths cannot
/// disagree about which processes a tag names. `None` when the enumeration
/// fails, which the reader's leave decision reads as "cannot prove the tag
/// gone", never as "no tag". On macOS the enumeration spawns `ps` — the same
/// unbounded child [`JobRecord::recorded_child_alive`] reads with — for every
/// reader sweep and every startup sweep.
#[cfg(unix)]
fn find_tagged_pids(job_id: &str) -> Option<Vec<u32>> {
    #[cfg(all(test, unix))]
    TAG_WALK_COUNT.fetch_add(1, Ordering::Relaxed);
    let skip = self_and_ancestors();
    Some(
        crate::runtime::live_process_pids()?
            .into_iter()
            .filter(|pid| !skip.contains(pid) && env_carries_tag(*pid, job_id))
            .collect(),
    )
}

/// Whether any live process carries the run's tag: the reader sweep's
/// read-only half of [`find_tagged_pids`] — no signal, no sleep — which is
/// what keeps a `monitor` call's sweep off the stdio server's runtime. The
/// stop itself stays the startup sweep's. `None` when the enumeration failed:
/// an unreadable process table cannot prove the tag gone.
#[cfg(unix)]
pub(crate) fn tag_lives(job_id: &str) -> Option<bool> {
    Some(!find_tagged_pids(job_id)?.is_empty())
}

/// The escaped-process sweep: stop every live process whose environment
/// carries `CLAUTH_DELEGATE_JOB_ID=<job id>` exactly, except this process and
/// its ancestors. A Bash call inside the delegate runs in its own session, so
/// a `&` child it starts sits outside the delegate's process group — the group
/// stop's `killpg` never reaches it — and carries the run's tag instead.
/// TERM first, then the same grace the group stop uses (polling, as
/// [`stop_recorded_child`] does), then KILL whatever still lives. The tag is
/// re-checked right before each signal, so a pid the OS reused is never
/// signalled blind.
///
/// What the sweep cannot reach: a process started with a scrubbed environment
/// (`env -i`) carries no tag, and on macOS some system binaries hide their
/// environment, so a tagged-looking one reads untagged. Both keep running.
///
/// Returns whether the process walk completed. `false` (already logged by the
/// enumeration) cannot prove the tag gone, so the record that names it must
/// stay for a later sweep rather than convert.
#[cfg(unix)]
pub(crate) fn stop_tagged(job_id: &str) -> bool {
    // A failed enumeration already logged; a stop finds nothing to signal
    // rather than panicking on it.
    let Some(mut alive) = find_tagged_pids(job_id) else {
        return false;
    };
    for pid in &alive {
        signal_pid(*pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + GROUP_STOP_GRACE;
    while !alive.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        alive.retain(|pid| tagged_alive(*pid));
    }
    for pid in alive {
        // The grace is the pid-reuse window: re-read the tag before the KILL.
        if env_carries_tag(pid, job_id) {
            signal_pid(pid, libc::SIGKILL);
        }
    }
    true
}

/// The exit stop's backstop: stop the delegate child a dead server left
/// running, when the recorded pid still names it, and stop the processes that
/// run escaped outside its group ([`stop_tagged`], keyed on the run's job id
/// tag). The escaped sweep runs whether or not the child lives: a delegate
/// that finished or died on its own after its server died still left the
/// processes it backgrounded running, and no later sweep targets their job
/// id. The start token tells the run's own process from a pid the OS reused —
/// a mismatched one is left alone. Runs ONLY from the startup sweep's own
/// thread (the full scope): the kill spends up to the group grace, and the
/// reader sweep a `monitor` call triggers must never block the stdio server's
/// runtime on it. Unix-only; Windows keeps today's behaviour.
///
/// Returns whether the tag sweep's process walk completed: a failed walk
/// cannot prove the tag gone, so the caller keeps the record that names it.
#[cfg(unix)]
fn stop_recorded_child(record: &JobRecord) -> bool {
    let child_alive = record.recorded_child_alive();
    if child_alive {
        signal_group(record.child_pid, libc::SIGTERM);
    }
    // The deadline is anchored before the sweep so the sweep's grace — the
    // same length, on its own TERM — overlaps this one instead of running
    // after it: one shared grace window, both KILLs at its end, never two
    // serial graces.
    let deadline = Instant::now() + GROUP_STOP_GRACE;
    let walked = stop_tagged(&record.job_id);
    if !child_alive {
        return walked;
    }
    // Poll the group across the grace, the way the supervision loop polls
    // its child: the grace is spent only while anyone survived the TERM. The
    // KILL fires even when the recorded leader itself already exited on the
    // TERM — its surviving descendants hold the group, and Linux keeps the
    // reaped pid allocated while any group member lives, so the pgid cannot
    // be reused under it.
    while group_alive(record.child_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if group_alive(record.child_pid) {
        signal_group(record.child_pid, libc::SIGKILL);
    }
    walked
}

fn sweep(now: u64, scope: Scope) {
    let full = scope == Scope::Everything;
    let Ok(dir) = jobs_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    // The ids whose records this sweep kept. A result file none of them names
    // has lost its job — retired here by the same Done-TTL rule the record
    // itself answers to, or in an earlier sweep — and goes with it, under the
    // full sweep only, like every destructive rule: never from a reader.
    let mut surviving: HashSet<String> = HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            if full {
                // A pending marker is the tag's name a failed walk parked; it
                // is swept, never swept away. A delivery ledger is kept while
                // the unknown answer it feeds matters and reaped with the done
                // TTL; everything else here is a stray tmp / foreign file.
                #[cfg(unix)]
                if let Some(job_id) = pending_marker_job_id(&path) {
                    sweep_pending_marker(&path, &job_id);
                    continue;
                }
                // A resumed marker is the fence that keeps a resumed run
                // from being re-told: a live one is spared whatever else
                // this sweep would delete here, an aged one reaped on the
                // store's Done TTL. The stat and the unlink run under the
                // state lock the marker's writer takes, so a fresh marker a
                // concurrent resume lands is never the file reaped.
                if resumed_marker_session_id(&path).is_some() {
                    let _ = with_state_lock(|_held| {
                        if resumed_marker_expired(&path, now) {
                            let _ = std::fs::remove_file(&path);
                        }
                        Ok(())
                    });
                    continue;
                }
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
            // A collectable corpse tied to a session is kept until it has been
            // silent past the window: the session's watcher still owes it one
            // lost-job wake. An untied corpse has no watcher to owe, a silent
            // one has had the window, and a liveness record keeps its own arm
            // below. A corpse whose server is gone AND whose record carries
            // its resume handle is an exit-shaped stop, so the tied keep only
            // holds the handleless corpses the lost wake owes — the stop arm
            // below classifies the handled one at once (the startup sweep
            // stopping the child it left; the reader leaving the record until
            // the child and the tag are provably gone).
            JobState::Running => {
                running_is_corpse(&record, now)
                    && (kind == RecordKind::Liveness
                        || record.host_session.is_none()
                        || (record.session_id.is_some() && owner_is_gone(&record))
                        || running_is_silent(&record, now))
            }
        };
        if !expired {
            surviving.insert(record.job_id.clone());
            continue;
        }
        // A corpse whose server is gone AND whose record carries its resume
        // handle was a delegate the host session's exit stopped or killed:
        // it becomes a stopped tombstone instead of being reaped, and the
        // child it may have left running is stopped first. A handleless
        // corpse is the watcher's lost wake to carry (kept above while tied
        // and unsilent), and an ownerless silent record is not proven to be
        // an exit, so both keep the arms below — a `session_id`, when one
        // carries it, stays on the crash path.
        if record.state == JobState::Running
            && record.session_id.is_some()
            && owner_is_gone(&record)
        {
            // The kill spends up to the group grace, so it runs only from
            // the startup sweep's own thread (the full scope). A reader
            // sweep whose corpse's recorded child STILL RUNS leaves the
            // record for that startup sweep — a tombstone written now would
            // read `stopped` for a live, spending delegate (see
            // [`StoredJob::phase`]) — and one whose child is provably gone
            // converts here, kill or no kill. The tag probe adds one more
            // leave: a gone child's run may still have escaped processes
            // carrying the job-id tag, and the startup sweep's stop targets
            // that id through this record — a sweep that converted the corpse
            // would retire the only record naming the tag, and those
            // processes would never be stopped. The probe is read-only, so
            // the reader still sends no signal and sleeps not at all. A
            // failed enumeration is not an empty one: only a completed walk
            // that found no tagged process proves the tag gone, so only that
            // may convert, on the startup path as on the reader's.
            #[cfg(unix)]
            {
                if full {
                    if !stop_recorded_child(&record) {
                        // A box whose enumeration never works keeps this
                        // corpse, one logged failure per sweep, until it can.
                        surviving.insert(record.job_id.clone());
                        continue;
                    }
                    // A child that survived the TERM+KILL keeps the running
                    // record: a tombstone would read stopped while the child
                    // still spends, and the note would offer the live
                    // transcript for a resume. The settle lets a KILLed
                    // child's death land before the verdict, so only a child
                    // the KILL cannot reach is judged the survivor.
                    let settle = Instant::now() + STOP_SURVIVOR_SETTLE;
                    while record.recorded_child_alive() && Instant::now() < settle {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if record.recorded_child_alive() {
                        surviving.insert(record.job_id.clone());
                        continue;
                    }
                } else if record.recorded_child_alive() || tag_lives(&record.job_id) != Some(false)
                {
                    surviving.insert(record.job_id.clone());
                    continue;
                }
            }
            // The conversion writes to the COLLECTABLE spelling, a file this
            // sweep has not read (or the corpse itself, for a background
            // record). Never overwrite a record that carries an envelope: a
            // finish whose liveness leftover is the stale file here must keep
            // its result.
            if kind == RecordKind::Liveness
                && read(&record.job_id).is_some_and(|existing| existing.envelope.is_some())
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            #[cfg(test)]
            if REMOVE_CONVERSION_SOURCE.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(&path);
            }
            // The conversion's atomic step claims the source: the rename fails
            // exactly when the file this sweep read no longer stands — a
            // concurrent resume's removal retired the record — and a tombstone
            // written then would resurrect it, so nothing is written. The
            // claim also moves the only copy a later removal could still
            // reach, so the write below cannot recreate a removed record. A
            // crash between the claim and the write loses the record's only
            // copy, the same accepted cost the delivery claim carries (a
            // leftover claimed file is reaped by the startup sweep's
            // foreign-file arm).
            let claimed = path.with_extension("json.claim");
            if std::fs::rename(&path, &claimed).is_err() {
                continue;
            }
            #[cfg(test)]
            if HOLD_CONVERSION_AFTER_CLAIM.load(std::sync::atomic::Ordering::Relaxed) {
                hold_conversion_park();
            }
            let mut stopped = record;
            stopped.state = JobState::Done;
            stopped.done_at = now;
            stopped.envelope = None;
            stopped.stopped = true;
            // The crash marker stays: the server DID die without finishing —
            // the stop is the exit's classification of that death, not a
            // replacement for it.
            stopped.crashed = true;
            // Drop the source only once the tombstone landed: a failed write
            // (ENOSPC, read-only dir) renames the claim back, so the source
            // stays the surviving carrier of the handle.
            if write_atomic(&stopped, RecordKind::Collectable).is_ok() {
                let _ = std::fs::remove_file(&claimed);
                // The resume's marker predates its removal: a tombstone for a
                // run that started before the resumed run is the record the
                // resume already took — a claim this sweep landed inside the
                // resume's own removal is seen by neither side otherwise — so
                // it must not outlive the resume. A run that started at or
                // after the resumed run, or one the resume listed live and
                // finds stopped here, is a new stop, and its tombstone stands.
                if stopped.session_id.as_deref().is_some_and(|session_id| {
                    resumed_marker_fences(session_id, &stopped.job_id, stopped.started_at)
                }) && let Ok(tombstone) = job_path(&stopped.job_id, RecordKind::Collectable)
                {
                    let _ = std::fs::remove_file(tombstone);
                }
            } else {
                let _ = std::fs::rename(&claimed, &path);
            }
        } else if record.state == JobState::Running && kind == RecordKind::Liveness {
            // The conversion writes to the COLLECTABLE spelling, a file this
            // sweep has not read. Never overwrite a record that carries an
            // envelope: a finish whose liveness leftover is the stale file here
            // must keep its result.
            if read(&record.job_id).is_some_and(|existing| existing.envelope.is_some()) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            // The same proof the branch above takes, for the corpses it
            // cannot classify as a stopped peer: an ownerless record, or one
            // that never pinned a session id, still leaves processes carrying
            // its job-id tag, and retiring the record retires the tag's only
            // handle. The startup sweep stops the tag first; the reader stays
            // read-only and leaves whatever a completed walk cannot prove
            // gone.
            #[cfg(unix)]
            {
                if full {
                    if !stop_tagged(&record.job_id) {
                        // A box whose enumeration never works keeps this
                        // corpse, one logged failure per sweep, until it can.
                        surviving.insert(record.job_id.clone());
                        continue;
                    }
                } else if tag_lives(&record.job_id) != Some(false) {
                    surviving.insert(record.job_id.clone());
                    continue;
                }
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
            // The reap retires the tag's only name: an expired Running record
            // may still have escaped processes carrying its tag, so the walk
            // must complete first, and the record is retired only once the
            // pending marker naming that tag stands. A finished record keeps
            // today's reap: its stop's walk already ran, and a failed one
            // parked the marker then.
            #[cfg(unix)]
            {
                if record.state == JobState::Running {
                    if full {
                        if stop_tagged(&record.job_id) {
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                        if write_pending_marker(&record.job_id).is_ok() {
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                        // The marker write failed: the record is the tag's
                        // only name still, so the reap waits for the next
                        // sweep's retry.
                        surviving.insert(record.job_id.clone());
                        continue;
                    }
                    // The reader sends no signal and writes no marker: only a
                    // completed walk that found nothing proves the tag gone.
                    if tag_lives(&record.job_id) != Some(false) {
                        surviving.insert(record.job_id.clone());
                        continue;
                    }
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }
    if full {
        reap_unbacked_results(&surviving);
    }
}

/// Reap the result files the store no longer backs: every `<safe job
/// id>.txt` in `jobs/results/` whose id no surviving record names. A record's
/// expiry is decided by the sweep that just ran, on the same [`DONE_TTL_MS`]
/// horizon the record itself answers to, so a result file outliving its record
/// is past retention by construction — one clock, not a second one. Only the
/// shape `write_result_file` creates is judged; anything else in the directory
/// is left exactly as found.
fn reap_unbacked_results(surviving: &HashSet<String>) {
    let Ok(dir) = jobs_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir.join("results")) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_safe_job_id(id) || surviving.contains(id) {
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_jobs.rs"]
mod tests;
