#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The events stream, driven against an in-memory sink: the feed frames, the
//! herdr degrade and relay, the deadline, and the keepalive. The herdr socket
//! is a fake `UnixListener`, so no real herdr is ever touched.

#![cfg(unix)]

use super::*;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::daemon::api::devices::{self, Tier};
use crate::daemon::api::http::{Request, Response};
use crate::daemon::api::routes::{self, ApiContext};
use crate::profile::{AppConfig, AppState, ConfigHandle};
use crate::testutil::HomeSandbox;

/// The bearer the router test pairs as a control device.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A status feed body shaped like the published one. `generated_at` is the
/// field the daemon moves every tick whether or not anything visible changed.
fn feed(active: &str, generated_at: &str) -> String {
    format!(
        r#"{{"schema":1,"generated_at":"{generated_at}","active_profile":"{active}","pending_switch":null,"wrap_off":false,"refresh_interval_ms":120000,"profiles":[]}}"#
    )
}

fn status_path() -> PathBuf {
    let dir = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    dir.join("status.json")
}

fn write_feed(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write status.json");
}

/// The tag `etag_for` gives a body, the same one a `status` frame's `id` uses.
fn tag_of(body: &str) -> String {
    crate::daemon::api::routes::etag_for(body.as_bytes())
}

fn no_herdr() -> HerdrSeam {
    Arc::new(|| None)
}

fn herdr_at(path: PathBuf) -> HerdrSeam {
    Arc::new(move || Some(path.clone()))
}

/// Run the stream loop against a `Vec<u8>` sink.
fn drive(feed: &Path, resolve: &HerdrSeam, deadline: Instant) -> Vec<u8> {
    let mut sink = Vec::new();
    run_stream(&mut sink, deadline, feed, resolve.as_ref()).expect("stream");
    sink
}

/// A sink the stream thread writes and the test thread reads concurrently, so
/// the test can wait for a frame to land before writing the next feed — no
/// wall-clock races on when a poll happens to run.
#[derive(Clone)]
struct SharedSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedSink {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(Vec::new())))
    }

    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn status_events(sink: &SharedSink) -> usize {
    parse_frames(&sink.bytes())
        .iter()
        .filter(|f| f.0 == "status")
        .count()
}

fn wait_until(cond: impl Fn() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A stream `Response` whose closure runs the loop, for the head+stream render.
fn stream_response(feed: PathBuf, resolve: HerdrSeam) -> Response {
    Response::stream(
        200,
        "text/event-stream",
        Box::new(move |sink, deadline| run_stream(sink, deadline, &feed, resolve.as_ref())),
    )
}

/// Render head + stream through the same writer `serve_connection` uses.
fn render_response(resp: Response, deadline: Instant) -> Vec<u8> {
    let mut out = Vec::new();
    crate::daemon::api::http::write_response(
        &mut out,
        resp,
        &crate::daemon::api::http::Disposition::Close,
        deadline,
    )
    .expect("write response");
    out
}

/// Split the sink into `(event, id, data)` frames, in order. `retry:` and
/// `: keepalive` lines are not events and are dropped.
fn parse_frames(sink: &[u8]) -> Vec<(String, Option<String>, String)> {
    let text = String::from_utf8(sink.to_vec())
        .expect("utf8 stream")
        .replace('\r', "");
    let mut out = Vec::new();
    let mut name = String::new();
    let mut id = None;
    let mut data = Vec::new();
    let mut in_event = false;
    for line in text.split('\n') {
        if let Some(n) = line.strip_prefix("event: ") {
            if in_event {
                out.push((std::mem::take(&mut name), id.take(), data.join("\n")));
            }
            name = n.to_string();
            id = None;
            data = Vec::new();
            in_event = true;
        } else if let Some(v) = line.strip_prefix("id: ") {
            id = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("data: ") {
            data.push(v.to_string());
        }
    }
    if in_event {
        out.push((name, id, data.join("\n")));
    }
    out
}

/// The `(present, panes)` a `herdr` frame carries, parsed back out of its data.
fn herdr_data(frame: &(String, Option<String>, String)) -> (bool, Vec<serde_json::Value>) {
    assert_eq!(frame.0, "herdr", "not a herdr frame: {frame:?}");
    let value: serde_json::Value = serde_json::from_str(&frame.2).expect("herdr json");
    let present = value["present"].as_bool().expect("present");
    let panes = value
        .get("panes")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    (present, panes)
}

/// Every `data:` line is one JSON value: a frame whose data carried a raw
/// newline would split into a fragment that does not parse.
fn assert_single_line_data(sink: &[u8]) {
    let text = String::from_utf8(sink.to_vec()).expect("utf8 stream");
    for line in text.replace('\r', "").split('\n') {
        if let Some(data) = line.strip_prefix("data: ") {
            serde_json::from_str::<serde_json::Value>(data)
                .unwrap_or_else(|e| panic!("data line is not one JSON value ({e}): {data:?}"));
        }
    }
}

fn empty_config() -> ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    }))
}

