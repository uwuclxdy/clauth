#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Probe contract (#27, #57):
//!   * `claim_singleton` caps the daemon tree at one active instance plus one
//!     standby: a third arrival is `Redundant` and exits instead of parking.
//!   * `daemon_health` drives the `[ daemon ]` header chip from two signals —
//!     the `clauthd.lock` flock (presence) and `status.json` freshness (health):
//!     no lock → Absent (dim), held + fresh → Fresh (green), held + stale →
//!     Stale (amber).
//!   * `singleton_held` asks the same presence question as a DECISION rather
//!     than a display: where the chip dims for an unreadable lock, `--status` fails
//!     on it instead of telling a supervisor to spawn.
//!   * `claim_by_replacing` (`--replace`) terminates the running daemon and takes
//!     over, refusing to signal a pid it can't confirm is a running clauth daemon.
//!   * `FetchLease` is the single-fetcher lease over `usage-fetch.lock`: exactly
//!     one holder at a time, held while its ticks complete, released on drop
//!     or by the lease watchdog past `LEASE_STALE_BOUND` so a waiter takes
//!     over.

use super::*;
use crate::profile::clauth_dir;
use crate::testutil::HomeSandbox;
use crate::usage::{epoch_secs_to_iso, now_epoch_secs, now_ms};

fn write_status(generated_at: &str) {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join(super::super::STATUS_FILE),
        format!(r#"{{"schema":1,"generated_at":"{generated_at}","profiles":[]}}"#),
    )
    .expect("write status");
}

// ── status_is_fresh (pure half) ──────────────────────────────────────────────

