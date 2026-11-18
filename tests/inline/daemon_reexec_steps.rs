//! The re-exec decision's steps and wiring: the stop runs before the exec,
//! the exec runs only when the watch is due AND the stop won the race against
//! the signal path, a stop signal parked while the stop ran ends the run by
//! the signal exit instead of the exec, and an exec failure reaches the
//! abort. The step tests inject every side effect; the wiring tests drive the
//! production `reexec_if_due` with the self-exe seam pinned to a replaced
//! file, a real one-owner kit, and the exec/signal-exit hooks armed in place
//! of the process-ending effects. The run loop's stand-down and capable ends
//! are driven end to end by
//! `daemon::tests::the_run_loop_stands_down_on_a_replacement_that_cannot_take_the_handover`
//! and `daemon::tests::the_run_loop_reexecs_into_a_replacement_that_answers_the_probe`;
//! the abort arm by the `reexec_step` unit tests and the wiring test for a
//! replacement that changes after the probe.
#![allow(
    clippy::expect_used,
    reason = "helpers outside #[test] fns expect on fixtures, and this cfg(all(test, unix)) mod gets no cfg(test) exemption"
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::ReexecWatch;
use super::RestartAdvice;
use super::SpendDrain;
use super::api::tls::{CertPaths, CertSource};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::reexec_command;
#[cfg(target_os = "linux")]
use super::reexec_command_with_argv;
use super::reexec_if_due;
use super::reexec_step;
use super::reexec_wiring_test;
use crate::lockorder::RankedMutex;
use crate::plugin_host::ExeChange;
use crate::usage::ActivityStore;

/// The stop runs before the exec, and a successful exec (one that never
/// returned) never reaches the abort.
#[test]
fn the_stop_runs_before_the_exec_and_a_successful_exec_never_aborts() {
    let order = RefCell::new(Vec::new());
    let mut signal_exited = false;
    let mut aborted = false;
    reexec_step(
        true,
        || {
            order.borrow_mut().push('s');
            true
        },
        || None,
        || {
            order.borrow_mut().push('e');
            true
        },
        |_| signal_exited = true,
        || aborted = true,
    );
    assert_eq!(
        order.borrow().as_slice(),
        ['s', 'e'],
        "the stop precedes the exec"
    );
    assert!(!signal_exited, "no parked signal, no signal exit");
    assert!(!aborted, "a successful exec never returns to abort");
}

/// A not-due step runs nothing at all: no stop, no exec, no signal exit, no
/// abort.
#[test]
fn a_not_due_step_runs_nothing() {
    let mut stopped = false;
    let mut exec_called = false;
    let mut signal_exited = false;
    let mut aborted = false;
    reexec_step(
        false,
        || {
            stopped = true;
            true
        },
        || None,
        || {
            exec_called = true;
            true
        },
        |_| signal_exited = true,
        || aborted = true,
    );
    assert!(!stopped, "a not-due step runs no stop");
    assert!(!exec_called, "a not-due step runs no exec");
    assert!(!signal_exited, "a not-due step runs no signal exit");
    assert!(!aborted, "a not-due step never aborts");
}

/// A stop that lost the race to the signal path (it already took the stop
/// kit) skips the exec: the signal path owns this process's exit, and
/// re-execing now would cut its token-save drain short.
#[test]
fn a_stop_that_lost_the_race_runs_no_exec() {
    let mut exec_called = false;
    let mut signal_exited = false;
    let mut aborted = false;
    reexec_step(
        true,
        || false,
        || None,
        || {
            exec_called = true;
            false
        },
        |_| signal_exited = true,
        || aborted = true,
    );
    assert!(!exec_called, "no exec after a lost race");
    assert!(!signal_exited, "nor a signal exit");
    assert!(
        !aborted,
        "no abort either: the signal path ends the process"
    );
}

/// An exec that returned (it failed) reaches the abort step.
#[test]
fn an_exec_failure_reaches_the_abort_step() {
    let mut signal_exited = false;
    let mut aborted = false;
    reexec_step(
        true,
        || true,
        || None,
        || false,
        |_| signal_exited = true,
        || aborted = true,
    );
    assert!(!signal_exited, "no parked signal, no signal exit");
    assert!(aborted, "an exec error aborts");
}

/// An exec that never returned (it succeeded) never reaches the abort step.
#[test]
fn a_successful_exec_never_reaches_the_abort_step() {
    let mut signal_exited = false;
    let mut aborted = false;
    reexec_step(
        true,
        || true,
        || None,
        || true,
        |_| signal_exited = true,
        || aborted = true,
    );
    assert!(!signal_exited, "no parked signal, no signal exit");
    assert!(!aborted, "a successful exec never returns to abort");
}

/// A stop signal parked while the stop ran ends the run by the signal-exit
/// step with that signal: the exec never runs and the abort never runs.
#[test]
fn a_parked_stop_signal_ends_by_the_signal_exit_step() {
    let order = RefCell::new(Vec::new());
    let exited = RefCell::new(Vec::new());
    let mut exec_called = false;
    let mut aborted = false;
    reexec_step(
        true,
        || {
            order.borrow_mut().push('s');
            true
        },
        || Some(libc::SIGTERM),
        || {
            exec_called = true;
            true
        },
        |signal| {
            order.borrow_mut().push('x');
            exited.borrow_mut().push(signal);
        },
        || aborted = true,
    );
    assert_eq!(
        order.borrow().as_slice(),
        ['s', 'x'],
        "the signal exit follows the stop"
    );
    assert_eq!(
        *exited.borrow(),
        [libc::SIGTERM],
        "the exit carries the parked signal"
    );
    assert!(!exec_called, "a parked stop signal never reaches the exec");
    assert!(!aborted, "and never the abort");
}

/// A replacement that never clears (a daemon whose stop kit is empty, or an
/// old daemon's forever-replaced binary) must not overflow the streak
/// counter and must not re-report due every tick: 300 consecutive replaced
/// reads never panic and report due exactly once.
#[test]
fn consecutive_replaced_reads_report_due_once_and_never_overflow() {
    let mut watch = ReexecWatch::default();
    let dues = (0..300)
        .filter(|_| watch.observe(Some(ExeChange::Replaced)))
        .count();
    assert_eq!(dues, 1, "the streak reports due once, on the second read");
}

/// The re-exec stop runs on the watched main loop, so the drain it waits on
/// must fit inside the watchdog's running-time deadline with the tick budget
/// the watchdog's own doc reserves (a legal slow tick runs up to the
/// deadline minus `TICK`): retuning the token deadline past the relation
/// must red here before it aborts real daemons mid-drain. The post-drain
/// joins in `stop_for_exit` are unbounded and stand outside this pin.
#[test]
fn the_spend_drain_bound_fits_the_watchdog_deadline() {
    assert!(
        super::SPEND_DRAIN_BOUND.saturating_add(super::TICK) < super::WATCHDOG_DEADLINE,
        "the drain bound plus a tick must stay inside the watchdog deadline: \
         {} + {} >= {}",
        super::SPEND_DRAIN_BOUND.as_secs(),
        super::TICK.as_secs(),
        super::WATCHDOG_DEADLINE.as_secs()
    );
}

