#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
//! The per-proxy supervisor over the generic machine: one stub proxy runs
//! healthy and restarts alone, a disabled or missing-binary row runs nothing,
//! a foreign answerer and a contract-major mismatch are refused by name, and a
//! daemon stop sends exactly one SIGTERM with the orphan finished by the next
//! daemon at its recorded deadline. Every test drives a stub
//! `clauth-<service>-proxy` under a `HomeSandbox`.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

use super::*;
#[cfg(unix)]
use crate::daemon::gateway::{
    DAEMON_STOP_BUDGET, Supervised as _, Supervisor, Tick, start_kind, stop_sleep,
};
use crate::testutil::HomeSandbox;

use crate::daemon::gateway::tests::stub;

#[cfg(unix)]
const T0_WALL_MS: u64 = 1_790_000_000_000;

#[cfg(unix)]
fn t0() -> Tick {
    Tick {
        at: std::time::Instant::now(),
        wall_ms: T0_WALL_MS,
    }
}

#[cfg(unix)]
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A sandbox holding one enabled stub proxy: its registry row, admin token,
/// state dir, stub binary and `/health` answerer.
#[cfg(unix)]
struct Rig {
    server: stub::HealthServer,
    slots: ProxySlots,
    lines: crate::logline::LogLines,
    _capture: crate::logline::LogCapture,
    dir: PathBuf,
    binary: PathBuf,
    service: Service,
    port: u16,
    _home: HomeSandbox,
}

#[cfg(unix)]
impl Rig {
    fn new(service: &str) -> Self {
        Self::with_health(service, "1.2.0", "1.0")
    }

    fn with_health(service: &str, version: &str, contract: &str) -> Self {
        let home = HomeSandbox::new();
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();
        let dir = home.home().join("stub");
        let binary = stub::write_proxy_stub(&dir, service);
        let port = stub::free_port();
        let server = stub::HealthServer::start_proxy(&dir, port, service, version, contract);
        let bind =
            crate::proxy::enable(service, Some(port), Some(dir.as_os_str())).expect("enable");
        assert_eq!(bind, crate::proxy::bind(port));
        let service = Service::parse(service).expect("a service");
        Rig {
            server,
            slots: new_slots(),
            lines,
            _capture: capture,
            dir,
            binary,
            service,
            port,
            _home: home,
        }
    }

    fn supervisor(&self) -> Supervised {
        Supervised(Supervisor::for_proxy(
            Proxy {
                service: self.service.clone(),
            },
            Arc::clone(&self.slots),
        ))
    }

    fn slot(&self) -> ProxySlot {
        published(&self.slots, &self.service).expect("the supervisor publishes a slot")
    }

    /// Every run the stub recorded, once the record lists each spawn the
    /// supervisor logged on this thread.
    fn calls(&self) -> Vec<stub::Invocation> {
        let spawned = self
            .lines
            .snapshot()
            .iter()
            .filter(|line| {
                line.starts_with(&format!("clauth daemon: started proxy {}", self.service))
            })
            .count();
        let mut calls = Vec::new();
        stub::wait_until("the stub records the spawn", secs(10), || {
            calls = stub::invocations(&self.dir);
            calls.len() >= spawned
        });
        calls
    }

    fn only_call(&self) -> stub::Invocation {
        let calls = self.calls();
        assert_eq!(calls.len(), 1, "exactly one run: {calls:?}");
        calls.into_iter().next().expect("one run")
    }
}

/// A supervisor whose proxy is killed and reaped when the test ends.
#[cfg(unix)]
struct Supervised(Supervisor<Proxy>);

