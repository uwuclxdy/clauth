#![allow(
    unsafe_code,
    reason = "the handover tests make the raw libc calls an exec handover needs, which safe std cannot express"
)]
#![allow(
    clippy::expect_used,
    reason = "helpers outside #[test] fns expect on fixtures, and this cfg(all(test, unix)) mod gets no cfg(test) exemption"
)]
//! The exec handover's tests: the env-carried descriptors the replaced image
//! leaves are validated (refused when wrong, adopted when right), the flock
//! and the bound REST listener hold across a real exec driven through the
//! re-entry harness, and a competitor hammering the singleton or the port
//! across the exec never wins. The exec-level tests re-enter this binary:
//! stage 1 claims/registers and execs stage 2, whose test result IS the
//! verdict. A re-entered child runs under a sandbox HOME the parent sets, so
//! nothing here touches the real `~/.clauth`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::process::CommandExt as _;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

/// The env var naming the exec-level test's stage (absent in the suite run).
#[cfg(any(target_os = "linux", target_os = "macos"))]
const HANDOVER_STAGE_ENV: &str = "CLAUTH_TEST_HANDOVER_STAGE";
/// The env var naming the competitor's mode: "flock" hammers the singleton
/// lock, "bind" hammers the REST port.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COMPETITOR_MODE_ENV: &str = "CLAUTH_TEST_HANDOVER_COMPETITOR_MODE";
/// The env var naming the shared dir the competitor writes its markers into.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const VERDICT_DIR_ENV: &str = "CLAUTH_TEST_HANDOVER_VERDICT_DIR";
/// The env var naming the lock path the competitor hammers.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const HANDOVER_LOCK_ENV: &str = "CLAUTH_TEST_HANDOVER_LOCK_PATH";
/// The env var naming the bound REST address the competitor hammers.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const HANDOVER_LISTEN_ENV: &str = "CLAUTH_TEST_HANDOVER_LISTEN";
/// The env var naming the competitor's pid, so stage 2 can reap it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COMPETITOR_PID_ENV: &str = "CLAUTH_TEST_HANDOVER_COMPETITOR_PID";
/// The competitor's re-entry target.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COMPETITOR_TEST: &str = "daemon::handover_tests::handover_competitor_hammer";
/// The env vars naming the TLS fixture's cert and key, carried into the
/// re-entered `serve` child.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const HANDOVER_CERT_ENV: &str = "CLAUTH_TEST_HANDOVER_CERT";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const HANDOVER_KEY_ENV: &str = "CLAUTH_TEST_HANDOVER_KEY";

/// The pause between two of the competitor's takes. A zero-sleep bind loop
/// (measured 2026-11-11 on macOS 27 as an optimized standalone copy: ~1.4M
/// socket creations a second) makes macOS refuse new sockets box-wide
/// (`ENOBUFS`) for ~0.15-0.3 s after it stops, once for ~2.5 s, which reds
/// the next test's first bind. 100 µs caps a loop near 10k takes a second,
/// below the 80k a second that already drew refusals, while a resource the
/// restart leaves free for longer than one pause is still taken and reds the
/// run. The flock loop takes the same pause.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COMPETITOR_BACKOFF: Duration = Duration::from_micros(100);

/// The competitor re-entry target: with the mode env absent (the normal
/// suite run) it passes instantly; with it present it hammers the singleton
/// flock (or the REST port) across the stage-1→2 exec and writes `stolen`
/// the instant a take succeeds — stage 2 asserts that file is absent, so any
/// instant of the restart where the resource was free reds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn handover_competitor_hammer() {
    let Some(mode) = std::env::var_os(COMPETITOR_MODE_ENV) else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os(VERDICT_DIR_ENV).expect("the verdict dir"));
    std::fs::write(dir.join("started"), b"").expect("the started marker");
    let deadline = Instant::now() + Duration::from_secs(5);
    let stolen = match mode.to_str() {
        Some("flock") => {
            let path = PathBuf::from(std::env::var_os(HANDOVER_LOCK_ENV).expect("the lock path"));
            let mut stolen = false;
            while Instant::now() < deadline {
                // Held by the daemon image: the normal read. A broken open
                // or lock never reads as a steal.
                if matches!(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .map(|f| f.try_lock()),
                    Ok(Ok(()))
                ) {
                    stolen = true;
                    break;
                }
                std::thread::sleep(COMPETITOR_BACKOFF);
            }
            stolen
        }
        Some("bind") => {
            let addr: std::net::SocketAddr = std::env::var(HANDOVER_LISTEN_ENV)
                .expect("the bound address")
                .parse()
                .expect("the address parses");
            let mut stolen = false;
            while Instant::now() < deadline {
                if std::net::TcpListener::bind(addr).is_ok() {
                    stolen = true;
                    break;
                }
                std::thread::sleep(COMPETITOR_BACKOFF);
            }
            stolen
        }
        // Take the singleton's flock and park, so the parent poses a
        // handover whose planted lock descriptor names the right file while
        // ANOTHER process holds the flock (the handover lied). The park's
        // deadline bounds a red that never reaps the child.
        Some("hold") => {
            let path = PathBuf::from(std::env::var_os(HANDOVER_LOCK_ENV).expect("the lock path"));
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .expect("open the lock file");
            lock.try_lock().expect("the holder takes the singleton");
            std::fs::write(dir.join("holding"), b"").expect("the holding marker");
            let park_deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < park_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }
        Some(other) => panic!("unknown competitor mode {other}"),
        None => panic!("the competitor mode is not UTF-8"),
    };
    if stolen {
        std::fs::write(dir.join("stolen"), b"").expect("the stolen marker");
    }
    std::fs::write(dir.join("finished"), b"").expect("the finished marker");
}

/// Wait until the competitor wrote `name` under `dir`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_marker(dir: &Path, name: &str, who: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !dir.join(name).exists() {
        assert!(Instant::now() < deadline, "the {who} never wrote {name}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn the stage-1 image of `test`: a re-entered copy of this binary under
/// a sandbox HOME, with the verdict dir passed through and the stages' own
/// output routed to `stage_log` (the exec'd next stage inherits both
/// streams), so a red carries the inner assertion the stage panicked on,
/// never only its exit status.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_stage(
    test: &str,
    home: &Path,
    verdict_dir: &Path,
    stage_log: &Path,
) -> std::process::Child {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stage_log)
        .expect("stage log");
    let mut cmd = crate::testutil::reentry_command(test, &["daemon"]);
    cmd.env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env(HANDOVER_STAGE_ENV, "1")
        .env(VERDICT_DIR_ENV, verdict_dir)
        .stdout(std::process::Stdio::from(
            log.try_clone().expect("clone the stage log"),
        ))
        .stderr(std::process::Stdio::from(log));
    cmd.spawn().expect("spawn the stage-1 child")
}

