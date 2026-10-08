#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The gateway supervisor: the slot's closed state set, the record-only slot a
//! body builds without a supervisor, and the state machine stepped by hand
//! with an injected clock over a stub `shunt` in a `HomeSandbox`. The stub is
//! a `/bin/sh` script tied to a loopback `/health` answerer through a named
//! pipe (`tests/support/gateway_stub.rs`), so every stub test is
//! `#[cfg(unix)]`; the serialization and record-only tests run everywhere.

use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
use std::time::Duration;

use super::*;
// The local `struct Supervised` (unix-only) shadows the trait of the same
// name, so bring the trait's methods (`.intent`) into scope without binding
// its name. On windows the glob above already imports the trait.
#[cfg(unix)]
use super::Supervised as _;
use crate::gateway::GatewayRecord;
use crate::testutil::HomeSandbox;
#[cfg(unix)]
use crate::testutil::no_shunt_env;

#[cfg(unix)]
#[path = "../support/gateway_stub.rs"]
pub(crate) mod stub;

/// The injected wall clock every stepped test starts at, and its stamps.
#[cfg(unix)]
const T0_WALL_MS: u64 = 1_790_000_000_000;
const AT_T0: &str = "2026-09-21T14:13:20+00:00";
#[cfg(unix)]
const AT_T1: &str = "2026-09-21T14:13:21+00:00";
#[cfg(unix)]
const AT_T2: &str = "2026-09-21T14:13:22+00:00";
#[cfg(unix)]
const AT_T5: &str = "2026-09-21T14:13:25+00:00";
#[cfg(unix)]
const AT_T10: &str = "2026-09-21T14:13:30+00:00";
#[cfg(unix)]
const AT_T14: &str = "2026-09-21T14:13:34+00:00";
#[cfg(unix)]
const AT_T15: &str = "2026-09-21T14:13:35+00:00";

fn blank(state: GatewayState) -> GatewaySlot {
    GatewaySlot {
        state,
        config: None,
        binary: None,
        port: None,
        pid: None,
        version: None,
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    }
}

fn shown(path: &std::path::Path) -> Option<String> {
    Some(path.display().to_string())
}

// ── the slot ────────────────────────────────────────────────────────────────

const ALL_STATES: [GatewayState; 15] = [
    GatewayState::Absent,
    GatewayState::Disabled,
    GatewayState::Held,
    GatewayState::NoConfig,
    GatewayState::YamlRefused,
    GatewayState::Misconfigured,
    GatewayState::BinaryMissing,
    GatewayState::Foreign,
    GatewayState::Starting,
    GatewayState::Healthy,
    GatewayState::Unhealthy,
    GatewayState::BelowFloor,
    GatewayState::Restarting,
    GatewayState::Stopping,
    GatewayState::Unobserved,
];

/// One slot per state, every field fixed, and its exact bytes. The match is
/// wildcard-free, so a new state does not compile until it has a fixture.
fn fixture(state: GatewayState) -> (GatewaySlot, &'static str) {
    let config = Some("/etc/shunt/shunt.toml".to_string());
    let binary = Some("/usr/local/bin/shunt".to_string());
    let since = Some(AT_T0.to_string());
    match state {
        GatewayState::Absent => (
            GatewaySlot {
                since,
                ..blank(state)
            },
            r#"{"state":"absent","config":null,"binary":null,"port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Disabled => (
            GatewaySlot {
                config,
                binary,
                restarts: 2,
                last_exit: Some(ExitReport {
                    code: None,
                    signal: Some(15),
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"disabled","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":2,"last_exit":{"code":null,"signal":15},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Held => (
            GatewaySlot {
                config,
                binary,
                since,
                ..blank(state)
            },
            r#"{"state":"held","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::NoConfig => (
            GatewaySlot {
                config,
                binary: Some("shunt".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"no_config","config":"/etc/shunt/shunt.toml","binary":"shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::YamlRefused => (
            GatewaySlot {
                config: Some("/etc/shunt/shunt.yaml".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"yaml_refused","config":"/etc/shunt/shunt.yaml","binary":null,"port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Misconfigured => (
            GatewaySlot {
                config,
                binary,
                reason: Some(
                    "in env file /etc/shunt/tokens.env: line 3 holds a NUL byte; systemd refuses such a file whole, and so does clauth: remove the byte"
                        .to_string(),
                ),
                since,
                ..blank(state)
            },
            r#"{"state":"misconfigured","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":"in env file /etc/shunt/tokens.env: line 3 holds a NUL byte; systemd refuses such a file whole, and so does clauth: remove the byte","since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::BinaryMissing => (
            GatewaySlot {
                config,
                binary: Some("/opt/shunt/bin/shunt".to_string()),
                port: Some(3067),
                since,
                ..blank(state)
            },
            r#"{"state":"binary_missing","config":"/etc/shunt/shunt.toml","binary":"/opt/shunt/bin/shunt","port":3067,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Foreign => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                version: Some("0.49.1".to_string()),
                answerer: Some(Answerer::Shunt),
                since,
                ..blank(state)
            },
            r#"{"state":"foreign","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":null,"version":"0.49.1","answerer":"shunt","floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Starting => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                since,
                ..blank(state)
            },
            r#"{"state":"starting","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Healthy => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                version: Some("0.49.1".to_string()),
                restarts: 1,
                last_exit: Some(ExitReport {
                    code: Some(1),
                    signal: None,
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"healthy","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":"0.49.1","answerer":null,"floor":"0.48.0","restarts":1,"last_exit":{"code":1,"signal":null},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Unhealthy => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                version: Some("0.49.1".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"unhealthy","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":"0.49.1","answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::BelowFloor => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                version: Some("0.47.0".to_string()),
                last_exit: Some(ExitReport {
                    code: None,
                    signal: Some(15),
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"below_floor","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":null,"version":"0.47.0","answerer":null,"floor":"0.48.0","restarts":0,"last_exit":{"code":null,"signal":15},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Restarting => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                restarts: 3,
                last_exit: Some(ExitReport {
                    code: None,
                    signal: Some(9),
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"restarting","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":3,"last_exit":{"code":null,"signal":9},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Stopping => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                version: Some("0.49.1".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"stopping","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":"0.49.1","answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Unobserved => (
            GatewaySlot {
                config,
                binary,
                ..blank(state)
            },
            r#"{"state":"unobserved","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":null}"#,
        ),
    }
}

#[test]
fn every_state_serializes_to_its_fixture() {
    for state in ALL_STATES {
        let (slot, bytes) = fixture(state);
        assert_eq!(
            serde_json::to_string(&slot).expect("serialize"),
            bytes,
            "{state:?}"
        );
    }
    for (answerer, bytes) in [
        (Answerer::Shunt, r#""shunt""#),
        (Answerer::NotShunt, r#""not_shunt""#),
        (Answerer::NoAnswer, r#""no_answer""#),
    ] {
        assert_eq!(
            serde_json::to_string(&answerer).expect("serialize"),
            bytes,
            "{answerer:?}"
        );
    }
}

#[test]
fn the_drain_bound_is_the_env_then_the_config_then_shunts_default() {
    let with = "[server]\nshutdown_timeout_seconds = 45\n";
    for (config, env, secs, case) in [
        (with, Some("7"), 7, "the env outranks the config"),
        (with, None, 45, "the config's value"),
        (
            "[server]\nbind = \"127.0.0.1:3001\"\n",
            None,
            30,
            "shunt's default",
        ),
        ("", None, 30, "an empty config"),
        (with, Some("soon"), 3600, "an env value clauth cannot read"),
        (
            "[server]\nshutdown_timeout_seconds = \"soon\"\n",
            None,
            3600,
            "a config value that is not a number",
        ),
        (
            "[server]\nshutdown_timeout_seconds = 7200\n",
            None,
            3600,
            "past shunt's own maximum",
        ),
        ("not = [toml", None, 3600, "a config that does not parse"),
    ] {
        assert_eq!(
            crate::gateway::resolve_shutdown_timeout(config, env),
            Duration::from_secs(secs),
            "{case}"
        );
    }
}

/// With no supervisor (single-shot `status --json`, a daemonless republish),
/// the slot says only what the record says, and never claims a running state.
#[test]
fn without_a_supervisor_the_slot_reads_the_record_alone() {
    let home = HomeSandbox::new();
    assert_eq!(unsupervised_slot(), blank(GatewayState::Absent));

    let dir = home.home().join("etc");
    fs::create_dir_all(&dir).expect("etc");
    let config = dir.join("shunt.toml");
    fs::write(&config, "[server]\n").expect("config");
    let mut record = GatewayRecord::new(config).expect("adoptable");
    let canonical = shown(record.config());
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: canonical.clone(),
            binary: Some("shunt".to_string()),
            ..blank(GatewayState::Unobserved)
        }
    );

    record.disabled = true;
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: canonical.clone(),
            binary: Some("shunt".to_string()),
            ..blank(GatewayState::Disabled)
        }
    );

    record.disabled = false;
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save");
    fs::remove_file(record.config()).expect("remove the config");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: canonical,
            binary: Some("shunt".to_string()),
            ..blank(GatewayState::NoConfig)
        }
    );

    let path = crate::gateway::record_path().expect("record path");
    fs::write(&path, "config = \"/etc/shunt/shunt.yaml\"\n").expect("hand-edit");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: Some("/etc/shunt/shunt.yaml".to_string()),
            ..blank(GatewayState::YamlRefused)
        }
    );

    fs::write(&path, "config = \"shunt.toml\"\n").expect("hand-edit");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            reason: Some(format!(
                "invalid gateway record {}: the shunt config must be an absolute path, got shunt.toml",
                path.display()
            )),
            ..blank(GatewayState::Misconfigured)
        }
    );
}

/// The gateway's log stays under the daemon's size cap on the supervisor's
/// own cadence: a step trims an oversized `gateway.log` in place to its last
/// whole lines within 1 MiB.
#[test]
fn a_step_trims_an_oversized_gateway_log_to_its_tail() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let log = dir.join("gateway.log");
    // 52,429 lines of 100 bytes: 5,242,900 bytes, 20 past the 5 MiB cap.
    let text: String = (0..52_429).map(|n| format!("{n:099}\n")).collect();
    fs::write(&log, text).expect("an oversized log");

    Supervisor::new(new_handle()).step(Tick::now());

    // The last 1 MiB starts inside line 41,943, at byte 4,194,324; that
    // partial line goes, so lines 41,944 to 52,428 stay: 10,485 of them.
    let kept = fs::read_to_string(&log).expect("the log");
    assert_eq!(kept.len(), 1_048_500, "trimmed to its tail");
    assert_eq!(
        kept.lines().next(),
        Some(format!("{:099}", 41_944).as_str()),
        "the first whole line of the last 1 MiB"
    );
}

// ── the supervisor, over a stub ─────────────────────────────────────────────

/// A sandbox holding an adopted config bound to a free loopback port with a
/// 2 s drain, an env file, the stub `shunt` and its `/health` answerer, and
/// a record naming all three. Lines the supervisor logs on this thread are
/// captured. Declared before the supervisors a test steps, so they (and the
/// stubs they own) are gone before the answerer's teardown.
#[cfg(unix)]
struct Rig {
    server: stub::HealthServer,
    handle: GatewayHandle,
    lines: crate::logline::LogLines,
    _capture: crate::logline::LogCapture,
    dir: PathBuf,
    etc: PathBuf,
    config: PathBuf,
    binary: PathBuf,
    env_file: PathBuf,
    port: u16,
    home: HomeSandbox,
}

#[cfg(unix)]
impl Rig {
    fn new(version: &str) -> Self {
        let home = HomeSandbox::new();
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();
        fs::create_dir_all(home.home().join("etc")).expect("etc");
        // The record holds the canonical path, so every expectation does too.
        let etc = fs::canonicalize(home.home().join("etc")).expect("canonical etc");
        let port = stub::free_port();
        let config = etc.join("shunt.toml");
        fs::write(
            &config,
            format!("[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n"),
        )
        .expect("config");
        let dir = home.home().join("stub");
        let binary = stub::write_stub(&dir);
        let env_file = home.home().join("tokens.env");
        fs::write(&env_file, "GATEWAY_TEST_SECRET=from-env-file\n").expect("env file");
        let server = stub::HealthServer::start(&dir, port, version);
        let rig = Self {
            server,
            handle: new_handle(),
            lines,
            _capture: capture,
            dir,
            etc,
            config,
            binary,
            env_file,
            port,
            home,
        };
        rig.save(|_| {});
        rig
    }

    fn save(&self, edit: impl FnOnce(&mut GatewayRecord)) {
        let mut record = GatewayRecord::new(self.config.clone()).expect("adoptable");
        record.binary = Some(self.binary.clone());
        record.env_file = Some(self.env_file.clone());
        edit(&mut record);
        GatewayRecord::update(|slot| {
            *slot = Some(record);
            Ok(())
        })
        .expect("save the record");
    }

    fn supervisor(&self) -> Supervised {
        Supervised(Supervisor::new(Arc::clone(&self.handle)))
    }

    fn slot(&self) -> GatewaySlot {
        published(&self.handle).expect("the supervisor publishes a slot")
    }

    /// Every run the stub recorded, read once the record lists each spawn the
    /// supervisor logged on this thread: a stub writes its record after
    /// `spawn` returned, so a read right after a step would miss it.
    fn calls(&self) -> Vec<stub::Invocation> {
        let spawned = self
            .lines
            .snapshot()
            .iter()
            .filter(|line| line.starts_with("clauth daemon: started the shunt gateway (pid "))
            .count();
        let mut calls = Vec::new();
        stub::wait_until(
            &format!("the stub records the {spawned} runs the supervisor spawned"),
            secs(10),
            || {
                calls = stub::invocations(&self.dir);
                calls.len() >= spawned
            },
        );
        calls
    }

    /// The one run so far, asserted to be the only one.
    fn only_call(&self) -> stub::Invocation {
        let calls = self.calls();
        assert_eq!(calls.len(), 1, "exactly one gateway run: {calls:?}");
        calls.into_iter().next().expect("one run")
    }

    /// `state` naming this rig's config, stub and port.
    fn described(&self, state: GatewayState) -> GatewaySlot {
        GatewaySlot {
            config: shown(&self.config),
            binary: shown(&self.binary),
            port: Some(self.port),
            ..blank(state)
        }
    }

    fn touch(&self, name: &str) {
        fs::write(self.dir.join(name), "").expect("stub switch");
    }

    /// Rewrite the `shunt` stub to also answer `check`, keeping the run path
    /// [`stub::write_stub`] wrote and the fifo it made: a `run` behaves
    /// exactly as before, while a `check` records one line per run in
    /// `dir/checks` and exits by marker — `check-fail` exits 1 with one
    /// stderr line, `check-slow` sleeps 60 s (for a timeout), else it exits 0.
    fn arm_check_stub(&self) {
        let script = format!(
            "#!/bin/sh\n\
             d='{d}'\n\
             if [ \"$1\" = \"check\" ]; then\n\
             \x20 echo check >> \"$d/checks\"\n\
             \x20 if [ -e \"$d/check-slow\" ]; then echo $$ > \"$d/check-slow-pid\"; exec sleep 60; fi\n\
             \x20 if [ -e \"$d/check-fail\" ]; then echo 'config error: boom' >&2; exit 1; fi\n\
             \x20 exit 0\n\
             fi\n\
             if [ -e \"$d/ignore-term\" ]; then\n\
             \x20 trap 'echo \"term $$\" >> \"$d/signals\"' TERM\n\
             else\n\
             \x20 trap 'echo \"term $$\" >> \"$d/signals\"; trap - TERM; kill -s TERM $$' TERM\n\
             fi\n\
             {{\n\
             \x20 echo \"pid $$\"\n\
             \x20 echo \"bin $0\"\n\
             \x20 for a in \"$@\"; do echo \"arg $a\"; done\n\
             \x20 echo \"cwd $(pwd -P)\"\n\
             \x20 env | grep -e '^SHUNT_' -e '^CODEX_AUTH_FILE=' -e '^CLAUDE_CREDENTIALS=' | LC_ALL=C sort | sed 's/^/env /'\n\
             \x20 echo \"secret ${{GATEWAY_TEST_SECRET-}}\"\n\
             \x20 echo end\n\
             }} >> \"$d/calls\"\n\
             echo \"gateway stub stdout $$\"\n\
             echo \"gateway stub stderr $$\" >&2\n\
             if [ ! -e \"$d/no-health\" ]; then exec 3>\"$d/fifo\"; fi\n\
             i=0\n\
             while [ \"$i\" -lt 120 ]; do\n\
             \x20 sleep 1 3>&- &\n\
             \x20 wait $!\n\
             \x20 i=$((i + 1))\n\
             done\n",
            d = self.dir.display(),
        );
        fs::write(&self.binary, script).expect("arm the check stub");
    }

    /// The number of `check` runs the armed stub recorded.
    fn check_runs(&self) -> usize {
        fs::read_to_string(self.dir.join("checks"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .count()
    }
}

/// A supervisor whose gateway is killed and reaped when the test ends, red
/// or green.
#[cfg(unix)]
struct Supervised(Supervisor);

#[cfg(unix)]
impl std::ops::Deref for Supervised {
    type Target = Supervisor;
    fn deref(&self) -> &Supervisor {
        &self.0
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for Supervised {
    fn deref_mut(&mut self) -> &mut Supervisor {
        &mut self.0
    }
}

#[cfg(unix)]
impl Drop for Supervised {
    fn drop(&mut self) {
        self.0.kill_for_test();
    }
}

/// A process the test owns outright, killed and reaped at the end.
#[cfg(unix)]
struct Owned(Child);

#[cfg(unix)]
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn t0() -> Tick {
    Tick {
        at: Instant::now(),
        wall_ms: T0_WALL_MS,
    }
}

#[cfg(unix)]
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Step at `tick` until the slot reaches `state`: an exit or a probe landing
/// between two steps is waited for, never slept for.
#[cfg(unix)]
fn step_until(rig: &Rig, supervisor: &mut Supervised, tick: Tick, state: GatewayState) {
    stub::wait_until(&format!("the slot reads {state:?}"), secs(10), || {
        supervisor.step(tick);
        rig.slot().state == state
    });
}

#[cfg(unix)]
fn store_env(home: &std::path::Path) -> Vec<String> {
    let root = home.join(".clauth").join("shunt");
    let accounts = root.join("accounts");
    vec![
        format!(
            "CLAUDE_CREDENTIALS={}",
            root.join("claude-credentials.json").display()
        ),
        format!("CODEX_AUTH_FILE={}", root.join("codex-auth.json").display()),
        format!(
            "SHUNT_ANTIGRAVITY_ACCOUNTS_DIR={}",
            accounts.join("antigravity").display()
        ),
        format!(
            "SHUNT_ANTIGRAVITY_AUTH_FILE={}",
            root.join("antigravity-auth.json").display()
        ),
        format!(
            "SHUNT_CLAUDE_ACCOUNTS_DIR={}",
            accounts.join("claude").display()
        ),
        format!(
            "SHUNT_CODEX_ACCOUNTS_DIR={}",
            accounts.join("codex").display()
        ),
        format!(
            "SHUNT_CURSOR_AUTH_FILE={}",
            root.join("cursor-auth.json").display()
        ),
        format!(
            "SHUNT_KIMI_ACCOUNTS_DIR={}",
            accounts.join("kimi").display()
        ),
        format!(
            "SHUNT_XAI_AUTH_FILE={}",
            root.join("xai-auth.json").display()
        ),
    ]
}

#[cfg(unix)]
#[test]
fn a_first_step_spawns_one_gateway_with_every_store_under_clauth_and_reads_its_version() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    let calls = rig.calls();
    assert_eq!(calls.len(), 1, "exactly one gateway: {calls:?}");
    let call = &calls[0];
    assert_eq!(
        call.args,
        ["run", "--config", rig.config.to_str().expect("utf-8")]
    );
    assert_eq!(call.cwd, rig.etc, "the adopted config's own dir");
    assert_eq!(call.store_env(), store_env(rig.home.home()));
    assert_eq!(
        call.secret, "from-env-file",
        "the env file reaches the gateway"
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(call.pid),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Starting)
        }
    );
    let marker = read_marker().expect("read").expect("a child marker");
    assert_eq!(
        (marker.pid, marker.stop_bound_secs, marker.stop_deadline_ms),
        (call.pid, 12, None),
        "the drain's 2 s, shunt's 5 s blocking grace and a 5 s margin"
    );
    assert!(
        marker.start.is_some(),
        "the start time rides beside the pid"
    );
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(clauth_dir().expect("dir").join("gateway-child.json"))
            .expect("marker")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the marker is owner-only");
    }

    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(call.pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::Healthy)
        }
    );
    assert_eq!(rig.calls().len(), 1, "a healthy gateway is never respawned");
}

#[cfg(unix)]
#[test]
fn a_killed_gateway_restarts_after_its_backoff() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    let first = rig.only_call().pid;
    stub::signal(first, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(1)),
        GatewayState::Restarting,
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            restarts: 1,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::Restarting)
        }
    );
    rig.server.wait_serving(false);

    supervisor.step(t0.after(Duration::from_millis(1999)));
    assert_eq!(rig.calls().len(), 1, "the 1 s backoff has not run out");
    supervisor.step(t0.after(secs(2)));
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned once the backoff ran out");
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(calls[1].pid),
            restarts: 1,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Starting)
        }
    );
}

