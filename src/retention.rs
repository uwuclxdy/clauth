//! The operator's remembered transcript retention: the last valid
//! `cleanupPeriodDays` clauth read from `~/.claude/settings.json`, and whether a
//! put-back is waiting for the operator's keep-or-remove answer.
//!
//! Claude Code's retention sweep deletes every transcript older than
//! `cleanupPeriodDays ?? 30` from the store its config dir links, and a shared
//! runtime tree links the operator's own `~/.claude/projects`, so a launch that
//! resolves no number sweeps the operator's store at 30 days. No settings file
//! can bring a lost number back, so clauth keeps its own copy in
//! `~/.clauth/settings-retention.json` and writes it back wherever the key goes
//! missing: the operator file, every live shared runtime copy, and every shared
//! build. An isolated tree links no store and gets nothing.
//!
//! Only a positive integer is remembered — whatever its JSON spelling (`3650`,
//! `3650.0`, `3.65e3`) — the one shape Claude Code's schema takes, and a key
//! that is present, valid or not, is never overwritten. Every write keeps the
//! member's carried rank, keeps a runtime copy's record status, and records the
//! operator file, so a put-back changes no member's rank; at an exact rank tie
//! a put-back can still flip which member wins, since the merge engine breaks
//! ties by record status.
//!
//! The record is its own file rather than a field on the settings sync's
//! written-copy record: that one reads fail-open, so a torn read there would
//! forget the number, and a clauth predating this module rewrites it without
//! the field.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::jsonsync::MemberSnapshot;
use crate::lock::{StateLockHeld, with_state_lock};
use crate::logline::logline;
use crate::profile::{claude_dir, clauth_dir};

/// The `settings.json` key whose absence re-arms Claude Code's 30-day sweep.
const KEY: &str = "cleanupPeriodDays";

/// Latch for the record-fault lines — a record that cannot be read and one
/// that cannot be written share it, so a fault that stays is reported once per
/// process rather than on every check. Cleared by the next clean write, never
/// by a clean read: a standing write fault with clean reads would otherwise
/// report again on every check.
static RECORD_FAULT_WARNED: AtomicBool = AtomicBool::new(false);

/// The files whose put-back last failed, so a write fault that stays is
/// reported once per process rather than on every check. Per file: a put-back
/// landing in one file must not re-arm the line for another that still fails.
/// A file leaves the set when a rewrite of it lands, so a recurrence is
/// reported again.
static PUT_BACK_FAILURES: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// What clauth remembers about the operator's transcript retention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Retention {
    /// The operator's last valid `cleanupPeriodDays`, `None` when nothing is
    /// remembered.
    #[serde(default)]
    pub(crate) days: Option<u64>,
    /// Whether a put-back is waiting for the operator's keep-or-remove answer.
    #[serde(default)]
    pub(crate) question_open: bool,
}

/// `~/.clauth/settings-retention.json`.
pub(crate) fn record_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("settings-retention.json"))
}

/// The record as it stands on disk; a missing record remembers nothing.
///
/// # Errors
///
/// The record cannot be read or does not parse.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no caller outside the tests reads the record yet")
)]
pub(crate) fn load() -> Result<Retention> {
    read_record(&record_path()?)
}

/// The stat snapshot from the last [`reconcile`] pass that settled; the daemon
/// leg's fast-path key.
static LAST_RECONCILED: Mutex<Option<Vec<MemberSnapshot>>> = Mutex::new(None);

