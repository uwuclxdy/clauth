//! TUI runtime. `run` is the only public surface; everything below is glue
//! between ratatui, the `App` state machine, and shutdown housekeeping.

mod app;
mod clawd;
mod render;
pub(crate) mod theme;

pub(crate) use app::escape_control;
pub(crate) use clawd::full_sprite;
// A scheduler test draws a stood-down TUI's frame over the stores its tick
// wrote; test-only, so the shipped binary's reach stays as it was.
#[cfg(test)]
pub(crate) use app::App;
#[cfg(test)]
pub(crate) use render::draw;

use std::io::IsTerminal as _;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::profile::AppConfig;

/// 80ms tick: spinner advances every frame per contract; responsive without
/// burning CPU.
const TICK: Duration = Duration::from_millis(80);

/// How long a stop signal gives the frame loop to return before the watcher
/// ends the TUI itself. The loop looks for a stop once per frame, so only a
/// loop held inside one frame outlasts it: a slow key handler, an event read
/// on a terminal that hung up, which crossterm 0.29's unix reader retries
/// forever (measured 2026-11-10 on a closed pty: the read never returned), or
/// a draw's write to a terminal that stopped reading (measured 2026-11-11 on
/// an undrained pty: the write never returned while it stayed undrained).
#[cfg(unix)]
const STOP_GRACE: Duration = Duration::from_secs(1);

/// How long the terminal hand-back may take before the TUI exits without it,
/// on a stop signal's exit or after a panic: a stop signal's sender (a closed
/// pane, a supervisor) wants the process gone.
const GIVE_BACK_BOUND: Duration = Duration::from_millis(500);

/// How long [`run`]'s terminal hand-back may take on an exit no stop signal
/// decided: a user's quit, or an error. The user is at the terminal it hands
/// back, so it waits longer than [`GIVE_BACK_BOUND`] for one that stopped
/// taking output (a stalled ssh link, a suspended terminal window), and one
/// that resumes in time gets a clean shell. A quit over one that never resumes
/// ends the TUI at this bound with the alternate screen still up and the
/// cursor hidden; a stop signal arriving meanwhile does not shorten the wait,
/// as the watcher stood down when the frame loop returned.
const QUIT_GIVE_BACK_BOUND: Duration = Duration::from_secs(10);

/// A stop signal's ask to end the TUI. On unix a `clauth-tui-sig` thread takes
/// SIGHUP (a terminal or pane close) and SIGTERM, records the first one here
/// for the frame loop, and runs the shutdown and the exit itself if the loop
/// does not return within its one-second stop grace (`STOP_GRACE`). Windows
/// has no signal handling on this path, as `login_interrupt` has none: a
/// console close ends the TUI with no shutdown at all, so the records lose up
/// to the 60 s Windows cadence, a token spend in flight can lose its minted
/// pair, and the credential snapshot and detach never run.
struct StopWatch {
    signal: Arc<OnceLock<i32>>,
    /// Dropped once the frame loop returned: a watcher still inside its grace
    /// then leaves the shutdown and the exit to the frame loop's thread.
    #[cfg(unix)]
    frames_running: Option<std::sync::mpsc::Sender<()>>,
}

/// Take SIGHUP and SIGTERM from their default action. Both are taken even
/// when inherited as ignored (`nohup`): a TUI whose terminal closed can do
/// nothing, so it ends rather than outliving its terminal.
#[cfg(unix)]
fn stop_signals() -> Result<signal_hook::iterator::Signals> {
    use signal_hook::consts::signal::{SIGHUP, SIGTERM};
    signal_hook::iterator::Signals::new([SIGHUP, SIGTERM])
        .context("failed to install signal handlers")
}

impl StopWatch {
    /// A watch with nothing recorded, beside the record its watcher writes and
    /// the receiver that learns when the frame loop returned.
    #[cfg(unix)]
    fn new() -> (Self, Arc<OnceLock<i32>>, std::sync::mpsc::Receiver<()>) {
        let signal = Arc::new(OnceLock::new());
        let (frames_running, frames_left) = std::sync::mpsc::channel();
        let watch = Self {
            signal: Arc::clone(&signal),
            frames_running: Some(frames_running),
        };
        (watch, signal, frames_left)
    }