#[test]
fn fresh_stale_and_garbage_stamps() {
    let now = now_ms();
    let iso_now = epoch_secs_to_iso(now_epoch_secs());
    let fresh = format!(r#"{{"schema":1,"generated_at":"{iso_now}"}}"#);
    assert!(
        status_is_fresh(&fresh, now),
        "a just-written stamp is fresh"
    );

    let iso_old = epoch_secs_to_iso(now_epoch_secs() - 120);
    let stale = format!(r#"{{"schema":1,"generated_at":"{iso_old}"}}"#);
    assert!(!status_is_fresh(&stale, now), "a 2-min-old stamp is stale");

    // Clock skew: a stamp slightly in the future must not flap the probe.
    let iso_future = epoch_secs_to_iso(now_epoch_secs() + 60);
    let future = format!(r#"{{"schema":1,"generated_at":"{iso_future}"}}"#);
    assert!(
        status_is_fresh(&future, now),
        "a future stamp reads as fresh"
    );

    assert!(!status_is_fresh("not json", now));
    assert!(!status_is_fresh(r#"{"schema":1}"#, now), "missing stamp");
    assert!(
        !status_is_fresh(r#"{"generated_at":"yesterday-ish"}"#, now),
        "malformed stamp"
    );
    assert!(
        !status_is_fresh(r#"{"generated_at":12345}"#, now),
        "non-string stamp"
    );
}

/// The staleness window sits strictly above the watchdog deadline
/// (`DAEMON_STALE_MS`'s doc): the worst legal tick — a macOS keychain mirror
/// spending all three `security` deadlines — lands AT the deadline and must
/// read green, so amber means "wedged past what the watchdog tolerates", never
/// "slowest legal tick". `now` is synthesized from the same second the stamps
/// derive from, so both boundaries are exact with no clock race: the fresh
/// half's stamp is exactly one second past the deadline (at the deadline itself
/// a real `now_ms`'s sub-second remainder flips `<=` once in a while), the
/// stale half's exactly one second past the window.
#[test]
fn the_staleness_window_sits_above_the_watchdog_deadline() {
    // The owner-set margin (ruling 2026-09-10), pinned as a figure: a smaller
    // margin survives the const assert and both boundary halves below, since
    // any margin over ~1 s keeps the worst legal tick green.
    assert_eq!(
        super::DAEMON_STALE_MS,
        super::super::WATCHDOG_DEADLINE.as_millis() as u64 + 5_000
    );

    let base = now_epoch_secs();
    let now = base as u64 * 1000;

    let worst_legal_tick = format!(
        r#"{{"schema":1,"generated_at":"{}"}}"#,
        epoch_secs_to_iso(base - super::super::WATCHDOG_DEADLINE.as_secs() as i64 - 1)
    );
    assert!(
        status_is_fresh(&worst_legal_tick, now),
        "a stamp past the watchdog deadline but inside the window is the worst \
         legal tick and must read green"
    );

    let past_window = format!(
        r#"{{"schema":1,"generated_at":"{}"}}"#,
        epoch_secs_to_iso(base - (super::DAEMON_STALE_MS / 1000) as i64 - 1)
    );
    assert!(
        !status_is_fresh(&past_window, now),
        "a stamp past the window must read amber"
    );
}

// ── daemon_health (chip: presence + health) ─────────────────────────────────

#[test]
fn no_lock_file_reads_as_absent() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    assert_eq!(
        daemon_health(),
        DaemonHealth::Absent,
        "fresh status but no lock file ever → chip dimmed"
    );
    // And the probe must not have manufactured the lock file.
    assert!(
        !clauth_dir()
            .expect("dir")
            .join(super::super::LOCK_FILE)
            .exists(),
        "the probe never creates the lock file"
    );
}

#[test]
fn unheld_lock_reads_as_absent() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    // Lock file exists (a daemon ran once) but nobody holds it — died/exited.
    let f = hold_daemon_lock();
    drop(f);
    assert_eq!(
        daemon_health(),
        DaemonHealth::Absent,
        "a released flock means the daemon died → chip dimmed"
    );
}

#[test]
fn held_lock_with_fresh_status_is_fresh() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    let _held = hold_daemon_lock();
    assert_eq!(daemon_health(), DaemonHealth::Fresh, "up + fresh → green");
}

#[test]
fn held_lock_with_stale_or_missing_status_is_stale() {
    let _home = HomeSandbox::new();
    let _held = hold_daemon_lock();
    assert_eq!(
        daemon_health(),
        DaemonHealth::Stale,
        "up but no status.json yet → amber"
    );
    write_status(&epoch_secs_to_iso(now_epoch_secs() - 300));
    assert_eq!(
        daemon_health(),
        DaemonHealth::Stale,
        "up but a wedged holder's stale stamp → amber"
    );
}

// ── claim_singleton (one active + one standby, #57) ───────────────────────────

/// `~/.clauth` inside the sandbox, created so the lock files have a home.
fn sandbox_dir() -> std::path::PathBuf {
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// One claim attempt, no retry — the role decision on its own. The production
/// [`claim_singleton`] retries past a transient probe hold, which
/// `a_transient_probe_hold_never_forces_an_exit` covers separately.
fn claim_now(dir: &std::path::Path, standby: bool) -> Claim {
    claim_singleton_with(dir, standby, 1, std::time::Duration::ZERO).expect("claim")
}

/// How long the fake probe below keeps holding once a test ARMS its release. A
/// literal, deliberately NOT derived from [`CLAIM_RETRY`]: a hold that scales
/// with the constant under test shrinks with it, so the fixture could never
/// catch the retry window collapsing. It has to outlast the first attempt, so a
/// one-shot claim loses the race, and the full window has to clear it, so the
/// retry wins — the first holds by construction (the release is armed
/// immediately before the retried call), the second is pinned below.
///
/// The sleep runs INSIDE the probe thread (not a separate release thread), so
/// the locks drop the instant it wakes — no inter-thread channel hop between
/// the timer and the `drop`. That hop was the macOS debug-leg flake: a 3-core
/// runner under load scheduled the release thread's wakeup past the retry
/// margin, so all three attempts saw the probe holding.
const PROBE_HOLD: std::time::Duration = std::time::Duration::from_millis(60);
// The shipped schedule must outlive PROBE_HOLD, or the recovery this test proves
// cannot happen and it reds for a fixture reason instead of a real one.
// `as_millis` because Duration's comparisons are not const — it also floors a
// sub-millisecond CLAIM_RETRY to 0, where production's `!is_zero()` would pass.
const _: () = assert!(
    PROBE_HOLD.as_millis() < CLAIM_RETRY.as_millis() * (CLAIM_ATTEMPTS as u128 - 1),
    "the retry window no longer outlives PROBE_HOLD: a starting daemon would spend every \
     attempt inside a probe's hold and exit for good. Retune PROBE_HOLD with the schedule, \
     never silence this."
);

/// Take both singleton flocks the way a presence probe does, hold them for
/// `hold`, then let go. Returns once both are held, so the caller's clock starts
/// inside the hold: a claim that gets no second attempt is still inside it.
///
/// Stronger than either real probe on purpose — `daemon_health` releases the
/// singleton lock before `standby_waiting` opens the slot file, so nothing in
/// production holds both at once. Read it as a worst case, not as a model of the
/// probes.
fn probe_holding_both(dir: &std::path::Path) -> ProbeHold {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<std::time::Duration>();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let a = crate::profile::open_state_file(&dir.join(super::super::LOCK_FILE)).expect("open");
        a.try_lock().expect("probe takes the free singleton lock");
        let s = crate::profile::open_state_file(&dir.join(super::super::STANDBY_LOCK_FILE))
            .expect("open");
        s.try_lock().expect("probe takes the free standby lock");
        held_tx.send(()).expect("signal held");
        let hold = release_rx.recv().unwrap_or(std::time::Duration::ZERO);
        std::thread::sleep(hold);
    });
    held_rx.recv().expect("probe reports both flocks held");
    ProbeHold {
        release: release_tx,
        handle,
    }
}

/// A probe sitting on both flocks until told to release.
///
/// The probe thread receives the hold duration from [`release_after`] and sleeps
/// in-thread, so the locks drop the instant the sleep ends — no separate release
/// thread to schedule. The duration is armed at the retried call (not when the
/// probe took the locks), so a control running under the hold cannot expire it,
/// and the margin is whatever the caller arms.
struct ProbeHold {
    release: std::sync::mpsc::Sender<std::time::Duration>,
    handle: std::thread::JoinHandle<()>,
}

impl ProbeHold {
    /// Let go `after` from NOW. Call it immediately before the retried operation
    /// under test, so the margin is measured from that call and not from however
    /// long the assertions before it took.
    fn release_after(self, after: std::time::Duration) -> std::thread::JoinHandle<()> {
        let _ = self.release.send(after);
        self.handle
    }
}

#[test]
fn third_instance_is_redundant_and_the_promoted_standby_frees_the_slot() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();

    let Claim::Active(active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };
    assert_eq!(
        holder_pid(),
        Some(std::process::id()),
        "the holder stamps its pid so a `ps` dump names the live one"
    );

    let Claim::Standby(slot) = claim_now(&dir, true) else {
        panic!("the second instance takes the one standby slot");
    };
    assert!(
        standby_waiting(),
        "the parked instance is visible to a probe"
    );
    assert!(
        matches!(claim_now(&dir, true), Claim::Redundant),
        "a third instance exits instead of parking — this is the #57 pile-up"
    );

    // A decoy pid in the sidecar, planted while the holder is still up: this
    // process would stamp its own pid either way, so without it a promotion that
    // never re-stamps reads as correct. Written to PID_FILE, not LOCK_FILE —
    // the active handle holds LOCK_FILE's mandatory flock on Windows, so a
    // foreign write there would fault, and the pid no longer lives there anyway.
    std::fs::write(dir.join(super::super::PID_FILE), b"999\n").expect("plant a decoy pid");

    // The daemon exits: the standby takes over, off a thread so a promotion
    // that never unblocks fails the test instead of wedging the suite.
    drop(active);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(slot.promote());
    });
    let _promoted = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the standby promotes once the holder exits")
        .expect("promote");

    assert_eq!(
        holder_pid(),
        Some(std::process::id()),
        "the takeover re-stamps the sidecar, so the decoy pid can't outlive the handover"
    );
    assert!(
        !standby_waiting(),
        "promotion releases the slot: the takeover must not leave it held"
    );
    assert!(
        matches!(claim_now(&dir, true), Claim::Standby(_)),
        "the freed slot is available to the next arrival"
    );
}

