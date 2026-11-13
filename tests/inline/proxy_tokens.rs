#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The token poll's identity gate: a loopback stub registered in the sandbox
//! as proxy `stub` answers `/health`, `/clauth/v1/info` and
//! `/clauth/v1/tokens` off a record ring, and records every request head it
//! reads.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::testutil::HomeSandbox;
use crate::tps::{Producer, Snapshot};

struct StubState {
    /// The service `/health` names.
    service: String,
    /// The status `/clauth/v1/tokens` answers.
    tokens_status: u16,
    /// Every record the ring holds, oldest first, each with its sequence.
    records: Vec<(u64, Value)>,
    /// Every request's head, in arrival order.
    heads: Vec<String>,
}

struct Stub {
    admin: String,
    state: Arc<Mutex<StubState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    /// Register a contract 1.2 proxy `stub` listing `token_stats`, its admin
    /// token minted the way `clauth proxy enable` does, and start it.
    fn registered() -> Self {
        let service = crate::proxy::Service::parse("stub").unwrap();
        let admin = crate::proxy::ensure_proxy_token(&service)
            .unwrap()
            .expose()
            .to_string();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(StubState {
            service: "stub".to_string(),
            tokens_status: 200,
            records: Vec::new(),
            heads: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping, token) = (Arc::clone(&state), Arc::clone(&stop), admin.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => serve_one(sock, &shared, &token),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        let path = crate::proxy::registry_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("[stub]\nport = {port}\nenabled = true\n")).unwrap();
        Self {
            admin,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn answer_as(&self, service: &str) {
        service.clone_into(&mut self.state.lock().unwrap().service);
    }

    /// Make `/clauth/v1/tokens` answer 500 until told otherwise.
    fn fail_tokens(&self) {
        self.state.lock().unwrap().tokens_status = 500;
    }

    /// Land one record in the ring, as the proxy does at message_stop.
    fn land(&self, record: Value) {
        let mut state = self.state.lock().unwrap();
        let seq = state.records.len() as u64 + 1;
        state.records.push((seq, record));
    }

    /// The request line of every request whose head carries the admin token
    /// anywhere, header name or value alike.
    fn carrying_the_admin_token(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .heads
            .iter()
            .filter(|head| head.contains(&self.admin))
            .map(|head| head.lines().next().unwrap_or_default().to_string())
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

fn serve_one(mut sock: TcpStream, state: &Mutex<StubState>, admin: &str) {
    sock.set_nonblocking(false).ok();
    let head = String::from_utf8_lossy(&crate::testutil::read_request_head(&mut sock)).into_owned();
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let authed = head.lines().any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("authorization") && v.trim() == format!("Bearer {admin}")
        })
    });
    let mut state = state.lock().unwrap();
    state.heads.push(head);
    let path = target.split('?').next().unwrap_or("");
    let query = target.split_once('?').map_or("", |(_, q)| q);
    let (status, body) = match path {
        "/health" => (
            200,
            json!({"status": "ok", "service": state.service, "version": "0.0.1", "contract": "1.2"}),
        ),
        _ if !authed => (401, json!({"ok": false, "error": "unauthorized"})),
        "/clauth/v1/info" => (
            200,
            json!({
                "service": state.service, "version": "0.0.1", "contract": "1.2",
                "capabilities": ["token_stats"], "actions": [], "figures": [], "settings": [],
            }),
        ),
        "/clauth/v1/tokens" if state.tokens_status != 200 => (
            state.tokens_status,
            json!({"ok": false, "error": "internal"}),
        ),
        "/clauth/v1/tokens" => {
            let cursor = format!("life.{}", state.records.last().map_or(0, |(seq, _)| *seq));
            let since = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="));
            let known = since.and_then(|c| c.strip_prefix("life.")?.parse::<u64>().ok());
            let all: Vec<Value> = state.records.iter().map(|(_, r)| r.clone()).collect();
            let body = match (since, known) {
                (None, _) => json!({"records": all, "cursor": cursor}),
                (Some(_), Some(seq)) if seq <= state.records.len() as u64 => {
                    let after: Vec<Value> = state
                        .records
                        .iter()
                        .filter(|(s, _)| *s > seq)
                        .map(|(_, r)| r.clone())
                        .collect();
                    json!({"records": after, "cursor": cursor})
                }
                (Some(_), _) => json!({"records": all, "cursor": cursor, "dropped": true}),
            };
            (200, body)
        }
        _ => (404, json!({"ok": false, "error": "not_found"})),
    };
    reply(&mut sock, status, &body);
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

// ── the identity gate ───────────────────────────────────────────────────────

#[test]
fn a_proxy_whose_health_stops_naming_it_gets_no_admin_token_within_the_refresh_cadence() {
    let _home = HomeSandbox::new();
    let stub = Stub::registered();
    let mut poller = Poller::default();
    let start = 1_000_000;
    poller.poll(start);
    let first = vec![
        "GET /clauth/v1/info HTTP/1.1".to_string(),
        "GET /clauth/v1/tokens HTTP/1.1".to_string(),
    ];
    assert_eq!(
        stub.carrying_the_admin_token(),
        first,
        "the first poll reads /info, then the records"
    );
    stub.answer_as("someone-else");

    for second in 1..60 {
        poller.poll(start + second * 1_000);
    }

    assert_eq!(
        stub.carrying_the_admin_token(),
        first,
        "a bind no longer answering as the proxy is sent nothing carrying the admin token"
    );
}

// ── a producer beside the stub ──────────────────────────────────────────────

const SESSION: &str = "px-session";
const ACCOUNT: &str = "acct-1";
const TRANSCRIPT_MODEL: &str = "claude-opus-5-5";
const SECOND_MODEL: &str = "claude-opus-5-6";
const TRANSCRIPT_OUTPUT: u64 = 1222;
const RECORD_MODEL: &str = "glm-5";
const RECORD_OUTPUT: u64 = 1500;
/// The record's own figures: 1500 tokens over its 10 s span.
const RECORD_CELL: (u64, u64) = (RECORD_OUTPUT, 10_000);

fn ms(iso: &str) -> u64 {
    u64::try_from(
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis(),
    )
    .unwrap()
}

/// A day cell's `(output, gen_ms)`.
fn cell_day(snap: &Snapshot, day: &str, account: &str, model: &str) -> Option<(u64, u64)> {
    snap.days
        .get(day)?
        .get(account)?
        .get(model)
        .map(|d| (d.output, d.gen_ms))
}

fn record(id: &str, account: &str, started_at: &str, ended_at: &str) -> Value {
    json!({
        "id": id, "account": account, "model": RECORD_MODEL, "output_tokens": RECORD_OUTPUT,
        "started_at": started_at, "ended_at": ended_at,
    })
}

fn user_line(ts: &str) -> String {
    json!({
        "type": "user", "timestamp": ts, "sessionId": SESSION, "uuid": format!("u-{ts}"),
        "message": {"role": "user", "content": "go"},
    })
    .to_string()
}

fn assistant_line(ts: &str, id: &str, model: &str, output: u64) -> String {
    json!({
        "type": "assistant", "timestamp": ts, "sessionId": SESSION,
        "uuid": format!("a-{id}-{ts}"),
        "message": {
            "id": id, "model": model, "role": "assistant",
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

// ── the early-refresh cap ───────────────────────────────────────────────────

/// A proxy whose token route keeps failing is re-read once by the refresh a
/// failed poll brings forward, then not again until the refresh cadence: the
/// early refresh runs at most once per cadence whatever keeps failing.
#[test]
fn a_proxy_whose_token_route_fails_is_read_again_at_most_once_per_refresh_cadence() {
    let _home = HomeSandbox::new();
    let stub = Stub::registered();
    stub.fail_tokens();
    let mut poller = Poller::default();
    let start = 1_000_000;
    let read = vec![
        "GET /clauth/v1/info HTTP/1.1".to_string(),
        "GET /clauth/v1/tokens HTTP/1.1".to_string(),
    ];
    for second in 0..60 {
        poller.poll(start + second * 1_000);
    }
    assert_eq!(
        stub.carrying_the_admin_token(),
        [read.clone(), read].concat(),
        "the failed poll brings one early refresh, then none until the cadence"
    );
}

// ── the settled re-derive a record apply owes ───────────────────────────────

/// A polled record applying to a twin whose support starts at or above the
/// settle boundary owes only the peaks recompute; one whose transcript span
/// starts below the boundary owes the settled days' from-scratch re-derive.
#[test]
fn a_record_landing_above_the_settle_boundary_owes_no_settled_rederive_but_one_below_it_does() {
    let home = HomeSandbox::new();
    let stub = Stub::registered();
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-22T20:30:00.000Z"),
            assistant_line(
                "2026-09-22T20:30:14.000Z",
                "msg_lo",
                TRANSCRIPT_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
            user_line("2026-09-22T20:46:09.417Z"),
            assistant_line(
                "2026-09-22T20:46:23.014Z",
                "msg_hi",
                TRANSCRIPT_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
        ],
    );
    let mut producer = Producer::new();
    producer.tick(ms("2026-09-22T20:46:27.000Z"));
    stub.land(record(
        "msg_hi",
        ACCOUNT,
        "2026-09-22T20:46:12.000Z",
        "2026-09-22T20:46:22.000Z",
    ));
    let baseline = crate::tps::settle_rederives::taken();
    let landed = producer.tick(ms("2026-09-22T20:46:28.000Z"));
    assert_eq!(
        crate::tps::settle_rederives::taken() - baseline,
        0,
        "a twin whose support starts above the boundary owes only the peaks recompute"
    );
    assert_eq!(
        cell_day(&landed, "2026-09-22", "kerry", RECORD_MODEL),
        Some(RECORD_CELL),
        "the record applied"
    );
    stub.land(record(
        "msg_lo",
        ACCOUNT,
        "2026-09-22T20:30:04.000Z",
        "2026-09-22T20:30:14.000Z",
    ));
    let baseline = crate::tps::settle_rederives::taken();
    let landed = producer.tick(ms("2026-09-22T20:46:29.000Z"));
    assert_eq!(
        crate::tps::settle_rederives::taken() - baseline,
        1,
        "a twin whose transcript span starts below the boundary owes the settled re-derive"
    );
    assert_eq!(
        cell_day(&landed, "2026-09-22", "kerry", RECORD_MODEL),
        Some((3000, 20_000)),
        "the second record applied beside the first, both under the record's model"
    );
}

/// A record whose own span starts above the settle boundary still owes the
/// settled re-derive when its twin's transcript span started below it: the
/// settled windows hold the transcript shape the record replaces.
#[test]
fn a_record_whose_twin_started_below_the_boundary_owes_the_settled_rederive() {
    let home = HomeSandbox::new();
    let stub = Stub::registered();
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-22T20:30:00.000Z"),
            assistant_line(
                "2026-09-22T20:46:23.014Z",
                "msg_px",
                TRANSCRIPT_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
        ],
    );
    let mut producer = Producer::new();
    producer.tick(ms("2026-09-22T20:46:27.000Z"));
    stub.land(record(
        "msg_px",
        ACCOUNT,
        "2026-09-22T20:46:12.000Z",
        "2026-09-22T20:46:22.000Z",
    ));
    let baseline = crate::tps::settle_rederives::taken();
    let landed = producer.tick(ms("2026-09-22T20:46:28.000Z"));
    assert_eq!(
        crate::tps::settle_rederives::taken() - baseline,
        1,
        "the transcript shape the settled windows hold owes their re-derive"
    );
    assert_eq!(
        cell_day(&landed, "2026-09-22", "kerry", RECORD_MODEL),
        Some(RECORD_CELL),
        "the record applied"
    );
}

// ── the intake guards on a polled record ────────────────────────────────────

/// The intake refuses a record off the contract's shape — an empty id, a
/// span ending before it starts, an id past 256 UTF-16 units — and applies
/// one at exactly 256 units: only the conformant record overlays its twin.
#[test]
fn a_polled_record_off_the_intake_shape_is_refused_and_one_at_the_utf16_max_applies() {
    let home = HomeSandbox::new();
    let stub = Stub::registered();
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-22T20:46:09.417Z"),
            assistant_line(
                "2026-09-22T20:46:23.014Z",
                "msg_empty",
                TRANSCRIPT_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
            user_line("2026-09-22T20:46:24.000Z"),
            assistant_line(
                "2026-09-22T20:46:26.014Z",
                "msg_span",
                TRANSCRIPT_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
            user_line("2026-09-22T20:46:27.000Z"),
            assistant_line(
                "2026-09-22T20:46:29.014Z",
                &"\u{1F600}".repeat(129),
                TRANSCRIPT_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
            user_line("2026-09-22T20:46:30.000Z"),
            assistant_line(
                "2026-09-22T20:46:32.014Z",
                &"\u{1F600}".repeat(128),
                SECOND_MODEL,
                TRANSCRIPT_OUTPUT,
            ),
        ],
    );
    let mut producer = Producer::new();
    let before = producer.tick(ms("2026-09-22T20:46:40.000Z"));
    stub.land(record(
        "",
        ACCOUNT,
        "2026-09-22T20:46:12.000Z",
        "2026-09-22T20:46:22.000Z",
    ));
    stub.land(record(
        "msg_span",
        ACCOUNT,
        "2026-09-22T20:46:22.000Z",
        "2026-09-22T20:46:12.000Z",
    ));
    stub.land(record(
        &"\u{1F600}".repeat(129),
        ACCOUNT,
        "2026-09-22T20:46:20.000Z",
        "2026-09-22T20:46:24.000Z",
    ));
    stub.land(record(
        &"\u{1F600}".repeat(128),
        ACCOUNT,
        "2026-09-22T20:46:34.000Z",
        "2026-09-22T20:46:44.000Z",
    ));
    let after = producer.tick(ms("2026-09-22T20:46:41.000Z"));
    assert_eq!(
        cell_day(&after, "2026-09-22", "kerry", TRANSCRIPT_MODEL),
        cell_day(&before, "2026-09-22", "kerry", TRANSCRIPT_MODEL),
        "the refused records overlay nothing: their twins' transcript cells stay"
    );
    assert_eq!(
        cell_day(&after, "2026-09-22", "kerry", SECOND_MODEL),
        None,
        "the applied record replaces its twin's transcript cell"
    );
    assert_eq!(
        cell_day(&after, "2026-09-22", "kerry", RECORD_MODEL),
        Some(RECORD_CELL),
        "the record at 256 UTF-16 units applies"
    );
}