/// One check of the operator file and every live shared copy, under the state
/// flock: the daemon's leg, which runs with no `clauth start` session live.
///
/// The check sits behind a stat fast path over every file it reads (the
/// operator file, each live shared copy, the retention record), in the shape
/// of [`crate::jsonsync::run_with_cache`]: a wake whose inputs still read
/// those stats checks nothing. Only a pass that skipped no merge and wrote or
/// failed to write nothing keys it, so the next wake after any other pass
/// re-reads every file.
///
/// # Errors
///
/// A path cannot be resolved or the flock cannot be taken.
pub(crate) fn reconcile() -> Result<()> {
    let operator = claude_dir()?.join("settings.json");
    // The fast-path key only: the members are listed again inside the hold,
    // like `sync_members`. `known_paths` starts with the operator file itself,
    // so the key covers everything the check reads once the record is added.
    let mut stat_key = crate::settings_sync::known_paths()?;
    stat_key.push(record_path()?);
    crate::jsonsync::run_with_cache(&LAST_RECONCILED, &stat_key, || {
        let outcome = with_state_lock(|held| {
            settle(
                held,
                &operator,
                &crate::settings_sync::known_paths()?,
                &crate::settings_sync::written_record_path()?,
            )
        })?;
        // The key was statted before the pass, so after a write it describes
        // bytes that are gone: the same bytes restored at the same stat would
        // read as settled and never be put back.
        Ok(outcome.merge_ok && !outcome.attempted_write)
    })
}

/// The operator's `remove it` answer: the key leaves the operator file and
/// every live shared copy, and the number and its question are forgotten.
///
/// The key goes before the record does, so a write that fails leaves the
/// number remembered and the next pass puts it back rather than losing it.
/// A member Claude Code rewrites while this runs is re-read and retried, at
/// most three tries per member; one that keeps moving fails the whole remove
/// naming that file, with the record untouched, so the caller can say it
/// failed and ask again. A member that exists but cannot be read or does not
/// parse counts as moving too — its key may well still be there, and skipping
/// it would report success while a later merge flows the key back; it is
/// re-read after a pause of at most 100 ms, so a copy caught mid-write has
/// time to land, and the failure names whichever state the last read saw.
/// Only a member that is not there is skipped: nothing to remove; any other
/// stat error fails the remove, for the same reason.
///
/// # Errors
///
/// A path cannot be resolved, the flock cannot be taken, a member cannot be
/// statted, a write fails, or a member kept changing or stayed unreadable
/// across three tries.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no surface offers the remove answer yet")
)]
pub(crate) fn remove() -> Result<()> {
    with_state_lock(|_held| {
        let operator = claude_dir()?.join("settings.json");
        let sync_record = crate::settings_sync::written_record_path()?;
        for path in crate::settings_sync::known_paths()? {
            let mut tries = 0;
            loop {
                match std::fs::metadata(&path) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!(
                                "failed to stat {}, so its cleanupPeriodDays was not removed; \
                                 the key may still be there",
                                path.display()
                            )
                        });
                    }
                }
                let mut unreadable = false;
                if let Some(mut member) = read_member(&path) {
                    if !member.obj.contains_key(KEY) {
                        break;
                    }
                    let mut obj = std::mem::take(&mut member.obj);
                    obj.shift_remove(KEY);
                    if rewrite(&sync_record, &member, &obj, path == operator)? {
                        break;
                    }
                } else {
                    // Present but unreadable or not an object: Claude Code
                    // mid-write. Counts as moving, not as absent.
                    unreadable = true;
                }
                tries += 1;
                if tries == 3 {
                    anyhow::bail!(
                        "{} {}",
                        path.display(),
                        if unreadable {
                            "cannot be read or does not parse, so its cleanupPeriodDays was \
                             not removed; the key may still be there"
                        } else {
                            "kept changing while its cleanupPeriodDays was being removed; \
                             the key is still there"
                        }
                    );
                }
                if unreadable {
                    // The pause lets a write in flight land, so a retry does
                    // not spend the cap on the same torn bytes.
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        let record = record_path()?;
        match std::fs::remove_file(&record) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", record.display())),
        }
    })
}

/// What one [`settle`] pass decided.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SettleOutcome {
    /// The remembered number, the one a keyless copy built from this operator
    /// file carries. `None` when nothing is remembered.
    pub(crate) days: Option<u64>,
    /// Whether the caller's merge may run this pass. A failed put-back into
    /// the operator file clears it: a merge after it could strip the key from
    /// the copies that already got it back.
    pub(crate) merge_ok: bool,
    /// Whether this pass wrote a file or failed to: a settings put-back, a
    /// record write. A stat key taken before such a pass describes bytes the
    /// pass replaced, or a write still owed, so no caller caches it.
    pub(crate) attempted_write: bool,
}

