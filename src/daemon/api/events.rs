//! `GET /api/v1/events` — the server-sent-events stream.
//!
//! One connection, one stream: the client learns every published-feed change
//! (the bytes of `~/.clauth/status.json`, keyed on its entity tag), every
//! herdr agent-status change, and — on every (re)connect — every pane's current
//! agent status from the handshake's `pane.list`, and every pane the terminal
//! bridge saw fought over (`terminal_contention`), without polling. The stream
//! runs until the connection's deadline and then closes cleanly, so an
//! `EventSource` client reconnects on its own and gets the current feed first
//! again.
//!
//! The herdr half speaks herdr's socket protocol: one JSON object per line. It
//! is unix-only (a `std::os::unix::net::UnixStream`); on every other target the
//! seam reads as absent and the stream still serves status events.

use std::io::Write;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::io::Read;

use super::http::Response;
use super::routes::{ApiContext, Caller, ErrorBody, WAIT_POLL, read_feed_tagged};
use super::terminal::{Contention, TerminalHolders};

/// How the stream learns where herdr's API socket is. The daemon passes the
/// production resolver; a test passes an explicit path or `None` — never an
/// environment variable.
pub(crate) type HerdrSeam = std::sync::Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// The production resolver: one `herdr status server --json` probe, else
/// absent. The probe runs with the session env stripped (owner ruling
/// 2026-09-15, row 7), so a daemon started inside a herdr pane resolves the
/// default session's socket, not its ancestor's — the inherited
/// `HERDR_SOCKET_PATH` is deliberately not consulted at all.
#[cfg(unix)]
pub(crate) fn production_herdr_resolver() -> HerdrSeam {
    std::sync::Arc::new(resolve_herdr_socket)
}

/// No unix-domain-socket client exists on other targets, so herdr reads as
/// absent there.
#[cfg(not(unix))]
pub(crate) fn production_herdr_resolver() -> HerdrSeam {
    std::sync::Arc::new(|| None)
}

/// The socket path the daemon should connect to: the default session's, from
/// one bounded `herdr status server --json` probe, else nothing.
#[cfg(unix)]
fn resolve_herdr_socket() -> Option<PathBuf> {
    let bin = crate::herdr::resolved_bin()?;
    let out = crate::herdr::daemon_bounded_output_deadline(
        bin.to_str()?,
        &["status", "server", "--json"],
        crate::herdr::PROBE_TIMEOUT,
    )?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // The socket path is printed even with no server running, so a retry can
    // connect against it directly (and succeeds the moment herdr starts).
    value.get("socket")?.as_str().map(PathBuf::from)
}

/// The stream's tunables. Kept as a struct rather than globals so a test can
/// drive the keepalive and the retry clock in milliseconds.
#[derive(Clone, Copy)]
struct StreamConfig {
    /// How long with no other frame before a `: keepalive` comment line.
    keepalive: Duration,
    /// How often a lost herdr socket is retried while the stream lives.
    herdr_retry: Duration,
}

impl StreamConfig {
    const DEFAULT: Self = Self {
        keepalive: Duration::from_secs(15),
        herdr_retry: Duration::from_secs(5),
    };
}

/// Whether a keepalive is due: `interval` has fully elapsed since `last_frame`.
fn keepalive_due(last_frame: Instant, now: Instant, interval: Duration) -> bool {
    now.saturating_duration_since(last_frame) >= interval
}

/// `GET /api/v1/events` — the server-sent-events stream.
#[utoipa::path(
    get,
    path = "/api/v1/events",
    responses(
        (status = 200, description = "a server-sent-events stream, opened with `retry: 1000`, then an `event: herdr` frame naming herdr's presence and, when present, carrying every pane's current agent status (`{\"present\":true,\"panes\":[…]}`), re-sent on every (re)connect; then `event: status` frames carrying the whole published feed (each `id` is the feed's entity tag) whenever the tag moves, `event: pane_agent_status` frames relaying herdr agent-status changes, `event: terminal_contention` frames (`{\"pane_id\":…,\"devices\":[…]}`) when one pane changed hands between devices 3 times in 5 minutes, and a `: keepalive` comment after 15 s of silence; the stream ends cleanly at the connection's 120-second lifetime and the client's EventSource reconnects", content_type = "text/event-stream", body = String),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 500, description = "the device list does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn events(
    ctx: &ApiContext,
    _req: &super::http::Request,
    _caller: &Caller<'_>,
) -> Response {
    let status_path = ctx.status_path.clone();
    let herdr = std::sync::Arc::clone(&ctx.herdr);
    let holders = std::sync::Arc::clone(&ctx.terminal_holders);
    Response::stream(
        200,
        "text/event-stream",
        Box::new(move |sink, deadline| {
            run_stream(sink, deadline, &status_path, herdr.as_ref(), &holders)
        }),
    )
}