fn ctx_with_herdr(herdr: HerdrSeam) -> Arc<ApiContext> {
    devices::seed_for_tests("test", Tier::Control, TOKEN).expect("seed device");
    ApiContext::new(
        empty_config(),
        status_path(),
        None,
        crate::daemon::api::panes::absent_probe(),
        herdr,
        crate::daemon::api::terminal::unspawnable_terminal(),
    )
}

fn req(method: &str, path: &str, bearer: Option<&str>, body: &str) -> Request {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        bearer: bearer.map(str::to_string),
        if_none_match: None,
        body: body.as_bytes().to_vec(),
        keep_alive: true,
        ws: Default::default(),
    }
}

fn peer() -> std::net::SocketAddr {
    std::net::SocketAddr::from(([192, 0, 2, 7], 50_000))
}

/// One request line from a fake herdr connection.
fn read_client_line(stream: &mut std::os::unix::net::UnixStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(pos);
            return String::from_utf8(buf).expect("utf8 request");
        }
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "client closed before a request line");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// One parsed fake-herdr request line.
fn req_json(line: &str) -> serde_json::Value {
    serde_json::from_str(line).expect("request json")
}

/// Accept one connection from the non-blocking `listener` within `timeout`,
/// returning `None` when the deadline passes without one — so a missed
/// handshake reds the test on a later assertion instead of parking the fake in
/// a blocking `accept` and hanging `server.join()`.
fn accept_within(
    listener: &std::os::unix::net::UnixListener,
    timeout: Duration,
) -> Option<std::os::unix::net::UnixStream> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match listener.accept() {
            // BSD sockets (macOS) hand the listener's non-blocking flag down to
            // the accepted socket; the fakes read blocking.
            Ok((conn, _)) => {
                conn.set_nonblocking(false)
                    .expect("blocking accepted socket");
                return Some(conn);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    None
}

/// A `pane.list` success line over the given pane ids.
fn panes_response(id: &str, pane_ids: &[&str]) -> String {
    let panes: Vec<serde_json::Value> = pane_ids
        .iter()
        .map(|p| serde_json::json!({"pane_id": p}))
        .collect();
    serde_json::json!({"id": id, "result": {"type": "pane_list", "panes": panes}}).to_string()
        + "\n"
}

/// A `subscription_started` success line.
fn subscribe_ack(id: &str) -> String {
    serde_json::json!({"id": id, "result": {"type": "subscription_started"}}).to_string() + "\n"
}

/// The subscription list a subscribe request must carry for the given panes:
/// one `pane.agent_status_changed` per pane, then `pane.created`, `pane.closed`.
fn assert_subscriptions(line: &str, pane_ids: &[&str]) {
    let req = req_json(line);
    let subs = req["params"]["subscriptions"]
        .as_array()
        .expect("subscriptions");
    let mut expected: Vec<serde_json::Value> = pane_ids
        .iter()
        .map(|p| serde_json::json!({"type": "pane.agent_status_changed", "pane_id": p}))
        .collect();
    expected.push(serde_json::json!({"type": "pane.created"}));
    expected.push(serde_json::json!({"type": "pane.closed"}));
    assert_eq!(subs.len(), expected.len(), "subscription count");
    for (i, (got, want)) in subs.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got, want, "subscription {i}");
    }
}

#[test]
fn a_stream_emits_the_feed_and_only_on_etag_moves() {
    let _home = HomeSandbox::new();
    let path = status_path();
    let alpha = feed("alpha", "t1");
    write_feed(&path, &alpha);
    let none = no_herdr();

    let sink = SharedSink::new();
    let stream_sink = sink.clone();
    let stream_path = path.clone();
    let stream_resolve = Arc::clone(&none);
    let stream = std::thread::spawn(move || {
        let mut sink = stream_sink;
        run_stream(
            &mut sink,
            Instant::now() + Duration::from_secs(3),
            &stream_path,
            stream_resolve.as_ref(),
        )
    });

    // The first poll emits the current feed, whole and tagged.
    wait_until(|| status_events(&sink) >= 1, Duration::from_secs(2));
    let first = parse_frames(&sink.bytes());
    assert!(first[0].0 == "herdr" && first[0].2 == r#"{"present":false}"#);
    assert_eq!(first[1].0, "status");
    assert_eq!(first[1].1.as_deref(), Some(tag_of(&alpha).as_str()));
    assert_eq!(first[1].2, alpha, "the whole published feed, byte for byte");

    // One feed change: exactly one more status event.
    crate::profile::atomic_write_600(&path, feed("beta", "t2").as_bytes()).expect("beta write");
    wait_until(|| status_events(&sink) >= 2, Duration::from_secs(2));

    // A rewrite that moves only `generated_at`: zero more, even after the
    // stream has had several polls to observe it.
    crate::profile::atomic_write_600(&path, feed("beta", "t3").as_bytes())
        .expect("generated_at-only write");
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        status_events(&sink),
        2,
        "a generated_at-only rewrite is not a change"
    );

    let _ = stream.join().expect("stream thread");

    let frames = parse_frames(&sink.bytes());
    assert_eq!(
        frames.len(),
        3,
        "herdr then exactly two status events: {frames:?}"
    );
    assert_eq!(frames[2].0, "status");
    let beta: serde_json::Value = serde_json::from_str(&frames[2].2).expect("beta json");
    assert_eq!(beta["active_profile"], "beta");
    assert_eq!(
        frames[2].1.as_deref(),
        Some(tag_of(&frames[2].2).as_str()),
        "the id is the tag of the body it accompanies"
    );
    assert_single_line_data(&sink.bytes());
}

