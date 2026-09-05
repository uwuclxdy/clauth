//! Plugin tab — Claude Code integration health on the left, the selected row's
//! readout on the right, and the running delegates underneath. The master-detail
//! mirrors the Status tab's two-pane machinery and counts as 2 of the 3-panel
//! budget; the delegates pane is the third, so the budget is now SPENT and a
//! fourth panel means restructuring the screen.
//!
//! The left panel is one cursor-driven selector over the integration checks
//! (clauth on PATH, mcpServers wiring, plugin install, CC version, and a
//! `runtime` row that folds every profile's live sessions / credential link /
//! token freshness into one summary). Each row is a status dot + label, the
//! verdict in the detail pane. Enter descends into the detail pane; `f` applies
//! the selected row's fix (when one applies). All data is recomputed
//! synchronously on tab focus and `r` — there is no background thread, so the
//! title spinner only flickers while the cached `claude --version` is probed.
//!
//! The delegates pane is read-only and takes no keys at all: stopping a delegate
//! is `monitor({job_ids, cancel: true})`'s job, and a second stop path would
//! need its own confirm. That is also why its overflow reads `+N more` rather
//! than carrying a scrollbar — see [`delegate_lines`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::super::app::{
    App, HERDR_OPTIONS, Health, HerdrOption, InputState, PluginFocus, herdr_config_writable,
    parse_herdr_tag_secs,
};
use super::super::theme;
use super::format::spinner_frame;
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
    let rows = delegate_cells(&app.plugin.delegates, crate::usage::now_ms());
    let [top, delegates] = Layout::vertical([
        Constraint::Min(MASTER_MIN),
        Constraint::Length(delegates_height(rows.len(), area.height)),
    ])
    .areas(area);

    let (selector, detail) = master_detail(top, app.plugin.row_count());

    draw_selector(frame, selector, app);
    draw_detail(frame, detail, app);
    draw_delegates(frame, delegates, &rows);
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
            SelectorRow {
                label_style: theme::body(),
                // Checks are dot-only in the list; the dot color carries the
                // verdict and the full readout lives in the detail pane.
                value: "",
                label_pad: 0,
                selected: idx == app.plugin.cursor,
                focused,
                content_w,
                has_fix: check.fix.is_some(),
            },
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

/// The display context for one selector row — label styling, value, alignment
/// pad, selection and focus, pane width, and the fix-marker reservation.
/// Grouped so [`selector_row`] stays under clippy's argument limit without an
/// ad-hoc `#[allow]`.
#[derive(Clone, Copy)]
struct SelectorRow<'a> {
    label_style: Style,
    value: &'a str,
    label_pad: usize,
    selected: bool,
    focused: bool,
    content_w: usize,
    has_fix: bool,
}

