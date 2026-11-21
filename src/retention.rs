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
//! build; every shared launch also passes it on its spawn `--settings`. An
//! isolated tree links no store and gets nothing.
//!
//! The record also lists every spawn file a shared launch wrote — number or
//! not — each with the launch's floor (a delegate's own valid number, which
//! the larger-number rule never undercuts; none for an operator launch). A
//! background job's `respawnFlags` carry its launch's `--settings` file
//! (measured 2026-10-20, Claude Code 2.1.304), so a respawn reads it again:
//! the check rewrites each listed file when the number changes — the larger
//! of the remembered number and the floor — drops the entries whose file is
//! gone, and `remove it` rewrites each to its floor before the record goes. A
//! value the operator passed on their own `clauth start -- --settings` is
//! never registered, so it is never rewritten.
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

use std::collections::{BTreeMap, BTreeSet};
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
/// process rather than on every check. Cleared by the next clean write, or by
/// a `remove` whose record deletion lands, never by a clean read: a standing
/// write fault with clean reads would otherwise report again on every check.
static RECORD_FAULT_WARNED: AtomicBool = AtomicBool::new(false);

/// Latch for the state-flock fault line: one hold that cannot be taken — an
/// unresolvable `~/.clauth`, its lock file, or the flock itself — is reported
/// once, and a record write that lands re-arms it, as does a `remove` whose
/// record deletion lands; a hold that writes nothing leaves it set. Its own
/// latch keeps a lock fault from silencing the record fault that may follow
/// it.
static STATE_LOCK_WARNED: AtomicBool = AtomicBool::new(false);

/// The files whose put-back or keep-current rewrite last failed, so a write
/// fault that stays is reported once per process rather than on every check.
/// Per file: a put-back landing in one file must not re-arm the line for
/// another that still fails. A file leaves the set when a rewrite of it lands
/// or a read finds it already current, so a recurrence is reported again.
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
    /// Every spawn `--settings` file a shared launch wrote, each with its
    /// floor: a delegate's own valid number, so a later change to the number
    /// never undercuts it, `None` for an operator launch. The check rewrites
    /// these files when the number changes; a record without the field reads
    /// and writes the same bytes as one with it empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) spawn_files: BTreeMap<PathBuf, Option<u64>>,
}

