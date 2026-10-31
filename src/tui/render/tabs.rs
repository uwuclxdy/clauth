//! Tab bar — the primary navigation surface. Lowercase labels; the active tab
//! is underlined in sapphire, the rest sit dim. Switching is ← →.
//! An inactive tab with a pending background event takes that event's semantic
//! color until the user visits it (clears in `switch_tab`).
//!
//! # Overflow
//!
//! When `area.width` is too narrow to fit all tab labels (plus 3-space separators),
//! the bar degrades to the overflow form:
//!
//! ```text
//!  ‹   active   ›
//! ```
//!
//! `‹` / `›` are rendered in `TEXT_FAINT` (2 spaces padding each side) and
//! indicate more tabs exist to the left / right. A marker is omitted when the
//! active tab is already at that edge. The active label is always underlined in
//! place (`ACCENT + BOLD + UNDERLINED`).
//!
//! # Dynamic label truncation
//!
//! `truncate_label` is available for dynamic tabs that carry a progress suffix
//! (`(N%)` / `(✓)`) — the suffix is preserved and only the name body is trimmed.
//! The current static tabs always fit so this path is defensive / future-proof.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Tab, ToastKind};
use super::super::theme;

/// Separator between adjacent tabs in normal form.
const SEP: &str = "   ";
const SEP_WIDTH: usize = 3;

/// Chevron affordances (2 spaces padding each side) used in overflow form,
/// matching the visual contract: `‹   active   ›`.
const PREV_MARK: &str = "‹  ";
const NEXT_MARK: &str = "  ›";

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let spans = spans(app, area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme::base()), area);
}

/// The bar's spans in `avail` columns: every label while they fit, else the
/// `‹  active  ›` overflow form. Shared with the header's no-art row, where
/// the tabs ride the brand row.
pub(super) fn spans(app: &App, avail: usize) -> Vec<Span<'static>> {
    if fits_normal(avail) {
        build_normal(app, avail)
    } else {
        build_overflow(app)
    }
}

/// Total width of the tab strip in normal form (all labels + separators),
/// before any truncation. Exposed for the header's gauge-placement logic.
pub(super) fn full_strip_width() -> usize {
    Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let sep = if i > 0 { SEP_WIDTH } else { 0 };
            sep + t.title().len()
        })
        .sum()
}

/// The widest the overflow form renders over every tab: a label plus the edge
/// markers that tab's position draws. The header's no-art row reserves this,
/// so a chip never depends on which tab is active; the bar itself still draws
/// at the active tab's own width inside that slot.
pub(super) fn overflow_width_max() -> usize {
    Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let prev = if i > 0 { PREV_MARK.chars().count() } else { 0 };
            let next = if i + 1 < Tab::ALL.len() {
                NEXT_MARK.chars().count()
            } else {
                0
            };
            prev + t.title().chars().count() + next
        })
        .max()
        .unwrap_or(0)
}

/// Whether all tabs fit in `avail` columns without truncation.
fn fits_normal(avail: usize) -> bool {
    full_strip_width() <= avail
}

/// Build spans for the normal (all-tabs-visible) form.
/// Called only when `fits_normal` has already confirmed all labels fit — no truncation needed.
fn build_normal(app: &App, _avail: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, theme::dim()));
        }
        let label = tab.title().to_lowercase();
        if *tab == app.tab {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(theme::accent_color())
                    .bold()
                    .underlined(),
            ));
        } else {
            let color = activity_color(app.tab_activity[tab.index()]);
            spans.push(Span::styled(label, Style::default().fg(color)));
        }
    }
    spans
}

/// Build spans for the overflow form: `‹  active  ›` (chevrons in TEXT_FAINT).
/// Edge markers are omitted when there is no tab in that direction.
fn build_overflow(app: &App) -> Vec<Span<'static>> {
    let active_idx = app.tab.index();
    let last_idx = Tab::ALL.len().saturating_sub(1);

    let mut spans: Vec<Span<'static>> = Vec::new();

    if active_idx > 0 {
        spans.push(Span::styled(PREV_MARK, theme::faint()));
    }

    let label = app.tab.title().to_lowercase();
    spans.push(Span::styled(
        label,
        Style::default()
            .fg(theme::accent_color())
            .bold()
            .underlined(),
    ));

    if active_idx < last_idx {
        spans.push(Span::styled(NEXT_MARK, theme::faint()));
    }

    spans
}

/// Map a background activity kind to its semantic label color. `None` → TEXT_DIM
/// (the standard inactive tab color).
fn activity_color(activity: Option<ToastKind>) -> Color {
    match activity {
        None => theme::text_dim_color(),
        Some(ToastKind::Success) => theme::success_color(),
        Some(ToastKind::Danger) => theme::danger_color(),
        Some(ToastKind::Warning) => theme::warning_color(),
        Some(ToastKind::Info) => theme::text_color(),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_tabs.rs"]
mod tab_overflow_tests;
