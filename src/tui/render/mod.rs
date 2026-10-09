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
mod modals;
mod overview;
mod panes;
pub(crate) use panes::{ListView, follow_scroll_offset};
pub(super) mod prose;
mod services;
mod status;
mod tabs;
mod toasts;
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

    // Content frame's top border doubles as the header/content separator; the
    // header is always 3 rows (brand / count+status / tabs+gauge).
    let [header_area, content_area, footer_area] = Layout::vertical([
        Constraint::Length(header::header_height(app)),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);

    header::draw(frame, header_area, app);

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
        Tab::Overview => overview::draw(frame, body_area, app),
        Tab::Usage => usage::draw(frame, body_area, app),
        Tab::Tokens => tokens::draw(frame, body_area, app),
        Tab::Setup => config::draw(frame, body_area, app),
        Tab::Fallback => chain::draw(frame, body_area, app),
        Tab::Config => global_config::draw(frame, body_area, app),
        Tab::Status => status::draw(frame, body_area, app),
        Tab::Services => services::draw(frame, body_area, app),
    }
    footer::draw(frame, footer_area, app);

    toasts::draw(frame, area, app);

    if let Some(modal) = app.modals.last() {
        modals::draw(frame, area, app, modal);
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_mod.rs"]
mod render_smoke;