#[test]
fn the_stream_starts_without_a_feed_and_emits_it_when_it_lands() {
    let _home = HomeSandbox::new();
    let path = status_path();
    let none = no_herdr();

    let sink = SharedSink::new();
    let stream_sink = sink.clone();
    let stream_path = path.clone();
    let stream_resolve = Arc::clone(&none);
    let stream = std::thread::spawn(move || {
        let mut sink = stream_sink;
        run_stream(
            &mut sink,
            Instant::now() + Duration::from_secs(3),
            &stream_path,
            stream_resolve.as_ref(),
        )
    });

    // No feed yet: the stream still starts, head + herdr only.
    wait_until(
        || parse_frames(&sink.bytes()).iter().any(|f| f.0 == "herdr"),
        Duration::from_secs(2),
    );
    let early = parse_frames(&sink.bytes());
    assert_eq!(
        early.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
        vec!["herdr"],
        "no status frame before the feed lands: {early:?}"
    );

    // The feed appears: the status event follows.
    crate::profile::atomic_write_600(&path, feed("alpha", "t1").as_bytes()).expect("feed write");
    wait_until(|| status_events(&sink) >= 1, Duration::from_secs(2));
    let _ = stream.join().expect("stream thread");

    let frames = parse_frames(&sink.bytes());
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0].0, "herdr");
    assert_eq!(frames[0].2, r#"{"present":false}"#);
    assert_eq!(frames[1].0, "status");
    assert_eq!(frames[1].2, feed("alpha", "t1"));
}

#[test]
fn herdr_absent_degrades_to_present_false_while_status_flows() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));
    let missing = _home.home().join("no-such-herdr.sock");
    let seam = herdr_at(missing);

    let sink = drive(&path, &seam, Instant::now() + Duration::from_millis(400));
    let frames = parse_frames(&sink);
    assert_eq!(frames[0].0, "herdr");
    assert_eq!(frames[0].2, r#"{"present":false}"#);
    assert!(
        frames
            .iter()
            .any(|f| f.0 == "status" && f.2 == feed("alpha", "t1")),
        "status events flow with herdr absent"
    );
    assert_eq!(
        frames.iter().filter(|f| f.0 == "herdr").count(),
        1,
        "one present:false, no retry within the window"
    );
}

#[test]
fn a_herdr_socket_relays_agent_status_and_reports_drop() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));

    let socket_path = _home.home().join("fake-herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        let delivered = r#"{"event":"pane.agent_status_changed","data":{"pane_id":"pane-1","workspace_id":"ws-1","agent_status":"blocked"}}"#;

        let mut c1 = accept_within(&listener, Duration::from_secs(2)).expect("accept pane.list");
        let line = read_client_line(&mut c1);
        let req: serde_json::Value = serde_json::from_str(&line).expect("pane.list request");
        assert_eq!(req["method"], "pane.list");
        let id = req["id"].as_str().expect("id").to_string();
        c1.write_all(
            format!(
                "{{\"id\":\"{id}\",\"result\":{{\"type\":\"pane_list\",\"panes\":[{{\"pane_id\":\"pane-1\"}}]}}}}\n"
            )
            .as_bytes(),
        )
        .expect("pane.list response");
        drop(c1);

        let mut c2 = accept_within(&listener, Duration::from_secs(2)).expect("accept subscribe");
        let line = read_client_line(&mut c2);
        let req: serde_json::Value = serde_json::from_str(&line).expect("subscribe request");
        assert_eq!(req["method"], "events.subscribe");
        let subs = req["params"]["subscriptions"]
            .as_array()
            .expect("subscriptions");
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0]["type"], "pane.agent_status_changed");
        assert_eq!(subs[0]["pane_id"], "pane-1");
        assert_eq!(subs[1]["type"], "pane.created");
        assert_eq!(subs[2]["type"], "pane.closed");
        let id = req["id"].as_str().expect("id").to_string();
        c2.write_all(
            format!("{{\"id\":\"{id}\",\"result\":{{\"type\":\"subscription_started\"}}}}\n")
                .as_bytes(),
        )
        .expect("success");
        c2.write_all(delivered.as_bytes()).expect("delivered event");
        c2.write_all(b"\n").expect("newline");
        drop(c2);
    });

    let seam = herdr_at(socket_path);
    let sink = drive(&path, &seam, Instant::now() + Duration::from_millis(900));
    server.join().expect("server");

    let frames = parse_frames(&sink);
    assert_eq!(frames[0].0, "herdr");
    let (present, panes) = herdr_data(&frames[0]);
    assert!(present, "the first herdr frame is present:true");
    assert_eq!(
        panes,
        vec![serde_json::json!({"pane_id": "pane-1"})],
        "the herdr frame carries the pane.list snapshot verbatim"
    );

    let pane: Vec<_> = frames
        .iter()
        .filter(|f| f.0 == "pane_agent_status")
        .collect();
    assert_eq!(pane.len(), 1, "exactly one relayed agent-status event");
    assert_eq!(
        pane[0].2,
        r#"{"event":"pane.agent_status_changed","data":{"pane_id":"pane-1","workspace_id":"ws-1","agent_status":"blocked"}}"#,
        "the delivered line relayed byte for byte"
    );

    let herdr_frames: Vec<_> = frames.iter().filter(|f| f.0 == "herdr").collect();
    assert_eq!(
        herdr_frames.len(),
        2,
        "present:true then present:false: {frames:?}"
    );
    let (present, panes) = herdr_data(herdr_frames[1]);
    assert!(!present, "one present:false after the drop");
    assert!(panes.is_empty(), "a present:false frame carries no panes");
    assert!(frames.iter().any(|f| f.0 == "status"));
    assert_single_line_data(&sink);
}

