//! The managed gateway under a real `clauth daemon`: a boot with an adopted
//! config runs exactly one gateway and publishes it healthy, SIGTERM stops it
//! before the daemon dies of that signal, and a `--standby` daemon runs none
//! until it takes over. Every run drives the stub `shunt` of
//! `tests/support/gateway_stub.rs` under a sandbox `$HOME`.
//!
//! Unix only: the stub is a shell script tied to its `/health` answerer by a
//! named pipe, and the child resolves its home through `$HOME`, which only
//! unix lets a test point at a sandbox (`tests/closed_reader.rs`).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/gateway_stub.rs"]
mod stub;

use std::fs::{self, File};
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// A sandbox home with an adopted config on a free port, the stub, its
/// `/health` answerer and the gateway record, written the way
/// `GatewayRecord::update` writes it.
struct Sandbox {
    server: stub::HealthServer,
    dir: PathBuf,
    config: PathBuf,
    binary: PathBuf,
    port: u16,
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home");
        let clauth = home.path().join(".clauth");
        fs::create_dir_all(&clauth).expect(".clauth");
        let etc = fs::canonicalize(home.path())
            .expect("canonical home")
            .join("etc");
        fs::create_dir_all(&etc).expect("etc");
        let port = stub::free_port();
        let config = etc.join("shunt.toml");
        fs::write(
            &config,
            format!("[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n"),
        )
        .expect("config");
        let dir = home.path().join("stub");
        let binary = stub::write_stub(&dir);
        fs::write(
            clauth.join("gateway.toml"),
            format!(
                "config = \"{}\"\nbinary = \"{}\"\ndisabled = false\n",
                config.display(),
                binary.display()
            ),
        )
        .expect("gateway record");
        let server = stub::HealthServer::start(&dir, port, "0.49.1");
        Self {
            server,
            dir,
            config,
            binary,
            port,
            home,
        }
    }

    fn clauth(&self) -> PathBuf {
        self.home.path().join(".clauth")
    }

    fn log(&self) -> PathBuf {
        self.home.path().join("daemon-stderr.log")
    }

    /// `clauth daemon <args>` over this home, its stderr in [`Self::log`]
    /// (appended, as a supervisor points it), nothing inherited that names
    /// another home or reaches the network.
    fn daemon(&self, args: &[&str]) -> Daemon {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log())
            .expect("daemon log");
        let child = Command::new(env!("CARGO_BIN_EXE_clauth"))
            .arg("daemon")
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("CLAUTH_NO_UPDATE", "1")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn clauth daemon");
        Daemon(Some(child))
    }

    /// `clauth daemon` whose SIGHUP is inherited as ignored (`nohup`, a
    /// non-interactive shell's `&`): a shell sets `trap "" HUP` and execs it,
    /// which is exactly the disposition the daemon must not turn into a death.
    fn daemon_ignoring_hup(&self) -> Daemon {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log())
            .expect("daemon log");
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "trap '' HUP; exec '{}' daemon",
                env!("CARGO_BIN_EXE_clauth")
            ))
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("CLAUTH_NO_UPDATE", "1")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn clauth daemon ignoring SIGHUP");
        Daemon(Some(child))
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    /// The feed's `gateway` object, once one is published.
    fn gateway(&self) -> Option<Value> {
        let body = fs::read(self.clauth().join("status.json")).ok()?;
        let feed: Value = serde_json::from_slice(&body).ok()?;
        feed.get("gateway").cloned()
    }

    /// The feed's `generated_at`, which the daemon restamps on each publish.
    fn generated_at(&self) -> Option<String> {
        let body = fs::read(self.clauth().join("status.json")).ok()?;
        let feed: Value = serde_json::from_slice(&body).ok()?;
        feed.get("generated_at")?.as_str().map(str::to_owned)
    }

    fn calls(&self) -> Vec<stub::Invocation> {
        stub::invocations(&self.dir)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // A daemon that leaked its gateway (the regression these tests catch)
        // leaves a stub holding the health server's pipe; kill every stub the
        // record names — only while its command line still names this test's
        // own stub — before `server` drops, whose join otherwise waits 30 s.
        // Best-effort, never a panic: a panic here while a test unwinds aborts
        // the whole test binary.
        for pid in stub::recorded_pids(&self.dir) {
            stub::kill_best_effort(pid, &self.binary);
        }
    }
}

