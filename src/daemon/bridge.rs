//! The mod bridge: `~/.clauth/bridge.sock`, the session a socket peer resolves
//! to, the control routes that queue commands for a mod (`POST /event`,
//! `GET /poll`), and the per-ask wait (`GET /wait`) that parks a held check on
//! its own ask without touching the session's poll.
//!
//! The daemon binds the socket only under `--listen`, below the singleton claim.
//! Every event, poll and command resolves the peer pid back to an
//! [`Instance`] through the [`PeerResolver`] — never taking a session id off the
//! wire — and keys the session per `(pid, procStart, sessionId)` so a recycled
//! pid, a `/clear` or a second live process cannot cross sessions or swallow
//! another session's commands. The one fallback is an `end` the resolver no
//! longer knows the peer for, taken under the instance the hub already holds
//! for that pid ([`Hub::held_for_end`]) and only when it names that instance's
//! own session. Linux and macOS only: the whole module is `#[cfg(unix)]` at
//! its declaration in `mod.rs`.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::daemon::api::http::sanitize_for_log;
use crate::logline::logline;

/// One event body, in bytes. A mod event is small by construction (a row wake
/// carries its `uuid`, never its content; an ask carries its tool input), so a
/// body past this bound is refused 413 and the mod falls back to Claude Code's
/// own dialog rather than holding a check the daemon cannot relay.
const BODY_LIMIT: usize = 1024 * 1024;

/// A transcript record relayed past this many bytes goes out as a stub naming
/// its byte offset and size, never the row (SB-10); the client pages it through
/// the history route. The line's length without its newline is what is judged.
const RECORD_LIMIT: usize = 1024 * 1024;

/// Frames one subscription buffers before it is dropped: a full buffer is a
/// client not draining its stream, so the hub drops it and the client
/// reconnects for a fresh snapshot (SB-10).
pub(crate) const SUBSCRIBER_BOUND: usize = 64;

/// How often the sweep closes silent instances, so a silent session's end
/// frames go out near the silence bound, never at the next interaction.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// An instance whose poll has been silent this long closes: twice the server's
/// poll wait, the bound the mod's `$.http.fetch` stays under.
const SILENCE_LIMIT: Duration = Duration::from_secs(40);

/// The wait the bridge server hands every `GET /poll` that finds no command.
const POLL_WAIT: Duration = Duration::from_secs(20);

/// Cap on one request's head (request line plus headers). A real bridge head
/// is a few hundred bytes, so 16 KiB is far above it and no honest request
/// nears the cap, while a peer cannot make the daemon buffer an unbounded
/// head (SB-10).
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// The whole request's wall-clock budget — head and body together — so a peer
/// trickling one byte just under any per-read bound is cut off here, not after
/// days (SB-10). Deliberately not per read.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

/// How long one answer write may block: a peer that never reads a large poll
/// answer must not hold a connection thread forever (SB-10).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on concurrent event requests and every request's read phase — the work
/// the bridge does before a poll parks. Commands never arrive over the socket:
/// they leave inside poll answers. A parked poll drops this slot before it
/// parks, so every live session's parked poll never eats the slot an event
/// needs; the size matches the REST API's own connection cap (SB-10).
const MAX_EVENT_CONNECTIONS: usize = 32;

/// Cap on parked polls: each live mod parks one for up to [`POLL_WAIT`]. Far
/// above any realistic session count, while still bounding the threads a
/// same-user flood can park (SB-10's daemon-wide poll cap).
const MAX_PARKED_POLLS: usize = 256;

/// Cap on one instance's queued commands, counted together with any a poll has
/// drained and is still writing (the in-flight count). A live mod drains the
/// queue on every poll, so the queue stays empty or near-empty; the cap only
/// bites a mod that stopped polling, whose commands would otherwise grow
/// without bound while every command route kept answering 200 (SB-10, SB-19).
/// 8192 doubles the write-timeout pin's 4096-command batch, so the cap never
/// bites a live mod while still bounding one stopped-reading instance.
pub(crate) const MAX_QUEUED_COMMANDS: usize = 8192;

/// Cap on one instance's parked per-ask waits. Each held check parks its own
/// `GET /wait`; the cap bounds the parked connections one instance can pin
/// (SB-10's per-instance wait cap), so a mod that asks while already holding
/// many checks falls back to the stock dialog instead of parking without
/// bound.
pub(crate) const MAX_WAITS_PER_INSTANCE: usize = 8;

/// How long the accept loop pauses after an error it cannot attribute to one
/// connection, so a persistent failure cannot spin a core (mirrors the REST
/// API's accept loop).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The two wall-clock bounds on one connection, injected so a test drives a
/// deadline in milliseconds instead of minutes; production passes
/// [`Limits::DEFAULT`] (mirrors the REST API's `Limits`).
#[derive(Clone, Copy)]
struct Limits {
    request_deadline: Duration,
    write_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_deadline: REQUEST_DEADLINE,
            write_timeout: WRITE_TIMEOUT,
        }
    }
}

/// Releases one connection-budget slot however the handler thread ends, panic
/// included — a leaked slot would permanently shrink the cap (mirrors the REST
/// API's `ConnectionSlot`).
struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A resolved session: the id the peer's process is in now, and the process
/// start the record carries. The start keys a recycled pid apart from its
/// predecessor; the id keys a `/clear` apart.
#[derive(Clone, Debug)]
pub(crate) struct Instance {
    pub(crate) session_id: String,
    pub(crate) proc_start: String,
}

/// Resolves a socket peer pid to its [`Instance`], or `None` for a pid nothing
/// knows. The daemon fills it with [`production_resolver`]; a test fills it
/// with a table it can rewrite mid-test.
pub(crate) type PeerResolver = Box<dyn Fn(u32) -> Option<Instance> + Send + Sync>;

/// One `/events` subscriber: a bounded channel of `(event, data)` frames. A
/// full channel drops the subscription — the stream ends and the client
/// reconnects for a fresh snapshot (SB-10).
/// The hub dropped a subscription: its channel filled and was removed (SB-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dropped;

pub(crate) struct Subscription {
    rx: Receiver<(String, Value)>,
}

impl Subscription {
    /// The next frame within `timeout`, or `None` when none arrived (a zero
    /// timeout polls) or the hub dropped this subscription. Test-only: the
    /// events loop drives the non-consuming [`try_next`] instead.
    #[cfg(test)]
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<(String, Value)> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// The next frame, or the drop verdict, from one non-consuming read: a
    /// frame and the verdict come from the same call, so a frame landing between
    /// a drain and a drop check is never eaten (SB-10).
    pub(crate) fn try_next(&self) -> Result<Option<(String, Value)>, Dropped> {
        match self.rx.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(Dropped),
        }
    }
}

/// One open ask, in open order, with what the snapshot and the `session.ask`
/// frame carry.
struct OpenAsk {
    tool: Value,
    input: Value,
    agent_id: Value,
    /// The 1-based line the mod found an `Edit`'s `old_string` on, see
    /// [`posted_start_line`]. The daemon opens no file for it.
    start_line: Option<u64>,
    /// Whether the mod held this check for a device answer: `remote_flag()` at
    /// open time. An unheld ask is shown and closed, but never answered (SB-19).
    held: bool,
}

/// An `ask_opened` post's `start_line`, kept only as a positive integer: the
/// value goes on to every paired device, so a string, zero, a negative or a
/// float reads as absent.
fn posted_start_line(value: &Value) -> Option<u64> {
    value
        .get("start_line")
        .and_then(Value::as_u64)
        .filter(|line| *line > 0)
}

/// One tailed transcript file (the main file or a subagent's): the complete-
/// line offset and the torn tail read so far. A wake reads only the bytes past
/// the retained head, so a growing torn line is never re-read each wake, and
/// the read is capped past the last newline (SB-10).
#[derive(Clone)]
struct FileTail {
    path: PathBuf,
    /// Byte offset of the torn line's head (the next line to relay once
    /// complete); complete lines before it are consumed.
    offset: u64,
    /// The torn line's head bytes already read, capped at [`RECORD_LIMIT`]: a
    /// line past that relays as a stub, whose bytes are unneeded (SB-10).
    torn: Vec<u8>,
    /// The torn line's true length so far, past the cap: a stub's `size`.
    torn_len: u64,
    /// The file's `(dev, ino)` at first sight: a replaced file resets the
    /// offset to 0 rather than reading from mid-line (SB-11).
    dev: u64,
    ino: u64,
}

/// The hub: one per daemon, holding every mod instance in memory (never a file,
/// never a `live_sessions` row — SB-5) and the one place a command becomes a
/// queued mod command.
///
/// Lock order (acquire in this order, never reversed): `inner` → `subs` →
/// `push`. The three are plain std mutexes; a future taker of a later lock then
/// an earlier one deadlocks against any earlier-then-later site. No peer
/// resolve, device-store read, registry read, probe or transcript store walk
/// runs under any of them: the account resolve, the peer re-resolve and the
/// transcript locate run after the locks drop (SB-10).
pub(crate) struct Hub {
    listening: bool,
    resolver: PeerResolver,
    inner: Mutex<Inner>,
    changed: Condvar,
    /// Live `/events` subscribers; a full channel is dropped on the next
    /// broadcast (SB-10).
    subs: Mutex<Vec<SyncSender<(String, Value)>>>,
    /// The push sender's inbox, wired by the daemon after it stands: an ask
    /// opening a new wait announces the session's `blocked` transition through
    /// it (WP-8).
    push: Mutex<Option<crate::push::Queue>>,
    /// Test-only: the virtual clock's offset past [`Instant::now`], so a test
    /// pins the silence close without sleeping 40 s.
    #[cfg(test)]
    clock_offset: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct Inner {
    instances: std::collections::HashMap<u32, Live>,
    /// The next [`Live::generation`]; bumped only when a new instance is made.
    next_generation: u64,
    /// Pids already logged as unknown, for the SB-3 "not registered, logged
    /// once" memo.
    unknown_logged: HashSet<u32>,
}

/// One live claude instance, keyed by its peer pid. Liveness is the poll:
/// `last_poll` moves on every poll, and a poll silent past [`SILENCE_LIMIT`]
/// closes the instance (SB-13).
struct Live {
    proc_start: String,
    session_id: String,
    seq_high: u64,
    /// Open asks in open order, with what the snapshot and frames carry.
    open_asks: Vec<(String, OpenAsk)>,
    /// Ask ids already closed, so a device answer's later `ask_closed` event
    /// and a second answer both read as closed. The value is the answer a
    /// device queued (`null` for a desk close or the session's end), kept so a
    /// wait arriving after the answer returns it at once; the wait that
    /// delivers the answer drops the payload, keeping only the id, so a live
    /// instance never accumulates an answer per closed ask (W4).
    closed_asks: HashMap<String, Value>,
    /// Whether the current wait's `blocked` push is reserved: set in the same
    /// lock hold that decides to push, rolled back by [`Hub::note_ask_opened`]
    /// when no push queue is wired, and cleared when the last open ask closes.
    /// A second ask of the same wait reads the reservation and pushes nothing
    /// (WP-8).
    announced: bool,
    /// The `start` event's cwd and interactive, for the snapshot.
    cwd: Value,
    interactive: Value,
    /// The main transcript, located at registration and tailed on each
    /// main-file wake; `None` until the file is found (SB-11).
    main_file: Option<FileTail>,
    /// Subagent transcripts by `agent_id`, each tailed from its first sight.
    subagent_files: HashMap<String, FileTail>,
    /// Subagent files present at registration, by agent stem, with their length
    /// then: a file listed here starts at that length, one created after
    /// registration starts at 0 (SB-11).
    subagent_snapshot: HashMap<String, u64>,
    commands: VecDeque<Value>,
    /// Commands a poll has drained and is still writing its answer for. They
    /// stay counted against [`MAX_QUEUED_COMMANDS`] until the write settles, so
    /// a write in flight never opens headroom a command route fills past the
    /// cap (A#1).
    inflight: usize,
    /// Bumped whenever a poll parks on this instance, so a replaced poll can
    /// tell it was replaced and stop waiting.
    poll_epoch: u64,
    /// Parked per-ask waits (`GET /wait`) held right now, keyed by ask id and
    /// bounded by [`MAX_WAITS_PER_INSTANCE`] across the map. A wait answers at
    /// once when its ask is already closed, so it never spends a slot; only a
    /// wait parked on an open ask counts. The per-ask key lets a poll that
    /// delivered an answer command leave the payload for a wait still parked on
    /// that ask.
    parked_waits: HashMap<String, usize>,
    /// Monotonic per hub: a parked poll captures this and compares on wake, so
    /// a `/clear` or recycled pid that replaced the instance never feeds its
    /// commands to the abandoned poll (SB-4).
    generation: u64,
    /// Whether a transcript tail is running for this instance right now (the
    /// registration locate included). A wake arriving during one queues its own
    /// file in `rewake` and returns; the finishing tail drains `rewake`, so
    /// concurrent wakes never copy the same offset, relay the same rows, or
    /// relay through the wrong file (SB-11).
    tailing: bool,
    /// File re-runs queued while a tail ran, keyed by file: `None` is the main
    /// file, `Some(agent_id)` a subagent's. The finishing tail drains the set
    /// and runs each file's relay once, except while the instance is ending:
    /// the end's final tail keeps them and drains them before the removal
    /// (SB-11).
    rewake: HashSet<Option<String>>,
    /// Set while the `end` event's final tail is in flight: a duplicate end
    /// arriving before the removal answers 200 and touches nothing, so one end
    /// owns the final tail, the removal and the end frames. An ending instance
    /// takes no delivery: the mod stops polling before it posts `end`, so a
    /// command queued during the final read would be acknowledged and never
    /// run.
    ending: bool,
    last_poll: Instant,
}

impl Live {
    fn new(resolved: &Instance, now: Instant, generation: u64) -> Self {
        // Registration owns the tail slot from the start: `tailing` is `true`
        // until [`Hub::locate_at_registration`] ends it, so an append wake
        // racing that locate queues a re-run instead of locating the file at
        // offset 0 and replaying history (SB-11). `main_file` starts `None` and
        // is written by the same registration.
        Self {
            proc_start: resolved.proc_start.clone(),
            session_id: resolved.session_id.clone(),
            seq_high: 0,
            open_asks: Vec::new(),
            closed_asks: HashMap::new(),
            announced: false,
            cwd: Value::Null,
            interactive: Value::Null,
            main_file: None,
            subagent_files: HashMap::new(),
            subagent_snapshot: HashMap::new(),
            commands: VecDeque::new(),
            inflight: 0,
            poll_epoch: 0,
            parked_waits: HashMap::new(),
            generation,
            tailing: true,
            rewake: HashSet::new(),
            ending: false,
            last_poll: now,
        }
    }