#[test]
fn the_deadline_ends_the_stream_cleanly() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));
    let none = no_herdr();

    let sink = render_response(
        stream_response(path.clone(), Arc::clone(&none)),
        Instant::now() - Duration::from_millis(1),
    );
    let text = String::from_utf8(sink).expect("utf8");
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text:?}");
    assert!(
        text.contains("Content-Type: text/event-stream\r\n"),
        "{text:?}"
    );
    assert!(text.contains("Cache-Control: no-store\r\n"), "{text:?}");
    assert!(text.contains("Connection: close\r\n"), "{text:?}");
    assert!(!text.contains("Content-Length:"), "{text:?}");
    assert!(text.contains("retry: 1000\n"), "{text:?}");
    assert!(
        text.contains("event: herdr\ndata: {\"present\":false}\n\n"),
        "the herdr frame is complete, nothing torn: {text:?}"
    );
    assert!(!text.contains("event: status"), "{text:?}");

    let started = Instant::now();
    let sink = render_response(
        stream_response(path.clone(), Arc::clone(&none)),
        Instant::now() + Duration::from_millis(300),
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a 300ms deadline returns within a second: {:?}",
        started.elapsed()
    );
    let frames = parse_frames(&sink);
    assert!(frames.iter().any(|f| f.0 == "status"));
}

#[test]
fn a_keepalive_is_written_after_the_injected_interval() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));
    let none = no_herdr();
    let config = StreamConfig {
        keepalive: Duration::from_millis(50),
        herdr_retry: Duration::from_secs(5),
    };

    let mut sink = Vec::new();
    run_stream_with_config(
        &mut sink,
        Instant::now() + Duration::from_millis(500),
        &path,
        none.as_ref(),
        &config,
    )
    .expect("stream");
    let text = String::from_utf8(sink).expect("utf8");
    assert!(
        text.contains(": keepalive\n\n"),
        "a keepalive comment lands when nothing else does: {text:?}"
    );
}

#[test]
fn head_events_answers_the_stream_head_with_no_body() {
    let _home = HomeSandbox::new();
    let ctx = ctx_with_herdr(no_herdr());
    let handled = routes::handle(
        &ctx,
        &req("HEAD", "/api/v1/events", Some(TOKEN), ""),
        peer(),
    );
    assert_eq!(handled.response.status, 200);
    assert!(
        handled.response.stream.is_some(),
        "the handler answers a stream"
    );
    let head = handled.response.into_head();
    assert!(head.body.is_empty());
    assert!(head.stream.is_none(), "into_head drops the stream closure");
    assert_eq!(head.content_type, "text/event-stream");
}

#[test]
fn a_status_frame_frames_the_pretty_feed_line_by_line() {
    let _home = HomeSandbox::new();
    let path = status_path();
    let body = crate::daemon::status_json::StatusBody {
        schema: 1,
        generated_at: "t1".to_string(),
        active_profile: Some("alpha".to_string()),
        pending_switch: None,
        wrap_off: false,
        active_codex_profile: None,
        codex_fallback_chain: Vec::new(),
        codex_wrap_off: false,
        refresh_interval_ms: 120_000,
        clauth_version: env!("CARGO_PKG_VERSION").to_string(),
        gateway: None,
        proxies: Vec::new(),
        profiles: Vec::new(),
    };
    let bytes = serde_json::to_vec_pretty(&body).expect("pretty feed");
    assert!(bytes.contains(&b'\n'), "the fixture is multi-line");
    std::fs::write(&path, &bytes).expect("write status.json");

    let none = no_herdr();
    let sink = drive(&path, &none, Instant::now() + Duration::from_millis(300));
    let frames = parse_frames(&sink);
    let status = frames
        .iter()
        .find(|f| f.0 == "status")
        .expect("one status frame");
    assert_eq!(
        status.2.as_bytes(),
        bytes,
        "the re-joined data lines equal the published bytes"
    );
}

