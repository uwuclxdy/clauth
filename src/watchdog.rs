//! Filesystem-event-driven reconcile for credential and config files, with a
//! polling fallback for when events are unavailable or the watcher dies.
//!
//! Watches the parent DIRECTORY of every interesting file rather than the file
//! itself. Every one of those files is published by `rename(2)` (`copy_file`,
//! `atomic_write_600`), which unlinks the watched inode: inotify then drops the
//! watch (`IN_DELETE_SELF` / `IN_IGNORED`) with nothing re-arming it, so a file
//! watch survives exactly one write. A directory inode outlives its children's
//! renames — not its OWN: renaming a watched directory aside kills that watch
//! for the session the same way, just far more rarely, and nothing re-arms it.
//!
//! Dir watches widen the event surface, so [`Interest`] narrows each watch back
//! down to the children that can actually change a reconcile's outcome.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, unbounded};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::logline::logline;

/// The three legs one watchdog iteration can run. Split because the polling
/// fallback runs the config leg ten times per credential leg, where the event
/// path runs all three together.
pub(crate) trait Reconcile {
    /// Cross-profile `.claude.json` + `settings.json` sync.
    fn config(&self);
    /// Credential reconcile between the runtime link and the profile store.
    fn credentials(&self);
    /// Pick up a daemon-requested member swap.
    fn swap_poll(&self);
    /// A tick — the fallback cadence or the polling fallback — just drove a
    /// reconcile where a filesystem event was supposed to. Test-only hook:
    /// impls that pin the event leg count these, everything else keeps the
    /// no-op default. Compiled out of production builds.
    #[cfg(test)]
    fn tick_driven(&self) {}
}

/// Every interval the watchdog loop runs on. A struct rather than consts so a
/// test drives the loop on a bounded wait of milliseconds instead of minutes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timings {
    /// Coalescing window. One write produces several events (CREATE + MODIFY +
    /// CLOSE_WRITE, or MOVED_FROM + MOVED_TO); reconciling per event is waste.
    pub(crate) debounce: Duration,
    /// Minimum spacing between reconciles, measured from the END of the last
    /// one. Every reconcile writes into a watched directory, so without it a
    /// reconcile re-triggers on its own publishes.
    pub(crate) cooldown: Duration,
    /// Safety net for an event the watcher never delivered.
    pub(crate) fallback: Duration,
    /// Polling-fallback config cadence. Tighter than the credential leg because
    /// Claude Code rewrites `.claude.json` constantly; 100 ms keeps the window
    /// in which one profile observes another's stale shared state small. Also
    /// bounds watchdog-thread shutdown latency to one tick.
    pub(crate) config_poll: Duration,
    /// Polling-fallback credential cadence. 1 s instead of longer because
    /// fake-symlink mode needs a tight upper bound on how long a session can
    /// read stale credentials after a sibling refreshes — every additional
    /// second is another window in which a 401 could revoke an already-rotated
    /// refresh token chain.
    pub(crate) credential_poll: Duration,
    /// Swap-poll cadence, on BOTH paths. The daemon's intent lands in
    /// `~/.clauth/live_sessions/`, which no watch covers, so this leg has no
    /// filesystem signal to key on. Its own field rather than a second use of
    /// `credential_poll`: the two happen to share a value but are bounded by
    /// different things — that one by the single-use refresh chain, this one by
    /// how fast a daemon-requested member swap should land.
    pub(crate) swap_poll: Duration,
}

/// What `clauth start` runs on.
pub(crate) const PRODUCTION: Timings = Timings {
    debounce: Duration::from_millis(200),
    cooldown: Duration::from_millis(500),
    fallback: Duration::from_secs(30),
    config_poll: Duration::from_millis(100),
    credential_poll: Duration::from_secs(1),
    swap_poll: Duration::from_secs(1),
};

/// Which children of a watched directory are worth a reconcile.
#[derive(Debug, Clone)]
pub(crate) enum Interest {
    /// Only these names. Used where the directory holds unrelated hot state.
    Names(Vec<OsString>),
    /// Every child except clauth's own staging files — the tree mirror's
    /// surface, where the set of interesting names is the tree itself.
    AnyChild,
}