    /// The commands this instance owns right now: its queued commands plus any
    /// a poll has drained and is still writing. The cap counts both, so a
    /// stopped-reading mod can never pin more than [`MAX_QUEUED_COMMANDS`]
    /// commands even while one poll answer after another fails to write.
    fn owned(&self) -> usize {
        self.commands.len() + self.inflight
    }
}

/// What [`Hub::deliver`] found on the mod side of a prompt or interrupt.
pub(crate) enum ModDelivery {
    /// Exactly one live instance: the command is queued for its poll.
    Delivered,
    /// Two live instances resolve to the id: refused, never handed to either.
    Ambiguous,
    /// The one live instance's queue is at [`MAX_QUEUED_COMMANDS`]: refused
    /// 503 rather than queued past the cap (A#1).
    Busy,
    /// No live instance: the route falls back to herdr.
    NoMod,
}

/// What [`Hub::answer`] found on the mod side of an ask answer.
pub(crate) enum AskOutcome {
    /// The ask was open; the answer is queued and the ask closes.
    Queued,
    /// Two live instances resolve to the id.
    Ambiguous,
    /// The ask is open but the instance's queue is at [`MAX_QUEUED_COMMANDS`]:
    /// refused 503, the ask stays open for a retry (A#1).
    Busy,
    /// A `decision` sent to a question ask, or `answers` sent to any other
    /// ask: refused, nothing queued, the ask stays open.
    WrongForm,
    /// An answer or note key is not byte-equal to one of the open ask's
    /// question texts: refused, nothing queued, the ask stays open.
    UnknownQuestion,
    /// An answer is empty: refused, nothing queued, the ask stays open.
    EmptyAnswer,
    /// The ask was answered (or closed at the desk) already.
    AskClosed,
    /// The instance never held that ask id.
    AskNotFound,
    /// No live instance for the id.
    NoSession,
}

/// The commands one poll drained, kept so `serve` can put them back when the
/// answer write fails (A#1). `generation` names the instance they came from: a
/// closed or replaced instance drops them rather than leaking them into its
/// successor's queue, and `inflight` stays counted on the live instance until
/// the write settles.
struct Drained {
    peer_pid: u32,
    generation: u64,
    commands: Vec<Value>,
}

/// One poll's full answer: the wire pair every caller reads, plus the drained
/// commands only `serve` uses to requeue on a failed write.
struct PollAnswer {
    status: u16,
    body: Value,
    drained: Option<Drained>,
}

impl PollAnswer {
    /// An answer that drained nothing, so there is nothing to requeue.
    fn empty(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            drained: None,
        }
    }

    /// A 200 carrying `commands`, and the drained token when it is non-empty.
    fn commands(peer_pid: u32, generation: u64, remote: bool, commands: Vec<Value>) -> Self {
        let body = poll_body(remote, &commands);
        let drained = (!commands.is_empty()).then(|| Drained {
            peer_pid,
            generation,
            commands,
        });
        Self {
            status: 200,
            body,
            drained,
        }
    }
}