/// A stop whose thread spawns all fail drains FIRST, then runs its stops
/// inline on the main loop, so the serial bounded part that must stay inside
/// the watchdog's running-time deadline — the drain (its bound), one inline
/// child stop (the stop budget plus the answer grace) and the tick budget —
/// keeps the save inside the deadline even under a thread limit, or the
/// limit aborts real daemons mid-save. The proxies' serial stop is one such
/// bounded stop per child and the post-answer joins are unbounded, so those
/// stand outside the pin: a watchdog abort past them cuts a children's stop
/// only, which the next start reclaims.
#[test]
fn the_serial_stop_fits_the_watchdog_deadline() {
    let serial = super::gateway::DAEMON_STOP_BUDGET
        .saturating_add(super::gateway::STOP_ANSWER_GRACE)
        .saturating_add(super::SPEND_DRAIN_BOUND)
        .saturating_add(super::TICK);
    assert!(
        serial < super::WATCHDOG_DEADLINE,
        "one inline stop plus the drain plus a tick must stay inside the watchdog deadline: \
         {} >= {}",
        serial.as_secs(),
        super::WATCHDOG_DEADLINE.as_secs()
    );
}

/// The re-exec command keeps the argv it was given: argv[0] included, because
/// the daemon's identity guard reads argv[0]'s basename, so a daemon started
/// through a symlink must survive the re-exec with the name it started as.
/// The argv source is a seam, so the test controls it: a reentry-shaped argv
/// (`--exact` this test, one thread) makes the re-entered child run only
/// this test — the only helper whose argv[0] survives untouched, since the
/// kernel rewrites it for shebang scripts and option-parsing readers choke
/// on the harness's `--exact` — with an env marker that makes the child
/// write its own argv to a verdict file and return. The parent asserts the
/// child received exactly the argv the seam was given, never re-typed.
#[cfg(target_os = "linux")]
#[test]
fn the_reexec_command_keeps_the_process_argv() {
    let verdict_env = "CLAUTH_TEST_ARGV_VERDICT";
    if let Ok(verdict) = std::env::var(verdict_env) {
        // The re-entered child: report the argv this process actually
        // received, then let the harness finish.
        let argv = std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(verdict, argv).expect("write the verdict");
        return;
    }

    let scratch = tempfile::tempdir().expect("test scratch");
    let verdict = scratch.path().join("argv");
    let argv = [
        std::ffi::OsString::from("clauth-reexec-test-argv0"),
        std::ffi::OsString::from("--exact"),
        std::ffi::OsString::from(
            "daemon::reexec_steps_tests::the_reexec_command_keeps_the_process_argv",
        ),
        std::ffi::OsString::from("--test-threads=1"),
    ];
    let mut command =
        reexec_command_with_argv(std::path::Path::new("/proc/self/exe"), argv.iter().cloned());
    command
        .env(verdict_env, &verdict)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().expect("spawn the re-entered child");

    // Bounded reap: a red (no verdict) kills the child, so the suite never
    // leaks a live harness process.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                drop(child.kill());
                drop(child.wait());
                panic!("the child never finished reporting its argv");
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => panic!("waiting on the argv child failed: {e}"),
        }
    };
    assert!(status.success(), "the child passed: {status:?}");
    let printed: Vec<String> = std::fs::read_to_string(&verdict)
        .expect("the verdict")
        .lines()
        .map(str::to_owned)
        .collect();
    let expected: Vec<String> = argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        printed, expected,
        "the child received exactly the given argv, argv[0] included"
    );
}

/// The production wrapper feeds the process's own argv to the seam: the
/// recorded path as the program and every arg after argv[0] kept, so a
/// re-exec'd daemon re-runs the command line it was started with and the
/// identity guard still reads it as a daemon. The test re-enters itself with
/// args it chose, because a harness run with no args after argv[0] would make
/// any in-process comparison vacuous; the re-entered child builds the
/// production command and reports its args. The command is read, never run:
/// the recorded path is a name, not an executable here.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_production_reexec_command_keeps_this_process_args() {
    let verdict_env = "CLAUTH_TEST_PRODUCTION_ARGV_VERDICT";
    let recorded = std::path::Path::new("/recorded/clauth");
    if let Some(verdict) = std::env::var_os(verdict_env) {
        // The re-entered child: report the production command's args.
        let command = reexec_command(recorded);
        assert_eq!(
            command.get_program(),
            recorded.as_os_str(),
            "the re-exec runs the recorded path"
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(verdict, args).expect("write the verdict");
        return;
    }

    let scratch = tempfile::tempdir().expect("test scratch");
    let verdict = scratch.path().join("args");
    let mut command = crate::testutil::reentry_command(
        "daemon::reexec_steps_tests::the_production_reexec_command_keeps_this_process_args",
        &["daemon"],
    );
    let expected: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    command.env(verdict_env, &verdict);
    let mut child = command.spawn().expect("spawn the re-entered child");

    // Bounded reap: a red (no verdict) kills the child, so the suite never
    // leaks a live harness process.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                drop(child.kill());
                drop(child.wait());
                panic!("the child never finished reporting its args");
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => panic!("waiting on the args child failed: {e}"),
        }
    };
    assert!(status.success(), "the child passed: {status:?}");
    let printed: Vec<String> = std::fs::read_to_string(&verdict)
        .expect("the verdict")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        printed, expected,
        "the re-exec keeps this process's args after argv[0]"
    );
}

/// The executable stub a replacement answers the capability probe with: the
/// probe spawns it with the daemon's argv, so the capable path proceeds to
/// the stop and the exec hook. Its condition reads only the LAST argument
/// (the appended probe flag), because the re-entered argv here is the test
/// harness's own; the whole argv the probe builds is pinned by
/// `a_replacement_refusing_its_certificate_stands_down_with_the_reason`
/// against `reexec_command`.
fn answering_stub() -> String {
    format!(
        "#!/bin/sh\nfor last; do :; done\n[ \"$last\" = \"{}\" ] && {{ printf '{}'; exit 0; }}\nexit 0\n",
        super::EXEC_HANDOVER_FLAG,
        super::EXEC_HANDOVER_TOKEN,
    )
}

/// A scratch file pinned as this process's recorded self-exe and then
/// replaced in place, plus a fresh one-owner kit built over an empty
/// activity store: the wiring tests' fixture. The store and the shutdown
/// flag come back as handles the drain shares, so a test can pose a spend in
/// flight and read the flag the drain raises.
fn replaced_binary_and_kit() -> (
    tempfile::TempDir,
    crate::plugin_host::SelfExeOverride,
    Arc<super::gateway::StopKit>,
    ActivityStore,
    Arc<AtomicBool>,
) {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let stub = answering_stub();
    std::fs::write(&bin, &stub).expect("write the recorded binary");
    let pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    std::fs::write(&bin, format!("{stub}# replaced\n")).expect("replace the recorded binary");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub executable");
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let shutting_down = Arc::new(AtomicBool::new(false));
    let spends = SpendDrain::new(Arc::clone(&activity), Arc::clone(&shutting_down));
    let kit = super::gateway::StopKit::new(spends, None, super::proxies::ProxySupervision::idle());
    (scratch, pin, kit, activity, shutting_down)
}

