//! Status tab — incident list on the left, the selected incident's timeline on
//! the right. Master-detail (counts as 2 of the 3-panel budget; no third panel).
//!
//! The left panel is the focusable selector: each incident takes two rows, a
//! title row and a `[ phase ]` pill row with a right-aligned relative age. A
//! spinner sits in the title inset while a manual refresh is in flight; feed
//! health lives in the header's `● status.claude.ai` dot. The right panel is
//! a read-only timeline the list descends into with enter; up/down scrolls it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::super::app::{App, StatusFocus};
use super::super::theme;
use super::format::{clock_label, relative_age, spinner_frame};
use super::panes::{draw_scrollbar, empty_state, key_cell, master_detail, section_box, wrap_words};
use crate::status::{Impact, Incident, IncidentUpdate, UpdatePhase, shorten_component_status};

/// Detail-pane key column width (matches the usage tab's `KEY_W`).
const KEY_W: usize = 11;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Each incident renders two rows (title + phase pill) in the list. On desktop
    // this is the house `selector_width | Min(20)` horizontal split (upstream's
    // layout); on narrow/phone widths it stacks selector-above-detail — both the
    // fork's narrow-TUI behavior and upstream's desktop layout live in
    // `master_detail`.
    let (list, detail) = master_detail(area, app.status.incidents.len() * 2);

    draw_incident_list(frame, list, app);
    draw_incident_detail(frame, detail, app);
}

// ── Left panel: incident list ─────────────────────────────────────────────────

fn draw_incident_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.status.focus == StatusFocus::List;
    let block = list_block(app, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.status.incidents.is_empty() {
        // The widget already renders the `r to retry` action line, so the hint
        // line only states the condition.
        let widget = if app.status.error.is_some() {
            empty_state("fetch failed", "r", "to retry")
        } else {
            empty_state("no status data yet", "r", "to fetch")
        };
        frame.render_widget(widget, inner);
        return;
    }

    // 2 lines per incident; window so the selected item stays visible.
    const ITEM_H: usize = 2;
    let viewport_lines = inner.height as usize;
    // Items that fill the viewport (ceil — the last may be partially clipped).
    let viewport_items = viewport_lines
        .div_ceil(ITEM_H)
        .max(1)
        .min(app.status.incidents.len());
    let first_item = first_visible_item(
        app.status.cursor,
        viewport_items,
        app.status.incidents.len(),
    );

    let shown = (app.status.incidents.len() - first_item).min(viewport_items);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(shown * ITEM_H);
    let content_w = inner.width as usize;
    for (i, incident) in app
        .status
        .incidents
        .iter()
        .enumerate()
        .skip(first_item)
        .take(shown)
    {
        let selected = i == app.status.cursor;
        lines.extend(incident_rows(incident, selected, focused, content_w));
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);

    draw_scrollbar(
        frame,
        inner,
        app.status.incidents.len() * ITEM_H,
        first_item * ITEM_H,
        viewport_lines,
    );
}

/// First item index to render so `cursor` is near the center of the viewport,
/// shifting the window on both ↑ and ↓ (not only when the cursor hits the bottom).
fn first_visible_item(cursor: usize, viewport_items: usize, total: usize) -> usize {
    if total <= viewport_items {
        return 0;
    }
    let half = viewport_items / 2;
    if cursor < half {
        0
    } else {
        cursor.saturating_sub(half).min(total - viewport_items)
    }
}