impl Retention {
    /// The remembered number while a put-back of it waits for the operator's
    /// keep-or-remove answer.
    pub(crate) fn open_question(&self) -> Option<u64> {
        self.days.filter(|_| self.question_open)
    }
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
pub(crate) fn load() -> Result<Retention> {
    read_record(&record_path()?)
}

/// Whether `config_dir` names a shared runtime tree: one whose `projects`
/// links the operator's store. The name check is the whole axis — an isolated
/// tree's dir name marks it before any file is read.
pub(crate) fn is_shared(config_dir: &Path) -> bool {
    config_dir
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(crate::runtime::is_shared_runtime_dir_name)
}

/// The number a launch on `config_dir` may carry on its spawn `--settings`:
/// the operator file's own valid `cleanupPeriodDays`, else the remembered one;
/// `None` for an isolated tree, which links no store and reads neither, and
/// for a shared one with neither. This is the pre-check that keeps a launch
/// that writes no spawn file off the state flock, and the fallback number when
/// the flock cannot be taken; a launch that carries a number decides it fresh
/// under the flock in [`carry_into_spawn_file`]. A record fault carries no
/// number and is reported once.
pub(crate) fn launch_days(config_dir: &Path) -> Option<u64> {
    if !is_shared(config_dir) {
        return None;
    }
    // The operator file's own valid number first, as the tree build keeps it:
    // the record can lag it (a write that failed when the number was raised),
    // and this flag outranks the tree's copy.
    let operator = claude_dir()
        .ok()
        .and_then(|dir| read_member(&dir.join("settings.json")))
        .and_then(|home| home.obj.get(KEY).and_then(valid_days));
    operator.or_else(|| load().unwrap_or_else(|e| record_read_fault(&e)).days)
}

/// What one locked launch-carry ([`carry_into_spawn_file`]) produced.
#[derive(Debug)]
pub(crate) enum CarryOutcome {
    /// The spawn file was written — with the decided number, or without one —
    /// and registered in the record with its floor.
    Carried,
    /// Nothing was written: nothing is registered, and the caller's own
    /// settings stand.
    Nothing,
    /// The spawn-file write failed inside the hold: nothing is registered;
    /// the caller refuses a delegate while the decided number is due, and any
    /// other launch goes on as if no number was carried.
    WriteFailed(std::io::Error),
    /// The state flock could not be taken: nothing is registered, and the
    /// caller writes the file with the number it already read. The fault is
    /// reported once through the state-lock latch.
    LockedOut,
}

/// Decide the number a shared launch carries on its spawn `--settings` and
/// register the file that holds it, in ONE state-flock hold with the write
/// itself: a settle pass landing between the decision and the registration
/// would leave this launch's file at the number the pass just changed, which
/// a respawn would then read. `write` serializes the caller's settings object
/// into `settings_file` under the same hold, with the decided number when one
/// was decided, and returns whether it wrote the file. A shared launch
/// registers its spawn file with or without a number, so a number learned
/// later reaches a repairs-only file too; an isolated tree writes and records
/// nothing. A record fault never refuses the launch: a record that cannot be
/// read or parsed carries no remembered number, registers nothing and is never
/// overwritten; a registration write that fails is reported once through the
/// record-fault latch and the launch goes on.
pub(crate) fn carry_into_spawn_file(
    config_dir: &Path,
    settings_file: &Path,
    floor: Option<u64>,
    write: impl FnOnce(Option<u64>) -> std::io::Result<bool>,
) -> CarryOutcome {
    match with_state_lock(|_held| {
        let number = launch_days(config_dir);
        // Only the name check tells an isolated tree from a shared one with
        // nothing remembered, and an isolated tree carries and records
        // nothing.
        if number.is_none() && !is_shared(config_dir) {
            return Ok(CarryOutcome::Nothing);
        }
        match write(number) {
            // The caller declined: nothing to write, nothing to register.
            Ok(false) => return Ok(CarryOutcome::Nothing),
            Ok(true) => {}
            Err(e) => return Ok(CarryOutcome::WriteFailed(e)),
        }
        register_spawn_file(settings_file, floor);
        Ok(CarryOutcome::Carried)
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            state_lock_fault(format_args!(
                "clauth: {e:#}; the spawn settings file {} is not registered, so it is not \
                 kept current",
                settings_file.display()
            ));
            CarryOutcome::LockedOut
        }
    }
}

/// The caller's own `cleanupPeriodDays` in a settings object: the raw value
/// when the key is present, valid or not; `None` when absent. An operator
/// launch's own value wins over any remembered number and is never
/// registered, so a check never rewrites it.
pub(crate) fn own_retention(map: &Map<String, Value>) -> Option<&Value> {
    map.get(KEY)
}

/// Fold a launch's decided number into the settings object it writes: the
/// caller's own valid number stays when it is at least `days` — a delegate's
/// own value is a floor under the remembered number — and any other own
/// value, an invalid one included, plus an absent key, give way to `days`.
pub(crate) fn apply_launch_days(map: &mut Map<String, Value>, days: u64, own_valid: Option<u64>) {
    if own_valid.is_none_or(|own| own < days) {
        map.insert(KEY.to_owned(), Value::from(days));
    }
}

/// Record `file` as one a shared launch wrote, with its floor: the check
/// rewrites the file when the number changes, and `remove` strips it. Called
/// under the held state flock. A record that cannot be read or parsed
/// registers nothing and is never overwritten; a write that fails is reported
/// once through the record-fault latch and the launch goes on.
fn register_spawn_file(file: &Path, floor: Option<u64>) {
    // Unreachable in production: every caller runs under the held state flock,
    // which resolved this same `~/.clauth` for its lock file, so the record
    // path resolves here too. The skip keeps a launch going if it ever did not.
    let Ok(record) = record_path() else {
        return;
    };
    let mut stored = match read_record(&record) {
        Ok(stored) => stored,
        Err(e) => {
            record_fault(format_args!(
                "clauth: {e:#}; {} is not kept current",
                file.display()
            ));
            return;
        }
    };
    if stored.spawn_files.get(file) == Some(&floor) {
        return;
    }
    stored.spawn_files.insert(file.to_path_buf(), floor);
    let Ok(bytes) = serde_json::to_vec_pretty(&stored) else {
        record_fault(format_args!(
            "clauth: failed to serialize the retention record {}; {} is not kept current",
            record.display(),
            file.display()
        ));
        return;
    };
    match crate::profile::atomic_write_600(&record, bytes) {
        Ok(()) => {
            RECORD_FAULT_WARNED.store(false, Ordering::Relaxed);
            STATE_LOCK_WARNED.store(false, Ordering::Relaxed);
        }
        Err(e) => record_fault(format_args!(
            "clauth: failed to write the retention record {}: {e:#}; {} is not kept current",
            record.display(),
            file.display()
        )),
    }
}

/// The stat snapshot from the last [`reconcile`] pass that settled; the daemon
/// leg's fast-path key.
static LAST_RECONCILED: Mutex<Option<Vec<MemberSnapshot>>> = Mutex::new(None);

/// One check of the operator file and every live shared copy, under the state
/// flock: the daemon's leg, which runs with no `clauth start` session live.
///
/// The check sits behind a stat fast path over the operator file, each live
/// shared copy and the retention record, in the shape of
/// [`crate::jsonsync::run_with_cache`]: a wake whose inputs still read those
/// stats checks nothing. The registered spawn files are read too but are not
/// keyed: only clauth writes them, and a change to the number they follow
/// moves the operator file or the record. Only a pass that skipped no merge,
/// wrote or failed to write nothing, and could read and parse every spawn file
/// it had a number for keys it, so the next wake after any other pass re-reads
/// every file.
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

/// What one [`remove`] pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveOutcome {
    /// The key left every file and the record is gone.
    Removed,
    /// The question moved on before the answer landed: `~/.claude/settings.json`
    /// holds another valid number, or the record's open question is about
    /// another number. Nothing was written.
    Superseded,
}