/// Test-only count of [`settle`] passes per operator file, so a test can pin
/// the reads a wake costs through the fast paths in front of it. Keyed by the
/// operator path, one per sandbox home: the daemon leg runs its check on its
/// own thread, and parallel tests never share a home. Linux-only, like its one
/// reader, which needs inotify's per-write event delivery.
#[cfg(all(test, target_os = "linux"))]
static SETTLE_PASSES: Mutex<std::collections::BTreeMap<PathBuf, u64>> =
    Mutex::new(std::collections::BTreeMap::new());

/// How many [`settle`] passes have read `operator` so far. Test-only.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn settle_passes(operator: &Path) -> u64 {
    SETTLE_PASSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(operator)
        .copied()
        .unwrap_or(0)
}

/// One check under the held state flock: learn the operator file's number, put
/// a remembered one back into a keyless operator file, and carry it into every
/// keyless copy in `members` (the operator file among them is skipped). Every
/// call reads every file it settles: the callers' fast paths decide whether a
/// wake reaches it.
///
/// A keyless copy carries the operator file's own value whenever that file
/// holds the key, valid or not: a copy that then wins the merge pushes back
/// exactly what the operator file already holds. A missing or unparseable
/// operator file is never written, so it logs nothing and opens no question.
///
/// No retention fault fails the check. A record fault is reported once and the
/// decision this pass already made stands. A failed operator put-back —
/// including one refused because Claude Code rewrote the file since the read —
/// reverts the open question, reports once, and clears the caller's merge for
/// this pass only; the copies still get their put-back. A failed copy put-back
/// reports once and the pass goes on — the merge refusal keeps that copy from
/// winning while keyless, and a keyed operator file's win writes the key back
/// into it anyway. A failed write of any kind marks the pass
/// [`SettleOutcome::attempted_write`], so the next wake retries it.
///
/// # Errors
///
/// The record path cannot be resolved.
pub(crate) fn settle(
    _held: &StateLockHeld,
    operator: &Path,
    members: &[PathBuf],
    sync_record: &Path,
) -> Result<SettleOutcome> {
    let record = record_path()?;
    #[cfg(all(test, target_os = "linux"))]
    {
        *SETTLE_PASSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(operator.to_path_buf())
            .or_insert(0) += 1;
    }
    let stored = remembered_for_pass(&record);
    let mut on_disk = stored.clone();
    let mut next = stored;
    let mut operator_value: Option<Value> = None;
    let mut merge_ok = true;
    let mut attempted_write = false;
    if let Some(mut home) = read_member(operator) {
        match home.obj.get(KEY) {
            Some(value) => {
                if let Some(days) = valid_days(value)
                    && next.days != Some(days)
                {
                    next = Retention {
                        days: Some(days),
                        question_open: false,
                    };
                }
                operator_value = Some(value.clone());
            }
            None => {
                if let Some(days) = next.days {
                    // The open question lands before the write it announces:
                    // a record write failing after the put-back would leave
                    // the number back with nobody asked about it.
                    let was_open = next.question_open;
                    next.question_open = true;
                    attempted_write |= persist(&record, &mut on_disk, &next);
                    let mut obj = std::mem::take(&mut home.obj);
                    obj.insert(KEY.to_owned(), Value::from(days));
                    match rewrite(sync_record, &home, &obj, true) {
                        Ok(true) => {
                            logline!(
                                "clauth: cleanupPeriodDays was gone from ~/.claude/settings.json, \
                                 put {days} back; open clauth to keep or remove it"
                            );
                            operator_value = Some(Value::from(days));
                            attempted_write = true;
                        }
                        Ok(false) => {
                            // Same state as the `Err` arm: the newer keyless
                            // operator file would win the merge and strip the
                            // key from the copies this pass just healed.
                            next.question_open = was_open;
                            merge_ok = false;
                        }
                        Err(e) => {
                            next.question_open = was_open;
                            merge_ok = false;
                            attempted_write = true;
                            put_back_failure(operator, &e);
                        }
                    }
                }
            }
        }
    }
    if let Some(days) = next.days {
        let carried = operator_value.unwrap_or_else(|| Value::from(days));
        for path in members.iter().filter(|p| p.as_path() != operator) {
            let Some(mut copy) = read_member(path) else {
                continue;
            };
            if copy.obj.contains_key(KEY) {
                continue;
            }
            let mut obj = std::mem::take(&mut copy.obj);
            obj.insert(KEY.to_owned(), carried.clone());
            match rewrite(sync_record, &copy, &obj, false) {
                Ok(true) => attempted_write = true,
                // Claude Code rewrote the copy since the read: its own write
                // moved the stat, so the next wake reads it again.
                Ok(false) => {}
                Err(e) => {
                    attempted_write = true;
                    put_back_failure(path, &e);
                }
            }
        }
    }
    attempted_write |= persist(&record, &mut on_disk, &next);
    Ok(SettleOutcome {
        days: next.days,
        merge_ok,
        attempted_write,
    })
}