/// The production wiring runs the real stop before the exec attempt: two
/// `reexec_if_due` calls over a replaced recorded binary (arm, then due)
/// probe the capability, take the kit, run `stop_for_exit` once, and only
/// then reach the exec hook the test armed in place of the real exec. The
/// hook also reads the drain's shutdown flag, which only `stop_for_exit`'s
/// drain raises, and the heartbeat, which the capable verdict restamps
/// between the probe and the stop (each bounded leg gets its own watchdog
/// window): a stop call swapped for a plain drop of the kit parts would
/// leave both down and red here.
#[test]
fn the_reexec_wiring_stops_before_the_exec_attempt() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, shutting_down) = replaced_binary_and_kit();
    let heartbeat = Arc::new(AtomicU64::new(0));
    reexec_wiring_test::arm_exec({
        let shutting_down = Arc::clone(&shutting_down);
        let heartbeat = Arc::clone(&heartbeat);
        move |_, _| {
            assert_eq!(
                reexec_wiring_test::stops(),
                1,
                "the stop ran before the exec attempt"
            );
            assert!(
                shutting_down.load(Ordering::SeqCst),
                "the drain raised the shutdown flag before the exec attempt"
            );
            assert!(
                heartbeat.load(Ordering::Relaxed) > 0,
                "the capable verdict restamped the heartbeat between the probe and the stop"
            );
            true
        }
    });
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        0,
        "the arming read runs no stop"
    );
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the due read ran the stop once"
    );
    assert!(
        lines.snapshot().iter().any(|l| l.contains("re-executing")),
        "the due line logged: {:?}",
        lines.snapshot()
    );
}

/// A stop whose thread spawn fails under a thread limit still runs the
/// drain and reaches the exec: the failed spawn holds its payload until
/// after the drain, whose drop then runs the stop inline, so the run never
/// panics past the drain and a watchdog abort can cut only a children's
/// stop. The refusal seam poses the one real spawn failure the box cannot,
/// and the failure counter proves the degraded path ran.
#[test]
fn a_refused_stop_spawn_drains_then_stops_inline_and_still_execs() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, shutting_down) = replaced_binary_and_kit();
    let _refused = super::gateway::probe_seam::fail_stop_spawns();
    reexec_wiring_test::arm_exec({
        let shutting_down = Arc::clone(&shutting_down);
        move |_, _| {
            assert!(
                shutting_down.load(Ordering::SeqCst),
                "the drain ran before the exec attempt"
            );
            true
        }
    });

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the due read ran the stop once"
    );
    assert_eq!(
        super::gateway::probe_seam::stop_spawn_failures(),
        1,
        "the stop's one spawn failed through the seam, and the run survived it"
    );
}

/// A watcherless re-exec's stop spawns nothing at all: no child is left, so
/// its drain runs alone — a stop thread for an idle supervision would be a
/// needless spawn through a thread limit that already lost the watcher
/// once. Under the failure seam, no spawn is even attempted, so none fails.
#[test]
fn a_watcherless_reexec_spawns_no_stop_thread() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, shutting_down) = replaced_binary_and_kit();
    let _parts = kit
        .take()
        .expect("the spawn-failure arm took the kit at boot");
    kit.mark_watcher_lost();
    let _refused = super::gateway::probe_seam::fail_stop_spawns();
    reexec_wiring_test::arm_exec({
        let shutting_down = Arc::clone(&shutting_down);
        move |_, _| {
            assert!(
                shutting_down.load(Ordering::SeqCst),
                "the drain ran before the exec attempt"
            );
            true
        }
    });

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the due read ran the spare drain's stop once"
    );
    assert_eq!(
        super::gateway::probe_seam::stop_spawn_failures(),
        0,
        "the watcher-lost stop spawns nothing to fail"
    );
}

/// A stop that lost the race to the signal path (the kit was already taken)
/// runs neither the stop nor the exec; the due line still logs once.
#[test]
fn a_pretaken_kit_runs_neither_the_stop_nor_the_exec() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, _shutting_down) = replaced_binary_and_kit();
    let _parts = kit
        .take()
        .expect("the test takes the kit first, as the signal path would");
    reexec_wiring_test::arm_exec(|_, _| {
        panic!("a stop that lost the race must not reach the exec");
    });
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(reexec_wiring_test::stops(), 0, "no stop after a lost race");
    assert!(
        lines.snapshot().iter().any(|l| l.contains("re-executing")),
        "the due line logged: {:?}",
        lines.snapshot()
    );
}

/// A stop signal parked on the kit ends the production re-exec by the signal
/// exit after the real stop: the exec hook never runs.
#[test]
fn a_parked_stop_signal_ends_the_reexec_by_the_signal_exit() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, _shutting_down) = replaced_binary_and_kit();
    kit.park_stop_signal(libc::SIGTERM);
    let exited = Rc::new(RefCell::new(Vec::new()));
    reexec_wiring_test::arm_signal_exit({
        let exited = Rc::clone(&exited);
        move |signal| exited.borrow_mut().push(signal)
    });
    reexec_wiring_test::arm_exec(|_, _| {
        panic!("a parked stop signal must not reach the exec");
    });
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the real stop ran before the signal exit"
    );
    assert_eq!(
        *exited.borrow(),
        [libc::SIGTERM],
        "the run ends by the signal exit with the parked signal"
    );
}

/// A kit whose watcher never started (the spawn-failure arm took it and
/// marked it) still re-execs when due — the children are stopped, so the one
/// stop left is the spare drain's wait — and the due line logs once per
/// replacement, never once per tick: 300 consecutive reads run one stop, one
/// exec and one due line.
#[test]
fn a_watcherless_kit_reexecs_once_and_logs_once() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, _shutting_down) = replaced_binary_and_kit();
    let _parts = kit
        .take()
        .expect("the spawn-failure arm took the kit at boot");
    kit.mark_watcher_lost();
    let execs = Rc::new(RefCell::new(0usize));
    reexec_wiring_test::arm_exec({
        let execs = Rc::clone(&execs);
        move |_, _| {
            *execs.borrow_mut() += 1;
            true
        }
    });
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    for _ in 0..300 {
        reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    }
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the due read ran the one stop left: the spare drain's wait"
    );
    assert_eq!(
        *execs.borrow(),
        1,
        "the due re-exec runs once for the one replacement"
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|l| l.contains("re-executing"))
            .count(),
        1,
        "the due line logs once per replacement, never once per tick"
    );
}

