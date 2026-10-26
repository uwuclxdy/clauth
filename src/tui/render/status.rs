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
use super::format::{
    NO_DATA, duration_label, fixed, grapheme_cells, middle_truncate, prefix_in_cells, relative_age,
    spinner_frame, truncate,
};
use super::panes::{
    draw_scrollbar, empty_state, follow_scroll_offset, key_cell, master_detail, section_box,
    section_box_verbatim, with_bg, wrap_words,
};
use crate::status::{Impact, Incident, IncidentUpdate, UpdatePhase, shorten_component_status};

/// Detail-pane key column width, the usage tab's `TP_KEY_W` (11).
const KEY_W: usize = 11;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;
/// Rows per rendered incident (a title row + a phase-pill row).
const STATUS_ROW_LINES: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Each incident renders two rows (title + phase pill) in the list.
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
        app.status.selector.offset.set(0);
        app.status.selector.viewport.set(0);
        app.status.selector.starts.replace(Vec::new());
        app.status.selector.block_ends.replace(Vec::new());
        app.status.selector.total.set(0);
        // The widget already renders the `r to retry` action line, so the hint
        // line only states the condition.
        if app.status.error.is_some() {
            empty_state(frame, inner, "fetch failed", "r", "to retry")
        } else {
            empty_state(frame, inner, "no status data yet", "r", "to fetch")
        };
        return;
    }

    let content_w = inner.width as usize;
    let viewport = if inner.height > 1 {
        inner.height as usize / STATUS_ROW_LINES * STATUS_ROW_LINES
    } else {
        inner.height as usize
    };
    let whole_inner = Rect {
        height: viewport as u16,
        ..inner
    };
    let total = app.status.incidents.len() * STATUS_ROW_LINES;
    let cursor = app.status.cursor.min(app.status.incidents.len() - 1);
    let focus = (
        cursor * STATUS_ROW_LINES,
        cursor * STATUS_ROW_LINES + STATUS_ROW_LINES,
    );
    app.status.selector.viewport.set(viewport);
    app.status.selector.total.set(total);
    app.status.selector.starts.replace(
        (0..app.status.incidents.len())
            .map(|i| i * STATUS_ROW_LINES)
            .collect(),
    );
    app.status.selector.block_ends.replace(
        (0..app.status.incidents.len())
            .map(|i| i * STATUS_ROW_LINES + STATUS_ROW_LINES)
            .collect(),
    );
    let max_offset = total
        .saturating_sub(viewport)
        .min(total.saturating_sub(STATUS_ROW_LINES))
        & !(STATUS_ROW_LINES - 1);
    let offset = follow_scroll_offset(total, viewport, focus, app.status.selector.offset.get())
        .next_multiple_of(STATUS_ROW_LINES)
        .min(max_offset);
    app.status.selector.offset.set(offset);
    let lines: Vec<Line<'static>> = app
        .status
        .incidents
        .iter()
        .enumerate()
        .flat_map(|(i, incident)| incident_rows(incident, i == cursor, focused, content_w))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((offset.min(u16::MAX as usize) as u16, 0)),
        whole_inner,
    );
    draw_scrollbar(frame, inner, total, offset, viewport);
}