#[cfg(unix)]
impl std::ops::Deref for Supervised {
    type Target = Supervisor<Proxy>;
    fn deref(&self) -> &Supervisor<Proxy> {
        &self.0
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for Supervised {
    fn deref_mut(&mut self) -> &mut Supervisor<Proxy> {
        &mut self.0
    }
}

#[cfg(unix)]
impl Drop for Supervised {
    fn drop(&mut self) {
        self.0.kill_for_test();
    }
}

#[cfg(unix)]
fn step_until(rig: &Rig, supervisor: &mut Supervised, tick: Tick, state: ProxyState) {
    stub::wait_until(&format!("the slot reads {state:?}"), secs(10), || {
        supervisor.step(tick);
        rig.slot().state == state
    });
}

#[cfg(unix)]
#[test]
fn two_proxies_run_side_by_side_and_each_restarts_alone() {
    let home = HomeSandbox::new();
    let dir_a = home.home().join("stub-a");
    let dir_b = home.home().join("stub-b");
    stub::write_proxy_stub(&dir_a, "alpha");
    stub::write_proxy_stub(&dir_b, "beta");
    let port_a = stub::free_port();
    let port_b = stub::free_port();
    let server_a = stub::HealthServer::start_proxy(&dir_a, port_a, "alpha", "1.2.0", "1.0");
    let server_b = stub::HealthServer::start_proxy(&dir_b, port_b, "beta", "1.2.0", "1.0");
    crate::proxy::enable("alpha", Some(port_a), Some(dir_a.as_os_str())).expect("enable alpha");
    crate::proxy::enable("beta", Some(port_b), Some(dir_b.as_os_str())).expect("enable beta");
    let service_a = Service::parse("alpha").expect("a service");
    let service_b = Service::parse("beta").expect("a service");
    let slots = new_slots();
    let mut sa = Supervised(Supervisor::for_proxy(
        Proxy {
            service: service_a.clone(),
        },
        Arc::clone(&slots),
    ));
    let mut sb = Supervised(Supervisor::for_proxy(
        Proxy {
            service: service_b.clone(),
        },
        Arc::clone(&slots),
    ));
    let t0 = t0();
    sa.step(t0);
    sb.step(t0);
    server_a.wait_serving(true);
    server_b.wait_serving(true);
    sa.step(t0.after(secs(1)));
    sb.step(t0.after(secs(1)));
    assert_eq!(
        published(&slots, &service_a).expect("alpha").state,
        ProxyState::Healthy
    );
    assert_eq!(
        published(&slots, &service_b).expect("beta").state,
        ProxyState::Healthy
    );
    let a_pid = invocations(&dir_a)[0].pid;
    let b_pid = invocations(&dir_b)[0].pid;
    assert_ne!(a_pid, b_pid, "two side-by-side proxies");

    // Kill alpha alone: it restarts, beta stays on its first run.
    stub::signal(a_pid, "KILL");
    step_until_proxy(
        &slots,
        &service_a,
        &mut sa,
        t0.after(secs(1)),
        ProxyState::Restarting,
    );
    assert_eq!(published(&slots, &service_a).expect("alpha").restarts, 1);
    assert_eq!(invocations(&dir_b).len(), 1, "beta is never restarted");
    server_a.wait_serving(false);
    sa.step(t0.after(secs(2)));
    stub::wait_until("alpha respawns", secs(10), || {
        invocations(&dir_a).len() >= 2
    });
    assert_eq!(invocations(&dir_b).len(), 1, "beta still untouched");
}

#[cfg(unix)]
fn invocations(dir: &std::path::Path) -> Vec<stub::Invocation> {
    stub::invocations(dir)
}

#[cfg(unix)]
fn step_until_proxy(
    slots: &ProxySlots,
    service: &Service,
    supervisor: &mut Supervised,
    tick: Tick,
    state: ProxyState,
) {
    stub::wait_until(&format!("{service} reads {state:?}"), secs(10), || {
        supervisor.step(tick);
        published(slots, service).is_some_and(|slot| slot.state == state)
    });
}

#[cfg(unix)]
#[test]
fn a_disabled_row_and_a_missing_binary_run_nothing_and_say_which() {
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    let t0 = t0();
    // Disabled: no spawn, the entry says `disabled`.
    crate::proxy::disable("zcode").expect("disable");
    s.step(t0);
    assert_eq!(rig.slot().state, ProxyState::Disabled);
    assert_eq!(rig.calls().len(), 0, "a disabled proxy runs nothing");

    // Re-enable, then remove the binary: `binary_missing`, still no spawn.
    crate::proxy::enable("zcode", Some(rig.port), Some(rig.dir.as_os_str())).expect("re-enable");
    std::fs::remove_file(&rig.binary).expect("remove the binary");
    s.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, ProxyState::BinaryMissing);
    assert_eq!(rig.calls().len(), 0, "a missing binary runs nothing");
}

#[cfg(unix)]
#[test]
fn a_foreign_listener_on_the_port_is_refused_naming_it() {
    let rig = Rig::new("zcode");
    let _foreign = stub::HttpAnswer::bind(
        rig.port,
        200,
        r#"{"status":"ok","service":"qwen","version":"0.9.1","contract":"1.0"}"#,
    );
    let mut s = rig.supervisor();
    s.step(t0());
    assert_eq!(rig.slot().state, ProxyState::Foreign);
    assert_eq!(
        rig.slot().answerer,
        Some(ProxyAnswerer::Proxy {
            service: Some("qwen".to_string())
        }),
        "the entry names the answering proxy's service"
    );
    assert_eq!(rig.slot().contract.as_deref(), Some("1.0"));
    assert_eq!(rig.calls().len(), 0, "nothing spawned beside it");
}

#[cfg(unix)]
#[test]
fn a_contract_major_mismatch_is_refused_naming_both() {
    let rig = Rig::with_health("zcode", "1.2.0", "2.0");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.server.wait_serving(true);
    step_until(
        &rig,
        &mut s,
        t0.after(secs(1)),
        ProxyState::ContractMismatch,
    );
    let pid = rig.only_call().pid;
    assert_eq!(rig.slot().contract.as_deref(), Some("2.0"));
    assert_eq!(
        rig.slot().reason.as_deref(),
        Some("it declared contract major 2 (\"2.0\"), and clauth speaks major 1"),
        "the entry names both majors"
    );
    // The two refusal lines, pinned by equality with the pid fixed.
    let lines = rig.lines.snapshot();
    assert!(
        lines.contains(&format!(
            "clauth daemon: proxy zcode speaks contract major 2 (\"2.0\"), and clauth speaks major 1; stopping it (pid {pid})"
        )),
        "the refusal names both majors: {lines:?}"
    );
    assert!(
        lines.contains(&format!(
            "clauth daemon: stopping proxy zcode (pid {pid}): it declared contract major 2 (\"2.0\"), and clauth speaks major 1"
        )),
        "the stop names both majors: {lines:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_health_without_a_service_is_refused() {
    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    let binary = stub::write_proxy_stub(&dir, "zcode");
    let port = stub::free_port();
    let server = stub::HealthServer::start_proxy_without_service(&dir, port, "1.2.0", "1.0");
    crate::proxy::enable("zcode", Some(port), Some(dir.as_os_str())).expect("enable");
    let service = Service::parse("zcode").expect("a service");
    let slots = new_slots();
    let mut s = Supervised(Supervisor::for_proxy(
        Proxy {
            service: service.clone(),
        },
        Arc::clone(&slots),
    ));
    let t0 = t0();
    s.step(t0);
    server.wait_serving(true);
    stub::wait_until("the proxy is refused", secs(10), || {
        s.step(t0.after(secs(1)));
        published(&slots, &service).is_some_and(|slot| slot.state == ProxyState::ContractMismatch)
    });
    assert_eq!(
        published(&slots, &service).expect("a slot").state,
        ProxyState::ContractMismatch,
        "a /health lacking the service is refused"
    );
    assert_eq!(
        published(&slots, &service)
            .expect("a slot")
            .reason
            .as_deref(),
        Some("its /health names no service"),
        "the entry names the missing field"
    );
    let _ = binary;
}

#[cfg(unix)]
#[test]
fn a_daemon_stop_sends_each_proxy_exactly_one_sigterm() {
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.server.wait_serving(true);
    s.step(t0.after(secs(1)));
    let pid = rig.only_call().pid;
    assert_eq!(rig.slot().state, ProxyState::Healthy);

    // The stop's exit line and marker removal are unasserted here.
    let _ = s.shutdown(std::time::Instant::now() + secs(4));
    rig.server.wait_serving(false);
    assert_eq!(stub::terms(&rig.dir), [pid], "exactly one SIGTERM");
}

/// A refused per-proxy stop spawn runs the stop inline on the coordinator's
/// stop path, never a panic: the seam refuses the spawn (the one real spawn
/// failure the box cannot pose), the failure counter proves the degraded
/// path ran, and the inline stop still sends the proxy its one SIGTERM.
#[cfg(unix)]
#[test]
fn a_refused_per_proxy_stop_spawn_stops_the_proxy_inline() {
    let rig = Rig::new("zcode");
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    manage_round(&mut coordinator, &rig.slots, crate::usage::now_ms());
    stub::wait_until("the stub records its spawn", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = stub::invocations(&rig.dir)[0].pid;

    let _refused = crate::daemon::gateway::probe_seam::fail_stop_spawns();
    shutdown_threads(coordinator.threads, coordinator.reaping);
    assert_eq!(
        crate::daemon::gateway::probe_seam::stop_spawn_failures(),
        1,
        "the one per-proxy stop spawn failed through the seam, and the stop ran inline"
    );
    assert_eq!(
        stub::terms(&rig.dir),
        [pid],
        "the inline stop sent the proxy its one SIGTERM"
    );
    assert!(!stub::alive(pid), "the proxy stopped");
}

#[cfg(unix)]
#[test]
fn a_proxy_still_draining_is_finished_by_the_next_daemon_at_its_deadline() {
    let rig = Rig::new("zcode");
    // Drain 30 s, past the 4 s daemon budget, so a stop leaves it draining.
    std::fs::write(rig.dir.join("drain"), "30").expect("drain marker");
    let mut before = rig.supervisor();
    before.step(t0());
    rig.only_call();
    // The still-stopping line is unasserted; the marker read below is the
    // begin_stop deadline marker, which stays for the draining child.
    let _ = before.shutdown(std::time::Instant::now());
    stub::wait_until("the stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });
    let recorded = read_marker_at_for(&rig).expect("a marker");
    let deadline_ms = recorded.stop_deadline_ms.expect("the stop's deadline");
    let mut orphan = crate::testutil::Owned(before.abandon().expect("still running"));

    let mut after = rig.supervisor();
    after.step(Tick {
        at: std::time::Instant::now(),
        wall_ms: deadline_ms - 1,
    });
    assert_eq!(rig.slot().state, ProxyState::Stopping);
    assert_eq!(
        stub::terms(&rig.dir),
        [orphan.0.id()],
        "the exiting daemon's one SIGTERM, never a second"
    );

    after.step(Tick {
        at: std::time::Instant::now(),
        wall_ms: deadline_ms,
    });
    let mut status = None;
    stub::wait_until("the orphan exits at its deadline", secs(10), || {
        status = orphan.0.try_wait().expect("try_wait");
        status.is_some()
    });
    assert_eq!(
        crate::daemon::gateway::ExitReport::from(status.expect("exited")).signal,
        Some(9),
        "the still-draining proxy is hard-killed at its deadline"
    );
}

#[cfg(unix)]
fn read_marker_at_for(rig: &Rig) -> Option<crate::daemon::gateway::ChildMarker> {
    let path = crate::proxy::child_marker_path(&rig.service).expect("marker path");
    serde_json::from_slice(&std::fs::read(path).expect("read the marker")).ok()
}

/// The state dir clauth owns is 0700 and the supervisor's own writers land its
/// log and child marker 0600: a step spawns the stub, so the marker and log a
/// supervisor wrote are stat'd, never the helpers' output called by hand.
#[cfg(unix)]
#[test]
fn the_supervisors_log_and_marker_are_0600_and_the_dir_0700() {
    use std::os::unix::fs::PermissionsExt as _;
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    s.step(t0());
    rig.only_call();
    let dir = crate::proxy::state_dir(&rig.service).expect("state dir");
    assert_eq!(
        std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
        0o700,
        "the state dir is owner-only"
    );
    let log = crate::proxy::log_path(&rig.service).expect("log path");
    assert_eq!(
        std::fs::metadata(&log).expect("log").permissions().mode() & 0o777,
        0o600,
        "the supervisor's log is owner-only"
    );
    let marker = crate::proxy::child_marker_path(&rig.service).expect("marker path");
    assert_eq!(
        std::fs::metadata(&marker)
            .expect("marker")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "the supervisor's marker is owner-only"
    );
}

/// No `{:?}` of a slot, a row or the kind prints the proxy's admin token
/// bytes: the token is minted through `enable`'s real path, and every type
/// that could reach it is formatted and checked against the file's bytes.
#[cfg(unix)]
#[test]
fn no_proxy_debug_prints_the_token() {
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.server.wait_serving(true);
    s.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, ProxyState::Healthy);
    let token =
        std::fs::read_to_string(crate::proxy::admin_token_path(&rig.service).expect("token path"))
            .expect("the minted token");
    assert!(!token.is_empty(), "the token is minted");

    let slot = rig.slot();
    assert!(
        !format!("{slot:?}").contains(&token),
        "a slot's Debug never prints the token"
    );
    let row = crate::proxy::Registry::load()
        .expect("registry")
        .get(&rig.service)
        .cloned()
        .expect("a row");
    assert!(
        !format!("{row:?}").contains(&token),
        "a row's Debug never prints the token"
    );
    let kind = Proxy {
        service: rig.service.clone(),
    };
    assert!(
        !format!("{kind:?}").contains(&token),
        "the kind's Debug never prints the token"
    );
}

/// A disable of a RUNNING proxy stops it with one SIGTERM, publishes
/// `disabled`, and keeps its row.
#[cfg(unix)]
#[test]
fn a_disable_stops_a_running_proxy_with_one_sigterm_and_keeps_the_row() {
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.server.wait_serving(true);
    s.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, ProxyState::Healthy);
    let pid = rig.only_call().pid;

    crate::proxy::disable("zcode").expect("disable");
    stub::wait_until("the proxy reads disabled", secs(10), || {
        s.step(t0.after(secs(2)));
        rig.slot().state == ProxyState::Disabled
    });
    rig.server.wait_serving(false);
    assert_eq!(stub::terms(&rig.dir), [pid], "exactly one SIGTERM");
    let row = crate::proxy::Registry::load()
        .expect("registry")
        .get(&rig.service)
        .cloned()
        .expect("the row stays");
    assert!(!row.enabled, "the row is disabled, not deleted");
}

/// The stop bound is the manifest's `drain_secs` plus the 5 s margin, written
/// to the child marker the next daemon reads.
#[cfg(unix)]
#[test]
fn the_stop_bound_is_the_manifest_drain_plus_the_margin() {
    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("drain"), "7").expect("drain marker");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.only_call();
    rig.server.wait_serving(true);
    s.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, ProxyState::Healthy);