/// Two rendered lines for one incident: title row + phase-pill / age row. The
/// `BG_HOVER` tint spans both lines' full content width when selected.
fn incident_rows(
    incident: &Incident,
    selected: bool,
    pane_focused: bool,
    content_w: usize,
) -> Vec<Line<'static>> {
    let tint = if selected {
        Some(theme::bg_hover())
    } else {
        None
    };
    let with_bg = |style: Style| match tint {
        Some(c) => style.bg(c),
        None => style,
    };

    let caret = if selected && pane_focused {
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
    // Unselected titles read as primary content (TEXT); the selected one keeps
    // the focused TEXT + bold promotion.
    let title_style = if selected && pane_focused {
        with_bg(
            Style::default()
                .fg(theme::text_color())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        with_bg(theme::body())
    };
    let title = truncate(&incident.title, content_w.saturating_sub(2));
    let mut line1 = vec![caret, Span::styled(title, title_style)];
    pad_to(&mut line1, content_w, tint);

    // Line 2: 2-space gutter + `[ impact ]` pill (severity only, no status
    // word) + right-aligned age. Ongoing incidents swap the age for a semantic
    // dot. Fit priority: impact > age/dot.
    let mut line2: Vec<Span<'static>> = vec![Span::styled("  ", with_bg(Style::default()))];
    let mut used = 2usize;

    if !matches!(incident.impact, Impact::None) {
        let (iword, icolor) = impact_pill(&incident.impact);
        line2.extend([
            Span::styled("[ ", with_bg(theme::dim())),
            Span::styled(
                iword.clone(),
                with_bg(Style::default().fg(icolor).add_modifier(Modifier::BOLD)),
            ),
            Span::styled(" ]", with_bg(theme::dim())),
        ]);
        used += 2 + iword.chars().count() + 2; // "[ word ]" — no gap before it
    }

    if incident.is_active() {
        // Ongoing → semantic dot in place of the age, 1-char right padding.
        let dot_color = phase_text_color(&incident.phase);
        let dot = "●";
        let dot_w = dot.chars().count();
        // 1-char right pad so pad_to leaves a trailing space when possible.
        if used + 2 + dot_w <= content_w {
            let gap = content_w - used - dot_w - 1;
            line2.push(Span::styled(" ".repeat(gap), with_bg(Style::default())));
            line2.push(Span::styled(
                dot,
                with_bg(Style::default().fg(dot_color).add_modifier(Modifier::BOLD)),
            ));
        }
    } else {
        let age = relative_age(incident.started_ms);
        let age_w = age.chars().count();
        if used + 2 + age_w <= content_w {
            let gap = content_w - used - age_w - 1;
            line2.push(Span::styled(" ".repeat(gap), with_bg(Style::default())));
            line2.push(Span::styled(age, with_bg(theme::faint())));
        }
    }
    pad_to(&mut line2, content_w, tint);

    vec![Line::from(line1), Line::from(line2)]
}

/// Pad a span list with tinted filler so the `BG_HOVER` tint spans the full
/// content width (the ratatui filler-tint gotcha).
fn pad_to(spans: &mut Vec<Span<'static>>, content_w: usize, tint: Option<ratatui::style::Color>) {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = content_w.saturating_sub(used);
    if pad > 0 {
        let style = match tint {
            Some(c) => Style::default().bg(c),
            None => Style::default(),
        };
        spans.push(Span::styled(" ".repeat(pad), style));
    }
}

/// The list panel block. The Block draws every border dash (chrome owns the
/// dashes); the title is a bare token with single-space insets. A
/// manual-refresh spinner lives inside the title token's trailing inset
/// (` INCIDENTS ⠇ `), never appended after it. Feed health lives in the
/// header's `● status.claude.ai` dot, not here.
fn list_block(app: &App, focused: bool) -> Block<'static> {
    let border_color = if focused {
        theme::line_strong_color()
    } else {
        theme::line_color()
    };
    let border_style = Style::default().fg(border_color);

    // First panel on the screen → ACCENT_2 title, italic, bold when focused.
    let mut title_mods = Modifier::ITALIC;
    if focused {
        title_mods |= Modifier::BOLD;
    }
    let title_style = Style::default()
        .fg(theme::accent_2_color())
        .add_modifier(title_mods);

    // Title token: ` INCIDENTS ` with the spinner inside the trailing inset.
    let mut title_spans = vec![Span::styled(" INCIDENTS ", title_style)];
    if app.status.fetching {
        title_spans.push(Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        ));
    }

    Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(Line::from(title_spans))
        .padding(ratatui::widgets::Padding::horizontal(1))
}

