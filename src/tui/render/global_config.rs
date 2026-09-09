//! Program-wide Config tab — a single panel of global settings, distinct from
//! the per-account Setup tab. Rows back real persisted state in `AppState` and
//! run in the concern bands `GlobalConfigRow::band` names, each opened by an
//! eyebrow header: appearance (`theme`, `reset display`, and the `clock`
//! notation it gates), scheduler (`on mismatch`, `refresh`
//! cadence, `refresh spent` toggle, `rotation`), auto-switch (`weekly limit`,
//! `switch mode` = burn-aware, the burn-aware `burn floor`/`burn horizon`
//! tunables it gates (issue #8 follow-up b), then the `quota spent` halt), then
//! extra usage (`allow extra usage` opt-in + its own `extra usage spent` halt
//! default — real money).
//! ↑↓ walks the rows; space cycles a row's value in place; ⏎ opens the
//! refresh-interval and weekly-threshold custom-value editors and otherwise
//! mirrors space. No left selector, no popups — settings are global.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::profile::{
    ClockFormat, DEFAULT_BURN_FLOOR_PCT, DEFAULT_BURN_HORIZON_MS, DEFAULT_REFRESH_INTERVAL_MS,
    DEFAULT_WEEKLY_SWITCH_PCT, DivergenceChoice, MAX_REFRESH_INTERVAL_MS, MIN_REFRESH_INTERVAL_MS,
    ResetDisplay,
};

use super::super::app::{
    App, BURN_FLOOR_PRESETS, BURN_HORIZON_PRESETS, GLOBAL_CONFIG_ROWS, GlobalConfigRow, InputState,
    WEEKLY_PRESETS, format_weekly_pct, parse_refresh_secs, parse_weekly_pct,
};
use super::super::theme::{self, Tier};
use super::panes::{
    cycle_option, draw_scrolled_lines, head_cols, help_tooltip_lines, highlight_row,
    invalid_tooltip_lines, key_cell, label_style, section_box, value_caret,
};

