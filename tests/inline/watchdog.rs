//! Inline tests for `crate::watchdog` — the event filter, the watch's survival
//! of the rename every clauth write publishes through, the two loop properties
//! (cooldown measured from the reconcile's END, a cooled-down wake deferred
//! rather than dropped), and the filter-health signal that keeps a watcher
//! matching nothing from reading as a healthy one.
//!
//! The loop tests drive `run_events` through a plain channel instead of a real
//! watcher: the loop's timing behavior is what they pin, and a filesystem in the
//! path would only add flake. `a_dir_watch_survives_...` is the one that needs
//! real inotify, and every wait in this file is bounded.

use super::*;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_channel::Sender;

/// Long enough that a fallback tick cannot be mistaken for an event-driven or
/// deferred reconcile inside any of these tests.
const NEVER: Duration = Duration::from_secs(600);

/// Every wait here is bounded by this rather than blocking, so a regression
/// fails the suite instead of hanging it.
const BOUND: Duration = Duration::from_secs(5);

fn timings(cooldown: Duration) -> Timings {
    Timings {
        debounce: Duration::from_millis(20),
        cooldown,
        fallback: NEVER,
        config_poll: NEVER,
        credential_poll: NEVER,
        swap_poll: NEVER,
    }
}

/// One credential leg, as the loop itself saw it.
#[derive(Debug, Clone, Copy)]
struct Pass {
    entered: Instant,
    returned: Instant,
    /// Counters read INSIDE the leg, not by the test thread afterwards. The loop
    /// signals `done` at the END of `credentials()` and only then calls
    /// `swap_poll()`, so a cross-thread read after that signal races the loop's
    /// own next step and reads whatever it happens to catch.
    configs: usize,
    swap_polls: usize,
}

/// Records one [`Pass`] per credential leg and signals it, so a test can await
/// each reconcile on a deadline. `work` simulates a reconcile slower than its own
/// cooldown — the fake-mode tree walk plus a state flock that can block for 25 s.
struct Recorder {
    passes: Mutex<Vec<Pass>>,
    configs: AtomicUsize,
    swap_polls: AtomicUsize,
    work: Duration,
    done: Sender<()>,
}

impl Recorder {
    fn new(work: Duration, done: Sender<()>) -> Self {
        Self {
            passes: Mutex::new(Vec::new()),
            configs: AtomicUsize::new(0),
            swap_polls: AtomicUsize::new(0),
            work,
            done,
        }
    }

    fn pass(&self, i: usize) -> Pass {
        self.passes.lock().unwrap_or_else(|p| p.into_inner())[i]
    }
}

impl Reconcile for Recorder {
    fn config(&self) {
        self.configs.fetch_add(1, Ordering::Relaxed);
    }
    fn credentials(&self) {
        let entered = Instant::now();
        std::thread::sleep(self.work);
        self.passes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Pass {
                entered,
                returned: Instant::now(),
                configs: self.configs.load(Ordering::Relaxed),
                swap_polls: self.swap_polls.load(Ordering::Relaxed),
            });
        let _ = self.done.send(());
    }
    fn swap_poll(&self) {
        self.swap_polls.fetch_add(1, Ordering::Relaxed);
    }
}

/// An event the filter refused whose directory it still recognises — the steady
/// state of watching `$HOME` for one name.
fn refused() -> EventVerdict<'static> {
    EventVerdict {
        wake: false,
        matched: false,
        orphan: None,
    }
}

/// An event `wants` took.
fn took() -> EventVerdict<'static> {
    EventVerdict {
        wake: true,
        matched: true,
        orphan: None,
    }
}

/// An event delivered under a directory no spec can account for.
fn orphaned(path: &Path) -> EventVerdict<'_> {
    EventVerdict {
        wake: false,
        matched: false,
        orphan: Some(path),
    }
}

/// Exactly how `copy_file` and `atomic_write_600` land a file: write a hidden
/// staging sibling, then rename it over the target.
fn publish(dst: &Path, bytes: &[u8]) {
    let staging = crate::profile::tmp_sibling(dst);
    std::fs::write(&staging, bytes).expect("write staging");
    std::fs::rename(&staging, dst).expect("publish");
}