impl Hub {
    pub(crate) fn new(listening: bool, resolver: PeerResolver) -> Arc<Self> {
        Arc::new(Self {
            listening,
            resolver,
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            subs: Mutex::new(Vec::new()),
            push: Mutex::new(None),
            #[cfg(test)]
            clock_offset: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The hub's clock. Test builds move it with [`advance_clock`]; production
    /// runs on the real monotonic clock.
    fn now(&self) -> Instant {
        #[cfg(test)]
        {
            Instant::now() + Duration::from_nanos(self.clock_offset.load(Ordering::Relaxed))
        }
        #[cfg(not(test))]
        {
            Instant::now()
        }
    }

    /// Move the hub's virtual clock forward, so a test drives the silence close
    /// ([`SILENCE_LIMIT`]) without sleeping.
    #[cfg(test)]
    pub(crate) fn advance_clock(&self, delta: Duration) {
        self.clock_offset
            .fetch_add(delta.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Test-only: wait until a poll has parked on `pid`'s instance — its poll
    /// epoch reached `min_epoch` — signalled through `changed`, so a test
    /// orders two threads by a flag rather than a sleep (B#4). Returns `false`
    /// when the timeout elapses first. Sound because a poll holds the hub mutex
    /// from its epoch bump through `Condvar::wait_timeout`, so the epoch is
    /// visible here exactly once the poll is parked.
    #[cfg(test)]
    pub(crate) fn wait_until_parked(&self, pid: u32, min_epoch: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        while guard
            .instances
            .get(&pid)
            .is_none_or(|live| live.poll_epoch < min_epoch)
        {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (g, _) = self
                .changed
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
        }
        true
    }

    /// Open a bounded frame subscription. A full channel is dropped on the next
    /// broadcast, ending the client's stream (SB-10).
    pub(crate) fn subscribe(&self) -> Subscription {
        let (tx, rx) = sync_channel(SUBSCRIBER_BOUND);
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).push(tx);
        Subscription { rx }
    }

    /// Broadcast one frame to every live subscriber, dropping any whose channel
    /// is full — never blocking the hub on a slow client (SB-10).
    fn broadcast(&self, event: &str, data: Value) {
        let mut subs = self.subs.lock().unwrap_or_else(|p| p.into_inner());
        subs.retain(|tx| tx.try_send((event.to_string(), data.clone())).is_ok());
    }

    /// Drop every live subscription now, so a test ends a stream on a frame it
    /// observed rather than on a wall-clock deadline (the stream returns `Ok`
    /// once its channel reads `Dropped`). Test-only: production subscriptions
    /// end by filling their own channel (SB-10).
    #[cfg(test)]
    pub(crate) fn drop_subscriptions(&self) {
        self.subs.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// The end frames for a closing instance: every open ask closes `by:
    /// session_end`, then the `end` status (SB-13). Every removal path calls
    /// this, so the one notify here wakes any wait (and poll) parked on the
    /// instance, whatever path removed it.
    fn emit_instance_end(&self, live: &Live) {
        for (ask, _) in &live.open_asks {
            self.broadcast(
                "session.ask",
                json!({"session": live.session_id, "ask": ask, "state": "closed", "by": "session_end"}),
            );
        }
        self.broadcast(
            "session.status",
            json!({"session": live.session_id, "state": "end"}),
        );
        self.changed.notify_all();
    }

    /// The snapshot a (re)connecting stream opens with: every live session and
    /// every open ask, sessions sorted by id and asks by session then open
    /// order. The revalidation closes moved or silent instances first, with
    /// the peer resolve running off every hub lock (SB-10, SB-13).
    pub(crate) fn snapshot(&self) -> Value {
        let records: Vec<(u32, String, String)> = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let now = self.now();
            self.close_silent(&mut inner, now);
            inner
                .instances
                .iter()
                .map(|(pid, live)| (*pid, live.proc_start.clone(), live.session_id.clone()))
                .collect()
        };
        let moved = self.moved_records(&records);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.close_moved(&mut inner, &moved);
        let mut sessions: Vec<&Live> = inner.instances.values().collect();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let session_entries: Vec<Value> = sessions
            .iter()
            .map(|live| json!({"session": live.session_id, "cwd": live.cwd, "interactive": live.interactive}))
            .collect();
        let mut asks = Vec::new();
        for live in &sessions {
            for (ask, info) in &live.open_asks {
                asks.push(json!({
                    "session": live.session_id,
                    "ask": ask,
                    "tool": info.tool,
                    "input": info.input,
                    "agent_id": info.agent_id,
                    "start_line": info.start_line,
                }));
            }
        }
        json!({"sessions": session_entries, "asks": asks})
    }

    /// The periodic silence sweep: close every instance silent past the bound
    /// now, so its end frames go out near the bound, not at the next
    /// interaction (SB-13).
    pub(crate) fn sweep(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.close_silent(&mut inner, self.now());
    }

    /// Wire the push sender's inbox (production, after the sender starts): an
    /// ask opening a new wait announces the session's `blocked` transition
    /// through it (WP-8).
    pub(crate) fn wire_push(&self, queue: crate::push::Queue) {
        *self.push.lock().unwrap_or_else(|p| p.into_inner()) = Some(queue);
    }

    /// One event from the mod at `peer_pid`, resolved to a session and handled.
    /// The session comes from the resolver; only an `end` from a peer it no
    /// longer knows falls back to the instance the hub holds for that pid
    /// ([`Hub::held_for_end`]), then runs the same `end` path.
    pub(crate) fn event(&self, peer_pid: u32, body: &[u8]) -> (u16, Value) {
        if body.len() > BODY_LIMIT {
            logline!(
                "clauth bridge: peer {peer_pid} refused: body_len={}",
                body.len()
            );
            return (413, error_value("body_too_large"));
        }
        let parsed = serde_json::from_slice::<Value>(body).ok();
        let resolved = (self.resolver)(peer_pid);
        let fallback = resolved.is_none();
        let Some(resolved) = resolved.or_else(|| {
            parsed
                .as_ref()
                .and_then(|value| self.held_for_end(peer_pid, value))
        }) else {
            self.log_unknown_peer(peer_pid);
            return (403, error_value("unknown_peer"));
        };
        let Some(value) = parsed else {
            return (400, error_value("bad_request"));
        };
        let Some(named) = value.get("session").and_then(Value::as_str) else {
            return (400, error_value("bad_request"));
        };
        let Some(seq) = value.get("seq").and_then(Value::as_u64) else {
            return (400, error_value("bad_request"));
        };
        if named != resolved.session_id {
            logline!(
                "clauth bridge: peer {peer_pid} refused session '{}' (resolved '{}') body_len={}",
                sanitize_for_log(named),
                sanitize_for_log(&resolved.session_id),
                body.len()
            );
            return (403, error_value("session_mismatch"));
        }
        // Validate the kind and its fields before the sequence advances or the
        // instance is created (A#13): a refused event must not consume its seq.
        let kind = match value.get("kind").and_then(Value::as_str) {
            Some(k @ ("start" | "turn_start" | "turn_end" | "append" | "end")) => k,
            Some("ask_opened") if value.get("ask").and_then(Value::as_str).is_some() => {
                "ask_opened"
            }
            Some("ask_closed") if value.get("ask").and_then(Value::as_str).is_some() => {
                "ask_closed"
            }
            _ => return (400, error_value("bad_request")),
        };

        // The device store is read once per `ask_opened` (A#15), before any hub
        // lock is taken, so the read never stalls the hub (SB-10).
        let ask_hold = (kind == "ask_opened").then(|| self.remote_flag());

        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let now = self.now();
        self.close_silent(&mut inner, now);

        // `end` goes through the same reconcile path so a moved record closes
        // the old instance before the sequence is judged (A#14); an end taken
        // under the held instance reconciles onto that same instance. The final
        // transcript tail runs between the judge and the removal, with every
        // read off the hub lock, so the rows no wake reached relay before the
        // `end` status closes the stream's view. An `end` for a pid whose
        // instance this call just minted emits nothing: no `start` was ever
        // broadcast for it.
        if kind == "end" {
            // The fallback identity was read before this lock: if the instance
            // it named was replaced since, the end stays an unknown peer rather
            // than reconciling onto the successor.
            if fallback
                && !inner.instances.get(&peer_pid).is_some_and(|live| {
                    live.session_id == resolved.session_id && live.proc_start == resolved.proc_start
                })
            {
                drop(inner);
                self.log_unknown_peer(peer_pid);
                return (403, error_value("unknown_peer"));
            }
            let generation = {
                let (live, created) = self.reconcile(&mut inner, peer_pid, &resolved, now);
                if seq <= live.seq_high {
                    // A refused event must not leave the instance it just minted
                    // (A#13): a fresh instance still holds its registration tail
                    // slot, so leaving it would wedge every later tail.
                    if created {
                        inner.instances.remove(&peer_pid);
                    }
                    return (409, error_value("stale_sequence"));
                }
                // A first-contact end mints the instance this call owns: nothing
                // to relay, nothing to emit, and the fresh instance still holds
                // its registration tail slot, so remove it (A#13).
                if created {
                    inner.instances.remove(&peer_pid);
                    return (200, json!({ "ok": true }));
                }
                // A duplicate end while the first one still runs its final
                // tail: the first end owns the removal and the end frames.
                if live.ending {
                    return (200, json!({ "ok": true }));
                }
                live.ending = true;
                live.generation
            };
            drop(inner);

            // The final tail: relay the main transcript and each subagent file
            // the instance already holds a tail for, once more. The instance
            // stays live through the reads and is removed only once they
            // settle and no re-run is left, so every record of these reads
            // lands before the end frame.
            let mut queue = VecDeque::new();
            {
                let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                let Some(guard) = self.end_wait_tail_slot(guard, peer_pid, generation) else {
                    return (200, json!({ "ok": true }));
                };
                let live = guard
                    .instances
                    .get(&peer_pid)
                    .unwrap_or_else(|| unreachable!("the slot wait saw the instance"));
                queue.push_back(None);
                queue.extend(live.subagent_files.keys().cloned().map(Some));
            }
            loop {
                self.drain_tail_queue(peer_pid, named, &mut queue);
                let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                let Some(mut guard) = self.end_wait_tail_slot(guard, peer_pid, generation) else {
                    return (200, json!({ "ok": true }));
                };
                let live = guard
                    .instances
                    .get(&peer_pid)
                    .unwrap_or_else(|| unreachable!("the slot wait saw the instance"));
                if live.rewake.is_empty() {
                    let live = guard
                        .instances
                        .remove(&peer_pid)
                        .unwrap_or_else(|| unreachable!("the slot wait saw the instance"));
                    self.emit_instance_end(&live);
                    return (200, json!({ "ok": true }));
                }
                queue = guard
                    .instances
                    .get_mut(&peer_pid)
                    .unwrap_or_else(|| unreachable!("the slot wait saw the instance"))
                    .rewake
                    .drain()
                    .collect();
            }
        }

        let (live, created) = self.reconcile(&mut inner, peer_pid, &resolved, now);
        let generation = live.generation;
        if seq <= live.seq_high {
            // The same A#13 rule as the `end` path: a refused first contact
            // must not leave its freshly minted instance holding the tail slot.
            if created {
                inner.instances.remove(&peer_pid);
            }
            return (409, error_value("stale_sequence"));
        }
        live.seq_high = seq;

        let mut opened_wait = false;
        let mut append_agent: Option<Option<String>> = None;
        let (status, body) = match kind {
            "start" => {
                live.cwd = value.get("cwd").cloned().unwrap_or(Value::Null);
                live.interactive = value.get("interactive").cloned().unwrap_or(Value::Null);
                self.broadcast(
                    "session.status",
                    json!({"session": named, "state": "start", "cwd": live.cwd, "interactive": live.interactive}),
                );
                (200, json!({ "ok": true }))
            }
            "turn_start" | "turn_end" => {
                let turn = value.get("turn").cloned().unwrap_or(Value::Null);
                self.broadcast(
                    "session.status",
                    json!({"session": named, "state": kind, "turn": turn}),
                );
                (200, json!({ "ok": true }))
            }
            "append" => {
                append_agent = Some(
                    value
                        .get("agent_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
                (200, json!({ "ok": true }))
            }
            "ask_opened" => {
                let ask = value.get("ask").and_then(Value::as_str).unwrap_or_default();
                // The device store was read before the lock (A#15, SB-10); the
                // flag rides the response for the mod's hold decision (SB-9,
                // MB-1).
                let hold = ask_hold.unwrap_or(false);
                let tool = value.get("tool").cloned().unwrap_or(Value::Null);
                let input = value.get("input").cloned().unwrap_or(Value::Null);
                let agent_id = value.get("agent_id").cloned().unwrap_or(Value::Null);
                let start_line = posted_start_line(&value);
                // A wait announces its `blocked` push once: the decision and the
                // reservation share this lock hold, so a second ask of the same
                // wait handled before the push queues reads the reservation and
                // pushes nothing. `note_ask_opened` rolls the reservation back
                // when no push queue is wired, so an ask opened before the queue
                // still announces once it is (WP-8).
                let announce = !live.announced;
                if announce {
                    live.announced = true;
                }
                live.closed_asks.remove(ask);
                match live.open_asks.iter_mut().find(|(id, _)| id == ask) {
                    Some((_, info)) => {
                        info.tool = tool.clone();
                        info.input = input.clone();
                        info.agent_id = agent_id.clone();
                        info.start_line = start_line;
                        info.held |= hold;
                    }
                    None => live.open_asks.push((
                        ask.to_string(),
                        OpenAsk {
                            tool: tool.clone(),
                            input: input.clone(),
                            agent_id: agent_id.clone(),
                            start_line,
                            held: hold,
                        },
                    )),
                }
                self.broadcast(
                    "session.ask",
                    json!({"session": named, "ask": ask, "state": "opened", "tool": tool, "input": input, "agent_id": agent_id, "start_line": start_line}),
                );
                opened_wait = announce;
                (200, json!({ "ok": true, "remote": hold, "hold": hold }))
            }
            "ask_closed" => {
                let ask = value.get("ask").and_then(Value::as_str).unwrap_or_default();
                // Only the mod's own closers pass through: an ask its interrupt
                // aborted posts `interrupt`; anything else it sends — `desk`, a
                // daemon-owned stamp like `device` or `session_end`, any other
                // string, or none — reads as the desk.
                let by = match value.get("by").and_then(Value::as_str) {
                    Some("interrupt") => "interrupt",
                    _ => "desk",
                };
                if !live.closed_asks.contains_key(ask) {
                    live.open_asks.retain(|(id, _)| id != ask);
                    live.closed_asks.insert(ask.to_string(), Value::Null);
                    // The last open ask closed: the wait ends, so the next ask
                    // opens a new wait and announces again (WP-8).
                    if live.open_asks.is_empty() {
                        live.announced = false;
                    }
                    // Wake a wait parked on this ask (and the poll, which finds
                    // nothing to drain): a desk close queues no command, so this
                    // notify is the close's only wake.
                    self.changed.notify_all();
                    self.broadcast(
                        "session.ask",
                        json!({"session": named, "ask": ask, "state": "closed", "by": by}),
                    );
                } else {
                    // The mod's hold was already lost (a `503 busy`, `remote:false`
                    // or a rejected fetch), so its late `ask_closed` is the ask's
                    // last consumer: drop the kept payload, keep the id.
                    live.closed_asks.insert(ask.to_string(), Value::Null);
                }
                (200, json!({ "ok": true }))
            }
            _ => unreachable!("the kind was validated above"),
        };
        drop(inner);

        // The locate, the transcript tail and the ask-open push all run with no
        // hub lock held: a store walk, a file read or a herdr probe must never
        // stall the hub (SB-10, SB-11, WP-8).
        if created {
            self.locate_at_registration(peer_pid, named, generation);
        }
        if let Some(agent_id) = append_agent {
            self.relay_append(peer_pid, named, agent_id.as_deref());
        }
        if opened_wait {
            self.note_ask_opened(peer_pid, generation);
        }
        (status, body)
    }

    /// Whether a live mod instance holds `session` (WP-8): the follower skips
    /// its blocked push for a session the mod owns. A scan of the instance map
    /// under `inner` only — no IO, probe or store walk. The silence close runs
    /// first under the same lock (it does no IO), so an instance silent past
    /// the bound stops suppressing the follower's push here instead of up to
    /// [`SWEEP_INTERVAL`] later.
    pub(crate) fn has_live_instance(&self, session: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.close_silent(&mut inner, self.now());
        inner
            .instances
            .values()
            .any(|live| live.session_id == session)
    }

    /// The ask-open half of the `blocked` push (WP-8): the mod is the only push
    /// source for its session, so a new wait announces through the wired inbox,
    /// with no pane probe. The account resolve runs off every hub lock — it
    /// reads the registry, which must never stall the hub (SB-10). The wait
    /// latch is the instance's `announced`, reserved in the `ask_opened` arm
    /// under the same lock hold that decided to push: this sends through the
    /// wired inbox, or rolls the reservation back when none is wired, so an ask
    /// opened before the queue does not spend the wait (the last ask closing or
    /// the instance ending clears it).
    fn note_ask_opened(&self, peer_pid: u32, generation: u64) {
        let queue = self.push.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(queue) = queue else {
            // No push queue wired: roll the reservation back. Only while the
            // instance is unchanged (a replaced instance owns its own latch) and
            // the wait is still open (a closed wait was already reset) (WP-8).
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(live) = inner.instances.get_mut(&peer_pid)
                && live.generation == generation
                && !live.open_asks.is_empty()
            {
                live.announced = false;
            }
            return;
        };
        queue.send(crate::push::Trigger::SessionBlocked {
            account: self.session_account(peer_pid),
        });
    }

    /// The account the session's joined `live_sessions` row spends (pid
    /// membership, the same join [`session_config_dir_for`] makes), else the
    /// globally active profile.
    fn session_account(&self, peer_pid: u32) -> Option<String> {
        match session_row_for(peer_pid) {
            Some(row) => Some(
                row.current_member
                    .unwrap_or_else(|| row.start_profile.clone()),
            ),
            None => crate::profile::active_profile_name().map(|name| name.to_string()),
        }
    }

    /// Locate the main transcript at registration and snapshot the subagent
    /// files that exist now, with no hub lock held (the locate is a store
    /// walk). `generation` names the instance this registration belongs to: a
    /// `/clear` or recycled pid landing during the locate never inherits the
    /// stale tail. Registration owns the tail slot ([`Live::new`] starts
    /// `tailing`), so a wake racing this locate queues a re-run instead of
    /// locating at offset 0 and replaying history; ending the slot here runs
    /// those re-runs once (SB-11).
    fn locate_at_registration(&self, peer_pid: u32, session: &str, generation: u64) {
        self.finish_registration(
            peer_pid,
            session,
            generation,
            self.registration_tail(session),
        );
    }

    /// The registration locate's read half: walk the stores for the session's
    /// main transcript, read its length now, and open the tail at the last
    /// newline, with the subagent snapshot. Rows written after this returns
    /// relay once the slot ends. `None` when the locate misses or the file
    /// vanished before it could be opened.
    fn registration_tail(&self, session: &str) -> Option<(FileTail, HashMap<String, u64>)> {
        let walked = crate::sessions::locate(session)?;
        // Rows written between the instance's creation and this length read are
        // history-only, by design: they reach clients through the history route
        // (`GET /api/v1/sessions/{id}`), never the live relay (SB-11).
        let len = std::fs::metadata(&walked.path)
            .map(|m| m.len())
            .unwrap_or(0);
        let offset = back_to_newline(&walked.path, len);
        let snapshot = snapshot_subagents(&walked.path, session);
        let main = FileTail::new(walked.path, offset)?;
        Some((main, snapshot))
    }

    /// The registration locate's write half: hold the tail slot to the end,
    /// write the located tail back, and run the re-runs queued while the slot
    /// was held (SB-11).
    fn finish_registration(
        &self,
        peer_pid: u32,
        session: &str,
        generation: u64,
        located: Option<(FileTail, HashMap<String, u64>)>,
    ) {
        let Some((main, snapshot)) = located else {
            // The locate missed or the file vanished between the locate and the
            // read: skip the tail; a later wake locates it again (SB-11).
            let rewakes = self.finish_tail(peer_pid, generation);
            self.run_rewakes(peer_pid, session, rewakes);
            return;
        };
        let rewakes = self.write_back_tail(peer_pid, generation, Vec::new(), move |live| {
            live.main_file = Some(main);
            live.subagent_snapshot = snapshot;
        });
        self.run_rewakes(peer_pid, session, rewakes);
    }

    /// Relay the transcript rows an append wake points at. Every file read (the
    /// main locate, the subagents listing, the tail) runs with no hub lock
    /// held; the locks are taken only to copy the tail state out and write it
    /// back (SB-10, SB-11).
    fn relay_append(&self, peer_pid: u32, session: &str, agent_id: Option<&str>) {
        let mut queue: VecDeque<Option<String>> = VecDeque::new();
        queue.push_back(agent_id.map(str::to_owned));
        self.drain_tail_queue(peer_pid, session, &mut queue);
    }

    /// Run one tail for each key in `queue`, extending it with the re-runs each
    /// tail drains, until no re-run is pending: a wake for any file during any
    /// tail relays through its own file, never a different one (SB-11).
    fn drain_tail_queue(&self, peer_pid: u32, session: &str, queue: &mut VecDeque<Option<String>>) {
        while let Some(key) = queue.pop_front() {
            let more = match key {
                None => self.relay_main(peer_pid, session),
                Some(agent_id) => self.relay_subagent(peer_pid, session, &agent_id),
            };
            queue.extend(more);
        }
    }

    /// Run the re-runs a tail (or a registration locate) drained, one pass per
    /// file key, until none is pending (SB-11).
    fn run_rewakes(&self, peer_pid: u32, session: &str, rewakes: HashSet<Option<String>>) {
        let mut queue: VecDeque<Option<String>> = rewakes.into_iter().collect();
        self.drain_tail_queue(peer_pid, session, &mut queue);
    }

    /// One main-file tail pass: hold the tail slot, read the file, write the
    /// tail back, and return the file re-runs queued while it ran (SB-11).
    fn relay_main(&self, peer_pid: u32, session: &str) -> HashSet<Option<String>> {
        let (generation, main) = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let Some(live) = inner.instances.get_mut(&peer_pid) else {
                return HashSet::new();
            };
            if live.tailing {
                live.rewake.insert(None);
                return HashSet::new();
            }
            live.tailing = true;
            (live.generation, live.main_file.clone())
        };

        let mut tail = match main {
            Some(tail) => tail,
            None => {
                // The registration locate missed; the file appeared after, so
                // it starts at 0 (SB-11). The store walk runs with no lock.
                match crate::sessions::locate(session).and_then(|w| FileTail::new(w.path, 0)) {
                    Some(tail) => tail,
                    None => return self.finish_tail(peer_pid, generation),
                }
            }
        };

        let lines = tail_file(&mut tail);
        let frames: Vec<Value> = lines
            .into_iter()
            .filter_map(|(offset, line)| record_payload(session, None, offset, line))
            .collect();
        self.write_back_tail(peer_pid, generation, frames, move |live| {
            live.main_file = Some(tail);
        })
    }

    /// One subagent-file tail pass: hold the tail slot, list the subagent file,
    /// read it, write it back, and return the file re-runs queued while it ran
    /// (SB-11).
    fn relay_subagent(
        &self,
        peer_pid: u32,
        session: &str,
        agent_id: &str,
    ) -> HashSet<Option<String>> {
        let (generation, main, snapshot, sub) = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let Some(live) = inner.instances.get_mut(&peer_pid) else {
                return HashSet::new();
            };
            if live.tailing {
                live.rewake.insert(Some(agent_id.to_string()));
                return HashSet::new();
            }
            live.tailing = true;
            (
                live.generation,
                live.main_file.clone(),
                live.subagent_snapshot.clone(),
                live.subagent_files.get(agent_id).cloned(),
            )
        };

        let main = match main {
            Some(main) => main,
            None => match crate::sessions::locate(session).and_then(|w| FileTail::new(w.path, 0)) {
                Some(main) => main,
                None => return self.finish_tail(peer_pid, generation),
            },
        };
        let dir = subagents_dir(&main.path, session);
        let tail = match sub {
            Some(tail) => Some(tail),
            None => {
                let stem = format!("agent-{agent_id}");
                listed_subagent(&dir, &stem).and_then(|path| {
                    // A file snapshotted at registration starts at its snapshot
                    // length; one created after starts at 0 (SB-11).
                    let offset = snapshot
                        .get(agent_id)
                        .map_or(0, |len| back_to_newline(&path, *len));
                    FileTail::new(path, offset)
                })
            }
        };
        let Some(mut tail) = tail else {
            // Persist a newly located main file so later wakes skip the walk,
            // then end the slot (SB-11).
            return self.write_back_tail(peer_pid, generation, Vec::new(), move |live| {
                if live.main_file.is_none() {
                    live.main_file = Some(main);
                }
            });
        };
        let lines = tail_file(&mut tail);
        let frames: Vec<Value> = lines
            .into_iter()
            .filter_map(|(offset, line)| record_payload(session, Some(agent_id), offset, line))
            .collect();
        self.write_back_tail(peer_pid, generation, frames, move |live| {
            if live.main_file.is_none() {
                live.main_file = Some(main);
            }
            live.subagent_files.insert(agent_id.to_string(), tail);
        })
    }

    /// End a tail slot without a write-back or broadcast: clear `tailing` only
    /// while the instance at `peer_pid` still has `generation`, returning the
    /// file re-runs queued while the tail ran. An ending instance keeps them
    /// in `rewake` for its end's final tail to drain (SB-11).
    fn finish_tail(&self, peer_pid: u32, generation: u64) -> HashSet<Option<String>> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match inner.instances.get_mut(&peer_pid) {
            Some(live) if live.generation == generation => {
                live.tailing = false;
                // The slot just freed: wake the end path's wait, which only
                // removes the instance once no tail is running and no re-run
                // is pending.
                self.changed.notify_all();
                if live.ending {
                    HashSet::new()
                } else {
                    std::mem::take(&mut live.rewake)
                }
            }
            _ => HashSet::new(),
        }
    }

    /// Wait until the instance at `peer_pid` still has `generation` and no
    /// tail holds its slot. The end path parks here while any tail runs and
    /// re-checks `rewake` between drains, so the instance is removed only
    /// with the slot free and no re-run pending, and its own reads settle
    /// before its end frames. A re-run a tail took before the end began is
    /// out of this wait's sight: one still unstarted at the removal relays
    /// nothing. A close by another path (`close_silent`, a moved record, a
    /// replaced instance) removes without that wait and still drops an
    /// in-flight read's rows. Returns the guard to act under; `None` when the
    /// instance was closed or replaced — whoever removed it already emitted
    /// its end frames.
    fn end_wait_tail_slot<'a>(
        &self,
        mut guard: MutexGuard<'a, Inner>,
        peer_pid: u32,
        generation: u64,
    ) -> Option<MutexGuard<'a, Inner>> {
        loop {
            let live = guard.instances.get(&peer_pid)?;
            if live.generation != generation {
                return None;
            }
            if live.tailing {
                guard = self.changed.wait(guard).unwrap_or_else(|p| p.into_inner());
                continue;
            }
            return Some(guard);
        }
    }

    /// Write a tail back and broadcast its frames only while the instance at
    /// `peer_pid` still has `generation` — the check and the broadcasts share
    /// one lock hold, so a record can never follow its own instance's `end`
    /// (SB-11). Returns the file re-runs queued while the tail ran; an ending
    /// instance keeps them in `rewake` for its end's final tail to drain.
    fn write_back_tail(
        &self,
        peer_pid: u32,
        generation: u64,
        frames: Vec<Value>,
        write_back: impl FnOnce(&mut Live),
    ) -> HashSet<Option<String>> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let (alive, rewakes) = match inner.instances.get_mut(&peer_pid) {
            Some(live) if live.generation == generation => {
                write_back(live);
                live.tailing = false;
                (
                    true,
                    if live.ending {
                        HashSet::new()
                    } else {
                        std::mem::take(&mut live.rewake)
                    },
                )
            }
            _ => (false, HashSet::new()),
        };
        if alive {
            for data in &frames {
                self.broadcast("session.record", data.clone());
            }
            // The slot just freed: wake the end path's wait, which only
            // removes the instance once no tail is running and no re-run is
            // pending.
            self.changed.notify_all();
        }
        rewakes
    }

