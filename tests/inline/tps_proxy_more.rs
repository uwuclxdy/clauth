#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The tps producer's record lifetime and the re-derive sites the overlay
//! tests beside it do not drive: a record outliving its folded day's response
//! in the kept tail, the
//! records save cadence beside a broken speed ledger, and the reattribute and
//! profile-refresh paths that re-apply an applied record. A loopback stub
//! proxy serves the poll where a test needs records to land; the rest seed
//! the saved file. Expected values are hand computed from the fixture times.

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
const ACCOUNT: &str = "acct-1";
const TRANSCRIPT_MODEL: &str = "claude-opus-5-5";
const TRANSCRIPT_OUTPUT: u64 = 1222;
const RECORD_MODEL: &str = "glm-5";
const RECORD_OUTPUT: u64 = 1500;
/// The record's own figures: 1500 tokens over its 10 s span.
const RECORD_CELL: (u64, u64) = (RECORD_OUTPUT, 10_000);
/// The records save cadence, hand-written from the product's platform split
/// (never read from the product's constant: a cadence regression would move
/// this test's ticks with it and keep it green).
const CADENCE_MS: u64 = if cfg!(windows) { 60_000 } else { 10 * 60_000 };

fn ms(iso: &str) -> u64 {
    u64::try_from(
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis(),
    )
    .unwrap()
}

/// A day cell's `(output, gen_ms)`, its speed peak left out.
fn cell(snap: &Snapshot, account: &str, model: &str) -> Option<(u64, u64)> {
    cell_day(snap, "2026-09-22", account, model)
}

fn cell_day(snap: &Snapshot, day: &str, account: &str, model: &str) -> Option<(u64, u64)> {
    snap.days
        .get(day)?
        .get(account)?
        .get(model)
        .map(|d| (d.output, d.gen_ms))
}

// ── the stub proxy ──────────────────────────────────────────────────────────

struct StubState {
    capabilities: Value,
    /// Every record the ring holds, oldest first, each with its sequence.
    ring: Vec<(u64, Value)>,
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

