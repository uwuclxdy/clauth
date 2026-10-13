//! Services tab — the shunt gateway, the running delegates, the Claude Code
//! plugin and herdr on one selector, each row's readout on the right. The
//! master-detail mirrors the Status tab's two-pane machinery and counts as 2 of
//! the 3-panel budget.
//!
//! The left panel is one cursor-driven selector over the service rows. Each row
//! is a status dot + label, the verdict in the detail pane, plus a trailing
//! `f` on a row with a fix line. Enter descends into the detail pane; `f`
//! walks to the selected row's next fix line (descending from the list) and
//! `↵` on a focused fix line opens its confirm. All data is recomputed
//! synchronously on tab focus and `r`, the herdr probe excepted: it runs on a
//! worker, its row appears when it lands, and the title spinner shows while it
//! runs.
//!
//! The delegates detail is a descendable job list: `↵` on the row descends when
//! at least one job is listed, ↑↓ walks the jobs, and the selected job expands
//! under itself into its full facts and run directory. Stopping a delegate is
//! `monitor({job_ids, cancel: true})`'s job, so the pane binds no stop key.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, ListItem, Paragraph};

use super::super::app::{
    App, Check, ConfigEntryRef, HERDR_OPTIONS, Health, HerdrOption, InputState, ModelFieldKind,
    OUTDATED_PLUGIN_MARK, RouteFieldKind, SHUNT_OK_MS, ServiceFix, ServicesFocus, ShuntAddKind,
    ShuntConfigRead, ShuntDraftField, ShuntEntryKey, ShuntFieldEdit, ShuntFieldKey, ShuntFocus,
    ShuntOkKey, ShuntSaveKey, UnreadConfig, UpstreamFieldKind, add_row_offered, config_entry_keys,
    config_field_stops, config_view, cycle_options, draft_field_keys, entry_field_keys,
    entry_for_key, entry_kind, entry_name, escape_control, herdr_config_writable, is_cycle_field,
    parse_herdr_tag_secs, removable, shunt_field_value, written_display,
};
use super::super::theme;
use super::format::{middle_truncate, spinner_frame};
use super::header::pulse_name_spans;
use super::modals::danger_button;
use super::panes::{
    bold_when, chunk_graphemes, cycle_row_lines, draw_following_list, draw_scrollbar,
    draw_scrolled_lines, draw_scrolled_lines_with_footnote, empty_state, empty_state_lines,
    footnote_lines, footnote_split, head_cols, help_tooltip_lines, help_tooltip_lines_at,
    highlight_row, invalid_tooltip_lines, key_cell, label_style, master_detail, section_box,
    select_line, value_caret, wrap_words,
};
use super::prose;
use crate::format::truncate;
use crate::gateway_edit::{
    BindOverride, ConfigView, EnvFileFailure, ModelView, ProviderForm, RouteView, ServerView,
    UpstreamView, Written,
};
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
        app.services.selector.offset.set(0);
        app.services.selector.viewport.set(0);
        app.services.selector.starts.replace(Vec::new());
        app.services.selector.block_ends.replace(Vec::new());
        app.services.selector.total.set(0);
        let widget = if app.services.error.is_some() {
            empty_state("check failed", "r", "to retry")
        } else {
            empty_state("no services yet", "r", "to run")
        };
        frame.render_widget(widget, inner);
        return;
    }

    let content_w = inner.width as usize;
    let rows: Vec<ListItem<'static>> = app
        .services
        .checks
        .iter()
        .enumerate()
        .map(|(idx, check)| {
            ListItem::new(selector_row(
                check.health,
                check.label,
                check.fix.is_some(),
                idx == app.services.cursor,
                focused,
                content_w,
            ))
        })
        .collect();
    draw_following_list(
        frame,
        inner,
        rows,
        app.services.cursor,
        &app.services.selector,
    );
}

/// The selector's trailing `f` on a row with a fix line, styled as the key it
/// names; the help modal's legend row draws this same span.
pub(super) fn fix_mark() -> Span<'static> {
    Span::styled(
        "f",
        Style::default()
            .fg(theme::accent_color())
            .add_modifier(Modifier::BOLD),
    )
}

/// The contract's minimum gap between content and a right-aligned column.
const RIGHT_COLUMN_GAP: usize = 3;