#[test]
fn no_standby_exits_rather_than_taking_the_free_slot() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let Claim::Active(_active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };

    assert!(
        matches!(claim_now(&dir, false), Claim::Redundant),
        "--no-standby never queues, even with the slot free"
    );
    assert!(
        !dir.join(super::super::STANDBY_LOCK_FILE).exists(),
        "--no-standby never even creates the slot file"
    );
}

/// Both presence probes take the flock they test and release it microseconds
/// later, so one lost try-lock is not proof of a daemon. A `Redundant` decided
/// on a reader would exit a supervisor's instance for good.
///
/// Runs against the SHIPPED [`claim_singleton`] rather than an injected
/// schedule: `CLAIM_ATTEMPTS`/`CLAIM_RETRY` are what the recovery is made of,
/// and a test that supplies its own numbers stays green with the production
/// window collapsed back to one attempt.
#[test]
fn a_transient_probe_hold_never_forces_an_exit() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let probe = probe_holding_both(&dir);

    // Positive control: a single attempt reads the probe as a live daemon plus a
    // live standby, which is exactly the wrong answer the retry exists to undo.
    // Runs under an unconditional hold, so it cannot race the release.
    assert!(
        matches!(claim_now(&dir, true), Claim::Redundant),
        "one attempt cannot tell a probe from a holder"
    );

    let probe = probe.release_after(PROBE_HOLD);
    let claim = claim_singleton(&dir, true).expect("claim");
    assert!(
        matches!(claim, Claim::Active(_)),
        "the shipped retry window outlives a probe's hold instead of exiting for good"
    );
    probe.join().expect("probe thread");
}

/// The retry is for a `Redundant` verdict alone. Dropping a won standby slot to
/// re-take it hands the one seat back for most of the window and delays a
/// takeover that already has its answer.
#[test]
fn a_won_standby_slot_is_kept_rather_than_re_taken() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let Claim::Active(_active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };

    let attempts = claim_attempts(&dir);
    let claim = claim_singleton(&dir, true).expect("claim");

    assert!(
        matches!(claim, Claim::Standby(_)),
        "the second instance parks in the standby slot"
    );
    assert_eq!(
        claim_attempts(&dir),
        attempts + 1,
        "a won slot must stand: the claim re-tested an answer it already had instead \
         of returning it"
    );
    assert!(
        standby_waiting(),
        "the slot reads as taken the moment the claim returns, never re-opened by a retry"
    );
}

/// `clauth daemon --status` decides on `singleton_held`, not on the header chip:
/// a lock it cannot read has to surface as an error there, since a `--status ||
/// spawn` supervisor respawns on the chip's dim state ("no daemon"). These are the three
/// answers a sandbox can produce — the io-error arm needs a filesystem without
/// working locks.
#[test]
fn singleton_held_separates_a_missing_lock_a_free_one_and_a_held_one() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    assert!(
        !singleton_held().expect("a missing lock file is an answer, not a failure"),
        "no lock file ever → no daemon has started here"
    );
    assert!(
        !dir.join(super::super::LOCK_FILE).exists(),
        "the probe never creates the lock file"
    );

    let Claim::Active(active) = claim_now(&dir, true) else {
        panic!("the first instance takes the singleton lock");
    };
    assert!(
        singleton_held().expect("a held lock"),
        "a held lock is a running daemon"
    );

    drop(active);
    assert!(
        !singleton_held().expect("a released lock"),
        "a released flock means the holder died → no daemon, still not an error"
    );
}