/// One watched directory plus the children that matter inside it.
#[derive(Debug, Clone)]
pub(crate) struct WatchSpec {
    dir: PathBuf,
    /// The same directory as the event backend spells it. macOS FSEvents
    /// reports realpaths, so a watch armed through a symlinked ancestor (a
    /// symlinked `$HOME`, or `TMPDIR` under `/var/folders` → `/private/var`)
    /// delivers a parent that never equals `dir` and the filter drops every
    /// event — silently, since the watch itself armed fine. Falls back to `dir`
    /// when the directory does not exist, which is already the case where the
    /// arm itself fails: every notify backend rejects a missing path before
    /// arming, so a spec that cannot resolve also never delivers an event. Build
    /// specs AFTER the directories exist, or that fallback quietly restores the
    /// pre-fix behaviour on macOS with nothing said.
    ///
    /// Deliberately NOT `claude::paths_equivalent`'s shape (resolve both sides
    /// at comparison time): that runs on the notify callback thread, once per
    /// event per spec, over `$HOME` — a directory Claude Code rewrites
    /// constantly. Resolving once at construction costs four stat-chains per
    /// session instead, and keeps [`wants`] pure.
    canonical_dir: PathBuf,
    interest: Interest,
}

impl WatchSpec {
    pub(crate) fn new(dir: impl Into<PathBuf>, interest: Interest) -> Self {
        let dir = dir.into();
        let canonical_dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        Self {
            dir,
            canonical_dir,
            interest,
        }
    }
}

/// A running filesystem watcher.
pub(crate) struct EventWatcher {
    /// Held to keep the watcher alive. Dropped on watchdog exit.
    #[allow(dead_code)]
    handle: RecommendedWatcher,
    /// Debounced wake signals from the coalescer thread. Disconnects when the
    /// debouncer thread exits (panic or early return) — the watchdog detects
    /// this and falls back to polling.
    pub(crate) wake: Receiver<()>,
    /// How many of the requested directories actually armed. Below
    /// `specs.len()` the unarmed surface has no event coverage at all, so
    /// [`run`] shortens the fallback rather than leaving it on 30 s.
    armed: usize,
    /// Shared with the notify callback, read by [`run_events`].
    health: Arc<FilterHealth>,
}

impl EventWatcher {
    /// Whether every directory the caller asked for armed. Its own method so
    /// the dead-filter gate is a named predicate a test can assert, rather than
    /// a comparison buried in a `then`.
    pub(crate) fn fully_armed(&self, requested: usize) -> bool {
        self.armed == requested
    }
}

/// What one delivered event means, both for the loop and for the filter's
/// health. Pure, so the rescan rule below is pinned without a backend that can
/// be made to drop events on demand.
struct EventVerdict<'a> {
    /// The loop must reconcile.
    wake: bool,
    /// [`wants`] took at least one of the event's paths.
    matched: bool,
    /// A path whose parent is no spec's directory under EITHER spelling. notify
    /// forwards only children of the key it armed, so an event nobody can
    /// account for means a spelling this watcher will never match.
    orphan: Option<&'a Path>,
}

fn classify<'a>(specs: &[WatchSpec], event: &'a notify::Event) -> EventVerdict<'a> {
    let matched = event.paths.iter().any(|p| wants(specs, p));
    EventVerdict {
        // A dropped-event overflow says the queue lost changes nobody can
        // name, so it must reconcile even though it carries no path.
        wake: matched || event.need_rescan(),
        matched,
        // Only for an event nothing took: one that matched is accounted for,
        // and this walks every spec a second time on the callback thread.
        orphan: (!matched)
            .then(|| event.paths.iter().find(|p| !attributable(specs, p)))
            .flatten()
            .map(PathBuf::as_path),
    }
}

/// Whether `path` belongs to some spec's directory under EITHER spelling.
/// Independent of [`Interest`]: this asks whether the event can be accounted
/// for at all, not whether it is worth a reconcile.
///
/// The path IS a spec directory for every event about a watched directory
/// itself: inotify leaves `name` empty there and notify then reports the watch
/// root (`inotify.rs`, `None => self.paths.get(&event.wd).cloned()`), with
/// `WatchMask::OPEN` armed, so every `opendir` of a watched directory lands
/// here. clauth's own config leg does two of them per reconcile
/// (`runtime::shared_runtime_dirs` reads the profile store directory), which
/// is enough on its own to cross the orphan floor on a healthy install. Such
/// an event came from a watch we armed, so it is accounted for by definition.
fn attributable(specs: &[WatchSpec], path: &Path) -> bool {
    let belongs = |dir: &Path| {
        specs
            .iter()
            .any(|spec| spec.dir == dir || spec.canonical_dir == dir)
    };
    belongs(path) || path.parent().is_some_and(belongs)
}

