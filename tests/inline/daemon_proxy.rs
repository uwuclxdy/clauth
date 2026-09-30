#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The per-proxy supervisor over the generic machine: one stub proxy runs
//! healthy and restarts alone, a disabled or missing-binary row runs nothing,
//! a foreign answerer and a contract-major mismatch are refused by name, and a
//! daemon stop sends exactly one SIGTERM with the orphan finished by the next
//! daemon at its recorded deadline. Every test drives a stub
//! `clauth-<service>-proxy` under a `HomeSandbox`.

#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use super::*;
#[cfg(unix)]
use crate::daemon::gateway::{DAEMON_STOP_BUDGET, Supervisor, Tick, start_kind};
#[cfg(unix)]
use crate::testutil::HomeSandbox;

#[cfg(unix)]
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

    s.shutdown(std::time::Instant::now() + secs(4));
    rig.server.wait_serving(false);
    assert_eq!(stub::terms(&rig.dir), [pid], "exactly one SIGTERM");
}

/// A process the test owns outright, killed and reaped at the end.
#[cfg(unix)]
struct Owned(std::process::Child);

#[cfg(unix)]
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
    before.shutdown(std::time::Instant::now());
    let recorded = read_marker_at_for(&rig).expect("a marker");
    let deadline_ms = recorded.stop_deadline_ms.expect("the stop's deadline");
    let mut orphan = Owned(before.abandon().expect("still running"));

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

    s.shutdown(std::time::Instant::now());
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
    let mut coordinator = Coordinator {
        threads: BTreeMap::new(),
        reaping: BTreeMap::new(),
        last_registry_error: None,
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
fn orphan_rig(state: &str) -> (HomeSandbox, Owned) {
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
    let orphan = Owned(
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
/// SIGTERM (the reaper's), never the sweep's SIGTERM plus the reaper's.
#[cfg(unix)]
#[test]
fn the_orphan_sweep_skips_a_service_its_reaper_owns() {
    let home = HomeSandbox::new();
    let dir = crate::profile::clauth_dir().expect("dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    let slots = new_slots();
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
        last_registry_error: None,
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
    // does a second round's. The parked reaper owns the stop, so any SIGTERM
    // seen before the release is the sweep's.
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
    assert_eq!(
        stub::terms(&dir_a),
        Vec::<u32>::new(),
        "the sweep never signals a service whose reaper owns the stop"
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
        "exactly one SIGTERM, the reaper's, never the sweep's plus the reaper's"
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
        let mut reaping = BTreeMap::new();
        reaping.insert(
            alpha,
            std::thread::spawn(move || {
                // A reaper mid-stop: parked until released, as if its stop had
                // not yet sent the child's SIGTERM.
                let _ = started_tx.send(());
                let _ = go_rx.recv();
            }),
        );
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

/// The state letter `ps` names for a pid (`S`, `R`, `Z`), `None` once the pid
/// is reaped and gone. A zombie still reads `Some(_)`. `ps`, never `/proc`, so
/// the reap is actually proven on every platform CI runs (macOS has no
/// `/proc`).
#[cfg(unix)]
fn proc_state(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
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
        proc_state(pid).is_none()
    });
}

/// The proxy memoizes its prepared manifest across retry rounds; the gateway
/// never does, so its supervisor holds no env-file values for its life.
#[cfg(unix)]
#[test]
fn the_proxy_memoizes_and_the_gateway_does_not() {
    use crate::daemon::gateway::{Gateway, Supervised};
    assert!(
        Proxy {
            service: Service::parse("zcode").expect("a service")
        }
        .memoizes()
    );
    assert!(
        !Gateway.memoizes(),
        "the gateway never reads its memo back and must not hold the env file's values"
    );
}