#[cfg(unix)]
#[test]
fn a_shunt_already_answering_on_the_port_blocks_the_spawn_until_it_leaves() {
    let rig = Rig::new("0.49.1");
    let foreign = stub::HttpAnswer::bind(rig.port, 200, r#"{"status":"ok","version":"0.49.1"}"#);
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.49.1".to_string()),
            answerer: Some(Answerer::Shunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawned beside it");
    supervisor.step(t0.after(Duration::from_millis(4999)));
    assert_eq!(
        foreign.hits(),
        1,
        "re-probed on the 5 s cadence, not every step"
    );

    drop(foreign);
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 1, "foreign is never terminal");
    assert_eq!(rig.slot().state, GatewayState::Starting);
}

#[cfg(unix)]
#[test]
fn something_else_answering_on_the_port_reads_foreign_and_not_shunt() {
    let rig = Rig::new("0.49.1");
    let _foreign = stub::HttpAnswer::bind(rig.port, 404, "not found");
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            answerer: Some(Answerer::NotShunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    assert_eq!(rig.calls().len(), 0);
}

#[cfg(unix)]
#[test]
fn a_gateway_below_the_floor_is_stopped_and_held_until_its_binary_changes() {
    let rig = Rig::new("0.47.0");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let pid = rig.only_call().pid;
    rig.server.wait_serving(true);

    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(1)),
        GatewayState::BelowFloor,
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.47.0".to_string()),
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(15),
            }),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::BelowFloor)
        }
    );
    assert!(
        rig.lines.snapshot().contains(&format!(
            "clauth daemon: shunt 0.47.0 is older than 0.48.0, the oldest release clauth supervises; stopping the gateway it started (pid {pid})"
        )),
        "the refusal names both versions: {:?}",
        rig.lines.snapshot()
    );

    supervisor.step(t0.after(secs(600)));
    assert_eq!(rig.calls().len(), 1, "no restart while nothing changed");
    assert_eq!(rig.slot().state, GatewayState::BelowFloor);

    crate::testutil::set_mtime(
        &rig.binary,
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    supervisor.step(t0.after(secs(601)));
    assert_eq!(rig.calls().len(), 2, "a changed binary gets a fresh start");
}

#[cfg(unix)]
#[test]
fn disabled_or_a_missing_config_runs_nothing_and_says_which() {
    let rig = Rig::new("0.49.1");
    rig.save(|record| record.disabled = true);
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Disabled)
        }
    );

    rig.save(|_| {});
    fs::remove_file(&rig.config).expect("remove the config");
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::NoConfig)
        }
    );
    assert_eq!(rig.calls().len(), 0, "neither runs a gateway");
}

#[cfg(unix)]
#[test]
fn turning_disabled_on_stops_the_running_gateway_with_sigterm() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    rig.save(|record| record.disabled = true);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(2)),
        GatewayState::Disabled,
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(15),
            }),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Disabled)
        }
    );
    assert_eq!(rig.calls().len(), 1, "a stop clauth asked for is no crash");
    assert_eq!(
        read_marker().expect("read"),
        None,
        "the marker goes with it"
    );
}

/// A record whose spawn inputs change under a running gateway restarts it
/// with the normal stop: one SIGTERM to the run on the old binary, one spawn
/// of the new one, and no crash counted. The first stub ignores SIGTERM, so
/// the `terms` pin counts every signal it got and can red on a second one.
#[cfg(unix)]
#[test]
fn a_changed_binary_stops_the_running_gateway_once_and_spawns_the_new_one_once() {
    let rig = Rig::new("0.49.1");
    rig.touch("ignore-term");
    let second = rig.home.home().join("bin").join("shunt");
    fs::create_dir_all(second.parent().expect("bin dir")).expect("bin dir");
    fs::copy(&rig.binary, &second).expect("the second stub");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let first = rig.only_call();

    rig.save(|record| record.binary = Some(second.clone()));
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(first.pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Stopping)
        }
    );

    // The first stub ignores SIGTERM (records it and lives on), so the
    // supervisor kills it at its 12 s stop bound, then respawns the new one.
    // The virtual clock reaches that bound microseconds after the SIGTERM, so
    // the kill waits for the stub's own record; the respawn waits for the rig's
    // answerer to see the old stub go, or it probes the port as foreign.
    stub::wait_until("the first stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });
    supervisor.step(t0.after(secs(14)));
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(14)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(
        calls
            .iter()
            .map(|call| call.bin.clone())
            .collect::<Vec<_>>(),
        [rig.binary.clone(), second.clone()],
        "the old binary's run, then the new one's"
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            binary: shown(&second),
            pid: Some(calls[1].pid),
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T14.to_string()),
            ..rig.described(GatewayState::Starting)
        },
        "no backoff: the stop was asked for, and the kill is not a second SIGTERM"
    );
    let stop_line = format!(
        "clauth daemon: stopping the shunt gateway (pid {}): its config, binary or env file changed",
        first.pid
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == stop_line)
            .count(),
        1,
        "the sender side: one stop line, which never coalesces"
    );
    assert_eq!(
        stub::terms(&rig.dir),
        [first.pid],
        "the receiver side: exactly one SIGTERM, not two"
    );
    assert_eq!(
        rig.calls().len(),
        2,
        "the new binary is spawned exactly once"
    );

    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(15)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            binary: shown(&second),
            pid: Some(calls[1].pid),
            version: Some("0.49.1".to_string()),
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T15.to_string()),
            ..rig.described(GatewayState::Healthy)
        }
    );
}

/// The rig's config text with `[server.admin]` added after the bind table, so
/// the same port and drain keep the spawn preparable while the admin-table
/// fact flips.
#[cfg(unix)]
fn with_admin_table(rig: &Rig) -> String {
    format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n[server.admin]\n",
        port = rig.port
    )
}