    /// One poll from the mod at `peer_pid` naming `session`, blocking up to
    /// `wait` for a command. Commands are delivered once, to their own
    /// instance's queue alone. A direct caller (a test) that receives the
    /// answer has the write succeed by construction, so its drained commands
    /// are consumed here. Production polls only through [`poll_answer`], so the
    /// consume-or-requeue choice stays in `serve`.
    #[cfg(test)]
    pub(crate) fn poll(&self, peer_pid: u32, session: &str, wait: Duration) -> (u16, Value) {
        let PollAnswer {
            status,
            body,
            drained,
        } = self.poll_answer(peer_pid, session, wait);
        if let Some(drained) = drained {
            self.consume(drained);
        }
        (status, body)
    }

    /// Resolve `peer_pid` to its instance and check `session` against it: the
    /// one guard every poll and wait answer shares, so an unknown peer and a
    /// session mismatch refuse alike. `what` names the route in the audit line.
    fn resolve_peer(
        &self,
        peer_pid: u32,
        session: &str,
        what: &str,
    ) -> Result<Instance, (u16, Value)> {
        let Some(resolved) = (self.resolver)(peer_pid) else {
            self.log_unknown_peer(peer_pid);
            return Err((403, error_value("unknown_peer")));
        };
        if resolved.session_id != session {
            logline!(
                "clauth bridge: peer {peer_pid} refused {what} for '{}' (resolved '{}')",
                sanitize_for_log(session),
                sanitize_for_log(&resolved.session_id)
            );
            return Err((403, error_value("session_mismatch")));
        }
        Ok(resolved)
    }

    /// [`poll`] with the drained commands carried back, so `serve` can requeue
    /// them when the answer write fails rather than letting a 200'd command
    /// vanish (A#1).
    fn poll_answer(&self, peer_pid: u32, session: &str, wait: Duration) -> PollAnswer {
        let resolved = match self.resolve_peer(peer_pid, session, "poll") {
            Ok(resolved) => resolved,
            Err((status, body)) => return PollAnswer::empty(status, body),
        };
        // The device store is read once per poll answer (A#15), before the hub
        // lock, so the read never stalls the hub (SB-10).
        let remote = self.remote_flag();
        let (created, generation, commands) = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let now = self.now();
            self.close_silent(&mut inner, now);
            let (live, created) = self.reconcile(&mut inner, peer_pid, &resolved, now);
            live.last_poll = now;
            let commands: Vec<Value> = live.commands.drain(..).collect();
            live.inflight += commands.len();
            (created, live.generation, commands)
        };
        // A poll as the instance's first contact registers its transcripts too
        // (SB-11): a daemon restart under a live mod meets it by poll first,
        // and its first wake must then read only rows written after the poll.
        if created {
            self.locate_at_registration(peer_pid, session, generation);
        }
        if !commands.is_empty() || wait.is_zero() {
            return PollAnswer::commands(peer_pid, generation, remote, commands);
        }