/// Write the stream until the deadline, starting with the reconnect delay and
/// the herdr presence frame, then the current feed.
fn run_stream(
    sink: &mut dyn Write,
    deadline: Instant,
    feed: &Path,
    resolve_herdr: &(dyn Fn() -> Option<PathBuf> + Send + Sync),
    holders: &TerminalHolders,
) -> std::io::Result<()> {
    run_stream_with_config(
        sink,
        deadline,
        feed,
        resolve_herdr,
        holders,
        &StreamConfig::DEFAULT,
    )
}

/// The loop itself, with the config injected. Everything is flushed frame by
/// frame so a client sees an event the moment it is written.
fn run_stream_with_config(
    sink: &mut dyn Write,
    deadline: Instant,
    feed: &Path,
    resolve_herdr: &(dyn Fn() -> Option<PathBuf> + Send + Sync),
    holders: &TerminalHolders,
    config: &StreamConfig,
) -> std::io::Result<()> {
    // The one field line every stream opens with, so a reconnect is bounded.
    sink.write_all(b"retry: 1000\n")?;
    sink.flush()?;

    // The socket path is resolved once, up front: the production resolver may
    // run a bounded subprocess, which must not re-run per retry. From here on a
    // retry is a bare connect on this path, so a missing socket file fails in
    // microseconds.
    let herdr_socket = resolve_herdr();

    let mut last_tag: Option<String> = None;
    // Contention found before this stream opened is not replayed: the warning
    // is about a fight in progress, and a live one warns again.
    let mut contention_cursor = holders.cursor();
    let mut herdr: Option<Box<dyn HerdrSource + Send>> = None;
    let mut next_herdr_try = Instant::now();
    // A re-open that failed keeps the live connection; this is when to retry it.
    let mut reopen_at: Option<Instant> = None;

    // The herdr event is written before the first status read: the client needs
    // to know herdr's presence — and, when present, the current pane statuses —
    // whether or not the feed has landed yet.
    match open_herdr(herdr_socket.as_deref()) {
        Ok((conn, panes)) => {
            write_herdr(sink, true, &panes)?;
            herdr = Some(conn);
        }
        Err(_) => {
            write_herdr(sink, false, &[])?;
            next_herdr_try = Instant::now() + config.herdr_retry;
        }
    }
    let mut last_frame = Instant::now();

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }

        // The published feed, whole, keyed on its entity tag: a rewrite that
        // moved only `generated_at` is not a change, so no frame goes out.
        if let Some((body, tag)) = read_feed_tagged(feed)
            && last_tag.as_deref() != Some(tag.as_str())
        {
            write_status(sink, &tag, &body)?;
            last_tag = Some(tag);
            last_frame = Instant::now();
        }

        for fight in holders.contentions_since(&mut contention_cursor) {
            write_contention(sink, &fight)?;
            last_frame = Instant::now();
        }

        match herdr.take() {
            Some(mut conn) => {
                if reopen_at.is_some_and(|at| Instant::now() >= at) {
                    reopen_at = None;
                    match open_herdr(herdr_socket.as_deref()) {
                        Ok((new_conn, panes)) => {
                            // The old connection goes unread: the snapshot
                            // carries every status it could still hold.
                            drop(conn);
                            write_herdr(sink, true, &panes)?;
                            last_frame = Instant::now();
                            herdr = Some(new_conn);
                        }
                        Err(_) => {
                            herdr = Some(conn);
                            reopen_at = Some(Instant::now() + config.herdr_retry);
                        }
                    }
                } else {
                    match conn.next_event_line() {
                        Ok(Some(line)) => match classify_line(&line) {
                            Delivered::AgentStatus => {
                                write_pane_agent_status(sink, &line)?;
                                last_frame = Instant::now();
                                herdr = Some(conn);
                            }
                            Delivered::PaneSetChanged => {
                                // A pane appeared or left: the pane snapshot is
                                // stale, so re-subscribe on a fresh connection.
                                // A re-open DOES write a herdr frame now — the
                                // fresh snapshot is the point.
                                match open_herdr(herdr_socket.as_deref()) {
                                    Ok((new_conn, panes)) => {
                                        drop(conn);
                                        reopen_at = None;
                                        write_herdr(sink, true, &panes)?;
                                        last_frame = Instant::now();
                                        herdr = Some(new_conn);
                                    }
                                    Err(_) => {
                                        // The re-open failed: herdr refused the
                                        // subscribe (a pane closed between
                                        // pane.list and the subscribe). The old
                                        // connection is still live, so keep
                                        // serving it and retry the re-open on
                                        // the clock. No present:false — presence
                                        // did not change.
                                        herdr = Some(conn);
                                        reopen_at = Some(Instant::now() + config.herdr_retry);
                                    }
                                }
                            }
                            Delivered::Other => herdr = Some(conn),
                        },
                        Ok(None) => herdr = Some(conn),
                        Err(_) => {
                            // Dropped mid-stream: one present:false, then the
                            // retry clock.
                            reopen_at = None;
                            write_herdr(sink, false, &[])?;
                            last_frame = Instant::now();
                            next_herdr_try = Instant::now() + config.herdr_retry;
                        }
                    }
                }
            }
            None => {
                let now = Instant::now();
                if now >= next_herdr_try {
                    next_herdr_try = now + config.herdr_retry;
                    // Still absent: no event, `present:false` fired once on
                    // the transition.
                    if let Ok((conn, panes)) = open_herdr(herdr_socket.as_deref()) {
                        write_herdr(sink, true, &panes)?;
                        last_frame = Instant::now();
                        herdr = Some(conn);
                    }
                }
            }
        }

        let now = Instant::now();
        if keepalive_due(last_frame, now, config.keepalive) {
            sink.write_all(b": keepalive\n\n")?;
            sink.flush()?;
            last_frame = Instant::now();
        }

        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep(WAIT_POLL.min(deadline - now));
    }
}