#[test]
fn write_status_frames_a_trailing_newline_as_an_empty_data_line() {
    let mut sink = Vec::new();
    write_status(&mut sink, "tag", b"{\n  \"a\": 1\n}\n").expect("write");
    assert_eq!(
        String::from_utf8(sink).expect("utf8"),
        "event: status\nid: tag\ndata: {\ndata:   \"a\": 1\ndata: }\ndata: \n\n",
        "each line is its own data line; a trailing newline yields a trailing empty data line"
    );
}

#[test]
fn a_pane_created_mid_stream_resubscribes_and_a_pane_closed_reopens_silently() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));

    let socket_path = _home.home().join("fake-herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        // Handshake 1: pane.list -> [pane-1].
        let mut c1 = accept_within(&listener, Duration::from_secs(2)).expect("c1 list");
        let req1 = read_client_line(&mut c1);
        assert_eq!(req_json(&req1)["method"], "pane.list");
        let id1 = req_json(&req1)["id"].as_str().expect("id").to_string();
        c1.write_all(panes_response(&id1, &["pane-1"]).as_bytes())
            .expect("list1");
        drop(c1);

        // Handshake 1: subscribe covers pane-1 plus the pane-set lifecycle;
        // then a pane_created event arrives. c2 stays open past the re-open.
        let mut c2 = accept_within(&listener, Duration::from_secs(2)).expect("c2 subscribe");
        let req2 = read_client_line(&mut c2);
        assert_eq!(req_json(&req2)["method"], "events.subscribe");
        assert_subscriptions(&req2, &["pane-1"]);
        let id2 = req_json(&req2)["id"].as_str().expect("id").to_string();
        c2.write_all(subscribe_ack(&id2).as_bytes()).expect("ack2");
        c2.write_all(br#"{"event":"pane_created","data":{"type":"pane_created","pane":{"pane_id":"pane-2"}}}"#)
            .expect("created");
        c2.write_all(b"\n").expect("nl");

        // Re-open: pane.list now lists two panes.
        let mut c3 = accept_within(&listener, Duration::from_secs(2)).expect("c3 list");
        let req3 = read_client_line(&mut c3);
        assert_eq!(req_json(&req3)["method"], "pane.list");
        let id3 = req_json(&req3)["id"].as_str().expect("id").to_string();
        c3.write_all(panes_response(&id3, &["pane-1", "pane-2"]).as_bytes())
            .expect("list2");
        drop(c3);

        // Re-open: subscribe covers both panes.
        let mut c4 = accept_within(&listener, Duration::from_secs(2)).expect("c4 subscribe");
        let req4 = read_client_line(&mut c4);
        assert_eq!(req_json(&req4)["method"], "events.subscribe");
        assert_subscriptions(&req4, &["pane-1", "pane-2"]);
        let id4 = req_json(&req4)["id"].as_str().expect("id").to_string();
        c4.write_all(subscribe_ack(&id4).as_bytes()).expect("ack4");

        // The old connection goes unread once the new one is acked: whatever
        // it still held is superseded by the snapshot the re-open carried.
        drop(c2);

        // An agent-status for pane-2 then a pane_closed arrive on the new
        // connection.
        c4.write_all(br#"{"event":"pane.agent_status_changed","data":{"pane_id":"pane-2","workspace_id":"ws-2","agent_status":"working"}}"#)
            .expect("status");
        c4.write_all(b"\n").expect("nl");
        c4.write_all(
            br#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"pane-2"}}"#,
        )
        .expect("closed");
        c4.write_all(b"\n").expect("nl");
        drop(c4);

        // Re-open after pane_closed: list back to one pane.
        let mut c5 = accept_within(&listener, Duration::from_secs(2)).expect("c5 list");
        let req5 = read_client_line(&mut c5);
        assert_eq!(req_json(&req5)["method"], "pane.list");
        let id5 = req_json(&req5)["id"].as_str().expect("id").to_string();
        c5.write_all(panes_response(&id5, &["pane-1"]).as_bytes())
            .expect("list3");
        drop(c5);

        // Final subscribe: ack, then drop so the stream reports the drop.
        let mut c6 = accept_within(&listener, Duration::from_secs(2)).expect("c6 subscribe");
        let req6 = read_client_line(&mut c6);
        assert_eq!(req_json(&req6)["method"], "events.subscribe");
        assert_subscriptions(&req6, &["pane-1"]);
        let id6 = req_json(&req6)["id"].as_str().expect("id").to_string();
        c6.write_all(subscribe_ack(&id6).as_bytes()).expect("ack6");
        drop(c6);
    });

    let seam = herdr_at(socket_path);
    let config = StreamConfig {
        keepalive: Duration::from_secs(15),
        herdr_retry: Duration::from_millis(50),
    };
    let mut sink = Vec::new();
    run_stream_with_config(
        &mut sink,
        Instant::now() + Duration::from_secs(3),
        &path,
        seam.as_ref(),
        &config,
    )
    .expect("stream");
    server.join().expect("server");

    let frames = parse_frames(&sink);
    let herdr_frames: Vec<_> = frames.iter().filter(|f| f.0 == "herdr").collect();
    assert_eq!(
        herdr_frames.len(),
        4,
        "initial, two re-opens, then the drop: {frames:?}"
    );
    let (present, panes) = herdr_data(herdr_frames[0]);
    assert!(present, "initial present:true");
    assert_eq!(
        panes,
        vec![serde_json::json!({"pane_id": "pane-1"})],
        "the initial snapshot lists the one pane"
    );
    let (present, panes) = herdr_data(herdr_frames[1]);
    assert!(present, "a re-open writes present:true");
    assert_eq!(
        panes,
        vec![
            serde_json::json!({"pane_id": "pane-1"}),
            serde_json::json!({"pane_id": "pane-2"})
        ],
        "the pane_created re-open carries the grown list"
    );
    let (present, panes) = herdr_data(herdr_frames[2]);
    assert!(present, "the pane_closed re-open writes present:true");
    assert_eq!(
        panes,
        vec![serde_json::json!({"pane_id": "pane-1"})],
        "the pane_closed re-open carries the shrunk list"
    );
    let (present, panes) = herdr_data(herdr_frames[3]);
    assert!(!present, "one present:false after the final drop");
    assert!(panes.is_empty(), "a present:false frame carries no panes");

    let pane: Vec<_> = frames
        .iter()
        .filter(|f| f.0 == "pane_agent_status")
        .collect();
    assert_eq!(
        pane.len(),
        1,
        "exactly the status on the new connection is relayed: {frames:?}"
    );
    assert_eq!(
        pane[0].2,
        r#"{"event":"pane.agent_status_changed","data":{"pane_id":"pane-2","workspace_id":"ws-2","agent_status":"working"}}"#,
        "the agent-status on the new connection is relayed byte for byte"
    );
}