    // The still-stopping line is unasserted; the marker read below is the
    // begin_stop deadline marker, which stays for the draining child.
    let _ = s.shutdown(std::time::Instant::now());
    let marker = read_marker_at_for(&rig).expect("a marker");
    assert_eq!(marker.stop_bound_secs, 12, "drain_secs 7 + the 5 s margin");
}

/// The spawn sets `serve` argv, the state dir cwd and the three
/// `CLAUTH_PROXY_*` variables, by equality.
#[cfg(unix)]
#[test]
fn the_spawn_sets_argv_env_and_cwd() {
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    s.step(t0());
    let call = rig.only_call();
    let state_dir = crate::proxy::state_dir(&rig.service).expect("state dir");
    let cwd = std::fs::canonicalize(&state_dir).expect("canonical state dir");
    let token = crate::proxy::admin_token_path(&rig.service).expect("token path");
    assert_eq!(call.args, ["serve"], "argv is `<binary> serve`");
    assert_eq!(call.cwd, cwd, "cwd is the state dir");
    assert_eq!(
        call.env,
        [
            format!("CLAUTH_PROXY_ADMIN_TOKEN_FILE={}", token.display()),
            format!("CLAUTH_PROXY_BIND=127.0.0.1:{}", rig.port),
            format!("CLAUTH_PROXY_STATE_DIR={}", state_dir.display()),
        ],
        "the three CLAUTH_PROXY_* variables, exact"
    );
}

/// A missing admin token runs nothing and its entry names the token and the
/// fix.
#[cfg(unix)]
#[test]
fn a_missing_token_runs_nothing_and_names_the_fix() {
    let rig = Rig::new("zcode");
    let token_path = crate::proxy::admin_token_path(&rig.service).expect("token path");
    std::fs::remove_file(&token_path).expect("remove the token");
    let mut s = rig.supervisor();
    s.step(t0());
    assert_eq!(rig.slot().state, ProxyState::NoToken);
    assert_eq!(
        rig.slot().reason.as_deref(),
        Some(format!(
            "the admin token of proxy \"zcode\" in {} is missing; run `clauth proxy enable zcode`",
            token_path.display()
        )
        .as_str()),
        "the reason names the token and the fix"
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawns without its token");
}

/// An unreadable registry never reads as `manifest_refused`: the supervisor
/// keeps its last good slot, and a running child keeps running untouched.
#[cfg(unix)]
#[test]
fn an_unreadable_registry_keeps_the_last_good_slot() {
    let rig = Rig::new("zcode");
    crate::proxy::disable("zcode").expect("disable");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    assert_eq!(rig.slot().state, ProxyState::Disabled);

    std::fs::write(
        crate::proxy::registry_path().expect("registry"),
        "not = [toml",
    )
    .expect("corrupt the registry");
    s.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot().state,
        ProxyState::Disabled,
        "the last good slot stays"
    );
    assert!(
        rig.slot().reason.is_none(),
        "an unreadable registry never reads as a refusal: {:?}",
        rig.slot()
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawns");
}

/// The coordinator follows the registry at runtime: an enable after start
/// runs, a disable stops with one SIGTERM and `disabled`, a removal reaps the
/// supervisor and drops its slot, and a stop of two draining proxies stays
/// inside one aggregate budget with one SIGTERM each.
#[cfg(unix)]
#[test]
fn the_coordinator_follows_the_registry_and_stops_in_one_budget() {
    let home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let slots = new_slots();
    let supervision = start(Arc::clone(&slots), &singleton).expect("the coordinator");
    let dir_a = home.home().join("stub-a");
    let dir_b = home.home().join("stub-b");
    stub::write_proxy_stub(&dir_a, "alpha");
    stub::write_proxy_stub(&dir_b, "beta");
    for dir in [&dir_a, &dir_b] {
        std::fs::write(dir.join("no-health"), "").expect("no-health");
        std::fs::write(dir.join("drain"), "30").expect("drain");
    }
    crate::proxy::enable("alpha", Some(stub::free_port()), Some(dir_a.as_os_str()))
        .expect("enable a");
    let alpha = Service::parse("alpha").expect("alpha");
    stub::wait_until("alpha runs without a restart", secs(10), || {
        stub::invocations(&dir_a).len() == 1
    });

    // Disable alpha: one SIGTERM, then `disabled`.
    std::fs::write(dir_a.join("drain"), "0").expect("drain 0");
    crate::proxy::disable("alpha").expect("disable");
    stub::wait_until("alpha reads disabled", secs(10), || {
        published(&slots, &alpha).is_some_and(|slot| slot.state == ProxyState::Disabled)
    });
    assert_eq!(stub::terms(&dir_a).len(), 1, "one SIGTERM on disable");

    // Remove alpha's row: its supervisor is reaped and its slot leaves the map.
    std::fs::write(crate::proxy::registry_path().expect("path"), "").expect("remove");
    stub::wait_until("alpha's slot leaves the map", secs(10), || {
        published(&slots, &alpha).is_none()
    });

    // Two draining proxies, then one aggregate stop.
    std::fs::write(dir_a.join("drain"), "30").expect("drain 30");
    crate::proxy::enable("alpha", Some(stub::free_port()), Some(dir_a.as_os_str()))
        .expect("re-enable a");
    crate::proxy::enable("beta", Some(stub::free_port()), Some(dir_b.as_os_str()))
        .expect("enable b");
    stub::wait_until("both run", secs(10), || {
        stub::invocations(&dir_a).len() == 2 && stub::invocations(&dir_b).len() == 1
    });
    let started = std::time::Instant::now();
    supervision.shutdown();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(6),
        "one aggregate budget, never two serial budgets: {took:?}"
    );
    assert_eq!(stub::terms(&dir_a).len(), 2, "alpha: one SIGTERM per run");
    assert_eq!(stub::terms(&dir_b).len(), 1, "beta: one SIGTERM");
    for dir in [&dir_a, &dir_b] {
        for call in stub::invocations(dir) {
            stub::signal_best_effort(call.pid, "KILL");
        }
    }
}

/// The coordinator logs an unreadable registry once per distinct error, driven
/// round by round (never by a wall clock), pinned by whole-line equality.
#[cfg(unix)]
#[test]
fn an_unreadable_registry_is_logged_once_per_distinct_error() {
    let _home = HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    let slots = new_slots();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    // The coordinator runs inside the daemon, whose `serve()` flips the sticky
    // stamp on; the per-thread override is what lets a test pin that role's
    // prefix without flipping the process-global STAMP for every sibling test.
    let _daemon = crate::logline::StampOverride::new(true);
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    let path = crate::proxy::registry_path().expect("registry");
    let now_ms = crate::usage::now_ms();
    let line = |port: u16| {
        format!(
            "clauth daemon: cannot read the proxy registry: invalid proxy registry {}: rows \"qwen\" and \"zcode\" both hold port {port}",
            path.display()
        )
    };

    std::fs::write(&path, "[qwen]\nport = 9101\n[zcode]\nport = 9101\n").expect("corrupt");
    manage_round(&mut coordinator, &slots, now_ms);
    assert_eq!(
        lines.snapshot(),
        vec![line(9101)],
        "the first error is logged whole"
    );

    manage_round(&mut coordinator, &slots, now_ms);
    assert_eq!(
        lines.snapshot(),
        vec![line(9101)],
        "the same error is logged once"
    );

    std::fs::write(&path, "[qwen]\nport = 9102\n[zcode]\nport = 9102\n").expect("differently");
    manage_round(&mut coordinator, &slots, now_ms);
    assert_eq!(
        lines.snapshot(),
        vec![line(9101), line(9102)],
        "a distinct error logs once more"
    );
}

/// A live process a daemon that died hard left, with its child marker written
/// (pid + start time, no stop asked for), under a `state` row (enabled,
/// disabled or removed).
#[cfg(unix)]
fn orphan_rig(state: &str) -> (HomeSandbox, crate::testutil::Owned) {
    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    std::fs::write(dir.join("no-health"), "").expect("no-health");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir.as_os_str())).expect("enable");
    let service = Service::parse("zcode").expect("a service");
    match state {
        "enabled" => {}
        "disabled" => crate::proxy::disable("zcode").expect("disable"),
        "removed" => {
            std::fs::write(crate::proxy::registry_path().expect("path"), "").expect("remove")
        }
        other => panic!("{other}"),
    }
    let orphan = crate::testutil::Owned(
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
    (home, orphan)
}