        // Park: this poll replaces any prior parked poll of the same instance,
        // and a parked poll only ever drains its own instance's queue (SB-4).
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let my_epoch = match inner.instances.get_mut(&peer_pid) {
            Some(live) if live.generation == generation => {
                live.poll_epoch = live.poll_epoch.wrapping_add(1);
                live.poll_epoch
            }
            // The instance was replaced or closed during the registration
            // locate: this poll no longer owns the park.
            _ => return PollAnswer::empty(200, poll_body(remote, &[])),
        };
        let my_generation = generation;
        // Wake the poll this one replaced (A#2).
        self.changed.notify_all();
        let mut guard = inner;
        let deadline = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return PollAnswer::empty(200, poll_body(remote, &[]));
            }
            let (g, timeout) = self
                .changed
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
            let (commands, replaced) = match guard.instances.get_mut(&peer_pid) {
                // The instance was replaced (a `/clear` or recycled pid): empty,
                // never the new instance's commands (A#3).
                Some(live) if live.generation != my_generation => (Vec::new(), true),
                // A newer poll parked on the same instance: empty, never a
                // command the replacement owns (A#1).
                Some(live) if live.poll_epoch != my_epoch => (Vec::new(), true),
                Some(live) => {
                    let commands: Vec<Value> = live.commands.drain(..).collect();
                    live.inflight += commands.len();
                    (commands, false)
                }
                None => (Vec::new(), true),
            };
            if !commands.is_empty() {
                return PollAnswer::commands(peer_pid, my_generation, remote, commands);
            }
            if replaced || timeout.timed_out() {
                return PollAnswer::empty(200, poll_body(remote, &[]));
            }
        }
    }

    /// Park one `GET /wait` on its own open ask until that ask closes, its
    /// bound elapses, or the instance dies — never touching the session's
    /// parked poll. Unlike a poll, a wait drains nothing, never bumps
    /// `poll_epoch`, and never mints an instance: it always follows an
    /// `ask_opened`, so an absent instance reads as an ask never opened.
    pub(crate) fn wait_ask(
        &self,
        peer_pid: u32,
        session: &str,
        ask: &str,
        wait: Duration,
    ) -> (u16, Value) {
        if let Err((status, body)) = self.resolve_peer(peer_pid, session, "wait") {
            return (status, body);
        }
        // The device store is read once per wait answer (A#15), before the hub
        // lock, so the read never stalls the hub (SB-10).
        let remote = self.remote_flag();

        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = inner.instances.get_mut(&peer_pid) else {
            return (404, error_value("ask_not_found"));
        };
        let generation = live.generation;
        if let Some(answer) = live.closed_asks.get(ask) {
            let answer = answer.clone();
            live.closed_asks.insert(ask.to_string(), Value::Null);
            return (200, wait_body(remote, Some(answer), true));
        }
        if !live.open_asks.iter().any(|(id, _)| id == ask) {
            return (404, error_value("ask_not_found"));
        }
        if live.parked_waits.values().sum::<usize>() >= MAX_WAITS_PER_INSTANCE {
            return (503, error_value("busy"));
        }
        *live.parked_waits.entry(ask.to_string()).or_default() += 1;
        let deadline = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= deadline {
                leave_wait(&mut inner, peer_pid, generation, ask);
                return (200, wait_body(remote, None, false));
            }
            let (guard, _timed_out) = self
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            inner = guard;
            let Some(live) = inner.instances.get(&peer_pid) else {
                leave_wait(&mut inner, peer_pid, generation, ask);
                return (200, wait_body(remote, None, true));
            };
            if live.generation != generation {
                leave_wait(&mut inner, peer_pid, generation, ask);
                return (200, wait_body(remote, None, true));
            }
            let Some(answer) = live.closed_asks.get(ask).cloned() else {
                // Still open — a spurious wake or another ask's event woke the
                // shared condvar: park again.
                continue;
            };
            leave_wait(&mut inner, peer_pid, generation, ask);
            // The answer rode this wait out: drop the payload, keep the id so
            // the ask still reads as closed to a later answer or `ask_closed`.
            if let Some(live) = inner.instances.get_mut(&peer_pid)
                && live.generation == generation
            {
                live.closed_asks.insert(ask.to_string(), Value::Null);
            }
            return (200, wait_body(remote, Some(answer), true));
        }
    }

    /// Mark a poll's drained commands delivered: the write succeeded, so the
    /// commands are consumed (dropped) and the instance's in-flight count
    /// falls. A replaced or closed instance needs nothing — its count died
    /// with it — and the token's commands drop here either way.
    fn consume(&self, drained: Drained) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = inner.instances.get_mut(&drained.peer_pid) else {
            return;
        };
        if live.generation != drained.generation {
            return;
        }
        live.inflight = live.inflight.saturating_sub(drained.commands.len());
        // A delivered `answer` command is that ask's last consumer via the poll
        // only when no wait is parked on it: a parked wait still delivers the
        // payload, and dropping it here would race the wait to a null answer.
        // Drop the kept payload then, keep the id so the ask still reads as
        // closed to a later answer or `ask_closed`.
        for command in &drained.commands {
            if command.get("kind").and_then(Value::as_str) == Some("answer")
                && let Some(ask) = command.get("ask").and_then(Value::as_str)
                && !live.parked_waits.contains_key(ask)
            {
                live.closed_asks.insert(ask.to_string(), Value::Null);
            }
        }
    }

    /// Put a poll's drained commands back at the front of their instance's
    /// queue, in order, when the answer write failed (A#1). Only when that
    /// instance is still the live one at `peer_pid`; a closed or replaced
    /// instance drops them — its poll already took them off its own queue and
    /// the mod will re-poll, so neither its successor nor a dead session
    /// inherits them. Never checked against the cap: these commands were
    /// already in the queue and are still counted in `inflight` until now, so
    /// restoring them never adds headroom a command route already used.
    fn requeue(&self, drained: Drained) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = inner.instances.get_mut(&drained.peer_pid) else {
            return;
        };
        if live.generation != drained.generation {
            return;
        }
        live.inflight = live.inflight.saturating_sub(drained.commands.len());
        for command in drained.commands.into_iter().rev() {
            live.commands.push_front(command);
        }
        self.changed.notify_all();
    }

    /// Queue `command` for the named session's live mod instance. Re-resolves
    /// only the instances holding the named id first (A#15), off every hub
    /// lock, so a moved or silent instance closes before the count decides
    /// (SB-4, SB-10, SB-13).
    pub(crate) fn deliver(&self, session: &str, command: Value) -> ModDelivery {
        let records: Vec<(u32, String, String)> = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            self.close_silent(&mut inner, self.now());
            inner
                .instances
                .iter()
                .filter(|(_, live)| live.session_id == session)
                .map(|(pid, live)| (*pid, live.proc_start.clone(), live.session_id.clone()))
                .collect()
        };
        let moved = self.moved_records(&records);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.close_moved(&mut inner, &moved);
        let matching: Vec<u32> = inner
            .instances
            .iter()
            .filter(|(_, live)| live.session_id == session && !live.ending)
            .map(|(pid, _)| *pid)
            .collect();
        match matching.len() {
            0 => ModDelivery::NoMod,
            1 => {
                let live = inner.instances.get_mut(&matching[0]).unwrap_or_else(|| {
                    unreachable!("the matching pid was counted from the same map")
                });
                if live.owned() >= MAX_QUEUED_COMMANDS {
                    return ModDelivery::Busy;
                }
                live.commands.push_back(command);
                self.changed.notify_all();
                ModDelivery::Delivered
            }
            _ => ModDelivery::Ambiguous,
        }
    }

    /// Answer a held ask on the named session's live mod instance: the first
    /// answer [`answer_refusal`] takes with room left in the queue wins and
    /// closes the ask; every other outcome leaves the ask open and queues
    /// nothing.
    pub(crate) fn answer(&self, session: &str, ask: &str, command: Value) -> AskOutcome {
        let records: Vec<(u32, String, String)> = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            self.close_silent(&mut inner, self.now());
            inner
                .instances
                .iter()
                .filter(|(_, live)| live.session_id == session)
                .map(|(pid, live)| (*pid, live.proc_start.clone(), live.session_id.clone()))
                .collect()
        };
        let moved = self.moved_records(&records);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.close_moved(&mut inner, &moved);
        let matching: Vec<u32> = inner
            .instances
            .iter()
            .filter(|(_, live)| live.session_id == session && !live.ending)
            .map(|(pid, _)| *pid)
            .collect();
        match matching.len() {
            0 => AskOutcome::NoSession,
            2.. => AskOutcome::Ambiguous,
            1 => {
                let live = inner.instances.get_mut(&matching[0]).unwrap_or_else(|| {
                    unreachable!("the matching pid was counted from the same map")
                });
                match live.open_asks.iter().find(|(id, _)| id == ask) {
                    Some((_, info)) => {
                        // Only a held ask is answerable (SB-19): the mod holds
                        // the check only when a remote answer was possible at
                        // open time, and an unheld ask is never answerable
                        // whatever the device state at answer time.
                        if !info.held {
                            return AskOutcome::AskNotFound;
                        }
                        if let Some(refusal) = answer_refusal(&info.tool, &info.input, &command) {
                            return refusal;
                        }
                        if live.owned() >= MAX_QUEUED_COMMANDS {
                            return AskOutcome::Busy;
                        }
                        live.open_asks.retain(|(id, _)| id != ask);
                        // The answer rides the closed-asks record, so a wait
                        // arriving after it returns it at once (stripped of the
                        // `kind`/`ask` wire wrapper the poll command keeps).
                        live.closed_asks
                            .insert(ask.to_string(), answer_value(&command));
                        // The last open ask closed: the wait ends, so the next
                        // ask opens a new wait and announces again (WP-8).
                        if live.open_asks.is_empty() {
                            live.announced = false;
                        }
                        live.commands.push_back(command);
                        self.changed.notify_all();
                        self.broadcast(
                            "session.ask",
                            json!({"session": session, "ask": ask, "state": "closed", "by": "device"}),
                        );
                        AskOutcome::Queued
                    }
                    None if live.closed_asks.contains_key(ask) => AskOutcome::AskClosed,
                    None => AskOutcome::AskNotFound,
                }
            }
        }
    }

    /// The session ids with a live mod instance, for the listing's `via`. Every
    /// live peer is re-resolved once — the listing has no named id to scope by
    /// (A#15) — off every hub lock, so a moved or silent instance cannot claim
    /// its old id (SB-10).
    pub(crate) fn live_session_ids(&self) -> Vec<String> {
        let records: Vec<(u32, String, String)> = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            self.close_silent(&mut inner, self.now());
            inner
                .instances
                .iter()
                .map(|(pid, live)| (*pid, live.proc_start.clone(), live.session_id.clone()))
                .collect()
        };
        let moved = self.moved_records(&records);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        self.close_moved(&mut inner, &moved);
        inner
            .instances
            .values()
            .map(|live| live.session_id.clone())
            .collect()
    }

    /// Whether a remote answer is possible: the daemon's listener is bound and a
    /// control-tier device is paired, read fresh on every call (SB-9, MB-1).
    fn remote_flag(&self) -> bool {
        self.listening && crate::daemon::api::devices::has_control_device()
    }

    /// The identity an `end` from a peer the resolver no longer knows is taken
    /// under: at `/exit` the resolver already answers `None` for the peer when
    /// the mod posts `end` (observed 2/2 in a sandbox, 2026-11-07). Only an
    /// `end` naming the very session the hub already holds for `peer_pid` and
    /// carrying the `seq` the end path judges qualifies; anything else stays an
    /// unknown peer. No process-start check of its own: the pid is the
    /// connecting process's own (`SO_PEERCRED` / `LOCAL_PEERPID`), so the poster
    /// is the held instance's process or one that took its pid after it died.
    /// A recycled pid keeps its predecessor's instance until a re-resolve
    /// ([`process_gone_or_moved`]: linux by start time, macOS by liveness alone)
    /// or the silence sweep closes it, and can end it first: the end frame that
    /// dead instance gets anyway, sent early.
    fn held_for_end(&self, peer_pid: u32, value: &Value) -> Option<Instance> {
        if value.get("kind").and_then(Value::as_str) != Some("end") {
            return None;
        }
        let named = value.get("session").and_then(Value::as_str)?;
        value.get("seq").and_then(Value::as_u64)?;
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let live = inner
            .instances
            .get(&peer_pid)
            .filter(|live| live.session_id == named)?;
        Some(Instance {
            session_id: live.session_id.clone(),
            proc_start: live.proc_start.clone(),
        })
    }

    /// Log the SB-3 "not registered, logged once" line for `peer_pid`.
    fn log_unknown_peer(&self, peer_pid: u32) {
        let first = self
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unknown_logged
            .insert(peer_pid);
        if first {
            logline!("clauth bridge: peer {peer_pid} not registered");
        }
    }

    /// Close every instance whose poll is silent past [`SILENCE_LIMIT`], each
    /// with its end frames. No resolver call: liveness is the poll alone
    /// (SB-13).
    fn close_silent(&self, inner: &mut Inner, now: Instant) {
        let pids: Vec<u32> = inner
            .instances
            .iter()
            .filter(|(_, live)| now.saturating_duration_since(live.last_poll) > SILENCE_LIMIT)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in pids {
            if let Some(live) = inner.instances.remove(&pid) {
                self.emit_instance_end(&live);
            }
        }
    }

    /// Find or create the instance at `pid`; a record whose id or process start
    /// moved closes the old instance first (with its end frames), so a recycled
    /// pid or `/clear` never inherits the session that used to be there. A
    /// second live instance on an id another instance holds is audited (SB-4).
    fn reconcile<'a>(
        &self,
        inner: &'a mut Inner,
        pid: u32,
        resolved: &Instance,
        now: Instant,
    ) -> (&'a mut Live, bool) {
        let moved = inner.instances.get(&pid).is_some_and(|live| {
            live.proc_start != resolved.proc_start || live.session_id != resolved.session_id
        });
        if moved && let Some(old) = inner.instances.remove(&pid) {
            self.emit_instance_end(&old);
        }
        let existed = inner.instances.contains_key(&pid);
        let peers: Vec<u32> = if existed {
            Vec::new()
        } else {
            inner
                .instances
                .iter()
                .filter(|&(other, live)| *other != pid && live.session_id == resolved.session_id)
                .map(|(other, _)| *other)
                .collect()
        };
        let (live, created) = match inner.instances.entry(pid) {
            Entry::Occupied(slot) => (slot.into_mut(), false),
            Entry::Vacant(slot) => {
                inner.next_generation = inner.next_generation.wrapping_add(1);
                (
                    slot.insert(Live::new(resolved, now, inner.next_generation)),
                    true,
                )
            }
        };
        // A pid that resolves is registered, so forget its "unknown, logged
        // once" memo: the set stays bounded to pids still unknown (A#7).
        inner.unknown_logged.remove(&pid);
        if !peers.is_empty() {
            let mut pids = peers;
            pids.push(pid);
            pids.sort_unstable();
            logline!(
                "clauth bridge: session '{}' now has {} live instances (pids {})",
                sanitize_for_log(&resolved.session_id),
                pids.len(),
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        (live, created)
    }

    /// Re-resolve the live instances `records` snapshotted and return which of
    /// them moved, keyed by pid with the identity the resolve compared against.
    /// The resolve runs off every hub lock: the resolver reads `sessions/`,
    /// `/proc` and the `live_sessions` registry, none of which may stall the
    /// hub (SB-10).
    fn moved_records(&self, records: &[(u32, String, String)]) -> HashMap<u32, (String, String)> {
        let mut moved = HashMap::new();
        for (pid, proc_start, session_id) in records {
            if self.pid_moved(*pid, proc_start, session_id) {
                moved.insert(*pid, (proc_start.clone(), session_id.clone()));
            }
        }
        moved
    }

    /// Whether `pid`'s record moved away from the live instance's recorded
    /// identity (a recycled pid, a `/clear`), or the record is gone and the
    /// process is too (SB-19). A resolver `None` for a live, same-start
    /// process is not moved (SB-13): the record is briefly unreadable, not the
    /// session gone.
    fn pid_moved(&self, pid: u32, proc_start: &str, session_id: &str) -> bool {
        match (self.resolver)(pid) {
            None => process_gone_or_moved(pid, proc_start),
            Some(resolved) => {
                proc_start != resolved.proc_start || session_id != resolved.session_id
            }
        }
    }

    /// Close every pid in `moved` whose live instance still carries the
    /// identity the resolve compared against, with its end frames. The identity
    /// re-check keeps a pid replaced while the resolve ran from closing as its
    /// successor (SB-4).
    fn close_moved(&self, inner: &mut Inner, moved: &HashMap<u32, (String, String)>) {
        for (pid, (proc_start, session_id)) in moved {
            let matches = inner.instances.get(pid).is_some_and(|live| {
                live.proc_start == *proc_start && live.session_id == *session_id
            });
            if matches && let Some(live) = inner.instances.remove(pid) {
                self.emit_instance_end(&live);
            }
        }
    }
}

/// Whether the process behind a resolver-`None` pid is gone or is another
/// process: the record vanished, so the instance closes only when the peer it
/// was keyed to no longer exists (a live, same-start process's brief
/// unreadability keeps it — SB-13). Linux compares field 22 of
/// `/proc/<pid>/stat` against the recorded start (a dead pid fails the read, a
/// recycled pid the compare); macOS compares nothing but liveness, since the
/// `procStart` form there is unmeasured.
fn process_gone_or_moved(pid: u32, proc_start: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        !crate::session_record::proc_start_matches(pid, proc_start)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = proc_start;
        !crate::runtime::namespaced_keychain_ledger::pid_alive(pid)
    }
}

