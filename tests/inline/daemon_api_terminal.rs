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

use crate::daemon::api::devices::{self, Tier};
use crate::daemon::api::http::{Request, Response, WsHeaders};
use crate::daemon::api::panes::{HerdrOut, HerdrProbeOut};
use crate::daemon::api::routes::{ApiContext, handle};
use crate::daemon::api::tests as server_tests;
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
        if_none_match: None,
        body: Vec::new(),
        keep_alive: true,
        ws: WsHeaders {
            requested: true,
            protocol: Some("websocket".to_string()),
            key: Some(WS_KEY.to_string()),
            version: Some("13".to_string()),
        },
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
    // Three handovers, each more than the window apart from the first.
    let window = super::CONTENTION_WINDOW.as_secs();
    holders.attach("w9:p3", DRIVER, at(0));
    holders.attach("w9:p3", "phone", at(1));
    holders.attach("w9:p3", DRIVER, at(window));
    holders.attach("w9:p3", "phone", at(2 * window));
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