/// One selector row: `❯ ● label`, the dot color carrying the verdict and the
/// full readout living in the detail pane. A row whose detail shows a fix
/// line ends in a right-aligned `f` (the key's own ACCENT + bold, cloudy
/// 2026-10-12), kept apart from the dot so health and fixability stay two
/// signals; it drops whole where the 3-cell gap cannot fit. The hover tint
/// spans the full content width when selected (the ratatui filler-tint
/// gotcha); the caret shows only in the focused pane.
fn selector_row(
    health: Health,
    label: &str,
    has_fix: bool,
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
    let used: usize = spans.iter().map(Span::width).sum();
    let mark = has_fix && used + RIGHT_COLUMN_GAP < content_w;
    let pad = content_w.saturating_sub(used + usize::from(mark));
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), with_bg(Style::default())));
    }
    if mark {
        let f = fix_mark();
        let style = with_bg(f.style);
        spans.push(f.style(style));
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
        || app.services.move_plan_probe.running
        || app.services.pool_probe.running
        || app.services.shunt_action.running
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

    // The shunt card renders its action section verbatim with per-line focus —
    // the MOVE PLAN copy holds a literal `: ` that the shared `key: value`
    // splitter would misread.
    if check.label == "shunt" {
        draw_shunt_detail(frame, inner, app, check);
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

    // The plugin detail walks its fixable problems while descended, and the
    // herdr detail's heal line is a walk stop; the focused fix line carries
    // the caret + hover tint.
    let problem_focus = if !focused || app.services.diagnostic_focus || check.problems.is_empty() {
        None
    } else if check.label == "plugin" {
        Some(app.services.problem_cursor)
    } else if check.label == "herdr" && app.services.herdr_fix_focus {
        Some(0)
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
        let fix_focus = problem_focus
            .and_then(|i| check.problems.get(i))
            .map(|p| p.line);
        draw_herdr_detail(frame, inner, app, lines, fix_focus);
        return;
    }

    if check.label == "plugin" && app.services.diagnostic_focus {
        draw_diagnostic_lines(frame, inner, app, lines);
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
        app.services.detail_viewport.set(inner.height);
        app.services.detail_max_scroll.set(
            lines
                .len()
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16,
        );
        let offset = draw_scrolled_lines(frame, inner, lines, (line, line + 1), None);
        app.services.displayed_scroll.set(offset as u16);
        return;
    }

    let total = lines.len();
    let viewport = inner.height as usize;

    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.services.detail_viewport.set(inner.height);
    app.services.detail_max_scroll.set(max_scroll);
    let scroll = app.services.detail_scroll.min(max_scroll);
    app.services.displayed_scroll.set(scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

fn draw_diagnostic_lines(frame: &mut Frame<'_>, inner: Rect, app: &App, lines: Vec<Line<'static>>) {
    let total = lines.len();
    let viewport = inner.height as usize;
    let max = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.services.detail_viewport.set(inner.height);
    app.services.detail_max_scroll.set(max);
    let offset = app.services.detail_scroll.min(max);
    app.services.displayed_scroll.set(offset);
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((offset, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, offset as usize, viewport);
}

/// The `shunt` detail: [`Check::detail`]'s field lines through the shared
/// key:value splitter, then the action section (from `shunt_action_start` on)
/// verbatim — the MOVE PLAN rows and the pool note are prose whose approved
/// copy holds a literal `: `, so they never go through the splitter. The
/// `enabled` row's tier-aware toggle glyph is drawn here off the cached record.
///
/// Descended, ↑↓ walks the card's actionable lines one at a time; the render
/// reads the focused line from the key handler's state and re-settles it (the
/// descend rule) when a resize or a card change pushed the held line out of
/// view, or when focus is none while an actionable line is in view, drawing
/// the caret + hover only while that stop is in view. It publishes the
/// geometry the key handler walks over (each stop's rendered row, the pane
/// height, `detail_max_scroll`).
fn draw_shunt_detail(frame: &mut Frame<'_>, inner: Rect, app: &App, check: &Check) {
    let action_start = check.shunt_action_start.unwrap_or(check.detail.len());
    let width = inner.width as usize;

    // The key column is over the field lines only: the action section is
    // verbatim, so it never contributes a `": "` key.
    let key_w = check
        .detail
        .iter()
        .take(action_start)
        .filter(|line| !line.starts_with("  "))
        .filter_map(|line| line.split_once(": ").map(|(k, _)| k.chars().count()))
        .max()
        .unwrap_or(0)
        .min(18);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // The rendered line each actionable stop lands on, in `check.shunt_focus`
    // order: a `refused`/`pool` sentence above a stop can wrap to several rows,
    // so the in-view test is the rendered index, never the detail index.
    let mut stop_rendered: Vec<usize> = Vec::with_capacity(check.shunt_focus.len());
    let mut move_plan_line: Option<usize> = None;
    for (idx, text) in check.detail.iter().enumerate() {
        if idx < action_start {
            lines.push(detail_line(text, key_w, width));
            continue;
        }
        if let Some(focus) = check.shunt_focus.iter().find(|f| f.line() == Some(idx)) {
            let start = lines.len();
            lines.push(shunt_focus_line(focus, text, false, app));
            stop_rendered.push(start);
        } else {
            if text == "MOVE PLAN" {
                move_plan_line = Some(lines.len());
            }
            lines.extend(shunt_action_lines(text, width));
        }
    }

    // The config section: one blank line after the card's last line, then the
    // SERVER / UPSTREAMS / MODELS / ROUTES / CLIENT TOKENS groups (or one
    // `config  not read` line). Entry rows join `stop_rendered` in
    // `check.shunt_focus` order; `eyebrows` records each group header's line
    // for the underline pass.
    let mut eyebrows: [Option<usize>; 5] = [None; 5];
    match &app.services.shunt_config {
        Some(ShuntConfigRead::Ok(view)) => {
            lines.push(Line::from(""));
            if let BindOverride::Unknown(reason) = &view.server.bind_override {
                lines.push(env_file_not_read_line(*reason));
            }
            render_shunt_config(
                &mut lines,
                &mut stop_rendered,
                &mut eyebrows,
                view,
                &app.services.shunt_expanded,
                app,
                width,
            );
        }
        Some(ShuntConfigRead::Unread(unread)) => {
            lines.push(Line::from(""));
            lines.push(config_not_read_line(*unread));
        }
        Some(ShuntConfigRead::Refused(sentence)) => {
            lines.push(Line::from(""));
            lines.push(config_not_editable_line());
            lines.extend(help_tooltip_lines_at(sentence, width, CARET_GUTTER));
        }
        None => {}
    }

    let total = lines.len();
    let viewport = inner.height as usize;
    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    app.services.detail_max_scroll.set(max_scroll);
    let scroll = app.services.detail_scroll.min(max_scroll);

    // The focused actionable line. The render re-settles the key handler's
    // identity (the descend rule — the first actionable line in view, else
    // none) here, where the geometry is published, when a resize or a card
    // change pushed the held line out of view, or when focus is none while a
    // stop is in view; the caret, the footer and every acting key then see the
    // re-settled line the same frame. The caret and the bold promotion show
    // only while the detail pane itself holds focus; blurred (the list focused)
    // the card keeps the line's hover tint and drops both.
    let detail_focused = app.services.focus == ServicesFocus::Detail;
    app.services
        .re_settle_shunt_focus(&check.shunt_focus, &stop_rendered, scroll, viewport);
    let focus_stop = app
        .services
        .shunt_focus_kind()
        .and_then(|kind| check.shunt_focus.iter().position(|s| s.kind() == kind))
        .filter(|&i| {
            stop_rendered.get(i).is_some_and(|&rendered| {
                rendered >= scroll as usize && rendered < scroll as usize + viewport
            })
        });

    // The MOVE PLAN eyebrow underlines while the focus rests on its one
    // actionable line (`f move stores in`) and the detail pane holds focus,
    // the herdr OPTIONS section's rule; blurred, only the hover tint stays.
    if let Some(idx) = move_plan_line {
        let underlined = detail_focused
            && focus_stop.is_some_and(|i| {
                matches!(
                    &check.shunt_focus[i],
                    ShuntFocus::Fix {
                        fix: ServiceFix::MoveStoresIn,
                        ..
                    }
                )
            });
        let style = if underlined {
            theme::label().underlined()
        } else {
            theme::label()
        };
        lines[idx] = Line::from(Span::styled("MOVE PLAN", style));
    }

    // A config group's eyebrow underlines while the focused entry or field sits
    // in that group and the detail pane holds focus (the herdr OPTIONS
    // section's rule).
    let focused_group = focus_stop.and_then(|i| match &check.shunt_focus[i] {
        ShuntFocus::Entry { key } => Some(entry_group(key)),
        ShuntFocus::Field { key } => Some(field_group(key)),
        ShuntFocus::Add { kind } | ShuntFocus::SaveAdd { kind } => Some(kind.group()),
        ShuntFocus::Remove { key } => Some(entry_group(key)),
        ShuntFocus::Token { .. } | ShuntFocus::AddToken => Some(4),
        _ => None,
    });
    if detail_focused
        && let Some(group) = focused_group
        && let Some(line) = eyebrows[group]
    {
        lines[line] = Line::from(Span::styled(
            eyebrow_name(group),
            theme::label().underlined(),
        ));
    }

    // The field whose typed editor is open, for the native cursor (published
    // only when its row is in view).
    let mut edit_caret: Option<(usize, usize)> = None;
    if let Some(idx) = focus_stop {
        let focus = &check.shunt_focus[idx];
        let rendered = stop_rendered.get(idx).copied();
        match focus {
            ShuntFocus::Entry { key } => {
                let expanded = app.services.shunt_expanded.contains(key);
                let Some(view) = config_view(app) else {
                    unreachable!("an entry stop renders only over a read config");
                };
                if let Some(rendered) = rendered {
                    lines[rendered] = highlight_row(
                        entry_line(view, key, expanded, detail_focused, app, width),
                        width,
                    );
                }
            }
            ShuntFocus::Field { key } => {
                let Some(view) = config_view(app) else {
                    unreachable!("a field stop renders only over a read config");
                };
                let caret = field_row_caret(app, key);
                // Replace every rendered line of the focused field with its
                // selected render, each tinted across the full width: a wrapped
                // cycle run keeps the focus bracket and the `BG_HOVER` tint on
                // every line, not just the first. A refusal's `└` lines trail
                // the field's own row and stay untinted, like every other
                // tooltip in the app.
                let rows = field_row_lines(view, key, detail_focused, app, width);
                let refusal = app
                    .services
                    .shunt_field_edit
                    .as_ref()
                    .filter(|s| s.key == *key)
                    .is_some_and(|s| s.refusal.is_some());
                if let Some(rendered) = rendered {
                    if let Some(caret) = caret {
                        edit_caret = Some((rendered, caret));
                    }
                    for (i, row) in rows.into_iter().enumerate() {
                        if rendered + i < lines.len() {
                            lines[rendered + i] = if i == 0 || !refusal {
                                highlight_row(row, width)
                            } else {
                                row
                            };
                        }
                    }
                }
            }
            ShuntFocus::Add { kind } => {
                if let Some(rendered) = rendered {
                    lines[rendered] = highlight_row(add_line(*kind, detail_focused, app), width);
                }
            }
            ShuntFocus::SaveAdd { kind } => {
                let rows = save_add_lines(*kind, detail_focused, app, width);
                if let Some(rendered) = rendered {
                    for (i, row) in rows.into_iter().enumerate() {
                        if rendered + i < lines.len() {
                            lines[rendered + i] = if i == 0 {
                                highlight_row(row, width)
                            } else {
                                row
                            };
                        }
                    }
                }
            }
            ShuntFocus::Remove { key } => {
                let rows = remove_lines(key, detail_focused, app, width);
                if let Some(rendered) = rendered {
                    for (i, row) in rows.into_iter().enumerate() {
                        if rendered + i < lines.len() {
                            lines[rendered + i] = if i == 0 {
                                highlight_row(row, width)
                            } else {
                                row
                            };
                        }
                    }
                }
            }
            ShuntFocus::Token { key } => {
                // A token row is an action-chip row: the caret + inverse
                // `remove` block carry focus, no `BG_HOVER` row tint, and a
                // refusal's `└` line trails untinted.
                let rows = token_row_lines(key, detail_focused, app, width);
                if let Some(rendered) = rendered {
                    for (i, row) in rows.into_iter().enumerate() {
                        if rendered + i < lines.len() {
                            lines[rendered + i] = row;
                        }
                    }
                }
            }
            ShuntFocus::AddToken => {
                if let Some(rendered) = rendered {
                    lines[rendered] = highlight_row(add_token_line(detail_focused, app), width);
                }
            }
            _ => {
                let Some(line) = focus.line() else {
                    unreachable!("only a config row lacks a detail line");
                };
                if let Some(rendered) = rendered {
                    lines[rendered] = highlight_row(
                        shunt_focus_line(focus, &check.detail[line], detail_focused, app),
                        width,
                    );
                }
            }
        }
    }

    // Publish the geometry the key handler walks over (after the highlight
    // borrows it), exactly like `detail_max_scroll`.
    app.services.shunt_viewport.set(inner.height);
    app.services.shunt_stop_rows.replace(stop_rendered);

    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);

    // The native terminal cursor at the typed editor's caret (the Setup
    // editor's rule): the value column plus the caret's char column, on the
    // row's visible line.
    if let Some((row, caret)) = edit_caret
        && let Some(visible) = row.checked_sub(scroll as usize).filter(|v| *v < viewport)
    {
        frame.set_cursor_position((
            inner.x.saturating_add((CONFIG_VALUE_COL + caret) as u16),
            inner.y.saturating_add(visible as u16),
        ));
    }
}

/// The group index of one entry kind, in the config section's order
/// (SERVER, UPSTREAMS, MODELS, ROUTES).
fn entry_group(key: &ShuntEntryKey) -> usize {
    match key {
        ShuntEntryKey::Upstream { .. } => 1,
        ShuntEntryKey::Model { .. } => 2,
        ShuntEntryKey::Route(_) => 3,
    }
}

/// The group index of one field row, in the same order.
fn field_group(key: &ShuntFieldKey) -> usize {
    match key {
        ShuntFieldKey::Bind | ShuntFieldKey::DefaultProvider => 0,
        ShuntFieldKey::Upstream { .. } => 1,
        ShuntFieldKey::Model { .. } => 2,
        ShuntFieldKey::Route { .. } => 3,
        ShuntFieldKey::Draft { kind, .. } => kind.group(),
    }
}

/// The group eyebrow word for a group index (SERVER, UPSTREAMS, MODELS,
/// ROUTES, CLIENT TOKENS).
fn eyebrow_name(group: usize) -> &'static str {
    match group {
        1 => "UPSTREAMS",
        2 => "MODELS",
        3 => "ROUTES",
        4 => "CLIENT TOKENS",
        _ => "SERVER",
    }
}

/// The config section's render state: the rows built so far, the published
/// focus-stop rows (parallel to [`Check::shunt_focus`]), and the one-row-model
/// field stops every focusable row's membership is checked against.
struct ConfigRows<'a> {
    lines: &'a mut Vec<Line<'static>>,
    stop_rendered: &'a mut Vec<usize>,
    field_stops: &'a [ShuntFocus],
}

/// The config groups, after the card's existing lines and one blank line: the
/// SERVER field rows, then the UPSTREAMS / MODELS / ROUTES disclosure rows,
/// each entry expanding in place, then the CLIENT TOKENS group. Field rows and
/// token rows join the focus walk (in [`config_field_stops`]'s order) and take
/// the caret; the bind row is read-only while overridden or the env file is
/// unread, the legacy `providers` row, its note, the eyebrows and the unmanaged
/// token rows never take focus. The entries, fields and token rows derive from
/// [`config_entry_keys`] / [`entry_field_keys`] / [`draft_field_keys`] /
/// [`config_field_stops`] — the same row model the focus walk builds its stops
/// from — so the two cannot disagree for any view.
fn render_shunt_config(
    lines: &mut Vec<Line<'static>>,
    stop_rendered: &mut Vec<usize>,
    eyebrows: &mut [Option<usize>; 5],
    view: &ConfigView,
    expanded: &HashSet<ShuntEntryKey>,
    app: &App,
    width: usize,
) {
    let keys = config_entry_keys(view);
    let drafts = &app.services.shunt_drafts;
    // The one row model both the render and the focus walk derive from: a
    // field's focusability is `config_field_stops`' membership, never a
    // predicate copied beside it.
    let field_stops = config_field_stops(view, expanded, drafts, &app.services.shunt_tokens);
    let mut rows = ConfigRows {
        lines,
        stop_rendered,
        field_stops: &field_stops,
    };

    eyebrows[0] = Some(rows.lines.len());
    rows.lines
        .push(Line::from(Span::styled("SERVER", theme::label())));
    push_field(&mut rows, view, &ShuntFieldKey::Bind, app, width);
    push_field(&mut rows, view, &ShuntFieldKey::DefaultProvider, app, width);

    eyebrows[1] = Some(rows.lines.len());
    rows.lines
        .push(Line::from(Span::styled("UPSTREAMS", theme::label())));
    match &view.form {
        ProviderForm::Upstreams => {
            for key in keys
                .iter()
                .filter(|k| matches!(k, ShuntEntryKey::Upstream { .. }))
            {
                push_entry(&mut rows, view, key, expanded, app, width);
            }
        }
        ProviderForm::Legacy { providers } => {
            rows.lines.push(providers_field_line(providers, width));
            rows.lines.extend(legacy_note_lines(width));
        }
    }
    push_add(&mut rows, view, ShuntAddKind::Upstream, app, width);

    eyebrows[2] = Some(rows.lines.len());
    rows.lines
        .push(Line::from(Span::styled("MODELS", theme::label())));
    for key in keys
        .iter()
        .filter(|k| matches!(k, ShuntEntryKey::Model { .. }))
    {
        push_entry(&mut rows, view, key, expanded, app, width);
    }
    push_add(&mut rows, view, ShuntAddKind::Model, app, width);

    eyebrows[3] = Some(rows.lines.len());
    rows.lines
        .push(Line::from(Span::styled("ROUTES", theme::label())));
    for key in keys.iter().filter(|k| matches!(k, ShuntEntryKey::Route(_))) {
        push_entry(&mut rows, view, key, expanded, app, width);
    }
    push_add(&mut rows, view, ShuntAddKind::Route, app, width);

    eyebrows[4] = Some(rows.lines.len());
    rows.lines
        .push(Line::from(Span::styled("CLIENT TOKENS", theme::label())));
    push_token_rows(&mut rows, app, width);
}

/// The `+ add <kind>` row ending its group (a first-class `+ add` row, `ACCENT`
/// and bold while selected), plus, while its draft is open, the blank field
/// rows and the `save <kind>` row. Nothing renders where the group is not
/// editable (no `+ add upstream` on a legacy `[providers.*]` config).
fn push_add(
    rows: &mut ConfigRows<'_>,
    view: &ConfigView,
    kind: ShuntAddKind,
    app: &App,
    width: usize,
) {
    if !add_row_offered(view, kind) {
        return;
    }
    let start = rows.lines.len();
    rows.lines.push(add_line(kind, false, app));
    rows.stop_rendered.push(start);
    if app.services.shunt_drafts.is_open(kind) {
        for field in draft_field_keys(view, kind, &app.services.shunt_drafts) {
            push_field(rows, view, &field, app, width);
        }
        let save_start = rows.lines.len();
        rows.lines.extend(save_add_lines(kind, false, app, width));
        rows.stop_rendered.push(save_start);
    }
}

/// The `CLIENT TOKENS` group's rows: one action-chip row per stored token (the
/// profile's name, chip `remove`), the unmanaged token names after them (faint,
/// never a walk stop) or the `tokens  not read` / `other clients  not read` /
/// `<variable>  not parsed` faint line (a read failure's sentence wrapped on a
/// `└` line under the first two), then `+ add token` (a walk stop). The stored
/// rows and `+ add token` derive from [`config_field_stops`] — the same row
/// model the focus walk builds its stops from.
fn push_token_rows(rows: &mut ConfigRows<'_>, app: &App, width: usize) {
    let tokens = &app.services.shunt_tokens;
    if let Some(reason) = &tokens.error {
        rows.lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("tokens  not read", theme::faint()),
        ]));
        rows.lines.extend(help_tooltip_lines_at(
            &escape_control(reason),
            width,
            CARET_GUTTER,
        ));
    } else {
        for name in &tokens.stored {
            let start = rows.lines.len();
            rows.lines.extend(token_row_lines(name, false, app, width));
            rows.stop_rendered.push(start);
        }
        if let Some(reason) = &tokens.unmanaged_error {
            // The store read fine, so its rows stay; only the unmanaged names
            // are hidden behind one `other clients  not read` line.
            rows.lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("other clients  not read", theme::faint()),
            ]));
            rows.lines.extend(help_tooltip_lines_at(
                &escape_control(reason),
                width,
                CARET_GUTTER,
            ));
        } else {
            for (name, source) in &tokens.unmanaged {
                rows.lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(escape_control(name).to_string(), theme::faint()),
                    Span::styled("  ", Style::default()),
                    Span::styled(source.word(), theme::dim()),
                ]));
            }
            if let Some((variable, _)) = &tokens.unparsed {
                rows.lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{}  not parsed", escape_control(variable)),
                        theme::faint(),
                    ),
                ]));
            }
        }
    }
    let add_start = rows.lines.len();
    rows.lines.push(add_token_line(false, app));
    rows.stop_rendered.push(add_start);
}