/// Whether the event filter is matching anything at all.
///
/// [`EventWatcher::armed`] is the only other health signal, and a watcher that
/// matches NO event satisfies it fully: the macOS realpath bug armed every
/// directory and then dropped every event it was handed, so the program
/// reported itself covered while the whole surface sat on the 30 s fallback.
/// Four tests failing on a Mac are what surfaced that; nothing in the running
/// program said a word for the bug's entire life.
///
/// Three counters rather than a match count, because a match count alone cannot
/// tell a dead filter from an idle machine, and an aggregate one cannot tell a
/// dead SPEC from three healthy siblings:
///   * `seen` vs `matched` catches a filter that takes nothing at all (a wrong
///     [`Interest`] name list, where the directory spelling is right);
///   * `orphans` catches one spec whose spelling is wrong while the others
///     match happily, which is the shape a dotfiles-managed `~/.claude` would
///     take in production.
pub(crate) struct FilterHealth {
    /// The watched directories, rendered once at arm time so the diagnostic can
    /// name the surface without the callback thread ever walking specs. Both
    /// spellings where they differ: that difference IS the bug being hunted.
    dirs: String,
    seen: AtomicU64,
    matched: AtomicU64,
    orphans: AtomicU64,
    /// The first unaccountable path, kept so the line names the delivered
    /// spelling rather than only the armed one.
    orphan_sample: OnceLock<PathBuf>,
}