/// The next daemon reclaims the orphan a disabled row left behind: the
/// coordinator walks the state dirs, not the registry rows, so a disabled (or
/// removed) row's orphan is stopped with one SIGTERM.
#[cfg(unix)]
fn orphan_is_stopped_by_the_next_daemon(state: &str) {
    let (_home, mut orphan) = orphan_rig(state);
    let dir = crate::profile::clauth_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let supervision = start(new_slots(), &singleton).expect("the coordinator");
    let mut status = None;
    let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        stub::wait_until("the orphan is stopped", secs(5), || {
            status = orphan.0.try_wait().expect("try_wait");
            status.is_some()
        })
    }));
    supervision.shutdown();
    assert!(
        waited.is_ok(),
        "a {state} row's orphan is never stopped by the next daemon"
    );
    assert_eq!(
        crate::daemon::gateway::ExitReport::from(status.expect("exited")).signal,
        Some(15),
        "one SIGTERM"
    );
}

#[cfg(unix)]
#[test]
fn a_disabled_rows_orphan_is_stopped_by_the_next_daemon() {
    orphan_is_stopped_by_the_next_daemon("disabled");
}

#[cfg(unix)]
#[test]
fn a_removed_rows_orphan_is_stopped_by_the_next_daemon() {
    orphan_is_stopped_by_the_next_daemon("removed");
}

/// A `stopping` entry keeps the contract beside the version the `/health`
/// answer carried, instead of blanking it.
#[cfg(unix)]
#[test]
fn a_stopping_entry_keeps_the_contract_beside_the_version() {
    let rig = Rig::with_health("zcode", "1.2.0", "1.0");
    std::fs::write(rig.dir.join("ignore-term"), "").expect("ignore-term");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.server.wait_serving(true);
    s.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, ProxyState::Healthy);
    assert_eq!(rig.slot().version.as_deref(), Some("1.2.0"));
    assert_eq!(rig.slot().contract.as_deref(), Some("1.0"));

    crate::proxy::disable("zcode").expect("disable");
    stub::wait_until("the proxy reads stopping", secs(10), || {
        s.step(t0.after(secs(2)));
        rig.slot().state == ProxyState::Stopping
    });
    assert_eq!(rig.slot().version.as_deref(), Some("1.2.0"));
    assert_eq!(rig.slot().contract.as_deref(), Some("1.0"));
}

/// A proxy's manifest is read once per binary identity, cached across retry
/// rounds: a foreign listener on the port holds the spawn, but the `manifest`
/// subprocess runs once, never once per 5 s retry.
#[cfg(unix)]
#[test]
fn the_manifest_is_read_once_per_binary_identity() {
    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("manifests"), "").expect("clear enable's manifest read");
    let _foreign = stub::HttpAnswer::bind(
        rig.port,
        200,
        r#"{"status":"ok","service":"qwen","version":"0.9.1","contract":"1.0"}"#,
    );
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    s.step(t0.after(secs(6)));
    s.step(t0.after(secs(11)));
    assert_eq!(rig.slot().state, ProxyState::Foreign);
    let manifests = std::fs::read_to_string(rig.dir.join("manifests"))
        .expect("manifests")
        .lines()
        .count();
    assert_eq!(manifests, 1, "the manifest is memoized per identity");
}

/// A foreign proxy-shaped answer lacking `service` publishes `None`, never a
/// sentinel string.
#[cfg(unix)]
#[test]
fn a_foreign_proxy_without_a_service_reads_none_not_a_sentinel() {
    let rig = Rig::new("zcode");
    let _foreign = stub::HttpAnswer::bind(
        rig.port,
        200,
        r#"{"status":"ok","version":"0.9.1","contract":"1.0"}"#,
    );
    let mut s = rig.supervisor();
    s.step(t0());
    assert_eq!(rig.slot().state, ProxyState::Foreign);
    assert_eq!(
        rig.slot().answerer,
        Some(ProxyAnswerer::Proxy { service: None }),
        "no sentinel service string"
    );
}

/// A proxy that crashes while `proxies.toml` is unreadable is restarted on its
/// last good row (ruling A-F17: "every supervisor keeps its last good row"),
/// never frozen in `restarting`.
#[cfg(unix)]
#[test]
fn a_crash_under_an_unreadable_registry_restarts_on_the_last_good_row() {
    let rig = Rig::new("zcode");
    let mut s = rig.supervisor();
    let t0 = t0();
    s.step(t0);
    rig.server.wait_serving(true);
    s.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, ProxyState::Healthy);
    let first = rig.only_call().pid;

    std::fs::write(
        crate::proxy::registry_path().expect("registry"),
        "not = [toml",
    )
    .expect("corrupt");
    stub::signal(first, "KILL");
    step_until(&rig, &mut s, t0.after(secs(1)), ProxyState::Restarting);
    rig.server.wait_serving(false);
    s.step(t0.after(secs(3)));
    stub::wait_until(
        "the crash is restarted on the last good row",
        secs(5),
        || stub::invocations(&rig.dir).len() >= 2,
    );
    assert_eq!(
        stub::invocations(&rig.dir).len(),
        2,
        "restarted once, not held off"
    );
}

/// A removed row's reap stops its draining child off the coordinator thread:
/// a daemon stop landing while the reap runs stays inside the one aggregate
/// budget (never the reap's serial budget stacked on the stop's).
#[cfg(unix)]
#[test]
fn a_stop_during_a_reap_stays_inside_one_budget() {
    let home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let slots = new_slots();
    let supervision = start(Arc::clone(&slots), &singleton).expect("the coordinator");
    let dir_a = home.home().join("stub-a");
    let dir_b = home.home().join("stub-b");
    stub::write_proxy_stub(&dir_a, "alpha");
    stub::write_proxy_stub(&dir_b, "beta");
    for d in [&dir_a, &dir_b] {
        std::fs::write(d.join("no-health"), "").expect("no-health");
        std::fs::write(d.join("drain"), "30").expect("drain");
    }
    crate::proxy::enable("alpha", Some(stub::free_port()), Some(dir_a.as_os_str())).expect("a");
    crate::proxy::enable("beta", Some(stub::free_port()), Some(dir_b.as_os_str())).expect("b");
    stub::wait_until("both run", secs(10), || {
        stub::invocations(&dir_a).len() == 1 && stub::invocations(&dir_b).len() == 1
    });

    // Remove alpha's row alone (keep beta): its reap stops a 30 s drain.
    let beta_port = crate::proxy::Registry::load()
        .expect("registry")
        .get(&Service::parse("beta").expect("beta"))
        .expect("row")
        .port;
    std::fs::write(
        crate::proxy::registry_path().expect("path"),
        format!(
            "[beta]\nport = {beta_port}\nenabled = true\nbinary = \"{}\"\n",
            dir_b.join("clauth-beta-proxy").display()
        ),
    )
    .expect("remove alpha");
    stub::wait_until("alpha is asked to stop", secs(10), || {
        stub::terms(&dir_a).len() == 1
    });
    // Let the detached reaper be mid-flight when the daemon stop arrives.
    std::thread::sleep(Duration::from_millis(1200));
    let started = std::time::Instant::now();
    supervision.shutdown();
    let took = started.elapsed();
    for d in [&dir_a, &dir_b] {
        for call in stub::invocations(d) {
            stub::signal_best_effort(call.pid, "KILL");
        }
    }
    assert!(
        took < Duration::from_secs(6),
        "one aggregate budget, never the reap's serial budget: {took:?}"
    );
}