/// One stored token's action-chip row: the caret gutter, the profile name (the
/// label), then the `remove` chip — `DANGER` blurred, a `DANGER` inverse block
/// focused, the spinner in the caret slot and a dim `checking` trail while its
/// remove checks. A refusal (the last-token guard) renders on the `└` line
/// under it. The label stays `TEXT_DIM` even focused; the caret and the chip
/// carry focus alone (no `BG_HOVER` row tint).
fn token_row_lines(name: &str, selected: bool, app: &App, width: usize) -> Vec<Line<'static>> {
    let saving = app.services.saving(&ShuntSaveKey::Token(name.to_string()));
    let ok = app.services.shunt_ok.as_ref().is_some_and(|(k, at)| {
        matches!(k, ShuntOkKey::Token(held) if held == name)
            && at.elapsed().as_millis() < SHUNT_OK_MS as u128
    });
    let caret = if ok {
        Span::styled("✓ ", theme::success())
    } else if saving {
        Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        )
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::styled("  ", Style::default())
    };
    let mut spans = vec![
        caret,
        Span::styled(escape_control(name).to_string(), theme::dim()),
        Span::raw("  "),
    ];
    if saving {
        spans.push(Span::styled("checking", theme::dim()));
    } else {
        spans.push(danger_button(" remove ", selected));
    }
    let mut rows = vec![Line::from(spans)];
    if let Some((profile, sentence)) = app.services.shunt_token_refusal.borrow().as_ref()
        && profile == name
    {
        rows.extend(invalid_tooltip_lines(sentence, width));
    }
    rows
}

