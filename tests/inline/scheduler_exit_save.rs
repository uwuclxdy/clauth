#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A clean stop's wait on the scheduler's tick thread: the daemon's stop
//! helper wakes the thread, waits for it, and the thread saves the token
//! record its producer applied inside the records cadence, under the lease
//! of its last producer pass only. The thread runs a tick body holding only
//! the producer leg, with its clock fixed per tick, so the cadence never
//! lands the record on its own; a loopback stub proxy serves the record.
//! Expected values are the fixture's own record ids.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::testutil::HomeSandbox;

const SESSION: &str = "exit-session";
const ACCOUNT: &str = "acct-1";

/// The producer's clock on the thread's first tick, each later tick 1 s on.
fn t0() -> u64 {
    u64::try_from(
        chrono::DateTime::parse_from_rfc3339("2026-09-22T20:46:30.000Z")
            .unwrap()
            .timestamp_millis(),
    )
    .unwrap()
}

/// Producer passes the tick thread has finished.
static PASSES: AtomicU64 = AtomicU64::new(0);

/// The tick body: the producer leg alone, under the fetch lease's ticket.
fn producer_leg(state: &SchedulerState) {
    let Some(ticket) = state.fetch_lease.acquire_ticket() else {
        return;
    };
    let pass = PASSES.load(Ordering::SeqCst);
    tps_producer_tick_leased(t0() + pass * 1_000, Some(ticket));
    PASSES.store(pass + 1, Ordering::SeqCst);
}

/// Stops and waits for the thread however the test ends, so it never
/// outlives the sandbox.
struct StopsOnDrop(crate::daemon::TickThread);

impl Drop for StopsOnDrop {
    fn drop(&mut self) {
        self.0.stop();
        self.0.wait_stopped(Instant::now(), Duration::from_secs(60));
    }
}

/// A contract 1.2 proxy serving `/info` with `token_stats` and `/tokens` off
/// its ring, the cursor naming how many records the ring held.
struct Stub {
    port: u16,
    ring: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    fn start(admin: String) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("local_addr").port();
        let ring = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (Arc::clone(&ring), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => serve_one(sock, &shared, &admin),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            port,
            ring,
            stop,
            thread: Some(thread),
        }
    }

    fn land(&self, record: Value) {
        self.ring.lock().unwrap().push(record);
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn reply(sock: &mut TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes());
    let _ = sock.write_all(body.as_bytes());
    let _ = sock.shutdown(std::net::Shutdown::Write);
}

fn serve_one(mut sock: TcpStream, ring: &Mutex<Vec<Value>>, admin: &str) {
    sock.set_nonblocking(false).ok();
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => req.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&req).into_owned();
    let mut lines = head.lines();
    let target = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let authed = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("authorization") && v.trim() == format!("Bearer {admin}")
        })
    });
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path == "/health" {
        let body =
            json!({"status": "ok", "service": "stub", "version": "0.0.1", "contract": "1.2"});
        return reply(&mut sock, 200, &body);
    }
    if !authed {
        return reply(
            &mut sock,
            401,
            &json!({"ok": false, "error": "unauthorized"}),
        );
    }
    let ring = ring.lock().unwrap();
    match path {
        "/clauth/v1/info" => {
            let body = json!({
                "service": "stub", "version": "0.0.1", "contract": "1.2",
                "capabilities": ["token_stats"], "actions": [], "figures": [], "settings": [],
            });
            reply(&mut sock, 200, &body);
        }
        "/clauth/v1/tokens" => {
            let seen = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="))
                .and_then(|c| c.strip_prefix("c.")?.parse::<usize>().ok())
                .filter(|seen| *seen <= ring.len())
                .unwrap_or(0);
            let body =
                json!({"records": ring[seen..].to_vec(), "cursor": format!("c.{}", ring.len())});
            reply(&mut sock, 200, &body);
        }
        _ => reply(&mut sock, 404, &json!({"ok": false, "error": "not_found"})),
    }
}