/// Wait out `child` (bounded, killing it on a red) and assert it passed,
/// carrying the stages' own output into the red.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_stage_passes(mut child: std::process::Child, stage_log: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                drop(child.kill());
                drop(child.wait());
                panic!("the {what} child never finished");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("waiting on the {what} child failed: {e}"),
        }
    };
    assert!(
        status.success(),
        "the {what} child passed: {status:?}\nstage output:\n{}",
        std::fs::read_to_string(stage_log).unwrap_or_default()
    );
}

/// Exec this same binary forward into `stage`, carrying `extra` env pairs
/// and the prepared handover named on the command's env. Linux spawns
/// through `/proc/self/exe` (the same choice `testutil::reentry_command`
/// makes, so the running inode execs even after a rebuild); macOS keeps
/// `current_exe()`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exec_self_forward(
    stage: &str,
    extra: &[(&str, &str)],
    handover: super::exec_handover::Handover,
) -> ! {
    #[cfg(target_os = "linux")]
    let exe = PathBuf::from("/proc/self/exe");
    #[cfg(target_os = "macos")]
    let exe = std::env::current_exe().expect("the test binary");
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(HANDOVER_STAGE_ENV, stage);
    for (key, value) in extra {
        cmd.env(key, value);
    }
    super::exec_handover::apply_handover(&mut cmd, handover);
    let err = cmd.exec();
    panic!("the exec failed: {err}");
}

/// Point `home_dir()` at the sandbox HOME the suite parent set: under
/// `cfg(test)` the resolver accepts only its override, so a re-entered
/// stage (a fresh process with no sandbox held) sets it before any
/// `~/.clauth` path is built. Each exec'd stage re-sets it — the override
/// does not survive the exec.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sandbox_home() {
    let home = std::env::var_os("HOME").expect("the parent set HOME");
    crate::profile::set_home_override(PathBuf::from(home));
}

/// Reap the competitor this image spawned (the exec keeps the pid, so the
/// stage-2 image is its parent) so no stage process lingers.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reap_competitor() {
    let pid: libc::pid_t = std::env::var(COMPETITOR_PID_ENV)
        .expect("the competitor pid")
        .parse()
        .expect("the competitor pid parses");
    // SAFETY: the pid was recorded at the competitor's spawn in this same
    // process (the exec keeps the pid); waitpid with a null status pointer
    // writes no memory of ours and blocks only until this image's own child
    // exits.
    let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
    assert_eq!(reaped, pid, "the competitor was reaped");
}

/// The singleton's flock holds across the exec end to end: a stage-1 image
/// claims the singleton, a competitor hammers `clauthd.lock` across the
/// exec, and the stage-2 image adopts the inherited descriptor — the same
/// device and inode as the lock file, the flock still held, and the
/// competitor never once took it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_singleton_flock_holds_across_the_exec() {
    match std::env::var(HANDOVER_STAGE_ENV).ok().as_deref() {
        None => {
            let _home = crate::testutil::HomeSandbox::new();
            let child_home = tempfile::tempdir().expect("child home");
            let scratch = tempfile::tempdir().expect("test scratch");
            let verdict_dir = scratch.path().join("verdicts");
            std::fs::create_dir_all(&verdict_dir).expect("verdict dir");
            let stage_log = scratch.path().join("stage.log");
            let child = spawn_stage(
                "daemon::handover_tests::the_singleton_flock_holds_across_the_exec",
                child_home.path(),
                &verdict_dir,
                &stage_log,
            );
            assert_stage_passes(child, &stage_log, "flock handover");
        }
        Some("1") => {
            sandbox_home();
            let dir = crate::profile::clauth_dir().expect("the clauth dir");
            crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
            let claim = super::probe::claim_singleton(&dir, false).expect("claim the singleton");
            let super::probe::Claim::Active(_lock) = claim else {
                panic!("the claim must win on a fresh dir");
            };
            // `_lock` lives until the exec: it owns the fd the handover
            // clears close-on-exec on.
            let verdict_dir =
                PathBuf::from(std::env::var_os(VERDICT_DIR_ENV).expect("the verdict dir"));
            let lock_path = dir.join(super::LOCK_FILE);
            let mut competitor = crate::testutil::reentry_command(COMPETITOR_TEST, &[]);
            competitor
                .env(COMPETITOR_MODE_ENV, "flock")
                .env(VERDICT_DIR_ENV, &verdict_dir)
                .env(HANDOVER_LOCK_ENV, &lock_path);
            #[expect(
                clippy::zombie_processes,
                reason = "the exec'd stage-2 image reaps it by pid"
            )]
            let competitor = competitor.spawn().expect("spawn the competitor");
            // Recorded at spawn: stage 2 reaps it by pid, and a competitor
            // whose pid was never captured fails closed, never lingers.
            let competitor_pid = competitor.id().to_string();
            wait_for_marker(&verdict_dir, "started", "competitor");
            let handover = super::exec_handover::prepare_for_exec();
            let lock_path = lock_path.to_string_lossy().into_owned();
            exec_self_forward(
                "2",
                &[
                    (HANDOVER_LOCK_ENV, lock_path.as_str()),
                    (COMPETITOR_PID_ENV, competitor_pid.as_str()),
                ],
                handover,
            );
        }
        Some("2") => {
            use std::os::unix::fs::MetadataExt as _;
            sandbox_home();
            let dir = crate::profile::clauth_dir().expect("the clauth dir");
            let lock_path =
                PathBuf::from(std::env::var_os(HANDOVER_LOCK_ENV).expect("the lock path"));
            let adopted = super::exec_handover::take_at_start(&dir, None);
            let Some(lock) = adopted else {
                panic!("the new image must adopt the inherited lock descriptor")
            };
            let inherited = lock.metadata().expect("fstat the adopted lock");
            let expected = std::fs::metadata(&lock_path).expect("stat the lock path");
            assert_eq!(
                (inherited.dev(), inherited.ino()),
                (expected.dev(), expected.ino()),
                "the adopted descriptor is the lock file, same device and inode"
            );
            // The flock rides the inherited open file description: a fresh
            // open cannot take it, and the inherited fd still holds it.
            let fresh = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .expect("reopen the lock file");
            assert!(
                matches!(fresh.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
                "the flock is still held across the exec"
            );
            lock.try_lock()
                .expect("the inherited fd still holds the flock");
            // The competitor hammered across the exec and never once took
            // the singleton.
            let verdict_dir =
                PathBuf::from(std::env::var_os(VERDICT_DIR_ENV).expect("the verdict dir"));
            wait_for_marker(&verdict_dir, "finished", "competitor");
            assert!(
                !verdict_dir.join("stolen").exists(),
                "the competitor never took the singleton across the exec"
            );
            reap_competitor();
        }
        Some(stage) => panic!("unknown handover stage {stage:?}"),
    }
}

