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
