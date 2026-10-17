//! `GET /api/v1/panes/<id>/stream` — the WebSocket terminal bridge.
//!
//! Bridges herdr's hidden `terminal session observe|control` group onto the
//! API: one WebSocket per pane, relaying herdr's newline-delimited JSON frames
//! as text frames verbatim and (on a control-tier device) forwarding the
//! documented control commands back to herdr's stdin. The route is outside the
//! HTTP route table and the OpenAPI document on purpose: OpenAPI covers HTTP
//! only, and the frame vocabulary stays hand-written beside the route table.
//!
//! The device tier decides the mode, never the client: a view-tier device gets
//! `observe` and any data frame it sends closes the socket with a policy code;
//! a control-tier device gets `control --takeover`. Every control attach
//! displaces the pane's current controller, which herdr sends a
//! `terminal.closed` record (`terminal attach taken over`) that relays like any
//! other frame before the socket closes; with no controller to displace,
//! `--takeover` attaches exactly like a plain control attach. A client
//! therefore reconnects only after its lifetime close, never after being taken
//! over, or two open clients steal the pane from each other in a loop.
//!
//! The daemon keeps a per-pane record of which device holds control
//! ([`TerminalHolders`]). A pane that changes hands between devices
//! [`CONTENTION_HANDOVERS`] times inside [`CONTENTION_WINDOW`] is a fight: one
//! `daemon.log` warning and one `terminal_contention` record for `/events`,
//! once per window.
//!
//! Geometry: a control attach without explicit `--cols/--rows` imposes
//! herdr's 120x40 default on the real pane (measured, T1), yanking the
//! desktop user's layout on every attach. The bridge therefore reads the
//! pane's own rect from `herdr api snapshot` — the only surface that names a
//! pane's width — and attaches at exactly that, so the pane never moves. A
//! client that later sends `terminal.resize` to a phone shape has chosen that
//! deliberately; the resize is forwarded and the audit line says so.
//!
//! A stream honours the connection loop's lifetime and ends there with a clean
//! close the client reconnects through (owner ruling 2026-09-15, row 6: no
//! slot is ever held forever); a reconnect re-attaches at the pane's own
//! geometry, so the mirror resumes rather than resets. Inside its lifetime the
//! bridge's own bounds apply: the 200 ms read slice, the inherited 10 s
//! single-write timeout, a bounded frames channel, and control commands that
//! cross to herdr's stdin on their own untimed-wait-free thread.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::devices::{Device, Tier};
use super::http::{Request, Response, accept_key};
use super::panes::{NO_SERVER, NOT_INSTALLED, PANE_NOT_FOUND};
use super::routes::{self, ApiContext, Handled};
use crate::herdr::parse_snapshot_rects;
use crate::lockorder::{RankedMutex, rank};

/// A client data frame larger than this is a client that has stopped framing
/// honestly; the socket closes with `1009` rather than buffering without end.
/// The largest legitimate frame is a keystroke or a resize — a few hundred
/// bytes.
const MAX_CLIENT_FRAME: usize = 1024 * 1024;
/// A herdr line longer than this is a herdr that has stopped framing honestly;
/// the bridge ends rather than relay it. Real frames measure ~53 KB.
const MAX_HERDR_LINE: usize = 4 * 1024 * 1024;
/// How long one wait for client bytes may block before the IO loop drains
/// the frames channel again. Purely a liveness bound: herdr output keeps
/// flowing on the relay thread whatever this is set to.
const READ_SLICE: Duration = Duration::from_millis(200);
/// Handovers between devices inside [`CONTENTION_WINDOW`] that make a fight.
pub(crate) const CONTENTION_HANDOVERS: usize = 3;
/// The window [`CONTENTION_HANDOVERS`] are counted over, and the quiet period
/// after a warning before the same pane can warn again.
pub(crate) const CONTENTION_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Contention records kept for `/events` streams to catch up on. A stream reads
/// them each tick, so a handful covers any burst between two ticks.
const CONTENTION_BACKLOG: usize = 32;

/// Which half of herdr's terminal group a connection bridged to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Observe,
    Control,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Control => "control",
        }
    }
}

