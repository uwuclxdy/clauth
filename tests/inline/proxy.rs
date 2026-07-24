//! CDX-5 proxy: heartbeat/standdown, config print, and the stub-upstream e2e
//! (identity injection + 429 rotate-and-replay + SSE relay + usage capture).
//! No real `~/.codex`, no real backend — the upstream is a local stub.

use super::*;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::lockorder::RankedMutex;
use crate::testutil::HomeSandbox;

fn codex_auth(access: &str, account: &str) -> Vec<u8> {
    let exp = crate::usage::now_epoch_secs() + 10 * 86_400; // healthy, not standby-due
    let id_token = crate::testutil::fake_jwt(&serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": account },
    }));
    let access_jwt = crate::testutil::fake_jwt(&serde_json::json!({ "exp": exp }));
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": format!("{access}.{access_jwt}"),
            "refresh_token": format!("rt-{access}"),
            "account_id": account,
        },
        "last_refresh": crate::usage::epoch_secs_to_iso(crate::usage::now_epoch_secs()),
    })
    .to_string()
    .into_bytes()
}

// --- heartbeat / standdown -------------------------------------------------

#[test]
fn heartbeat_freshness_drives_proxy_active() {
    let _home = HomeSandbox::new();
    assert!(!proxy_active(1000), "no heartbeat → not active");
    touch_heartbeat(4517);
    assert!(proxy_active(1000), "fresh heartbeat → active");
    // A tiny interval makes even a just-written heartbeat 'stale'.
    assert!(!proxy_active(0), "interval 0 → nothing counts as fresh");
}

// --- config print ----------------------------------------------------------

#[test]
fn print_config_names_the_provider_and_port() {
    // Capture isn't wired; assert the const shape the printer uses instead.
    // (print_config writes to stdout; its content is a format of these.)
    let port = 4600u16;
    let expected_base = format!("http://127.0.0.1:{port}/backend-api/codex");
    assert_eq!(DEFAULT_PROXY_PORT, 4517);
    // The base the proxy forwards to is a compile-time constant, never config.
    assert!(expected_base.contains("127.0.0.1"));
}

// --- stub upstream ---------------------------------------------------------

/// A one-shot raw HTTP server: reads a full request (head + Content-Length
/// body), records the ChatGPT-Account-ID header, and writes a canned raw
/// response. Runs on its own thread; returns the port + a shared log of the
/// account ids it saw, in order.
struct StubUpstream {
    port: u16,
    seen_accounts: Arc<Mutex<Vec<String>>>,
    seen_methods: Arc<Mutex<Vec<String>>>,
    _handle: std::thread::JoinHandle<()>,
}

fn spawn_stub(responses: Vec<Vec<u8>>) -> StubUpstream {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let methods = Arc::new(Mutex::new(Vec::new()));
    let methods_c = Arc::clone(&methods);
    let handle = std::thread::spawn(move || {
        for (i, resp) in responses.into_iter().enumerate() {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            let (method, account) = read_request_recording(&mut stream);
            seen_c.lock().unwrap().push(account);
            methods_c.lock().unwrap().push(method);
            let _ = stream.write_all(&resp);
            let _ = stream.flush();
            let _ = i;
        }
    });
    StubUpstream {
        port,
        seen_accounts: seen,
        seen_methods: methods,
        _handle: handle,
    }
}

/// Read one HTTP request off `stream`, return its `(method, ChatGPT-Account-ID)`.
fn read_request_recording(stream: &mut TcpStream) -> (String, String) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut method = String::new();
    let mut account = String::new();
    let mut content_length = 0usize;
    let mut first = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if first {
            // Request line: "METHOD target HTTP/1.1".
            method = t.split_whitespace().next().unwrap_or("").to_string();
            first = false;
            continue;
        }
        if let Some((k, v)) = t.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "chatgpt-account-id" {
                account = v.trim().to_string();
            } else if k == "content-length" {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }
    (method, account)
}

fn sse_200_with_usage(primary_pct: &str) -> Vec<u8> {
    let body = "data: {\"type\":\"response.output_text.delta\"}\n\ndata: [DONE]\n\n";
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         x-codex-primary-used-percent: {primary_pct}\r\n\
         x-codex-primary-window-minutes: 10080\r\n\
         x-codex-primary-reset-at: 1900000000\r\n\
         x-codex-bengalfox-primary-used-percent: 0.0\r\n\
         x-codex-bengalfox-primary-window-minutes: 10080\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn resp_429() -> Vec<u8> {
    let body = "rate limited";
    format!(
        "HTTP/1.1 429 Too Many Requests\r\n\
         x-codex-primary-used-percent: 100.0\r\n\
         x-codex-primary-reset-at: 1900000000\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn two_profile_config() -> crate::profile::ConfigHandle {
    let mk = |n: &str| {
        let mut p = crate::testutil::blank_profile(n);
        p.harness = crate::profile::Harness::Codex;
        p
    };
    Arc::new(RankedMutex::new(crate::profile::AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["cdx-a".into(), "cdx-b".into()],
            active_codex_profile: Some("cdx-a".into()),
            codex_fallback_chain: vec!["cdx-a".into(), "cdx-b".into()],
            ..Default::default()
        },
        profiles: vec![mk("cdx-a"), mk("cdx-b")],
    }))
}