// ── claim_by_replacing (--replace, #57) ───────────────────────────────────────

/// `--replace` with nothing running is just a normal start: it takes the free
/// lock rather than erroring on "no daemon to replace".
#[test]
fn replace_starts_clean_when_no_daemon_is_running() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let claim = claim_by_replacing_with(
        &dir,
        std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(1),
        std::time::Duration::from_millis(10),
    )
    .expect("replace with no daemon just starts");
    assert!(
        matches!(claim, Claim::Active(_)),
        "no holder to replace → take the lock like any first start"
    );
}

/// A running daemon whose pid can't be read (a torn or absent sidecar) is never
/// signalled: `--replace` bails and leaves the kill to the operator.
#[test]
fn replace_refuses_a_holder_whose_pid_is_unreadable() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let _held = hold_daemon_lock(); // holds the lock, stamps no pid sidecar
    let err = claim_by_replacing_with(
        &dir,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
    )
    .expect_err("an unreadable pid must not be signalled");
    assert!(
        err.to_string().contains("unreadable"),
        "the bail flags the unreadable pid, got {err}"
    );
}

/// The identity guard refuses to signal a pid that is not a running `clauth
/// daemon` (the recycled / in-handover window). pid 1 (init/systemd) is a
/// never-a-daemon stand-in; asserting the bail proves the guard fired before any
/// signal reached it. Unix-only: it reads argv, which the Windows probe can't.
#[cfg(unix)]
#[test]
fn replace_refuses_a_pid_that_is_not_the_running_daemon() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let _held = hold_daemon_lock();
    std::fs::write(dir.join(super::super::PID_FILE), "1\n").expect("stamp pid 1");
    let err = claim_by_replacing_with(
        &dir,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
    )
    .expect_err("a pid that is not the running daemon must not be signalled");
    assert!(
        err.to_string().contains("not a running clauth daemon"),
        "the bail flags the pid as not the daemon, got {err}"
    );
}

/// `--replace`'s wait claims the lock the moment a holder releases it — the poll
/// loop bridges the gap between a killed daemon's death and its flock
/// auto-releasing. Removing the loop reds this: the first attempt still reads
/// the lock held.
#[test]
fn the_replace_wait_claims_once_the_holder_releases() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let held = hold_daemon_lock();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        drop(held);
    });
    let lock = poll_until(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(10),
        || claim_active(&dir),
    );
    assert!(
        lock.is_some(),
        "the wait must poll until the freed flock is claimable"
    );
    releaser.join().expect("releaser thread");
}

/// The wait is bounded: a holder that never releases makes it time out and
/// return None rather than block forever, so `--replace` can escalate
/// (SIGTERM → SIGKILL on unix) and, past that, give up with an error.
#[test]
fn the_replace_wait_times_out_while_the_lock_stays_held() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let _held = hold_daemon_lock(); // never released
    let started = std::time::Instant::now();
    let lock = poll_until(
        std::time::Duration::from_millis(80),
        std::time::Duration::from_millis(10),
        || claim_active(&dir),
    );
    assert!(
        lock.is_none(),
        "a never-released lock must time out, not block"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(80),
        "it waited the full window before giving up"
    );
}

/// A transient reader of the singleton lock (TUI header chip at 1 Hz,
/// `clauth daemon --status`) holds the flock for microseconds and releases it.
/// Without retry, `claim_by_replacing_with`'s fast path reads this as a daemon,
/// falls through to `holder_pid` (which returns `None` for a reader with no pid
/// sidecar), and bails naming a daemon that isn't there. The retry clears it.
#[test]
fn replace_retries_past_a_transient_lock_reader() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();

    // A transient probe holds the singleton lock briefly, then releases.
    let probe = probe_holding_both(&dir);

    // Positive control: a one-shot (no retry) reads the probe as a held daemon
    // and fails because the transient reader has no pid sidecar. Runs under an
    // unconditional hold, so it cannot race the release.
    let err = claim_by_replacing_retry_with(
        &dir,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
        1,
        std::time::Duration::ZERO,
    )
    .expect_err("one-shot reads the transient probe as a daemon and bails on the missing pid");
    assert!(
        err.to_string().contains("unreadable"),
        "the bail names the unreadable pid, got {err}"
    );

    // With the production retry schedule: the probe clears by the second
    // attempt and `--replace` takes the free lock as a clean start.
    let probe = probe.release_after(PROBE_HOLD);
    let claim = claim_by_replacing_with(
        &dir,
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
    )
    .expect("replace retries past the transient probe");
    assert!(
        matches!(claim, Claim::Active(_)),
        "the retry cleared the transient and claimed the lock"
    );

    probe.join().expect("probe thread");
}