/// The bound REST listener holds across the exec end to end: a stage-1
/// image binds a loopback listener and registers it, a competitor hammers a
/// fresh bind of the same address across the exec, and the stage-2 image
/// adopts the inherited socket — still bound to the same address, a fresh
/// bind refused, and an accept through it answers a real connection.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_rest_listener_stays_bound_across_the_exec() {
    match std::env::var(HANDOVER_STAGE_ENV).ok().as_deref() {
        None => {
            let _home = crate::testutil::HomeSandbox::new();
            let child_home = tempfile::tempdir().expect("child home");
            let scratch = tempfile::tempdir().expect("test scratch");
            let verdict_dir = scratch.path().join("verdicts");
            std::fs::create_dir_all(&verdict_dir).expect("verdict dir");
            let stage_log = scratch.path().join("stage.log");
            let child = spawn_stage(
                "daemon::handover_tests::the_rest_listener_stays_bound_across_the_exec",
                child_home.path(),
                &verdict_dir,
                &stage_log,
            );
            assert_stage_passes(child, &stage_log, "listener handover");
        }
        Some("1") => {
            use std::os::fd::AsRawFd as _;
            sandbox_home();
            let listener =
                std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the listener");
            let addr = listener.local_addr().expect("the bound address");
            super::exec_handover::register_listener(listener.as_raw_fd());
            // `listener` lives until the exec: it owns the fd the handover
            // clears close-on-exec on.
            let verdict_dir =
                PathBuf::from(std::env::var_os(VERDICT_DIR_ENV).expect("the verdict dir"));
            let addr_str = addr.to_string();
            let mut competitor = crate::testutil::reentry_command(COMPETITOR_TEST, &[]);
            competitor
                .env(COMPETITOR_MODE_ENV, "bind")
                .env(VERDICT_DIR_ENV, &verdict_dir)
                .env(HANDOVER_LISTEN_ENV, &addr_str);
            #[expect(
                clippy::zombie_processes,
                reason = "the exec'd stage-2 image reaps it by pid"
            )]
            let competitor = competitor.spawn().expect("spawn the competitor");
            let competitor_pid = competitor.id().to_string();
            wait_for_marker(&verdict_dir, "started", "competitor");
            let handover = super::exec_handover::prepare_for_exec();
            exec_self_forward(
                "2",
                &[
                    (HANDOVER_LISTEN_ENV, addr_str.as_str()),
                    (COMPETITOR_PID_ENV, competitor_pid.as_str()),
                ],
                handover,
            );
        }
        Some("2") => {
            sandbox_home();
            let dir = crate::profile::clauth_dir().expect("the clauth dir");
            let addr: std::net::SocketAddr = std::env::var(HANDOVER_LISTEN_ENV)
                .expect("the bound address")
                .parse()
                .expect("the address parses");
            let handover = super::exec_handover::take_at_start(&dir, Some(addr));
            assert!(
                handover.is_none(),
                "no lock was handed over, and nothing may be rejected"
            );
            let listener = super::exec_handover::take_listener(addr).expect("the adopted listener");
            assert_eq!(
                listener.local_addr().expect("getsockname"),
                addr,
                "the adopted socket is still bound to the configured address"
            );
            assert!(
                matches!(
                    std::net::TcpListener::bind(addr).map(|_| ()),
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
                ),
                "the port is still held by the inherited socket"
            );
            // A real accept through the inherited socket: a connector writes
            // one probe, the adopted listener accepts and reads it back.
            let connector = std::thread::spawn(move || {
                let mut stream = std::net::TcpStream::connect(addr).expect("connect to the port");
                std::io::Write::write_all(&mut stream, b"hi").expect("write the probe");
            });
            listener.set_nonblocking(true).expect("nonblocking accept");
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut stream, _peer) = loop {
                match listener.accept() {
                    Ok(accepted) => break accepted,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "the adopted listener never accepted"
                        );
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => panic!("accepting on the adopted listener failed: {e}"),
                }
            };
            let mut probe = [0u8; 2];
            std::io::Read::read_exact(&mut stream, &mut probe).expect("read the probe");
            assert_eq!(
                &probe, b"hi",
                "the new image accepts on the inherited socket"
            );
            connector.join().expect("join the connector");
            // The competitor hammered a fresh bind across the exec and never
            // once won the port.
            let verdict_dir =
                PathBuf::from(std::env::var_os(VERDICT_DIR_ENV).expect("the verdict dir"));
            wait_for_marker(&verdict_dir, "finished", "competitor");
            assert!(
                !verdict_dir.join("stolen").exists(),
                "the competitor never bound the port across the exec"
            );
            reap_competitor();
        }
        Some(stage) => panic!("unknown handover stage {stage:?}"),
    }
}