/// Register the stub as enabled proxy `stub` with its admin token minted the
/// way `clauth proxy enable` does, and profile `px` holding `ACCOUNT` on it.
fn stub_proxy_with_profile() -> Stub {
    let service = crate::proxy::Service::parse("stub").unwrap();
    let admin = crate::proxy::ensure_proxy_token(&service)
        .unwrap()
        .expose()
        .to_string();
    let stub = Stub::start(admin);
    let path = crate::proxy::registry_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("[stub]\nport = {}\nenabled = true\n", stub.port),
    )
    .unwrap();
    let mut profile = crate::profile::Profile::new(
        "px".to_string(),
        Some(format!("http://127.0.0.1:{}", stub.port)),
        Some("clp_stub-key-0".to_string()),
    );
    crate::profile::save_profile(&profile).unwrap();
    let record = crate::profile::ProxyAccount {
        service,
        account: ACCOUNT.to_string(),
    };
    crate::profile::write_proxy_record(&profile.name, &record).unwrap();
    profile.proxy = Some(record);
    let mut config = crate::profile::load_config().unwrap_or(crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    });
    config.add(profile);
    crate::profile::save_app_state(&config.state).unwrap();
    stub
}

fn record(id: &str, started_at: &str, ended_at: &str) -> Value {
    json!({
        "id": id, "account": ACCOUNT, "model": "glm-5", "output_tokens": 1500,
        "started_at": started_at, "ended_at": ended_at, "ttft_ms": 900,
    })
}

/// The session's transcript in `kerry`'s isolated runtime, with its live row.
fn kerry_transcript(home: &Path) -> PathBuf {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
    home.join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj")
        .join(format!("{SESSION}.jsonl"))
}

fn transcript_line(ts: &str, message: Value, kind: &str) -> String {
    json!({
        "type": kind, "timestamp": ts, "sessionId": SESSION,
        "uuid": format!("{kind}-{ts}"), "message": message,
    })
    .to_string()
}

