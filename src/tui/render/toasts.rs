//! Transient toast stack — top-right, floating, no border.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::app::{App, Toast, ToastKind};
use super::super::theme;
use super::panes::wrap_words;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.toasts.is_empty() {
        return;
    }
    let toasts: Vec<&Toast> = app.toasts.iter().collect();

    // Full toast width (bar + padding + content + padding) caps at min(60, w − 4)
    // (the −4 is the 2-cell inset each side). col_width adds the
    // 3-cell chrome (`┃ ` + 1 right pad), so content = full − 3.
    let content_cap = 60_u16.min(area.width.saturating_sub(4)).saturating_sub(3);

    // Widest natural line across all toasts, clamped to cap — drives column width and wrap budget.
    let max_content_width = toasts
        .iter()
        .flat_map(|t| t.body.lines().map(|l| l.chars().count() as u16))
        .max()
        .unwrap_or(0)
        .min(content_cap);

    // +3: `┃ ` (bar cell + space) on the left, 1 trailing pad cell on the right.
    // The pad cell carries no text, so the Paragraph's `bg_sunken` base fills it.
    let col_width = max_content_width + 3;

    let x = area.x + area.width.saturating_sub(col_width + 2);
    let mut row = area.y + 2;

    for toast in &toasts {
        let color = match toast.kind {
            ToastKind::Info => theme::info_color(),
            ToastKind::Success => theme::success_color(),
            ToastKind::Warning => theme::warning_color(),
            ToastKind::Danger => theme::danger_color(),
        };
        let bar_style = Style::default().fg(color).bg(theme::bg_sunken());
        let title_style = Style::default()
            .fg(theme::text_color())
            .bg(theme::bg_sunken())
            .bold();
        let detail_style = Style::default()
            .fg(theme::text_dim_color())
            .bg(theme::bg_sunken());

        let mut lines_iter = toast.body.lines();
        let first = lines_iter.next().unwrap_or("");

        let mut render_lines: Vec<Line<'_>> = Vec::new();
        for wrapped in wrap_words(first, max_content_width as usize) {
            render_lines.push(Line::from(vec![
                Span::styled("┃ ", bar_style),
                Span::styled(wrapped, title_style),
            ]));
        }
        for detail in lines_iter {
            for wrapped in wrap_words(detail, max_content_width as usize) {
                render_lines.push(Line::from(vec![
                    Span::styled("┃ ", bar_style),
                    Span::styled(wrapped, detail_style),
                ]));
            }
        }
        if render_lines.is_empty() {
            render_lines.push(Line::from(vec![Span::styled("┃ ", bar_style)]));
        }

        let height = render_lines.len() as u16;
        let rect = Rect {
            x,
            y: row,
            width: col_width,
            height,
        };
        if fits_in(area, rect) {
            // Glass pane: capture the bg currently beneath each cell, render the
            // toast (which paints a solid `bg_sunken` base), then re-blend each
            // cell's bg as `bg_sunken` at 75 % over what was beneath it.
            // `blend_over` no-ops on the compatible tier → solid `bg_sunken`.
            let buf = frame.buffer_mut();
            let mut beneath: Vec<ratatui::style::Color> =
                Vec::with_capacity((rect.width as usize) * (rect.height as usize));
            for cy in rect.y..rect.y + rect.height {
                for cx in rect.x..rect.x + rect.width {
                    let bg = buf
                        .cell((cx, cy))
                        .and_then(|c| c.style().bg)
                        .unwrap_or(theme::bg_sunken());
                    beneath.push(bg);
                }
            }

            // Clear wipes underlying symbols to spaces (the captured `beneath`
            // bg above is unaffected); without it, Paragraph leaves stray glyphs
            // in the pad/short-wrap cells it never writes.
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(render_lines).style(Style::default().bg(theme::bg_sunken())),
                rect,
            );

            let buf = frame.buffer_mut();
            let mut i = 0;
            for cy in rect.y..rect.y + rect.height {
                for cx in rect.x..rect.x + rect.width {
                    let glass = theme::blend_over(beneath[i], theme::bg_sunken(), 0.75);
                    if let Some(cell) = buf.cell_mut((cx, cy)) {
                        cell.set_bg(glass);
                    }
                    i += 1;
                }
            }
        }
        row += height;
    }
}

fn fits_in(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_toasts.rs"]
mod tests;