/// A removal reaps a RUNNING proxy's supervisor: the orphan sweep skips the
/// service while its reaper owns the stop, so the removal sends exactly one
/// SIGTERM, never the sweep's SIGTERM on top of the supervisor's stop.
#[cfg(unix)]
#[test]
fn the_orphan_sweep_skips_a_service_its_reaper_owns() {
    let home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    let slots = new_slots();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let dir_a = home.home().join("stub-a");
    stub::write_proxy_stub(&dir_a, "alpha");
    std::fs::write(dir_a.join("no-health"), "").expect("no-health");
    std::fs::write(dir_a.join("ignore-term"), "").expect("ignore-term");
    crate::proxy::enable("alpha", Some(stub::free_port()), Some(dir_a.as_os_str()))
        .expect("enable");
    let alpha = Service::parse("alpha").expect("alpha");

    // The coordinator's own round starts alpha; once its child marker exists a
    // wrong sweep has a live child to signal.
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    stub::wait_until("alpha runs", secs(10), || {
        stub::invocations(&dir_a).len() == 1
    });
    let marker = child_marker_path(&alpha).expect("marker path");
    stub::wait_until("alpha's child marker is written", secs(10), || {
        marker.exists()
    });

    // The removal: the next round reaps alpha through `reap_removed`, whose
    // reaper parks until released, then the same round's sweep runs, and so
    // does a second round's. The sweep runs on this thread, so its "stopping a
    // previous daemon left running" line is what pins its signal — the signal
    // count is not, since the supervisor's own round can send the one SIGTERM
    // while the reaper is parked.
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    REAPER_PARKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(alpha.clone(), go_rx);
    std::fs::write(crate::proxy::registry_path().expect("registry path"), "")
        .expect("remove alpha's row");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert!(
        coordinator.reaping.contains_key(&alpha),
        "the removal records alpha's reaper"
    );
    assert!(
        lines
            .snapshot()
            .iter()
            .all(|line| !line.contains("a previous daemon left running")),
        "the sweep never signals a service whose reaper owns the stop: {:?}",
        lines.snapshot()
    );

    // The reaper then sends alpha's one SIGTERM.
    let _ = go_tx.send(());
    stub::wait_until("the reaper stops alpha", secs(10), || {
        stub::terms(&dir_a).len() == 1
    });
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert_eq!(
        stub::terms(&dir_a).len(),
        1,
        "exactly one SIGTERM, never the sweep's on top of the supervisor's stop"
    );
    for call in stub::invocations(&dir_a) {
        stub::signal_best_effort(call.pid, "KILL");
    }
    shutdown_threads(coordinator.threads, coordinator.reaping);
}

/// The daemon's signal stop joins every detached reap stop: `shutdown_threads`
/// waits for a reaper still stopping its child, so the stop never returns
/// while a removed row's SIGTERM is unsent.
#[cfg(unix)]
#[test]
fn a_stop_joins_every_detached_reap_stop() {
    let alpha = Service::parse("alpha").expect("alpha");
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let reaping = {
        let mut reaping: BTreeMap<Service, Vec<std::thread::JoinHandle<()>>> = BTreeMap::new();
        reaping
            .entry(alpha)
            .or_default()
            .push(std::thread::spawn(move || {
                // A reaper mid-stop: parked until released, as if its stop had
                // not yet sent the child's SIGTERM.
                let _ = started_tx.send(());
                let _ = go_rx.recv();
            }));
        reaping
    };
    started_rx.recv().expect("the reaper runs");
    let done = std::thread::spawn(|| shutdown_threads(BTreeMap::new(), reaping));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !done.is_finished(),
        "the stop waits for the reaper, never drops it detached"
    );
    let _ = go_tx.send(());
    let _ = done.join();
}

/// A shutdown landing while the first `manifest` read runs is preempted: the
/// bounded manifest child is killed and reaped, and the stop answers fast,
/// never reporting a stuck stop with no child running.
#[cfg(unix)]
#[test]
fn a_shutdown_during_a_manifest_read_is_preempted() {
    let rig = Rig::new("zcode");
    let wrapper = rig.dir.join("wrapped").join("clauth-zcode-proxy");
    std::fs::create_dir_all(wrapper.parent().expect("parent")).expect("dir");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = manifest ] && [ -e '{d}/slow' ]; then sleep 4.8; fi\nexec '{b}' \"$@\"\n",
            d = rig.dir.display(),
            b = rig.binary.display(),
        ),
    )
    .expect("wrapper");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    crate::proxy::enable(
        "zcode",
        None,
        Some(wrapper.parent().expect("parent").as_os_str()),
    )
    .expect("re-enable on the wrapper");
    std::fs::write(rig.dir.join("slow"), "").expect("slow");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-proxy",
    )
    .expect("start");
    std::thread::sleep(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let stopped = thread.shutdown(DAEMON_STOP_BUDGET);
    let took = started.elapsed();
    std::thread::sleep(Duration::from_secs(2));
    for call in stub::invocations(&rig.dir) {
        stub::signal_best_effort(call.pid, "KILL");
    }
    assert!(
        stopped,
        "the stop is preempted, never a missed budget with no child"
    );
    assert!(
        took < Duration::from_secs(2),
        "preempted fast, not the manifest's 4.8 s bound: {took:?}"
    );
}

/// With no daemon and an unreadable registry, the single-shot `status --json`
/// path publishes `"proxies": []` and names the error once: `entries`' load
/// error arm is never silent, and the same error is logged once.
#[cfg(unix)]
#[test]
fn an_unreadable_registry_publishes_no_proxies_and_names_the_error_once() {
    let _home = HomeSandbox::new();
    // The `clauth:` prefix comes from `role(stamped())`: pin it, since a prior
    // sibling test that ran `serve()` flips the process-global stamp on.
    let _cli = crate::logline::StampOverride::new(false);
    let dir = crate::profile::clauth_dir().expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    std::fs::write(
        dir.join("proxies.toml"),
        "[qwen]\nport = 9101\n[zcode]\nport = 9101\n",
    )
    .expect("registry");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    assert_eq!(
        entries(None),
        Vec::new(),
        "no rows to publish, still exit-0 empty"
    );
    let expected = format!(
        "clauth: cannot read the proxy registry: invalid proxy registry {}: rows \"qwen\" and \"zcode\" both hold port 9101",
        dir.join("proxies.toml").display()
    );
    assert_eq!(
        lines.snapshot(),
        vec![expected],
        "one line naming the registry error, whole"
    );
    entries(None);
    assert_eq!(
        lines.snapshot().len(),
        1,
        "the same error is logged once, not per read"
    );
}

/// The state letter `ps` names for a pid (`S`, `R`, `Z`), `Ok(None)` once the
/// pid is reaped and gone. A zombie still reads `Ok(Some(_))`. `ps`, never
/// `/proc`, so the reap is actually proven on every platform CI runs (macOS
/// has no `/proc`). A spawn failure is an error the caller logs, never a
/// reaped verdict.
#[cfg(unix)]
fn proc_state(ps: &str, pid: u32) -> Result<Option<String>, String> {
    let output = std::process::Command::new(ps)
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            format!("{ps} could not run: {e}; a reaped verdict without it is no evidence")
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string))
}

/// `proc_state` never reads a spawn failure as reaped: a `ps` that cannot run
/// is an error the caller logs, never `Ok(None)`.
#[cfg(unix)]
#[test]
fn proc_state_never_reads_reaped_when_ps_cannot_spawn() {
    let err = proc_state("/definitely/not/ps", 1).expect_err("a reaped verdict is no evidence");
    assert!(
        err.contains("could not run"),
        "the error names the failed spawn: {err}"
    );
}

/// A stop hands a still-draining child to a detached reaper: once the child
/// dies it is reaped, never left a zombie until the daemon exits.
#[cfg(unix)]
#[test]
fn a_stop_hands_a_still_draining_child_to_a_reaper() {
    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("ignore-term"), "").expect("ignore-term");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-reap",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    assert!(thread.shutdown(DAEMON_STOP_BUDGET), "the stop answers");
    // The child still drains (ignore-term); the reaper holds its handle.
    stub::signal(pid, "KILL");
    stub::wait_until("the child is reaped, not a zombie", secs(5), || {
        proc_state("ps", pid).expect("ps").is_none()
    });
}

/// A supervisor whose own deadline wake runs late still answers its stop: a
/// late answer is a healthy thread, never a stuck one.
#[cfg(unix)]
#[test]
fn a_stop_answered_late_past_its_deadline_still_answers() {
    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("ignore-term"), "").expect("ignore-term");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-late",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    let _late =
        crate::daemon::gateway::probe_seam::delay_answer("zz-late", Duration::from_millis(100));
    let answered = thread.shutdown(Duration::from_millis(300));
    stub::signal(pid, "KILL");
    assert!(answered, "an answer 100 ms past the deadline still counts");
    stub::wait_until("the child is reaped, not a zombie", secs(5), || {
        proc_state("ps", pid).expect("ps").is_none()
    });
}

/// The lines a stop raised on the test thread that call it a missed budget.
#[cfg(unix)]
fn missed_budget_lines(rig: &Rig) -> Vec<String> {
    rig.lines
        .snapshot()
        .into_iter()
        .filter(|line| line.contains("did not stop within"))
        .collect()
}

/// A stop whose child outlives its deadline answers before it writes: a
/// stderr stalled at the "still stopping" line neither reads as a missed
/// budget nor logs one. The stall (2 s) is more than twice the answer wait
/// (the 300 ms budget plus the 500 ms answer grace, 800 ms): the defect, the
/// stall before the answer, answers at the 2.3 s budget-plus-stall, 1.5 s
/// past the wait, while the correct order answers at its deadline wake, inside
/// the grace.
#[cfg(unix)]
#[test]
fn a_write_stall_after_the_deadline_is_never_a_missed_budget() {
    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("ignore-term"), "").expect("ignore-term");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-stall-deadline",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    let _stall = crate::daemon::gateway::probe_seam::stall_stop_writes(
        "zz-stall-deadline",
        Duration::from_secs(2),
    );
    // 2 s stall over the 300 ms budget + 500 ms grace: the stalled write alone
    // overruns the answer wait whenever it sits before the answer.
    let answered = thread.shutdown(Duration::from_millis(300));
    stub::signal(pid, "KILL");
    assert!(
        answered,
        "a write stalled after the deadline is not a stuck thread"
    );
    assert_eq!(missed_budget_lines(&rig), Vec::<String>::new());
    stub::wait_until("the child is reaped, not a zombie", secs(5), || {
        proc_state("ps", pid).expect("ps").is_none()
    });
}