/// A daemon the test owns: stopped with SIGTERM when the test ends, so its
/// own shutdown stops the gateway, then killed if it outlives that.
struct Daemon(Option<Child>);

impl Daemon {
    fn pid(&self) -> u32 {
        self.0.as_ref().expect("running").id()
    }

    /// The daemon's exit status, `None` while it runs; a reaped zombie reads
    /// as `Some`, unlike a `kill -s 0` probe. Once it returns `Some` the
    /// Child is taken out, so a later `Drop` never signals a reaped pid.
    fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        let status = self.0.as_mut()?.try_wait().ok().flatten();
        if status.is_some() {
            self.0 = None;
        }
        status
    }

    fn signal(&mut self, name: &str) -> std::process::ExitStatus {
        stub::signal(self.pid(), name);
        let child = self.0.as_mut().expect("running");
        let mut status = None;
        stub::wait_until(
            "the daemon exits after its signal",
            Duration::from_secs(10),
            || {
                status = child.try_wait().expect("try_wait");
                status.is_some()
            },
        );
        // Take the reaped Child only once it has exited, so a daemon that
        // ignores the signal and trips the wait's panic still finds the Child
        // in `Drop`, which kills and reaps it.
        self.0 = None;
        status.expect("exited")
    }

    fn terminate(&mut self) -> std::process::ExitStatus {
        self.signal("TERM")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        // Best-effort, never a panic: a panic in a destructor while a test
        // unwinds aborts the whole test binary.
        stub::signal_best_effort(child.id(), "TERM");
        for _ in 0..500 {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn wait_for_healthy(sandbox: &Sandbox) -> Value {
    let mut slot = Value::Null;
    stub::wait_until(
        "the feed publishes a healthy gateway",
        Duration::from_secs(20),
        || {
            slot = sandbox.gateway().unwrap_or(Value::Null);
            slot["state"] == "healthy"
        },
    );
    slot
}

#[test]
fn a_daemon_boot_runs_one_healthy_gateway_and_stops_it_on_sigterm() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.daemon(&[]);

    let slot = wait_for_healthy(&sandbox);
    let calls = sandbox.calls();
    assert_eq!(calls.len(), 1, "exactly one gateway: {calls:?}");
    assert_eq!(
        calls[0].args,
        ["run", "--config", sandbox.config.to_str().expect("utf-8")]
    );
    assert_eq!(slot["pid"], calls[0].pid);
    assert_eq!(slot["port"], sandbox.port);
    assert_eq!(slot["version"], "0.49.1", "the version /health answered");
    assert_eq!(slot["binary"], sandbox.binary.to_str().expect("utf-8"));
    assert!(sandbox.server.serving(), "/health answers");

    let status = daemon.terminate();
    assert_eq!(
        status.signal(),
        Some(15),
        "the daemon still dies of the SIGTERM it got"
    );
    sandbox.server.wait_serving(false);
    assert!(
        sandbox.log_text().contains(&format!(
            "clauth daemon: the shunt gateway (pid {}) stopped (signal 15)",
            calls[0].pid
        )),
        "the gateway got SIGTERM from its daemon: {}",
        sandbox.log_text()
    );
    assert!(
        !sandbox.clauth().join("gateway-child.json").exists(),
        "a clean stop leaves no child marker"
    );
    assert_eq!(sandbox.calls().len(), 1, "no restart on the way out");
}

/// The gateway's stdout and stderr land in `~/.clauth/gateway.log`, created
/// owner-only, and never in the daemon's own stderr: shunt logs every request
/// at `info`, which would flood the daemon's capped log.
#[test]
fn the_gateways_output_lands_in_its_own_owner_only_log() {
    use std::os::unix::fs::PermissionsExt as _;

    let sandbox = Sandbox::new();
    let mut daemon = sandbox.daemon(&[]);
    wait_for_healthy(&sandbox);
    let pid = sandbox.calls()[0].pid;
    daemon.terminate();

    let log = sandbox.clauth().join("gateway.log");
    let written = fs::read_to_string(&log).unwrap_or_default();
    let mode = fs::metadata(&log)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777);
    let daemon_stderr = sandbox.log_text();
    let leaked: Vec<&str> = daemon_stderr
        .lines()
        .filter(|line| line.contains("gateway stub"))
        .collect();
    assert_eq!(
        (written.as_str(), mode, leaked),
        (
            format!("gateway stub stdout {pid}\ngateway stub stderr {pid}\n").as_str(),
            Some(0o600),
            Vec::<&str>::new()
        ),
        "the gateway's own owner-only log holds its output, the daemon's stderr none of it"
    );
}