/// The defect that made the event path permanently self-disabling: a watch on
/// the FILE arms `IN_DELETE_SELF`/`IN_MOVE_SELF`, and the rename that publishes
/// every clauth-written file unlinks that inode, so notify drops the watch with
/// nothing re-arming it. One write per path and the watcher is dead — silently,
/// because the channel stays connected. A directory inode outlives its
/// children's renames.
#[test]
fn a_dir_watch_survives_the_rename_that_publishes_a_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("settings.json");
    std::fs::write(&target, b"{}").expect("seed target");

    let debounce = Duration::from_millis(50);
    let specs = vec![WatchSpec::new(
        tmp.path(),
        Interest::Names(vec!["settings.json".into()]),
    )];
    let watcher = try_start(&specs, debounce).expect("watcher");

    for round in 0..3u32 {
        // Clear of the previous burst's coalescing window, so this round tests
        // only the watch's survival and nothing about coalescing.
        std::thread::sleep(debounce * 3);
        publish(&target, format!(r#"{{"round":{round}}}"#).as_bytes());
        assert!(
            watcher.wake.recv_timeout(BOUND).is_ok(),
            "publish {round} produced no wake: the watch did not survive a rename"
        );
    }
}

/// A publish landing INSIDE a coalescing window still has to reach reconcile.
/// The head wake is emitted before that publish exists and the consumer takes it
/// in microseconds against a window of hundreds of milliseconds, so without a
/// wake emitted at the END of the window the change has no replay at all and
/// waits out the entire fallback — the drop-with-no-replay `run_events` refuses
/// to do one layer down.
#[test]
fn a_publish_inside_the_coalescing_window_still_wakes_the_watchdog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("settings.json");
    std::fs::write(&target, b"{}").expect("seed target");

    let debounce = Duration::from_millis(100);
    let specs = vec![WatchSpec::new(
        tmp.path(),
        Interest::Names(vec!["settings.json".into()]),
    )];
    let watcher = try_start(&specs, debounce).expect("watcher");

    publish(&target, br#"{"round":0}"#);
    watcher
        .wake
        .recv_timeout(BOUND)
        .expect("the head publish produced no wake");

    // No sleep: this lands while the first burst's window is still open, which
    // is where a credential write racing an unrelated one in the same directory
    // actually lands.
    publish(&target, br#"{"round":1}"#);
    assert!(
        watcher.wake.recv_timeout(BOUND).is_ok(),
        "a publish inside the coalescing window was swallowed with no replay"
    );
}

/// A burst coalesces into one wake per WINDOW, never into one wake for the whole
/// burst. A stream that keeps the queue non-empty never goes idle, so an idle-gap
/// window emits at the head and then nothing until the stream stops — every
/// change in between reaching reconcile only on the fallback interval.
///
/// Driven through `debounce_loop` directly, with an UNBOUNDED wake channel and
/// the count taken after the feed stops. Measuring through the production
/// `bounded(1)` channel instead counts what a consumer managed to dequeue, which
/// under starvation is 1 no matter how the window behaves — pinned to one CPU
/// that read the healthy debouncer as the bug it was written to catch.
#[test]
fn a_sustained_event_stream_wakes_once_per_window_not_once_per_burst() {
    let debounce = Duration::from_millis(30);
    let windows = 20;
    let (raw_tx, raw_rx) = unbounded();
    let (wake_tx, wake_rx) = unbounded();

    let mut events = 0u32;
    std::thread::scope(|scope| {
        scope.spawn(|| debounce_loop(&raw_rx, &wake_tx, debounce));

        let deadline = Instant::now() + debounce * windows;
        while Instant::now() < deadline {
            events += 1;
            raw_tx.send(()).expect("feed");
            std::thread::sleep(debounce / 6);
        }
        // Ends the loop, so the count below is total signals emitted rather than
        // a sample taken while it was still running.
        drop(raw_tx);
    });

    // The scope borrowed `wake_tx` for the loop; releasing it here is what lets
    // `iter()` terminate instead of blocking on a sender that still exists.
    drop(wake_tx);
    let wakes = wake_rx.iter().count();
    assert!(
        wakes >= 5,
        "{events} events with no idle gap over {windows} windows produced \
         {wakes} wakes: the whole burst was coalesced into one"
    );
}

/// Signals only once the store actually holds `want`, so a reconcile that ran
/// for some other reason cannot satisfy the wait.
///
/// A wake alone is NOT evidence here. macOS hands a freshly-armed FSEvents
/// stream operations that PRECEDED the arm — this fixture's own `create_dir_all`
/// calls and seed write among them — so a reconcile fires before the publish
/// under test happens at all. Counting wakes, the test passed in 0.166 s with
/// its `publish` line deleted. Reading the bytes is what ties the reconcile to
/// the publish instead of to the seeding, on both platforms.
struct AwaitContent {
    store: PathBuf,
    want: &'static [u8],
    done: Sender<()>,
    /// Ticks the loop ran. The forbidden leg: a ticker driving the reconcile
    /// this test pins to the publish event.
    ticks: AtomicUsize,
}

impl AwaitContent {
    fn ticks(&self) -> usize {
        self.ticks.load(Ordering::Relaxed)
    }
}

impl Reconcile for AwaitContent {
    fn config(&self) {}
    fn credentials(&self) {
        if std::fs::read(&self.store).is_ok_and(|got| got == self.want) {
            let _ = self.done.send(());
        }
    }
    fn swap_poll(&self) {}
    fn tick_driven(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }
}

/// Every ticker is set past the bound, so a publish into a watched directory is
/// the only thing that can drive a reconcile at all. The tick counter below is
/// what makes a green here mean the event path and not a poll that happened to
/// land: a reconcile reached on a tick instead of the publish's event reads 1.
#[test]
fn a_store_publish_reconciles_with_every_ticker_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("store").join("credentials.json");
    let runtime = tmp.path().join("runtime-1-0");
    let claude_home = tmp.path().join(".claude");
    for dir in [
        store.parent().expect("store parent"),
        runtime.as_path(),
        claude_home.as_path(),
    ] {
        std::fs::create_dir_all(dir).expect("mkdir");
    }
    std::fs::write(&store, b"{}").expect("seed store");

    let specs = watch_specs(&runtime, &store, &claude_home);
    let t = timings(Duration::ZERO);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = crossbeam_channel::unbounded();
    const FRESH: &[u8] = br#"{"claudeAiOauth":{"accessToken":"fresh"}}"#;
    let rec = AwaitContent {
        store: store.clone(),
        want: FRESH,
        done: done_tx,
        ticks: AtomicUsize::new(0),
    };

    // Armed before the spawn, exactly as `runtime::acquire` does it, so the
    // publish below cannot beat the watch up. This used to be a
    // `sleep(debounce * 5)` after the spawn: a blind constant that happened to
    // clear the ~34 ms macOS arm with little margin on a loaded runner.
    let watcher = try_start(&specs, t.debounce);
    let requested = specs.len();
    let (shutdown, timings, recorder) = (&shutdown_rx, &t, &rec);

    std::thread::scope(|scope| {
        scope.spawn(move || run_with_watcher(watcher, requested, shutdown, timings, recorder));

        publish(&store, FRESH);
        done_rx
            .recv_timeout(BOUND)
            .expect("a publish into the credential store drove no reconcile");

        drop(shutdown_tx);
    });

    // Read after the scope: the loop has exited, so this is every tick the
    // watchdog ever ran, not a sample taken at the moment the publish was seen.
    assert_eq!(
        rec.ticks(),
        0,
        "the reconcile that observed the publish ran on a tick, not on the \
         filesystem event: every ticker is disabled, so any tick means the \
         loop ignored its timings"
    );
}

/// The filter is what keeps a directory watch from costing a reconcile per
/// unrelated write in a hot directory — and what keeps clauth's own staging
/// halves from waking the loop on every publish it makes.
#[test]
fn the_filter_takes_named_children_and_drops_staging_siblings() {
    let store = Path::new("/clauth/profiles/acct");
    let tree = Path::new("/clauth/profiles/acct/runtime-1-0");
    let specs = vec![
        WatchSpec::new(store, Interest::Names(vec!["credentials.json".into()])),
        WatchSpec::new(tree, Interest::AnyChild),
    ];

    assert!(wants(&specs, &store.join("credentials.json")));
    assert!(
        !wants(&specs, &store.join("kick_block.json")),
        "an unnamed sibling in the store must not wake the watchdog"
    );
    assert!(
        !wants(&specs, &store.join("sub").join("credentials.json")),
        "the watch is NonRecursive, so a nested path is not this directory's child"
    );

    assert!(wants(&specs, &tree.join("statusline.sh")));
    assert!(wants(&specs, &tree.join(".credentials.json")));
    assert!(
        !wants(
            &specs,
            &crate::profile::tmp_sibling(&tree.join("settings.json"))
        ),
        "`tmp_sibling`'s staging half is our own write in flight"
    );
    assert!(
        // `runtime::relink_to_canonical`'s staging name, which carries no seq.
        !wants(&specs, &tree.join(".credentials.json.tmp.4242")),
        "the relink staging half is our own write in flight"
    );
    assert!(!wants(&specs, Path::new("/elsewhere/credentials.json")));
}

/// A watch armed through a symlinked ancestor must still take its events. macOS
/// FSEvents reports realpaths, so such a watch delivers a parent that never
/// equals the spelling it was armed on, `wants` drops every event, and the
/// watchdog falls back to its 30 s poll while still reporting itself armed.
///
/// Every macOS test run hits it: `HOME_OVERRIDE` points at a `tempfile` dir
/// under `TMPDIR`, which lives in `/var/folders`, a symlink onto
/// `/private/var/folders`. Production reaches it only where a spec directory
/// itself resolves through a symlink — a dotfiles-managed `~/.claude` is the
/// plausible one — since `home_dir()` is otherwise `/Users/<name>`, which
/// resolves to itself.
///
/// Posed with an explicit symlink rather than by leaning on `TMPDIR`, so the
/// guard is pinned on every platform instead of only where the temp dir happens
/// to be symlinked.
#[cfg(unix)]
#[test]
fn a_watch_armed_through_a_symlink_takes_events_spelled_by_realpath() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).expect("real dir");
    // The realpath as the backend would deliver it: `tmp` is itself under a
    // symlinked ancestor on macOS, so joining is not enough.
    let real = std::fs::canonicalize(&real).expect("realpath");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let specs = vec![WatchSpec::new(
        &link,
        Interest::Names(vec!["credentials.json".into()]),
    )];

    assert!(
        wants(&specs, &link.join("credentials.json")),
        "the spelling the watch was armed on must still match"
    );
    assert!(
        wants(&specs, &real.join("credentials.json")),
        "the realpath spelling FSEvents delivers must match too, or macOS never wakes"
    );
    assert!(
        !wants(&specs, &real.join("kick_block.json")),
        "resolving the directory must not widen which names the filter takes"
    );
    // The health side of the same both-spellings rule: a realpath delivery is
    // accounted for, so a working macOS watcher never accuses itself.
    let refused_but_known = notify::Event::new(notify::EventKind::Other)
        .add_path(real.join("kick_block.json"))
        .add_path(link.join("kick_block.json"));
    let verdict = classify(&specs, &refused_but_known);
    assert!(!verdict.matched, "neither name is in the interest list");
    assert!(
        verdict.orphan.is_none(),
        "a child of the watched directory under EITHER spelling is accounted \
         for; reading one as unaccountable makes a healthy mac accuse itself"
    );
}