/// `event: status`, carrying the whole published feed and its entity tag as the
/// event id, so a client's `Last-Event-ID` round-trips an `ETag`.
///
/// The feed is pretty-printed, so its bytes hold raw newlines. Each line becomes
/// its own `data:` field line (the SSE multi-line `data` form); the client
/// re-joins them with `\n`, which restores the body byte for byte. A body ending
/// in `\n` therefore yields one trailing empty `data:` line.
fn write_status(sink: &mut dyn Write, tag: &str, body: &[u8]) -> std::io::Result<()> {
    sink.write_all(b"event: status\nid: ")?;
    sink.write_all(tag.as_bytes())?;
    sink.write_all(b"\n")?;
    for line in body.split(|&b| b == b'\n') {
        sink.write_all(b"data: ")?;
        sink.write_all(line)?;
        sink.write_all(b"\n")?;
    }
    sink.write_all(b"\n")?;
    sink.flush()
}

/// `event: terminal_contention`: one pane changed hands between devices often
/// enough to be two clients stealing it from each other.
fn write_contention(sink: &mut dyn Write, fight: &Contention) -> std::io::Result<()> {
    sink.write_all(b"event: terminal_contention\ndata: ")?;
    sink.write_all(&serde_json::to_vec(fight).map_err(std::io::Error::other)?)?;
    sink.write_all(b"\n\n")?;
    sink.flush()
}

/// `event: herdr`, carrying herdr's presence and, when present, the pane
/// snapshot the handshake's `pane.list` returned — so every (re)connect hands
/// the client every pane's current agent status without a second round trip.
/// The snapshot is a projection to the status fields; the rest of a pane.list
/// object (its cwd, its title) is not carried here. A relayed
/// `pane.agent_status_changed` event still carries what herdr puts in it.
fn write_herdr(
    sink: &mut dyn Write,
    present: bool,
    panes: &[serde_json::Value],
) -> std::io::Result<()> {
    let mut body = Vec::new();
    if present {
        body.extend_from_slice(b"{\"present\":true,\"panes\":[");
        for (i, pane) in panes.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(&serde_json::to_vec(pane).map_err(std::io::Error::other)?);
        }
        body.extend_from_slice(b"]}");
    } else {
        body.extend_from_slice(b"{\"present\":false}");
    }
    sink.write_all(b"event: herdr\ndata: ")?;
    sink.write_all(&body)?;
    sink.write_all(b"\n\n")?;
    sink.flush()
}

