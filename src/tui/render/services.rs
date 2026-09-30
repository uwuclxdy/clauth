//! Services tab — the shunt gateway, the running delegates, the Claude Code
//! plugin and herdr on one selector, each row's readout on the right. The
//! master-detail mirrors the Status tab's two-pane machinery and counts as 2 of
//! the 3-panel budget.
//!
//! The left panel is one cursor-driven selector over the service rows. Each row
//! is a status dot + label, the verdict in the detail pane. Enter descends into
//! the detail pane; `f` applies the focused row's (or, on the plugin detail,
//! focused problem's) fix. All data is recomputed synchronously on tab focus
//! and `r`, the herdr probe excepted: it runs on a worker, its row appears when
//! it lands, and the title spinner shows while it runs.
//!
//! The delegates detail is read-only and takes no keys at all: stopping a
//! delegate is `monitor({job_ids, cancel: true})`'s job, and a second stop path
//! would need its own confirm. That is also why its overflow reads `+N more`
//! rather than carrying a scrollbar — see [`delegate_lines`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::super::app::{
    App, Check, HERDR_OPTIONS, Health, HerdrOption, InputState, ServicesFocus, escape_control,
    herdr_config_writable, parse_herdr_tag_secs,
};
use super::super::theme;
use super::format::{middle_truncate, spinner_frame};
use super::panes::{
    cycle_option, draw_scrollbar, draw_scrolled_lines, empty_state, head_cols, help_tooltip_lines,
    highlight_row, invalid_tooltip_lines, key_cell, label_style, master_detail, section_box,
    value_caret,
};
use crate::format::truncate;
use crate::mcp::jobs::{self, JobPhase, RunningLiveness, StoredJob};
use crate::profile::{HerdrSettings, PopupWidth};
use crate::usage::humanize_duration;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (selector, detail) = master_detail(area, app.services.row_count());
    draw_selector(frame, selector, app);
    draw_detail(frame, detail, app);
}

// ── Left panel: service rows selector ──────────────────────────────────────────

fn draw_selector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let focused = app.services.focus == ServicesFocus::List;
    let block = list_block(app, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.services.row_count() == 0 {
        let widget = if app.services.error.is_some() {
            empty_state("check failed", "r", "to retry")
        } else {
            empty_state("no services yet", "r", "to run")
        };
        frame.render_widget(widget, inner);
        return;
    }

    let content_w = inner.width as usize;
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cursor_line = 0usize;

    for (idx, check) in app.services.checks.iter().enumerate() {
        if idx == app.services.cursor {
            cursor_line = rows.len();
        }
        rows.push(selector_row(
            check.health,
            check.label,
            idx == app.services.cursor,
            focused,
            content_w,
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

/// One selector row: `❯ ● label`. Rows are dot-only — the dot color carries the
/// verdict and the full readout lives in the detail pane — so there is no
/// `[f]` cue on the row. The hover tint spans the full content width when
/// selected (the ratatui filler-tint gotcha); the caret shows only in the
/// focused pane.
fn selector_row(
    health: Health,
    label: &str,
    selected: bool,
    focused: bool,
    content_w: usize,
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
        with_bg(theme::body().add_modifier(Modifier::BOLD))
    } else {
        with_bg(theme::body())
    };

    let mut spans = vec![caret, dot, Span::styled(label.to_string(), label_style)];
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = content_w.saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), with_bg(Style::default())));
    }
    Line::from(spans)
}