/// `(word, color)` for an incident's status pill — delegates to
/// [`phase_text_color`] so the pill, detail header, and timeline all agree.
fn phase_pill(incident: &Incident) -> (String, ratatui::style::Color) {
    (incident.phase.word(), phase_text_color(&incident.phase))
}

// ── Right panel: incident timeline ─────────────────────────────────────────────

fn draw_incident_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let incident = app.status.selected();
    let title = incident.map(|i| i.title.as_str()).unwrap_or("status");
    // Read-only detail pane (focus descends but it's the second panel): blurred
    // when the list owns focus, strong when the detail is focused.
    let focused = app.status.focus == StatusFocus::Detail;
    let block = section_box(title, focused, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(incident) = incident else {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no incident selected",
            theme::dim(),
        )))
        .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    let lines = detail_lines(incident, inner.width as usize);
    let total = lines.len();
    let viewport = inner.height as usize;

    // Clamp the scroll so the last line stays reachable but the body never
    // scrolls past its end. Record the bound so the key handler can clamp state
    // too (otherwise a held ↓ inflates `detail_scroll` past the end).
    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.status.detail_max_scroll.set(max_scroll);
    let scroll = app.status.detail_scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

/// Build the full detail view as styled lines (header, started / components
/// rows, link, eyebrow, per-update timeline). The caller scrolls / clamps; this
/// is pure layout.
fn detail_lines(incident: &Incident, inner_w: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header: status pill + impact pill + age left; duration right-aligned.
    let (pill_word, pill_color) = phase_pill(incident);
    let mut header = vec![
        Span::styled("[ ", theme::dim()),
        Span::styled(
            pill_word,
            Style::default().fg(pill_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ]", theme::dim()),
    ];
    if !matches!(incident.impact, Impact::None) {
        let (iword, icolor) = impact_pill(&incident.impact);
        header.extend([
            Span::styled("  [ ", theme::dim()),
            Span::styled(
                iword,
                Style::default().fg(icolor).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ]", theme::dim()),
        ]);
    }
    let age_str = relative_age(incident.started_ms);
    header.push(Span::styled(format!("  {age_str}"), theme::faint()));

    let dur_str = match incident.resolved_ms {
        Some(resolved) if resolved >= incident.started_ms => {
            let dur = duration_label((resolved - incident.started_ms) / 1000);
            format!("lasted {dur}")
        }
        _ => "ongoing".to_string(),
    };
    let left_w: usize = header.iter().map(|s| s.content.chars().count()).sum();
    if left_w + dur_str.chars().count() < inner_w {
        let gap = inner_w - left_w - dur_str.chars().count();
        header.push(Span::styled(" ".repeat(gap), Style::default()));
        header.push(Span::styled(dur_str, theme::faint()));
        lines.push(Line::from(header));
    } else {
        // No room to right-align: the duration drops to its own line instead
        // of gluing onto the age (`…2026-06-06ongoing`).
        lines.push(Line::from(header));
        lines.push(Line::from(Span::styled(
            format!("  {dur_str}"),
            theme::faint(),
        )));
    }

    // started row (the one place the `utc` suffix appears).
    lines.push(Line::from(vec![
        key_span("started"),
        Span::styled(clock_label(incident.started_ms, true), theme::body()),
    ]));

    if !incident.components.is_empty() {
        lines.push(components_line(&incident.components, inner_w));
    }

    // Link line, middle-truncated to width (kept as-is per user).
    if !incident.link.is_empty() {
        lines.push(Line::from(Span::styled(
            middle_truncate(&incident.link, inner_w),
            theme::faint(),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("TIMELINE", theme::dim())));

    // Per update (newest first). Columns: time | phase word | wrapped body.
    let time_col = incident
        .updates
        .iter()
        .map(|u| clock_label(u.at_ms, false).chars().count())
        .max()
        .unwrap_or(0)
        .max("jun 6, 10:14".chars().count())
        .min(18);
    let phase_col = 13; // widest phase word ("investigating") + gap
    let indent = time_col + 1 + phase_col + 1;
    let body_w = inner_w.saturating_sub(indent).max(8);

    for update in &incident.updates {
        lines.extend(update_lines(update, time_col, phase_col, body_w, indent));
    }
    lines
}

/// Render one update: a `time | phase | wrapped body` row (body wraps with a
/// hanging indent under the body column), then one `TEXT_FAINT` continuation
/// line per changed component, indented to the body column.
fn update_lines(
    update: &IncidentUpdate,
    time_col: usize,
    phase_col: usize,
    body_w: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    let time = pad_right(&clock_label(update.at_ms, false), time_col);
    let phase = pad_right(&update.phase.word(), phase_col);
    let phase_color = phase_text_color(&update.phase);

    let wrapped = wrap_words(&update.text, body_w);
    let mut out = Vec::with_capacity(wrapped.len() + update.transitions.len());
    let first_body = wrapped.first().cloned().unwrap_or_default();
    out.push(Line::from(vec![
        Span::styled(format!("{time} "), theme::dim()),
        Span::styled(format!("{phase} "), Style::default().fg(phase_color)),
        Span::styled(first_body, theme::dim()),
    ]));
    for cont in wrapped.iter().skip(1) {
        out.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(cont.clone(), theme::dim()),
        ]));
    }
    out.extend(transition_lines(&update.transitions, indent, body_w));
    out
}