#[test]
fn a_failed_reopen_keeps_the_live_connection_and_retries() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));

    let socket_path = _home.home().join("fake-herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        // Initial handshake: pane.list -> [pane-1], subscribe acked (live).
        let mut c1 = accept_within(&listener, Duration::from_secs(2)).expect("c1 list");
        let req1 = read_client_line(&mut c1);
        assert_eq!(req_json(&req1)["method"], "pane.list");
        let id1 = req_json(&req1)["id"].as_str().expect("id").to_string();
        c1.write_all(panes_response(&id1, &["pane-1"]).as_bytes())
            .expect("list1");
        drop(c1);

        let mut c2 = accept_within(&listener, Duration::from_secs(2)).expect("c2 subscribe");
        let req2 = read_client_line(&mut c2);
        assert_eq!(req_json(&req2)["method"], "events.subscribe");
        assert_subscriptions(&req2, &["pane-1"]);
        let id2 = req_json(&req2)["id"].as_str().expect("id").to_string();
        c2.write_all(subscribe_ack(&id2).as_bytes()).expect("ack2");

        // A pane_created triggers a re-open whose subscribe is refused; c2
        // stays open, so the live subscription survives the failure.
        c2.write_all(br#"{"event":"pane_created","data":{"type":"pane_created","pane":{"pane_id":"pane-2"}}}"#)
            .expect("created");
        c2.write_all(b"\n").expect("nl");

        let mut c3 = accept_within(&listener, Duration::from_secs(2)).expect("c3 list");
        let req3 = read_client_line(&mut c3);
        assert_eq!(req_json(&req3)["method"], "pane.list");
        let id3 = req_json(&req3)["id"].as_str().expect("id").to_string();
        c3.write_all(panes_response(&id3, &["pane-1", "pane-2"]).as_bytes())
            .expect("list2");
        drop(c3);

        let mut c4 = accept_within(&listener, Duration::from_secs(2)).expect("c4 subscribe");
        let req4 = read_client_line(&mut c4);
        assert_eq!(req_json(&req4)["method"], "events.subscribe");
        let id4 = req_json(&req4)["id"].as_str().expect("id").to_string();
        c4.write_all(
            format!("{{\"id\":\"{id4}\",\"error\":{{\"code\":\"pane_not_found\",\"message\":\"gone\"}}}}\n")
                .as_bytes(),
        )
        .expect("refuse");
        drop(c4);

        // The live c2 keeps serving: an agent-status line arrives on it.
        c2.write_all(br#"{"event":"pane.agent_status_changed","data":{"pane_id":"pane-1","workspace_id":"ws-1","agent_status":"blocked"}}"#)
            .expect("status on live conn");
        c2.write_all(b"\n").expect("nl");

        // The retry (herdr_retry) re-opens successfully.
        let mut c5 = accept_within(&listener, Duration::from_secs(2)).expect("c5 list");
        let req5 = read_client_line(&mut c5);
        assert_eq!(req_json(&req5)["method"], "pane.list");
        let id5 = req_json(&req5)["id"].as_str().expect("id").to_string();
        c5.write_all(panes_response(&id5, &["pane-1", "pane-2"]).as_bytes())
            .expect("list3");
        drop(c5);

        let mut c6 = accept_within(&listener, Duration::from_secs(2)).expect("c6 subscribe");
        let req6 = read_client_line(&mut c6);
        assert_eq!(req_json(&req6)["method"], "events.subscribe");
        assert_subscriptions(&req6, &["pane-1", "pane-2"]);
        let id6 = req_json(&req6)["id"].as_str().expect("id").to_string();
        c6.write_all(subscribe_ack(&id6).as_bytes()).expect("ack6");
        drop(c6);
    });

    let seam = herdr_at(socket_path);
    let config = StreamConfig {
        keepalive: Duration::from_secs(15),
        herdr_retry: Duration::from_millis(1500),
    };
    let mut sink = Vec::new();
    run_stream_with_config(
        &mut sink,
        Instant::now() + Duration::from_secs(3),
        &path,
        seam.as_ref(),
        &config,
    )
    .expect("stream");
    server.join().expect("server");

    let frames = parse_frames(&sink);
    let herdr_frames: Vec<_> = frames.iter().filter(|f| f.0 == "herdr").collect();
    assert_eq!(
        herdr_frames.len(),
        3,
        "initial, the retried re-open, then the final drop: {frames:?}"
    );
    let (present, panes) = herdr_data(herdr_frames[0]);
    assert!(present, "initial present:true");
    assert_eq!(panes, vec![serde_json::json!({"pane_id": "pane-1"})]);
    let (present, panes) = herdr_data(herdr_frames[1]);
    assert!(present, "the retried re-open writes present:true");
    assert_eq!(
        panes,
        vec![
            serde_json::json!({"pane_id": "pane-1"}),
            serde_json::json!({"pane_id": "pane-2"})
        ],
        "the retried re-open carries the grown list"
    );
    let (present, panes) = herdr_data(herdr_frames[2]);
    assert!(!present, "only the final drop writes present:false");
    assert!(panes.is_empty(), "a present:false frame carries no panes");

    let pane: Vec<_> = frames
        .iter()
        .filter(|f| f.0 == "pane_agent_status")
        .collect();
    assert_eq!(
        pane.len(),
        1,
        "the live connection's status line is still relayed: {frames:?}"
    );
    assert_eq!(
        pane[0].2,
        r#"{"event":"pane.agent_status_changed","data":{"pane_id":"pane-1","workspace_id":"ws-1","agent_status":"blocked"}}"#,
        "the failed re-open does not lose the live subscription"
    );
}