fn error_value(code: &str) -> Value {
    json!({ "error": code })
}

fn poll_body(remote: bool, commands: &[Value]) -> Value {
    json!({ "remote": remote, "commands": commands })
}

/// One `GET /wait` answer: the device flag, the ask's answer (`null` when the
/// ask closed without one or the bound elapsed with it still open), and
/// whether the ask is closed.
fn wait_body(remote: bool, answer: Option<Value>, closed: bool) -> Value {
    json!({ "remote": remote, "answer": answer, "closed": closed })
}

/// The answer a device queued, stripped of the `kind`/`ask` wire wrapper the
/// poll command carries: the wait answers with the payload alone, the same
/// shapes the mod form-matches in a poll answer.
fn answer_value(command: &Value) -> Value {
    let mut stripped = command.as_object().cloned().unwrap_or_default();
    stripped.remove("kind");
    stripped.remove("ask");
    Value::Object(stripped)
}

/// The question texts an `AskUserQuestion` ask holds: its `input` is the
/// tool's `questions` array, each entry naming its text under `question`.
/// `input` alone never makes an ask a question ask; one whose input yields no
/// text refuses every `answers` key as unknown.
fn ask_questions(input: &Value) -> Vec<&str> {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("question").and_then(Value::as_str))
        .collect()
}

/// Why an answer the model could not use is refused, or `None` to take it.
/// The ask's kind comes from its `tool`, never its `input`'s shape: a
/// `decision` sent to a question ask, or `answers` sent to any other, is the
/// wrong form. For a question ask every `answers` and `notes` key must be
/// byte-equal to one of its question texts (a key matching none would reach
/// the model as no answer at all), then every answer must be a non-empty
/// string. A question left unanswered is allowed, a note on it included.
fn answer_refusal(tool: &Value, input: &Value, command: &Value) -> Option<AskOutcome> {
    let answers = command.get("answers").and_then(Value::as_object);
    let question_ask = tool.as_str() == Some("AskUserQuestion");
    if question_ask != answers.is_some() {
        return Some(AskOutcome::WrongForm);
    }
    // A permission ask's decision is the form it takes: nothing to check.
    let answers = answers?;
    let no_notes = Map::new();
    let notes = command
        .get("notes")
        .and_then(Value::as_object)
        .unwrap_or(&no_notes);
    let questions = ask_questions(input);
    if !answers
        .keys()
        .chain(notes.keys())
        .all(|key| questions.contains(&key.as_str()))
    {
        return Some(AskOutcome::UnknownQuestion);
    }
    if !answers
        .values()
        .all(|answer| answer.as_str().is_some_and(|text| !text.is_empty()))
    {
        return Some(AskOutcome::EmptyAnswer);
    }
    None
}

/// Release a parked wait's slot, only when the instance it parked on is still
/// the live one at `peer_pid` — a replaced or closed instance's counter died
/// with it, so the successor's must not be charged for its wait.
fn leave_wait(inner: &mut Inner, peer_pid: u32, generation: u64, ask: &str) {
    if let Some(live) = inner.instances.get_mut(&peer_pid)
        && live.generation == generation
        && let Some(parked) = live.parked_waits.get_mut(ask)
    {
        if *parked <= 1 {
            live.parked_waits.remove(ask);
        } else {
            *parked -= 1;
        }
    }
}

/// One complete transcript line a wake relayed.
enum RelayedLine {
    /// The full line bytes, whatever their size: parsed (or stubbed by size) in
    /// [`record_payload`].
    Record(Vec<u8>),
    /// A line that outgrew [`RECORD_LIMIT`] while torn, already decided to
    /// carry a `uuid` by streaming its file range at completion (SB-10).
    Stub { size: u64 },
}

impl FileTail {
    /// A new tail for `path` starting at `offset`, recording its `(dev, ino)`
    /// so a later replacement resets the offset (SB-11). `None` when the file
    /// cannot be read: a metadata failure skips the tail rather than recording
    /// `(0, 0)` and replaying the whole file as a replaced one.
    fn new(path: PathBuf, offset: u64) -> Option<Self> {
        let meta = std::fs::metadata(&path).ok()?;
        let (dev, ino) = file_id(&meta);
        Some(Self {
            path,
            offset,
            torn: Vec::new(),
            torn_len: 0,
            dev,
            ino,
        })
    }
}

/// The `(dev, ino)` of a file's metadata, the identity that survives a rename.
fn file_id(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Read `[start, start + max)` of `path`, or to EOF, whichever is first.
fn read_range(path: &Path, start: u64, max: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

/// One past the last newline at or before `len` in `path`: the first byte of
/// the line containing `len`, so a file first seen mid-line starts at that
/// line's head, never mid-line (SB-11). The scan reads a bounded tail window.
fn back_to_newline(path: &Path, len: u64) -> u64 {
    let window = (RECORD_LIMIT as u64).min(len);
    let start = len - window;
    let Ok(bytes) = read_range(path, start, window) else {
        return 0;
    };
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(nl) => start + nl as u64 + 1,
        None => 0,
    }
}

/// Read the complete lines of `tail`'s file newly written past `tail`,
/// returning each with its byte offset and advancing `tail` to the new torn
/// head. The read is bounded: a torn line's head is retained once (capped at
/// [`RECORD_LIMIT`]) and a wake reads only the bytes past it, so a growing torn
/// line is never re-read each wake (SB-10).
fn tail_file(tail: &mut FileTail) -> Vec<(u64, RelayedLine)> {
    let mut lines = Vec::new();
    let meta = match std::fs::metadata(&tail.path) {
        Ok(meta) => meta,
        Err(_) => return lines,
    };
    let id = file_id(&meta);
    if id != (tail.dev, tail.ino) {
        tail.dev = id.0;
        tail.ino = id.1;
        tail.offset = 0;
        tail.torn.clear();
        tail.torn_len = 0;
    }
    let len = meta.len();
    if tail.offset > len || tail.offset + tail.torn_len > len {
        // The file shrank below the torn head: start over at the new end.
        tail.offset = tail.offset.min(len);
        tail.torn.clear();
        tail.torn_len = 0;
    }
    let start = tail.offset + tail.torn_len;
    if start > len {
        return lines;
    }
    // Read the new bytes past the retained head to the file's end: complete
    // lines (however long) relay, and the retained torn head is capped below so
    // a growing torn line is never buffered past [`RECORD_LIMIT`] (SB-10).
    let Ok(buf) = read_range(&tail.path, start, len - start) else {
        return lines;
    };

    let rest = if tail.torn_len > 0 {
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            // The pending torn head completes at the first newline.
            let size = tail.torn_len + nl as u64;
            if size > RECORD_LIMIT as u64 {
                // The line outgrew the bound while torn: decide its `uuid`
                // exactly now, by streaming its file range (SB-10). The
                // retained head is unneeded either way.
                std::mem::take(&mut tail.torn);
                if range_has_uuid(&tail.path, tail.offset, size) {
                    lines.push((tail.offset, RelayedLine::Stub { size }));
                }
            } else {
                let mut full = std::mem::take(&mut tail.torn);
                full.extend_from_slice(&buf[..nl]);
                lines.push((tail.offset, RelayedLine::Record(full)));
            }
            tail.offset += size + 1;
            tail.torn_len = 0;
            &buf[nl + 1..]
        } else {
            // Still torn: retain the head capped at [`RECORD_LIMIT`], count the
            // rest so the stub's `size` is right when it completes (SB-10).
            let room = RECORD_LIMIT.saturating_sub(tail.torn.len());
            tail.torn.extend_from_slice(&buf[..room.min(buf.len())]);
            tail.torn_len += buf.len() as u64;
            return lines;
        }
    } else {
        &buf[..]
    };

    let mut pos = 0usize;
    while let Some(nl) = rest[pos..].iter().position(|&b| b == b'\n') {
        let end = pos + nl;
        lines.push((tail.offset, RelayedLine::Record(rest[pos..end].to_vec())));
        tail.offset += (nl + 1) as u64;
        pos = end + 1;
    }
    let torn_rest = &rest[pos..];
    tail.torn = torn_rest[..RECORD_LIMIT.min(torn_rest.len())].to_vec();
    tail.torn_len = torn_rest.len() as u64;
    lines
}

/// A typed partial parse that decides a line's `uuid` without building a tree
/// (SB-10). `IgnoredAny` consumes the value, whatever its shape.
#[derive(serde::Deserialize)]
struct UuidOnly {
    uuid: Option<serde::de::IgnoredAny>,
}

/// The frame one complete line relays, or `None` when it relays nothing: a
/// record carrying a `uuid` goes out as the history route's `{offset, record}`,
/// one past the bound as a stub naming its offset and size (SB-10, SB-11). A
/// [`RelayedLine::Stub`] was already decided to carry a `uuid` at completion.
fn record_payload(
    session: &str,
    agent_id: Option<&str>,
    offset: u64,
    line: RelayedLine,
) -> Option<Value> {
    match line {
        RelayedLine::Record(line) => {
            if line.len() > RECORD_LIMIT {
                return has_uuid(&line).then(|| {
                    json!({"session": session, "agent_id": agent_id, "offset": offset, "size": line.len(), "truncated": true})
                });
            }
            let record = serde_json::from_slice::<Value>(&line).ok()?;
            record.get("uuid")?;
            Some(
                json!({"session": session, "agent_id": agent_id, "offset": offset, "record": record}),
            )
        }
        RelayedLine::Stub { size } => Some(json!({
            "session": session,
            "agent_id": agent_id,
            "offset": offset,
            "size": size,
            "truncated": true,
        })),
    }
}