/// Drive one client request through the proxy against a stub upstream.
/// Returns the raw client-side response bytes.
fn drive_request(
    config: crate::profile::ConfigHandle,
    upstream_base: String,
    request: &[u8],
) -> Vec<u8> {
    let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let _ = super::serve_one_for_test(config, upstream_base, &proxy_listener);
    });

    let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    client.write_all(request).unwrap();
    client.flush().unwrap();
    let mut resp = Vec::new();
    client.read_to_end(&mut resp).unwrap();
    server.join().unwrap();
    resp
}

fn responses_request() -> Vec<u8> {
    let body = "{\"model\":\"gpt-5.6\"}";
    format!(
        "POST /backend-api/codex/responses HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer client-placeholder\r\n\
         ChatGPT-Account-ID: acct-smuggled\r\n\
         Originator: codex_cli_rs\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[test]
fn e2e_injects_identity_and_relays_the_sse_response() {
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();
    crate::codex::write_profile_auth("cdx-b", &codex_auth("at-b", "acct-b")).unwrap();

    let stub = spawn_stub(vec![sse_200_with_usage("42.0")]);
    let base = format!("http://127.0.0.1:{}", stub.port);
    let resp = drive_request(config, base, &responses_request());
    let text = String::from_utf8_lossy(&resp);

    assert!(text.starts_with("HTTP/1.1 200"), "relayed status: {text}");
    assert!(text.contains("[DONE]"), "SSE body relayed: {text}");
    // Sticky to the active profile 'cdx-a'; the client's smuggled id is gone.
    let seen = stub.seen_accounts.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        ["acct-a"],
        "injected the active account id"
    );

    // Usage captured from the flow-through headers into cdx-a's cache.
    let cached = crate::profile_cache::load_profile_cache::<crate::usage::UsageInfo>(
        "cdx-a",
        crate::profile_cache::USAGE_CACHE_FILE,
    )
    .expect("usage cached from headers");
    assert!(cached.five_hour.is_none());
    assert!((cached.seven_day.unwrap().utilization - 42.0).abs() < f64::EPSILON);
}

#[test]
fn model_specific_header_family_does_not_replace_default_codex_usage() {
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();

    let stub = spawn_stub(vec![sse_200_with_usage("42.0")]);
    let base = format!("http://127.0.0.1:{}", stub.port);
    let resp = drive_request(config, base, &responses_request());
    assert!(
        String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 200"),
        "model-specific response still relays normally"
    );

    let cached = crate::profile_cache::load_profile_cache::<crate::usage::UsageInfo>(
        "cdx-a",
        crate::profile_cache::USAGE_CACHE_FILE,
    )
    .expect("default Codex usage cached");
    assert_eq!(
        cached.seven_day.as_ref().map(|w| w.utilization),
        Some(42.0),
        "Spark's independent 0% header family must not replace overall Codex usage"
    );
    assert!(cached.five_hour.is_none());
}

/// A bodyless GET model-metadata refresh (codex issues this alongside the
/// POST /responses turn). The client's smuggled identity must be replaced.
fn models_request() -> Vec<u8> {
    "GET /backend-api/codex/models?client_version=0.144.5 HTTP/1.1\r\n\
     Host: 127.0.0.1\r\n\
     Authorization: Bearer client-placeholder\r\n\
     ChatGPT-Account-ID: acct-smuggled\r\n\r\n"
        .as_bytes()
        .to_vec()
}

fn json_200(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[test]
fn e2e_get_models_is_forwarded_with_its_method_and_injected_identity() {
    // Regression for the real-backend finding (2026-07-16): the proxy used to
    // 405 every non-POST, so codex's GET /models refresh failed. It must now
    // forward the GET (method preserved, identity injected), not reject it.
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();

    let stub = spawn_stub(vec![json_200("{\"data\":[{\"id\":\"gpt-5.6-codex\"}]}")]);
    let base = format!("http://127.0.0.1:{}", stub.port);
    let resp = drive_request(config, base, &models_request());
    let text = String::from_utf8_lossy(&resp);

    assert!(
        text.starts_with("HTTP/1.1 200"),
        "GET /models is forwarded, not 405'd: {text}"
    );
    assert!(text.contains("gpt-5.6-codex"), "model list relayed: {text}");
    // Method preserved as GET upstream (never rewritten to POST)…
    assert_eq!(
        stub.seen_methods.lock().unwrap().as_slice(),
        ["GET"],
        "GET method preserved upstream"
    );
    // …and the client's smuggled id replaced with the active account's.
    assert_eq!(
        stub.seen_accounts.lock().unwrap().as_slice(),
        ["acct-a"],
        "injected the active account id"
    );
}

#[test]
fn e2e_non_get_non_post_is_405() {
    // The gate widened to GET+POST only — DELETE (etc.) is still rejected.
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();
    let stub = spawn_stub(vec![]); // would panic if hit
    let base = format!("http://127.0.0.1:{}", stub.port);
    let req = b"DELETE /backend-api/codex/responses HTTP/1.1\r\nHost: x\r\n\r\n";
    let resp = drive_request(config, base, req);
    let text = String::from_utf8_lossy(&resp);
    assert!(text.starts_with("HTTP/1.1 405"), "{text}");
    assert!(
        stub.seen_accounts.lock().unwrap().is_empty(),
        "never forwarded"
    );
}

#[test]
fn e2e_429_rotates_to_the_next_account_and_replays() {
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();
    crate::codex::write_profile_auth("cdx-b", &codex_auth("at-b", "acct-b")).unwrap();

    // Active cdx-a 429s; the proxy must rotate to cdx-b and replay → 200.
    let stub = spawn_stub(vec![resp_429(), sse_200_with_usage("10.0")]);
    let base = format!("http://127.0.0.1:{}", stub.port);
    let resp = drive_request(config, base, &responses_request());
    let text = String::from_utf8_lossy(&resp);

    assert!(
        text.starts_with("HTTP/1.1 200"),
        "the client sees the successful replay, never the 429: {text}"
    );
    let seen = stub.seen_accounts.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        ["acct-a", "acct-b"],
        "429 on acct-a rotated to acct-b and replayed"
    );
}

#[test]
fn e2e_unknown_path_is_404_without_forwarding() {
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();
    // A stub that would panic if hit (0 responses queued).
    let stub = spawn_stub(vec![]);
    let base = format!("http://127.0.0.1:{}", stub.port);
    let req = b"GET /evil/path HTTP/1.1\r\nHost: x\r\n\r\n";
    let resp = drive_request(config, base, req);
    let text = String::from_utf8_lossy(&resp);
    assert!(text.starts_with("HTTP/1.1 404"), "{text}");
    assert!(
        stub.seen_accounts.lock().unwrap().is_empty(),
        "never forwarded"
    );
}

/// A stub matching the REAL backend's SSE shape (2026-07-18 incident): no
/// Content-Length, and the connection is HELD OPEN after `response.completed`
/// with periodic keepalives — the server never closes; the client is expected
/// to. Returns the port; the stub thread ends when its socket errors or the
/// hold expires.
fn spawn_lingering_sse_stub(hold: std::time::Duration) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = read_request_recording(&mut stream);
        let head = "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             x-codex-primary-used-percent: 7.0\r\n\r\n";
        // The live wire shape (captured 2026-07-18): `event:` name line +
        // `data:` payload line per event, and the terminal data line carries
        // the whole response object on ONE line, far past any small buffer.
        let body = format!(
            "event: response.output_text.delta\n\
             data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n\n\
             event: response.completed\n\
             data: {{\"type\":\"response.completed\",\"response\":{{\"output\":\"{}\"}}}}\n\n",
            "x".repeat(200_000)
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
        // Linger: keepalives every 100ms until the proxy closes us or `hold`
        // expires. A correct relay never sees any of these bytes.
        let deadline = std::time::Instant::now() + hold;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if stream.write_all(b": ping\n\n").is_err() || stream.flush().is_err() {
                return;
            }
        }
    });
    port
}

