#![allow(clippy::unwrap_used, clippy::expect_used)]

//! F1 (filed 2026-08-14): a detached background delegate still running when
//! `HomeSandbox` tears down must never touch the operator's REAL `$HOME`.
//! `launch_background_delegate` detaches via `tokio::task::spawn_blocking`
//! with no handle the caller keeps, so nothing joined it before
//! `HOME_OVERRIDE` cleared. `HomeSandbox::drop` now blocks on
//! `testutil::register_background_task`'s completion signal for the same
//! reason it already joins `tui::TEST_WORKERS`.

use super::*;
use crate::testutil::HomeSandbox;
use std::time::Duration;

/// Drive one detached background delegate directly — skipping `delegate()`'s
/// own pre-flight, which isn't part of the race under test — against a
/// profile name that exists in neither the sandboxed nor the real config.
/// `run_delegate` fails fast at `load_config().find` regardless of which
/// `$HOME` it resolves; only the WRITE LOCATION of the resulting `done` job
/// file differs between the two, which is exactly the leak this test is
/// pinned on.
#[test]
fn detached_task_still_running_at_teardown_never_touches_the_real_home() {
    let profile = format!("clauth-f1-leak-probe-{}", std::process::id());
    let home = HomeSandbox::new();

    // Resolved while `HOME_TEST_LOCK` is still held, when no other test's env
    // pin can be live. `FakeClaude` pins `$HOME`, which `dirs::home_dir()`
    // reads first: resolving after `drop(home)` could read a concurrent
    // test's sandbox home and false-green the probe.
    let real_home = dirs::home_dir().expect("resolve real home for the probe");

    // Arm the gate only after `home` holds `HOME_TEST_LOCK`: a single global
    // slot shared by every test, so arming it earlier could gate some other
    // test's unrelated background task instead of this one.
    let release_gate = arm_detach_gate();

    let reserved = reserve_background_job(&profile, None, None, true, None, Isolation::Shared)
        .expect("reserve background job");
    let job_id = reserved.spec.job_id.clone();
    // `spawn_blocking` needs an entered Tokio runtime; the runtime itself must
    // outlive the spawn (dropping it can wait on outstanding blocking tasks,
    // which would fold its own join into this test's timing), so it's kept
    // alive for the rest of the function rather than dropped right after.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        launch_background_delegate(
            profile.clone(),
            BackgroundOpts {
                prompt: std::sync::Arc::from("hello"),
                model: None,
                cwd: None,
                env: HashMap::new(),
                extra_args: Vec::new(),
                timeout_secs: None,
                idle_secs: None,
                resume: None,
                isolation: Isolation::Isolated,
                depth: 0,
            },
            reserved,
            None,
        );
    });

    // Release the gate from a second thread so it can race `drop(home)`:
    // pre-fix, `drop` returns without waiting on the task at all, so the
    // release only needs to land some time after `drop` was CALLED, not
    // after it returned — a short sleep covers that. Post-fix, `drop` blocks
    // on the task's completion signal until this fires, and `HOME_TEST_LOCK`
    // stays held for that whole wait (`HomeSandbox`'s custom `drop` body runs
    // before its own `_guard` field drops), so no other test can race the
    // override in either branch.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let _ = release_gate.send(());
    });
    drop(home);
    releaser.join().expect("releaser thread joins");

    // `real_home` was captured at the top while the lock was held, so it is
    // the OS-resolved home by construction.
    let real_job_path = real_home
        .join(".clauth")
        .join("jobs")
        .join(format!("{job_id}.json"));

    // The write is one small local JSON file with no network hop; poll
    // briefly rather than asserting the instant `drop` returns.
    let mut leaked = false;
    for _ in 0..25 {
        if real_job_path.exists() {
            leaked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Never delete anything but this test's own uniquely-named job file — the
    // real jobs dir is the operator's live directory.
    if leaked {
        let _ = std::fs::remove_file(&real_job_path);
    }

    assert!(
        !leaked,
        "detached background delegate wrote its job file into the REAL jobs \
         dir at {} — HomeSandbox::drop returned (or the task ran) before the \
         detached task finished, so it resolved the operator's real $HOME \
         instead of the sandbox",
        real_job_path.display(),
    );
}

/// The text of a caught panic, whichever payload the macro produced:
/// `assert!` with a bare literal panics with a `&str`, `format!`-shaped
/// messages with a `String`, and reading only one of the two makes a fired
/// guard read as a missing one.
fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
        })
        .unwrap_or_default()
}