/// What a delivered herdr line is to the stream.
enum Delivered {
    /// A `pane.agent_status_changed` event to relay verbatim.
    AgentStatus,
    /// A `pane.created`/`pane.closed` event: the pane snapshot is stale.
    PaneSetChanged,
    /// A response line, an unparseable line, or another subscription's event.
    Other,
}

/// Classify a delivered line by its `event` field. herdr names these
/// differently per delivery kind: agent-status envelopes carry the dot name
/// `pane.agent_status_changed`, while pane create/close arrive as app events
/// whose `EventKind` serializes snake_case (`pane_created`, `pane_closed`).
fn classify_line(line: &[u8]) -> Delivered {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return Delivered::Other;
    };
    match value.get("event").and_then(|e| e.as_str()) {
        Some("pane.agent_status_changed") => Delivered::AgentStatus,
        Some("pane_created") | Some("pane_closed") => Delivered::PaneSetChanged,
        _ => Delivered::Other,
    }
}

/// `event: pane_agent_status`, relaying a delivered herdr line byte for byte.
/// The caller already classified the line as `pane.agent_status_changed`.
fn write_pane_agent_status(sink: &mut dyn Write, line: &[u8]) -> std::io::Result<()> {
    sink.write_all(b"event: pane_agent_status\ndata: ")?;
    sink.write_all(line)?;
    sink.write_all(b"\n\n")?;
    sink.flush()
}

/// A live herdr subscription, polled for delivered event lines.
trait HerdrSource {
    /// The next delivered event line (JSON, no newline), `Ok(None)` when none
    /// has arrived, `Err` when the socket dropped.
    fn next_event_line(&mut self) -> std::io::Result<Option<Vec<u8>>>;
}

/// The pane set `pane.list` reported: the ids to subscribe to and the pane
/// objects themselves, which a `present:true` frame carries as the snapshot.
#[cfg(unix)]
struct PaneSnapshot {
    ids: Vec<String>,
    panes: Vec<serde_json::Value>,
}

/// List the panes on `path`, subscribe to `pane.agent_status_changed` on each
/// plus the pane-set lifecycle events, and hand back the live stream with its
/// success line consumed and the pane snapshot a `present:true` frame carries.
#[cfg(unix)]
fn open_herdr(
    path: Option<&Path>,
) -> std::io::Result<(Box<dyn HerdrSource + Send>, Vec<serde_json::Value>)> {
    let path =
        path.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no herdr socket"))?;
    let snapshot = list_panes(path)?;
    let conn = subscribe(path, &snapshot.ids)?;
    Ok((Box::new(conn), snapshot.panes))
}

#[cfg(not(unix))]
fn open_herdr(
    _path: Option<&Path>,
) -> std::io::Result<(Box<dyn HerdrSource + Send>, Vec<serde_json::Value>)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the herdr socket client is unix-only",
    ))
}

/// Take one `\n`-terminated line out of `buf`, without its newline, when a
/// complete line is buffered.
#[cfg(unix)]
fn take_line(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let line = buf.drain(..=pos).collect::<Vec<u8>>();
    Some(line[..line.len() - 1].to_vec())
}

#[cfg(unix)]
struct HerdrConn {
    stream: std::os::unix::net::UnixStream,
    /// Bytes already read past the last complete line (or not yet split).
    buf: Vec<u8>,
}

#[cfg(unix)]
impl HerdrSource for HerdrConn {
    fn next_event_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut chunk = [0u8; 512];
        loop {
            if let Some(line) = take_line(&mut self.buf) {
                if line.is_empty() {
                    continue;
                }
                return Ok(Some(line));
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "herdr closed the socket",
                    ));
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }
}