/// A stop whose child exits inside its budget answers before it logs the
/// exit and removes the marker: a stall at those writes never reads as a
/// missed budget. The stall (5 s) is twice the answer wait (the 2 s budget
/// plus the 500 ms answer grace, 2.5 s): the defect answers 5 s after the
/// child's exit, 2.5 s past the wait, while the correct order answers once the
/// child exits.
#[cfg(unix)]
#[test]
fn a_write_stall_after_the_child_exits_is_never_a_missed_budget() {
    let rig = Rig::new("zcode");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-stall-exit",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    // The stub exits on its SIGTERM at once, well inside the 2 s budget; 5 s
    // of stall is twice the 2.5 s answer wait (the budget plus the grace), so
    // the stalled write alone overruns the wait whenever it sits before the
    // answer.
    let _stall = crate::daemon::gateway::probe_seam::stall_stop_writes(
        "zz-stall-exit",
        Duration::from_secs(5),
    );
    let answered = thread.shutdown(Duration::from_secs(2));
    assert!(
        answered,
        "a write stalled after the exit is not a stuck thread"
    );
    assert_eq!(missed_budget_lines(&rig), Vec::<String>::new());
    assert_eq!(
        stub::terms(&rig.dir).len(),
        1,
        "one SIGTERM, and it ended the child"
    );
    stub::wait_until("the child is reaped, not a zombie", secs(5), || {
        proc_state("ps", pid).expect("ps").is_none()
    });
}

/// The clamp a stop applies to its last sleep, pinned two ways: by equality
/// on hand-computed inputs through the pure [`stop_sleep`], then driven
/// through the real loop, asserting the loop slept through the clamp and each
/// requested sleep stayed inside the remaining time it clamped to. The loop's
/// one sleep line sleeps `stop_poll`'s output, and the seam records that
/// `(remaining, request)` pair without sleeping, so the pin holds at any
/// dispatch latency or real-sleep oversleep and never counts polls, and a
/// call site that sleeps the poll regardless records nothing. The 2030 ms
/// budget (40 polls plus a 30 ms remainder, not a multiple of the 50 ms poll)
/// is ~2x the >=1 s latency a loaded box has shown between the budget's start
/// and the loop (`begin_stop`'s `kill` spawn and marker write), so the loop
/// always sleeps at least once; the 30 ms remainder puts the last remaining
/// below the poll unless an oversleep jumps it.
#[cfg(unix)]
#[test]
fn the_stop_wait_clamps_its_last_sleep_to_the_deadline() {
    let ms = Duration::from_millis;
    assert_eq!(stop_sleep(ms(30)), ms(30));
    assert_eq!(stop_sleep(ms(50)), ms(50));
    assert_eq!(stop_sleep(ms(80)), ms(50));
    assert_eq!(stop_sleep(Duration::ZERO), Duration::ZERO);

    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("ignore-term"), "").expect("ignore-term");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-clamp",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    let _arm = crate::daemon::gateway::probe_seam::arm_stop_sleeps("zz-clamp");
    let answered = thread.shutdown(Duration::from_millis(2030));
    stub::signal(pid, "KILL");
    assert!(answered, "the stop answers");
    let sleeps = crate::daemon::gateway::probe_seam::stop_sleeps();
    assert!(
        !sleeps.is_empty()
            && sleeps
                .iter()
                .all(|(remaining, request)| *request <= *remaining),
        "the loop slept through the clamp, each sleep inside its remaining time: {sleeps:?}"
    );
    stub::wait_until("the child is reaped, not a zombie", secs(5), || {
        proc_state("ps", pid).expect("ps").is_none()
    });
}

/// A stalled stderr at the "stopping" line never delays the SIGTERM: the stop
/// sends its one SIGTERM before that write, so a child's drain budget is not
/// eaten by the log. Margins from the two orders this distinguishes, the
/// SIGTERM at L (before the stall) against S + L (after it, the old order):
/// the 3 s cap is half the 6 s stall, so the correct path keeps 3 s for the
/// `/bin/kill` spawn latency while the old order still misses by 3 s, and the
/// 12 s shutdown budget, twice the stall, still answers over stall + spawn +
/// marker fsync.
#[cfg(unix)]
#[test]
fn the_stopping_line_write_never_delays_the_sigterm() {
    let rig = Rig::new("zcode");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-begin-stall",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    let _stall = crate::daemon::gateway::probe_seam::stall_begin_stop(
        "zz-begin-stall",
        Duration::from_secs(6),
    );
    let answered = std::thread::scope(|scope| {
        let shutdown = scope.spawn(move || thread.shutdown(secs(12)));
        stub::wait_until(
            "the stub records its SIGTERM before the stop line stall ends",
            secs(3),
            || !stub::terms(&rig.dir).is_empty(),
        );
        shutdown.join().expect("the supervisor thread joins")
    });
    assert!(answered, "the stop answers");
    assert_eq!(stub::terms(&rig.dir), [pid], "exactly one SIGTERM");
}

/// A daemon hard-killed while the "stopping" line's write stalls still leaves
/// the stop's deadline in the child marker: the SIGTERM goes first, then the
/// deadline marker, then that line, so the next start waits out the drain
/// instead of sending the second SIGTERM that skips it. The arm stalls 6 s at
/// the line; the test waits for the deadline marker's write to land inside
/// that stall, poses the next start, then KILLs the stub so the supervisor
/// answers at the stall's end rather than the 12 s shutdown budget.
#[cfg(unix)]
#[test]
fn a_daemon_killed_during_the_stopping_line_leaves_the_deadline_and_no_second_sigterm() {
    let rig = Rig::new("zcode");
    std::fs::write(rig.dir.join("ignore-term"), "").expect("ignore-term");
    let thread = start_kind(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        "zz-probe-crash",
    )
    .expect("start");
    stub::wait_until("the child runs", secs(10), || {
        stub::invocations(&rig.dir).len() == 1
    });
    let pid = rig.only_call().pid;
    let _stall = crate::daemon::gateway::probe_seam::stall_begin_stop(
        "zz-probe-crash",
        Duration::from_secs(6),
    );
    let (marker, second) = std::thread::scope(|scope| {
        let shutdown = scope.spawn(move || thread.shutdown(secs(12)));
        stub::wait_until("the stub records its SIGTERM", secs(3), || {
            !stub::terms(&rig.dir).is_empty()
        });
        // Wait for the deadline marker's write to land inside the stall, so a
        // daemon killed in that window is posed with the marker it must leave.
        let mut marker = None;
        stub::wait_until(
            "the deadline marker lands inside the stall",
            secs(3),
            || {
                marker = read_marker_at_for(&rig).filter(|m| m.stop_deadline_ms.is_some());
                marker.is_some()
            },
        );
        let marker = marker.expect("a marker");
        // The next daemon's start over that marker.
        let path = crate::proxy::child_marker_path(&rig.service).expect("marker path");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let _ = crate::daemon::gateway::stop_left_behind(&path, "probe", now_ms);
        // A "nothing happens" window the negative needs: `terminate_pid` waits
        // on `kill`'s exit, so when `stop_left_behind` sends the second
        // SIGTERM (the defect) it is already pending when this window opens,
        // and the window only has to cover the stub's trap scheduling plus its
        // record append. A replica of the stub's TERM trap recorded a second
        // SIGTERM within 0.6 ms in 300 of 300 runs at load ~19 (linux,
        // 2026-10-18), so 300 ms is ~500x that; the stop's own timing is
        // already synced by the SIGTERM and marker waits above.
        std::thread::sleep(Duration::from_millis(300));
        let second = stub::terms(&rig.dir).len();
        // KILL inside the scope, before the join, so the supervisor answers at
        // the stall's end instead of waiting out the 12 s budget.
        stub::signal(pid, "KILL");
        let _ = shutdown.join();
        (marker, second)
    });
    assert!(
        marker.stop_deadline_ms.is_some(),
        "the deadline marker is on disk once the SIGTERM is sent: {:?}, SIGTERMs after a next start: {second}",
        marker.stop_deadline_ms
    );
    assert_eq!(second, 1, "a next start never sends a second SIGTERM");
}

