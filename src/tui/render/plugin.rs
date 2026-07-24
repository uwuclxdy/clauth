//! Plugin tab — Claude Code integration health on the left, the selected row's
//! readout on the right. Master-detail, mirroring the Status tab's two-pane
//! machinery (counts as 2 of the 3-panel budget; no third panel).
//!
//! The left panel is one cursor-driven selector over the integration checks
//! (clauth on PATH, mcpServers wiring, plugin install, CC version, and a
//! `runtime` row that folds every profile's live sessions / credential link /
//! token freshness into one summary). Each row is a status dot + label, the
//! verdict in the detail pane. Enter descends into the detail pane; `f` applies
//! the selected row's fix (when one applies). All data is recomputed
//! synchronously on tab focus and `r` — there is no background thread, so the
//! title spinner only flickers while the cached `claude --version` is probed.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::super::app::{App, Health, PluginFocus};
use super::super::theme;
use super::format::spinner_frame;
use super::panes::{draw_scrollbar, empty_state, master_detail, section_box};
use crate::format::truncate;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // `master_detail` owns the selector|detail split: desktop uses upstream's
    // house contract (`selector_width` + `Min(20)`), narrow terminals stack the
    // panes instead of shrinking the detail to ~13 cells. Delegating keeps the
    // 0.12.0 layout intent while preserving the fork's phone adaptation.
    let (selector, detail) = master_detail(area, app.plugin.row_count());

    draw_selector(frame, selector, app);
    draw_detail(frame, detail, app);
}

// ── Left panel: checks + profiles selector ──────────────────────────────────────

fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.plugin.focus == PluginFocus::List;
    let block = list_block(app, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.plugin.row_count() == 0 {
        let widget = if app.plugin.error.is_some() {
            empty_state("check failed", "r", "to retry")
        } else {
            empty_state("no checks yet", "r", "to run")
        };
        frame.render_widget(widget, inner);
        return;
    }

    let content_w = inner.width as usize;
    let mut rows: Vec<Line<'static>> = Vec::new();
    // Display-line index of the cursor (the separator row shifts profile rows).
    let mut cursor_line = 0usize;

    for (idx, check) in app.plugin.checks.iter().enumerate() {
        if idx == app.plugin.cursor {
            cursor_line = rows.len();
        }
        rows.push(selector_row(
            check.health,
            check.label,
            theme::body(),
            // Checks are dot-only in the list; the dot color carries the verdict
            // and the full readout lives in the detail pane.
            "",
            0,
            idx == app.plugin.cursor,
            focused,
            content_w,
            check.fix.is_some(),
        ));
    }

    let viewport = inner.height as usize;
    let start = window_start(cursor_line, viewport, rows.len());
    let shown = rows.len().saturating_sub(start).min(viewport.max(1));
    let window: Vec<Line<'static>> = rows.iter().skip(start).take(shown).cloned().collect();

    frame.render_widget(Paragraph::new(window).style(theme::base()), inner);
    draw_scrollbar(frame, inner, rows.len(), start, viewport);
}

/// Keep `focus` near the center of a `viewport`-tall window over `total` rows.
fn window_start(focus: usize, viewport: usize, total: usize) -> usize {
    if total <= viewport || viewport == 0 {
        return 0;
    }
    let half = viewport / 2;
    if focus < half {
        0
    } else {
        focus.saturating_sub(half).min(total - viewport)
    }
}

/// One selector row: `❯ ● label   value`. Checks render dot + label only
/// (`value` empty); profile rows pad the label to `label_pad` so their values
/// line up in a column. The hover tint spans the full content width when
/// selected (the ratatui filler-tint gotcha); the caret shows only in the
/// focused pane.
#[allow(clippy::too_many_arguments)]
fn selector_row(
    health: Health,
    label: &str,
    label_style: Style,
    value: &str,
    label_pad: usize,
    selected: bool,
    focused: bool,
    content_w: usize,
    has_fix: bool,
) -> Line<'static> {
    let tint = selected.then(theme::bg_hover);
    let with_bg = |style: Style| match tint {
        Some(color) => style.bg(color),
        None => style,
    };

    let caret = if selected && focused {
        Span::styled(
            "❯ ",
            with_bg(
                Style::default()
                    .fg(theme::accent_color())
                    .add_modifier(Modifier::BOLD),
            ),
        )
    } else {
        Span::styled("  ", with_bg(Style::default()))
    };
    let dot = Span::styled("● ", with_bg(Style::default().fg(health_color(health))));
    let label_style = if selected && focused {
        with_bg(label_style.add_modifier(Modifier::BOLD))
    } else {
        with_bg(label_style)
    };

    let label_len = label.chars().count();
    // Pad short labels to the group's widest so the values share a left edge.
    let align = label_pad.saturating_sub(label_len);
    // 2 (caret) + 2 (dot + space) + label + alignment pad, then the value trails
    // a 2-space gap.
    let head_w = 4 + label_len + align;
    let mut spans = vec![caret, dot, Span::styled(label.to_string(), label_style)];
    if align > 0 {
        spans.push(Span::styled(" ".repeat(align), with_bg(Style::default())));
    }

    // Reserve room at the right edge for a fix marker (`[f]`) when the row has one.
    let marker_reserve = if has_fix { 4 } else { 0 };
    let value_room = content_w.saturating_sub(head_w + 2 + marker_reserve);
    if value_room > 0 && !value.is_empty() {
        spans.push(Span::styled("  ".to_string(), with_bg(Style::default())));
        spans.push(Span::styled(
            truncate(value, value_room),
            with_bg(theme::dim()),
        ));
    }
    if has_fix {
        // Right-aligned `[f]` cue so a fixable row is visible without descending.
        pad_to(&mut spans, content_w.saturating_sub(3), tint);
        spans.push(Span::styled(
            "[f]".to_string(),
            with_bg(theme::accent().add_modifier(Modifier::BOLD)),
        ));
    } else {
        pad_to(&mut spans, content_w, tint);
    }
    Line::from(spans)
}