/// The selector panel block. First panel on the screen → ACCENT_2 title; a
/// manual-refresh spinner sits in the trailing title inset (` SERVICES ⠇ `).
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

    let mut title_spans = vec![
        Span::styled("─", Style::default().fg(border_color)),
        Span::styled(" SERVICES ", title_style),
    ];
    if app.services.fetching
        || app.services.herdr_probe.running
        || app.services.standalone_probe.running
    {
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
    let focused = app.services.focus == ServicesFocus::Detail;

    let Some(check) = app.services.selected_check() else {
        let block = section_box("services", focused, false);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let hint = Paragraph::new(Line::from(Span::styled("no row selected", theme::dim())))
            .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    };

    let block = section_box(check.label, focused, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if check.label == "delegates" {
        draw_delegates_detail(frame, inner, app);
        return;
    }

    if check.detail.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled("no row selected", theme::dim())))
            .style(theme::base());
        frame.render_widget(hint, inner);
        return;
    }

    // Width of the key column = widest `key: value` key, capped so a long key
    // can't shove the values off-pane. Fix lines (`f  <verb>`) and indented
    // sub-lines never split on `": "`, so they stay out of the column.
    let key_w = check
        .detail
        .iter()
        .filter(|line| !line.starts_with("  "))
        .filter_map(|line| line.split_once(": ").map(|(k, _)| k.chars().count()))
        .max()
        .unwrap_or(0)
        .min(18);

    // The plugin detail walks its fixable problems while descended; its fix
    // line carries the caret + hover tint then.
    let problem_focus = if focused && check.label == "plugin" && !check.problems.is_empty() {
        Some(app.services.problem_cursor)
    } else {
        None
    };
    let lines: Vec<Line<'static>> = check
        .detail
        .iter()
        .enumerate()
        .map(|(idx, line)| match problem_index(check, idx) {
            Some(pidx) => {
                let selected = problem_focus == Some(pidx);
                let line = problem_line(line, selected);
                if selected {
                    highlight_row(line, inner.width as usize)
                } else {
                    line
                }
            }
            None => detail_line(line, key_w, inner.width as usize),
        })
        .collect();

    // The herdr detail appends its options section and takes per-row focus;
    // every other detail keeps the scroll-only path below.
    if check.label == "herdr" {
        draw_herdr_detail(frame, inner, app, lines);
        return;
    }

    // The plugin detail's problem walk scrolls to keep the focused line on
    // screen (the herdr-options `draw_scrolled_lines` shape). `.get` rather
    // than an index: a fix landing under the focused cursor can shrink the
    // problem set, and the render must never panic on a stale cursor.
    if check.label == "plugin"
        && problem_focus.is_some()
        && let Some(problem) = check.problems.get(app.services.problem_cursor)
    {
        let line = problem.line;
        draw_scrolled_lines(frame, inner, lines, (line, line + 1));
        return;
    }

    let total = lines.len();
    let viewport = inner.height as usize;

    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.services.detail_max_scroll.set(max_scroll);
    let scroll = app.services.detail_scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

/// The `Check::problems` index of a detail line, `None` for a plain line.
fn problem_index(check: &Check, line: usize) -> Option<usize> {
    check.problems.iter().position(|p| p.line == line)
}

/// The `delegates` detail: the job list, capped with `+N more`, the closing
/// steer line underneath. The delegates check's one `key: value` detail line
/// (the rate-limit warning) rides above the list when it applies. No key
/// reaches it, so no scrollbar.
fn draw_delegates_detail(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    if inner.height == 0 {
        return;
    }
    let rows = delegate_cells(&app.services.delegates, crate::usage::now_ms());
    let mut body = inner;
    if let Some(line) = app
        .services
        .selected_check()
        .and_then(|c| c.detail.first().cloned())
    {
        let [warn_area, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let key_w = line
            .split_once(": ")
            .map(|(k, _)| k.chars().count())
            .unwrap_or(0)
            .min(18);
        frame.render_widget(
            Paragraph::new(detail_line(&line, key_w, warn_area.width as usize))
                .style(theme::base()),
            warn_area,
        );
        body = rest;
    }
    let [list_area, steer_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(body);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(DELEGATES_STEER, theme::faint())))
            .style(theme::base()),
        steer_area,
    );

    if rows.is_empty() {
        frame.render_widget(empty_state("no delegates", "r", "to refresh"), list_area);
        return;
    }
    let lines = delegate_lines(&rows, list_area.height as usize, list_area.width as usize);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), list_area);
}