impl FilterHealth {
    fn new(specs: &[WatchSpec]) -> Self {
        let dirs = specs
            .iter()
            .map(|s| {
                if s.canonical_dir == s.dir {
                    s.dir.display().to_string()
                } else {
                    format!("{} (as {})", s.dir.display(), s.canonical_dir.display())
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            dirs,
            seen: AtomicU64::new(0),
            matched: AtomicU64::new(0),
            orphans: AtomicU64::new(0),
            orphan_sample: OnceLock::new(),
        }
    }

    /// Record one event the callback was handed.
    fn saw(&self, verdict: &EventVerdict<'_>) {
        // Both exonerating counters move BEFORE `seen`: a reader landing
        // between two increments then observes a state that under-accuses,
        // never one that never existed.
        if verdict.matched {
            self.matched.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(path) = verdict.orphan {
            // Sampled at the floor, not on the first one: an early stray would
            // otherwise shadow the stream that actually triggers the report,
            // and the sample is the whole diagnosis.
            if self.orphans.fetch_add(1, Ordering::Relaxed) + 1 >= DEAD_FILTER_MIN_EVENTS {
                let _ = self.orphan_sample.set(path.to_path_buf());
            }
        }
        self.seen.fetch_add(1, Ordering::Relaxed);
    }

    fn counts(&self) -> Counts {
        Counts {
            seen: self.seen.load(Ordering::Relaxed),
            matched: self.matched.load(Ordering::Relaxed),
            orphans: self.orphans.load(Ordering::Relaxed),
        }
    }

    /// `, e.g. <path>` for the diagnostic, empty when nothing was orphaned.
    fn orphan_hint(&self) -> String {
        self.orphan_sample
            .get()
            .map(|p| format!(", e.g. {}", p.display()))
            .unwrap_or_default()
    }
}

/// One fallback tick's reading of [`FilterHealth`].
#[derive(Debug, Clone, Copy)]
struct Counts {
    seen: u64,
    matched: u64,
    orphans: u64,
}

/// Fallback intervals a fully-armed watcher gets before its counters are read
/// as a verdict. What this buys is time for a legitimate match to show up, not
/// grace for a burst: `seen` is monotonic, so events counted at arm time still
/// count at the horizon.
const DEAD_FILTER_INTERVALS: u32 = 3;

/// Volume floor under either arm of the check. Unmatched events are the normal
/// steady state — `$HOME` is watched for one name and rewritten by everything —
/// so the signal is a STREAM of them, never a stray one.
const DEAD_FILTER_MIN_EVENTS: u64 = 16;

/// One-shot detector for a fully-armed watcher whose filter is not doing its
/// job. Driven on [`run_events`]'s fallback ticks, the one cadence such a
/// watcher still has: with no matches there are no wakes either.
#[derive(Default)]
struct DeadFilter {
    intervals: u32,
    reported: bool,
}

/// Which reading of the counters fired. The two are different defects with
/// different operator-facing wording: `Unaccountable` fires WITH matches on the
/// healthy specs by construction, so one shared "matched nothing" headline
/// would be flatly wrong for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadFilterKind {
    /// Events arriving under a directory no spec accounts for: one spec's
    /// spelling is wrong while its siblings may be matching fine.
    Unaccountable,
    /// Nothing matched at all, with the spellings all accounted for: the
    /// [`Interest`] lists, not the directories.
    NothingMatched,
}

impl DeadFilter {
    /// What to report, at most ONCE per watcher: the condition holds for the
    /// rest of the session by construction, so repeating it per interval would
    /// turn a defect report into a heartbeat in the log.
    fn fires(&mut self, health: &FilterHealth) -> Option<(DeadFilterKind, Counts)> {
        if self.reported {
            return None;
        }
        self.intervals += 1;
        if self.intervals < DEAD_FILTER_INTERVALS {
            return None;
        }
        let counts = health.counts();
        let kind = if counts.orphans >= DEAD_FILTER_MIN_EVENTS {
            Some(DeadFilterKind::Unaccountable)
        } else if counts.matched == 0 && counts.seen >= DEAD_FILTER_MIN_EVENTS {
            Some(DeadFilterKind::NothingMatched)
        } else {
            None
        };
        self.reported = kind.is_some();
        kind.map(|kind| (kind, counts))
    }
}

/// clauth publishes every file as a hidden `.<name>.tmp.<pid>[.<seq>]` sibling
/// renamed into place (`profile::tmp_sibling`, `relink_to_canonical`). Waking on
/// the staging half costs a reconcile per publish and can only ever observe a
/// path that is about to move anyway.
///
/// Also the skip rule for every walk over a shared fake-mode tree: the mirror's
/// (`runtime::union_children`) and the acquire-time build's, which is both the
/// top-level walk in `runtime::build_runtime_dir_with_active_env` and
/// `runtime::copy_tree`'s recursion under it. A walk that treats one as tree
/// content either fails when the source is renamed away mid-copy, or lands an
/// orphan the mirror can never delete. On Windows it fails a third way, which is
/// how this surfaced: share modes are per-handle, so a source another THREAD of
/// this same process holds open for writing refuses the copy outright.
pub(crate) fn is_staging(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| n.starts_with('.') && n.contains(".tmp."))
}

/// Whether a changed `path` can alter a reconcile's outcome. Pure — both
/// spellings a spec compares against were resolved when it was built — so the
/// filter that bounds the event surface is pinned without a filesystem.
fn wants(specs: &[WatchSpec], path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    specs.iter().any(|spec| {
        (spec.dir == dir || spec.canonical_dir == dir)
            && match &spec.interest {
                Interest::Names(names) => names.iter().any(|n| n.as_os_str() == name),
                Interest::AnyChild => !is_staging(name),
            }
    })
}

/// The directories the watchdog watches, and what it cares about in each.
///
/// The runtime tree and `~/.claude/` take every child: fake-symlink mode mirrors
/// them against each other, so any entry appearing on one side is reconcile
/// input. The profile store and `$HOME` take a name list — the first holds the
/// per-profile JSON caches a scheduler rewrites on its own cadence, the second
/// is the operator's whole home directory and only `.claude.json` is ours.
///
/// Ceiling: NonRecursive, so a change nested under `~/.claude/projects/` or the
/// runtime tree reaches reconcile on the fallback interval rather than on its
/// event. Recursive would cost one inotify watch per project directory and turn
/// every Claude Code transcript append into an event. Upgrade path if deep
/// fake-mode latency ever matters: watch `<tree>/projects` explicitly rather
/// than making the whole tree recursive.
pub(crate) fn watch_specs(
    runtime: &Path,
    canonical_creds: &Path,
    claude_home: &Path,
) -> Vec<WatchSpec> {
    let mut specs = Vec::with_capacity(4);

    // Runtime tree: `.credentials.json` (Claude Code rewrites it on a re-login),
    // `settings.json`, and every entry the fake-mode mirror carries.
    specs.push(WatchSpec::new(runtime, Interest::AnyChild));

    // The profile's credential store. A swap moves `canonical_creds` to another
    // member's directory and this list is not rebuilt, which costs nothing: a
    // swap only happens under `LinkMode::Real`, where the runtime path is a
    // symlink onto the store and a store-side write needs no reconcile to be
    // visible. Fake mode, where the mirror does need it, never swaps.
    if let (Some(dir), Some(name)) = (canonical_creds.parent(), canonical_creds.file_name()) {
        specs.push(WatchSpec::new(dir, Interest::Names(vec![name.to_owned()])));
    }

    // Global `.claude.json`, a sibling of `~/.claude/` rather than a child.
    if let Some(home) = claude_home.parent() {
        specs.push(WatchSpec::new(
            home,
            Interest::Names(vec![OsString::from(".claude.json")]),
        ));
    }

    // The operator's `~/.claude/`: `settings.json` plus the mirror's other side.
    specs.push(WatchSpec::new(claude_home, Interest::AnyChild));

    specs
}

/// Try to create a filesystem watcher for `specs`.
///
/// A directory that cannot be armed is logged and skipped rather than failing
/// the whole watcher. `inotify_add_watch` fails per-directory, so a box at
/// `fs.inotify.max_user_watches` hands back some arms and not others, and the
/// ones that did arm are worth keeping. What partial arming must NOT do is leave
/// the rest on the 30 s fallback, which is 30x worse than the poll it replaced —
/// [`run`] reads [`EventWatcher::armed`] and shortens the fallback for that.
///
/// Returns `None` only when nothing could be armed, or when `notify` itself is
/// unavailable (inotify instance limit, unsupported platform); the caller then
/// falls back to polling.
pub(crate) fn try_start(specs: &[WatchSpec], debounce: Duration) -> Option<EventWatcher> {
    let (raw_tx, raw_rx) = unbounded();

    let filter: Vec<WatchSpec> = specs.to_vec();
    let health = Arc::new(FilterHealth::new(specs));
    let callback_health = Arc::clone(&health);
    let mut handle = match notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };
            let verdict = classify(&filter, &event);
            callback_health.saw(&verdict);
            if verdict.wake {
                let _ = raw_tx.send(());
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            logline!("clauth: fs watcher unavailable: {e}");
            return None;
        }
    };

    let mut armed = 0usize;
    for spec in specs {
        match handle.watch(&spec.dir, RecursiveMode::NonRecursive) {
            Ok(()) => armed += 1,
            Err(e) => logline!(
                "clauth: fs watcher cannot watch {}: {e}",
                spec.dir.display()
            ),
        }
    }
    if armed == 0 {
        return None;
    }

    let (wake_tx, wake_rx) = bounded::<()>(1);

    // Debouncer thread: coalesces a burst of events into one wake. Detached —
    // it ends when `RecommendedWatcher`'s drop disconnects `raw_rx`, and its
    // exit drops `wake_tx`, which is how the loop learns the debouncer died.
    std::thread::Builder::new()
        .name("clauth-wdog-evt".into())
        .spawn(move || debounce_loop(&raw_rx, &wake_tx, debounce))
        .ok()?;

    Some(EventWatcher {
        handle,
        wake: wake_rx,
        armed,
        health,
    })
}

