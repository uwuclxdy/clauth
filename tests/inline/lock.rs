use super::*;

use std::sync::{Arc, Barrier};
use std::time::Instant;

/// Two threads entering `with_state_lock` simultaneously must serialize their
/// closures — no two intervals may overlap.
#[test]
fn cross_thread_with_state_lock_serializes() {
    // Sandbox-pinned: the lock path resolves through the process-global home
    // override, and without holding the sandbox lock a concurrently-running
    // sandboxed test can swap that override mid-test — two of the threads
    // below would then flock DIFFERENT files and legitimately overlap
    // (observed as a rare parallel-run flake, 2026-07-09).
    let _home = crate::testutil::HomeSandbox::new();
    const THREADS: usize = 4;
    let barrier = Arc::new(Barrier::new(THREADS));
    let intervals = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let epoch = Instant::now();

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let intervals = Arc::clone(&intervals);
            std::thread::spawn(move || {
                // All threads rendezvous here to maximize concurrent entry.
                barrier.wait();
                with_state_lock(|_held| {
                    let start = epoch.elapsed().as_nanos() as u64;
                    // Sleep widens the interval so overlaps are detectable.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    let end = epoch.elapsed().as_nanos() as u64;
                    intervals.lock().unwrap().push((start, end));
                    Ok(())
                })
                .expect("with_state_lock failed");
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let intervals = intervals.lock().unwrap();
    assert_eq!(
        intervals.len(),
        THREADS,
        "each thread must record one interval"
    );

    // [a_start, a_end) and [b_start, b_end) overlap when a_start < b_end && b_start < a_end.
    for i in 0..intervals.len() {
        for j in (i + 1)..intervals.len() {
            let (a_start, a_end) = intervals[i];
            let (b_start, b_end) = intervals[j];
            assert!(
                a_end <= b_start || b_end <= a_start,
                "intervals overlap: [{a_start}, {a_end}) and [{b_start}, {b_end})"
            );
        }
    }
}

/// A wedged flock under one tick's flag costs the tick one wait, never once
/// per landing/kick hold: three holds sharing ONE `timed_out` flag — the first
/// waits out the bound and sets the flag, the other two skip without waiting.
/// Count, never wall time: exactly one hold reports a NONZERO `waited` (the one
/// that timed out); the skips report zero. The holds run SEQUENTIALLY, exactly
/// as the drain applies landings in completion order — the flag only skips a
/// hold that STARTS after an earlier hold already timed out.
#[test]
fn a_wedged_flock_waits_once_per_flag() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    let timed_out = AtomicBool::new(false);

    let wait_of = |timed_out: &AtomicBool| {
        crate::lock::with_state_lock_flag(Some(timed_out), |_h| Ok::<(), anyhow::Error>(()))
            .expect_err("a wedged flock times out")
            .downcast::<crate::lock::StateLockTimeout>()
            .expect("the refusal is a state-lock timeout")
            .waited
    };

    let first = wait_of(&timed_out);
    let second = wait_of(&timed_out);
    let third = wait_of(&timed_out);
    drop(holder);
    crate::lock::set_state_lock_timeout_override(None);

    assert!(
        first > Duration::ZERO,
        "the first hold waits out the bound, got {first:?}"
    );
    assert_eq!(
        (second, third),
        (Duration::ZERO, Duration::ZERO),
        "once one hold timed out the later holds skip without waiting"
    );
    assert!(
        timed_out.load(Ordering::Relaxed),
        "a timed-out wait sets the flag"
    );
}

/// W1: two CONCURRENT holds sharing one tick's flag under a wedged flock take
/// one timed-out wait, never one each. The first hold parks inside its flock
/// wait (a seam, holding the in-process `THREAD_LOCK`); the second starts only
/// once the first is parked, so its pre-check runs while the flag is still
/// unset and it queues on `THREAD_LOCK` behind the first. It must re-read the
/// flag after winning `THREAD_LOCK` and skip without waiting — otherwise it
/// waits its own full bound (2 waits). Ordered by channel, never wall time.
#[test]
fn a_wedged_flock_costs_two_concurrent_holds_one_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let _home = crate::testutil::HomeSandbox::new();
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");

    let flag = AtomicBool::new(false);
    crate::lock::WAITED_ACQUISITIONS.store(0, Ordering::Relaxed);
    let hold = |flag: &AtomicBool| {
        let _ = crate::lock::with_state_lock_flag(Some(flag), |_h| Ok::<(), anyhow::Error>(()));
    };
    std::thread::scope(|s| {
        // Park the first hold inside its flock wait: it holds THREAD_LOCK there
        // and has not yet timed out, so the flag is still unset.
        let park = crate::lock::flock_wait_park::arm();
        s.spawn(|| hold(&flag));
        park.parked
            .recv()
            .expect("the first hold parks inside its flock wait");
        // The second starts only once the first is parked, so its pre-check
        // runs before the first times out and sets the flag; it then queues on
        // THREAD_LOCK and must skip on the re-check.
        s.spawn(|| hold(&flag));
        park.release.send(()).expect("release the first hold");
    });

    assert_eq!(
        crate::lock::WAITED_ACQUISITIONS.load(Ordering::Relaxed),
        1,
        "two concurrent holds sharing one tick flag must take one timed-out wait"
    );
    assert!(
        flag.load(Ordering::Relaxed),
        "a timed-out wait sets the flag"
    );

    drop(holder);
    crate::lock::set_state_lock_timeout_override(None);
}