/// The herdr detail: the read-only prose above, then an `options` section of
/// six focusable form rows editing the `AppState.herdr` knobs. While the
/// detail pane is descended, ↑↓ walks the rows and the whole form scrolls so
/// the focused row (plus its tooltip) stays on screen — the form-pane
/// `draw_scrolled_lines` shape, so the prose scrolls to follow the cursor
/// rather than holding a manual offset. The section header underlines while
/// focus rests on one of its rows.
fn draw_herdr_detail(frame: &mut Frame<'_>, inner: Rect, app: &App, mut lines: Vec<Line<'static>>) {
    let focused = app.services.focus == ServicesFocus::Detail;

    lines.push(Line::from(""));
    let mut section_style = theme::label();
    if focused {
        section_style = section_style.underlined();
    }
    lines.push(Line::from(Span::styled("OPTIONS", section_style)));

    let settings = app.config().state.herdr.clone();
    let editing = app.services.herdr_tag_draft.as_ref();
    let writable = herdr_config_writable(app);
    // The focused row's block (row + its tooltip lines) for the scroll focus,
    // and the native-cursor slot for the tag editor.
    let mut focus = (0usize, 1usize);
    let mut caret: Option<(u16, usize)> = None;

    for (i, row) in HERDR_OPTIONS.iter().enumerate() {
        let selected = focused && i == app.services.herdr_options_cursor;
        let row_editing = if *row == HerdrOption::TagRefresh {
            editing
        } else {
            None
        };
        let inert = *row == HerdrOption::DelegateRowText && !writable;
        if selected {
            focus.0 = lines.len();
        }
        let line = option_row(*row, &settings, selected, row_editing, inert);
        match row_editing {
            Some(input) => {
                // The edit row renders plain (no highlight) with the edit
                // gutter; the native terminal cursor owns the caret. x = "✎ "
                // (2) + label + the 2-space value gap + pre-caret cols.
                let cx = inner.x.saturating_add(
                    (2 + row.label().chars().count() + 2 + head_cols(input)) as u16,
                );
                caret = Some((cx, lines.len()));
                lines.push(line);
                lines.extend(tag_refresh_range_tooltip(input, inner.width as usize));
            }
            None => {
                lines.push(if selected {
                    highlight_row(line, inner.width as usize)
                } else {
                    line
                });
                if selected && inert {
                    lines.extend(help_tooltip_lines(
                        herdr_row_text_tooltip(app),
                        inner.width as usize,
                    ));
                }
            }
        }
        if selected {
            focus.1 = lines.len();
        }
    }

    let offset = draw_scrolled_lines(frame, inner, lines, focus);
    // A caret scrolled off the top has no cell to sit in; leaving the cursor
    // unset is better than parking it on an unrelated row.
    if let Some((cx, row)) = caret
        && let Some(visible) = row
            .checked_sub(offset)
            .filter(|v| *v < inner.height as usize)
    {
        frame.set_cursor_position((cx, inner.y.saturating_add(visible as u16)));
    }
}

/// One herdr-options form row: caret gutter + lowercase label + the row's
/// control, the value trailing the label by a 2-space gap. Ragged rows by
/// design — the caret and tint carry alignment, so no key column. `selected`
/// promotes the label and brackets the cycle row's option; the caller adds
/// the tint via `highlight_row`. `inert` (delegate row text while herdr's
/// config cannot be rewritten) renders the whole row faint — a true disabled
/// row.
fn option_row(
    row: HerdrOption,
    settings: &HerdrSettings,
    selected: bool,
    editing: Option<&InputState>,
    inert: bool,
) -> Line<'static> {
    let arrow = if editing.is_some() {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold())
    } else if selected && inert {
        Span::styled("❯ ", theme::faint())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    let key_style = if inert {
        theme::faint()
    } else {
        label_style(selected)
    };
    let mut spans = vec![arrow, Span::styled(format!("{}  ", row.label()), key_style)];
    match row {
        HerdrOption::PopupWidth => {
            let width = settings.popup_width;
            for (i, (label, active)) in [
                ("fit", width == PopupWidth::Fit),
                ("half", width == PopupWidth::Half),
                ("split-right", width == PopupWidth::SplitRight),
                ("split-top", width == PopupWidth::SplitTop),
            ]
            .iter()
            .enumerate()
            {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(cycle_option(label, *active, selected));
            }
        }
        HerdrOption::PaneTag => spans.push(toggle_value(settings.pane_tag, inert)),
        HerdrOption::TagRefresh => match editing {
            Some(input) => {
                let invalid = parse_herdr_tag_secs(input.trimmed()).is_none();
                spans.extend(value_caret(input, invalid));
                let unit_style = if invalid {
                    theme::danger()
                } else {
                    theme::faint()
                };
                spans.push(Span::styled(" s", unit_style));
            }
            None => spans.push(Span::styled(
                format!("{}s", settings.tag_watch_secs),
                theme::accent(),
            )),
        },
        HerdrOption::BorderLabel => spans.push(toggle_value(settings.border_label, inert)),
        HerdrOption::DelegateDot => spans.push(toggle_value(settings.delegate_dot, inert)),
        HerdrOption::DelegateRowText => spans.push(toggle_value(settings.delegate_row_text, inert)),
    }
    Line::from(spans)
}