/// The `+ add token` row: a first-class `+ add` row — `ACCENT` label, bold
/// while selected, the spinner in the caret slot while its add checks.
fn add_token_line(selected: bool, app: &App) -> Line<'static> {
    let saving = app.services.saving(&ShuntSaveKey::AddToken);
    let caret = if saving {
        Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        )
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::styled("  ", Style::default())
    };
    let label = if selected {
        theme::accent().bold()
    } else {
        theme::accent()
    };
    Line::from(vec![caret, Span::styled("+ add token", label)])
}

/// Draw one entry row (and its expanded fields when expanded), publishing each
/// focusable row for the focus walk's in-view test.
fn push_entry(
    rows: &mut ConfigRows<'_>,
    view: &ConfigView,
    key: &ShuntEntryKey,
    expanded: &HashSet<ShuntEntryKey>,
    app: &App,
    width: usize,
) {
    let start = rows.lines.len();
    rows.lines.push(entry_line(
        view,
        key,
        expanded.contains(key),
        false,
        app,
        width,
    ));
    rows.stop_rendered.push(start);
    if expanded.contains(key) {
        for field in entry_field_keys(view, key) {
            push_field(rows, view, &field, app, width);
        }
        // The entry's last child: the `remove <kind>` row (absent where the
        // engine's `Remove*` cannot address the entry — a duplicated name/id).
        if removable(view, key) {
            let remove_start = rows.lines.len();
            rows.lines.extend(remove_lines(key, false, app, width));
            rows.stop_rendered.push(remove_start);
        }
    }
}

/// Draw one field row (and, under an edited field, its refusal `└` line). A
/// focusable field publishes its rendered row for the focus walk's in-view
/// test; a read-only one (the bind row while overridden or the env file is
/// unread) draws but never joins the stops — focusability is
/// [`config_field_stops`]' membership, the one row model.
fn push_field(
    rows: &mut ConfigRows<'_>,
    view: &ConfigView,
    key: &ShuntFieldKey,
    app: &App,
    width: usize,
) {
    let start = rows.lines.len();
    let field_rows = field_row_lines(view, key, false, app, width);
    rows.lines.extend(field_rows);
    if rows
        .field_stops
        .contains(&ShuntFocus::Field { key: key.clone() })
    {
        rows.stop_rendered.push(start);
    }
}

/// One config entry row (an upstream, model or route): the caret gutter, the
/// `▶`/`▼` disclosure glyph, the name/id (the row's label) padded to the shared
/// value column, then the dim summary. The glyph is `TEXT_DIM` collapsed,
/// `ACCENT` expanded; only the label promotes to `TEXT + bold` on focus. A
/// route's whole label is one label run; names and summaries trail-ellipsize.
fn entry_line(
    view: &ConfigView,
    key: &ShuntEntryKey,
    expanded: bool,
    selected: bool,
    app: &App,
    width: usize,
) -> Line<'static> {
    let ok = app.services.shunt_ok.as_ref().is_some_and(|(k, at)| {
        matches!(k, ShuntOkKey::Entry(entry) if entry == key)
            && at.elapsed().as_millis() < SHUNT_OK_MS as u128
    });
    let mut spans = entry_lead(expanded, selected, ok);
    match entry_for_key(view, key) {
        Some(ConfigEntryRef::Upstream(u)) => {
            spans.push(entry_label(u.name.as_ref(), selected));
            if let Some(summary) = upstream_summary(u) {
                spans.push(Span::styled(
                    truncate(&summary, width.saturating_sub(CONFIG_VALUE_COL)),
                    theme::dim(),
                ));
            }
        }
        Some(ConfigEntryRef::Model(m)) => {
            spans.push(entry_label(m.id.as_ref(), selected));
            if let Some(summary) = model_summary(m) {
                spans.push(Span::styled(
                    truncate(&summary, width.saturating_sub(CONFIG_VALUE_COL)),
                    theme::dim(),
                ));
            }
        }
        Some(ConfigEntryRef::Route(r)) => {
            spans.push(Span::styled(
                route_label(r, width.saturating_sub(ENTRY_INDENT)),
                label_style(selected),
            ));
        }
        None => {}
    }
    Line::from(spans)
}

/// A route row's whole label, one label run: `<position>  <model> → <provider>`,
/// an absent side reading `—`. The position, the arrow and the provider stay
/// whole; the model slug alone shrinks with a trailing `…`. A provider that
/// leaves the model no room shrinks too, keeping one cell for the model.
fn route_label(r: &RouteView, room: usize) -> String {
    let model = escape_control(
        r.model
            .as_ref()
            .map(written_display)
            .as_deref()
            .unwrap_or("—"),
    );
    let provider = escape_control(
        r.provider
            .as_ref()
            .map(written_display)
            .as_deref()
            .unwrap_or("—"),
    );
    let head = format!("{}  ", r.position);
    let arrow = " → ";
    let head_arrow = head.chars().count() + arrow.chars().count();
    // The model's room after the position, the arrow and the whole provider.
    let model_room = room.saturating_sub(head_arrow + provider.chars().count());
    if model.chars().count() <= model_room {
        return format!("{head}{model}{arrow}{provider}");
    }
    if model_room > 0 {
        return format!("{head}{}{arrow}{provider}", truncate(&model, model_room));
    }
    // The provider leaves the model no room: the model keeps one cell (`…` for
    // a cut slug, `—` for an absent one) and the provider gives way.
    format!(
        "{head}{}{arrow}{}",
        truncate(&model, 1),
        truncate(&provider, room.saturating_sub(head_arrow + 1))
    )
}

/// The shared caret + disclosure glyph lead of an entry row. The `✓` (an add's
/// success, `SHUNT_OK_MS`) replaces the caret the way a field row's does.
fn entry_lead(expanded: bool, selected: bool, ok: bool) -> Vec<Span<'static>> {
    let caret = if ok {
        Span::styled("✓ ", theme::success())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::styled("  ", Style::default())
    };
    let glyph = if expanded { "▼" } else { "▶" };
    let glyph_style = if expanded {
        theme::accent()
    } else {
        theme::dim()
    };
    vec![caret, Span::styled(format!("{glyph} "), glyph_style)]
}

/// The caret slot of an action row (a `+ add`, `save <kind>` or `remove
/// <kind>` row): the braille spinner while its save runs, else `❯` focused /
/// blank.
fn action_lead(saving: bool, selected: bool, app: &App) -> Span<'static> {
    if saving {
        Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        )
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::styled("  ", Style::default())
    }
}

/// The `+ add <kind>` row: a first-class `+ add` row — `ACCENT` label, bold
/// while selected (the cloudy-tui Master-detail `+ new` rule), no disclosure
/// glyph.
fn add_line(kind: ShuntAddKind, selected: bool, app: &App) -> Line<'static> {
    let label = if selected {
        theme::accent().bold()
    } else {
        theme::accent()
    };
    Line::from(vec![
        action_lead(false, selected, app),
        Span::styled(kind.add_label(), label),
    ])
}

/// The `save <kind>` row (an action line, indented under the `+ add` row at
/// the entry-child indent), plus its refusal `└` line (a required field left
/// blank, or the engine's own sentence): the caret slot shows the spinner and
/// the label the shimmer crest while its save checks. At rest it takes the
/// `create account` treatment (ACCENT, bold while selected).
fn save_add_lines(
    kind: ShuntAddKind,
    selected: bool,
    app: &App,
    width: usize,
) -> Vec<Line<'static>> {
    let saving = app.services.saving(&ShuntSaveKey::SaveAdd(kind));
    let mut spans = vec![
        action_lead(saving, selected, app),
        Span::raw(" ".repeat(ENTRY_INDENT.saturating_sub(CARET_GUTTER))),
    ];
    if saving {
        let elapsed = app
            .services
            .shunt_save
            .as_ref()
            .map(|save| save.started.elapsed().as_millis() as u64)
            .unwrap_or(0);
        spans.extend(pulse_name_spans(
            &kind.save_label(),
            theme::accent(),
            elapsed,
        ));
        spans.push(Span::styled("  checking", theme::dim()));
    } else {
        spans.push(Span::styled(
            kind.save_label(),
            bold_when(theme::accent(), selected),
        ));
    }
    let mut rows = vec![Line::from(spans)];
    if let Some(refusal) = app.services.shunt_drafts.refusal(kind) {
        rows.extend(invalid_tooltip_lines(refusal, width));
    }
    rows
}