/// Coalesce raw events into wakes, one per `debounce`-long window, until either
/// channel disconnects.
///
/// Split out of the spawn so the window logic is testable without a filesystem
/// and without the `bounded(1)` wake channel: under starvation that channel
/// drops signals by design, which makes a wake COUNT taken through it a measure
/// of the consumer's scheduling rather than of this loop.
fn debounce_loop(
    raw_rx: &Receiver<()>,
    wake_tx: &crossbeam_channel::Sender<()>,
    debounce: Duration,
) {
    // Signal, coalescing against a wake already queued. `Full` needs no second
    // signal: an unconsumed wake already guarantees a reconcile that STARTS
    // after this event, which is the property that matters. Blocking instead
    // would stall the drain below behind a reconcile.
    let signal = || {
        !matches!(
            wake_tx.try_send(()),
            Err(crossbeam_channel::TrySendError::Disconnected(()))
        )
    };
    loop {
        // Block until the first event of a burst, or until the watcher is
        // dropped (which disconnects raw_rx).
        if raw_rx.recv().is_err() || !signal() {
            return;
        }
        // Coalesce for a FIXED window rather than until events go idle. A
        // sustained write stream never goes idle, so an idle-gap window emits
        // one wake at the head of the burst and then nothing at all until the
        // stream stops.
        let window_ends = Instant::now() + debounce;
        let mut coalesced = false;
        loop {
            let left = window_ends.saturating_duration_since(Instant::now());
            // Leave on the CLOCK, not on the queue going quiet. `recv_timeout`
            // with nothing left to wait returns the next queued event rather
            // than timing out, so draining until it times out means draining
            // until the producer pauses — which is the idle-gap behavior this
            // window replaced, reappearing exactly when events outrun the
            // drain. What is still queued belongs to the next window.
            if left.is_zero() {
                break;
            }
            match raw_rx.recv_timeout(left) {
                Ok(()) => coalesced = true,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
        // The head wake was emitted BEFORE everything this window swallowed, and
        // the consumer drains it in microseconds against a window of hundreds of
        // milliseconds. Without a wake strictly after the last coalesced event,
        // that event has no replay at all — the same drop-with-no-replay the
        // loop refuses to do one layer down.
        if coalesced && !signal() {
            return;
        }
    }
}

/// Why the event loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// The shutdown channel fired or disconnected.
    Shutdown,
    /// The debouncer thread died; the caller must fall back to polling.
    WatcherLost,
}

/// Run the watchdog until `shutdown` fires: event-driven while a watcher can be
/// armed, polling otherwise.
///
/// A partially-armed watcher runs the event loop on a SHORTENED fallback. The
/// unarmed directories have no event coverage, and leaving them on the 30 s
/// safety net would be 30x worse than the 1 s credential poll this path replaced
/// — the one outcome a "some coverage is better than none" degradation must not
/// buy.
///
/// Ceiling: one clamped ticker cannot reproduce the poll's split cadence (config
/// at 100 ms, credentials at 1 s), so an unarmed directory's CONFIG surface
/// lands at 1 s — still 10x slower than polling would have been. Clamping to
/// `config_poll` instead would fix that and pull the credential leg's state
/// flock to 10 Hz, which the split existed to avoid. Upgrade path if it ever
/// matters: give the event loop its own config ticker rather than one fallback.
/// The reconcile loop over an ALREADY-ARMED watcher, `requested` being how many
/// directories the caller asked for so a partial arm is still detectable.
///
/// Takes the watcher rather than arming one, so a caller can arm BEFORE handing
/// the loop to a thread. Arming is not free — 18-34 ms on macOS, where FSEvents
/// resolves and registers each directory — and a caller that spawns first has no
/// signal for when its watch went live. A write landing in that window generates
/// no event at all, so a fully-armed watcher then waits out its entire 30 s
/// fallback. There is deliberately no arm-then-spawn convenience wrapper here:
/// every caller holding the watcher first is what makes that race unwritable.
pub(crate) fn run_with_watcher(
    watcher: Option<EventWatcher>,
    requested: usize,
    shutdown: &Receiver<()>,
    t: &Timings,
    r: &dyn Reconcile,
) {
    if let Some(watcher) = watcher {
        let mut t = *t;
        // Only a FULLY armed watcher is a candidate for the dead-filter check:
        // an unarmed directory already explains a missing match, and it reports
        // itself below, so the two diagnostics never accuse the same session.
        let health = watcher
            .fully_armed(requested)
            .then(|| watcher.health.as_ref());
        if watcher.armed < requested {
            // Said once here rather than left to the per-directory arm errors:
            // those name a moment, this names a cost the whole session pays.
            logline!(
                "clauth: fs watcher armed {} of {} directories; the rest reconcile \
                 every {:?} instead of on their events",
                watcher.armed,
                requested,
                t.credential_poll
            );
            t.fallback = t.credential_poll;
        }
        match run_events(&watcher.wake, shutdown, &t, r, health) {
            Exit::Shutdown => return,
            Exit::WatcherLost => {
                logline!("clauth: fs watcher event channel disconnected, switching to poll")
            }
        }
    }
    run_poll(shutdown, t, r);
}

/// Event-driven loop. Reconciles on a wake, no faster than one `cooldown` after
/// the previous reconcile RETURNED, with the fallback ticker covering an event
/// that never arrived.
///
/// `health` is `Some` only for a fully-armed watcher, whose filter is then held
/// to matching something (see [`FilterHealth`]).
pub(crate) fn run_events(
    wake: &Receiver<()>,
    shutdown: &Receiver<()>,
    t: &Timings,
    r: &dyn Reconcile,
    health: Option<&FilterHealth>,
) -> Exit {
    let fallback = crossbeam_channel::tick(t.fallback);
    let swap_poll = crossbeam_channel::tick(t.swap_poll);
    // `None` rather than a back-dated `Instant`: the first wake must reconcile
    // at once, and `Instant` arithmetic can underflow near boot.
    let mut last_reconcile: Option<Instant> = None;
    // A wake inside the cooldown is DEFERRED, never dropped: `wake` is
    // `bounded(1)` and the debouncer discards what it coalesces, so a dropped
    // one has no replay and its change waits out the whole fallback interval.
    let mut pending = false;
    let mut dead = DeadFilter::default();

    loop {
        let idle = if pending {
            last_reconcile.map_or(Duration::ZERO, |at| t.cooldown.saturating_sub(at.elapsed()))
        } else {
            t.fallback
        };
        crossbeam_channel::select! {
            recv(shutdown) -> _ => return Exit::Shutdown,
            recv(swap_poll) -> _ => r.swap_poll(),
            recv(wake) -> res => {
                if res.is_err() {
                    return Exit::WatcherLost;
                }
                pending = true;
            }
            recv(fallback) -> _ => {
                #[cfg(test)]
                r.tick_driven();
                pending = true;
                if let Some(health) = health
                    && let Some((kind, counts)) = dead.fires(health)
                {
                    // Log only. Unlike a partial arm this state is never
                    // legitimate, so shortening the fallback would buy the
                    // latency back while hiding the defect that has to be fixed.
                    match kind {
                        DeadFilterKind::Unaccountable => logline!(
                            "clauth: fs watcher is being handed events under a directory it \
                             cannot account for ({} of {} seen{}), so that surface reconciles \
                             on the {:?} fallback rather than on its own events. Watched: {}",
                            counts.orphans,
                            counts.seen,
                            health.orphan_hint(),
                            t.fallback,
                            health.dirs
                        ),
                        DeadFilterKind::NothingMatched => logline!(
                            "clauth: fs watcher armed every directory but matched none of its \
                             {} events, so every change reconciles on the {:?} fallback rather \
                             than on its own event. Watched: {}",
                            counts.seen,
                            t.fallback,
                            health.dirs
                        ),
                    }
                }
            }
            // Only reachable with a deferred wake outstanding; `idle` is then
            // exactly what is left of its cooldown.
            default(idle) => {}
        }
        if pending && last_reconcile.is_none_or(|at| at.elapsed() >= t.cooldown) {
            pending = false;
            r.config();
            r.credentials();
            r.swap_poll();
            last_reconcile = Some(Instant::now());
        }
    }
}

/// Polling fallback, reached when no directory could be armed or the debouncer
/// died. Config reconcile every `config_poll`, credentials every
/// `credential_poll`.
pub(crate) fn run_poll(shutdown: &Receiver<()>, t: &Timings, r: &dyn Reconcile) {
    let cred_every = (t.credential_poll.as_millis() / t.config_poll.as_millis().max(1)).max(1);
    let mut until_cred = cred_every;
    let ticker = crossbeam_channel::tick(t.config_poll);
    loop {
        crossbeam_channel::select! {
            recv(shutdown) -> _ => return,
            recv(ticker) -> _ => {
                #[cfg(test)]
                r.tick_driven();
                r.config();
                until_cred -= 1;
                if until_cred == 0 {
                    until_cred = cred_every;
                    r.credentials();
                    r.swap_poll();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/watchdog.rs"]
mod tests;