/// Y1: the flag-skip-when-set pre-check lets a flagged hold skip `THREAD_LOCK`
/// entirely once the flag is already set, so it is not delayed by an UNFLAGGED
/// waiter (a ride-along anchor write, a rotation persist) mid-way through a
/// wedged flock wait holding `THREAD_LOCK`. The waiter is parked by a seam,
/// never a sleep; the flagged hold's return is received on a channel BEFORE the
/// waiter is released, so the order is asserted by the channel, not wall time.
/// Without the pre-check (Z1) the hold queues on `THREAD_LOCK` behind the
/// parked waiter and nothing arrives.
#[test]
fn a_flagged_hold_skips_a_wedged_unflagged_waiter_holding_thread_lock() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Duration;

    let _home = crate::testutil::HomeSandbox::new();
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");

    // An UNFLAGGED waiter enters its flock wait and parks there holding the
    // process-wide THREAD_LOCK.
    let park = crate::lock::flock_wait_park::arm();
    let waiter = std::thread::spawn(|| {
        let _ = crate::lock::with_state_lock(|_h| Ok::<(), anyhow::Error>(()));
    });
    park.parked
        .recv()
        .expect("the unflagged waiter parks inside its flock wait");

    // The tick's flag is already set (an earlier hold timed out).
    let flag = AtomicBool::new(true);

    // A later flagged hold must return its timeout while the waiter STILL holds
    // THREAD_LOCK: with the pre-check it skips THREAD_LOCK at once, without it
    // it queues behind the parked waiter and nothing is ever sent.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let flagged = std::thread::spawn(move || {
        let err = crate::lock::with_state_lock_flag(Some(&flag), |_h| Ok::<(), anyhow::Error>(()))
            .expect_err("a flagged hold with the flag set takes the timeout path");
        let _ = done_tx.send(());
        err.downcast::<crate::lock::StateLockTimeout>()
            .expect("the refusal is a state-lock timeout");
    });

    struct ReleaseOnDrop(mpsc::Sender<()>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    // Panic-safe release: on any exit (the receive below failing included) the
    // parked waiter still unparks, so a red never leaks a thread holding
    // THREAD_LOCK into the rest of the run. The receive returns at once on the
    // green path — the timeout only ever bounds the red path, never orders it.
    {
        let _release = ReleaseOnDrop(park.release.clone());
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the flagged hold did not return while the unflagged waiter still held THREAD_LOCK: the pre-check is missing");
    }

    flagged.join().expect("flagged hold");
    waiter.join().expect("unflagged waiter");

    drop(holder);
    crate::lock::set_state_lock_timeout_override(None);
}