/// `serve`'s OWN `take_at_start` call adopts the handed-over pair: a
/// stage-1 image claims the singleton and binds a loopback listener, the
/// exec into stage 2 carries both descriptors, and stage 2 runs the real
/// [`super::serve`] with the pair in its env. The adopted socket answers a
/// real TLS handshake and the child's log holds no refusal line, so the
/// adoption ran through serve's own take — a take deleted from `serve`
/// (or a fresh claim in its place) reads the inherited flock as redundant
/// and exits before the port ever answers. Skipped where `openssl` cannot
/// build the certificate fixture, like every TLS test.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_handed_over_pair_is_adopted_by_serves_own_take() {
    match std::env::var(HANDOVER_STAGE_ENV).ok().as_deref() {
        None => {
            use std::os::unix::process::ExitStatusExt as _;

            let _home = crate::testutil::HomeSandbox::new();
            let child_home = tempfile::tempdir().expect("child home");
            let scratch = tempfile::tempdir().expect("test scratch");
            let Some((paths, ca_crt)) =
                super::api::tests::generate_chain(scratch.path()).expect("the chain")
            else {
                eprintln!(
                    "SKIPPED a_handed_over_pair_is_adopted_by_serves_own_take: openssl is not usable here"
                );
                return;
            };
            let verdict_dir = scratch.path().join("verdicts");
            std::fs::create_dir_all(&verdict_dir).expect("verdict dir");
            let stage_log = scratch.path().join("stage.log");
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stage_log)
                .expect("stage log");
            let mut cmd = crate::testutil::reentry_command(
                "daemon::handover_tests::a_handed_over_pair_is_adopted_by_serves_own_take",
                &["daemon"],
            );
            cmd.env_clear()
                .env("HOME", child_home.path())
                .env("PATH", "/usr/bin:/bin")
                .env("LC_ALL", "C")
                .env("CLAUTH_NO_UPDATE", "1")
                .env(HANDOVER_STAGE_ENV, "1")
                .env(VERDICT_DIR_ENV, &verdict_dir)
                .env(HANDOVER_CERT_ENV, &paths.cert)
                .env(HANDOVER_KEY_ENV, &paths.key)
                .stdout(std::process::Stdio::from(
                    log.try_clone().expect("clone the stage log"),
                ))
                .stderr(std::process::Stdio::from(log));
            // `Owned` kills and reaps the child on a red before the stop
            // loop, so a failed readiness wait leaks no live daemon.
            let mut child = crate::testutil::Owned(cmd.spawn().expect("spawn the serve child"));
            // The adopted port answering a real TLS handshake is the
            // readiness signal: only an adopted listener serves there (a
            // fresh claim exits, a fresh bind loses to the inherited
            // socket). A child that exits first reds with its log.
            wait_for_marker(&verdict_dir, "addr", "the serve child");
            let addr: std::net::SocketAddr = std::fs::read_to_string(verdict_dir.join("addr"))
                .expect("the bound address")
                .trim()
                .parse()
                .expect("the address parses");
            let client = super::api::tests::client_config(&ca_crt).expect("the client config");
            let name = rustls::pki_types::ServerName::try_from(super::api::tests::SERVER_NAME)
                .expect("the server name")
                .to_owned();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match child.0.try_wait() {
                    Ok(Some(status)) => panic!(
                        "the serve child exited before the adopted port answered: {status:?}\nstage output:\n{}",
                        std::fs::read_to_string(&stage_log).unwrap_or_default()
                    ),
                    Ok(None) => {}
                    Err(e) => panic!("waiting on the serve child failed: {e}"),
                }
                if answered(&client, &name, addr) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the adopted port never answered\nstage output:\n{}",
                    std::fs::read_to_string(&stage_log).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            let log_text = std::fs::read_to_string(&stage_log).unwrap_or_default();
            assert!(
                !log_text.contains("failed validation"),
                "no descriptor was refused: {log_text}"
            );
            // A clean stop: SIGTERM runs the daemon's own stop path, and
            // the child dies of that signal (a SIGKILL on the deadline
            // would red the signal assert).
            let pid = child.0.id();
            let killed = std::process::Command::new("kill")
                .args(["-s", "TERM", &pid.to_string()])
                .status()
                .expect("send SIGTERM");
            assert!(killed.success(), "the SIGTERM was sent");
            let status = loop {
                match child.0.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() >= deadline => {
                        drop(child.0.kill());
                        drop(child.0.wait());
                        panic!("the serve child never died of the SIGTERM");
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(e) => panic!("waiting on the serve child failed: {e}"),
                }
            };
            assert_eq!(
                status.signal(),
                Some(libc::SIGTERM),
                "the adopted daemon died of the SIGTERM, not before: {status:?}"
            );
        }
        Some("1") => {
            sandbox_home();
            let dir = crate::profile::clauth_dir().expect("the clauth dir");
            crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
            let claim = super::probe::claim_singleton(&dir, false).expect("claim the singleton");
            let super::probe::Claim::Active(_lock) = claim else {
                panic!("the claim must win on a fresh dir");
            };
            // `_lock` lives until the exec: it owns the lock fd, which the
            // claim already registered for the handover, and the handover
            // clears close-on-exec on it.
            let listener =
                std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the listener");
            let addr = listener.local_addr().expect("the bound address");
            super::exec_handover::register_listener({
                use std::os::fd::AsRawFd as _;
                listener.as_raw_fd()
            });
            let verdict_dir =
                PathBuf::from(std::env::var_os(VERDICT_DIR_ENV).expect("the verdict dir"));
            std::fs::write(verdict_dir.join("addr"), addr.to_string()).expect("the addr marker");
            let handover = super::exec_handover::prepare_for_exec();
            let addr_str = addr.to_string();
            exec_self_forward("2", &[(HANDOVER_LISTEN_ENV, addr_str.as_str())], handover);
        }
        Some("2") => {
            sandbox_home();
            let cert = PathBuf::from(std::env::var_os(HANDOVER_CERT_ENV).expect("the cert path"));
            let key = PathBuf::from(std::env::var_os(HANDOVER_KEY_ENV).expect("the key path"));
            let addr: std::net::SocketAddr = std::env::var(HANDOVER_LISTEN_ENV)
                .expect("the bound address")
                .parse()
                .expect("the address parses");
            // A SIGKILLed fixture parent runs no `Drop`: watch the pid this
            // child started under and exit once the reparent moves it, so
            // no `serve` process outlives its fixture parent.
            let parent = std::os::unix::process::parent_id();
            let _watcher =
                std::thread::spawn(move || crate::testutil::exit_when_reparented(parent));
            let certs = super::api::tls::CertSource::Explicit(super::api::tls::CertPaths {
                cert,
                issuer: None,
                key,
            });
            drop(super::serve(
                super::StartMode::ExitIfRunning,
                Some(addr),
                &certs,
            ));
        }
        Some(stage) => panic!("unknown handover stage {stage:?}"),
    }
}

/// One TLS answer through `addr`: a full request that came back with an
/// HTTP response head. A refused connection, a stalled handshake or a
/// non-HTTP answer reads false. The read deadline bounds a handshake
/// against a socket nobody accepts yet (the child booting or gone).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn answered(
    client: &std::sync::Arc<rustls::ClientConfig>,
    name: &rustls::pki_types::ServerName<'static>,
    addr: std::net::SocketAddr,
) -> bool {
    use std::io::{Read as _, Write as _};
    let Ok(sock) = std::net::TcpStream::connect(addr) else {
        return false;
    };
    if sock.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
        return false;
    }
    let Ok(conn) = rustls::ClientConnection::new(std::sync::Arc::clone(client), name.clone())
    else {
        return false;
    };
    let mut stream = rustls::StreamOwned::new(conn, sock);
    if stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    drop(stream.flush());
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk) {
            Ok(n) if n > 0 => {
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            _ => return false,
        }
    }
    String::from_utf8_lossy(&head).starts_with("HTTP/1.1 ")
}

