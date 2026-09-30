//! Full-width body banner for critical sticky system conditions.
//!
//! Single line, full width, full-width semantic background tint, leading ` ! `
//! glyph in the semantic color. Stays until the condition resolves — not
//! user-dismissable.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{Banner, BannerSeverity};
use super::super::theme;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, banner: &Banner) {
    let (fg, bg) = match banner.severity {
        BannerSeverity::Danger => (theme::danger_color(), theme::bg_danger_color()),
        BannerSeverity::Warning => (theme::warning_color(), theme::bg_warning_color()),
    };

    let spans = vec![
        Span::styled(" ! ", Style::default().fg(fg).bg(bg)),
        Span::styled(banner.message.as_str(), Style::default().fg(fg).bg(bg)),
    ];

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        area,
    );
}