/// The `remove <kind>` row ending an expanded entry (an action line, indented
/// under the entry's name like its field rows), plus its refusal `└` line (an
/// unowned comment in a gap the remove touches). At rest it takes the `delete
/// account` treatment (`DANGER`, bold); while its remove checks it shows the
/// same saving shape as `save <kind>` — the spinner, the shimmer crest over
/// its label and a dim `checking` trail.
fn remove_lines(
    key: &ShuntEntryKey,
    selected: bool,
    app: &App,
    width: usize,
) -> Vec<Line<'static>> {
    let saving = app.services.saving(&ShuntSaveKey::Remove(key.clone()));
    let mut spans = vec![
        action_lead(saving, selected, app),
        Span::raw(" ".repeat(ENTRY_INDENT.saturating_sub(CARET_GUTTER))),
    ];
    if saving {
        let elapsed = app
            .services
            .shunt_save
            .as_ref()
            .map(|save| save.started.elapsed().as_millis() as u64)
            .unwrap_or(0);
        spans.extend(pulse_name_spans(
            &entry_kind(key).remove_label(),
            theme::accent(),
            elapsed,
        ));
        spans.push(Span::styled("  checking", theme::dim()));
    } else {
        spans.push(Span::styled(
            entry_kind(key).remove_label(),
            theme::danger().bold(),
        ));
    }
    let mut rows = vec![Line::from(spans)];
    if let Some(refusal) = app.services.shunt_remove_refusal.borrow().as_ref()
        && refusal.key == *key
    {
        rows.extend(invalid_tooltip_lines(&refusal.sentence, width));
    }
    rows
}

/// An entry's name/id, escaped and padded to the shared value column, in the
/// row's label style.
fn entry_label(w: Option<&Written>, selected: bool) -> Span<'static> {
    Span::styled(
        config_key(&entry_name(w), ENTRY_INDENT),
        label_style(selected),
    )
}

/// The dim summary of a collapsed upstream: the auth mode, plus the account
/// count (` · N accounts`, singular at one) when the auth carries `accounts`
/// or `account`. No auth, no summary.
fn upstream_summary(u: &UpstreamView) -> Option<String> {
    let Some(auth) = &u.auth else {
        return None;
    };
    let mode = auth
        .mode
        .as_ref()
        .map(|w| escape_control(&w.value.clone().unwrap_or_else(|| w.raw.clone())));
    let count = match &auth.accounts {
        Some(accounts) => Some(accounts.len()),
        None if auth.account.is_some() => Some(1),
        None => None,
    };
    let mut parts = Vec::new();
    if let Some(mode) = mode {
        parts.push(mode);
    }
    if let Some(n) = count {
        parts.push(if n == 1 {
            "1 account".to_string()
        } else {
            format!("{n} accounts")
        });
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// The dim summary of a collapsed model: `<N> upstreams` (singular at one),
/// none when `upstream_model` is absent or empty.
fn model_summary(m: &ModelView) -> Option<String> {
    let n = m.upstream_model.as_ref().map(Vec::len).filter(|&n| n > 0)?;
    Some(if n == 1 {
        "1 upstream".to_string()
    } else {
        format!("{n} upstreams")
    })
}

/// The caret gutter every focusable config row carries; the entry rows'
/// disclosure glyph occupies it with its trailing space, so the name opens at
/// [`ENTRY_INDENT`].
const CARET_GUTTER: usize = 2;
/// The children of an expanded entry indented under its label: the caret gutter
/// (2) + the disclosure glyph and its space (2).
const ENTRY_INDENT: usize = 4;
/// The shared column every config value and collapsed summary opens at, read
/// relative to the pane's content edge.
const CONFIG_VALUE_COL: usize = 20;
/// The fixed gap between a padded label and its value.
const CONFIG_KEY_GUTTER: usize = 2;

/// A label padded so its value opens at [`CONFIG_VALUE_COL`], trailing `…` when
/// it overruns its slot. `indent` is the column the label starts at.
fn config_key(label: &str, indent: usize) -> String {
    let width = CONFIG_VALUE_COL
        .saturating_sub(indent)
        .saturating_sub(CONFIG_KEY_GUTTER);
    let label = truncate(&escape_control(label), width);
    format!("{label:<width$}{}", " ".repeat(CONFIG_KEY_GUTTER))
}

/// A field row's label word: the provider key for an `upstream_model` row.
fn field_label(key: &ShuntFieldKey) -> String {
    match key {
        ShuntFieldKey::Bind => "bind".to_string(),
        ShuntFieldKey::DefaultProvider => "default_provider".to_string(),
        ShuntFieldKey::Upstream { field, .. } => match field {
            UpstreamFieldKind::Name => "name",
            UpstreamFieldKind::Provider => "provider",
            UpstreamFieldKind::Kind => "kind",
            UpstreamFieldKind::BaseUrl => "base_url",
            UpstreamFieldKind::AuthMode => "auth mode",
            UpstreamFieldKind::AuthAccount => "account",
            UpstreamFieldKind::Accounts => "accounts",
            UpstreamFieldKind::AuthEnv => "auth env",
            UpstreamFieldKind::AuthHeader => "auth header",
            UpstreamFieldKind::Effort => "effort",
            UpstreamFieldKind::ServiceTier => "service_tier",
        }
        .to_string(),
        ShuntFieldKey::Model { field, .. } => match field {
            ModelFieldKind::Id => "id".to_string(),
            ModelFieldKind::DisplayName => "display_name".to_string(),
            ModelFieldKind::UpstreamModel(provider) => provider.clone(),
        },
        ShuntFieldKey::Route { field, .. } => match field {
            RouteFieldKind::Model => "model",
            RouteFieldKind::Provider => "provider",
            RouteFieldKind::UpstreamModel => "upstream_model",
            RouteFieldKind::Effort => "effort",
            RouteFieldKind::ServiceTier => "service_tier",
        }
        .to_string(),
        ShuntFieldKey::Draft { field, .. } => match field {
            ShuntDraftField::Upstream(field) => match field {
                UpstreamFieldKind::Name => "name",
                UpstreamFieldKind::Provider => "provider",
                UpstreamFieldKind::Kind => "kind",
                UpstreamFieldKind::BaseUrl => "base_url",
                UpstreamFieldKind::AuthMode => "auth mode",
                UpstreamFieldKind::AuthAccount => "account",
                UpstreamFieldKind::Accounts => "accounts",
                UpstreamFieldKind::AuthEnv => "auth env",
                UpstreamFieldKind::AuthHeader => "auth header",
                UpstreamFieldKind::Effort => "effort",
                UpstreamFieldKind::ServiceTier => "service_tier",
            }
            .to_string(),
            ShuntDraftField::Model(field) => match field {
                ModelFieldKind::Id => "id".to_string(),
                ModelFieldKind::DisplayName => "display_name".to_string(),
                ModelFieldKind::UpstreamModel(provider) => provider.clone(),
            },
            ShuntDraftField::Route(field) => match field {
                RouteFieldKind::Model => "model",
                RouteFieldKind::Provider => "provider",
                RouteFieldKind::UpstreamModel => "upstream_model",
                RouteFieldKind::Effort => "effort",
                RouteFieldKind::ServiceTier => "service_tier",
            }
            .to_string(),
        },
    }
}

/// A field row's indent: the server fields sit at the card gutter; a draft
/// field and an entry's field sit under their parent's label (`ENTRY_INDENT`,
/// two cells deeper than the `+ add` row's label), values at col 20.
fn field_indent(key: &ShuntFieldKey) -> usize {
    match key {
        ShuntFieldKey::Bind | ShuntFieldKey::DefaultProvider => CARET_GUTTER,
        _ => ENTRY_INDENT,
    }
}

/// The caret slot of a field row: `✎` while its editor is open (a pending pick
/// included), the braille spinner (`ACCENT`) while its save runs, `✓` (`SUCCESS`)
/// for about a second after it lands, else `❯` focused / blank.
fn field_lead(key: &ShuntFieldKey, selected: bool, app: &App) -> Span<'static> {
    let editing = app
        .services
        .shunt_field_edit
        .as_ref()
        .is_some_and(|s| s.key == *key);
    let saving = app.services.saving(&ShuntSaveKey::Field(key.clone()));
    let ok = app.services.shunt_ok.as_ref().is_some_and(|(k, at)| {
        matches!(k, ShuntOkKey::Field(field) if field == key)
            && at.elapsed().as_millis() < SHUNT_OK_MS as u128
    });
    if saving {
        Span::styled(
            format!("{} ", spinner_frame(app.tick_count)),
            theme::accent(),
        )
    } else if editing {
        Span::styled("✎ ", theme::accent().bold())
    } else if ok {
        Span::styled("✓ ", theme::success())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::styled("  ", Style::default())
    }
}

/// The typed editor's caret char column in the value (the native cursor), when
/// the field's typed editor is open; `None` for a pending pick, a saving field
/// (the row shows the spinner, not `✎`), or no editor.
fn field_row_caret(app: &App, key: &ShuntFieldKey) -> Option<usize> {
    let state = app.services.shunt_field_edit.as_ref()?;
    if state.key != *key {
        return None;
    }
    if app.services.saving(&ShuntSaveKey::Field(key.clone())) {
        return None;
    }
    match &state.edit {
        ShuntFieldEdit::Typed { input, .. } => Some(head_cols(input)),
        ShuntFieldEdit::Pick { .. } => None,
    }
}

/// The bind row's read-only suffix: `set by <variable>` while overridden,
/// `env file not read` when the env file failed, none otherwise.
fn bind_suffix(server: &ServerView) -> Option<String> {
    match &server.bind_override {
        BindOverride::Set(variable) => Some(format!("set by {variable}")),
        BindOverride::Unknown(_) => Some("env file not read".to_string()),
        BindOverride::None => None,
    }
}

/// One field row (plus, under an edited field, its refusal `└` line): the caret
/// slot, the label, then the value — the typed buffer on `bg_sunken` while its
/// editor is open, a cycle run at rest or over a pending pick, and while the
/// save runs a shimmer crest over the value with a dim `checking` trail.
fn field_row_lines(
    view: &ConfigView,
    key: &ShuntFieldKey,
    selected: bool,
    app: &App,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = field_indent(key);
    let label = field_label(key);
    let edit_state = app
        .services
        .shunt_field_edit
        .as_ref()
        .filter(|s| s.key == *key);
    let saving = app.services.saving(&ShuntSaveKey::Field(key.clone()));
    let value = shunt_field_value(view, &app.services.shunt_drafts, key).flatten();
    let room = width.saturating_sub(CONFIG_VALUE_COL);

    let lead = || {
        vec![
            field_lead(key, selected, app),
            Span::styled(
                // The caret slot already covers the gutter's first two cells:
                // the label opens at `indent` (2 server, 4 entry).
                format!(
                    "{}{}",
                    " ".repeat(indent.saturating_sub(CARET_GUTTER)),
                    config_key(&label, indent)
                ),
                label_style(selected),
            ),
        ]
    };

    // While a save runs the row shows the saving state — the spinner (already
    // in the caret slot), the value being saved under the shimmer crest and a
    // dim `checking` trail — typed and cycle rows alike. The editor stays open
    // with the typed text, but the row shows the value the save checks.
    if saving {
        let Some(save) = app
            .services
            .shunt_save
            .as_ref()
            .filter(|s| s.key == ShuntSaveKey::Field(key.clone()))
        else {
            unreachable!("saving implies a save in flight");
        };
        let mut spans = lead();
        spans.extend(field_text_spans(save.value.as_deref(), room, Some(save)));
        spans.push(Span::styled("  checking", theme::dim()));
        return vec![Line::from(spans)];
    }

    // The typed editor: the buffer on `bg_sunken` (the shared Setup editor
    // body, `DANGER` while a refusal shows), the native cursor owns the caret
    // glyph (no simulated block). The refusal line trails under it.
    if let Some(edit) = edit_state {
        match &edit.edit {
            ShuntFieldEdit::Typed { input, .. } => {
                let mut spans = lead();
                spans.extend(value_caret(input, edit.refusal.is_some()));
                let mut rows = vec![Line::from(spans)];
                if let Some(refusal) = &edit.refusal {
                    rows.extend(invalid_tooltip_lines(refusal, width));
                }
                return rows;
            }
            ShuntFieldEdit::Pick { picked, saved } => {
                return field_cycle_lines(
                    lead(),
                    key,
                    Some(picked),
                    saved.as_deref(),
                    selected,
                    width,
                );
            }
        }
    }

    if is_cycle_field(key) {
        // An unset cycle row reads `—` (the approved unset form, never shunt's
        // default), not the bare option run; a held value shows the run.
        return match value.as_deref() {
            None => {
                let mut spans = lead();
                spans.push(Span::styled("—", theme::faint()));
                vec![Line::from(spans)]
            }
            Some(held) => field_cycle_lines(lead(), key, Some(held), None, selected, width),
        };
    }

    // A typed field at rest. The bind row's read-only suffix trails the value.
    let mut spans = lead();
    spans.extend(field_text_spans(value.as_deref(), room, None));
    if key == &ShuntFieldKey::Bind
        && let Some(suffix) = bind_suffix(&view.server)
    {
        let value_width = value
            .as_deref()
            .map(|v| v.chars().count().min(room))
            .unwrap_or(1);
        let suffix_room = room.saturating_sub(value_width + 2);
        if suffix_room > 0 {
            spans.push(Span::styled("  ", theme::dim()));
            spans.push(Span::styled(truncate(&suffix, suffix_room), theme::dim()));
        }
    }
    vec![Line::from(spans)]
}

/// The typed value spans: `ACCENT` (or `—` faint) at rest; while `save` runs,
/// the header's [`pulse_name_spans`] shimmer crest over the value.
fn field_text_spans(
    value: Option<&str>,
    room: usize,
    save: Option<&crate::tui::app::ShuntSave>,
) -> Vec<Span<'static>> {
    match value {
        None => vec![Span::styled("—", theme::faint())],
        Some(v) => {
            let display = truncate(&escape_control(v), room);
            match save {
                Some(save) => pulse_name_spans(
                    &display,
                    theme::accent(),
                    save.started.elapsed().as_millis() as u64,
                ),
                None => vec![Span::styled(display, theme::accent())],
            }
        }
    }
}