/// A planted lock descriptor (an open scratch file, not `clauthd.lock`) is
/// refused: the handover env is removed at once, the planted fd is left
/// open and re-marked close-on-exec (the handover had cleared the flag), one
/// daemon line names the fallback, and the fresh claim then takes the
/// singleton exactly as a first start would.
#[cfg(unix)]
#[test]
fn a_planted_lock_descriptor_is_refused_and_the_fresh_claim_runs() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    // The test keeps the handle: a refused number is left open, so the
    // descriptor stays the test's to close.
    let planted =
        std::fs::File::create(scratch.path().join("not-the-lock")).expect("the planted fd");
    let planted_fd = {
        use std::os::fd::AsRawFd as _;
        planted.as_raw_fd()
    };
    // The exec's shape: the image registered the number and the handover
    // cleared close-on-exec on it, so only the refusal's re-mark puts the
    // flag back.
    super::exec_handover::register_lock(planted_fd);
    super::exec_handover::prepare_for_exec();
    assert!(
        !cloexec(planted_fd),
        "the handover cleared close-on-exec on the registered lock"
    );
    let planted_var = planted_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LOCK_FD_ENV,
            Some(std::ffi::OsStr::new(&planted_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let re_marks = super::exec_handover::refused_remarks();
    let handover = super::exec_handover::take_at_start(&dir, None);
    drop(capture);
    assert!(
        handover.is_none(),
        "the planted descriptor is refused, never adopted"
    );
    assert!(
        planted.metadata().is_ok(),
        "the refused descriptor is left open and untouched"
    );
    assert!(
        cloexec(planted_fd),
        "the refused lock descriptor is re-marked close-on-exec"
    );
    assert_eq!(
        super::exec_handover::refused_remarks(),
        re_marks + 1,
        "the refusal re-marked the open number once"
    );
    super::exec_handover::reset();
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the lock descriptor handed over across the re-exec failed validation; claiming the singleton afresh"
                .to_string()
        ],
        "one daemon line names the lock's fallback"
    );
    assert!(
        std::env::var_os(super::exec_handover::LOCK_FD_ENV).is_none(),
        "the handover env is removed at once, so no child inherits it"
    );
    // The fallback is today's fresh claim: nothing holds the singleton, so
    // the claim takes it.
    let claim = super::probe::claim_singleton(&dir, false).expect("the fresh claim");
    assert!(
        matches!(claim, super::probe::Claim::Active(_)),
        "the fresh claim runs"
    );
}

/// A planted listener descriptor (a regular file, not a socket) is refused
/// the same way: left open, one daemon line, nothing adopted, and the
/// configured address binds afresh.
#[cfg(unix)]
#[test]
fn a_planted_listener_descriptor_is_refused_and_the_port_binds_afresh() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    let planted =
        std::fs::File::create(scratch.path().join("not-a-socket")).expect("the planted fd");
    let planted_fd = {
        use std::os::fd::AsRawFd as _;
        planted.as_raw_fd()
    };
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], 0).into();
    let planted_var = planted_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LISTEN_FD_ENV,
            Some(std::ffi::OsStr::new(&planted_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let handover = super::exec_handover::take_at_start(&dir, Some(addr));
    drop(capture);
    assert!(
        handover.is_none(),
        "the planted listener is refused, never adopted"
    );
    assert!(
        planted.metadata().is_ok(),
        "the refused descriptor is left open and untouched"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the listener descriptor handed over across the re-exec failed validation; binding the REST listener afresh"
                .to_string()
        ],
        "one daemon line names the listener's fallback"
    );
    assert!(
        super::exec_handover::take_listener(addr).is_none(),
        "no listener was adopted"
    );
    let listener = std::net::TcpListener::bind(addr).expect("the fresh bind");
    drop(listener);
}

/// A listener number handed to a start with no `--listen` configured is
/// refused with a line that promises no bind: this start serves no REST
/// listener at all.
#[cfg(unix)]
#[test]
fn a_listener_number_without_a_configured_listen_logs_that_no_listener_runs() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    let planted =
        std::fs::File::create(scratch.path().join("not-a-socket")).expect("the planted fd");
    let planted_fd = {
        use std::os::fd::AsRawFd as _;
        planted.as_raw_fd()
    };
    let planted_var = planted_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LISTEN_FD_ENV,
            Some(std::ffi::OsStr::new(&planted_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let handover = super::exec_handover::take_at_start(&dir, None);
    drop(capture);
    assert!(handover.is_none(), "nothing is adopted without a listen");
    assert!(
        planted.metadata().is_ok(),
        "the refused descriptor is left open and untouched"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the listener descriptor handed over across the re-exec failed validation; no REST listener is configured for this start"
                .to_string()
        ],
        "the line promises no bind this start never runs"
    );
}

/// A planted stdio number (stderr) is refused before ownership: the
/// descriptor is left open, so the daemon's log (stderr IS daemon.log)
/// stays writable after the refusal.
#[cfg(unix)]
#[test]
fn a_planted_stdio_descriptor_is_refused_and_stderr_stays_writable() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LOCK_FD_ENV,
            Some(std::ffi::OsStr::new("2")),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let handover = super::exec_handover::take_at_start(&dir, None);
    drop(capture);
    assert!(
        handover.is_none(),
        "the planted stdio number is refused, never adopted"
    );
    // fd 2 still answers writes (through libc, never `io::stderr`, whose
    // writes the harness captures before they reach the fd): an adoption
    // that closed it would take the daemon's log with it.
    let probe = b"planted 2 survives\n";
    // SAFETY: fd 2 is this process's stderr; the write touches nothing
    // else.
    let written = unsafe { libc::write(2, probe.as_ptr().cast::<libc::c_void>(), probe.len()) };
    assert_eq!(written as usize, probe.len(), "stderr is still writable");
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the lock descriptor handed over across the re-exec failed validation; claiming the singleton afresh"
                .to_string()
        ],
        "one daemon line names the lock's fallback"
    );
}

/// A listener number equal to the adopted lock's descriptor is refused
/// without touching it: the adopted lock's handle keeps the flock held.
#[cfg(unix)]
#[test]
fn a_listener_number_matching_the_adopted_lock_is_refused_and_the_flock_holds() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    // Owned by the handover alone: a claim's handle would double-own the fd
    // once `take_at_start` adopts it.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(super::LOCK_FILE))
        .expect("open the lock file");
    lock.try_lock().expect("take the flock");
    let lock_fd = {
        use std::os::fd::IntoRawFd as _;
        lock.into_raw_fd()
    };
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], 0).into();
    let lock_var = lock_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                super::exec_handover::LOCK_FD_ENV,
                Some(std::ffi::OsStr::new(&lock_var)),
            ),
            (
                super::exec_handover::LISTEN_FD_ENV,
                Some(std::ffi::OsStr::new(&lock_var)),
            ),
        ],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let adopted = super::exec_handover::take_at_start(&dir, Some(addr));
    drop(capture);
    assert!(adopted.is_some(), "the lock descriptor was adopted");
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the listener descriptor handed over across the re-exec failed validation; binding the REST listener afresh"
                .to_string()
        ],
        "one daemon line names the listener's fallback"
    );
    assert!(
        super::exec_handover::take_listener(addr).is_none(),
        "no listener was adopted from the lock's number"
    );
    // The lock's handle was never dropped: the flock still rides the
    // adopted description.
    let fresh = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(super::LOCK_FILE))
        .expect("reopen the lock file");
    assert!(
        matches!(fresh.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "the flock is still held after the refusal"
    );
    super::exec_handover::reset();
}

