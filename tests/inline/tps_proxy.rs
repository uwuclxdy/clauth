#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The tps producer overlaying a proxy's token records (contract 1.2) on
//! their transcript twins: a loopback stub proxy registered in the sandbox
//! serves `/clauth/v1/info` and `/clauth/v1/tokens`, and the producer's own
//! tick polls it. Expected values are hand computed from the fixture times.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::testutil::HomeSandbox;

const SESSION: &str = "px-session";
const USER_AT: &str = "2026-09-22T20:46:09.417Z";
/// The transcript twin's last line: its span from `USER_AT` is 13.597 s.
const LINE_AT: &str = "2026-09-22T20:46:23.014Z";
const TRANSCRIPT_MODEL: &str = "claude-opus-5-5";
const TRANSCRIPT_OUTPUT: u64 = 1222;
/// The record's span: 10 s, inside the transcript's.
const STARTED_AT: &str = "2026-09-22T20:46:12.000Z";
const ENDED_AT: &str = "2026-09-22T20:46:22.000Z";
const RECORD_MODEL: &str = "glm-5";
const RECORD_OUTPUT: u64 = 1500;
const DAY: &str = "2026-09-22";
const ACCOUNT: &str = "acct-1";

fn ms(iso: &str) -> u64 {
    u64::try_from(
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis(),
    )
    .unwrap()
}

fn close(actual: f64, want: f64, what: &str) {
    assert!(
        (actual - want).abs() < 1e-3,
        "{what}: got {actual}, want {want}"
    );
}

// ── the stub proxy ──────────────────────────────────────────────────────────

struct StubState {
    capabilities: Value,
    /// Every record the ring holds, oldest first, each with its sequence.
    ring: Vec<(u64, Value)>,
    /// Every request's target, query included, in arrival order.
    targets: Vec<String>,
}