/// Whether a complete line carries a `uuid`, decided with a typed partial
/// struct that builds no tree (SB-10).
fn has_uuid(line: &[u8]) -> bool {
    serde_json::from_slice::<UuidOnly>(line)
        .map(|only| only.uuid.is_some())
        .unwrap_or(false)
}

/// Whether the `[offset, offset + size)` byte range of `path` is one JSON line
/// carrying a `uuid`: the torn-oversize path streams the range instead of
/// scanning a retained prefix, so a `uuid` after the bulky `message`/
/// `attachment` keys still reads, and a nested `"uuid"` inside the content
/// does not false-positive (SB-10).
fn range_has_uuid(path: &Path, offset: u64, size: u64) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    serde_json::from_reader::<_, UuidOnly>(file.take(size))
        .map(|only| only.uuid.is_some())
        .unwrap_or(false)
}

/// The session's `subagents/` dir, next to the main transcript.
fn subagents_dir(main_path: &Path, session: &str) -> PathBuf {
    main_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(session)
        .join("subagents")
}

/// The subagent files under the session's `subagents/` dir that exist now, by
/// agent stem, with their length: a file present at registration starts at its
/// length; one created after starts at 0 (SB-11).
fn snapshot_subagents(main_path: &Path, session: &str) -> HashMap<String, u64> {
    let dir = subagents_dir(main_path, session);
    let mut snapshot = HashMap::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return snapshot;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(agent_id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("agent-"))
        else {
            continue;
        };
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        snapshot.insert(agent_id.to_string(), len);
    }
    snapshot
}

/// The `agent-<id>.jsonl` file listed under `dir` whose stem matches `stem`
/// exactly; a forged id (`../x`, `a/b`) matches no listed name (SB-11).
fn listed_subagent(dir: &Path, stem: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path.file_stem().and_then(|s| s.to_str()) == Some(stem)
        {
            return Some(path);
        }
    }
    None
}

/// Bind the bridge socket at `path`: the dir is created 0700 and the socket
/// lands 0600, owned by the daemon alone. A path already holding a live
/// listener is refused; a refused connect proves the file stale, and only then
/// is it unlinked and re-bound (SB-1). The daemon singleton claim is the
/// caller's to hold — this bind's job is the connect probe.
pub(crate) fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket path has no parent",
        )
    })?;
    crate::profile::mkdir_700(dir)?;
    match bind_0600(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Only a refused connect proves the file stale; any other connect
            // error (a mode-000 live socket, say) refuses rather than unlink.
            match UnixStream::connect(path) {
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "a live listener holds the bridge socket",
                )),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path)?;
                    bind_0600(path)
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn bind_0600(path: &Path) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve HTTP/1.1 (`POST /event`, `GET /poll?session=<id>`,
/// `GET /wait?session=<id>&ask=<ask>`) over the socket,
/// reading the peer pid off each accepted connection. One thread per
/// connection, so a parked 20 s poll never blocks another connection's event.
pub(crate) fn spawn_server(listener: UnixListener, hub: Arc<Hub>) -> std::io::Result<Server> {
    spawn_server_with_limits(listener, hub, Limits::default())
}

/// The bridge server's join handle and, in test builds, a stop handle so a test
/// that starts a server can join its accept thread before the sandbox drops
/// (A#6). Production holds no handle: the loop runs for the daemon's life and
/// the thread is detached.
pub(crate) struct Server {
    #[cfg(test)]
    thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    stop: TestStop,
}

/// Stops the accept loop so the server thread can be joined (A#6). Test-only:
/// production never stops the loop.
#[cfg(test)]
struct TestStop {
    flag: Arc<AtomicBool>,
    path: Option<PathBuf>,
    /// Live connection threads (spawned, not yet returned): `stop` waits for
    /// this to reach zero before the sandbox drops (A#2).
    conns: Arc<AtomicUsize>,
}

/// How long `stop` waits for in-flight connection threads to finish. One
/// thread lives at most a parked poll's [`POLL_WAIT`] (or a request read plus
/// answer write in production), so 30 s covers every injected test limit with
/// margin. A bound, never a join-forever: a thread that outlives it is left
/// detached rather than hanging the test.
#[cfg(test)]
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Releases one live-connection count when a connection thread ends, panic
/// included — `stop` waits on this count reaching zero (A#2). The accept loop
/// increments before spawning, so the count never under-reports a thread that
/// was spawned but has not yet run.
struct ConnDone<'a>(&'a AtomicUsize);

impl Drop for ConnDone<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
impl Server {
    /// Signal the accept loop and join it and its connection threads. Panics
    /// when the self-connect that must wake the accept loop fails: without the
    /// wake the join would hang, so the loud failure names the gone socket
    /// instead (A#2).
    pub(crate) fn stop(mut self) {
        self.stop_impl(false);
    }

    /// The stop machinery shared with [`Drop`]: `from_drop` keeps it silent —
    /// a panic during unwind would abort — and best-effort where the explicit
    /// `stop` would panic.
    fn stop_impl(&mut self, from_drop: bool) {
        let Some(thread) = self.thread.take() else {
            return; // already stopped
        };
        self.stop.flag.store(true, Ordering::Relaxed);
        // One self-connect wakes `incoming()` out of its accept, so the loop
        // observes the flag and breaks; the connection is never read. A failed
        // connect means the loop cannot be woken, so the join would hang.
        let woke = match &self.stop.path {
            Some(path) => UnixStream::connect(path).is_ok(),
            None => false,
        };
        if !woke {
            let reason = match &self.stop.path {
                Some(path) => format!("the socket at {} is gone or unreachable", path.display()),
                None => "the listener has no path".to_string(),
            };
            if from_drop {
                // Cannot wake the loop; dropping `thread` detaches it rather
                // than joining forever. Say so, but never panic during unwind.
                logline!("clauth bridge: server stop could not wake the accept loop: {reason}");
                return;
            }
            panic!("bridge server stop could not wake the accept loop: {reason}");
        }
        let _ = thread.join();
        // Wait for the in-flight connection threads to finish, bounded (A#2).
        let deadline = Instant::now() + STOP_JOIN_TIMEOUT;
        while self.stop.conns.load(Ordering::Relaxed) != 0 {
            if Instant::now() >= deadline {
                // A thread still running past the bound is left detached. The
                // explicit stop fails loudly naming the count; the Drop path,
                // already unwinding, logs it instead.
                let count = self.stop.conns.load(Ordering::Relaxed);
                if from_drop {
                    logline!("clauth bridge: server stop left {count} live connection(s)");
                    return;
                }
                panic!("bridge server stop timed out with {count} live connection(s)");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
impl Drop for Server {
    /// Run on the panic path too: a failing assertion must still stop the
    /// server before the sandbox drops (B#3). Never panics here.
    fn drop(&mut self) {
        self.stop_impl(true);
    }
}

/// The same server over injected [`Limits`], so a test drives a deadline in
/// milliseconds rather than minutes.
fn spawn_server_with_limits(
    listener: UnixListener,
    hub: Arc<Hub>,
    limits: Limits,
) -> std::io::Result<Server> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let accept_flag = Arc::clone(&stop_flag);
    let conns = Arc::new(AtomicUsize::new(0));
    #[cfg(test)]
    let conns_stop = Arc::clone(&conns);
    #[cfg(test)]
    let path = listener
        .local_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(PathBuf::from));
    let spawned = std::thread::Builder::new()
        .name("clauth-bridge".to_string())
        .spawn(move || {
            let events = Arc::new(AtomicUsize::new(0));
            let parked = Arc::new(AtomicUsize::new(0));
            for conn in listener.incoming() {
                if accept_flag.load(Ordering::Relaxed) {
                    break;
                }
                let stream = match conn {
                    Ok(stream) => stream,
                    Err(e) => {
                        logline!("clauth bridge: accept failed: {e}");
                        std::thread::sleep(ACCEPT_BACKOFF);
                        continue;
                    }
                };
                // Claim the event slot in the accept loop, BEFORE spawning the
                // per-connection thread (A#4): a reconnect storm is bounded by
                // the cap, never by thread-spawn failure. The thread re-wraps
                // the claim in a [`Slot`] so every exit path releases it.
                let Some(stream) = claim_event(stream, &events, limits) else {
                    continue;
                };
                let hub = Arc::clone(&hub);
                let events_conn = Arc::clone(&events);
                let parked = Arc::clone(&parked);
                let conns_conn = Arc::clone(&conns);
                // Count the connection before the spawn so `stop` never joins
                // past a thread that was spawned but has not yet run; the
                // thread's `ConnDone` releases it however it ends.
                conns.fetch_add(1, Ordering::Relaxed);
                if std::thread::Builder::new()
                    .name("clauth-bridge-conn".to_string())
                    .spawn(move || {
                        let _conn = ConnDone(&conns_conn);
                        let event = Slot(&events_conn);
                        serve(stream, &hub, event, &parked, limits);
                    })
                    .is_err()
                {
                    // The claim was made in the accept loop; a failed spawn never
                    // ran the `Slot` or `ConnDone` guards, so release both here
                    // (A#4).
                    events.fetch_sub(1, Ordering::Relaxed);
                    conns.fetch_sub(1, Ordering::Relaxed);
                    logline!("clauth bridge: failed to spawn a connection thread");
                }
            }
        });
    #[cfg(not(test))]
    let _ = spawned?;
    #[cfg(test)]
    let thread = spawned?;
    Ok(Server {
        #[cfg(test)]
        thread: Some(thread),
        #[cfg(test)]
        stop: TestStop {
            flag: stop_flag,
            path,
            conns: conns_stop,
        },
    })
}

/// Claim one event-budget slot, refusing the connection with a 503 and one log
/// line when the budget is full. The accept loop calls this BEFORE spawning a
/// per-connection thread (A#4); the stream is only handed to a thread when the
/// slot is held.
fn claim_event(mut stream: UnixStream, events: &AtomicUsize, limits: Limits) -> Option<UnixStream> {
    if events.fetch_add(1, Ordering::Relaxed) >= MAX_EVENT_CONNECTIONS {
        events.fetch_sub(1, Ordering::Relaxed);
        logline!(
            "clauth bridge: refused a connection: {MAX_EVENT_CONNECTIONS} concurrent event connections"
        );
        let _ = stream.set_write_timeout(Some(limits.write_timeout));
        write_response(&mut stream, 503, &error_value("busy"));
        None
    } else {
        Some(stream)
    }
}

fn serve(
    mut stream: UnixStream,
    hub: &Arc<Hub>,
    event: Slot<'_>,
    parked: &AtomicUsize,
    limits: Limits,
) {
    // The write bound applies to every answer, the budget refusals included, so
    // a peer that never reads cannot hold the thread in `write!` (A#3).
    if stream
        .set_write_timeout(Some(limits.write_timeout))
        .is_err()
    {
        return;
    }
    let Some(pid) = peer_pid(&stream) else {
        write_response(&mut stream, 403, &error_value("unknown_peer"));
        return;
    };
    let request = match read_request(&mut stream, Instant::now() + limits.request_deadline) {
        Err(ReadError::Eof) => return,
        Err(ReadError::HeadTooLarge) => {
            logline!("clauth bridge: peer {pid} refused: request head over the cap");
            write_response(&mut stream, 400, &error_value("bad_request"));
            return;
        }
        Err(ReadError::Chunked) => {
            logline!("clauth bridge: peer {pid} refused: chunked transfer encoding");
            write_response(&mut stream, 400, &error_value("bad_request"));
            return;
        }
        Err(ReadError::BodyTooLarge(len)) => {
            logline!("clauth bridge: peer {pid} refused: body_len={len}");
            write_response(&mut stream, 413, &error_value("body_too_large"));
            return;
        }
        Err(ReadError::Deadline) => {
            logline!("clauth bridge: peer {pid} refused: request deadline exceeded");
            write_response(&mut stream, 503, &error_value("busy"));
            return;
        }
        Err(ReadError::Malformed) => {
            logline!("clauth bridge: peer {pid} refused: malformed request");
            write_response(&mut stream, 400, &error_value("bad_request"));
            return;
        }
        Ok(request) => request,
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/event") => {
            let (status, answer) = hub.event(pid, &request.body);
            write_response(&mut stream, status, &answer);
        }
        ("GET", path) if path.starts_with("/poll") => {
            let session = parse_poll_session(path).unwrap_or_default();
            // Leave the event budget before parking: a parked poll holds the
            // poll budget, never an event slot (SB-10).
            drop(event);
            if parked.fetch_add(1, Ordering::Relaxed) >= MAX_PARKED_POLLS {
                parked.fetch_sub(1, Ordering::Relaxed);
                logline!("clauth bridge: refused a poll: {MAX_PARKED_POLLS} parked polls");
                write_response(&mut stream, 503, &error_value("busy"));
                return;
            }
            let _poll_slot = Slot(parked);
            let PollAnswer {
                status,
                body,
                drained,
            } = hub.poll_answer(pid, session, POLL_WAIT);
            // `written` only means the kernel took the bytes, not that the mod
            // processed them: a mod that closes after a short buffered answer
            // loses those commands, and a failed write settling after a newer
            // poll delivered newer commands requeues the older ones behind them
            // (order holds within the queue, not across that race). An
            // application-level ack per applied command would close both.
            let written = write_response(&mut stream, status, &body);
            match drained {
                // A failed write never delivered them: put them back at the
                // front for the next poll, in order (A#1).
                Some(drained) if !written => hub.requeue(drained),
                Some(drained) => hub.consume(drained),
                None => {}
            }
        }
        ("GET", path) if path.starts_with("/wait") => {
            let Some((session, ask)) = parse_wait_query(path) else {
                write_response(&mut stream, 400, &error_value("bad_request"));
                return;
            };
            // Leave the event budget before parking, exactly as a poll does: a
            // parked wait holds the poll budget, never an event slot (SB-10).
            drop(event);
            if parked.fetch_add(1, Ordering::Relaxed) >= MAX_PARKED_POLLS {
                parked.fetch_sub(1, Ordering::Relaxed);
                logline!("clauth bridge: refused a wait: {MAX_PARKED_POLLS} parked polls");
                write_response(&mut stream, 503, &error_value("busy"));
                return;
            }
            let _wait_slot = Slot(parked);
            let (status, body) = hub.wait_ask(pid, session, ask, POLL_WAIT);
            let _ = write_response(&mut stream, status, &body);
        }
        _ => {
            write_response(&mut stream, 404, &error_value("not_found"));
        }
    }
}

/// The peer pid of an accepted bridge connection: `SO_PEERCRED` on linux,
/// `LOCAL_PEERPID` on macOS. `None` only when the kernel refuses to say, which
/// reads as an unknown peer rather than trusting any id off the wire.
#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    #[allow(unsafe_code)]
    // SAFETY: `getsockopt` copies the kernel's credential for this live socket
    // into `cred`, which we own and whose size `len` names; the fd is open and
    // belongs to `stream`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.pid as u32)
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    #[allow(unsafe_code)]
    // SAFETY: `getsockopt` copies the kernel's peer pid for this live socket
    // into a `pid_t` we own; the length is that type's, and the fd is open.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut libc::pid_t as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(pid as u32)
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Why [`read_request`] could not produce a [`Request`].
#[derive(Debug)]
enum ReadError {
    /// The peer closed cleanly before a request completed: nothing to answer.
    Eof,
    /// The head grew past [`MAX_HEAD_BYTES`].
    HeadTooLarge,
    /// `Transfer-Encoding` was present: chunked is refused, not implemented.
    Chunked,
    /// `Content-Length` named a body past [`BODY_LIMIT`], decided before the
    /// body is read (SB-10).
    BodyTooLarge(usize),
    /// The whole request outlived its deadline (a trickling peer), not one read.
    Deadline,
    /// Truncated, unreadable, or self-contradictory.
    Malformed,
}

/// One deadline-bounded read. The socket's timeout is re-armed to the remaining
/// budget before every read, so the WHOLE request — head and body — is bounded
/// by `deadline`, never a per-read window a trickling peer can reset (SB-10).
fn read_chunk(
    stream: &mut UnixStream,
    deadline: Instant,
    tmp: &mut [u8; 4096],
) -> Result<usize, ReadError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ReadError::Deadline);
    }
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| ReadError::Malformed)?;
    stream.read(tmp).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            ReadError::Deadline
        } else {
            ReadError::Malformed
        }
    })
}