    #[cfg(unix)]
    fn start(
        mut signals: signal_hook::iterator::Signals,
        shutdown: Arc<app::Shutdown>,
    ) -> Result<Self> {
        let (watch, recorded, frames_left) = Self::new();
        std::thread::Builder::new()
            .name("clauth-tui-sig".into())
            .spawn(move || {
                watch_stop_signals(
                    signals.forever(),
                    &recorded,
                    &frames_left,
                    STOP_GRACE,
                    |signal| stop_fallback(signal, &shutdown),
                );
            })
            .context("failed to spawn the stop-signal watcher")?;
        Ok(watch)
    }

    #[cfg(not(unix))]
    fn start() -> Result<Self> {
        Ok(Self {
            signal: Arc::default(),
        })
    }

    fn requested(&self) -> bool {
        self.signal.get().is_some()
    }

    fn frames_left(&mut self) {
        #[cfg(unix)]
        drop(self.frames_running.take());
    }

    fn signal(&self) -> Option<i32> {
        self.signal.get().copied()
    }
}

/// The watcher thread's body over the stop signals as they arrive: record the
/// first for the frame loop, end the TUI through `fallback` unless the loop
/// returns within `grace`, then drop every later signal, so none cuts short
/// the shutdown the frame loop's thread runs.
#[cfg(unix)]
fn watch_stop_signals(
    mut arrivals: impl Iterator<Item = i32>,
    recorded: &OnceLock<i32>,
    frames_left: &std::sync::mpsc::Receiver<()>,
    grace: Duration,
    fallback: impl FnOnce(i32),
) {
    let Some(first) = arrivals.next() else {
        return;
    };
    let _ = recorded.set(first);
    end_unless_frames_left(first, frames_left, grace, fallback);
    arrivals.for_each(drop);
}

/// The watcher's end of a TUI whose frame loop did not return in time: the
/// shared shutdown, the terminal handed back (a loop only slow keeps a live
/// terminal), then the exit as `128 + signal`. A panic in the shutdown is
/// logged and the exit still comes: signal-hook-registry 1.4.8 leaves its
/// handler installed when a dead watcher's `Signals` unregisters, so without
/// this exit the TUI would ignore every later SIGHUP and SIGTERM.
#[cfg(unix)]
fn stop_fallback(signal: i32, shutdown: &app::Shutdown) {
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shutdown.run()));
    if ran.is_err() {
        crate::logline::logline!(
            "clauth: the TUI's shutdown on signal {signal} panicked; exiting without the rest of it"
        );
    }
    let _ = give_back_terminal_within(GIVE_BACK_BOUND);
    exit_process(128 + signal);
}

/// [`std::process::exit`], which a test captures on its own thread instead.
#[cfg(unix)]
fn exit_process(code: i32) {
    #[cfg(test)]
    if EXIT_CODES
        .with(|codes| codes.borrow_mut().as_mut().map(|codes| codes.push(code)))
        .is_some()
    {
        return;
    }
    std::process::exit(code);
}