struct Stub {
    port: u16,
    state: Arc<Mutex<StubState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    /// A contract 1.2 proxy answering `/info` with `capabilities` and its
    /// token route off its ring, demanding `admin` on every control route.
    fn start(admin: String, capabilities: Value) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("local_addr").port();
        let state = Arc::new(Mutex::new(StubState {
            capabilities,
            ring: Vec::new(),
            targets: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (Arc::clone(&state), Arc::clone(&stop));
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
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, StubState> {
        self.state.lock().expect("stub state")
    }

    /// Land one record in the ring, as the proxy does at `message_stop`.
    fn land(&self, record: Value) {
        let mut state = self.state();
        let seq = state.ring.len() as u64 + 1;
        state.ring.push((seq, record));
    }

    /// The token route's reads, query included, in arrival order.
    fn token_reads(&self) -> Vec<String> {
        self.state()
            .targets
            .iter()
            .filter(|t| t.split('?').next() == Some("/clauth/v1/tokens"))
            .cloned()
            .collect()
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

/// The cursor naming the ring's newest record (`life.<seq>`), as the core
/// spells it; `life.0` before any record lands.
fn cursor_of(ring: &[(u64, Value)]) -> String {
    format!("life.{}", ring.last().map_or(0, |(seq, _)| *seq))
}

fn serve_one(mut sock: TcpStream, state: &Mutex<StubState>, admin: &str) {
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
    let mut state = state.lock().expect("stub state");
    state.targets.push(target.clone());
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path == "/health" {
        let body =
            json!({"status": "ok", "service": "stub", "version": "0.0.1", "contract": "1.2"});
        return reply(&mut sock, 200, &body);
    }
    if !path.starts_with("/clauth/v1/") {
        return reply(&mut sock, 404, &json!({"type": "error"}));
    }
    if !authed {
        return reply(
            &mut sock,
            401,
            &json!({"ok": false, "error": "unauthorized"}),
        );
    }
    match path {
        "/clauth/v1/info" => {
            let body = json!({
                "service": "stub", "version": "0.0.1", "contract": "1.2",
                "capabilities": state.capabilities, "actions": [], "figures": [], "settings": [],
            });
            reply(&mut sock, 200, &body);
        }
        "/clauth/v1/tokens" => {
            let cursor = cursor_of(&state.ring);
            let since = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="));
            let known = since.and_then(|c| c.strip_prefix("life.")?.parse::<u64>().ok());
            let all: Vec<Value> = state.ring.iter().map(|(_, r)| r.clone()).collect();
            let body = match (since, known) {
                (None, _) => json!({"records": all, "cursor": cursor}),
                (Some(_), Some(seq)) if seq <= state.ring.len() as u64 => {
                    let after: Vec<Value> = state
                        .ring
                        .iter()
                        .filter(|(s, _)| *s > seq)
                        .map(|(_, r)| r.clone())
                        .collect();
                    json!({"records": after, "cursor": cursor})
                }
                (Some(_), _) => json!({"records": all, "cursor": cursor, "dropped": true}),
            };
            reply(&mut sock, 200, &body);
        }
        _ => reply(&mut sock, 404, &json!({"ok": false, "error": "not_found"})),
    }
}

// ── the sandbox around it ───────────────────────────────────────────────────

/// Register the stub as proxy `stub` (enabled or not) and mint its admin
/// token the way `clauth proxy enable` does, then start it.
fn stub_proxy(enabled: bool, capabilities: Value) -> Stub {
    let service = crate::proxy::Service::parse("stub").unwrap();
    let admin = crate::proxy::ensure_proxy_token(&service)
        .unwrap()
        .expose()
        .to_string();
    let stub = Stub::start(admin, capabilities);
    let path = crate::proxy::registry_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("[stub]\nport = {}\nenabled = {enabled}\n", stub.port),
    )
    .unwrap();
    stub
}

fn token_stats_proxy() -> Stub {
    stub_proxy(true, json!(["events", "token_stats", "count_tokens"]))
}

/// Profile `name` on the stub's bind holding account `account` of proxy
/// `stub`, on disk the way `clauth login --proxy` leaves it.
fn proxy_profile(stub: &Stub, name: &str, account: &str) {
    let mut profile = crate::profile::Profile::new(
        name.to_string(),
        Some(format!("http://127.0.0.1:{}", stub.port)),
        Some("clp_stub-key-0".to_string()),
    );
    crate::profile::save_profile(&profile).unwrap();
    let record = crate::profile::ProxyAccount {
        service: crate::proxy::Service::parse("stub").unwrap(),
        account: account.to_string(),
    };
    crate::profile::write_proxy_record(&profile.name, &record).unwrap();
    profile.proxy = Some(record);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    if let Ok(existing) = crate::profile::load_config() {
        config = existing;
    }
    config.add(profile);
    crate::profile::save_app_state(&config.state).unwrap();
}

fn record(id: &str, account: &str) -> Value {
    json!({
        "id": id, "account": account, "model": RECORD_MODEL, "output_tokens": RECORD_OUTPUT,
        "started_at": STARTED_AT, "ended_at": ENDED_AT, "ttft_ms": 900,
    })
}

fn user_line(ts: &str) -> String {
    json!({
        "type": "user", "timestamp": ts, "sessionId": SESSION, "uuid": format!("u-{ts}"),
        "message": {"role": "user", "content": "go"},
    })
    .to_string()
}

fn assistant_line(ts: &str, id: &str, output: u64) -> String {
    json!({
        "type": "assistant", "timestamp": ts, "sessionId": SESSION,
        "uuid": format!("a-{id}-{ts}"),
        "message": {
            "id": id, "model": TRANSCRIPT_MODEL, "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 12, "output_tokens": output,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        },
    })
    .to_string()
}

/// The session's transcript in `kerry`'s isolated runtime, whose live row
/// attributes it to `kerry` through the routing timeline.
fn kerry_transcript(home: &Path) -> PathBuf {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
    home.join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj")
        .join(format!("{SESSION}.jsonl"))
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(path, body).unwrap();
}

fn append_lines(path: &Path, lines: &[String]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

/// The twin of record `msg_px`: one response, `USER_AT` to `LINE_AT`.
fn twin(path: &Path) {
    write_lines(
        path,
        &[
            user_line(USER_AT),
            assistant_line(LINE_AT, "msg_px", TRANSCRIPT_OUTPUT),
        ],
    );
}

/// A day cell's `(output, gen_ms)`, its speed peak left out.
fn cell(snap: &Snapshot, account: &str, model: &str) -> Option<(u64, u64)> {
    snap.days
        .get(DAY)?
        .get(account)?
        .get(model)
        .map(|d| (d.output, d.gen_ms))
}

/// The record's own figures: 1500 tokens over its 10 s span.
const RECORD_CELL: (u64, u64) = (RECORD_OUTPUT, 10_000);
const RECORD_SPEED: f64 = 1500.0 / 10.0;

// ── the overlay ─────────────────────────────────────────────────────────────

#[test]
fn a_proxy_record_overlays_its_transcript_twins_span_output_model_and_account() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));
    stub.land(record("msg_px", ACCOUNT));

    let snap = Producer::new().tick(ms(LINE_AT) + 4_000);

    assert_eq!(cell(&snap, "px", RECORD_MODEL), Some(RECORD_CELL));
    assert_eq!(cell(&snap, "kerry", TRANSCRIPT_MODEL), None);
    assert_eq!(
        snap.accounts.keys().collect::<Vec<_>>(),
        vec!["px"],
        "the record's account spent the response, not the session's member"
    );
    close(
        snap.accounts["px"].m5.speed.unwrap(),
        RECORD_SPEED,
        "px speed",
    );
    close(
        snap.total.m5.rate,
        1500.0 / 300.0,
        "the response counts once",
    );
}

#[test]
fn a_record_whose_account_maps_to_no_profile_keeps_the_timelines_member() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));
    stub.land(record("msg_px", "acct-9"));