/// Read one request: the head through `\r\n\r\n` bounded by [`MAX_HEAD_BYTES`],
/// a case-insensitive `Content-Length`, then exactly that many body bytes, all
/// under one wall-clock `deadline`. A body past [`BODY_LIMIT`] is refused from
/// the header alone, before any of it is read (SB-10); chunked is refused
/// outright.
fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<Request, ReadError> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Err(ReadError::HeadTooLarge);
        }
        let n = read_chunk(stream, deadline, &mut tmp)?;
        if n == 0 {
            return Err(ReadError::Eof);
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ReadError::Malformed)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(ReadError::Malformed)?.to_string();
    let path = parts.next().ok_or(ReadError::Malformed)?.to_string();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadError::Malformed);
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ReadError::Chunked);
        }
        if name.eq_ignore_ascii_case("content-length") {
            // `1*DIGIT` only (RFC 9110 §8.6), so a repeated header that
            // disagrees is a hard error rather than a second reading of where
            // one message ends.
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ReadError::Malformed);
            }
            let parsed: usize = value.parse().map_err(|_| ReadError::Malformed)?;
            if content_length.is_some_and(|seen| seen != parsed) {
                return Err(ReadError::Malformed);
            }
            content_length = Some(parsed);
        }
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > BODY_LIMIT {
        return Err(ReadError::BodyTooLarge(content_length));
    }
    let body_start = head_end + 4;
    while buf.len() < body_start + content_length {
        let n = read_chunk(stream, deadline, &mut tmp)?;
        if n == 0 {
            return Err(ReadError::Malformed);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[body_start..body_start + content_length].to_vec();
    Ok(Request { method, path, body })
}

fn parse_poll_session(path: &str) -> Option<&str> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "session").then_some(value)
    })
}

/// The `session` and `ask` of a `GET /wait` query, both required — a missing
/// one reads as `400 bad_request`, never a default that could wait on the wrong
/// ask.
fn parse_wait_query(path: &str) -> Option<(&str, &str)> {
    let query = path.split_once('?')?.1;
    let mut session = None;
    let mut ask = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "session" => session = Some(value),
            "ask" => ask = Some(value),
            _ => {}
        }
    }
    Some((session?, ask?))
}

fn write_response(stream: &mut UnixStream, status: u16, body: &Value) -> bool {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .is_ok()
}

/// The production resolver: reads the peer pid's Claude Code session record
/// under the config dir the peer's `live_sessions` row joins (else `~/.claude`)
/// and verifies `procStart` against `/proc/<pid>/stat` field 22 on linux.
pub(crate) fn production_resolver() -> PeerResolver {
    Box::new(|pid| {
        let dir = session_config_dir_for(pid)?;
        let record = crate::session_record::session_record(&dir.join("sessions"), pid)?;
        // The record's `procStart` must equal field 22 of `/proc/<pid>/stat`
        // (measured equal); a mismatch is a stale record a recycled pid left,
        // so the peer reads as unknown.
        // The macOS `procStart` form is unmeasured, so nothing there compares it
        // and a recycled pid is not detectable on macOS until it is measured.
        #[cfg(target_os = "linux")]
        if !crate::session_record::process_matches_record(pid, &record) {
            return None;
        }
        Some(Instance {
            session_id: record.session_id,
            proc_start: record.proc_start,
        })
    })
}

/// Bind `~/.clauth/bridge.sock` and serve it, returning the hub for the
/// [`ApiContext`]. A bind or spawn failure logs one line naming the path and
/// why, and the daemon runs on with no bridge (`None`) — rotation and fallback
/// never stop for it (SB-1). A `--standby` daemon never reaches this before it
/// takes over: `serve_prepared` runs only below the singleton claim and a
/// promotion.
pub(crate) fn start() -> Option<Arc<Hub>> {
    let hub = Hub::new(true, production_resolver());
    spawn_sweep(&hub);
    let path = match crate::profile::clauth_dir() {
        Ok(dir) => dir.join("bridge.sock"),
        Err(e) => {
            logline!("clauth daemon: no mod bridge: {e:#}");
            return None;
        }
    };
    let listener = match bind(&path) {
        Ok(listener) => listener,
        Err(e) => {
            logline!("clauth daemon: no mod bridge on {}: {e}", path.display());
            return None;
        }
    };
    match spawn_server(listener, Arc::clone(&hub)) {
        Ok(_server) => Some(hub),
        Err(e) => {
            logline!("clauth daemon: no mod bridge: failed to spawn the bridge server: {e}");
            None
        }
    }
}

/// The periodic silence sweep, detached for the daemon's life: a `Weak` handle
/// so the thread never keeps the hub alive past its last owner, and the sweep
/// closes silent instances near the bound, not at the next interaction (SB-13).
fn spawn_sweep(hub: &Arc<Hub>) {
    let weak = Arc::downgrade(hub);
    let spawned = std::thread::Builder::new()
        .name("clauth-bridge-sweep".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(SWEEP_INTERVAL);
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                hub.sweep();
            }
        });
    if let Err(e) = spawned {
        logline!(
            "clauth bridge: failed to start the sweep ({e}); silent sessions close on the next interaction"
        );
    }
}

/// The `live_sessions` row the peer pid joins to, by the same membership
/// [`session_config_dir_for`] uses: the peer itself, else its direct parent
/// when that parent is a `clauth start`/`resume` (SB-5).
fn session_row_for(pid: u32) -> Option<crate::live_sessions::LiveSession> {
    let rows = crate::live_sessions::list();
    let newest = crate::daemon::api::panes::newest_per_pid(&rows);
    newest
        .iter()
        .find(|row| row.pid == pid)
        .map(|row| (*row).clone())
        .or_else(|| {
            let parent = crate::session_record::parent_pid(pid)?;
            if !is_clauth_launch(parent) {
                return None;
            }
            newest
                .iter()
                .find(|row| row.pid == parent)
                .map(|row| (*row).clone())
        })
}

/// The config dir the peer's session reads: the runtime tree of the
/// `live_sessions` row whose pid is the peer itself (a delegate's row carries
/// the claude pid), else the peer's direct parent when that parent is a
/// `clauth start`/`resume`; `~/.claude` when no row joins (SB-3, SB-5).
///
/// A background row resolves to the account's fixed `runtime-bg`, never the
/// per-session trees: the launcher's session record and the daemon's watchdog
/// both write through that one tree.
fn session_config_dir_for(pid: u32) -> Option<PathBuf> {
    match session_row_for(pid) {
        Some(row) => {
            let profile = crate::profile::ProfileName::from(row.start_profile.as_str());
            if row.background {
                return crate::runtime::background_runtime_tree(&profile).ok();
            }
            crate::runtime::session_sessions_tree(&row.start_profile, row.isolated, &row.session_id)
        }
        None => crate::profile::claude_dir().ok(),
    }
}

// Test seam for the SB-5 parent arm: the production check reads
// `/proc/<pid>/cmdline` for a binary literally named `clauth`, which a test
// cannot stage without a second binary. [`LaunchOverride`] declares one pid a
// launch for the calling thread's duration; each test drives its own resolver
// call on its own thread.
#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static LAUNCH_OVERRIDE: std::cell::RefCell<std::collections::HashSet<u32>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) struct LaunchOverride {
    pid: u32,
}

#[cfg(all(test, target_os = "linux"))]
impl LaunchOverride {
    pub(crate) fn pose(pid: u32) -> Self {
        LAUNCH_OVERRIDE.with(|set| {
            set.borrow_mut().insert(pid);
        });
        Self { pid }
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for LaunchOverride {
    fn drop(&mut self) {
        LAUNCH_OVERRIDE.with(|set| {
            set.borrow_mut().remove(&self.pid);
        });
    }
}

#[cfg(target_os = "linux")]
fn is_clauth_launch(pid: u32) -> bool {
    #[cfg(test)]
    if LAUNCH_OVERRIDE.with(|set| set.borrow().contains(&pid)) {
        return true;
    }
    use std::os::unix::ffi::OsStrExt;
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let mut args = cmdline.split(|byte| *byte == 0).filter(|a| !a.is_empty());
    let (Some(argv0), Some(argv1)) = (args.next(), args.next()) else {
        return false;
    };
    let name = Path::new(std::ffi::OsStr::from_bytes(argv0)).file_name();
    name == Some(std::ffi::OsStr::new("clauth")) && matches!(argv1, b"start" | b"resume")
}

#[cfg(target_os = "macos")]
fn is_clauth_launch(pid: u32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    #[allow(unsafe_code)]
    // SAFETY: `proc_pidpath` copies the executable path for `pid` into `buf`,
    // which we own; the return is the byte length, 0 on failure.
    let rc = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as u32,
        )
    };
    if rc <= 0 {
        return false;
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(&buf[..rc as usize]));
    // The executable alone is readable here (proc_pidpath gives no argv), so a
    // `clauth` whose second argument is not `start`/`resume` still reads as a
    // launch: the argv check is unmeasured on macOS.
    path.file_name() == Some(std::ffi::OsStr::new("clauth"))
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_bridge.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_bridge_extra.rs"]
mod extra_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_bridge_fix.rs"]
mod fix_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_bridge_fix2.rs"]
mod fix2_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_bridge_frames.rs"]
mod frames_tests;

#[cfg(test)]
#[path = "../../tests/inline/daemon_bridge_wait.rs"]
mod wait_tests;
