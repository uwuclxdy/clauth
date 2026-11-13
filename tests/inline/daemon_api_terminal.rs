#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `GET /api/v1/panes/<id>/stream`: the WebSocket terminal bridge.
//!
//! Two layers: the route's validation cascade drives `routes::handle` directly
//! (no socket), and the bridge itself runs end to end over a real TLS listener
//! against a `/bin/sh` stand-in for herdr — the same fixture discipline as the
//! panes route: no test runs a real herdr.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Drained, drain_frames};
use crate::daemon::api::devices::{self, RowWatch, Tier};
use crate::daemon::api::http::{Request, Response, WsHeaders};
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut};
use crate::daemon::api::routes::{ApiContext, handle};
use crate::daemon::api::tests as server_tests;
use crate::daemon::api::ticket::{Stamp, Tickets};
use crate::daemon::api::{Limits, serve_connection, tls};
use crate::testutil::{HomeSandbox, body_json};

/// The bearer of the control-tier device; the bridge gives it `control`.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The bearer of the view-tier device; the bridge gives it `observe`.
const VIEW_TOKEN: &str = "89abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123";
const DRIVER: &str = "driver";
const VIEWER: &str = "viewer";

/// The RFC 6455's own worked example, so the accept hash is pinned against
/// the spec rather than against this implementation.
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// `herdr api snapshot`'s envelope, trimmed to the one pane the bridge looks
/// up, at the geometry the tests below expect the control attach to carry.
const SNAPSHOT: &str = r#"{"id":"cli:api:snapshot","result":{"snapshot":{"focused_pane_id":"w9:p1","panes":[{"pane_id":"w9:p1","rect":{"height":57,"width":88,"x":0,"y":0}}],"layouts":[]},"type":"api_result"}}"#;

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

fn status_path() -> std::path::PathBuf {
    crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("status.json")
}

fn seed_devices() {
    devices::seed_for_tests(DRIVER, Tier::Control, TOKEN).expect("seed the control device");
    devices::seed_for_tests(VIEWER, Tier::View, VIEW_TOKEN).expect("seed the view device");
}

/// A context with both devices paired, the snapshot probe, and a `/bin/sh`
/// stand-in for herdr that records its argv and stdin and speaks the frames
/// the test baked into `script`.
fn ctx(script: &str) -> Arc<ApiContext> {
    seed_devices();
    let spawn = sh_spawn(script);
    ApiContext::new(
        config(),
        status_path(),
        None,
        None,
        Box::new(|args, _deadline| match args {
            ["api", "snapshot"] => snapshot_out(),
            _ => HerdrProbeOut::Ran(None),
        }),
        Arc::new(|| None),
        spawn,
        super::super::create::unspawnable_session(),
        #[cfg(unix)]
        None,
    )
}

fn snapshot_out() -> HerdrProbeOut {
    HerdrProbeOut::Ran(Some(HerdrOut {
        success: true,
        stdout: SNAPSHOT.as_bytes().to_vec(),
        stderr: Vec::new(),
    }))
}

/// The herdr stand-in: `sh -c <script> -- <args…>`, so the script sees the
/// bridge's spawn argv as `"$@"` and whatever the bridge forwards on stdin as
/// its own stdin.
fn sh_spawn(script: &str) -> super::TerminalSpawn {
    let script = script.to_string();
    Arc::new(move |args: &[&str]| {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(&script).arg("--").args(args);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        Ok(super::TerminalChild {
            stdin: Box::new(child.stdin.take().expect("sh stdin")),
            stdout: Box::new(child.stdout.take().expect("sh stdout")),
            child: Some(child),
        })
    })
}

/// The response head must be the upgrade this test asked for; several flows
/// below discard it otherwise, and a 401 read as "no frames" costs minutes.
fn assert_101(client: &mut WsClient) {
    let head = client.read_head().expect("read 101");
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
}

fn peer() -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 7], 50_000))
}

/// A request with the WebSocket handshake headers filled in, for the
/// validation cascade.
fn ws_request(method: &str, path: &str, bearer: &str) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        bearer: Some(bearer.to_string()),
        authorization_seen: true,
        if_none_match: None,
        body: Vec::new(),
        keep_alive: true,
        ws: WsHeaders {
            requested: true,
            protocol: Some("websocket".to_string()),
            key: Some(WS_KEY.to_string()),
            version: Some("13".to_string()),
            offer: Default::default(),
        },
        cross_origin: Default::default(),
        content_type: Default::default(),
    }
}

fn call(ctx: &ApiContext, req: &Request) -> Response {
    handle(ctx, req, peer()).response
}

// --- The validation cascade, without a socket --------------------------------

#[test]
fn the_accept_key_matches_the_rfc_vector() {
    assert_eq!(
        crate::daemon::api::http::accept_key(WS_KEY).as_deref(),
        Some(WS_ACCEPT)
    );
    // A key that is not the base64 of 16 bytes is refused, not hashed.
    assert_eq!(crate::daemon::api::http::accept_key("not a key"), None);
    assert_eq!(crate::daemon::api::http::accept_key("AAAA"), None);
}

#[test]
fn strip_session_env_removes_every_session_var() {
    let mut cmd = std::process::Command::new("/usr/bin/env");
    cmd.env("HERDR_ENV", "1")
        .env("HERDR_SOCKET_PATH", "/session/x")
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_TAB_ID", "w1:t1")
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_BIN_PATH", "/kept/herdr");
    crate::herdr::strip_session_env(&mut cmd);
    let out = cmd.output().expect("run env");
    let text = String::from_utf8_lossy(&out.stdout);
    for gone in [
        "HERDR_ENV=",
        "HERDR_SOCKET_PATH=",
        "HERDR_PANE_ID=",
        "HERDR_TAB_ID=",
        "HERDR_WORKSPACE_ID=",
    ] {
        assert!(!text.contains(gone), "{gone} survived the strip");
    }
    assert!(text.contains("HERDR_BIN_PATH=/kept/herdr"));
}

#[test]
fn a_missing_upgrade_is_refused_with_426() {
    let _home = HomeSandbox::new();
    let ctx = ctx(":");
    let mut req = ws_request("GET", "/api/v1/panes/w9:p1/stream", TOKEN);
    req.ws = WsHeaders::default();
    let resp = call(&ctx, &req);
    assert_eq!(resp.status, 426);
}

#[test]
fn a_wrong_websocket_version_is_refused() {
    let _home = HomeSandbox::new();
    let ctx = ctx(":");
    let mut req = ws_request("GET", "/api/v1/panes/w9:p1/stream", TOKEN);
    req.ws.version = Some("8".to_string());
    let resp = call(&ctx, &req);
    assert_eq!(resp.status, 400);
}

#[test]
fn a_post_to_the_stream_path_is_a_405() {
    let _home = HomeSandbox::new();
    let ctx = ctx(":");
    let resp = call(
        &ctx,
        &ws_request("POST", "/api/v1/panes/w9:p1/stream", TOKEN),
    );
    assert_eq!(resp.status, 405);
}

#[test]
fn absent_herdr_answers_503() {
    let _home = HomeSandbox::new();
    seed_devices();
    let ctx = ApiContext::new(
        config(),
        status_path(),
        None,
        None,
        Box::new(|_, _| HerdrProbeOut::NotInstalled),
        Arc::new(|| None),
        super::unspawnable_terminal(),
        super::super::create::unspawnable_session(),
        #[cfg(unix)]
        None,
    );
    let resp = call(
        &ctx,
        &ws_request("GET", "/api/v1/panes/w9:p1/stream", TOKEN),
    );
    assert_eq!(resp.status, 503);
    assert!(String::from_utf8_lossy(&resp.body).contains("herdr is not installed"));
}

/// The pane id is looked up in the snapshot after one percent-decode at the
/// binding (a generated client sends `w9%3Ap1`), and a pane the snapshot does
/// not hold answers 404 however it is spelled.
#[test]
fn a_pane_missing_from_the_snapshot_answers_404() {
    let _home = HomeSandbox::new();
    let ctx = ctx(":");
    let pane_not_found = serde_json::json!({
        "ok": false,
        "error": "pane_not_found",
        "reason": "no pane with that id in herdr's default session",
    });
    for path in [
        "/api/v1/panes/w9:pZZ/stream",
        "/api/v1/panes/w9%3ApZZ/stream",
    ] {
        let resp = call(&ctx, &ws_request("GET", path, TOKEN));
        assert_eq!(
            (resp.status, body_json(&resp)),
            (404, pane_not_found.clone()),
            "{path}"
        );
    }
    for path in ["/api/v1/panes/w9:p1/stream", "/api/v1/panes/w9%3Ap1/stream"] {
        let handled = handle(&ctx, &ws_request("GET", path, TOKEN), peer());
        assert_eq!(handled.response.status, 101, "{path}");
        assert_eq!(
            handled.hijack.map(|hijack| hijack.pane_id),
            Some("w9:p1".to_string()),
            "{path}"
        );
    }
    let malformed = call(
        &ctx,
        &ws_request("GET", "/api/v1/panes/w9%3zp1/stream", TOKEN),
    );
    assert_eq!(
        (malformed.status, body_json(&malformed)),
        (404, serde_json::json!({"ok": false, "error": "not_found"})),
        "a malformed encoding is no route at all, never a missing pane"
    );
}

// --- The bridge, end to end over TLS -----------------------------------------