/// The proxy memoizes its prepared manifest across retry rounds; the gateway
/// never does, so its supervisor holds no env-file values for its life. The
/// gateway half pins the gated STORE (the memo stays `None` after a spawn),
/// never only the `memoizes()` method's word.
#[cfg(unix)]
#[test]
fn the_proxy_memoizes_and_the_gateway_does_not() {
    use crate::daemon::gateway::Supervised;
    assert!(
        Proxy {
            service: Service::parse("zcode").expect("a service")
        }
        .memoizes()
    );
    assert!(
        !crate::daemon::gateway::Gateway::new().memoizes(),
        "the gateway never reads its memo back and must not hold the env file's values"
    );

    let home = HomeSandbox::new();
    let etc = home.home().join("etc");
    std::fs::create_dir_all(&etc).expect("etc");
    let port = stub::free_port();
    let config = etc.join("shunt.toml");
    std::fs::write(
        &config,
        format!("[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n"),
    )
    .expect("config");
    let dir = home.home().join("stub");
    let binary = stub::write_stub(&dir);
    std::fs::write(dir.join("no-health"), "").expect("no-health");
    let mut record = crate::gateway::GatewayRecord::new(config).expect("adoptable");
    record.binary = Some(binary);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record);
        Ok(())
    })
    .expect("save the record");
    let handle = crate::daemon::gateway::new_handle();
    let mut supervisor = Supervisor::new(std::sync::Arc::clone(&handle));
    supervisor.step(t0());
    assert!(
        supervisor.prepared_memo_is_none(),
        "the gateway never stores its prepare memo (its env file holds secrets)"
    );
    supervisor.kill_for_test();
}

/// One dedup source, not two: the coordinator and the feed's `entries` reading
/// the same bad registry raise one line between them, never one each.
#[cfg(unix)]
#[test]
fn the_same_registry_error_logs_once_across_the_feed_and_the_coordinator() {
    let _home = HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    let slots = new_slots();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    let path = crate::proxy::registry_path().expect("registry");
    std::fs::write(&path, "[qwen]\nport = 9101\n[zcode]\nport = 9101\n").expect("corrupt");

    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    entries(None);

    assert_eq!(
        lines.snapshot().len(),
        1,
        "one line per distinct error across both readers: {:?}",
        lines.snapshot()
    );
}

/// A slot no supervisor owns leaves the map in one round, on both registry
/// arms: the disabled row reads `disabled` after the sweep, and the sweep runs
/// while the registry cannot be read.
#[cfg(unix)]
#[test]
fn a_slot_no_supervisor_owns_leaves_the_map_in_one_round() {
    let _home = HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    let path = crate::proxy::registry_path().expect("registry");
    let alpha = Service::parse("alpha").expect("alpha");
    let slots = new_slots();
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    // A dying supervisor's last `absent` publish lands after its row is gone:
    // the next round's sweep removes it, so the disabled row reads `disabled`,
    // never the stale `absent`.
    std::fs::write(&path, "[alpha]\nport = 9101\n").expect("a disabled alpha row");
    Proxy {
        service: alpha.clone(),
    }
    .publish(&slots, ProxySlot::of(ProxyState::Absent, &alpha));
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert_eq!(
        published(&slots, &alpha),
        None,
        "a slot no supervisor owns leaves the map in one round"
    );
    let live = super::slots(&slots);
    assert_eq!(
        entries(Some(&live)).first().map(|slot| slot.state),
        Some(ProxyState::Disabled),
        "the disabled row reads disabled, never the stale absent"
    );

    // The sweep runs on the unreadable-registry arm too.
    std::fs::write(&path, "not = [toml").expect("corrupt the registry");
    Proxy {
        service: alpha.clone(),
    }
    .publish(&slots, ProxySlot::of(ProxyState::Absent, &alpha));
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert_eq!(
        published(&slots, &alpha),
        None,
        "the sweep runs while the registry is unreadable"
    );
}

/// A slot a supervisor owns survives the sweep: its build-time publish lands
/// before the round's sweep, so a live proxy's slot is never dropped.
#[cfg(unix)]
#[test]
fn a_slot_a_supervisor_owns_survives_the_sweep() {
    let rig = Rig::new("zcode");
    let wrapper = rig.dir.join("wrapped").join("clauth-zcode-proxy");
    std::fs::create_dir_all(wrapper.parent().expect("parent")).expect("dir");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = manifest ] && [ -e '{d}/slow' ]; then sleep 4.8; fi\nexec '{b}' \"$@\"\n",
            d = rig.dir.display(),
            b = rig.binary.display(),
        ),
    )
    .expect("wrapper");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    crate::proxy::enable(
        "zcode",
        None,
        Some(wrapper.parent().expect("parent").as_os_str()),
    )
    .expect("re-enable on the wrapper");
    std::fs::write(rig.dir.join("slow"), "").expect("slow");
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    // The build's publish precedes the sweep, and the supervisor's first
    // republish is ≥ 4.8 s away (the slow `manifest` read), so the owned slot
    // must survive the sweep right after the round returns.
    manage_round(&mut coordinator, &rig.slots, crate::usage::now_ms());
    assert!(
        published(&rig.slots, &rig.service).is_some(),
        "a slot the coordinator owns survives the sweep"
    );
    shutdown_threads(coordinator.threads, coordinator.reaping);
    for call in stub::invocations(&rig.dir) {
        stub::signal_best_effort(call.pid, "KILL");
    }
}

/// A stop asked for while the first `manifest` read runs is preempted: the
/// cancelled read is a no-op in the idle round — it publishes nothing, so the
/// slot stays exactly as the step found it, and logs no "cannot start …
/// cancelled".
#[cfg(unix)]
#[test]
fn a_cancelled_manifest_read_publishes_nothing() {
    let rig = Rig::new("zcode");
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut supervisor = Supervisor::build_with_cancel(
        Proxy {
            service: rig.service.clone(),
        },
        Arc::clone(&rig.slots),
        Arc::clone(&cancel),
    );
    let before = rig.slot();
    cancel.store(true, std::sync::atomic::Ordering::Release);
    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        before,
        "a cancelled read publishes nothing: the slot is the one the step found"
    );
    assert!(
        rig.lines
            .snapshot()
            .iter()
            .all(|line| !line.contains("was cancelled while it ran")),
        "the cancellation is never logged as a start failure: {:?}",
        rig.lines.snapshot()
    );
}

/// A service removed, re-enabled and removed again while its first reaper
/// still owns the stop reaps nothing on the second removal: the re-enabled
/// row starts no supervisor beside the reaper, so no second stop exists to
/// drop, and the one live reaper's handle stays recorded for the daemon
/// stop to join.
#[cfg(unix)]
#[test]
fn a_second_removal_while_the_reaper_owns_the_stop_adds_no_reaper() {
    let _home = HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    let slots = new_slots();
    let alpha = Service::parse("alpha").expect("alpha");
    let path = crate::proxy::registry_path().expect("registry");
    let row =
        "[alpha]\nport = 9101\nenabled = true\nbinary = \"/nonexistent/clauth-alpha-proxy\"\n";

    std::fs::write(&path, row).expect("row");
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert!(
        coordinator.threads.contains_key(&alpha),
        "alpha is supervised"
    );

    let (go1_tx, go1_rx) = std::sync::mpsc::channel();
    REAPER_PARKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(alpha.clone(), go1_rx);
    std::fs::write(&path, "").expect("remove");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert_eq!(
        coordinator.reaping.get(&alpha).map(Vec::len),
        Some(1),
        "the first reaper is recorded"
    );

    // Re-enabled then removed again while the reaper is still parked: the
    // re-enabled row starts nothing, so the second removal records no
    // second reaper and the one live handle is never dropped.
    std::fs::write(&path, row).expect("re-enable");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    std::fs::write(&path, "").expect("remove again");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());

    assert_eq!(
        coordinator.reaping.get(&alpha).map(Vec::len),
        Some(1),
        "the second removal records no reaper and the one live handle stays"
    );

    let _ = go1_tx.send(());
    shutdown_threads(coordinator.threads, coordinator.reaping);
}