    let snap = Producer::new().tick(ms(LINE_AT) + 4_000);

    assert_eq!(cell(&snap, "kerry", RECORD_MODEL), Some(RECORD_CELL));
    assert_eq!(cell(&snap, "px", RECORD_MODEL), None);
    assert_eq!(cell(&snap, "kerry", TRANSCRIPT_MODEL), None);
}

#[test]
fn applied_records_survive_a_restart_with_the_proxy_gone() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));
    stub.land(record("msg_px", ACCOUNT));
    let now = ms(LINE_AT) + 4_000;
    let first = Producer::new().tick(now);
    assert_eq!(cell(&first, "px", RECORD_MODEL), Some(RECORD_CELL));
    drop(stub);

    let restarted = Producer::new().tick(now + 1_000);

    assert_eq!(cell(&restarted, "px", RECORD_MODEL), Some(RECORD_CELL));
    assert_eq!(cell(&restarted, "kerry", TRANSCRIPT_MODEL), None);
}

#[test]
fn a_record_polled_before_its_twin_lands_overlays_it_once_the_twin_is_read() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    let path = kerry_transcript(home.home());
    write_lines(&path, &[user_line(USER_AT)]);
    stub.land(record("msg_px", ACCOUNT));
    let mut producer = Producer::new();
    let before = producer.tick(ms(ENDED_AT) + 500);
    assert_eq!(
        before.days.get(DAY).map(BTreeMap::len).unwrap_or(0),
        0,
        "a record is never a response of its own"
    );
    append_lines(
        &path,
        &[assistant_line(LINE_AT, "msg_px", TRANSCRIPT_OUTPUT)],
    );

    let snap = producer.tick(ms(LINE_AT) + 4_000);

    assert_eq!(cell(&snap, "px", RECORD_MODEL), Some(RECORD_CELL));
    assert_eq!(cell(&snap, "kerry", TRANSCRIPT_MODEL), None);
}