/// A cycle field's row: the cycle run at rest (the held value `ACCENT`,
/// bracketed only while focused), or over a pending pick (`held` the pick,
/// `saved` the file's value). A held value outside the set trails the run as
/// the `ACCENT` custom value; a saved out-of-set value trails faint while a
/// pick is pending (a stop the cycle can return to). Each is control-escaped
/// like every config-derived string.
fn field_cycle_lines(
    lead: Vec<Span<'static>>,
    key: &ShuntFieldKey,
    held: Option<&str>,
    saved: Option<&str>,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let options = cycle_options(key);
    let marked: Vec<(&str, bool)> = options.iter().map(|o| (*o, held == Some(o))).collect();
    let custom: Option<(String, bool)> = match held {
        Some(h) if !options.contains(&h) => Some((escape_control(h), true)),
        _ => saved
            .filter(|s| !options.contains(s))
            .map(|s| (escape_control(s), false)),
    };
    cycle_row_lines(
        lead,
        &marked,
        custom.as_ref().map(|(c, active)| (c.as_str(), *active)),
        None,
        selected,
        width,
    )
}

/// The legacy `providers` row: the names comma-joined as the value.
fn providers_field_line(names: &[String], width: usize) -> Line<'static> {
    let room = width.saturating_sub(CONFIG_VALUE_COL);
    let value = names
        .iter()
        .map(|n| escape_control(n))
        .collect::<Vec<_>>()
        .join(", ");
    Line::from(vec![
        Span::styled(
            format!(
                "{}{}",
                " ".repeat(CARET_GUTTER),
                config_key("providers", CARET_GUTTER)
            ),
            theme::label(),
        ),
        Span::styled(truncate(&value, room), theme::accent()),
    ])
}

/// The card's `config  not read (<reason>)` line, in the action-section shape
/// (label key + two-space gap), the reason word the card already uses.
fn config_not_read_line(unread: UnreadConfig) -> Line<'static> {
    Line::from(vec![
        Span::styled("config  ", theme::label()),
        Span::styled(format!("not read ({})", unread.label()), theme::body()),
    ])
}

/// The card's `config  not editable` line for a shape refusal (its sentence
/// renders on the `└` line under it).
fn config_not_editable_line() -> Line<'static> {
    Line::from(vec![
        Span::styled("config  ", theme::label()),
        Span::styled("not editable", theme::body()),
    ])
}

/// The card's `env file  not read (<reason>)` line, above the groups.
fn env_file_not_read_line(reason: EnvFileFailure) -> Line<'static> {
    Line::from(vec![
        Span::styled("env file  ", theme::label()),
        Span::styled(format!("not read ({})", reason.label()), theme::body()),
    ])
}

/// The legacy convert note under `providers`: `└` at [`CARET_GUTTER`] (under the
/// label), text faint, wrapped within the pane.
fn legacy_note_lines(width: usize) -> Vec<Line<'static>> {
    help_tooltip_lines_at(
        "[providers.*] form; convert to [[upstreams]] by hand to edit",
        width,
        CARET_GUTTER,
    )
}