/// A validated upgrade, handed to the connection loop to run. Everything the
/// bridge needs is decided here, before the `101`, so no refusal can arrive
/// over the upgraded protocol that could have been an HTTP status.
pub(crate) struct Hijack {
    pub(crate) accept: String,
    /// The target for the herdr spawn, exactly as the URL spelled it.
    pane_id: String,
    /// The same id flattened for the audit lines; the URL is the least
    /// trusted input that reaches them.
    pane_for_log: String,
    mode: Mode,
    /// The paired device's name, sanitized at construction: the store is a
    /// hand-editable file whose name field has no parser between it and these
    /// log lines, and `logline!` forges an entry on an embedded newline.
    device: String,
    cols: u16,
    rows: u16,
}

/// One pane fought over: `terminal_contention` on `/events`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Contention {
    pub(crate) pane_id: String,
    /// Every device the window's handovers moved control between, in the order
    /// each first appeared.
    pub(crate) devices: Vec<String>,
}

/// Which device holds control of each pane, and the contention records the
/// `/events` streams relay. Lives on the [`ApiContext`]: the bridge writes it,
/// the events loop reads it each tick.
pub(crate) struct TerminalHolders {
    state: RankedMutex<HoldersState, rank::TerminalHolders>,
}

#[derive(Default)]
struct HoldersState {
    /// The token the next control stream is handed.
    next_stream: u64,
    panes: HashMap<String, PaneHold>,
    /// The sequence number of the newest contention record.
    seq: u64,
    contentions: VecDeque<(u64, Contention)>,
}

#[derive(Default)]
struct PaneHold {
    /// The control stream holding the pane (its token) and its device.
    holder: Option<(u64, String)>,
    /// `(when, from, to)` for each handover inside the window, oldest first.
    handovers: VecDeque<(Instant, String, String)>,
    /// No second warning for this pane before this.
    quiet_until: Option<Instant>,
}