/// The pane snapshot from one `pane.list` round trip on its own short-lived
/// connection (herdr serves one request per connection).
#[cfg(unix)]
fn list_panes(path: &Path) -> std::io::Result<PaneSnapshot> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(WAIT_POLL))?;
    stream.set_write_timeout(Some(WAIT_POLL))?;
    stream
        .write_all(b"{\"id\":\"clauth-events:list\",\"method\":\"pane.list\",\"params\":{}}\n")?;
    let line = read_one_line(&mut stream, &mut Vec::new())?;
    let value: serde_json::Value = serde_json::from_slice(&line)
        .map_err(|_| std::io::Error::other("bad pane.list response"))?;
    if value.get("error").is_some() {
        return Err(std::io::Error::other("pane.list refused"));
    }
    let mut snapshot = PaneSnapshot {
        ids: Vec::new(),
        panes: Vec::new(),
    };
    if let Some(panes) = value
        .get("result")
        .and_then(|r| r.get("panes"))
        .and_then(|p| p.as_array())
    {
        for pane in panes {
            if let Some(id) = pane.get("pane_id").and_then(|v| v.as_str()) {
                snapshot.ids.push(id.to_string());
            }
            snapshot.panes.push(project_pane(pane));
        }
    }
    Ok(snapshot)
}

/// The status fields of one `pane.list` object, nothing else.
#[cfg(unix)]
fn project_pane(pane: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for key in ["pane_id", "workspace_id", "agent", "agent_status"] {
        if let Some(v) = pane.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    serde_json::Value::Object(out)
}

/// Open the subscribe connection, send one `events.subscribe` covering every
/// pane, read the success line, and hand the stream back with any bytes already
/// past that line kept in its buffer.
#[cfg(unix)]
fn subscribe(path: &Path, pane_ids: &[String]) -> std::io::Result<HerdrConn> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(WAIT_POLL))?;
    stream.set_write_timeout(Some(WAIT_POLL))?;

    let mut subscriptions: Vec<serde_json::Value> = pane_ids
        .iter()
        .map(|id| serde_json::json!({"type": "pane.agent_status_changed", "pane_id": id}))
        .collect();
    // The pane-set lifecycle events carry no pane id: a pane opened or closed
    // mid-stream invalidates the snapshot, so the stream re-subscribes.
    subscriptions.push(serde_json::json!({"type": "pane.created"}));
    subscriptions.push(serde_json::json!({"type": "pane.closed"}));
    let request = serde_json::json!({
        "id": "clauth-events",
        "method": "events.subscribe",
        "params": {"subscriptions": subscriptions},
    });
    let mut line = serde_json::to_vec(&request).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;

    let mut buf = Vec::new();
    let response = read_one_line(&mut stream, &mut buf)?;
    let value: serde_json::Value = serde_json::from_slice(&response)
        .map_err(|_| std::io::Error::other("bad events.subscribe response"))?;
    if value.get("error").is_some() {
        return Err(std::io::Error::other("events.subscribe refused"));
    }

    // Non-blocking from here: the stream loop polls at its own cadence and the
    // feed poll must not slip past `WAIT_POLL`.
    stream.set_nonblocking(true)?;
    Ok(HerdrConn { stream, buf })
}