/// A wait that SUCCEEDS — a peer holds the flock for a moment inside the
/// bound, then frees it — sets no flag, so the next hold still waits normally
/// rather than skipping (cloudy 2026-10-17; ruling test (d)).
#[test]
fn a_wait_that_succeeds_sets_no_flag() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;
    let _home = crate::testutil::HomeSandbox::new();
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    let (locked_tx, locked_rx) = mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        holder.lock().expect("hold the flock");
        locked_tx.send(()).expect("signal the hold");
        std::thread::sleep(Duration::from_millis(200));
        drop(holder);
    });
    locked_rx.recv().expect("the peer holds the flock");

    let timed_out = AtomicBool::new(false);
    crate::lock::with_state_lock_flag(Some(&timed_out), |_h| Ok::<(), anyhow::Error>(()))
        .expect("the first hold waits out the brief peer hold and succeeds");
    assert!(
        !timed_out.load(Ordering::Relaxed),
        "a wait that succeeds sets no flag"
    );
    // The flock is free again: the next hold still waits normally (never skips),
    // so it acquires.
    crate::lock::with_state_lock_flag(Some(&timed_out), |_h| Ok::<(), anyhow::Error>(()))
        .expect("the next hold waits normally, never skips");
    peer.join().expect("peer");
    crate::lock::set_state_lock_timeout_override(None);
}

/// N concurrent waiters under a peer hold that ends INSIDE the bound all get
/// through, and none of them sets the flag — a successful wait sets nothing, so
/// the flag never drops a waiter (the old deadline's per-thread-sum bug, now
/// impossible because there is no deadline). The count is the number of waiters
/// that acquired (all four).
#[test]
fn concurrent_waiters_under_a_hold_inside_the_deadline_all_get_through() {
    let ok = concurrent_waiters_through(600);
    assert_eq!(ok, 4, "a hold inside the bound drops no waiter");
}

/// The control: a short hold lets every waiter through and still sets no flag.
#[test]
fn a_short_hold_lets_every_waiter_through() {
    assert_eq!(concurrent_waiters_through(100), 4);
}

fn concurrent_waiters_through(release_after_ms: u64) -> usize {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;
    let _home = crate::testutil::HomeSandbox::new();
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    let (locked_tx, locked_rx) = mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        holder.lock().expect("hold the flock");
        locked_tx.send(()).expect("signal the hold");
        std::thread::sleep(Duration::from_millis(release_after_ms));
        drop(holder);
    });
    locked_rx.recv().expect("the peer holds the flock");

    let timed_out = AtomicBool::new(false);
    let ok = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                if crate::lock::with_state_lock_flag(Some(&timed_out), |_h| {
                    Ok::<(), anyhow::Error>(())
                })
                .is_ok()
                {
                    ok.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert!(
        !timed_out.load(Ordering::Relaxed),
        "a hold that ends inside the bound sets no flag"
    );
    peer.join().expect("peer");
    crate::lock::set_state_lock_timeout_override(None);
    ok.load(Ordering::SeqCst)
}

/// Same-thread nested `with_state_lock` calls must not deadlock.
#[test]
fn same_thread_reentrancy_does_not_deadlock() {
    let _home = crate::testutil::HomeSandbox::new();
    let result =
        with_state_lock(|_held| with_state_lock(|_held| with_state_lock(|_held| Ok(42u32))));
    assert_eq!(result.unwrap(), 42);
}