impl TerminalHolders {
    pub(crate) fn new() -> Self {
        Self {
            state: RankedMutex::new(HoldersState::default()),
        }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut HoldersState) -> T) -> T {
        match self.state.lock() {
            Ok(mut state) => f(&mut state),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    /// A control stream by `device` attached to `pane_id` at `now`. Returns the
    /// stream's token for [`Self::release`]. When this handover completes a
    /// fight, writes the one warning line and queues the one `/events` record.
    pub(crate) fn attach(&self, pane_id: &str, device: &str, now: Instant) -> u64 {
        let (token, fight) = self.with_state(|state| {
            let token = state.next_stream;
            state.next_stream += 1;
            state.panes.retain(|_, hold| {
                hold.holder.is_some()
                    || hold.handovers.back().is_some_and(|(at, ..)| {
                        now.saturating_duration_since(*at) < CONTENTION_WINDOW
                    })
            });
            let hold = state.panes.entry(pane_id.to_string()).or_default();
            let previous = hold.holder.replace((token, device.to_string()));
            let Some((_, from)) = previous.filter(|(_, from)| from != device) else {
                return (token, None);
            };
            hold.handovers.push_back((now, from, device.to_string()));
            while hold
                .handovers
                .front()
                .is_some_and(|(at, ..)| now.saturating_duration_since(*at) >= CONTENTION_WINDOW)
            {
                hold.handovers.pop_front();
            }
            if hold.handovers.len() < CONTENTION_HANDOVERS
                || hold.quiet_until.is_some_and(|until| now < until)
            {
                return (token, None);
            }
            hold.quiet_until = Some(now + CONTENTION_WINDOW);
            let mut devices: Vec<String> = Vec::new();
            for (_, from, to) in &hold.handovers {
                for name in [from, to] {
                    if !devices.contains(name) {
                        devices.push(name.clone());
                    }
                }
            }
            let fight = Contention {
                pane_id: pane_id.to_string(),
                devices,
            };
            state.seq += 1;
            state.contentions.push_back((state.seq, fight.clone()));
            if state.contentions.len() > CONTENTION_BACKLOG {
                state.contentions.pop_front();
            }
            (token, Some(fight))
        });
        if let Some(fight) = fight {
            let devices: Vec<String> = fight.devices.iter().map(|d| format!("'{d}'")).collect();
            crate::logline::logline!(
                "clauth api: warning: pane '{}' changed hands {} times in {} minutes between devices {}; \
                 two clients are likely reconnecting after being taken over",
                super::http::sanitize_for_log(&fight.pane_id),
                CONTENTION_HANDOVERS,
                CONTENTION_WINDOW.as_secs() / 60,
                devices.join(", ")
            );
        }
        token
    }

    /// The control stream `token` on `pane_id` ended. Clears the record if this
    /// stream still holds the pane; otherwise returns the device that took it.
    pub(crate) fn release(&self, pane_id: &str, token: u64) -> Option<String> {
        self.with_state(|state| {
            let hold = state.panes.get_mut(pane_id)?;
            match &hold.holder {
                Some((holder, _)) if *holder == token => {
                    hold.holder = None;
                    None
                }
                Some((_, device)) => Some(device.clone()),
                None => None,
            }
        })
    }

    /// Where a new `/events` stream starts reading: after every record so far.
    pub(crate) fn cursor(&self) -> u64 {
        self.with_state(|state| state.seq)
    }

    /// The records newer than `cursor`, moving it past them.
    pub(crate) fn contentions_since(&self, cursor: &mut u64) -> Vec<Contention> {
        self.with_state(|state| {
            let fresh: Vec<Contention> = state
                .contentions
                .iter()
                .filter(|(seq, _)| *seq > *cursor)
                .map(|(_, fight)| fight.clone())
                .collect();
            *cursor = state.seq;
            fresh
        })
    }
}

/// `/panes/<id>/stream` with `<id>` non-empty and slash-free, else `None`.
/// The id is herdr's own pane id (`w1:p1`), decoded once through the route
/// table's [`routes::decode_segment`] (a client encodes the `:`), then used
/// as the target.
pub(crate) fn pane_stream_target(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/panes/")?;
    let (id, tail) = rest.split_once('/')?;
    if tail != "stream" || id.is_empty() || id.contains('/') {
        return None;
    }
    routes::decode_segment(id)
}

/// The route half: validate, decide the mode, and hand back the hijack. Every
/// failure answers as an ordinary HTTP response, before any upgrade.
pub(crate) fn request(ctx: &ApiContext, req: &Request, device: &Device, pane_id: &str) -> Handled {
    if req.method != "GET" {
        return Handled {
            response: Response::error(405, "method_not_allowed"),
            device: Some(device.name.clone()),
            hijack: None,
        };
    }
    let mode = match &device.tier {
        Tier::Control => Mode::Control,
        Tier::View => Mode::Observe,
        Tier::Unknown(_) => {
            return Handled {
                response: routes::refuse_unknown_tier(device),
                device: Some(device.name.clone()),
                hijack: None,
            };
        }
    };
    if !req.ws.requested
        || !req
            .ws
            .protocol
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case("websocket"))
    {
        return Handled {
            response: Response::error(426, "upgrade_required"),
            device: Some(device.name.clone()),
            hijack: None,
        };
    }
    if req.ws.version.as_deref() != Some("13") {
        return Handled {
            response: Response::refused(
                400,
                "bad_websocket_version",
                "this server speaks WebSocket version 13 only",
            ),
            device: Some(device.name.clone()),
            hijack: None,
        };
    }
    let Some(accept) = req.ws.key.as_deref().and_then(accept_key) else {
        return Handled {
            response: Response::refused(
                400,
                "bad_websocket_key",
                "Sec-WebSocket-Key must be the base64 of 16 bytes",
            ),
            device: Some(device.name.clone()),
            hijack: None,
        };
    };
    let rect = match herdr_pane_rect(ctx, pane_id) {
        HerdrPaneRect::Absent => {
            return Handled {
                response: Response::refused(404, "pane_not_found", PANE_NOT_FOUND),
                device: Some(device.name.clone()),
                hijack: None,
            };
        }
        HerdrPaneRect::Reason(reason) => {
            return Handled {
                response: Response::refused(503, "herdr_unavailable", reason),
                device: Some(device.name.clone()),
                hijack: None,
            };
        }
        HerdrPaneRect::Rect(rect) => rect,
    };
    crate::logline::logline!(
        "clauth api: device '{}' opened {} on pane '{}'",
        super::http::sanitize_for_log(&device.name),
        mode.as_str(),
        super::http::sanitize_for_log(pane_id)
    );
    Handled {
        // Never written: the connection loop writes the 101 head itself and
        // hands the socket to the bridge. The status is 101 so a future
        // refactor that does write it cannot silently serve a 200-body
        // response where a client expects an upgrade.
        response: Response::raw_json(101, Vec::new()),
        device: Some(device.name.clone()),
        hijack: Some(Hijack {
            pane_id: pane_id.to_string(),
            pane_for_log: super::http::sanitize_for_log(pane_id),
            mode,
            device: super::http::sanitize_for_log(&device.name),
            accept,
            cols: rect.width,
            rows: rect.height,
        }),
    }
}