/// The operator's `remove it` answer to the question about `days`: the key
/// leaves the operator file and every live shared copy, and the number and its
/// question are forgotten.
///
/// The answer names the number the modal asked about. A question that moved on
/// before the answer lands — the operator file holds another valid number, or
/// the record's open question is about another number — changes nothing and
/// returns [`RemoveOutcome::Superseded`]: the operator's own new number is
/// honoured silently, and a stale modal never overrules it.
///
/// The record is read first, before any member is touched: it names the spawn
/// files that still carry the number, so a record that cannot be read or does
/// not parse fails the remove naming it, changing nothing — stripping the key
/// while armed spawn files stay armed would report a removal that never
/// happened. The key goes before the record does, so a write that fails
/// leaves the number remembered and the next pass puts it back rather than
/// losing it. A member Claude Code rewrites while this runs is re-read and
/// retried, at most three tries per member; one that keeps moving fails the
/// whole remove naming that file, with the record untouched, so the caller
/// can say it failed and ask again. A member that exists but cannot be read
/// or does not parse counts as moving too — its key may well still be there,
/// and skipping it would report success while a later merge flows the key
/// back; it is re-read after a pause of at most 100 ms, so a copy caught
/// mid-write has time to land, and the failure names whichever state the last
/// read saw. Only a member that is not there is skipped: nothing to remove;
/// any other stat error fails the remove, for the same reason.
///
/// Each registered spawn file is rewritten to its floor — the key removed
/// when it has none — before the record goes, so a respawn reading one does
/// not re-arm the operator's cutoff. A file that is gone is skipped, and a
/// rewrite that cannot land fails the remove naming that file, with the
/// record kept, like a member that keeps moving.
///
/// # Errors
///
/// A path cannot be resolved, the flock cannot be taken, a member cannot be
/// statted, a write fails, the record cannot be read or does not parse, or a
/// member kept changing or stayed unreadable across three tries.
pub(crate) fn remove(days: u64) -> Result<RemoveOutcome> {
    with_state_lock(|_held| {
        let record = record_path()?;
        let stored = read_record(&record).with_context(|| {
            format!(
                "{} cannot be read or does not parse, so cleanupPeriodDays was not removed; \
                 the key may still be there",
                record.display()
            )
        })?;
        let operator = claude_dir()?.join("settings.json");
        if stored.open_question().is_some_and(|open| open != days) {
            return Ok(RemoveOutcome::Superseded);
        }
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
                    let moved_on = |member: &Member| {
                        member
                            .obj
                            .get(KEY)
                            .and_then(valid_days)
                            .is_some_and(|held| held != days)
                    };
                    // `known_paths` lists the operator file first, so this
                    // read decides before any copy is stripped.
                    if path == operator && moved_on(&member) {
                        return Ok(RemoveOutcome::Superseded);
                    }
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
        for (path, floor) in &stored.spawn_files {
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "failed to read {}, so its cleanupPeriodDays was not removed; \
                             the key may still be there",
                            path.display()
                        )
                    });
                }
            };
            let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(&bytes) else {
                anyhow::bail!(
                    "{} cannot be read or does not parse, so its cleanupPeriodDays was not \
                     removed; the key may still be there",
                    path.display()
                );
            };
            match *floor {
                Some(floor) => {
                    obj.insert(KEY.to_owned(), Value::from(floor));
                }
                None => {
                    obj.shift_remove(KEY);
                }
            }
            let json = Value::Object(obj).to_string();
            crate::profile::atomic_write_600(path, json.as_bytes()).with_context(|| {
                format!(
                    "failed to write {}, so its cleanupPeriodDays was not removed; \
                     the key may still be there",
                    path.display()
                )
            })?;
        }
        match std::fs::remove_file(&record) {
            Ok(()) => {
                RECORD_FAULT_WARNED.store(false, Ordering::Relaxed);
                STATE_LOCK_WARNED.store(false, Ordering::Relaxed);
                Ok(RemoveOutcome::Removed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RemoveOutcome::Removed),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", record.display())),
        }
    })
}