/// `wake` is `bounded(1)` and the debouncer discards what it coalesces, so an
/// event dropped for being inside the cooldown has no replay: its change waits
/// out the entire fallback interval. It must be deferred and serviced once the
/// cooldown expires.
#[test]
fn a_wake_inside_the_cooldown_is_deferred_not_dropped() {
    let cooldown = Duration::from_millis(300);
    let t = timings(cooldown);
    let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);

    std::thread::scope(|scope| {
        scope.spawn(|| run_events(&wake_rx, &shutdown_rx, &t, &rec, None));

        wake_tx.send(()).expect("first wake");
        done_rx.recv_timeout(BOUND).expect("first reconcile");
        // Lands well inside the cooldown the reconcile just started.
        wake_tx.send(()).expect("second wake");
        done_rx
            .recv_timeout(BOUND)
            .expect("the cooled-down wake was dropped instead of deferred");

        drop(shutdown_tx);
    });

    assert_eq!(rec.pass(1).configs, 2, "both reconciles must run every leg");
}

/// Stamping the cooldown before the reconcile spends it on the reconcile
/// itself: anything slower than the cooldown (a fake-mode tree walk, a
/// `with_state_lock` that can block for 25 s) returns already cooled down and
/// re-triggers on its own writes.
#[test]
fn the_cooldown_is_measured_from_the_end_of_the_reconcile() {
    let cooldown = Duration::from_millis(200);
    let work = cooldown * 2;
    let t = timings(cooldown);
    let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(work, done_tx);

    std::thread::scope(|scope| {
        scope.spawn(|| run_events(&wake_rx, &shutdown_rx, &t, &rec, None));

        wake_tx.send(()).expect("first wake");
        // Mid-reconcile, standing in for the events that reconcile's own writes
        // produce in the directories it publishes into.
        std::thread::sleep(work / 4);
        wake_tx.send(()).expect("second wake");

        done_rx.recv_timeout(BOUND).expect("first reconcile");
        done_rx.recv_timeout(BOUND).expect("second reconcile");

        drop(shutdown_tx);
    });

    let gap = rec.pass(1).entered.duration_since(rec.pass(0).returned);
    assert!(
        gap >= cooldown,
        "the second reconcile started {gap:?} after the first returned, \
         inside the {cooldown:?} cooldown: the cooldown was spent by the \
         reconcile that owned it"
    );
}