/// The pane's own rect, or why it could not be read.
enum HerdrPaneRect {
    Rect(crate::herdr::PaneRect),
    /// herdr answered and the pane is not there (or carries no rect).
    Absent,
    /// The fixed sentence naming the absent state, for a 503.
    Reason(&'static str),
}

fn herdr_pane_rect(ctx: &ApiContext, pane_id: &str) -> HerdrPaneRect {
    use super::panes::{HerdrOut, HerdrProbeOut};
    match (ctx.herdr_probe)(&["api", "snapshot"], crate::herdr::PROBE_TIMEOUT) {
        HerdrProbeOut::NotInstalled => HerdrPaneRect::Reason(NOT_INSTALLED),
        HerdrProbeOut::Ran(None) => HerdrPaneRect::Reason(NO_SERVER),
        HerdrProbeOut::Ran(Some(HerdrOut { success: false, .. })) => {
            HerdrPaneRect::Reason(NO_SERVER)
        }
        HerdrProbeOut::Ran(Some(out)) => match parse_snapshot_rects(&out.stdout) {
            None => HerdrPaneRect::Reason(NO_SERVER),
            Some(panes) => match panes.iter().find(|(id, _)| id == pane_id) {
                Some((_, Some(rect))) => HerdrPaneRect::Rect(crate::herdr::PaneRect {
                    width: rect.width,
                    height: rect.height,
                }),
                _ => HerdrPaneRect::Absent,
            },
        },
    }
}

/// One spawned `herdr terminal session …` process, reduced to the two pipes
/// the bridge relays plus the handle it terminates and reaps.
pub(crate) struct TerminalChild {
    pub(crate) stdin: Box<dyn Write + Send>,
    pub(crate) stdout: Box<dyn Read + Send>,
    child: Option<Child>,
}

impl TerminalChild {
    /// Kill the process if it is still running. Idempotent: a child that
    /// already exited is left alone.
    fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }

    /// Reap the process if it has not been reaped.
    fn reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

/// The seam between the bridge and the herdr subprocess, so no test runs a
/// real herdr. The daemon fills [`ApiContext::terminal_spawn`] with
/// [`real_terminal_spawn`]; tests spawn fixture scripts.
pub(crate) type TerminalSpawn =
    Arc<dyn Fn(&[&str]) -> std::io::Result<TerminalChild> + Send + Sync>;

/// The production spawn: the resolved herdr binary, the session env stripped
/// (owner ruling 2026-09-15, row 7 — the daemon targets herdr's default
/// session wherever it was started), piped both ways, stderr discarded.
pub(crate) fn real_terminal_spawn() -> TerminalSpawn {
    Arc::new(|args: &[&str]| {
        let bin = crate::herdr::resolved_bin()
            .ok_or_else(|| std::io::Error::other("herdr is not installed on this host"))?;
        let mut cmd = Command::new(&bin);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::herdr::strip_session_env(&mut cmd);
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("herdr stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("herdr stdout was not piped"))?;
        Ok(TerminalChild {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            child: Some(child),
        })
    })
}

/// The spawn every test context gets by default: fails loudly, because a test
/// that reaches the bridge without installing a fixture spawn is a test that
/// would otherwise have run a real herdr.
#[cfg(test)]
pub(crate) fn unspawnable_terminal() -> TerminalSpawn {
    Arc::new(|_: &[&str]| {
        Err(std::io::Error::other(
            "no terminal spawn installed for this test",
        ))
    })
}

/// Run the bridge to its end. Owns the connection: when this returns, the
/// socket has been closed (WebSocket close frame where the protocol allows
/// one, then TLS close_notify) and the herdr child has been terminated and
/// reaped. `leftover` is whatever the connection reader had buffered past the
/// handshake — a client may pipeline its first frames behind the upgrade.
///
/// The stream ends at `deadline`, the connection's own lifetime, with a clean
/// close the client reconnects through (owner ruling 2026-09-15, row 6: no
/// slot is ever held forever; the reconnect re-attaches at the pane's own
/// geometry). A control stream records itself in `holders` once herdr is
/// spawned and clears that record when it ends, unless another stream took the
/// pane meanwhile.
pub(crate) fn run(
    mut stream: rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
    leftover: Vec<u8>,
    hijack: Hijack,
    spawn: &TerminalSpawn,
    holders: &TerminalHolders,
    deadline: std::time::Instant,
) {
    let mut args: Vec<String> = vec![
        "terminal".into(),
        "session".into(),
        hijack.mode.as_str().into(),
        hijack.pane_id.clone(),
    ];
    if hijack.mode == Mode::Control {
        args.push("--takeover".into());
    }
    args.extend([
        "--cols".into(),
        hijack.cols.to_string(),
        "--rows".into(),
        hijack.rows.to_string(),
    ]);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let pane_for_log = hijack.pane_for_log.clone();
    let mut child = match spawn(&arg_refs) {
        Ok(child) => child,
        Err(e) => {
            crate::logline::logline!(
                "clauth api: device '{}' {} stream on pane '{}' failed to spawn herdr: {e}",
                hijack.device,
                hijack.mode.as_str(),
                pane_for_log
            );
            let _ = ws_write(
                &mut stream,
                OP_CLOSE,
                &close_payload(1011, "herdr unavailable"),
            );
            send_close_notify(&mut stream);
            return;
        }
    };
    // Recorded once herdr is running, so a spawn that failed displaced nobody.
    let held = (hijack.mode == Mode::Control)
        .then(|| holders.attach(&hijack.pane_id, &hijack.device, Instant::now()));

    // One IO thread owns the socket: the loop below is the only thing that
    // reads or writes the TLS stream, so the relay never competes for it. The
    // relay hands herdr's lines over a channel instead — a shared mutex here
    // starved the relay for seconds against the reader's timeout re-lock
    // cycle (std::sync::Mutex has no fairness), delaying frames far past any
    // usable latency.
    // Bounded on purpose: a full channel blocks the relay, herdr's stdout pipe
    // fills behind it, and the backpressure lands on the pane's own output —
    // the one place it is correct. An unbounded channel let a busy pane outpace
    // a slow client without bound.
    let (frames_tx, frames_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
    // The relay's completion signal. The join below is bounded: a grandchild
    // inheriting herdr's stdout pipe would keep `read_line` blocked past the
    // kill, and an unbounded join would hold the connection's slot forever for
    // a thread that can no longer do anything with it.
    let (relay_done_tx, relay_done_rx) = std::sync::mpsc::channel::<()>();
    // Control commands cross to herdr's stdin on their own thread for the same
    // reason: a pipe write has no timeout, and a herdr that stops reading must
    // not wedge the IO loop past the lifetime deadline.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (input_done_tx, input_done_rx) = std::sync::mpsc::channel::<()>();
    let mut input_child_stdin = std::mem::replace(&mut child.stdin, Box::new(std::io::sink()));
    let input_writer = std::thread::Builder::new()
        .name("clauth-api-terminal-input".into())
        .spawn(move || {
            while let Ok(line) = input_rx.recv() {
                if input_child_stdin.write_all(&line).is_err() {
                    break;
                }
                if input_child_stdin.flush().is_err() {
                    break;
                }
            }
            let _ = input_done_tx.send(());
        });
    if let Err(e) = input_writer {
        // Without the input writer the audit would keep saying input was sent
        // while nothing moved, so the stream ends rather than lie.
        crate::logline::logline!(
            "clauth api: failed to spawn the terminal input writer thread: {e}"
        );
        if let Some(token) = held {
            holders.release(&hijack.pane_id, token);
        }
        child.terminate();
        child.reap();
        let _ = ws_write(&mut stream, OP_CLOSE, &close_payload(1011, "internal"));
        send_close_notify(&mut stream);
        return;
    }

    // The relay: herdr's stdout lines, verbatim, into the channel. herdr ends
    // every stream with a `terminal.closed` record and exits, which is the
    // normal end of this thread.
    let relay_pane = hijack.pane_for_log.clone();
    let relay_done = relay_done_tx;
    let mut relay_child_stdout = std::mem::replace(&mut child.stdout, Box::new(std::io::empty()));
    let relay = std::thread::Builder::new()
        .name("clauth-api-terminal-relay".into())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(&mut relay_child_stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line.len() > MAX_HERDR_LINE {
                            crate::logline::logline!(
                                "clauth api: herdr stream on pane '{}' exceeded the line ceiling",
                                relay_pane
                            );
                            break;
                        }
                        let trimmed = line.trim_end_matches(['\n', '\r']);
                        if trimmed.is_empty() {
                            continue;
                        }
                        if frames_tx.send(trimmed.as_bytes().to_vec()).is_err() {
                            // The IO loop is gone; the client left.
                            break;
                        }
                    }
                }
            }
            let _ = relay_done.send(());
        });
    let relay = match relay {
        Ok(relay) => relay,
        Err(e) => {
            // Without the relay nothing drains herdr's stdout; end the stream
            // rather than let a full pipe wedge the child.
            crate::logline::logline!("clauth api: failed to spawn the terminal relay thread: {e}");
            if let Some(token) = held {
                holders.release(&hijack.pane_id, token);
            }
            child.terminate();
            child.reap();
            let _ = ws_write(&mut stream, OP_CLOSE, &close_payload(1011, "internal"));
            send_close_notify(&mut stream);
            return;
        }
    };

    // The IO loop: herdr's frames out, client frames in, control commands
    // forwarded. One read timeout bounds the wait; frames queued meanwhile go
    // out at the top of the next cycle.
    let _ = stream.sock.set_read_timeout(Some(READ_SLICE));
    let mut buf = leftover;
    let mut client_closed = false;
    let mut herdr_ended = false;
    let mut lifetime_ended = false;
    'io: loop {
        if Instant::now() >= deadline {
            lifetime_ended = true;
            let _ = ws_write(&mut stream, OP_CLOSE, &close_payload(1000, ""));
            break 'io;
        }
        // Out with herdr's frames first, so a queued frame never waits on a
        // client that stays silent for the whole read slice.
        loop {
            match frames_rx.try_recv() {
                Ok(line) => {
                    if ws_write(&mut stream, OP_TEXT, &line).is_err() {
                        client_closed = true;
                        break 'io;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // herdr exited; whatever it printed — its `terminal.closed`
                    // record included — has been queued and drained, so this is
                    // the clean end of the stream.
                    herdr_ended = true;
                    break;
                }
            }
        }
        if herdr_ended {
            let _ = ws_write(&mut stream, OP_CLOSE, &close_payload(1000, ""));
            break;
        }
        // In with the client's bytes.
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => {
                client_closed = true;
                break;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                client_closed = true;
                break;
            }
        }
        loop {
            match parse_client_frame(&mut buf) {
                ClientFrame::Incomplete => break,
                ClientFrame::TooBig => {
                    let _ = ws_write(
                        &mut stream,
                        OP_CLOSE,
                        &close_payload(1009, "frame too large"),
                    );
                    client_closed = true;
                    break 'io;
                }
                ClientFrame::Bad(why) => {
                    crate::logline::logline!(
                        "clauth api: device '{}' sent a malformed frame on pane '{}': {why}",
                        hijack.device,
                        pane_for_log
                    );
                    let _ = ws_write(
                        &mut stream,
                        OP_CLOSE,
                        &close_payload(1002, "protocol error"),
                    );
                    client_closed = true;
                    break 'io;
                }
                ClientFrame::Close => {
                    let _ = ws_write(&mut stream, OP_CLOSE, &close_payload(1000, ""));
                    client_closed = true;
                    break 'io;
                }
                ClientFrame::Ping(payload) => {
                    if ws_write(&mut stream, OP_PONG, &payload).is_err() {
                        client_closed = true;
                        break 'io;
                    }
                }
                // A client's pong answers nothing here; drop it.
                ClientFrame::Pong => {}
                ClientFrame::Text(payload) => {
                    match client_command(&hijack, &input_tx, &payload) {
                        Ok(()) => {}
                        Err(
                            refusal @ (CloseBecause::Policy
                            | CloseBecause::MalformedJson
                            | CloseBecause::NotUtf8),
                        ) => {
                            let (code, why) = match refusal {
                                CloseBecause::Policy => {
                                    (1008, "view tier cannot send terminal input")
                                }
                                CloseBecause::MalformedJson => (1008, "unknown terminal command"),
                                CloseBecause::NotUtf8 => (1007, "invalid utf-8"),
                                CloseBecause::Io => unreachable!(),
                            };
                            let _ = ws_write(&mut stream, OP_CLOSE, &close_payload(code, why));
                            client_closed = true;
                            break 'io;
                        }
                        Err(CloseBecause::Io) => {
                            // herdr's stdin died because it exited; the stdout
                            // half ends the stream through the channel.
                            continue;
                        }
                    }
                }
            }
        }
    }

    // Teardown, in the one order that cannot wedge: the input writer first
    // (dropping its channel ends its recv), then the child (its death unblocks
    // the relay's read), then the relay, then the reap. The input writer is
    // detached by design: an untimed pipe write must not hold the slot.
    drop(input_tx);
    let _ = input_done_rx.recv_timeout(Duration::from_secs(2));
    child.terminate();
    // A relay blocked on a full frames queue (a busy pane behind a vanished
    // client) stays blocked until the queue drains; emptying it here lets the
    // kill's pipe close end the thread instead of timing the wait out.
    while frames_rx.try_recv().is_ok() {}
    if relay_done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        // Detach, never join: a grandchild holding the stdout pipe keeps the
        // relay's `read_line` blocked past the kill, and joining it would hold
        // this connection's slot for a thread that can no longer do anything
        // with it. The relay ends on its own once the pipe closes; it holds no
        // locks and no slot.
        drop(relay);
        crate::logline::logline!(
            "clauth api: terminal relay on pane '{}' outlived its teardown",
            pane_for_log
        );
    }
    child.reap();
    let taken_by = held.and_then(|token| holders.release(&hijack.pane_id, token));
    let why = if lifetime_ended {
        " (connection lifetime)".to_string()
    } else if herdr_ended && !client_closed {
        match taken_by {
            Some(device) => format!(" (taken over by device '{device}')"),
            None => " (herdr ended it)".to_string(),
        }
    } else {
        String::new()
    };
    crate::logline::logline!(
        "clauth api: device '{}' {} stream on pane '{}' closed{why}",
        hijack.device,
        hijack.mode.as_str(),
        pane_for_log,
    );
    send_close_notify(&mut stream);
}