/// A proxy row removed and re-enabled while its child drains starts no
/// second supervisor until its reaper finishes the stop: a supervisor
/// starting beside the reaper would read the same child marker, and its
/// reclaim could reap the still-draining child out from under the reaper's
/// `try_wait`. Once the reaper is done, the re-enabled row starts.
#[cfg(unix)]
#[test]
fn a_reenabled_row_starts_no_supervisor_until_its_reaper_finishes() {
    let home = HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("dir")).expect("mkdir");
    let slots = new_slots();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "alpha");
    std::fs::write(dir.join("ignore-term"), "").expect("ignore-term");
    let port = stub::free_port();
    crate::proxy::enable("alpha", Some(port), Some(dir.as_os_str())).expect("enable");
    let alpha = Service::parse("alpha").expect("alpha");
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    let marker_path = child_marker_path(&alpha).expect("marker path");
    stub::wait_until(
        "alpha's child marker is written",
        Duration::from_secs(10),
        || marker_path.exists(),
    );
    // The answerer binds only once the child is spawned: the supervisor's
    // pre-spawn foreign probe refuses to start a child beside an answerer.
    let _health = stub::HttpAnswer::bind(port, 200, PROXY_HEALTH);
    stub::wait_until("alpha reads healthy before its removal", secs(15), || {
        super::slots(&slots)
            .iter()
            .any(|slot| slot.service == alpha.as_str() && slot.state == ProxyState::Healthy)
    });

    // The removal: the next round hands alpha's supervisor to a reaper that
    // parks until released, as if its stop had not yet sent the SIGTERM, so
    // the reaper provably owns the stop through the rounds that follow.
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    REAPER_PARKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(alpha.clone(), go_rx);
    std::fs::write(crate::proxy::registry_path().expect("registry path"), "")
        .expect("remove alpha's row");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert_eq!(
        coordinator.reaping.get(&alpha).map(Vec::len),
        Some(1),
        "the removal records alpha's reaper"
    );

    // Re-enabled while the reaper owns the stop: no second supervisor may
    // start beside it. The row is written back directly: `proxy::enable`
    // re-probes the port, which the answerer the test bound now answers.
    let row = format!(
        "[alpha]\nport = {port}\nenabled = true\nbinary = \"{}\"\n",
        dir.join("clauth-alpha-proxy").display()
    );
    std::fs::write(crate::proxy::registry_path().expect("registry path"), &row)
        .expect("re-enable alpha's row");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert!(
        !coordinator.threads.contains_key(&alpha),
        "a re-enabled row starts no second supervisor while its reaper owns the stop"
    );

    // Once the reaper finishes — the child drains past the stop budget — the
    // re-enabled row starts its supervisor. Drop the ignore-term marker
    // first: any later child dies of its SIGTERM, so the teardown never
    // waits out a second drain.
    std::fs::remove_file(dir.join("ignore-term")).expect("stop ignoring");
    let _ = go_tx.send(());
    stub::wait_until("the reaper finishes its stop", secs(10), || {
        manage_round(&mut coordinator, &slots, crate::usage::now_ms());
        !coordinator.reaping.contains_key(&alpha)
    });
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert!(
        coordinator.threads.contains_key(&alpha),
        "the re-enabled row starts a supervisor once its reaper finishes"
    );

    for call in stub::invocations(&dir) {
        stub::signal_best_effort(call.pid, "KILL");
    }
    shutdown_threads(coordinator.threads, coordinator.reaping);
}

/// With no daemon left, `stop_left_behind_proxies` stops a marker-only orphan:
/// a live pid recorded in a child marker alone, with no supervisor running,
/// gets its one SIGTERM.
#[cfg(unix)]
#[test]
fn stop_left_behind_proxies_stops_a_marker_only_orphan() {
    let home = HomeSandbox::new();
    let dir = home.home().join("stub");
    stub::write_proxy_stub(&dir, "zcode");
    let port = stub::free_port();
    crate::proxy::enable("zcode", Some(port), Some(dir.as_os_str())).expect("enable");
    let service = Service::parse("zcode").expect("a service");
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

    stop_left_behind_proxies();

    let mut status = None;
    stub::wait_until("the marker-only orphan is stopped", secs(5), || {
        status = orphan.0.try_wait().expect("try_wait");
        status.is_some()
    });
    assert_eq!(
        crate::daemon::gateway::ExitReport::from(status.expect("exited")).signal,
        Some(15),
        "one SIGTERM"
    );
}

// ── a removed row's reap stops its running child, cross-platform ──────────

/// The proxy's `/health` body, matching the stub's manifest (`1.2.0`, `1.0`).
const PROXY_HEALTH: &str =
    r#"{"status":"ok","service":"alpha","version":"1.2.0","contract":"1.0"}"#;

/// Write the stub `clauth-<service>-proxy` into `dir`, before `enable` runs
/// its `manifest` subprocess. Unix reuses the `/bin/sh` stub, steered off
/// serving `/health` so the test process answers on every OS.
#[cfg(unix)]
fn write_stub(dir: &std::path::Path, service: &str) {
    stub::write_proxy_stub(dir, service);
}

/// The Windows stub is a `cmd` script (a file `Command::new` routes through
/// `cmd.exe`): it answers `manifest`, then lives until it is force-killed. The
/// loop counts iterations, not seconds, so a stub a failing test left behind
/// ends on its own after about 120 s, more under load.
#[cfg(windows)]
fn write_stub(dir: &std::path::Path, service: &str) {
    std::fs::create_dir_all(dir).expect("stub dir");
    let path = dir.join(format!("clauth-{service}-proxy.cmd"));
    let script = format!(
        "@echo off\r\n\
         if /i \"%~1\"==\"manifest\" (\r\n\
         echo {{\"service\":\"{service}\",\"display_name\":\"\",\"version\":\"1.2.0\",\"contract\":\"1.0\",\"capabilities\":[],\"drain_secs\":0}}\r\n\
         exit /b 0\r\n\
         )\r\n\
         set /a i=0\r\n\
         :loop\r\n\
         if %i% geq 120 exit /b 0\r\n\
         ping -n 2 127.0.0.1 >nul\r\n\
         set /a i=i+1\r\n\
         goto loop\r\n",
        service = service,
    );
    std::fs::write(&path, script).expect("write the cmd stub");
}

/// A removed row's reap stops its supervisor's running child on every OS: the
/// coordinator starts the proxy and the slot reads healthy with its pid and
/// start time tied to the child marker, then the row is removed and the test
/// waits for that pid's recorded start time to stop matching (the child is
/// dead, never a recycled pid reading as alive). The removal round asserts the
/// coordinator dropped alpha's supervisor, so the reap that never runs fails
/// on that assert rather than passing on the supervisor's own idle-stop of an
/// `Absent` row. The residuals this test cannot see both leave the child
/// stopped by another stopper (the idle-stop, or the supervisor's own `Drop`),
/// so the final wait stays green: a leak inside the
/// reaper's joiner (never shutting its supervisor down), and a supervisor
/// removed from `threads` but never handed to a joiner. Every prior test here
/// is `#[cfg(unix)]`, so this is the one that runs the reap on Windows.
#[test]
fn a_removed_rows_supervisor_stops_its_running_child() {
    let home = HomeSandbox::new();
    crate::profile::mkdir_700(&crate::profile::clauth_dir().expect("clauth dir")).expect("mkdir");
    let slots = new_slots();
    let dir = home.home().join("stub");
    let port = stub::free_port();
    write_stub(&dir, "alpha");
    // The `/bin/sh` stub serves `/health` through its named pipe unless this
    // steers it off; the test process answers on every OS.
    #[cfg(unix)]
    std::fs::write(dir.join("no-health"), "").expect("no-health");
    crate::proxy::enable("alpha", Some(port), Some(dir.as_os_str())).expect("enable");

    let alpha = Service::parse("alpha").expect("alpha");
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
    };
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    let marker_path = crate::proxy::child_marker_path(&alpha).expect("marker path");
    stub::wait_until(
        "alpha's child marker is written",
        Duration::from_secs(10),
        || marker_path.exists(),
    );

    // Only once the child is spawned may the answerer bind: the supervisor's
    // pre-spawn foreign probe refuses to start the child beside an answerer
    // already answering `/health` on the port.
    let _health = stub::HttpAnswer::bind(port, 200, PROXY_HEALTH);

    stub::wait_until(
        "alpha reads healthy before its removal",
        Duration::from_secs(15),
        || {
            super::slots(&slots)
                .iter()
                .any(|slot| slot.service == alpha.as_str() && slot.state == ProxyState::Healthy)
        },
    );

    // Read the marker only after the slot reads healthy, and tie the child to
    // that slot: its pid is the marker's pid and its recorded start still
    // matches, so the removal stops a child that is really alive at that
    // moment, never one that exited on its own within the wait.
    let marker: crate::daemon::gateway::ChildMarker =
        serde_json::from_slice(&std::fs::read(&marker_path).expect("read the marker"))
            .expect("parse the marker");
    let pid = marker.pid;
    let start = marker
        .start
        .expect("the marker records the child's start time");
    let slot = super::slots(&slots)
        .into_iter()
        .find(|slot| slot.service == alpha.as_str())
        .expect("alpha's slot is published");
    assert_eq!(slot.state, ProxyState::Healthy, "the slot reads healthy");
    assert_eq!(
        slot.pid,
        Some(pid),
        "the healthy slot's pid is the marker's pid"
    );
    assert_eq!(
        crate::daemon::gateway::process_start_time(pid).as_deref(),
        Some(start.as_str()),
        "the child is alive at its recorded start time before the removal"
    );

    // Remove alpha's row: the next round's reap stops the child off the
    // coordinator thread.
    std::fs::write(crate::proxy::registry_path().expect("registry path"), "").expect("remove");
    manage_round(&mut coordinator, &slots, crate::usage::now_ms());
    assert!(
        !coordinator.threads.contains_key(&alpha),
        "the removal drops alpha's supervisor"
    );
    stub::wait_until(
        "the removed row's child is stopped",
        Duration::from_secs(10),
        || crate::daemon::gateway::process_start_time(pid).as_deref() != Some(start.as_str()),
    );
    shutdown_threads(coordinator.threads, coordinator.reaping);
}