/// The force-kill pass runs its own window, separate from the SIGTERM pass:
/// a holder that outlives both (a fake daemon that ignores SIGTERM, and a
/// successor that keeps the singleton held after the kill) makes the stop bail
/// only after the kill wait, so the bail names `kill_wait`, not `wait`. A
/// regression to `kill_wait == wait` reds on the elapsed-time floor.
#[cfg(unix)]
#[test]
fn replace_force_kill_wait_is_separate_from_the_term_wait() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let work = tempfile::tempdir().expect("tempdir");
    let done = stand_in(fake_daemon(work.path(), true), true);
    let wait = std::time::Duration::from_millis(50);
    let kill_wait = std::time::Duration::from_secs(1);
    let started = std::time::Instant::now();
    let err = claim_by_replacing_with(&dir, wait, kill_wait, std::time::Duration::from_millis(10))
        .expect_err("a holder that outlives both passes bails on the force-kill wait");
    assert!(
        err.to_string().contains("within 1s of the force kill"),
        "the bail names the force-kill window, got {err}"
    );
    assert!(
        started.elapsed() >= kill_wait,
        "the force-kill pass waited its own window, not the term pass's"
    );
    assert!(
        started.elapsed() < wait + 2 * kill_wait,
        "the force-kill pass did not exceed its own window"
    );
    let held = done.recv().expect("holder thread");
    drop(held); // release the singleton the stop saw as wedged
}

// ── stop_running (the TUI's `stop daemon`) ───────────────────────────────────

/// A process `pid_is_clauth_daemon` accepts: argv `clauth daemon`, where
/// `daemon` is a script in `dir` that sh runs by that name. `ignore_term`
/// makes it survive SIGTERM, so only the SIGKILL pass ends it.
#[cfg(unix)]
fn fake_daemon(dir: &std::path::Path, ignore_term: bool) -> std::process::Child {
    use std::os::unix::process::CommandExt as _;
    let trap = if ignore_term { "trap '' TERM\n" } else { "" };
    std::fs::write(
        dir.join("daemon"),
        format!("{trap}while :; do sleep 0.05; done\n"),
    )
    .expect("write the fake daemon's script");
    std::process::Command::new("/bin/sh")
        .arg0("clauth")
        .arg("daemon")
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn the fake daemon")
}

/// Stand the fake in as the running daemon: stamp its pid, and hold the
/// singleton in-process for exactly its lifetime (a real daemon's flock
/// releases when it dies). With `successor` the singleton stays held past the
/// death, the way a parked standby takes it at once; the handle comes back on
/// the returned channel so the test can keep it held while it asserts. It is
/// the same handle, never a re-acquire: a re-acquire could collide with the
/// stop's own probe, and its gap would read as no successor.
#[cfg(unix)]
fn stand_in(
    mut fake: std::process::Child,
    successor: bool,
) -> std::sync::mpsc::Receiver<Option<std::fs::File>> {
    std::fs::write(
        sandbox_dir().join(super::super::PID_FILE),
        format!("{}\n", fake.id()),
    )
    .expect("stamp the fake's pid");
    let held = hold_daemon_lock();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = fake.wait();
        let _ = tx.send(successor.then_some(held));
    });
    rx
}

/// The retry schedule the stop tests pass: the in-process holder drops the
/// lock microseconds after the fake is reaped, well inside one retry.
const STOP_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

#[test]
fn stop_with_no_daemon_signals_nothing() {
    let _home = HomeSandbox::new();
    let _ = sandbox_dir();
    let stopped = stop_running_with(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        STOP_RETRY,
    )
    .expect("a free lock is no error");
    assert_eq!(stopped, DaemonStop::NotRunning);
}

/// SIGTERM ends a daemon that honours it, and the box is left with no daemon:
/// the stop never claims the singleton for itself.
#[cfg(unix)]
#[test]
fn stop_terminates_the_daemon_and_leaves_the_singleton_free() {
    let _home = HomeSandbox::new();
    let work = tempfile::tempdir().expect("tempdir");
    let done = stand_in(fake_daemon(work.path(), false), false);
    let started = std::time::Instant::now();
    let stopped = stop_running_with(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        STOP_RETRY,
    )
    .expect("the stop lands");
    assert_eq!(stopped, DaemonStop::Stopped);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "SIGTERM alone ended it: no wait ran out before the escalation"
    );
    assert!(done.recv().expect("holder thread").is_none());
    assert!(
        !singleton_held().expect("lock readable"),
        "the stop left the singleton free, not held by this process"
    );
}

/// A daemon that survives SIGTERM gets the escalation `--replace` sends.
#[cfg(unix)]
#[test]
fn stop_escalates_to_sigkill_past_the_wait() {
    let _home = HomeSandbox::new();
    let work = tempfile::tempdir().expect("tempdir");
    let done = stand_in(fake_daemon(work.path(), true), false);
    let wait = std::time::Duration::from_millis(300);
    let started = std::time::Instant::now();
    let stopped = stop_running_with(
        wait,
        wait,
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        STOP_RETRY,
    )
    .expect("the escalation lands");
    assert_eq!(stopped, DaemonStop::Stopped);
    assert!(
        started.elapsed() >= wait,
        "the first pass waited out its window before escalating"
    );
    assert!(done.recv().expect("holder thread").is_none());
}