/// A toggle row's value: the tier-dependent glyph, ACCENT when on, faint when
/// off — and whole-faint on an inert row whatever its state.
fn toggle_value(on: bool, inert: bool) -> Span<'static> {
    let style = if inert || !on {
        theme::faint()
    } else {
        theme::accent()
    };
    Span::styled(
        if on {
            theme::toggle_on()
        } else {
            theme::toggle_off()
        },
        style,
    )
}

/// Sub-line under the tag-refresh field while typing: the floor, DANGER when
/// the buffer parses under it, else faint — the Config-tab refresh editor's
/// shape.
fn tag_refresh_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    const RANGE: &str = "min is 1 s";
    if parse_herdr_tag_secs(input.trimmed()).is_none() {
        invalid_tooltip_lines(RANGE, width)
    } else {
        help_tooltip_lines(RANGE, width)
    }
}

/// The disabled-row reason for `delegate row text`: the heal behind it writes
/// through herdr's parse, so a config that cannot be read or parsed leaves the
/// row nothing it can do.
fn herdr_row_text_tooltip(app: &App) -> &'static str {
    if app
        .services
        .herdr_config
        .as_ref()
        .is_some_and(|c| !c.parsed)
    {
        "herdr's config doesn't parse, so clauth can't rewrite the row"
    } else {
        "herdr's config can't be read, so clauth can't rewrite the row"
    }
}

/// Style one detail line: two-space-indented sub-lines (MCP tool list, copyable
/// commands) dim, `key: value` source rows as a label key column + tone-colored
/// value (colon dropped, gap-aligned to `key_w`), everything else body text.
/// Values and prose truncate to the pane (`width` cells) instead of clipping at
/// the border: path values keep both ends (middle ellipsis), prose trails.
fn detail_line(text: &str, key_w: usize, width: usize) -> Line<'static> {
    if text.is_empty() {
        return Line::from("");
    }
    if text.starts_with("  ") {
        return Line::from(Span::styled(truncate(text, width), theme::dim()));
    }
    if let Some((key, value)) = text.split_once(": ") {
        let pad = key_w.saturating_sub(key.chars().count()) + 2;
        let value_w = width.saturating_sub(key_w + 2);
        let rendered = if is_path_key(key) {
            middle_truncate(value, value_w)
        } else {
            truncate(value, value_w)
        };
        return Line::from(vec![
            Span::styled(format!("{key}{}", " ".repeat(pad)), theme::label()),
            Span::styled(rendered, value_tone(key, value)),
        ]);
    }
    Line::from(Span::styled(truncate(text, width), theme::body()))
}

/// The detail keys whose value is a filesystem path: a truncated path keeps
/// both ends, because the head (which tree) and the leaf (which file) both
/// carry meaning.
fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "data" | "path" | "project" | "root" | "config" | "binary" | "found"
    )
}

/// One `f  <verb>` fix line. Dim while unfocused — the whole line, `f`
/// included, per the fix-hint ruling; the focused problem takes the selected
/// row's caret + `TEXT + bold` (the caller adds the hover tint).
fn problem_line(text: &str, selected: bool) -> Line<'static> {
    if selected {
        Line::from(vec![
            Span::styled("❯ ", theme::accent().bold()),
            Span::styled(text.to_string(), label_style(true)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(text.to_string(), theme::dim()),
        ])
    }
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
        ("installed", "yes") | ("mcp entry", "registered") | ("mcp server", "ok") => {
            theme::success()
        }
        ("installed", "no") | ("mcp entry", "not") => theme::warning(),
        ("mcp server", "won't") => theme::danger(),
        ("plugin", "linked") => theme::success(),
        ("plugin", "installed") => theme::success(),
        ("plugin", "disabled") => theme::warning(),
        ("plugin", "not") => theme::warning(),
        ("key", "not") => theme::warning(),
        ("sidebar", "templated") => theme::success(),
        ("sidebar", "not") => theme::warning(),
        ("state", "not") => theme::dim(),
        _ => theme::body(),
    }
}