/// A kit whose watcher never started (the spawn-failure arm dropped the
/// drain with the children, which stopped them) still waits out a token
/// spend in flight before the due re-exec's exec attempt: the kit's spare
/// drain raises the scheduler's shutdown flag, and the exec runs only once
/// the spend has saved. The saver holds its claim until the drain logs its
/// wait, so the save lands mid-wait however late any thread wakes, and the
/// exec hook reads the claim gone.
#[test]
fn a_watcherless_reexec_waits_out_the_spend_in_flight() {
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, activity, shutting_down) = replaced_binary_and_kit();
    let _parts = kit
        .take()
        .expect("the spawn-failure arm took the kit at boot");
    kit.mark_watcher_lost();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    // A token spend in flight when the re-exec becomes due.
    let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
    let saver = {
        let activity = Arc::clone(&activity);
        let shutting_down = Arc::clone(&shutting_down);
        let lines = lines.clone();
        std::thread::spawn(move || {
            let name = crate::profile::ProfileName::from("mid-spend");
            let claim = crate::usage::claim_scheduler_spend_unless_shutting_down(
                &activity,
                &name,
                shutting_down.as_ref(),
            )
            .expect("an unset flag admits the spend");
            claimed_tx.send(()).expect("the test waits for the claim");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !lines.snapshot().iter().any(|l| l.contains("waiting up to")) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the drain never logged its wait"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // The save lands while the drain waits; dropping the claim
            // clears the in-flight marker.
            drop(claim);
        })
    };
    claimed_rx.recv().expect("the spender claimed");
    reexec_wiring_test::arm_exec({
        let activity = Arc::clone(&activity);
        let shutting_down = Arc::clone(&shutting_down);
        move |_, _| {
            assert!(
                shutting_down.load(Ordering::SeqCst),
                "the drain raised the shutdown flag before the exec attempt"
            );
            assert!(
                !crate::usage::token_spend_in_flight(&activity),
                "the exec attempt runs only once the spend in flight saved"
            );
            true
        }
    });

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    saver.join().expect("the saver ran");
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the due read ran the spare drain's stop once"
    );
    assert!(
        lines.snapshot().iter().any(|l| l.contains("waiting up to")),
        "the drain saw the spend in flight and waited: {:?}",
        lines.snapshot()
    );
}

/// Whether `fd` is close-on-exec, read with `F_GETFD` through libc: the
/// wiring test's witness for "inheritable across the exec".
#[expect(
    unsafe_code,
    reason = "reading a test-owned descriptor's flags through libc, the same call the handover makes"
)]
fn cloexec(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: `fd` is an open descriptor this process owns; F_GETFD only
    // reads its descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags & libc::FD_CLOEXEC != 0
}

/// The production handover runs before the exec attempt and hands the
/// registered descriptors over the way the exec receives them: both named
/// on the exec command's env (never this process's, which the live threads
/// must not race), each inheritable (close-on-exec cleared), and no other
/// descriptor made inheritable. The hook reads the command the production
/// exec runs, so an edit of the production command build (a defaulted or
/// deleted handover payload) reds here instead of the exec quietly running
/// without the pair. The sandbox serializes the process-global
/// registrations against every other test that touches them.
#[test]
fn the_reexec_wiring_hands_the_registered_descriptors_over() {
    use std::os::fd::AsRawFd as _;
    let _home = crate::testutil::HomeSandbox::new();
    reexec_wiring_test::reset();
    let (_scratch, _pin, kit, _activity, _shutting_down) = replaced_binary_and_kit();
    let lock_file = tempfile::tempfile().expect("the registered lock fd");
    let lock_fd = lock_file.as_raw_fd();
    super::exec_handover::register_lock(lock_fd);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("the registered listener");
    let listen_fd = listener.as_raw_fd();
    super::exec_handover::register_listener(listen_fd);
    let bystander = tempfile::tempfile().expect("the bystander fd");
    let bystander_fd = bystander.as_raw_fd();
    let saw = Rc::new(RefCell::new(false));
    reexec_wiring_test::arm_exec({
        let saw = Rc::clone(&saw);
        move |_path, command| {
            // What the real exec would run: the production command with the
            // handover already applied, read off it.
            let envs: std::collections::HashMap<_, _> = command
                .get_envs()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            let lock_named = envs
                .get(super::exec_handover::LOCK_FD_ENV)
                .map(|value| value.as_deref());
            assert_eq!(
                lock_named,
                Some(Some(lock_fd.to_string().as_str())),
                "the lock descriptor number is named on the exec command's env"
            );
            let listen_named = envs
                .get(super::exec_handover::LISTEN_FD_ENV)
                .map(|value| value.as_deref());
            assert_eq!(
                listen_named,
                Some(Some(listen_fd.to_string().as_str())),
                "the listener descriptor number is named on the exec command's env"
            );
            assert!(
                std::env::var_os(super::exec_handover::LOCK_FD_ENV).is_none()
                    && std::env::var_os(super::exec_handover::LISTEN_FD_ENV).is_none(),
                "this process's environment never holds the handover vars"
            );
            assert!(!cloexec(lock_fd), "the lock descriptor is inheritable");
            assert!(
                !cloexec(listen_fd),
                "the listener descriptor is inheritable"
            );
            assert!(cloexec(bystander_fd), "nothing else was made inheritable");
            *saw.borrow_mut() = true;
            true
        }
    });

    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert!(*saw.borrow(), "the exec hook ran and saw the handover");
    // The registered fds close when their owners drop at the end of this
    // test: a later production handover must never touch a recycled number.
    super::exec_handover::reset();
}

/// A one-owner kit with no supervisors, the wiring tests' idle shape.
fn idle_kit() -> (Arc<super::gateway::StopKit>, ActivityStore, Arc<AtomicBool>) {
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let shutting_down = Arc::new(AtomicBool::new(false));
    let spends = SpendDrain::new(Arc::clone(&activity), Arc::clone(&shutting_down));
    let kit = super::gateway::StopKit::new(spends, None, super::proxies::ProxySupervision::idle());
    (kit, activity, shutting_down)
}

/// Write an executable `/bin/sh` stub over the recorded binary path: one
/// invocation line appended to `probes` (when given) per spawn, then
/// `body`. The probe spawns the stub with the daemon's argv, so a stub
/// that exits 0 without printing the token answers "not capable".
fn install_stub(bin: &std::path::Path, probes: Option<&std::path::Path>, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let count = probes
        .map(|probes| format!("echo x >> '{}'\n", probes.display()))
        .unwrap_or_default();
    std::fs::write(bin, format!("#!/bin/sh\n{count}{body}\n")).expect("write the stub");
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub executable");
}

/// How many times the stub behind `probes` was spawned.
fn probe_count(probes: &std::path::Path) -> usize {
    std::fs::read_to_string(probes).map_or(0, |text| text.lines().count())
}

/// The capability probe's stand-down: a replacement that never answers the
/// hidden flag leaves the daemon running — no stop, no exec, no
/// re-executing line — and the verdict logs once. The same file is never
/// re-probed: 300 consecutive checks run the one probe, the stub counting
/// its own invocations.
#[test]
fn a_replacement_that_never_answers_the_probe_stands_down_once_and_is_never_reprobed() {
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let probes = scratch.path().join("probes");
    install_stub(&bin, Some(&probes), "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    // The replacement: the same shape, different content (a new identity).
    install_stub(&bin, Some(&probes), "exit 0 # replaced");
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("a not-capable replacement must not exec"));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    for _ in 0..300 {
        reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    }
    assert_eq!(reexec_wiring_test::stops(), 0, "a stand-down stops nothing");
    assert_eq!(
        probe_count(&probes),
        1,
        "the one replacement probed once, never on every check"
    );
    let verdict = format!(
        "clauth daemon: {} does not understand the restart handover; keeping this image running until a manual restart",
        bin.display()
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|l| l.as_str() == verdict)
            .count(),
        1,
        "the not-capable verdict logged once: {:?}",
        lines.snapshot()
    );
    assert!(
        !lines.snapshot().iter().any(|l| l.contains("re-executing")),
        "the stand-down never announces a re-exec: {:?}",
        lines.snapshot()
    );
    assert_eq!(
        heartbeat.load(Ordering::Relaxed),
        0,
        "no capable verdict, no heartbeat restamp"
    );
}