/// A parked standby takes the singleton the instant the daemon dies. The stop
/// reports that, and returns on the death rather than sitting out both passes
/// waiting for a free lock that never comes.
#[cfg(unix)]
#[test]
fn stop_names_a_standby_that_took_over() {
    let _home = HomeSandbox::new();
    let work = tempfile::tempdir().expect("tempdir");
    let done = stand_in(fake_daemon(work.path(), false), true);
    let wait = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();
    let stopped = stop_running_with(
        wait,
        wait,
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        STOP_RETRY,
    )
    .expect("the stop lands");
    let successor = done.recv().expect("holder thread");
    assert_eq!(stopped, DaemonStop::Replaced);
    assert!(
        started.elapsed() < wait,
        "the stop returned on the death, not after a wait ran out"
    );
    drop(successor);
}

/// A gateway the stopped daemon left running (Windows always: its daemon ends
/// by `taskkill /F`; unix after the SIGKILL pass) is asked to stop at once,
/// with its deadline recorded for a later daemon, rather than left to a next
/// start that may never come. A successor that took the singleton reclaims it
/// itself, so then nothing is signalled.
#[cfg(unix)]
#[test]
fn a_stop_leaving_no_daemon_stops_the_gateway_it_left() {
    use super::super::gateway::{ChildMarker, process_start_time, read_marker, write_marker};
    for successor in [false, true] {
        let _home = HomeSandbox::new();
        let work = tempfile::tempdir().expect("tempdir");
        let mut gateway = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn the stand-in gateway");
        let marker = ChildMarker {
            pid: gateway.id(),
            start: process_start_time(gateway.id()),
            stop_bound_secs: 12,
            stop_deadline_ms: None,
        };
        assert!(marker.start.is_some(), "the stand-in's start time reads");
        write_marker(&marker).expect("marker");
        let done = stand_in(fake_daemon(work.path(), false), successor);
        let before = crate::usage::now_ms();
        let stopped = stop_running_with(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(10),
            CLAIM_ATTEMPTS,
            STOP_RETRY,
        )
        .expect("the stop lands");
        let next = done.recv().expect("holder thread");
        let exited = poll_until(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(20),
            || gateway.try_wait().expect("try_wait"),
        );
        let left = read_marker().expect("read");
        if successor {
            assert_eq!(stopped, DaemonStop::Replaced);
            assert_eq!(exited, None, "the successor's start owns the reclaim");
            assert_eq!(left, Some(marker), "the marker is the successor's to read");
            let _ = gateway.kill();
            let _ = gateway.wait();
        } else {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(stopped, DaemonStop::Stopped);
            assert_eq!(
                exited.and_then(|s| s.signal()),
                Some(libc::SIGTERM),
                "one SIGTERM, so shunt drains"
            );
            let deadline = left.and_then(|m| m.stop_deadline_ms).expect("a deadline");
            assert!(
                (before + 12_000..=crate::usage::now_ms() + 12_000).contains(&deadline),
                "the deadline is the stop bound from the ask: {deadline}"
            );
        }
        drop(next);
    }
}

/// A marker-only proxy orphan — a live pid recorded in a child marker alone,
/// no supervisor running — is stopped by the daemon-stop path, not only by a
/// direct `stop_left_behind_proxies` call: `stop daemon` runs the same sweep
/// once no daemon is left, and the orphan exits by its one SIGTERM.
#[cfg(unix)]
#[test]
fn a_stop_leaving_no_daemon_stops_a_marker_only_proxy_orphan() {
    let _home = HomeSandbox::new();
    let work = tempfile::tempdir().expect("tempdir");
    let service = crate::proxy::Service::parse("zcode").expect("a service");
    crate::profile::mkdir_700(&crate::proxy::state_dir(&service).expect("state dir"))
        .expect("state dir");
    let mut orphan = crate::testutil::Owned(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn the orphan"),
    );
    let pid = orphan.0.id();
    let start = crate::daemon::gateway::process_start_time(pid).expect("start time");
    let marker = crate::daemon::gateway::ChildMarker {
        pid,
        start: Some(start),
        stop_bound_secs: 5,
        stop_deadline_ms: None,
    };
    std::fs::write(
        crate::proxy::child_marker_path(&service).expect("marker path"),
        serde_json::to_vec(&marker).expect("json"),
    )
    .expect("write the marker");

    let done = stand_in(fake_daemon(work.path(), false), false);
    let stopped = stop_running_with(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        STOP_RETRY,
    )
    .expect("the stop lands");
    assert_eq!(stopped, DaemonStop::Stopped);
    assert!(done.recv().expect("holder thread").is_none());

    let exited = poll_until(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(20),
        || orphan.0.try_wait().expect("try_wait"),
    );
    assert_eq!(
        crate::daemon::gateway::ExitReport::from(exited.expect("exited")).signal,
        Some(15),
        "one SIGTERM"
    );
}

/// A transient reader of the singleton (the TUI header's 1 Hz probe,
/// `clauth daemon --status`) must not read as a daemon to stop: with no
/// retry the stop would go after a pid sidecar no reader has.
#[test]
fn stop_retries_past_a_transient_lock_reader() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let probe = probe_holding_both(&dir);
    let err = stop_running_with(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
        1,
        std::time::Duration::ZERO,
    )
    .expect_err("one attempt reads the probe as a daemon");
    assert!(err.to_string().contains("unreadable"), "got {err}");

    let probe = probe.release_after(PROBE_HOLD);
    let stopped = stop_running_with(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        CLAIM_RETRY,
    )
    .expect("the retry clears the probe");
    assert_eq!(stopped, DaemonStop::NotRunning);
    probe.join().expect("probe thread");
}

