#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Proxy-account pins, driven against a stub proxy that
//! speaks contract v1 on a loopback port. Mounted inside `usage::scheduler`
//! because the leg pins drive the private `fetch_third_party_due` and build
//! `SchedulerState` directly; the client arms they exercise live in
//! `crate::proxy_account`, `crate::cmd_login`, and `crate::cmd_delete`.
//!
//! The stub serves: a login flow reaching `done` with an inference key and a
//! distinguishable upstream secret (which must land NOWHERE under clauth's
//! home), every figure kind, a steerable usage body, capturing PATCH/DELETE,
//! and auth on every control route.

use super::*;
use crate::claude::tests::break_proxy_record;
use crate::cli::LoginArgs;
use crate::profile::{ProfileName, ProxyAccount};
use crate::proxy_account::{AwaitLogin, Control, LoginFlow, LoginOutcome};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "clp_stub-key-0";
const ACCOUNT: &str = "acct-1";
/// A byte sequence the stub's `done` body carries beside the key, standing in
/// for an upstream credential. No clauth file or log may ever hold it.
const UPSTREAM_SECRET: &str = "UPSTREAM-JWT-SECRET-P6-bytes";
const EXPIRES: &str = "2030-01-01T00:00:00Z";
/// The flow id the core's fixture proxy minted for the captured bodies below,
/// so a captured body answers for the stub's own flow verbatim.
const FLOW: &str = "f_m53cafb597";

// ── bytes captured off clauth-proxy-core's own fixture proxy ────────────────
//
// `bun test/fixtures/serve.ts serve` (its fake adapter: a poll-only login that
// completes on the first upstream poll), read with curl on 2026-10-20. The
// inference key is a fresh random value per run and is replaced by
// `CORE_KEY`, a value of the same form; every other byte is as served.

/// The key the captured `done` answer carries.
const CORE_KEY: &str = "clp_CoreFixtureKey-standsInForTheCapturedValue0";
/// The account the captured login created.
const CORE_ACCOUNT: &str = "wc5cdzkad0ps";
/// `GET /accounts/login/{flow}` before the login completed.
const CORE_PENDING: &str = r#"{"flow":"f_m53cafb597","state":"pending","poll_interval_ms":1000,"expires_at":"2026-10-20T11:54:09.127Z"}"#;
/// The first answer that saw the flow `done`: the account object, and the
/// inference key, handed out on this answer alone.
const CORE_DONE_WITH_KEY: &str = r#"{"flow":"f_m53cafb597","state":"done","account":{"id":"wc5cdzkad0ps","label":"kitty@example.com","state":"ready","created_at":"2026-10-20T11:44:10.129Z","plans":[{"id":"coding-plan","label":"coding plan"},{"id":"start-plan","label":"start plan"}],"settings":{"plan":"coding-plan","off_peak":false,"timezone":null}},"inference_key":"clp_CoreFixtureKey-standsInForTheCapturedValue0"}"#;
/// The key `POST /accounts/{id}/key` minted, standing in like [`CORE_KEY`].
const CORE_MINTED_KEY: &str = "clp_CoreFixtureMint-standsInForTheMintedValue01";
/// The key mint's answer.
const CORE_MINTED: &str = r#"{"inference_key":"clp_CoreFixtureMint-standsInForTheMintedValue01"}"#;
/// A paste into a flow that already finished `done` (2026-10-20, same
/// fixture): 409, and the next read still carries the key nobody took.
const CORE_PASTE_CONFLICT: &str = r#"{"ok":false,"error":"conflict"}"#;
/// Every later read of the same `done` flow: the account object, no key.
const CORE_DONE_LATER: &str = r#"{"flow":"f_m53cafb597","state":"done","account":{"id":"wc5cdzkad0ps","label":"kitty@example.com","state":"ready","created_at":"2026-10-20T11:44:10.129Z","plans":[{"id":"coding-plan","label":"coding plan"},{"id":"start-plan","label":"start plan"}],"settings":{"plan":"coding-plan","off_peak":false,"timezone":null}}}"#;

fn core(captured: &str) -> Value {
    serde_json::from_str(captured).expect("a captured body parses")
}

/// A `done` answer in the core's shape ([`CORE_DONE_LATER`]) naming
/// `account`, carrying `key` when it is the answer that first saw `done`.
fn done_body(account: &str, key: Option<&str>) -> Value {
    let mut body = core(CORE_DONE_LATER);
    body["account"]["id"] = json!(account);
    if let Some(key) = key {
        body["inference_key"] = json!(key);
    }
    body
}

// ── the stub proxy ──────────────────────────────────────────────────────────

struct StubState {
    admin: String,
    /// What `GET /accounts/login/{flow}` answers per poll, in order; the last
    /// one repeats.
    flow_states: Vec<Value>,
    flow_polls: usize,
    /// The `expires_at` the login start answers.
    expires_at: String,
    pastes_seen: Vec<String>,
    /// What the paste door answers: the flow's login-status after the paste.
    paste_answer: Value,
    /// The `GET /clauth/v1/usage` body; tests mutate it between legs.
    usage: Value,
    /// The `account` query every usage read carried, in order: an empty string
    /// for a whole-pool read.
    usage_queries: Vec<String>,
    patches: Vec<(String, Value)>,
    deleted: Vec<String>,
    cancelled: bool,
    requests: Vec<String>,
    /// Every request that carried an `Authorization` header, as
    /// `METHOD path` — the bearer side of the identity gate's pins.
    bearered: Vec<String>,
    /// What `GET /health` answers.
    health: Value,
    /// The `url` the login start answers.
    login_url: String,
    /// Every `POST /accounts/login` body, in order.
    login_bodies: Vec<Value>,
    /// An answer the login start gives in place of its flow.
    login_refusal: Option<String>,
    /// An answer every flow poll gives in place of its state.
    poll_refusal: Option<String>,
    /// An answer the paste door gives in place of its 204.
    paste_refusal: Option<String>,
    /// An answer the account delete gives in place of its 204.
    delete_refusal: Option<String>,
    /// A status `/health` answers with in place of its 200 body.
    health_status: Option<u16>,
    /// `/health` takes the connection and closes it unanswered.
    drop_health: bool,
    /// The account delete takes the connection and closes it unanswered.
    drop_delete: bool,
    /// Holds the first request named `METHOD path` unanswered until a send or
    /// its sender's drop, at most [`HOLD`]; every other connection is served
    /// meanwhile.
    hold: Option<(String, std::sync::mpsc::Receiver<()>)>,
    /// The request the hold took, once it arrived, answered or still held.
    held: Option<String>,
    /// The held request was let through: released, or its hold ran out.
    released: bool,
    /// An answer the flow cancel gives in place of its 204.
    cancel_refusal: Option<String>,
    /// Raw answers given in place of the poll at each 0-based index, the poll
    /// still counted ([`NO_ANSWER`] drops it after the state moved on).
    poll_answers: Vec<(usize, String)>,
    /// Every account `POST /accounts/{id}/key` minted a key for, in order.
    minted: Vec<String>,
    /// An answer the key mint gives in place of [`CORE_MINTED`].
    key_refusal: Option<String>,
    /// The `poll_interval_ms` the login start answers.
    poll_interval_ms: u64,
    /// What `/health` answers from the moment a flow poll was answered on:
    /// the bind taken over by another answerer between two requests.
    health_after_poll: Option<Value>,
    /// What `/health` answers from the moment a paste was answered on.
    health_after_paste: Option<Value>,
    /// [`Self::hold`] takes its request only once this many flow polls were
    /// answered.
    hold_from_poll: usize,
}

/// The longest the stub holds a request: under the `CONTROL_TIMEOUT` the
/// request waits, so a held request still lands.
const HOLD: Duration = Duration::from_secs(10);

/// The flow cancel's request, as [`StubState::hold`] names it.
fn cancel_request() -> String {
    format!("DELETE /clauth/v1/accounts/login/{FLOW}")
}

impl StubState {
    fn new(admin: String, usage: Value) -> Self {
        Self {
            admin,
            flow_states: vec![{
                let mut done = done_body(ACCOUNT, Some(KEY));
                done["upstream_jwt"] = json!(UPSTREAM_SECRET);
                done
            }],
            flow_polls: 0,
            expires_at: EXPIRES.to_string(),
            pastes_seen: Vec::new(),
            paste_answer: core(CORE_PENDING),
            usage,
            usage_queries: Vec::new(),
            patches: Vec::new(),
            deleted: Vec::new(),
            cancelled: false,
            requests: Vec::new(),
            bearered: Vec::new(),
            health: json!({"status": "ok", "service": "stub", "version": "0.0.1", "contract": "1.0"}),
            login_url: "https://stub.example/login".to_string(),
            login_bodies: Vec::new(),
            login_refusal: None,
            poll_refusal: None,
            paste_refusal: None,
            delete_refusal: None,
            health_status: None,
            drop_health: false,
            drop_delete: false,
            hold: None,
            held: None,
            released: false,
            cancel_refusal: None,
            poll_answers: Vec::new(),
            minted: Vec::new(),
            key_refusal: None,
            poll_interval_ms: 1000,
            health_after_poll: None,
            health_after_paste: None,
            hold_from_poll: 0,
        }
    }
}

/// What [`handle`] answers to close the connection with no reply at all.
const NO_ANSWER: &str = "";

fn reply(status: u16, body: Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn empty(status: u16) -> String {
    format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn err_body(status: u16, code: &str) -> String {
    reply(
        status,
        json!({"ok": false, "error": code, "reason": "stub refusal"}),
    )
}

fn handle(
    s: &mut StubState,
    method: &str,
    path: &str,
    query: &str,
    auth: Option<&str>,
    body: &str,
) -> String {
    if let Some(rest) = path.strip_prefix("/clauth/v1") {
        let want = format!("Bearer {}", s.admin);
        if auth != Some(want.as_str()) {
            return err_body(401, "unauthorized");
        }
        let segs: Vec<&str> = rest.split('/').filter(|seg| !seg.is_empty()).collect();
        match (method, segs.as_slice()) {
            ("GET", ["info"]) => reply(
                200,
                json!({
                    "service": "stub", "version": "0.0.1", "contract": "1.0",
                    "capabilities": [], "actions": [],
                    "figures": [
                        {"id": "plan-5h", "kind": "window", "label": "plan 5h", "chain": "5h"},
                        {"id": "credit", "kind": "balance", "label": "credit"},
                    ],
                    "settings": [
                        {"key": "plan", "scope": "account", "label": "plan", "hint": "which plan serves requests",
                         "type": "enum", "default": "coding-plan",
                         "options": [{"value": "coding-plan", "label": "coding plan"}, {"value": "start-plan", "label": "start plan"}]},
                        {"key": "off_peak", "scope": "account", "label": "off-peak", "hint": "use the off-peak channel",
                         "type": "bool", "default": false},
                    ],
                }),
            ),
            ("POST", ["accounts", "login"]) => {
                s.login_bodies
                    .push(serde_json::from_str(body).unwrap_or(Value::Null));
                if let Some(refusal) = &s.login_refusal {
                    return refusal.clone();
                }
                reply(
                    201,
                    json!({"flow": FLOW, "state": "pending", "url": s.login_url,
                           "modes": ["poll", "paste"], "poll_interval_ms": s.poll_interval_ms,
                           "expires_at": s.expires_at}),
                )
            }
            ("GET", ["accounts", "login", flow]) => {
                if *flow != FLOW {
                    return err_body(404, "not_found");
                }
                if let Some(refusal) = &s.poll_refusal {
                    return refusal.clone();
                }
                let polled = s.flow_polls;
                if let Some(health) = s.health_after_poll.take() {
                    s.health = health;
                }
                if let Some((_, answer)) = s.poll_answers.iter().find(|(at, _)| *at == polled) {
                    let answer = answer.clone();
                    s.flow_polls += 1;
                    return answer;
                }
                let state = if s.flow_polls < s.flow_states.len() {
                    s.flow_states[s.flow_polls].clone()
                } else {
                    s.flow_states.last().cloned().unwrap_or(Value::Null)
                };
                s.flow_polls += 1;
                reply(200, state)
            }
            ("POST", ["accounts", "login", flow]) => {
                if *flow != FLOW {
                    return err_body(404, "not_found");
                }
                let code = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                s.pastes_seen.push(code);
                if let Some(health) = s.health_after_paste.take() {
                    s.health = health;
                }
                if let Some(refusal) = &s.paste_refusal {
                    return refusal.clone();
                }
                reply(200, s.paste_answer.clone())
            }
            ("DELETE", ["accounts", "login", flow]) => {
                if *flow != FLOW {
                    return err_body(404, "not_found");
                }
                if let Some(refusal) = &s.cancel_refusal {
                    return refusal.clone();
                }
                s.cancelled = true;
                empty(204)
            }
            ("POST", ["accounts", id, "key"]) => {
                s.minted.push(id.to_string());
                match &s.key_refusal {
                    Some(refusal) => refusal.clone(),
                    None => reply(200, core(CORE_MINTED)),
                }
            }
            ("PATCH", ["accounts", id]) => {
                let Ok(settings) = serde_json::from_str::<Value>(body) else {
                    return err_body(400, "bad_request");
                };
                s.patches.push((id.to_string(), settings.clone()));
                reply(
                    200,
                    json!({"id": id, "settings": settings.get("settings").cloned().unwrap_or(json!({}))}),
                )
            }
            ("DELETE", ["accounts", id]) => {
                if *id != ACCOUNT {
                    return err_body(404, "not_found");
                }
                if s.drop_delete {
                    return NO_ANSWER.to_string();
                }
                if let Some(refusal) = &s.delete_refusal {
                    return refusal.clone();
                }
                s.deleted.push(id.to_string());
                empty(204)
            }
            ("GET", ["usage"]) => {
                let want = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("account="))
                    .unwrap_or_default();
                s.usage_queries.push(want.to_string());
                if !want.is_empty() && want != ACCOUNT {
                    return err_body(404, "not_found");
                }
                reply(200, s.usage.clone())
            }
            _ => err_body(404, "not_found"),
        }
    } else if method == "GET" && path == "/health" {
        if s.drop_health {
            NO_ANSWER.to_string()
        } else if let Some(status) = s.health_status {
            empty(status)
        } else {
            reply(200, s.health.clone())
        }
    } else {
        err_body(404, "not_found")
    }
}

struct Stub {
    base: String,
    port: u16,
    state: Arc<Mutex<StubState>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stub {
    fn start(admin: String, usage: Value) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("local addr").port();
        let base = format!("http://127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(StubState::new(admin, usage)));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (Arc::clone(&state), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    // A connection of its own per request, so a held one
                    // blocks no other: a client that stops waiting on a held
                    // request is served its next one at once.
                    Ok((sock, _)) => {
                        let shared = Arc::clone(&shared);
                        std::thread::spawn(move || serve_one(sock, &shared));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            base,
            port,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, StubState> {
        self.state.lock().expect("stub state")
    }

    /// Stop accepting and join the serve thread; the Drop runs the same for a
    /// stub that never stopped.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
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

fn serve_one(mut sock: TcpStream, state: &Mutex<StubState>) {
    sock.set_nonblocking(false).ok();
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        match sock.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => req.extend_from_slice(&tmp[..n]),
        }
        if let Some(h) = req.windows(4).position(|w| w == b"\r\n\r\n") {
            break h;
        }
    };
    let head = String::from_utf8_lossy(&req[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (
        first.next().unwrap_or("").to_string(),
        first.next().unwrap_or("").to_string(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while req.len() < head_end + 4 + len {
        match sock.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => req.extend_from_slice(&tmp[..n]),
        }
    }
    let body = String::from_utf8_lossy(&req[head_end + 4..]).into_owned();
    let auth = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.clone());
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let request = format!("{method} {path}");
    // Taken out of the state first, so the hold blocks no other reader.
    let hold = {
        let mut state = state.lock().expect("stub state");
        let from_poll = state.hold_from_poll;
        match state.hold.take() {
            Some((want, release)) if want == request && state.flow_polls >= from_poll => {
                state.held = Some(request);
                Some(release)
            }
            other => {
                state.hold = other;
                None
            }
        }
    };
    let released = hold.map(|release| {
        let _ = release.recv_timeout(HOLD);
    });
    let mut state = state.lock().expect("stub state");
    if released.is_some() {
        state.released = true;
    }
    state.requests.push(format!("{method} {path}"));
    if auth.is_some() {
        state.bearered.push(format!("{method} {path}"));
    }
    let out = handle(&mut state, &method, path, query, auth.as_deref(), &body);
    let _ = sock.write_all(out.as_bytes());
    let _ = sock.shutdown(std::net::Shutdown::Both);
}

// ── fixtures ────────────────────────────────────────────────────────────────

/// The full usage body: a chain-5h window at `window_used` of 4M tokens, a
/// currency balance at `balance_remaining`, one figure of every other kind,
/// and one kind clauth does not know, which only carries a summary.
fn usage_body(window_used: f64, balance_remaining: f64) -> Value {
    json!({
        "accounts": [{
            "account": ACCOUNT, "state": "ready", "available": true,
            "figures": [
                {"kind": "window", "id": "plan-5h", "label": "plan 5h", "used": window_used,
                 "limit": 4_000_000.0, "unit": "tokens", "window_secs": 18000,
                 "resets_at": "2030-01-01T05:00:00Z", "chain": "5h",
                 "read_at": "2030-01-01T00:00:00Z"},
                {"kind": "balance", "id": "credit", "label": "credit", "remaining": balance_remaining,
                 "limit": 1_000_000.0, "used": 1_000_000.0 - balance_remaining, "unit": "currency",
                 "currency": "CNY", "read_at": "2030-01-01T00:00:00Z"},
                {"kind": "channel", "id": "off-peak", "label": "off-peak", "open": false,
                 "next_open_at": "2030-01-01T06:00:00Z", "read_at": "2030-01-01T00:00:00Z"},
                {"kind": "offer", "id": "trials", "label": "trials", "read_at": "2030-01-01T00:00:00Z",
                 "items": [{"id": "weekend", "label": "weekend plan", "state": "available",
                            "grants": [{"label": "tokens", "amount": 5_000_000.0, "unit": "tokens"}]}]},
                {"kind": "stats", "id": "traffic", "label": "traffic", "since": "2030-01-01T00:00:00Z",
                 "requests": {"attempted": 3, "succeeded": 3, "failed": 0, "cancelled": 0},
                 "mean_latency_ms": 840,
                 "tokens": {"input": 210, "output": 48, "cache_read": 900, "cache_creation": 30},
                 "read_at": "2030-01-01T00:00:00Z"},
                {"kind": "quantum-flux", "id": "mystery", "label": "mystery",
                 "summary": "7 qubits committed", "read_at": "2030-01-01T00:00:00Z"},
            ],
        }]
    })
}

fn register_proxy_row(port: u16) {
    let path = crate::proxy::registry_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("[stub]\nport = {port}\nenabled = true\n")).unwrap();
}

/// Mint the admin token the way `clauth proxy enable` does; the caller hands
/// the value to the stub, whose control routes then demand it.
fn mint_admin_token() -> String {
    crate::proxy::ensure_proxy_token(&crate::proxy::Service::parse("stub").unwrap())
        .unwrap()
        .expose()
        .to_string()
}

/// The proxy profile `name` on the stub's bind, on disk the way
/// `clauth login --proxy` leaves it: key in config.toml, `[proxy]` record
/// beside it, roster entry saved. The caller's in-memory `Profile` carries
/// the record as a load would read it.
fn create_proxy_profile_on_disk(config: &mut crate::profile::AppConfig, name: &str, base: &str) {
    let mut profile = crate::profile::Profile::new(
        name.to_string(),
        Some(base.to_string()),
        Some(KEY.to_string()),
    );
    profile.fallback_threshold = Some(95.0);
    crate::profile::save_profile(&profile).expect("save profile");
    let record = ProxyAccount {
        service: crate::proxy::Service::parse("stub").unwrap(),
        account: ACCOUNT.to_string(),
    };
    crate::profile::write_proxy_record(&profile.name, &record).expect("write the proxy record");
    profile.proxy = Some(record);
    config.add(profile);
    crate::profile::save_app_state(&config.state).expect("save state");
}

/// The scheduler state the leg pins drive, its fetcher the REAL
/// `fetch_third_party_usage` so every request hits the stub.
fn leg_state() -> SchedulerState {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    SchedulerState {
        config: Arc::new(RankedMutex::new(crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: vec![],
        })),
        tokens: Arc::new(RankedMutex::new(vec![])),
        store: Arc::new(RankedMutex::new(HashMap::new())),
        status: Arc::new(RankedMutex::new(HashMap::new())),
        refresh_interval: Arc::new(AtomicU64::new(90_000)),
        next_refresh_per_profile: Arc::new(RankedMutex::new(HashMap::new())),
        activity: Arc::new(RankedMutex::new(HashMap::new())),
        last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
        poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
        weekly_reset_kicks: Arc::new(RankedMutex::new(HashSet::new())),
        auto_start_queue: crate::usage::new_auto_start_queue_state(),
        pending_switch: Arc::new(RankedMutex::new(HashSet::new())),
        pending_switch_off: Arc::new(RankedMutex::new(false)),
        refetch_queue: Arc::new(RankedMutex::new(HashSet::new())),
        third_party_tokens: Arc::new(RankedMutex::new(vec![])),
        third_party_usage_store: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_streaks: Arc::new(RankedMutex::new(HashMap::new())),
        third_party_broken: Arc::new(RankedMutex::new(HashMap::new())),
        skipped_unreadable: Arc::new(RankedMutex::new(HashSet::new())),
        suppressed_auth_expired: Arc::new(RankedMutex::new(HashMap::new())),
        shutting_down: Arc::new(AtomicBool::new(false)),
        fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
        display_reader: false,
        last_history_prune: AtomicU64::new(now_ms()),
        claude_rolling: crate::lockorder::RankedMutex::new(ClaudeRollingPacing::default()),
        claude_resets_memo: new_claude_resets_memo(),
        fetcher: crate::providers::fetch_third_party_usage,
        // No reload compare has seen a name yet, so every entry reads as
        // fresh (a name with no snapshot half is never stale): the pins drive
        // profiles that were never forgotten.
        snapshot: Arc::new(RankedMutex::new(HashMap::new())),
    }
}

/// Drive one third-party leg for `config`'s proxy profiles against the stub.
fn run_leg(state: &SchedulerState, config: &crate::profile::AppConfig) {
    let mut entries = collect_third_party_entries(&config.profiles);
    // Stamped as the tick's partition stamps it: the epoch map outlives every
    // test in one process, so an entry left at 0 lands nothing once any earlier
    // test forgot the same name.
    for entry in &mut entries {
        entry.epoch_at_spawn =
            crate::usage::epoch_value(&entry.name, crate::usage::Half::ThirdParty);
    }
    fetch_third_party_due(state, entries);
}

/// Every file under `dir`, as paths.
fn walk_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn no_file_under_home_holds(home: &std::path::Path, needle: &str) -> bool {
    let mut files = Vec::new();
    walk_files(home, &mut files);
    files.iter().all(|path| {
        std::fs::read(path)
            .map(|bytes| !String::from_utf8_lossy(&bytes).contains(needle))
            .unwrap_or(true)
    })
}

// ── pin 1: login --proxy ────────────────────────────────────────────────────

/// `clauth login px --proxy stub` creates the profile with the bind, the key
/// and the record; the stub's upstream-secret bytes appear NOWHERE under the
/// sandboxed clauth home or the logs the login raised.
#[test]
fn login_creates_the_profile_with_the_key_and_the_upstream_secret_lands_nowhere() {
    let home = crate::testutil::HomeSandbox::new();
    register_proxy_row(0);
    let admin = mint_admin_token();
    let stub = Stub::start(admin.clone(), usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);

    let logs = crate::logline::LogLines::new();
    let _capture = logs.capture_here();
    let opened = crate::platform::OpenedUrls::capture_here();
    crate::cmd_login(proxy_login("px", None)).expect("the proxy login runs");
    drop(_capture);
    assert_eq!(
        opened.urls(),
        vec!["https://stub.example/login".to_string()],
        "the flow's page reaches the opener seam, never a real browser"
    );

    let config = crate::profile::load_config().expect("config");
    let profile = config
        .find(&ProfileName::from("px"))
        .expect("profile created");
    assert_eq!(profile.base_url.as_deref(), Some(stub.base.as_str()));
    assert_eq!(
        profile.api_key.as_deref(),
        Some(KEY),
        "the inference key is the profile's api key"
    );
    let name = ProfileName::from("px");
    let record = crate::profile::stored_proxy_record(&name).expect("the record on disk");
    assert_eq!(
        (record.service.as_str(), record.account.as_str()),
        ("stub", ACCOUNT)
    );
    let config_text =
        std::fs::read_to_string(crate::profile::profile_subpath(&name, "config.toml").unwrap())
            .unwrap();
    assert!(
        config_text.contains("[proxy]"),
        "the record is in config.toml"
    );
    // The on-disk round-trip: a reload that rewrites the file (a field edit)
    // carries the unmodelled record rather than dropping it.
    let mut reloaded = crate::profile::load_profile(&name).expect("reload");
    reloaded.fallback_threshold = Some(90.0);
    crate::profile::save_profile(&reloaded).expect("rewrite config.toml");
    let record =
        crate::profile::stored_proxy_record(&name).expect("the record survives the rewrite");
    assert_eq!(record.account, ACCOUNT);

    assert!(
        no_file_under_home_holds(home.home(), UPSTREAM_SECRET),
        "the stub's upstream-secret bytes must appear nowhere under the clauth home"
    );
    assert!(
        !logs
            .snapshot()
            .iter()
            .any(|line| line.contains(UPSTREAM_SECRET)),
        "and nowhere in the logs the login raised"
    );
    assert!(stub.state().deleted.is_empty(), "a login deletes nothing");
}

/// The paste door submits the code the caller holds; a flow that failed or
/// expired is cancelled, so no run leaves one polling upstream.
#[test]
fn the_paste_door_submits_and_a_failed_flow_is_cancelled() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let control = stub_control();

    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
    assert_eq!(flow.start().flow, FLOW);
    assert_eq!(flow.start().url, "https://stub.example/login");
    crate::proxy_account::submit_paste(&control, &flow.start().flow, "paste-code-9")
        .expect("the paste is accepted");
    assert_eq!(stub.state().pastes_seen, vec!["paste-code-9".to_string()]);

    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "failed"})];
    let err = await_quietly(flow).expect_err("a failed flow refuses");
    assert!(err.to_string().contains("failed"), "{err}");
    assert!(stub.state().cancelled, "the failed flow is cancelled");

    // A flow whose expiry has passed refuses the same way, without polling forever.
    stub.state().expires_at = "2020-01-01T00:00:00Z".to_string();
    stub.state().cancelled = false;
    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let _ = await_quietly(flow);
    assert!(stub.state().cancelled, "the expired flow is cancelled");
}

/// The done body is read for the account id and the key alone: an outcome
/// holding a key redacts it through its Debug, a re-bind done body that names
/// no key reads as none, and neither the key nor the body's upstream secret
/// survives into the Debug.
#[test]
fn a_rebind_flow_keeps_the_stored_key_when_the_done_body_names_none() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let control = stub_control();

    // A done body carrying the key (and the stub's upstream secret beside it).
    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
    let outcome: LoginOutcome = await_quietly(flow).expect("done");
    assert_eq!(outcome.account, ACCOUNT);
    let debug = format!("{outcome:?}");
    assert!(debug.contains("Secret(<redacted>)"), "{debug}");
    assert!(!debug.contains(KEY), "{debug}");
    assert!(!debug.contains(UPSTREAM_SECRET), "{debug}");

    // A re-bind done body that names no key: none read, none invented.
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let flow = crate::proxy_account::start_login(&control, Some(ACCOUNT), &mut || None)
        .expect("the re-bind opens");
    let outcome: LoginOutcome = await_quietly(flow).expect("done");
    assert_eq!(outcome.account, ACCOUNT);
    assert!(
        outcome.inference_key.is_none(),
        "no key in the body, none read"
    );
}

/// A login against the core proxy's own bytes: its `done` answer names the
/// account as the account OBJECT (the contract's login-status `account`), and
/// the key rides the first answer that saw `done` alone. The profile lands on
/// the object's `id` with that key.
#[test]
fn a_login_reads_the_core_proxys_own_done_answers() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![
        core(CORE_PENDING),
        core(CORE_DONE_WITH_KEY),
        core(CORE_DONE_LATER),
    ];

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");

    assert_eq!(
        line,
        "clauth: captured into profile 'px' (proxy \"stub\", account \"wc5cdzkad0ps\"). Switch \
         to it with:  clauth px"
    );
    let name = ProfileName::from("px");
    let config = crate::profile::load_config().expect("config");
    assert_eq!(
        config
            .find(&name)
            .expect("profile created")
            .api_key
            .as_deref(),
        Some(CORE_KEY)
    );
    let record = crate::profile::stored_proxy_record(&name).expect("the record on disk");
    assert_eq!(record.account, CORE_ACCOUNT);
    assert_eq!(
        stub.state().flow_polls,
        2,
        "pending, then the done with its key"
    );
}

/// The control client for the stub, whose row and token the caller set.
fn stub_control() -> Control {
    Control::connect(&stub_service()).expect("the stub's row and token")
}

fn stub_service() -> crate::proxy::Service {
    crate::proxy::Service::parse("stub").unwrap()
}

/// Drive `flow` to its end with the paste door closed, no interrupt, a wait
/// that never sleeps, and a re-bind keeping the key its profile stores.
fn await_quietly(flow: LoginFlow<'_>) -> anyhow::Result<LoginOutcome> {
    let closed = std::sync::mpsc::channel().1;
    flow.await_done(AwaitLogin {
        paste: &closed,
        interrupted: &mut || None,
        profile: "px",
        wait: &|_| {},
        say: &|_| {},
        key_stored: &|| true,
        holder: &|_| None,
    })
}

// ── pin 2: the 5h window drives the chain walk ─────────────────────────────

/// The stub's chain-5h window, fetched through the real leg, is the window the
/// chain walk judges: at 100% the member is excluded from the walk, at 50% it
/// is picked. Windowless (the pre-proxy-arm shape) reads as headroom, which is
/// exactly the defect this arm exists to close.
#[test]
fn a_5h_window_at_100_percent_excludes_the_member_from_the_chain_walk() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(4_000_000.0, 100.0));
    register_proxy_row(stub.port);

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    config.state.fallback_chain = vec!["active".into(), "px".into()];
    config.state.active_profile = Some(ProfileName::from("active"));

    let mut active = crate::profile::Profile::new("active".to_string(), None, None);
    active.fallback_threshold = Some(95.0);
    config.add(active);
    create_proxy_profile_on_disk(&mut config, "px", &stub.base);

    let state = leg_state();
    crate::usage::reset_request_slots();

    // The active is exhausted, so the walk must look for somewhere to go.
    state.store.lock().unwrap().insert(
        "active".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some("2030-01-01T05:00:00Z".to_string()),
            }),
            ..Default::default()
        },
    );

    // The stub serves 100%: the fetched window is the one the walk judges...
    run_leg(&state, &config);
    let (snap, usage, names) = walk_view(&state, &config);
    let px_window = usage
        .get("px")
        .and_then(|usage| usage.five_hour.clone())
        .expect("the fetched window feeds the walk");
    assert_eq!(px_window.utilization, 100.0);
    // ...and the walk must NOT land on the spent member.
    assert_eq!(
        crate::fallback::next_auto_switch_target_with_usage(
            &snap,
            &usage,
            &names,
            crate::fallback::ActiveBurnRate::FromDisk,
            false,
            false,
        ),
        None,
        "a 5h window at 100% excludes the member: the walk parks instead of switching into it"
    );

    // The control: the same member at 50% is picked — the exclusion above came
    // from the fetched figure, not from anything else about the profile.
    stub.state().usage = usage_body(2_000_000.0, 100.0);
    crate::usage::reset_request_slots();
    run_leg(&state, &config);
    let (snap, usage, names) = walk_view(&state, &config);
    let px_window = usage
        .get("px")
        .and_then(|usage| usage.five_hour.clone())
        .expect("the refreshed window");
    assert_eq!(px_window.utilization, 50.0);
    assert_eq!(
        crate::fallback::next_auto_switch_target_with_usage(
            &snap,
            &usage,
            &names,
            crate::fallback::ActiveBurnRate::FromDisk,
            false,
            false,
        ),
        Some(crate::fallback::SwitchAction::To("px".to_string())),
        "the same member with headroom is picked"
    );
}

/// The chain walk's own view of the stores: the snapshot fill
/// `scan_auto_switch` runs, so a member's window here is the one the walk
/// judges — a third-party member's derived from the provider store.
fn walk_view(
    state: &SchedulerState,
    config: &crate::profile::AppConfig,
) -> (
    crate::fallback::ChainSnapshot,
    HashMap<String, crate::usage::UsageInfo>,
    HashSet<ProfileName>,
) {
    let mut snap =
        crate::fallback::snapshot_chain(config, &HashSet::new()).expect("a chain snapshot");
    let names: HashSet<ProfileName> = config
        .profiles
        .iter()
        .filter(|p| p.usage_cache_is_third_party())
        .map(|p| p.name.clone())
        .collect();
    let usage = fill_chain_snapshot_stores(
        &mut snap,
        &state.store,
        &state.third_party_usage_store,
        &state.status,
        &state.third_party_status,
        &state.poll_streaks,
        &state.third_party_streaks,
        &state.kick_blocks,
        &names,
        &polled_third_party_names(config),
    );
    (snap, usage, names)
}

// ── pin 3: the moving figure's burn rate ───────────────────────────────────

/// The stub's balance moves 100.00 → 95.00 → 90.00 CNY across three landings —
/// the two moving polls the floor's "two real changes" admits, after a first
/// landing seeds the series. The wallet rate is read at an injected instant
/// that makes the fit's points exactly collinear, so the expected figure is
/// exact arithmetic (derived beside the assertion).
#[test]
fn two_moving_polls_give_the_hand_computed_wallet_burn_rate() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", &stub.base);

    let state = leg_state();
    crate::usage::reset_request_slots();

    // The per-host request spacing is 5 s; reset it before each leg so the
    // series spans are the sleeps this test controls, not the limiter's.
    crate::usage::reset_request_slots();
    run_leg(&state, &config); // seeds the series at 100.00
    stub.state().usage = usage_body(500_000.0, 95.0);
    std::thread::sleep(Duration::from_millis(250));
    crate::usage::reset_request_slots();
    run_leg(&state, &config); // moving poll 1: -5.00
    stub.state().usage = usage_body(500_000.0, 90.0);
    std::thread::sleep(Duration::from_millis(250));
    crate::usage::reset_request_slots();
    run_leg(&state, &config); // moving poll 2: -5.00

    let name = ProfileName::from("px");
    let series = crate::profile::load_wallet_history(&name);
    assert_eq!(
        series.len(),
        5,
        "one line per landing plus a bridge per change: {series:?}"
    );
    assert_eq!(
        (
            series[0].amount,
            series[1].amount,
            series[3].amount,
            series[4].amount
        ),
        (100.0, 100.0, 95.0, 90.0),
        "the known deltas landed, bridges included"
    );
    // The series is (t0,100) (t1-1,100) (t1,95) (t2-1,95) (t2,90): each bridge
    // stamps the previous amount 1 ms before the moving reading.
    let (t1, t2) = (series[2].ts, series[4].ts);
    assert_eq!(
        (series[1].ts, series[2].amount, series[3].ts),
        (t1 - 1, 95.0, t2 - 1),
        "the bridges sit 1 ms before each moving reading"
    );
    // Read the balance at now = 2*t2 - t1 - 1. The fit appends (now, 90), and
    // dedup keeps each flat run's NEWEST stamp: (t1-1,100) (t2-1,95) (now,90).
    // With d = t2 - t1 the x steps are (t2-1)-(t1-1) = d and now-(t2-1) = d, and
    // y falls 5 per step, so the points are collinear and any weighting fits
    // slope -5/d per ms exactly: 5/d * 86_400_000 = 432_000_000/d CNY/day.
    // Nothing is cut: no equal pair spans the 6 h gap cut, all of it sits in
    // the 24 h lookback, and the amount never rises.
    let d = t2 - t1;
    let now = 2 * t2 - t1 - 1;
    let expected_per_day = 432_000_000.0 / d as f64;

    let rows = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the cached rows")
    .rows;
    let rate =
        crate::usage::burn::funded_wallet_rate_at(&series, &rows, now).expect("the rate derives");
    assert_eq!(rate.currency, "CNY");
    assert_eq!(rate.amount, 90.0);
    let got = rate.per_day;
    // Float rounding only: the weighted sums carry a few ulps of error, many
    // orders below what a changed fit moves.
    assert!(
        (got - expected_per_day).abs() <= expected_per_day * 1e-12,
        "hand-computed {expected_per_day} CNY/day, got {got}"
    );
}

// ── pin 5: delete sends the DELETE, removes the dir whole; a dead proxy deletes nothing ──

/// With the stub up, `clauth delete px` sends `DELETE /accounts/{id}` and
/// removes the profile like any delete: the whole profile dir goes, its usage
/// and wallet series with it, while the token counts in the global token
/// ledger stay byte for byte. Nothing of `px` is left behind, so a rename onto
/// the name succeeds and a new login under it runs.
#[test]
fn delete_sends_the_delete_removes_the_dir_whole_and_keeps_the_token_ledger() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 90.0));
    register_proxy_row(stub.port);

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", &stub.base);
    let name = ProfileName::from("px");
    let t = now_ms();
    crate::profile::append_wallet_readings_at(&name, &cached_stats(90.0), t);
    crate::profile::append_usage_sample(&name, None, &crate::usage::UsageInfo::default());
    let dir = crate::profile::profile_dir(&name).unwrap();
    assert!(
        dir.join("usage_history.jsonl").exists() && dir.join("wallet_history.jsonl").exists(),
        "the fixture's two series are on disk before the delete"
    );
    // The account's token counts: one finalized day in the global ledger.
    let clauth_dir = crate::profile::clauth_dir().unwrap();
    let mut ledger = crate::token_ledger::Ledger::default();
    let mut base = crate::tokens::TokenStats::default();
    let split = crate::tokens::ModelTokens {
        model: "claude-opus-4-8".to_string(),
        input: 7,
        output: 3,
        cache_read: 100,
        cache_create: 20,
        fast_input: 0,
        fast_output: 0,
        fast_cache_read: 0,
        fast_cache_create: 0,
        shape: Default::default(),
    };
    base.daily_models.push(crate::tokens::DayModelTokens {
        date: "2026-06-16".to_string(),
        model: split.model.clone(),
        in_out: 10,
        split: Some(split),
        hours: None,
    });
    assert!(
        ledger.record_covering(&base, "2026-06-18", true),
        "the day records"
    );
    ledger.save(&clauth_dir);
    let ledger_path = clauth_dir.join("token_ledger.json");
    let ledger_before = std::fs::read(&ledger_path).expect("the ledger landed");

    crate::cmd_delete("px", true, false).expect("the delete runs");

    assert_eq!(
        stub.state().deleted,
        vec![ACCOUNT.to_string()],
        "the DELETE landed"
    );
    let mut config = crate::profile::load_config().unwrap();
    assert!(
        config.find(&name).is_none(),
        "the profile is gone from the roster"
    );
    assert!(
        !dir.exists(),
        "the profile dir goes whole, its usage and wallet series with it"
    );
    assert_eq!(
        std::fs::read(&ledger_path).ok(),
        Some(ledger_before),
        "the token counts survive the delete byte for byte"
    );

    // A rename onto the freed name succeeds, as after any delete.
    let other = ProfileName::from("other");
    crate::actions::create_blank_profile(
        &mut config,
        "other".to_string(),
        Some("https://other.example".to_string()),
        Some("sk-other".to_string()),
        crate::profile::ModelSettings::default(),
        None,
    )
    .expect("create other");
    let guard = crate::actions::rotation_guard_for_mutation(&other).unwrap();
    crate::actions::rename_profile(&mut config, &other, &name, &guard)
        .expect("a rename onto the deleted name succeeds");
    drop(guard);
    assert_eq!(
        crate::profile::load_config()
            .unwrap()
            .find(&name)
            .and_then(|p| p.base_url.clone())
            .as_deref(),
        Some("https://other.example"),
        "the name now holds the renamed profile"
    );
    let guard = crate::actions::rotation_guard_for_mutation(&name).unwrap();
    crate::actions::rename_profile(&mut config, &name, &other, &guard).expect("and back");
    drop(guard);

    // A new login under the freed name runs, as after any delete.
    crate::cmd_login(proxy_login("px", None)).expect("a new login under the name runs");
}

fn cached_stats(balance: f64) -> crate::providers::ThirdPartyStats {
    crate::providers::ThirdPartyStats {
        is_available: true,
        rows: vec![crate::providers::StatRow {
            label: crate::providers::DEEPSEEK_BALANCE_ROW_LABEL.to_string(),
            value: format!("{balance} CNY"),
            kind: crate::providers::StatRowKind::Body,
            raw: None,
        }],
        bars: vec![],
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

/// With the stub down, the delete refuses naming the proxy and deletes
/// NOTHING; the warm figures the last good poll left render stale — the cache
/// keeps its value and its age — never zero, never emptied.
#[test]
fn with_the_stub_down_nothing_is_deleted_and_figures_go_stale_never_zero() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin_token = mint_admin_token();
    let mut stub = Stub::start(admin_token, usage_body(2_000_000.0, 90.0));
    register_proxy_row(stub.port);

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", &stub.base);
    let name = ProfileName::from("px");
    let dir = crate::profile::profile_dir(&name).unwrap();

    // A warm reading from a live poll.
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);
    assert!(matches!(
        state.third_party_status.lock().unwrap().get("px"),
        Some(FetchStatus::Fresh)
    ));
    let cache_path = crate::profile_cache::profile_cache_path(
        &name,
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("cache path");
    let warm_cache = std::fs::read(&cache_path).unwrap();
    let deleted_before = stub.state().deleted.clone();
    stub.stop(); // the proxy is now unreachable

    let err = crate::cmd_delete("px", true, false).expect_err("the delete refuses");
    let message = format!("{err:#}");
    assert!(
        message.contains("stub"),
        "the refusal names the proxy: {message}"
    );
    assert!(message.contains("Nothing was deleted"), "{message}");
    assert_eq!(deleted_before, Vec::<String>::new());
    let config = crate::profile::load_config().unwrap();
    assert!(config.find(&name).is_some(), "the profile stays");
    assert!(dir.join("config.toml").exists(), "nothing local moved");

    // The scheduled poll against the dead proxy: the figure goes STALE —
    // last value + old cache, status Cached — never zero, never emptied.
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);
    assert!(
        matches!(
            state.third_party_status.lock().unwrap().get("px"),
            Some(FetchStatus::Cached)
        ),
        "a failing read reads as stale"
    );
    assert_eq!(
        std::fs::read(&cache_path).unwrap(),
        warm_cache,
        "the cache keeps its last value byte-for-byte"
    );
    // The walk derives a third-party member's windows from the provider
    // store, which the failed poll cold-filled from the cache.
    let window = state
        .third_party_usage_store
        .lock()
        .unwrap()
        .get("px")
        .and_then(crate::providers::ThirdPartyStats::to_usage_info)
        .and_then(|usage| usage.five_hour)
        .expect("the window survives the failed poll");
    assert_eq!(window.utilization, 50.0, "stale, not zero");
}

// ── the settings engine ────────────────────────────────────────────────────

/// A change validated against its declared type reaches the stub as one PATCH
/// body equal to the change; a wrong-typed change and an unknown key are
/// refused client-side, before any request.
#[test]
fn a_setting_change_is_validated_then_reaches_the_stub() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let service = crate::proxy::Service::parse("stub").unwrap();

    // Client-side refusal: wrong type, unknown key — no request leaves.
    crate::proxy_account::apply_account_settings(
        &service,
        ACCOUNT,
        &[("off_peak".to_string(), json!("yes"))],
    )
    .expect_err("a wrong-typed change is refused");
    crate::proxy_account::apply_account_settings(
        &service,
        ACCOUNT,
        &[("no_such_knob".to_string(), json!(true))],
    )
    .expect_err("an unknown key is refused");
    assert_eq!(
        stub.state()
            .requests
            .iter()
            .filter(|r| r.starts_with("PATCH"))
            .count(),
        0,
        "a refused change sends nothing"
    );

    // The valid change lands as one PATCH whose settings are exactly the change.
    crate::proxy_account::apply_account_settings(
        &service,
        ACCOUNT,
        &[("plan".to_string(), json!("start-plan"))],
    )
    .expect("the change lands");
    let patches = stub.state().patches.clone();
    assert_eq!(
        patches,
        vec![(
            ACCOUNT.to_string(),
            json!({"settings": {"plan": "start-plan"}})
        )],
    );

    // The declared schema resolves, account scope only, types intact.
    let declared = crate::proxy_account::declared_account_settings(&service).expect("declared");
    let keys: Vec<&str> = declared.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(keys, vec!["plan", "off_peak"]);
    assert_eq!(declared[0].typ, "enum");
    assert_eq!(declared[1].typ, "bool");
}

// ── the login route, the identity gate, refusals ────────────────────────────

/// `clauth login <name> --proxy stub`'s arguments.
fn proxy_login(name: &str, model: Option<&str>) -> LoginArgs {
    LoginArgs {
        profile: name.to_string(),
        base_url: None,
        api_key: None,
        setup_token: false,
        codex: false,
        browser: false,
        proxy: Some("stub".to_string()),
        yes: false,
        model: model.map(str::to_string),
    }
}

/// A sandbox holding the proxy profile `px` on a live stub: the row, the
/// token, the profile with its record and the old key [`KEY`].
fn proxy_profile_on_stub(
    usage: Value,
) -> (
    crate::testutil::HomeSandbox,
    Stub,
    crate::profile::AppConfig,
) {
    proxy_profile_on_stub_built(|| usage)
}

/// [`proxy_profile_on_stub`] with the usage body built only once the sandbox
/// is held, so a body stamped from the clock is not aged by the wait for the
/// home test lock.
fn proxy_profile_on_stub_built(
    usage: impl FnOnce() -> Value,
) -> (
    crate::testutil::HomeSandbox,
    Stub,
    crate::profile::AppConfig,
) {
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage());
    register_proxy_row(stub.port);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", &stub.base);
    (home, stub, config)
}

fn config_bytes(name: &str) -> Vec<u8> {
    std::fs::read(crate::profile::profile_subpath(&ProfileName::from(name), "config.toml").unwrap())
        .unwrap()
}

/// A same-service re-login re-binds the account in place through the same
/// entry the new-name login takes: the start names the stored account, the
/// record keeps its id, and the re-minted key replaces the old one.
#[test]
fn a_proxy_relogin_rebinds_in_place_and_writes_the_new_key() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body(ACCOUNT, Some("clp_stub-key-1"))];

    crate::cmd_login(proxy_login("px", None)).expect("the re-login runs");

    assert_eq!(
        stub.state().login_bodies,
        vec![json!({"account": ACCOUNT})],
        "the start re-binds the stored account"
    );
    let name = ProfileName::from("px");
    let profile = crate::profile::load_profile(&name).expect("the profile stays");
    assert_eq!(profile.api_key.as_deref(), Some("clp_stub-key-1"));
    assert_eq!(profile.base_url.as_deref(), Some(stub.base.as_str()));
    assert_eq!(
        crate::profile::stored_proxy_record(&name),
        Some(ProxyAccount {
            service: stub_service(),
            account: ACCOUNT.to_string(),
        })
    );
}

/// `--model` on a proxy re-login sets the profile's default model, the way
/// the standard re-login applies it.
#[test]
fn a_proxy_relogin_applies_the_model_override() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];

    crate::cmd_login(proxy_login("px", Some("glm-5.1"))).expect("the re-login runs");

    let profile = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(profile.models.default.as_deref(), Some("glm-5.1"));
    assert_eq!(
        profile.api_key.as_deref(),
        Some(KEY),
        "no key re-minted, the old one stays"
    );
}

/// A re-bind the proxy completes onto ANOTHER account id is refused: the
/// record and the key stay as they were.
#[test]
fn a_relogin_landing_on_another_account_is_refused_and_changes_nothing() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body("acct-2", Some("clp_stub-key-2"))];
    let before = config_bytes("px");

    let err = crate::cmd_login(proxy_login("px", None)).expect_err("a foreign re-bind refuses");

    assert_eq!(
        err.to_string(),
        "proxy \"stub\" logged 'px' in as account \"acct-2\", not its own account \"acct-1\"; \
         nothing was changed. Run `clauth proxy check stub` for details"
    );
    assert_eq!(config_bytes("px"), before, "record and key unchanged");
}

/// An existing name on another proxy's record, or with no record at all, is
/// refused before any request, and nothing on disk moves.
#[test]
fn a_relogin_of_a_foreign_or_non_proxy_name_is_refused_before_any_request() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "other-px", &stub.base);
    crate::profile::write_proxy_record(
        &ProfileName::from("other-px"),
        &ProxyAccount {
            service: crate::proxy::Service::parse("other").unwrap(),
            account: ACCOUNT.to_string(),
        },
    )
    .unwrap();
    let plain = crate::profile::Profile::new(
        "plain".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-plain".to_string()),
    );
    crate::profile::save_profile(&plain).unwrap();
    config.add(plain);
    crate::profile::save_app_state(&config.state).unwrap();
    let (other_before, plain_before) = (config_bytes("other-px"), config_bytes("plain"));

    let err = crate::cmd_login(proxy_login("other-px", None)).expect_err("another service refuses");
    assert_eq!(
        err.to_string(),
        "'other-px' is already a proxy account on \"other\"; log in to proxy \"stub\" under \
         another name"
    );
    let err = crate::cmd_login(proxy_login("plain", None)).expect_err("a non-proxy name refuses");
    assert_eq!(
        err.to_string(),
        "'plain' exists and is not a proxy account; `--proxy` registers a proxy profile under a \
         new name"
    );

    assert_eq!(config_bytes("other-px"), other_before);
    assert_eq!(config_bytes("plain"), plain_before);
    assert_eq!(
        stub.state().requests,
        Vec::<String>::new(),
        "no request left"
    );
}

/// What a foreign answerer's `/health` body is, and how the refusal words it.
fn foreign_answerers() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "a foreign service",
            json!({"status": "ok", "service": "other", "version": "0.0.1", "contract": "1.0"}),
            "a proxy answers as service \"other\"",
        ),
        (
            "a foreign contract major",
            json!({"status": "ok", "service": "stub", "version": "0.0.1", "contract": "2.0"}),
            "it answers on contract \"2.0\"",
        ),
    ]
}

/// The consequence a login refused mid-flow by the identity gate names; the
/// settings calls keep `Nothing was changed`.
const MID_LOGIN_REFUSED: &str = "The login stopped and clauth saved nothing";

fn identity_refusal(port: u16, answered: &str, consequence: &str) -> String {
    format!(
        "proxy \"stub\" at 127.0.0.1:{port} is not answering as itself (expected service \
         \"stub\", contract 1): {answered}. {consequence}. Check it with `clauth proxy list`, \
         then retry"
    )
}

/// The login sends the admin token only once `/health` named the expected
/// service on a contract major clauth speaks: a foreign service, a foreign
/// major and a dead port each refuse with no bearer request sent.
#[test]
fn a_login_sends_no_bearer_request_to_a_bind_that_is_not_the_proxy() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let mut stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    for (case, health, answered) in foreign_answerers() {
        stub.state().health = health;
        let err = crate::cmd_login(proxy_login("px", None)).expect_err(case);
        assert_eq!(
            err.to_string(),
            identity_refusal(stub.port, answered, "No login was started"),
            "{case}"
        );
        assert_eq!(stub.state().bearered, Vec::<String>::new(), "{case}");
    }
    stub.stop();
    let err = crate::cmd_login(proxy_login("px", None)).expect_err("a dead port refuses");
    assert_eq!(
        err.to_string(),
        identity_refusal(stub.port, "nothing answers there", "No login was started")
    );
    let config = crate::profile::load_config().unwrap();
    assert!(
        config.find(&ProfileName::from("px")).is_none(),
        "no profile created"
    );
}

/// The delete sends the admin token only to the proxy itself: a foreign
/// service, a foreign major and a dead port each refuse, sending no bearer
/// request and deleting nothing.
#[test]
fn a_delete_sends_no_bearer_request_to_a_bind_that_is_not_the_proxy() {
    let (_home, mut stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let before = config_bytes("px");
    for (case, health, answered) in foreign_answerers() {
        stub.state().health = health;
        let err = crate::cmd_delete("px", true, false).expect_err(case);
        assert_eq!(
            err.to_string(),
            identity_refusal(stub.port, answered, "Nothing was deleted"),
            "{case}"
        );
        assert_eq!(stub.state().bearered, Vec::<String>::new(), "{case}");
    }
    stub.stop();
    let err = crate::cmd_delete("px", true, false).expect_err("a dead port refuses");
    assert_eq!(
        err.to_string(),
        identity_refusal(stub.port, "nothing answers there", "Nothing was deleted")
    );
    assert_eq!(config_bytes("px"), before, "the profile stays as it was");
}

/// Both settings calls refuse a foreign answerer with the settings
/// consequence, sending no bearer request.
#[test]
fn a_settings_call_to_a_bind_that_is_not_the_proxy_changes_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().health =
        json!({"status": "ok", "service": "other", "version": "0.0.1", "contract": "1.0"});
    let expected = identity_refusal(
        stub.port,
        "a proxy answers as service \"other\"",
        "Nothing was changed",
    );

    let err = crate::proxy_account::apply_account_settings(
        &stub_service(),
        ACCOUNT,
        &[("plan".to_string(), json!("start-plan"))],
    )
    .expect_err("the change refuses");
    assert_eq!(err.to_string(), expected);
    let err = crate::proxy_account::declared_account_settings(&stub_service())
        .expect_err("the settings read refuses");
    assert_eq!(err.to_string(), expected);
    assert_eq!(stub.state().bearered, Vec::<String>::new());
}

/// The per-tick usage read sends the admin token only to the proxy itself.
#[test]
fn a_usage_read_sends_no_bearer_request_to_a_bind_that_is_not_the_proxy() {
    let (_home, stub, config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    for (case, health, _) in foreign_answerers() {
        stub.state().health = health;
        let state = leg_state();
        crate::usage::reset_request_slots();
        run_leg(&state, &config);
        assert!(
            !matches!(
                state.third_party_status.lock().unwrap().get("px"),
                Some(FetchStatus::Fresh)
            ),
            "{case}: no reading lands"
        );
        assert_eq!(stub.state().bearered, Vec::<String>::new(), "{case}");
    }
}

/// A login page that is not a plain URL — here one carrying a newline and an
/// ESC byte — is refused before anything prints or opens, and the flow the
/// proxy opened is cancelled.
#[test]
fn a_login_url_carrying_control_bytes_is_refused_and_its_flow_cancelled() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().login_url = "https://stub.example/login\n\u{1b}[2Jclauth: login done".to_string();
    let opened = crate::platform::OpenedUrls::capture_here();

    let err = crate::cmd_login(proxy_login("px", None)).expect_err("the url refuses");

    assert_eq!(
        err.to_string(),
        "proxy \"stub\" answered a login body without a usable url; run `clauth proxy check \
         stub` for details"
    );
    assert_eq!(opened.urls(), Vec::<String>::new(), "nothing opened");
    assert!(stub.state().cancelled, "the opened flow is cancelled");
    let config = crate::profile::load_config().unwrap();
    assert!(config.find(&ProfileName::from("px")).is_none());
}

/// A paste the proxy refuses ends the login, and the flow is cancelled.
#[test]
fn a_refused_paste_cancels_the_flow() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().paste_refusal = Some(err_body(422, "code_invalid"));
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let control = stub_control();

    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
    let (paste, pasted) = std::sync::mpsc::channel();
    paste.send("bad-code".to_string()).unwrap();
    flow.await_done(AwaitLogin {
        paste: &pasted,
        interrupted: &mut || None,
        profile: "px",
        wait: &|_| {},
        say: &|_| {},
        key_stored: &|| false,
        holder: &|_| None,
    })
    .expect_err("a refused paste ends the login");

    assert_eq!(stub.state().pastes_seen, vec!["bad-code".to_string()]);
    assert!(stub.state().cancelled, "the flow is cancelled");
}

/// A poll the proxy refuses ends the login, and the flow is cancelled.
#[test]
fn a_refused_poll_cancels_the_flow() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().poll_refusal = Some(err_body(500, "internal"));
    let control = stub_control();

    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
    await_quietly(flow).expect_err("a refused poll ends the login");

    assert!(stub.state().cancelled, "the flow is cancelled");
}

/// A refused control call names the status alone: the proxy's free-text
/// `reason` reaches no clauth surface.
#[test]
fn a_refused_control_call_never_carries_the_proxys_reason() {
    const REASON: &str = "REASON-P6-sentinel \u{1b}[31m do this instead";
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().login_refusal = Some(reply(
        503,
        json!({"ok": false, "error": "upstream_unavailable", "reason": REASON}),
    ));

    let err = crate::cmd_login(proxy_login("px", None)).expect_err("the start refuses");

    assert_eq!(
        format!("{err:#}"),
        "proxy \"stub\" refused the request (HTTP 503); run `clauth proxy check stub` for details"
    );
}

/// The delete reads a 404 as already deleted only for the contract's
/// `not_found`: another code refuses, and the profile stays.
#[test]
fn a_delete_404_with_another_code_refuses_and_keeps_the_profile() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().delete_refusal = Some(err_body(404, "conflict"));
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the delete refuses");

    assert_eq!(
        err.to_string(),
        "proxy \"stub\" refused the account delete (HTTP 404); nothing was deleted. Run \
         `clauth proxy check stub` for details"
    );
    assert_eq!(config_bytes("px"), before, "the profile stays");
    let config = crate::profile::load_config().unwrap();
    assert!(config.find(&ProfileName::from("px")).is_some());
}

/// A `[proxy]` table that does not parse still names an upstream account: the
/// delete refuses before any request and deletes nothing.
#[test]
fn a_delete_of_a_profile_whose_proxy_record_does_not_parse_refuses() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    let path = crate::profile::profile_subpath(&name, "config.toml").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("service = \"stub\"", "service = \"Not A Service\""),
    )
    .unwrap();
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the delete refuses");

    assert_eq!(
        err.to_string(),
        format!(
            "profile 'px' has a [proxy] table in {path:?} that clauth cannot read, so its \
             account on the proxy cannot be deleted; nothing was deleted. Give the table a \
             valid `service` and `account`, or remove it to delete the profile alone"
        )
    );
    assert_eq!(config_bytes("px"), before);
    assert_eq!(stub.state().requests, Vec::<String>::new());
}

/// The proxy's `state` string reaches the account row escaped: a control
/// byte in it prints as its `\u{..}` escape.
#[test]
fn a_proxy_account_state_renders_escaped() {
    let mut usage = usage_body(500_000.0, 100.0);
    usage["accounts"][0]["state"] = json!("login_required\u{1b}[2J");
    let (_home, _stub, config) = proxy_profile_on_stub(usage);
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let rows = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the cached rows")
    .rows;
    assert_eq!(rows[0].value, "account login_required\\u{001b}[2J");
}

/// A window that claims the 5h chain role and stops mapping (here a zero
/// `limit`) never leaves the member windowless: the read counts as failed,
/// and the walk keeps judging the last good window.
#[test]
fn a_declared_chain_window_that_fails_to_map_keeps_the_last_good_window() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(4_000_000.0, 100.0));
    config.state.fallback_chain = vec!["active".into(), "px".into()];
    config.state.active_profile = Some(ProfileName::from("active"));
    let mut active = crate::profile::Profile::new("active".to_string(), None, None);
    active.fallback_threshold = Some(95.0);
    config.add(active);
    let state = leg_state();
    state.store.lock().unwrap().insert(
        "active".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some("2030-01-01T05:00:00Z".to_string()),
            }),
            ..Default::default()
        },
    );
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let mut broken = usage_body(4_000_000.0, 100.0);
    broken["accounts"][0]["figures"][0]["limit"] = json!(0);
    stub.state().usage = broken;
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let (snap, usage, names) = walk_view(&state, &config);
    assert_eq!(
        usage
            .get("px")
            .and_then(|usage| usage.five_hour.as_ref())
            .map(|window| window.utilization),
        Some(100.0),
        "the last good window stands"
    );
    assert_eq!(
        crate::fallback::next_auto_switch_target_with_usage(
            &snap,
            &usage,
            &names,
            crate::fallback::ActiveBurnRate::FromDisk,
            false,
            false,
        ),
        None,
        "the spent member stays excluded"
    );
}

/// The proxy-account delete's confirm names the upstream half it deletes.
#[test]
fn the_proxy_delete_confirm_names_the_proxy_account() {
    let record = ProxyAccount {
        service: stub_service(),
        account: ACCOUNT.to_string(),
    };
    assert_eq!(
        crate::delete_prompt("px", Some(&record)),
        "clauth: delete profile 'px', its credentials, and its account on proxy \"stub\" (this \
         also signs that account out of its provider)? [y/N] "
    );
    assert_eq!(
        crate::delete_prompt("px", None),
        "clauth: delete profile 'px' and all its credentials? [y/N] "
    );
}

/// The login's progress and success lines, for the values the login hands
/// them (the url and account already escaped by the caller).
#[test]
fn the_proxy_login_lines_name_the_proxy_profile_and_account() {
    assert_eq!(
        crate::proxy_login_opening_line(&stub_service(), "px", "https://stub.example/login"),
        "clauth: logging in to proxy \"stub\" for 'px', opening https://stub.example/login..."
    );
    assert_eq!(
        crate::proxy_login_captured_line(&stub_service(), "px", ACCOUNT),
        "clauth: captured into profile 'px' (proxy \"stub\", account \"acct-1\"). Switch to it \
         with:  clauth px"
    );
}

/// The zcode proxy's authorize-url shape: an encoded `redirect_uri` query
/// takes every such url past 80 characters.
const LONG_URL: &str = "https://auth.example.com/authorize?redirect_uri=https%3A%2F%2Fzcode.z.ai%2Fapp%2Foauth%2Flogin%3Fredirect%3Dzcode%253A%252F%252Foauth%252Fcallback%26app_version%3D1.2.3";

/// The login page prints whole — never cut — escaped, with clauth's admin
/// token redacted wherever the proxy put it.
#[test]
fn a_long_login_url_prints_whole_with_the_admin_token_redacted() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin = mint_admin_token();
    let stub = Stub::start(admin.clone(), usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().login_url = format!("{LONG_URL}&state={admin}");
    let control = stub_control();

    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");

    assert_eq!(
        crate::proxy_login_opening_line(
            &stub_service(),
            "px",
            &control.printable_url(&flow.start().url)
        ),
        format!(
            "clauth: logging in to proxy \"stub\" for 'px', opening {LONG_URL}&state=<redacted>..."
        )
    );
}

/// A refusal clauth can decide locally is decided before the irreversible
/// upstream delete: an unparsable client-token store refuses with the proxy
/// sent nothing at all and the profile as it was.
#[test]
fn a_local_refusal_is_decided_before_the_upstream_delete() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let store = crate::gateway_tokens::store_path().unwrap();
    std::fs::write(&store, "not = [toml").unwrap();
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the token store refuses");

    assert_eq!(
        err.to_string(),
        format!(
            "cannot delete profile \"px\": clauth's client-token store does not parse; delete it \
             and add the tokens again; store: {}",
            store.display()
        )
    );
    assert_eq!(
        stub.state().requests,
        Vec::<String>::new(),
        "the proxy is sent nothing"
    );
    assert_eq!(config_bytes("px"), before, "the profile stays as it was");
}

/// A runtime tree whose supervisor ignores SIGTERM refuses the proxy delete
/// before the upstream delete: the stub answers every request the happy path
/// sends and counts each, and it is sent none.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_supervisor_refusal_is_decided_before_the_upstream_delete() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let tree = crate::profile::profile_dir(&ProfileName::from("px"))
        .unwrap()
        .join("runtime-4242-0");
    std::fs::create_dir_all(&tree).unwrap();
    let sup = crate::testutil::FakeSupervisor::spawn(&tree, true);
    crate::testutil::write_daemon_lock(&tree, sup.pid, &sup.proc_start);
    let _deadline =
        crate::runtime::SupervisorStopDeadlineGuard::set(std::time::Duration::from_millis(300));
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the supervisor refuses");

    assert_eq!(
        err.to_string(),
        format!(
            "'px': runtime tree {} still has a background supervisor clauth cannot stop on this \
             host; retry once it has exited, or pass --force to remove it anyway",
            tree.display()
        )
    );
    let state = stub.state();
    assert_eq!(
        (state.requests.clone(), state.deleted.clone()),
        (Vec::<String>::new(), Vec::<String>::new()),
        "(requests the proxy saw, accounts it deleted)"
    );
    assert_eq!(config_bytes("px"), before, "the profile stays as it was");
}

/// Deleting the ACTIVE proxy account rebuilds Claude Code's settings without
/// it, and a settings file that rebuild cannot read refuses before the
/// upstream delete: the proxy is sent nothing and the profile stays.
#[test]
fn an_unreadable_settings_file_refuses_the_active_delete_before_the_upstream_delete() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    config.state.active_profile = Some(ProfileName::from("px"));
    crate::profile::save_app_state(&config.state).unwrap();
    let settings = crate::claude::claude_settings_path().unwrap();
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{not json").unwrap();
    let before = config_bytes("px");

    crate::cmd_delete("px", true, false).expect_err("the settings file refuses");

    assert_eq!(
        stub.state().requests,
        Vec::<String>::new(),
        "the proxy is sent nothing"
    );
    assert_eq!(config_bytes("px"), before, "the profile stays as it was");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{not json");
}

/// What only the local removal can hit after the proxy deleted the account
/// says the account is gone and that a rerun finishes, and the rerun does.
#[test]
fn a_local_failure_after_the_upstream_delete_says_a_rerun_finishes() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    let store = crate::gateway_tokens::store_path().unwrap();
    let broken = store.clone();
    // Runs between the upstream delete and the removal's own hold.
    crate::gateway_tokens::set_pre_hold_hook(
        "px",
        std::sync::Arc::new(move || std::fs::write(&broken, "not = [toml").unwrap()),
    );

    let err = crate::cmd_delete("px", true, false).expect_err("the local half fails");

    assert_eq!(
        err.to_string(),
        format!(
            "proxy \"stub\" deleted account \"acct-1\", but clauth could not remove profile 'px': \
             cannot delete profile \"px\": clauth's client-token store does not parse; delete it \
             and add the tokens again; store: {}. Run `clauth delete px` again to finish",
            store.display()
        )
    );
    assert_eq!(stub.state().deleted, vec![ACCOUNT.to_string()]);
    assert!(
        crate::profile::load_config().unwrap().find(&name).is_some(),
        "the profile stays for the rerun"
    );

    std::fs::remove_file(&store).unwrap();
    crate::cmd_delete("px", true, false).expect("the rerun finishes");
    assert!(crate::profile::load_config().unwrap().find(&name).is_none());
}

/// A proxy that drops the connection on the account delete is named with its
/// bind, and nothing local moves.
#[test]
fn a_dropped_account_delete_names_the_proxy_and_deletes_nothing() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().drop_delete = true;
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the delete refuses");

    assert_eq!(
        err.to_string(),
        format!(
            "proxy \"stub\" at 127.0.0.1:{} did not answer the account delete. Nothing was \
             deleted. Check it with `clauth proxy list`, then retry",
            stub.port
        )
    );
    assert_eq!(config_bytes("px"), before);
}

/// The shared delete (the TUI's) refuses a proxy account — a parsed record,
/// a table that does not read, and a config.toml that no longer parses around
/// one — sending the proxy nothing and leaving every byte, where `clauth
/// delete` deletes the account on the proxy too
/// (`delete_sends_the_delete_removes_the_dir_whole_and_keeps_the_token_ledger`).
#[test]
fn the_shared_delete_refuses_a_proxy_account_and_sends_nothing() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    let path = crate::profile::profile_subpath(&name, "config.toml").unwrap();
    let before = config_bytes("px");
    let delete = |config: &mut crate::profile::AppConfig| {
        let rotation = crate::actions::rotation_guard_for_mutation(&name).unwrap();
        crate::actions::delete_profile(config, &name, false, &rotation)
            .expect_err("the shared delete refuses")
            .to_string()
    };

    assert_eq!(
        delete(&mut config),
        "'px' is an account on proxy \"stub\": delete it with `clauth delete px`, which also \
         deletes it on the proxy"
    );
    assert_eq!(config_bytes("px"), before);

    let unreadable = format!(
        "profile 'px' has a [proxy] table in {path:?} that clauth cannot read, so its account on \
         the proxy cannot be deleted; nothing was deleted. Give the table a valid `service` and \
         `account`, or remove it to delete the profile alone"
    );
    let text = String::from_utf8(before.clone()).unwrap();
    std::fs::write(
        &path,
        text.replace("service = \"stub\"", "service = \"Not A Service\""),
    )
    .unwrap();
    assert_eq!(
        delete(&mut config),
        unreadable,
        "a table that does not read"
    );
    std::fs::write(&path, format!("{text}\nbroken = [\n")).unwrap();
    assert_eq!(
        delete(&mut config),
        unreadable,
        "a file that no longer parses"
    );

    assert_eq!(stub.state().requests, Vec::<String>::new());
    assert!(config.find(&name).is_some(), "the profile stays");
}

/// The refusal of a delete of `px` whose `config.toml` cannot be classified.
const UNCLASSIFIABLE_DELETE: &str = "can't read the config of 'px', so clauth can't tell whether \
                                     it is an account on a proxy; fix its config.toml, then \
                                     delete it again";

/// The shared delete (the TUI's) of a proxy account whose `config.toml` turned
/// unclassifiable after the config loaded (a directory in its place, a
/// `[proxy] # note` header that fails to parse) refuses: every byte of the
/// profile stays and the proxy hears nothing.
#[test]
fn the_shared_delete_refuses_an_account_whose_config_cannot_be_classified() {
    use crate::claude::tests::Unclassifiable;
    let name = ProfileName::from("px");
    let outcomes: Vec<_> = Unclassifiable::BOTH
        .iter()
        .map(|shape| {
            let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
            shape.pose("px");
            let tree = profile_tree("px");
            let rotation = crate::actions::rotation_guard_for_mutation(&name).unwrap();
            let outcome = crate::actions::delete_profile(&mut config, &name, false, &rotation)
                .map_err(|e| format!("{e:#}"));
            drop(rotation);
            let requests = stub.state().requests.clone();
            (
                outcome,
                profile_tree("px") == tree,
                crate::profile::profile_subpath(&name, "config.toml")
                    .unwrap()
                    .exists(),
                config.find(&name).is_some(),
                requests,
            )
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![
            (
                Err(UNCLASSIFIABLE_DELETE.to_string()),
                true,
                true,
                true,
                Vec::<String>::new()
            );
            2
        ],
        "(the delete, px's files kept, its config.toml kept, the profile kept, the requests \
         the proxy saw) for a directory and a noted header"
    );
}

/// `clauth delete` of a proxy account whose `config.toml` turns unclassifiable
/// after the command loaded every profile refuses before the confirm: the
/// whole line on stderr, exit 2, every byte of the profile kept and nothing
/// sent to the proxy. Without `--yes` on a stdin that is no terminal the
/// confirm would refuse on its own sentence, so that run shows the refusal
/// comes first.
#[test]
fn clauth_delete_refuses_an_account_whose_config_turned_unclassifiable() {
    use crate::claude::tests::Unclassifiable;
    let runs = [
        (Unclassifiable::Directory, true),
        (Unclassifiable::NotedProxyHeader, true),
        (Unclassifiable::Directory, false),
    ];
    let outcomes: Vec<_> = runs
        .iter()
        .map(|&(shape, yes)| {
            let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
            let posed = std::rc::Rc::new(std::cell::RefCell::new(None));
            let seen = std::rc::Rc::clone(&posed);
            crate::set_before_delete_claim(move || {
                shape.pose("px");
                *seen.borrow_mut() = Some(profile_tree("px"));
            });
            let mut stderr = Vec::new();
            let code = crate::exit_code_to(crate::cmd_delete("px", yes, false), &mut stderr);
            let kept = posed.borrow().as_ref() == Some(&profile_tree("px"));
            let requests = stub.state().requests.clone();
            (String::from_utf8(stderr).unwrap(), code, kept, requests)
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![
            (
                format!("Error: {UNCLASSIFIABLE_DELETE}\n"),
                2,
                true,
                Vec::<String>::new()
            );
            3
        ],
        "(stderr, exit code, px's files as posed, the requests the proxy saw) for a directory \
         and a noted header with `--yes`, and a directory without it"
    );
}

/// A re-login of a profile whose `[proxy]` table does not read refuses before
/// any request, naming the table, never as "not a proxy account".
#[test]
fn a_relogin_of_a_profile_whose_proxy_record_does_not_parse_refuses() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    let path = crate::profile::profile_subpath(&name, "config.toml").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("service = \"stub\"", "service = \"Not A Service\""),
    )
    .unwrap();
    let before = config_bytes("px");

    let err = crate::cmd_login(proxy_login("px", None)).expect_err("the re-login refuses");

    assert_eq!(
        err.to_string(),
        format!(
            "profile 'px' has a [proxy] table in {path:?} that clauth cannot read; nothing was \
             changed. Give the table a valid `service` and `account`, then log in again"
        )
    );
    assert_eq!(config_bytes("px"), before);
    assert_eq!(stub.state().requests, Vec::<String>::new());
}

/// A signal during the login cancels the flow, says so, and exits `128 +
/// signal` — 130 for Ctrl-C. Driven through the doors seam: the test process
/// is never signalled.
#[test]
fn an_interrupted_login_cancels_its_flow_and_exits_130() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();

    let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
    signal.join().unwrap();
    assert_eq!(
        err.to_string(),
        "clauth: login cancelled; proxy \"stub\" stopped its login flow"
    );
    assert!(
        stub.state().cancelled,
        "the flow is cancelled on the way out"
    );

    stub.state().cancelled = false;
    let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
    let result = crate::cmd_login(proxy_login("px", None));
    signal.join().unwrap();
    assert!(stub.state().cancelled);
    assert_eq!(crate::exit_code(result), 130);
    assert!(
        crate::profile::load_config()
            .unwrap()
            .find(&ProfileName::from("px"))
            .is_none(),
        "no profile created"
    );
}

/// A second signal while the first one's flow cancel is still in flight ends
/// the login at once with `128 +` that second signal, printing nothing more:
/// the cancelled-flow line belongs to a cancel that finished. The cancel the
/// first signal started still reaches the proxy.
#[test]
fn a_second_signal_during_the_cancel_exits_at_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let (release, held) = std::sync::mpsc::channel();
    {
        let mut state = stub.state();
        state.flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
        state.hold = Some((cancel_request(), held));
    }
    let doors = crate::ProxyLoginDoors::capture_here();
    let interrupt = doors.interrupt();
    let first = signal_after_the_next_poll(&stub, interrupt.clone(), 2);
    let state = Arc::clone(&stub.state);
    // SIGTERM lands once the proxy holds the cancel SIGINT started.
    let second = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = interrupt.send(15);
    });

    let result = crate::cmd_login(proxy_login("px", None));
    let answered_before_return = stub.state().cancelled;
    first.join().unwrap();
    second.join().unwrap();
    drop(release);

    assert!(
        !answered_before_return,
        "the login returned while the proxy still held the cancel"
    );
    let err = result.expect_err("interrupted");
    assert!(
        err.downcast_ref::<crate::proxy_account::LoginInterrupted>()
            .is_none(),
        "no cancelled-flow line for a cancel that never finished"
    );
    assert_eq!(crate::exit_code(Err(err)), 143);
    let deadline = std::time::Instant::now() + HOLD;
    while !stub.state().cancelled && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(stub.state().cancelled, "the started cancel still lands");
}

/// A proxy that loses its port to a foreign answerer between two polls of a
/// long login is sent no further bearer request: no poll, no cancel. The
/// stand-in answers `done` from the swap on, so a poll that did carry the
/// bearer would end the login rather than wait out the flow.
#[test]
fn a_proxy_swapped_out_between_two_polls_gets_no_bearer_request() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let control = stub_control();
    let flow = crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
    let closed = std::sync::mpsc::channel().1;
    let bearered_before_swap = std::cell::RefCell::new(None);

    let err = flow
        .await_done(AwaitLogin {
            paste: &closed,
            interrupted: &mut || None,
            profile: "px",
            wait: &|_| {
                let mut seen = bearered_before_swap.borrow_mut();
                if seen.is_none() {
                    let mut state = stub.state();
                    *seen = Some(state.bearered.clone());
                    state.health = json!({"status": "ok", "service": "other",
                                          "version": "0.0.1", "contract": "1.0"});
                    state.flow_states = vec![done_body(ACCOUNT, Some(KEY))];
                }
            },
            say: &|_| {},
            key_stored: &|| false,
            holder: &|_| None,
        })
        .expect_err("the swapped bind refuses");

    assert_eq!(
        err.to_string(),
        identity_refusal(
            stub.port,
            "a proxy answers as service \"other\"",
            MID_LOGIN_REFUSED
        )
    );
    assert_eq!(
        Some(stub.state().bearered.clone()),
        bearered_before_swap.into_inner(),
        "nothing after the swap carried the bearer"
    );
    assert!(!stub.state().cancelled, "not even the cancel");
}

/// The browser door's `done` lands while the paste prompt still waits: the
/// login finishes and the code typed afterwards is never submitted.
#[test]
fn a_browser_done_lands_while_the_paste_prompt_waits() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    let paste = doors.paste();
    let state = Arc::clone(&stub.state);
    // The code arrives only once the proxy has answered a poll: a prompt the
    // poll waits behind would hold it for this thread's whole bound.
    let typist = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while state.lock().unwrap().flow_polls == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = paste.send("late-code".to_string());
    });

    crate::cmd_login(proxy_login("px", None)).expect("the login finishes");
    typist.join().unwrap();

    assert_eq!(stub.state().pastes_seen, Vec::<String>::new());
    assert!(
        crate::profile::load_config()
            .unwrap()
            .find(&ProfileName::from("px"))
            .is_some()
    );
}

/// The browser door winning while the paste prompt still waits leaves the
/// prompt's line open, so the success line starts on a line of its own.
#[test]
fn a_browser_win_over_a_waiting_prompt_starts_the_success_line_on_its_own_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    // Held unsent across the login: the prompt's reader is still waiting.
    let paste = doors.paste();

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");
    drop(paste);

    assert_eq!(
        line,
        "\nclauth: captured into profile 'px' (proxy \"stub\", account \"acct-1\"). Switch to \
         it with:  clauth px"
    );
}

/// A line the paste prompt read ended it (the user's Enter), so the success
/// line follows with no blank line; so does a login that showed no prompt.
#[test]
fn a_success_line_after_a_read_or_no_prompt_starts_where_it_is() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![
        json!({"flow": FLOW, "state": "pending"}),
        done_body(ACCOUNT, Some(KEY)),
    ];
    let doors = crate::ProxyLoginDoors::capture_here();
    // The reader sends the one line it read and ends, as the TTY reader does.
    let paste = doors.paste();
    paste.send("code-7".to_string()).unwrap();
    drop(paste);

    let pasted = crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");
    stub.state().flow_states = vec![done_body("acct-2", Some(KEY))];
    let unprompted =
        crate::login_proxy(&proxy_login("py", None), "stub").expect("the login finishes");

    assert_eq!(stub.state().pastes_seen, vec!["code-7".to_string()]);
    assert_eq!(
        pasted,
        "clauth: captured into profile 'px' (proxy \"stub\", account \"acct-1\"). Switch to it \
         with:  clauth px"
    );
    assert_eq!(
        unprompted,
        "clauth: captured into profile 'py' (proxy \"stub\", account \"acct-2\"). Switch to it \
         with:  clauth py"
    );
}

/// A code pasted while the browser poll still reads `pending` is submitted at
/// once, and the login finishes on the poll after it.
#[test]
fn a_pasted_code_lands_while_the_browser_poll_waits() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![
        json!({"flow": FLOW, "state": "pending"}),
        done_body(ACCOUNT, Some(KEY)),
    ];
    let doors = crate::ProxyLoginDoors::capture_here();
    doors.paste().send("code-7".to_string()).unwrap();

    crate::cmd_login(proxy_login("px", None)).expect("the login finishes");

    assert_eq!(stub.state().pastes_seen, vec!["code-7".to_string()]);
    assert_eq!(
        stub.state().flow_polls,
        2,
        "the paste went in between the two polls"
    );
}

/// The paste that completes a flow is the answer that first sees `done`, so
/// the core hands the key out on it and on no later read (`flows.ts` `paste`
/// answers through `responseJson`): the login keeps the key the paste's
/// answer carries and polls no more.
#[test]
fn a_paste_that_completes_the_flow_keeps_the_key_its_answer_carries() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![core(CORE_PENDING), core(CORE_DONE_LATER)];
    stub.state().paste_answer = core(CORE_DONE_WITH_KEY);
    let doors = crate::ProxyLoginDoors::capture_here();
    doors.paste().send("code-7".to_string()).unwrap();

    crate::cmd_login(proxy_login("px", None)).expect("the login finishes");

    let config = crate::profile::load_config().expect("config");
    assert_eq!(
        config
            .find(&ProfileName::from("px"))
            .expect("profile created")
            .api_key
            .as_deref(),
        Some(CORE_KEY),
        "the key the paste's answer carried"
    );
    let state = stub.state();
    assert_eq!(
        (state.pastes_seen.clone(), state.flow_polls),
        (vec!["code-7".to_string()], 1),
        "the paste's done answer ended the login"
    );
}

/// A login page holding whitespace, or a character the escaping would rewrite
/// (here a bidi override, which is no whitespace), is refused before anything
/// prints or opens, and its flow is cancelled.
#[test]
fn a_login_url_with_whitespace_or_a_bidi_character_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let opened = crate::platform::OpenedUrls::capture_here();
    for url in [
        "https://stub.example/login?x=1 clauth: login done",
        "https://stub.example/login\u{202e}nigol",
    ] {
        stub.state().login_url = url.to_string();
        stub.state().cancelled = false;
        let err = crate::cmd_login(proxy_login("px", None)).expect_err(url);
        assert_eq!(
            err.to_string(),
            "proxy \"stub\" answered a login body without a usable url; run `clauth proxy check \
             stub` for details",
            "{url:?}"
        );
        assert!(stub.state().cancelled, "{url:?}: its flow is cancelled");
    }
    assert_eq!(opened.urls(), Vec::<String>::new(), "nothing opened");
}

/// Every other answerer the identity gate refuses — a proxy naming no
/// service, a foreign service name holding a `"`, something that is no proxy,
/// a `/health` that never answers — is named by equality and sent no bearer
/// request.
#[test]
fn the_gate_names_every_other_answerer_and_sends_no_bearer() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    type Pose = fn(&mut StubState);
    let cases: [(&str, Pose, &str); 4] = [
        (
            "no service",
            |s| s.health = json!({"status": "ok", "version": "0.0.1", "contract": "1.0"}),
            "a proxy answers naming no service",
        ),
        (
            "a quote in a foreign service",
            |s| s.health = json!({"status": "ok", "service": "ot\"her", "version": "0.0.1", "contract": "1.0"}),
            "a proxy answers as service \"ot\\\"her\"",
        ),
        (
            "no proxy",
            |s| s.health_status = Some(503),
            "something answers there with HTTP 503 and no proxy /health",
        ),
        (
            "no answer",
            |s| s.drop_health = true,
            "its /health did not answer",
        ),
    ];
    for (case, pose, answered) in cases {
        {
            let mut state = stub.state();
            *state = StubState::new(state.admin.clone(), usage_body(500_000.0, 100.0));
            pose(&mut state);
        }
        let err = crate::cmd_login(proxy_login("px", None)).expect_err(case);
        assert_eq!(
            err.to_string(),
            identity_refusal(stub.port, answered, "No login was started"),
            "{case}"
        );
        assert_eq!(stub.state().bearered, Vec::<String>::new(), "{case}");
    }
}

/// A display-only window labelled `5h` never feeds the chain: listed ahead of
/// the member's spent chain-5h window it neither hides that window nor stands
/// in for it, so the member stays excluded, and it still renders as a row.
#[test]
fn a_display_only_5h_label_never_feeds_the_chain() {
    let mut body = usage_body(4_000_000.0, 100.0);
    body["accounts"][0]["figures"]
        .as_array_mut()
        .unwrap()
        .insert(
            0,
            json!({"kind": "window", "id": "tools-5h", "label": "5h", "used": 10.0,
               "limit": 100.0, "unit": "requests", "window_secs": 18000,
               "resets_at": "2030-01-01T05:00:00Z", "chain": null,
               "read_at": "2030-01-01T00:00:00Z"}),
        );
    let (_home, _stub, mut config) = proxy_profile_on_stub(body);
    config.state.fallback_chain = vec!["active".into(), "px".into()];
    config.state.active_profile = Some(ProfileName::from("active"));
    let mut active = crate::profile::Profile::new("active".to_string(), None, None);
    active.fallback_threshold = Some(95.0);
    config.add(active);
    let state = leg_state();
    state.store.lock().unwrap().insert(
        "active".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some("2030-01-01T05:00:00Z".to_string()),
            }),
            ..Default::default()
        },
    );
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let (snap, usage, names) = walk_view(&state, &config);
    assert_eq!(
        usage
            .get("px")
            .and_then(|usage| usage.five_hour.as_ref())
            .map(|window| window.utilization),
        Some(100.0),
        "the chain window is the one the walk judges"
    );
    assert_eq!(
        crate::fallback::next_auto_switch_target_with_usage(
            &snap,
            &usage,
            &names,
            crate::fallback::ActiveBurnRate::FromDisk,
            false,
            false,
        ),
        None,
        "the spent member stays excluded"
    );
    let stats = state
        .third_party_usage_store
        .lock()
        .unwrap()
        .get("px")
        .cloned()
        .unwrap();
    assert_eq!(
        stats
            .bars
            .iter()
            .map(|bar| bar.label.as_str())
            .collect::<Vec<_>>(),
        vec!["5h"],
        "one bar, the chain window's"
    );
    assert_eq!(
        (stats.rows[0].label.as_str(), stats.rows[0].value.as_str()),
        ("5h", "10% used"),
        "the display-only window renders as a row"
    );
}

/// A figure the proxy marks stale keeps its old `read_at`, and clauth's copy
/// is dated by it: the cache's age is that old, and the walk does not count
/// the member fresh.
#[test]
fn a_figure_the_proxy_marks_stale_dates_the_copy() {
    let read_at = crate::usage::now_epoch_secs() - 3 * 3600;
    let stamp = chrono::DateTime::from_timestamp(read_at, 0)
        .unwrap()
        .to_rfc3339();
    let mut body = usage_body(2_000_000.0, 100.0);
    body["accounts"][0]["figures"][0]["read_at"] = json!(stamp);
    body["accounts"][0]["figures"][0]["stale_reason"] = json!("upstream_unavailable");
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let age = crate::profile_json::cache_age_secs(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("a dated copy");
    let expected = u64::try_from(crate::usage::now_epoch_secs() - read_at).unwrap();
    assert!(
        age.abs_diff(expected) <= 1,
        "the copy is {age} s old, its stalest figure {expected} s"
    );
    let mut config = config.clone();
    config.state.fallback_chain = vec!["px".into()];
    config.state.active_profile = Some(ProfileName::from("px"));
    let (snap, _usage, _names) = walk_view(&state, &config);
    assert_eq!(
        snap.fresh,
        Vec::<ProfileName>::new(),
        "the walk does not count it fresh"
    );
}

/// A usage block without the contract's `available` verdict is malformed:
/// the read fails into the stale path and never lands as funded.
#[test]
fn a_usage_block_without_available_is_malformed() {
    let mut body = usage_body(500_000.0, 100.0);
    body["accounts"][0]
        .as_object_mut()
        .unwrap()
        .remove("available");
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    assert!(matches!(
        state.third_party_status.lock().unwrap().get("px"),
        Some(FetchStatus::Failed)
    ));
    assert!(
        crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
            &ProfileName::from("px"),
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        )
        .is_none(),
        "nothing landed"
    );
}

/// Every proxy string a usage row or bar carries is escaped: labels, the
/// summary, an offer's label, state and failure, the currency and both unit
/// spellings. Each sits beside a control byte its escape must rewrite.
#[test]
fn every_proxy_string_in_a_usage_row_renders_escaped() {
    let esc = "\u{1b}[2J";
    let body = json!({"accounts": [{
        "account": ACCOUNT, "state": "ready", "available": true,
        "figures": [
            {"kind": "window", "id": "w", "label": format!("tools{esc}"), "used": 1.0,
             "limit": 4.0, "unit": "requests", "window_secs": 3600, "resets_at": null,
             "read_at": "2030-01-01T00:00:00Z"},
            {"kind": "balance", "id": "b1", "label": "credit", "remaining": 5.0,
             "unit": "currency", "currency": format!("CNY{esc}"), "read_at": "2030-01-01T00:00:00Z"},
            {"kind": "balance", "id": "b2", "label": "credit", "remaining": 6.0,
             "unit": "custom", "unit_label": format!("pts{esc}"), "read_at": "2030-01-01T00:00:00Z"},
            {"kind": "balance", "id": "b3", "label": "credit", "remaining": 7.0,
             "unit": format!("gems{esc}"), "read_at": "2030-01-01T00:00:00Z"},
            {"kind": "channel", "id": "c", "label": format!("lane{esc}"), "open": true,
             "read_at": "2030-01-01T00:00:00Z"},
            {"kind": "offer", "id": "o", "label": "trials", "read_at": "2030-01-01T00:00:00Z",
             "items": [
                {"id": "i1", "label": format!("weekend{esc}"), "state": format!("claiming{esc}"),
                 "grants": []},
                {"id": "i2", "label": "night", "state": "failed", "failure": format!("quota{esc}"),
                 "grants": []},
             ]},
            {"kind": "quantum-flux", "id": "q", "label": "mystery",
             "summary": format!("7 qubits{esc}"), "read_at": "2030-01-01T00:00:00Z"},
        ],
    }]});
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let stats = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the cached rows");
    let escaped = "\\u{001b}[2J";
    let balance = crate::providers::DEEPSEEK_BALANCE_ROW_LABEL;
    assert_eq!(
        stats
            .bars
            .iter()
            .map(|bar| bar.label.clone())
            .collect::<Vec<_>>(),
        vec![format!("tools{escaped}")]
    );
    assert_eq!(
        stats
            .rows
            .iter()
            .map(|row| (row.label.clone(), row.value.clone()))
            .collect::<Vec<_>>(),
        vec![
            (balance.to_string(), format!("5 CNY{escaped}")),
            (balance.to_string(), format!("6 pts{escaped}")),
            (balance.to_string(), format!("7 gems{escaped}")),
            (format!("lane{escaped}"), "open".to_string()),
            (format!("weekend{escaped}"), format!("claiming{escaped}")),
            ("night".to_string(), format!("quota{escaped}")),
            ("mystery".to_string(), format!("7 qubits{escaped}")),
        ]
    );
}

/// A change to `proxies.toml` moves the reload fingerprint, so a fixed row
/// re-picks the proxy accounts' usage arm on the next reload check instead of
/// waiting for an unrelated profile write.
#[test]
fn a_registry_change_moves_the_reload_fingerprint() {
    let _home = crate::testutil::HomeSandbox::new();
    let before = crate::profile::reload_fingerprint();
    register_proxy_row(9141);
    assert_ne!(crate::profile::reload_fingerprint(), before);
}

/// The line for a flow cancel the proxy confirmed.
const CANCEL_CONFIRMED: &str = "clauth: login cancelled; proxy \"stub\" stopped its login flow";
/// The line for a cancel the proxy did not confirm.
const CANCEL_UNCONFIRMED: &str = "clauth: login cancelled, but proxy \"stub\" did not confirm its \
                                  login flow stopped; do not finish the login in the browser";

/// The ACTIVE proxy account `px` with its chain 5h window at 100% beside a
/// logged-in sibling `other` at 10%, read through the real leg with every figure read
/// now except figure `stale` of [`usage_body`]'s, which the proxy marks stale
/// with a `read_at` 3 h old. Runs the production `scan_auto_switch`; returns
/// `px`'s leg status, the switches queued, and how far the age the copy shows
/// is off the stale figure's own.
fn spent_active_proxy_with_a_stale_figure(stale: usize) -> (Option<FetchStatus>, usize, u64) {
    let at = |secs: i64| {
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .to_rfc3339()
    };
    let mut now = 0;
    let (_home, _stub, mut config) = proxy_profile_on_stub_built(|| {
        now = crate::usage::now_epoch_secs();
        let mut body = usage_body(4_000_000.0, 100.0);
        for figure in body["accounts"][0]["figures"].as_array_mut().unwrap() {
            figure["read_at"] = json!(at(now));
        }
        body["accounts"][0]["figures"][stale]["read_at"] = json!(at(now - 3 * 3600));
        body["accounts"][0]["figures"][stale]["stale_reason"] = json!("upstream_unavailable");
        body
    });
    config.state.fallback_chain = vec!["px".into(), "other".into()];
    config.state.active_profile = Some(ProfileName::from("px"));
    let mut other = crate::profile::Profile::new("other".to_string(), None, None);
    other.fallback_threshold = Some(95.0);
    other.credentials = Some(crate::testutil::oauth_pair("other"));
    config.add(other);
    let state = leg_state();
    *state.config.lock().unwrap() = config.clone();
    state.store.lock().unwrap().insert(
        "other".to_string(),
        crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 10.0,
                resets_at: Some("2030-01-01T05:00:00Z".to_string()),
            }),
            ..Default::default()
        },
    );
    state
        .status
        .lock()
        .unwrap()
        .insert("other".to_string(), FetchStatus::Fresh);
    crate::usage::reset_request_slots();
    run_leg(&state, &config);
    scan_auto_switch(
        &state.config,
        &state.store,
        &state.third_party_usage_store,
        &state.status,
        &state.third_party_status,
        &state.third_party_streaks,
        &state.third_party_broken,
        &state.skipped_unreadable,
        &state.poll_streaks,
        &state.kick_blocks,
        &state.activity,
        &state.pending_switch,
        &state.pending_switch_off,
    );
    let age = crate::profile_json::cache_age_secs(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("a dated copy");
    let stale_for = u64::try_from(crate::usage::now_epoch_secs() - (now - 3 * 3600)).unwrap();
    let status = state.third_party_status.lock().unwrap().get("px").copied();
    let queued = state.pending_switch.lock().unwrap().len();
    (status, queued, age.abs_diff(stale_for))
}

/// The age a proxy reading shows is its stalest figure's, but whether the
/// walk may act on it is the date of its chain windows alone: a spent active
/// proxy account whose 5h window was read this second switches away though an
/// offer figure beside it is 3 h stale, and its copy still shows 3 h. The same
/// account whose 5h window itself is 3 h stale does not act.
#[test]
fn a_stale_display_figure_never_holds_back_the_switch_a_fresh_chain_window_calls_for() {
    let (status, queued, off) = spent_active_proxy_with_a_stale_figure(3);
    assert_eq!(
        (status, queued),
        (Some(FetchStatus::Fresh), 1),
        "a fresh chain window at 100% switches away"
    );
    assert!(
        off <= 1,
        "the copy shows its stalest figure's age, not {off} s off it"
    );

    let (status, queued, off) = spent_active_proxy_with_a_stale_figure(0);
    assert_eq!(
        (status, queued),
        (Some(FetchStatus::Cached), 0),
        "a stale chain window is not acted on"
    );
    assert!(off <= 1, "{off} s off");
}

/// A mirror of `px`'s copy seeded at `now`, as every boot, takeover and
/// stand-down tick seeds it: the mirror's state, and the poll partition of it
/// at `now`.
fn seeded_mirror(
    config: &crate::profile::AppConfig,
    now: u64,
) -> (SchedulerState, Vec<ThirdPartyEntry>, HashMap<LegKey, u64>) {
    let entries = collect_third_party_entries(&config.profiles);
    let mirror = leg_state();
    seed_third_party_name(
        &mirror.third_party_usage_store,
        &mirror.third_party_status,
        &mirror.last_fetched,
        &ProfileName::from("px"),
        true,
        now,
        90_000,
    );
    let (due, next) = partition_due(
        &entries,
        now,
        &mirror.last_fetched,
        &mirror.activity,
        90_000,
        &HashMap::new(),
    );
    (mirror, due, next)
}

/// The seed stamps a proxy copy's next poll on the leg's own clock, the
/// instant clauth wrote the copy, never at the date its figures carry: a copy
/// landed now but dated 3 h old shows as `Cached` and is not due again until
/// one interval after it landed.
#[test]
fn a_dated_copy_reseeds_its_poll_on_the_legs_own_clock() {
    let read_at = crate::usage::now_epoch_secs() - 3 * 3600;
    let stamp = chrono::DateTime::from_timestamp(read_at, 0)
        .unwrap()
        .to_rfc3339();
    let mut body = usage_body(2_000_000.0, 100.0);
    body["accounts"][0]["figures"][0]["read_at"] = json!(stamp);
    body["accounts"][0]["figures"][0]["stale_reason"] = json!("upstream_unavailable");
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);

    let now = now_ms();
    let (mirror, due, next) = seeded_mirror(&config, now);

    assert_eq!(due.len(), 0, "a copy landed now is not due at once");
    let next_in: Vec<i64> = next.values().map(|at| *at as i64 - now as i64).collect();
    assert!(
        next_in.len() == 1 && next_in[0] > 0 && next_in[0] <= 90_000,
        "next poll one interval after the landing, not {next_in:?} ms out"
    );
    assert_eq!(
        mirror.third_party_status.lock().unwrap().get("px").copied(),
        Some(FetchStatus::Cached),
        "the copy still shows its 3 h age"
    );
}

/// A copy no figure dates — its mtime is the instant clauth wrote it — keeps
/// that mtime as its poll stamp, whatever instant the seed runs at.
#[test]
fn an_undated_copy_reseeds_its_poll_at_its_mtime() {
    let mut body = usage_body(2_000_000.0, 100.0);
    for figure in body["accounts"][0]["figures"].as_array_mut().unwrap() {
        figure.as_object_mut().unwrap().remove("read_at");
    }
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    let mtime = profile_cache_mtime_ms(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the copy");

    let (mirror, _due, _next) = seeded_mirror(&config, mtime + 600_000);

    let stamp = mirror
        .last_fetched
        .lock()
        .unwrap()
        .get(&FetchLeg::ThirdParty.key(ProfileName::from("px")))
        .copied()
        .expect("stamped")
        .as_millis();
    assert_eq!(stamp, mtime);
}

/// A `read_at` in the future proves the proxy's clock moved, not that the
/// figure is newer: the copy is dated now, and a seed of it keeps the next
/// poll one interval out rather than at that future date. Every figure of
/// [`usage_body`] reads 2030.
#[test]
fn a_read_at_in_the_future_dates_the_copy_now_and_keeps_the_poll_on_schedule() {
    let (_home, _stub, config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let before = now_ms();
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    let mtime = profile_cache_mtime_ms(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the copy");
    let now = now_ms();
    assert!(
        mtime + 1_000 >= before && mtime <= now,
        "dated now: {mtime} not in [{before}, {now}]"
    );

    let (_mirror, due, next) = seeded_mirror(&config, now);

    assert_eq!(due.len(), 0);
    let next_in: Vec<i64> = next.values().map(|at| *at as i64 - now as i64).collect();
    assert!(
        next_in.len() == 1 && next_in[0] > 0 && next_in[0] <= 90_000,
        "next poll one interval out, not {next_in:?} ms"
    );
}

/// A Ctrl-C after the proxy stopped answering mid-login (the stub stops after
/// the first poll): the cancel never reached it, so clauth does not claim the
/// flow stopped. It says the proxy did not confirm and warns off finishing the
/// login in the browser, still on SIGINT.
#[test]
fn an_interrupt_after_the_proxy_died_says_the_cancel_was_not_confirmed() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();
    let interrupt = doors.interrupt();
    let (state, stop) = (Arc::clone(&stub.state), Arc::clone(&stub.stop));
    let killer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().flow_polls == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        let _ = interrupt.send(2);
    });

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
    killer.join().unwrap();

    assert_eq!(err.to_string(), CANCEL_UNCONFIRMED);
    assert_eq!(
        err.downcast_ref::<crate::proxy_account::LoginInterrupted>()
            .map(crate::proxy_account::LoginInterrupted::signal),
        Some(2)
    );
    assert!(!stub.state().cancelled, "no cancel reached the proxy");
}

/// The cancel counts as confirmed on a 2xx or a 404 (the proxy no longer
/// knows the flow) and on nothing else: a 404 keeps the confirmed line, a 500
/// gets the unconfirmed line, and both exit 130.
#[test]
fn only_a_2xx_or_404_cancel_confirms_the_flow_stopped() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();

    for (answer, line) in [
        (err_body(404, "not_found"), CANCEL_CONFIRMED),
        (err_body(500, "internal"), CANCEL_UNCONFIRMED),
    ] {
        stub.state().cancel_refusal = Some(answer);
        let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
        let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
        signal.join().unwrap();
        assert_eq!(err.to_string(), line);
        let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
        let result = crate::cmd_login(proxy_login("px", None));
        signal.join().unwrap();
        assert_eq!(crate::exit_code(result), 130);
    }
}

/// Send `signal` through `interrupt` once the stub answered a poll beyond
/// those it had answered when called: the flow is open, so the signal lands
/// on the poll loop, never on the start.
fn signal_after_the_next_poll(
    stub: &Stub,
    interrupt: std::sync::mpsc::Sender<i32>,
    signal: i32,
) -> std::thread::JoinHandle<()> {
    let polled = stub.state().flow_polls;
    let state = Arc::clone(&stub.state);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().flow_polls == polled && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = interrupt.send(signal);
    })
}

/// Hold `request` on the stub, send `signal` through the doors seam once it
/// arrived, and run `clauth login px --proxy stub`: the login's result, and
/// whether the stub had let the held request through by the time it returned.
fn login_signalled_during(
    stub: &Stub,
    request: &str,
    signal: i32,
    interrupt: std::sync::mpsc::Sender<i32>,
) -> (anyhow::Result<String>, bool) {
    let (release, held) = std::sync::mpsc::channel();
    stub.state().hold = Some((request.to_string(), held));
    let state = Arc::clone(&stub.state);
    let sender = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = interrupt.send(signal);
    });
    let result = crate::login_proxy(&proxy_login("px", None), "stub");
    let answered = stub.state().released;
    sender.join().unwrap();
    drop(release);
    (result, answered)
}

/// Hold the login start on the stub, send SIGINT through the doors seam once
/// it arrived, and three door slices later either let the start through (the
/// proxy opens the flow after the signal) or send `second` with the start
/// still held. Runs `clauth login px --proxy stub`: the login's result, and
/// whether the stub had let the start through by the time it returned.
fn login_signalled_during_the_start(
    stub: &Stub,
    interrupt: std::sync::mpsc::Sender<i32>,
    second: Option<i32>,
) -> (anyhow::Result<String>, bool) {
    let (release, held) = std::sync::mpsc::channel::<()>();
    stub.state().hold = Some(("POST /clauth/v1/accounts/login".to_string(), held));
    let state = Arc::clone(&stub.state);
    let sender = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = interrupt.send(2);
        std::thread::sleep(Duration::from_millis(300));
        match second {
            Some(signal) => {
                let _ = interrupt.send(signal);
                Some(release)
            }
            None => {
                let _ = release.send(());
                None
            }
        }
    });
    let result = crate::login_proxy(&proxy_login("px", None), "stub");
    let released = stub.state().released;
    drop(sender.join().unwrap());
    (result, released)
}

/// A signal while the login start is in flight cannot leave the flow the
/// proxy opens once it handles the start: the login waits for the start's
/// answer and cancels the flow it names, printing line 4 when the proxy
/// confirms the cancel and the unconfirmed line when it does not. No poll ever
/// runs. Driven through the doors seam with a stub that opens the flow only
/// after the signal.
#[test]
fn a_signal_during_the_login_start_cancels_the_flow_the_start_opens() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();

    let (result, released) = login_signalled_during_the_start(&stub, doors.interrupt(), None);
    assert!(released, "the proxy opened the flow after the signal");
    let err = result.expect_err("interrupted");
    assert_eq!(err.to_string(), CANCEL_CONFIRMED);
    assert!(
        stub.state().cancelled,
        "the flow the start opened is cancelled"
    );
    assert_eq!(stub.state().flow_polls, 0, "no poll ran");

    stub.state().cancelled = false;
    stub.state().released = false;
    stub.state().cancel_refusal = Some(err_body(500, "internal"));
    let (result, released) = login_signalled_during_the_start(&stub, doors.interrupt(), None);
    assert!(released);
    assert_eq!(
        result.expect_err("interrupted").to_string(),
        CANCEL_UNCONFIRMED
    );
}

/// A second signal while the interrupted start still waits on the proxy ends
/// the login at once with `128 +` that signal, printing nothing more, as a
/// second signal during an interrupt's cancel does.
#[test]
fn a_second_signal_while_the_start_still_waits_exits_at_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();

    let (result, released) = login_signalled_during_the_start(&stub, doors.interrupt(), Some(15));

    assert!(
        !released,
        "the login returned while the proxy still held its start"
    );
    let err = result.expect_err("interrupted");
    assert!(
        err.downcast_ref::<crate::proxy_account::LoginInterrupted>()
            .is_none(),
        "no cancel line for a cancel that never ran"
    );
    assert_eq!(crate::exit_code(Err(err)), 143);
}

/// A signal while a poll is in flight cancels the flow at once, without
/// waiting for the poll's answer.
#[test]
fn a_signal_during_a_poll_in_flight_cancels_at_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();

    let (result, answered) = login_signalled_during(
        &stub,
        &format!("GET /clauth/v1/accounts/login/{FLOW}"),
        2,
        doors.interrupt(),
    );

    assert!(
        !answered,
        "the login returned while the proxy still held the poll"
    );
    assert_eq!(
        result.expect_err("interrupted").to_string(),
        CANCEL_CONFIRMED
    );
    assert!(stub.state().cancelled);
}

/// A signal while a pasted code is in flight cancels the flow at once,
/// without waiting for the paste's answer.
#[test]
fn a_signal_during_a_paste_in_flight_cancels_at_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();
    doors.paste().send("code-7".to_string()).unwrap();

    let (result, answered) = login_signalled_during(
        &stub,
        &format!("POST /clauth/v1/accounts/login/{FLOW}"),
        2,
        doors.interrupt(),
    );

    assert!(
        !answered,
        "the login returned while the proxy still held the paste"
    );
    assert_eq!(
        result.expect_err("interrupted").to_string(),
        CANCEL_CONFIRMED
    );
    assert!(stub.state().cancelled);
}

/// A signal while a failed login's own cancel is in flight exits at once with
/// `128 +` that signal, as a second signal does during an interrupt's cancel,
/// and the failure that started the cancel is still the line it prints: the
/// signal cut the cleanup short, not the reason the login failed. The started
/// cancel still reaches the proxy.
#[test]
fn a_signal_during_the_error_paths_cancel_exits_at_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().poll_refusal = Some(err_body(500, "internal"));
    let doors = crate::ProxyLoginDoors::capture_here();

    let (result, answered) = login_signalled_during(&stub, &cancel_request(), 2, doors.interrupt());

    assert!(
        !answered,
        "the login returned while the proxy still held the cancel"
    );
    let err = result.expect_err("interrupted");
    assert_eq!(
        err.to_string(),
        "proxy \"stub\" refused the request (HTTP 500); run `clauth proxy check stub` for details",
        "the refused poll is still the error"
    );
    // The line is the failure's `{:?}`, which carries a backtrace under
    // RUST_BACKTRACE, so it is taken off the failure itself.
    let line = format!(
        "Error: {:?}\n",
        err.downcast_ref::<crate::InterruptedAfter>()
            .expect("the failure, cut short by the signal")
            .error
    );
    let mut stderr = Vec::new();
    assert_eq!(
        crate::exit_code_to(Err(err), &mut stderr),
        130,
        "the signal sets the code"
    );
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        line,
        "the refused poll is still the line"
    );
    let deadline = std::time::Instant::now() + HOLD;
    while !stub.state().cancelled && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(stub.state().cancelled, "the started cancel still lands");
}

/// A live `clauth start` session on the proxy account refuses an unforced
/// delete before the irreversible upstream delete: the proxy is sent nothing.
#[test]
fn a_live_session_refuses_the_proxy_delete_before_the_upstream_delete() {
    let (home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let _session = crate::testutil::arm_live_session(home.home(), "px");
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the live session refuses");

    assert_eq!(
        err.to_string(),
        "'px' has a live session, pass --force to delete it anyway"
    );
    assert_eq!(
        stub.state().requests,
        Vec::<String>::new(),
        "the proxy is sent nothing"
    );
    assert_eq!(config_bytes("px"), before);
}

/// A readable client-token store in which `px` holds the last token of a
/// gateway that requires one: the last-token guard refuses before the
/// upstream delete, and the proxy is sent nothing.
#[test]
fn the_gateways_last_token_refuses_the_proxy_delete_before_the_upstream_delete() {
    let (home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let _env = crate::testutil::no_shunt_env(&home);
    let etc = home.home().join("etc");
    std::fs::create_dir_all(&etc).unwrap();
    std::fs::write(etc.join("shunt.toml"), "[server.auth]\n").unwrap();
    let env_file = home.home().join("tokens.env");
    std::fs::write(&env_file, "").unwrap();
    let mut record = crate::gateway::GatewayRecord::new(etc.join("shunt.toml")).unwrap();
    record.env_file = Some(env_file);
    crate::gateway::GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .unwrap();
    crate::gateway_tokens::add_client_token(&record, "px").unwrap();
    let store = std::fs::read_to_string(crate::gateway_tokens::store_path().unwrap()).unwrap();
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the last token refuses");

    assert_eq!(
        format!("{err:#}"),
        "cannot remove the last client token while the gateway requires tokens; add another \
         token or remove [server.auth] from the config first"
    );
    assert_eq!(
        stub.state().requests,
        Vec::<String>::new(),
        "the proxy is sent nothing"
    );
    assert_eq!(config_bytes("px"), before);
    assert_eq!(
        std::fs::read_to_string(crate::gateway_tokens::store_path().unwrap()).unwrap(),
        store,
        "the token stays"
    );
}

/// The paste prompt as the login prints it.
const PROMPT: &str = "clauth: paste the code from the proxy's login page (empty to wait): ";

/// A stdout recording every byte a login offers it. With `leaves_at_prompt`
/// its reader leaves once the paste prompt landed: every later write still
/// reaches the record, then fails `EPIPE`.
#[derive(Clone, Default)]
struct RecordedStdout {
    bytes: Arc<Mutex<Vec<u8>>>,
    leaves_at_prompt: bool,
}

impl RecordedStdout {
    fn text(&self) -> String {
        String::from_utf8(self.bytes.lock().unwrap().clone()).unwrap()
    }

    /// What the login offered after the prompt, `None` when it printed none.
    fn after_prompt(&self) -> Option<String> {
        self.text()
            .split_once(PROMPT)
            .map(|(_, after)| after.to_string())
    }
}

impl Write for RecordedStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut bytes = self.bytes.lock().unwrap();
        let gone = self.leaves_at_prompt && String::from_utf8_lossy(&bytes).contains(PROMPT);
        bytes.extend_from_slice(buf);
        if gone {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        } else {
            Ok(buf.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An exit that leaves the paste prompt's line open ends that line before
/// its own prints: a refused poll and a Ctrl-C here, each offering the login's
/// stdout one newline after the prompt. A login that showed no prompt ends
/// nothing, and neither does a success, whose line starts on its own already
/// (`a_browser_win_over_a_waiting_prompt_starts_the_success_line_on_its_own_line`).
#[test]
fn an_exit_over_an_open_paste_prompt_ends_its_line_first() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let login = || crate::login_proxy(&proxy_login("px", None), "stub");

    stub.state().poll_refusal = Some(err_body(500, "internal"));
    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout::default();
    doors.stdout(stdout.clone());
    let paste = doors.paste();
    let err = login().expect_err("the refused poll");
    drop(paste);
    assert_eq!(
        err.to_string(),
        "proxy \"stub\" refused the request (HTTP 500); run `clauth proxy check stub` for details"
    );
    assert_eq!(
        stdout.after_prompt().as_deref(),
        Some("\n"),
        "a refused poll ends the open line"
    );
    drop(doors);

    stub.state().poll_refusal = None;
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout::default();
    doors.stdout(stdout.clone());
    let paste = doors.paste();
    let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
    assert_eq!(
        login().expect_err("interrupted").to_string(),
        CANCEL_CONFIRMED
    );
    signal.join().unwrap();
    drop(paste);
    assert_eq!(
        stdout.after_prompt().as_deref(),
        Some("\n"),
        "a Ctrl-C ends the open line"
    );
    drop(doors);

    stub.state().poll_refusal = Some(err_body(500, "internal"));
    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout::default();
    doors.stdout(stdout.clone());
    login().expect_err("the refused poll");
    assert_eq!(stdout.after_prompt(), None, "no prompt");
    assert!(
        stdout.text().ends_with("https://stub.example/login\n\n"),
        "no line to end after the fallback url: {:?}",
        stdout.text()
    );
    drop(doors);

    stub.state().poll_refusal = None;
    stub.state().flow_states = vec![done_body(ACCOUNT, Some(KEY))];
    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout::default();
    doors.stdout(stdout.clone());
    let paste = doors.paste();
    assert!(
        login()
            .expect("the login finishes")
            .starts_with("\nclauth: captured")
    );
    drop(paste);
    assert_eq!(
        stdout.after_prompt().as_deref(),
        Some(""),
        "the success line ends it, once"
    );
}

/// A login interrupted or failed with the paste prompt open after stdout's
/// reader left (`clauth login px --proxy stub | tee log`, Ctrl-C at the
/// prompt) still ends as it does with the reader there: the prompt's closing
/// newline goes to the login's stdout, which reports the reader gone, never
/// through `outln!`, whose exit would end the run at 0 with no line; the
/// ending is still the login's own (line 4 and 130, or the `Error:` line and
/// the usage code 2).
#[test]
fn a_reader_gone_at_the_open_prompt_still_gets_the_ending_line_and_code() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let outln = crate::out::OutCapture::new();
    let _outln = outln.capture_here();

    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout {
        leaves_at_prompt: true,
        ..RecordedStdout::default()
    };
    doors.stdout(stdout.clone());
    let paste = doors.paste();
    let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
    signal.join().unwrap();
    drop(paste);
    assert_eq!(err.to_string(), CANCEL_CONFIRMED);
    let interrupted = err
        .downcast::<crate::proxy_account::LoginInterrupted>()
        .expect("the login's own ending");
    assert_eq!(interrupted.signal(), 2);
    assert_eq!(stdout.after_prompt().as_deref(), Some("\n"));
    assert!(stub.state().cancelled);
    drop(doors);

    stub.state().poll_refusal = Some(err_body(500, "internal"));
    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout {
        leaves_at_prompt: true,
        ..RecordedStdout::default()
    };
    doors.stdout(stdout.clone());
    let paste = doors.paste();
    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");
    drop(paste);
    let line = format!("Error: {err:?}\n");
    let mut stderr = Vec::new();
    assert_eq!(crate::exit_code_to(Err(err), &mut stderr), 2);
    assert_eq!(String::from_utf8(stderr).unwrap(), line);
    assert!(line.contains("refused the request (HTTP 500)"));
    assert_eq!(stdout.after_prompt().as_deref(), Some("\n"));
    assert_eq!(
        outln.snapshot(),
        Vec::<u8>::new(),
        "nothing went through outln!"
    );
}

const THREE_HOURS: i64 = 3 * 3600;

/// The cancel line for a signal after the proxy created the new account,
/// which clauth then deleted.
const LINE_DELETED: &str = "clauth: login cancelled; proxy \"stub\" had already created account \
                            \"acct-1\" and clauth deleted it";
/// The cancel line for the same signal when that delete failed.
const LINE_DELETE_FAILED: &str = "clauth: login cancelled after proxy \"stub\" created account \
                                  \"acct-1\", and deleting it failed; delete it on the proxy";
/// The cancel line for a signal after the proxy re-bound a re-login.
const LINE_REBOUND: &str = "clauth: login cancelled too late: proxy \"stub\" had already re-bound \
                            profile \"px\", which stays logged in";

/// `px` lands its chain 5h window at `pct`% through the real leg, every
/// figure read now but the chain window itself, read `chain_age` seconds ago,
/// and [`usage_body`]'s offer figure, read `offer_age` seconds ago; a figure
/// read in the past carries the proxy's `stale_reason`.
fn landing(
    pct: f64,
    chain_age: i64,
    offer_age: i64,
) -> (
    crate::testutil::HomeSandbox,
    Stub,
    crate::profile::AppConfig,
    SchedulerState,
) {
    let (home, stub, config) = proxy_profile_on_stub_built(|| {
        let now = crate::usage::now_epoch_secs();
        let at = |secs: i64| {
            chrono::DateTime::from_timestamp(secs, 0)
                .unwrap()
                .to_rfc3339()
        };
        let mut body = usage_body(4_000_000.0 * pct / 100.0, 100.0);
        for figure in body["accounts"][0]["figures"].as_array_mut().unwrap() {
            figure["read_at"] = json!(at(now));
        }
        for (figure, age) in [(0, chain_age), (3, offer_age)] {
            if age > 0 {
                body["accounts"][0]["figures"][figure]["read_at"] = json!(at(now - age));
                body["accounts"][0]["figures"][figure]["stale_reason"] =
                    json!("upstream_unavailable");
            }
        }
        body
    });
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);
    (home, stub, config, state)
}

/// Add the generic endpoint account `name` to `config`, its provider copy
/// written now with a 5h bar at 10%.
fn add_fresh_generic(config: &mut crate::profile::AppConfig, name: &str) {
    crate::testutil::register_names(&[name]);
    let profile = crate::profile::Profile::new(
        name.to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-other".to_string()),
    );
    crate::profile::save_profile(&profile).unwrap();
    config.add(profile);
    crate::profile_cache::write_profile_cache(
        &ProfileName::from(name),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![crate::providers::UsageBar {
                label: "5h".to_string(),
                pct: 10.0,
                resets_at: Some("2030-01-01T05:00:00Z".to_string()),
                used: None,
                total: None,
                inference_quota: false,
            }],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );
}

/// `clauth start --auto` keys a proxy member's freshness on its chain
/// windows: it starts on the first-in-chain proxy account `px`, whose 5h
/// window was read now, though an offer figure beside it is 3 h stale — and
/// the row still shows that age as stale. The same account whose 5h window
/// itself is 3 h old is passed over for the fresh `other` behind it.
#[test]
fn the_start_walk_starts_on_a_proxy_member_whose_chain_window_is_fresh() {
    let walk = |chain_age: i64, offer_age: i64| {
        let (_home, _stub, mut config, _state) = landing(10.0, chain_age, offer_age);
        add_fresh_generic(&mut config, "other");
        config.state.fallback_chain = vec!["px".into(), "other".into()];
        let (rows, pick) = crate::fallback::start_walk(
            &config,
            crate::runtime::Isolation::Shared,
            &[],
            [None, None, None],
            false,
            None,
        );
        let rows: Vec<(String, bool, bool)> = rows
            .iter()
            .map(|row| (row.name.to_string(), row.fresh, row.stale))
            .collect();
        (rows, pick)
    };
    let row = |name: &str, fresh: bool, stale: bool| (name.to_string(), fresh, stale);

    assert_eq!(
        walk(0, THREE_HOURS),
        (
            vec![row("px", true, true), row("other", true, false)],
            Some(0)
        ),
        "a fresh chain window starts the walk on px"
    );
    assert_eq!(
        walk(THREE_HOURS, 0),
        (
            vec![row("px", false, true), row("other", true, false)],
            Some(1)
        ),
        "a stale chain window is passed over"
    );
}

/// The daemon's 5h bell and window push judge a proxy member by its chain
/// windows: `px`'s 5h window read now at 97% rings the bell and queues the
/// window push though an offer figure beside it is 3 h stale, while the
/// published `stale` still says the copy's stalest figure is old. The same
/// account whose 5h window itself is 3 h old stays silent, under the status
/// the leg gave it and under a `Fresh` one alike, so the chain window's own
/// age is what silences it.
#[test]
fn the_alerts_ring_for_a_proxy_member_whose_chain_window_is_fresh() {
    let alerts = |chain_age: i64, offer_age: i64, forced: Option<FetchStatus>| {
        let (_home, _stub, mut config, state) = landing(97.0, chain_age, offer_age);
        config.state.bell_command = Some("notify-send %s".to_string());
        config.state.fallback_chain = vec!["px".into()];
        config
            .profiles
            .iter_mut()
            .find(|p| p.name.as_str() == "px")
            .unwrap()
            .bell_threshold = Some(90.0);
        let mut statuses = state.third_party_status.lock().unwrap().clone();
        if let Some(forced) = forced {
            statuses.insert("px".to_string(), forced);
        }
        crate::daemon::proxy_p6_feed_tests::alerts_on_one_tick(config, statuses, "px")
    };

    assert_eq!(
        alerts(0, THREE_HOURS, None),
        (
            vec!["alert: px at 97%".to_string()],
            vec![crate::push::Trigger::Window {
                profile: "px".to_string()
            }],
            true
        ),
        "a fresh chain window rings both, and the feed still publishes stale"
    );
    assert_eq!(
        alerts(THREE_HOURS, 0, None),
        (vec![], vec![], true),
        "a stale chain window rings neither"
    );
    assert_eq!(
        alerts(THREE_HOURS, 0, Some(FetchStatus::Fresh)),
        (vec![], vec![], true),
        "a stale chain window rings neither under a Fresh status either"
    );
}

/// The boot, takeover and stand-down seed takes a proxy copy's status off its
/// chain windows, as the leg that wrote the copy did: a copy whose 5h window
/// was read now seeds `Fresh` though an offer figure beside it is 3 h stale.
/// The control, a copy whose chain window itself is 3 h old seeding `Cached`,
/// is `a_dated_copy_reseeds_its_poll_on_the_legs_own_clock`.
#[test]
fn the_seed_takes_a_proxy_copys_status_off_its_chain_windows() {
    let (_home, _stub, config, state) = landing(10.0, 0, THREE_HOURS);
    assert_eq!(
        state.third_party_status.lock().unwrap().get("px").copied(),
        Some(FetchStatus::Fresh),
        "the leg's own verdict"
    );

    let (mirror, _due, _next) = seeded_mirror(&config, now_ms());

    assert_eq!(
        mirror.third_party_status.lock().unwrap().get("px").copied(),
        Some(FetchStatus::Fresh)
    );
}

/// The seed reads a chain date ahead of the clock (the clock stepped back
/// after the write) as undated: a copy whose chain windows are dated 1 h
/// ahead seeds `Cached`, never `Fresh`, nor the `Fresh` its 60 s old mtime
/// would earn.
#[test]
fn the_seed_never_takes_fresh_from_a_chain_date_ahead_of_the_clock() {
    let _home = crate::testutil::HomeSandbox::new();
    let px = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some(KEY.to_string()),
    );
    crate::profile::save_profile(&px).expect("save the proxy profile");
    crate::testutil::register_names(&["px"]);
    let now_s = crate::usage::now_epoch_secs();
    let ahead_ms = u64::try_from(now_s + 3600).unwrap() * 1000;
    assert!(crate::profile_cache::write_third_party_copy(
        &ProfileName::from("px"),
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]),
        Some(ahead_ms),
        ahead_ms,
        std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(now_s - 60).unwrap()),
    ));
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    config.add(px);

    let (mirror, _due, _next) = seeded_mirror(&config, now_ms());

    assert_eq!(
        mirror.third_party_status.lock().unwrap().get("px").copied(),
        Some(FetchStatus::Cached),
        "the seeded status of a copy whose chain date is 1 h ahead"
    );
}

/// The poll stamp a seed left for `name` in `state`.
fn seeded_stamp(state: &SchedulerState, name: &str) -> u64 {
    state
        .last_fetched
        .lock()
        .unwrap()
        .get(&FetchLeg::ThirdParty.key(ProfileName::from(name)))
        .copied()
        .expect("stamped")
        .as_millis()
}

/// The seed parses no profile's `config.toml` for a proxy record: an
/// ordinary third-party copy keeps its mtime as its poll stamp, and a proxy
/// copy dated 3 h back takes the write instant it records, so a stand-down
/// tick parses no record for either name.
#[test]
fn the_seed_reads_no_proxy_record_for_an_ordinary_third_party_copy() {
    let landed_from = now_ms();
    let (_home, _stub, mut config, _state) = landing(10.0, 0, THREE_HOURS);
    let landed_by = now_ms();
    add_fresh_generic(&mut config, "other");
    let mirror = leg_state();
    let seed = |name: &str| {
        seed_third_party_name(
            &mirror.third_party_usage_store,
            &mirror.third_party_status,
            &mirror.last_fetched,
            &ProfileName::from(name),
            true,
            now_ms(),
            90_000,
        );
    };
    let mtime = |name: &str| {
        profile_cache_mtime_ms(
            &ProfileName::from(name),
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        )
        .expect("the copy")
    };
    let before = crate::profile::proxy_claim_reads();

    seed("other");
    seed("px");

    assert_eq!(
        crate::profile::proxy_claim_reads() - before,
        0,
        "neither seed reads a proxy record"
    );
    assert_eq!(seeded_stamp(&mirror, "other"), mtime("other"));
    assert!(
        mtime("px") + 3_600_000 < landed_from,
        "px's copy is dated back"
    );
    let stamp = seeded_stamp(&mirror, "px");
    assert!(
        (landed_from..=landed_by).contains(&stamp),
        "a dated copy restarts its poll at its write, not {stamp} outside [{landed_from}, {landed_by}]"
    );
    crate::profile::stored_proxy_record(&ProfileName::from("px"));
    assert_eq!(
        crate::profile::proxy_claim_reads() - before,
        1,
        "the counter sees a record read"
    );
}

/// A write instant a copy records past the seed's `now` (the clock moved back
/// since) restarts the poll at `now`, never at that later date, and never at
/// the copy's dated-back mtime.
#[test]
fn a_recorded_write_past_now_restarts_the_poll_at_now() {
    const NOW: u64 = 1_800_000_000_000;
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let name = ProfileName::from("px");
    crate::profile_cache::write_third_party_copy(
        &name,
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]),
        Some(NOW - 60_000),
        NOW + 3_600_000,
        std::time::UNIX_EPOCH + Duration::from_millis(NOW - 3 * 3_600_000),
    );
    let mirror = leg_state();

    seed_third_party_name(
        &mirror.third_party_usage_store,
        &mirror.third_party_status,
        &mirror.last_fetched,
        &name,
        true,
        NOW,
        90_000,
    );

    assert_eq!(seeded_stamp(&mirror, "px"), NOW);
}

/// The walk reads a chain window's age in seconds: `px`'s 5h window read a
/// minute ago is 60 s old, under the 2 h stale line, so the walk starts on it
/// and its windows carry that minute as `chain_age_secs`.
#[test]
fn a_chain_window_read_a_minute_ago_is_fresh_to_the_walk() {
    let (_home, _stub, mut config, _state) = landing(10.0, 60, THREE_HOURS);
    add_fresh_generic(&mut config, "other");
    config.state.fallback_chain = vec!["px".into(), "other".into()];

    let crate::profile_json::ProfileWindows::ThirdParty { chain_age_secs, .. } =
        crate::profile_json::profile_windows_for(&ProfileName::from("px"))
    else {
        panic!("px reads a third-party copy");
    };
    assert!(
        chain_age_secs
            .flatten()
            .is_some_and(|age| (60..=70).contains(&age)),
        "a minute, not {chain_age_secs:?} s"
    );
    let (rows, pick) = crate::fallback::start_walk(
        &config,
        crate::runtime::Isolation::Shared,
        &[],
        [None, None, None],
        false,
        None,
    );
    assert_eq!(
        (rows[0].name.to_string(), rows[0].fresh, pick),
        ("px".to_string(), true, Some(0))
    );
}

/// A chain window whose `read_at` lies in the future is persisted at the
/// instant clauth wrote the copy, never at that date: every figure of
/// [`usage_body`] reads 2030-01-01T00:00:00Z (1_893_456_000_000 ms).
#[test]
fn a_future_chain_read_persists_as_the_write_instant() {
    let (_home, _stub, config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let before = now_ms();
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    let after = now_ms();

    let copy =
        crate::profile_cache::load_third_party_copy(&ProfileName::from("px")).expect("the copy");
    let written = copy.written_at.expect("the leg records its write");
    assert!(
        (before..=after).contains(&written),
        "written {written} outside [{before}, {after}]"
    );
    assert_eq!(copy.chain_read_at, Some(written));
}

/// A reading with two chain windows is dated by the older one: its 5h
/// window was read 2026-01-01T01:00:00Z and its 7d window, listed after it,
/// 2026-01-01T00:00:00Z, so the copy carries 1_767_225_600_000 ms.
#[test]
fn a_copy_with_two_chain_windows_carries_the_older_read() {
    let mut body = usage_body(500_000.0, 100.0);
    let figures = body["accounts"][0]["figures"].as_array_mut().unwrap();
    figures[0]["read_at"] = json!("2026-01-01T01:00:00Z");
    figures.push(json!({
        "kind": "window", "id": "plan-7d", "label": "plan 7d", "used": 1_000_000.0,
        "limit": 40_000_000.0, "unit": "tokens", "window_secs": 604_800,
        "resets_at": "2030-01-07T00:00:00Z", "chain": "7d",
        "read_at": "2026-01-01T00:00:00Z",
    }));
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);

    let copy =
        crate::profile_cache::load_third_party_copy(&ProfileName::from("px")).expect("the copy");
    assert_eq!(copy.chain_read_at, Some(1_767_225_600_000));
}

/// A signal that lands once the proxy finished the flow never reports a
/// stopped flow: the login reads the flow once more before cancelling, and a
/// `done` on a NEW account is deleted on the proxy (line 5), or said to need
/// deleting when the delete fails (line 6); the finished flow is never
/// cancelled and no profile is made. A flow still pending takes today's
/// cancel (line 4), after that one more read.
#[test]
fn a_signal_after_the_proxy_made_the_account_deletes_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let pending = json!({"flow": FLOW, "state": "pending"});
    let done = done_body(ACCOUNT, Some(KEY));
    let doors = crate::ProxyLoginDoors::capture_here();
    let login = |states: Vec<Value>| {
        {
            let mut state = stub.state();
            state.flow_states = states;
            state.flow_polls = 0;
            state.cancelled = false;
            state.deleted.clear();
        }
        let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);
        let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
        signal.join().unwrap();
        let state = stub.state();
        (
            err.to_string(),
            state.deleted.clone(),
            state.cancelled,
            state.flow_polls,
        )
    };

    assert_eq!(
        login(vec![pending.clone(), done.clone()]),
        (
            LINE_DELETED.to_string(),
            vec![ACCOUNT.to_string()],
            false,
            2
        )
    );
    stub.state().delete_refusal = Some(err_body(500, "internal"));
    assert_eq!(
        login(vec![pending.clone(), done]),
        (LINE_DELETE_FAILED.to_string(), vec![], false, 2)
    );
    stub.state().delete_refusal = None;
    assert_eq!(
        login(vec![pending]),
        (CANCEL_CONFIRMED.to_string(), vec![], true, 2),
        "a pending flow is cancelled after its one more read"
    );
    assert!(
        crate::profile::load_config()
            .unwrap()
            .find(&ProfileName::from("px"))
            .is_none(),
        "no profile made"
    );
}

/// A signal that lands once the proxy finished a RE-LOGIN never deletes: the
/// account is the profile's own, now bound to the new upstream login, so the
/// line says the profile stays logged in (line 7), and nothing is cancelled.
#[test]
fn a_signal_after_the_proxy_rebound_a_relogin_keeps_the_account() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![
        json!({"flow": FLOW, "state": "pending"}),
        done_body(ACCOUNT, None),
    ];
    let doors = crate::ProxyLoginDoors::capture_here();
    let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
    signal.join().unwrap();

    assert_eq!(err.to_string(), LINE_REBOUND);
    let state = stub.state();
    assert_eq!(
        state.login_bodies,
        vec![json!({"account": ACCOUNT})],
        "a re-login"
    );
    assert_eq!(state.deleted, Vec::<String>::new(), "no account delete");
    assert!(!state.cancelled);
    assert_eq!(state.flow_polls, 2);
}

/// A writer whose reader is gone by the time a chunk is flushed, the way a
/// prompt's `EPIPE` surfaces: a line goes through unflushed, a prompt does not.
struct FlushFails;

impl Write for FlushFails {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }
}

/// A stdout reader that leaves while the login's flow is open
/// (`clauth login px --proxy stub | head -0`) never ends the run with the flow
/// left polling upstream: the login cancels the flow first, then ends at exit
/// 0 by `out`'s rule, before any poll. The reader leaves before the progress
/// line (a closed pipe), or at the paste prompt. The flow expires in seconds,
/// so a login that kept going fails on that instead of polling on.
#[test]
fn a_stdout_reader_that_leaves_mid_login_cancels_the_flow_before_exit_0() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
        state.expires_at = chrono::DateTime::from_timestamp(crate::usage::now_epoch_secs() + 5, 0)
            .unwrap()
            .to_rfc3339();
    }
    let doors = crate::ProxyLoginDoors::capture_here();
    let left = || {
        let err =
            crate::login_proxy(&proxy_login("px", None), "stub").expect_err("the reader left");
        let state = stub.state();
        (
            err.downcast_ref::<crate::ReaderLeft>().is_some(),
            state.cancelled,
            state.flow_polls,
            crate::exit_code(Err(err)),
        )
    };

    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    doors.stdout(writer);
    assert_eq!(left(), (true, true, 0, 0), "gone before the progress line");

    stub.state().cancelled = false;
    doors.stdout(FlushFails);
    let paste = doors.paste();
    assert_eq!(left(), (true, true, 0, 0), "gone at the paste prompt");
    drop(paste);
}

// ── a login that leaves clauth keyless mints the account a key ──────────────

/// The success line for `name` on the core's captured account.
fn captured_line(name: &str, account: &str) -> String {
    format!(
        "clauth: captured into profile '{name}' (proxy \"stub\", account \"{account}\"). Switch \
         to it with:  clauth {name}"
    )
}

/// The stored key of profile `name`, read off disk.
fn stored_key(name: &str) -> Option<String> {
    crate::profile::load_profile(&ProfileName::from(name))
        .expect("the profile loads")
        .api_key
}

/// `name`'s stored key cleared on disk, its record and every other field kept.
fn clear_stored_key(name: &str) {
    let mut profile = crate::profile::load_profile(&ProfileName::from(name)).unwrap();
    profile.api_key = None;
    crate::profile::save_profile(&profile).unwrap();
}

/// A re-bind never carries a key (`flows.ts` `applyPollResult` mints only for
/// a new account), so a re-login of a profile that stores none mints one
/// through `POST /accounts/{id}/key` and saves it. A re-login of a profile
/// that stores a key keeps it and mints nothing: the mint replaces the
/// account's one key under every session using it.
#[test]
fn a_relogin_mints_a_key_only_for_a_profile_that_stores_none() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    clear_stored_key("px");
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(stub.state().minted, vec![ACCOUNT.to_string()]);
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(
        stub.state().minted,
        vec![ACCOUNT.to_string()],
        "a stored key is never minted over"
    );
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));
    assert_eq!(
        stub.state().login_bodies,
        vec![json!({"account": ACCOUNT}), json!({"account": ACCOUNT})],
        "both logins re-bound the stored account"
    );
}

/// A keyless re-bind of a profile that stores no key, landing on ANOTHER
/// account, mints nothing: that account's key belongs to another login, and a
/// mint would cut it. The re-login is refused and changes nothing.
#[test]
fn a_keyless_relogin_landing_on_another_account_mints_nothing() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    clear_stored_key("px");
    stub.state().flow_states = vec![done_body("acct-2", None)];
    let before = config_bytes("px");

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");

    assert_eq!(
        err.to_string(),
        "proxy \"stub\" logged 'px' in as account \"acct-2\", not its own account \"acct-1\"; \
         nothing was changed. Run `clauth proxy check stub` for details"
    );
    assert_eq!(stub.state().minted, Vec::<String>::new());
    assert_eq!(config_bytes("px"), before, "record and key unchanged");
}

/// A key another clauth stores into the profile while its keyless re-login
/// is open is kept: the login asks for the stored key once the flow is done,
/// so it mints nothing over it, and its save keeps that key.
#[test]
fn a_key_stored_during_a_relogin_is_never_minted_over() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    clear_stored_key("px");
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let (release, held) = std::sync::mpsc::channel::<()>();
    stub.state().hold = Some((format!("GET /clauth/v1/accounts/login/{FLOW}"), held));
    let state = Arc::clone(&stub.state);
    let writer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut profile = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
        profile.api_key = Some("clp_another-login".to_string());
        crate::profile::save_profile(&profile).unwrap();
        let _ = release.send(());
    });

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");
    writer.join().unwrap();

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(stub.state().minted, Vec::<String>::new());
    assert_eq!(stored_key("px").as_deref(), Some("clp_another-login"));
}

/// The poll answer that carried the key never arrived (the stub cuts the
/// connection on it): the flow is read once more, reads `done` without its
/// key, and the login mints one and lands it.
#[test]
fn a_login_whose_keyed_answer_was_lost_mints_its_key() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![
            core(CORE_PENDING),
            core(CORE_DONE_WITH_KEY),
            core(CORE_DONE_LATER),
        ];
        state.poll_answers = vec![(1, NO_ANSWER.to_string())];
    }

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");

    assert_eq!(line, captured_line("px", CORE_ACCOUNT));
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));
    let state = stub.state();
    assert_eq!(
        (
            state.minted.clone(),
            state.flow_polls,
            state.cancelled,
            state.deleted.clone()
        ),
        (vec![CORE_ACCOUNT.to_string()], 3, false, vec![])
    );
}

/// A lost poll answer on a flow still pending is no failure: the read after
/// it sees `pending`, and the login polls on to the key.
#[test]
fn a_lost_poll_answer_on_a_pending_flow_keeps_polling() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![
            core(CORE_PENDING),
            core(CORE_PENDING),
            core(CORE_PENDING),
            core(CORE_DONE_WITH_KEY),
        ];
        state.poll_answers = vec![(1, NO_ANSWER.to_string())];
    }

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");

    assert_eq!(line, captured_line("px", CORE_ACCOUNT));
    assert_eq!(stored_key("px").as_deref(), Some(CORE_KEY));
    let state = stub.state();
    assert_eq!(
        (state.minted.clone(), state.flow_polls, state.cancelled),
        (vec![], 4, false)
    );
}

/// A lost poll answer whose flow then reads gone fails as the lost read
/// always did: the proxy did not answer, and the flow is cancelled.
#[test]
fn a_lost_poll_answer_on_a_gone_flow_fails_as_unanswered() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().poll_answers = vec![(0, NO_ANSWER.to_string()), (1, err_body(404, "not_found"))];

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("the login fails");

    assert_eq!(
        err.to_string(),
        "proxy \"stub\" did not answer; run `clauth proxy check stub` for details"
    );
    let state = stub.state();
    assert_eq!(
        (state.minted.clone(), state.flow_polls, state.cancelled),
        (vec![], 2, true)
    );
}

/// A paste refused 409 meets a flow that is no longer pending, which the
/// browser door may have finished: the flow is read once, and a `done` ends
/// the login, keyed by that read when it carries the key nobody took
/// ([`CORE_PASTE_CONFLICT`]'s capture) and by a mint when another reader took
/// it. A flow still pending fails as the refused paste did.
#[test]
fn a_paste_refused_409_reads_the_flow_once_and_keeps_its_done() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    let login = |name: &str, after_paste: Value| {
        {
            let mut state = stub.state();
            state.flow_states = vec![core(CORE_PENDING), after_paste];
            state.flow_polls = 0;
            state.paste_refusal = Some(reply(409, core(CORE_PASTE_CONFLICT)));
            state.pastes_seen.clear();
            state.minted.clear();
            state.cancelled = false;
        }
        doors.paste().send("code-7".to_string()).unwrap();
        let result = crate::login_proxy(&proxy_login(name, None), "stub");
        let state = stub.state();
        (
            result.map_err(|e| e.to_string()),
            state.pastes_seen.clone(),
            state.flow_polls,
            state.minted.clone(),
            state.cancelled,
        )
    };

    assert_eq!(
        login("px", core(CORE_DONE_LATER)),
        (
            Ok(captured_line("px", CORE_ACCOUNT)),
            vec!["code-7".to_string()],
            2,
            vec![CORE_ACCOUNT.to_string()],
            false
        )
    );
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));

    assert_eq!(
        login(
            "py",
            core(&CORE_DONE_WITH_KEY.replace(CORE_ACCOUNT, "wc5cdzkad0pt"))
        ),
        (
            Ok(captured_line("py", "wc5cdzkad0pt")),
            vec!["code-7".to_string()],
            2,
            vec![],
            false
        )
    );
    assert_eq!(stored_key("py").as_deref(), Some(CORE_KEY));

    assert_eq!(
        login("pz", core(CORE_PENDING)),
        (
            Err(
                "proxy \"stub\" refused the request (HTTP 409); run `clauth proxy check stub` \
                 for details"
                    .to_string()
            ),
            vec!["code-7".to_string()],
            2,
            vec![],
            true
        )
    );
    assert!(
        crate::profile::load_config()
            .unwrap()
            .find(&ProfileName::from("pz"))
            .is_none(),
        "no profile made"
    );
}

/// A mint that fails ends the login as a failure and writes no keyless
/// profile: a refused mint names its status, an unanswered one says so, and
/// a mint answer without a key, or with a blank one, is a login without one. The new account stays
/// on the proxy as any failed login after `done` leaves it (no cancel, no
/// delete); a re-login's profile stays byte for byte as it was.
#[test]
fn a_mint_that_fails_ends_the_login_and_writes_no_keyless_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let login = |name: &str, done: Value, refusal: String| {
        {
            let mut state = stub.state();
            state.flow_states = vec![done];
            state.key_refusal = Some(refusal);
            state.minted.clear();
        }
        let err = crate::login_proxy(&proxy_login(name, None), "stub").expect_err("refused");
        let state = stub.state();
        (
            err.to_string(),
            state.minted.clone(),
            state.cancelled,
            state.deleted.clone(),
        )
    };
    let failed = |line: &str, account: &str| {
        (
            line.to_string(),
            vec![account.to_string()],
            false,
            Vec::<String>::new(),
        )
    };
    let refused_500 =
        "proxy \"stub\" refused the request (HTTP 500); run `clauth proxy check stub` for details";

    assert_eq!(
        login("px", core(CORE_DONE_LATER), err_body(500, "internal")),
        failed(refused_500, CORE_ACCOUNT)
    );
    assert_eq!(
        login("px", core(CORE_DONE_LATER), NO_ANSWER.to_string()),
        failed(
            "proxy \"stub\" did not answer; run `clauth proxy check stub` for details",
            CORE_ACCOUNT
        )
    );
    let keyless = "proxy \"stub\" completed the login without an inference key; run `clauth \
                   proxy check stub` for details";
    assert_eq!(
        login("px", core(CORE_DONE_LATER), reply(200, json!({}))),
        failed(keyless, CORE_ACCOUNT)
    );
    assert_eq!(
        login(
            "px",
            core(CORE_DONE_LATER),
            reply(200, json!({"inference_key": " "}))
        ),
        failed(keyless, CORE_ACCOUNT),
        "a blank key is none"
    );
    assert!(
        crate::profile::load_config()
            .unwrap()
            .find(&ProfileName::from("px"))
            .is_none(),
        "no profile made"
    );

    let mut config = crate::profile::load_config().unwrap();
    create_proxy_profile_on_disk(&mut config, "pq", &stub.base);
    clear_stored_key("pq");
    let before = config_bytes("pq");
    assert_eq!(
        login("pq", done_body(ACCOUNT, None), err_body(500, "internal")),
        failed(refused_500, ACCOUNT)
    );
    assert_eq!(config_bytes("pq"), before, "the re-login changed nothing");
}

/// A signal while the mint is in flight ends the login as a signal after
/// `done` does: the flow is read once more, and the new account it names is
/// deleted (line 5), without waiting for the mint's answer.
#[test]
fn a_signal_during_the_mint_deletes_the_new_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let doors = crate::ProxyLoginDoors::capture_here();

    let (result, answered) = login_signalled_during(
        &stub,
        &format!("POST /clauth/v1/accounts/{ACCOUNT}/key"),
        2,
        doors.interrupt(),
    );

    assert!(
        !answered,
        "the login returned while the proxy still held the mint"
    );
    assert_eq!(result.expect_err("interrupted").to_string(), LINE_DELETED);
    let state = stub.state();
    assert_eq!(
        (state.deleted.clone(), state.cancelled, state.flow_polls),
        (vec![ACCOUNT.to_string()], false, 2)
    );
}

/// Hold `request` on the stub once `from_poll` flow polls were answered, and
/// run `clauth login px --proxy stub` while a sender sends SIGINT after the
/// first poll and SIGTERM once the held request arrived: the login's result,
/// and whether the stub had let the held request through by the time it
/// returned.
fn login_signalled_twice(
    stub: &Stub,
    doors: &crate::ProxyLoginDoors,
    request: &str,
    from_poll: usize,
) -> (anyhow::Result<String>, bool) {
    let (release, held) = std::sync::mpsc::channel::<()>();
    {
        let mut state = stub.state();
        state.hold_from_poll = from_poll;
        state.hold = Some((request.to_string(), held));
    }
    let interrupt = doors.interrupt();
    let state = Arc::clone(&stub.state);
    let sender = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().flow_polls == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = interrupt.send(2);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = interrupt.send(15);
    });
    let result = crate::login_proxy(&proxy_login("px", None), "stub");
    let released = stub.state().released;
    sender.join().unwrap();
    drop(release);
    (result, released)
}

/// A second signal while an interrupted login's extra read of the flow, or
/// its delete of the account a `done` flow created, is in flight ends the
/// login at once with `128 +` that signal and nothing more to print, as a
/// second signal during its cancel does: neither held request is waited out.
/// The delete already sent still lands once the proxy answers.
#[test]
fn a_second_signal_during_the_extra_read_or_the_delete_exits_at_once() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    let second = |result: anyhow::Result<String>| {
        let err = result.expect_err("interrupted");
        (
            err.downcast_ref::<crate::proxy_account::LoginInterrupted>()
                .is_some(),
            crate::exit_code(Err(err)),
        )
    };

    stub.state().flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
    let (result, released) = login_signalled_twice(
        &stub,
        &doors,
        &format!("GET /clauth/v1/accounts/login/{FLOW}"),
        1,
    );
    assert!(
        !released,
        "the login returned while the extra read was held"
    );
    assert_eq!(second(result), (false, 143), "the extra read");
    assert!(!stub.state().cancelled, "no cancel after the second signal");
    // The held read lands once released, after the login: wait it out, so it
    // counts no poll of the next login.
    let deadline = std::time::Instant::now() + HOLD;
    while !stub.state().released && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    {
        let mut state = stub.state();
        state.flow_states = vec![
            json!({"flow": FLOW, "state": "pending"}),
            done_body(ACCOUNT, Some(KEY)),
        ];
        state.flow_polls = 0;
        state.held = None;
        state.released = false;
    }
    let (result, released) = login_signalled_twice(
        &stub,
        &doors,
        &format!("DELETE /clauth/v1/accounts/{ACCOUNT}"),
        0,
    );
    assert!(!released, "the login returned while the delete was held");
    assert_eq!(second(result), (false, 143), "the delete");
    // The delete was already on the wire: once the proxy answers it, it lands.
    // The login did not wait for that answer (`released` above).
    let deadline = std::time::Instant::now() + HOLD;
    while !stub.state().released && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(stub.state().deleted, vec![ACCOUNT.to_string()]);
}

/// `/health` as a foreign answerer gives it.
fn foreign_health() -> Value {
    json!({"status": "ok", "service": "other", "version": "0.0.1", "contract": "1.0"})
}

/// The identity gate runs before the key mint as before every poll: a bind
/// another answerer took over right after the poll that read a keyless `done`
/// is sent no mint, and the login stops on the mid-login line, saving
/// nothing.
#[test]
fn a_bind_swapped_out_before_the_mint_gets_no_bearer_request() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![done_body(ACCOUNT, None)];
        state.health_after_poll = Some(foreign_health());
    }

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");

    assert_eq!(
        err.to_string(),
        identity_refusal(
            stub.port,
            "a proxy answers as service \"other\"",
            MID_LOGIN_REFUSED
        )
    );
    let state = stub.state();
    assert_eq!(state.minted, Vec::<String>::new());
    assert_eq!(
        state.bearered,
        vec![
            "POST /clauth/v1/accounts/login".to_string(),
            format!("GET /clauth/v1/accounts/login/{FLOW}"),
        ]
    );
    assert!(
        crate::profile::load_config()
            .unwrap()
            .find(&ProfileName::from("px"))
            .is_none(),
        "no profile made"
    );
}

/// The identity gate runs before a pasted code as before every poll: a bind
/// another answerer took over right after a poll is sent neither the code nor
/// the cancel, and the login stops on the mid-login line.
#[test]
fn a_bind_swapped_out_before_the_paste_gets_no_bearer_request() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
        state.health_after_poll = Some(foreign_health());
    }
    let doors = crate::ProxyLoginDoors::capture_here();
    doors.paste().send("code-7".to_string()).unwrap();

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");

    assert_eq!(
        err.to_string(),
        identity_refusal(
            stub.port,
            "a proxy answers as service \"other\"",
            MID_LOGIN_REFUSED
        )
    );
    let state = stub.state();
    assert_eq!(state.pastes_seen, Vec::<String>::new());
    assert_eq!(
        state.bearered,
        vec![
            "POST /clauth/v1/accounts/login".to_string(),
            format!("GET /clauth/v1/accounts/login/{FLOW}"),
        ]
    );
    assert!(!state.cancelled);
}

/// A Claude Code background job on a tree under `px` is a live session for
/// the proxy delete's hold too (`has_live_session` counts job trees beside
/// the markers): an unforced delete refuses before the irreversible upstream
/// delete, and the proxy is sent nothing.
#[test]
fn a_live_background_job_refuses_the_proxy_delete_before_the_upstream_delete() {
    let (home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let runtime = crate::profile::profile_dir(&ProfileName::from("px"))
        .unwrap()
        .join("runtime-4242-0");
    std::fs::create_dir_all(&runtime).unwrap();
    crate::testutil::write_claude_job(home.home(), "37560ded", &runtime);
    let before = config_bytes("px");

    let err = crate::cmd_delete("px", true, false).expect_err("the live job refuses");

    assert_eq!(
        err.to_string(),
        "'px' has a live session, pass --force to delete it anyway"
    );
    assert_eq!(
        stub.state().requests,
        Vec::<String>::new(),
        "the proxy is sent nothing"
    );
    assert_eq!(config_bytes("px"), before);
}

/// The two chain windows of `a_copy_with_two_chain_windows_carries_the_older_read`
/// listed the other way round, the older one first: the copy still carries
/// the older read, 2026-01-01T00:00:00Z (1_767_225_600_000 ms), never the one
/// listed last.
#[test]
fn a_copy_with_two_chain_windows_listed_older_first_carries_the_older_read() {
    let mut body = usage_body(500_000.0, 100.0);
    let figures = body["accounts"][0]["figures"].as_array_mut().unwrap();
    figures[0]["read_at"] = json!("2026-01-01T00:00:00Z");
    figures.push(json!({
        "kind": "window", "id": "plan-7d", "label": "plan 7d", "used": 1_000_000.0,
        "limit": 40_000_000.0, "unit": "tokens", "window_secs": 604_800,
        "resets_at": "2030-01-07T00:00:00Z", "chain": "7d",
        "read_at": "2026-01-01T01:00:00Z",
    }));
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);

    let copy =
        crate::profile_cache::load_third_party_copy(&ProfileName::from("px")).expect("the copy");
    assert_eq!(copy.chain_read_at, Some(1_767_225_600_000));
}

/// A copy write that fails leaves the old copy as it was, bytes and mtime:
/// the dated copy lands in one write carrying its mtime, so a failure never
/// re-dates the old figures to a reading they are not. The fault
/// is a profile dir that refuses new files, so the write's temp file cannot
/// be made; the second leg did read the proxy's new figures.
#[test]
fn a_failed_copy_write_leaves_the_old_copy_and_its_mtime() {
    let (_home, stub, config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    let dir = crate::profile::profile_dir(&ProfileName::from("px")).unwrap();
    let path = dir.join(crate::profile_cache::THIRD_PARTY_CACHE_FILE);
    // 2026-01-01T00:00:00Z
    let old_mtime = std::time::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
    crate::testutil::set_mtime(&path, old_mtime);
    let old_bytes = std::fs::read(&path).unwrap();
    let hour_ago = chrono::DateTime::from_timestamp(crate::usage::now_epoch_secs() - 3600, 0)
        .unwrap()
        .to_rfc3339();
    let mut body = usage_body(900_000.0, 50.0);
    for figure in body["accounts"][0]["figures"].as_array_mut().unwrap() {
        figure["read_at"] = json!(hour_ago);
    }
    stub.state().usage = body;

    let unwritable = crate::testutil::UnwritableDir::new(&dir);
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    drop(unwritable);

    assert_eq!(
        stub.state()
            .requests
            .iter()
            .filter(|r| *r == "GET /clauth/v1/usage")
            .count(),
        2,
        "the second leg read the new figures"
    );
    assert_eq!(std::fs::read(&path).unwrap(), old_bytes, "the old bytes");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        old_mtime,
        "the old mtime"
    );
}

/// A leg whose copy write fails still judges the reading it read, never the
/// copy left on disk: a reading with no chain window earns its status off its
/// own stalest figure. Read now, it is `Fresh` beside an old copy dated 3 h
/// back; read 3 h ago with no copy on disk at all, it is `Cached`. The fault
/// is a profile dir that refuses new files.
#[test]
fn a_failed_copy_write_judges_the_reading_the_leg_holds() {
    let at = |secs: i64| {
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .to_rfc3339()
    };
    let chainless = |read_at: i64| {
        let mut body = usage_body(500_000.0, 100.0);
        for figure in body["accounts"][0]["figures"].as_array_mut().unwrap() {
            let _ = figure.as_object_mut().unwrap().remove("chain");
            figure["read_at"] = json!(at(read_at));
        }
        body
    };
    let (_home, stub, config) =
        proxy_profile_on_stub_built(|| chainless(crate::usage::now_epoch_secs() - 3 * 3600));
    let now = crate::usage::now_epoch_secs();
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    let dir = crate::profile::profile_dir(&ProfileName::from("px")).unwrap();
    let path = dir.join(crate::profile_cache::THIRD_PARTY_CACHE_FILE);
    assert!(path.exists(), "fixture: the old copy landed");
    let failed_leg = |reading: Value| {
        stub.state().usage = reading;
        let state = leg_state();
        let unwritable = crate::testutil::UnwritableDir::new(&dir);
        crate::usage::reset_request_slots();
        run_leg(&state, &config);
        drop(unwritable);
        state.third_party_status.lock().unwrap().get("px").copied()
    };

    assert_eq!(
        failed_leg(chainless(now)),
        Some(FetchStatus::Fresh),
        "a reading read now, its write failed beside a copy 3 h old"
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        failed_leg(chainless(now - 3 * 3600)),
        Some(FetchStatus::Cached),
        "a reading read 3 h ago, its write failed with no copy on disk"
    );
    assert!(
        !path.exists(),
        "fixture: neither failed write landed a copy"
    );
}

/// A re-login of the ACTIVE proxy profile that stores no key mints one and
/// wires it into the live Claude settings, as an endpoint edit on the active
/// profile does: the keyless profile was wired with no key helper, so without
/// the re-apply the global session kept no key behind the success line.
#[test]
fn an_active_keyless_relogin_wires_the_minted_key_into_the_live_settings() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    config.state.active_profile = Some("px".into());
    crate::profile::save_app_state(&config.state).unwrap();
    clear_stored_key("px");
    let keyless = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    crate::claude::apply_profile_to_claude_settings(&keyless, &[]).unwrap();
    let live_key = || {
        crate::claude::read_claude_endpoint_config()
            .unwrap()
            .api_key
    };
    assert_eq!(
        live_key(),
        None,
        "fixture: the keyless profile is wired keyless"
    );
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(stub.state().minted, vec![ACCOUNT.to_string()]);
    assert_eq!(live_key().as_deref(), Some(CORE_MINTED_KEY));
}

/// A paste whose answer never arrived whole may have been the one answer
/// carrying the key of the account it completed, as a lost poll answer may
/// be: the flow is read once more. A `done` ends the login, minting the key
/// the lost answer took; a flow still pending keeps polling; a flow that
/// reads gone fails as unanswered, and its flow is cancelled.
#[test]
fn a_lost_paste_answer_reads_the_flow_once_more() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    let login = |name: &str, states: Vec<Value>, answers: Vec<(usize, String)>| {
        {
            let mut state = stub.state();
            state.flow_states = states;
            state.poll_answers = answers;
            state.flow_polls = 0;
            state.paste_refusal = Some(NO_ANSWER.to_string());
            state.pastes_seen.clear();
            state.minted.clear();
            state.cancelled = false;
        }
        doors.paste().send("code-7".to_string()).unwrap();
        let result = crate::login_proxy(&proxy_login(name, None), "stub");
        let state = stub.state();
        (
            result.map_err(|e| e.to_string()),
            state.pastes_seen.clone(),
            state.flow_polls,
            state.minted.clone(),
            state.cancelled,
        )
    };

    assert_eq!(
        login(
            "px",
            vec![core(CORE_PENDING), core(CORE_DONE_LATER)],
            vec![]
        ),
        (
            Ok(captured_line("px", CORE_ACCOUNT)),
            vec!["code-7".to_string()],
            2,
            vec![CORE_ACCOUNT.to_string()],
            false
        ),
        "done: the key is minted"
    );
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));

    assert_eq!(
        login(
            "py",
            vec![
                core(CORE_PENDING),
                core(CORE_PENDING),
                core(&CORE_DONE_WITH_KEY.replace(CORE_ACCOUNT, "wc5cdzkad0pt"))
            ],
            vec![]
        ),
        (
            Ok(captured_line("py", "wc5cdzkad0pt")),
            vec!["code-7".to_string()],
            3,
            vec![],
            false
        ),
        "pending: the poll goes on"
    );
    assert_eq!(stored_key("py").as_deref(), Some(CORE_KEY));

    assert_eq!(
        login(
            "pz",
            vec![core(CORE_PENDING)],
            vec![(1, err_body(404, "not_found"))]
        ),
        (
            Err(
                "proxy \"stub\" did not answer; run `clauth proxy check stub` for details"
                    .to_string()
            ),
            vec!["code-7".to_string()],
            2,
            vec![],
            true
        ),
        "gone: unanswered"
    );
}

/// A paste the proxy answers 503 `upstream_unavailable` (the core could not
/// check the code upstream, and keeps the flow pending with its poll door
/// running) keeps the login alive: one line on stderr says so, once, nothing
/// more prints on stdout, the poll goes on, and the browser door's `done`
/// still finishes the login. The flow is never cancelled.
#[test]
fn a_paste_answered_503_keeps_the_login_polling() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![
            core(CORE_PENDING),
            core(CORE_PENDING),
            core(CORE_DONE_WITH_KEY),
        ];
        state.paste_refusal = Some(err_body(503, "upstream_unavailable"));
    }
    let doors = crate::ProxyLoginDoors::capture_here();
    let stdout = RecordedStdout::default();
    doors.stdout(stdout.clone());
    let stderr = RecordedStdout::default();
    doors.stderr(stderr.clone());
    doors.paste().send("code-7".to_string()).unwrap();

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");

    assert_eq!(line, captured_line("px", CORE_ACCOUNT));
    assert_eq!(stored_key("px").as_deref(), Some(CORE_KEY));
    let state = stub.state();
    assert_eq!(
        (state.pastes_seen.clone(), state.flow_polls, state.cancelled),
        (vec!["code-7".to_string()], 3, false)
    );
    assert_eq!(
        stderr.text(),
        "clauth: proxy \"stub\" could not check that code (HTTP 503); finish signing in in the \
         browser, or press Ctrl-C and run the login again\n",
        "the 503 is said once, on stderr"
    );
    assert_eq!(
        stdout.after_prompt().as_deref(),
        Some(""),
        "nothing more prints on stdout"
    );
}

/// The login paces its polls at the start's `poll_interval_ms`, floored at
/// the contract's 1000 ms and capped at 30 s, under the 60 s a proxy keeps a
/// finished flow before forgetting it (`flows.ts` `TERMINAL_GRACE_MS`): a
/// longer wait could let a `done` flow, and the key it holds, vanish between
/// two polls.
#[test]
fn the_poll_interval_hint_is_floored_and_capped() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let control = stub_control();
    let paced = |hint: u64| {
        stub.state().poll_interval_ms = hint;
        let flow =
            crate::proxy_account::start_login(&control, None, &mut || None).expect("flow opens");
        flow.start().poll_interval
    };

    assert_eq!(paced(999), Duration::from_millis(1000));
    assert_eq!(paced(1000), Duration::from_millis(1000));
    assert_eq!(paced(2500), Duration::from_millis(2500));
    assert_eq!(paced(30_000), Duration::from_secs(30));
    assert_eq!(paced(30_001), Duration::from_secs(30));
    assert_eq!(paced(120_000), Duration::from_secs(30));
}

/// Hold the first flow poll of a login and run `edit` on the disk while it is
/// held, then let the poll through.
fn while_the_first_poll_waits(
    stub: &Stub,
    edit: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let (release, held) = std::sync::mpsc::channel::<()>();
    stub.state().hold = Some((format!("GET /clauth/v1/accounts/login/{FLOW}"), held));
    let state = Arc::clone(&stub.state);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // A hold that never took the poll would run the edit after the login
        // instead of during it, which every save passes.
        assert!(
            state.lock().unwrap().held.is_some(),
            "the hold took the first poll before the edit"
        );
        edit();
        let _ = release.send(());
    })
}

/// Another clauth creates `px` while a new proxy login of `px` waits on the
/// browser. Returns the login's error text and the account ids the proxy saw
/// deleted.
fn a_new_login_racing_a_create_of_its_name(stub: &Stub) -> (String, Vec<String>) {
    let writer = while_the_first_poll_waits(stub, || {
        let mut config = crate::profile::load_config().unwrap();
        crate::actions::create_blank_profile(
            &mut config,
            "px".to_string(),
            Some("https://api.example.com".to_string()),
            Some("sk-other".to_string()),
            crate::profile::ModelSettings::default(),
            None,
        )
        .unwrap();
    });
    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");
    writer.join().unwrap();
    (err.to_string(), stub.state().deleted.clone())
}

/// A profile another clauth creates under the name while a new proxy login
/// waits on the browser is refused, never overwritten: the login reads the
/// roster again under the flock when it saves, and the other profile keeps
/// its endpoint and key, with no proxy record written into it. The account
/// the proxy made for the refused login is deleted, so no provider login
/// sits on the proxy with no profile behind it.
#[test]
fn a_profile_created_during_a_new_login_is_refused_never_overwritten() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);

    assert_eq!(
        a_new_login_racing_a_create_of_its_name(&stub),
        (
            "a profile named 'px' already exists; clauth deleted the account proxy \"stub\" \
             had just created (\"acct-1\")"
                .to_string(),
            vec![ACCOUNT.to_string()]
        )
    );
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (px.base_url.as_deref(), px.api_key.as_deref()),
        (Some("https://api.example.com"), Some("sk-other"))
    );
    assert!(matches!(
        crate::profile::stored_proxy_claim(&ProfileName::from("px")),
        crate::profile::ProxyClaim::None
    ));
}

/// The same refused save, with the proxy refusing the delete: the line says
/// the account is still there and where to remove it, and the other
/// clauth's `px` stands untouched.
#[test]
fn a_refused_new_login_whose_delete_fails_names_the_account_left_on_the_proxy() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    stub.state().delete_refusal = Some(err_body(500, "internal"));

    assert_eq!(
        a_new_login_racing_a_create_of_its_name(&stub),
        (
            "a profile named 'px' already exists, and deleting the account proxy \"stub\" had \
             just created (\"acct-1\") failed; delete it on the proxy"
                .to_string(),
            vec![]
        )
    );
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (px.base_url.as_deref(), px.api_key.as_deref()),
        (Some("https://api.example.com"), Some("sk-other"))
    );
}

/// A switch and a chain edit landing while a new proxy login waits on the
/// browser (a daemon auto-switch) stand after it: the login saves the roster
/// on disk plus its own name, never the copy it loaded before the flow opened.
#[test]
fn a_switch_during_a_new_login_survives_its_save() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let mut config = crate::profile::load_config().unwrap();
    add_fresh_generic(&mut config, "other");
    crate::profile::save_app_state(&config.state).unwrap();
    let writer = while_the_first_poll_waits(&stub, || {
        let mut state = crate::profile::load_app_state().unwrap();
        state.active_profile = Some("other".into());
        state.fallback_chain = vec!["other".into()];
        crate::profile::save_app_state(&state).unwrap();
    });

    crate::login_proxy(&proxy_login("px", None), "stub").expect("the login finishes");
    writer.join().unwrap();

    let state = crate::profile::load_app_state().unwrap();
    assert_eq!(
        (
            state.profiles.clone(),
            state.active_profile.clone(),
            state.fallback_chain.clone()
        ),
        (
            vec!["other".into(), "px".into()],
            Some("other".into()),
            vec!["other".into()]
        )
    );
}

/// A field another writer changes on the profile while its proxy re-login
/// waits on the browser stands after it: the re-login saves the profile read
/// again under the flock, never the copy it loaded before the flow opened.
#[test]
fn a_field_changed_during_a_relogin_survives_its_save() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let writer = while_the_first_poll_waits(&stub, || {
        let mut px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
        px.fallback_threshold = Some(80.0);
        crate::profile::save_profile_config(&px).unwrap();
    });

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");
    writer.join().unwrap();

    assert_eq!(line, captured_line("px", ACCOUNT));
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (px.fallback_threshold, px.api_key.as_deref()),
        (Some(80.0), Some(KEY))
    );
}

/// The live Claude settings as the global session reads them: the endpoint,
/// the key its helper resolves, and the top-level default model.
fn live_settings() -> (Option<String>, Option<String>, Option<String>) {
    let endpoint = crate::claude::read_claude_endpoint_config().unwrap();
    let path = crate::claude::claude_settings_path().unwrap();
    let model = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|settings| settings["model"].as_str().map(str::to_string));
    (endpoint.base_url, endpoint.api_key, model)
}

/// `name`'s stored profile made the active one, its settings live: what a
/// switch to it leaves on disk.
fn switch_to(name: &str) {
    let mut state = crate::profile::load_app_state().unwrap();
    state.active_profile = Some(name.into());
    crate::profile::save_app_state(&state).unwrap();
    let profile = crate::profile::load_profile(&ProfileName::from(name)).unwrap();
    crate::claude::apply_profile_to_claude_settings(&profile, &[]).unwrap();
}

/// A `--model` re-login of the active proxy profile, during which a switch to
/// `other` lands, leaves the live settings to `other`: the model lands on the
/// profile alone, and the re-apply reads the active marker saved meanwhile,
/// never the one loaded before the flow opened.
#[test]
fn a_switch_away_during_a_model_relogin_leaves_the_live_settings_to_the_new_active() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    add_fresh_generic(&mut config, "other");
    crate::profile::save_app_state(&config.state).unwrap();
    switch_to("px");
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let writer = while_the_first_poll_waits(&stub, || switch_to("other"));

    let line = crate::login_proxy(&proxy_login("px", Some("m-relogin")), "stub")
        .expect("the re-login runs");
    writer.join().unwrap();

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(
        live_settings(),
        (
            Some("https://api.example.com".to_string()),
            Some("sk-other".to_string()),
            None
        ),
        "the live settings stay the active account's"
    );
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (px.models.default.as_deref(), px.api_key.as_deref()),
        (Some("m-relogin"), Some(KEY))
    );
}

/// The reverse: a switch TO the profile while its `--model` re-login waits
/// leaves it active at the save, so the model lands on the live settings too.
#[test]
fn a_switch_to_the_profile_during_a_model_relogin_lands_its_model_live() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    add_fresh_generic(&mut config, "other");
    crate::profile::save_app_state(&config.state).unwrap();
    switch_to("other");
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let writer = while_the_first_poll_waits(&stub, || switch_to("px"));

    let line = crate::login_proxy(&proxy_login("px", Some("m-relogin")), "stub")
        .expect("the re-login runs");
    writer.join().unwrap();

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(
        live_settings(),
        (
            Some(stub.base.clone()),
            Some(KEY.to_string()),
            Some("m-relogin".to_string())
        ),
        "the live settings carry the model the re-login set"
    );
}

/// A keyless re-login of a profile that is NOT active mints its key and
/// leaves the live settings to the active account: only the profile the
/// fresh roster marks active is re-applied.
#[test]
fn a_keyless_relogin_of_an_inactive_profile_leaves_the_live_settings_alone() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    add_fresh_generic(&mut config, "other");
    crate::profile::save_app_state(&config.state).unwrap();
    switch_to("other");
    clear_stored_key("px");
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));
    assert_eq!(
        live_settings(),
        (
            Some("https://api.example.com".to_string()),
            Some("sk-other".to_string()),
            None
        ),
        "the active account's settings stay live"
    );
}

/// The read after a lost poll answer fails with its own line: a bind that
/// answers as another service by then gets the mid-login refusal, an
/// HTTP status its `refused (HTTP n)` line, and only a second lost answer
/// reads as unanswered.
#[test]
fn the_read_after_a_lost_poll_answer_fails_with_its_own_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let own_health = stub.state().health.clone();
    let login = |reread: Option<String>, health_after: Option<Value>| {
        {
            let mut state = stub.state();
            state.flow_states = vec![json!({"flow": FLOW, "state": "pending"})];
            state.flow_polls = 0;
            state.health = own_health.clone();
            state.health_after_poll = health_after;
            state.poll_answers = std::iter::once((0, NO_ANSWER.to_string()))
                .chain(reread.map(|answer| (1, answer)))
                .collect();
        }
        crate::login_proxy(&proxy_login("px", None), "stub")
            .expect_err("the login fails")
            .to_string()
    };

    assert_eq!(
        login(None, Some(foreign_health())),
        identity_refusal(
            stub.port,
            "a proxy answers as service \"other\"",
            MID_LOGIN_REFUSED
        ),
        "the bind answers as another service by the read"
    );
    assert_eq!(
        login(Some(empty(500)), None),
        "proxy \"stub\" refused the request (HTTP 500); run `clauth proxy check stub` for details",
        "the read is answered 500"
    );
    assert_eq!(
        login(Some(NO_ANSWER.to_string()), None),
        "proxy \"stub\" did not answer; run `clauth proxy check stub` for details",
        "the read's answer is lost too"
    );
}

/// The read after a paste refused 409 fails with its own line too: the
/// mid-login refusal when the bind answers as another service by
/// then, `refused (HTTP 500)` when the read is answered 500; a flow still
/// pending keeps failing as the refused paste did.
#[test]
fn the_read_after_a_paste_refused_409_fails_with_its_own_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let own_health = stub.state().health.clone();
    let doors = crate::ProxyLoginDoors::capture_here();
    let login = |reread: Option<String>, health_after: Option<Value>| {
        {
            let mut state = stub.state();
            state.flow_states = vec![core(CORE_PENDING)];
            state.flow_polls = 0;
            state.health = own_health.clone();
            state.health_after_paste = health_after;
            state.paste_refusal = Some(reply(409, core(CORE_PASTE_CONFLICT)));
            state.poll_answers = reread.into_iter().map(|answer| (1, answer)).collect();
        }
        doors.paste().send("code-7".to_string()).unwrap();
        crate::login_proxy(&proxy_login("px", None), "stub")
            .expect_err("the login fails")
            .to_string()
    };

    assert_eq!(
        login(None, Some(foreign_health())),
        identity_refusal(
            stub.port,
            "a proxy answers as service \"other\"",
            MID_LOGIN_REFUSED
        ),
        "the bind answers as another service by the read"
    );
    assert_eq!(
        login(Some(empty(500)), None),
        "proxy \"stub\" refused the request (HTTP 500); run `clauth proxy check stub` for details",
        "the read is answered 500"
    );
    assert_eq!(
        login(None, None),
        "proxy \"stub\" refused the request (HTTP 409); run `clauth proxy check stub` for details",
        "the flow reads pending"
    );
}

/// A key cleared on the profile after its re-login's flow read `done` (so
/// the flow kept the stored key and minted none) and before the save is
/// never saved over: the save reads the profile under the flock, finds no
/// key, and the login mints one, as any re-bind of a keyless profile does,
/// so its success line never stands over a keyless profile.
#[test]
fn a_key_cleared_before_the_relogin_save_is_minted_never_saved_keyless() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let doors = crate::ProxyLoginDoors::capture_here();
    doors.before_save(|| clear_stored_key("px"));

    let line = crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(stub.state().minted, vec![ACCOUNT.to_string()]);
    assert_eq!(stored_key("px").as_deref(), Some(CORE_MINTED_KEY));
}

/// That late mint's save carries the login's `--model` too: on the active
/// profile the model lands on the profile and, with the minted key, on the
/// live settings.
#[test]
fn a_late_minted_relogin_lands_its_model_on_the_profile_and_live() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    switch_to("px");
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let doors = crate::ProxyLoginDoors::capture_here();
    doors.before_save(|| clear_stored_key("px"));

    let line =
        crate::login_proxy(&proxy_login("px", Some("m-late")), "stub").expect("the re-login runs");

    assert_eq!(line, captured_line("px", ACCOUNT));
    assert_eq!(stub.state().minted, vec![ACCOUNT.to_string()]);
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (px.models.default.as_deref(), px.api_key.as_deref()),
        (Some("m-late"), Some(CORE_MINTED_KEY)),
        "the profile carries the model and the minted key"
    );
    assert_eq!(
        live_settings(),
        (
            Some(stub.base.clone()),
            Some(CORE_MINTED_KEY.to_string()),
            Some("m-late".to_string())
        ),
        "the live settings carry the model and the minted key"
    );
}

/// A signal while that late mint waits ends the login as a signal after a
/// re-bind does (the re-bound line, exit 130), and saves nothing: the profile
/// stays as the other writer left it.
#[test]
fn a_signal_during_the_late_mint_ends_the_relogin_rebound_and_saves_nothing() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    let (release, held) = std::sync::mpsc::channel::<()>();
    stub.state().hold = Some((format!("POST /clauth/v1/accounts/{ACCOUNT}/key"), held));
    let doors = crate::ProxyLoginDoors::capture_here();
    let left = std::rc::Rc::new(std::cell::RefCell::new(None));
    let after_clear = std::rc::Rc::clone(&left);
    doors.before_save(move || {
        clear_stored_key("px");
        *after_clear.borrow_mut() = Some(config_bytes("px"));
    });
    let interrupt = doors.interrupt();
    let state = Arc::clone(&stub.state);
    let signaller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().held.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            state.lock().unwrap().held.is_some(),
            "the hold took the mint before the signal"
        );
        let _ = interrupt.send(2);
    });

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("interrupted");
    signaller.join().unwrap();
    drop(release);

    assert_eq!(err.to_string(), LINE_REBOUND);
    assert_eq!(
        err.downcast_ref::<crate::proxy_account::LoginInterrupted>()
            .map(crate::proxy_account::LoginInterrupted::signal),
        Some(2)
    );
    assert_eq!(
        Some(config_bytes("px")),
        left.borrow().clone(),
        "nothing was saved"
    );
}

/// A dated copy is dated before it lands: read at the instant of its rename,
/// it already carries its stalest figure's read (3 h ago), never a later
/// dating's.
#[test]
fn a_dated_copy_carries_its_date_the_instant_it_lands() {
    let read_at = crate::usage::now_epoch_secs() - 3 * 3600;
    let mut body = usage_body(500_000.0, 100.0);
    body["accounts"][0]["figures"][2]["read_at"] = json!(
        chrono::DateTime::from_timestamp(read_at, 0)
            .unwrap()
            .to_rfc3339()
    );
    body["accounts"][0]["figures"][2]["stale_reason"] = json!("upstream_unavailable");
    let (_home, _stub, config) = proxy_profile_on_stub(body);
    let path = crate::profile::profile_dir(&ProfileName::from("px"))
        .unwrap()
        .join(crate::profile_cache::THIRD_PARTY_CACHE_FILE);
    let landed = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&landed);
    crate::profile::set_landed_hook(
        &path,
        Arc::new(move |path: &std::path::Path| {
            *seen.lock().unwrap() = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        }),
    );

    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);

    assert_eq!(
        *landed.lock().unwrap(),
        Some(std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(read_at).unwrap())),
        "the copy's mtime as it landed"
    );
}

/// A copy whose dating fails never lands: the old copy keeps its bytes and
/// its mtime, as when the temp file cannot be made at all.
#[test]
fn a_copy_whose_dating_fails_leaves_the_old_copy_and_its_mtime() {
    let (_home, stub, config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);
    let path = crate::profile::profile_dir(&ProfileName::from("px"))
        .unwrap()
        .join(crate::profile_cache::THIRD_PARTY_CACHE_FILE);
    // 2026-01-01T00:00:00Z
    let old_mtime = std::time::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
    crate::testutil::set_mtime(&path, old_mtime);
    let old_bytes = std::fs::read(&path).unwrap();
    stub.state().usage = usage_body(900_000.0, 50.0);
    crate::profile::fail_next_dating(&path);

    crate::usage::reset_request_slots();
    run_leg(&leg_state(), &config);

    assert_eq!(std::fs::read(&path).unwrap(), old_bytes, "the old bytes");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        old_mtime,
        "the old mtime"
    );
}

/// A proxy account whose key is gone is a third-party usage account to both
/// cache selectors, the runtime profile's and the name-keyed one: its record
/// on its loopback bind decides, read once per name-keyed classification.
#[test]
fn both_readers_read_a_keyless_proxy_account_as_third_party() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    clear_stored_key("px");
    let name = ProfileName::from("px");
    let before = crate::profile::proxy_claim_reads();

    assert_eq!(
        (
            crate::profile::load_profile(&name)
                .unwrap()
                .usage_cache_is_third_party(),
            crate::profile::stored_usage_cache_is_third_party(&name),
        ),
        (true, true)
    );
    assert_eq!(
        crate::profile::proxy_claim_reads() - before,
        2,
        "one record read per reader"
    );
}

/// A chain date ahead of the clock (the clock stepped back after the write)
/// is undated, as a future mtime is: never fresh, and stale to a routing
/// read. The start walk then prefers a fresh member over it, and picks it
/// only when no fresh member is clear.
#[test]
fn a_chain_date_ahead_of_the_clock_is_undated_to_every_freshness_read() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("px");
    let px = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some(KEY.to_string()),
    );
    crate::profile::save_profile(&px).expect("save the proxy profile");
    crate::testutil::register_names(&["px"]);
    let ahead = u64::try_from(crate::usage::now_epoch_secs() + 3600).unwrap();
    assert!(crate::profile_cache::write_third_party_copy(
        &name,
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]),
        Some(ahead * 1000),
        ahead * 1000,
        std::time::UNIX_EPOCH + Duration::from_secs(ahead),
    ));

    let w = crate::profile_json::profile_windows_for(&name);
    assert_eq!(
        (w.age_secs(), w.stale(), w.routing_stale(), w.fresh()),
        (None, false, true, false),
        "(shown age, stale, routing_stale, fresh) of a copy whose chain date and mtime are 1 h ahead"
    );
    let stalest = u64::try_from(crate::usage::now_epoch_secs() - 60).unwrap();
    assert!(crate::profile_cache::write_third_party_copy(
        &name,
        &crate::testutil::stats_with_bars(vec![crate::testutil::bar("5h", 10.0)]),
        Some(ahead * 1000),
        ahead * 1000,
        std::time::UNIX_EPOCH + Duration::from_secs(stalest),
    ));
    let w = crate::profile_json::profile_windows_for(&name);
    assert_eq!(
        (
            w.age_secs().is_some_and(|age| (60..=70).contains(&age)),
            w.routing_stale(),
            w.fresh()
        ),
        (true, true, false),
        "(shown age a minute, routing_stale, fresh) of a copy whose chain date alone is 1 h ahead"
    );

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    config.add(px);
    add_fresh_generic(&mut config, "other");
    let walk = |config: &crate::profile::AppConfig| {
        crate::fallback::start_walk(
            config,
            crate::runtime::Isolation::Shared,
            &[],
            [None, None, None],
            false,
            None,
        )
    };
    config.state.fallback_chain = vec!["px".into(), "other".into()];
    let (rows, pick) = walk(&config);
    assert_eq!(
        (rows[0].fresh, rows[1].fresh, pick),
        (false, true, Some(1)),
        "(px fresh, other fresh, pick): the fresh member is preferred"
    );
    config.state.fallback_chain = vec!["px".into()];
    let (rows, pick) = walk(&config);
    assert_eq!(
        (rows[0].block.is_none(), pick),
        (true, Some(0)),
        "(px clear, pick): with no fresh member it is still picked"
    );
}

/// A port spelled with a sign is no loopback port: `loopback_port` refuses
/// `+9101` as `is_loopback_url_on` does, so a recorded keyless profile there
/// reads the OAuth cache in both cache selectors, agreeing with the usage arm,
/// which never polls it.
#[test]
fn a_signed_port_is_no_loopback_port() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = ProfileName::from("signed");
    crate::profile::save_profile(&crate::profile::Profile::new(
        "signed".to_string(),
        Some("http://127.0.0.1:+9101".to_string()),
        None,
    ))
    .expect("save profile");
    crate::profile::write_proxy_record(
        &name,
        &ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: "acct-1".to_string(),
        },
    )
    .expect("write the proxy record");
    assert_eq!(
        (
            crate::profile::load_profile(&name)
                .unwrap()
                .usage_cache_is_third_party(),
            crate::profile::stored_usage_cache_is_third_party(&name),
        ),
        (false, false),
        "(runtime, name-keyed) for a recorded keyless profile on a signed port"
    );
    assert_eq!(
        (
            crate::proxy::loopback_port("http://127.0.0.1:+9101"),
            crate::proxy::loopback_port("http://127.0.0.1:9101"),
        ),
        (None, Some(9101)),
        "(signed, unsigned) port"
    );
}

// ── a new login's failed save deletes its account ───────────────────────────

/// Run a proxy login of `px` with `dir` made to refuse new files while the
/// login's first poll waits: the login's config load retightens the clauth
/// tree to 0700 before its flow opens, so a fault armed earlier is undone.
/// Returns the login's error and the text std gives on this platform for a
/// new file in `dir` ([`new_file_refusal`]).
fn login_with_dir_refusing_new_files(
    stub: &Stub,
    dir: std::path::PathBuf,
) -> (anyhow::Error, String) {
    let armed = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&armed);
    let fault = dir.clone();
    let writer = while_the_first_poll_waits(stub, move || {
        *slot.lock().unwrap() = Some(crate::testutil::UnwritableDir::new(&fault));
    });
    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("the save fails");
    writer.join().unwrap();
    let refused = new_file_refusal(&dir);
    drop(armed.lock().unwrap().take());
    (err, refused)
}

/// The text std gives on this platform for a new file in `dir` while an
/// [`crate::testutil::UnwritableDir`] holds it, or `""` when it was created.
/// A file, never a directory: on Windows the guard's deny lets a new
/// subdirectory through (CI `windows-latest`, 2026-10-21), so a save fault
/// these tests pose is a file write on every platform.
fn new_file_refusal(dir: &std::path::Path) -> String {
    let probe = dir.join("px.refusal-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            String::new()
        }
        Err(e) => e.to_string(),
    }
}

/// The directory every profile lives in.
fn profiles_root() -> std::path::PathBuf {
    let root = crate::profile::profile_dir(&ProfileName::from("px"))
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A save failure's line once the proxy deleted the account it made: the
/// clauth dir refuses the roster's write ([`save_fails_on_the_roster`]).
const ROSTER_REFUSED_AND_DELETED: &str = "failed to write profiles.toml; clauth deleted the \
                                          account proxy \"stub\" had just created (\"acct-1\")";

/// A new proxy login of `px` whose save fails on the roster's write: the
/// clauth dir refuses new files once the first poll waits, while the profiles
/// root already stands, so the profile's own files land and `profiles.toml`'s
/// write is the one refused, on every platform.
fn save_fails_on_the_roster(stub: &Stub) -> (anyhow::Error, String) {
    profiles_root();
    login_with_dir_refusing_new_files(stub, crate::profile::clauth_dir().unwrap())
}

/// A new login whose save fails on a write, after the proxy answered `done`
/// with the account and its key, deletes that account on the proxy and says
/// so after the save's own text, its causes kept beneath it (the printed
/// error's `Caused by:` block); `px` is never put on the roster.
/// The fault goes through the suite's save-failure fixture, with no
/// process-wide lock deadline to shorten.
#[test]
fn a_new_login_whose_save_fails_deletes_the_account_the_proxy_made() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);

    let (err, refused) = save_fails_on_the_roster(&stub);

    assert_eq!(
        (
            err.chain().map(ToString::to_string).collect::<Vec<_>>(),
            stub.state().deleted.clone()
        ),
        (
            vec![ROSTER_REFUSED_AND_DELETED.to_string(), refused],
            vec![ACCOUNT.to_string()]
        ),
        "(the error's chain, deleted)"
    );
    assert!(
        !crate::profile::load_app_state()
            .unwrap()
            .profiles
            .iter()
            .any(|n| n.as_str() == "px"),
        "no profile px on the roster"
    );
}

/// A new login whose keyed `done` answer was lost re-reads the flow and
/// mints the account's key: it is still a NEW account, so a save that fails
/// after the mint deletes it the same way.
#[test]
fn a_new_login_that_minted_after_a_lost_answer_deletes_its_account_on_a_failed_save() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    {
        let mut state = stub.state();
        state.flow_states = vec![
            core(CORE_PENDING),
            done_body(ACCOUNT, Some(KEY)),
            done_body(ACCOUNT, None),
        ];
        state.poll_answers = vec![(1, NO_ANSWER.to_string())];
    }

    let (err, _) = save_fails_on_the_roster(&stub);

    let state = stub.state();
    assert_eq!(
        (err.to_string(), state.minted.clone(), state.deleted.clone()),
        (
            ROSTER_REFUSED_AND_DELETED.to_string(),
            vec![ACCOUNT.to_string()],
            vec![ACCOUNT.to_string()]
        ),
        "(error, minted, deleted)"
    );
}

/// A re-login whose save fails deletes nothing: the account it re-bound is the
/// profile's own, and it stays with the profile that still names it. The
/// re-mint carries a new key, so the `config.toml` write is the save that
/// fails: an unchanged config is never rewritten.
#[test]
fn a_relogin_whose_save_fails_deletes_nothing() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![done_body(ACCOUNT, Some("clp_stub-key-1"))];

    let (err, _) = login_with_dir_refusing_new_files(
        &stub,
        crate::profile::profile_dir(&ProfileName::from("px")).unwrap(),
    );

    let state = stub.state();
    assert_eq!(
        (
            err.to_string(),
            state.login_bodies.clone(),
            state.deleted.clone()
        ),
        (
            "failed to write config.toml".to_string(),
            vec![json!({"account": ACCOUNT})],
            vec![]
        ),
        "(error, login bodies, deleted)"
    );
}

// ── a keyless proxy account's cache by its loaded record ────────────────────

/// A keyless proxy account loaded from disk is never polled: no work list
/// holds it and no fetch leg reads it, though it carries its record and reads
/// its copy (its last figures render, aging to stale).
#[test]
fn a_loaded_keyless_proxy_account_is_never_polled() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    clear_stored_key("px");
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();

    assert_eq!(
        (
            px.proxy.as_ref().map(|record| record.account.as_str()),
            px.usage_cache_is_third_party(),
            collect_third_party_entries(std::slice::from_ref(&px)).len(),
            any_fetch_leg_polls(&px),
        ),
        (Some(ACCOUNT), true, 0, false),
        "(record, third-party cache, work-list entries, any leg polls)"
    );
}

/// `name` saved keyless at `base_url` with a `[proxy]` record.
fn recorded_keyless(name: &str, base_url: Option<&str>) -> ProfileName {
    crate::profile::save_profile(&crate::profile::Profile::new(
        name.to_string(),
        base_url.map(str::to_string),
        None,
    ))
    .expect("save profile");
    let name = ProfileName::from(name);
    crate::profile::write_proxy_record(
        &name,
        &ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: ACCOUNT.to_string(),
        },
    )
    .expect("write the proxy record");
    name
}

/// A recorded keyless profile reads its third-party copy only on a loopback
/// bind: with no `base_url`, or one off loopback, the runtime profile's
/// selector reads the OAuth cache. The loopback one is the control that the
/// record loaded.
#[test]
fn a_recorded_keyless_profile_off_loopback_reads_the_oauth_cache_at_runtime() {
    let _home = crate::testutil::HomeSandbox::new();
    let runtime = |base_url: Option<&str>, name: &str| {
        crate::profile::load_profile(&recorded_keyless(name, base_url))
            .unwrap()
            .usage_cache_is_third_party()
    };

    assert_eq!(
        (
            runtime(Some("http://127.0.0.1:9141"), "on"),
            runtime(None, "bare"),
            runtime(Some("https://relay.example.com"), "off"),
        ),
        (true, false, false),
        "(loopback, no base_url, off loopback) at runtime"
    );
}

/// The name-keyed selector reads the same off-loopback answers as the
/// runtime one.
#[test]
fn a_recorded_keyless_profile_off_loopback_reads_the_oauth_cache_by_name() {
    let _home = crate::testutil::HomeSandbox::new();
    let by_name = |base_url: Option<&str>, name: &str| {
        crate::profile::stored_usage_cache_is_third_party(&recorded_keyless(name, base_url))
    };

    assert_eq!(
        (
            by_name(Some("http://127.0.0.1:9141"), "on"),
            by_name(None, "bare"),
            by_name(Some("https://relay.example.com"), "off"),
        ),
        (true, false, false),
        "(loopback, no base_url, off loopback) by name"
    );
}

/// A keyless proxy account whose key was cleared on the setup tab (the
/// endpoint edit's save, after the profile loaded) still carries its record
/// in memory and on disk, so it reads its copy with no record read.
#[test]
fn a_key_cleared_on_the_setup_tab_keeps_the_proxy_accounts_copy() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    let mut config = crate::profile::AppConfig {
        state: crate::profile::load_app_state().unwrap(),
        profiles: vec![crate::profile::load_profile(&name).unwrap()],
    };
    let base_url = config.find(&name).unwrap().base_url.clone();
    crate::actions::edit_profile_endpoint(&mut config, &name, |stored_url, api_key| {
        *stored_url = base_url;
        *api_key = None;
    })
    .unwrap();
    let before = crate::profile::proxy_claim_reads();

    let px = config.find(&name).unwrap();
    assert_eq!(
        (
            px.api_key.as_deref(),
            px.usage_cache_is_third_party(),
            crate::profile::proxy_claim_reads() - before,
        ),
        (None, true, 0),
        "(key, third-party cache, record reads) in memory"
    );
    assert_eq!(
        crate::profile::stored_proxy_record(&name).map(|record| record.account),
        Some(ACCOUNT.to_string()),
        "the record on disk"
    );
}

/// A refused new login's line once the proxy deleted the account it made.
const REFUSED_AND_DELETED: &str = "a profile named 'px' already exists; clauth deleted the account \
                                   proxy \"stub\" had just created (\"acct-1\")";

/// What `exit_code` answers for `err` and the line it prints.
fn exit_line(err: anyhow::Error) -> (i32, String) {
    let mut line = Vec::new();
    let code = crate::exit_code_to(Err(err), &mut line);
    (code, String::from_utf8_lossy(&line).into_owned())
}

/// Another clauth creates `px` while a new proxy login of `px` waits on its
/// first poll, so the login's save is refused once the flow reads `done`.
fn create_px_elsewhere() {
    let mut config = crate::profile::load_config().unwrap();
    crate::actions::create_blank_profile(
        &mut config,
        "px".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-other".to_string()),
        crate::profile::ModelSettings::default(),
        None,
    )
    .unwrap();
}

/// A Ctrl-C pressed while a new login's save waits on the state flock is
/// still pending when that save is refused: the delete of the account the
/// proxy made runs to its end anyway, and the login ends with the save's text
/// plus what became of the account, at `128 +` the signal.
#[test]
fn a_signal_pending_from_a_refused_save_still_deletes_the_new_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    let interrupt = doors.interrupt();
    let (locked_tx, locked) = std::sync::mpsc::channel::<()>();
    let holder: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&holder);
    let writer = while_the_first_poll_waits(&stub, move || {
        create_px_elsewhere();
        // The flock is held past the `done` poll, so the login's save waits on
        // it; the signal arrives during that wait, read by nothing until the
        // save was refused.
        let flock = std::thread::spawn(move || {
            crate::lock::with_state_lock(|_held| {
                let _ = locked_tx.send(());
                std::thread::sleep(Duration::from_millis(800));
                let _ = interrupt.send(2);
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .unwrap();
        });
        locked.recv().unwrap();
        *slot.lock().unwrap() = Some(flock);
    });

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");
    let deleted = stub.state().deleted.clone();
    writer.join().unwrap();
    holder.lock().unwrap().take().unwrap().join().unwrap();

    let text = err.to_string();
    assert_eq!(
        (text.as_str(), deleted, exit_line(err)),
        (
            REFUSED_AND_DELETED,
            vec![ACCOUNT.to_string()],
            (130, format!("Error: {REFUSED_AND_DELETED}\n"))
        ),
        "(error, deleted when the login returned, (exit code, line))"
    );
}

/// A new login of `px` whose save is refused ([`create_px_elsewhere`] while
/// the first poll waits), with the delete of the account the proxy made held
/// on the stub: once it is held, `signals` arrive 200 ms apart, and the
/// delete is let through `release_after` the last of them, or else once the
/// login returned. Returns the login's error, and whether the stub had let
/// the delete through and what it had deleted when the login returned.
fn a_refused_login_signalled_during_its_delete(
    stub: &Stub,
    doors: &crate::ProxyLoginDoors,
    signals: &'static [i32],
    release_after: Option<Duration>,
) -> (anyhow::Error, bool, Vec<String>) {
    let delete = format!("DELETE /clauth/v1/accounts/{ACCOUNT}");
    let (release, held) = std::sync::mpsc::channel::<()>();
    let slot = Arc::clone(&stub.state);
    let hold = delete.clone();
    let writer = while_the_first_poll_waits(stub, move || {
        create_px_elsewhere();
        // The first poll's hold was taken out of the slot: the delete's goes
        // in before that poll is answered, so it is there when the delete is.
        slot.lock().unwrap().hold = Some((hold, held));
    });
    let interrupt = doors.interrupt();
    let (returned_tx, returned) = std::sync::mpsc::channel::<()>();
    let watched = Arc::clone(&stub.state);
    let sender = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + HOLD;
        while watched.lock().unwrap().held.as_deref() != Some(delete.as_str())
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        // The first poll's release set the flag before the delete was sent;
        // from here it says whether the delete was let through.
        watched.lock().unwrap().released = false;
        for (at, signal) in signals.iter().enumerate() {
            if at > 0 {
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = interrupt.send(*signal);
        }
        match release_after {
            Some(after) => {
                std::thread::sleep(after);
                let _ = release.send(());
            }
            None => {
                let _ = returned.recv_timeout(HOLD);
                drop(release);
            }
        }
    });

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");
    let (released, deleted) = {
        let state = stub.state();
        (state.released, state.deleted.clone())
    };
    let _ = returned_tx.send(());
    writer.join().unwrap();
    sender.join().unwrap();
    (err, released, deleted)
}

/// A first signal while the delete of a refused new login's account is in
/// flight does not cut that delete: it runs to its end, and the login ends
/// with the save's text plus what became of the account, at `128 +` the
/// signal. The delete is let through 1 s after the signal, ten times the
/// slice a signal is looked for in, so a login the signal cut returns long
/// before it.
#[test]
fn a_first_signal_during_a_refused_saves_delete_waits_it_out() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();

    let (err, released, deleted) = a_refused_login_signalled_during_its_delete(
        &stub,
        &doors,
        &[2],
        Some(Duration::from_secs(1)),
    );

    assert!(
        released,
        "the login returned while the proxy still held the delete"
    );
    let text = err.to_string();
    assert_eq!(
        (text.as_str(), deleted, exit_line(err)),
        (
            REFUSED_AND_DELETED,
            vec![ACCOUNT.to_string()],
            (130, format!("Error: {REFUSED_AND_DELETED}\n"))
        ),
        "(error, deleted when the login returned, (exit code, line))"
    );
}

/// A second signal during that delete ends the login at once with the save's
/// text alone, at `128 +` the second signal, as a second signal during a
/// cancel does: the delete is not waited out, and lands once the
/// proxy answers it.
#[test]
fn a_second_signal_during_a_refused_saves_delete_exits_with_the_save_error_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();

    let (err, released, deleted) =
        a_refused_login_signalled_during_its_delete(&stub, &doors, &[2, 15], None);

    assert!(!released, "the login returned while the delete was held");
    let text = err.to_string();
    let saved = "a profile named 'px' already exists";
    assert_eq!(
        (text.as_str(), deleted, exit_line(err)),
        (saved, vec![], (143, format!("Error: {saved}\n"))),
        "(error, deleted when the login returned, (exit code, line))"
    );
    let deadline = std::time::Instant::now() + HOLD;
    while !stub.state().released && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        stub.state().deleted,
        vec![ACCOUNT.to_string()],
        "the delete already sent lands once the proxy answers it"
    );
}

/// A signal pending from a new login's refused save ends the login at `128 +`
/// that signal with the error the unsignalled ending has: the save's text plus
/// what became of the account, its causes still beneath it. The save waits on
/// a state flock a test thread holds while the signal arrives, then the
/// roster's write is refused (the clauth dir refusing new entries).
#[test]
fn a_signalled_refused_saves_error_keeps_the_saves_causes() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    profiles_root();
    let clauth_dir = crate::profile::clauth_dir().unwrap();
    let doors = crate::ProxyLoginDoors::capture_here();
    let interrupt = doors.interrupt();
    let (locked_tx, locked) = std::sync::mpsc::channel::<()>();
    let held: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let armed = Arc::new(Mutex::new(None));
    let (holder, fault) = (Arc::clone(&held), Arc::clone(&armed));
    let dir = clauth_dir.clone();
    let writer = while_the_first_poll_waits(&stub, move || {
        let flock = std::thread::spawn(move || {
            crate::lock::with_state_lock(|_held| {
                let _ = locked_tx.send(());
                std::thread::sleep(Duration::from_millis(800));
                let _ = interrupt.send(2);
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .unwrap();
        });
        locked.recv().unwrap();
        *fault.lock().unwrap() = Some(crate::testutil::UnwritableDir::new(&dir));
        *holder.lock().unwrap() = Some(flock);
    });

    let err = crate::login_proxy(&proxy_login("px", None), "stub").expect_err("refused");
    writer.join().unwrap();
    held.lock().unwrap().take().unwrap().join().unwrap();
    let refused = new_file_refusal(&clauth_dir);
    drop(armed.lock().unwrap().take());

    let chain = err.chain().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        (chain, stub.state().deleted.clone(), exit_line(err).0),
        (
            vec![
                "failed to write profiles.toml; clauth deleted the account proxy \"stub\" had \
                 just created (\"acct-1\")"
                    .to_string(),
                refused
            ],
            vec![ACCOUNT.to_string()],
            130
        ),
        "(the error's chain, deleted, exit code)"
    );
}

/// The proxy record a new proxy login saves for `px`.
fn px_record() -> ProxyAccount {
    ProxyAccount {
        service: crate::proxy::Service::parse("stub").unwrap(),
        account: ACCOUNT.to_string(),
    }
}

/// A new proxy profile's create hands back, in the config it was given, a
/// profile holding the record it wrote: an in-process caller
/// reads it as the proxy account it is with no reload.
#[test]
fn a_created_proxy_profile_holds_its_record_in_memory() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::load_config().unwrap();

    crate::actions::create_proxy_profile(
        &mut config,
        "px".to_string(),
        px_record(),
        "http://127.0.0.1:9141".to_string(),
        KEY.to_string(),
        None,
    )
    .unwrap();

    assert_eq!(
        config
            .find(&ProfileName::from("px"))
            .map(|p| p.proxy.clone()),
        Some(Some(px_record()))
    );
}

/// A proxy re-login's save hands back, in the config it was given, the fresh
/// profile holding the record it wrote, where the profile loaded
/// before the write held none.
#[test]
fn a_relogged_proxy_profile_holds_its_record_in_memory() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::load_config().unwrap();
    crate::actions::create_blank_profile(
        &mut config,
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
        crate::profile::ModelSettings::default(),
        None,
    )
    .unwrap();

    let saved = crate::actions::update_proxy_login(
        &mut config,
        &ProfileName::from("px"),
        &px_record(),
        Some(KEY.to_string()),
        None,
    )
    .unwrap();

    assert_eq!(
        (
            saved,
            config
                .find(&ProfileName::from("px"))
                .map(|p| p.proxy.clone())
        ),
        (crate::actions::ProxyRelogin::Saved, Some(Some(px_record())))
    );
}

/// A re-login of a proxy account that does not name its proxy (here the api
/// arm, `--base-url` + `--api-key`, which asks nothing and opens no browser) is
/// refused before anything runs and changes nothing: the account stays the
/// proxy's, and the line names the re-login that keeps it so and the delete
/// that ends it.
#[test]
fn a_relogin_of_a_proxy_account_without_its_proxy_is_refused_and_changes_nothing() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let before = config_bytes("px");
    let sent = stub.state().requests.len();

    let err = crate::cmd_login(LoginArgs {
        proxy: None,
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some("sk-other".to_string()),
        ..proxy_login("px", None)
    })
    .expect_err("refused");

    let line = "'px' is an account on proxy \"stub\": log in to it again with `clauth login px \
                --proxy stub`, or delete it with `clauth delete px` first";
    let text = err.to_string();
    assert_eq!(
        (text.as_str(), exit_line(err)),
        (line, (2, format!("Error: {line}\n"))),
        "(error, (exit code, printed line))"
    );
    assert_eq!(config_bytes("px"), before, "the profile's config.toml");
    assert_eq!(
        crate::profile::stored_proxy_record(&ProfileName::from("px")),
        Some(px_record()),
        "the record"
    );
    assert_eq!(
        stub.state().requests.len(),
        sent,
        "no request reached the proxy"
    );
}

/// The line every re-login of `px` (proxy "stub") that does not name its
/// proxy is refused with.
const PX_RELOGIN_REFUSED: &str = "'px' is an account on proxy \"stub\": log in to it again with \
                                  `clauth login px --proxy stub`, or delete it with `clauth \
                                  delete px` first";

/// `clauth login px` with no `--proxy`, its other flags left to the caller.
fn login_off_proxy() -> LoginArgs {
    LoginArgs {
        proxy: None,
        ..proxy_login("px", None)
    }
}

/// `crate::cmd_login(args)` run on a worker thread that holds its own browser
/// capture, answering its result and every url it opened. A login that got
/// past the refusal sits in a browser or prompt wait no test answers, so the
/// wait for the worker is bounded and a worker still waiting fails the test
/// (left parked: the test process ends it).
fn cmd_login_off_thread(args: LoginArgs) -> (anyhow::Result<()>, Vec<String>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let opened = crate::platform::OpenedUrls::capture_here();
        let result = crate::cmd_login(args);
        let _ = tx.send((result, opened.urls()));
    });
    let answer = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the re-login returned at once instead of waiting on a browser or a prompt");
    worker.join().unwrap();
    answer
}

/// What a re-login of `px` leaves: its error's line and exit code, every url
/// opened, whether its config.toml kept its bytes, its record, and the
/// requests the proxy saw.
type ReloginTrace = (
    Option<(String, i32)>,
    Vec<String>,
    bool,
    Option<ProxyAccount>,
    usize,
);

/// [`ReloginTrace`] of `crate::cmd_login(args)` on `px`.
fn refused_off_proxy(stub: &Stub, args: LoginArgs) -> ReloginTrace {
    let before = config_bytes("px");
    let sent = stub.state().requests.len();
    let (result, opened) = cmd_login_off_thread(args);
    (
        result.err().map(|err| (err.to_string(), exit_line(err).0)),
        opened,
        config_bytes("px") == before,
        crate::profile::stored_proxy_record(&ProfileName::from("px")),
        stub.state().requests.len() - sent,
    )
}

/// A browser re-login of a proxy account (`clauth login px`) is refused before
/// its browser opens: no url reaches the opener, and with it no loopback
/// listener (`run_oauth` binds one right before it opens the page), and the
/// profile is unchanged.
#[test]
fn a_browser_relogin_of_a_proxy_account_is_refused_before_its_browser() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));

    assert_eq!(
        refused_off_proxy(&stub, login_off_proxy()),
        (
            Some((PX_RELOGIN_REFUSED.to_string(), 2)),
            vec![],
            true,
            Some(px_record()),
            0
        ),
        "(error and exit code, urls opened, config.toml kept, record, requests)"
    );
}

/// A `--setup-token` re-login of a proxy account is refused before its token
/// prompt, writing no long-lived token beside the account.
#[test]
fn a_setup_token_relogin_of_a_proxy_account_is_refused_before_its_prompt() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));

    let refused = refused_off_proxy(
        &stub,
        LoginArgs {
            setup_token: true,
            yes: true,
            ..login_off_proxy()
        },
    );

    assert_eq!(
        (
            refused,
            crate::claude::has_session_token(&ProfileName::from("px"))
        ),
        (
            (
                Some((PX_RELOGIN_REFUSED.to_string(), 2)),
                vec![],
                true,
                Some(px_record()),
                0
            ),
            false
        ),
        "((error and exit code, urls opened, config.toml kept, record, requests), long-lived token)"
    );
}

/// A proxy account whose endpoint was edited to an Alibaba Model Studio one
/// reaches `clauth login`'s console arm, which opens the console in a browser:
/// the re-login is refused before it.
#[test]
fn a_console_relogin_of_a_proxy_account_is_refused_before_its_browser() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let mut px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    px.base_url = Some("https://coding-intl.dashscope.aliyuncs.com/apps/anthropic".to_string());
    crate::profile::save_profile(&px).unwrap();
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (px.provider, px.proxy.clone()),
        (Some(crate::providers::Provider::Alibaba), Some(px_record())),
        "(provider, record): the console arm's shape"
    );

    assert_eq!(
        refused_off_proxy(&stub, login_off_proxy()),
        (
            Some((PX_RELOGIN_REFUSED.to_string(), 2)),
            vec![],
            true,
            Some(px_record()),
            0
        ),
        "(error and exit code, urls opened, config.toml kept, record, requests)"
    );
}

/// A re-login without `--proxy` of a profile whose `[proxy]` table does not
/// read is refused the way the `--proxy` re-login refuses it, changing
/// nothing.
#[test]
fn a_relogin_off_proxy_of_an_unreadable_proxy_record_is_refused() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let path = crate::profile::profile_subpath(&ProfileName::from("px"), "config.toml").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("service = \"stub\"", "service = \"Not A Service\""),
    )
    .unwrap();

    assert_eq!(
        refused_off_proxy(
            &stub,
            LoginArgs {
                base_url: Some("https://api.example.com".to_string()),
                api_key: Some("sk-other".to_string()),
                ..login_off_proxy()
            }
        ),
        (
            Some((
                format!(
                    "profile 'px' has a [proxy] table in {path:?} that clauth cannot read; \
                     nothing was changed. Give the table a valid `service` and `account`, then \
                     log in again"
                ),
                2
            )),
            vec![],
            true,
            None,
            0
        ),
        "(error and exit code, urls opened, config.toml kept, record, requests)"
    );
}

/// A re-login of a profile with no `[proxy]` table passes the proxy refusal:
/// `clauth login plain --base-url … --api-key …` replaces its endpoint and key
/// as for any profile that is no proxy account.
#[test]
fn an_api_relogin_of_a_non_proxy_profile_still_replaces_its_endpoint() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::load_config().unwrap();
    crate::actions::create_blank_profile(
        &mut config,
        "plain".to_string(),
        Some("https://old.example.com".to_string()),
        Some("sk-old".to_string()),
        crate::profile::ModelSettings::default(),
        None,
    )
    .unwrap();

    let (result, opened) = cmd_login_off_thread(LoginArgs {
        profile: "plain".to_string(),
        base_url: Some("https://new.example.com".to_string()),
        api_key: Some("sk-new".to_string()),
        ..login_off_proxy()
    });

    let plain = crate::profile::load_profile(&ProfileName::from("plain")).unwrap();
    assert_eq!(
        (
            result.map_err(|e| e.to_string()),
            opened,
            plain.base_url.as_deref(),
            plain.api_key.as_deref()
        ),
        (
            Ok(()),
            vec![],
            Some("https://new.example.com"),
            Some("sk-new")
        ),
        "(result, urls opened, base url, api key)"
    );
}

// ── Claude Code's own login is never adopted into a proxy account ───────────

/// The sandbox's live `~/.claude/.credentials.json`, a completed login
/// Claude Code's own `/login` wrote (a regular file, never clauth's link).
fn write_live_login(access: &str, refresh: &str) -> std::path::PathBuf {
    let path = crate::claude::claude_credentials_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": 4_102_444_800_000_u64,
            }
        })
        .to_string(),
    )
    .unwrap();
    path
}

/// Every file under `name`'s profile dir with its bytes.
fn profile_tree(name: &str) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = Vec::new();
    walk_files(
        &crate::profile::profile_dir(&ProfileName::from(name)).unwrap(),
        &mut files,
    );
    files
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

/// The active proxy account `px` on disk, its `[proxy]` record beside it.
fn active_proxy_account() -> (crate::testutil::HomeSandbox, crate::profile::AppConfig) {
    let home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", "http://127.0.0.1:9");
    config.state.active_profile = Some(ProfileName::from("px"));
    crate::profile::save_app_state(&config.state).unwrap();
    (home, config)
}

/// The not-saved line, as a process in role `prefix` logs it.
fn not_saved_line(prefix: &str) -> String {
    format!(
        "{prefix}: Claude Code signed in while proxy account 'px' was active; clauth did not \
         save that login into 'px'. To keep it as an account, run `clauth login <name>` with a \
         new name"
    )
}

/// The snapshot sink every switch and switch-off runs (the daemon's drain and
/// fallback legs among them) never adopts Claude Code's own login into an
/// active proxy account: the live file and every profile byte stay. The
/// not-saved line goes out once per distinct live login: two passes over one
/// login log one line, a new login logs a second, and the prefix follows the
/// process role.
#[test]
fn the_silent_adopt_skips_a_proxy_account_and_warns_once_per_login() {
    let (_home, mut config) = active_proxy_account();
    let tree = profile_tree("px");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let daemon = crate::logline::StampOverride::new(true);
    let live = write_live_login("at-r18-snap-1", "rt-r18-snap-1");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    let after_one = lines.snapshot().len();
    write_live_login("at-r18-snap-2", "rt-r18-snap-2");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    let after_two = lines.snapshot().len();
    drop(daemon);
    let _cli = crate::logline::StampOverride::new(false);
    write_live_login("at-r18-snap-3", "rt-r18-snap-3");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();

    assert_eq!(
        (
            after_one,
            after_two,
            lines.snapshot(),
            profile_tree("px") == tree,
            std::fs::symlink_metadata(&live)
                .unwrap()
                .file_type()
                .is_symlink(),
            std::fs::read_to_string(&live)
                .unwrap()
                .contains("at-r18-snap-3"),
        ),
        (
            1,
            2,
            vec![
                not_saved_line("clauth daemon"),
                not_saved_line("clauth daemon"),
                not_saved_line("clauth"),
            ],
            true,
            false,
            true
        ),
        "(lines after login 1, after login 2, the lines, profile bytes kept, live file \
         relinked, live login kept)"
    );
}

/// A `[proxy]` table clauth cannot read is skipped the same way: nothing is
/// adopted and nothing is written.
#[test]
fn the_silent_adopt_skips_an_unreadable_proxy_record() {
    let (_home, mut config) = active_proxy_account();
    let path = crate::profile::profile_subpath(&ProfileName::from("px"), "config.toml").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("service = \"stub\"", "service = \"Not A Service\""),
    )
    .unwrap();
    let tree = profile_tree("px");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let _daemon = crate::logline::StampOverride::new(true);
    let live = write_live_login("at-r18-unreadable", "rt-r18-unreadable");

    crate::claude::snapshot_active_credentials(&mut config).unwrap();

    assert_eq!(
        (
            lines.snapshot(),
            profile_tree("px") == tree,
            std::fs::read_to_string(&live)
                .unwrap()
                .contains("at-r18-unreadable"),
        ),
        (vec![not_saved_line("clauth daemon")], true, true),
        "(lines, profile bytes kept, live login kept)"
    );
}

/// The silent adopt skips an active proxy account whose `config.toml` turned
/// unclassifiable after the config loaded (a directory in its place, a
/// `[proxy] # note` header that fails to parse): nothing is adopted, no
/// profile byte moves, and the live login stays. Its line names the config
/// clauth cannot read, never a proxy account.
#[test]
fn the_silent_adopt_skips_an_account_whose_config_cannot_be_classified() {
    use crate::claude::tests::Unclassifiable;
    let outcomes: Vec<_> = Unclassifiable::BOTH
        .iter()
        .enumerate()
        .map(|(i, shape)| {
            let (_home, mut config) = active_proxy_account();
            shape.pose("px");
            let tree = profile_tree("px");
            let lines = crate::logline::LogLines::new();
            let _capture = lines.capture_here();
            let _daemon = crate::logline::StampOverride::new(true);
            // A login per shape: the not-saved line goes out once per login.
            let access = format!("at-r29l-adopt-{i}");
            let live = write_live_login(&access, &format!("rt-r29l-adopt-{i}"));

            let snapshot =
                crate::claude::snapshot_active_credentials(&mut config).map_err(|e| e.to_string());

            (
                snapshot,
                lines.snapshot(),
                profile_tree("px") == tree,
                std::fs::read_to_string(&live).unwrap().contains(&access),
            )
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![
            (
                Ok(()),
                vec![
                    "clauth daemon: Claude Code signed in while 'px' was active, but clauth can't \
                     read its config, so it did not save that login into 'px'. Fix its \
                     config.toml, or run `clauth login <name>` with a new name to keep it"
                        .to_string()
                ],
                true,
                true
            );
            2
        ],
        "(the snapshot, lines, profile bytes kept, live login kept) for a directory and a \
         noted header"
    );
}

/// `other`, an OAuth account beside the active proxy account `px`.
fn with_other_oauth_account(config: &mut crate::profile::AppConfig) {
    let mut other = crate::profile::Profile::new("other".to_string(), None, None);
    other.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-other".to_string(),
            refresh_token: Some("rt-other".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&other).expect("save other");
    config.add(other);
    config.state.profiles = vec![ProfileName::from("px"), ProfileName::from("other")];
    crate::profile::save_app_state(&config.state).unwrap();
    crate::testutil::register_names(&["px", "other"]);
}

/// The login the adopt skipped on a proxy account is dropped by the next
/// manual switch away, with no hold: the switch goes through, the live slot
/// no longer holds it, and nothing of px's moved.
#[test]
fn a_switch_away_drops_the_login_the_adopt_skipped() {
    let (_home, mut config) = active_proxy_account();
    with_other_oauth_account(&mut config);
    let tree = profile_tree("px");
    let live = write_live_login("at-r19-drop", "rt-r19-drop");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let switched = crate::actions::switch_profile_locked(&mut config, &ProfileName::from("other"))
        .map_err(|e| e.to_string());

    assert_eq!(
        (
            switched,
            std::fs::read_to_string(&live)
                .unwrap_or_default()
                .contains("rt-r19-drop"),
            profile_tree("px") == tree,
            lines.snapshot().len(),
        ),
        (Ok(true), false, true, 1),
        "(switch, the live slot still holds the skipped login, px's bytes kept, lines)"
    );
}

/// The same at a switch-off: the skipped login is dropped the same way.
#[test]
fn a_switch_off_drops_the_login_the_adopt_skipped() {
    let (_home, mut config) = active_proxy_account();
    let tree = profile_tree("px");
    let live = write_live_login("at-r19-off", "rt-r19-off");

    let off = crate::actions::switch_off_locked(&mut config).map_err(|e| e.to_string());

    assert_eq!(
        (
            off,
            std::fs::read_to_string(&live)
                .unwrap_or_default()
                .contains("rt-r19-off"),
            profile_tree("px") == tree,
        ),
        (Ok(true), false, true),
        "(switch-off, the live slot still holds the skipped login, px's bytes kept)"
    );
}

/// A new login whose roster write fails deletes its account on the proxy and
/// leaves none of the profile's own files behind: the profile
/// dir it created holds the dead account's record and key otherwise.
#[test]
fn a_failed_roster_write_leaves_no_profile_dir() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);

    let (_err, _) = save_fails_on_the_roster(&stub);

    let dir = crate::profile::profile_dir(&ProfileName::from("px")).unwrap();
    let mut left = Vec::new();
    walk_files(&dir, &mut left);
    let names: Vec<String> = left
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        (stub.state().deleted.clone(), names),
        (vec![ACCOUNT.to_string()], Vec::<String>::new()),
        "(deleted on the proxy, files left under profiles/px)"
    );
}

/// A plain account created later under the name of a new proxy login whose
/// roster write failed is no proxy account: nothing of the dead account's
/// `[proxy]` record carries into it.
#[test]
fn a_name_reused_after_a_failed_roster_write_is_no_proxy_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let (_err, _) = save_fails_on_the_roster(&stub);

    let mut config = crate::profile::load_config().unwrap();
    let created = crate::actions::create_blank_profile(
        &mut config,
        "px".to_string(),
        Some("https://api.example.com".to_string()),
        Some("sk-fresh".to_string()),
        crate::profile::ModelSettings::default(),
        None,
    )
    .map_err(|e| e.to_string());

    assert_eq!(
        (
            created,
            stub.state().deleted.clone(),
            crate::profile::stored_proxy_record(&ProfileName::from("px")),
        ),
        (Ok(()), vec![ACCOUNT.to_string()], None),
        "(the plain create, deleted on the proxy, px's proxy record)"
    );
}

/// Claude Code's home `.claude.json`, naming `account` as the signed-in one.
fn write_home_account(account: &str) {
    std::fs::write(
        crate::profile::home_dir().unwrap().join(".claude.json"),
        json!({ "oauthAccount": { "accountUuid": account } }).to_string(),
    )
    .unwrap();
}

/// The not-saved line keys on the account Claude Code signed in as: a token
/// refresh of the same account logs nothing more, a different account logs a
/// second line.
#[test]
fn the_not_saved_line_keys_on_the_signed_in_account() {
    let (_home, mut config) = active_proxy_account();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let _cli = crate::logline::StampOverride::new(false);

    write_home_account("uuid-r19-snap-a");
    write_live_login("at-r19-snap-1", "rt-r19-snap-1");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    write_live_login("at-r19-snap-2", "rt-r19-snap-2");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    let after_refresh = lines.snapshot().len();
    write_home_account("uuid-r19-snap-b");
    write_live_login("at-r19-snap-3", "rt-r19-snap-3");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();

    assert_eq!(
        (after_refresh, lines.snapshot()),
        (1, vec![not_saved_line("clauth"), not_saved_line("clauth")]),
        "(lines after the refresh, the lines)"
    );
}

/// A CLI switch away from a proxy account whose stored login the live one
/// diverged from asks no "save it into" question: the switch goes ahead, the
/// live login is dropped, and nothing of px's moves.
#[test]
fn a_cli_switch_away_from_a_proxy_account_asks_nothing_and_drops_the_login() {
    let (_home, mut config) = active_proxy_account();
    with_other_oauth_account(&mut config);
    let px = config.find_mut(&ProfileName::from("px")).unwrap();
    px.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-r19-px-held".to_string(),
            refresh_token: Some("rt-r19-px-held".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(px).unwrap();
    let tree = profile_tree("px");
    let live = write_live_login("at-r19-cli", "rt-r19-cli");
    let out = crate::out::OutCapture::new();
    let _capture = out.capture_here();

    let switched =
        crate::actions::switch_profile_cli(config, &ProfileName::from("other"), std::io::empty())
            .map_err(|e| e.to_string());

    assert_eq!(
        (
            switched,
            String::from_utf8(out.snapshot()).unwrap(),
            profile_tree("px") == tree,
            std::fs::read_to_string(&live)
                .unwrap_or_default()
                .contains("rt-r19-cli"),
        ),
        (
            Ok(()),
            "clauth: switched to 'other'\n".to_string(),
            true,
            false
        ),
        "(switch, stdout, px's bytes kept, the live slot still holds the live login)"
    );
}

/// `clauth capture <name>` over an existing proxy account names the login its
/// proxy takes; over any other account the plain `clauth login <name>`.
#[test]
fn a_capture_onto_a_proxy_accounts_name_names_its_proxy_login() {
    let (_home, mut config) = active_proxy_account();
    with_other_oauth_account(&mut config);

    let px =
        crate::actions::capture_current_login(&mut config, "px", false).map_err(|e| e.to_string());
    let other = crate::actions::capture_current_login(&mut config, "other", false)
        .map_err(|e| e.to_string());

    assert_eq!(
        (px, other),
        (
            Err(
                "a profile named 'px' already exists; re-authenticate it with:  clauth login px \
                 --proxy stub"
                    .to_string()
            ),
            Err(
                "a profile named 'other' already exists; re-authenticate it with:  clauth \
                 login other"
                    .to_string()
            )
        ),
        "(capture px, capture other)"
    );
}

/// A failed new login removes only a profile dir it created: a dir already on
/// disk under the name (one the roster never named, so the name check let the
/// login through) keeps what it held.
#[test]
fn a_failed_roster_write_keeps_a_profile_dir_it_did_not_create() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let dir = crate::profile::profile_dir(&ProfileName::from("px")).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.txt"), b"kept").unwrap();

    let (_err, _) = save_fails_on_the_roster(&stub);

    assert_eq!(
        (
            stub.state().deleted.clone(),
            std::fs::read(dir.join("note.txt")).ok()
        ),
        (vec![ACCOUNT.to_string()], Some(b"kept".to_vec())),
        "(deleted on the proxy, the dir's own file)"
    );
}

/// An OAuth login with the given tokens.
fn oauth_login_of(access: &str, refresh: &str) -> crate::profile::ClaudeCredentials {
    crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// A `clauth start` session on proxy account px whose Claude Code ran its own
/// `/login`, then `clauth delete px --force` while the session lives: the
/// session never writes that login into a px store (the session tree lives
/// under the profile dir the delete removes).
#[test]
fn a_force_deleted_proxy_accounts_session_login_never_lands_on_disk() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    std::fs::create_dir_all(crate::profile::home_dir().unwrap().join(".claude")).unwrap();
    let px = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    let dir = crate::profile::profile_dir(&px.name).unwrap();
    let session = crate::runtime::ProfileRuntime::acquire(
        &px,
        crate::runtime::Isolation::Shared,
        &[],
        false,
        crate::live_sessions::Launcher::Own,
    )
    .expect("acquire px");
    let live = session.config_dir().join(".credentials.json");
    let _ = std::fs::remove_file(&live);
    std::fs::write(
        &live,
        br#"{"claudeAiOauth":{"accessToken":"at-r20-del","refreshToken":"rt-r20-del","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    crate::cmd_delete("px", true, true).expect("the forced delete runs");
    drop(session);
    let landed = std::fs::read_to_string(dir.join("credentials.json"))
        .ok()
        .map(|s| s.contains("rt-r20-del"));
    assert_eq!(
        (stub.state().deleted.clone(), dir.exists(), landed),
        (vec![ACCOUNT.to_string()], false, None),
        "(deleted on the proxy, px's dir after the session ended, the session's login in a px store)"
    );
}

/// A re-login's landing (`overwrite_captured_profile`, which `clauth login px`
/// reaches after its browser) over a config loaded before px was deleted and
/// re-created as a proxy account during the browser round trip is refused at
/// the landing, changing nothing.
#[test]
fn a_relogin_landing_onto_a_name_recreated_as_a_proxy_account_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut plain = crate::profile::Profile::new("px".to_string(), None, None);
    plain.credentials = Some(oauth_login_of("at-r20-old", "rt-r20-old"));
    crate::profile::save_profile(&plain).unwrap();
    crate::testutil::register_names(&["px"]);
    let mut config = crate::profile::load_config().unwrap();
    let account = crate::profile::claim_profile_id(&ProfileName::from("px")).unwrap();

    std::fs::remove_dir_all(crate::profile::profile_dir(&ProfileName::from("px")).unwrap())
        .unwrap();
    let recreated = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_key".to_string()),
    );
    crate::profile::save_profile(&recreated).unwrap();
    crate::profile::write_proxy_record(&recreated.name, &px_record()).unwrap();

    let snapshot = crate::actions::CaptureSnapshot {
        credentials: Some(oauth_login_of("at-r20-cli", "rt-r20-cli")),
        base_url: None,
        api_key: None,
        anchor: crate::actions::AnchorAction::Unproven,
        plan: None,
    };
    let landed = crate::actions::overwrite_captured_profile(
        &mut config,
        &ProfileName::from("px"),
        &account,
        snapshot,
    )
    .map_err(|e| e.to_string());
    let reloaded = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (
            landed.is_err(),
            crate::profile::stored_proxy_record(&ProfileName::from("px")),
            reloaded.base_url.clone(),
            reloaded.api_key.clone(),
            reloaded.credentials.is_some(),
        ),
        (
            true,
            Some(px_record()),
            Some("http://127.0.0.1:9141".to_string()),
            Some("clp_key".to_string()),
            false
        ),
        "(the landing refused, px's record, px's base_url, px's key, a login stored in px)"
    );
}

/// Claude Code writes the login before its `.claude.json` account block lands,
/// then refreshes: one not-saved line in all (the memo is replaced on every
/// check, so the late block is no new login).
#[test]
fn a_refresh_after_the_account_block_lands_late_raises_no_second_line() {
    let (_home, mut config) = active_proxy_account();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let _cli = crate::logline::StampOverride::new(false);

    write_live_login("at-r20-late-1", "rt-r20-late-1");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    write_home_account("uuid-r20-late");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();
    write_live_login("at-r20-late-2", "rt-r20-late-2");
    crate::claude::snapshot_active_credentials(&mut config).unwrap();

    assert_eq!(
        lines.snapshot(),
        vec![not_saved_line("clauth")],
        "(lines: login, account block lands, refresh)"
    );
}

/// A CLI switch away from a plain account whose live login diverged still asks
/// its question, and Enter (stdin at EOF) saves the login into it: the proxy
/// arm leaves a non-proxy account byte-for-byte.
#[test]
fn a_cli_switch_away_from_a_plain_account_still_asks() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    for (name, access, refresh) in [
        ("acc", "at-r20-acc-held", "rt-r20-acc-held"),
        ("other", "at-r20-other", "rt-r20-other"),
    ] {
        let mut p = crate::profile::Profile::new(name.to_string(), None, None);
        p.credentials = Some(oauth_login_of(access, refresh));
        crate::profile::save_profile(&p).unwrap();
        config.add(p);
    }
    config.state.profiles = vec![ProfileName::from("acc"), ProfileName::from("other")];
    config.state.active_profile = Some(ProfileName::from("acc"));
    crate::profile::save_app_state(&config.state).unwrap();
    crate::testutil::register_names(&["acc", "other"]);
    write_live_login("at-r20-cli-plain", "rt-r20-cli-plain");
    let out = crate::out::OutCapture::new();
    let _capture = out.capture_here();

    let switched =
        crate::actions::switch_profile_cli(config, &ProfileName::from("other"), std::io::empty())
            .map_err(|e| e.to_string());
    let acc_store = std::fs::read_to_string(
        crate::profile::profile_dir(&ProfileName::from("acc"))
            .unwrap()
            .join("credentials.json"),
    )
    .unwrap_or_default();

    assert_eq!(
        (
            switched,
            String::from_utf8(out.snapshot()).unwrap(),
            acc_store.contains("rt-r20-cli-plain"),
        ),
        (
            Ok(()),
            "clauth: 'acc' has a newer login in ~/.claude. save it into 'acc' and switch to \
             'other'? [Y/n] clauth: switched to 'other'\n"
                .to_string(),
            true
        ),
        "(switch, stdout, acc's store holds the live login)"
    );
}

/// A plain `px` saved, then deleted and re-created on disk as an account on
/// proxy "stub" (`clauth delete px` + `clauth login px --proxy stub` in another
/// terminal), handing back the config a re-login loaded before the swap.
fn px_recreated_as_a_proxy_account_under(
    plain: crate::profile::Profile,
) -> crate::profile::AppConfig {
    crate::profile::save_profile(&plain).unwrap();
    crate::testutil::register_names(&["px"]);
    let config = crate::profile::load_config().unwrap();
    std::fs::remove_dir_all(crate::profile::profile_dir(&ProfileName::from("px")).unwrap())
        .unwrap();
    let recreated = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        Some("clp_key".to_string()),
    );
    crate::profile::save_profile(&recreated).unwrap();
    crate::profile::write_proxy_record(&recreated.name, &px_record()).unwrap();
    config
}

/// A console re-login's landing (`store_console_login`, which `clauth login
/// px` on an Alibaba profile reaches after its browser) onto a name re-created
/// as a proxy account meanwhile is refused at the landing with the re-login
/// line and exit 2, storing nothing.
#[test]
fn a_console_landing_onto_a_name_recreated_as_a_proxy_account_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = px_recreated_as_a_proxy_account_under(crate::profile::Profile::new(
        "px".into(),
        None,
        None,
    ));
    let account = crate::profile::claim_profile_id(&ProfileName::from("px")).unwrap();
    let before = config_bytes("px");

    let err = crate::actions::store_console_login(
        &mut config,
        &ProfileName::from("px"),
        &account,
        crate::profile::ConsoleCredential {
            token: "console-r20".to_string(),
            site: crate::profile::ConsoleSite::International,
            region: "ap-southeast-1".to_string(),
        },
    )
    .expect_err("the landing refuses");

    assert_eq!(
        (
            err.to_string(),
            exit_line(err),
            config_bytes("px") == before
        ),
        (
            PX_RELOGIN_REFUSED.to_string(),
            (2, format!("Error: {PX_RELOGIN_REFUSED}\n")),
            true
        ),
        "(error, (exit code, printed line), px's config.toml kept)"
    );
}

/// A setup-token capture's landing (`cmd_login_setup_token`'s write, plain and
/// rolling) onto a name re-created as a proxy account while the token was
/// pasted is refused at the landing with the re-login line and exit 2,
/// writing no sidecar and no backup.
#[test]
fn a_setup_token_landing_onto_a_name_recreated_as_a_proxy_account_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();
    px_recreated_as_a_proxy_account_under(crate::profile::Profile::new("px".into(), None, None));
    let before = config_bytes("px");
    let px = ProfileName::from("px");
    let token = "sk-ant-oat01-r20-setup-token";

    let plain = crate::claude::write_session_token(&px, token, 0).map_err(exit_line);
    let rolling = crate::claude::write_session_token_with_backup(&px, token, 0).map_err(exit_line);
    let refused = (2, format!("Error: {PX_RELOGIN_REFUSED}\n"));

    assert_eq!(
        (
            plain,
            rolling,
            crate::profile::profile_subpath(&px, "session-token.json")
                .unwrap()
                .exists(),
            crate::profile::profile_subpath(&px, "session-token.static.json")
                .unwrap()
                .exists(),
            config_bytes("px") == before,
        ),
        (Err(refused.clone()), Err(refused), false, false, true),
        "(the plain write, the rolling write, px's sidecar, px's backup, px's config.toml kept)"
    );
}

// ── the macOS bare Keychain item's carry (its rules run on every platform) ──

/// A store's login, expiring in 2096.
const STORE_EXPIRES_AT: i64 = 4_000_000_001_000;
/// The bare item's later login, which outranks a store's: the one a carry
/// writes over it.
const ITEM_EXPIRES_AT: i64 = 4_000_000_002_000;

/// A rotating login as the bare item and a store hold it.
fn bare_login(access: &str, refresh: &str, expires_at: i64) -> Value {
    json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
        }
    })
}

/// Plain `a` and proxy account `px`, each holding a login of its own with its
/// account id cached (`uuid-a`, `px_account`), `active` the active one.
/// Returns px's stored `credentials.json` bytes. Each caller names its own
/// `px_account`: the not-saved line goes out once per account per process.
fn bare_item_beside_a_proxy_account(active: &str, px_account: &str) -> Vec<u8> {
    for (name, account) in [("a", "uuid-a"), ("px", px_account)] {
        let profile = ProfileName::from(name);
        crate::profile::save_profile(&crate::profile::Profile::new(name.to_string(), None, None))
            .unwrap();
        let dir = crate::profile::profile_dir(&profile).unwrap();
        std::fs::write(
            dir.join("credentials.json"),
            serde_json::to_vec_pretty(&bare_login(
                &format!("{name}-acc"),
                &format!("{name}-ref"),
                STORE_EXPIRES_AT,
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join(crate::profile_cache::ACCOUNT_ID_CACHE_FILE),
            serde_json::to_vec(&crate::profile::AccountId::from(account)).unwrap(),
        )
        .unwrap();
    }
    crate::profile::write_proxy_record(
        &ProfileName::from("px"),
        &ProxyAccount {
            service: stub_service(),
            account: ACCOUNT.to_string(),
        },
    )
    .unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        active_profile: Some(active.into()),
        profiles: vec!["a".into(), "px".into()],
        ..Default::default()
    })
    .unwrap();
    std::fs::read(
        crate::profile::profile_subpath(&ProfileName::from("px"), "credentials.json").unwrap(),
    )
    .unwrap()
}

fn px_store() -> Vec<u8> {
    std::fs::read(
        crate::profile::profile_subpath(&ProfileName::from("px"), "credentials.json").unwrap(),
    )
    .unwrap()
}

/// The not-saved line for a login Claude Code signed in with on `px`.
const PX_LOGIN_NOT_SAVED: &str = "Claude Code signed in while proxy account 'px' was active; clauth \
                                  did not save that login into 'px'. To keep it as an account, \
                                  run `clauth login <name>` with a new name";

/// The not-saved line for a bare Keychain login whose owner is proxy account
/// `px` while another account is active.
const PX_BARE_ITEM_NOT_SAVED: &str = "Claude Code's Keychain login for proxy account 'px' was not \
                                      saved into 'px'. To keep it as an account, run `clauth \
                                      login <name>` with a new name";

/// A switch whose bare item holds a login of proxy account `px`'s account
/// never saves it into `px`: the settle says so and lets the switch write
/// over the item, as a session's carry skips a proxy store.
#[test]
fn a_switch_never_settles_the_bare_items_login_into_a_proxy_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let before = bare_item_beside_a_proxy_account("a", "uuid-px-settle");
    let item = bare_login("px-settle-acc", "px-settle-ref", ITEM_EXPIRES_AT);

    let settled = crate::claude::settle_bare_item_before_write(
        &item,
        crate::claude::BareItemWriter::Switch,
        |_| Ok(crate::profile::AccountId::from("uuid-px-settle")),
    )
    .map_err(|e| format!("{e:#}"));

    let placed: [u8; 32] =
        <sha2::Sha256 as sha2::Digest>::digest("px-settle-acc".as_bytes()).into();
    assert_eq!(
        (settled, px_store() == before),
        (
            Ok(crate::claude::BareItemSettle::Settled(
                crate::claude::BareItemSettled {
                    notice: Some(crate::claude::BareItemNotice::NotSaved(
                        PX_BARE_ITEM_NOT_SAVED.to_string()
                    )),
                    saved_into: None,
                    placed: Some(placed),
                }
            )),
            true
        ),
        "(the settle, px's store kept)"
    );
}

/// The user's "save" of the bare item into proxy account `px` (the CLI
/// prompt's and the TUI modal's) refuses with the re-login line, writing
/// nothing.
#[test]
fn saving_the_bare_items_login_into_a_proxy_account_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();
    let before = bare_item_beside_a_proxy_account("a", "uuid-px-save");
    let item = bare_login("px-save-acc", "px-save-ref", ITEM_EXPIRES_AT);

    let saved = crate::claude::save_bare_item_into(
        &ProfileName::from("px"),
        &crate::testutil::profile_id("px"),
        &item,
        crate::claude::BareItemWriter::Switch,
    )
    .map_err(|e| format!("{e:#}"));

    assert_eq!(
        (saved, px_store() == before),
        (Err(PX_RELOGIN_REFUSED.to_string()), true),
        "(the save, px's store kept)"
    );
}

/// The daemon's bare-item leg never carries a login into proxy account `px`,
/// neither as the active account's own successor nor as a rescue into it
/// while another account is active: the item stays where Claude Code left
/// it, nothing is restored over it, and the not-saved line goes out.
#[test]
fn the_bare_item_leg_never_carries_a_login_into_a_proxy_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let _cli = crate::logline::StampOverride::new(false);
    let mut runs = Vec::new();
    for (active, token) in [("px", "px-adopt"), ("a", "px-rescue")] {
        let account = format!("uuid-{token}");
        let before = bare_item_beside_a_proxy_account(active, &account);
        let item = bare_login(
            &format!("{token}-acc"),
            &format!("{token}-ref"),
            ITEM_EXPIRES_AT,
        );
        let restores = std::cell::RefCell::new(0_usize);
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();
        crate::claude::correct_bare_item(
            &item,
            Some(&ProfileName::from(active)),
            |_| Ok(crate::profile::AccountId::from(account.as_str())),
            |_| {
                *restores.borrow_mut() += 1;
                Ok(true)
            },
        );
        drop(capture);
        runs.push((
            px_store() == before,
            restores.into_inner(),
            lines.snapshot(),
        ));
    }
    assert_eq!(
        runs,
        vec![
            (true, 0, vec![format!("clauth: {PX_LOGIN_NOT_SAVED}")]),
            (true, 0, vec![format!("clauth: {PX_BARE_ITEM_NOT_SAVED}")])
        ],
        "(px's store kept, restores, lines) with px active, then a"
    );
}

/// The claim a bare-item save makes as it names its owner (the CLI prompt's
/// and the TUI modal's) never claims proxy account `px`: its `config.toml`,
/// which holds no id, gets none minted.
#[test]
fn a_bare_item_save_never_claims_a_proxy_account() {
    let _home = crate::testutil::HomeSandbox::new();
    bare_item_beside_a_proxy_account("a", "uuid-px-claim");
    let config = || {
        std::fs::read(
            crate::profile::profile_subpath(&ProfileName::from("px"), "config.toml").unwrap(),
        )
        .unwrap()
    };
    let before = config();

    let claimed = crate::claude::claim_bare_item_owner(&ProfileName::from("px"))
        .map(|account| format!("{account:?}"))
        .map_err(|e| format!("{e:#}"));

    assert_eq!(
        (claimed, config() == before),
        (Ok("None".to_string()), true),
        "(the claim, px's config.toml kept)"
    );
}

/// A switch's settle and the daemon's bare-item leg, meeting an item whose
/// login is proxy account `px`'s, never mint an id into `px`'s `config.toml`
/// (which holds none): neither while `px`'s account is cached, nor while the
/// settle has to look it up first.
#[test]
fn a_settle_or_the_bare_item_leg_never_claims_a_proxy_account() {
    let config = || {
        std::fs::read(
            crate::profile::profile_subpath(&ProfileName::from("px"), "config.toml").unwrap(),
        )
        .unwrap()
    };
    let mut kept = Vec::new();
    for (run, cached) in [("settle", true), ("settle", false), ("leg", true)] {
        let _home = crate::testutil::HomeSandbox::new();
        let account = format!("uuid-px-{run}-{cached}");
        bare_item_beside_a_proxy_account("a", &account);
        if !cached {
            std::fs::remove_file(
                crate::profile::profile_subpath(
                    &ProfileName::from("px"),
                    crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
                )
                .unwrap(),
            )
            .unwrap();
        }
        let before = config();
        let item = bare_login(
            &format!("px-{run}-acc"),
            &format!("px-{run}-ref"),
            ITEM_EXPIRES_AT,
        );
        if run == "settle" {
            let _ = crate::claude::settle_bare_item_before_write(
                &item,
                crate::claude::BareItemWriter::Switch,
                |_| Ok(crate::profile::AccountId::from(account.as_str())),
            );
        } else {
            crate::claude::correct_bare_item(
                &item,
                Some(&ProfileName::from("a")),
                |_| Ok(crate::profile::AccountId::from(account.as_str())),
                |_| Ok(true),
            );
        }
        kept.push((run, cached, config() == before));
    }
    assert_eq!(
        kept,
        vec![
            ("settle", true, true),
            ("settle", false, true),
            ("leg", true, true)
        ],
        "(path, px's account cached, px's config.toml kept)"
    );
}

/// A switch away from proxy account `px` while `px` is active, whose bare
/// item holds `px`'s login, says the login Claude Code signed in with on `px`
/// was not saved: the active-account sentence, never the owner one.
#[test]
fn a_switch_off_an_active_proxy_owner_settles_with_the_active_sentence() {
    let _home = crate::testutil::HomeSandbox::new();
    let before = bare_item_beside_a_proxy_account("px", "uuid-px-active");
    let item = bare_login("px-active-acc", "px-active-ref", ITEM_EXPIRES_AT);

    let settled = crate::claude::settle_bare_item_before_write(
        &item,
        crate::claude::BareItemWriter::Switch,
        |_| Ok(crate::profile::AccountId::from("uuid-px-active")),
    )
    .map_err(|e| format!("{e:#}"));

    let placed: [u8; 32] =
        <sha2::Sha256 as sha2::Digest>::digest("px-active-acc".as_bytes()).into();
    assert_eq!(
        (settled, px_store() == before),
        (
            Ok(crate::claude::BareItemSettle::Settled(
                crate::claude::BareItemSettled {
                    notice: Some(crate::claude::BareItemNotice::NotSaved(
                        PX_LOGIN_NOT_SAVED.to_string()
                    )),
                    saved_into: None,
                    placed: Some(placed),
                }
            )),
            true
        ),
        "(the settle, px's store kept)"
    );
}

/// A proxy create into an off-roster dir holding a `credentials.json` refuses
/// before writing anything, as every other create does
/// (`a_create_over_a_leftover_login_refuses_and_keeps_its_bytes`): the
/// leftover login, which may hold a single-use refresh token, survives.
#[test]
fn a_proxy_create_over_a_leftover_login_refuses_and_keeps_its_bytes() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::load_config().unwrap();
    let dir = crate::profile::profile_dir(&ProfileName::from("left")).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let login = serde_json::to_vec(&crate::testutil::oauth_pair("left-login")).unwrap();
    std::fs::write(dir.join("credentials.json"), &login).unwrap();
    std::fs::write(dir.join("config.toml"), "# leftover\n").unwrap();

    let created = crate::actions::create_proxy_profile(
        &mut config,
        "left".to_string(),
        px_record(),
        "http://127.0.0.1:9141".to_string(),
        KEY.to_string(),
        None,
    )
    .map_err(|e| e.to_string());

    assert_eq!(
        (
            created,
            std::fs::read(dir.join("credentials.json")).ok(),
            std::fs::read_to_string(dir.join("config.toml")).ok(),
            crate::profile::load_app_state()
                .unwrap()
                .profiles
                .contains(&ProfileName::from("left")),
        ),
        (
            Err(format!(
                "profile 'left' not created: {} holds a credentials.json no account owns, move \
                 or delete that directory first",
                dir.display()
            )),
            Some(login),
            Some("# leftover\n".to_string()),
            false,
        ),
        "(the create, the leftover login, the leftover config, rostered)"
    );
}

/// `clauth login <name> --proxy` under a new name whose off-roster dir holds a
/// `credentials.json` refuses before the browser flow opens: no login starts
/// on the proxy, so no account is made there, and the leftover login keeps its
/// bytes.
#[test]
fn a_proxy_login_over_a_leftover_login_refuses_before_the_flow_opens() {
    let _home = crate::testutil::HomeSandbox::new();
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let doors = crate::ProxyLoginDoors::capture_here();
    let paste = doors.paste();
    let dir = crate::profile::profile_dir(&ProfileName::from("left")).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let login = serde_json::to_vec(&crate::testutil::oauth_pair("left-login")).unwrap();
    std::fs::write(dir.join("credentials.json"), &login).unwrap();

    let result = crate::login_proxy(&proxy_login("left", None), "stub").map_err(|e| e.to_string());
    drop(paste);
    let (started, deleted) = {
        let seen = stub.state();
        (seen.login_bodies.len(), seen.deleted.clone())
    };

    assert_eq!(
        (
            result,
            started,
            deleted,
            std::fs::read(dir.join("credentials.json")).ok(),
        ),
        (
            Err(format!(
                "profile 'left' not created: {} holds a credentials.json no account owns, move \
                 or delete that directory first",
                dir.display()
            )),
            0,
            Vec::<String>::new(),
            Some(login),
        ),
        "(the login, logins started on the proxy, accounts deleted there, the leftover login)"
    );
}

/// A live usage read of a proxy account authenticates with clauth's admin
/// token, never the profile's key, so it says nothing about a key a delegate's
/// 401 proved dead: the dead-key record and the member's broken mark both
/// stand.
#[test]
fn a_live_proxy_usage_read_keeps_a_dead_key_verdict() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin_token = mint_admin_token();
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", &stub.base);
    let name = ProfileName::from("px");
    let profile = config.find(&name).unwrap().clone();
    crate::profile_cache::write_dead_key(
        &name,
        crate::usage::profile_key_fingerprint(&profile).unwrap(),
    );
    let mark = dead_login_fingerprint(&profile).unwrap();
    let state = leg_state();
    state
        .third_party_broken
        .lock()
        .unwrap()
        .insert("px".to_string(), mark);
    let before = crate::fallback::third_party_key_rejected(&profile, &name);
    crate::usage::reset_request_slots();

    run_leg(&state, &config);

    assert_eq!(
        (
            before,
            state
                .third_party_usage_store
                .lock()
                .unwrap()
                .contains_key("px"),
            crate::fallback::third_party_key_rejected(&profile, &name),
            state.third_party_broken.lock().unwrap().get("px").copied(),
        ),
        (true, true, true, Some(mark)),
        "(rejected before the read, the read landed, rejected after it, the broken mark)"
    );
}

/// Proxy account `name` (proxy `stub`) holding a stored OAuth chain beside its
/// inference credential, on disk and in `config`. `keyed`: the proxy-local key,
/// with a dead-key record for it; else no key and an `[env]` token, the
/// keyless shape an own endpoint reads.
fn proxy_account_with_a_chain(config: &mut crate::profile::AppConfig, name: &str, keyed: bool) {
    create_proxy_profile_on_disk(config, name, "http://127.0.0.1:9141");
    let pn = ProfileName::from(name);
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    if !keyed {
        profile.api_key = None;
        profile
            .env
            .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "tok-env".to_string());
    }
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(profile).unwrap();
    let loaded = crate::profile::load_profile(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            crate::claude::has_usable_api_key(&loaded),
            loaded.refresh_token().is_some(),
        ),
        (true, keyed, true),
        "fixture control: (the runtime proxy claim, a usable key, a stored chain)"
    );
    if keyed {
        crate::profile_cache::write_dead_key(
            &pn,
            crate::usage::profile_key_fingerprint(&loaded).unwrap(),
        );
    }
}

/// `clauth rolling-token` on a quarantined proxy account refuses with
/// the token-mode refusal, for a rejected key and for a missing one; the
/// quarantine's own copy for each still names the login through its proxy.
#[test]
fn rolling_token_on_a_quarantined_proxy_account_names_the_login_through_its_proxy() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    proxy_account_with_a_chain(&mut config, "px", true);
    proxy_account_with_a_chain(&mut config, "pk", false);
    config.state.auth_broken = vec!["px".into(), "pk".into()];
    crate::profile::save_app_state(&config.state).unwrap();

    let copy = |name: &str| {
        let pn = ProfileName::from(name);
        crate::oauth::third_party_dead_chain_copy(config.find(&pn), &pn)
    };
    assert_eq!(
        (
            cli_outcome(crate::cmd_rolling_token("px")),
            cli_outcome(crate::cmd_rolling_token("pk")),
            copy("px"),
            copy("pk"),
        ),
        (
            Err((no_claude_code_login("px"), 2)),
            Err((no_claude_code_login("pk"), 2)),
            Some("api key rejected: px (run `clauth login px --proxy stub`)".to_string()),
            Some("profile has no api key: pk (run `clauth login pk --proxy stub`)".to_string()),
        ),
        "(rolling-token on px, on pk as (text, exit), the quarantine's copy for px, for pk)"
    );
}

/// The quarantine's own log line on a proxy account names the login through
/// its proxy, for a rejected key and for a missing one.
#[test]
fn the_quarantine_logline_on_a_proxy_account_names_the_login_through_its_proxy() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    proxy_account_with_a_chain(&mut config, "px", true);
    proxy_account_with_a_chain(&mut config, "pk", false);
    let handle = Arc::new(RankedMutex::new(config));

    let sink = crate::logline::LogLines::new();
    let _capture = sink.capture_here();
    for name in ["px", "pk"] {
        crate::oauth::mark_auth_broken(&handle, &ProfileName::from(name), true);
    }

    assert_eq!(
        sink.snapshot(),
        vec![
            "clauth: api key rejected: px (run `clauth login px --proxy stub`) \
             (flagged auth_broken)"
                .to_string(),
            "clauth: profile has no api key: pk (run `clauth login pk --proxy stub`) \
             (flagged auth_broken)"
                .to_string(),
        ]
    );
}

/// The manual-rotate worker never spends a proxy account's stored chain, for a
/// keyed account and a keyless one, even when it runs from a confirm armed
/// before the name became a proxy account: no token request leaves, no result
/// is sent (the silent leg of a profile with no refresh token), no quarantine
/// is written and the stored pair stays byte for byte.
#[test]
fn the_rotate_worker_never_spends_a_proxy_accounts_stored_chain() {
    let home = crate::testutil::HomeSandbox::new();
    // A correct run sends nothing; a spend sends one refresh per account, so
    // two records every request a spend could make.
    let (base, server) = crate::testutil::serve_endpoints(2, |_, _| {
        (400, r#"{"error": "invalid_grant"}"#.to_string())
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    proxy_account_with_a_chain(&mut config, "px", true);
    proxy_account_with_a_chain(&mut config, "pk", false);
    let handle = Arc::new(RankedMutex::new(config));
    let activity = crate::testutil::fresh_activity();
    let refetch: RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let stored = |name: &str| {
        std::fs::read(
            crate::profile::profile_subpath(&ProfileName::from(name), "credentials.json").unwrap(),
        )
        .unwrap()
    };
    let before = (stored("px"), stored("pk"));

    let rotate = |name: &str| {
        let persisted = crate::oauth::rotate_one(
            &handle,
            &ProfileName::from(name),
            &refetch,
            &activity,
            None,
            &tx,
        );
        let sent = rx.try_recv().ok().map(|result| match result.outcome {
            Ok(_) => "<rotated>".to_string(),
            Err(e) => e.to_string(),
        });
        (persisted, sent)
    };
    let results = (rotate("px"), rotate("pk"));
    let seen = server.join().unwrap();

    assert_eq!(
        (
            results,
            seen,
            ["px", "pk"].map(|name| {
                crate::profile::load_app_state()
                    .unwrap()
                    .is_auth_broken(&ProfileName::from(name))
            }),
            (stored("px"), stored("pk")) == before,
        ),
        (
            ((false, None), (false, None)),
            Vec::<String>::new(),
            [false, false],
            true,
        ),
        "((px rotated, px result), (pk ...)), the requests sent, the quarantines on disk, \
         the stored pairs kept"
    );
}

/// The token-mode refusal `clauth static-token` and `clauth rolling-token`
/// print for proxy account `name` of proxy `stub`.
fn no_claude_code_login(name: &str) -> String {
    format!(
        "'{name}' is an account on proxy \"stub\": it signs in through its proxy and has no \
         Claude Code login to roll or restore"
    )
}

/// A CLI command's outcome as (its error text, its exit code).
fn cli_outcome(result: anyhow::Result<()>) -> Result<(), (String, i32)> {
    result.map_err(|e| {
        let text = format!("{e:#}");
        (text, crate::exit_code_to(Err(e), &mut Vec::new()))
    })
}

/// Every file directly under profile `name`'s dir, by name, with its bytes.
fn profile_dir_files(name: &str) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let config = crate::profile::profile_subpath(&ProfileName::from(name), "config.toml").unwrap();
    let mut files: Vec<_> = std::fs::read_dir(config.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            (
                path.file_name().unwrap().to_owned(),
                std::fs::read(&path).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

/// `clauth static-token` and `clauth rolling-token` on a proxy account refuse
/// up front with the token-mode refusal at exit 2 and change nothing: the
/// `rolling_token` flag static-token would flip first stays on, no sidecar or
/// lock file appears, and no request leaves.
#[test]
fn static_and_rolling_token_refuse_a_proxy_account_and_change_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    // A correct run sends nothing; a refresh or a roll would send at least one
    // request per command.
    let (base, server) = crate::testutil::serve_endpoints(2, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (400, r#"{"error": "invalid_grant"}"#.to_string())
        } else {
            (401, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let pn = ProfileName::from("px");
    crate::profile::update_profile_config(&pn, |p| {
        p.rolling_token = true;
        Ok(())
    })
    .unwrap();
    let loaded = crate::profile::load_profile(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.rolling_token,
            loaded.refresh_token()
        ),
        (true, true, Some("rt-px")),
        "fixture control: (the runtime proxy claim, the rolling flag, the stored chain)"
    );
    let before = profile_dir_files("px");

    let outcomes = (
        cli_outcome(crate::cmd_static_token("px")),
        cli_outcome(crate::cmd_rolling_token("px")),
    );
    let seen = server.join().unwrap();

    assert_eq!(
        (outcomes, profile_dir_files("px") == before, seen),
        (
            (
                Err((no_claude_code_login("px"), 2)),
                Err((no_claude_code_login("px"), 2)),
            ),
            true,
            Vec::<String>::new(),
        ),
        "((static-token, rolling-token) as (text, exit)), px's files kept, the requests sent"
    );
}

/// `clauth rolling-token` on a quarantined proxy account on its bind whose
/// stored key authenticates nothing (one the header check refuses, so the
/// load keeps the bind) refuses with the token-mode refusal; the quarantine's own
/// copy for it still names the missing key and the login through its proxy.
#[test]
fn rolling_token_on_a_quarantined_keyless_proxy_account_on_its_bind_names_its_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    proxy_account_with_a_chain(&mut config, "pu", true);
    let pn = ProfileName::from("pu");
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.api_key = Some("sk-test\r\nInjected: x".to_string());
    crate::profile::save_profile(profile).unwrap();
    config.state.auth_broken = vec!["pu".into()];
    crate::profile::save_app_state(&config.state).unwrap();
    let loaded = crate::profile::load_profile(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.base_url.as_deref(),
            loaded.is_third_party(),
            crate::claude::has_inference_auth(&loaded),
        ),
        (true, Some("http://127.0.0.1:9141"), false, false),
        "fixture control: (the runtime proxy claim, the bind, a recognised provider, an auth source)"
    );

    assert_eq!(
        (
            cli_outcome(crate::cmd_rolling_token("pu")),
            crate::oauth::third_party_dead_chain_copy(Some(&loaded), &pn),
        ),
        (
            Err((no_claude_code_login("pu"), 2)),
            Some("profile has no api key: pu (run `clauth login pu --proxy stub`)".to_string()),
        ),
        "(rolling-token as (text, exit), the quarantine's copy)"
    );
}

/// `clauth rolling-token` on a quarantined proxy account whose key still works
/// refuses with the token-mode refusal; the quarantine's own copy names the login
/// through its proxy as its fix, never `--api-key`, which clauth refuses for a
/// proxy account.
#[test]
fn rolling_token_on_a_proxy_account_with_a_dead_chain_names_the_login_through_its_proxy() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", "http://127.0.0.1:9141");
    let pn = ProfileName::from("px");
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-px".to_string(),
            refresh_token: Some("rt-px".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(profile).unwrap();
    config.state.auth_broken = vec!["px".into()];
    crate::profile::save_app_state(&config.state).unwrap();
    let loaded = crate::profile::load_profile(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            crate::claude::has_usable_api_key(&loaded),
            loaded.refresh_token().is_some(),
            crate::fallback::third_party_key_rejected(&loaded, &pn),
        ),
        (true, true, true, false),
        "fixture control: (the runtime proxy claim, a usable key, a stored chain, a rejected key)"
    );

    assert_eq!(
        (
            cli_outcome(crate::cmd_rolling_token("px")),
            crate::oauth::third_party_dead_chain_copy(Some(&loaded), &pn),
        ),
        (
            Err((no_claude_code_login("px"), 2)),
            Some(
                "stored OAuth chain is dead, its api key still works: px \
                 (run `clauth login px --proxy stub` to clear the quarantine)"
                    .to_string()
            ),
        ),
        "(rolling-token as (text, exit), the quarantine's copy)"
    );
}

/// A `--proxy` re-login that saves clears the account's quarantine, like every
/// other re-login, so the dead-chain sentence naming it as the fix stops.
#[test]
fn a_proxy_relogin_clears_the_quarantine() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::load_config().unwrap();
    crate::actions::create_blank_profile(
        &mut config,
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
        crate::profile::ModelSettings::default(),
        None,
    )
    .unwrap();
    let pn = ProfileName::from("px");
    let mut state = crate::profile::load_app_state().unwrap();
    state.set_auth_broken(&pn, true);
    crate::profile::save_app_state(&state).unwrap();
    config.state.set_auth_broken(&pn, true);
    assert!(
        crate::profile::load_app_state()
            .unwrap()
            .is_auth_broken(&pn),
        "fixture control: the quarantine is on disk before the re-login"
    );

    let saved = crate::actions::update_proxy_login(
        &mut config,
        &pn,
        &px_record(),
        Some(KEY.to_string()),
        None,
    )
    .unwrap();

    assert_eq!(
        (
            saved,
            crate::profile::load_app_state()
                .unwrap()
                .is_auth_broken(&pn),
        ),
        (crate::actions::ProxyRelogin::Saved, false),
        "(the re-login saved, the quarantine on disk after it)"
    );
}

/// A keyless proxy account whose `[env] ANTHROPIC_BASE_URL` routes it off its
/// bind is on no bind: the keyless test reads the endpoint the account
/// actually routes to, and an unrecognised one may take no key.
#[test]
fn a_keyless_proxy_account_routed_off_its_bind_reads_not_keyless() {
    let mut profile = crate::profile::Profile::new(
        "px".to_string(),
        Some("http://127.0.0.1:9141".to_string()),
        None,
    );
    profile.env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "http://10.0.0.1:1234".to_string(),
    );
    profile.proxy = Some(px_record());
    assert_eq!(
        (
            profile.routing_endpoint(),
            profile.provider,
            crate::claude::has_inference_auth(&profile),
        ),
        (Some("http://10.0.0.1:1234"), None, false),
        "fixture control: (the routed endpoint, a recognised provider, an auth source)"
    );

    assert!(!profile.is_keyless());
}

/// A CLI re-login's claim onto a name that is an id-less proxy account by the
/// time it runs refuses with the re-login refusal under the hold the claim
/// writes under, and mints no `id` into the account's `config.toml`.
#[test]
fn a_reauth_claim_onto_a_proxy_account_is_refused_and_mints_no_id() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", "http://127.0.0.1:9141");
    let pn = ProfileName::from("px");
    let path = crate::profile::profile_subpath(&pn, "config.toml").unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(
        !before.lines().any(|line| line.starts_with("id ")),
        "fixture control: px's config.toml holds no id"
    );

    let reauth = crate::reauth_profile(
        &pn,
        None,
        |_| Ok(false),
        |_| anyhow::bail!("the claim refuses before any credential is collected"),
    )
    .map(|landed| landed.is_some())
    .map_err(|e| {
        let text = format!("{e:#}");
        (text, crate::exit_code_to(Err(e), &mut Vec::new()))
    });

    assert_eq!(
        (reauth, std::fs::read_to_string(&path).unwrap() == before),
        (Err((PX_RELOGIN_REFUSED.to_string(), 2)), true),
        "((the refusal, its exit code), px's config.toml kept)"
    );
}

/// A bare-item save that claimed nothing for a name still a proxy account
/// refuses with the re-login refusal, never as a name re-created since.
#[test]
fn a_claimless_bare_item_save_into_a_proxy_account_refuses_with_the_relogin_line() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", "http://127.0.0.1:9141");
    let pn = ProfileName::from("px");
    let path = crate::profile::profile_subpath(&pn, "config.toml").unwrap();
    let before = std::fs::read(&path).unwrap();

    let saved = crate::claude::save_bare_item_into_claimed(
        &pn,
        None,
        &json!({}),
        crate::claude::BareItemWriter::Switch,
    )
    .map_err(|e| format!("{e:#}"));

    assert_eq!(
        (
            saved,
            std::fs::read(&path).unwrap() == before,
            crate::profile::profile_subpath(&pn, "credentials.json")
                .unwrap()
                .exists(),
        ),
        (Err(PX_RELOGIN_REFUSED.to_string()), true, false),
        "(the save, px's config.toml kept, a credentials.json written)"
    );
}

/// Proxy account `name` on the stub's bind holding a stored OAuth chain whose
/// access token expired a minute ago, saved with `api_key` and `env` as given,
/// loaded back the way every reader sees it. `quarantined` writes its
/// `auth_broken` flag to disk.
fn proxy_account_with_an_expired_chain(
    name: &str,
    api_key: Option<&str>,
    env: &[(&str, &str)],
    quarantined: bool,
) -> crate::profile::AppConfig {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, name, "http://127.0.0.1:9141");
    let pn = ProfileName::from(name);
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.api_key = api_key.map(str::to_string);
    for (key, value) in env {
        profile.env.insert(key.to_string(), value.to_string());
    }
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(crate::usage::now_ms() as i64 - 60_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(profile).unwrap();
    if quarantined {
        config.state.auth_broken = vec![name.into()];
        crate::profile::save_app_state(&config.state).unwrap();
    }
    crate::profile::load_config().unwrap()
}

/// A scheduler tick over the work list its real producer builds never uses
/// a proxy account's stored OAuth chain, though its access token has expired
/// and the token endpoint would refuse the refresh: no token request and no
/// `/usage` poll on the chain leave, the quarantine stays
/// clear, and the stored pair stays byte for byte.
#[test]
fn a_scheduler_tick_never_spends_a_proxy_accounts_stored_chain() {
    let home = crate::testutil::HomeSandbox::new();
    // A correct run sends nothing on the chain; a use sends the usage poll and
    // the refresh, so four records every request a use could make.
    let (base, server) = crate::testutil::serve_endpoints(4, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (400, r#"{"error": "invalid_grant"}"#.to_string())
        } else {
            (401, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let config = proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let pn = ProfileName::from("px");
    let loaded = config.find(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.refresh_token(),
            crate::usage::token_clock_expired(loaded.access_token_expires_at(), now_ms() as i64),
        ),
        (true, Some("rt-px"), true),
        "fixture control: (the runtime proxy claim, the stored chain, an expired access token)"
    );
    let credentials = crate::profile::profile_subpath(&pn, "credentials.json").unwrap();
    let stored = std::fs::read(&credentials).unwrap();
    let state = SchedulerState {
        tokens: Arc::new(RankedMutex::new(collect_tokens(&config))),
        config: Arc::new(RankedMutex::new(config)),
        ..leg_state()
    };

    tick(&state);
    let seen = server.join().unwrap();

    assert_eq!(
        (
            seen.iter().any(|path| path.starts_with("/v1/oauth/token")),
            state.config.lock().unwrap().is_auth_broken(&pn),
            crate::profile::load_app_state()
                .unwrap()
                .is_auth_broken(&pn),
            std::fs::read(&credentials).unwrap() == stored,
        ),
        (false, false, false, true),
        "(a token request, the quarantine in memory, on disk, the stored pair kept): {seen:?}"
    );
    assert_eq!(seen, Vec::<String>::new(), "the requests sent on the chain");
}

/// The switch gate never refreshes a proxy account's stored chain, even one
/// the load reads as an OAuth login (a stored pair and no key drop its bind):
/// it installs as is, with no refresh sent and no quarantine written.
#[test]
fn the_switch_gate_never_refreshes_a_proxy_accounts_stored_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = proxy_account_with_an_expired_chain("px", None, &[], false);
    let pn = ProfileName::from("px");
    let loaded = config.find(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.is_oauth(),
            loaded.refresh_token()
        ),
        (true, true, Some("rt-px")),
        "fixture control: (the runtime proxy claim, read as an OAuth login, the stored chain)"
    );
    let handle = Arc::new(RankedMutex::new(config));
    let sent = Mutex::new(Vec::new());

    let gate = crate::oauth::ensure_installable(
        &handle,
        &pn,
        |rt: &str, _scopes: Option<&str>| {
            sent.lock().unwrap().push(rt.to_string());
            Err(crate::oauth::RefreshError::Invalid {
                failure: crate::oauth::TokenFailure::Status(400),
                invalid_grant: true,
            })
        },
        None,
    );

    assert_eq!(
        (
            matches!(gate, crate::oauth::AuthGate::Ready),
            sent.into_inner().unwrap(),
            crate::profile::load_app_state()
                .unwrap()
                .is_auth_broken(&pn),
        ),
        (true, Vec::<String>::new(), false),
        "(the gate installs as is, the refreshes sent, the quarantine on disk)"
    );
}

/// Rotate-all names every account holding a stored chain except a proxy
/// account's.
#[test]
fn rotate_all_never_names_a_proxy_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let mut plain = crate::profile::Profile::new("oa".to_string(), None, None);
    plain.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-oa".to_string(),
            refresh_token: Some("rt-oa".to_string()),
            expires_at: Some(now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    config.add(plain);

    assert_eq!(
        crate::oauth::rotation_candidates(&config, true),
        vec![(ProfileName::from("oa"), "rt-oa".to_string())]
    );
}

/// The CLI switch's window kick never fires on a proxy account's stored
/// chain: no inference request and no refresh leave.
#[test]
fn the_switch_kick_never_fires_on_a_proxy_accounts_stored_chain() {
    let home = crate::testutil::HomeSandbox::new();
    // A correct run sends nothing; a kick sends itself, then the refresh its
    // 401 earns, so three records every request a kick could make.
    let (base, server) = crate::testutil::serve_endpoints(3, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (400, r#"{"error": "invalid_grant"}"#.to_string())
        } else {
            (401, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let mut config = proxy_account_with_an_expired_chain("px", None, &[], false);
    let pn = ProfileName::from("px");
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.auto_start = true;
    assert_eq!(
        (
            profile.proxy.is_some(),
            profile.is_oauth(),
            profile.access_token()
        ),
        (true, true, Some("at-px")),
        "fixture control: (the runtime proxy claim, read as an OAuth login, the stored chain)"
    );
    let handle = Arc::new(RankedMutex::new(config));

    let opened = crate::oauth::prime_window(&handle, &pn);
    let seen = server.join().unwrap();

    assert_eq!(
        (
            opened,
            seen,
            crate::profile::load_app_state()
                .unwrap()
                .is_auth_broken(&pn)
        ),
        (false, Vec::<String>::new(), false),
        "(the window opened, the requests sent, the quarantine on disk)"
    );
}

/// `clauth rolling-token` on a quarantined proxy account routed off its bind
/// with no key refuses with the token-mode refusal; the quarantine's own copy names
/// the missing key and the login through its proxy, never the browser login
/// clauth refuses for a proxy account.
#[test]
fn rolling_token_on_a_quarantined_proxy_account_routed_off_its_bind_names_its_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let config = proxy_account_with_an_expired_chain(
        "px",
        None,
        &[("ANTHROPIC_BASE_URL", "http://10.0.0.1:1234")],
        true,
    );
    let loaded = config.find(&ProfileName::from("px")).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.routing_endpoint(),
            loaded.is_keyless(),
            crate::claude::has_inference_auth(loaded),
            loaded.refresh_token().is_some(),
        ),
        (true, Some("http://10.0.0.1:1234"), false, false, true),
        "fixture control: (the runtime proxy claim, the routed endpoint, keyless, \
         an auth source, a stored chain)"
    );

    assert_eq!(
        (
            cli_outcome(crate::cmd_rolling_token("px")),
            crate::oauth::third_party_dead_chain_copy(Some(loaded), &ProfileName::from("px")),
        ),
        (
            Err((no_claude_code_login("px"), 2)),
            Some("profile has no api key: px (run `clauth login px --proxy stub`)".to_string()),
        ),
        "(rolling-token as (text, exit), the quarantine's copy)"
    );
}

/// `clauth rolling-token` on a proxy account with no stored chain refuses
/// with the token-mode refusal, the same as one holding a chain.
#[test]
fn rolling_token_on_a_proxy_account_without_a_chain_prescribes_no_login() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "pu", "http://127.0.0.1:9141");
    clear_stored_key("pu");
    let loaded = crate::profile::load_profile(&ProfileName::from("pu")).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.credentials.is_some(),
            crate::claude::has_own_inference_endpoint(&loaded),
        ),
        (true, false, false),
        "fixture control: (the runtime proxy claim, a stored chain, an own inference endpoint)"
    );

    assert_eq!(
        cli_outcome(crate::cmd_rolling_token("pu")),
        Err((no_claude_code_login("pu"), 2))
    );
}

/// `clauth static-token --clear` on a proxy account refuses with the
/// token-mode refusal at exit 2 and changes nothing, like the other
/// token-mode verbs.
#[test]
fn static_token_clear_refuses_a_proxy_account_and_changes_nothing() {
    let _home = crate::testutil::HomeSandbox::new();
    proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let before = profile_dir_files("px");

    let outcome = cli_outcome(crate::cmd_static_token_clear("px", true));

    assert_eq!(
        (outcome, profile_dir_files("px") == before),
        (Err((no_claude_code_login("px"), 2)), true),
        "(static-token --clear as (text, exit), px's files kept)"
    );
}

/// A proxy account whose `[proxy]` table clauth cannot read is still a proxy
/// account to the scheduler: a tick spends none of its stored chain, writes
/// no quarantine and keeps the stored pair byte for byte.
#[test]
fn a_tick_never_spends_the_chain_of_a_proxy_account_with_an_unreadable_record() {
    let home = crate::testutil::HomeSandbox::new();
    // A correct run sends nothing on the chain; a use sends the usage poll and
    // the refresh, so four records every request a use could make.
    let (base, server) = crate::testutil::serve_endpoints(4, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (400, r#"{"error": "invalid_grant"}"#.to_string())
        } else {
            (401, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let pn = ProfileName::from("px");
    let path = crate::profile::profile_subpath(&pn, "config.toml").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text.replace(&format!("account = \"{ACCOUNT}\""), "account = \"\"");
    assert_ne!(
        edited, text,
        "fixture control: the record's account was emptied"
    );
    std::fs::write(&path, edited).unwrap();
    let config = crate::profile::load_config().unwrap();
    let loaded = config.find(&pn).unwrap();
    assert_eq!(
        (
            matches!(
                crate::profile::stored_proxy_claim(&pn),
                crate::profile::ProxyClaim::Unreadable
            ),
            loaded.refresh_token(),
            crate::usage::token_clock_expired(loaded.access_token_expires_at(), now_ms() as i64),
        ),
        (true, Some("rt-px"), true),
        "fixture control: (an unreadable proxy claim, the stored chain, an expired access token)"
    );
    let credentials = crate::profile::profile_subpath(&pn, "credentials.json").unwrap();
    let stored = std::fs::read(&credentials).unwrap();
    let state = SchedulerState {
        tokens: Arc::new(RankedMutex::new(collect_tokens(&config))),
        config: Arc::new(RankedMutex::new(config)),
        ..leg_state()
    };

    tick(&state);
    let seen = server.join().unwrap();

    assert_eq!(
        (
            seen,
            crate::profile::load_app_state()
                .unwrap()
                .is_auth_broken(&pn),
            std::fs::read(&credentials).unwrap() == stored,
        ),
        (Vec::<String>::new(), false, true),
        "(the requests sent on the chain, the quarantine on disk, the stored pair kept)"
    );
}

/// The switch gate on a proxy account with `rolling_token` on and a live
/// stored chain (a hand edit) stamps no rolling sidecar from that chain and
/// sends no refresh: it installs as is.
#[test]
fn the_rolling_gate_never_stamps_from_a_proxy_accounts_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", "http://127.0.0.1:9141");
    let pn = ProfileName::from("px");
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.rolling_token = true;
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-px".to_string(),
            refresh_token: Some("rt-px".to_string()),
            expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(profile).unwrap();
    let config = crate::profile::load_config().unwrap();
    let loaded = config.find(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.rolling_token,
            loaded.access_token()
        ),
        (true, true, Some("at-px")),
        "fixture control: (the runtime proxy claim, the rolling flag, the stored chain)"
    );
    let sidecar = crate::profile::profile_subpath(&pn, "session-token.json").unwrap();
    let handle = Arc::new(RankedMutex::new(config));
    let sent = Mutex::new(Vec::new());

    let gate = crate::oauth::ensure_installable(
        &handle,
        &pn,
        |rt: &str, _scopes: Option<&str>| {
            sent.lock().unwrap().push(rt.to_string());
            Err(crate::oauth::RefreshError::Invalid {
                failure: crate::oauth::TokenFailure::Status(400),
                invalid_grant: true,
            })
        },
        None,
    );

    assert_eq!(
        (
            matches!(gate, crate::oauth::AuthGate::Ready),
            sent.into_inner().unwrap(),
            sidecar.exists(),
        ),
        (true, Vec::<String>::new(), false),
        "(the gate installs as is, the refreshes sent, a rolling sidecar written)"
    );
}

/// Plain OAuth account `name` on disk holding the chain `at-<name>` /
/// `rt-<name>` whose access token expires at `expires_at`, with
/// `rolling_token` as given; the config every process loaded before its
/// `config.toml` turned unclassifiable.
fn plain_chain_loaded_then_unclassifiable(
    name: &str,
    expires_at: i64,
    rolling_token: bool,
    shape: crate::claude::tests::Unclassifiable,
) -> crate::profile::ConfigHandle {
    let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
    profile.rolling_token = rolling_token;
    profile.auto_start = true;
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(expires_at),
            scopes: Some(vec![
                "user:inference".to_string(),
                "user:profile".to_string(),
            ]),
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    crate::profile::save_profile(&profile).unwrap();
    crate::testutil::register_names(&[name]);
    let loaded = crate::profile::load_profile(&profile.name).unwrap();
    assert!(
        loaded.spends_oauth_chain(),
        "fixture control: the loaded profile spends its chain"
    );
    let config = crate::profile::AppConfig {
        state: crate::profile::load_app_state().unwrap(),
        profiles: vec![loaded],
    };
    shape.pose(name);
    Arc::new(RankedMutex::new(config))
}

/// `name`'s store, stage and rolling sidecar, as bytes.
fn chain_files(name: &str) -> [Option<Vec<u8>>; 3] {
    [
        "credentials.json",
        "credentials.json.staged",
        "session-token.json",
    ]
    .map(|file| {
        std::fs::read(crate::profile::profile_subpath(&ProfileName::from(name), file).unwrap()).ok()
    })
}

/// The switch gate on a rolling account whose `config.toml` turned
/// unclassifiable after the config loaded (a directory in its place, a
/// `[proxy] # note` header that fails to parse) stamps no rolling sidecar
/// from its live chain and sends no refresh: it installs as is, its store,
/// stage and sidecar byte for byte.
#[test]
fn the_rolling_gate_stamps_nothing_on_an_account_whose_config_cannot_be_classified() {
    use crate::claude::tests::Unclassifiable;
    let _home = crate::testutil::HomeSandbox::new();
    let outcomes: Vec<_> = Unclassifiable::BOTH
        .iter()
        .enumerate()
        .map(|(i, &shape)| {
            let name = format!("roll-{i}");
            let handle = plain_chain_loaded_then_unclassifiable(
                &name,
                now_ms() as i64 + 3_600_000,
                true,
                shape,
            );
            let before = chain_files(&name);
            let sent = Mutex::new(Vec::new());
            let gate = crate::oauth::ensure_installable(
                &handle,
                &ProfileName::from(name.as_str()),
                |rt: &str, _scopes: Option<&str>| {
                    sent.lock().unwrap().push(rt.to_string());
                    Err(crate::oauth::RefreshError::Invalid {
                        failure: crate::oauth::TokenFailure::Status(400),
                        invalid_grant: true,
                    })
                },
                None,
            );
            (
                matches!(gate, crate::oauth::AuthGate::Ready),
                sent.into_inner().unwrap(),
                chain_files(&name) == before,
            )
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![(true, Vec::<String>::new(), true); 2],
        "(the gate installs as is, the refreshes sent, the store, stage and sidecar kept) for \
         a directory and a noted header"
    );
}

/// Pose `name`'s rolling sidecar as a mis-fill (a rotating pair) beside a live
/// preserved mint: the shape the rolling gate heals.
fn pose_misfill_beside_a_live_mint(name: &str) {
    let pn = ProfileName::from(name);
    let write = |file: &str, oauth: crate::profile::OAuthToken| {
        let creds = crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(oauth),
        };
        std::fs::write(
            crate::profile::profile_subpath(&pn, file).unwrap(),
            serde_json::to_vec(&creds).unwrap(),
        )
        .unwrap();
    };
    write(
        "session-token.json",
        crate::profile::OAuthToken {
            access_token: "at-misfill".to_string(),
            refresh_token: Some("rt-misfill".to_string()),
            expires_at: Some(now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        },
    );
    write(
        "session-token.static.json",
        crate::profile::OAuthToken {
            access_token: "sk-ant-oat01-mint".to_string(),
            refresh_token: None,
            expires_at: Some(now_ms() as i64 + 300 * 86_400_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        },
    );
    assert_eq!(
        crate::claude::session_token_status(&pn),
        Some(crate::claude::SessionTokenStatus::NotLongLived),
        "fixture control: {name}'s sidecar reads as a mis-fill"
    );
}

/// The switch gate on a rolling account whose `config.toml` holds a proxy
/// record, or turned unclassifiable after the config loaded, never heals a
/// mis-filled sidecar beside a live preserved mint: nothing under the
/// profile's dir moves, and the gate installs as is.
#[test]
fn the_rolling_gate_heals_no_sidecar_of_an_account_that_may_be_a_proxy_account() {
    use crate::claude::tests::Unclassifiable;
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_proxy_profile_on_disk(&mut config, "px", "http://127.0.0.1:9141");
    let pn = ProfileName::from("px");
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.rolling_token = true;
    crate::profile::save_profile(profile).unwrap();
    let recorded = Arc::new(RankedMutex::new(crate::profile::load_config().unwrap()));
    assert_eq!(
        crate::profile::stored_proxy_claim_failing_closed(&pn),
        crate::profile::ProxyClaim::Account(ProxyAccount {
            service: crate::proxy::Service::parse("stub").unwrap(),
            account: ACCOUNT.to_string(),
        }),
        "fixture control: px's config.toml holds a readable record"
    );
    let mut cases = vec![("px".to_string(), recorded)];
    for (i, &shape) in Unclassifiable::BOTH.iter().enumerate() {
        let name = format!("heal-{i}");
        let handle =
            plain_chain_loaded_then_unclassifiable(&name, now_ms() as i64 + 3_600_000, true, shape);
        cases.push((name, handle));
    }

    let outcomes: Vec<_> = cases
        .iter()
        .map(|(name, handle)| {
            pose_misfill_beside_a_live_mint(name);
            let before = profile_tree(name);
            let gate = crate::oauth::ensure_installable(
                handle,
                &ProfileName::from(name.as_str()),
                |_rt: &str, _scopes: Option<&str>| {
                    Err(crate::oauth::RefreshError::Invalid {
                        failure: crate::oauth::TokenFailure::Status(400),
                        invalid_grant: true,
                    })
                },
                None,
            );
            (
                name.clone(),
                matches!(gate, crate::oauth::AuthGate::Ready),
                profile_tree(name) == before,
            )
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![
            ("px".to_string(), true, true),
            ("heal-0".to_string(), true, true),
            ("heal-1".to_string(), true, true),
        ],
        "(account, the gate installs as is, its profile dir kept) for a readable record, a \
         directory and a noted header"
    );
}

/// The switch gate on a plain account whose access token is expiring and
/// whose `config.toml` turned unclassifiable after the config loaded sends no
/// refresh on its chain: it installs as is, its store and stage byte for
/// byte.
#[test]
fn the_switch_gate_spends_nothing_on_an_account_whose_config_cannot_be_classified() {
    use crate::claude::tests::Unclassifiable;
    let _home = crate::testutil::HomeSandbox::new();
    let outcomes: Vec<_> = Unclassifiable::BOTH
        .iter()
        .enumerate()
        .map(|(i, &shape)| {
            let name = format!("spend-{i}");
            let handle = plain_chain_loaded_then_unclassifiable(
                &name,
                now_ms() as i64 - 60_000,
                false,
                shape,
            );
            let before = chain_files(&name);
            let sent = Mutex::new(Vec::new());
            let gate = crate::oauth::ensure_installable(
                &handle,
                &ProfileName::from(name.as_str()),
                |rt: &str, _scopes: Option<&str>| {
                    sent.lock().unwrap().push(rt.to_string());
                    Err(crate::oauth::RefreshError::Invalid {
                        failure: crate::oauth::TokenFailure::Status(400),
                        invalid_grant: true,
                    })
                },
                None,
            );
            (
                matches!(gate, crate::oauth::AuthGate::Ready),
                sent.into_inner().unwrap(),
                chain_files(&name) == before,
            )
        })
        .collect();

    assert_eq!(
        outcomes,
        vec![(true, Vec::<String>::new(), true); 2],
        "(the gate installs as is, the refreshes sent, the store and stage kept) for a \
         directory and a noted header"
    );
}

/// The window kick on an opted-in account whose `config.toml` turned
/// unclassifiable after the config loaded sends nothing: no window opens and
/// its store and stage stay byte for byte.
#[test]
fn the_kick_sends_nothing_on_an_account_whose_config_cannot_be_classified() {
    use crate::claude::tests::Unclassifiable;
    let home = crate::testutil::HomeSandbox::new();
    // A kick per shape and the rotation a 401 would take, all recorded.
    let (base, server) = crate::testutil::serve_endpoints(4, |_, _| (401, "{}".to_string()));
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    let outcomes: Vec<_> = Unclassifiable::BOTH
        .iter()
        .enumerate()
        .map(|(i, &shape)| {
            let name = format!("kick-{i}");
            let expires_at = now_ms() as i64 + 3_600_000;
            let handle = plain_chain_loaded_then_unclassifiable(&name, expires_at, false, shape);
            let before = chain_files(&name);
            let kicked = crate::oauth::auto_start_kick(
                &handle,
                &ProfileName::from(name.as_str()),
                &format!("at-{name}"),
                Some(&format!("rt-{name}")),
                Some(expires_at),
                None,
                crate::oauth::LockWait::NoWait,
            );
            (kicked.opened, chain_files(&name) == before)
        })
        .collect();
    let seen = server.join().unwrap();

    assert_eq!(
        (outcomes, seen),
        (vec![(false, true); 2], Vec::<String>::new()),
        "((window opened, store and stage kept) per shape, the requests sent)"
    );
}

/// The auto-start queue never admits a proxy account holding a stored chain
/// with `auto_start` on, since no leg can kick it; a plain OAuth account
/// beside it is admitted.
#[test]
fn the_auto_start_queue_never_admits_a_proxy_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let pn = ProfileName::from("px");
    config
        .profiles
        .iter_mut()
        .find(|p| p.name == pn)
        .unwrap()
        .auto_start = true;
    let mut plain = crate::profile::Profile::new("oa".to_string(), None, None);
    plain.auto_start = true;
    plain.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at-oa".to_string(),
            refresh_token: Some("rt-oa".to_string()),
            expires_at: Some(now_ms() as i64 + 3_600_000),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    config.add(plain);
    config.state.auto_start_queue = true;

    assert_eq!(
        crate::usage::auto_start_queue_members(&config, &[], |_| false, |_| false),
        vec![ProfileName::from("oa")]
    );
}

/// `clauth static-token`, `clauth static-token --clear` and `clauth
/// rolling-token` on a profile whose `[proxy]` table clauth cannot read refuse
/// up front with the unreadable-table refusal at exit 2 and change nothing:
/// the `rolling_token` flag stays on, no sidecar or lock file appears, and no
/// request leaves.
#[test]
fn token_mode_commands_refuse_an_unreadable_proxy_record_and_change_nothing() {
    let home = crate::testutil::HomeSandbox::new();
    // A correct run sends nothing; a refresh or a roll would send at least one
    // request per command.
    let (base, server) = crate::testutil::serve_endpoints(3, |path, _| {
        if path.starts_with("/v1/oauth/token") {
            (400, r#"{"error": "invalid_grant"}"#.to_string())
        } else {
            (401, "{}".to_string())
        }
    });
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
    proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let pn = ProfileName::from("px");
    crate::profile::update_profile_config(&pn, |p| {
        p.rolling_token = true;
        Ok(())
    })
    .unwrap();
    let path = break_proxy_record("px");
    let loaded = crate::profile::load_profile(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.has_unreadable_proxy_record,
            loaded.rolling_token,
            loaded.refresh_token()
        ),
        (false, true, true, Some("rt-px")),
        "fixture control: (a readable proxy claim, an unreadable one, the rolling flag, the \
         stored chain)"
    );
    let before = profile_dir_files("px");
    let refusal = format!(
        "profile 'px' has a [proxy] table in {path:?} that clauth cannot read; nothing was \
         changed. Give the table a valid `service` and `account`, then log in again"
    );

    let outcomes = (
        cli_outcome(crate::cmd_static_token("px")),
        cli_outcome(crate::cmd_static_token_clear("px", true)),
        cli_outcome(crate::cmd_rolling_token("px")),
    );
    let seen = server.join().unwrap();

    assert_eq!(
        (outcomes, profile_dir_files("px") == before, seen),
        (
            (
                Err((refusal.clone(), 2)),
                Err((refusal.clone(), 2)),
                Err((refusal, 2)),
            ),
            true,
            Vec::<String>::new(),
        ),
        "((static-token, static-token --clear, rolling-token) as (text, exit)), px's files \
         kept, the requests sent"
    );
}

/// The rolling gate asks the rotation block (macOS's live-session refusal)
/// only before a refresh it would spend: a proxy account with `rolling_token`
/// on and an expired stored chain, with that block posed on this host,
/// installs as is, sends no refresh and stamps no sidecar.
#[test]
fn the_rotation_block_never_refuses_a_rolling_gate_on_a_chain_clauth_never_spends() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = proxy_account_with_an_expired_chain("px", Some(KEY), &[], false);
    let pn = ProfileName::from("px");
    let profile = config.profiles.iter_mut().find(|p| p.name == pn).unwrap();
    profile.rolling_token = true;
    crate::profile::save_profile(profile).unwrap();
    let config = crate::profile::load_config().unwrap();
    let loaded = config.find(&pn).unwrap();
    assert_eq!(
        (
            loaded.proxy.is_some(),
            loaded.rolling_token,
            loaded.refresh_token(),
            crate::usage::token_clock_expired(loaded.access_token_expires_at(), now_ms() as i64),
        ),
        (true, true, Some("rt-px"), true),
        "fixture control: (the runtime proxy claim, the rolling flag, the stored chain, an \
         expired access token)"
    );
    let sidecar = crate::profile::profile_subpath(&pn, "session-token.json").unwrap();
    let handle = Arc::new(RankedMutex::new(config));
    let sent = Mutex::new(Vec::new());

    crate::runtime::set_rotation_blocked_override(Some(true));
    let gate = crate::oauth::ensure_installable(
        &handle,
        &pn,
        |rt: &str, _scopes: Option<&str>| {
            sent.lock().unwrap().push(rt.to_string());
            Err(crate::oauth::RefreshError::Invalid {
                failure: crate::oauth::TokenFailure::Status(400),
                invalid_grant: true,
            })
        },
        None,
    );
    crate::runtime::set_rotation_blocked_override(None);

    assert_eq!(
        (
            matches!(gate, crate::oauth::AuthGate::Ready),
            sent.into_inner().unwrap(),
            sidecar.exists(),
        ),
        (true, Vec::<String>::new(), false),
        "(the gate installs as is, the refreshes sent, a rolling sidecar written)"
    );
}

/// `clauth start`'s arming leg stamps no session token from a proxy
/// account's stored chain, its `[proxy]` table readable or not, though the
/// account carries `rolling_token` and a comfortable chain whose grant
/// classifies as rolling. A plain account of the same shape is stamped.
#[test]
fn the_start_time_arming_never_stamps_from_a_proxy_accounts_chain() {
    let _home = crate::testutil::HomeSandbox::new();
    let comfortable = |name: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(format!("rt-{name}")),
            expires_at: Some(now_ms() as i64 + 10 * 3_600_000),
            scopes: Some(vec!["user:inference".into(), "user:profile".into()]),
            subscription_type: Some("max".into()),
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let mut plain = crate::profile::Profile::new("plain".to_string(), None, None);
    plain.rolling_token = true;
    plain.credentials = Some(comfortable("plain"));
    crate::profile::save_profile(&plain).unwrap();
    for name in ["px", "pu"] {
        let mut config = proxy_account_with_an_expired_chain(name, Some(KEY), &[], false);
        let profile = config
            .profiles
            .iter_mut()
            .find(|p| p.name.as_str() == name)
            .unwrap();
        profile.rolling_token = true;
        profile.credentials = Some(comfortable(name));
        crate::profile::save_profile(profile).unwrap();
    }
    break_proxy_record("pu");
    let shape = |name: &str| {
        let loaded = crate::profile::load_profile(&ProfileName::from(name)).unwrap();
        (
            loaded.proxy.is_some(),
            loaded.has_unreadable_proxy_record,
            loaded.rolling_token,
        )
    };
    assert_eq!(
        (shape("plain"), shape("px"), shape("pu")),
        (
            (false, false, true),
            (true, false, true),
            (false, true, true)
        ),
        "fixture control: (a readable proxy claim, an unreadable one, the rolling flag) for \
         plain, px, pu"
    );

    let stamped = |name: &str| {
        let pn = ProfileName::from(name);
        crate::claude::arm_rolling_from_disk(&pn);
        let sidecar = crate::profile::profile_subpath(&pn, "session-token.json").unwrap();
        std::fs::read_to_string(sidecar)
            .ok()
            .map(|s| s.contains(&format!("at-{name}")))
    };

    assert_eq!(
        (stamped("plain"), stamped("px"), stamped("pu")),
        (Some(true), None, None),
        "the sidecar each arm left, as holding its chain's access token: plain, px, pu"
    );
}

/// The refusal of a switch away from `work`, whose `config.toml` cannot be
/// classified, while Claude Code holds a login clauth has not saved.
const UNCLASSIFIABLE_SWITCH: &str = "can't read the config of 'work', so clauth can't tell whether \
                                     Claude Code's current login belongs in it; fix its \
                                     config.toml, then switch again";

/// The not-saved warning for `work`, whose `config.toml` cannot be classified.
const UNCLASSIFIABLE_NOT_SAVED: &str = "Claude Code signed in while 'work' was active, but clauth \
                                        can't read its config, so it did not save that login into \
                                        'work'. Fix its config.toml, or run `clauth login <name>` \
                                        with a new name to keep it";

/// The not-saved warning for `work` while it reads as a proxy account.
const PROXY_NOT_SAVED: &str = "Claude Code signed in while proxy account 'work' was active; \
                               clauth did not save that login into 'work'. To keep it as an \
                               account, run `clauth login <name>` with a new name";

/// What Claude Code holds in `~/.claude` as a switch leaves `work`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkLogin {
    /// A login of its own beside work's stored one: diverged and unsaved.
    Diverged,
    /// A login on work, which stores none: a first login the adopt skips.
    First,
    /// work's stored login through clauth's link: saved.
    Saved,
}

/// The runs of a switch away from `work`: an unsaved login over each shape,
/// then a saved one over the noted header. A saved login over a directory
/// never reaches the refusal: the switch's own snapshot fails reading
/// `config.toml` first.
const SWITCH_RUNS: [(crate::claude::tests::Unclassifiable, WorkLogin); 5] = {
    use crate::claude::tests::Unclassifiable::{Directory, NotedProxyHeader};
    [
        (Directory, WorkLogin::Diverged),
        (Directory, WorkLogin::First),
        (NotedProxyHeader, WorkLogin::Diverged),
        (NotedProxyHeader, WorkLogin::First),
        (NotedProxyHeader, WorkLogin::Saved),
    ]
};

/// [`work_loaded`], then work's `config.toml` turned `shape`.
fn work_loaded_then_unclassifiable(
    shape: crate::claude::tests::Unclassifiable,
    login: WorkLogin,
) -> crate::profile::AppConfig {
    let config = work_loaded(login);
    shape.pose("work");
    config
}

/// Plain account `work`, active, beside `other` (each storing a login but a
/// [`WorkLogin::First`] work), both on disk; `login` sits in `~/.claude`; the
/// config every process loaded.
fn work_loaded(login: WorkLogin) -> crate::profile::AppConfig {
    work_loaded_beside(login, None)
}

/// [`work_loaded`] with other's access token expiring at `other_expires_at`.
fn work_loaded_beside(
    login: WorkLogin,
    other_expires_at: Option<i64>,
) -> crate::profile::AppConfig {
    let work = ProfileName::from("work");
    let mut profile = crate::profile::Profile::new("work".to_string(), None, None);
    if login != WorkLogin::First {
        profile.credentials = Some(oauth_login_of("at-work", "rt-work"));
    }
    crate::profile::save_profile(&profile).unwrap();
    let mut other = crate::profile::Profile::new("other".to_string(), None, None);
    let mut other_login = oauth_login_of("at-other", "rt-other");
    if let Some(oauth) = other_login.claude_ai_oauth.as_mut() {
        oauth.expires_at = other_expires_at;
    }
    other.credentials = Some(other_login);
    crate::profile::save_profile(&other).unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        active_profile: Some(work.clone()),
        profiles: vec![work.clone(), ProfileName::from("other")],
        ..crate::profile::AppState::default()
    })
    .unwrap();
    match login {
        WorkLogin::Saved => crate::claude::force_link_profile_credentials(&work).unwrap(),
        WorkLogin::Diverged | WorkLogin::First => {
            write_live_login("at-cc-work", "rt-cc-work");
        }
    }
    crate::profile::load_config().unwrap()
}

/// The bytes `~/.claude/.credentials.json` resolves to.
fn live_login_bytes() -> Option<Vec<u8>> {
    std::fs::read(crate::claude::claude_credentials_path().unwrap()).ok()
}

/// The account disk names active.
fn disk_active() -> Option<String> {
    crate::profile::load_app_state()
        .unwrap()
        .active_profile
        .map(|name| name.to_string())
}

/// `clauth switch other` away from `work`, whose `config.toml` turned
/// unclassifiable after the command loaded every profile (a directory in its
/// place, a `[proxy] # note` header that fails to parse), refuses while Claude
/// Code holds a login clauth has not saved (diverged from work's store, or a
/// first login): the whole line on stderr, exit 1, work's files, the live
/// login and the active account as they were. Over work's own saved login
/// the switch goes ahead.
#[test]
fn a_cli_switch_away_from_an_unclassifiable_account_refuses_over_an_unsaved_login() {
    let mut outcomes = Vec::new();
    for (shape, login) in SWITCH_RUNS {
        let _home = crate::testutil::HomeSandbox::new();
        let config = work_loaded_then_unclassifiable(shape, login);
        let tree = profile_tree("work");
        let live = live_login_bytes();
        let out = crate::out::OutCapture::new();
        let capture = out.capture_here();
        let mut stderr = Vec::new();
        let code = crate::exit_code_to(
            crate::actions::switch_profile_cli(
                config,
                &ProfileName::from("other"),
                std::io::empty(),
            ),
            &mut stderr,
        );
        drop(capture);
        outcomes.push((
            String::from_utf8(stderr).unwrap(),
            code,
            String::from_utf8(out.snapshot()).unwrap(),
            profile_tree("work") == tree,
            live_login_bytes() == live,
            disk_active(),
        ));
    }

    let refused = (
        format!("Error: {UNCLASSIFIABLE_SWITCH}\n"),
        1,
        String::new(),
        true,
        true,
        Some("work".to_string()),
    );
    let switched = (
        String::new(),
        0,
        "clauth: switched to 'other'\n".to_string(),
        true,
        false,
        Some("other".to_string()),
    );
    assert_eq!(
        outcomes,
        vec![
            refused.clone(),
            refused.clone(),
            refused.clone(),
            refused,
            switched
        ],
        "(stderr, exit code, stdout, work's files kept, the live login kept, disk's active) over \
         {SWITCH_RUNS:?}"
    );
}

/// The headless switch (`clauth mcp`'s `switch_profile`, whose `reason` is
/// this Display; the REST `switch`, whose body reflects it) away from `work`,
/// whose `config.toml` turned unclassifiable after the config loaded,
/// refuses with the same sentence under every divergence default while
/// Claude Code holds a login clauth has not saved, before the gate or any
/// write: work's files, the live login and the active account as they were.
/// Over work's own saved login it switches.
#[test]
fn a_headless_switch_away_from_an_unclassifiable_account_refuses_over_an_unsaved_login() {
    use crate::profile::DivergenceChoice;
    let mut outcomes = Vec::new();
    for (shape, login) in SWITCH_RUNS {
        for choice in [
            None,
            Some(DivergenceChoice::Overwrite),
            Some(DivergenceChoice::Discard),
        ] {
            let _home = crate::testutil::HomeSandbox::new();
            let config = work_loaded_then_unclassifiable(shape, login);
            let tree = profile_tree("work");
            let live = live_login_bytes();
            let refreshes = std::cell::Cell::new(0);
            let handle = Arc::new(crate::lockorder::RankedMutex::new(config));
            let switched = crate::actions::switch_profile_noninteractive(
                &handle,
                &ProfileName::from("other"),
                choice,
                |_: &str, _: Option<&str>| {
                    refreshes.set(refreshes.get() + 1);
                    Err(crate::oauth::RefreshError::Transient(
                        crate::oauth::TokenFailure::Transport,
                    ))
                },
                None,
            )
            .map(|(previous, active, _)| (previous, active))
            .map_err(|e| (e.to_string(), e.deep_refusal()));
            outcomes.push((
                switched,
                refreshes.get(),
                profile_tree("work") == tree,
                live_login_bytes() == live,
                disk_active(),
            ));
        }
    }

    let refused = (
        Err((
            UNCLASSIFIABLE_SWITCH.to_string(),
            Some(UNCLASSIFIABLE_SWITCH.to_string()),
        )),
        0,
        true,
        true,
        Some("work".to_string()),
    );
    let switched = (
        Ok((Some("work".to_string()), "other".to_string())),
        0,
        true,
        false,
        Some("other".to_string()),
    );
    let expected: Vec<_> = std::iter::repeat_n(refused, 12)
        .chain(std::iter::repeat_n(switched, 3))
        .collect();
    assert_eq!(
        outcomes, expected,
        "(the switch's reply or its reason and reflected refusal, refreshes sent, work's files \
         kept, the live login kept, disk's active) with no default, overwrite and discard, over \
         {SWITCH_RUNS:?}"
    );
}

/// [`work_loaded`], then work's `config.toml`, still readable, mentioning
/// `proxy` in a comment, a table name and a string value.
fn work_loaded_mentioning_proxy(login: WorkLogin) -> crate::profile::AppConfig {
    let config = work_loaded(login);
    let work = ProfileName::from("work");
    let path = crate::profile::profile_subpath(&work, "config.toml").unwrap();
    let text = format!(
        "{}# proxy note\n[proxyfoo]\nnote = \"proxy\"\n",
        std::fs::read_to_string(&path).unwrap()
    );
    assert!(
        text.parse::<toml::Table>().is_ok(),
        "fixture control: work's config.toml parses"
    );
    std::fs::write(&path, text).unwrap();
    assert_eq!(
        crate::profile::stored_proxy_claim_failing_closed(&work),
        crate::profile::ProxyClaim::None,
        "fixture control: work's config.toml claims no proxy account"
    );
    config
}

/// The refresh token work's store holds.
fn work_store_refresh() -> Option<String> {
    let path =
        crate::profile::profile_subpath(&ProfileName::from("work"), "credentials.json").unwrap();
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<crate::profile::ClaudeCredentials>(&bytes)
        .ok()?
        .claude_ai_oauth?
        .refresh_token
}

/// A plain account whose `config.toml` reads (and mentions `proxy` only
/// outside any key) switches away over a login clauth has not saved as it
/// always did: `clauth switch` asks and, on a closed stdin's default, saves a
/// diverged login into work before switching, and adopts a first login; the
/// headless switch refuses a diverged login under no default, saves it under
/// overwrite and drops it under discard, and adopts a first login under each;
/// the shared delete deletes work.
#[test]
fn a_plain_readable_account_switches_and_deletes_over_an_unsaved_login() {
    use crate::profile::DivergenceChoice;
    let logins = [WorkLogin::Diverged, WorkLogin::First];
    let mut cli = Vec::new();
    let mut headless = Vec::new();
    let mut deletes = Vec::new();
    for login in logins {
        {
            let _home = crate::testutil::HomeSandbox::new();
            let config = work_loaded_mentioning_proxy(login);
            let out = crate::out::OutCapture::new();
            let capture = out.capture_here();
            let mut stderr = Vec::new();
            let code = crate::exit_code_to(
                crate::actions::switch_profile_cli(
                    config,
                    &ProfileName::from("other"),
                    std::io::empty(),
                ),
                &mut stderr,
            );
            drop(capture);
            cli.push((
                String::from_utf8(stderr).unwrap(),
                code,
                String::from_utf8(out.snapshot()).unwrap(),
                work_store_refresh(),
                disk_active(),
            ));
        }
        for choice in [
            None,
            Some(DivergenceChoice::Overwrite),
            Some(DivergenceChoice::Discard),
        ] {
            let _home = crate::testutil::HomeSandbox::new();
            let handle = Arc::new(crate::lockorder::RankedMutex::new(
                work_loaded_mentioning_proxy(login),
            ));
            let switched = crate::actions::switch_profile_noninteractive(
                &handle,
                &ProfileName::from("other"),
                choice,
                |_: &str, _: Option<&str>| {
                    Err(crate::oauth::RefreshError::Transient(
                        crate::oauth::TokenFailure::Transport,
                    ))
                },
                None,
            )
            .map(|(previous, active, _)| (previous, active))
            .map_err(|e| e.to_string());
            headless.push((switched, work_store_refresh(), disk_active()));
        }
        {
            let _home = crate::testutil::HomeSandbox::new();
            let mut config = work_loaded_mentioning_proxy(login);
            let work = ProfileName::from("work");
            let rotation = crate::actions::rotation_guard_for_mutation(&work).unwrap();
            let deleted = crate::actions::delete_profile(&mut config, &work, false, &rotation)
                .map_err(|e| format!("{e:#}"));
            drop(rotation);
            deletes.push((
                deleted,
                crate::profile::profile_dir(&work).unwrap().exists(),
                config.find(&work).is_some(),
            ));
        }
    }

    let switched = |store: &str| {
        (
            Ok((Some("work".to_string()), "other".to_string())),
            Some(store.to_string()),
            Some("other".to_string()),
        )
    };
    assert_eq!(
        (cli, headless, deletes),
        (
            vec![
                (
                    String::new(),
                    0,
                    "clauth: 'work' has a newer login in ~/.claude. save it into 'work' and \
                     switch to 'other'? [Y/n] clauth: switched to 'other'\n"
                        .to_string(),
                    Some("rt-cc-work".to_string()),
                    Some("other".to_string()),
                ),
                (
                    String::new(),
                    0,
                    "clauth: switched to 'other'\n".to_string(),
                    Some("rt-cc-work".to_string()),
                    Some("other".to_string()),
                ),
            ],
            vec![
                (
                    Err(
                        "'work' has a login clauth hasn't saved, resolve the divergence in the \
                         `clauth` TUI"
                            .to_string()
                    ),
                    Some("rt-work".to_string()),
                    Some("work".to_string()),
                ),
                switched("rt-cc-work"),
                switched("rt-work"),
                switched("rt-cc-work"),
                switched("rt-cc-work"),
                switched("rt-cc-work"),
            ],
            vec![(Ok(()), false, false); 2],
        ),
        "per login (diverged, first): (`clauth switch`'s stderr, exit code, stdout, work's \
         stored refresh token, disk's active); (the headless switch's reply, work's stored \
         refresh token, disk's active) with no default, overwrite and discard; (the shared \
         delete, work's dir kept, work kept in the config)"
    );
}

/// Whether a listener that answers nothing has no connection queued: no
/// request was sent to it.
fn nothing_reached(listener: &std::net::TcpListener) -> bool {
    matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

/// A switch away from `work`, whose `config.toml` cannot be classified, over
/// a login clauth has not saved refuses before the AUTH-1 gate could refresh
/// the target's expired login: `clauth switch`'s real refresh, aimed at a
/// listener that answers nothing, sends nothing, and the headless switch
/// never calls its refresher, under every divergence default.
#[test]
fn a_switch_away_from_an_unclassifiable_account_refuses_before_the_gate() {
    use crate::profile::DivergenceChoice;
    let mut outcomes = Vec::new();
    for (shape, login) in &SWITCH_RUNS[..4] {
        let (shape, login) = (*shape, *login);
        let home = crate::testutil::HomeSandbox::new();
        let (listener, base) = crate::testutil::deaf_listener();
        let endpoints = crate::testutil::EndpointSandbox::new(&home, &base);
        let config = work_loaded_beside(login, Some(1));
        shape.pose("work");
        let mut stderr = Vec::new();
        crate::exit_code_to(
            crate::actions::switch_profile_cli(
                config,
                &ProfileName::from("other"),
                std::io::empty(),
            ),
            &mut stderr,
        );
        outcomes.push((
            "cli".to_string(),
            String::from_utf8(stderr).unwrap(),
            nothing_reached(&listener),
        ));
        drop(endpoints);
        drop(home);

        for choice in [
            None,
            Some(DivergenceChoice::Overwrite),
            Some(DivergenceChoice::Discard),
        ] {
            let _home = crate::testutil::HomeSandbox::new();
            let handle = Arc::new(crate::lockorder::RankedMutex::new(work_loaded_beside(
                login,
                Some(1),
            )));
            shape.pose("work");
            let refreshes = std::cell::Cell::new(0);
            let switched = crate::actions::switch_profile_noninteractive(
                &handle,
                &ProfileName::from("other"),
                choice,
                |_: &str, _: Option<&str>| {
                    refreshes.set(refreshes.get() + 1);
                    Err(crate::oauth::RefreshError::Transient(
                        crate::oauth::TokenFailure::Transport,
                    ))
                },
                None,
            )
            .map(|_| ())
            .map_err(|e| e.to_string());
            outcomes.push((
                format!("{choice:?}"),
                format!("{switched:?}"),
                refreshes.get() == 0,
            ));
        }
    }

    let refused = Err::<(), _>(UNCLASSIFIABLE_SWITCH.to_string());
    let per_run = [
        (
            "cli".to_string(),
            format!("Error: {UNCLASSIFIABLE_SWITCH}\n"),
            true,
        ),
        ("None".to_string(), format!("{refused:?}"), true),
        ("Some(Overwrite)".to_string(), format!("{refused:?}"), true),
        ("Some(Discard)".to_string(), format!("{refused:?}"), true),
    ];
    assert_eq!(
        outcomes,
        per_run.iter().cycle().take(16).cloned().collect::<Vec<_>>(),
        "(the surface, its refusal, nothing sent) over {:?}",
        &SWITCH_RUNS[..4]
    );
}

/// The headless switch away from `work` whose `config.toml` turns
/// unclassifiable after its pre-check and gate, before its hold, refuses
/// under the hold of whichever switch the divergence default dispatches (the
/// plain one for a first login, the reconcile for overwrite, the discard for
/// discard, none for no default) with the same sentence, changing nothing.
#[test]
fn a_config_turning_unclassifiable_after_the_pre_check_refuses_under_the_hold() {
    use crate::profile::DivergenceChoice;
    let mut outcomes = Vec::new();
    for (shape, login) in &SWITCH_RUNS[..4] {
        let (shape, login) = (*shape, *login);
        for choice in [
            None,
            Some(DivergenceChoice::Overwrite),
            Some(DivergenceChoice::Discard),
        ] {
            let _home = crate::testutil::HomeSandbox::new();
            let handle = Arc::new(crate::lockorder::RankedMutex::new(work_loaded(login)));
            let live = live_login_bytes();
            crate::testutil::switch_race_seam::plant(move || shape.pose("work"));
            let switched = crate::actions::switch_profile_noninteractive(
                &handle,
                &ProfileName::from("other"),
                choice,
                |_: &str, _: Option<&str>| {
                    Err(crate::oauth::RefreshError::Transient(
                        crate::oauth::TokenFailure::Transport,
                    ))
                },
                None,
            )
            .map(|(previous, active, _)| (previous, active))
            .map_err(|e| e.to_string());
            crate::testutil::switch_race_seam::assert_empty();
            outcomes.push((switched, live_login_bytes() == live, disk_active()));
        }
    }

    assert_eq!(
        outcomes,
        vec![
            (
                Err(UNCLASSIFIABLE_SWITCH.to_string()),
                true,
                Some("work".to_string())
            );
            12
        ],
        "(the switch's reply or its reason, the live login kept, disk's active) with no default, \
         overwrite and discard, over {:?}",
        &SWITCH_RUNS[..4]
    );
}

/// The silent adopt (every switch's and switch-off's snapshot sink, the
/// daemon's drains among them) of a first login on `work`, whose `config.toml`
/// turned unclassifiable, writes nothing and logs the warning that says what
/// is true, in each process role; a `[proxy]` table that does not read keeps
/// the proxy account's line.
#[test]
fn the_silent_adopt_says_clauth_cannot_read_an_unclassifiable_config() {
    use crate::claude::tests::Unclassifiable;
    let mut outcomes = Vec::new();
    // Each run's logins are its own, so the once-per-login memo another run
    // or test left never silences one.
    for (run, shape) in [
        Some(Unclassifiable::Directory),
        Some(Unclassifiable::NotedProxyHeader),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let _home = crate::testutil::HomeSandbox::new();
        let mut config =
            work_loaded_then_unclassifiable(Unclassifiable::Directory, WorkLogin::First);
        write_live_login(
            &format!("at-r29m-adopt-{run}-1"),
            &format!("rt-r29m-adopt-{run}-1"),
        );
        let config_path =
            crate::profile::profile_subpath(&ProfileName::from("work"), "config.toml").unwrap();
        std::fs::remove_dir(&config_path).unwrap();
        match shape {
            Some(shape) => shape.pose("work"),
            None => {
                std::fs::write(
                    &config_path,
                    "[proxy]\nservice = \"stub\"\naccount = \"\"\n",
                )
                .unwrap();
                assert_eq!(
                    crate::profile::stored_proxy_claim_failing_closed(&ProfileName::from("work")),
                    crate::profile::ProxyClaim::Unreadable,
                    "fixture control: a table that does not read"
                );
            }
        }
        let tree = profile_tree("work");
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();
        {
            let _daemon = crate::logline::StampOverride::new(true);
            crate::claude::snapshot_active_credentials(&mut config).unwrap();
        }
        write_live_login(
            &format!("at-r29m-adopt-{run}-2"),
            &format!("rt-r29m-adopt-{run}-2"),
        );
        let live = live_login_bytes();
        {
            let _cli = crate::logline::StampOverride::new(false);
            crate::claude::snapshot_active_credentials(&mut config).unwrap();
        }
        drop(capture);
        outcomes.push((
            lines.snapshot(),
            profile_tree("work") == tree,
            live_login_bytes() == live,
        ));
    }

    let said = |sentence: &str| {
        (
            vec![
                format!("clauth daemon: {sentence}"),
                format!("clauth: {sentence}"),
            ],
            true,
            true,
        )
    };
    assert_eq!(
        outcomes,
        vec![
            said(UNCLASSIFIABLE_NOT_SAVED),
            said(UNCLASSIFIABLE_NOT_SAVED),
            said(PROXY_NOT_SAVED),
        ],
        "(the lines as the daemon then the CLI, work's files kept, the live login kept) for a \
         directory, a noted header and a table that does not read"
    );
}

/// `clauth limit-reset` on a proxy account refuses before any request, in
/// `--list` and spend alike, with a stored chain or without one: the chain is
/// never used, and the plain account's `clauth login` advice would itself be
/// refused. A `[proxy]` table that does not read refuses the same way.
#[test]
fn limit_reset_on_a_proxy_account_refuses_before_any_request() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    proxy_account_with_a_chain(&mut config, "px", true);
    create_proxy_profile_on_disk(&mut config, "pk", "http://127.0.0.1:9141");
    proxy_account_with_a_chain(&mut config, "pb", true);
    let broken = break_proxy_record("pb");
    let (listener, base) = crate::testutil::deaf_listener();
    let urls = crate::usage::codex_reset::ResetUrls::under(&base);
    let refuse = |name: &str, list: bool, yes: bool| {
        let err = crate::limit_reset_with(name, list, yes, false, &urls, |prompt| {
            panic!("this path must not prompt, asked: {prompt}")
        })
        .expect_err("refused");
        (
            err.downcast_ref::<crate::UsageError>().is_some(),
            err.to_string(),
        )
    };
    let on_proxy = |name: &str| {
        format!(
            "'{name}' is an account on proxy \"stub\": it signs in through its proxy and has no \
             Claude Code login to use a reset with"
        )
    };

    assert_eq!(
        (
            refuse("px", true, false),
            refuse("px", false, true),
            refuse("pk", false, true),
            refuse("pb", false, true),
        ),
        (
            (true, on_proxy("px")),
            (true, on_proxy("px")),
            (true, on_proxy("pk")),
            (
                true,
                format!(
                    "profile 'pb' has a [proxy] table in {broken:?} that clauth cannot read; \
                     nothing was changed. Give the table a valid `service` and `account`, then \
                     log in again"
                )
            ),
        ),
        "(a chain listed, a chain spent, a key alone, a broken table): (usage error, sentence)"
    );
    crate::testutil::assert_nothing_sent(&listener);
}

/// The refusal of a new login the proxy bound to an account `px` holds.
const LINE_HELD: &str = "proxy \"stub\" signed 'py' in as account \"acct-1\", which 'px' already \
                         holds: 'px' is logged in again, and clauth saved no 'py'";

/// A bare login of an upstream user the proxy already holds re-binds that
/// account and answers `done` with no key, on the contract's own terms, and a
/// proxy not built on the core may still hand a key over: either way the
/// account is `px`'s, so clauth mints nothing, deletes nothing and saves nothing.
#[test]
fn a_new_login_of_a_held_account_refuses_and_leaves_its_holder_alone() {
    for answered in [
        done_body(ACCOUNT, None),
        done_body(ACCOUNT, Some("clp_new")),
    ] {
        let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
        stub.state().flow_states = vec![answered];
        let (px_key, px_record) = (
            stored_key("px"),
            crate::profile::stored_proxy_record(&ProfileName::from("px")),
        );

        let err = crate::login_proxy(&proxy_login("py", None), "stub").expect_err("refused");

        assert_eq!(err.to_string(), LINE_HELD);
        let roster = crate::profile::load_app_state().unwrap().profiles;
        let (minted, deleted) = {
            let state = stub.state();
            (state.minted.clone(), state.deleted.clone())
        };
        assert_eq!(
            (
                minted,
                deleted,
                roster,
                stored_key("px"),
                crate::profile::stored_proxy_record(&ProfileName::from("px")),
                crate::profile::profile_dir(&ProfileName::from("py"))
                    .unwrap()
                    .exists(),
            ),
            (
                Vec::<String>::new(),
                Vec::<String>::new(),
                vec![ProfileName::from("px")],
                px_key,
                px_record,
                false,
            ),
            "(minted, deleted, the roster, px's key, px's record, a py dir)"
        );
    }
}

/// A signal on that login, once the flow reads `done`, ends on the line that
/// names the holder as re-bound, and deletes nothing.
#[test]
fn a_signal_on_a_new_login_of_a_held_account_leaves_it_bound() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    stub.state().flow_states = vec![
        json!({"flow": FLOW, "state": "pending"}),
        done_body(ACCOUNT, None),
    ];
    let doors = crate::ProxyLoginDoors::capture_here();
    let signal = signal_after_the_next_poll(&stub, doors.interrupt(), 2);

    let err = crate::login_proxy(&proxy_login("py", None), "stub").expect_err("the signal ends it");
    signal.join().unwrap();

    assert_eq!(err.to_string(), LINE_REBOUND);
    let state = stub.state();
    assert_eq!(
        (state.minted.clone(), state.deleted.clone()),
        (Vec::<String>::new(), Vec::<String>::new()),
        "(minted, deleted)"
    );
}

/// The dead-key fix line names `clauth login px --proxy stub`, so that login
/// mints over a key carrying the verdict and clears it, while a verdict on an
/// older key leaves the stored key alone.
#[test]
fn a_relogin_over_a_key_with_a_dead_key_verdict_mints_and_clears_it() {
    let (_home, stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    let profile = crate::profile::load_profile(&name).unwrap();
    crate::profile_cache::write_dead_key(
        &name,
        crate::usage::profile_key_fingerprint(&profile).unwrap() ^ 1,
    );
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");
    assert_eq!(
        (stub.state().minted.clone(), stored_key("px").as_deref()),
        (Vec::<String>::new(), Some(KEY)),
        "a verdict on another key mints nothing"
    );

    crate::profile_cache::write_dead_key(
        &name,
        crate::usage::profile_key_fingerprint(&profile).unwrap(),
    );
    assert!(crate::fallback::third_party_key_rejected(&profile, &name));
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    let after = crate::profile::load_profile(&name).unwrap();
    assert_eq!(
        (
            stub.state().minted.clone(),
            after.api_key.as_deref() == Some(KEY),
            crate::fallback::third_party_key_rejected(&after, &name),
            crate::oauth::third_party_dead_chain_copy(Some(&after), &name),
        ),
        (
            vec![ACCOUNT.to_string()],
            false,
            false,
            Some(
                "stored OAuth chain is dead, its api key still works: px \
                 (run `clauth login px --proxy stub` to clear the quarantine)"
                    .to_string()
            )
        ),
        "(minted, the old key kept, the verdict, the dead-chain copy)"
    );
    // A new key alone leaves the verdict inert, so the record itself is read:
    // nothing else clears it on a proxy account.
    assert!(
        !crate::profile_cache::dead_key_matches(
            &name,
            crate::usage::profile_key_fingerprint(&profile).unwrap()
        ),
        "the verdict on the old key is gone"
    );
}

/// An `[env]` inference key is the stored key a re-login keeps, like the api
/// key slot, and the one a dead-key verdict names: without a verdict nothing is
/// minted, with one the login mints, as it does over the slot.
#[test]
fn a_relogin_treats_an_env_inference_key_as_the_stored_key() {
    let (_home, admin_token) = (crate::testutil::HomeSandbox::new(), mint_admin_token());
    let stub = Stub::start(admin_token, usage_body(500_000.0, 100.0));
    register_proxy_row(stub.port);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    let mut profile = crate::profile::Profile::new("px".to_string(), Some(stub.base.clone()), None);
    profile
        .env
        .insert("ANTHROPIC_API_KEY".to_string(), KEY.to_string());
    crate::profile::save_profile(&profile).expect("save profile");
    let record = ProxyAccount {
        service: crate::proxy::Service::parse("stub").unwrap(),
        account: ACCOUNT.to_string(),
    };
    crate::profile::write_proxy_record(&profile.name, &record).expect("write the record");
    let name = profile.name.clone();
    config.add(profile);
    crate::profile::save_app_state(&config.state).expect("save state");
    let loaded = crate::profile::load_profile(&name).unwrap();
    let fingerprint = crate::usage::profile_key_fingerprint(&loaded).expect("the env key");

    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");
    assert_eq!(
        (stub.state().minted.clone(), stored_key("px")),
        (Vec::<String>::new(), None),
        "(minted, the slot) with no verdict"
    );

    crate::profile_cache::write_dead_key(&name, fingerprint);
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");
    assert_eq!(
        (
            stub.state().minted.clone(),
            crate::profile_cache::dead_key_matches(&name, fingerprint),
        ),
        (vec![ACCOUNT.to_string()], false),
        "(minted, the verdict) with one"
    );
}

/// A claim landing after the read at `done` meets the re-check under the
/// flock, whichever profile lands it: another name, or the login's own name
/// taken meanwhile, whose refusal would otherwise reach the failed save's
/// delete of an account that profile now holds.
#[test]
fn a_claim_landing_before_a_new_logins_save_refuses_and_deletes_nothing() {
    const LANDED: &str = "acct-2";
    let cases = [
        ("px", vec![ProfileName::from("px")], false),
        (
            "py",
            vec![ProfileName::from("px"), ProfileName::from("py")],
            true,
        ),
    ];
    for (claimant, roster, py_dir) in cases {
        let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
        stub.state().flow_states = vec![done_body(LANDED, Some("clp_new"))];
        let base = stub.base.clone();
        let doors = crate::ProxyLoginDoors::capture_here();
        doors.before_save(move || {
            if claimant == "py" {
                create_proxy_profile_on_disk(&mut config, "py", &base);
            }
            let record = ProxyAccount {
                service: crate::proxy::Service::parse("stub").unwrap(),
                account: LANDED.to_string(),
            };
            crate::profile::write_proxy_record(&ProfileName::from(claimant), &record).unwrap();
        });

        let err = crate::login_proxy(&proxy_login("py", None), "stub").expect_err("refused");

        assert_eq!(
            err.to_string(),
            format!(
                "proxy \"stub\" signed 'py' in as account \"acct-2\", which '{claimant}' already \
                 holds: '{claimant}' is logged in again, and clauth saved no 'py'"
            )
        );
        assert_eq!(
            (
                stub.state().deleted.clone(),
                crate::profile::load_app_state().unwrap().profiles,
                crate::profile::profile_dir(&ProfileName::from("py"))
                    .unwrap()
                    .exists(),
            ),
            (Vec::<String>::new(), roster, py_dir),
            "(deleted, the roster, a py dir)"
        );
    }
}

/// What a session's settings send: (`ANTHROPIC_API_KEY`,
/// `ANTHROPIC_AUTH_TOKEN`, whether `apiKeyHelper` is wired). Claude Code sends
/// an env key and the helper's key in different headers, and the core answers
/// 401 when the two differ (measured 2026-11-07).
fn session_auth(settings: &str) -> (Option<String>, Option<String>, bool) {
    let settings: Value = serde_json::from_str(settings).expect("settings parse");
    let env_value = |key: &str| settings["env"][key].as_str().map(str::to_string);
    (
        env_value("ANTHROPIC_API_KEY"),
        env_value("ANTHROPIC_AUTH_TOKEN"),
        settings.get("apiKeyHelper").is_some(),
    )
}

/// A proxy account's slot key is the one key a session sends: an `[env]`
/// inference entry beside it stays out of the settings. An env-only account
/// and a non-proxy account keep today's "profile env last".
#[test]
fn a_proxy_accounts_slot_key_outranks_an_env_inference_key_in_the_session() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let keyed = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert!(
        keyed.proxy.is_some() && keyed.api_key.is_some(),
        "the fixture is a claimed account holding a slot key"
    );
    let mut keyless = keyed.clone();
    keyless.api_key = None;
    let mut unclaimed = keyed.clone();
    unclaimed.proxy = None;
    let session = |base: &crate::profile::Profile, entries: &[&str]| {
        let mut profile = base.clone();
        for entry in entries {
            profile
                .env
                .insert((*entry).to_string(), "env-key".to_string());
        }
        session_auth(&crate::claude::build_claude_settings_json(None, &profile, &[]).unwrap())
    };
    let env_key = || Some("env-key".to_string());

    assert_eq!(
        [
            session(&keyless, &["ANTHROPIC_API_KEY"]),
            session(&keyless, &["ANTHROPIC_AUTH_TOKEN"]),
        ],
        [(env_key(), None, false), (None, env_key(), false)],
        "an env-only proxy account keeps its entry"
    );
    assert_eq!(
        [
            session(&unclaimed, &["ANTHROPIC_API_KEY"]),
            session(&unclaimed, &["ANTHROPIC_AUTH_TOKEN"]),
        ],
        [(env_key(), None, true), (None, env_key(), true)],
        "a non-proxy account keeps profile env last"
    );
    assert_eq!(
        [
            session(&keyed, &["ANTHROPIC_API_KEY"]),
            session(&keyed, &["ANTHROPIC_AUTH_TOKEN"]),
            session(&keyed, &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]),
        ],
        [(None, None, true), (None, None, true), (None, None, true)],
        "a slot key: each entry alone, then both"
    );
}

/// The dead-key cure on the shape that used to 401: the entry equals the slot
/// key, an older apply wrote it into the live settings, and a verdict makes
/// the re-login mint. The active session then sends the new key alone, and
/// the hand-set entry stays in `config.toml`.
#[test]
fn a_minted_relogin_sends_the_new_key_alone_and_keeps_the_env_entry() {
    let (_home, stub, mut config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let name = ProfileName::from("px");
    config.state.active_profile = Some(name.clone());
    crate::profile::save_app_state(&config.state).expect("save state");
    crate::profile::update_profile_config(&name, |profile| {
        profile
            .env
            .insert("ANTHROPIC_API_KEY".to_string(), KEY.to_string());
        Ok(())
    })
    .expect("save the entry")
    .expect("the profile exists");
    let loaded = crate::profile::load_profile(&name).unwrap();
    let mut unclaimed = loaded.clone();
    unclaimed.proxy = None;
    crate::claude::apply_profile_to_claude_settings(&unclaimed, &[]).expect("the older apply");
    let live = || {
        let path = crate::claude::claude_settings_path().unwrap();
        session_auth(&std::fs::read_to_string(path).expect("the live settings"))
    };
    assert_eq!(
        live(),
        (Some(KEY.to_string()), None, true),
        "the older apply wrote the entry beside the helper"
    );

    crate::profile_cache::write_dead_key(
        &name,
        crate::usage::profile_key_fingerprint(&loaded).unwrap(),
    );
    stub.state().flow_states = vec![done_body(ACCOUNT, None)];
    crate::login_proxy(&proxy_login("px", None), "stub").expect("the re-login runs");

    let after = crate::profile::load_profile(&name).unwrap();
    assert_eq!(
        (
            stub.state().minted.clone(),
            after.api_key.as_deref() == Some(KEY),
            live(),
            after.env.get("ANTHROPIC_API_KEY").map(String::as_str),
        ),
        (
            vec![ACCOUNT.to_string()],
            false,
            (None, None, true),
            Some(KEY)
        ),
        "(minted, the old key kept, what the session sends, the entry in config.toml)"
    );
}

/// `px` holding `entry` under `ANTHROPIC_API_KEY`, loaded back from disk.
fn proxy_profile_with_env_entry(entry: &str) -> crate::profile::Profile {
    let name = ProfileName::from("px");
    crate::profile::update_profile_config(&name, |profile| {
        profile
            .env
            .insert("ANTHROPIC_API_KEY".to_string(), entry.to_string());
        Ok(())
    })
    .expect("save the entry")
    .expect("the profile exists");
    crate::profile::load_profile(&name).unwrap()
}

/// An older apply wrote `profile`'s entry into the live settings, then the
/// user wrote their own `value` over it.
fn write_users_own_live_key(profile: &crate::profile::Profile, value: &str) {
    let mut unclaimed = profile.clone();
    unclaimed.proxy = None;
    crate::claude::apply_profile_to_claude_settings(&unclaimed, &[]).expect("the older apply");
    let path = crate::claude::claude_settings_path().unwrap();
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the live settings")).unwrap();
    settings["env"]["ANTHROPIC_API_KEY"] = serde_json::json!(value);
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
}

/// What the live settings send after `profile`'s apply.
fn live_session_auth_after_apply(
    profile: &crate::profile::Profile,
) -> (Option<String>, Option<String>, bool) {
    crate::claude::apply_profile_to_claude_settings(profile, &["ANTHROPIC_API_KEY".to_string()])
        .expect("the apply");
    let path = crate::claude::claude_settings_path().unwrap();
    session_auth(&std::fs::read_to_string(path).expect("the live settings"))
}

/// A hand-broken `[proxy]` table leaves the claim unreadable, and such an
/// account is still a proxy account: its slot key is the one key it sends.
#[test]
fn an_unreadable_proxy_table_still_sends_the_slot_key_alone() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    proxy_profile_with_env_entry("env-key");
    crate::claude::tests::break_proxy_record("px");
    let loaded = crate::profile::load_profile(&ProfileName::from("px")).unwrap();
    assert!(
        loaded.proxy.is_none() && loaded.has_unreadable_proxy_record,
        "the table reads unreadable"
    );
    assert_eq!(
        session_auth(&crate::claude::build_claude_settings_json(None, &loaded, &[]).unwrap()),
        (None, None, true)
    );
}

/// An entry equal to the slot key is left out too: the key reaches a session
/// only through the helper, never as a raw value in the live settings, which
/// are written at the umask.
#[test]
fn an_entry_equal_to_the_slot_key_stays_out_of_the_live_settings() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let loaded = proxy_profile_with_env_entry(KEY);
    let mut unclaimed = loaded.clone();
    unclaimed.proxy = None;
    crate::claude::apply_profile_to_claude_settings(&unclaimed, &[]).expect("the older apply");
    let live_text = || {
        std::fs::read_to_string(crate::claude::claude_settings_path().unwrap())
            .expect("the live settings")
    };
    assert!(
        live_text().contains(KEY),
        "control: the older apply wrote the key's bytes"
    );
    assert_eq!(live_session_auth_after_apply(&loaded), (None, None, true));
    assert!(
        !live_text().contains(KEY),
        "the key's bytes appear nowhere in the live settings"
    );
}

/// Leaving an entry out never touches a value the user wrote.
#[test]
fn an_entry_left_out_keeps_a_users_own_live_value() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let loaded = proxy_profile_with_env_entry("env-key");
    write_users_own_live_key(&loaded, "user-key");
    assert_eq!(
        live_session_auth_after_apply(&loaded),
        (Some("user-key".to_string()), None, true)
    );
}

/// The slot key going returns the account to the env-only arm: the entry
/// comes back and the helper leaves.
#[test]
fn the_entry_returns_when_the_slot_key_goes() {
    let (_home, _stub, _config) = proxy_profile_on_stub(usage_body(500_000.0, 100.0));
    let mut keyless = proxy_profile_with_env_entry("env-key");
    assert_eq!(
        live_session_auth_after_apply(&keyless),
        (None, None, true),
        "the slot key leaves the entry out"
    );
    keyless.api_key = None;
    assert_eq!(
        live_session_auth_after_apply(&keyless),
        (Some("env-key".to_string()), None, false),
        "the env-only arm sends the entry alone"
    );
}

// ── pin 12: the pool read ───────────────────────────────────────────────────

/// A whole-pool `/usage` body: two accounts the proxy has figures for and one
/// it has read nothing for, every figure stamped `read_at`.
fn pool_usage_body(read_at: &str) -> Value {
    json!({"accounts": [
        {
            "account": "acct-1", "state": "ready", "available": true,
            "figures": [
                {"kind": "window", "id": "quota-5h", "label": "5h window", "used": 10.0,
                 "limit": 100.0, "unit": "percent", "window_secs": 18000,
                 "resets_at": "2030-01-01T05:00:00Z", "chain": "5h", "read_at": read_at},
                {"kind": "window", "id": "quota-7d", "label": "7d window", "used": 100.0,
                 "limit": 100.0, "unit": "percent", "window_secs": 604800,
                 "resets_at": "2030-01-05T00:00:00Z", "chain": "7d", "read_at": read_at},
                {"kind": "window", "id": "quota-30d", "label": "30d window", "used": 25.0,
                 "limit": 100.0, "unit": "percent", "window_secs": 2592000,
                 "resets_at": null, "read_at": read_at},
                {"kind": "balance", "id": "credit", "label": "credit",
                 "remaining": 204_977_981.0, "limit": 300_000_000.0, "used": 95_022_019.0,
                 "unit": "tokens", "expires_at": "2030-01-02T01:00:00Z", "read_at": read_at},
                {"kind": "channel", "id": "off-peak", "label": "off-peak", "open": true,
                 "read_at": read_at},
                {"kind": "channel", "id": "coding-plan-renewal", "label": "coding plan renewal",
                 "open": true, "summary": "renews 2030-02-01", "read_at": read_at},
                {"kind": "channel", "id": "reset-5h", "label": "5h resets", "open": false,
                 "summary": "0 available", "read_at": read_at},
                {"kind": "channel", "id": "reset-7d", "label": "7d resets", "open": true,
                 "summary": "1 available", "read_at": read_at},
                {"kind": "stats", "id": "request-stats", "label": "request stats",
                 "since": "2030-01-01T00:00:00Z",
                 "requests": {"attempted": 10000, "succeeded": 10000, "failed": 0, "cancelled": 0},
                 "mean_latency_ms": 840,
                 "tokens": {"input": 1, "output": 2, "cache_read": 3, "cache_creation": 4},
                 "read_at": read_at},
                {"kind": "offer", "id": "claim-offers", "label": "offers", "read_at": read_at,
                 "items": [{"id": "plan-a", "label": "plan a", "state": "available",
                            "grants": [{"label": "tokens", "amount": 5.0, "unit": "tokens"}]}]},
            ],
        },
        {
            "account": "acct-2", "state": "ready", "available": true,
            "figures": [
                {"kind": "balance", "id": "credit", "label": "credit",
                 "remaining": 79_999_980.0, "limit": 100_000_000.0, "used": 20_000_020.0,
                 "unit": "tokens", "expires_at": "2030-01-03T01:00:00Z", "read_at": read_at},
                {"kind": "channel", "id": "off-peak", "label": "off-peak", "open": false,
                 "read_at": read_at},
                {"kind": "channel", "id": "reset-5h", "label": "5h resets", "open": false,
                 "summary": "0 available", "read_at": read_at},
                {"kind": "channel", "id": "reset-7d", "label": "7d resets", "open": false,
                 "read_at": read_at},
                {"kind": "stats", "id": "request-stats", "label": "request stats",
                 "since": "2030-01-01T00:00:00Z",
                 "requests": {"attempted": 2345, "succeeded": 2344, "failed": 1, "cancelled": 0},
                 "mean_latency_ms": null,
                 "tokens": {"input": 0, "output": 0, "cache_read": 0, "cache_creation": 0},
                 "read_at": read_at},
            ],
        },
        {"account": "acct-3", "state": "ready", "available": true, "figures": []},
    ]})
}

/// A profile on the stub's bind with no `[proxy]` record: the funnel profile
/// an auto-route setup points sessions at.
fn create_bare_profile_on_disk(config: &mut crate::profile::AppConfig, name: &str, base: &str) {
    let profile = crate::profile::Profile::new(
        name.to_string(),
        Some(base.to_string()),
        Some(KEY.to_string()),
    );
    crate::profile::save_profile(&profile).expect("save profile");
    config.add(profile);
    crate::profile::save_app_state(&config.state).expect("save state");
}

/// A profile on a registered proxy's bind with no `[proxy]` record reads the
/// whole pool: one bar per balance pool carrying the compact grant-versus-left
/// figure, the pool's totals, one line per account, and a copy dated by its
/// stalest figure. The admin token authenticates the read; the profile's own
/// key never rides it.
#[test]
fn a_bare_profile_on_a_proxy_bind_reads_the_pool() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let read_epoch = crate::usage::now_epoch_secs() - 3 * 3600;
    let read_at = chrono::DateTime::from_timestamp(read_epoch, 0)
        .unwrap()
        .to_rfc3339();
    let admin = mint_admin_token();
    let stub = Stub::start(admin, pool_usage_body(&read_at));
    register_proxy_row(stub.port);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_bare_profile_on_disk(&mut config, "px", &stub.base);

    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let stats = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the pool read landed");
    assert!(
        stats.is_available,
        "one ready account makes the pool available"
    );

    assert_eq!(stats.bars.len(), 1, "one bar per declared balance pool");
    let bar = &stats.bars[0];
    assert_eq!(bar.label, "credit | 285M left of 400M");
    assert!(
        (bar.pct - 28.755_509_75).abs() < 1e-6,
        "spent/granted is 28.75550975%, got {}",
        bar.pct
    );
    assert_eq!(
        bar.resets_at.as_deref(),
        Some("2030-01-02T01:00:00Z"),
        "the pool's earliest expiry"
    );
    assert_eq!(
        (bar.used, bar.total),
        (None, None),
        "the compact detail replaces the raw amount column"
    );

    let rows: Vec<(&str, &str)> = stats
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.value.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("accounts", "2 ready | 1 unread"),
            ("resets", "0 open (5h) | 1 open (7d)"),
            ("offers", "1 claimable | 0 held"),
            ("requests", "12,345 (1 failed)"),
            (
                "acct-1",
                "5h 10% | 7d 100% | 30d window 25% | 205M left | off-peak open | coding plan renewal renews 2030-02-01 | 5h resets 0 available | 7d resets 1 available"
            ),
            (
                "acct-2",
                "5h - | 7d - | 80M left | off-peak closed | 5h resets 0 available | 7d resets closed"
            ),
            ("acct-3", "no figures"),
        ]
    );

    let age = crate::profile_json::cache_age_secs(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("a dated copy");
    let expected = u64::try_from(crate::usage::now_epoch_secs() - read_epoch).unwrap();
    assert!(
        age.abs_diff(expected) <= 1,
        "the copy is {age} s old, its stalest figure {expected} s"
    );

    assert_eq!(
        stub.state().usage_queries,
        vec![String::new()],
        "one whole-pool read, no account filter"
    );
    assert!(
        stub.state()
            .bearered
            .iter()
            .any(|line| line == "GET /clauth/v1/usage"),
        "the admin token carried the read"
    );
}

/// A profile carrying a `[proxy]` record keeps its per-account read: the pool
/// arm answers only a bare endpoint profile on the same bind.
#[test]
fn a_recorded_proxy_profile_reads_only_its_own_account() {
    let (_home, stub, config) = proxy_profile_on_stub(pool_usage_body("2030-01-01T00:00:00Z"));
    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    assert_eq!(
        stub.state().usage_queries,
        vec![ACCOUNT.to_string()],
        "the account filter every per-account read carries"
    );
    let stats = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the read landed");
    assert_eq!(
        stats
            .bars
            .iter()
            .map(|bar| bar.label.as_str())
            .collect::<Vec<_>>(),
        vec!["5h", "7d", "30d window"],
        "the recorded profile's own account's windows"
    );
}

/// A pool with nothing read for it reports no totals rather than a row of
/// zeros, and an unavailable account reads as its own state.
#[test]
fn a_pool_with_no_figures_reports_no_totals_instead_of_zero() {
    let body = json!({"accounts": [
        {"account": "acct-1", "state": "ready", "available": true, "figures": []},
        {"account": "acct-2", "state": "login_required", "available": false, "figures": []},
    ]});
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["px"]);
    let admin = mint_admin_token();
    let stub = Stub::start(admin, body);
    register_proxy_row(stub.port);
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    create_bare_profile_on_disk(&mut config, "px", &stub.base);

    let state = leg_state();
    crate::usage::reset_request_slots();
    run_leg(&state, &config);

    let stats = crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
        &ProfileName::from("px"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("the pool read landed");
    assert!(stats.bars.is_empty(), "no granted total, no bar");
    let rows: Vec<(&str, &str)> = stats
        .rows
        .iter()
        .map(|row| (row.label.as_str(), row.value.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("accounts", "0 ready | 1 not ready | 1 unread"),
            ("acct-1", "no figures"),
            ("acct-2", "login required"),
        ]
    );
}