/// Events unavailable — no directory could be armed — must still reconcile, on
/// the poll cadence and at the poll ratio.
#[test]
fn the_poll_fallback_reconciles_when_no_directory_can_be_armed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs = vec![WatchSpec::new(
        tmp.path().join("does-not-exist"),
        Interest::AnyChild,
    )];
    let t = Timings {
        debounce: Duration::from_millis(20),
        cooldown: Duration::ZERO,
        fallback: NEVER,
        config_poll: Duration::from_millis(20),
        credential_poll: Duration::from_millis(200),
        swap_poll: Duration::from_millis(200),
    };
    assert!(
        try_start(&specs, t.debounce).is_none(),
        "an unwatchable directory must leave the caller on the polling path"
    );

    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);

    let (shutdown, timings, recorder) = (&shutdown_rx, &t, &rec);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            run_with_watcher(
                try_start(&specs, t.debounce),
                specs.len(),
                shutdown,
                timings,
                recorder,
            )
        });

        done_rx
            .recv_timeout(BOUND)
            .expect("the polling fallback never reconciled credentials");
        done_rx
            .recv_timeout(BOUND)
            .expect("the polling fallback stopped after one credential reconcile");

        drop(shutdown_tx);
    });

    // Read off the legs themselves: the loop bumps `swap_polls` AFTER the signal
    // `done` rides, so a cross-thread read here trails the loop by a step.
    assert_eq!(
        rec.pass(0).configs,
        10,
        "the config leg runs `credential_poll / config_poll` times per credential leg"
    );
    assert_eq!(
        rec.pass(1).configs,
        20,
        "and keeps that ratio on the second credential leg"
    );
    assert_eq!(
        rec.pass(1).swap_polls,
        1,
        "each credential leg is followed by exactly one swap poll"
    );
}