/// Width of the key column: the longest keys (`allow extra usage` /
/// `extra usage spent`, 17). Keys pad to it, then [`KEY_GUTTER`] separates them
/// from the value — so every row's value starts at the same column (the Config
/// tab is a cloudy-tui tight chip group).
const KEY_W: usize = 17;
/// Fixed gap between the padded key and the value column.
const KEY_GUTTER: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("settings", true, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = {
        let cfg = app.config();
        let state = &cfg.state;
        RowState {
            switch_off_when_spent: state.switch_off_when_spent,
            burn_aware: state.burn_aware_switching,
            spend_budget: state.spend_budget_switching,
            switch_off_when_budget_spent: state.switch_off_when_budget_spent,
            preemptive: state.preemptive_rotation,
            refresh_spent: state.refresh_spent_accounts,
            auto_start_queue: state.auto_start_queue,
            any_auto_start: cfg.profiles.iter().any(|p| p.auto_start),
            reset_display: state.reset_display(),
            clock_format: state.clock_format(),
        }
    };
    let tunables = {
        let state = &app.config().state;
        RowTunables {
            refresh_interval_ms: app
                .refresh_interval
                .load(std::sync::atomic::Ordering::Relaxed),
            weekly_pct: state.weekly_switch_threshold_pct(),
            burn_floor_pct: state.burn_switch_floor_pct(),
            burn_horizon_ms: state.burn_horizon_cap_ms(),
            default_divergence: state.default_divergence,
        }
    };
    let cursor = app
        .global_config_cursor
        .min(GLOBAL_CONFIG_ROWS.len().saturating_sub(1));
    let editing = app.refresh_interval_draft.as_ref();
    let weekly_editing = app.weekly_threshold_draft.as_ref();

    let focused_band = GLOBAL_CONFIG_ROWS[cursor].band();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut caret: Option<(u16, usize)> = None;
    // Start + end of the focused row's block (row plus its tooltip lines), so a
    // wrapped hint can't scroll off the bottom while its row stays visible.
    let mut focus = (0usize, 1usize);
    let mut band: Option<&str> = None;
    for (i, row) in GLOBAL_CONFIG_ROWS.iter().enumerate() {
        let selected = i == cursor;
        // Eyebrow header opening each concern band, preceded by a blank spacer
        // from the second band on so the groups read as separate sections.
        if band != Some(row.band()) {
            if band.is_some() {
                lines.push(Line::default());
            }
            band = Some(row.band());
            lines.push(band_header(row.band(), row.band() == focused_band));
        }
        if selected {
            focus.0 = lines.len();
        }
        let row_editing = match row {
            GlobalConfigRow::RefreshInterval => editing,
            GlobalConfigRow::WeeklyThreshold => weekly_editing,
            _ => None,
        };
        let line = detail_row(*row, selected, rows, tunables, row_editing);
        match row_editing {
            Some(input) => {
                // The native terminal cursor owns the caret; the row renders plain
                // (no highlight) with the edit gutter + sunken field, like the chain
                // threshold editor. x = "✎ " (2) + key block + pre-caret cols.
                let cx = inner
                    .x
                    .saturating_add((2 + KEY_W + KEY_GUTTER + head_cols(input)) as u16);
                caret = Some((cx, lines.len()));
                lines.push(line);
                lines.extend(if *row == GlobalConfigRow::WeeklyThreshold {
                    weekly_range_tooltip(input, inner.width as usize)
                } else {
                    refresh_range_tooltip(input, inner.width as usize)
                });
            }
            None => {
                lines.push(if selected {
                    highlight_row(line, inner.width as usize)
                } else {
                    line
                });
                if selected && let Some(tip) = row_hint(*row, rows, tunables) {
                    lines.extend(help_tooltip_lines(&tip, inner.width as usize));
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

/// Eyebrow header opening a concern band: UPPERCASE, `TEXT_DIM + bold` at all
/// times (a fixed label treatment, not a focus cue), gaining an underline while
/// the cursor sits on one of its rows — the contract's section-header form.
fn band_header(label: &str, focused: bool) -> Line<'static> {
    let style = theme::label();
    let style = if focused { style.underlined() } else { style };
    Line::from(Span::styled(label.to_uppercase(), style))
}

/// Inline help for rows whose value doesn't self-describe. Phrased for the
/// value currently selected, so cycling a row re-explains what it now does.
/// The persisted values the Config tab's non-numeric rows render, bundled so
/// `detail_row` / `row_hint` stay within clippy's argument budget as rows
/// accumulate.
#[derive(Clone, Copy)]
struct RowState {
    switch_off_when_spent: bool,
    burn_aware: bool,
    spend_budget: bool,
    switch_off_when_budget_spent: bool,
    preemptive: bool,
    refresh_spent: bool,
    auto_start_queue: bool,
    /// Whether ANY account has opted into `auto_start` — the queue toggle is
    /// inert without one (there is nothing to space), so the row dims and its
    /// key no-ops, like every other row another setting makes inert.
    any_auto_start: bool,
    reset_display: ResetDisplay,
    clock_format: ClockFormat,
}

/// The numeric tunables the Config tab's rows render, gathered once per draw.
/// Bundled for the same argument-budget reason as [`RowState`]; [`row_hint`]
/// reads the same bundle, so the two stay in sync as rows accumulate.
#[derive(Clone, Copy)]
struct RowTunables {
    refresh_interval_ms: u64,
    weekly_pct: f64,
    burn_floor_pct: f64,
    burn_horizon_ms: u64,
    default_divergence: Option<DivergenceChoice>,
}

fn row_hint(row: GlobalConfigRow, rows: RowState, tunables: RowTunables) -> Option<String> {
    let RowTunables {
        refresh_interval_ms,
        weekly_pct,
        burn_floor_pct,
        burn_horizon_ms,
        default_divergence,
    } = tunables;
    // The default + units live on the row as a faint span (only when the value
    // is off its default), so the hint states behavior alone, interpolating the
    // live value. Rows another toggle makes inert render dimmed and keep their
    // behavior hint — the dim is the "can't touch this", not a gate clause.
    let tip: String = match row {
        GlobalConfigRow::Theme => return None,
        GlobalConfigRow::ResetShape => String::from(match rows.reset_display {
            ResetDisplay::Relative => "show how long a usage window has left",
            ResetDisplay::Clock => "show the time of day a usage window resets",
            ResetDisplay::Both => "show how long a window has left and the time it resets",
        }),
        GlobalConfigRow::ClockNotation => String::from(match rows.clock_format {
            ClockFormat::H24 => "write reset times as 21:20, in your local timezone",
            ClockFormat::H12 => "write reset times as 9:20pm, in your local timezone",
        }),
        GlobalConfigRow::DivergenceDefault => String::from(match default_divergence {
            None => "ask what to do when claude code signs in over the active account",
            Some(DivergenceChoice::Overwrite) => {
                "fold a new login into the active account, replacing its credentials"
            }
            Some(DivergenceChoice::NewProfile) => {
                "pick which account to save a new login into, keeping the current one"
            }
            Some(DivergenceChoice::Discard) => {
                "restore the previous credentials and drop the new login"
            }
        }),
        GlobalConfigRow::RefreshInterval => {
            format!(
                "check every account's usage every {}s",
                refresh_interval_ms / 1000
            )
        }
        GlobalConfigRow::WeeklyThreshold => format!(
            "don't send new work to an account past {}% of its weekly limit",
            format_weekly_pct(weekly_pct)
        ),
        GlobalConfigRow::SwitchOffWhenSpent => String::from(if rows.switch_off_when_spent {
            "sign every account out once they're all spent, unless one is marked last resort"
        } else {
            "keep using whichever account is active once they're all spent"
        }),
        GlobalConfigRow::BurnAware => String::from(if rows.burn_aware {
            "switch away once the burn rate would hit 100% before the next check"
        } else {
            "switch the active account away once its usage crosses its threshold"
        }),
        GlobalConfigRow::BurnFloor => format!(
            "never switch away before {}% used, however fast the burn",
            format_weekly_pct(burn_floor_pct)
        ),
        GlobalConfigRow::BurnHorizon => format!(
            "look at most {}s ahead when guessing the next switch",
            burn_horizon_ms / 1000
        ),
        GlobalConfigRow::SpendBudget => String::from(if rows.spend_budget {
            "let a spent account keep working on paid usage, up to its max spend"
        } else {
            "never spend real money automatically"
        }),
        GlobalConfigRow::SwitchOffWhenBudgetSpent => {
            String::from(if rows.switch_off_when_budget_spent {
                "once an account's extra usage runs out, switch everything off"
            } else {
                "once an account's extra usage runs out, stay on it and keep billing"
            })
        }
        GlobalConfigRow::PreemptiveRotation => String::from(if rows.preemptive {
            "rotate the login before it expires"
        } else {
            "rotate the login only when a request rejects it"
        }),
        GlobalConfigRow::RefreshSpentAccounts => String::from(if rows.refresh_spent {
            "keep checking accounts that are already at 100%"
        } else {
            "skip refreshing a spent account until its window resets"
        }),
        GlobalConfigRow::AutoStartQueue => String::from(if rows.auto_start_queue {
            "space auto-start windows evenly, so one resets every 5h / accounts"
        } else {
            "auto-start usage windows as soon as possible"
        }),
    };
    Some(tip)
}

fn detail_row(
    row: GlobalConfigRow,
    selected: bool,
    rows: RowState,
    tunables: RowTunables,
    editing: Option<&InputState>,
) -> Line<'static> {
    let RowTunables {
        refresh_interval_ms,
        weekly_pct,
        burn_floor_pct,
        burn_horizon_ms,
        default_divergence,
    } = tunables;
    let arrow = if editing.is_some() {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    let tier = theme::tier();
    match row {
        GlobalConfigRow::Theme => cycle_row(
            arrow,
            "theme",
            &[
                ("full", tier == Tier::Full),
                ("compatible", tier == Tier::Compatible),
            ],
            selected,
        ),
        GlobalConfigRow::ResetShape => cycle_row(
            arrow,
            "reset display",
            &[
                ("relative", rows.reset_display == ResetDisplay::Relative),
                ("clock", rows.reset_display == ResetDisplay::Clock),
                ("both", rows.reset_display == ResetDisplay::Both),
            ],
            selected,
        ),
        // Inert until a reset renders a clock — the notation decides nothing
        // under `relative`. Dimmed AND the key no-ops, like `extra usage spent`.
        GlobalConfigRow::ClockNotation => {
            let options = [
                ("24h", rows.clock_format == ClockFormat::H24),
                ("12h", rows.clock_format == ClockFormat::H12),
            ];
            if rows.reset_display.shows_clock() {
                cycle_row(arrow, "clock", &options, selected)
            } else {
                dimmed_cycle_row("clock", &options, selected)
            }
        }
        GlobalConfigRow::RefreshInterval => match editing {
            Some(input) => refresh_edit_line(arrow, input),
            None => refresh_cycle_line(arrow, refresh_interval_ms, selected),
        },
        GlobalConfigRow::WeeklyThreshold => match editing {
            Some(input) => weekly_edit_line(arrow, input),
            None => weekly_cycle_line(arrow, weekly_pct, selected),
        },
        GlobalConfigRow::DivergenceDefault => cycle_row(
            arrow,
            "on mismatch",
            &[
                ("ask", default_divergence.is_none()),
                (
                    "overwrite",
                    default_divergence == Some(DivergenceChoice::Overwrite),
                ),
                (
                    "new",
                    default_divergence == Some(DivergenceChoice::NewProfile),
                ),
                (
                    "discard",
                    default_divergence == Some(DivergenceChoice::Discard),
                ),
            ],
            selected,
        ),
        GlobalConfigRow::SwitchOffWhenSpent => cycle_row(
            arrow,
            "quota spent",
            &[
                ("stay on active", !rows.switch_off_when_spent),
                ("switch off all", rows.switch_off_when_spent),
            ],
            selected,
        ),
        GlobalConfigRow::BurnAware => cycle_row(
            arrow,
            "switch mode",
            &[
                ("static", !rows.burn_aware),
                ("burn-aware", rows.burn_aware),
            ],
            selected,
        ),
        GlobalConfigRow::BurnFloor => {
            burn_floor_line(arrow, burn_floor_pct, selected, rows.burn_aware)
        }
        GlobalConfigRow::BurnHorizon => {
            burn_horizon_line(arrow, burn_horizon_ms, selected, rows.burn_aware)
        }
        GlobalConfigRow::SpendBudget => cycle_row(
            arrow,
            "allow extra usage",
            &[
                ("off", !rows.spend_budget),
                ("pay-as-you-go", rows.spend_budget),
            ],
            selected,
        ),
        // Same two values as `quota spent` on purpose: the pairing is the point.
        // Only the default differs — staying is free there and costs money here.
        // Inert until `allow extra usage` is on: nothing spends, so nothing halts on a
        // spent budget. Rendered dimmed AND the key no-ops (a true disabled row),
        // so `faint` never decouples from "not editable".
        GlobalConfigRow::SwitchOffWhenBudgetSpent => {
            let options = [
                ("stay on active", !rows.switch_off_when_budget_spent),
                ("switch off all", rows.switch_off_when_budget_spent),
            ];
            if rows.spend_budget {
                cycle_row(arrow, "extra usage spent", &options, selected)
            } else {
                dimmed_cycle_row("extra usage spent", &options, selected)
            }
        }
        GlobalConfigRow::PreemptiveRotation => {
            let options = [("lazy", !rows.preemptive), ("preemptive", rows.preemptive)];
            cycle_row(arrow, "rotation", &options, selected)
        }
        GlobalConfigRow::RefreshSpentAccounts => {
            toggle_row(arrow, "refresh spent", rows.refresh_spent, selected)
        }
        // Inert until some account opts into `auto_start` (rendered dimmed):
        // a queue with no possible member spaces nothing, so it stays a true
        // disabled row — the key is a no-op, and `faint` never decouples from
        // "not editable" (the `extra usage spent` contract above).
        GlobalConfigRow::AutoStartQueue => {
            if rows.any_auto_start {
                toggle_row(arrow, "auto-start queue", rows.auto_start_queue, selected)
            } else {
                dimmed_toggle_row("auto-start queue", rows.auto_start_queue, selected)
            }
        }
    }
}

/// Trailing faint ` default: X` reminder appended to a value row only when its
/// live value is off the shipped default — the operator sees the default at the
/// moment they've moved away from it, and never as noise otherwise (the chain
/// `rotate at` idiom).
pub(super) fn default_reminder(value: String) -> Span<'static> {
    Span::styled(format!("   default: {value}"), theme::faint())
}

/// The presets the refresh row steps through, paired with their `ms` value.
/// Mirrors the `step_refresh_interval` ladder in `app.rs`.
const REFRESH_PRESETS: [(&str, u64); 6] = [
    ("15s", 15_000),
    ("30s", 30_000),
    ("60s", 60_000),
    ("90s", 90_000),
    ("120s", 120_000),
    ("300s", 300_000),
];

/// The `refresh` row at rest: a segmented control over [`REFRESH_PRESETS`]. A
/// chip is bracketed only when the interval **exactly** equals that preset; a
/// custom value (set via ⏎) matches none, so the real `<n>s` is appended in
/// `ACCENT` instead of mis-highlighting the nearest preset.
fn refresh_cycle_line(
    arrow: Span<'static>,
    refresh_interval_ms: u64,
    selected: bool,
) -> Line<'static> {
    let options: Vec<(&str, bool)> = REFRESH_PRESETS
        .iter()
        .map(|(label, ms)| (*label, *ms == refresh_interval_ms))
        .collect();
    let mut line = cycle_row(arrow, "refresh", &options, selected);
    if !REFRESH_PRESETS
        .iter()
        .any(|(_, ms)| *ms == refresh_interval_ms)
    {
        // 2 spaces + the last option's reserved trailing cell = the same 3-cell
        // gap the options keep between themselves.
        line.push_span(Span::styled(
            format!("  {}s", refresh_interval_ms / 1000),
            theme::accent(),
        ));
    }
    if refresh_interval_ms != DEFAULT_REFRESH_INTERVAL_MS {
        line.push_span(default_reminder(format!(
            "{}s",
            DEFAULT_REFRESH_INTERVAL_MS / 1000
        )));
    }
    line
}

/// The `refresh` row mid-edit: edit gutter + `refresh` key block + the typed
/// buffer (DANGER when out of range) + ` s` unit. The terminal cursor owns the
/// caret, so the buffer renders with uniform styling — no simulated block cursor.
fn refresh_edit_line(arrow: Span<'static>, input: &InputState) -> Line<'static> {
    let invalid = parse_refresh_secs(input.trimmed()).is_none();
    let mut spans = vec![
        arrow,
        Span::styled(key_cell("refresh", KEY_W, KEY_GUTTER), label_style(true)),
    ];
    spans.extend(value_caret(input, invalid));
    let unit_style = if invalid {
        theme::danger()
    } else {
        theme::faint()
    };
    spans.push(Span::styled(" s", unit_style));
    Line::from(spans)
}

/// Sub-line under the refresh field while typing: the valid range, in DANGER
/// when the current buffer parses out of range (or non-numeric), else faint.
fn refresh_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = format!(
        "{}-{} s",
        MIN_REFRESH_INTERVAL_MS / 1000,
        MAX_REFRESH_INTERVAL_MS / 1000
    );
    if parse_refresh_secs(input.trimmed()).is_none() {
        invalid_tooltip_lines(&range, width)
    } else {
        help_tooltip_lines(&range, width)
    }
}

/// The `weekly limit` row at rest: a segmented control over
/// [`WEEKLY_PRESETS`], with a custom value (set via ⏎) appended in `ACCENT`
/// when it matches no preset — same grammar as the refresh row.
fn weekly_cycle_line(arrow: Span<'static>, weekly_pct: f64, selected: bool) -> Line<'static> {
    let labels: Vec<String> = WEEKLY_PRESETS
        .iter()
        .map(|p| format!("{}%", format_weekly_pct(*p)))
        .collect();
    let options: Vec<(&str, bool)> = labels
        .iter()
        .zip(WEEKLY_PRESETS.iter())
        .map(|(label, p)| (label.as_str(), *p == weekly_pct))
        .collect();
    let mut line = cycle_row(arrow, "weekly limit", &options, selected);
    if !WEEKLY_PRESETS.contains(&weekly_pct) {
        line.push_span(Span::styled(
            format!("  {}%", format_weekly_pct(weekly_pct)),
            theme::accent(),
        ));
    }
    if (weekly_pct - DEFAULT_WEEKLY_SWITCH_PCT).abs() > f64::EPSILON {
        line.push_span(default_reminder(format!(
            "{}%",
            format_weekly_pct(DEFAULT_WEEKLY_SWITCH_PCT)
        )));
    }
    line
}

/// The `weekly limit` row mid-edit: edit gutter + key block + typed buffer
/// (DANGER when out of range) + ` %` unit. Mirrors `refresh_edit_line`.
fn weekly_edit_line(arrow: Span<'static>, input: &InputState) -> Line<'static> {
    let invalid = parse_weekly_pct(input.trimmed()).is_none();
    let mut spans = vec![
        arrow,
        Span::styled(
            key_cell("weekly limit", KEY_W, KEY_GUTTER),
            label_style(true),
        ),
    ];
    spans.extend(value_caret(input, invalid));
    let unit_style = if invalid {
        theme::danger()
    } else {
        theme::faint()
    };
    spans.push(Span::styled(" %", unit_style));
    Line::from(spans)
}

/// Sub-line under the weekly field while typing: the valid range, in DANGER
/// when the buffer parses out of range, else faint.
fn weekly_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "50-100 %";
    if parse_weekly_pct(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

/// The `burn floor` row: burn-aware early-switch floor as a segmented control
/// over [`BURN_FLOOR_PRESETS`]. Dimmed + inert when burn-aware is off (the
/// projection it gates never runs), mirroring the `extra usage spent` row. A
/// hand-edited in-band value matching no preset is appended in `ACCENT`, same
/// grammar as the weekly row.
fn burn_floor_line(
    arrow: Span<'static>,
    floor_pct: f64,
    selected: bool,
    burn_aware: bool,
) -> Line<'static> {
    let labels: Vec<String> = BURN_FLOOR_PRESETS
        .iter()
        .map(|p| format!("{}%", format_weekly_pct(*p)))
        .collect();
    let options: Vec<(&str, bool)> = labels
        .iter()
        .zip(BURN_FLOOR_PRESETS.iter())
        .map(|(label, p)| (label.as_str(), *p == floor_pct))
        .collect();
    if !burn_aware {
        return dimmed_cycle_row("burn floor", &options, selected);
    }
    let mut line = cycle_row(arrow, "burn floor", &options, selected);
    if !BURN_FLOOR_PRESETS.contains(&floor_pct) {
        line.push_span(Span::styled(
            format!("  {}%", format_weekly_pct(floor_pct)),
            theme::accent(),
        ));
    }
    if (floor_pct - DEFAULT_BURN_FLOOR_PCT).abs() > f64::EPSILON {
        line.push_span(default_reminder(format!(
            "{}%",
            format_weekly_pct(DEFAULT_BURN_FLOOR_PCT)
        )));
    }
    line
}

/// The `burn horizon` row: burn-aware projection look-ahead cap as a segmented
/// control over [`BURN_HORIZON_PRESETS`] (labelled in seconds). Dimmed + inert
/// when burn-aware is off. Custom in-band value appended in `ACCENT`.
fn burn_horizon_line(
    arrow: Span<'static>,
    horizon_ms: u64,
    selected: bool,
    burn_aware: bool,
) -> Line<'static> {
    let labels: Vec<String> = BURN_HORIZON_PRESETS
        .iter()
        .map(|ms| format!("{}s", ms / 1000))
        .collect();
    let options: Vec<(&str, bool)> = labels
        .iter()
        .zip(BURN_HORIZON_PRESETS.iter())
        .map(|(label, ms)| (label.as_str(), *ms == horizon_ms))
        .collect();
    if !burn_aware {
        return dimmed_cycle_row("burn horizon", &options, selected);
    }
    let mut line = cycle_row(arrow, "burn horizon", &options, selected);
    if !BURN_HORIZON_PRESETS.contains(&horizon_ms) {
        line.push_span(Span::styled(
            format!("  {}s", horizon_ms / 1000),
            theme::accent(),
        ));
    }
    if horizon_ms != DEFAULT_BURN_HORIZON_MS {
        line.push_span(default_reminder(format!(
            "{}s",
            DEFAULT_BURN_HORIZON_MS / 1000
        )));
    }
    line
}

/// A cloudy-tui cycle row: `key  label  [active]  other`. Options are bare
/// labels separated by 2-space gaps; the active option is `ACCENT` and wraps in
/// `[]` only while the row holds the cursor, the rest stay `TEXT_FAINT`. `space`
/// cycles the value in place. Reads as the segmented control it is, instead of
/// a single value that silently swaps text on cycle.
fn cycle_row(
    arrow: Span<'static>,
    key: &str,
    options: &[(&str, bool)],
    row_selected: bool,
) -> Line<'static> {
    let mut spans = vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(row_selected)),
    ];
    for (i, (label, active)) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(cycle_option(label, *active, row_selected));
    }
    Line::from(spans)
}