/// The rig's config text with clauth's own `[server.admin]` entry — the shape
/// [`crate::gateway::add_admin_table`] writes: a table plus a `write_keys`
/// entry carrying clauth's id and `${file:}` key reference.
#[cfg(unix)]
fn with_clauth_admin_table(rig: &Rig) -> String {
    let key_ref = format!(
        "${{file:{}}}",
        rig.home
            .home()
            .join(".clauth")
            .join("gateway-admin-token")
            .display()
    );
    format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n[server.admin]\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"{key_ref}\"\n",
        port = rig.port
    )
}

/// A config that gains `[server.admin]` after the child spawned restarts it
/// with the normal stop: one SIGTERM to the old run, one spawn on the changed
/// config, no crash counted and `restarts` unchanged. The first stub ignores
/// SIGTERM, so the `terms` pin counts every signal it got.
#[cfg(unix)]
#[test]
fn a_config_gaining_the_admin_table_restarts_the_gateway_once_without_a_restart_count() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("ignore-term");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let first = rig.only_call();

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "a config that gained [server.admin] restarts the child"
    );

    stub::wait_until("the first stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });
    supervisor.step(t0.after(secs(14)));
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(14)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned exactly once: {calls:?}");
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
    assert_eq!(
        stub::terms(&rig.dir),
        [first.pid],
        "the receiver side: exactly one SIGTERM, not two"
    );
}

/// The client-token pairs a run saw, as `KEY=VALUE`.
#[cfg(unix)]
fn tokens_env(call: &stub::Invocation) -> Vec<String> {
    call.env
        .iter()
        .filter(|pair| pair.starts_with("SHUNT_CLIENT_TOKENS="))
        .cloned()
        .collect()
}

/// A client-token store change after the child spawned restarts it once with
/// the normal stop: one SIGTERM to the old run, one spawn carrying the new
/// pairs, no crash counted and `restarts` unchanged. A store op that writes
/// nothing restarts nothing. The first stub ignores SIGTERM, so the `terms`
/// pin counts every signal it got.
#[cfg(unix)]
#[test]
fn a_client_token_store_change_restarts_the_gateway_once_without_a_restart_count() {
    let rig = Rig::new("0.49.1");
    let _env = no_shunt_env(&rig.home);
    rig.touch("ignore-term");
    let record = GatewayRecord::load().expect("load").expect("a record");
    let add = |profile: &str| {
        crate::gateway_tokens::add_client_token(&record, profile)
            .expect("add")
            .expose()
            .to_string()
    };
    let kerry = add("kerry");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let first = rig.only_call();
    assert_eq!(
        tokens_env(&first),
        [format!("SHUNT_CLIENT_TOKENS=kerry:{kerry}")]
    );

    assert!(
        !crate::gateway_tokens::remove_client_token(&record, "nobody").expect("remove"),
        "a profile holding no token"
    );
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a store op that writes nothing keeps the child"
    );

    let alice = add("alice");
    supervisor.step(t0.after(secs(3)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "a store change restarts the child"
    );
    stub::wait_until("the first stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });
    supervisor.step(t0.after(secs(15)));
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(15)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned exactly once: {calls:?}");
    assert_eq!(
        tokens_env(&calls[1]),
        [format!("SHUNT_CLIENT_TOKENS=alice:{alice},kerry:{kerry}")]
    );
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
    assert_eq!(
        stub::terms(&rig.dir),
        [first.pid],
        "the receiver side: exactly one SIGTERM, not two"
    );

    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(16)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert_eq!(
        rig.calls().len(),
        2,
        "the new run's own store restarts nothing"
    );
}

/// A hand edit naming another tokens variable (`[server.auth].tokens_env`)
/// while a token is stored changes the variable shunt reads them from, which
/// it reads at spawn alone: the gateway restarts once and the new run carries
/// the pairs in the new variable. An edit leaving the variable as it was
/// restarts nothing.
#[cfg(unix)]
#[test]
fn a_changed_tokens_variable_restarts_the_gateway_once() {
    let rig = Rig::new("0.49.1");
    let _env = no_shunt_env(&rig.home);
    rig.arm_check_stub();
    let record = GatewayRecord::load().expect("load").expect("a record");
    let kerry = crate::gateway_tokens::add_client_token(&record, "kerry")
        .expect("add")
        .expose()
        .to_string();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let base = format!(
        "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );

    fs::write(
        &rig.config,
        format!("{base}[server.auth]\ntokens_env = \"SHUNT_CLIENT_TOKENS\"\n"),
    )
    .expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "the same variable spelled out restarts nothing"
    );

    fs::write(
        &rig.config,
        format!("{base}[server.auth]\ntokens_env = \"SHUNT_OTHER_TOKENS\"\n"),
    )
    .expect("config");
    supervisor.step(t0.after(secs(3)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "another variable restarts the child"
    );
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(4)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned exactly once: {calls:?}");
    assert_eq!(
        calls[1]
            .env
            .iter()
            .filter(|pair| pair.contains("_TOKENS="))
            .cloned()
            .collect::<Vec<_>>(),
        [format!("SHUNT_OTHER_TOKENS=kerry:{kerry}")]
    );
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
}

/// A tokens-variable change is a config hand edit, so its restart passes the
/// same `shunt check` gate the restart-only settings do: a refused check
/// keeps the serving child and is memoized; the fixed edit restarts it once.
#[cfg(unix)]
#[test]
fn a_tokens_variable_change_shunt_check_refuses_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    let _env = no_shunt_env(&rig.home);
    rig.arm_check_stub();
    rig.touch("check-fail");
    let record = GatewayRecord::load().expect("load").expect("a record");
    crate::gateway_tokens::add_client_token(&record, "kerry").expect("add");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;
    let base = format!(
        "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );

    fs::write(
        &rig.config,
        format!("{base}[server.auth]\ntokens_env = \"SHUNT_OTHER_TOKENS\"\n"),
    )
    .expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a refused check keeps the child"
    );
    assert!(stub::alive(pid), "the child serves on");
    assert_eq!(rig.check_runs(), 1, "one check on the refused bytes");
    supervisor.step(t0.after(secs(3)));
    assert_eq!(rig.check_runs(), 1, "unchanged bytes are not re-checked");

    fs::remove_file(rig.dir.join("check-fail")).expect("clear the refusal");
    fs::write(
        &rig.config,
        format!("{base}[server.auth]\ntokens_env = \"SHUNT_THIRD_TOKENS\"\n"),
    )
    .expect("config");
    supervisor.step(t0.after(secs(4)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "a passing check restarts the child"
    );
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(5)),
        GatewayState::Starting,
    );
    assert_eq!(rig.calls().len(), 2, "respawned exactly once");
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
}

/// A store name the env file's own value now holds (the env file edited
/// after the add) keeps the gateway down, the slot naming the name and never
/// a value: shunt refuses a duplicate name whole.
#[cfg(unix)]
#[test]
fn a_store_name_the_env_file_now_holds_keeps_the_gateway_down_naming_it() {
    let rig = Rig::new("0.49.1");
    let _env = no_shunt_env(&rig.home);
    let record = GatewayRecord::load().expect("load").expect("a record");
    crate::gateway_tokens::add_client_token(&record, "kerry").expect("add");
    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n[server.auth]\n",
            rig.port
        ),
    )
    .expect("config");
    fs::write(
        &rig.env_file,
        "GATEWAY_TEST_SECRET=from-env-file\nSHUNT_CLIENT_TOKENS=kerry:k0-secret\n",
    )
    .expect("env file");
    let mut supervisor = rig.supervisor();
    supervisor.step(t0());
    let slot = rig.slot();
    assert_eq!(slot.state, GatewayState::Misconfigured);
    assert_eq!(
        slot.reason,
        Some(format!(
            "the gateway's env file {} and clauth's client-token store both name a client \"kerry\" in SHUNT_CLIENT_TOKENS, and shunt refuses a duplicate name; remove that entry from the env file, or remove clauth's token for that profile",
            rig.env_file.display()
        ))
    );
    assert!(
        stub::invocations(&rig.dir).is_empty(),
        "nothing was spawned"
    );
}

/// An edit to a hot-reloadable setting keeps the child: only the restart-only
/// facts force a restart.
#[cfg(unix)]
#[test]
fn an_edit_to_a_hot_reloadable_setting_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\nsse_keepalive_seconds = 60\n",
            rig.port
        ),
    )
    .expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a hot-reloadable edit keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a hot-reloadable edit");
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// A re-read that does not parse is no evidence: the child keeps running (the
/// "a record that does not load keeps a running gateway" precedent).
#[cfg(unix)]
#[test]
fn a_config_that_becomes_unparseable_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, "[server\nbind = \"127.0.0.1:1\"\n").expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "an unparseable re-read is no evidence and keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for an unparseable config");
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// A config that had the table at spawn and still has it keeps the child.
#[cfg(unix)]
#[test]
fn a_config_that_had_the_admin_table_at_spawn_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a table held since spawn keeps the child"
    );
    assert!(
        stub::alive(pid),
        "never stopped for a table it spawned with"
    );
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// The stop line for an admin-table restart names its cause distinctly from a
/// record change, so `daemon.log` says why.
#[cfg(unix)]
#[test]
fn the_admin_table_restart_names_its_cause() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    let stop_line = format!(
        "clauth daemon: stopping the shunt gateway (pid {pid}): its config changed [server.admin]"
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == stop_line)
            .count(),
        1,
        "the log line names the cause distinctly from a record change"
    );
}

/// A gained table whose `shunt check` refuses keeps the child, logs the
/// refusal once (the daemon's own words, never the check's stderr), and does
/// not re-run the check while the bytes stay unchanged within the re-check
/// bound.
#[cfg(unix)]
#[test]
fn a_gained_table_whose_check_is_refused_keeps_the_child_and_logs_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-fail");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a gained table whose check is refused keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a refused check");
    assert_eq!(rig.calls().len(), 1, "no respawn on a refused check");
    assert_eq!(rig.check_runs(), 1, "one check for the gained table");

    let keep_line = format!(
        "clauth daemon: the shunt gateway's config changed [server.admin], but `{} check --config {}` refused it (exit 1); keeping the running gateway until the check passes; run that command to see why",
        rig.binary.display(),
        rig.config.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == keep_line)
            .count(),
        1,
        "the refusal is logged once, in the daemon's own words"
    );

    // Unchanged bytes within the re-check bound: the memo keeps the check
    // from re-running.
    supervisor.step(t0.after(secs(3)));
    supervisor.step(t0.after(secs(4)));
    assert_eq!(rig.check_runs(), 1, "an unchanged config is not re-checked");
    assert_eq!(rig.calls().len(), 1, "still no respawn");
    assert!(
        stub::alive(pid),
        "the child stays alive across the memoized rounds"
    );
}

/// The same refused config, then fixed (bytes change, check passes): the
/// changed bytes are re-checked and the child restarts exactly once.
#[cfg(unix)]
#[test]
fn a_gained_table_refused_then_fixed_restarts_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-fail");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert!(stub::alive(pid), "the refused check keeps the child");
    assert_eq!(rig.check_runs(), 1, "one check on the refused bytes");

    // The user finishes the edit: the bytes change and the check now passes.
    fs::remove_file(rig.dir.join("check-fail")).expect("clear the refusal");
    fs::write(&rig.config, with_clauth_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(3)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "a fixed config whose check passes restarts the child"
    );
    assert_eq!(rig.check_runs(), 2, "the changed bytes are re-checked once");
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(4)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned exactly once: {calls:?}");
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
}

/// A `shunt check` that outruns its bound keeps the child: the restart is
/// gated on the check passing, and a wedged check is a refusal.
#[cfg(unix)]
#[test]
fn a_check_that_times_out_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-slow");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    let _short = crate::gateway::CheckTimeoutOverride::set(Duration::from_millis(200));
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a timed-out check keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a timed-out check");
    assert_eq!(rig.calls().len(), 1, "no respawn on a timed-out check");
    assert_eq!(rig.check_runs(), 1, "one check ran");
    let keep_line = format!(
        "clauth daemon: the shunt gateway's config changed [server.admin], but `{} check --config {}` ran past 200ms and was stopped; keeping the running gateway until the check passes; run that command to see why it does not finish",
        rig.binary.display(),
        rig.config.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == keep_line)
            .count(),
        1,
        "the timeout is logged once, in the daemon's own words"
    );
}

/// A refusal fixed only through the stub's answer (the config bytes
/// unchanged) is re-checked once the re-check bound elapses on the harness's
/// clock, never before, and restarts exactly once.
#[cfg(unix)]
#[test]
fn a_refusal_fixed_through_the_stub_restarts_once_the_bound_elapses() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-fail");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert!(stub::alive(pid), "the refused check keeps the child");
    assert_eq!(rig.check_runs(), 1, "one check on the refused bytes");

    // Fix only what the stub answers; the config bytes stay byte-identical.
    fs::remove_file(rig.dir.join("check-fail")).expect("clear the refusal");

    supervisor.step(t0.after(secs(61)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "no restart before the re-check bound elapses"
    );
    assert_eq!(rig.check_runs(), 1, "no re-check before the bound");

    // The bound elapses 60 s after the refusal (at 2 s): the unchanged bytes
    // are re-checked, the check now passes, and the child restarts once.
    supervisor.step(t0.after(secs(62)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "a fixed check restarts once the bound elapses"
    );
    assert_eq!(
        rig.check_runs(),
        2,
        "the unchanged bytes are re-checked after the bound"
    );
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(63)),
        GatewayState::Starting,
    );
    assert_eq!(rig.calls().len(), 2, "respawned exactly once");
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
}

