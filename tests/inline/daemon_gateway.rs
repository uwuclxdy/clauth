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
use crate::gateway::GatewayRecord;
use crate::testutil::HomeSandbox;

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

const ALL_STATES: [GatewayState; 14] = [
    GatewayState::Absent,
    GatewayState::Disabled,
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
                "invalid gateway record {}: the adopted shunt config must be an absolute path, got shunt.toml",
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