/// The operator's `keep` answer: the question about `days` closes, and the
/// number stays remembered and wherever it was put back. A record whose open
/// question is about another number, or that has none, is left as it is: that
/// question is still owed its own answer.
///
/// # Errors
///
/// A path cannot be resolved, the flock cannot be taken, or the record cannot
/// be read, does not parse or cannot be written.
pub(crate) fn keep(days: u64) -> Result<()> {
    with_state_lock(|_held| {
        let record = record_path()?;
        let mut stored = read_record(&record)?;
        if stored.open_question() != Some(days) {
            return Ok(());
        }
        stored.question_open = false;
        let bytes = serde_json::to_vec_pretty(&stored)
            .with_context(|| format!("failed to serialize {}", record.display()))?;
        crate::profile::atomic_write_600(&record, bytes)
            .with_context(|| format!("failed to write {}", record.display()))?;
        RECORD_FAULT_WARNED.store(false, Ordering::Relaxed);
        STATE_LOCK_WARNED.store(false, Ordering::Relaxed);
        Ok(())
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
    /// Whether this pass wrote a file, failed to, or could not read or parse a
    /// spawn file it keeps current: a settings put-back, a spawn-file rewrite,
    /// a record write. A stat key taken before such a pass describes bytes the
    /// pass replaced, or work still owed, so no caller caches it.
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
/// into it anyway. Registered spawn files follow the decided number here too
/// (the record's `spawn_files`): a gone file drops its entry, and a file with
/// no number to keep in it is left as it is. When there is a number, a file
/// that cannot be read or is not an object keeps its entry, stays as it is,
/// and reports once, and a rewrite that fails keeps the entry and reports
/// once, so a later pass retries; a file found already current clears its
/// fault latch. A file the check had a number for that cannot be read or
/// parsed, and a failed write of any kind, marks the pass
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
    let stored = read_record(&record).unwrap_or_else(|e| record_read_fault(&e));
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
                    next.days = Some(days);
                    next.question_open = false;
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
    // The spawn files a shared launch wrote follow the number here: a
    // background job's `respawnFlags` carry its launch's `--settings` file
    // (measured 2026-10-20, Claude Code 2.1.304), so a respawn reads it again,
    // and one left at an old number sweeps the store at the old cutoff. A
    // file the GC collected drops its entry instead of being written back.
    // With no number to keep in it, a file is read only for that drop and
    // left alone: nothing is owed. With a number, a file that cannot be read
    // or is not a JSON object keeps its entry, stays as it is, is reported
    // once through the rewrite-fault latch, like a failed rewrite, and marks
    // the pass uncacheable, so a later pass retries. A file found already
    // current leaves the latch, so a fault that returns is reported again.
    let mut gone = Vec::new();
    for (path, floor) in &next.spawn_files {
        // The wanted value is the larger of the decided number and the
        // delegate's floor. With neither, the file stays as it is: a check
        // never takes a number away, and a delegate's own value, even one
        // Claude Code's schema refuses, is the caller's; only `remove` strips.
        let wanted = next.days.max(*floor);
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                gone.push(path.clone());
                continue;
            }
            Err(e) => {
                if wanted.is_some() {
                    attempted_write = true;
                    spawn_rewrite_failure(path, &e.into());
                }
                continue;
            }
        };
        let Some(wanted) = wanted.map(Value::from) else {
            continue;
        };
        let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(&bytes) else {
            attempted_write = true;
            spawn_rewrite_failure(path, &anyhow::anyhow!("it does not hold a JSON object"));
            continue;
        };
        if obj.get(KEY) != Some(&wanted) {
            obj.insert(KEY.to_owned(), wanted);
            attempted_write = true;
            // Compact JSON, as the launch wrote the file.
            let json = Value::Object(obj).to_string();
            if let Err(e) = crate::profile::atomic_write_600(path, json.as_bytes()) {
                spawn_rewrite_failure(path, &e.into());
                continue;
            }
        }
        // Current, or a rewrite landed: a fault this file had is over, so one
        // that returns is reported again.
        PUT_BACK_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
    }
    for path in gone {
        next.spawn_files.remove(&path);
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
    match obj.entry(KEY) {
        serde_json::map::Entry::Occupied(_) => Ok(settings),
        serde_json::map::Entry::Vacant(slot) => {
            slot.insert(Value::from(days));
            serde_json::to_string_pretty(&value).context("failed to serialize settings.json")
        }
    }
}

/// The number Claude Code's schema takes: a positive integer, whatever its
/// JSON spelling (`3650`, `3650.0`, `3.65e3` all parse to the integer 3650 in
/// Claude Code's `JSON.parse` + zod `int().positive()` chain, which accepts
/// every integral JSON number).
pub(crate) fn valid_days(value: &Value) -> Option<u64> {
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

/// A record read for a check or a launch: a record that cannot be read or
/// parsed remembers nothing, is reported once through [`record_fault`], and is
/// rewritten once the operator file holds a valid number. The line names what
/// is lost while the operator file holds no number — the put-back and the
/// launch carry — and that sessions already running keep the number they hold.
fn record_read_fault(e: &anyhow::Error) -> Retention {
    record_fault(format_args!(
        "clauth: {e:#}; clauth forgot the cleanupPeriodDays it remembered, so \
         while ~/.claude/settings.json holds no number it is neither put back nor passed to a \
         launch, and sessions already running keep the number they hold now"
    ));
    Retention::default()
}

/// Report one record fault under [`RECORD_FAULT_WARNED`]: the first of a run
/// logs `line`, the rest are silent until a record write or a `remove`'s
/// record deletion lands.
fn record_fault(line: std::fmt::Arguments<'_>) {
    if !RECORD_FAULT_WARNED.swap(true, Ordering::Relaxed) {
        logline!("{line}");
    }
}

/// Report one state-flock fault under [`STATE_LOCK_WARNED`]: the first since
/// a record write or a `remove`'s record deletion last landed logs `line`,
/// the rest are silent.
fn state_lock_fault(line: std::fmt::Arguments<'_>) {
    if !STATE_LOCK_WARNED.swap(true, Ordering::Relaxed) {
        logline!("{line}");
    }
}

/// Clear every fault latch, so a test counting latched lines never reads a
/// latch an earlier test in the same process left set.
#[cfg(test)]
pub(crate) fn reset_latches() {
    RECORD_FAULT_WARNED.store(false, Ordering::Relaxed);
    STATE_LOCK_WARNED.store(false, Ordering::Relaxed);
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

/// Report one failed keep-current rewrite into `path` under
/// [`PUT_BACK_FAILURES`], with its own line: the number was not missing there,
/// only stale, so "put back" would misdescribe the write.
fn spawn_rewrite_failure(path: &Path, e: &anyhow::Error) {
    let first = PUT_BACK_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf());
    if first {
        logline!(
            "clauth: failed to keep cleanupPeriodDays current in {}: {e:#}",
            path.display()
        );
    }
}

/// Write `next` owner-only when it differs from what is on disk. A failure is
/// reported once through [`record_fault`] and never fails the check: the
/// decision this pass already made stands, and a later pass retries the write.
/// A clean write clears both fault latches, so the next fault on either is
/// reported again; a hold that writes nothing clears neither. Returns whether
/// it wrote or failed to, `false` only when the record already said `next`.
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
            STATE_LOCK_WARNED.store(false, Ordering::Relaxed);
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