/// Partial arming must not silently cost 30x. The unarmed directories have no
/// event coverage at all, so leaving them on the 30 s safety net is worse than
/// the 1 s poll this path replaced — the loop shortens the fallback instead.
#[test]
fn a_partially_armed_watcher_shortens_the_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let live = tmp.path().join("live");
    std::fs::create_dir_all(&live).expect("mkdir live");
    let specs = vec![
        WatchSpec::new(&live, Interest::AnyChild),
        WatchSpec::new(tmp.path().join("does-not-exist"), Interest::AnyChild),
    ];
    let t = Timings {
        debounce: Duration::from_millis(20),
        cooldown: Duration::ZERO,
        // Both far past the bound, so only the CLAMP can explain a reconcile.
        fallback: NEVER,
        config_poll: NEVER,
        credential_poll: Duration::from_millis(200),
        swap_poll: NEVER,
    };
    let watcher = try_start(&specs, t.debounce).expect("one directory still arms");
    assert_eq!(watcher.armed, 1, "exactly one of the two must have armed");
    assert!(
        !watcher.fully_armed(specs.len()),
        "a partial arm must not be handed to the dead-filter check: its unarmed \
         directory already explains every missing match"
    );
    assert!(
        watcher.fully_armed(1),
        "and the predicate must read the REQUESTED count, not a constant"
    );
    drop(watcher);

    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);

    let (shutdown, timings, recorder) = (&shutdown_rx, &t, &rec);
    std::thread::scope(|scope| {
        scope.spawn(move || {
            run_with_watcher(
                try_start(&specs, t.debounce),
                specs.len(),
                shutdown,
                timings,
                recorder,
            )
        });

        // Nothing is published, so no event exists: a reconcile inside the bound
        // can only come from the fallback having been clamped to
        // `credential_poll`. Unclamped it would be `NEVER`.
        done_rx
            .recv_timeout(BOUND)
            .expect("a partially armed watcher left the unarmed surface on the long fallback");

        drop(shutdown_tx);
    });
}

/// The callback has to count events it was HANDED apart from the ones it took.
/// From the match count alone a filter that matches nothing reads exactly like
/// a quiet disk, which is the ambiguity that let the macOS realpath bug sit
/// undetected for its whole life.
#[test]
fn the_callback_counts_events_it_was_handed_apart_from_the_ones_it_took() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let debounce = Duration::from_millis(20);
    let specs = vec![WatchSpec::new(
        tmp.path(),
        Interest::Names(vec!["settings.json".into()]),
    )];
    let watcher = try_start(&specs, debounce).expect("watcher");

    // A name the filter refuses, inside a directory it watches: the backend
    // hands the callback an event and `wants` drops it.
    std::fs::write(tmp.path().join("unrelated.json"), b"{}").expect("write unrelated");
    let deadline = Instant::now() + BOUND;
    while watcher.health.counts().seen == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let counts = watcher.health.counts();
    assert!(
        counts.seen > 0,
        "an event the filter refused was never counted as seen, so a dead filter \
         is indistinguishable from an idle machine"
    );
    assert_eq!(
        counts.matched, 0,
        "a refused event must not count as a match"
    );
    assert_eq!(
        counts.orphans, 0,
        "a refused CHILD of a watched directory is accounted for: the spelling \
         is right and only the name was uninteresting"
    );

    publish(&tmp.path().join("settings.json"), b"{}");
    watcher
        .wake
        .recv_timeout(BOUND)
        .expect("the watched name produced no wake");
    assert!(
        watcher.health.counts().matched > 0,
        "the event that woke the loop was never counted as a match"
    );
}