/// Aligned, colored transition block, one line per changed component, indented
/// to the body column: `name  → new`. The name column is left-padded to the
/// block's widest name + a 2-space gap; the arrow renders `TEXT_FAINT`; the new
/// status carries its semantic color (the previous state is dropped — the prior
/// timeline entry already shows it). Width-safe: an over-long line truncates the
/// NAME column first, then trailing-truncates the whole line as a last resort.
fn transition_lines(
    transitions: &[(String, String, String)],
    indent: usize,
    body_w: usize,
) -> Vec<Line<'static>> {
    if transitions.is_empty() {
        return Vec::new();
    }
    let rows: Vec<(String, String)> = transitions
        .iter()
        .map(|(name, _, new)| (name.to_lowercase(), shorten_component_status(new)))
        .collect();
    let widest_name = rows
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    // Reserve room for ` → new` so the name column can shrink under pressure.
    let max_status = rows
        .iter()
        .map(|(_, n)| 2 + n.chars().count())
        .max()
        .unwrap_or(0);
    // Name column width: widest name, but shrunk so `name  → new` fits body_w.
    let name_col = widest_name
        .min(body_w.saturating_sub(2 + max_status).max(3))
        .max(1);

    rows.into_iter()
        .map(|(name, new)| {
            let name = pad_right(&truncate(&name, name_col), name_col);
            let new_color = component_status_color(&new);
            let spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(format!("{name}  "), theme::faint()),
                Span::styled("→ ", theme::faint()),
                Span::styled(new, Style::default().fg(new_color)),
            ];
            // Last-resort guard: if the assembled content still overflows body_w,
            // trailing-truncate the whole line so ratatui never clips silently.
            let content_w: usize = spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
                - indent;
            if content_w > body_w {
                let mut flat = String::new();
                for s in spans.iter().skip(1) {
                    flat.push_str(&s.content);
                }
                Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled(truncate(&flat, body_w), theme::faint()),
                ])
            } else {
                Line::from(spans)
            }
        })
        .collect()
}

/// Semantic timeline color per phase: resolved/completed SUCCESS, monitoring &
/// in-progress/verifying INFO, identified/investigating WARNING, scheduled &
/// update/other TEXT_DIM.
fn phase_text_color(phase: &UpdatePhase) -> ratatui::style::Color {
    match phase {
        UpdatePhase::Resolved | UpdatePhase::Completed => theme::success_color(),
        UpdatePhase::Monitoring | UpdatePhase::InProgress | UpdatePhase::Verifying => {
            theme::info_color()
        }
        UpdatePhase::Identified | UpdatePhase::Investigating => theme::warning_color(),
        UpdatePhase::Update | UpdatePhase::Scheduled | UpdatePhase::Other(_) => {
            theme::text_dim_color()
        }
    }
}