/// A minimal WebSocket client: the handshake, then masked client frames and
/// parsed server frames.
struct WsClient {
    stream: rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    buf: Vec<u8>,
}

impl WsClient {
    fn connect(port: u16, client: &Arc<rustls::ClientConfig>) -> std::io::Result<Self> {
        let name = rustls::pki_types::ServerName::try_from(server_tests::SERVER_NAME)
            .map_err(std::io::Error::other)?
            .to_owned();
        let conn = rustls::ClientConnection::new(Arc::clone(client), name)
            .map_err(std::io::Error::other)?;
        let sock = std::net::TcpStream::connect(("127.0.0.1", port))?;
        sock.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Self {
            stream: rustls::StreamOwned::new(conn, sock),
            buf: Vec::new(),
        })
    }

    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }

    fn upgrade_request(&mut self, path: &str, bearer: &str) -> std::io::Result<()> {
        self.send(
            format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Authorization: Bearer {bearer}\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Key: {WS_KEY}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
    }

    /// The response head, up to and including the blank line.
    fn read_head(&mut self) -> std::io::Result<String> {
        let mut chunk = [0u8; 512];
        loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..i + 4]).into_owned();
                self.buf.drain(..i + 4);
                return Ok(head);
            }
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::other("eof before a response head"));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// One client text frame, masked with the RFC's example key.
    fn send_text(&mut self, payload: &str) -> std::io::Result<()> {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let payload = payload.as_bytes();
        let mut frame = vec![0x81];
        match payload.len() {
            len @ 0..=125 => frame.push(0x80 | len as u8),
            len @ 126..=65_535 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        for (i, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[i % 4]);
        }
        self.send(&frame)
    }

    /// One server frame: `(opcode, payload)`.
    fn recv_frame(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(frame) = try_parse_frame(&mut self.buf) {
                return Ok(frame);
            }
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::other("eof mid-frame"));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Parse one unmasked server frame out of `buf`, if a whole one is there.
fn try_parse_frame(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0F;
    let len7 = (buf[1] & 0x7F) as usize;
    let (len, header) = match len7 {
        126 => {
            if buf.len() < 4 {
                return None;
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4)
        }
        127 => {
            if buf.len() < 10 {
                return None;
            }
            let mut wide = [0u8; 8];
            wide.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(wide) as usize, 10)
        }
        len => (len, 2),
    };
    if buf.len() < header + len {
        return None;
    }
    let payload = buf[header..header + len].to_vec();
    buf.drain(..header + len);
    Some((opcode, payload))
}

/// One end-to-end server: a listener and one served connection. The TLS
/// configs are fully loaded before the tempdir drops, so nothing on the wire
/// outlives this call.
struct Served {
    port: u16,
    client_tls: Arc<rustls::ClientConfig>,
    /// The loglines the serving thread captured, handed back once it is done.
    lines: Arc<std::sync::Mutex<Option<Vec<String>>>>,
}

fn served(ctx: Arc<ApiContext>) -> Option<Served> {
    served_cfg(ctx, 1, Limits::DEFAULT)
}

/// [`served`] for the cases that need more than one connection or a tighter
/// connection budget.
fn served_cfg(ctx: Arc<ApiContext>, conns: usize, limits: Limits) -> Option<Served> {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((paths, ca_crt)) = server_tests::generate_chain(dir.path()).expect("fixture") else {
        eprintln!("SKIPPED: openssl is not usable here");
        return None;
    };
    let server_tls = tls::server_config_from(&paths).expect("server config");
    let client_tls = server_tests::client_config(&ca_crt).expect("client config");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let captured = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&captured);
    std::thread::Builder::new()
        .name("terminal-test-server".into())
        .spawn(move || {
            let lines = crate::logline::LogLines::new();
            let _capture = lines.capture_here();
            for _ in 0..conns {
                let (stream, peer) = listener.accept().expect("accept");
                serve_connection(stream, peer, &server_tls, &ctx, limits);
            }
            *sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(lines.snapshot());
        })
        .expect("spawn server thread");
    Some(Served {
        port,
        client_tls,
        lines: captured,
    })
}

/// The loglines the serving thread captured, once it has finished.
fn captured_lines(served: &Served) -> Vec<String> {
    for _ in 0..250 {
        if let Some(lines) = served
            .lines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the serving thread never reported its captured loglines");
}

/// Wait until `path` contains `needle`, or fail.
fn wait_for_file_contains(path: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains(needle)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{path:?} never contained {needle:?}");
}

/// Wait until `path` holds exactly this whitespace-split argv, or fail. The
/// fixture writes one argument per line.
fn wait_for_argv(path: &Path, want: &[&str]) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            let fields: Vec<String> = text.split_whitespace().map(str::to_string).collect();
            if !fields.is_empty() {
                let matched: Vec<&str> = fields.iter().map(String::as_str).collect();
                assert_eq!(
                    matched, want,
                    "the attach argv, exactly (until the deadline this retries)"
                );
                return fields;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{path:?} never held an argv");
}

// --- The browser ticket, over TLS --------------------------------------------

const TICKET_ORIGIN: &str = "https://app.example.com";
const TICKET_PATH: &str = "/api/v1/panes/w9:p1/ticket";
const STREAM_PATH: &str = "/api/v1/panes/w9:p1/stream";
const HOLD_A_FRAME: &str =
    "printf '%s\\n' '{\"type\":\"terminal.frame\",\"seq\":1}'; cat > /dev/null";

/// One mint on its own connection, answered as `(head, body)`.
fn mint(served: &Served, bearer: &str, origin: Option<&str>) -> (String, Vec<u8>) {
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    let origin = origin.map_or(String::new(), |origin| format!("Origin: {origin}\r\n"));
    client
        .send(
            format!(
                "POST {TICKET_PATH} HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Authorization: Bearer {bearer}\r\n\
                 {origin}\
                 Content-Length: 0\r\n\
                 Connection: close\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the mint");
    let head = client.read_head().expect("read the mint's head");
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")?
                .parse::<usize>()
                .ok()
        })
        .expect("a sized body");
    let mut body = std::mem::take(&mut client.buf);
    let mut chunk = [0u8; 512];
    while body.len() < length {
        let n = client.stream.read(&mut chunk).expect("read the body");
        assert_ne!(n, 0, "eof inside the body");
        body.extend_from_slice(&chunk[..n]);
    }
    (head, body)
}

/// The live ticket out of a `201`.
fn minted(served: &Served, bearer: &str) -> String {
    let (head, body) = mint(served, bearer, Some(TICKET_ORIGIN));
    assert!(head.starts_with("HTTP/1.1 201 "), "the head: {head}");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("a JSON body");
    body["ticket"].as_str().expect("a ticket").to_string()
}

/// An upgrade carrying no `Authorization`, as a browser's does. `offer` is the
/// whole `Sec-WebSocket-Protocol` value.
fn present(served: &Served, origin: &str, offer: &str) -> (WsClient, String) {
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .send(
            format!(
                "GET {STREAM_PATH} HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Origin: {origin}\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Key: {WS_KEY}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Protocol: {offer}\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the upgrade");
    let head = client.read_head().expect("read the upgrade's head");
    (client, head)
}

fn offer_of(ticket: &str) -> String {
    format!("clauth.terminal.v1, clauth.ticket.{ticket}")
}

#[test]
fn a_mint_answers_a_ticket_that_nothing_may_cache() {
    let _home = HomeSandbox::new();
    let Some(served) = served(ctx(HOLD_A_FRAME)) else {
        return;
    };
    let (head, body) = mint(&served, VIEW_TOKEN, Some(TICKET_ORIGIN));
    assert!(head.starts_with("HTTP/1.1 201 "), "the head: {head}");
    assert!(
        head.contains("Cache-Control: no-store\r\n"),
        "the head: {head}"
    );
    let body: serde_json::Value = serde_json::from_slice(&body).expect("a JSON body");
    let ticket = body["ticket"].as_str().expect("a ticket");
    assert_eq!(ticket.len(), 64, "{ticket:?}");
    assert!(
        ticket
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    );
    assert_eq!(
        body,
        serde_json::json!({"ticket": ticket, "expires_in": 30})
    );
}

#[test]
fn a_mint_with_no_origin_mints_nothing() {
    let _home = HomeSandbox::new();
    let Some(served) = served(ctx(HOLD_A_FRAME)) else {
        return;
    };
    let (head, body) = mint(&served, TOKEN, None);
    assert!(head.starts_with("HTTP/1.1 400 "), "the head: {head}");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("origin_required"), "the body: {body}");
    assert!(!body.contains("\"ticket\""), "the body: {body}");
}

#[test]
fn a_ticket_attaches_at_its_devices_tier_and_selects_a_fixed_protocol() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = dir.path().join("argv");
    let script = format!("printf '%s\\n' \"$@\" > {}; {HOLD_A_FRAME}", argv.display());
    let Some(served) = served_cfg(ctx(&script), 2, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, VIEW_TOKEN);
    let (mut client, head) = present(&served, TICKET_ORIGIN, &offer_of(&ticket));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    assert!(
        head.contains("Sec-WebSocket-Protocol: clauth.terminal.v1\r\n"),
        "the head: {head}"
    );
    assert!(!head.contains(&ticket), "the head: {head}");
    let (opcode, payload) = client.recv_frame().expect("a frame");
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, br#"{"type":"terminal.frame","seq":1}"#);
    wait_for_argv(
        &argv,
        &[
            "terminal", "session", "observe", "w9:p1", "--cols", "88", "--rows", "57",
        ],
    );
}