/// An unchanged refusal re-checked after each bound still logs once: the
/// refusal line is logged when its text differs from the last one for this
/// child, never once per re-check.
#[cfg(unix)]
#[test]
fn an_unchanged_refusal_rechecked_twice_logs_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-fail");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert_eq!(rig.check_runs(), 1, "the first check");

    // Two re-checks, one per elapsed bound, each still refused.
    supervisor.step(t0.after(secs(62)));
    assert_eq!(rig.check_runs(), 2, "re-checked once the bound elapses");
    supervisor.step(t0.after(secs(63)));
    assert_eq!(
        rig.check_runs(),
        2,
        "the bound restarts at each re-check, never one check per round"
    );
    supervisor.step(t0.after(secs(122)));
    assert_eq!(rig.check_runs(), 3, "re-checked again a bound later");
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert!(stub::alive(pid), "never stopped for an unchanged refusal");
    assert_eq!(rig.calls().len(), 1, "no respawn");

    let keep_line = format!(
        "clauth daemon: the shunt gateway's config changed [server.admin], but `{} check --config {}` refused it (exit 1); keeping the running gateway until the check passes; run that command to see why",
        rig.binary.display(),
        rig.config.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == keep_line)
            .count(),
        1,
        "the refusal is logged once, not once per re-check"
    );
}

/// A `shunt check` whose binary is gone keeps the child and logs the missing
/// binary by path, in the daemon's own words.
#[cfg(unix)]
#[test]
fn a_missing_check_binary_keeps_the_child_and_names_the_binary() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    // The binary the child already runs as is gone by the time the check
    // would run: the running child is unaffected, the check finds nothing.
    fs::remove_file(&rig.binary).expect("remove the binary");
    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a missing check binary keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a missing binary");
    assert_eq!(rig.calls().len(), 1, "no respawn");

    let keep_line = format!(
        "clauth daemon: the shunt gateway's config changed [server.admin], but {} is not there to check it; keeping the running gateway until the check passes",
        rig.binary.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == keep_line)
            .count(),
        1,
        "the missing binary is logged once, by path"
    );
}

/// A check cancelled before it starts — the cancel flag already set — yields
/// the cancellation outcome (never a timeout) and never restarts the child.
#[cfg(unix)]
#[test]
fn a_check_cancelled_before_it_starts_is_cancelled_not_a_timeout_and_never_restarts() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-slow");

    // By value: a cancel already set when the check starts yields the
    // cancellation outcome, never `CheckTimedOut`.
    {
        let mut record = GatewayRecord::new(rig.config.clone()).expect("adoptable");
        record.binary = Some(rig.binary.clone());
        record.env_file = Some(rig.env_file.clone());
        let cancel = AtomicBool::new(true);
        let _short = crate::gateway::CheckTimeoutOverride::set(Duration::from_millis(200));
        let err = crate::gateway::check_adopted_config(&record, &cancel)
            .expect_err("a cancelled check refuses");
        assert_eq!(
            err.downcast_ref::<crate::gateway::ConfigEditRefusal>(),
            None,
            "the cancellation is not a ConfigEditRefusal, never CheckTimedOut"
        );
        assert_eq!(
            err.to_string(),
            format!("{} was cancelled while it ran", rig.binary.display()),
            "the cancellation outcome by value"
        );
    }

    // Through the gate: a cancel already set keeps the child silently.
    let cancel = Arc::new(AtomicBool::new(false));
    let mut supervisor = Supervised(Supervisor::build_with_cancel(
        Gateway::for_daemon(),
        Arc::clone(&rig.handle),
        Arc::clone(&cancel),
    ));
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    cancel.store(true, Ordering::SeqCst);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a check cancelled before it starts never restarts the child"
    );
    assert!(stub::alive(pid), "the child stays alive");
    assert_eq!(rig.calls().len(), 1, "no respawn");
    assert!(
        !rig.lines
            .snapshot()
            .iter()
            .any(|line| line.contains("keeping")),
        "a cancelled check is silent, never a refusal line"
    );
}

/// A gained table shaped as clauth's own entry (the shape `add_admin_table`
/// writes) reads as present and restarts: the fact is the key's presence,
/// never which admin step it needs.
#[cfg(unix)]
#[test]
fn a_gained_table_shaped_as_clauths_own_restarts() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    fs::write(&rig.config, with_clauth_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "a table shaped as clauth's own entry restarts the child"
    );
    assert_eq!(rig.check_runs(), 1, "the gain is gated on one check");
}

/// A child spawned while `[server.admin]` was a non-table value does not
/// restart when the table later appears: the fact reads the key's presence,
/// and a non-table `admin` is already present, so the table's appearance is
/// no change.
#[cfg(unix)]
#[test]
fn a_config_whose_admin_shape_was_not_a_table_at_spawn_does_not_restart_on_a_gained_table() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\nadmin = \"not-a-table\"\n",
            rig.port
        ),
    )
    .expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a non-table admin already present at spawn keeps the child"
    );
    assert!(
        stub::alive(pid),
        "never stopped for a table that was already present as a value"
    );
    assert_eq!(rig.calls().len(), 1, "no respawn");
    assert_eq!(rig.check_runs(), 0, "no check runs when nothing changed");
}

/// Writes `changed` over the rig's config and drives one restart: the child
/// stops (one SIGTERM), a fresh spawn happens, no crash is counted, one check
/// ran, and the stop line names `name` (the changed key paths) by equality.
#[cfg(unix)]
fn expect_restart(
    rig: &Rig,
    supervisor: &mut Supervised,
    t0: Tick,
    first_pid: u32,
    name: &str,
    changed: &str,
) {
    fs::write(&rig.config, changed).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "the restart-only edit restarts the child"
    );
    let stop_line = format!(
        "clauth daemon: stopping the shunt gateway (pid {first_pid}): its config changed {name}"
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == stop_line)
            .count(),
        1,
        "the log names the setting by its key path, never its value"
    );
    // The stub dies on its own SIGTERM: wait for the pipe to fall silent,
    // then step until the reap respawns it (the default-trap restart path,
    // never a kill at the drain bound).
    rig.server.wait_serving(false);
    step_until(rig, supervisor, t0.after(secs(3)), GatewayState::Starting);
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned exactly once: {calls:?}");
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
    assert_eq!(
        stub::terms(&rig.dir),
        [first_pid],
        "the receiver side: exactly one SIGTERM, not two"
    );
    assert_eq!(rig.check_runs(), 1, "the restart is gated on one check");
}

/// The base `[server]` table every restart-only edit starts from.
#[cfg(unix)]
fn base_config(port: u16) -> String {
    format!("[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n")
}

/// A restart-only case: `(spawn config, changed config)` over a port.
#[cfg(unix)]
type RestartCase = dyn Fn(u16) -> (String, String);

/// `bind = "127.0.0.1:3067"` edited to another host keeps the port, so the
/// same rig port keeps answering `/health` while the bind fact flips.
#[cfg(unix)]
#[test]
fn a_bind_edit_restarts_once_and_names_the_setting() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    expect_restart(&rig, &mut supervisor, t0, pid, "server.bind", &changed);
}

/// Losing `[server.admin]` restarts too: the fact is the key's presence in
/// either direction.
#[cfg(unix)]
#[test]
fn removing_the_admin_table_restarts_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        pid,
        "[server.admin]",
        &base_config(rig.port),
    );
}

/// A model's `display_name` is hot-reloadable: editing it never restarts.
#[cfg(unix)]
#[test]
fn a_model_display_name_edit_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n[[models]]\nid = \"fable\"\ndisplay_name = \"Fable\"\n",
            rig.port
        ),
    )
    .expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n[[models]]\nid = \"fable\"\ndisplay_name = \"Renamed\"\n",
            rig.port
        ),
    )
    .expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a display_name edit keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a display_name edit");
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// A comment added above `bind`, or `bind` moved within `[server]`, compares
/// as the same value: no restart.
#[cfg(unix)]
#[test]
fn a_comment_or_key_move_above_bind_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let reordered = format!(
        "# a comment above bind\n[server]\nshutdown_timeout_seconds = 2\nbind = \"127.0.0.1:{}\"\n",
        rig.port
    );
    fs::write(&rig.config, reordered).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a comment and key move keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a comment or key move");
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// A refused restart-only edit keeps the child, logs the refusal once, and
/// does not re-run the check while the bytes stay unchanged.
#[cfg(unix)]
#[test]
fn a_refused_bind_edit_keeps_the_child_and_logs_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-fail");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    fs::write(&rig.config, changed).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a refused bind edit keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a refused check");
    assert_eq!(rig.calls().len(), 1, "no respawn on a refused check");
    assert_eq!(rig.check_runs(), 1, "one check for the changed bind");

    let keep_line = format!(
        "clauth daemon: the shunt gateway's config changed server.bind, but `{} check --config {}` refused it (exit 1); keeping the running gateway until the check passes; run that command to see why",
        rig.binary.display(),
        rig.config.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == keep_line)
            .count(),
        1,
        "the refusal is logged once, naming the setting, never its value"
    );

    supervisor.step(t0.after(secs(3)));
    supervisor.step(t0.after(secs(4)));
    assert_eq!(rig.check_runs(), 1, "an unchanged config is not re-checked");
    assert_eq!(rig.calls().len(), 1, "still no respawn");
    assert!(
        stub::alive(pid),
        "the child stays alive across memoized rounds"
    );
}

/// Every member of the restart-only fact set, in every transition its kind
/// admits. The transitions are generated from the fact table itself: the
/// profiles below are cross-checked against `FACT_SPECS` (length, label,
/// walk, kind) before any transition runs, so a member added, removed,
/// re-pathed, or re-kinded fails this test instead of drifting.
#[cfg(unix)]
#[test]
fn every_restart_only_setting_restarts_once_when_it_changes() {
    let profiles = member_profiles();
    assert_eq!(
        profiles.len(),
        FACT_SPECS.len(),
        "the test profiles drifted from the fact set's length"
    );
    for (profile, spec) in profiles.iter().zip(FACT_SPECS.iter()) {
        assert_eq!(
            profile.path, spec.path,
            "a test profile's label drifted from the fact set: {}",
            spec.path
        );
        assert_eq!(
            profile.keys, spec.keys,
            "a test profile's walk drifted from the fact set: {}",
            spec.path
        );
        assert_eq!(
            kind_name(profile.kind),
            kind_name(spec.kind),
            "a test profile's kind drifted from the fact set: {}",
            spec.path
        );
    }
    for profile in &profiles {
        match profile.kind {
            FactKind::Value => {
                // absent -> present, present -> a different value, and back to
                // absent.
                restart_case(profile, profile.absent, profile.present_a);
                restart_case(
                    profile,
                    profile.present_a,
                    profile.present_b.expect("a Value fact carries present_b"),
                );
                restart_case(profile, profile.present_a, profile.absent);
            }
            FactKind::Present => {
                // absent -> present and back; a content edit inside the present
                // table is a keep, covered by the non-member test.
                restart_case(profile, profile.absent, profile.present_a);
                restart_case(profile, profile.present_a, profile.absent);
            }
            FactKind::NonEmpty => {
                let empty = profile
                    .empty
                    .expect("a NonEmpty fact carries its empty config");
                // absent -> non-empty, [] -> non-empty, and each back.
                restart_case(profile, profile.absent, profile.present_a);
                restart_case(profile, empty, profile.present_a);
                restart_case(profile, profile.present_a, empty);
                restart_case(profile, profile.present_a, profile.absent);
            }
        }
    }
}

/// A restart-only fact's test profile: the fact-table entries it must match,
/// and the config fragments that drive every transition its kind admits.
#[cfg(unix)]
struct MemberProfile {
    /// The log key path, matched against `FactSpec::path`.
    path: &'static str,
    /// The document walk, matched against `FactSpec::keys`.
    keys: &'static [&'static str],
    /// The fact kind, matched against `FactSpec::kind`.
    kind: FactKind,
    /// The config with the member's key absent.
    absent: fn(u16) -> String,
    /// The config with the member present (value A).
    present_a: fn(u16) -> String,
    /// The config with the member present (value B, different): a `Value` fact
    /// only — a `Present` or `NonEmpty` member's content edit is a keep, not a
    /// restart.
    present_b: Option<fn(u16) -> String>,
    /// A `NonEmpty` member's config with the key present but `[]`.
    empty: Option<fn(u16) -> String>,
    /// Env-file line(s) pinning the probe port while the config's `bind` key
    /// is absent, so `server.bind`'s absent <-> present transitions stay
    /// spawnable (the daemon would otherwise probe shunt's default port).
    env_pin: Option<fn(u16) -> String>,
}

/// The fact's kind as a name, so the cross-check compares kinds without a
/// `PartialEq` derive on `FactKind`.
#[cfg(unix)]
fn kind_name(kind: FactKind) -> &'static str {
    match kind {
        FactKind::Value => "Value",
        FactKind::Present => "Present",
        FactKind::NonEmpty => "NonEmpty",
    }
}