/// The probe compares the whole answer, never a prefix: a replacement that
/// prints the token and one more byte — a later handover protocol version
/// built on this one — reads as not capable, so it is never exec'd into.
/// The exact-answer replacement afterwards is the control: the same fixture
/// drives the capable path (the stop taken, the exec hook reached) once the
/// answer matches byte for byte.
#[test]
fn a_replacement_answering_the_token_plus_a_byte_reads_not_capable() {
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let probes = scratch.path().join("probes");
    install_stub(&bin, Some(&probes), "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    install_stub(
        &bin,
        Some(&probes),
        &format!("printf '{}x'; exit 0", super::EXEC_HANDOVER_TOKEN),
    );
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("a prefix answer must not exec"));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        0,
        "a prefix answer stands down: nothing stops"
    );
    assert_eq!(probe_count(&probes), 1, "the replacement probed once");
    assert_eq!(
        heartbeat.load(Ordering::Relaxed),
        0,
        "no capable verdict, no heartbeat restamp"
    );
    let verdict = format!(
        "clauth daemon: {} does not understand the restart handover; keeping this image running until a manual restart",
        bin.display()
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|l| l.as_str() == verdict)
            .count(),
        1,
        "the prefix answer logged the stand-down: {:?}",
        lines.snapshot()
    );

    // Control: the same probe, an answer of exactly the token, is capable.
    install_stub(
        &bin,
        Some(&probes),
        &format!("printf '{}'; exit 0", super::EXEC_HANDOVER_TOKEN),
    );
    reexec_wiring_test::arm_exec(|_, _| true);
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the exact answer is capable: the stop ran"
    );
}

/// A replacement that cannot load the API certificate answers the
/// cert-refused marker and its reason: the daemon keeps its image (no stop,
/// no exec), logs the certificate line once with the reason made one
/// printable line of at most 200 chars, and never re-probes that file.
/// The probe hands the replacement the re-exec's own args plus the hidden
/// flag, so it judges the start the exec would run.
#[test]
fn a_replacement_refusing_its_certificate_stands_down_with_the_reason() {
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let probes = scratch.path().join("probes");
    let args_file = scratch.path().join("args");
    install_stub(&bin, Some(&probes), "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    install_stub(
        &bin,
        Some(&probes),
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\nprintf 'clauth-exec-handover-cert-refused\\nbad\\033[31m cert\\tpath {}'; exit 1",
            args_file.display(),
            "a".repeat(250)
        ),
    );
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("a cert-refused replacement must not exec"));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    for _ in 0..300 {
        reexec_if_due(
            &mut watch,
            &kit,
            &heartbeat,
            &RestartAdvice {
                listen: Some("127.0.0.1:0".parse().expect("the test listener")),
                ..RestartAdvice::default()
            },
        );
    }
    assert_eq!(
        reexec_wiring_test::stops(),
        0,
        "a cert refusal stops nothing"
    );
    assert_eq!(probe_count(&probes), 1, "the replacement probed once");
    assert_eq!(
        heartbeat.load(Ordering::Relaxed),
        0,
        "no capable verdict, no heartbeat restamp"
    );

    let mut expected_args: Vec<String> = super::reexec_command(&bin)
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    expected_args.push(super::EXEC_HANDOVER_FLAG.to_owned());
    let seen_args: Vec<String> = std::fs::read_to_string(&args_file)
        .expect("the stub recorded its args")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        seen_args, expected_args,
        "the probe runs the re-exec's own args plus the hidden flag"
    );

    // Control characters become spaces, then the reason is cut to 200 chars:
    // "bad [31m cert path " is 19 chars, so 181 of the 250 `a`s survive.
    let reason = format!("bad [31m cert path {}", "a".repeat(181));
    let head = format!(
        "clauth daemon: {} cannot load the API certificate ({reason}); keeping this image running until a manual restart: `clauth daemon --replace --listen ",
        bin.display()
    );
    let snapshot = lines.snapshot();
    let cert_lines: Vec<&String> = snapshot
        .iter()
        .filter(|line| line.contains("cannot load the API certificate"))
        .collect();
    assert_eq!(
        cert_lines.len(),
        1,
        "the cert line logged once: {snapshot:?}"
    );
    assert!(
        cert_lines[0].starts_with(&head)
            && cert_lines[0].ends_with("` after fixing the certificate"),
        "the approved cert line, reason sanitized: {:?}",
        cert_lines[0]
    );
    assert!(
        !snapshot.iter().any(
            |line| line.contains("does not understand the restart handover")
                || line.contains("re-executing")
        ),
        "a cert refusal is neither the no-handover verdict nor a re-exec: {snapshot:?}"
    );
}

/// A NEW replacement (a different identity) re-arms the watch after a
/// stand-down: two consecutive checks on the new binary probe again.
#[test]
fn a_new_replacement_re_arms_the_watch_after_a_stand_down() {
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let probes = scratch.path().join("probes");
    install_stub(&bin, Some(&probes), "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    install_stub(&bin, Some(&probes), "exit 0 # first replacement");
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("a not-capable replacement must not exec"));
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(probe_count(&probes), 1, "the first replacement probed once");
    // A new binary lands at the path: a different length, so a different
    // identity however fast the two writes land.
    install_stub(&bin, Some(&probes), "exit 0 # second replacement");
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        probe_count(&probes),
        2,
        "the new replacement re-armed the watch and probed once more"
    );
}

/// A replacement the watch cannot key (a directory at the recorded path)
/// stands down once and stays stood down: no identity can tell it from a
/// later one, so re-probing every due cycle would spawn and log forever. A
/// readable replacement at the path re-arms the probe.
#[test]
fn an_unkeyable_replacement_stands_down_once_until_a_readable_one_appears() {
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let probes = scratch.path().join("probes");
    install_stub(&bin, Some(&probes), "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    std::fs::remove_file(&bin).expect("remove the recorded binary");
    std::fs::create_dir_all(&bin).expect("replace it with a directory the watch cannot key");
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("an unkeyable replacement must not exec"));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    for _ in 0..300 {
        reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    }
    let verdict = format!(
        "clauth daemon: {} does not understand the restart handover; keeping this image running until a manual restart",
        bin.display()
    );
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|line| line.as_str() == verdict)
            .count(),
        1,
        "the unkeyable replacement is judged once, never re-probed per cycle: {:?}",
        lines.snapshot()
    );
    std::fs::remove_dir(&bin).expect("clear the directory");
    install_stub(&bin, Some(&probes), "exit 0 # readable replacement");
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        probe_count(&probes),
        1,
        "the readable replacement re-armed the probe"
    );
}