/// Why a client frame ends the connection.
enum CloseBecause {
    Policy,
    MalformedJson,
    /// Invalid UTF-8 in a text frame: RFC 6455 reserves 1007 for exactly this.
    NotUtf8,
    Io,
}

/// One validated client text frame: audit it, forward it. The audit names the
/// frame's arrival and never its bytes (threat-model defect 5) — the stream
/// routinely carries pasted secrets, and the log must not become a second copy.
fn client_command(
    hijack: &Hijack,
    input_tx: &std::sync::mpsc::Sender<Vec<u8>>,
    payload: &[u8],
) -> Result<(), CloseBecause> {
    if hijack.mode != Mode::Control {
        return Err(CloseBecause::Policy);
    }
    if std::str::from_utf8(payload).is_err() {
        return Err(CloseBecause::NotUtf8);
    }
    let value: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(_) => return Err(CloseBecause::MalformedJson),
    };
    let kind = value.get("type").and_then(|t| t.as_str());
    match kind {
        Some("terminal.input") => {
            crate::logline::logline!(
                "clauth api: device '{}' sent terminal.input to pane '{}'",
                hijack.device,
                hijack.pane_for_log
            );
        }
        Some("terminal.resize") => {
            let cols = value.get("cols").and_then(|v| v.as_u64());
            let rows = value.get("rows").and_then(|v| v.as_u64());
            // A resize away from the pane's own geometry is the client's
            // deliberate choice; a phone-shaped one says so, so the log can be
            // read as "the user asked", never as a default the server imposed.
            let phone_shaped = cols.is_some_and(|c| c < 80) || rows.is_some_and(|r| r < 24);
            crate::logline::logline!(
                "clauth api: device '{}' resized pane '{}' to {}x{}{}",
                hijack.device,
                hijack.pane_for_log,
                cols.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                rows.map(|r| r.to_string()).unwrap_or_else(|| "?".into()),
                if phone_shaped {
                    " (phone-shaped, deliberate)"
                } else {
                    ""
                }
            );
        }
        Some("terminal.scroll") | Some("terminal.release") => {}
        _ => return Err(CloseBecause::MalformedJson),
    }
    let mut line = payload.to_vec();
    line.push(b'\n');
    input_tx.send(line).map_err(|_| CloseBecause::Io)
}