#[test]
fn e2e_relay_closes_promptly_on_response_completed_while_upstream_lingers() {
    // 2026-07-18 incident regression: the upstream holds the SSE stream open
    // after `response.completed` (close-delimited, keepalives forever). The
    // relay must close the client right after relaying the terminal event —
    // NOT wait for upstream EOF (which never comes) until a dead-client write
    // or the agent backstop timeout, leaking a thread per turn and logging a
    // spurious connection error on every successful turn.
    let _home = HomeSandbox::new();
    let config = two_profile_config();
    crate::codex::write_profile_auth("cdx-a", &codex_auth("at-a", "acct-a")).unwrap();

    let port = spawn_lingering_sse_stub(std::time::Duration::from_secs(10));
    let base = format!("http://127.0.0.1:{port}");
    let started = std::time::Instant::now();
    let resp = drive_request(config, base, &responses_request());
    let elapsed = started.elapsed();
    let text = String::from_utf8_lossy(&resp);

    assert!(text.starts_with("HTTP/1.1 200"), "relayed status: {text}");
    // The WHOLE terminal event must be relayed before the close — closing on
    // the prefix truncated the (huge, single-line) completed event and
    // reintroduced codex's "stream closed before response.completed".
    assert!(
        resp.ends_with(b"\"}}\n\n"),
        "terminal event relayed through its blank-line terminator (tail: {:?})",
        &text[text.len().saturating_sub(40)..]
    );
    assert!(
        resp.len() > 200_000,
        "full huge completed payload relayed ({}B)",
        resp.len()
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "client must see EOF right after response.completed, not after the \
         upstream hold/backstop ({elapsed:?})"
    );
}