    /// Land one record in the ring, as the proxy does at `message_stop`.
    fn land(&self, record: Value) {
        let mut state = self.state.lock().expect("stub state");
        let seq = state.ring.len() as u64 + 1;
        state.ring.push((seq, record));
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
    let state = state.lock().expect("stub state");
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
    profile_holding(
        name,
        account,
        Some(format!("http://127.0.0.1:{}", stub.port)),
    );
}

/// Profile `name` holding account `account` of proxy `stub`, with no bind to
/// probe: a test that never polls needs only the profile's proxy record.
fn proxy_profile_only(name: &str, account: &str) {
    profile_holding(name, account, Some("http://127.0.0.1:1".to_string()));
}

fn profile_holding(name: &str, account: &str, base_url: Option<String>) {
    let mut profile = crate::profile::Profile::new(
        name.to_string(),
        base_url,
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

fn record(id: &str, account: &str, started_at: &str, ended_at: &str) -> Value {
    json!({
        "id": id, "account": account, "model": RECORD_MODEL, "output_tokens": RECORD_OUTPUT,
        "started_at": started_at, "ended_at": ended_at, "ttft_ms": 900,
    })
}

/// The saved shape of an applied record, as `tps_attribution.json` holds it.
fn saved_record(id: &str, account: &str, started_at: &str, ended_at: &str) -> Value {
    json!({
        "id": id, "service": "stub", "account": account, "model": RECORD_MODEL,
        "output": RECORD_OUTPUT,
        "start_ms": ms(started_at), "end_ms": ms(ended_at),
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

/// Re-file the session's live row with `member` as its current member, as a
/// swap re-writes it.
fn reg(member: Option<&str>) {
    crate::live_sessions::register(&crate::live_sessions::LiveSession {
        isolated: true,
        current_member: member.map(str::to_owned),
        ..crate::testutil::live_row("4242-1", "kerry")
    })
    .unwrap();
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(path, body).unwrap();
}

/// One kerry response on `day` (`output` tokens over a `gen_ms` span from
/// 12:00), alone in an isolated-store transcript last written that day, so
/// only a read back to `day` sees it.
fn day_transcript(home: &Path, day: &str, output: u64, gen_ms: u64) {
    let session = format!("owed-{day}");
    let path = home
        .join(".clauth/profiles/kerry/runtime-isolated-4242-1/projects/-home-u-proj")
        .join(format!("{session}.jsonl"));
    let start = ms(&format!("{day}T12:00:00.000Z"));
    let user = json!({
        "type": "user", "timestamp": iso_ms(start), "sessionId": session,
        "uuid": format!("u-{day}"),
        "message": {"role": "user", "content": "go"},
    })
    .to_string();
    let assistant = json!({
        "type": "assistant", "timestamp": iso_ms(start + gen_ms), "sessionId": session,
        "uuid": format!("a-msg_{day}"),
        "message": {
            "id": format!("msg_{day}"), "model": TRANSCRIPT_MODEL, "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 12, "output_tokens": output,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        },
    })
    .to_string();
    write_lines(&path, &[user, assistant]);
    crate::testutil::set_mtime(
        &path,
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(start + gen_ms + 1_000),
    );
}

fn iso_ms(millis: u64) -> String {
    chrono::DateTime::from_timestamp_millis(i64::try_from(millis).unwrap())
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The applied records a previous producer saved, entries empty.
fn write_attribution(home: &Path, records: &[Value]) {
    let path = home.join(".clauth").join(TPS_ATTRIBUTION_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let wire = serde_json::json!({"version": 1, "entries": [], "records": records});
    std::fs::write(&path, wire.to_string()).unwrap();
}

fn attribution(home: &Path) -> Value {
    let path = home.join(".clauth").join(TPS_ATTRIBUTION_FILE);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn record_ids(home: &Path) -> Vec<String> {
    attribution(home)
        .get("records")
        .and_then(Value::as_array)
        .map(|records| {
            records
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ── a record outliving its folded day ───────────────────────────────────────

/// The tail keeps a folded day's last 10 s of responses for yesterday's first
/// windows: a record overlaying one of those stays in the saved file after
/// the day's fold, so a restart re-applies it and re-derives the record's
/// figures.
#[test]
fn a_record_whose_twin_outlives_its_folded_day_stays_saved_and_reapplies_on_restart() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-20T23:59:40.000Z"),
            assistant_line("2026-09-20T23:59:55.000Z", "msg_px", TRANSCRIPT_OUTPUT),
        ],
    );
    let mut producer = Producer::new();
    producer.tick(ms("2026-09-21T00:01:00.000Z"));
    producer.tick(ms("2026-09-22T00:00:30.000Z")); // the day folds
    stub.land(record(
        "msg_px",
        ACCOUNT,
        "2026-09-20T23:59:45.000Z",
        "2026-09-20T23:59:55.000Z",
    ));
    producer.tick(ms("2026-09-22T00:01:00.000Z"));
    producer.tick(ms("2026-09-22T00:02:00.000Z"));
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_px".to_string()],
        "the record stays saved while its response sits in the kept tail"
    );

    drop(stub);
    let restarted = Producer::new().tick(ms("2026-09-21T23:59:59.000Z"));

    assert_eq!(
        cell_day(&restarted, "2026-09-20", "px", RECORD_MODEL),
        Some(RECORD_CELL),
        "the restart re-applies the saved record to the response it overlays"
    );
    assert_eq!(
        cell_day(&restarted, "2026-09-20", "kerry", TRANSCRIPT_MODEL),
        None
    );
}

// ── the re-derive sites ─────────────────────────────────────────────────────

/// A member change after the overlay re-applies the record: the cell stays
/// under the record's profile, never the member the swap observed.
#[test]
fn a_member_change_after_the_overlay_keeps_the_records_profile() {
    let home = HomeSandbox::new();
    proxy_profile_only("px", ACCOUNT);
    let path = kerry_transcript(home.home());
    write_lines(
        &path,
        &[
            user_line("2026-09-22T20:46:09.417Z"),
            assistant_line("2026-09-22T20:46:23.014Z", "msg_px", TRANSCRIPT_OUTPUT),
        ],
    );
    write_attribution(
        home.home(),
        &[saved_record(
            "msg_px",
            ACCOUNT,
            "2026-09-22T20:46:12.000Z",
            "2026-09-22T20:46:22.000Z",
        )],
    );
    let mut producer = Producer::new();
    let first = producer.tick(ms("2026-09-22T20:46:27.000Z"));
    assert_eq!(
        cell(&first, "px", RECORD_MODEL),
        Some(RECORD_CELL),
        "the record's profile spent the response"
    );
    reg(Some("mommy"));

    let swapped = producer.tick(ms("2026-09-22T20:46:28.000Z"));

    assert_eq!(
        cell(&swapped, "px", RECORD_MODEL),
        Some(RECORD_CELL),
        "the member change leaves the cell under the record's profile"
    );
    assert_eq!(cell(&swapped, "kerry", RECORD_MODEL), None);
    assert_eq!(cell(&swapped, "mommy", RECORD_MODEL), None);
}

/// A profile created after the overlay takes the record once the profile map
/// refreshes: the cell moves from the timeline's member to that profile.
#[test]
fn a_profile_created_after_the_overlay_takes_the_record_once_the_map_refreshes() {
    let home = HomeSandbox::new();
    let path = kerry_transcript(home.home());
    write_lines(
        &path,
        &[
            user_line("2026-09-22T20:46:09.417Z"),
            assistant_line("2026-09-22T20:46:23.014Z", "msg_px", TRANSCRIPT_OUTPUT),
        ],
    );
    write_attribution(
        home.home(),
        &[saved_record(
            "msg_px",
            ACCOUNT,
            "2026-09-22T20:46:12.000Z",
            "2026-09-22T20:46:22.000Z",
        )],
    );
    let mut producer = Producer::new();
    let first = producer.tick(ms("2026-09-22T20:46:27.000Z"));
    assert_eq!(
        cell(&first, "kerry", RECORD_MODEL),
        Some(RECORD_CELL),
        "no profile holds the account yet: the timeline's member spends it"
    );
    proxy_profile_only("px", ACCOUNT);

    let refreshed =
        producer.tick(ms("2026-09-22T20:46:27.000Z") + crate::proxy_tokens::REFRESH_INTERVAL_MS);

    assert_eq!(
        cell(&refreshed, "px", RECORD_MODEL),
        Some(RECORD_CELL),
        "the refreshed profile map hands the record's account to its profile"
    );
    assert_eq!(cell(&refreshed, "kerry", RECORD_MODEL), None);
}

/// A day holding an applied record leaves the window: the fold waits for the
/// record's save, so the record rides the fold tick's save, and the prune
/// drops it from the file on the next records save once its response left
/// the tail.
#[test]
fn a_record_applied_to_a_leaving_day_rides_the_fold_and_leaves_with_the_prune() {
    let home = HomeSandbox::new();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    let stub = token_stats_proxy();
    let mut producer = Producer::new();
    producer.tick(ms("2026-09-21T23:59:59.000Z"));
    stub.land(record(
        "msg_2026-09-20",
        ACCOUNT,
        "2026-09-20T12:00:00.000Z",
        "2026-09-20T12:00:03.000Z",
    ));
    producer.tick(ms("2026-09-22T00:00:30.000Z"));
    producer.tick(ms("2026-09-22T00:00:31.000Z"));
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_2026-09-20".to_string()],
        "the record rode the save the fold waited for"
    );

    producer.tick(ms("2026-09-22T00:00:30.000Z") + CADENCE_MS / 2);
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_2026-09-20".to_string()],
        "the prune's records-only change waits for the cadence"
    );

    producer.tick(ms("2026-09-22T00:00:30.000Z") + CADENCE_MS + 2_000);

    assert_eq!(
        record_ids(home.home()),
        Vec::<String>::new(),
        "the prune dropped the record once its response left the tail"
    );
}

// ── the records save cadence beside a broken ledger ─────────────────────────

/// A held day outlasting a broken speed ledger no longer rewrites the
/// attribution file on every record tick: the cadence holds while no fold
/// could land, and the tick where the fold can land again saves the records
/// first and folds.
#[test]
fn a_held_day_outlasting_a_broken_ledger_saves_records_on_the_cadence_and_the_recovering_fold_saves_first()
 {
    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":99,"days":{}}"#,
    )
    .unwrap();
    day_transcript(home.home(), "2026-09-20", 7, 3);
    let stub = token_stats_proxy();
    let twins: Vec<String> = std::iter::once(user_line("2026-09-21T23:59:00.000Z"))
        .chain((1..=5).map(|i| {
            assistant_line(
                &format!("2026-09-21T23:59:1{i}.000Z"),
                &format!("msg_r{i}"),
                TRANSCRIPT_OUTPUT,
            )
        }))
        .collect();
    write_lines(&kerry_transcript(home.home()), &twins);
    let mut producer = Producer::new();
    producer.tick(ms("2026-09-21T23:59:59.000Z"));
    let writes_before = attribution_writes::taken();
    for i in 1..=5 {
        let ended = format!("2026-09-21T23:59:1{i}.000Z");
        stub.land(record(
            &format!("msg_r{i}"),
            ACCOUNT,
            "2026-09-21T23:59:05.000Z",
            &ended,
        ));
        producer.tick(ms("2026-09-22T00:00:30.000Z") + (i - 1) * 1_000);
    }
    assert!(
        attribution_writes::taken() - writes_before <= 1,
        "a broken ledger keeps the records save on its cadence: {} writes",
        attribution_writes::taken() - writes_before
    );
    drop(stub);
    drop(home);

    let home = HomeSandbox::new();
    let clauth = home.home().join(".clauth");
    day_transcript(home.home(), "2026-09-20", 7, 3);
    let stub = token_stats_proxy();
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-21T23:59:40.000Z"),
            assistant_line("2026-09-21T23:59:58.000Z", "msg_r", TRANSCRIPT_OUTPUT),
        ],
    );
    let mut producer = Producer::new();
    producer.tick(ms("2026-09-21T23:59:59.000Z"));
    stub.land(record(
        "msg_r",
        ACCOUNT,
        "2026-09-21T23:59:48.000Z",
        "2026-09-21T23:59:58.000Z",
    ));
    producer.tick(ms("2026-09-22T00:00:01.000Z"));
    let writes_before = attribution_writes::taken();
    producer.tick(ms("2026-09-22T00:00:02.000Z"));
    assert_eq!(
        attribution_writes::taken() - writes_before,
        1,
        "the tick where the fold can land saves the records first"
    );
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_r".to_string()],
        "the fold tick's save carried the record"
    );
    let folded = crate::token_ledger::SpeedLedger::load(&clauth)
        .days
        .get("2026-09-20")
        .and_then(|accounts| accounts.get("kerry"))
        .and_then(|models| models.get(TRANSCRIPT_MODEL))
        .map(|d| (d.output, d.gen_ms));
    assert_eq!(folded, Some((7, 3)), "the fold lands right after the save");
}

// ── the records save cadence and the exit save ──────────────────────────────

/// Records applied while no fold waits land on the save cadence: no tick
/// inside it rewrites the attribution file, and the first tick at its end
/// does, carrying every record applied since the last save.
#[test]
fn applied_records_with_no_fold_waiting_save_once_the_cadence_has_passed() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-22T20:46:00.000Z"),
            assistant_line("2026-09-22T20:46:10.000Z", "msg_r1", TRANSCRIPT_OUTPUT),
            assistant_line("2026-09-22T20:46:20.000Z", "msg_r2", TRANSCRIPT_OUTPUT),
        ],
    );
    stub.land(record(
        "msg_r1",
        ACCOUNT,
        "2026-09-22T20:46:01.000Z",
        "2026-09-22T20:46:10.000Z",
    ));
    let t0 = ms("2026-09-22T20:46:30.000Z");
    let mut producer = Producer::new();
    producer.tick(t0);
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_r1".to_string()],
        "a life's first save lands at once"
    );
    let writes_before = attribution_writes::taken();
    stub.land(record(
        "msg_r2",
        ACCOUNT,
        "2026-09-22T20:46:11.000Z",
        "2026-09-22T20:46:20.000Z",
    ));
    for offset in [1_000, CADENCE_MS / 2, CADENCE_MS - 1] {
        producer.tick(t0 + offset);
    }
    assert_eq!(
        attribution_writes::taken() - writes_before,
        0,
        "no tick inside the cadence rewrites the file"
    );
    assert_eq!(record_ids(home.home()), vec!["msg_r1".to_string()]);

    producer.tick(t0 + CADENCE_MS);

    assert_eq!(
        attribution_writes::taken() - writes_before,
        1,
        "the tick at the cadence's end saves once"
    );
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_r1".to_string(), "msg_r2".to_string()],
        "the save carries the record applied inside the cadence"
    );
}