/// `(word, color)` for an impact pill. Minor → WARNING; major/critical → DANGER;
/// none/maintenance/other → neutral TEXT_DIM.
fn impact_pill(impact: &Impact) -> (String, ratatui::style::Color) {
    let color = match impact {
        Impact::Minor => theme::warning_color(),
        Impact::Major | Impact::Critical => theme::danger_color(),
        Impact::None | Impact::Maintenance | Impact::Other(_) => theme::text_dim_color(),
    };
    (impact.word(), color)
}

/// Semantic color for a component status (raw or shortened word): operational →
/// SUCCESS, degraded → WARNING, partial/major outage → DANGER, maintenance →
/// TEXT_DIM, unknown → TEXT_FAINT.
fn component_status_color(status: &str) -> ratatui::style::Color {
    match status.trim().to_ascii_lowercase().as_str() {
        "operational" => theme::success_color(),
        "degraded" | "degraded_performance" => theme::warning_color(),
        "partial outage" | "partial_outage" | "major outage" | "major_outage" => {
            theme::danger_color()
        }
        "maintenance" | "under_maintenance" => theme::text_dim_color(),
        _ => theme::text_faint_color(),
    }
}

/// Build the detail `components` row: `● name  ● name`. The dot alone carries
/// the component's first-reported status (user decision — no status word; the
/// name is `theme::body()`, lowercased — value-row consistency, a house
/// deviation from the dim-label rule). Fits whole entries only; dropped entries
/// append `+N`. When the column has zero room, shows just `…`.
fn components_line(components: &[(String, String)], inner_w: usize) -> Line<'static> {
    let avail = inner_w.saturating_sub(KEY_W + KEY_GUTTER);
    let mut spans: Vec<Span<'static>> = vec![key_span("components")];

    // Zero-width guard: no room for even one entry → a faint ellipsis.
    if avail == 0 {
        spans.push(Span::styled("…", theme::faint()));
        return Line::from(spans);
    }

    let mut used = 0usize;
    let mut shown = 0usize;
    for (i, (name, status)) in components.iter().enumerate() {
        let label = name.to_lowercase();
        let gap = if i == 0 { 0 } else { 2 };
        let entry_w = gap + 2 + label.chars().count();
        // Reserve room for a possible trailing `  +N` when more remain.
        let remaining_after = components.len() - i - 1;
        let reserve = if remaining_after > 0 {
            2 + 1 + count_digits(remaining_after)
        } else {
            0
        };
        if used + entry_w + reserve > avail && shown > 0 {
            break;
        }
        if gap > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            "●",
            Style::default().fg(component_status_color(status)),
        ));
        spans.push(Span::styled(format!(" {label}"), theme::body()));
        used += entry_w;
        shown += 1;
    }
    let dropped = components.len() - shown;
    if dropped > 0 {
        spans.push(Span::styled(format!("  +{dropped}"), theme::faint()));
    }
    Line::from(spans)
}

/// Decimal digit count of `n` (for reserving `+N` width).
fn count_digits(n: usize) -> usize {
    if n == 0 { 1 } else { (n.ilog10() + 1) as usize }
}

/// Single-largest-unit duration label for a span in seconds: `47m`, `2h`, `3d`.
fn duration_label(secs: u64) -> String {
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days >= 1 {
        format!("{days}d")
    } else if hours >= 1 {
        format!("{hours}h")
    } else {
        format!("{}m", mins.max(1))
    }
}

/// Detail key column: `key` padded to the shared key-cell width in the eyebrow
/// label style.
fn key_span(key: &str) -> Span<'static> {
    Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::label())
}

// ── Small text helpers ──────────────────────────────────────────────────────

use crate::format::truncate;

/// Middle-ellipsis truncation (for URLs / IDs — both ends carry meaning).
fn middle_truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 3 {
        return truncate(s, max);
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let front: String = chars[..head].iter().collect();
    let back: String = chars[chars.len() - tail..].iter().collect();
    format!("{front}…{back}")
}

/// Pad `s` on the right to `width` chars (truncating with `…` if longer).
fn pad_right(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count > width {
        return truncate(s, width);
    }
    format!("{s}{}", " ".repeat(width - count))
}