/// An event about a watched directory ITSELF is accounted for by definition:
/// it came from a watch we armed. inotify leaves `name` empty for those and
/// notify then reports the watch root, with `WatchMask::OPEN` armed — so every
/// `opendir` of a watched directory produces one, and clauth's own config leg
/// does two per reconcile (`runtime::shared_runtime_dirs` reads the profile
/// store directory). Reading them as unaccountable crosses the orphan floor in
/// under two minutes on a healthy install, which trains the operator to ignore
/// the one line this whole signal exists to emit.
#[test]
fn an_event_about_the_watched_directory_itself_is_never_an_orphan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let specs = vec![WatchSpec::new(
        tmp.path(),
        Interest::Names(vec!["settings.json".into()]),
    )];

    let on_the_dir = notify::Event::new(notify::EventKind::Access(
        notify::event::AccessKind::Open(notify::event::AccessMode::Any),
    ))
    .add_path(tmp.path().to_path_buf());
    let verdict = classify(&specs, &on_the_dir);
    assert!(
        !verdict.matched,
        "the directory is not one of the names the filter takes"
    );
    assert!(
        verdict.orphan.is_none(),
        "an event whose path IS the watched directory came from our own watch"
    );

    // The backend leg, which is what would have caught this: inotify's OPEN is
    // the mask that makes a plain `read_dir` observable. Linux-only because the
    // event is inotify's, not because the rule is.
    #[cfg(target_os = "linux")]
    {
        let watcher = try_start(&specs, Duration::from_millis(20)).expect("watcher");
        let _ = std::fs::read_dir(tmp.path()).expect("read the watched directory");
        let deadline = Instant::now() + BOUND;
        while watcher.health.counts().seen == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let counts = watcher.health.counts();
        assert!(
            counts.seen > 0,
            "reading a watched directory produced no event at all, so this leg \
             proves nothing"
        );
        assert_eq!(
            counts.orphans, 0,
            "clauth's own reconcile reads this directory twice per pass: \
             counting that as unaccountable accuses every healthy session"
        );
    }
}

/// A rescan carries NO paths (`Event::new(EventKind::Other).set_flag(Rescan)`),
/// so `wants` never runs on it. Counting it as a match retires the detector for
/// the whole session on the first queue overflow — and heavy churn, which is
/// what overflows the queue, is exactly when a degraded watcher matters.
#[test]
fn a_rescan_wakes_the_loop_but_never_counts_as_a_match() {
    let specs = vec![WatchSpec::new(
        "/home/u/.claude",
        Interest::Names(vec!["settings.json".into()]),
    )];
    let rescan = notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);

    let verdict = classify(&specs, &rescan);

    assert!(
        verdict.wake,
        "a dropped-event overflow must still reconcile"
    );
    assert!(
        !verdict.matched,
        "a pathless event proves nothing about the filter, so it must not \
         exonerate one that matches nothing"
    );
    assert!(
        verdict.orphan.is_none(),
        "an event with no paths cannot accuse a spelling either"
    );
}

/// One spec whose spelling is wrong while its siblings match is the shape a
/// dotfiles-managed `~/.claude` takes in production, and an aggregate match
/// count cannot see it: the healthy siblings keep `matched` above zero forever.
/// An event nobody can account for is the discriminator — notify forwards only
/// children of the key it armed.
#[test]
fn a_stream_of_unaccountable_events_is_reported_even_while_another_spec_matches() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    health.saw(&took());
    let delivered = Path::new("/private/var/u/.claude/settings.json");
    for _ in 0..DEAD_FILTER_MIN_EVENTS {
        health.saw(&orphaned(delivered));
    }
    let mut dead = DeadFilter::default();

    for interval in 1..DEAD_FILTER_INTERVALS {
        assert!(
            dead.fires(&health).is_none(),
            "interval {interval} reported before the horizon"
        );
    }
    let (kind, counts) = dead
        .fires(&health)
        .expect("a spec delivering events nobody can account for went unreported");
    assert_eq!(
        kind,
        DeadFilterKind::Unaccountable,
        "the arm decides the wording, and a 'matched nothing' headline is false \
         here: this fires WITH matches from the healthy siblings"
    );
    assert_eq!(counts.orphans, DEAD_FILTER_MIN_EVENTS);
    assert!(
        counts.matched > 0,
        "the fixture must keep a healthy sibling, or this passes for the \
         zero-match reason instead of the one it names"
    );
    assert!(
        health
            .orphan_hint()
            .contains("/private/var/u/.claude/settings.json"),
        "the diagnostic must name the spelling the backend DELIVERED, which is \
         the whole discriminator: {}",
        health.orphan_hint()
    );
}