/// The test profiles, in the fact set's order.
#[cfg(unix)]
fn member_profiles() -> Vec<MemberProfile> {
    vec![
        MemberProfile {
            path: "server.bind",
            keys: &["server", "bind"],
            kind: FactKind::Value,
            absent: |_| "[server]\nshutdown_timeout_seconds = 2\n".to_string(),
            present_a: base_config,
            present_b: Some(|p| {
                format!("[server]\nbind = \"0.0.0.0:{p}\"\nshutdown_timeout_seconds = 2\n")
            }),
            empty: None,
            env_pin: Some(|p| format!("SHUNT_SERVER__BIND=127.0.0.1:{p}")),
        },
        MemberProfile {
            path: "server.max_concurrent_requests",
            keys: &["server", "max_concurrent_requests"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| format!("{}max_concurrent_requests = 2048\n", base_config(p)),
            present_b: Some(|p| format!("{}max_concurrent_requests = 4096\n", base_config(p))),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.shutdown_timeout_seconds",
            keys: &["server", "shutdown_timeout_seconds"],
            kind: FactKind::Value,
            absent: |p| format!("[server]\nbind = \"127.0.0.1:{p}\"\n"),
            present_a: base_config,
            present_b: Some(|p| {
                format!("[server]\nbind = \"127.0.0.1:{p}\"\nshutdown_timeout_seconds = 3\n")
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.access_control]",
            keys: &["server", "access_control"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.access_control]\nallow_cidrs = [\"10.0.0.0/8\"]\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.access_control]\nallow_cidrs = [\"10.0.0.0/16\"]\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.limits.max_request_header_bytes",
            keys: &["server", "limits", "max_request_header_bytes"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.limits]\nmax_request_header_bytes = 512\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.limits]\nmax_request_header_bytes = 1024\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.limits.max_url_length",
            keys: &["server", "limits", "max_url_length"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| format!("{}[server.limits]\nmax_url_length = 256\n", base_config(p)),
            present_b: Some(|p| {
                format!("{}[server.limits]\nmax_url_length = 512\n", base_config(p))
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.rate_limits]",
            keys: &["server", "rate_limits"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.rate_limits.device_verify]\nmax = 3\nwindow_seconds = 20\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.rate_limits.device_verify]\nmax = 5\nwindow_seconds = 20\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.admin]",
            keys: &["server", "admin"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.admin]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.gateway]",
            keys: &["server", "gateway"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.gateway]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.spend]",
            keys: &["server", "spend"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.spend]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.spend.state_path",
            keys: &["server", "spend", "state_path"],
            kind: FactKind::Value,
            absent: |p| format!("{}[server.spend]\n", base_config(p)),
            present_a: |p| {
                format!(
                    "{}[server.spend]\nstate_path = \"/tmp/spend.json\"\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.spend]\nstate_path = \"/tmp/spend-2.json\"\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.codex_endpoint]",
            keys: &["server", "codex_endpoint"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.codex_endpoint]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.usage]",
            keys: &["server", "usage"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.usage]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.oauth_usage]",
            keys: &["server", "oauth_usage"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.oauth_usage]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[sentry]",
            keys: &["sentry"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[sentry]\ndsn = \"\"\nenvironment = \"dev\"\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[sentry]\ndsn = \"\"\nenvironment = \"prod\"\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[otel]",
            keys: &["otel"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[otel]\nendpoint = \"http://localhost:4318\"\nsample_ratio = 0.5\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[otel]\nendpoint = \"http://localhost:4318\"\nsample_ratio = 1.0\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.pool.usage_refresh_seconds",
            keys: &["server", "pool", "usage_refresh_seconds"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.pool]\nusage_refresh_seconds = 120\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.pool]\nusage_refresh_seconds = 300\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.pool.state_path",
            keys: &["server", "pool", "state_path"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.pool]\nstate_path = \"/tmp/pool.json\"\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.pool]\nstate_path = \"/tmp/pool-2.json\"\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.status]",
            keys: &["server", "status"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.status]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.status].sources",
            keys: &["server", "status", "sources"],
            kind: FactKind::NonEmpty,
            absent: |p| format!("{}[server.status]\n", base_config(p)),
            present_a: |p| {
                format!(
                    "{}[server.status]\n[[server.status.sources]]\nprovider = \"claude\"\nurl = \"https://status.claude.com/api/v2/summary.json\"\n",
                    base_config(p)
                )
            },
            present_b: None,
            empty: Some(|p| format!("{}[server.status]\nsources = []\n", base_config(p))),
            env_pin: None,
        },
        MemberProfile {
            path: "server.status.refresh_seconds",
            keys: &["server", "status", "refresh_seconds"],
            kind: FactKind::Value,
            absent: |p| format!("{}[server.status]\n", base_config(p)),
            present_a: |p| format!("{}[server.status]\nrefresh_seconds = 120\n", base_config(p)),
            present_b: Some(|p| {
                format!("{}[server.status]\nrefresh_seconds = 300\n", base_config(p))
            }),
            empty: None,
            env_pin: None,
        },
    ]
}

/// One restart-only change, driven end to end in a fresh sandbox: spawn on
/// `spawn(p)`, edit to `changed(p)`, and assert exactly one restart gated on
/// one check, the stop line naming the profile's key path by equality.
#[cfg(unix)]
fn restart_case(profile: &MemberProfile, spawn: fn(u16) -> String, changed: fn(u16) -> String) {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    if let Some(pin) = profile.env_pin {
        fs::write(
            &rig.env_file,
            format!("GATEWAY_TEST_SECRET=from-env-file\n{}\n", pin(rig.port)),
        )
        .expect("env pin");
    }
    let spawn_config = spawn(rig.port);
    fs::write(&rig.config, spawn_config).expect("spawn config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "{}: spawns healthy",
        profile.path
    );
    let first = rig.only_call().pid;
    let changed_config = changed(rig.port);
    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        first,
        profile.path,
        &changed_config,
    );
}

/// Representative hot-reloadable non-members: editing them never restarts.
#[cfg(unix)]
#[test]
fn hot_reloadable_settings_never_restart_the_child() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let base = base_config(rig.port);
    let edits: Vec<(&str, String)> = vec![
        (
            "an [[upstreams]] field",
            format!(
                "{base}[[upstreams]]\nname = \"u\"\nkind = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"\n"
            ),
        ),
        (
            "[[models]]",
            format!("{base}[[models]]\nid = \"fable\"\ndisplay_name = \"Fable\"\n"),
        ),
        (
            "[[routes]]",
            format!("{base}[[routes]]\nmodel = \"fable\"\n"),
        ),
        ("[server.auth]", format!("{base}[server.auth]\n")),
        (
            "[server.pool].hard_threshold",
            format!("{base}[server.pool]\nhard_threshold = 0.9\n"),
        ),
    ];
    for (name, changed) in edits {
        fs::write(&rig.config, changed).expect("config");
        supervisor.step(t0.after(secs(2)));
        assert_eq!(
            rig.slot().state,
            GatewayState::Healthy,
            "{name} keeps the child"
        );
        assert!(stub::alive(pid), "{name} never stops the child");
    }
    assert_eq!(
        rig.calls().len(),
        1,
        "no respawn for any hot-reloadable edit"
    );
}

/// One hot-reloadable edit inside a present restart-only table, driven end to
/// end in a fresh sandbox: spawn on `spawn_config`, edit to `changed_config`,
/// and assert no restart and no check.
#[cfg(unix)]
fn keep_case(name: &str, case: &RestartCase) {
    let rig = Rig::new("0.49.1");
    let (spawn_config, changed_config) = case(rig.port);
    fs::write(&rig.config, spawn_config).expect("spawn config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "{name}: spawns healthy"
    );
    let pid = rig.only_call().pid;

    fs::write(&rig.config, changed_config).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "{name} keeps the child"
    );
    assert!(stub::alive(pid), "{name} never stops the child");
    assert_eq!(rig.calls().len(), 1, "no respawn for {name}");
}

/// A non-member sibling inside a restart-only table hot-reloads: editing it
/// never restarts the child, even while the member table is present.
#[cfg(unix)]
#[test]
fn a_non_member_edit_inside_a_member_table_keeps_the_child() {
    // One keep case per member table: a sibling or content edit that leaves
    // every fact inside it unchanged. `[server.usage]` and `[server.oauth_usage]`
    // have no fields of their own, so an arbitrary key stands in for their
    // content (presence alone is the fact).
    let cases: Vec<(&str, Box<RestartCase>)> = vec![
        (
            "[server] sse_keepalive_seconds beside the bind facts",
            Box::new(|p| {
                (
                    format!("{}sse_keepalive_seconds = 60\n", base_config(p)),
                    format!("{}sse_keepalive_seconds = 61\n", base_config(p)),
                )
            }),
        ),
        (
            "a [[server.admin.write_keys]] edit under a present table",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.admin]\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"first\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.admin]\n[[server.admin.write_keys]]\nid = \"clauth\"\nkey = \"second\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.limits] max_request_bytes",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.limits]\nmax_request_bytes = 1000\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.limits]\nmax_request_bytes = 2000\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.spend] blocked_message",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.spend]\nblocked_message = \"nope\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.spend]\nblocked_message = \"denied\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.pool] hard_threshold beside usage_refresh_seconds",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.pool]\nusage_refresh_seconds = 120\nhard_threshold = 0.9\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.pool]\nusage_refresh_seconds = 120\nhard_threshold = 0.95\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.gateway] trust_forwarded_for",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.gateway]\ntrust_forwarded_for = true\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.gateway]\ntrust_forwarded_for = false\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.codex_endpoint] provider",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.codex_endpoint]\nprovider = \"codex\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.codex_endpoint]\nprovider = \"other\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.usage] a field",
            Box::new(|p| {
                (
                    format!("{}[server.usage]\nnote = \"a\"\n", base_config(p)),
                    format!("{}[server.usage]\nnote = \"b\"\n", base_config(p)),
                )
            }),
        ),
        (
            "[server.oauth_usage] a field",
            Box::new(|p| {
                (
                    format!("{}[server.oauth_usage]\nnote = \"a\"\n", base_config(p)),
                    format!("{}[server.oauth_usage]\nnote = \"b\"\n", base_config(p)),
                )
            }),
        ),
        (
            "a [[server.status.sources]] URL edit with the list non-empty",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.status]\n[[server.status.sources]]\nprovider = \"claude\"\nurl = \"https://status.claude.com/api/v2/summary.json\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.status]\n[[server.status.sources]]\nprovider = \"claude\"\nurl = \"https://status.anthropic.com/api/v2/summary.json\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.status] sources absent -> sources = [] (shunt starts no poller either way)",
            Box::new(|p| {
                (
                    format!("{}[server.status]\nrefresh_seconds = 300\n", base_config(p)),
                    format!(
                        "{}[server.status]\nrefresh_seconds = 300\nsources = []\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.status] sources = [] -> absent",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.status]\nrefresh_seconds = 300\nsources = []\n",
                        base_config(p)
                    ),
                    format!("{}[server.status]\nrefresh_seconds = 300\n", base_config(p)),
                )
            }),
        ),
    ];
    for (name, case) in &cases {
        keep_case(name, case.as_ref());
    }
}

/// Changing two restart-only members at once names both key paths in fact-set
/// order, comma-separated.
#[cfg(unix)]
#[test]
fn a_two_setting_edit_names_both_key_paths_in_order() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n[server.admin]\n",
        rig.port
    );
    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        pid,
        "server.bind, [server.admin]",
        &changed,
    );
}

/// A child whose spawn-time read failed (a fact path crossed a non-table)
/// adopts the first readable round's facts as its baseline, logged once, and
/// then restarts on a later change like any other child.
#[cfg(unix)]
#[test]
fn a_child_whose_spawn_read_failed_adopts_the_first_readable_round_then_restarts_on_a_change() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\nlimits = \"not-a-table\"\n",
            rig.port
        ),
    )
    .expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    // The first readable round adopts its facts as the baseline, logged once.
    fs::write(&rig.config, base_config(rig.port)).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "adopting the baseline never restarts"
    );
    assert!(stub::alive(pid), "no stop while adopting the baseline");
    assert_eq!(
        rig.calls().len(),
        1,
        "no respawn while adopting the baseline"
    );
    assert_eq!(
        rig.check_runs(),
        0,
        "no check runs while adopting the baseline"
    );
    let adopt_line = "clauth daemon: the shunt gateway's config could not be read at spawn; adopting the current config as its restart baseline";
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == adopt_line)
            .count(),
        1,
        "the adoption is logged once"
    );

    // A later change restarts like any other child.
    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    expect_restart(
        &rig,
        &mut supervisor,
        t0.after(secs(2)),
        pid,
        "server.bind",
        &changed,
    );
}

/// A config whose `[sentry]` subtree holds `nan` compares equal to itself
/// round after round (the mark holds a digest, never the raw value): an
/// unchanged round never restarts.
#[cfg(unix)]
#[test]
fn a_config_holding_nan_in_a_value_fact_does_not_restart_on_an_unchanged_round() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    fs::write(
        &rig.config,
        format!(
            "{}[sentry]\ndsn = \"\"\ntraces_sample_rate = nan\n",
            base_config(rig.port)
        ),
    )
    .expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    // The config is untouched: a NaN value fact must not read as changed.
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "an unchanged NaN fact never restarts"
    );
    assert!(
        stub::alive(pid),
        "the child stays alive across unchanged rounds"
    );
    assert_eq!(rig.calls().len(), 1, "no respawn on an unchanged NaN fact");
    assert_eq!(
        rig.check_runs(),
        0,
        "no check runs on an unchanged NaN fact"
    );
}

/// The stop bound is the drain shunt runs with plus its 5 s blocking grace and
/// a 5 s margin; the env file's 3 s drain outranks the config's 2 s, so the
/// kill lands 13 s after the SIGTERM a stub that ignores it got.
#[cfg(unix)]
#[test]
fn a_gateway_ignoring_sigterm_is_killed_at_its_drain_bound() {
    let rig = Rig::new("0.49.1");
    fs::write(
        &rig.env_file,
        "GATEWAY_TEST_SECRET=from-env-file\nSHUNT_SERVER__SHUTDOWN_TIMEOUT_SECONDS=3\n",
    )
    .expect("env file");
    rig.touch("ignore-term");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    // Its record is written once its SIGTERM trap is set.
    let pid = rig.only_call().pid;

    rig.save(|record| record.disabled = true);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    // Counted on the supervisor's own kill line: `kill -s 0` still answers
    // for a killed child nobody has reaped yet.
    let kill = format!(
        "clauth daemon: the shunt gateway (pid {pid}) did not exit within 13s of SIGTERM; killing it"
    );
    let kills = || rig.lines.snapshot().iter().filter(|l| **l == kill).count();
    supervisor.step(t0.after(Duration::from_millis(14_999)));
    assert_eq!(kills(), 0, "SIGTERM ignored, the bound not yet run out");
    assert_eq!(rig.slot().state, GatewayState::Stopping);

    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(15)),
        GatewayState::Disabled,
    );
    assert_eq!(kills(), 1, "killed once, at the bound");
    assert_eq!(
        rig.slot().last_exit,
        Some(ExitReport {
            code: None,
            signal: Some(9),
        })
    );
}