/// One actionable line on the shunt card: an `f  <verb>` fix line (the plugin
/// detail's `problem_line` shape) or the `enabled` toggle row (its glyph via
/// the shared [`toggle_value`]). While an action runs both are inert and render
/// whole-faint (the disabled-row treatment), the renderer's `running` gate
/// matching the key handler's.
fn shunt_focus_line(focus: &ShuntFocus, text: &str, selected: bool, app: &App) -> Line<'static> {
    let inert = app.services.shunt_action.running;
    match focus {
        ShuntFocus::Fix { .. } => {
            if inert {
                Line::from(vec![
                    Span::styled(if selected { "❯ " } else { "  " }, theme::faint()),
                    Span::styled(text.to_string(), theme::faint()),
                ])
            } else {
                problem_line(text, selected)
            }
        }
        ShuntFocus::Enabled { .. } => {
            let on = app
                .services
                .shunt_record
                .as_ref()
                .map(|r| !r.disabled)
                .unwrap_or(false);
            let (arrow, label) = if inert {
                (
                    Span::styled(if selected { "❯ " } else { "  " }, theme::faint()),
                    Span::styled("enabled  ", theme::faint()),
                )
            } else if selected {
                (
                    Span::styled("❯ ", theme::accent().bold()),
                    Span::styled("enabled  ", label_style(true)),
                )
            } else {
                (
                    Span::raw("  "),
                    Span::styled("enabled  ", label_style(false)),
                )
            };
            let mut spans = vec![arrow, label];
            spans.extend(toggle_value(on, inert));
            Line::from(spans)
        }
        ShuntFocus::CheckOutput { .. } => problem_line(text, selected),
        ShuntFocus::Entry { .. } => unreachable!("entry rows render through entry_line"),
        ShuntFocus::Field { .. } => unreachable!("field rows render through field_row_lines"),
        ShuntFocus::Add { .. } => unreachable!("the `+ add` row renders through add_line"),
        ShuntFocus::SaveAdd { .. } => {
            unreachable!("the `save <kind>` row renders through save_add_lines")
        }
        ShuntFocus::Remove { .. } => {
            unreachable!("the `remove <kind>` row renders through remove_lines")
        }
        ShuntFocus::Token { .. } => unreachable!("a token row renders through token_row_lines"),
        ShuntFocus::AddToken => {
            unreachable!("the `+ add token` row renders through add_token_line")
        }
    }
}

/// The spans of one action-section line, before its walk gutter: the label key
/// (`moves`/`stays`/`refused`/`codex`/`pool`/`admin`) in the field-label style
/// and its value in its own tone. `moves`/`stays` middle-truncate their path
/// and keep the `stays` reason whole (the contract's path rule). The `MOVE
/// PLAN` eyebrow renders in the label style.
fn shunt_action_spans(text: &str, width: usize) -> Vec<Span<'static>> {
    if text == "MOVE PLAN" {
        return vec![Span::styled("MOVE PLAN", theme::label())];
    }
    let Some((key, value)) = text.split_once("  ") else {
        return vec![Span::styled(truncate(text, width), theme::body())];
    };
    let key_span = Span::styled(format!("{key}  "), theme::label());
    match key {
        "moves" => vec![
            key_span,
            Span::styled(
                middle_truncate(value, width.saturating_sub(key.len() + 2)),
                theme::body(),
            ),
        ],
        "stays" => {
            // `value` = `"<path>  <reason>"`; the reason is the approved
            // sentence (single spaces only), so the LAST double space is the
            // path/reason boundary. The path gives first — it middle-truncates
            // to whatever the whole reason leaves, and the reason is cut only
            // when the pane cannot hold it and a four-cell path floor.
            let (path, reason) = value.rsplit_once("  ").unwrap_or((value, ""));
            let after_key = width.saturating_sub(key.len() + 2 + 2);
            let path_room = after_key.saturating_sub(reason.chars().count()).max(4);
            let reason_room = after_key.saturating_sub(path_room);
            vec![
                key_span,
                Span::styled(middle_truncate(path, path_room), theme::body()),
                Span::raw("  "),
                Span::styled(truncate(reason, reason_room), theme::body()),
            ]
        }
        _ => vec![
            key_span,
            Span::styled(
                truncate(value, width.saturating_sub(key.len() + 2)),
                theme::body(),
            ),
        ],
    }
}

/// One or more rendered lines for a read-only action line: `refused` and
/// `pool` wrap their sentence within the pane (the tooltip's [`wrap_words`]
/// shape) so the fix instruction is never cut; every other line renders as one
/// row through [`shunt_action_spans`].
fn shunt_action_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::from("")];
    }
    if let Some((key, value)) = text.split_once("  ")
        && matches!(key, "refused" | "pool")
    {
        let wrap_w = width.saturating_sub(key.len() + 2).max(8);
        let indent = " ".repeat(key.len() + 2);
        return wrap_words(value, wrap_w)
            .into_iter()
            .enumerate()
            .map(|(i, seg)| {
                if i == 0 {
                    Line::from(vec![
                        Span::styled(format!("{key}  "), theme::label()),
                        Span::styled(seg, theme::warning()),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(indent.clone(), Style::default()),
                        Span::styled(seg, theme::warning()),
                    ])
                }
            })
            .collect();
    }
    vec![Line::from(shunt_action_spans(text, width))]
}

/// The `Check::problems` index of a detail line, `None` for a plain line.
fn problem_index(check: &Check, line: usize) -> Option<usize> {
    check.problems.iter().position(|p| p.line == line)
}
/// The `delegates` detail: a descendable job list whose selected row expands
/// under itself into its full facts and run directory. The delegates check's
/// one `key: value` detail line (the rate-limit warning) rides above the list
/// when it applies; the steer line pins as a footnote under the list. While
/// the selector holds focus the list renders blurred: top-anchored with no
/// selection before the first descent, after it the last-selected row's tint
/// and offset with no caret and no expansion. While the detail is descended,
/// the selection's row expands and the list scrolls to follow it.
fn draw_delegates_detail(frame: &mut Frame<'_>, inner: Rect, app: &App) {
    if inner.height == 0 {
        return;
    }
    let descended = app.services.focus == ServicesFocus::Detail;
    let cells = delegate_cells(&app.services.delegates, crate::usage::now_ms());
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

    if cells.is_empty() {
        draw_delegates_empty(frame, body);
        return;
    }

    let width = body.width as usize;
    // A selection shows once the user has descended at least once: descended
    // gains the caret + expansion; blurred keeps the last row's tint only.
    let selection = (descended || app.services.delegates_has_descended)
        .then_some(app.services.delegates_cursor.min(cells.len() - 1));
    let (lines, starts, block_ends) = delegate_job_lines(&cells, selection, descended, width);
    // The selected job's block (row + expansion) is what the scroll follows;
    // a blurred pane follows the row alone, and never-descended is top-anchored.
    let focus = match selection {
        Some(sel) if descended => (starts[sel], block_ends[sel]),
        Some(sel) => (starts[sel], starts[sel] + 1),
        None => (0, 0),
    };
    let draw = draw_scrolled_lines_with_footnote(
        frame,
        body,
        lines,
        focus,
        Some(&app.services.delegates_offset),
        Some(DELEGATES_STEER),
    );
    app.services.delegates_viewport.set(draw.viewport);
    app.services.delegates_total.set(draw.total);
    app.services.delegates_starts.replace(starts);
    app.services.delegates_block_ends.replace(block_ends);
}

/// The empty delegates detail: the framed empty state in the scroll area and
/// the steer line in the footnote area when both fit; on a pane too short to
/// hold the framed box (or inlined), the bare lines compose with the steer
/// through the shared helper.
fn draw_delegates_empty(frame: &mut Frame<'_>, body: Rect) {
    let foot = footnote_lines(DELEGATES_STEER, body.width as usize);
    let (scroll_area, foot_area) = footnote_split(body, foot.len());
    // The framed empty state needs its 4 rows (2 borders + 2 lines).
    if let Some(area) = foot_area
        && scroll_area.height >= 4
    {
        frame.render_widget(empty_state("no delegates", "r", "to refresh"), scroll_area);
        frame.render_widget(Paragraph::new(foot).style(theme::base()), area);
    } else {
        let lines = empty_state_lines("no delegates", "r", "to refresh");
        draw_scrolled_lines_with_footnote(frame, body, lines, (0, 0), None, Some(DELEGATES_STEER));
    }
}

