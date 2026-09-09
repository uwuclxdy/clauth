//! Probe contract (#27, #57):
//!   * `claim_singleton` caps the daemon tree at one active instance plus one
//!     standby: a third arrival is `Redundant` and exits instead of parking.
//!   * `daemon_health` drives the `● daemon` header dot from two signals —
//!     the `clauthd.lock` flock (presence) and `status.json` freshness (health):
//!     no lock → Absent (hidden), held + fresh → Fresh (green), held + stale →
//!     Stale (amber).
//!   * `singleton_held` asks the same presence question as a DECISION rather
//!     than a display: where the dot hides an unreadable lock, `--status` fails
//!     on it instead of telling a supervisor to spawn.
//!   * `claim_by_replacing` (`--replace`) terminates the running daemon and takes
//!     over, refusing to signal a pid it can't confirm is a running clauth daemon.
//!   * `FetchLease` is the single-fetcher lease over `usage-fetch.lock`: exactly
//!     one holder at a time, held for life, released on drop so a waiter takes
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

// ── daemon_health (dot: presence + health) ───────────────────────────────────

#[test]
fn no_lock_file_reads_as_absent() {
    let _home = HomeSandbox::new();
    write_status(&epoch_secs_to_iso(now_epoch_secs()));
    assert_eq!(
        daemon_health(),
        DaemonHealth::Absent,
        "fresh status but no lock file ever → dot hidden"
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
        "a released flock means the daemon died → dot hidden"
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

/// `clauth daemon --status` decides on `singleton_held`, not on the header dot:
/// a lock it cannot read has to surface as an error there, since a `--status ||
/// spawn` supervisor respawns on the dot's "no daemon". These are the three
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
        std::time::Duration::from_millis(10),
    )
    .expect_err("a pid that is not the running daemon must not be signalled");
    assert!(
        err.to_string().contains("not a running clauth daemon"),
        "the bail flags the pid as not the daemon, got {err}"
    );
}

/// `wait_for_active` claims the lock the moment a holder releases it — the poll
/// loop bridges the gap between a killed daemon's death and its flock
/// auto-releasing. Removing the loop reds this: the first attempt still reads
/// the lock held.
#[test]
fn wait_for_active_claims_once_the_holder_releases() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let held = hold_daemon_lock();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        drop(held);
    });
    let lock = wait_for_active(
        &dir,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(10),
    );
    assert!(
        lock.is_some(),
        "wait_for_active must poll until the freed flock is claimable"
    );
    releaser.join().expect("releaser thread");
}

/// The wait is bounded: a holder that never releases makes `wait_for_active`
/// time out and return None rather than block forever, so `--replace` can
/// escalate SIGTERM → SIGKILL and, past that, give up with an error.
#[test]
fn wait_for_active_times_out_while_the_lock_stays_held() {
    let _home = HomeSandbox::new();
    let dir = sandbox_dir();
    let _held = hold_daemon_lock(); // never released
    let started = std::time::Instant::now();
    let lock = wait_for_active(
        &dir,
        std::time::Duration::from_millis(80),
        std::time::Duration::from_millis(10),
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

/// A transient reader of the singleton lock (TUI header dot at 1 Hz,
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
        std::time::Duration::from_millis(10),
    )
    .expect("replace retries past the transient probe");
    assert!(
        matches!(claim, Claim::Active(_)),
        "the retry cleared the transient and claimed the lock"
    );

    probe.join().expect("probe thread");
}

// ── FetchLease (single-fetcher lease over usage-fetch.lock) ───────────────────

#[test]
fn one_holder_at_a_time_and_a_waiter_takes_the_freed_lease() {
    let _home = HomeSandbox::new();

    // First instance wins the lease and holds it for life.
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