/// Two rendered lines for one incident: title row + phase-pill / age row. The
/// `BG_HOVER` tint spans both lines' full content width when selected.
fn incident_rows(
    incident: &Incident,
    selected: bool,
    pane_focused: bool,
    content_w: usize,
) -> Vec<Line<'static>> {
    let tint = selected.then(theme::bg_hover);

    // The stacked selector reaches content widths under the 2-cell gutter; the
    // row keeps its leading indent inside `content_w` rather than overrunning.
    let gutter = content_w.min(2);
    let (caret_text, caret_w) = prefix_in_cells(
        if selected && pane_focused {
            "❯ "
        } else {
            "  "
        },
        gutter,
    );
    let caret = if selected && pane_focused {
        Span::styled(
            caret_text,
            with_bg(
                Style::default()
                    .fg(theme::accent_color())
                    .add_modifier(Modifier::BOLD),
                tint,
            ),
        )
    } else {
        Span::styled(caret_text, with_bg(Style::default(), tint))
    };
    // Unselected titles read as primary content (TEXT); the selected one keeps
    // the focused TEXT + bold promotion.
    let title_style = if selected && pane_focused {
        with_bg(
            Style::default()
                .fg(theme::text_color())
                .add_modifier(Modifier::BOLD),
            tint,
        )
    } else {
        with_bg(theme::body(), tint)
    };
    let title = truncate(&incident.title, content_w.saturating_sub(caret_w));
    let mut line1 = vec![caret, Span::styled(title, title_style)];
    pad_to(&mut line1, content_w, tint);

    // Line 2: 2-space gutter + `[ impact ]` pill (severity only, no status
    // word) + right-aligned age. Ongoing incidents swap the age for the phase
    // colour's `●` beside its word. Ongoing: phase word > impact; resolved:
    // impact > age.
    let mut line2: Vec<Span<'static>> = vec![Span::styled(
        " ".repeat(gutter),
        with_bg(Style::default(), tint),
    )];
    let mut used = gutter;

    let active = incident.is_active();
    let word = if active {
        Some(incident.phase.word())
    } else {
        None
    };
    let word_w = word.as_deref().map_or(0, grapheme_cells);

    if !matches!(incident.impact, Impact::None) {
        let (iword, icolor) = impact_pill(&incident.impact);
        let pill_w = 2 + grapheme_cells(&iword) + 2; // "[ word ]" — no gap before it
        // An ongoing row keeps the pill only while the 2-cell gap, `● ` and the
        // whole phase word still fit after it with the 1-cell right pad; the
        // word outranks the pill, which drops whole rather than truncating.
        let pill_fits = content_w.saturating_sub(used + pill_w + (2 + word_w) + 1) >= 2;
        // A resolved row keeps the pill ahead of the age; a pill that fills the
        // row exactly stays whole, and the trailing pad cell yields first.
        let pill_stays_whole = if active {
            pill_fits
        } else {
            used + pill_w <= content_w
        };
        if pill_stays_whole {
            line2.extend([
                Span::styled("[ ", with_bg(theme::dim(), tint)),
                Span::styled(
                    iword,
                    with_bg(
                        Style::default().fg(icolor).add_modifier(Modifier::BOLD),
                        tint,
                    ),
                ),
                Span::styled(" ]", with_bg(theme::dim(), tint)),
            ]);
            used += pill_w;
        } else if !active {
            // The 4 bracket cells come off the word's budget; the trailing pad
            // cell yields first, as it does for the age.
            let budget = content_w.saturating_sub(used + 4);
            if let Some((cut, cut_w)) =
                truncate_keeping_a_grapheme(&iword, grapheme_cells(&iword), budget)
            {
                used += 4 + cut_w;
                line2.extend([
                    Span::styled("[ ", with_bg(theme::dim(), tint)),
                    Span::styled(
                        cut,
                        with_bg(
                            Style::default().fg(icolor).add_modifier(Modifier::BOLD),
                            tint,
                        ),
                    ),
                    Span::styled(" ]", with_bg(theme::dim(), tint)),
                ]);
            }
        }
    }

    if let Some(word) = word {
        // The gutter and any kept pill are already in `used`; a 2-cell floor
        // (the 1-cell gap and the 1-cell right pad) and `● ` come off the phase
        // word's budget.
        let budget = content_w.saturating_sub(used + 4);
        let dot = Span::styled(
            "●",
            with_bg(
                Style::default()
                    .fg(phase_text_color(&incident.phase))
                    .add_modifier(Modifier::BOLD),
                tint,
            ),
        );
        if let Some((cut, cut_w)) = truncate_keeping_a_grapheme(&word, word_w, budget) {
            let gap = content_w.saturating_sub(used + (2 + cut_w) + 1);
            line2.push(Span::styled(
                " ".repeat(gap),
                with_bg(Style::default(), tint),
            ));
            line2.push(dot);
            line2.push(Span::styled(" ", with_bg(Style::default(), tint)));
            line2.push(Span::styled(cut, with_bg(theme::dim(), tint)));
        } else {
            // Not even one word cell fits: the bare dot, never a lone ellipsis.
            let gap = content_w.saturating_sub(used + 2);
            if gap > 0 {
                line2.push(Span::styled(
                    " ".repeat(gap),
                    with_bg(Style::default(), tint),
                ));
                line2.push(dot);
            }
        }
    } else {
        let age = relative_age(incident.started_ms);
        let age_w = grapheme_cells(&age);
        if used + 2 + age_w <= content_w {
            let gap = content_w.saturating_sub(used + age_w + 1);
            line2.push(Span::styled(
                " ".repeat(gap),
                with_bg(Style::default(), tint),
            ));
            line2.push(Span::styled(age, with_bg(theme::faint(), tint)));
        }
    }
    pad_to(&mut line2, content_w, tint);

    vec![Line::from(line1), Line::from(line2)]
}