/// Both spellings, because the difference between them IS the bug this detects.
/// A line naming only the directory clauth armed tells the operator nothing
/// they did not already configure.
#[test]
fn the_watched_list_names_both_spellings_when_they_differ() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    std::fs::create_dir_all(&real).expect("mkdir real");
    // Resolved, or the "resolves to itself" half is untrue wherever TMPDIR is
    // itself symlinked: the `cargo.sh` test leg and every macOS run.
    let real = std::fs::canonicalize(&real).expect("realpath");

    // Gated for want of `std::os::unix::fs::symlink`, not because the rule is
    // unix-only. Ungated it is E0433 on windows, and a test module that cannot
    // compile takes every sibling test in this file down with it.
    #[cfg(unix)]
    {
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let linked = FilterHealth::new(&[WatchSpec::new(&link, Interest::AnyChild)]);
        assert!(
            linked.dirs.contains(&real.display().to_string())
                && linked.dirs.contains(&link.display().to_string()),
            "a spec armed through a symlink must name what it watches AND what \
             the backend will deliver: {}",
            linked.dirs
        );
    }

    let plain = FilterHealth::new(&[WatchSpec::new(&real, Interest::AnyChild)]);
    assert_eq!(
        plain.dirs,
        real.display().to_string(),
        "a directory that resolves to itself must not be printed twice"
    );
}

/// A fully-armed watcher that matches nothing degrades exactly as silently as
/// an unarmed one used to, so it must say so — ONCE. The condition holds for
/// the rest of the session by construction, so a line per interval would be a
/// heartbeat in the log rather than a defect report.
#[test]
fn a_watcher_that_matches_nothing_reports_once_and_not_per_interval() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    for _ in 0..DEAD_FILTER_MIN_EVENTS {
        health.saw(&refused());
    }
    let mut dead = DeadFilter::default();

    for interval in 1..DEAD_FILTER_INTERVALS {
        assert!(
            dead.fires(&health).is_none(),
            "fallback interval {interval} accused a filter before the horizon"
        );
    }
    let (kind, counts) = dead
        .fires(&health)
        .expect("a stream of events with nothing matching went unreported");
    assert_eq!(
        kind,
        DeadFilterKind::NothingMatched,
        "every spelling here is accounted for, so this is the interest lists"
    );
    assert_eq!(
        (counts.seen, counts.matched),
        (DEAD_FILTER_MIN_EVENTS, 0),
        "the report must carry the counters the decision was made on"
    );
    for repeat in 1..=3 {
        assert!(
            dead.fires(&health).is_none(),
            "the diagnostic repeated on interval {repeat} after the horizon"
        );
    }
}

/// One match proves the filter works, whatever volume of events it refuses
/// after that: `$HOME` is watched for a single name and rewritten by
/// everything, so refusals are the steady state rather than a symptom.
#[test]
fn one_matched_event_clears_the_suspicion_for_good() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    health.saw(&took());
    for _ in 0..DEAD_FILTER_MIN_EVENTS * 4 {
        health.saw(&refused());
    }
    let mut dead = DeadFilter::default();

    for interval in 1..=DEAD_FILTER_INTERVALS * 4 {
        assert!(
            dead.fires(&health).is_none(),
            "interval {interval} accused a filter that had already matched"
        );
    }
}

/// A quiet disk is not a dead filter. Without the volume floor, a session that
/// saw a couple of stray events and nothing else would be reported as broken.
#[test]
fn a_quiet_watcher_is_never_accused() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    for _ in 0..DEAD_FILTER_MIN_EVENTS - 1 {
        health.saw(&refused());
    }
    let mut dead = DeadFilter::default();

    for interval in 1..=DEAD_FILTER_INTERVALS * 4 {
        assert!(
            dead.fires(&health).is_none(),
            "interval {interval} accused a session that saw almost no events"
        );
    }
}

/// Short enough that the three intervals the detector waits out cost the suite
/// nothing, long enough to read as a duration in the line it prints.
const TICK: Duration = Duration::from_millis(10);

/// Run `run_events` over `health` until `ticks` fallback reconciles have
/// returned, and hand back every line the loop raised on its OWN thread.
///
/// The wait is on the reconciles rather than on the buffer filling, because it
/// is counting TICKS, not lines: a silent loop is one of the outcomes under
/// test, and polling a buffer that stays empty cannot tell it from a loop that
/// has not run yet. The lines are read after the scope joins, so nothing here
/// rests on where the detector sits inside one iteration.
///
/// Every caller runs PAST the horizon rather than up to it. A tick budget of
/// exactly `DEAD_FILTER_INTERVALS` is derived from the constant under test, so
/// it tracks that constant wherever it moves and can never observe it moving;
/// spare ticks are also what makes a per-interval repeat visible as extra lines.
fn drive_fallback_ticks(health: &FilterHealth, ticks: u32) -> Vec<String> {
    let t = Timings {
        debounce: Duration::from_millis(20),
        cooldown: Duration::ZERO,
        fallback: TICK,
        config_poll: NEVER,
        credential_poll: NEVER,
        swap_poll: NEVER,
    };
    // Held for the whole run: dropping the sender disconnects `wake`, and the
    // loop then exits `WatcherLost` before its first fallback tick.
    let (_wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (done_tx, done_rx) = unbounded();
    let rec = Recorder::new(Duration::ZERO, done_tx);
    let lines = crate::logline::LogLines::new();
    let sink = lines.clone();

    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _capture = sink.capture_here();
            run_events(&wake_rx, &shutdown_rx, &t, &rec, Some(health))
        });
        for tick in 1..=ticks {
            done_rx
                .recv_timeout(BOUND)
                .unwrap_or_else(|_| panic!("fallback reconcile {tick} never returned"));
        }
        drop(shutdown_tx);
    });

    lines.snapshot()
}

