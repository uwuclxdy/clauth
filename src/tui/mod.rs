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

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::profile::AppConfig;

/// 80ms tick: spinner advances every frame per contract; responsive without burning CPU.
const TICK: Duration = Duration::from_millis(80);

/// Launch the full-screen TUI. Returns on quit (`q` twice, `⌃c` twice, or a
/// fatal error).
/// `herdr_mode` and `full_sprite` are decided once by `cmd_tui` from
/// `HERDR_ENV` and `TERM_PROGRAM` and applied to the constructed [`app::App`]
/// via [`app::App::with_herdr_mode`] / [`app::App::with_full_sprite`]; nothing
/// else here reads the environment.
pub(crate) fn run(config: AppConfig, herdr_mode: bool, full_sprite: bool) -> Result<()> {
    // `try_init` owns raw mode + alt screen and installs a restore panic hook,
    // so a panic mid-draw no longer leaves the terminal corrupted.
    let mut terminal = ratatui::try_init().context("Failed to initialize the terminal")?;
    let outcome = run_loop(&mut terminal, config, herdr_mode, full_sprite);
    ratatui::restore();
    outcome
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    config: AppConfig,
    herdr_mode: bool,
    full_sprite: bool,
) -> Result<()> {
    let mut application = app::App::new(config)
        .with_herdr_mode(herdr_mode)
        .with_full_sprite(full_sprite);
    // Non-blocking reconcile: fast path runs inline; verdict sequenced via
    // `StartupSignal`. Bootstrap is spawned from `on_tick` once reconcile
    // settles — neither blocks the first paint.
    app::reconcile_startup(&mut application);

    let mut last_tick = Instant::now();

    while !application.quit {
        terminal.draw(|frame| render::draw(frame, &application))?;
        let size = terminal.size()?;
        // Update compact + width-floor state each frame so the banner appears as
        // soon as the terminal shrinks below 14 rows or the width floor (and
        // clears when it grows back).
        application.update_compact(size.height, size.width);

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app::handle_key(&mut application, key);
                }
                Event::Resize(_, _) => { /* redraw next iteration */ }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            app::on_tick(&mut application);
            last_tick = Instant::now();
        }
    }

    app::shutdown(&mut application)
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