/// The same identity guard as `--replace`: a recorded pid that is not a
/// running clauth daemon is never signalled.
#[test]
fn stop_refuses_a_pid_that_is_not_the_running_daemon() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let _held = hold_daemon_lock();
    std::fs::write(dir.join(super::super::PID_FILE), "1\n").expect("stamp pid 1");
    let err = stop_running_with(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(10),
        CLAIM_ATTEMPTS,
        STOP_RETRY,
    )
    .expect_err("pid 1 is not the daemon");
    assert_eq!(
        err.to_string(),
        "the recorded daemon pid 1 is not a running clauth daemon (it exited during handover \
         or was recycled); re-run once it settles, or kill the daemon manually"
    );
}

// ── FetchLease (single-fetcher lease over usage-fetch.lock) ───────────────────

#[test]
fn one_holder_at_a_time_and_a_waiter_takes_the_freed_lease() {
    let _home = HomeSandbox::new();

    // First instance wins the lease and holds it.
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    assert!(
        a.acquire(),
        "a held lease is idempotent — still the fetcher"
    );

    // A second instance over the SAME file is denied → it must stand down.
    let b = FetchLease::new();
    assert!(
        !b.acquire(),
        "two lease holders never both fetch — the second stands down"
    );

    // The holder exits (its File drops → flock released). The waiter's next
    // acquire wins.
    drop(a);
    assert!(
        b.acquire(),
        "on the holder's exit the waiter takes over the freed lease"
    );
}

#[test]
fn an_unreadable_lock_stands_down() {
    let _home = HomeSandbox::new();
    // Make `usage-fetch.lock` a directory so the lease can never open it as a
    // file — the acquire must fail closed (stand down), never dup-fetch.
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(dir.join(super::super::FETCH_LOCK_FILE)).expect("mkdir lockpath");
    let lease = FetchLease::new();
    assert!(
        !lease.acquire(),
        "an unopenable lock file stands down rather than fetching"
    );
}

const LEASE_STALE_BOUND: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[test]
fn a_holder_whose_ticks_stop_drops_the_lease_and_a_waiter_takes_it() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    let t0 = 1_000_000_u64;
    a.stamp_tick(t0);
    let generation = a.generation();

    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let dropped = a.release_if_stale(t0 + 5 * 60 * 1000 + 1, LEASE_STALE_BOUND);
    drop(capture);

    assert!(dropped, "a stamp older than the bound drops the lease");
    assert_ne!(
        a.generation(),
        generation,
        "the drop moves the generation a resumed tick re-checks"
    );
    assert!(!a.fetches_here(), "the dropped holder no longer fetches");
    assert!(
        clauth_dir()
            .expect("clauth dir")
            .join(super::super::FETCH_LOCK_FILE)
            .is_file(),
        "the drop never unlinks the lock file"
    );
    let b = FetchLease::new();
    assert!(b.acquire(), "a waiter takes the dropped lease");
    let logged = lines.snapshot();
    let pid = std::process::id().to_string();
    assert_eq!(
        logged.iter().filter(|l| l.contains(&pid)).count(),
        1,
        "one line names the holder's pid: {logged:?}"
    );
}

#[test]
fn a_holder_whose_last_tick_is_within_the_bound_keeps_the_lease() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    let t0 = 1_000_000_u64;
    a.stamp_tick(t0);
    let generation = a.generation();

    assert!(
        !a.release_if_stale(t0 + 5 * 60 * 1000, LEASE_STALE_BOUND),
        "a stamp exactly at the bound keeps the lease"
    );
    assert_eq!(
        a.generation(),
        generation,
        "a kept lease keeps its generation"
    );
    let b = FetchLease::new();
    assert!(!b.acquire(), "the waiter still stands down");
}

#[test]
fn a_take_stamps_the_clock_so_a_fresh_holder_is_never_stale() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    assert!(
        !a.release_if_stale(crate::usage::now_ms(), LEASE_STALE_BOUND),
        "a holder that has run no tick yet is stamped at its take"
    );
    let b = FetchLease::new();
    assert!(!b.acquire(), "the waiter still stands down");
}

/// The bound the shipped watchdog judges on is five minutes: the tests around
/// this one stamp a local copy, so a changed production constant would otherwise
/// move only live behaviour.
#[test]
fn the_lease_stale_bound_is_the_shipped_five_minutes() {
    assert_eq!(
        super::LEASE_STALE_BOUND,
        std::time::Duration::from_secs(300),
        "the lease watchdog releases a holder five minutes after its last completed tick"
    );
}

const LEASE_WATCHDOG_POLL: std::time::Duration = std::time::Duration::from_secs(10);
const LEASE_BOUND_MS: u64 = 5 * 60 * 1000;