#[test]
fn a_retry_after_absence_hands_back_the_snapshot() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));

    let socket_path = _home.home().join("fake-herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        // First attempt: accept the pane.list connection and drop it
        // unanswered, so the stream reports herdr absent.
        let c1 = accept_within(&listener, Duration::from_secs(2)).expect("c1");
        drop(c1);

        // Retry: a full handshake.
        let mut c2 = accept_within(&listener, Duration::from_secs(2)).expect("c2 list");
        let req2 = read_client_line(&mut c2);
        assert_eq!(req_json(&req2)["method"], "pane.list");
        let id2 = req_json(&req2)["id"].as_str().expect("id").to_string();
        c2.write_all(panes_response(&id2, &["pane-1"]).as_bytes())
            .expect("list2");
        drop(c2);

        let mut c3 = accept_within(&listener, Duration::from_secs(2)).expect("c3 subscribe");
        let req3 = read_client_line(&mut c3);
        assert_eq!(req_json(&req3)["method"], "events.subscribe");
        assert_subscriptions(&req3, &["pane-1"]);
        let id3 = req_json(&req3)["id"].as_str().expect("id").to_string();
        c3.write_all(subscribe_ack(&id3).as_bytes()).expect("ack3");
        // Hold the subscription open until the stream's deadline closes it, so
        // the retried connection never drops mid-stream.
        c3.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let _ = c3.read(&mut [0u8; 1]);
    });

    let seam = herdr_at(socket_path);
    let config = StreamConfig {
        keepalive: Duration::from_secs(15),
        herdr_retry: Duration::from_millis(50),
    };
    let mut sink = Vec::new();
    run_stream_with_config(
        &mut sink,
        Instant::now() + Duration::from_secs(1),
        &path,
        seam.as_ref(),
        &config,
    )
    .expect("stream");
    server.join().expect("server");

    let frames = parse_frames(&sink);
    let herdr_frames: Vec<_> = frames.iter().filter(|f| f.0 == "herdr").collect();
    assert_eq!(
        herdr_frames.len(),
        2,
        "present:false then present:true: {frames:?}"
    );
    let (present, panes) = herdr_data(herdr_frames[0]);
    assert!(!present, "the first frame reports herdr absent");
    assert!(panes.is_empty(), "a present:false frame carries no panes");
    let (present, panes) = herdr_data(herdr_frames[1]);
    assert!(present, "the retry reports herdr present");
    assert_eq!(
        panes,
        vec![serde_json::json!({"pane_id": "pane-1"})],
        "the retry carries the snapshot"
    );
}