#[cfg(unix)]
#[test]
fn a_gateway_that_never_answers_reads_unhealthy_and_keeps_running() {
    let rig = Rig::new("0.49.1");
    rig.touch("no-health");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let pid = rig.only_call().pid;

    supervisor.step(t0.after(Duration::from_millis(9999)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Starting,
        "inside the 10 s grace"
    );
    supervisor.step(t0.after(secs(10)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(pid),
            since: Some(AT_T10.to_string()),
            ..rig.described(GatewayState::Unhealthy)
        }
    );
    supervisor.step(t0.after(secs(120)));
    // A second step reaps a child the first one killed, which a `kill -s 0`
    // probe cannot tell from a live one.
    supervisor.step(t0.after(secs(120)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Unhealthy,
        "never killed for failing its probes"
    );
    assert!(stub::alive(pid), "never killed for failing its probes");
    assert_eq!(rig.calls().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_missing_binary_is_named_and_picked_up_once_it_exists() {
    let rig = Rig::new("0.49.1");
    let missing = rig.home.home().join("bin").join("shunt");
    rig.save(|record| record.binary = Some(missing.clone()));
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            binary: shown(&missing),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::BinaryMissing)
        }
    );

    fs::create_dir_all(missing.parent().expect("bin dir")).expect("bin dir");
    fs::copy(&rig.binary, &missing).expect("install the stub");
    supervisor.step(t0.after(Duration::from_millis(4999)));
    assert_eq!(rig.calls().len(), 0, "looked at again on the 5 s cadence");
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 1);
    assert_eq!(rig.slot().since.as_deref(), Some(AT_T5));
}

#[cfg(unix)]
#[test]
fn an_orphan_matching_its_recorded_start_is_stopped_before_a_fresh_spawn() {
    let rig = Rig::new("0.49.1");
    let mut before = rig.supervisor();
    let t0 = t0();
    before.step(t0);
    rig.server.wait_serving(true);
    // The daemon dies hard: its gateway and marker stay behind.
    let mut orphan = Owned(before.abandon().expect("a running gateway"));
    let orphan_pid = orphan.0.id();

    let mut after = rig.supervisor();
    after.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(orphan_pid),
            since: Some(AT_T1.to_string()),
            ..blank(GatewayState::Stopping)
        }
    );
    let status = orphan.0.wait().expect("reap the orphan");
    assert_eq!(
        ExitReport::from(status),
        ExitReport {
            code: None,
            signal: Some(15),
        },
        "SIGTERM, shunt's graceful stop"
    );
    rig.server.wait_serving(false);

    after.step(t0.after(secs(2)));
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "a fresh gateway once the orphan is gone");
    assert_eq!(rig.slot().pid, Some(calls[1].pid));
}

#[cfg(unix)]
#[test]
fn a_recorded_pid_whose_start_differs_is_never_signalled() {
    let rig = Rig::new("0.49.1");
    let mut stranger = Owned(
        Command::new(&rig.binary)
            .args(["run", "--config", "stranger.toml"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the stranger"),
    );
    rig.server.wait_serving(true);
    write_marker(&ChildMarker {
        pid: stranger.0.id(),
        // No process mints this: a real start time is a tick count.
        start: Some("never-a-start-time".to_string()),
        stop_bound_secs: 12,
        stop_deadline_ms: None,
    })
    .expect("marker");
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        stranger.0.try_wait().expect("try_wait"),
        None,
        "a pid with another start time is someone else's"
    );
    assert_eq!(
        read_marker().expect("read"),
        None,
        "the stale marker is dropped"
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.49.1".to_string()),
            answerer: Some(Answerer::Shunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 1, "only the stranger's own run: {calls:?}");
    assert_eq!(calls[0].args, ["run", "--config", "stranger.toml"]);
}

/// A daemon exiting on a signal SIGTERMs its gateway and leaves the deadline
/// in the marker when the gateway outlives its budget. The next daemon never
/// sends a second SIGTERM (shunt reads one as "skip the drain"): it waits the
/// recorded deadline out, then kills.
#[cfg(unix)]
#[test]
fn a_stop_left_running_by_an_exiting_daemon_is_finished_at_its_recorded_deadline() {
    let rig = Rig::new("0.49.1");
    rig.touch("ignore-term");
    let mut before = rig.supervisor();
    before.step(t0());
    // Its record is written once its SIGTERM trap is set.
    rig.only_call();
    before.shutdown(Instant::now());
    let recorded = read_marker().expect("read").expect("a marker");
    let deadline_ms = recorded.stop_deadline_ms.expect("the stop's deadline");
    let mut orphan = Owned(before.abandon().expect("still running"));

    let mut after = rig.supervisor();
    let early = Tick {
        at: Instant::now(),
        wall_ms: deadline_ms - 1,
    };
    after.step(early);
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    assert_eq!(
        read_marker().expect("read"),
        Some(recorded),
        "the recorded stop stands as written"
    );
    assert_eq!(orphan.0.try_wait().expect("try_wait"), None);

    after.step(Tick {
        at: Instant::now(),
        wall_ms: deadline_ms,
    });
    let mut status = None;
    stub::wait_until("the orphan exits at its deadline", secs(10), || {
        status = orphan.0.try_wait().expect("try_wait");
        status.is_some()
    });
    assert_eq!(
        ExitReport::from(status.expect("exited")),
        ExitReport {
            code: None,
            signal: Some(9),
        }
    );
    assert_eq!(
        stub::terms(&rig.dir),
        [orphan.0.id()],
        "the exiting daemon's one SIGTERM, never a second"
    );
    rig.server.wait_serving(false);
    after.step(Tick {
        at: Instant::now(),
        wall_ms: deadline_ms,
    });
    assert_eq!(rig.calls().len(), 2, "a fresh gateway once it is gone");
}

#[cfg(unix)]
#[test]
fn the_supervisor_thread_stops_its_gateway_on_shutdown_and_joins() {
    let rig = Rig::new("0.49.1");
    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes a healthy gateway", secs(15), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Healthy)
    });
    let pid = rig.only_call().pid;

    assert!(thread.shutdown(DAEMON_STOP_BUDGET), "answered and joined");
    assert!(!stub::alive(pid), "the gateway is gone");
    assert_eq!(read_marker().expect("read"), None);
    rig.server.wait_serving(false);
}

/// CX-1 for the slot: neither an env-file value nor the admin token reaches
/// it, in a running state or in the refusal a bad env file gets.
#[cfg(unix)]
#[test]
fn the_slot_never_carries_an_env_value_or_the_admin_token() {
    let env_canary = "clauth-canary-gateway-env-7f3a";
    let line_canary = "clauth-canary-gateway-line-7f3a";
    let rig = Rig::new("0.49.1");
    let token = crate::gateway::ensure_admin_token().expect("token");
    fs::write(&rig.env_file, format!("GATEWAY_TEST_SECRET={env_canary}\n")).expect("env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert_eq!(
        rig.only_call().secret,
        env_canary,
        "the canary did reach the gateway"
    );

    let live = crate::daemon::LiveStores {
        gateway: Arc::clone(&rig.handle),
        ..Default::default()
    };
    let snapshot = live.snapshot();
    let config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    let body = serde_json::to_string(&crate::daemon::build_status(
        &config,
        300_000,
        Some(&snapshot.signals()),
        false,
    ))
    .expect("body");
    assert!(body.contains(r#""state":"healthy""#), "{body}");
    for canary in [env_canary, token.expose()] {
        assert!(!body.contains(canary), "{canary} leaked: {body}");
    }

    supervisor.kill_for_test();
    rig.server.wait_serving(false);
    fs::write(
        &rig.env_file,
        format!("GATEWAY_TEST_SECRET={env_canary}\nLINE={line_canary}\0\n"),
    )
    .expect("env file");
    let mut refused = rig.supervisor();
    refused.step(t0.after(secs(2)));
    let slot = rig.slot();
    assert_eq!(
        slot.reason,
        Some(format!(
            "in env file {}: line 2 holds a NUL byte; systemd refuses such a file whole, and so does clauth: remove the byte",
            rig.env_file.display()
        ))
    );
    let bytes = serde_json::to_string(&slot).expect("slot");
    for canary in [env_canary, line_canary, token.expose()] {
        assert!(!bytes.contains(canary), "{canary} leaked: {bytes}");
    }
}

/// A shutdown preempts a probe: while the supervisor's child probe blocks on
/// the seam, the shutdown reaches the running gateway as its one SIGTERM and
/// answers inside `DAEMON_STOP_BUDGET`.
#[cfg(unix)]
#[test]
fn a_shutdown_preempts_a_probe_and_stops_the_gateway_inside_the_budget() {
    let rig = Rig::new("0.49.1");
    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes a healthy gateway", secs(15), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Healthy)
    });
    let pid = rig.only_call().pid;

    // Hold the next child probe (they run every 5 s) on the seam, then ask
    // for a shutdown: it must preempt the probe, not wait it out.
    let seam = probe_seam::arm();
    seam.wait_entered();
    assert!(
        thread.shutdown(DAEMON_STOP_BUDGET),
        "answered and joined inside the budget"
    );
    seam.release();
    assert!(!stub::alive(pid), "the gateway is gone");
    assert_eq!(stub::terms(&rig.dir), [pid], "exactly one SIGTERM");
    assert_eq!(read_marker().expect("read"), None);
    rig.server.wait_serving(false);
}

/// A shutdown landing while a stop is already underway sends no second
/// SIGTERM: shunt reads a second signal as "skip the drain".
#[cfg(unix)]
#[test]
fn a_shutdown_during_an_underway_stop_sends_no_second_sigterm() {
    let rig = Rig::new("0.49.1");
    rig.touch("ignore-term");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let pid = rig.only_call().pid;

    rig.save(|record| record.disabled = true);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    // The stub's trap writes its record a moment after the SIGTERM lands.
    stub::wait_until("the stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });

    supervisor.shutdown(Instant::now());
    assert_eq!(
        stub::terms(&rig.dir),
        [pid],
        "the disable's one SIGTERM, never a second from the shutdown"
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| line.starts_with("clauth daemon: stopping the shunt gateway (pid "))
            .count(),
        1,
        "one stop line, not two"
    );
}

/// A listener that takes the connection and never answers reads `foreign`
/// with `no_answer`, never a spawn beside a wedged answerer.
#[cfg(unix)]
#[test]
fn a_wedged_listener_on_the_port_reads_foreign_and_no_answer() {
    let rig = Rig::new("0.49.1");
    let _hold = stub::HoldListener::bind(rig.port);
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            answerer: Some(Answerer::NoAnswer),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawned beside it");
}

/// A foreign answerer's version is any string; it enters `daemon.log` through
/// `{:?}`, so a forged newline cannot write a second log line.
#[cfg(unix)]
#[test]
fn a_foreign_version_with_a_newline_enters_the_log_through_debug() {
    let rig = Rig::new("0.49.1");
    let _foreign = stub::HttpAnswer::bind(
        rig.port,
        200,
        r#"{"status":"ok","version":"0.49.1\nforged"}"#,
    );
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.49.1\nforged".to_string()),
            answerer: Some(Answerer::Shunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    let expected = format!(
        "clauth daemon: port {} already answers /health (shunt {:?}); not starting the managed gateway beside it",
        rig.port, "0.49.1\nforged"
    );
    let snapshot = rig.lines.snapshot();
    let foreign_lines: Vec<&str> = snapshot
        .iter()
        .filter(|line| line.starts_with("clauth daemon: port "))
        .map(|line| line.as_str())
        .collect();
    assert_eq!(
        foreign_lines,
        vec![expected.as_str()],
        "the version is repr'd, not interpolated raw"
    );
}

/// The env file's skipped lines are named once, by number only, when a spawn
/// runs without them; a repeated set does not log again.
#[cfg(unix)]
#[test]
fn an_env_file_with_skipped_lines_says_which_once() {
    let rig = Rig::new("0.49.1");
    fs::write(
        &rig.env_file,
        "GATEWAY_TEST_SECRET=from-env-file\nexport SKIPPED=1\nNO_EQUALS\n",
    )
    .expect("env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let call = rig.only_call();
    assert_eq!(call.secret, "from-env-file", "the good line still loads");
    let line = format!(
        "clauth daemon: the gateway's env file {} assigns nothing on line(s) 2, 3 (systemd skips such lines too); the gateway runs without them",
        rig.env_file.display()
    );
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "the skipped lines are named once, by number: {:?}",
        rig.lines.snapshot()
    );

    // A second spawn with the same skipped set does not log again.
    stub::signal(call.pid, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(1)),
        GatewayState::Restarting,
    );
    rig.server.wait_serving(false);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.calls().len(), 2, "respawned");
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "the same skipped set is not logged twice"
    );
}