/// The herdr detail: the read-only prose above, then an `options` section of
/// six focusable form rows editing the `AppState.herdr` knobs. While the
/// detail pane is descended, ↑↓ walks the rows and the whole form scrolls so
/// the focused row (plus its tooltip) stays on screen — the form-pane
/// `draw_scrolled_lines` shape, so the prose scrolls to follow the cursor
/// rather than holding a manual offset. The section header underlines while
/// focus rests on one of its rows.
/// `fix_focus` is the detail line of the heal fix while focus sits on it;
/// the option rows then hold no selection.
fn draw_herdr_detail(
    frame: &mut Frame<'_>,
    inner: Rect,
    app: &App,
    mut lines: Vec<Line<'static>>,
    fix_focus: Option<usize>,
) {
    let focused = app.services.focus == ServicesFocus::Detail;

    lines.push(Line::from(""));
    let mut section_style = theme::label();
    // The eyebrow underlines while focus rests on one of its rows, never while
    // it sits on the heal line above the section.
    if focused && fix_focus.is_none() {
        section_style = section_style.underlined();
    }
    lines.push(Line::from(Span::styled("OPTIONS", section_style)));

    let settings = app.config().state.herdr.clone();
    let editing = app.services.herdr_tag_draft.as_ref();
    let writable = herdr_config_writable(app);
    // The focused row's block (row + its tooltip lines) for the scroll focus,
    // and the native-cursor slot for the tag editor.
    let mut focus = fix_focus.map_or((0usize, 1usize), |line| (line, line + 1));
    let mut caret: Option<(u16, usize)> = None;

    for (i, row) in HERDR_OPTIONS.iter().enumerate() {
        let selected = focused
            && fix_focus.is_none()
            && !app.services.diagnostic_focus
            && i == app.services.herdr_options_cursor;
        let row_editing = if *row == HerdrOption::TagRefresh {
            editing
        } else {
            None
        };
        let inert = *row == HerdrOption::DelegateRowText && !writable;
        if selected {
            focus.0 = lines.len();
        }
        let row_lines = option_row(
            *row,
            &settings,
            selected,
            row_editing,
            inert,
            inner.width as usize,
        );
        match row_editing {
            Some(input) => {
                // The edit row renders plain (no highlight) with the edit
                // gutter; the native terminal cursor owns the caret. x = "✎ "
                // (2) + label + the 2-space value gap + pre-caret cols.
                let cx = inner.x.saturating_add(
                    (2 + row.label().chars().count() + 2 + head_cols(input)) as u16,
                );
                caret = Some((cx, lines.len()));
                lines.extend(row_lines);
                lines.extend(tag_refresh_range_tooltip(input, inner.width as usize));
            }
            None => {
                for line in row_lines {
                    lines.push(if selected {
                        highlight_row(line, inner.width as usize)
                    } else {
                        line
                    });
                }
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

    if app.services.diagnostic_focus {
        draw_diagnostic_lines(frame, inner, app, lines);
        return;
    }
    app.services.detail_viewport.set(inner.height);
    app.services.detail_max_scroll.set(
        lines
            .len()
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16,
    );
    let offset = draw_scrolled_lines(frame, inner, lines, focus, Some(&app.services.form_offset));
    app.services.displayed_scroll.set(offset as u16);
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
    width: usize,
) -> Vec<Line<'static>> {
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
            let popup = settings.popup_width;
            let options = [
                ("fit", popup == PopupWidth::Fit),
                ("half", popup == PopupWidth::Half),
                ("split-right", popup == PopupWidth::SplitRight),
                ("split-top", popup == PopupWidth::SplitTop),
            ];
            return cycle_row_lines(spans, &options, None, None, selected, width);
        }
        HerdrOption::PaneTag => spans.extend(toggle_value(settings.pane_tag, inert)),
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
        HerdrOption::BorderLabel => spans.extend(toggle_value(settings.border_label, inert)),
        HerdrOption::DelegateDot => spans.extend(toggle_value(settings.delegate_dot, inert)),
        HerdrOption::DelegateRowText => {
            spans.extend(toggle_value(settings.delegate_row_text, inert))
        }
    }
    vec![Line::from(spans)]
}

/// A toggle row's value: the tier's glyph from [`theme::toggle_on`] /
/// [`theme::toggle_off`], painted per the contract — `full` `─●` (track
/// `LINE`, knob `ACCENT`) / `○─` (knob `TEXT_DIM`, track `LINE`); `compatible`
/// `[on]`/`[off]` (brackets `TEXT_DIM`, `on` `ACCENT`, `off` `TEXT_DIM`). An
/// inert row paints the whole glyph faint whatever its state. One helper
/// feeds the shunt `enabled` row and the herdr toggles, so both change
/// together.
fn toggle_value(on: bool, inert: bool) -> Vec<Span<'static>> {
    let glyph = if on {
        theme::toggle_on()
    } else {
        theme::toggle_off()
    };
    if inert {
        return vec![Span::styled(glyph, theme::faint())];
    }
    let state = if on { theme::accent() } else { theme::dim() };
    match theme::tier() {
        theme::Tier::Full => glyph
            .char_indices()
            .map(|(at, c)| {
                let part = &glyph[at..at + c.len_utf8()];
                Span::styled(part, if c == '─' { theme::line() } else { state })
            })
            .collect(),
        theme::Tier::Compatible => {
            let word = glyph.trim_start_matches('[').trim_end_matches(']');
            vec![
                Span::styled("[", theme::dim()),
                Span::styled(word, state),
                Span::styled("]", theme::dim()),
            ]
        }
    }
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
        return Line::from(prose::fit(text, width, theme::dim()));
    }
    if let Some((key, value)) = text.split_once(": ") {
        let pad = key_w.saturating_sub(key.chars().count()) + 2;
        let value_w = width.saturating_sub(key_w + 2);
        let tone = value_tone(key, value);
        let mut spans = vec![Span::styled(
            format!("{key}{}", " ".repeat(pad)),
            theme::label(),
        )];
        if is_path_key(key) {
            spans.push(Span::styled(middle_truncate(value, value_w), tone));
        } else {
            spans.extend(prose::fit(value, value_w, tone));
        }
        return Line::from(spans);
    }
    Line::from(prose::fit(text, width, theme::body()))
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

/// One `f  <verb>` fix line: its key is a key in prose (`ACCENT + bold`), the
/// verb dim while unfocused; the focused problem takes the selected row's caret
/// + `TEXT + bold` verb (the caller adds the hover tint).
fn problem_line(text: &str, selected: bool) -> Line<'static> {
    let (lead, base) = if selected {
        (
            Span::styled("❯ ", theme::accent().bold()),
            label_style(true),
        )
    } else {
        (Span::styled("  ", Style::default()), theme::dim())
    };
    let marked = match text.split_once("  ") {
        Some((key, verb)) => format!("{}  {verb}", prose::key(key)),
        None => text.to_string(),
    };
    let mut spans = vec![lead];
    spans.extend(prose::spans(&marked, base));
    Line::from(spans)
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
        // An install older than this clauth: the reason the plugin row warns.
        ("version", _) if value.contains(OUTDATED_PLUGIN_MARK) => theme::warning(),
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

/// The line under the list, verbatim.
const DELEGATES_STEER: &str = "manage delegates in clauth app on web or mobile (coming soon)";

/// The whole job list as scroll content: one row per job, the selected job's
/// expansion under it. Returns the lines plus the per-job geometry the scroll
/// follow and the page keys read: each job's start line and its focused block
/// end (its own row plus the expansion it gains once selected). `selected` is
/// the tinted job (`None` before the first descent); `descended` adds the
/// caret, label promotion and expansion to the selected job.
fn delegate_job_lines(
    cells: &[DelegateCells],
    selected: Option<usize>,
    descended: bool,
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<usize>) {
    let name_w = cells
        .iter()
        .map(|c| c.profile.chars().count())
        .max()
        .unwrap_or(0)
        .min(NAME_W_MAX);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts = Vec::with_capacity(cells.len());
    let mut block_ends = Vec::with_capacity(cells.len());
    for (i, cell) in cells.iter().enumerate() {
        starts.push(lines.len());
        let is_selected = selected == Some(i);
        let focused = descended && is_selected;
        lines.push(delegate_row_line(cell, name_w, width, is_selected, focused));
        let expansion = delegate_expansion_lines(cell, width);
        block_ends.push(lines.len() + expansion.len());
        if focused {
            lines.extend(expansion);
        }
    }
    (lines, starts, block_ends)
}

/// The selected row's full facts: each fact on its own line, then the full
/// run directory, wrapped at the content width instead of middle-truncated.
fn delegate_expansion_lines(cells: &DelegateCells, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.saturating_sub(2).max(8);
    let mut lines = Vec::new();
    for fact in &cells.facts {
        for seg in wrap_words(fact, wrap_w) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, theme::dim()),
            ]));
        }
    }
    if let Some(cwd) = &cells.cwd {
        // A directory is chunked by display width, never on whitespace: a name
        // holding runs of spaces keeps every byte.
        for seg in chunk_graphemes(cwd, wrap_w) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, theme::faint()),
            ]));
        }
    }
    lines
}

/// One delegate row: `  ● state  account  facts  cwd`, the 2-cell caret gutter
/// reserved on every row (`❯ ` on the focused one). `selected` carries the
/// `BG_HOVER` tint; `focused` adds the caret and promotes only the account
/// label to `TEXT + bold`.
fn delegate_row_line(
    cells: &DelegateCells,
    name_w: usize,
    width: usize,
    selected: bool,
    focused: bool,
) -> Line<'static> {
    let caret = if selected && focused {
        Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD))
    } else {
        Span::raw("  ")
    };
    let mut spans = vec![
        caret,
        Span::styled(
            format!("{} ", state_mark(cells.state)),
            Style::default().fg(state_color(cells.state)),
        ),
        Span::styled(key_cell(cells.state.label(), STATE_W, 2), theme::dim()),
        Span::styled(
            key_cell(&truncate(&cells.profile, name_w), name_w, 2),
            if selected && focused {
                theme::body().add_modifier(Modifier::BOLD)
            } else {
                theme::body()
            },
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
    select_line(Line::from(spans), selected, focused, width as u16)
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

pub(super) fn health_color(health: Health) -> ratatui::style::Color {
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