/// A panic inside the closure unwinds through `StateLock::Drop`, which closes
/// the flock `File` and releases `THREAD_LOCK` (poisoning it). The next
/// acquisition must recover via `into_inner()`, observe the cleared slot, and
/// re-flock — the lock must not be permanently wedged.
#[test]
fn poison_recovery_after_panicking_closure() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let _home = crate::testutil::HomeSandbox::new();

    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _guard = StateLock::acquire().expect("acquire before panic");
        panic!("closure blew up while holding the state lock");
    }));
    assert!(panicked.is_err(), "the inner closure must have panicked");

    // DEPTH resets to 0 — Drop ran during unwind.
    DEPTH.with(|d| assert_eq!(d.get(), 0, "depth must reset to 0 after unwind"));

    // So does the subprocess budget. A spent budget surviving the unwind would
    // strangle the very recovery path below, on a thread nothing is waiting on.
    let huge = Duration::from_secs(3600);
    assert_eq!(
        clamp_to_hold_budget(huge),
        huge,
        "the panicking hold must release its subprocess budget while unwinding"
    );

    // THREAD_LOCK poisoned + slot None; fresh acquire must recover and re-flock.
    let result = with_state_lock(|_held| Ok(7u32));
    assert_eq!(result.unwrap(), 7, "lock must be reusable after a panic");

    // Reentrancy must still work post-recovery.
    let again = with_state_lock(|_held| with_state_lock(|_held| Ok(8u32)));
    assert_eq!(again.unwrap(), 8, "reentrancy still works post-recovery");
}

/// The subprocess budget belongs to the HOLD, so it binds inside one and nothing
/// outside one. A caller with no state lock blocks no peer (`oauth.rs` mirrors a
/// rotation after its lock closure ends), so clamping it would cost a deadline
/// with nobody to spend it on.
#[test]
fn the_subprocess_budget_binds_only_inside_a_hold() {
    let _home = crate::testutil::HomeSandbox::new();
    // Larger than any real deadline, so the clamp is the only thing that can
    // shrink it and the assertions read the budget rather than the base.
    let huge = Duration::from_secs(3600);

    assert_eq!(
        clamp_to_hold_budget(huge),
        huge,
        "outside a hold there is nothing to bound"
    );

    with_state_lock(|_held| {
        let inside = clamp_to_hold_budget(huge);
        assert!(
            inside <= SUBPROCESS_BUDGET,
            "a hold caps its subprocess work at SUBPROCESS_BUDGET, got {inside:?}"
        );
        assert!(
            inside > Duration::ZERO,
            "a fresh hold must start with budget to spend, got {inside:?}"
        );
        Ok(())
    })
    .expect("hold");

    assert_eq!(
        clamp_to_hold_budget(huge),
        huge,
        "releasing the outermost hold releases its budget"
    );
}

/// The daemon's tick arms ONE budget its two sequential drains share. The
/// pre-fix shape armed a fresh budget per acquisition, so a tick draining a
/// queued switch and a queued switch-off got 20 s apiece against the daemon's
/// 30 s watchdog. Every acquisition inside the shared scope must spend the
/// SHARED window rather than a fresh one, and must not clear it on release —
/// the second iteration is the drain the first one's budget must still bind.
#[test]
fn a_shared_budget_spans_sequential_acquisitions() {
    let _home = crate::testutil::HomeSandbox::new();
    // Short enough that a fresh 20 s budget and the shared remnant differ by
    // an assertable margin; long enough for two flock round-trips.
    let shared = Duration::from_millis(500);

    let wide = SharedSubprocessBudget::arm(shared);
    for i in 0..2 {
        with_state_lock(|_held| Ok(())).expect("hold");
        let left = clamp_to_hold_budget(Duration::from_secs(20));
        assert!(
            left <= shared,
            "acquisition {i} inside the shared scope must spend the shared window, got {left:?}"
        );
    }
    drop(wide);

    // The shared guard disarms what it armed, so an unscoped hold is back on
    // a full budget of its own.
    with_state_lock(|_held| {
        let inside = clamp_to_hold_budget(Duration::from_secs(20));
        assert!(
            inside > Duration::from_secs(19),
            "after the shared scope drops, a fresh hold arms its own full budget, got {inside:?}"
        );
        Ok(())
    })
    .expect("hold");
}