/// A life's end saves the records applied inside the cadence: under a spent
/// lease ticket it writes nothing (another holder owns the file), under a
/// live one it writes once, at once, carrying every applied record, and a
/// second exit save with nothing new writes nothing.
#[test]
fn the_exit_save_lands_records_applied_inside_the_cadence_under_a_live_lease_only() {
    let home = HomeSandbox::new();
    let stub = token_stats_proxy();
    proxy_profile(&stub, "px", ACCOUNT);
    write_lines(
        &kerry_transcript(home.home()),
        &[
            user_line("2026-09-22T20:46:00.000Z"),
            assistant_line("2026-09-22T20:46:10.000Z", "msg_r1", TRANSCRIPT_OUTPUT),
            assistant_line("2026-09-22T20:46:20.000Z", "msg_r2", TRANSCRIPT_OUTPUT),
        ],
    );
    stub.land(record(
        "msg_r1",
        ACCOUNT,
        "2026-09-22T20:46:01.000Z",
        "2026-09-22T20:46:10.000Z",
    ));
    let t0 = ms("2026-09-22T20:46:30.000Z");
    let mut producer = Producer::new();
    producer.tick(t0);
    stub.land(record(
        "msg_r2",
        ACCOUNT,
        "2026-09-22T20:46:11.000Z",
        "2026-09-22T20:46:20.000Z",
    ));
    producer.tick(t0 + 1_000);
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_r1".to_string()],
        "fixture: the second record is applied but unsaved inside the cadence"
    );

    let lease = crate::daemon::FetchLease::new();
    let writes_before = attribution_writes::taken();
    producer.save_before_exit(
        t0 + 2_000,
        Some(crate::testutil::spent_lease_ticket(&lease)),
    );
    assert_eq!(
        attribution_writes::taken() - writes_before,
        0,
        "an exit save under a spent ticket writes nothing"
    );
    assert_eq!(record_ids(home.home()), vec!["msg_r1".to_string()]);

    let current = lease.acquire_ticket().expect("retake the fetch lease");
    producer.save_before_exit(t0 + 3_000, Some(current));
    assert_eq!(
        attribution_writes::taken() - writes_before,
        1,
        "the exit save under a live ticket writes once, inside the cadence"
    );
    assert_eq!(
        record_ids(home.home()),
        vec!["msg_r1".to_string(), "msg_r2".to_string()],
        "the exit save carries the record applied inside the cadence"
    );

    producer.save_before_exit(t0 + 4_000, Some(current));
    assert_eq!(
        attribution_writes::taken() - writes_before,
        1,
        "an exit save with nothing unsaved writes nothing"
    );
}