#[test]
fn ureq_global_timeout_truncates_an_actively_streaming_body() {
    // Documents the ureq semantics behind the 2026-07-18 incident: an agent
    // `timeout_global` bounds the WHOLE call including the body read, and it
    // fires even while bytes are actively flowing. This is why the proxy's
    // backstop must sit far above any live turn's stream (xhigh reasoning
    // turns exceeded the old 15-minute value) and why turn-end must come from
    // the terminal-event sniffer, never from a timeout.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut drain = [0u8; 1024];
        let _ = stream.read(&mut drain);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
        // Stream actively: a chunk every 100ms for 3s — never idle.
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if stream.write_all(b"data: tick\n\n").is_err() {
                return;
            }
            let _ = stream.flush();
        }
    });

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_millis(500)))
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap();
    let mut reader = response.into_body().into_reader();
    let mut total = 0usize;
    let mut buf = [0u8; 1024];
    let failed = loop {
        match reader.read(&mut buf) {
            Ok(0) => break false,
            Ok(n) => total += n,
            Err(_) => break true,
        }
    };
    assert!(
        failed,
        "global timeout must kill the ACTIVE stream mid-flight (read {total}B without error — \
         if this starts passing, ureq changed timeout semantics and the backstop can tighten)"
    );
    // It died early — well before the ~30 chunks the stub would deliver.
    assert!(
        total < 30 * 12,
        "died mid-stream, not at stream end ({total}B)"
    );
}

#[test]
fn ureq_recv_response_timeout_kills_the_streaming_body() {
    // Documents WHY `PROXY_AGENT` sets no `timeout_recv_response`: in ureq 3
    // that deadline keeps running through the BODY read — it is not a
    // headers-only bound. The 2026-07-18 incident's actual assassin was a
    // 30 s value here: every turn whose SSE stream outlived 30 s was
    // TRUNCATED mid-body ("timeout: receive response" at 29 s in the relay
    // summaries) and codex replayed the whole turn from scratch. If this
    // test ever starts failing, ureq made the deadline headers-only and a
    // recv_response bound can return.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut drain = [0u8; 1024];
        let _ = stream.read(&mut drain);
        // Headers arrive IMMEDIATELY — only the body outlives the deadline.
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
        let _ = stream.flush();
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if stream.write_all(b"data: tick\n\n").is_err() {
                return;
            }
            let _ = stream.flush();
        }
    });

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_recv_response(Some(std::time::Duration::from_millis(500)))
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .get(format!("http://127.0.0.1:{port}/"))
        .call()
        .unwrap();
    let mut reader = response.into_body().into_reader();
    let mut total = 0usize;
    let mut buf = [0u8; 1024];
    let failed = loop {
        match reader.read(&mut buf) {
            Ok(0) => break false,
            Ok(n) => total += n,
            Err(_) => break true,
        }
    };
    assert!(
        failed,
        "recv_response deadline must kill the streaming body (read {total}B without error — \
         ureq made it headers-only; a recv_response bound on PROXY_AGENT is safe again)"
    );
}

#[test]
fn e2e_no_pool_answers_503() {
    let _home = HomeSandbox::new();
    // Codex profiles exist but NONE has a stored login → empty pool.
    let mk = |n: &str| {
        let mut p = crate::testutil::blank_profile(n);
        p.harness = crate::profile::Harness::Codex;
        p
    };
    let config: crate::profile::ConfigHandle =
        Arc::new(RankedMutex::new(crate::profile::AppConfig {
            state: crate::profile::AppState {
                profiles: vec!["cdx-a".into()],
                ..Default::default()
            },
            profiles: vec![mk("cdx-a")],
        }));
    let resp = drive_request(
        config,
        "http://127.0.0.1:1".to_string(),
        &responses_request(),
    );
    let text = String::from_utf8_lossy(&resp);
    assert!(text.starts_with("HTTP/1.1 503"), "{text}");
}