/// A planted number that is not an open descriptor is refused without a
/// panic: the validation runs through libc on the raw number (EBADF),
/// never through an ownership-taking `from_raw_fd`, and the refusal skips
/// the close-on-exec re-mark — a closed number has no flag to restore.
#[cfg(unix)]
#[test]
fn a_planted_closed_descriptor_is_refused_without_a_panic() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    let closed_fd = {
        use std::os::fd::IntoRawFd as _;
        std::fs::File::create(scratch.path().join("closed"))
            .expect("the file to close")
            .into_raw_fd()
    };
    let closed_var = closed_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LOCK_FD_ENV,
            Some(std::ffi::OsStr::new(&closed_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    // SAFETY: the descriptor is ours and open; closing it frees the number
    // for the plant below, and only the short window to `take_at_start`
    // lets another thread recycle it.
    unsafe {
        libc::close(closed_fd);
    }
    let re_marks = super::exec_handover::refused_remarks();
    let handover = super::exec_handover::take_at_start(&dir, None);
    drop(capture);
    assert!(
        handover.is_none(),
        "the closed number is refused, never adopted"
    );
    assert_eq!(
        super::exec_handover::refused_remarks(),
        re_marks,
        "a closed number is never re-marked close-on-exec"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the lock descriptor handed over across the re-exec failed validation; claiming the singleton afresh"
                .to_string()
        ],
        "one daemon line names the lock's fallback"
    );
}

/// A closed number planted as the listener is refused on its probe's
/// `EBADF` too, with no close-on-exec re-mark: the skip lives in both
/// refusal arms, not only the lock's.
#[cfg(unix)]
#[test]
fn a_planted_closed_listener_number_is_refused_without_a_remark() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    let closed_fd = {
        use std::os::fd::IntoRawFd as _;
        std::fs::File::create(scratch.path().join("closed-listener"))
            .expect("the file to close")
            .into_raw_fd()
    };
    let closed_var = closed_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LISTEN_FD_ENV,
            Some(std::ffi::OsStr::new(&closed_var)),
        )],
    );
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], 0).into();
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    // SAFETY: as in the closed lock test: ours and open until here.
    unsafe {
        libc::close(closed_fd);
    }
    let re_marks = super::exec_handover::refused_remarks();
    let handover = super::exec_handover::take_at_start(&dir, Some(addr));
    drop(capture);
    assert!(
        handover.is_none(),
        "the closed listener number is refused, never adopted"
    );
    assert_eq!(
        super::exec_handover::refused_remarks(),
        re_marks,
        "a closed listener number is never re-marked close-on-exec"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the listener descriptor handed over across the re-exec failed validation; binding the REST listener afresh"
                .to_string()
        ],
        "one daemon line names the listener's fallback"
    );
}

/// Whether `fd` is close-on-exec, read with `F_GETFD` through libc.
#[cfg(unix)]
fn cloexec(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: `fd` is an open descriptor this process owns; F_GETFD only
    // reads its descriptor flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags & libc::FD_CLOEXEC != 0
}

/// Adopted descriptors are close-on-exec again once `take_at_start` owns
/// them: the handover cleared the flag for the exec, and the re-mark
/// keeps the inherited fds out of this image's children.
#[cfg(unix)]
#[test]
fn adopted_descriptors_are_close_on_exec_again_after_take_at_start() {
    use std::os::fd::IntoRawFd as _;
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    // Owned by the handover alone (see the matching-number test).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(super::LOCK_FILE))
        .expect("open the lock file");
    lock.try_lock().expect("take the flock");
    let lock_fd = lock.into_raw_fd();
    super::exec_handover::register_lock(lock_fd);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the listener");
    let addr = listener.local_addr().expect("the bound address");
    let listen_fd = listener.into_raw_fd();
    super::exec_handover::register_listener(listen_fd);
    super::exec_handover::prepare_for_exec();
    assert!(
        !cloexec(lock_fd) && !cloexec(listen_fd),
        "the handover cleared close-on-exec on both registered fds"
    );
    let lock_var = lock_fd.to_string();
    let listen_var = listen_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[
            (
                super::exec_handover::LOCK_FD_ENV,
                Some(std::ffi::OsStr::new(&lock_var)),
            ),
            (
                super::exec_handover::LISTEN_FD_ENV,
                Some(std::ffi::OsStr::new(&listen_var)),
            ),
        ],
    );
    let adopted = super::exec_handover::take_at_start(&dir, Some(addr));
    assert!(adopted.is_some(), "the lock descriptor was adopted");
    assert!(
        cloexec(lock_fd),
        "adoption re-marks the lock descriptor close-on-exec"
    );
    assert!(
        cloexec(listen_fd),
        "adoption re-marks the listener descriptor close-on-exec"
    );
    assert!(
        super::exec_handover::take_listener(addr).is_some(),
        "the listener was adopted"
    );
    super::exec_handover::reset();
}