/// A stood-down replacement that gains its exec bit still re-arms the
/// probe: the identity carries the mode bits, so an installer that lands
/// the file before its exec bit and chmods it later is judged again once it
/// can run.
#[test]
fn a_chmod_that_makes_a_stood_down_replacement_executable_re_arms_the_probe() {
    use std::os::unix::fs::PermissionsExt as _;
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let probes = scratch.path().join("probes");
    install_stub(&bin, Some(&probes), "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    install_stub(&bin, Some(&probes), "exit 0 # replaced");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644))
        .expect("strip the exec bit");
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("a not-capable replacement must not exec"));
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(
        probe_count(&probes),
        0,
        "the unexecutable replacement could not run"
    );
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("restore the exec bit");
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert_eq!(probe_count(&probes), 1, "the chmod re-armed the probe");
}

/// The capable verdict is bound to the bytes the probe spawned: a second
/// replacement landing while the stop runs is never exec'd unprobed (a
/// pre-handover build there would die as "already running" with the
/// singleton free). The probe's stub renames a different, non-answering
/// file over the recorded path before it answers, so the exec's own re-read
/// sees a new identity; the run takes the exec-failure abort, which the
/// armed hook records, and the exec attempt never runs.
#[test]
fn a_replacement_that_changes_after_the_probe_aborts_instead_of_execing() {
    use std::os::unix::fs::PermissionsExt as _;
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let replacement = scratch.path().join("replacement");
    std::fs::write(&replacement, "#!/bin/sh\nexit 0\n").expect("write the replacement");
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755))
        .expect("make the replacement executable");
    install_stub(&bin, None, "exit 0");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    let answering = format!(
        "mv '{}' \"$0\"\nprintf '{}'\nexit 0",
        replacement.display(),
        super::EXEC_HANDOVER_TOKEN,
    );
    install_stub(&bin, None, &answering);
    let (kit, _activity, _shutting_down) = idle_kit();
    let aborted = Rc::new(std::cell::Cell::new(false));
    reexec_wiring_test::arm_abort({
        let aborted = Rc::clone(&aborted);
        move || aborted.set(true)
    });
    reexec_wiring_test::arm_exec(|_, _| panic!("the changed replacement must not be exec'd"));
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert!(
        aborted.get(),
        "the changed replacement reaches the abort arm"
    );
    assert_eq!(
        reexec_wiring_test::stops(),
        1,
        "the stop ran before the abort"
    );
    assert!(
        lines
            .snapshot()
            .iter()
            .any(|line| line.contains("the replacement changed after the probe")),
        "one line names the changed replacement: {:?}",
        lines.snapshot()
    );
}

/// The re-exec stop joins the scheduler's tick thread and waits its exit
/// save: the drain that runs on the re-exec path stops the same thread the
/// signal path stops, and the exec attempt runs only after the save landed.
#[test]
fn the_reexec_stop_joins_the_scheduler_tick_threads_exit_save() {
    use std::os::unix::fs::PermissionsExt as _;
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    let stub = answering_stub();
    std::fs::write(&bin, &stub).expect("write the recorded binary");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    std::fs::write(&bin, format!("{stub}# replaced\n")).expect("replace the recorded binary");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub executable");
    let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
    let drain_flag = Arc::new(AtomicBool::new(false));
    let tick_flag = Arc::new(AtomicBool::new(false));
    let marker = scratch.path().join("tick-save");
    let tick = super::TickThread::new(
        Some(std::thread::spawn({
            let tick_flag = Arc::clone(&tick_flag);
            let marker = marker.clone();
            move || {
                while !tick_flag.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                std::fs::write(&marker, b"").expect("the tick exit save");
            }
        })),
        Arc::clone(&tick_flag),
    );
    let spends = SpendDrain::new(activity, drain_flag).with_scheduler(tick);
    let kit = super::gateway::StopKit::new(spends, None, super::proxies::ProxySupervision::idle());
    reexec_wiring_test::arm_exec({
        let marker = marker.clone();
        move |_, _| {
            assert!(
                marker.exists(),
                "the drain joined the tick thread's exit save before the exec attempt"
            );
            true
        }
    });
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    assert!(
        marker.exists(),
        "the re-exec's drain stopped the tick thread and waited its save"
    );
}

/// The probe's bound: a replacement that hangs is cut at the bound and
/// reads as not capable — no stop, no exec, and the wait was actually
/// bounded by the deadline, never by an early refusal. The stub execs the
/// sleep, so the bound's kill reaches the sleeping process itself instead
/// of a shell that leaves the sleeper holding the probe's stdout pipe.
#[test]
fn a_hanging_replacement_is_cut_at_the_probe_bound() {
    reexec_wiring_test::reset();
    let scratch = tempfile::tempdir().expect("test scratch");
    let bin = scratch.path().join("clauth");
    install_stub(&bin, None, "exec sleep 60");
    let _pin = crate::plugin_host::SelfExeOverride::pin(Some(&bin));
    install_stub(&bin, None, "exec sleep 60 # replaced");
    let (kit, _activity, _shutting_down) = idle_kit();
    reexec_wiring_test::arm_exec(|_, _| panic!("a hanging replacement must not exec"));
    let heartbeat = AtomicU64::new(0);
    let mut watch = ReexecWatch::default();
    let started = std::time::Instant::now();
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    reexec_if_due(&mut watch, &kit, &heartbeat, &RestartAdvice::default());
    let elapsed = started.elapsed();
    assert_eq!(reexec_wiring_test::stops(), 0, "a stand-down stops nothing");
    assert!(
        elapsed >= super::EXEC_PROBE_BOUND.saturating_sub(std::time::Duration::from_secs(1)),
        "the hang was waited out to the probe bound, not refused early: {elapsed:?}"
    );
    assert!(
        elapsed < super::EXEC_PROBE_BOUND + std::time::Duration::from_secs(10),
        "the probe was cut by the bound: {elapsed:?}"
    );
}

/// The capability probe runs on the watched main loop before the stop; the
/// heartbeat is restamped between the probe and the stop, so each leg gets
/// its own watchdog window — the probe's bound plus the tick budget must
/// stay inside the deadline, or a long probe followed by the stop's serial
/// legs would abort a real daemon mid-drain.
#[test]
fn the_capability_probe_fits_the_watchdog_deadline() {
    assert!(
        super::EXEC_PROBE_BOUND.saturating_add(super::TICK) < super::WATCHDOG_DEADLINE,
        "the probe bound plus a tick must stay inside the watchdog deadline: \
         {} + {} >= {}",
        super::EXEC_PROBE_BOUND.as_secs(),
        super::TICK.as_secs(),
        super::WATCHDOG_DEADLINE.as_secs()
    );
}