/// `truncate` to `budget` cells, unless the cut keeps no grapheme of `value`:
/// `truncate` spends a cell on `…`, so a cut that is only `…` is punctuation
/// without the thing it truncates.
fn truncate_keeping_a_grapheme(
    value: &str,
    value_w: usize,
    budget: usize,
) -> Option<(String, usize)> {
    let cut = truncate(value, budget);
    let cut_w = grapheme_cells(&cut);
    let keeps_a_grapheme = cut_w > 0 && (value_w <= budget || cut != "…");
    keeps_a_grapheme.then_some((cut, cut_w))
}

/// Pad a span list with tinted filler so the `BG_HOVER` tint spans the full
/// content width (the ratatui filler-tint gotcha).
fn pad_to(spans: &mut Vec<Span<'static>>, content_w: usize, tint: Option<ratatui::style::Color>) {
    let used: usize = spans.iter().map(|span| grapheme_cells(&span.content)).sum();
    let pad = content_w.saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(
            " ".repeat(pad),
            with_bg(Style::default(), tint),
        ));
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

    // Title token: ` INCIDENTS ` with the spinner inside the trailing inset,
    // opened by the corner-adjacent border dash (`╭─ INCIDENTS`).
    let mut title_spans = vec![
        Span::styled("─", border_style),
        Span::styled(" INCIDENTS ", title_style),
    ];
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
    // Read-only detail pane (focus descends but it's the second panel): blurred
    // when the list owns focus, strong when the detail is focused.
    let focused = app.status.focus == StatusFocus::Detail;
    // The title IS the selected incident's own identity when one is selected, so
    // it keeps its case (the Panel proper-noun exception); the no-selection
    // fallback is a structural label and stays uppercase.
    let block = match incident {
        Some(i) => section_box_verbatim(&i.title, focused, false, area.width),
        None => section_box("status", focused, false, area.width),
    };
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
    app.status.detail_viewport.set(viewport as u16);
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
    let age_span = Span::styled(format!("  {age_str}"), theme::faint());

    let dur_str = match incident.resolved_ms {
        Some(resolved) if resolved >= incident.started_ms => {
            // The floor keeps the pre-shared-formatter output: a sub-minute
            // incident reads `1m`, never `45s` (see `duration_label`).
            let dur = duration_label(((resolved - incident.started_ms) / 1000).max(60));
            format!("lasted {dur}")
        }
        _ => "ongoing".to_string(),
    };
    let pills_w: usize = header.iter().map(Span::width).sum();
    let age_w = age_span.width();
    let dur_w = Span::raw(&dur_str).width();
    if pills_w + age_w + dur_w < inner_w {
        let gap = inner_w - pills_w - age_w - dur_w;
        header.push(age_span);
        header.push(Span::styled(" ".repeat(gap), Style::default()));
        header.push(Span::styled(dur_str, theme::faint()));
        lines.push(Line::from(header));
    } else if pills_w + age_w < inner_w {
        // The duration drops to its own line instead of gluing onto the age
        // (`…2026-06-06ongoing`).
        header.push(age_span);
        lines.push(Line::from(header));
        lines.push(Line::from(Span::styled(
            format!("  {dur_str}"),
            theme::faint(),
        )));
    } else {
        // The pills alone fill the pane: age then duration each stack below.
        lines.push(Line::from(header));
        lines.push(Line::from(Span::styled(
            format!("  {age_str}"),
            theme::faint(),
        )));
        lines.push(Line::from(Span::styled(
            format!("  {dur_str}"),
            theme::faint(),
        )));
    }

    // `started`: the stamp drops to its own line once the key leaves no room.
    let started_stamp = stamp(incident.started_ms);
    if KEY_W + KEY_GUTTER + Span::raw(&started_stamp).width() <= inner_w {
        lines.push(Line::from(vec![
            key_span("started"),
            Span::styled(started_stamp, theme::body()),
        ]));
    } else {
        lines.push(Line::from(key_span("started")));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(started_stamp, theme::body()),
        ]));
    }

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
    // Every stamp renders the fixed-width local shape (see `stamp`), so the
    // column is that shape's own width rather than the widest rendered row.
    let time_col = 19; // `YYYY-MM-DD HH:MM:SS`
    let phase_col = 13; // widest phase word ("investigating") + gap

    for update in &incident.updates {
        lines.extend(update_lines(update, time_col, phase_col, inner_w));
    }
    lines
}