/// A refused handover descriptor is re-marked close-on-exec: the handover
/// cleared the flag for the exec, the refusal leaves the descriptor open,
/// and the re-mark keeps the refused fd out of this image's children. The
/// refused shape: `--listen` names the handed-over socket's own port on
/// another address (every interface, while the socket holds loopback), so
/// only the address half of the validation refuses it, while it still
/// holds its port. Nothing binds the configured address, so the shape runs
/// on every unix CI leg.
#[cfg(unix)]
#[test]
fn a_refused_handover_listener_is_close_on_exec_again_and_stays_bound() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    // The test keeps the handle: a refused number is left open, so the
    // descriptor stays the test's to close.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the listener");
    let bound = listener.local_addr().expect("the bound address");
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], bound.port()).into();
    let listen_fd = {
        use std::os::fd::AsRawFd as _;
        listener.as_raw_fd()
    };
    super::exec_handover::register_listener(listen_fd);
    super::exec_handover::prepare_for_exec();
    assert!(
        !cloexec(listen_fd),
        "the handover cleared close-on-exec on the registered listener"
    );
    // The port matches; the address does not, so the validation refuses
    // the number.
    let listen_var = listen_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LISTEN_FD_ENV,
            Some(std::ffi::OsStr::new(&listen_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let handover = super::exec_handover::take_at_start(&dir, Some(addr));
    drop(capture);
    assert!(
        handover.is_none(),
        "no lock was handed over, and the listener was refused"
    );
    assert!(
        cloexec(listen_fd),
        "a refused handover descriptor is re-marked close-on-exec"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the listener descriptor handed over across the re-exec failed validation; binding the REST listener afresh"
                .to_string()
        ],
        "one daemon line names the listener's fallback"
    );
    // Left open and still bound: the refused listener holds its ephemeral
    // port, and only the test's own handle closes it.
    assert!(
        matches!(
            std::net::TcpListener::bind(bound).map(|_| ()),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
        ),
        "the refused listener still holds its bound port"
    );
    super::exec_handover::reset();
}

/// A configured nonzero port refuses a listener handed over on another port
/// of the same ip: `--listen 127.0.0.1:<p>` names one port, and the
/// port-0 wildcard must not widen it into any port on that address. Two
/// live ephemeral binds guarantee the ports differ; without the port
/// equality the handed-over socket would be adopted and the refusal line
/// never log.
#[cfg(unix)]
#[test]
fn a_fixed_port_listen_refuses_a_listener_handed_over_on_another_port() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    // The test keeps the handle: a refused number is left open, so the
    // descriptor stays the test's to close.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the listener");
    let bound = listener.local_addr().expect("the bound address");
    let other = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the configured port");
    let configured = other.local_addr().expect("the configured address");
    assert_ne!(
        bound.port(),
        configured.port(),
        "two live ephemeral binds never share a port"
    );
    let listen_fd = {
        use std::os::fd::AsRawFd as _;
        listener.as_raw_fd()
    };
    super::exec_handover::register_listener(listen_fd);
    super::exec_handover::prepare_for_exec();
    assert!(
        !cloexec(listen_fd),
        "the handover cleared close-on-exec on the registered listener"
    );
    let listen_var = listen_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LISTEN_FD_ENV,
            Some(std::ffi::OsStr::new(&listen_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let handover = super::exec_handover::take_at_start(&dir, Some(configured));
    drop(capture);
    assert!(
        handover.is_none(),
        "no lock was handed over, and the listener was refused"
    );
    assert!(
        cloexec(listen_fd),
        "the refused handover descriptor is re-marked close-on-exec"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth daemon: the listener descriptor handed over across the re-exec failed validation; binding the REST listener afresh"
                .to_string()
        ],
        "one daemon line names the listener's fallback"
    );
    assert!(
        matches!(
            std::net::TcpListener::bind(bound).map(|_| ()),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
        ),
        "the refused listener still holds its bound port"
    );
    assert!(
        super::exec_handover::take_listener(configured).is_none(),
        "the wrong port was never adopted"
    );
    super::exec_handover::reset();
}

/// A listener the handover adopted is the socket `serve_prepared` serves
/// on: the slot `take_at_start` validated is what `take_listener` hands
/// it, so the bind never runs (a fresh bind would lose to the adopted
/// socket's own hold on the address), and a real TLS handshake answers
/// through the inherited socket. Skipped where `openssl` cannot build the
/// certificate fixture, like every TLS test.
#[cfg(unix)]
#[test]
fn an_adopted_listener_is_the_socket_serve_prepared_serves_on() {
    use std::io::{Read as _, Write as _};
    use std::os::fd::IntoRawFd as _;
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    let Some((paths, ca_crt)) =
        super::api::tests::generate_chain(scratch.path()).expect("the chain")
    else {
        eprintln!(
            "SKIPPED an_adopted_listener_is_the_socket_serve_prepared_serves_on: openssl is not usable here"
        );
        return;
    };
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the adopted listener");
    let addr = listener.local_addr().expect("the bound address");
    let listen_fd = listener.into_raw_fd();
    super::exec_handover::register_listener(listen_fd);
    super::exec_handover::prepare_for_exec();
    let listen_var = listen_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LISTEN_FD_ENV,
            Some(std::ffi::OsStr::new(&listen_var)),
        )],
    );
    let adopted_lock = super::exec_handover::take_at_start(&dir, Some(addr));
    assert!(
        adopted_lock.is_none(),
        "no lock was handed over, and nothing may be rejected"
    );
    let prepared = super::api::prepare(addr, &super::api::tls::CertSource::Explicit(paths))
        .expect("prepare reads the certificate");
    let config: crate::profile::ConfigHandle = std::sync::Arc::new(
        crate::lockorder::RankedMutex::new(crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        }),
    );
    super::api::serve_prepared(
        prepared,
        config,
        crate::profile::clauth_dir()
            .expect("dir")
            .join("status.json"),
        super::LiveStores::default(),
        crate::usage::MemoryStores::empty_for_test(),
    )
    .expect("the adopted listener serves");
    assert!(
        super::exec_handover::take_listener(addr).is_none(),
        "serve_prepared consumed the adopted listener slot"
    );
    // A real TLS handshake through the inherited socket: the client trusts
    // only the fixture's CA, so a response proves the adopted socket
    // answers.
    let client = super::api::tests::client_config(&ca_crt).expect("the client config");
    let name = rustls::pki_types::ServerName::try_from(super::api::tests::SERVER_NAME)
        .expect("the server name")
        .to_owned();
    let conn = rustls::ClientConnection::new(std::sync::Arc::clone(&client), name)
        .expect("the client connection");
    let sock = std::net::TcpStream::connect(addr).expect("connect to the port");
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read deadline");
    let mut stream = rustls::StreamOwned::new(conn, sock);
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write the request");
    stream.flush().expect("flush the request");
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).expect("read the response");
        assert!(n > 0, "eof before a response head");
        head.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 "),
        "the adopted socket answered the TLS handshake: {head}"
    );
    super::exec_handover::reset();
}