/// A stuck detached task must fail by NAME. Both waits behind the sandbox
/// fence used to be an unbounded `recv()`, so a task that never reached its
/// final send turned teardown into a CI job that timed out printing nothing —
/// indistinguishable from a slow suite, and naming neither the task nor the
/// wait. Bounded here against a short deadline; the shipped one is a hang
/// detector, not a race bound.
#[test]
fn a_stuck_background_task_fails_by_name_instead_of_hanging() {
    let _home = HomeSandbox::new();
    // Kept alive: dropping the sender is the OTHER exit, and it is the one
    // that must NOT be reported as a stall.
    let _never_sent = crate::testutil::register_background_task();

    let stalled = std::panic::catch_unwind(|| {
        crate::testutil::join_background_tasks_with(Duration::from_millis(50));
    })
    .expect_err("a task that never signals must fail the wait");
    let message = panic_text(&stalled);
    assert!(
        message.contains("did not signal completion") && message.contains("join_background_tasks"),
        "the failure names the wait it was stuck in: {message}"
    );
}

/// The other exit: a task that PANICS drops its sender while unwinding, and
/// the panic itself is what the run should report. A disconnect is a finished
/// wait, never a stall.
#[test]
fn a_dropped_sender_ends_the_wait_without_reporting_a_stall() {
    let _home = HomeSandbox::new();
    drop(crate::testutil::register_background_task());
    // No panic, and no 50ms spent: `recv` on a disconnected channel returns at
    // once.
    crate::testutil::join_background_tasks_with(Duration::from_millis(50));
}

/// Registering with no sandbox alive is a named failure at the REGISTRATION,
/// not a silent wait charged to some unrelated later test. The registry is
/// process-global, so a receiver pushed with nothing to drain it sits there
/// until the next teardown blocks on a task that test never launched.
#[test]
fn registering_a_background_task_with_no_sandbox_fails_by_name() {
    // `HOME_TEST_LOCK` without an override is exactly the shape under test:
    // holding it keeps a concurrent sandbox from setting one underneath us
    // when the suite runs in-process (`cargo test`) instead of per-test
    // (`nextest`). Cleared rather than restored, because an unset override is
    // the safe resting state — `profile::home_dir` panics on it, which is how
    // a test that forgot its sandbox is meant to fail.
    let _lock = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::profile::clear_home_override();

    let refused = std::panic::catch_unwind(|| {
        drop(crate::testutil::register_background_task());
    })
    .expect_err("no sandbox alive, so the registration must fail");
    let message = panic_text(&refused);
    assert!(
        message.contains("no home sandbox alive"),
        "the failure names what is missing: {message}"
    );
    assert_eq!(
        crate::testutil::pending_background_tasks(),
        0,
        "a refused registration leaves nothing for a later test's teardown"
    );
}

/// The detach gate is bounded too: a test that arms it and dies before
/// releasing would otherwise park the task forever, and the teardown waiting
/// on that task turns it into the same anonymous timeout.
#[test]
fn an_unreleased_detach_gate_gives_up_instead_of_parking_the_task() {
    let _home = HomeSandbox::new();
    let _never_released = arm_detach_gate();

    // Driven from a second thread so the UNBOUNDED shape fails as a red rather
    // than as a hang: without the bound this thread never finishes, and the
    // `recv_timeout` below is what turns that into an assertion.
    let (returned, waited_for_return) = std::sync::mpsc::channel();
    let probe = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        detach_test_gate_with(Duration::from_millis(50));
        let _ = returned.send(started.elapsed());
    });

    let waited = waited_for_return
        .recv_timeout(Duration::from_secs(5))
        .expect("the gate gives up at its bound instead of blocking on a release that never comes");
    assert!(
        waited >= Duration::from_millis(50),
        "and it waits for that bound first: {waited:?}"
    );
    probe.join().expect("probe thread joins");
}

/// A job left at `running` must red. This is the outcome the flake produced —
/// a detached task discarded un-run, its record never finalized — and the
/// assertion the drivers lean on to say so. Posed directly rather than by
/// racing a runtime shutdown, because the race is what the drivers now
/// prevent.
#[test]
fn a_job_left_running_fails_the_finalized_assertion() {
    let _home = HomeSandbox::new();
    jobs::write_running(&jobs::RunningSpec {
        job_id: "d-probe-running".to_string(),
        profile: "solo".to_string(),
        started_at: 1,
        recorded_at: 1,
        timeout_secs: 60,
        endpoint: None,
        isolated: false,
        idle_secs: None,
        kind: jobs::RecordKind::Collectable,
    })
    .expect("pose a running job record");

    let unfinalized = std::panic::catch_unwind(|| crate::testutil::assert_jobs_done(1))
        .expect_err("a job still at `running` is not a finalized job");
    let message = panic_text(&unfinalized);
    assert!(
        message.contains("Running"),
        "the failure shows which job is unfinalized: {message}"
    );
}