/// Render one update: a `time | phase | wrapped body` row (body wraps with a
/// hanging indent under the body column), then one `TEXT_FAINT` continuation
/// line per changed component, indented to the body column. When the fixed
/// time + phase columns leave too little room, the body stacks beneath them;
/// past that, the time and phase themselves stack.
fn update_lines(
    update: &IncidentUpdate,
    time_col: usize,
    phase_col: usize,
    inner_w: usize,
) -> Vec<Line<'static>> {
    let time = fixed(&stamp(update.at_ms), time_col);
    let phase = fixed(&update.phase.word(), phase_col);
    let phase_color = phase_text_color(&update.phase);
    let indent = time_col + 1 + phase_col + 1;
    let body_w = inner_w.saturating_sub(indent).max(8);
    let wrapped = wrap_words(&update.text, body_w);
    let mut out = Vec::with_capacity(wrapped.len() + update.transitions.len() + 2);
    // Transitions indent under the body column only while the time + phase
    // columns sit side by side; the two stacked arms align them at the pane's
    // full width instead — a fixed 34-cell indent there blanks every row.
    let t_indent = if inner_w >= indent + 8 { indent } else { 0 };
    let t_body_w = if inner_w >= indent + 8 {
        body_w
    } else {
        inner_w
    };
    if inner_w >= indent + 8 {
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
    } else if inner_w >= indent {
        // The body drops below the time+phase line, wrapped to the full width.
        out.push(Line::from(vec![
            Span::styled(format!("{time} "), theme::dim()),
            Span::styled(format!("{phase} "), Style::default().fg(phase_color)),
        ]));
        for seg in wrap_words(&update.text, inner_w) {
            out.push(Line::from(vec![Span::styled(seg, theme::dim())]));
        }
    } else {
        // Time and phase no longer fit side by side: stack them, then the body.
        out.push(Line::from(vec![Span::styled(
            stamp(update.at_ms),
            theme::dim(),
        )]));
        out.push(Line::from(vec![Span::styled(
            update.phase.word().to_string(),
            Style::default().fg(phase_color),
        )]));
        for seg in wrap_words(&update.text, inner_w) {
            out.push(Line::from(vec![Span::styled(seg, theme::dim())]));
        }
    }
    out.extend(transition_lines(&update.transitions, t_indent, t_body_w));
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
        .map(|(n, _)| Span::raw(n).width())
        .max()
        .unwrap_or(0);
    // Reserve room for ` → new` so the name column can shrink under pressure.
    let max_status = rows
        .iter()
        .map(|(_, n)| 2 + Span::raw(n).width())
        .max()
        .unwrap_or(0);
    // Name column width: widest name, but shrunk so `name  → new` fits body_w.
    let name_col = widest_name
        .min(body_w.saturating_sub(2 + max_status).max(3))
        .max(1);

    rows.into_iter()
        .map(|(name, new)| {
            let name = fixed(&name, name_col);
            let new_color = component_status_color(&new);
            let spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(format!("{name}  "), theme::faint()),
                Span::styled("→ ", theme::faint()),
                Span::styled(new, Style::default().fg(new_color)),
            ];
            // Last-resort guard: if the assembled content still overflows body_w,
            // trailing-truncate the whole line so ratatui never clips silently.
            let content_w: usize = spans.iter().map(Span::width).sum::<usize>() - indent;
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

/// Detail key column: `key` padded to the shared key-cell width in the eyebrow
/// label style.
fn key_span(key: &str) -> Span<'static> {
    Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::label())
}

// ── Small text helpers ──────────────────────────────────────────────────────

use crate::format::local_stamp;

/// The incident stamps (`started` row + timeline rows): local wall clock in
/// the 2026-08-22 ruling's `YYYY-MM-DD HH:MM:SS` shape, through the one
/// prose-stamp formatter. A bare stamp reads as local, so the fallback for an
/// instant chrono cannot represent is the no-data glyph, never a UTC reading.
fn stamp(epoch_ms: u64) -> String {
    local_stamp((epoch_ms / 1000) as i64).unwrap_or_else(|| NO_DATA.to_string())
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_status.rs"]
mod tests;