/// A built `settings.json` carrying `days` when it holds no `cleanupPeriodDays`
/// of its own; one that holds the key keeps it as it is.
///
/// # Errors
///
/// `settings` is not a JSON object.
pub(crate) fn carry_into(settings: String, days: u64) -> Result<String> {
    let mut value: Value =
        serde_json::from_str(&settings).context("failed to parse the built settings.json")?;
    let obj = value
        .as_object_mut()
        .context("the built settings.json is not an object")?;
    if obj.contains_key(KEY) {
        return Ok(settings);
    }
    obj.insert(KEY.to_owned(), Value::from(days));
    serde_json::to_string_pretty(&value).context("failed to serialize settings.json")
}

/// The number Claude Code's schema takes: a positive integer, whatever its
/// JSON spelling (`3650`, `3650.0`, `3.65e3` all parse to the integer 3650 in
/// Claude Code's `JSON.parse` + zod `int().positive()` chain, which accepts
/// every integral JSON number).
fn valid_days(value: &Value) -> Option<u64> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(days) = number.as_u64() {
        return (days > 0).then_some(days);
    }
    let days = number.as_f64()?;
    (days.is_finite() && days > 0.0 && days.fract() == 0.0 && days <= u64::MAX as f64)
        .then_some(days as u64)
}

fn read_record(path: &Path) -> Result<Retention> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Retention::default()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        crate::format::parse_error(path.display(), crate::format::json_parse_position(&e))
    })
}

/// [`read_record`] for a check: a record that cannot be read or parsed
/// remembers nothing, is reported once, and is rewritten only once the
/// operator file holds a valid number, which would replace whatever it held.
fn remembered_for_pass(path: &Path) -> Retention {
    match read_record(path) {
        Ok(record) => record,
        Err(e) => {
            record_fault(format_args!(
                "clauth: {e:#}; cleanupPeriodDays is not put back until \
                 ~/.claude/settings.json holds a number again"
            ));
            Retention::default()
        }
    }
}

/// Report one record fault under [`RECORD_FAULT_WARNED`]: the first of a run
/// logs `line`, the rest are silent until a record write lands.
fn record_fault(line: std::fmt::Arguments<'_>) {
    if !RECORD_FAULT_WARNED.swap(true, Ordering::Relaxed) {
        logline!("{line}");
    }
}

/// Clear both latches, so a test counting latched lines never reads a latch an
/// earlier test in the same process left set.
#[cfg(test)]
pub(crate) fn reset_latches() {
    RECORD_FAULT_WARNED.store(false, Ordering::Relaxed);
    PUT_BACK_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Report one failed put-back write into `path` under [`PUT_BACK_FAILURES`].
fn put_back_failure(path: &Path, e: &anyhow::Error) {
    let first = PUT_BACK_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf());
    if first {
        logline!(
            "clauth: failed to put cleanupPeriodDays back into {}: {e:#}",
            path.display()
        );
    }
}