/// A cloudy-tui Disabled row for a cycle setting another toggle makes inert: the
/// whole row (caret, key, current value) renders `TEXT_FAINT`, no bracket
/// highlight — just the current value. Focusable but inert (the key handler
/// no-ops it), so `TEXT_FAINT` keeps meaning "can't touch this". The `draw` loop
/// still tints + shows the `└` reason on focus.
fn dimmed_cycle_row(key: &str, options: &[(&str, bool)], selected: bool) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::faint())
    } else {
        Span::raw("  ")
    };
    let value = options
        .iter()
        .find(|(_, active)| *active)
        .map(|(label, _)| *label)
        .unwrap_or("");
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::faint()),
        Span::styled(value.to_string(), theme::faint()),
    ])
}

/// [`dimmed_cycle_row`]'s toggle sibling: the whole row — caret, key, knob —
/// renders `TEXT_FAINT`, keeping the current value visible with no accent even
/// while on. Focusable but inert (the key handler no-ops it), so `TEXT_FAINT`
/// keeps meaning "can't touch this".
fn dimmed_toggle_row(key: &str, on: bool, selected: bool) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::faint())
    } else {
        Span::raw("  ")
    };
    let glyph = if on {
        theme::toggle_on()
    } else {
        theme::toggle_off()
    };
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::faint()),
        Span::styled(glyph, theme::faint()),
    ])
}

/// A cloudy-tui toggle row: `key  ─●` / `key  ○─`. A pure on/off boolean is a
/// toggle, not a 2-option cycle — `on`/`off` labels in brackets read as a cycle,
/// not the switch the contract draws. Knob `ACCENT` when on, `TEXT_FAINT` off.
fn toggle_row(arrow: Span<'static>, key: &str, on: bool, row_selected: bool) -> Line<'static> {
    let (glyph, style) = if on {
        (theme::toggle_on(), theme::accent())
    } else {
        (theme::toggle_off(), theme::faint())
    };
    Line::from(vec![
        arrow,
        Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(row_selected)),
        Span::styled(glyph, style),
    ])
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_global_config.rs"]
mod tests;