/// Pad a span list with tinted filler so the hover tint spans the full width.
fn pad_to(spans: &mut Vec<Span<'static>>, content_w: usize, tint: Option<ratatui::style::Color>) {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = content_w.saturating_sub(used);
    if pad > 0 {
        let style = match tint {
            Some(color) => Style::default().bg(color),
            None => Style::default(),
        };
        spans.push(Span::styled(" ".repeat(pad), style));
    }
}

/// The selector panel block. First panel on the screen → ACCENT_2 title; a
/// manual-refresh spinner sits in the trailing title inset (` PLUGIN ⠇ `).
fn list_block(app: &App, focused: bool) -> Block<'static> {
    let border_color = if focused {
        theme::line_strong_color()
    } else {
        theme::line_color()
    };
    let mut title_mods = Modifier::ITALIC;
    if focused {
        title_mods |= Modifier::BOLD;
    }
    let title_style = Style::default()
        .fg(theme::accent_2_color())
        .add_modifier(title_mods);

    let mut title_spans = vec![Span::styled(" PLUGIN ", title_style)];
    if app.plugin.fetching {
        title_spans.push(Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        ));
    }

    Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(title_spans))
        .padding(ratatui::widgets::Padding::horizontal(1))
}

// ── Right panel: selected-row detail ────────────────────────────────────────────

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.plugin.focus == PluginFocus::Detail;

    // Title + body for the selected check. Labels are lowercase prose, so they
    // uppercase into a structural panel title via `section_box`.
    let (block, detail): (Block<'static>, &[String]) =
        if let Some(check) = app.plugin.selected_check() {
            (
                section_box(check.label, focused, false),
                check.detail.as_slice(),
            )
        } else {
            (section_box("plugin", focused, false), [].as_slice())
        };

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if detail.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled("no row selected", theme::dim())))
            .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    }

    // Width of the key column = widest `key: value` key, capped so a long key
    // can't shove the values off-pane.
    let key_w = detail
        .iter()
        // Indented sub-lines (model overrides) render dim, never through the key
        // column — exclude them so they can't inflate the real rows' gap.
        .filter(|line| !line.starts_with("  "))
        .filter_map(|line| line.split_once(": ").map(|(k, _)| k.chars().count()))
        .max()
        .unwrap_or(0)
        .min(18);
    let lines: Vec<Line<'static>> = detail.iter().map(|line| detail_line(line, key_w)).collect();
    let total = lines.len();
    let viewport = inner.height as usize;

    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.plugin.detail_max_scroll.set(max_scroll);
    let scroll = app.plugin.detail_scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

/// Style one detail line: the `[f] …` fix row as a hint (ACCENT-bold `[f]` key +
/// dim action, matching the footer hint bar), two-space-indented sub-lines (MCP
/// tool list, copyable commands) dim, `key: value` source rows as a label key
/// column + tone-colored value (colon dropped, gap-aligned to `key_w`), everything
/// else body text.
fn detail_line(text: &str, key_w: usize) -> Line<'static> {
    if text.is_empty() {
        return Line::from("");
    }
    if let Some(rest) = text.strip_prefix("[f]") {
        let label = rest.trim_start();
        return Line::from(vec![
            Span::styled("[f]", theme::accent().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(label.to_string(), theme::dim()),
        ]);
    }
    if text.starts_with("  ") {
        return Line::from(Span::styled(text.to_string(), theme::dim()));
    }
    if let Some((key, value)) = text.split_once(": ") {
        let pad = key_w.saturating_sub(key.chars().count()) + 2;
        return Line::from(vec![
            Span::styled(format!("{key}{}", " ".repeat(pad)), theme::label()),
            Span::styled(value.to_string(), value_tone(key, value)),
        ]);
    }
    Line::from(Span::styled(text.to_string(), theme::body()))
}

/// Tone the value of a `key: value` row by health-bearing (key, value-head) pairs.
/// Only genuinely health-bearing keys are colored; everything else stays body.
fn value_tone(key: &str, value: &str) -> Style {
    // Throughput rows carry a variable model-name key, so they tone by content:
    // a recent rate-limit or degraded pace warns.
    if value.contains("rate-limited") || value.contains("degraded") {
        return theme::warning();
    }
    let head = value.split_whitespace().next().unwrap_or(value);
    match (key, head) {
        ("present" | "installed", "yes") => theme::success(),
        ("present" | "installed", "no") => theme::warning(),
        ("server", "boots") => theme::success(),
        ("server", "failed") => theme::danger(),
        ("link", "linked") => theme::success(),
        ("link", "diverged") => theme::warning(),
        ("link", "missing") => theme::danger(),
        _ => theme::body(),
    }
}

fn health_color(health: Health) -> ratatui::style::Color {
    match health {
        Health::Ok => theme::success_color(),
        Health::Warn => theme::warning_color(),
        Health::Danger => theme::danger_color(),
        // Neutral: a profile that is neither linked nor live — not green.
        Health::Idle => theme::text_dim_color(),
    }
}