/// A `SharedSubprocessBudget` taken INSIDE an already-budgeted scope adopts
/// that scope's budget and clears nothing: the ownership chain stays single
/// however the scopes nest, so the inner guard's drop leaves the wider
/// scope's budget armed for the rest of its body. Pinned against BOTH
/// failure directions with a SHORT outer budget: a full-budget assert cannot
/// tell an adopted budget from a cleared one, since an unscoped clamp reads
/// the full 20 s base either way.
#[test]
fn a_shared_scope_inside_a_hold_adopts_its_budget() {
    let _home = crate::testutil::HomeSandbox::new();
    // Short enough that an adopted remnant and a cleared-then-rearmed full
    // budget differ by an assertable margin.
    let shared = Duration::from_millis(500);

    let wide = SharedSubprocessBudget::arm(shared);
    with_state_lock(|_held| {
        {
            let _inner = SharedSubprocessBudget::arm(Duration::from_millis(10));
        }
        // The inner guard did not arm, so its drop cleared nothing; the wide
        // scope's budget is still live and the hold inside it keeps spending it.
        let inside = clamp_to_hold_budget(Duration::from_secs(20));
        assert!(
            inside <= shared,
            "an adopting inner guard must not end the wider scope's budget, got {inside:?}"
        );
        Ok(())
    })
    .expect("hold");
    drop(wide);
}
/// The budget is armed by the OUTERMOST acquisition alone. A reentrant hold that
/// re-armed would hand each nested frame a full budget, which is exactly the
/// shape this bounds: the two Keychain mirrors of a first-login-adopting switch
/// reach `security` through nested `with_state_lock` frames, so a per-frame
/// budget would bound neither of them together.
#[test]
fn a_reentrant_hold_keeps_spending_the_outer_budget() {
    let _home = crate::testutil::HomeSandbox::new();
    let huge = Duration::from_secs(3600);

    // Asserted as a MARGIN, never as `inner < outer`. Both readings are one
    // deadline minus a different `now`, so a re-arming mutant lands them within
    // nanoseconds of each other and a bare `<` passes on whichever way the noise
    // fell — measured surviving a "re-arm on every acquisition" mutation. The
    // sleep is what the correct code must visibly spend and the mutant cannot.
    const SPENT: Duration = Duration::from_millis(50);

    with_state_lock(|_held| {
        let outer = clamp_to_hold_budget(huge);
        std::thread::sleep(SPENT);
        with_state_lock(|_held| {
            let inner = clamp_to_hold_budget(huge);
            let spent = outer.saturating_sub(inner);
            assert!(
                spent >= SPENT,
                "a reentrant acquisition must keep spending the outer hold's budget, not \
                 reset it: {SPENT:?} of sleep moved it only {spent:?} \
                 (outer {outer:?}, inner {inner:?})"
            );
            Ok(())
        })
    })
    .expect("nested hold");
}

/// The cross-process flock wait is bounded. With `~/.clauth/.lock` already held
/// (here by a second, independent open file description — `flock(2)` locks are
/// per-description, so this conflicts exactly as a second process would), an
/// acquisition times out with a [`StateLockTimeout`] instead of hanging; once the
/// holder releases, the next acquisition runs its closure. Both directions of the
/// #35 wedge fix.
#[test]
fn held_flock_times_out_then_recovers_on_release() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir ~/.clauth");
    let lock_path = dir.join(LOCK_FILENAME);

    // Stand in for a second process holding the state lock.
    let holder = crate::profile::open_state_file(&lock_path).expect("open holder handle");
    holder.lock().expect("hold the flock");

    // Direction 1: a held flock times out at the deadline, never hangs.
    let deadline = std::time::Duration::from_millis(300);
    let start = Instant::now();
    let err = match StateLock::acquire_with_timeout(deadline) {
        Ok(_) => panic!("acquisition must time out while the flock is held"),
        Err(e) => e,
    };
    let waited = start.elapsed();
    assert!(
        err.downcast_ref::<StateLockTimeout>().is_some(),
        "a held flock must surface as StateLockTimeout, got: {err:#}"
    );
    assert!(
        waited >= deadline,
        "must wait the full deadline before timing out, waited {waited:?}"
    );
    assert!(
        waited < deadline * 10,
        "must return at the deadline, not hang, waited {waited:?}"
    );

    // Direction 2: once the holder releases, the next acquisition succeeds.
    drop(holder);
    let ran = with_state_lock(|_held| Ok(1234u32)).expect("acquire after the holder releases");
    assert_eq!(ran, 1234, "closure runs once the flock is free");
}