/// A spent ticket, a ticket from the wrong page and no ticket at all answer the
/// same bytes, and the wrong page's try burns the ticket it held.
#[test]
fn a_dead_or_misused_ticket_answers_what_no_ticket_answers() {
    let _home = HomeSandbox::new();
    let Some(served) = served_cfg(ctx(HOLD_A_FRAME), 7, Limits::DEFAULT) else {
        return;
    };
    let (_, absent) = present(&served, TICKET_ORIGIN, "clauth.terminal.v1");
    assert!(absent.starts_with("HTTP/1.1 401 "), "the head: {absent}");
    assert!(
        absent.contains("Connection: close\r\n"),
        "the head: {absent}"
    );

    let spent = minted(&served, TOKEN);
    let (client, head) = present(&served, TICKET_ORIGIN, &offer_of(&spent));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    drop(client);
    let (_, replay) = present(&served, TICKET_ORIGIN, &offer_of(&spent));
    assert_eq!(replay, absent);

    let burned = minted(&served, TOKEN);
    let (_, wrong_page) = present(&served, "https://b.example", &offer_of(&burned));
    assert_eq!(wrong_page, absent);
    let (_, right_page) = present(&served, TICKET_ORIGIN, &offer_of(&burned));
    assert_eq!(right_page, absent);
}