/// Write `next` owner-only when it differs from what is on disk. A failure is
/// reported once through [`record_fault`] and never fails the check: the
/// decision this pass already made stands, and a later pass retries the write.
/// A clean write clears the fault latch. Returns whether it wrote or failed
/// to, `false` only when the record already said `next`.
fn persist(path: &Path, on_disk: &mut Retention, next: &Retention) -> bool {
    if on_disk == next {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec_pretty(next) else {
        record_fault(format_args!(
            "clauth: failed to serialize the retention record {}; \
             the remembered cleanupPeriodDays is not durable until it writes cleanly",
            path.display()
        ));
        return true;
    };
    match crate::profile::atomic_write_600(path, bytes) {
        Ok(()) => {
            RECORD_FAULT_WARNED.store(false, Ordering::Relaxed);
            *on_disk = next.clone();
        }
        Err(e) => {
            record_fault(format_args!(
                "clauth: failed to write the retention record {}: {e:#}; \
                 the remembered cleanupPeriodDays is not durable until it writes cleanly",
                path.display()
            ));
        }
    }
    true
}

/// One `settings.json` as this check read it.
struct Member {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
    bytes: Vec<u8>,
    obj: Map<String, Value>,
}

/// `None` for a file that is missing, unreadable or not a JSON object: such a
/// file is never written.
fn read_member(path: &Path) -> Option<Member> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let bytes = std::fs::read(path).ok()?;
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&bytes) else {
        return None;
    };
    Some(Member {
        path: path.to_path_buf(),
        mtime,
        size: meta.len(),
        bytes,
        obj,
    })
}

/// The [`REWRITE_HOOK`] closure type.
#[cfg(test)]
type RewriteHook = dyn Fn(&Path) + Send + Sync;

/// Test-only interleave point at the top of [`rewrite`], after the caller's
/// read and before the re-stat: the span a Claude Code write landing in the
/// read-to-re-stat window occupies. A test arms it to land such a write and
/// pin the rewrite's refusal. `None` is inert; armed and disarmed only while
/// the arming test holds a `HomeSandbox`, so `HOME_TEST_LOCK` serializes it.
#[cfg(test)]
static REWRITE_HOOK: Mutex<Option<std::sync::Arc<RewriteHook>>> = Mutex::new(None);

/// Arm or clear the [`REWRITE_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_rewrite_hook(hook: Option<Box<RewriteHook>>) {
    *REWRITE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`REWRITE_HOOK`], if any, handing it the file the rewrite
/// is about to touch. Clones the `Arc` so the hook runs outside the slot lock.
#[cfg(test)]
fn run_rewrite_hook(path: &Path) {
    let hook = REWRITE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook(path);
    }
}

/// Write `obj` over `member` at the rank the sync record already gives it (its
/// own read mtime when it is not clauth's echo), so the pass that follows picks
/// the winner it would have picked anyway. The operator file is always
/// recorded: it is exempt from the record's winner gate, and the entry is what
/// keeps its fresh mtime from making it the newest member. A runtime copy is
/// recorded only when the record already names it, so an unrecorded copy stays
/// unable to win. Returns `false`, writing nothing, when Claude Code rewrote
/// the file since this check read it: its newer bytes stay and the next check
/// decides again.
fn rewrite(
    sync_record: &Path,
    member: &Member,
    obj: &Map<String, Value>,
    operator: bool,
) -> Result<bool> {
    #[cfg(test)]
    run_rewrite_hook(&member.path);
    if crate::jsonsync::member_changed_since_read(&member.path, member.mtime, member.size) {
        return Ok(false);
    }
    let rank = crate::jsonsync::carried_rank_of(
        sync_record,
        &member.path,
        member.mtime,
        member.size,
        &member.bytes,
    );
    let record = (operator || crate::jsonsync::is_recorded(sync_record, &member.path))
        .then_some(sync_record);
    let bytes = serde_json::to_vec_pretty(obj)
        .with_context(|| format!("failed to serialize {}", member.path.display()))?;
    crate::jsonsync::write_copy_and_record(record, &member.path, &bytes, rank, !operator, None)
        .with_context(|| format!("failed to write {}", member.path.display()))?;
    PUT_BACK_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&member.path);
    Ok(true)
}
