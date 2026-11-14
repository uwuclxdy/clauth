//! Transient toast stack — top-right, floating, no border.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::app::{App, Toast, ToastKind};
use super::super::theme;
use super::format::grapheme_cells;
use super::panes::spans_cells;
use super::prose;

/// A laid-out toast: its draw rect and its wrapped rows.
struct LaidOut {
    rect: Rect,
    kind: ToastKind,
    rows: Vec<Vec<Span<'static>>>,
}

/// The stack's draw rects, topmost first — the exact geometry [`draw`] paints.
pub(super) fn stack_rects(area: Rect, app: &App) -> Vec<Rect> {
    layout(area, app)
        .into_iter()
        .map(|laid| laid.rect)
        .collect()
}

/// Whether any rect in `rects` contains `pos`.
pub(super) fn covers(rects: &[Rect], pos: Position) -> bool {
    rects.iter().any(|rect| rect.contains(pos))
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    for laid in layout(area, app) {
        let color = match laid.kind {
            ToastKind::Info => theme::info_color(),
            ToastKind::Success => theme::success_color(),
            ToastKind::Warning => theme::warning_color(),
            ToastKind::Danger => theme::danger_color(),
        };
        let bar_style = Style::default().fg(color);

        let render_lines: Vec<Line<'_>> = laid
            .rows
            .into_iter()
            .map(|wrapped| {
                let mut spans = vec![Span::styled(format!("{} ", theme::BAR), bar_style)];
                spans.extend(wrapped);
                Line::from(spans)
            })
            .collect();

        let rect = laid.rect;
        // Glass pane: capture the bg beneath each cell, then paint the blend of
        // `bg_sunken` at 75 % over it onto every cell. The capture maps an
        // unpainted cell to `Reset`, which the compatible-tier blend counts as
        // `BG` (the full tier returns `over` flat for a `Reset` beneath).
        let buf = frame.buffer_mut();
        let mut beneath: Vec<Color> =
            Vec::with_capacity((rect.width as usize) * (rect.height as usize));
        for cy in rect.y..rect.y + rect.height {
            for cx in rect.x..rect.x + rect.width {
                let bg = buf
                    .cell((cx, cy))
                    .and_then(|c| c.style().bg)
                    .unwrap_or(Color::Reset);
                beneath.push(bg);
            }
        }

        // Clear wipes underlying symbols to spaces (the captured `beneath` bg
        // above is unaffected); without it, Paragraph leaves stray glyphs in
        // the pad/short-wrap cells it never writes.
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(render_lines), rect);

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
}

/// The laid-out stack: content-fit column, topmost first, each toast's rows
/// wrapped to the shared budget.
fn layout(area: Rect, app: &App) -> Vec<LaidOut> {
    if app.toasts.is_empty() {
        return Vec::new();
    }
    // Full toast width (bar + padding + content + padding) caps at min(60, w − 4)
    // (the −4 is the 2-cell inset each side). col_width adds the
    // 3-cell chrome (`┃ ` + 1 right pad), so content = full − 3.
    let content_cap = 60_u16.min(area.width.saturating_sub(4)).saturating_sub(3);

    // Widest natural line across all toasts, in cells, clamped to cap: the wrap
    // budget. Each grapheme's own width, summed — the measure the buffer lays
    // down; a whole-string width folds a lam-alef pair to one cell and
    // force-wraps a line that fits.
    let budget = app
        .toasts
        .iter()
        .flat_map(|t| t.body.lines().map(|l| grapheme_cells(&prose::plain(l))))
        .max()
        .unwrap_or(0)
        .min(usize::from(content_cap));

    let stack: Vec<(&Toast, Vec<Vec<Span<'static>>>)> = app
        .toasts
        .iter()
        .map(|toast| (toast, toast_rows(toast, budget)))
        .collect();

    // The column fits the widest row shown, so a line the height cap cut
    // reserves no width. Drawn cells, like the budget above.
    let content_width = stack
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|spans| spans_cells(spans)))
        .max()
        .unwrap_or(0)
        .min(usize::from(content_cap));
    // +3: `┃ ` (bar cell + space) on the left, 1 trailing pad cell on the right.
    let col_width = u16::try_from(content_width).unwrap_or(content_cap) + 3;

    let x = area.x + area.width.saturating_sub(col_width + 2);
    let mut row = area.y + 2;

    let mut laid = Vec::new();
    for (toast, rows) in stack {
        let height = rows.len() as u16;
        let rect = Rect {
            x,
            y: row,
            width: col_width,
            height,
        };
        if fits_in(area, rect) {
            laid.push(LaidOut {
                rect,
                kind: toast.kind,
                rows,
            });
        }
        row += height;
    }
    laid
}

/// A toast wraps to at most three rows.
const MAX_ROWS: usize = 3;

/// One toast's rows wrapped to `budget` cells: only displayed row 1 is bold
/// (the toast title), the head's continuation rows stay `TEXT` without bold,
/// and the detail rows dim; past [`MAX_ROWS`] the first rows stay and the last
/// kept one ends in `…`.
pub(crate) fn toast_rows(toast: &Toast, budget: usize) -> Vec<Vec<Span<'static>>> {
    let title = Style::default().fg(theme::text_color()).bold();
    let title_rest = Style::default().fg(theme::text_color());
    let detail = Style::default().fg(theme::text_dim_color());

    let mut lines = toast.body.lines();
    let first = lines.next().unwrap_or("");
    let mut rows: Vec<(Vec<Span<'static>>, Style)> = prose::wrap(first, budget, title)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let base = if i == 0 { title } else { title_rest };
            (row, base)
        })
        .collect();
    // The head wrapped with the bold title style; un-bold the continuation
    // rows' prose spans (a key keeps its own bold, a command its non-bold).
    for span in rows.iter_mut().skip(1).flat_map(|(row, _)| row) {
        if span.style.fg == Some(theme::text_color()) {
            span.style = span.style.remove_modifier(Modifier::BOLD);
        }
    }
    for detail_line in lines {
        rows.extend(
            prose::wrap(detail_line, budget, detail)
                .into_iter()
                .map(|row| (row, detail)),
        );
    }
    if rows.len() > MAX_ROWS {
        rows.truncate(MAX_ROWS);
        if let Some((spans, base)) = rows.last_mut() {
            end_in_ellipsis(spans, *base, budget);
        }
    }
    rows.into_iter().map(|(spans, _)| spans).collect()
}

/// Marks the last kept row of a cut toast: `…` after its text, or in place of
/// as many trailing graphemes as free a cell when the row fills `width`.
fn end_in_ellipsis(spans: &mut Vec<Span<'static>>, base: Style, width: usize) {
    // Drawn cells, like every measure that sizes the toast: a whole-string sum
    // folds a lam-alef pair to one cell and pops nothing, landing the `…` a
    // drawn cell past the budget.
    while spans_cells(spans) >= width
        && let Some(last) = spans.pop()
    {
        let mut graphemes: Vec<&str> = last
            .styled_graphemes(Style::default())
            .map(|grapheme| grapheme.symbol)
            .collect();
        graphemes.pop();
        if !graphemes.is_empty() {
            spans.push(Span::styled(graphemes.concat(), last.style));
        }
    }
    spans.push(Span::styled("…", base));
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