/// The hidden capability argument parses on the real CLI surface, answers
/// the exact token bytes through the production writer, and stays out of
/// --help and the hand-written completions: it is the daemon's probe
/// protocol, never a user flag.
#[test]
fn the_hidden_daemon_argument_parses_answers_and_stays_hidden() {
    use clap::Parser as _;
    let cli =
        crate::cli::Cli::try_parse_from(["clauth", "daemon", crate::daemon::EXEC_HANDOVER_FLAG])
            .expect("the hidden argument parses");
    let crate::cli::Command::Daemon { exec_handover, .. } =
        cli.command.expect("the daemon command")
    else {
        panic!("the hidden argument belongs to the daemon command");
    };
    assert!(exec_handover, "the hidden argument is set");
    // The answer: the exact bytes the probe compares against the spawn's
    // stdout, nothing else.
    let mut answer = Vec::new();
    crate::write_exec_handover_token(&mut answer).expect("write the token");
    assert_eq!(
        answer.as_slice(),
        crate::daemon::EXEC_HANDOVER_TOKEN.as_bytes(),
        "the token bytes, nothing else"
    );
    let mut command = <crate::cli::Cli as clap::CommandFactory>::command();
    let help = command
        .find_subcommand_mut("daemon")
        .expect("the daemon subcommand")
        .render_help()
        .to_string();
    assert!(
        !help
            .lines()
            .any(|line| line.contains(crate::daemon::EXEC_HANDOVER_FLAG)),
        "the hidden flag stays out of --help: {help}"
    );
    for shell in ["bash", "zsh", "fish"] {
        let completions = crate::completions::emission(shell).expect("every emitted shell");
        assert!(
            !completions.contains(crate::daemon::EXEC_HANDOVER_FLAG),
            "the hidden flag stays out of the {shell} completions"
        );
    }
}

/// The cert-refused answer writes the marker line, then the certificate error
/// as one line naming the unreadable file, and ends the run through
/// `FailureReported` (exit 1, no `Error:` line) rather than exiting the process
/// from inside the helper.
#[test]
fn the_cert_refused_answer_returns_failure_reported_after_its_marker_and_reason() {
    // Holds `HOME_TEST_LOCK`: the answer reads `CLAUTH_NO_API`, which the
    // kill-switch tests set under that lock.
    let _home = crate::testutil::HomeSandbox::new();
    let scratch = tempfile::tempdir().expect("test scratch");
    let cert = scratch.path().join("missing.crt");
    let certs = crate::daemon::api::tls::CertSource::from_flags(
        Some(cert.clone()),
        Some(scratch.path().join("missing.key")),
    );
    let mut answer = Vec::new();
    let error = crate::write_exec_handover_answer(
        &mut answer,
        Some("127.0.0.1:0".parse().expect("an address")),
        &certs,
    )
    .expect_err("an unreadable certificate refuses the handover");
    assert!(
        error.downcast_ref::<crate::FailureReported>().is_some(),
        "the refusal is already reported on stdout: {error:#}"
    );
    let mut stderr = Vec::new();
    assert_eq!(
        crate::exit_code_to(Err(error), &mut stderr),
        1,
        "the refusal exits 1"
    );
    assert!(
        stderr.is_empty(),
        "no `Error:` line: {:?}",
        String::from_utf8_lossy(&stderr)
    );
    let answer = String::from_utf8(answer).expect("a UTF-8 answer");
    let reason = answer
        .strip_prefix(&format!("{}\n", crate::daemon::EXEC_HANDOVER_CERT_REFUSED))
        .expect("the answer opens with the marker line");
    assert!(
        reason.contains(&cert.display().to_string()),
        "the reason names the unreadable certificate: {reason:?}"
    );
    assert!(!reason.contains('\n'), "the reason is one line: {reason:?}");
}

/// The built binary answers its own hidden argument through the production
/// `dispatch` -> `cmd_daemon` wiring, before anything touches `~/.clauth`:
/// the re-entered child holds the sandbox HOME with no home override, so a
/// home read panics instead of answering, and the answer's exact bytes are
/// read off the fd 1 the child points at the verdict file (the harness
/// writes its own preamble to the inherited stdout before the body runs, so
/// the pipe cannot carry the answer alone). A swapped dispatch pair makes
/// the child write the OpenAPI document there instead, which reds the byte
/// comparison; a home access reds the child's exit.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(
    unsafe_code,
    reason = "the gated child points its fd 1 at the verdict file, so the answer's bytes are read exactly"
)]
#[expect(
    clippy::exit,
    reason = "the re-entered child must end before libtest writes its result lines to fd 1, which now points at the verdict file the parent compares byte for byte"
)]
#[test]
fn the_built_binary_answers_its_hidden_argument_before_any_home_access() {
    const VERDICT_ENV: &str = "CLAUTH_TEST_HANDOVER_ANSWER_VERDICT";
    const TEST: &str = "daemon::reexec_steps_tests::the_built_binary_answers_its_hidden_argument_before_any_home_access";
    if let Some(verdict) = std::env::var_os(VERDICT_ENV) {
        use clap::Parser as _;
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::create(&verdict).expect("create the verdict file");
        // SAFETY: the re-entered child owns its descriptors, and `file` is
        // open for the swap; fd 1 becomes the verdict file the production
        // writer answers on.
        unsafe {
            libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO);
        }
        let cli = crate::cli::Cli::try_parse_from([
            "clauth",
            "daemon",
            crate::daemon::EXEC_HANDOVER_FLAG,
        ])
        .expect("the hidden argument parses");
        crate::dispatch(cli).expect("the handover answer dispatches");
        std::process::exit(0);
    }
    let home = crate::testutil::HomeSandbox::new();
    let scratch = tempfile::tempdir().expect("test scratch");
    let verdict = scratch.path().join("answer");
    // `reentry_command` nulls stdout, and the harness owns it; build the
    // command directly and keep `--nocapture`, so the answer reaches fd 1
    // instead of libtest's capture sink.
    #[cfg(target_os = "linux")]
    let exe = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(target_os = "macos")]
    let exe = std::env::current_exe().expect("the test binary");
    let mut command = std::process::Command::new(exe);
    command
        .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
        .env_clear()
        .env("HOME", home.home())
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env(VERDICT_ENV, &verdict)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn the re-entered child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                drop(child.kill());
                drop(child.wait());
                panic!("the re-entered child never answered");
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => panic!("waiting on the re-entered child failed: {e}"),
        }
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read as _;
        drop(pipe.read_to_string(&mut stderr));
    }
    assert!(
        status.success(),
        "the child answered through dispatch: {status:?}\nstderr:\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&verdict).expect("the answer landed"),
        crate::daemon::EXEC_HANDOVER_TOKEN.as_bytes(),
        "the answer is exactly the token bytes and nothing else"
    );
    assert!(
        !home.home().join(".clauth").exists(),
        "the answer touched no home: the sandbox holds no .clauth"
    );
}

/// The cert-refused line's manual-restart command names the daemon's own
/// `--listen` address, and the explicit `--cert`/`--key` pair after it, so the
/// advised `--replace` reloads the same certificate source the daemon started
/// with — the bare `--listen` form would fall back to the lego source. A
/// daemon that serves no API gets the advised command without a listener, the
/// only start that reproduces its own.
#[test]
fn the_cert_refused_line_names_the_daemons_own_restart_source() {
    let address: std::net::SocketAddr = "127.0.0.1:8443".parse().expect("an address");
    let lego = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        "bad certificate",
        &RestartAdvice {
            listen: Some(address),
            explicit: None,
        },
    );
    assert_eq!(
        lego,
        "clauth daemon: /opt/clauth cannot load the API certificate (bad certificate); \
         keeping this image running until a manual restart: \
         `clauth daemon --replace --listen 127.0.0.1:8443` after fixing the certificate"
    );
    let explicit = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        "bad certificate",
        &RestartAdvice {
            listen: Some(address),
            explicit: Some((
                std::path::PathBuf::from("/etc/tls/server.crt"),
                std::path::PathBuf::from("/etc/tls/server.key"),
            )),
        },
    );
    assert_eq!(
        explicit,
        "clauth daemon: /opt/clauth cannot load the API certificate (bad certificate); \
         keeping this image running until a manual restart: \
         `clauth daemon --replace --listen 127.0.0.1:8443 \
         --cert /etc/tls/server.crt --key /etc/tls/server.key` after fixing the certificate"
    );
    let no_listener = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        "bad certificate",
        &RestartAdvice::default(),
    );
    assert_eq!(
        no_listener,
        "clauth daemon: /opt/clauth cannot load the API certificate (bad certificate); \
         keeping this image running until a manual restart: \
         `clauth daemon --replace` after fixing the certificate"
    );
}