#[test]
fn a_frame_arrives_over_the_bridge() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = dir.path().join("argv");
    let script = format!(
        "printf '%s\\n' \"$@\" > {argv}; \
         printf '%s\\n' '{{\"type\":\"terminal.frame\",\"seq\":1}}'; \
         cat > /dev/null",
        argv = argv.display()
    );
    let Some(served) = served(ctx(&script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", VIEW_TOKEN)
        .expect("send upgrade");
    let head = client.read_head().expect("read 101");
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    assert!(head.contains(&format!("Sec-WebSocket-Accept: {WS_ACCEPT}")));
    let (opcode, payload) = client.recv_frame().expect("a frame");
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, br#"{"type":"terminal.frame","seq":1}"#);
    // The view tier attached the observe half at the pane's own geometry.
    wait_for_argv(
        &argv,
        &[
            "terminal", "session", "observe", "w9:p1", "--cols", "88", "--rows", "57",
        ],
    );
}

#[test]
fn a_closed_record_relays_then_the_socket_closes_cleanly() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let script = format!(
        "printf '%s\\n' '{{\"type\":\"terminal.closed\",\"reason\":\"terminal attach taken over\"}}'; \
         printf '%s\\n' \"$@\" > {argv}",
        argv = dir.path().join("argv").display()
    );
    let Some(served) = served(ctx(&script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", VIEW_TOKEN)
        .expect("send upgrade");
    assert_101(&mut client);
    let (opcode, payload) = client.recv_frame().expect("the closed record");
    assert_eq!(opcode, 0x1);
    assert!(payload.starts_with(br#"{"type":"terminal.closed""#));
    let (opcode, payload) = client.recv_frame().expect("the close frame");
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1000u16.to_be_bytes());
}

#[test]
fn a_view_only_token_cannot_send_input() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let input_log = dir.path().join("in");
    let script = format!(
        "printf '%s\\n' \"$@\" > {argv}; cat > {input_log}",
        argv = dir.path().join("argv").display(),
        input_log = input_log.display()
    );
    let Some(served) = served(ctx(&script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", VIEW_TOKEN)
        .expect("send upgrade");
    assert_101(&mut client);
    client
        .send_text(r#"{"type":"terminal.input","text":"let me in"}"#)
        .expect("send input");
    let (opcode, payload) = client.recv_frame().expect("the policy close");
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1008u16.to_be_bytes());
    assert!(String::from_utf8_lossy(&payload[2..]).contains("view tier"));
    // And nothing reached herdr's stdin.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !std::fs::read_to_string(&input_log)
            .unwrap_or_default()
            .contains("let me in")
    );
}

#[test]
fn a_control_attach_sends_the_panes_own_geometry() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = dir.path().join("argv");
    let pid_file = dir.path().join("pid");
    let script = format!(
        "echo $$ > {pid}; printf '%s\\n' \"$@\" > {argv}; printf '%s\\n' 'x'; cat > /dev/null",
        pid = pid_file.display(),
        argv = argv.display(),
    );
    let Some(served) = served(ctx(&script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", TOKEN)
        .expect("send upgrade");
    assert_101(&mut client);
    let _ = client.recv_frame().expect("the hold frame");
    wait_for_argv(
        &argv,
        &[
            "terminal",
            "session",
            "control",
            "w9:p1",
            "--takeover",
            "--cols",
            "88",
            "--rows",
            "57",
        ],
    );
    // The detach half of the geometry clause: the client leaving ends the
    // herdr child, which is what makes herdr revert the pane it had resized.
    // The observable is the fixture's own pid going away: teardown kills, and
    // a kill leaves no trap to run.
    drop(client);
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the fixture wrote its pid")
        .trim()
        .parse()
        .expect("a pid");
    let proc = Path::new("/proc").join(pid.to_string());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !proc.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the herdr child (pid {pid}) outlived the client disconnect");
}

// --- Who holds control, and contention ---------------------------------------

/// The `daemon.log` warnings among the lines captured so far.
fn contention_warnings(lines: &crate::logline::LogLines) -> usize {
    lines
        .snapshot()
        .iter()
        .filter(|line| line.contains("changed hands"))
        .count()
}

#[test]
fn three_handovers_in_the_window_warn_once_and_two_do_not() {
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let holders = super::TerminalHolders::new();
    let mut cursor = holders.cursor();
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);

    // Two handovers: driver, then phone, then driver again.
    holders.attach("w9:p1", DRIVER, at(0));
    holders.attach("w9:p1", "phone", at(10));
    holders.attach("w9:p1", DRIVER, at(20));
    assert!(holders.contentions_since(&mut cursor).is_empty());
    assert_eq!(contention_warnings(&lines), 0);

    // The third: one record naming both devices, one warning line.
    holders.attach("w9:p1", "phone", at(30));
    assert_eq!(
        holders.contentions_since(&mut cursor),
        vec![super::Contention {
            pane_id: "w9:p1".to_string(),
            devices: vec![DRIVER.to_string(), "phone".to_string()],
        }]
    );
    assert_eq!(contention_warnings(&lines), 1);

    // More handovers inside the same window add nothing: one per window.
    holders.attach("w9:p1", DRIVER, at(40));
    holders.attach("w9:p1", "phone", at(50));
    assert!(holders.contentions_since(&mut cursor).is_empty());
    assert_eq!(contention_warnings(&lines), 1);
}

#[test]
fn reconnects_and_slow_alternation_are_not_contention() {
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let holders = super::TerminalHolders::new();
    let mut cursor = holders.cursor();
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);

    // The same device reconnecting, many times, never changes hands.
    for i in 0..6 {
        holders.attach("w9:p1", DRIVER, at(i));
    }
    // A pane released at a lifetime close is free: the next attach takes it
    // from nobody.
    for (i, device) in [DRIVER, "phone", DRIVER, "phone"].into_iter().enumerate() {
        let token = holders.attach("w9:p2", device, at(10 + i as u64));
        assert_eq!(holders.release("w9:p2", token), None);
    }
    // Three handovers spread over two windows.
    let window = super::CONTENTION_WINDOW.as_secs();
    holders.attach("w9:p3", DRIVER, at(0));
    holders.attach("w9:p3", "phone", at(1));
    holders.attach("w9:p3", DRIVER, at(window));
    holders.attach("w9:p3", "phone", at(2 * window));
    // Three handovers whose first is exactly one window older than the third:
    // the window is half-open, so the first has aged out.
    holders.attach("w9:p4", DRIVER, at(0));
    holders.attach("w9:p4", "phone", at(1));
    holders.attach("w9:p4", DRIVER, at(2));
    holders.attach("w9:p4", "phone", at(1 + window));
    assert!(holders.contentions_since(&mut cursor).is_empty());
    assert_eq!(contention_warnings(&lines), 0);
}

#[test]
fn a_displaced_stream_learns_who_took_the_pane() {
    let holders = super::TerminalHolders::new();
    let now = Instant::now();
    let first = holders.attach("w9:p1", DRIVER, now);
    let second = holders.attach("w9:p1", "phone", now);
    // The displaced stream leaves the record alone and names the taker.
    assert_eq!(holders.release("w9:p1", first), Some("phone".to_string()));
    // The holder's own close clears it, so the next attach displaces nobody.
    assert_eq!(holders.release("w9:p1", second), None);
    assert_eq!(holders.release("w9:p1", second), None);
}

#[test]
fn a_warning_after_a_long_fight_counts_every_handover_in_its_window() {
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let holders = super::TerminalHolders::new();
    let t0 = Instant::now();
    let mut device = DRIVER;
    // A handover every 10 s from t=10 to t=330: the first warning lands at the
    // third (t=30), the second once the quiet period ends (t=330), when the 30
    // handovers from t=40 to t=330 sit in its window.
    holders.attach("w9:p1", device, t0);
    for secs in (10..=330).step_by(10) {
        device = if device == DRIVER { "phone" } else { DRIVER };
        holders.attach("w9:p1", device, t0 + Duration::from_secs(secs));
    }
    let warnings: Vec<String> = lines
        .snapshot()
        .into_iter()
        .filter(|line| line.contains("changed hands"))
        .collect();
    let line = |count: usize, first: &str, second: &str| {
        format!(
            "clauth api: warning: pane 'w9:p1' changed hands {count} times in 5 minutes \
             between devices '{first}', '{second}'; two clients are likely reconnecting \
             after being taken over"
        )
    };
    // The second window's first handover (t=40) is phone -> driver.
    assert_eq!(
        warnings,
        vec![line(3, DRIVER, "phone"), line(30, "phone", DRIVER)]
    );
}

#[test]
fn a_device_name_cannot_forge_a_line_through_the_warning() {
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let holders = super::TerminalHolders::new();
    let now = Instant::now();
    for device in [
        DRIVER,
        "ph\nclauth api: forged",
        DRIVER,
        "ph\nclauth api: forged",
    ] {
        holders.attach("w9:p1", device, now);
    }
    let warnings: Vec<String> = lines
        .snapshot()
        .into_iter()
        .filter(|line| line.contains("changed hands"))
        .collect();
    assert_eq!(
        warnings,
        vec![
            "clauth api: warning: pane 'w9:p1' changed hands 3 times in 5 minutes between \
             devices 'driver', 'ph clauth api: forged'; two clients are likely reconnecting \
             after being taken over"
                .to_string()
        ]
    );
}

/// A second control device, for the two-device takeover below.
const PHONE_TOKEN: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

#[test]
fn a_takeover_through_the_bridge_names_the_taker_and_frees_the_pane() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    // Longer than a log line keeps a name, so the record must hold the real
    // name and only the log line the shortened one.
    let phone = "p".repeat(130);
    let go = dir.path().join("go");
    // The first attach holds until the test says go, then ends the way herdr
    // ends a displaced controller; the second holds until its client leaves.
    let script = format!(
        "if mkdir {first} 2>/dev/null; then \
           printf '%s\\n' '{{\"type\":\"terminal.frame\",\"seq\":1}}'; \
           while [ ! -e {go} ]; do sleep 0.05; done; \
           printf '%s\\n' '{{\"type\":\"terminal.closed\",\"reason\":\"terminal attach taken over\"}}'; \
         else \
           printf '%s\\n' '{{\"type\":\"terminal.frame\",\"seq\":2}}'; \
           cat > /dev/null; \
         fi",
        first = dir.path().join("first").display(),
        go = go.display(),
    );
    let ctx = ctx(&script);
    devices::seed_for_tests(&phone, Tier::Control, PHONE_TOKEN).expect("seed the phone");
    // One server per client: a served connection blocks its server thread, and
    // both streams must be live at once. Both share the one context.
    let Some(driver_server) = served(Arc::clone(&ctx)) else {
        return;
    };
    let Some(phone_server) = served(Arc::clone(&ctx)) else {
        return;
    };

    let mut driver = WsClient::connect(driver_server.port, &driver_server.client_tls)
        .expect("connect the driver");
    driver
        .upgrade_request("/api/v1/panes/w9:p1/stream", TOKEN)
        .expect("send the driver's upgrade");
    assert_101(&mut driver);
    let _ = driver.recv_frame().expect("the driver's hold frame");

    let mut taker =
        WsClient::connect(phone_server.port, &phone_server.client_tls).expect("connect the phone");
    taker
        .upgrade_request("/api/v1/panes/w9:p1/stream", PHONE_TOKEN)
        .expect("send the phone's upgrade");
    assert_101(&mut taker);
    // The phone's first frame is relayed only after its stream recorded itself.
    let _ = taker.recv_frame().expect("the phone's hold frame");
    assert_eq!(
        ctx.terminal_holders.release("w9:p1", u64::MAX),
        Some(phone.clone()),
        "the record names the phone's whole name while it holds the pane"
    );

    std::fs::write(&go, b"").expect("let the driver's herdr end");
    let (opcode, payload) = driver.recv_frame().expect("the taken-over record");
    assert_eq!(opcode, 0x1);
    assert!(payload.starts_with(br#"{"type":"terminal.closed""#));
    let (opcode, _) = driver.recv_frame().expect("the driver's close frame");
    assert_eq!(opcode, 0x8);
    let driver_lines = captured_lines(&driver_server);
    // A log line keeps 128 characters of a name and marks the cut.
    let shortened = format!("{}...", "p".repeat(128));
    let want = format!(
        "clauth api: device '{DRIVER}' control stream on pane 'w9:p1' closed \
         (taken over by device '{shortened}')"
    );
    assert!(
        driver_lines.contains(&want),
        "the driver's close line names the taker: {driver_lines:?}"
    );

    drop(taker);
    let _ = captured_lines(&phone_server);
    // The phone's own close freed the pane: two handovers a window later are
    // two, not three counting a stale phone.
    let later = Instant::now() + 2 * super::CONTENTION_WINDOW;
    let mut cursor = ctx.terminal_holders.cursor();
    ctx.terminal_holders.attach("w9:p1", "x", later);
    ctx.terminal_holders.attach("w9:p1", "y", later);
    ctx.terminal_holders.attach("w9:p1", "x", later);
    assert!(
        ctx.terminal_holders
            .contentions_since(&mut cursor)
            .is_empty()
    );
}

#[test]
fn control_input_reaches_herdr_and_is_audited_without_its_bytes() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let input_log = dir.path().join("in");
    let script = format!(
        "printf '%s\\n' \"$@\" > {argv}; cat > {input_log}",
        argv = dir.path().join("argv").display(),
        input_log = input_log.display()
    );
    let Some(served) = served(ctx(&script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", TOKEN)
        .expect("send upgrade");
    assert_101(&mut client);
    client
        .send_text(r#"{"type":"terminal.input","text":"sekrit-paste"}"#)
        .expect("send input");
    client
        .send_text(r#"{"type":"terminal.resize","cols":40,"rows":10}"#)
        .expect("send resize");
    wait_for_file_contains(&input_log, "sekrit-paste");
    wait_for_file_contains(&input_log, r#""cols":40"#);
    // The capture only lands when the connection ends, so close it first.
    drop(client);
    let lines = captured_lines(&served);
    let input_line = lines
        .iter()
        .find(|line| line.contains("sent terminal.input to pane 'w9:p1'"))
        .expect("the input audit line");
    assert!(
        !input_line.contains("sekrit"),
        "the audit never carries bytes"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("resized pane 'w9:p1' to 40x10 (phone-shaped, deliberate)")),
        "the phone-shaped resize is named deliberate: {lines:?}"
    );
}

#[test]
fn frames_pipelined_behind_the_handshake_arrive() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let input_log = dir.path().join("in");
    let script = format!(
        "printf '%s\\n' \"$@\" > {argv}; cat > {input_log}",
        argv = dir.path().join("argv").display(),
        input_log = input_log.display()
    );
    let Some(served) = served(ctx(&script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    // The upgrade and the first input frame in ONE write: the bridge must not
    // lose bytes the request reader had already buffered.
    let mut pipelined = format!(
        "GET /api/v1/panes/w9:p1/stream HTTP/1.1\r\n\
         Host: {}\r\n\
         Authorization: Bearer {TOKEN}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        server_tests::SERVER_NAME
    )
    .into_bytes();
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let payload = br#"{"type":"terminal.input","text":"early"}"#;
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    for (i, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[i % 4]);
    }
    pipelined.extend_from_slice(&frame);
    client.send(&pipelined).expect("send handshake + frame");
    let head = client.read_head().expect("read 101");
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    wait_for_file_contains(&input_log, "early");
}

#[test]
fn a_forged_device_name_cannot_write_audit_lines() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let input_log = dir.path().join("in");
    let script = format!(
        "printf '%s\\n' \"$@\" > {argv}; cat > {input_log}",
        argv = dir.path().join("argv").display(),
        input_log = input_log.display()
    );
    // Seeded straight into the store, the way a hand edit or a newer build
    // would land one: the CLI's name parser is a UI gate, not a store
    // invariant, so the audit lines must hold for any bytes the store holds.
    devices::seed_for_tests("forge\nclauth api: paired device 'x'", Tier::Control, TOKEN)
        .expect("seed the forged device");
    let ctx = ApiContext::new(
        config(),
        status_path(),
        None,
        None,
        Box::new(|args, _deadline| match args {
            ["api", "snapshot"] => snapshot_out(),
            _ => HerdrProbeOut::Ran(None),
        }),
        Arc::new(|| None),
        sh_spawn(&script),
        super::super::create::unspawnable_session(),
        #[cfg(unix)]
        None,
    );
    let Some(served) = served(ctx) else { return };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", TOKEN)
        .expect("send upgrade");
    assert_101(&mut client);
    client
        .send_text(r#"{"type":"terminal.input","text":"x"}"#)
        .expect("send input");
    wait_for_file_contains(&input_log, r#""text":"x""#);
    drop(client);
    // The invariant is the entry boundary, not the substring: a sanitized
    // name rides inside one prefixed line, while the raw newline forged a
    // SECOND line that reads as its own audit entry.
    for line in captured_lines(&served) {
        // Capture keeps one message whole, raw newline and all, so the forged
        // second line shows up as a newline inside the entry.
        assert!(
            !line.contains('\n'),
            "a device name broke an audit line: {line:?}"
        );
        assert!(
            !line.starts_with("clauth api: paired device"),
            "a forged standalone audit line escaped: {line:?}"
        );
    }
}

/// One client frame with arbitrary header bits: `[b0, b1, extended length,
/// mask, payload]` when the mask bit is set, the unmasked shape otherwise.
fn raw_frame(b0: u8, b1: u8, extended: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![b0, b1];
    frame.extend_from_slice(extended);
    if b1 & 0x80 == 0 {
        frame.extend_from_slice(payload);
        return frame;
    }
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    frame.extend_from_slice(&mask);
    for (i, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[i % 4]);
    }
    frame
}

/// Open a control-tier stream and hand back the connected client.
fn control_stream(client_tls: &Arc<rustls::ClientConfig>, port: u16) -> WsClient {
    let mut client = WsClient::connect(port, client_tls).expect("connect");
    client
        .upgrade_request("/api/v1/panes/w9:p1/stream", TOKEN)
        .expect("send upgrade");
    assert_101(&mut client);
    client
}

#[test]
fn malformed_client_frames_close_with_protocol_error() {
    let _home = HomeSandbox::new();
    let script = "printf '%s\\n' \"$@\" > /dev/null; cat > /dev/null";
    // (first byte, first payload byte, any extended header bytes, expected
    // close code). Every case is a complete frame the parser can reject on its
    // own terms.
    /// One refusal case: the two header bytes, the extended-length bytes, the
    /// payload, and the close code it must earn.
    struct Bad {
        b0: u8,
        b1: u8,
        extended: &'static [u8],
        payload: &'static [u8],
        want: u16,
    }
    /// The over-ceiling case's declared length, as the 127-coded header bytes.
    const TWO_MIB: &[u8] = &(2 * 1024 * 1024u64).to_be_bytes();
    let cases = [
        // unmasked text
        Bad {
            b0: 0x81,
            b1: 0x03,
            extended: &[],
            payload: b"abc",
            want: 1002,
        },
        // rsv bits set
        Bad {
            b0: 0xC1,
            b1: 0x81,
            extended: &[],
            payload: &[],
            want: 1002,
        },
        // a fragmented text frame
        Bad {
            b0: 0x01,
            b1: 0x81,
            extended: &[],
            payload: &[],
            want: 1002,
        },
        // a continuation frame with no fragment to continue
        Bad {
            b0: 0x80,
            b1: 0x81,
            extended: &[],
            payload: &[0x00],
            want: 1002,
        },
        // a ping claiming an extended length
        Bad {
            b0: 0x89,
            b1: 0xFE,
            extended: &[0x00, 0x7E],
            payload: &[],
            want: 1002,
        },
        // a non-minimal extended text length (126 encoding the value 125)
        Bad {
            b0: 0x81,
            b1: 0xFE,
            extended: &[0x00, 0x7D],
            payload: &[],
            want: 1002,
        },
        // an unknown opcode
        Bad {
            b0: 0x83,
            b1: 0x81,
            extended: &[],
            payload: &[],
            want: 1002,
        },
        // a binary frame
        Bad {
            b0: 0x82,
            b1: 0x81,
            extended: &[],
            payload: &[0x00],
            want: 1002,
        },
        // a text frame whose payload is not UTF-8
        Bad {
            b0: 0x81,
            b1: 0x81,
            extended: &[],
            payload: &[0xff],
            want: 1007,
        },
        // a close whose payload is one byte, so no code
        Bad {
            b0: 0x88,
            b1: 0x81,
            extended: &[],
            payload: &[0x00],
            want: 1002,
        },
        // a declared length over the frame ceiling
        Bad {
            b0: 0x81,
            b1: 0xFF,
            extended: TWO_MIB,
            payload: &[],
            want: 1009,
        },
    ];
    let Some(served) = served_cfg(ctx(script), cases.len(), Limits::DEFAULT) else {
        return;
    };
    for (i, case) in cases.iter().enumerate() {
        let mut client = control_stream(&served.client_tls, served.port);
        let frame = raw_frame(case.b0, case.b1, case.extended, case.payload);
        client.send(&frame).expect("send the bad frame");
        let (opcode, payload) = client.recv_frame().expect("a close");
        assert_eq!(opcode, 0x8, "case {i} closes");
        assert_eq!(
            &payload[..2],
            &case.want.to_be_bytes(),
            "case {i} (b0={:#x}, b1={:#x}) closes with {}",
            case.b0,
            case.b1,
            case.want
        );
    }
}

#[test]
fn a_ping_is_answered_with_a_pong() {
    let _home = HomeSandbox::new();
    let script = "printf '%s\n' \"$@\" > /dev/null; cat > /dev/null";
    let Some(served) = served(ctx(script)) else {
        return;
    };
    let mut client = control_stream(&served.client_tls, served.port);
    client
        .send(&raw_frame(0x89, 0x82, &[], b"hi"))
        .expect("send ping");
    let (opcode, payload) = client.recv_frame().expect("a pong");
    assert_eq!(opcode, 0xA);
    assert_eq!(payload, b"hi");
}

#[test]
fn a_stream_ends_at_the_connection_lifetime() {
    let _home = HomeSandbox::new();
    let script = "printf '%s\n' \"$@\" > /dev/null; cat > /dev/null";
    let limits = Limits {
        io_timeout: Duration::from_secs(10),
        lifetime: Duration::from_millis(1200),
        max_requests: 100,
    };
    let Some(served) = served_cfg(ctx(script), 1, limits) else {
        return;
    };
    let mut client = control_stream(&served.client_tls, served.port);
    // Nothing is sent; the only thing that can end this stream is the
    // lifetime. No slot is ever held forever (owner ruling 2026-09-15, row 6).
    let started = Instant::now();
    let (opcode, payload) = client.recv_frame().expect("the lifetime close");
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1000u16.to_be_bytes());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the lifetime ended the stream, not the client timeout"
    );
}

/// A pane printing without pause behind a client that reads slowly: the client
/// leaves its socket unread for the first second, then reads one 3 MB frame
/// every 200 ms, so the socket stays full and the drain stays inside its frame
/// loop instead of returning to the loop top. A frame that big empties the few
/// buffered rows ahead of the close at the client's own 15 MB/s, and the
/// 16-slot queue absorbs any pause of the relay. The stream can end only at
/// the drain's own between-frames check, never at a pass boundary that read the
/// queue empty.
#[test]
fn a_saturated_stream_ends_at_the_lifetime() {
    let _home = HomeSandbox::new();
    let pane_dir = tempfile::tempdir().expect("tempdir");
    let row = pane_dir.path().join("row");
    std::fs::write(&row, "x".repeat(3_000_000)).expect("write the pane's row");
    let flood = format!("while :; do cat '{}'; echo; done", row.display());
    let limits = Limits {
        io_timeout: Duration::from_secs(10),
        lifetime: Duration::from_secs(2),
        max_requests: 100,
    };
    let Some(served) = served_cfg(ctx(&flood), 1, limits) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request(STREAM_PATH, VIEW_TOKEN)
        .expect("upgrade");
    assert_101(&mut client);
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    loop {
        // Below the row's own 5 s check, so a close from there cannot pass for
        // the lifetime's, and above the 2 s lifetime plus the queued rows'
        // drain and one paced read.
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the stream outlived its 2 s lifetime"
        );
        match client.recv_frame() {
            Ok((0x8, payload)) => {
                assert_eq!(&payload[..2], &1000u16.to_be_bytes());
                break;
            }
            Ok((0x1, _)) => std::thread::sleep(Duration::from_millis(200)),
            Ok((opcode, _)) => panic!("an unexpected opcode {opcode}"),
            Err(e) => panic!("the stream stayed open: {e}"),
        }
    }
}

// --- The ticket on the wire, and an open stream's revoke -----------------------

/// A revoke closes a stream within 5 s plus one 200 ms read slice; the rest of
/// this bound is scheduling room on a loaded box.
const REVOKE_BOUND: Duration = Duration::from_secs(7);
const STREAM_LINE_401: &str = "GET /api/v1/panes/w9:p1/stream -> 401 origin=unlisted";

/// `[serve] app_origins`, as a hand edit leaves it.
fn list_origin(origin: &str) {
    std::fs::write(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("profiles.toml"),
        format!("profiles = []\n\n[serve]\napp_origins = [{origin:?}]\n"),
    )
    .expect("write profiles.toml");
}

/// An upgrade with these extra header lines and no `Authorization`.
fn upgrade_with(served: &Served, path: &str, lines: &[String]) -> (WsClient, String) {
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    let extra: String = lines.iter().map(|line| format!("{line}\r\n")).collect();
    client
        .send(
            format!(
                "GET {path} HTTP/1.1\r\n\
                 Host: {}\r\n\
                 {extra}\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Key: {WS_KEY}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the upgrade");
    let head = client.read_head().expect("read the upgrade's head");
    (client, head)
}

/// Every byte of a closed answer: its head, then the body to the close.
fn whole_answer((mut client, head): (WsClient, String)) -> Vec<u8> {
    let mut bytes = head.into_bytes();
    bytes.append(&mut client.buf);
    let mut chunk = [0u8; 512];
    loop {
        match client.stream.read(&mut chunk) {
            Ok(0) => return bytes,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) => panic!("the answer did not close: {e}"),
        }
    }
}

fn offer_line(ticket: &str) -> String {
    format!("Sec-WebSocket-Protocol: {}", offer_of(ticket))
}

fn origin_line(origin: &str) -> String {
    format!("Origin: {origin}")
}

/// The context [`ctx`] builds with a clock the test moves.
fn ctx_with_clock(script: &str) -> (Arc<ApiContext>, Arc<std::sync::Mutex<Stamp>>) {
    let mut ctx = ctx(script);
    let clock = Arc::new(std::sync::Mutex::new(Stamp::now()));
    let reader = Arc::clone(&clock);
    Arc::get_mut(&mut ctx).expect("a fresh context").tickets =
        Tickets::new(Box::new(move || *reader.lock().expect("the clock")));
    (ctx, clock)
}

/// Read until the close frame, past any text frames, and return its code.
fn close_code(client: &mut WsClient) -> u16 {
    close_frame(client).0
}

/// [`close_code`] with the close frame's reason beside its code.
fn close_frame(client: &mut WsClient) -> (u16, String) {
    loop {
        match client.recv_frame() {
            Ok((0x8, payload)) => {
                return (
                    u16::from_be_bytes([payload[0], payload[1]]),
                    String::from_utf8_lossy(&payload[2..]).into_owned(),
                );
            }
            Ok((0x1, _)) => {}
            Ok((opcode, _)) => panic!("an unexpected opcode {opcode}"),
            Err(e) => panic!("the stream stayed open: {e}"),
        }
    }
}

#[test]
fn a_listed_origin_can_read_its_mint() {
    let _home = HomeSandbox::new();
    let context = ctx(HOLD_A_FRAME);
    list_origin(TICKET_ORIGIN);
    let Some(served) = served(context) else {
        return;
    };
    let (head, _) = mint(&served, VIEW_TOKEN, Some(TICKET_ORIGIN));
    assert!(head.starts_with("HTTP/1.1 201 "), "the head: {head}");
    assert!(
        head.contains(&format!("Access-Control-Allow-Origin: {TICKET_ORIGIN}\r\n")),
        "the head: {head}"
    );
}

/// The wire half of "an `Origin` no page can be bound to": the parser is what
/// turns these two shapes into an unusable header.
#[test]
fn two_origin_lines_or_bytes_that_are_not_text_mint_nothing() {
    let _home = HomeSandbox::new();
    let context = ctx(HOLD_A_FRAME);
    let table = Arc::clone(&context);
    let Some(served) = served_cfg(context, 2, Limits::DEFAULT) else {
        return;
    };
    for origins in [
        b"Origin: https://a.example\r\nOrigin: https://a.example\r\n".to_vec(),
        b"Origin: https://\xff.example\r\n".to_vec(),
    ] {
        let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
        let mut request = format!(
            "POST {TICKET_PATH} HTTP/1.1\r\n\
             Host: {}\r\n\
             Authorization: Bearer {TOKEN}\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n",
            server_tests::SERVER_NAME
        )
        .into_bytes();
        request.extend_from_slice(&origins);
        request.extend_from_slice(b"\r\n");
        client.send(&request).expect("send the mint");
        let head = client.read_head().expect("read the mint's head");
        let answer = String::from_utf8(whole_answer((client, head))).expect("text");
        assert!(answer.starts_with("HTTP/1.1 403 "), "the answer: {answer}");
        assert!(
            answer.contains("origin_not_allowed"),
            "the answer: {answer}"
        );
    }
    assert_eq!(table.tickets.held(), 0);
}

#[test]
fn an_offer_on_two_header_lines_attaches() {
    let _home = HomeSandbox::new();
    let Some(served) = served_cfg(ctx(HOLD_A_FRAME), 2, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, VIEW_TOKEN);
    let (mut client, head) = upgrade_with(
        &served,
        STREAM_PATH,
        &[
            origin_line(TICKET_ORIGIN),
            "Sec-WebSocket-Protocol: clauth.terminal.v1".to_string(),
            format!("Sec-WebSocket-Protocol: clauth.ticket.{ticket}"),
        ],
    );
    assert_eq!(
        head,
        format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {WS_ACCEPT}\r\n\
             Sec-WebSocket-Protocol: clauth.terminal.v1\r\n\
             \r\n"
        )
    );
    let (opcode, _) = client.recv_frame().expect("a frame");
    assert_eq!(opcode, 0x1);
}

/// Absent, malformed, unknown, spent, expired, wrong pane, wrong `Origin` and
/// revoked: one answer, byte for byte, closed, and `-` on each line.
#[test]
fn eight_dead_presentations_answer_one_set_of_bytes() {
    let _home = HomeSandbox::new();
    let (context, clock) = ctx_with_clock(HOLD_A_FRAME);
    let Some(served) = served_cfg(context, 14, Limits::DEFAULT) else {
        return;
    };
    let origin = origin_line(TICKET_ORIGIN);
    let at = |path: &str, origin: String, offer: String| {
        whole_answer(upgrade_with(&served, path, &[origin, offer]))
    };

    let absent = whole_answer(upgrade_with(
        &served,
        STREAM_PATH,
        std::slice::from_ref(&origin),
    ));
    let text = String::from_utf8_lossy(&absent);
    assert!(text.starts_with("HTTP/1.1 401 "), "the answer: {text}");
    assert!(text.contains("Connection: close\r\n"), "the answer: {text}");
    assert!(
        text.ends_with(r#"{"ok":false,"error":"unauthorized"}"#),
        "the answer: {text}"
    );

    let malformed = at(
        STREAM_PATH,
        origin.clone(),
        "Sec-WebSocket-Protocol: clauth.terminal.v1, clauth.ticket.nope".to_string(),
    );
    assert_eq!(malformed, absent, "malformed");

    let unknown = at(STREAM_PATH, origin.clone(), offer_line(&"a".repeat(64)));
    assert_eq!(unknown, absent, "unknown");

    let ticket = minted(&served, TOKEN);
    let (client, head) = upgrade_with(&served, STREAM_PATH, &[origin.clone(), offer_line(&ticket)]);
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    drop(client);
    let spent = at(STREAM_PATH, origin.clone(), offer_line(&ticket));
    assert_eq!(spent, absent, "spent");

    let ticket = minted(&served, TOKEN);
    {
        let mut clock = clock.lock().expect("the clock");
        clock.mono += Duration::from_secs(30);
        clock.wall += Duration::from_secs(30);
    }
    let expired = at(STREAM_PATH, origin.clone(), offer_line(&ticket));
    assert_eq!(expired, absent, "expired");

    let ticket = minted(&served, TOKEN);
    let wrong_pane = at(
        "/api/v1/panes/w9:p2/stream",
        origin.clone(),
        offer_line(&ticket),
    );
    assert_eq!(wrong_pane, absent, "wrong pane");

    let ticket = minted(&served, TOKEN);
    let wrong_origin = at(
        STREAM_PATH,
        origin_line("https://b.example"),
        offer_line(&ticket),
    );
    assert_eq!(wrong_origin, absent, "wrong origin");

    let ticket = minted(&served, VIEW_TOKEN);
    devices::revoke(VIEWER).expect("revoke");
    let revoked = at(STREAM_PATH, origin.clone(), offer_line(&ticket));
    assert_eq!(revoked, absent, "revoked");

    let lines = captured_lines(&served);
    let dashes = lines
        .iter()
        .filter(|line| line.ends_with(&format!(" - {STREAM_LINE_401}")))
        .count();
    assert_eq!(dashes, 7, "the lines: {lines:?}");
    assert_eq!(
        lines
            .iter()
            .filter(
                |line| line.ends_with(" - GET /api/v1/panes/w9:p2/stream -> 401 origin=unlisted")
            )
            .count(),
        1,
        "the lines: {lines:?}"
    );
}

#[test]
fn a_ticket_attach_names_its_device_its_origin_and_its_credential() {
    let _home = HomeSandbox::new();
    let context = ctx(HOLD_A_FRAME);
    list_origin(TICKET_ORIGIN);
    let Some(served) = served_cfg(context, 3, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, TOKEN);
    let (client, head) = present(&served, TICKET_ORIGIN, &offer_of(&ticket));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    drop(client);
    let mut bearer = WsClient::connect(served.port, &served.client_tls).expect("connect");
    bearer.upgrade_request(STREAM_PATH, TOKEN).expect("upgrade");
    assert_101(&mut bearer);
    drop(bearer);

    let lines = captured_lines(&served);
    let of = |suffix: &str| lines.iter().filter(|line| line.ends_with(suffix)).count();
    assert_eq!(
        of(&format!(
            " {DRIVER} GET {STREAM_PATH} -> 101 origin={TICKET_ORIGIN} auth=ticket"
        )),
        1,
        "the lines: {lines:?}"
    );
    assert_eq!(
        of(&format!(" {DRIVER} GET {STREAM_PATH} -> 101")),
        1,
        "the bearer's line carries no auth field: {lines:?}"
    );
    assert_eq!(
        of(&format!(
            "clauth api: device '{DRIVER}' opened control on pane 'w9:p1'"
        )),
        2,
        "the lines: {lines:?}"
    );
}

/// The bearer upgrade's head, as the code before the ticket wrote it, with a
/// live ticket on the request and without one.
#[test]
fn a_bearer_upgrade_writes_todays_head() {
    let _home = HomeSandbox::new();
    let context = ctx(HOLD_A_FRAME);
    let table = Arc::clone(&context);
    let Some(served) = served_cfg(context, 3, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, VIEW_TOKEN);
    let want = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {WS_ACCEPT}\r\n\
         \r\n"
    );
    let mut plain = WsClient::connect(served.port, &served.client_tls).expect("connect");
    plain.upgrade_request(STREAM_PATH, TOKEN).expect("upgrade");
    assert_eq!(plain.read_head().expect("the head"), want);
    drop(plain);

    let (mut both, head) = upgrade_with(
        &served,
        STREAM_PATH,
        &[
            format!("Authorization: Bearer {TOKEN}"),
            origin_line(TICKET_ORIGIN),
            offer_line(&ticket),
        ],
    );
    assert_eq!(head, want);
    let (opcode, _) = both.recv_frame().expect("a frame");
    assert_eq!(opcode, 0x1);
    assert_eq!(table.tickets.held(), 1, "the bearer left the ticket live");
}

#[test]
fn a_view_tickets_input_closes_1008() {
    let _home = HomeSandbox::new();
    let Some(served) = served_cfg(ctx(HOLD_A_FRAME), 2, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, VIEW_TOKEN);
    let (mut client, head) = present(&served, TICKET_ORIGIN, &offer_of(&ticket));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    client
        .send_text(r#"{"type":"terminal.input","text":"x"}"#)
        .expect("send input");
    assert_eq!(close_code(&mut client), 1008);
}

#[test]
fn a_control_tickets_attach_takes_over() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = dir.path().join("argv");
    let script = format!("printf '%s\\n' \"$@\" > {}; {HOLD_A_FRAME}", argv.display());
    let Some(served) = served_cfg(ctx(&script), 2, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, TOKEN);
    let (mut client, head) = present(&served, TICKET_ORIGIN, &offer_of(&ticket));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    client.recv_frame().expect("a frame");
    wait_for_argv(
        &argv,
        &[
            "terminal",
            "session",
            "control",
            "w9:p1",
            "--takeover",
            "--cols",
            "88",
            "--rows",
            "57",
        ],
    );
}

/// The row at the upgrade decides the mode, never the row at the mint: a tier
/// moving under one pairing is a hand edit, and the read honours it.
#[test]
fn a_tier_edited_after_the_mint_decides_the_mode() {
    let _home = HomeSandbox::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = dir.path().join("argv");
    let script = format!("printf '%s\\n' \"$@\" > {}; {HOLD_A_FRAME}", argv.display());
    let Some(served) = served_cfg(ctx(&script), 2, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, VIEW_TOKEN);
    let path = crate::profile::clauth_dir()
        .expect("clauth dir")
        .join("devices.json");
    let text = std::fs::read_to_string(&path).expect("read the list");
    assert!(text.contains("\"view\""), "the fixture holds the tier");
    std::fs::write(&path, text.replace("\"view\"", "\"control\"")).expect("edit the list");
    let (mut client, head) = present(&served, TICKET_ORIGIN, &offer_of(&ticket));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    client.recv_frame().expect("a frame");
    wait_for_argv(
        &argv,
        &[
            "terminal",
            "session",
            "control",
            "w9:p1",
            "--takeover",
            "--cols",
            "88",
            "--rows",
            "57",
        ],
    );
}

#[test]
fn no_line_holds_a_ticket() {
    let _home = HomeSandbox::new();
    let Some(served) = served_cfg(ctx(HOLD_A_FRAME), 6, Limits::DEFAULT) else {
        return;
    };
    let attached = minted(&served, TOKEN);
    let (client, head) = present(&served, TICKET_ORIGIN, &offer_of(&attached));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    drop(client);
    let wrong_page = minted(&served, TOKEN);
    let (_, head) = present(&served, "https://b.example", &offer_of(&wrong_page));
    assert!(head.starts_with("HTTP/1.1 401 "), "the head: {head}");
    let unmatched = "c".repeat(64);
    let (_, head) = present(&served, TICKET_ORIGIN, &offer_of(&unmatched));
    assert!(head.starts_with("HTTP/1.1 401 "), "the head: {head}");
    let (_, head) = present(&served, TICKET_ORIGIN, &offer_of(&attached));
    assert!(head.starts_with("HTTP/1.1 401 "), "the head: {head}");

    let lines = captured_lines(&served);
    assert!(
        lines.iter().any(|line| line.contains(" -> 101 ")),
        "the capture holds the attach: {lines:?}"
    );
    for canary in [&attached, &wrong_page, &unmatched] {
        assert!(
            !lines.iter().any(|line| line.contains(canary.as_str())),
            "a line holds a ticket"
        );
    }
}

#[test]
fn a_revoke_closes_an_open_ticket_stream() {
    let _home = HomeSandbox::new();
    // Reset so the silence asserted below is this test's, never a latch an
    // earlier test flipped; every flipper holds a sandbox.
    crate::daemon::api::routes::READ_FAILED_NOTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let Some(served) = served_cfg(ctx(HOLD_A_FRAME), 2, Limits::DEFAULT) else {
        return;
    };
    let ticket = minted(&served, TOKEN);
    let (mut client, head) = present(&served, TICKET_ORIGIN, &offer_of(&ticket));
    assert!(head.starts_with("HTTP/1.1 101 "), "the head: {head}");
    client.recv_frame().expect("a frame");
    let revoked = Instant::now();
    devices::revoke(DRIVER).expect("revoke");
    assert_eq!(close_code(&mut client), 1008);
    assert!(revoked.elapsed() < REVOKE_BOUND, "{:?}", revoked.elapsed());
    drop(client);
    let lines = captured_lines(&served);
    let want = "clauth api: device 'driver' control stream on pane 'w9:p1' closed (device no longer paired)";
    assert_eq!(
        lines.iter().filter(|line| line.as_str() == want).count(),
        1,
        "the revoke's one close line: {lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("clauth api: refusing every request until")),
        "a revoke is no unreadable list: {lines:?}"
    );
}

#[test]
fn a_revoke_closes_an_open_bearer_stream() {
    let _home = HomeSandbox::new();
    let Some(served) = served(ctx(HOLD_A_FRAME)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request(STREAM_PATH, VIEW_TOKEN)
        .expect("upgrade");
    assert_101(&mut client);
    client.recv_frame().expect("a frame");
    let revoked = Instant::now();
    devices::revoke(VIEWER).expect("revoke");
    assert_eq!(close_code(&mut client), 1008);
    assert!(revoked.elapsed() < REVOKE_BOUND, "{:?}", revoked.elapsed());
}

/// A pane printing without pause behind a client that reads slower than it
/// prints: the close lands behind a full socket, and 60 KB frames at 5 ms each
/// read those few MB back inside the bound.
#[test]
fn a_revoke_closes_a_stream_whose_pane_outruns_its_client() {
    let _home = HomeSandbox::new();
    let flood = "line=$(head -c 60000 /dev/zero | tr '\\0' x); yes \"$line\"";
    let Some(served) = served(ctx(flood)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request(STREAM_PATH, VIEW_TOKEN)
        .expect("upgrade");
    assert_101(&mut client);
    let revoked = Instant::now();
    devices::revoke(VIEWER).expect("revoke");
    let code = loop {
        match client.recv_frame() {
            Ok((0x8, payload)) => break u16::from_be_bytes([payload[0], payload[1]]),
            Ok((0x1, _)) => std::thread::sleep(Duration::from_millis(5)),
            Ok((opcode, _)) => panic!("an unexpected opcode {opcode}"),
            Err(e) => panic!("the stream stayed open: {e}"),
        }
    };
    assert_eq!(code, 1008);
    assert!(revoked.elapsed() < REVOKE_BOUND, "{:?}", revoked.elapsed());
}

/// An unreadable list closes the stream as a server fault, never as an unpair
/// the user did not make: `1011 "device list unreadable"`, a close line saying
/// so, and the read error itself through the once-per-process refusal line.
#[test]
fn a_device_list_that_stops_reading_closes_an_open_stream() {
    let _home = HomeSandbox::new();
    crate::daemon::api::routes::READ_FAILED_NOTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let Some(served) = served(ctx(HOLD_A_FRAME)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request(STREAM_PATH, VIEW_TOKEN)
        .expect("upgrade");
    assert_101(&mut client);
    client.recv_frame().expect("a frame");
    std::fs::write(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("devices.json"),
        "not a list",
    )
    .expect("corrupt the list");
    assert_eq!(
        close_frame(&mut client),
        (1011, "device list unreadable".to_string())
    );
    drop(client);
    let lines = captured_lines(&served);
    let want = "clauth api: device 'viewer' observe stream on pane 'w9:p1' closed (device list unreadable)";
    assert_eq!(
        lines.iter().filter(|line| line.as_str() == want).count(),
        1,
        "the one close line: {lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line
                .starts_with("clauth api: refusing every request until the device list reads: "))
            .count(),
        1,
        "the read error's one line: {lines:?}"
    );
}

/// The `/events` twin: the stream ends, and the read error reaches the log
/// through the same once-per-process line.
#[test]
fn a_device_list_that_stops_reading_ends_an_events_stream_and_logs_why() {
    let _home = HomeSandbox::new();
    crate::daemon::api::routes::READ_FAILED_NOTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let Some(served) = served(ctx(":")) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .send(
            format!(
                "GET /api/v1/events HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Authorization: Bearer {VIEW_TOKEN}\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the request");
    let head = client.read_head().expect("the head");
    assert!(head.starts_with("HTTP/1.1 200 "), "the head: {head}");
    let damaged = Instant::now();
    std::fs::write(
        crate::profile::clauth_dir()
            .expect("clauth dir")
            .join("devices.json"),
        "not a list",
    )
    .expect("corrupt the list");
    let mut chunk = [0u8; 512];
    loop {
        match client.stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("the stream stayed open: {e}"),
        }
    }
    assert!(damaged.elapsed() < REVOKE_BOUND, "{:?}", damaged.elapsed());
    drop(client);
    let lines = captured_lines(&served);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line
                .starts_with("clauth api: refusing every request until the device list reads: "))
            .count(),
        1,
        "the read error's one line: {lines:?}"
    );
}

/// The stream lives through its row checks while its device stays paired: a
/// second frame arrives after the first check would have closed it.
#[test]
fn a_paired_devices_stream_outlives_the_row_check() {
    let _home = HomeSandbox::new();
    let script =
        "printf '%s\\n' '{\"seq\":1}'; sleep 6; printf '%s\\n' '{\"seq\":2}'; cat > /dev/null";
    let Some(served) = served(ctx(script)) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .upgrade_request(STREAM_PATH, VIEW_TOKEN)
        .expect("upgrade");
    assert_101(&mut client);
    assert_eq!(client.recv_frame().expect("a frame").1, br#"{"seq":1}"#);
    assert_eq!(client.recv_frame().expect("a frame").1, br#"{"seq":2}"#);
}

#[test]
fn a_revoke_closes_an_open_events_stream() {
    let _home = HomeSandbox::new();
    let Some(served) = served(ctx(":")) else {
        return;
    };
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .send(
            format!(
                "GET /api/v1/events HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Authorization: Bearer {VIEW_TOKEN}\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the request");
    let head = client.read_head().expect("the head");
    assert!(head.starts_with("HTTP/1.1 200 "), "the head: {head}");
    let revoked = Instant::now();
    devices::revoke(VIEWER).expect("revoke");
    let mut chunk = [0u8; 512];
    loop {
        match client.stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("the stream stayed open: {e}"),
        }
    }
    assert!(revoked.elapsed() < REVOKE_BOUND, "{:?}", revoked.elapsed());
}

/// A long poll whose device is revoked mid-wait answers 401 on the wire and
/// closes its connection, like every 401 no device earned: a revoked device
/// keeps no slot.
#[test]
fn a_long_poll_revoked_mid_wait_closes_its_connection() {
    let _home = HomeSandbox::new();
    let Some(served) = served(ctx(":")) else {
        return;
    };
    std::fs::write(status_path(), r#"{"schema":1,"active_profile":"alpha"}"#).expect("feed");
    let (_, tag) =
        crate::daemon::api::routes::read_feed_tagged(&status_path()).expect("the feed's tag");
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .send(
            format!(
                "GET /api/v1/status?wait=10 HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Authorization: Bearer {VIEW_TOKEN}\r\n\
                 If-None-Match: {tag}\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the request");
    let path = status_path();
    let revoker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        devices::revoke(VIEWER).expect("revoke");
        std::fs::write(&path, r#"{"schema":1,"active_profile":"beta"}"#).expect("republish");
    });
    let head = client.read_head().expect("the head");
    revoker.join().expect("revoker thread");
    assert!(head.starts_with("HTTP/1.1 401 "), "the head: {head}");
    assert!(
        head.contains("\r\nConnection: close\r\n"),
        "a revoked device keeps no connection: {head}"
    );
    drop(client);
    // Only the mid-wait 401 names its device: a 401 at the request's start
    // logs `-`, so this line proves the request authenticated before the
    // revoke and the close is the waited answer's.
    let lines = captured_lines(&served);
    assert_eq!(
        lines
            .iter()
            .filter(|line| {
                line.contains(&format!(" {VIEWER} GET /api/v1/status")) && line.ends_with(" -> 401")
            })
            .count(),
        1,
        "the waited 401's access line: {lines:?}"
    );
}

/// The twin: with its device paired, an `/events` stream runs past its first
/// row check to the 6 s lifetime this test gives it.
#[test]
fn a_paired_devices_events_stream_outlives_the_row_check() {
    let _home = HomeSandbox::new();
    let limits = Limits {
        io_timeout: Duration::from_secs(10),
        lifetime: Duration::from_secs(6),
        max_requests: 100,
    };
    let Some(served) = served_cfg(ctx(":"), 1, limits) else {
        return;
    };
    // Stamped before the connect, so the server's own 6 s clock starts after
    // this instant: a correct stream necessarily reads at least 6 s.
    let opened = Instant::now();
    let mut client = WsClient::connect(served.port, &served.client_tls).expect("connect");
    client
        .send(
            format!(
                "GET /api/v1/events HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Authorization: Bearer {VIEW_TOKEN}\r\n\
                 \r\n",
                server_tests::SERVER_NAME
            )
            .as_bytes(),
        )
        .expect("send the request");
    client.read_head().expect("the head");
    let mut chunk = [0u8; 512];
    loop {
        match client.stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("the stream outlived its lifetime: {e}"),
        }
    }
    assert!(
        opened.elapsed() >= Duration::from_secs(6),
        "the stream ended at its lifetime, not at the check: {:?}",
        opened.elapsed()
    );
}

// --- The frames drain, between frames --------------------------------------------

/// A clock that moves 1 s on every read, from `start`.
fn ticking(start: Instant) -> impl FnMut() -> Instant {
    let mut reads = 0u64;
    move || {
        reads += 1;
        start + Duration::from_secs(reads)
    }
}

/// 16 queued frames behind a sender the caller keeps, so the drain never reads
/// the queue empty or ended before it runs out of frames.
fn full_queue() -> (
    std::sync::mpsc::SyncSender<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel(16);
    for _ in 0..16 {
        tx.send(b"{}".to_vec()).expect("queue a frame");
    }
    (tx, rx)
}

/// The view device's row, its first check due 5 s after `opened`.
fn row_opened_at(opened: Instant) -> RowWatch {
    seed_devices();
    let device = devices::authenticate(Some(VIEW_TOKEN))
        .expect("the list reads")
        .expect("the device");
    RowWatch::new(&device, opened)
}

#[test]
fn a_row_check_coming_due_stops_a_drain_whose_queue_never_empties() {
    let _home = HomeSandbox::new();
    let opened = Instant::now();
    let row = row_opened_at(opened);
    let (_tx, rx) = full_queue();
    let mut written = 0;
    let drained = drain_frames(
        &rx,
        opened + Duration::from_secs(3600),
        &row,
        ticking(opened),
        |_| {
            written += 1;
            Ok(())
        },
    );
    assert_eq!((drained, written), (Drained::Due, 4), "due at the 5th read");
}

#[test]
fn a_lifetime_coming_due_stops_a_drain_whose_queue_never_empties() {
    let _home = HomeSandbox::new();
    let opened = Instant::now();
    let row = row_opened_at(opened);
    let (_tx, rx) = full_queue();
    let mut written = 0;
    let drained = drain_frames(
        &rx,
        opened + Duration::from_secs(3),
        &row,
        ticking(opened),
        |_| {
            written += 1;
            Ok(())
        },
    );
    assert_eq!((drained, written), (Drained::Due, 2), "due at the 3rd read");
}

/// The drain's other three ends, as the loop before it ended them.
#[test]
fn a_drain_stops_where_the_queue_or_the_client_says() {
    let _home = HomeSandbox::new();
    let opened = Instant::now();
    let row = row_opened_at(opened);
    let deadline = opened + Duration::from_secs(3600);
    let now = || opened;

    let (_tx, empty) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    assert_eq!(
        drain_frames(&empty, deadline, &row, now, |_| Ok(())),
        Drained::Empty
    );

    let (tx, ended) = full_queue();
    drop(tx);
    let mut written = 0;
    let drained = drain_frames(&ended, deadline, &row, now, |_| {
        written += 1;
        Ok(())
    });
    assert_eq!((drained, written), (Drained::Ended, 16));

    let (_tx, refused) = full_queue();
    let mut tried = 0;
    let drained = drain_frames(&refused, deadline, &row, now, |_| {
        tried += 1;
        Err(std::io::Error::other("the client left"))
    });
    assert_eq!((drained, tried), (Drained::WriteFailed, 1));
}

/// The frame left queued when the row comes due is the first the next drain
/// writes: a drain that took it before checking the row would lose it, and one
/// that wrote it twice would show here too.
#[test]
fn a_frame_left_queued_when_the_row_comes_due_is_written_next() {
    let _home = HomeSandbox::new();
    let opened = Instant::now();
    let row = row_opened_at(opened);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    for seq in 0..16u8 {
        tx.send(vec![seq]).expect("queue a frame");
    }
    let mut first = Vec::new();
    let drained = drain_frames(
        &rx,
        opened + Duration::from_secs(3600),
        &row,
        ticking(opened),
        |line| {
            first.push(line[0]);
            Ok(())
        },
    );
    assert_eq!(
        (drained, first.len()),
        (Drained::Due, 4),
        "due at the 5th read"
    );
    let mut rest = Vec::new();
    let drained = drain_frames(
        &rx,
        opened + Duration::from_secs(3600),
        &row,
        || opened,
        |line| {
            rest.push(line[0]);
            Ok(())
        },
    );
    assert_eq!(drained, Drained::Empty);
    assert_eq!(first, vec![0, 1, 2, 3], "every frame before the due check");
    assert_eq!(
        rest,
        (4u8..16).collect::<Vec<u8>>(),
        "the frame the due check left queued goes out first, none lost or twice"
    );
    drop(tx);
}