/// Flock waits spend the armed budget like the shell-outs do: an acquisition
/// inside a `SharedSubprocessBudget::arm_clamped` scope is clamped to what the
/// window leaves, never handed a fresh full [`STATE_LOCK_TIMEOUT`]. The daemon's two
/// drains take one acquisition each, so without the clamp a tick against a
/// wedged holder waits 2 × `STATE_LOCK_TIMEOUT` (50 s) past the 30 s
/// `WATCHDOG_DEADLINE`. The 1 s lock-timeout override keeps the broken wait
/// observable in ~1 s instead of the real 25 s; the 300 ms budget is the bound
/// that must win.
#[test]
fn an_armed_budget_clamps_the_flock_wait() {
    let _home = crate::testutil::HomeSandbox::new();
    set_state_lock_timeout_override(Some(std::time::Duration::from_secs(1)));
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir ~/.clauth");
    let holder =
        crate::profile::open_state_file(&dir.join(LOCK_FILENAME)).expect("open holder handle");
    holder.lock().expect("hold the flock");

    let _wide = SharedSubprocessBudget::arm_clamped(std::time::Duration::from_millis(300));
    let start = Instant::now();
    let err = match StateLock::acquire() {
        Ok(_) => panic!("acquisition must time out while the flock is held"),
        Err(e) => e,
    };
    let waited = start.elapsed();
    assert!(
        err.downcast_ref::<StateLockTimeout>().is_some(),
        "a held flock must surface as StateLockTimeout, got: {err:#}"
    );
    assert!(
        waited >= std::time::Duration::from_millis(250),
        "the clamped wait must still spend the window, not fail instantly, waited {waited:?}"
    );
    assert!(
        waited < std::time::Duration::from_millis(500),
        "the flock wait must be clamped to the armed budget (300 ms), not the full lock \
         timeout, waited {waited:?}"
    );

    // The window the wait spent is gone: a second acquisition is handed no
    // wait at all (immediate StateLockTimeout), never a fresh full one.
    let second_start = Instant::now();
    let err2 = match StateLock::acquire() {
        Ok(_) => panic!("a spent window must hand the second acquisition no wait"),
        Err(e) => e,
    };
    assert!(
        err2.downcast_ref::<StateLockTimeout>().is_some(),
        "the second acquisition must time out, got: {err2:#}"
    );
    assert!(
        second_start.elapsed() < std::time::Duration::from_millis(100),
        "a spent window hands the next acquisition no wait, waited {:?}",
        second_start.elapsed()
    );

    // The window bounds the WAIT, never the hold: a freed flock still acquires
    // instantly past a spent window.
    drop(holder);
    let ran = with_state_lock(|_held| Ok(1234u32)).expect("acquire after the holder releases");
    assert_eq!(ran, 1234, "closure runs once the flock is free");

    set_state_lock_timeout_override(None);
}