fn assistant(id: &str) -> Value {
    json!({
        "id": id, "model": "claude-opus-5-5", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "usage": {"input_tokens": 12, "output_tokens": 1222,
                  "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
    })
}

/// The record ids the attribution file holds, or none while it is absent.
fn saved_record_ids(home: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(home.join(".clauth").join(crate::tps::TPS_ATTRIBUTION_FILE))
    else {
        return Vec::new();
    };
    let file: Value = serde_json::from_slice(&bytes).unwrap();
    file["records"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Poll `done` every 20 ms; a hang detector, not a race bound.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "still waiting after 60 s: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The stub proxy and profile, the transcript holding both twins, and
/// `msg_r1` in the ring.
fn fixture(home: &Path) -> Stub {
    let stub = stub_proxy_with_profile();
    let transcript = kerry_transcript(home);
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let lines = [
        transcript_line(
            "2026-09-22T20:46:00.000Z",
            json!({"role": "user", "content": "go"}),
            "user",
        ),
        transcript_line("2026-09-22T20:46:10.000Z", assistant("msg_r1"), "assistant"),
        transcript_line("2026-09-22T20:46:20.000Z", assistant("msg_r2"), "assistant"),
    ];
    std::fs::write(&transcript, format!("{}\n", lines.join("\n"))).unwrap();
    stub.land(record(
        "msg_r1",
        "2026-09-22T20:46:01.000Z",
        "2026-09-22T20:46:10.000Z",
    ));
    stub
}

fn land_r2(stub: &Stub) {
    stub.land(record(
        "msg_r2",
        "2026-09-22T20:46:11.000Z",
        "2026-09-22T20:46:20.000Z",
    ));
}

/// The exit save runs under the ticket of the producer's last leased pass:
/// after a watchdog release and a re-take with no producer pass since, it
/// writes nothing over the file the lease's other holder owned; once a pass
/// ran under the current lease, it writes the record applied inside the
/// cadence.
#[test]
fn the_tick_threads_exit_save_writes_only_under_the_lease_of_its_last_producer_pass() {
    let home = HomeSandbox::new();
    let stub = fixture(home.home());
    let lease = crate::daemon::FetchLease::new();
    let first = lease.acquire_ticket().expect("take the fetch lease");
    tps_producer_tick_leased(t0(), Some(first));
    assert_eq!(saved_record_ids(home.home()), ["msg_r1"], "fixture");
    land_r2(&stub);
    tps_producer_tick_leased(t0() + 1_000, Some(first));
    assert_eq!(
        saved_record_ids(home.home()),
        ["msg_r1"],
        "fixture: msg_r2 is applied but unsaved inside the cadence"
    );

    let bound = Duration::from_secs(5 * 60);
    assert!(
        lease.release_if_stale(crate::usage::now_ms() + 2 * bound.as_millis() as u64, bound),
        "fixture: the watchdog releases the lease"
    );
    let retaken = lease.acquire_ticket().expect("re-take the fetch lease");
    let writes_before = crate::tps::attribution_writes::taken();
    tps_exit_save(&lease);
    assert_eq!(
        crate::tps::attribution_writes::taken() - writes_before,
        0,
        "an exit save under the released lease's generation writes nothing"
    );
    assert_eq!(saved_record_ids(home.home()), ["msg_r1"]);

    tps_producer_tick_leased(t0() + 2_000, Some(retaken));
    tps_exit_save(&lease);
    assert_eq!(
        saved_record_ids(home.home()),
        ["msg_r1", "msg_r2"],
        "a pass under the re-taken lease lets the exit save land the record"
    );
}

/// A stop cuts the tick thread's interval sleep short: a sleep of a minute
/// ends well inside the 5 s wait.
#[test]
fn a_stop_ends_the_tick_threads_sleep_at_once() {
    // The give-up line a red logs stays out of the real home.
    let _home = HomeSandbox::new();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutting_down);
    let sleeper = std::thread::spawn(move || {
        sleep_unless_stopped(&flag, Duration::from_secs(60));
    });
    let thread = crate::daemon::TickThread::new(Some(sleeper), shutting_down);

    thread.stop();

    assert!(
        thread.wait_stopped(Instant::now(), Duration::from_secs(5)),
        "the stop woke the sleep"
    );
}

/// The daemon's stop helper returns only once the tick thread ended, its
/// exit save landed.
#[test]
fn a_daemon_stop_waits_for_the_tick_threads_save_of_an_applied_record() {
    let home = HomeSandbox::new();
    let stub = fixture(home.home());
    PASSES.store(0, Ordering::SeqCst);

    let state = SchedulerState::over_shared(
        Arc::new(RankedMutex::new(crate::profile::load_config().unwrap())),
        Arc::new(RankedMutex::new(Vec::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        Arc::new(RankedMutex::new(HashMap::new())),
        crate::usage::new_snapshot(&[]),
    );
    let spends = crate::daemon::SpendDrain::new(
        Arc::clone(&state.activity),
        Arc::clone(&state.shutting_down),
    );
    let thread = spawn_tick_thread(state, producer_leg);
    let _stops = StopsOnDrop(thread.clone());
    let spends = spends.with_scheduler(thread.clone());

    wait_for("the life's first save of msg_r1", || {
        saved_record_ids(home.home()) == ["msg_r1"]
    });
    land_r2(&stub);
    let landed_at = PASSES.load(Ordering::SeqCst);
    wait_for("a whole producer pass after msg_r2 landed", || {
        PASSES.load(Ordering::SeqCst) >= landed_at + 2
    });
    assert_eq!(
        saved_record_ids(home.home()),
        ["msg_r1"],
        "fixture: msg_r2 is applied but unsaved inside the cadence"
    );

    assert!(
        spends.drain(crate::daemon::SPEND_DRAIN_BOUND),
        "no token spend ran"
    );

    assert!(
        thread.wait_stopped(Instant::now(), Duration::ZERO),
        "the stop returned only once the tick thread ended"
    );
    assert_eq!(
        saved_record_ids(home.home()),
        ["msg_r1", "msg_r2"],
        "the stop saved the record applied inside the cadence"
    );
}