/// One selector row: `❯ ● label   value`. Checks render dot + label only
/// (`value` empty); profile rows pad the label to `label_pad` so their values
/// line up in a column. The hover tint spans the full content width when
/// selected (the ratatui filler-tint gotcha); the caret shows only in the
/// focused pane.
fn selector_row(health: Health, label: &str, row: SelectorRow<'_>) -> Line<'static> {
    let SelectorRow {
        label_style,
        value,
        label_pad,
        selected,
        focused,
        content_w,
        has_fix,
    } = row;
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

    // The herdr detail appends its options section and takes per-row focus;
    // every other detail keeps the scroll-only path below.
    if app
        .plugin
        .selected_check()
        .is_some_and(|c| c.label == "herdr")
    {
        draw_herdr_detail(frame, inner, app, lines);
        return;
    }

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

/// The herdr detail: the read-only prose above, then an `options` section of
/// six focusable form rows editing the `AppState.herdr` knobs. While the
/// detail pane is descended, ↑↓ walks the rows and the whole form scrolls so
/// the focused row (plus its tooltip) stays on screen — the form-pane
/// `draw_scrolled_lines` shape, so the prose scrolls to follow the cursor
/// rather than holding a manual offset. The section header underlines while
/// focus rests on one of its rows.
fn draw_herdr_detail(frame: &mut Frame<'_>, inner: Rect, app: &App, mut lines: Vec<Line<'static>>) {
    let focused = app.plugin.focus == PluginFocus::Detail;

    lines.push(Line::from(""));
    let mut section_style = theme::label();
    if focused {
        section_style = section_style.underlined();
    }
    lines.push(Line::from(Span::styled("OPTIONS", section_style)));

    let settings = app.config().state.herdr.clone();
    let editing = app.plugin.herdr_tag_draft.as_ref();
    let writable = herdr_config_writable(app);
    // The focused row's block (row + its tooltip lines) for the scroll focus,
    // and the native-cursor slot for the tag editor.
    let mut focus = (0usize, 1usize);
    let mut caret: Option<(u16, usize)> = None;

    for (i, row) in HERDR_OPTIONS.iter().enumerate() {
        let selected = focused && i == app.plugin.herdr_options_cursor;
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
    if app.plugin.herdr_config.as_ref().is_some_and(|c| !c.parsed) {
        "herdr's config doesn't parse, so clauth can't rewrite the row"
    } else {
        "herdr's config can't be read, so clauth can't rewrite the row"
    }
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
        ("plugin", "linked") => theme::success(),
        ("plugin", "installed") => theme::success(),
        ("plugin", "disabled") => theme::warning(),
        ("plugin", "not") => theme::warning(),
        ("key", "not") => theme::warning(),
        ("sidebar", "templated") => theme::success(),
        ("sidebar", "not") => theme::warning(),
        _ => theme::body(),
    }
}

// ── Bottom panel: what the delegates are doing ──────────────────────────────────

/// Rows the master-detail above keeps before the delegates pane may claim any:
/// the stacked selector's own floor plus its `Min(8)` detail pane, which is what
/// makes the tab usable at all. The delegates pane is what gives way.
const MASTER_MIN: u16 = 13;
/// Border rows plus the steer line — everything the pane spends that is not a
/// delegate.
const PANE_CHROME: u16 = 3;
/// Rows [`empty_state`] needs to draw its own frame and both its lines.
const EMPTY_STATE_H: usize = 4;
/// Most delegate rows the pane will grow to hold; past it the overflow marker
/// carries the rest. A tab whose subject is the integration checks does not give
/// half the screen to a fan-out.
const DELEGATE_ROWS_MAX: usize = 6;
/// Width of the state word column (`orphaned` is the longest).
const STATE_W: usize = 8;
/// Cap on the account column, so one long name cannot push every row's figures
/// off the pane.
const NAME_W_MAX: usize = 18;
/// A tail shorter than this is all ellipsis and no signal, so it is dropped
/// whole instead.
const TAIL_MIN_W: usize = 8;

/// The line under the list. Owner's words, verbatim: the pane takes no keys, so
/// this is the whole of what it offers beyond the list itself.
const DELEGATES_STEER: &str = "manage delegates in clauth app on web or mobile (coming soon)";

/// Rows the delegates pane takes: its content plus [`PANE_CHROME`], capped so
/// the master-detail above keeps [`MASTER_MIN`] rows, and `0` when that leaves
/// too little — the pane drops WHOLE rather than clipping to a box with nothing
/// readable in it, the same rule the overview's live column plays by.
///
/// Never a constant: pinned at one height it would waste rows on an empty store
/// and hide runs on a busy one.
///
/// The floor is compared against `want` as well as against itself, so a pane
/// that fits at its natural height (one delegate wants 4 rows) is drawn rather
/// than refused for being under a floor that only exists to stop clipping. That
/// pairing is also what keeps a one-row viewport from ever holding nothing but
/// an overflow marker: two or more rows want at least 5, which the floor then
/// requires, which leaves the list at least 2 rows.
fn delegates_height(rows: usize, area_height: u16) -> u16 {
    let content = if rows == 0 {
        EMPTY_STATE_H
    } else {
        rows.min(DELEGATE_ROWS_MAX)
    };
    let want = u16::try_from(content)
        .unwrap_or(u16::MAX)
        .saturating_add(PANE_CHROME);
    let room = area_height.saturating_sub(MASTER_MIN);
    let floor = PANE_CHROME
        + if rows == 0 {
            u16::try_from(EMPTY_STATE_H).unwrap_or(u16::MAX)
        } else {
            2
        };
    if room < floor.min(want) {
        return 0;
    }
    want.min(room)
}

fn draw_delegates(frame: &mut Frame<'_>, area: Rect, rows: &[DelegateCells]) {
    if area.height == 0 {
        return;
    }
    // Third panel on this body, and never focused — no key reaches it — so it
    // keeps the blurred chrome and the non-first panel's TEXT_DIM title.
    let block = section_box("delegates", false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let [list_area, steer_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(DELEGATES_STEER, theme::faint())))
            .style(theme::base()),
        steer_area,
    );

    if rows.is_empty() {
        frame.render_widget(empty_state("no delegates", "r", "to refresh"), list_area);
        return;
    }
    let lines = delegate_lines(rows, list_area.height as usize, list_area.width as usize);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), list_area);
}