/// The negative twin of the clamp: a scope armed via the plain
/// [`SharedSubprocessBudget::arm`] keeps the full [`STATE_LOCK_TIMEOUT`] for
/// its flock wait, never clamping it to its own shorter budget. The macOS GC
/// sweep arms a budget for its own shell-outs while a legit slow switch can
/// hold the flock ~20 s; its waiter must survive that hold, not
/// false-time-out at its own budget. A 300 ms lock
/// timeout against a 50 ms budget makes the wrong clamp observable in ~50 ms
/// instead of the real 25 s/20 s.
#[test]
fn a_plain_budget_does_not_clamp_the_flock_wait() {
    let _home = crate::testutil::HomeSandbox::new();
    set_state_lock_timeout_override(Some(std::time::Duration::from_millis(300)));
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir ~/.clauth");
    let holder =
        crate::profile::open_state_file(&dir.join(LOCK_FILENAME)).expect("open holder handle");
    holder.lock().expect("hold the flock");

    // Budget shorter than the lock timeout: a clamped wait would time out at
    // ~50 ms; the correct full wait sits at ~300 ms.
    let _plain = SharedSubprocessBudget::arm(std::time::Duration::from_millis(50));
    let start = Instant::now();
    let err = match StateLock::acquire() {
        Ok(_) => panic!("acquisition must time out while the flock is held"),
        Err(e) => e,
    };
    assert!(
        err.downcast_ref::<StateLockTimeout>().is_some(),
        "a held flock must surface as StateLockTimeout, got: {err:#}"
    );
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(250),
        "a plain budget must keep the full flock wait, not clamp it to the budget, waited {:?}",
        start.elapsed()
    );

    drop(holder);
    set_state_lock_timeout_override(None);
}

/// Keychain legs reached past a peer's state-flock hold keep their whole
/// window: [`with_keychain_budget`] arms at its own call, after the caller's
/// flock wait returned, and runs the closure under that budget, which ends
/// with it.
#[test]
fn keychain_legs_past_a_flock_wait_run_under_their_whole_budget() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("clauth dir"))
        .expect("mkdir ~/.clauth");
    let budget = Duration::from_secs(20);
    let held = Duration::from_millis(1500);
    let holder = crate::testutil::hold_state_flock();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(held);
        drop(holder);
    });
    let _ = take_keychain_leg_windows();

    let start = Instant::now();
    with_state_lock(|_held| Ok(())).expect("the flock wait the legs follow");
    let waited = start.elapsed();
    let inside = with_keychain_budget(budget, armed_budget_remaining);
    releaser.join().expect("releaser");

    assert!(waited >= held, "the caller waited out the peer: {waited:?}");
    let windows = take_keychain_leg_windows();
    assert_eq!(windows.len(), 1, "one Keychain leg scope");
    let noted = windows[0].expect("the legs begin under a budget");
    assert!(
        noted > budget - Duration::from_millis(500),
        "the flock wait spent the Keychain budget: {noted:?} left"
    );
    let inside = inside.expect("the closure runs under the budget");
    assert!(
        inside > budget - Duration::from_millis(500) && inside <= budget,
        "the closure's window is the whole budget: {inside:?}"
    );
    assert_eq!(
        armed_budget_remaining(),
        None,
        "the budget ends with the legs"
    );
}

/// A state-flock acquisition that starts inside a Keychain budget scope
/// refuses in debug builds: its wait would spend the window the scope's
/// `security` calls share, whichever caller opened the scope.
#[cfg(debug_assertions)]
#[test]
#[should_panic(
    expected = "a state-flock acquisition started inside a Keychain budget scope (`lock::with_keychain_budget`): its wait spends the window the scope's `security` calls share, so take the flock before the scope opens"
)]
fn a_state_flock_taken_inside_a_keychain_budget_scope_refuses() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("clauth dir"))
        .expect("mkdir ~/.clauth");
    with_keychain_budget(Duration::from_secs(20), || with_state_lock(|_held| Ok(())))
        .expect("the flock inside the scope");
}