#[test]
fn a_standby_daemon_spawns_nothing_until_it_takes_over() {
    let sandbox = Sandbox::new();
    // Stand in for the running daemon: hold the singleton lock.
    let holder = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sandbox.clauth().join("clauthd.lock"))
        .expect("open the daemon lock");
    holder.try_lock().expect("hold the daemon lock");

    let mut standby = sandbox.daemon(&["--standby"]);
    stub::wait_until("the standby parks", Duration::from_secs(10), || {
        sandbox.log_text().contains("standing by until it exits")
    });
    // A standby cannot reach gateway::start before promotion, so its own log
    // never records a spawn; the negative rests on that, not on a wall wait.
    assert!(
        !sandbox.log_text().contains("started the shunt gateway"),
        "a parked standby never spawns: {}",
        sandbox.log_text()
    );
    assert_eq!(sandbox.calls().len(), 0, "a standby runs no gateway");
    assert!(sandbox.gateway().is_none(), "nor publishes a feed");

    drop(holder);
    let slot = wait_for_healthy(&sandbox);
    let calls = sandbox.calls();
    assert_eq!(calls.len(), 1, "the promoted daemon runs exactly one");
    assert_eq!(slot["pid"], calls[0].pid);

    standby.terminate();
    sandbox.server.wait_serving(false);
}

/// SIGINT and SIGHUP stop the gateway before the daemon dies of that signal,
/// the SIGTERM row's siblings.
#[test]
fn a_daemon_stops_its_gateway_on_sigint_and_sighup_too() {
    for (name, expected) in [("INT", 2), ("HUP", 1)] {
        let sandbox = Sandbox::new();
        let mut daemon = sandbox.daemon(&[]);
        wait_for_healthy(&sandbox);
        let pid = sandbox.calls()[0].pid;

        let status = daemon.signal(name);
        assert_eq!(
            status.signal(),
            Some(expected),
            "the daemon still dies of the {name} it got"
        );
        sandbox.server.wait_serving(false);
        assert!(
            sandbox.log_text().contains(&format!(
                "clauth daemon: the shunt gateway (pid {pid}) stopped (signal 15)"
            )),
            "the gateway got SIGTERM from its daemon: {}",
            sandbox.log_text()
        );
        assert_eq!(sandbox.calls().len(), 1, "no restart on the way out");
    }
}