fn send_close_notify(
    stream: &mut rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>,
) {
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

// --- The WebSocket wire, server side only ---------------------------------
//
// Server frames are unmasked, FIN-only (no fragmentation, no extensions: rsv
// bits on a client frame are refused). Client frames must be masked.

const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PONG: u8 = 0xA;

/// One server frame: FIN + opcode, server frames carry no mask.
fn ws_write<W: Write>(w: &mut W, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(10);
    head.push(0x80 | opcode);
    match payload.len() {
        len @ 0..=125 => head.push(len as u8),
        len @ 126..=65_535 => {
            head.push(126);
            head.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            head.push(127);
            head.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    w.write_all(&head)?;
    w.write_all(payload)
}

/// A close frame's payload: the code, and the reason when one is worth sending.
fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(reason.as_bytes());
    payload
}

/// One client frame, parsed out of `buf`, or why there is not one yet.
enum ClientFrame {
    Text(Vec<u8>),
    Ping(Vec<u8>),
    /// A client pong. Nothing here pings first, so its payload is drained and
    /// dropped.
    Pong,
    Close,
    /// A complete frame that violates the protocol; `why` names it for the log.
    Bad(&'static str),
    TooBig,
    Incomplete,
}

fn parse_client_frame(buf: &mut Vec<u8>) -> ClientFrame {
    if buf.len() < 2 {
        return ClientFrame::Incomplete;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = b0 & 0x80 != 0;
    let rsv = b0 & 0x70;
    let opcode = b0 & 0x0F;
    let masked = b1 & 0x80 != 0;
    let len7 = (b1 & 0x7F) as usize;
    if rsv != 0 {
        return ClientFrame::Bad("rsv bits set without a negotiated extension");
    }
    if !masked {
        return ClientFrame::Bad("client frame not masked");
    }
    let opcode_str = match opcode {
        0x0 => return ClientFrame::Bad("fragmentation is not spoken here"),
        0x1 | 0x2 | 0x8 | 0x9 | 0xA => opcode,
        _ => return ClientFrame::Bad("unknown opcode"),
    };
    let control = matches!(opcode_str, 0x8..=0xA);
    if control && (!fin || len7 > 125) {
        return ClientFrame::Bad("control frames are FIN-only and at most 125 bytes");
    }
    if !control && !fin {
        return ClientFrame::Bad("fragmentation is not spoken here");
    }
    let (len, header_len) = match len7 {
        126 => {
            if buf.len() < 4 {
                return ClientFrame::Incomplete;
            }
            let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            // RFC 6455 §5.2: the minimal encoding only. A non-minimal one is
            // not a frame any conforming client sends.
            if len < 126 {
                return ClientFrame::Bad("non-minimal extended length");
            }
            (len, 4)
        }
        127 => {
            if buf.len() < 10 {
                return ClientFrame::Incomplete;
            }
            let mut wide = [0u8; 8];
            wide.copy_from_slice(&buf[2..10]);
            let len = u64::from_be_bytes(wide) as usize;
            if len < 65_536 {
                return ClientFrame::Bad("non-minimal extended length");
            }
            (len, 10)
        }
        len => (len, 2),
    };
    if len > MAX_CLIENT_FRAME {
        return ClientFrame::TooBig;
    }
    if buf.len() < header_len + 4 + len {
        return ClientFrame::Incomplete;
    }
    let mask: [u8; 4] = [
        buf[header_len],
        buf[header_len + 1],
        buf[header_len + 2],
        buf[header_len + 3],
    ];
    let start = header_len + 4;
    let mut payload = buf[start..start + len].to_vec();
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    buf.drain(..start + len);
    match opcode_str {
        0x1 => ClientFrame::Text(payload),
        0x2 => ClientFrame::Bad("binary frames are not terminal commands"),
        // A close carries the two-byte code, optionally followed by a reason;
        // a one-byte payload is no close any conforming client sends.
        0x8 if payload.len() == 1 => ClientFrame::Bad("close payload without a code"),
        0x8 => ClientFrame::Close,
        0x9 => ClientFrame::Ping(payload),
        _ => ClientFrame::Pong,
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_terminal.rs"]
mod tests;