// ── Delegates detail: what the delegates are doing ──────────────────────────────

/// Width of the state word column (`orphaned` is the longest).
const STATE_W: usize = 8;
/// Cap on the account column, so one long name cannot push every row's figures
/// off the pane.
const NAME_W_MAX: usize = 18;
/// A directory squeezed below this keeps too little of either end to name a
/// tree, so it is dropped whole instead.
const CWD_MIN_W: usize = 12;

/// The line under the list. Owner's words, verbatim: the pane takes no keys, so
/// this is the whole of what it offers beyond the list itself.
const DELEGATES_STEER: &str = "manage delegates in clauth app on web or mobile (coming soon)";

/// The rows that fit, plus a `+N more` line naming what did not.
///
/// **House deviation**: everywhere else a scrollbar is the only legal overflow
/// signal. This pane binds no key, so a scrollbar here would advertise
/// a scroll that cannot happen; a count says the same thing and promises
/// nothing.
fn delegate_lines(rows: &[DelegateCells], viewport: usize, width: usize) -> Vec<Line<'static>> {
    if viewport == 0 {
        return Vec::new();
    }
    let (shown, hidden) = if rows.len() > viewport {
        (viewport - 1, rows.len() - (viewport - 1))
    } else {
        (rows.len(), 0)
    };
    let visible = &rows[..shown];
    let name_w = visible
        .iter()
        .map(|r| r.profile.chars().count())
        .max()
        .unwrap_or(0)
        .min(NAME_W_MAX);
    let mut lines: Vec<Line<'static>> = visible
        .iter()
        .map(|row| delegate_line(row, name_w, width))
        .collect();
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more"),
            theme::faint(),
        )));
    }
    lines
}

/// One delegate row: `● running  account  facts …  cwd`.
fn delegate_line(cells: &DelegateCells, name_w: usize, width: usize) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{} ", state_mark(cells.state)),
            Style::default().fg(state_color(cells.state)),
        ),
        Span::styled(key_cell(cells.state.label(), STATE_W, 2), theme::dim()),
        Span::styled(
            key_cell(&truncate(&cells.profile, name_w), name_w, 2),
            theme::body(),
        ),
        Span::styled(cells.facts.join(" · "), theme::dim()),
    ];
    // Last, so the columns before it never move.
    let used: usize = spans.iter().map(Span::width).sum();
    let room = width.saturating_sub(used + 2); // the 2-space gap
    if let Some(cwd) = &cells.cwd
        && room >= CWD_MIN_W
    {
        spans.push(Span::styled(
            format!("  {}", middle_truncate(cwd, room)),
            theme::faint(),
        ));
    }
    Line::from(spans)
}

// How a delegate row reads: `JobPhase`, the crate's one classification of a
// stored record, plus the two things a TERMINAL adds to it.
//
// The four situations, the word for each, and the band a row sits in all live on
// `JobPhase` in `src/mcp/jobs.rs`, because `clauth jobs` and `monitor`'s listing
// answer the same questions and three copies of one rule is how they drift. What
// stays here is presentation and only presentation: the glyph and the hue. The
// word is mandatory beside the mark either way — three of the four share the `●`
// glyph, so hue alone cannot carry the state.

/// `●` for a run that is or was doing something, `○` for one whose server is
/// gone: the contract's active / disconnected dot pair.
fn state_mark(phase: JobPhase) -> &'static str {
    match phase {
        JobPhase::Running | JobPhase::Blocking | JobPhase::Done => "●",
        JobPhase::Orphaned => "○",
    }
}

fn state_color(phase: JobPhase) -> Color {
    match phase {
        JobPhase::Running | JobPhase::Blocking => theme::accent_color(),
        JobPhase::Done => theme::success_color(),
        // Disconnected carries no semantic charge of its own; the word does.
        JobPhase::Orphaned => theme::text_dim_color(),
    }
}