/// The restart backoff doubles (1 s, 2 s, 4 s), caps at a minute, and resets
/// to 1 s after a healthy minute.
#[cfg(unix)]
#[test]
fn the_backoff_doubles_caps_at_a_minute_and_resets_after_a_healthy_minute() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    let crash = |rig: &Rig, supervisor: &mut Supervised, tick: Tick| {
        let pid = rig.calls().last().expect("a run").pid;
        stub::signal(pid, "KILL");
        step_until(rig, supervisor, tick, GatewayState::Restarting);
        rig.server.wait_serving(false);
    };

    // crash 0 -> 1 s
    let mut tick = t0.after(secs(1));
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(999)));
    assert_eq!(rig.calls().len(), 1, "no spawn inside the 1 s backoff");
    tick = t0.after(secs(2));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 2, "crash 0 respawns after 1 s");

    // crash 1 -> 2 s
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(1_999)));
    assert_eq!(rig.calls().len(), 2, "no spawn inside the 2 s backoff");
    tick = t0.after(secs(4));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 3, "crash 1 respawns after 2 s");

    // crash 2 -> 4 s
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(3_999)));
    assert_eq!(rig.calls().len(), 3, "no spawn inside the 4 s backoff");
    tick = t0.after(secs(8));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 4, "crash 2 respawns after 4 s");

    // crash 3 -> 8 s, crash 4 -> 16 s, crash 5 -> 32 s
    crash(&rig, &mut supervisor, tick);
    tick = t0.after(secs(16));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 5, "crash 3 respawns after 8 s");

    crash(&rig, &mut supervisor, tick);
    tick = t0.after(secs(32));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 6, "crash 4 respawns after 16 s");

    crash(&rig, &mut supervisor, tick);
    tick = t0.after(secs(64));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 7, "crash 5 respawns after 32 s");

    // crash 6 -> the 60 s cap (the doubling's 64 s is capped)
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(59_999)));
    assert_eq!(rig.calls().len(), 7, "no spawn inside the 60 s cap");
    tick = t0.after(secs(124));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 8, "crash 6 respawns at the 60 s cap");

    // A minute healthy resets it: the next crash waits 1 s again.
    rig.server.wait_serving(true);
    let healthy = tick.after(secs(1));
    supervisor.step(healthy);
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let crash_tick = healthy.after(secs(61));
    crash(&rig, &mut supervisor, crash_tick);
    supervisor.step(crash_tick.after(secs(1)));
    assert_eq!(
        rig.calls().len(),
        9,
        "a healthy minute resets the backoff to 1 s"
    );
}

/// An unreadable or YAML record keeps a running gateway: it was started under
/// a record that read, and that record's intent is the last one clauth knows.
#[cfg(unix)]
#[test]
fn an_unreadable_record_keeps_a_healthy_gateway_running() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let path = crate::gateway::record_path().expect("record path");
    fs::write(&path, "config = \"/etc/shunt/shunt.yaml\"\n").expect("hand-edit");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::Healthy)
        },
        "an unreadable record keeps the running gateway"
    );
    assert!(stub::alive(pid), "never stopped for a bad record");
}

/// The production `run` loop, on a real thread, probes a `starting` gateway at
/// most once per [`SUPERVISE_POLL`]: a stub that never answers is re-probed on
/// the 1 s cadence, never in a spin through its startup grace. Counts probes
/// over a fixed window, a bound the round's pacing meets and a spin breaks by
/// orders of magnitude.
#[cfg(unix)]
#[test]
fn the_run_loop_probes_a_starting_gateway_at_most_once_a_poll() {
    let rig = Rig::new("0.49.1");
    rig.touch("no-health");
    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    // `start` runs the production loop; the handle's `Drop` sends the
    // shutdown and joins the thread on every path, a red one included, so no
    // supervisor outlives this test's sandbox.
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");

    // The child is spawned and the loop is probing it; count probes over a
    // fixed window from here.
    stub::wait_until("the stub records its run", secs(10), || {
        !stub::invocations(&rig.dir).is_empty()
    });
    probe_seam::reset_probes();
    std::thread::sleep(Duration::from_secs(5));
    let probes = probe_seam::probes();
    assert!(thread.shutdown(DAEMON_STOP_BUDGET), "answered and joined");

    // The counted window is the 5 s sleep, overrun only by the sleep's own
    // wake-up lag. One probe per SUPERVISE_POLL fits at most six in it (one
    // at each end); `< 12` leaves that much again for probe helper threads
    // started late on a loaded box, while a spin counts thousands.
    assert!(
        probes < 12,
        "a starting gateway is probed once a poll, not a spin: {probes} probes in 5 s"
    );
}

/// The skipped-lines memo is keyed on the env file's path and cleared by a
/// spawn that skips nothing: an R14 swap to another env file whose skipped
/// numbers equal the last logged set names the new path, and a fixed file
/// that later skips the same lines logs them again.
#[cfg(unix)]
#[test]
fn a_skipped_lines_memo_names_the_env_file_and_clears_on_a_clean_spawn() {
    let skipped = "GATEWAY_TEST_SECRET=from-env-file\nexport SKIPPED=1\nNO_EQUALS\n";
    let rig = Rig::new("0.49.1");
    fs::write(&rig.env_file, skipped).expect("env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.only_call();
    let first_line = format!(
        "clauth daemon: the gateway's env file {} assigns nothing on line(s) 2, 3 (systemd skips such lines too); the gateway runs without them",
        rig.env_file.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == first_line)
            .count(),
        1,
        "the skipped lines are named once: {:?}",
        rig.lines.snapshot()
    );

    // R14: another env file with the same skipped numbers logs again, naming
    // the new path.
    let other = rig.home.home().join("other.env");
    fs::write(&other, skipped).expect("other env file");
    rig.save(|record| record.env_file = Some(other.clone()));
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(3)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned on the new env file: {calls:?}");
    let second_line = format!(
        "clauth daemon: the gateway's env file {} assigns nothing on line(s) 2, 3 (systemd skips such lines too); the gateway runs without them",
        other.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == second_line)
            .count(),
        1,
        "the new path's skip is logged once: {:?}",
        rig.lines.snapshot()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == first_line)
            .count(),
        1,
        "the first path's skip stays logged once"
    );

    // A clean spawn (nothing skipped) clears the memo, so a later skip of the
    // same lines logs again.
    fs::write(&other, "GATEWAY_TEST_SECRET=from-env-file\n").expect("clean env file");
    stub::signal(calls[1].pid, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(4)),
        GatewayState::Restarting,
    );
    rig.server.wait_serving(false);
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 3, "the clean file's spawn");

    fs::write(&other, skipped).expect("re-skip");
    stub::signal(rig.calls()[2].pid, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(6)),
        GatewayState::Restarting,
    );
    rig.server.wait_serving(false);
    supervisor.step(t0.after(secs(8)));
    assert_eq!(rig.calls().len(), 4, "the re-skip's spawn");
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == second_line)
            .count(),
        2,
        "a clean spawn cleared the memo, so the same skip logs again: {:?}",
        rig.lines.snapshot()
    );

    // A spawn with no env file clears the memo too: the same file back, still
    // skipping the same lines, logs them again.
    rig.save(|record| record.env_file = None);
    supervisor.step(t0.after(secs(9)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(10)),
        GatewayState::Starting,
    );
    assert_eq!(rig.calls().len(), 5, "the no-env-file spawn");
    rig.save(|record| record.env_file = Some(other.clone()));
    supervisor.step(t0.after(secs(11)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(12)),
        GatewayState::Starting,
    );
    assert_eq!(rig.calls().len(), 6, "the same env file's spawn");
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == second_line)
            .count(),
        3,
        "a spawn with no env file cleared the memo, so the same skip logs again: {:?}",
        rig.lines.snapshot()
    );
}

/// The stub teardown signals only a pid still naming its own stub: a recorded
/// pid that names another process of the test's own making is never signalled.
#[cfg(unix)]
#[test]
fn the_stub_teardown_never_signals_a_pid_that_no_longer_names_the_stub() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let stub_pid = rig.only_call().pid;
    assert!(
        stub::names_stub(stub_pid, &rig.binary),
        "a live stub's command line names it"
    );

    // A process of this test's own making that is not the stub: a sleep.
    let mut sleeper = Owned(
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn the sleep"),
    );
    let sleeper_pid = sleeper.0.id();
    assert!(
        !stub::names_stub(sleeper_pid, &rig.binary),
        "`sleep` never names the stub"
    );
    // One whose command line holds the stub's path inside another word: the
    // stub is an argv word, never a substring of the line.
    let lookalike_arg0 = format!("{}.old", rig.binary.display());
    let mut lookalike = Owned(
        Command::new("sleep")
            .arg0(&lookalike_arg0)
            .arg("30")
            .spawn()
            .expect("spawn the lookalike"),
    );
    let lookalike_pid = lookalike.0.id();
    assert!(
        !stub::names_stub(lookalike_pid, &rig.binary),
        "`{lookalike_arg0} 30` never names the stub"
    );
    stub::kill_best_effort(sleeper_pid, &rig.binary);
    stub::kill_best_effort(lookalike_pid, &rig.binary);
    // Give a kill that would land time to land; the foreign pids must survive.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut exited = Vec::new();
    while std::time::Instant::now() < deadline && exited.is_empty() {
        for (name, owned) in [("sleep", &mut sleeper), ("lookalike", &mut lookalike)] {
            if owned.0.try_wait().expect("try_wait").is_some() {
                exited.push(name);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        exited,
        Vec::<&str>::new(),
        "a pid never naming the stub as a word is not signalled"
    );

    // A pid still naming the stub is signalled: the kill closes the stub's
    // pipe, so the health server falls silent (a `kill -s 0` probe would
    // still answer for the zombie, so liveness is not the pin).
    stub::kill_best_effort(stub_pid, &rig.binary);
    rig.server.wait_serving(false);
}

/// A shutdown whose thread never answers within the budget is logged once, by
/// equality: the seam holds the supervisor thread, so `shutdown` misses its
/// budget and names it.
#[cfg(unix)]
#[test]
fn a_shutdown_that_misses_its_budget_is_logged() {
    let rig = Rig::new("0.49.1");
    rig.save(|record| record.disabled = true);
    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes the disabled slot", secs(10), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Disabled)
    });

    let seam = probe_seam::arm();
    assert!(
        !thread.shutdown(DAEMON_STOP_BUDGET),
        "a held supervisor thread misses its budget"
    );
    seam.release();
    let line = format!(
        "clauth daemon: the shunt gateway did not stop within the {} s signal budget; the next daemon start finishes the stop",
        DAEMON_STOP_BUDGET.as_secs()
    );
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "the missed budget is logged once: {:?}",
        rig.lines.snapshot()
    );
}

/// A supervisor thread that ends in a panic is logged as a panic, never as a
/// missed budget: the seam panics the supervisor inside its shutdown.
#[cfg(unix)]
#[test]
fn a_supervisor_that_panics_is_logged_as_a_panic() {
    let rig = Rig::new("0.49.1");
    rig.save(|record| record.disabled = true);
    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes the disabled slot", secs(10), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Disabled)
    });

    let _panic = probe_seam::arm_panic();
    assert!(
        !thread.shutdown(DAEMON_STOP_BUDGET),
        "a panicked supervisor thread answers false"
    );
    assert_eq!(
        rig.lines.snapshot(),
        [
            "clauth daemon: the shunt gateway supervisor panicked; the next daemon start finishes the stop"
        ],
        "the panic is logged as a panic, and nothing else is"
    );
}

// ── the shipped gateway's baseline behaviour (adopted from the reviewer's
// probes; each pinned against `7e60b8e9`) ─────────────────────────────────────

/// A below-floor answer keeps the version read on the `stopping` slot, the
/// way baseline did: `running.version` is set before `begin_stop`.
#[cfg(unix)]
#[test]
fn a_below_floor_stopping_slot_carries_the_version_read() {
    let rig = Rig::new("0.47.0");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let _ = rig.only_call();
    rig.server.wait_serving(true);
    let mut seen = None;
    stub::wait_until("a stopping or below-floor slot", secs(10), || {
        supervisor.step(t0.after(secs(1)));
        let slot = rig.slot();
        if slot.state == GatewayState::Stopping && seen.is_none() {
            seen = Some(slot.clone());
        }
        slot.state == GatewayState::Stopping || slot.state == GatewayState::BelowFloor
    });
    let stopping = seen.expect("the stop publishes a stopping slot first");
    assert_eq!(
        stopping.version.as_deref(),
        Some("0.47.0"),
        "stopping slot: {stopping:?}"
    );
}

/// A spawn error's `reason` names the binary path, the bad input, as baseline
/// did.
#[cfg(unix)]
#[test]
fn a_spawn_error_reason_names_the_binary() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = Rig::new("0.49.1");
    fs::set_permissions(&rig.binary, fs::Permissions::from_mode(0o644)).expect("chmod");
    let mut supervisor = rig.supervisor();
    supervisor.step(t0());
    let slot = rig.slot();
    assert_eq!(slot.state, GatewayState::Misconfigured, "{slot:?}");
    assert_eq!(
        slot.reason,
        Some(format!(
            "cannot run {}: Permission denied (os error 13)",
            rig.binary.display()
        )),
        "{slot:?}"
    );
}