#[test]
fn the_lease_watchdog_drops_a_holder_stalled_past_the_bound() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    let t0 = 1_000_000_u64;
    a.stamp_tick(t0);
    let now = t0 + LEASE_BOUND_MS + 1;

    lease_watchdog_step(
        &a,
        LeaseWatch::default(),
        now - 10_000,
        now,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );

    let b = FetchLease::new();
    assert!(
        b.acquire(),
        "a poll that ran its whole window past a stamp 5 min + 1 ms old frees the lease"
    );
}

#[test]
fn the_lease_watchdog_keeps_a_holder_whose_gap_a_suspend_explains() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    let t0 = 1_000_000_u64;
    a.stamp_tick(t0);
    let generation = a.generation();
    // The poll began 1 s after the stamp and woke 6 min later: everything past
    // its 10 s window is a suspend, so the tick thread had 1 s of running time.
    let now = t0 + 6 * 60 * 1000 + 1_000;

    lease_watchdog_step(
        &a,
        LeaseWatch::default(),
        t0 + 1_000,
        now,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );

    assert_eq!(a.generation(), generation, "a suspend moves no generation");
    let b = FetchLease::new();
    assert!(!b.acquire(), "a suspended holder keeps the lease");
}

#[test]
fn a_suspend_before_the_last_tick_does_not_stretch_the_lease_bound() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    let t0 = 1_000_000_u64;
    a.stamp_tick(t0);
    // One hour asleep, then a tick completes on resume.
    let woke = t0 + 60 * 60 * 1000;
    let watch = lease_watchdog_step(
        &a,
        LeaseWatch::default(),
        t0,
        woke,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );
    a.stamp_tick(woke);
    // The tick thread then stalls; a normal poll lands 5 min + 1 ms later.
    let now = woke + LEASE_BOUND_MS + 1;

    lease_watchdog_step(
        &a,
        watch,
        now - 10_000,
        now,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );

    let b = FetchLease::new();
    assert!(
        b.acquire(),
        "the hour asleep before the last tick buys the stall after it nothing"
    );
}

/// The poll that holds a suspend also holds the first tick after the wake: the
/// total charged to the stamp is only the wall time since it, so the stall
/// after it is dropped one poll after its bound, not one suspend later.
#[test]
fn a_suspend_inside_the_poll_that_holds_the_wake_tick_does_not_stretch_the_lease_bound() {
    let _home = HomeSandbox::new();
    let a = FetchLease::new();
    assert!(a.acquire(), "first caller becomes the fetcher");
    let t0 = 1_000_000_u64;
    a.stamp_tick(t0);
    // One hour asleep; the poll wakes 10 s after the wake, and the first tick
    // after it completed 4 s into those 10 s.
    let woke = t0 + 60 * 60 * 1000;
    let stamp = woke + 4_000;
    a.stamp_tick(stamp);
    let watch = lease_watchdog_step(
        &a,
        LeaseWatch::default(),
        t0,
        woke + 10_000,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );
    let generation = a.generation();

    // The tick thread then wedges. A poll at exactly the bound keeps the lease.
    let at_bound = stamp + LEASE_BOUND_MS;
    let watch = lease_watchdog_step(
        &a,
        watch,
        at_bound - 10_000,
        at_bound,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );
    assert_eq!(
        a.generation(),
        generation,
        "a stamp at the bound keeps the lease"
    );

    // Two polls later the stamp is 20 s + 1 ms past the bound, 6 s of it
    // charged to the wake poll: the lease goes.
    let late = at_bound + 20_001;
    lease_watchdog_step(
        &a,
        watch,
        late - 10_000,
        late,
        LEASE_WATCHDOG_POLL,
        LEASE_STALE_BOUND,
    );

    let b = FetchLease::new();
    assert!(
        b.acquire(),
        "the hour asleep inside the wake poll buys the stall after the stamp nothing"
    );
}

// ── E2E `serve` stop ─────────────────────────────────────────────────────────

/// `--replace`'s termination (the same `stop_running_with` the TUI's
/// `stop daemon` sends) waits out a token spend before its `REPLACE_WAIT`
/// window ends, so the SIGKILL escalation never fires: the child that was
/// SIGTERMed mid-save dies of SIGTERM and its minted pair lands in the store.
/// The child is the re-entered `serve` child (`daemon::tests::…`), posed with
/// argv `clauth daemon` so `pid_is_clauth_daemon` accepts its pid.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn replace_waits_out_an_inflight_token_save_without_escalating() {
    let stopped = crate::testutil::serve_stop_e2e(
        |_pid, home| {
            // `stop_running_with` reads the singleton lock and pid sidecar
            // through the home override, so point it at the child's own home
            // for the duration.
            crate::profile::set_home_override(home.to_path_buf());
            std::thread::spawn(|| {
                stop_running_with(
                    REPLACE_WAIT,
                    KILL_WAIT,
                    REPLACE_POLL,
                    CLAIM_ATTEMPTS,
                    CLAIM_RETRY,
                )
            })
        },
        std::time::Duration::from_secs(2),
        "the escalation never fired: the child still died of SIGTERM",
    );
    assert_eq!(
        stopped.expect("the stop lands"),
        DaemonStop::Stopped,
        "no standby took over the lock"
    );
}