/// A planted lock descriptor that IS an open handle to `clauthd.lock` while
/// another process holds the flock is refused (the handover lied), left
/// open, and the fresh claim reads the singleton as held elsewhere.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_planted_lock_whose_flock_another_process_holds_is_refused() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let scratch = tempfile::tempdir().expect("test scratch");
    let verdict_dir = scratch.path().join("verdicts");
    std::fs::create_dir_all(&verdict_dir).expect("verdict dir");
    let holder_log = scratch.path().join("holder.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&holder_log)
        .expect("holder log");
    let lock_path = dir.join(super::LOCK_FILE);
    let mut holder = crate::testutil::reentry_command(COMPETITOR_TEST, &[]);
    holder
        .env(COMPETITOR_MODE_ENV, "hold")
        .env(VERDICT_DIR_ENV, &verdict_dir)
        .env(HANDOVER_LOCK_ENV, &lock_path)
        .stdout(std::process::Stdio::from(
            log.try_clone().expect("clone the holder log"),
        ))
        .stderr(std::process::Stdio::from(log));
    let mut holder = holder.spawn().expect("spawn the holder");
    // The holder took the flock and parked; a red before the kill is
    // bounded by the park's own deadline, and the re-entered child's own
    // output lands in the holder log.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !verdict_dir.join("holding").exists() {
        assert!(
            Instant::now() < deadline,
            "the holder never took the singleton: {}",
            std::fs::read_to_string(&holder_log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The planted descriptor: an open handle to the same file, a different
    // description whose flock is held by the holder process. Owned by the
    // handover alone (the exec's shape), so an adoption and the refusal
    // never double-own one number.
    let planted = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("the planted handle");
    let planted_fd = {
        use std::os::fd::IntoRawFd as _;
        planted.into_raw_fd()
    };
    let planted_var = planted_fd.to_string();
    let _pin = crate::testutil::EnvPin::new(
        &home,
        &[(
            super::exec_handover::LOCK_FD_ENV,
            Some(std::ffi::OsStr::new(&planted_var)),
        )],
    );
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let handover = super::exec_handover::take_at_start(&dir, None);
    drop(capture);
    assert!(
        handover.is_none(),
        "the lied handover is refused, never adopted"
    );
    // Still open: fstat on the raw number (a refusal closes nothing).
    // SAFETY: `libc::stat` is all integers, so the zeroed value is valid
    // and no field is read before `fstat` fills the struct.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `planted_fd` is this process's own descriptor; fstat writes
    // only to our `stat` local.
    let open = unsafe { libc::fstat(planted_fd, &raw mut stat) };
    assert_eq!(open, 0, "the refused descriptor is left open and untouched");
    // Re-own the number the handover never took: the refusal left it open
    // and owned by nobody, so this is the descriptor the test closes.
    // SAFETY: `planted_fd` is the planted descriptor, still open (the
    // probe above) and unowned since the transfer; re-owning hands it to
    // the drop.
    let _planted = unsafe {
        use std::os::fd::FromRawFd as _;
        std::fs::File::from_raw_fd(planted_fd)
    };
    // The fresh claim reads the singleton as held by the other process.
    let claim = super::probe::claim_singleton(&dir, false).expect("the fresh claim");
    assert!(
        matches!(claim, super::probe::Claim::Redundant),
        "the fresh claim reads the singleton held elsewhere"
    );
    holder.kill().expect("kill the holder");
    holder.wait().expect("reap the holder");
}

/// A `--replace` start whose pid sidecar names this process never signals
/// it: the refused-handover shape (this process holding the flock through
/// the inherited descriptor, the sidecar restamped with its own pid)
/// claims plainly and reads the singleton as held, where
/// `claim_by_replacing` would terminate its own pid.
#[cfg(unix)]
#[test]
fn a_replace_start_whose_sidecar_names_its_own_pid_claims_without_self_signal() {
    let _home = crate::testutil::HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("the clauth dir");
    crate::profile::mkdir_700(&dir).expect("create ~/.clauth");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(super::LOCK_FILE))
        .expect("open the lock file");
    lock.try_lock().expect("take the flock");
    std::fs::write(
        dir.join(super::PID_FILE),
        format!("{}\n", std::process::id()),
    )
    .expect("stamp the sidecar");
    let claim = super::fresh_claim(&dir, super::StartMode::Replace)
        .expect("the claim does not signal itself");
    assert!(
        matches!(claim, super::probe::Claim::Redundant),
        "the claim reads its own held singleton as redundant, never signals itself"
    );
}

/// A listener handed over under `--listen 127.0.0.1:0` adopts the inherited
/// socket on its real ephemeral port: the configured 0 matches any port on
/// that address, so the old image's bound port keeps serving instead of a
/// fresh bind beside the refused socket. Stage 1 binds :0 and registers,
/// stage 2 runs the same take and must come out with the same port.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_zero_port_listen_adopts_the_inherited_listener_on_its_bound_port() {
    match std::env::var(HANDOVER_STAGE_ENV).ok().as_deref() {
        None => {
            let _home = crate::testutil::HomeSandbox::new();
            let child_home = tempfile::tempdir().expect("child home");
            let scratch = tempfile::tempdir().expect("test scratch");
            let verdict_dir = scratch.path().join("verdicts");
            std::fs::create_dir_all(&verdict_dir).expect("verdict dir");
            let stage_log = scratch.path().join("stage.log");
            let child = spawn_stage(
                "daemon::handover_tests::a_zero_port_listen_adopts_the_inherited_listener_on_its_bound_port",
                child_home.path(),
                &verdict_dir,
                &stage_log,
            );
            assert_stage_passes(child, &stage_log, "zero-port listener handover");
        }
        Some("1") => {
            use std::os::fd::AsRawFd as _;
            sandbox_home();
            let listener =
                std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind the listener");
            let bound = listener.local_addr().expect("the bound address");
            super::exec_handover::register_listener(listener.as_raw_fd());
            // `listener` lives until the exec: it owns the fd the handover
            // clears close-on-exec on.
            exec_self_forward(
                "2",
                &[(HANDOVER_LISTEN_ENV, &bound.to_string())],
                super::exec_handover::prepare_for_exec(),
            );
        }
        Some("2") => {
            sandbox_home();
            let dir = crate::profile::clauth_dir().expect("the clauth dir");
            // The configured address, with the stage-1 image's actual bound
            // port as the adoption target.
            let bound: std::net::SocketAddr = std::env::var(HANDOVER_LISTEN_ENV)
                .expect("the bound address")
                .parse()
                .expect("the address parses");
            let configured = std::net::SocketAddr::new(bound.ip(), 0);
            let handover = super::exec_handover::take_at_start(&dir, Some(configured));
            assert!(
                handover.is_none(),
                "no lock was handed over, and nothing may be rejected"
            );
            let listener =
                super::exec_handover::take_listener(configured).expect("the adopted listener");
            assert_eq!(
                listener.local_addr().expect("getsockname"),
                bound,
                "the adopted socket is still bound to the stage-1 port"
            );
        }
        Some(stage) => panic!("unknown handover stage {stage:?}"),
    }
}