/// The rows that fit, plus a `+N more` line naming what did not.
///
/// **House deviation**: the cloudy-tui contract makes a scrollbar the only legal
/// overflow signal. This pane binds no key, so a scrollbar here would advertise
/// a scroll that cannot happen; a count says the same thing and promises
/// nothing. The reason lives here because it is the only written home it has —
/// the design-doc entry recording it is owed and not yet written.
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

/// One delegate row: `● running  account  facts …  "tail"`.
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
    // The delegate's own words, quoted so they cannot read as clauth's report
    // about it, and last so the columns before them never move.
    let used: usize = spans.iter().map(Span::width).sum();
    let room = width.saturating_sub(used + 4); // the 2-space gap and both quotes
    if !cells.tail.is_empty() && room >= TAIL_MIN_W {
        spans.push(Span::styled(
            format!("  \"{}\"", truncate(&cells.tail, room)),
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
    /// The delegate's own last words, already bounded by the writer. Empty when
    /// it has said nothing, and on every finished record (a done envelope
    /// carries the whole result, so a tail beside it says nothing new).
    tail: String,
}

/// One cell set per stored record, in the order they arrive.
///
/// **It sorts nothing.** Banding is `jobs::list_banded`'s, which is what
/// `recompute_plugin_checks` reads the store through, and what `clauth jobs` and
/// `monitor`'s listing read it through as well. This pane used to band its own
/// already-rendered cells off the same shared rank, which could not drift TODAY
/// and was still two sort sites: a later change to `list_banded` — a tiebreak, a
/// third band — would have reached the text surfaces and not this one, with
/// nothing to red.
fn delegate_cells(stored: &[StoredJob], now: u64) -> Vec<DelegateCells> {
    stored.iter().map(|job| delegate_row(job, now)).collect()
}

fn delegate_row(job: &StoredJob, now: u64) -> DelegateCells {
    let record = &job.record;
    let profile = record.profile.clone();
    // The store's own retention stamp, so a row is dated by the same field that
    // decides how long it survives.
    let since = job.age_secs(now);
    // Through `phase()` rather than by re-matching `(liveness, kind)` here: a
    // fifth situation added there must not compile clean into this pane still
    // classifying by the old four.
    let state = job.phase();
    match state {
        JobPhase::Done => DelegateCells {
            state,
            profile,
            facts: vec![format!("finished {}", age_phrase(since))],
            tail: String::new(),
        },
        JobPhase::Orphaned => DelegateCells {
            state,
            profile,
            facts: vec![format!("last seen {}", age_phrase(since))],
            tail: record.tail.clone(),
        },
        JobPhase::Running | JobPhase::Blocking => {
            let live = jobs::running_liveness(record, now);
            let mut facts = vec![format!(
                "elapsed {}",
                humanize_duration(live.elapsed_secs as i64)
            )];
            if live.recorded {
                facts.push(match live.last_output_secs_ago {
                    Some(secs) => format!("last output {}", age_phrase(secs)),
                    None => "no output yet".to_string(),
                });
                if let Some((label, secs)) = next_deadline(&live) {
                    facts.push(if secs == 0 {
                        format!("{label} now")
                    } else {
                        format!("{label} in {}", humanize_duration(secs as i64))
                    });
                }
            } else {
                // `monitor`'s own wording for the same gap, so one record does
                // not get two names for what is missing from it.
                facts.push("liveness not recorded".to_string());
            }
            DelegateCells {
                state,
                profile,
                facts,
                tail: record.tail.clone(),
            }
        }
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
        // Neutral: a profile that is neither linked nor live — not green.
        Health::Idle => theme::text_dim_color(),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_plugin.rs"]
mod tests;