/// The advised restart command shell-quotes the certificate paths: a path with
/// a space pastes as the single argument it was, while a plain path keeps the
/// unquoted rendering the approved line already had.
#[test]
fn the_cert_refused_line_quotes_certificate_paths_that_need_it() {
    let line = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        "bad certificate",
        &RestartAdvice {
            listen: Some("127.0.0.1:8443".parse().expect("an address")),
            explicit: Some((
                std::path::PathBuf::from("/etc/tls/my cert.crt"),
                std::path::PathBuf::from("/etc/tls/my key.pem"),
            )),
        },
    );
    assert_eq!(
        line,
        "clauth daemon: /opt/clauth cannot load the API certificate (bad certificate); \
         keeping this image running until a manual restart: \
         `clauth daemon --replace --listen 127.0.0.1:8443 \
         --cert '/etc/tls/my cert.crt' --key '/etc/tls/my key.pem'` after fixing the certificate"
    );
}

/// The advised command quotes the `--listen` address where a shell would read
/// it as a glob: an IPv6 address carries `[`, while a plain IPv4 address keeps
/// the approved line's unquoted rendering.
#[test]
fn the_cert_refused_line_quotes_an_ipv6_listen_address() {
    let line = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        "bad certificate",
        &RestartAdvice {
            listen: Some("[::1]:8443".parse().expect("an address")),
            explicit: None,
        },
    );
    assert_eq!(
        line,
        "clauth daemon: /opt/clauth cannot load the API certificate (bad certificate); \
         keeping this image running until a manual restart: \
         `clauth daemon --replace --listen '[::1]:8443'` after fixing the certificate"
    );
}

/// The advised command passes a certificate path starting with `-` as clap's
/// `--flag=value`, because the space-separated form reads it as another flag
/// and shell quoting cannot make clap take it; the value itself is still
/// shell-quoted.
#[test]
fn the_cert_refused_line_passes_a_hyphen_leading_path_as_the_flag_value() {
    let line = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        "bad certificate",
        &RestartAdvice {
            listen: Some("127.0.0.1:8443".parse().expect("an address")),
            explicit: Some((
                std::path::PathBuf::from("-weird.crt"),
                std::path::PathBuf::from("-my key.pem"),
            )),
        },
    );
    assert_eq!(
        line,
        "clauth daemon: /opt/clauth cannot load the API certificate (bad certificate); \
         keeping this image running until a manual restart: \
         `clauth daemon --replace --listen 127.0.0.1:8443 \
         --cert=-weird.crt --key='-my key.pem'` after fixing the certificate"
    );
}

/// `RestartAdvice::from_source` reads the daemon's own start: a lego start
/// names no explicit pair, an explicit `--cert`/`--key` start carries both
/// paths whole, so the advised restart reloads the same certificate source.
#[test]
fn the_restart_advice_reads_the_daemons_own_certificate_source() {
    let address: std::net::SocketAddr = "127.0.0.1:8443".parse().expect("an address");
    let lego = RestartAdvice::from_source(Some(address), &CertSource::Lego);
    assert_eq!(
        lego.listen,
        Some(address),
        "the listen address carries over"
    );
    assert!(lego.explicit.is_none(), "a lego start names no pair");

    let cert = std::path::PathBuf::from("/etc/tls/server.crt");
    let key = std::path::PathBuf::from("/etc/tls/server.key");
    let explicit = RestartAdvice::from_source(
        Some(address),
        &CertSource::Explicit(CertPaths {
            cert: cert.clone(),
            issuer: None,
            key: key.clone(),
        }),
    );
    assert_eq!(
        explicit.listen,
        Some(address),
        "the listen address carries over"
    );
    assert_eq!(
        explicit.explicit,
        Some((cert, key)),
        "the explicit pair survives whole, in cert-then-key order"
    );
}

/// The reason's cap counts characters, not bytes: a multibyte character
/// straddling the 200th character is kept whole rather than cut mid-codepoint,
/// which would panic the daemon while logging an untrusted answer.
#[test]
fn the_cert_refused_reason_keeps_a_multibyte_character_whole_at_the_cap() {
    let reason = format!("{}{}", "a".repeat(199), "\u{20ac}".repeat(50));
    let line = super::cert_refused_line(
        std::path::Path::new("/opt/clauth"),
        &reason,
        &RestartAdvice::default(),
    );
    let kept = format!("{}\u{20ac}", "a".repeat(199));
    assert_eq!(
        line,
        format!(
            "clauth daemon: /opt/clauth cannot load the API certificate ({kept}); \
             keeping this image running until a manual restart: `clauth daemon --replace` \
             after fixing the certificate"
        ),
        "the 200th character is kept whole"
    );
}

/// The answer arm's certificate read never waits on the state flock: with the
/// state lock held against it, a lego-source answer still arrives inside the
/// capability probe's bound, as the certificate failure — never a lock wait.
/// The probe kills a child at [`super::EXEC_PROBE_BOUND`], and a cut answer is
/// a permanent stand-down for that binary, so a wait here would retire a
/// capable replacement.
///
/// A seeded `tls.json` points the lego source at an empty directory, so the
/// certificate failure is this fixture's rather than this host's
/// `/etc/lego` tree's.
#[test]
fn the_answer_does_not_wait_on_the_state_flock() {
    let _home = crate::testutil::HomeSandbox::new();
    let certs = tempfile::tempdir().expect("an empty certificate directory");
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("create the clauth dir");
    let seeded = serde_json::json!({ "schema": 1, "cert_dir": certs.path() }).to_string();
    std::fs::write(dir.join("tls.json"), &seeded).expect("seed the certificate directory");

    let held = crate::testutil::hold_state_flock();

    let started = std::time::Instant::now();
    let answer = super::listener_setup(
        Some("127.0.0.1:0".parse().expect("an address")),
        &CertSource::Lego,
    );
    let elapsed = started.elapsed();
    drop(held);

    assert!(
        elapsed < super::EXEC_PROBE_BOUND,
        "the answer must arrive inside the probe bound: {elapsed:?}"
    );
    let error = match answer {
        Ok(_) => panic!("the seeded certificate directory holds no certificate"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("failed to read the TLS certificate"),
        "the answer is the certificate read, not a state-lock wait: {error}"
    );
    assert!(
        error.contains(&certs.path().display().to_string()),
        "the read took the seeded certificate directory: {error}"
    );
}