#[cfg(all(unix, test))]
thread_local! {
    /// The codes [`exit_process`] took on this thread while a test holds
    /// `Some`.
    static EXIT_CODES: std::cell::RefCell<Option<Vec<i32>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `fallback` with `signal` unless the frame loop returned (dropped its
/// sender) within `grace`: a loop that returns in time runs the shutdown and
/// the exit on its own thread.
#[cfg(unix)]
fn end_unless_frames_left(
    signal: i32,
    frames_left: &std::sync::mpsc::Receiver<()>,
    grace: Duration,
    fallback: impl FnOnce(i32),
) {
    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = frames_left.recv_timeout(grace) {
        fallback(signal);
    }
}

/// Launch the full-screen TUI. Returns on quit (`q` twice, `⌃c` twice, or a
/// fatal error); on unix a SIGHUP or SIGTERM ends it through the same
/// shutdown and exits `128 + signal`, also on a terminal already closed.
/// `herdr_mode` and `full_sprite` are decided once by `cmd_tui` from
/// `HERDR_ENV` and `TERM_PROGRAM` and applied to the constructed [`app::App`]
/// via [`app::App::with_herdr_mode`] / [`app::App::with_full_sprite`]; nothing
/// else here reads the environment.
pub(crate) fn run(config: AppConfig, herdr_mode: bool, full_sprite: bool) -> Result<()> {
    if std::io::stderr().is_terminal() {
        crate::logline::divert_to_logfile();
    }
    let outcome = open_terminal()
        .context("Failed to initialize the terminal")
        .and_then(|mut terminal| {
            let outcome = run_loop(&mut terminal, config, herdr_mode, full_sprite);
            // A stop signal's exit keeps the short bound: a loop that returned
            // within the grace stood the watcher down, so nothing else ends
            // that exit over a terminal that stopped taking output.
            let bound = if matches!(&outcome, Err(e) if e.is::<crate::Interrupted>()) {
                GIVE_BACK_BOUND
            } else {
                QUIT_GIVE_BACK_BOUND
            };
            match give_back_terminal_within(bound) {
                Some(Ok(())) => {}
                Some(Err(err)) => {
                    crate::out::errln_or_drop!("Failed to restore terminal: {err}");
                }
                // The terminal keeps the alternate screen and a hidden cursor:
                // leave a trace in the log, since the terminal cannot take one.
                None => crate::logline::logline!(
                    "clauth: the terminal hand-back did not finish within {bound:?}"
                ),
            }
            outcome
        });
    // The TUI's error line goes out here, dropped when stderr cannot take it:
    // `main`'s `exit_code` prints through `out::write_chunk`, which panics on
    // a failed write, and the TUI's stderr is a terminal that may have closed.
    outcome.map_err(|e| {
        if e.is::<crate::Interrupted>() {
            return e;
        }
        crate::out::errln_or_drop!("Error: {e:?}");
        anyhow::Error::new(crate::FailureReported)
    })
}

/// Raw mode, the alternate screen and a terminal over stdout, as
/// `ratatui::try_init` sets them up, under a panic hook that hands the
/// terminal back through [`give_back_terminal_within`] before the previous
/// hook prints. Not `try_init` itself: its hook restores through
/// `ratatui::restore`, whose `eprintln!` panics on a closed terminal, and a
/// panic inside the panic hook aborts (measured 2026-11-10: a core dump from a
/// TUI whose output terminal closed). A step that fails after raw mode is on
/// hands the terminal back before the error returns. The terminal is never
/// dropped: `Terminal`'s drop shows the cursor and `eprintln!`s a failure the
/// same way, and the process exits right after [`run`] returns.
fn open_terminal() -> std::io::Result<std::mem::ManuallyDrop<DefaultTerminal>> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = give_back_terminal_within(GIVE_BACK_BOUND);
        previous(info);
    }));
    ratatui::crossterm::terminal::enable_raw_mode()?;
    let opened = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    )
    .and_then(|()| {
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
    });
    if opened.is_err() {
        let _ = give_back_terminal();
    }
    opened.map(std::mem::ManuallyDrop::new)
}

/// Hand the terminal back: raw mode off, the alternate screen left, the cursor
/// shown. Prints nothing, so on a closed terminal it only returns the error.
fn give_back_terminal() -> std::io::Result<()> {
    let restored = ratatui::try_restore();
    let shown = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::cursor::Show);
    restored.and(shown)
}

/// [`give_back_terminal`] on a `clauth-tui-restore` thread, waited for at most
/// `bound`: `Some` with its result once it finished, `None` when it had not
/// finished by the bound or its helper died before sending. A terminal that
/// stopped reading holds the frame loop's write, and stdout's lock with it, so
/// the hand-back's own write would wait as long; a hand-back still waiting at
/// the bound is left behind, and the exit after it does not wait on stdout
/// (std's exit-time stdout cleanup only `try_lock`s it). With no thread left
/// (a thread limit, which is also what makes the TUI panic) it hands back
/// inline, unbounded, rather than not at all.
#[must_use = "dropping it leaves a failed or unfinished hand-back unreported"]
fn give_back_terminal_within(bound: Duration) -> Option<std::io::Result<()>> {
    let (given_back, done) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("clauth-tui-restore".into())
        .spawn(move || {
            let _ = given_back.send(give_back_terminal());
        });
    match spawned {
        Ok(_) => done.recv_timeout(bound).ok(),
        // No thread left: hand back inline, as the gateway's probe does.
        Err(_) => Some(give_back_terminal()),
    }
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    config: AppConfig,
    herdr_mode: bool,
    full_sprite: bool,
) -> Result<()> {
    // The stop signals are taken before the first frame, so before the
    // scheduler's tick thread can start: from the moment unsaved records
    // exist, a stop signal ends the TUI through its exit save, never the
    // default action.
    #[cfg(unix)]
    let signals = stop_signals()?;
    let mut application = app::App::new(config)
        .with_herdr_mode(herdr_mode)
        .with_full_sprite(full_sprite);
    #[cfg(unix)]
    let watch = StopWatch::start(signals, Arc::clone(&application.shutdown));
    #[cfg(not(unix))]
    let watch = StopWatch::start();
    let watch = match watch {
        Ok(watch) => watch,
        Err(e) => {
            app::shutdown(&mut application)?;
            return Err(e);
        }
    };
    // Non-blocking reconcile: fast path runs inline; verdict sequenced via
    // `StartupSignal`. Bootstrap is spawned from `on_tick` once reconcile
    // settles — neither blocks the first paint.
    app::reconcile_startup(&mut application);

    // Shutdown runs on every exit path, a draw or event error included: it
    // touches no terminal, so the tick thread's exit save still lands when
    // the frame loop dies of one.
    let shutdown = Arc::clone(&application.shutdown);
    frames_then_exit(
        watch,
        |stop_requested| run_frames(terminal, &mut application, stop_requested),
        || {
            shutdown.run();
            Ok(())
        },
    )
}