/// The counters and the wording are pinned above without a loop; what nothing
/// said is that the fallback arm ever reaches the `logline!`, or which of the
/// two lines it raises. A detector nobody hears is the defect it exists to
/// report, one layer up.
#[test]
fn the_loop_raises_the_unaccountable_line_once() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    health.saw(&took());
    let delivered = Path::new("/private/var/u/.claude/settings.json");
    for _ in 0..DEAD_FILTER_MIN_EVENTS {
        health.saw(&orphaned(delivered));
    }

    let lines = drive_fallback_ticks(&health, DEAD_FILTER_INTERVALS + 2);

    assert_eq!(
        lines,
        vec![
            "clauth: fs watcher is being handed events under a directory it cannot \
             account for (16 of 17 seen, e.g. /private/var/u/.claude/settings.json), \
             so that surface reconciles on the 10ms fallback rather than on its own \
             events. Watched: /home/u/.claude"
                .to_string()
        ],
        "the operator's whole diagnosis is this line: the counters, the spelling \
         the backend delivered, and what the surface now costs"
    );
}

/// The other arm through the same loop. Both are reachable from one `match`, so
/// a swapped pair formats perfectly and accuses the wrong subsystem.
#[test]
fn the_loop_raises_the_nothing_matched_line_once() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    for _ in 0..DEAD_FILTER_MIN_EVENTS {
        health.saw(&refused());
    }

    let lines = drive_fallback_ticks(&health, DEAD_FILTER_INTERVALS + 2);

    assert_eq!(
        lines,
        vec![
            "clauth: fs watcher armed every directory but matched none of its 16 \
             events, so every change reconciles on the 10ms fallback rather than on \
             its own event. Watched: /home/u/.claude"
                .to_string()
        ],
        "every spelling here is accounted for, so the line must name the interest \
         lists rather than the directories"
    );
}

/// The silent arm, over the same harness the two above are shown to fill: a
/// healthy watcher's log stays a log. Four times the horizon, each tick waited
/// out rather than slept through, so an empty buffer is a loop that ran and
/// said nothing.
#[test]
fn the_loop_says_nothing_about_a_watcher_that_matched() {
    let health = FilterHealth::new(&[WatchSpec::new("/home/u/.claude", Interest::AnyChild)]);
    health.saw(&took());
    for _ in 0..DEAD_FILTER_MIN_EVENTS * 4 {
        health.saw(&refused());
    }

    let lines = drive_fallback_ticks(&health, DEAD_FILTER_INTERVALS * 4);

    assert!(
        lines.is_empty(),
        "a working watcher was reported as broken: {lines:?}"
    );
}

/// `watch_specs` covers every file the reconcile reads, and covers them by
/// their PARENT so no entry is armed on an inode a rename will unlink.
#[test]
fn watch_specs_cover_each_reconciled_file_through_its_directory() {
    let runtime = Path::new("/clauth/profiles/acct/runtime-1-0");
    let store = Path::new("/clauth/profiles/acct/credentials.json");
    let claude_home = Path::new("/home/u/.claude");
    let specs = watch_specs(runtime, store, claude_home);

    for path in [
        runtime.join(".credentials.json"),
        runtime.join("settings.json"),
        store.to_path_buf(),
        Path::new("/home/u/.claude.json").to_path_buf(),
        claude_home.join("settings.json"),
        // The fake-mode mirror's own surface, which no file watch ever covered.
        claude_home.join("statusline.sh"),
        runtime.join("CLAUDE.md"),
    ] {
        assert!(wants(&specs, &path), "{} is unwatched", path.display());
    }
    assert!(
        !wants(&specs, Path::new("/home/u/.bash_history")),
        "watching $HOME for .claude.json must not wake on the rest of it"
    );
    assert!(
        !wants(
            &specs,
            &Path::new("/clauth/profiles/acct").join("kick_block.json")
        ),
        "the profile store directory holds caches a scheduler rewrites on its own cadence"
    );
}