/// Read one newline-terminated line through the stream's read timeout, leaving
/// any bytes already read past it in `buf`.
#[cfg(unix)]
fn read_one_line(
    stream: &mut std::os::unix::net::UnixStream,
    buf: &mut Vec<u8>,
) -> std::io::Result<Vec<u8>> {
    let mut chunk = [0u8; 512];
    loop {
        if let Some(line) = take_line(buf) {
            return Ok(line);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "herdr closed the socket",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// What the daemon's own herdr follower saw.
pub(crate) enum AgentStatusSeen {
    /// Every pane's `(pane_id, agent_status)`, on each (re)connect.
    Snapshot(Vec<(String, String)>),
    /// One pane's agent status changed.
    Changed { pane_id: String, status: String },
}

/// The pane id and status a `pane.agent_status_changed` line carries.
fn agent_status_of(line: &[u8]) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let data = value.get("data")?;
    Some((
        data.get("pane_id")?.as_str()?.to_string(),
        data.get("agent_status")?.as_str()?.to_string(),
    ))
}

/// The `(pane_id, agent_status)` pairs of a `pane.list` snapshot.
fn snapshot_statuses(panes: &[serde_json::Value]) -> Vec<(String, String)> {
    panes
        .iter()
        .filter_map(|pane| {
            Some((
                pane.get("pane_id")?.as_str()?.to_string(),
                pane.get("agent_status")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// How often an idle follower reads its subscription: a blocked push is
/// at most this late, and the daemon wakes this often for its whole life.
const FOLLOW_POLL: Duration = Duration::from_secs(1);

/// Follow every pane's agent status for the daemon's life, on the same
/// subscription a client's stream opens but independent of any client: a push
/// is for a phone that is NOT streaming. Hands `on_seen` the snapshot on every
/// (re)connect and each change after it; `on_seen` breaking ends the follow.
/// A lost or absent herdr is retried every `retry`, the socket re-resolved
/// only while none resolved (the path is printed with no server running, so
/// a plain reconnect finds it once herdr starts, with no herdr spawn per
/// retry). A pane opening or closing re-subscribes, keeping the live
/// connection until the new one stands, as the client stream does. Returns at
/// `deadline` (`None`: never).
pub(crate) fn follow_agent_status(
    resolve_herdr: &(dyn Fn() -> Option<PathBuf> + Send + Sync),
    retry: Duration,
    deadline: Option<Instant>,
    on_seen: &mut dyn FnMut(AgentStatusSeen) -> ControlFlow<()>,
) {
    let past = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    let pause = |wait: Duration| {
        let wait = match deadline {
            Some(deadline) => wait.min(deadline.saturating_duration_since(Instant::now())),
            None => wait,
        };
        std::thread::sleep(wait);
    };
    let mut socket = resolve_herdr();
    while !past() {
        let (mut conn, panes) = match open_herdr(socket.as_deref()) {
            Ok(opened) => opened,
            Err(_) => {
                pause(retry);
                if socket.is_none() {
                    socket = resolve_herdr();
                }
                continue;
            }
        };
        if on_seen(AgentStatusSeen::Snapshot(snapshot_statuses(&panes))).is_break() {
            return;
        }
        let mut reopen_at: Option<Instant> = None;
        while !past() {
            if reopen_at.is_some_and(|at| Instant::now() >= at) {
                reopen_at = None;
                match open_herdr(socket.as_deref()) {
                    Ok((fresh, panes)) => {
                        conn = fresh;
                        if on_seen(AgentStatusSeen::Snapshot(snapshot_statuses(&panes))).is_break()
                        {
                            return;
                        }
                    }
                    Err(_) => reopen_at = Some(Instant::now() + retry),
                }
                continue;
            }
            match conn.next_event_line() {
                Ok(Some(line)) => match classify_line(&line) {
                    Delivered::AgentStatus => {
                        if let Some((pane_id, status)) = agent_status_of(&line)
                            && on_seen(AgentStatusSeen::Changed { pane_id, status }).is_break()
                        {
                            return;
                        }
                    }
                    Delivered::PaneSetChanged => reopen_at = Some(Instant::now()),
                    Delivered::Other => {}
                },
                Ok(None) => pause(FOLLOW_POLL),
                Err(_) => {
                    pause(retry);
                    break;
                }
            }
        }
    }
}

/// The daemon's blocked-pane follower: [`follow_agent_status`] through one
/// [`crate::push::BlockedLatch`] for the follow's whole life, so a reconnect's
/// snapshot repeating a standing `blocked` hands `on_blocked` nothing. Hands it
/// each pane that moved INTO `blocked`; `on_blocked` breaking ends the follow.
pub(crate) fn follow_blocked(
    resolve_herdr: &(dyn Fn() -> Option<PathBuf> + Send + Sync),
    retry: Duration,
    deadline: Option<Instant>,
    on_blocked: &mut dyn FnMut(String) -> ControlFlow<()>,
) {
    let mut latch = crate::push::BlockedLatch::default();
    follow_agent_status(resolve_herdr, retry, deadline, &mut |seen| {
        let blocked = match seen {
            AgentStatusSeen::Snapshot(panes) => latch.snapshot(&panes),
            AgentStatusSeen::Changed { pane_id, status } => {
                if latch.changed(&pane_id, &status) {
                    vec![pane_id]
                } else {
                    Vec::new()
                }
            }
        };
        for pane_id in blocked {
            on_blocked(pane_id)?;
        }
        ControlFlow::Continue(())
    });
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_events.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "../../../tests/inline/daemon_api_events_follow.rs"]
mod follow_tests;