#[test]
fn a_record_landing_after_its_twin_was_read_overlays_it_from_the_next_cursor() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));
    let mut producer = Producer::new();
    let before = producer.tick(ms(LINE_AT) + 1_000);
    assert_eq!(
        cell(&before, "kerry", TRANSCRIPT_MODEL),
        Some((TRANSCRIPT_OUTPUT, 13_597))
    );
    stub.land(record("msg_px", ACCOUNT));

    let snap = producer.tick(ms(LINE_AT) + 4_000);

    assert_eq!(cell(&snap, "px", RECORD_MODEL), Some(RECORD_CELL));
    assert_eq!(cell(&snap, "kerry", TRANSCRIPT_MODEL), None);
    let reads = stub.token_reads();
    assert_eq!(
        reads.last().map(String::as_str),
        Some("/clauth/v1/tokens?since=life.0"),
        "the second poll resumes from the cursor the first one served: {reads:?}"
    );
}

#[test]
fn a_twin_line_growing_after_the_overlay_keeps_the_records_figures() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    let path = kerry_transcript(home.home());
    twin(&path);
    stub.land(record("msg_px", ACCOUNT));
    let mut producer = Producer::new();
    producer.tick(ms(LINE_AT) + 1_000);
    append_lines(
        &path,
        &[assistant_line(
            "2026-09-22T20:46:24.000Z",
            "msg_px",
            TRANSCRIPT_OUTPUT + 400,
        )],
    );

    let snap = producer.tick(ms(LINE_AT) + 4_000);

    assert_eq!(cell(&snap, "px", RECORD_MODEL), Some(RECORD_CELL));
    assert_eq!(cell(&snap, "kerry", TRANSCRIPT_MODEL), None);
}

#[test]
fn a_record_no_transcript_holds_is_dropped_after_ten_minutes_and_logged_once() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));
    stub.land(record("msg_orphan", ACCOUNT));
    stub.land(record("msg_orphan_2", ACCOUNT));
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let mut producer = Producer::new();

    let held = producer.tick(ms(ENDED_AT) + 9 * 60_000);
    assert_eq!(
        cell(&held, "kerry", TRANSCRIPT_MODEL),
        Some((TRANSCRIPT_OUTPUT, 13_597)),
        "an unmatched record changes no response"
    );
    assert_eq!(cell(&held, "px", RECORD_MODEL), None);
    let unmatched = |lines: &crate::logline::LogLines| {
        lines
            .snapshot()
            .into_iter()
            .filter(|l| l.contains("token record"))
            .collect::<Vec<_>>()
    };
    assert_eq!(unmatched(&lines), Vec::<String>::new(), "held, not dropped");

    producer.tick(ms(ENDED_AT) + 11 * 60_000);
    producer.tick(ms(ENDED_AT) + 12 * 60_000);
    drop(capture);

    assert_eq!(
        unmatched(&lines),
        vec![
            "clauth: proxy \"stub\": token record \"msg_orphan\" matches no transcript response; dropping it and every later unmatched record of this proxy without a line"
                .to_string()
        ]
    );
}

// ── who is polled ───────────────────────────────────────────────────────────

#[test]
fn a_proxy_without_token_stats_is_never_asked_for_records() {
    let home = HomeSandbox::new();
    let stub = stub_proxy(true, json!(["events", "count_tokens"]));
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));

    let snap = Producer::new().tick(ms(LINE_AT) + 4_000);

    assert_eq!(stub.token_reads(), Vec::<String>::new());
    assert_eq!(
        cell(&snap, "kerry", TRANSCRIPT_MODEL),
        Some((TRANSCRIPT_OUTPUT, 13_597))
    );
}

#[test]
fn a_disabled_proxy_is_never_asked_for_records() {
    let home = HomeSandbox::new();
    let stub = stub_proxy(false, json!(["events", "token_stats"]));
    proxy_profile(&stub, "px", ACCOUNT);
    twin(&kerry_transcript(home.home()));
    stub.land(record("msg_px", ACCOUNT));

    let snap = Producer::new().tick(ms(LINE_AT) + 4_000);

    assert_eq!(stub.state().targets, Vec::<String>::new());
    assert_eq!(cell(&snap, "px", RECORD_MODEL), None);
}