#[test]
fn the_herdr_socket_is_resolved_once_even_as_retries_keep_connecting() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));

    let socket_path = _home.home().join("fake-herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_serve = Arc::clone(&accepted);
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).expect("nonblocking");
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            match accept_within(&listener, end - Instant::now()) {
                Some(conn) => {
                    accepted_serve.fetch_add(1, Ordering::SeqCst);
                    drop(conn);
                }
                None => break,
            }
        }
    });

    let resolves = Arc::new(AtomicUsize::new(0));
    let resolves_seam = Arc::clone(&resolves);
    let socket_clone = socket_path.clone();
    let seam: HerdrSeam = Arc::new(move || {
        resolves_seam.fetch_add(1, Ordering::SeqCst);
        Some(socket_clone.clone())
    });
    let config = StreamConfig {
        keepalive: Duration::from_secs(15),
        herdr_retry: Duration::from_millis(50),
    };

    let mut sink = Vec::new();
    run_stream_with_config(
        &mut sink,
        Instant::now() + Duration::from_millis(1500),
        &path,
        seam.as_ref(),
        &config,
    )
    .expect("stream");
    server.join().expect("server");

    assert_eq!(
        resolves.load(Ordering::SeqCst),
        1,
        "the resolver seam runs once per stream"
    );
    assert!(
        accepted.load(Ordering::SeqCst) >= 2,
        "at least one retry connected to the socket"
    );
}

#[test]
fn keepalive_due_is_exact() {
    let start = Instant::now();
    let interval = Duration::from_millis(100);
    assert!(
        !keepalive_due(start, start, interval),
        "not due at zero elapsed"
    );
    assert!(
        keepalive_due(start, start + interval, interval),
        "due at exactly the interval"
    );
    assert!(
        keepalive_due(start, start + interval + Duration::from_millis(1), interval),
        "due past the interval"
    );
    assert!(
        !keepalive_due(start, start + interval - Duration::from_millis(1), interval),
        "not due one millisecond under"
    );
}

/// The snapshot a `present:true` frame carries is a projection to the status
/// fields: a pane's cwd, titles, tokens and scroll state never reach the view
/// tier through the stream.
#[test]
fn the_snapshot_carries_only_the_status_fields_of_a_pane() {
    let _home = HomeSandbox::new();
    let path = status_path();
    write_feed(&path, &feed("alpha", "t1"));

    let socket_path = _home.home().join("fake-herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let server = std::thread::spawn(move || {
        let mut c1 = accept_within(&listener, Duration::from_secs(2)).expect("c1 list");
        let req1 = read_client_line(&mut c1);
        let id1 = req_json(&req1)["id"].as_str().expect("id").to_string();
        let full = serde_json::json!({"id": id1, "result": {"type": "pane_list", "panes": [{
            "pane_id": "pane-1", "workspace_id": "ws-1", "tab_id": "tab-1", "agent": "claude",
            "agent_status": "working", "cwd": "/home/user/repos/app", "foreground_cwd": "/home/user/repos/app",
            "terminal_title": "\u{2733} Refactor the parser", "terminal_title_stripped": "Refactor the parser",
            "tokens": {"clauth": "DS5"}, "scroll": {"viewport_rows": 57}, "revision": 12, "focused": true
        }]}})
        .to_string()
            + "\n";
        c1.write_all(full.as_bytes()).expect("list");
        drop(c1);
        let mut c2 = accept_within(&listener, Duration::from_secs(2)).expect("c2 subscribe");
        let req2 = read_client_line(&mut c2);
        let id2 = req_json(&req2)["id"].as_str().expect("id").to_string();
        c2.write_all(subscribe_ack(&id2).as_bytes()).expect("ack");
        drop(c2);
    });

    let seam = herdr_at(socket_path);
    let mut sink = Vec::new();
    run_stream_with_config(
        &mut sink,
        Instant::now() + Duration::from_millis(600),
        &path,
        seam.as_ref(),
        &StreamConfig::DEFAULT,
    )
    .expect("stream");
    server.join().expect("server");

    let frames = parse_frames(&sink);
    let (present, panes) = herdr_data(&frames[0]);
    assert!(present);
    assert_eq!(
        panes,
        vec![serde_json::json!({
            "pane_id": "pane-1",
            "workspace_id": "ws-1",
            "agent": "claude",
            "agent_status": "working"
        })],
        "the snapshot is the four status fields, nothing else: {frames:?}"
    );
}