/// The frame loop under `watch`, then the shutdown, then the exit. The
/// watcher learns the loop returned before the shutdown starts, so a watcher
/// still inside its grace leaves the shutdown and the exit to this thread; one
/// already past it runs them too, and the one run-once shutdown and the one
/// recorded signal give both exits the same code. The stop signal is read
/// after the shutdown, so one that trails the frame error it caused still
/// decides the exit.
fn frames_then_exit(
    mut watch: StopWatch,
    frames: impl FnOnce(&mut dyn FnMut() -> bool) -> Result<()>,
    shutdown: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let frames = frames(&mut || watch.requested());
    watch.frames_left();
    let shutdown = shutdown();
    exit_outcome(frames, watch.signal(), shutdown)
}

/// The TUI's exit, mapped for [`frames_then_exit`] from the frame loop's
/// result, the stop signal recorded by the time the shutdown returned, and the
/// shutdown's own result. A stop signal decides the exit:
/// [`crate::Interrupted`], so `main` answers `128 + signal` with no `Error:`
/// line. A frame or shutdown error beside it is dropped: on a hangup it is the
/// hangup's own, and printing it would hit the closed terminal. With no stop
/// signal the frame loop's error comes first, then the shutdown's.
fn exit_outcome(frames: Result<()>, signal: Option<i32>, shutdown: Result<()>) -> Result<()> {
    if let Some(signal) = signal {
        return Err(crate::Interrupted(signal).into());
    }
    frames?;
    shutdown
}

fn run_frames<B>(
    terminal: &mut ratatui::Terminal<B>,
    application: &mut app::App,
    stop_requested: &mut dyn FnMut() -> bool,
) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: Send + Sync + 'static,
{
    let mut last_tick = Instant::now();

    while !application.quit {
        // A stop signal ends the loop before the next draw or event read, so
        // a loop that is not held inside a frame returns within one frame.
        if stop_requested() {
            return Ok(());
        }
        terminal.draw(|frame| render::draw(frame, application))?;
        let size = terminal.size()?;
        // Update compact + width-floor state each frame so the banner appears as
        // soon as the terminal shrinks below 14 rows or the width floor (and
        // clears when it grows back).
        application.update_compact(size.height, size.width);

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app::handle_key(application, key);
                }
                Event::Resize(_, _) => { /* redraw next iteration */ }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            app::on_tick(application);
            last_tick = Instant::now();
        }
    }
    Ok(())
}

// Fake-data TUI for README screenshots (test-only).
// Run: `cargo test showcase -- --ignored --nocapture`
#[cfg(test)]
#[path = "../../tests/inline/showcase.rs"]
mod showcase;

// Test-only: `HomeSandbox::drop` joins detached `spawn_worker` threads bounded
// before it clears `HOME_OVERRIDE`; a worker still running past the bound is
// detached, and its next home read resolves whichever `HOME_OVERRIDE` is
// installed then, never the operator's real `$HOME`.
#[cfg(test)]
pub(crate) use app::{detach_test_workers, join_test_workers_within};
