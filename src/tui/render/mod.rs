//! Single top-level `draw` fn dispatches the whole frame.
//!
//! Visual language: one content frame per tab, no nested boxes; chrome is dim
//! lowercase; sapphire = active element; orange = logo + active-account marker.
//!
//! Where new things go:
//!   - new tab → submodule + dispatch in `draw` + entry in `tabs.rs`
//!   - new modal → `modals.rs`
//!   - shared formatters → `format.rs`; shared widgets → `panes.rs`

mod banner;
mod chain;
mod config;
mod footer;
mod format;
mod global_config;
mod header;
pub(crate) use header::art_fits;
mod modals;
mod overview;
mod panes;
pub(crate) use format::truncate;
pub(crate) use panes::{ListView, follow_scroll_offset};
pub(super) mod prose;
mod services;
mod status;
mod tabs;
pub(crate) mod toasts;
mod tokens;
mod usage;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Block;

use super::app::{App, Tab};
use super::theme;

pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let background = Block::default().style(theme::base());
    frame.render_widget(background, area);

    // The toast rects are laid out before the tab and modal draws so every
    // caret can avoid them; the toasts themselves draw last, over any open
    // modal.
    let toast_rects = toasts::stack_rects(area, app);

    // While a modal is open, body editors under it must not leave a caret
    // visible on the modal (the same stray-cursor class the toast check
    // closes); hand the tab draws a whole-area blocker then. The modal still
    // places its own caret, so it keeps the toast rects.
    let body_caret_rects = if app.modals.is_empty() {
        toast_rects.as_slice()
    } else {
        std::slice::from_ref(&area)
    };

    // One per-frame readings snapshot, taken before any render entry locks
    // `Config`: the stores rank below it, so a store lock under a config guard
    // would invert the order the debug build asserts on. Every entry that
    // renders a reading reads this snapshot alone.
    let readings = app.readings_snapshot();

    // Content frame's top border doubles as the header/content separator; with
    // the art the header is the mascot's own rows (brand / live+gauge+status /
    // any blank rows / tabs), without it 2 (brand+tabs / live+gauge+status).
    let [_, content_area, footer_area] = Layout::vertical([
        Constraint::Length(header::header_height(area)),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);

    header::draw(frame, area, app, &readings);

    // When a banner is active, carve one row off the top of the body area.
    let body_area = if let Some(b) = &app.banner {
        let [banner_area, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(content_area);
        banner::draw(frame, banner_area, b);
        rest
    } else {
        content_area
    };

    match app.tab {
        Tab::Overview => overview::draw(frame, body_area, app, &readings),
        Tab::Usage => usage::draw(frame, body_area, app, &readings, body_caret_rects),
        Tab::Tokens => tokens::draw(frame, body_area, app),
        Tab::Setup => config::draw(frame, body_area, app, &readings, body_caret_rects),
        Tab::Fallback => chain::draw(frame, body_area, app, &readings, body_caret_rects),
        Tab::Config => global_config::draw(frame, body_area, app, body_caret_rects),
        Tab::Status => status::draw(frame, body_area, app),
        Tab::Services => services::draw(frame, body_area, app, body_caret_rects),
    }
    footer::draw(frame, footer_area, app);

    if let Some(modal) = app.modals.last() {
        modals::draw(frame, area, app, modal, &toast_rects);
    }

    toasts::draw(frame, area, app);
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_mod.rs"]
mod render_smoke;