/// One delegate's row before anything is styled.
///
/// Its liveness figures come from [`jobs::running_liveness`] — the same
/// derivation `monitor`'s running check renders for the calling model — so the
/// operator's row and the model's reply cannot disagree about one record. Pure
/// of the terminal AND of the clock, so a test can drive it at a fixed `now`.
#[derive(Debug, Clone)]
struct DelegateCells {
    state: JobPhase,
    profile: String,
    /// What this record can say, in the order a reader scans it.
    facts: Vec<String>,
    /// The directory the run works in, escaped for display. `None` on a record
    /// an older server wrote.
    cwd: Option<String>,
}

/// One cell set per stored record, in the order they arrive.
///
/// **It sorts nothing.** Banding is `jobs::list_banded`'s, which is what
/// `recompute_services_checks` reads the store through, and what `clauth jobs`
/// and `monitor`'s listing read it through as well. Banding lives there alone:
/// a later change to `list_banded` — a tiebreak, a third band — reaches this
/// pane and the text surfaces together, with nothing to drift.
fn delegate_cells(stored: &[StoredJob], now: u64) -> Vec<DelegateCells> {
    stored.iter().map(|job| delegate_row(job, now)).collect()
}

fn delegate_row(job: &StoredJob, now: u64) -> DelegateCells {
    let record = &job.record;
    let profile = record.profile.clone();
    // The store's own retention stamp, so a row is dated by the same field that
    // decides how long it survives.
    let since = job.age_secs(now);
    // Through `phase()` rather than by re-matching `(liveness, kind)` here: the
    // four situations and any later fifth live on `JobPhase`, so this pane
    // cannot drift its own classification.
    let state = job.phase();
    let (mut facts, live) = match state {
        JobPhase::Done => (vec![format!("finished {}", age_phrase(since))], None),
        JobPhase::Orphaned => (vec![format!("last seen {}", age_phrase(since))], None),
        JobPhase::Running | JobPhase::Blocking => {
            let live = jobs::running_liveness(record, now);
            let elapsed = format!("elapsed {}", humanize_duration(live.elapsed_secs as i64));
            (vec![elapsed], Some(live))
        }
    };
    if let Some(account) = &record.spawned_by {
        facts.push(format!("spawned by {account}"));
    }
    if let Some((label, secs)) = live.as_ref().and_then(next_deadline) {
        facts.push(if secs == 0 {
            format!("{label} now")
        } else {
            format!("{label} in {}", humanize_duration(secs as i64))
        });
    }
    DelegateCells {
        state,
        profile,
        facts,
        // The calling model's own `delegate` argument, so it reaches the
        // terminal only as visible escapes.
        cwd: record.cwd.as_deref().map(escape_control),
    }
}

/// Which kill lands first and how far off it is. `monitor` reports both figures
/// because a model can hold both; a row has width for one, and the one worth the
/// cell is the one that fires.
fn next_deadline(live: &RunningLiveness) -> Option<(&'static str, u64)> {
    match (live.idle_kill_in_secs, live.wall_kill_in_secs) {
        (Some(idle), Some(wall)) if wall <= idle => Some(("wall-kill", wall)),
        (Some(idle), _) => Some(("idle-kill", idle)),
        (None, Some(wall)) => Some(("wall-kill", wall)),
        (None, None) => None,
    }
}

/// A duration rendered as an age.
///
/// Two-unit [`humanize_duration`] rather than `relative_age`'s single unit, and
/// deliberately: every age here is a liveness figure read against a 300 s idle
/// guard, where collapsing everything under a minute to `just now` is the whole
/// signal lost. Zero takes that phrase anyway, because `humanize_duration`
/// spells it `now` and `now ago` is not a thing.
fn age_phrase(secs: u64) -> String {
    if secs == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", humanize_duration(secs as i64))
    }
}

fn health_color(health: Health) -> ratatui::style::Color {
    match health {
        Health::Ok => theme::success_color(),
        Health::Warn => theme::warning_color(),
        Health::Danger => theme::danger_color(),
        // Neutral: a service that is neither running nor healthy — not green.
        Health::Idle => theme::text_dim_color(),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_services.rs"]
mod tests;