/// A misconfigured gateway logs its reason once per distinct reason, not once
/// per retry round.
#[cfg(unix)]
#[test]
fn a_misconfigured_gateway_logs_its_reason_once() {
    let rig = Rig::new("0.49.1");
    fs::remove_file(&rig.env_file).expect("remove the env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    assert_eq!(rig.slot().state, GatewayState::Misconfigured);
    supervisor.step(t0.after(secs(5)));
    supervisor.step(t0.after(secs(10)));
    let lines: Vec<String> = rig
        .lines
        .snapshot()
        .into_iter()
        .filter(|line| line.starts_with("clauth daemon: cannot start the shunt gateway: "))
        .collect();
    assert_eq!(lines.len(), 1, "one line per distinct reason: {lines:?}");
}

/// A missing binary's line names the binary path and the fix, as baseline did.
#[cfg(unix)]
#[test]
fn a_missing_binary_line_names_the_binary_and_the_fix() {
    let rig = Rig::new("0.49.1");
    let missing = rig.home.home().join("bin").join("shunt");
    rig.save(|record| record.binary = Some(missing.clone()));
    let mut supervisor = rig.supervisor();
    supervisor.step(t0());
    let expected = format!(
        "clauth daemon: cannot start the shunt gateway: {} not found; install shunt or point the gateway at its binary",
        missing.display()
    );
    assert!(
        rig.lines.snapshot().contains(&expected),
        "lines: {:?}",
        rig.lines.snapshot()
    );
}

// ── the hold ────────────────────────────────────────────────────────────────

/// This test process's own daemon identity: the pid the supervisor's
/// `for_daemon` learns, and its start token.
#[cfg(unix)]
fn this_daemon() -> crate::gateway::DaemonIdentity {
    let pid = std::process::id();
    crate::gateway::DaemonIdentity {
        pid,
        start: process_start_time(pid),
    }
}

/// Write a hold naming `identity`, the shape `write_hold` writes.
#[cfg(unix)]
fn write_hold_naming(identity: &crate::gateway::DaemonIdentity) {
    let path = crate::gateway::hold_path().expect("hold path");
    fs::write(&path, serde_json::to_vec(identity).expect("json")).expect("hold");
}

/// A hold naming this daemon holds the gateway off: the slot reads `held` and
/// nothing spawns.
#[cfg(unix)]
#[test]
fn a_hold_naming_this_daemon_holds_the_gateway_off() {
    let rig = Rig::new("0.49.1");
    write_hold_naming(&this_daemon());
    let mut supervisor = rig.supervisor();
    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            config: shown(&rig.config),
            binary: shown(&rig.binary),
            since: Some(AT_T0.to_string()),
            ..blank(GatewayState::Held)
        }
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawns while held");
}

/// A hold naming this daemon appearing mid-run stops the running gateway with
/// the normal stop and lands on `held`.
#[cfg(unix)]
#[test]
fn a_hold_stops_the_running_gateway() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    write_hold_naming(&this_daemon());
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Stopping)
        }
    );
    step_until(&rig, &mut supervisor, t0.after(secs(2)), GatewayState::Held);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(15),
            }),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Held)
        }
    );
    assert_eq!(stub::terms(&rig.dir), [pid], "one SIGTERM, the normal stop");
}

/// A hold naming another pid, and one naming this pid with another start
/// token, are each removed with one log line and the gateway runs.
#[cfg(unix)]
#[test]
fn a_hold_naming_another_instance_is_removed_and_ignored() {
    let me = this_daemon();
    for (label, other) in [
        (
            "another pid",
            crate::gateway::DaemonIdentity {
                pid: me.pid.wrapping_add(1),
                start: me.start.clone(),
            },
        ),
        (
            "this pid, another start token",
            crate::gateway::DaemonIdentity {
                pid: me.pid,
                start: Some("another-start-token".to_string()),
            },
        ),
    ] {
        let rig = Rig::new("0.49.1");
        write_hold_naming(&other);
        let path = crate::gateway::hold_path().expect("hold path");
        let mut supervisor = rig.supervisor();
        supervisor.step(t0());
        assert!(!path.exists(), "{label}: the hold is removed");
        assert_eq!(rig.calls().len(), 1, "{label}: the gateway runs");
        let line = format!(
            "clauth daemon: ignoring {}: it names another daemon instance or does not parse; removed",
            path.display()
        );
        assert_eq!(
            rig.lines.snapshot().iter().filter(|l| **l == line).count(),
            1,
            "{label}: one log line"
        );
    }
}

/// A hold that does not parse is removed with one log line and the gateway
/// runs.
#[cfg(unix)]
#[test]
fn an_unparseable_hold_is_removed_and_ignored() {
    let rig = Rig::new("0.49.1");
    let path = crate::gateway::hold_path().expect("hold path");
    fs::write(&path, b"not json").expect("hold");
    let mut supervisor = rig.supervisor();
    supervisor.step(t0());
    assert!(!path.exists(), "an unparseable hold is removed");
    assert_eq!(rig.calls().len(), 1, "and the gateway runs");
    let line = format!(
        "clauth daemon: ignoring {}: it names another daemon instance or does not parse; removed",
        path.display()
    );
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "one log line"
    );
}

/// Writing and releasing a hold never writes the record's `disabled`: the
/// record's bytes are identical before and after.
#[cfg(unix)]
#[test]
fn writing_and_releasing_a_hold_leaves_the_record_unchanged() {
    let _rig = Rig::new("0.49.1");
    let record = crate::gateway::record_path().expect("record path");
    let before = fs::read(&record).expect("the record");
    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(_singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    crate::gateway::write_hold().expect("write the hold");
    assert!(
        crate::gateway::hold_path().expect("hold path").exists(),
        "the hold is written"
    );
    crate::gateway::remove_hold().expect("release the hold");
    assert!(
        !crate::gateway::hold_path().expect("hold path").exists(),
        "the hold is gone"
    );
    let after = fs::read(&record).expect("the record");
    assert_eq!(
        before, after,
        "write-hold and release leave the record byte-identical"
    );
}

/// `write_hold` errors with no daemon holding the singleton, and the file it
/// writes is owner-only.
#[cfg(unix)]
#[test]
fn write_hold_errors_without_a_daemon_and_writes_0600() {
    let _rig = Rig::new("0.49.1");
    let err = crate::gateway::write_hold().expect_err("no daemon to name");
    assert_eq!(
        err.to_string(),
        "no daemon holds the clauth singleton",
        "names what was missing"
    );

    let dir = clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(_singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    crate::gateway::write_hold().expect("write the hold");
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(crate::gateway::hold_path().expect("hold path"))
        .expect("hold")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the hold is owner-only");
}

// ── the hold, cross-platform (no unix rig needed) ──────────────────────────

/// A child that stays alive until its stdin closes: `cat` on unix, `cmd /D`
/// on windows.
fn parked_child() -> std::process::Child {
    #[cfg(not(windows))]
    let (program, args): (&str, &[&str]) = ("cat", &[]);
    #[cfg(windows)]
    let (program, args): (&str, &[&str]) = ("cmd", &["/D"]);
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn a parked child")
}

/// `write_hold` without a daemon names what was missing, pinned by equality,
/// on every platform (needs no unix rig).
#[test]
fn write_hold_errors_without_a_daemon() {
    let _home = HomeSandbox::new();
    let err = crate::gateway::write_hold().expect_err("no daemon to name");
    assert_eq!(err.to_string(), "no daemon holds the clauth singleton");
}

/// A stale sidecar naming a live pid with no daemon holding the singleton is
/// the missing-daemon error and writes nothing: the pid is never read past a
/// true `singleton_held`.
#[test]
fn write_hold_refuses_a_stale_sidecar_naming_a_live_pid() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("clauthd.pid"), format!("{}\n", std::process::id())).expect("sidecar");
    let err = crate::gateway::write_hold().expect_err("no daemon to name");
    assert_eq!(err.to_string(), "no daemon holds the clauth singleton");
    assert!(
        !crate::gateway::hold_path().expect("hold path").exists(),
        "no hold file is written"
    );
}

/// What `write_hold` writes is what the daemon honours: a hold written by
/// this process reads back as this daemon's own and stays.
#[test]
fn a_written_hold_reads_back_as_this_daemon_and_stays() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let super::super::probe::Claim::Active(_singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    crate::gateway::write_hold().expect("write the hold");
    let me = Gateway::for_daemon();
    assert!(
        matches!(me.hold(), Hold::Mine),
        "the daemon reads the hold as its own"
    );
    assert!(
        crate::gateway::hold_path().expect("hold path").exists(),
        "the hold stays"
    );
}

/// A daemon holds the singleton but its pid sidecar is torn (the daemon's own
/// stamp can fail silently): the error names a running daemon, never a missing
/// one, and no hold is written.
#[test]
fn write_hold_names_a_running_daemon_whose_pid_is_unreadable() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let super::super::probe::Claim::Active(_singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    fs::write(dir.join(super::super::PID_FILE), b"").expect("tear the sidecar");
    let err = crate::gateway::write_hold().expect_err("a torn sidecar names no pid");
    assert_eq!(
        err.to_string(),
        "a clauth daemon is running but its pid is unreadable"
    );
    assert!(
        !crate::gateway::hold_path().expect("hold path").exists(),
        "no hold is written"
    );
}

/// The written hold names the pid the sidecar stamps, not this process's own
/// pid: overwrite the sidecar with a parked child's pid and read the JSON.
#[test]
fn a_written_hold_names_the_pid_the_sidecar_stamps() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let super::super::probe::Claim::Active(_singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let mut child = parked_child();
    fs::write(dir.join("clauthd.pid"), format!("{}\n", child.id())).expect("sidecar");
    crate::gateway::write_hold().expect("write the hold");
    let bytes = fs::read(crate::gateway::hold_path().expect("hold path")).expect("the hold");
    let hold: crate::gateway::DaemonIdentity = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        hold.pid,
        child.id(),
        "the written hold names the sidecar pid"
    );
    assert_eq!(
        hold.start.as_deref(),
        process_start_time(child.id()).as_deref(),
        "the written hold names that pid's start token"
    );
    drop(child.stdin.take());
    child.wait().expect("reap");
}

/// A daemon whose own start token is unreadable never honours or removes a
/// hold, and logs that cause once, never blaming another daemon.
#[test]
fn a_daemon_whose_own_start_token_is_unreadable_leaves_the_hold() {
    let _home = HomeSandbox::new();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let hold_path = crate::gateway::hold_path().expect("hold path");
    fs::write(
        &hold_path,
        serde_json::to_vec(&crate::gateway::DaemonIdentity {
            pid: std::process::id(),
            start: Some("any-token".to_string()),
        })
        .expect("json"),
    )
    .expect("hold");
    let me = Gateway::for_test(Some(crate::gateway::DaemonIdentity {
        pid: std::process::id(),
        start: None,
    }));
    assert!(matches!(me.hold(), Hold::None), "never honours the hold");
    assert!(matches!(me.hold(), Hold::None), "still never honours it");
    assert!(hold_path.exists(), "never removes the hold");
    let expected = format!(
        "clauth daemon: cannot read this daemon's own start time; leaving the gateway hold {} in place",
        hold_path.display()
    );
    assert_eq!(
        lines.snapshot().iter().filter(|l| **l == expected).count(),
        1,
        "one log line naming the true cause: {:?}",
        lines.snapshot()
    );
    assert!(
        !lines
            .snapshot()
            .iter()
            .any(|l| l.contains("another daemon instance")),
        "never blames another daemon"
    );
}

/// Releasing a hold that is not there is fine, so a `start shunt` after a
/// daemon restart (which already removed the hold) still succeeds.
#[test]
fn remove_hold_with_no_hold_is_fine() {
    let _home = HomeSandbox::new();
    crate::gateway::remove_hold().expect("absent is fine");
}

/// A hold that cannot be read logs once per change, not once per round.
#[test]
fn a_hold_read_error_is_logged_once_per_change() {
    let _home = HomeSandbox::new();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let hold_path = crate::gateway::hold_path().expect("hold path");
    fs::create_dir_all(&hold_path).expect("a directory at the hold path");
    let me = Gateway::for_test(Some(crate::gateway::DaemonIdentity {
        pid: std::process::id(),
        start: Some("any-token".to_string()),
    }));
    assert!(matches!(me.hold(), Hold::None));
    assert!(matches!(me.hold(), Hold::None));
    assert_eq!(
        lines
            .snapshot()
            .iter()
            .filter(|l| l.contains("cannot read") && l.contains(&hold_path.display().to_string()))
            .count(),
        1,
        "one read-error line across two rounds: {:?}",
        lines.snapshot()
    );
}

/// A disabled record still reads `disabled` even with a hold naming this
/// daemon: the record's flag wins over the hold.
#[test]
fn disabled_wins_over_a_hold_naming_this_daemon() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let super::super::probe::Claim::Active(_singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let config = dir.join("shunt.toml");
    fs::write(&config, "[server]\nbind = \"127.0.0.1:1\"\n").expect("config");
    let mut record = GatewayRecord::new(config).expect("adoptable");
    record.disabled = true;
    GatewayRecord::update(|slot| {
        *slot = Some(record);
        Ok(())
    })
    .expect("save");
    crate::gateway::write_hold().expect("write the hold");
    let Intent::Idle(slot) = Gateway::for_daemon().intent() else {
        panic!("a disabled record is idle");
    };
    assert_eq!(slot.state, GatewayState::Disabled, "{slot:?}");
}

/// A left-behind gateway serves only while the marker's pid is a live process
/// started when the marker says and no stop was asked of it: no marker, a
/// recycled pid (another start time) and a stopping child all read as none.
#[test]
fn a_left_behind_gateway_runs_only_while_live_and_unstopped() {
    let _home = HomeSandbox::new();
    assert!(!left_behind_gateway_runs(), "no marker");

    let pid = std::process::id();
    let start = process_start_time(pid).expect("this process has a start time");
    let marker = |start: String, stop_deadline_ms: Option<u64>| ChildMarker {
        pid,
        start: Some(start),
        stop_bound_secs: 40,
        stop_deadline_ms,
    };

    write_marker(&marker(start.clone(), None)).expect("marker");
    assert!(left_behind_gateway_runs(), "a live, unstopped child serves");

    write_marker(&marker(start.clone(), Some(1))).expect("marker");
    assert!(
        !left_behind_gateway_runs(),
        "a child asked to stop is on its way out"
    );

    write_marker(&marker(format!("{start}-not"), None)).expect("marker");
    assert!(
        !left_behind_gateway_runs(),
        "a pid with another start time is not the child"
    );
}