/// A daemon that inherited SIGHUP as ignored (`nohup`) keeps running on a
/// hangup: only a signal it did not inherit as ignored is watched.
#[test]
fn a_daemon_that_inherited_sighup_ignored_keeps_running_on_hangup() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.daemon_ignoring_hup();
    wait_for_healthy(&sandbox);
    let pid = sandbox.calls()[0].pid;
    let daemon_pid = daemon.pid();

    // Pin the inherited disposition, not a wall window: SIGHUP (signal 1,
    // bit 0) is in `SigIgn`, not in `SigCgt`, and not in `SigBlk`, so no
    // handler watches it.
    #[cfg(target_os = "linux")]
    assert_eq!(
        sighup_state(daemon_pid),
        SighupState::Alive {
            ignored: true,
            caught: false,
            blocked: false
        },
        "the daemon inherited SIGHUP as ignored, uncaught and unblocked"
    );

    stub::signal(daemon_pid, "HUP");

    // Two fresh `generated_at` stamps: the daemon's run loop published twice
    // after the hangup, so a whole tick ran after it. A watched hangup stops
    // the gateway first, which the log and the stub show; a death by any other
    // path inside that span stops the stamps, and `try_wait` reaps it. A mask
    // read alone cannot see a thread parked in `sigwait` on the signal.
    let mut stamp = sandbox.generated_at();
    let mut fresh = 0;
    stub::wait_until(
        "the daemon keeps publishing after the hangup",
        Duration::from_secs(10),
        || {
            let now = sandbox.generated_at();
            if now.is_some() && now != stamp {
                fresh += 1;
                stamp = now;
            }
            fresh >= 2
        },
    );
    assert!(
        !sandbox
            .log_text()
            .contains(&format!("stopping the shunt gateway (pid {pid})")),
        "the hangup did not stop the gateway: {}",
        sandbox.log_text()
    );
    assert!(stub::alive(pid), "the gateway keeps running");
    assert!(
        daemon.try_wait().is_none(),
        "the daemon keeps running on a SIGHUP it inherited ignored"
    );

    // A clean stop through SIGTERM still works.
    let status = daemon.terminate();
    assert_eq!(
        status.signal(),
        Some(15),
        "the daemon still dies of SIGTERM"
    );
    sandbox.server.wait_serving(false);
    assert!(
        sandbox.log_text().contains(&format!(
            "clauth daemon: the shunt gateway (pid {pid}) stopped (signal 15)"
        )),
        "the gateway got SIGTERM from its daemon: {}",
        sandbox.log_text()
    );
}

/// A process's liveness and SIGHUP disposition, as `/proc/<pid>/status`
/// (linux) reads.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum SighupState {
    /// The status file is gone, or reads `State: Z`: the process died.
    Died,
    /// Alive, with SIGHUP's bit (signal 1, bit 0) in `SigIgn`, `SigCgt` and
    /// `SigBlk`.
    Alive {
        ignored: bool,
        caught: bool,
        blocked: bool,
    },
    /// A status this parser cannot read, whole.
    Unparsed(String),
}

/// `pid`'s [`SighupState`]: the inherited disposition and block mask, pinned
/// without a wall clock, and a death the masks alone cannot show.
#[cfg(target_os = "linux")]
fn sighup_state(pid: u32) -> SighupState {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return SighupState::Died;
    };
    let mut state = None;
    let mut sigign = None;
    let mut sigcgt = None;
    let mut sigblk = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("State:") {
            state = value.trim_start().chars().next();
        } else if let Some(value) = line.strip_prefix("SigIgn:") {
            sigign = u64::from_str_radix(value.trim(), 16).ok();
        } else if let Some(value) = line.strip_prefix("SigCgt:") {
            sigcgt = u64::from_str_radix(value.trim(), 16).ok();
        } else if let Some(value) = line.strip_prefix("SigBlk:") {
            sigblk = u64::from_str_radix(value.trim(), 16).ok();
        }
    }
    match (state, sigign, sigcgt, sigblk) {
        (Some('Z'), _, _, _) => SighupState::Died,
        (Some(_), Some(ign), Some(cgt), Some(blk)) => SighupState::Alive {
            ignored: ign & 1 == 1,
            caught: cgt & 1 == 1,
            blocked: blk & 1 == 1,
        },
        _ => SighupState::Unparsed(status),
    }
}