/// Every place under `src/` that arms a Keychain budget, pinned to its file:
/// the census (`keychain.rs`), [`with_keychain_budget`] itself (`lock.rs`) and
/// the GC sweep and a profile delete's tree walk (`runtime.rs`) arm one
/// directly, the daemon tick
/// (`daemon/tick.rs`) arms the clamped window, and the helper's three callers are
/// the landing (`oauth.rs`), `owned_keychain_leg` (`runtime.rs`) and the
/// bare-item read (`claude.rs`). The item
/// legs and their callers are macOS-only code no linux run reaches, and a bare
/// arm opens no helper scope the flock check could see, so a new arm anywhere
/// (above a caller's flock wait in another file included) reds here instead.
/// Each line is cut at its first `//`, as `no_bare_print_macro_under_src`
/// cuts it, so a comment naming a call never counts; an aliased type
/// (`use .. SharedSubprocessBudget as B`) is not seen, and an arm moved
/// within its file keeps the count.
#[test]
fn every_keychain_budget_arm_under_src_is_a_known_site() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = crate::testutil::rs_files(&src);
    assert!(
        files.len() > 10,
        "the scan found {} files under src/ — it is not reading the crate",
        files.len()
    );
    let mut sites: std::collections::BTreeMap<&str, std::collections::BTreeMap<String, usize>> =
        std::collections::BTreeMap::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read a source file");
        let rel = file
            .strip_prefix(&src)
            .expect("a file under src/")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        for call in [
            "SharedSubprocessBudget::arm(",
            "SharedSubprocessBudget::arm_clamped(",
            "with_keychain_budget(",
        ] {
            let n: usize = text
                .lines()
                .map(|line| {
                    line.split("//")
                        .next()
                        .unwrap_or_default()
                        .matches(call)
                        .count()
                })
                .sum();
            if n > 0 {
                *sites
                    .entry(call)
                    .or_default()
                    .entry(rel.clone())
                    .or_default() += n;
            }
        }
    }
    let known = |entries: &[(&str, usize)]| -> std::collections::BTreeMap<String, usize> {
        entries
            .iter()
            .map(|(f, n)| ((*f).to_string(), *n))
            .collect()
    };
    assert_eq!(
        sites,
        std::collections::BTreeMap::from([
            (
                "SharedSubprocessBudget::arm(",
                known(&[("keychain.rs", 1), ("lock.rs", 1), ("runtime.rs", 2)]),
            ),
            (
                "SharedSubprocessBudget::arm_clamped(",
                known(&[("daemon/tick.rs", 1)]),
            ),
            (
                "with_keychain_budget(",
                known(&[("claude.rs", 1), ("oauth.rs", 1), ("runtime.rs", 1)]),
            ),
        ]),
        "a Keychain budget armed outside its known sites: a per-session item leg arms \
         only inside `owned_keychain_leg`, a landing only through `with_keychain_budget`; \
         the census, GC sweep, delete walk and daemon tick arm above their holds by design, and no \
         other site arms"
    );
}

/// A Keychain budget scope opened inside a state-flock hold refuses in debug
/// builds: its `security` calls would run under the flock every peer waits on.
#[cfg(debug_assertions)]
#[test]
#[should_panic(
    expected = "a Keychain budget scope (`lock::with_keychain_budget`) was opened inside a state-flock hold: its `security` calls would run under the flock every peer waits on; open the scope after the hold returns"
)]
fn a_keychain_budget_scope_opened_inside_a_state_flock_hold_refuses() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("clauth dir"))
        .expect("mkdir ~/.clauth");
    with_state_lock(|_held| {
        with_keychain_budget(Duration::from_secs(20), || ());
        Ok(())
    })
    .expect("the scope inside the hold");
}

/// `state_lock_held` reads this thread's hold, at any depth.
#[test]
fn state_lock_held_follows_this_threads_hold() {
    let _home = crate::testutil::HomeSandbox::new();
    assert!(!state_lock_held());
    with_state_lock(|_held| {
        assert!(state_lock_held());
        with_state_lock(|_inner| {
            assert!(state_lock_held());
            Ok(())
        })
    })
    .expect("hold");
    assert!(!state_lock_held());
}
