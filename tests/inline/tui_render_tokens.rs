//! Tokens-dashboard render tests: the height-aware bar charts, the full-width
//! loading spinner, the honest activity caption, and the granularity badges.

use super::{
    INDET_BLOCK, activity_lines, bar_chart, bar_chart_sqrt, bucket_layout, comp_rows,
    day_cost_split, determinate_bar, hour_lines, indeterminate_bar, lifetime_cost,
    model_detail_cost, model_lines, today_cost, today_lines, total_lines, trend_lines, window_cost,
};
use crate::pricing::{
    Constraint, HourTokens, PriceEntry, PriceTable, PricedModel, RateSnapshot, SpeedMode,
};
use crate::profile::{AppConfig, AppState};
use crate::tokens::{
    DayActivity, DaySummary, DayTokens, HourlyModel, ModelTokens, PeriodDay, PeriodModel,
    TokenStats,
};
use crate::tui::app::{App, Tab, TokenPeriod};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Style;
use ratatui::text::Line;

/// Flatten a `Line`'s spans back into one string (pad spans included).
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// ── price-table builders ─────────────────────────────────────────────────────

/// One exact-match model from its price entries (ids resolve
/// case-insensitively).
fn priced_model(id: &str, entries: Vec<PriceEntry>) -> PricedModel {
    PricedModel {
        id: id.to_owned(),
        prices: entries,
        effective_at: None,
    }
}

/// One unconstrained price entry at the given per-token rates.
fn flat_entry(input: f64, output: f64, cache_read: f64, cache_write: f64) -> PriceEntry {
    PriceEntry {
        input,
        output,
        cache_read,
        cache_write,
        constraint: None,
        mode: None,
        window_only: false,
    }
}

/// A flat (one unconstrained entry) model at the given per-token rates.
fn flat_model(id: &str, input: f64, output: f64, cache_read: f64, cache_write: f64) -> PricedModel {
    priced_model(id, vec![flat_entry(input, output, cache_read, cache_write)])
}

/// deepseek-chat-shaped pricing: an off-peak fallback entry plus a peak
/// `00:30–16:30Z` time-window entry, which reversed-entry selection prefers
/// while it is active. Per the settled hour-granularity formula hour 0 prices
/// off-peak and hour 16 peak.
fn windowed_model(id: &str, peak: (f64, f64), off_peak: (f64, f64)) -> PricedModel {
    priced_model(
        id,
        vec![
            flat_entry(off_peak.0, off_peak.1, 0.0, 0.0),
            PriceEntry {
                input: peak.0,
                output: peak.1,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "00:30:00Z".to_owned(),
                    end: "16:30:00Z".to_owned(),
                }),
                mode: None,
                window_only: false,
            },
        ],
    )
}

/// A table from `models`, captured today with no history.
fn table_of(models: Vec<PricedModel>) -> PriceTable {
    PriceTable::capture(
        models,
        Vec::new(),
        Vec::new(),
        crate::pricing::CanonicalMap::default(),
        crate::tokens::today_date(),
        0,
        Vec::new(),
    )
}

/// A table whose newest snapshot is `models` and whose older snapshots are
/// given explicitly (`capture` appends nothing — the last history entry must
/// equal `models`).
fn table_with_history(models: Vec<PricedModel>, history: Vec<RateSnapshot>) -> PriceTable {
    PriceTable::capture(
        models,
        Vec::new(),
        Vec::new(),
        crate::pricing::CanonicalMap::default(),
        crate::tokens::today_date(),
        0,
        history,
    )
}

// ── period-row builders ──────────────────────────────────────────────────────

/// A one-model period row summing `days`.
fn period_row(model: &str, days: Vec<PeriodDay>) -> PeriodModel {
    let split = days.iter().fold(
        ModelTokens {
            model: model.to_owned(),
            ..Default::default()
        },
        |mut acc, d| {
            acc.input += d.split.input;
            acc.output += d.split.output;
            acc.cache_read += d.split.cache_read;
            acc.cache_create += d.split.cache_create;
            acc.shape = acc.shape.merge(d.split.shape);
            acc
        },
    );
    PeriodModel {
        model: model.to_owned(),
        in_out: split.in_out(),
        split,
        split_complete: true,
        days,
    }
}

/// An unhoured day row (the v1-ledger / stats-cache shape).
fn unhoured_day(date: &str, model: &str, input: u64, output: u64) -> PeriodDay {
    let split = ModelTokens {
        model: model.to_owned(),
        input,
        output,
        ..Default::default()
    };
    PeriodDay {
        date: date.to_owned(),
        split,
        hours: None,
    }
}

/// An hour-bearing day row with input/output tokens in the named hours.
fn hourly_day(date: &str, model: &str, buckets: &[(usize, u64, u64)]) -> PeriodDay {
    let mut hours = [HourTokens::default(); 24];
    let mut input = 0;
    let mut output = 0;
    for &(hour, i, o) in buckets {
        hours[hour] = HourTokens {
            input: i,
            output: o,
            ..Default::default()
        };
        input += i;
        output += o;
    }
    let split = ModelTokens {
        model: model.to_owned(),
        input,
        output,
        ..Default::default()
    };
    PeriodDay {
        date: date.to_owned(),
        split,
        hours: Some(hours),
    }
}

/// A `TokenStats` whose today card carries one model's hourly buckets.
fn day_with_model_hours(model: &str, hours: [HourTokens; 24]) -> DaySummary {
    DaySummary {
        date: crate::tokens::today_date(),
        model_hours: vec![HourlyModel {
            model: model.to_owned(),
            hours,
        }],
        ..Default::default()
    }
}

/// A `TokenStats` whose today card carries one model's hourly buckets.
fn stats_with_today(model: &str, hours: [HourTokens; 24]) -> TokenStats {
    TokenStats {
        today: Some(day_with_model_hours(model, hours)),
        ..Default::default()
    }
}

/// Flatten a rendered `TestBackend` buffer to one string of cell symbols.
fn render_dashboard(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer()).concat()
}

fn populated_stats() -> TokenStats {
    let daily: Vec<DayTokens> = (0..21)
        .map(|i| DayTokens {
            date: format!("2026-06-{:02}", i + 1),
            tokens: 1_000_000 + (i as u64 % 7) * 3_000_000,
        })
        .collect();
    let activity: Vec<DayActivity> = (0..21)
        .map(|i| DayActivity {
            date: format!("2026-06-{:02}", i + 1),
            messages: 100 + (i as u64 % 5) * 400,
            sessions: 3 + (i as u64 % 4),
            tool_calls: 50 + (i as u64 % 6) * 200,
        })
        .collect();
    let mut hour_counts = [0u64; 24];
    for (h, c) in hour_counts.iter_mut().enumerate() {
        *c = (h as u64 * 7) % 90;
    }
    TokenStats {
        models: vec![ModelTokens {
            model: "claude-opus-4-8".into(),
            input: 30_000_000,
            output: 70_000_000,
            cache_read: 4_000_000_000,
            cache_create: 500_000_000,
            shape: Default::default(),
            ..Default::default()
        }],
        daily,
        activity,
        hour_counts,
        total_input: 100_000_000,
        total_output: 70_000_000,
        total_sessions: 1000,
        total_messages: 200_000,
        first_session_date: Some("2026-01-18T00:00:00Z".into()),
        ..Default::default()
    }
}

fn app_with_stats(period: TokenPeriod) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Tokens;
    app.token_period = period;
    app.token_stats = Some(populated_stats());
    app
}

// ── bar_chart ─────────────────────────────────────────────────────────────────

#[test]
fn bar_chart_peak_fills_the_full_height() {
    // Three columns, the max value (10) in column 0, at 4 rows tall.
    let lines = bar_chart(&[10, 0, 5], 3, 4, Style::default(), 1, 0);
    assert_eq!(lines.len(), 4, "one line per chart row");
    for l in &lines {
        assert_eq!(
            line_text(l).chars().next().unwrap(),
            '█',
            "the peak column is a full block in every row"
        );
    }
    // A half-height column is filled only in its lower rows.
    assert_eq!(line_text(&lines[0]).chars().nth(2).unwrap(), ' ');
    assert_eq!(line_text(&lines[3]).chars().nth(2).unwrap(), '█');
}

#[test]
fn bar_chart_zero_series_renders_a_baseline() {
    let lines = bar_chart(&[0, 0, 0], 3, 3, Style::default(), 1, 0);
    assert_eq!(lines.len(), 3);
    assert_eq!(
        line_text(&lines[2]),
        "▁▁▁",
        "an all-zero series shows a flat baseline on the bottom row"
    );
    assert_eq!(
        line_text(&lines[0]),
        "   ",
        "rows above the baseline are blank"
    );
}

#[test]
fn bar_chart_short_series_is_left_padded() {
    // Two full-height columns in a 6-wide chart → centered by a 2-cell left pad.
    let lines = bar_chart(&[5, 5], 6, 2, Style::default(), 1, 0);
    assert!(
        line_text(&lines[0]).starts_with("  ██"),
        "a short series left-pads the bars, got {:?}",
        line_text(&lines[0])
    );
}

// ── sqrt scale ────────────────────────────────────────────────────────────────

/// Rows of a column occupied by any glyph (full or partial), bottom-up.
fn col_cells(lines: &[Line<'_>], col: usize) -> usize {
    lines
        .iter()
        .filter(|l| line_text(l).chars().nth(col) != Some(' '))
        .count()
}

#[test]
fn bar_chart_sqrt_peak_alone_fills_the_full_height() {
    // One 100× outlier next to quiet days: only the peak column reaches the
    // top row — no p95-style wall of identical full-height columns.
    let mut vals = vec![9_u64; 19];
    vals.push(100);
    let lines = bar_chart_sqrt(&vals, 20, 8, Style::default(), 1, 0);
    assert_eq!(
        line_text(&lines[0]).chars().nth(19).unwrap(),
        '█',
        "the peak column reaches the top row"
    );
    assert_eq!(
        line_text(&lines[0]).chars().filter(|&c| c != ' ').count(),
        1,
        "no other column joins the peak at the top"
    );
}

#[test]
fn bar_chart_sqrt_lifts_quiet_days_above_linear() {
    // 9% of the peak: linear leaves it inside the bottom cell; sqrt gives it
    // ~30% of the height so months of normal use stay readable.
    let vals = [9_u64, 100];
    let linear = bar_chart(&vals, 2, 8, Style::default(), 1, 0);
    let sqrt = bar_chart_sqrt(&vals, 2, 8, Style::default(), 1, 0);
    assert_eq!(col_cells(&linear, 0), 1, "linear flattens the quiet column");
    assert!(
        col_cells(&sqrt, 0) >= 2,
        "sqrt keeps the quiet column readable, got {} cells",
        col_cells(&sqrt, 0)
    );
}

#[test]
fn bar_chart_nonzero_keeps_the_floor_cell() {
    // 1/10_000 of the peak rounds to zero cells; a real day still shows the
    // ▁ floor instead of vanishing, while a true zero day stays blank.
    let lines = bar_chart(&[1, 0, 10_000], 3, 2, Style::default(), 1, 0);
    assert_eq!(
        line_text(&lines[1]),
        "▁ █",
        "nonzero floors at ▁, zero stays blank"
    );
}

// ── dashboard width clamp + TOTAL card ────────────────────────────────────────

#[test]
fn dashboard_reflows_to_two_columns_on_big_terminals() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_stats(TokenPeriod::Lifetime);
    let out = render_dashboard(&app, 160, 40);
    let row = |y: usize| -> String { out.chars().skip(y * 160).take(160).collect() };
    assert_eq!(
        row(0).chars().next(),
        Some('╭'),
        "the card column starts at the left edge (no centering margin)"
    );
    assert!(
        row(0).contains(" DAILY "),
        "the trend card shares the top row"
    );
    assert!(
        !row(0).contains(" TOTAL "),
        "total stacks under the first card instead of sitting beside it"
    );
    assert!(
        row(7).contains(" TOTAL "),
        "total is the second left-column card"
    );
    let act_row = (0..40).find(|&y| row(y).contains(" ACTIVITY "));
    assert!(
        act_row.is_some_and(|y| y >= 20),
        "activity sits in the lower right half, got row {act_row:?}"
    );
}

#[test]
fn dashboard_clamps_to_a_centered_band_on_wide_short_terminals() {
    let _home = crate::testutil::HomeSandbox::new();
    // Wide but under the 30-row reflow gate → the single-column centered band.
    let app = app_with_stats(TokenPeriod::Lifetime);
    let out = render_dashboard(&app, 160, 24);
    let row0: String = out.chars().take(160).collect();
    assert!(
        row0.starts_with(&" ".repeat(20)),
        "the left margin outside the 120-col band stays blank"
    );
    assert_eq!(
        row0.chars().nth(20),
        Some('╭'),
        "the first card's border opens at the band edge"
    );
    assert!(
        row0.trim_end().chars().count() <= 140,
        "the right margin outside the band stays blank"
    );
}

#[test]
fn total_card_groups_kv_rows_and_carries_the_range_meta() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_stats(TokenPeriod::Lifetime);
    let out = render_dashboard(&app, 120, 24);
    assert!(
        out.contains("jan 18 → jun 21"),
        "lifetime date range rides the title-right meta"
    );
    assert!(
        out.contains("jun 21 ─╮"),
        "the badge closes with a border cell before the corner"
    );
    assert!(out.contains("sessions"), "sessions is a spelled-out kv key");
    assert!(out.contains("1,000"), "session count is comma-grouped");
}

/// A focused card's border is one token: the dashes closing its meta badge
/// must not fall back to the blurred line color.
#[test]
fn a_focused_cards_meta_dashes_carry_the_focused_border() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_stats(TokenPeriod::Lifetime);
    app.token_dash_focus = crate::tui::app::CardKind::Total;
    let (w, h) = (120u16, 24u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);
    let y = rows
        .iter()
        .position(|r| r.contains(" TOTAL "))
        .expect("the total card renders");
    let cells: Vec<char> = rows[y].chars().collect();
    let title_x = rows[y]
        .find(" TOTAL ")
        .map(|b| rows[y][..b].chars().count());
    let title_x = title_x.expect("title column");
    let left = (0..title_x).rev().find(|&x| cells[x] == '╭').expect("╭");
    let right = (title_x..cells.len())
        .find(|&x| cells[x] == '╮')
        .expect("╮");
    let strong = crate::tui::theme::line_strong_color();
    let off: Vec<(usize, ratatui::style::Color)> = (left..=right)
        .filter_map(|x| {
            let cell = buf.cell((x as u16, y as u16)).unwrap();
            (matches!(cell.symbol(), "─" | "╭" | "╮") && cell.fg != strong).then_some((x, cell.fg))
        })
        .collect();
    assert!(
        rows[y].contains("jun 21 ─╮"),
        "the meta badge renders on the focused card, got {:?}",
        rows[y]
    );
    assert_eq!(
        off,
        Vec::new(),
        "every border cell of the focused card's top row is the focused border color"
    );
}

// ── model rows: unpriced cost dash ────────────────────────────────────────────

#[test]
fn model_lines_dash_unpriced_models_when_a_table_is_loaded() {
    let rows: Vec<PeriodModel> = [("claude-opus-4-8", 900_u64), ("glm-5.2", 800)]
        .iter()
        .map(|&(id, tokens)| {
            PeriodModel::from_full(&ModelTokens {
                model: id.into(),
                input: tokens,
                output: 0,
                cache_read: 0,
                cache_create: 0,
                shape: Default::default(),
                ..Default::default()
            })
        })
        .collect();
    let prices = table_of(vec![flat_model("claude-opus-4-8", 5e-6, 25e-6, 5e-7, 6e-6)]);

    let lines = model_lines(&rows, 60, 5, true, Some(&prices), "no model usage yet");
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert!(
        texts[0].contains('$'),
        "the priced model shows a cost, got {:?}",
        texts[0]
    );
    assert!(
        texts[1].trim_end().ends_with('—'),
        "the unpriced model shows the no-value dash, got {:?}",
        texts[1]
    );

    // No price table at all → the whole cost column stays hidden.
    let bare = model_lines(&rows, 60, 5, true, None, "no model usage yet");
    assert!(
        bare.iter().map(line_text).all(|t| !t.contains('—')),
        "no table → no dash column"
    );
}

// ── cost lens: hourly + dated resolution ─────────────────────────────────────

#[test]
fn day_cost_split_prices_the_fast_subset_at_the_fast_rate() {
    // The feed's opus-5-5 shape: a base entry plus a fast override at 2x.
    // The fast hour's subset prices through the fast walk, the standard
    // remainder through the standard walk.
    let table = table_of(vec![priced_model(
        "claude-opus-5-5",
        vec![
            flat_entry(4e-6, 2e-5, 2e-7, 5e-6),
            PriceEntry {
                input: 8e-6,
                output: 4e-5,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: None,
                mode: Some(SpeedMode::Fast),
                window_only: false,
            },
        ],
    )]);
    let mut hours = [HourTokens::default(); 24];
    hours[9] = HourTokens {
        input: 1_000_000,
        output: 1_000_000,
        fast_input: 1_000_000,
        fast_output: 1_000_000,
        ..Default::default()
    };
    hours[15] = HourTokens {
        input: 1_000_000,
        output: 1_000_000,
        ..Default::default()
    };
    let day = PeriodDay {
        date: "2026-10-29".to_owned(),
        split: ModelTokens {
            model: "claude-opus-5-5".into(),
            input: 2_000_000,
            output: 2_000_000,
            ..Default::default()
        },
        hours: Some(hours),
    };
    let s = day_cost_split(&table, &day).expect("priced");
    assert!(
        (s.input - 12.0).abs() < 1e-9,
        "got input ${}, expected $12 ($8 fast + $4 standard)",
        s.input
    );
    assert!(
        (s.output - 60.0).abs() < 1e-9,
        "got output ${}, expected $60 ($40 fast + $20 standard)",
        s.output
    );
}

#[test]
fn lifetime_total_prices_the_fast_subset_at_the_fast_rate() {
    // The feed's opus-5-5 shape: a base entry plus a fast override at 2x. The
    // flat lifetime aggregate carries its own fast subset, so the TOTAL card
    // and the detail card's date-less branch price it through the fast walk —
    // equal to the sum of the day views over the same tokens.
    let table = table_of(vec![priced_model(
        "claude-opus-5-5",
        vec![
            flat_entry(4e-6, 2e-5, 2e-7, 5e-6),
            PriceEntry {
                input: 8e-6,
                output: 4e-5,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: None,
                mode: Some(SpeedMode::Fast),
                window_only: false,
            },
        ],
    )]);
    // One hour of fast tokens (1M output) and one of standard (1M output):
    // $40 fast + $20 standard = $60 — not the all-standard $40.
    let mut hours = [HourTokens::default(); 24];
    hours[9] = HourTokens {
        output: 1_000_000,
        fast_output: 1_000_000,
        ..Default::default()
    };
    hours[15] = HourTokens {
        output: 1_000_000,
        ..Default::default()
    };
    let agg = ModelTokens {
        model: "claude-opus-5-5".into(),
        output: 2_000_000,
        fast_output: 1_000_000,
        ..Default::default()
    };

    let (total, floor) = lifetime_cost(Some(&table), std::slice::from_ref(&agg)).expect("priced");
    assert!(!floor, "every token priced");
    assert!(
        (total - 60.0).abs() < 1e-9,
        "got TOTAL ${total}, expected $60 ($40 fast + $20 standard)"
    );

    let m = PeriodModel::from_full(&agg);
    let (split, unpriced) = model_detail_cost(&table, &m).expect("priced");
    assert_eq!(unpriced, 0);
    assert!(
        (split.output - 60.0).abs() < 1e-9,
        "got detail output ${}, expected $60 ($40 fast + $20 standard)",
        split.output
    );

    let day = PeriodDay {
        date: "2026-10-29".to_owned(),
        split: agg.clone(),
        hours: Some(hours),
    };
    let s = day_cost_split(&table, &day).expect("priced");
    assert!(
        (s.output - total).abs() < 1e-9,
        "day split ${} != TOTAL ${}",
        s.output,
        total
    );
}

#[test]
fn today_card_prices_peak_and_off_peak_hours_at_their_tiers() {
    let table = table_of(vec![windowed_model(
        "deepseek-chat",
        (2e-6, 4e-6),
        (1e-6, 2e-6),
    )]);
    // 1000 input tokens in hour 0 (off-peak) and hour 16 (peak): $0.001 +
    // $0.002 = $0.003 — neither the all-off-peak ($0.002) nor the all-peak
    // ($0.004) flat total.
    let mut hours = [HourTokens::default(); 24];
    hours[0] = HourTokens {
        input: 1000,
        ..Default::default()
    };
    hours[16] = HourTokens {
        input: 1000,
        ..Default::default()
    };
    let stats = stats_with_today("deepseek-chat", hours);
    let lines = today_lines(&stats, 60, false, Some(&table), Line::default());
    let cost = line_text(&lines[1]);
    assert!(
        cost.contains("$0.003"),
        "mixed-hour day prices per hour, got {cost:?}"
    );
    assert!(
        !cost.contains('+'),
        "both hours priced ⇒ no floor mark, got {cost:?}"
    );

    // No table → the dash.
    let bare = today_lines(&stats, 60, false, None, Line::default());
    assert!(line_text(&bare[1]).contains('—'), "no table → dash");
}

#[test]
fn peak_window_boundary_hour_16_prices_peak() {
    let table = table_of(vec![windowed_model(
        "deepseek-chat",
        (2e-6, 4e-6),
        (1e-6, 2e-6),
    )]);
    // The settled hour-granularity formula samples the window at the hour's
    // start: 16:00 is inside 00:30–16:30, 0:00 is not.
    let mut peak = [HourTokens::default(); 24];
    peak[16] = HourTokens {
        input: 1000,
        ..Default::default()
    };
    let peak_lines = today_lines(
        &stats_with_today("deepseek-chat", peak),
        60,
        false,
        Some(&table),
        Line::default(),
    );
    assert!(
        line_text(&peak_lines[1]).contains("$0.002"),
        "hour 16 prices peak (2e-6), got {:?}",
        line_text(&peak_lines[1])
    );

    let mut off = [HourTokens::default(); 24];
    off[0] = HourTokens {
        input: 1000,
        ..Default::default()
    };
    let off_lines = today_lines(
        &stats_with_today("deepseek-chat", off),
        60,
        false,
        Some(&table),
        Line::default(),
    );
    assert!(
        line_text(&off_lines[1]).contains("$0.001"),
        "hour 0 prices off-peak (1e-6), got {:?}",
        line_text(&off_lines[1])
    );
}

#[test]
fn today_card_floors_unpriced_models_and_dashes_without_a_table() {
    let table = table_of(vec![flat_model("claude-opus-4-8", 1e-6, 2e-6, 0.0, 0.0)]);
    // Each bucket term alone must floor the day: glm-5.2 has no matching rate,
    // so whichever single term carries its tokens marks the figure as a floor.
    let buckets = [
        HourTokens {
            input: 1000,
            ..Default::default()
        },
        HourTokens {
            output: 1000,
            ..Default::default()
        },
        HourTokens {
            cache_read: 1000,
            ..Default::default()
        },
        HourTokens {
            cache_create: 1000,
            ..Default::default()
        },
    ];
    for b in buckets {
        let mut hours = [HourTokens::default(); 24];
        hours[10] = b;
        let (usd, floor) = today_cost(Some(&table), &day_with_model_hours("glm-5.2", hours))
            .expect("table present");
        assert_eq!(usd, 0.0);
        assert!(
            floor,
            "an unpriced model with only {b:?} tokens floors the day"
        );
    }
    // Rendered: the day total is a `+` floor; no table renders the dash.
    let mut hours = [HourTokens::default(); 24];
    hours[10] = HourTokens {
        input: 1000,
        ..Default::default()
    };
    let lines = today_lines(
        &stats_with_today("glm-5.2", hours),
        60,
        false,
        Some(&table),
        Line::default(),
    );
    assert!(
        line_text(&lines[1]).contains("$0+"),
        "unpriced token-bearing model floors the day, got {:?}",
        line_text(&lines[1])
    );
}

#[test]
fn period_cost_sums_each_day_at_its_dated_rate() {
    let cheap = flat_model("m", 1e-6, 0.0, 0.0, 0.0);
    let dear = flat_model("m", 2e-6, 0.0, 0.0, 0.0);
    let table = table_with_history(
        vec![dear.clone()],
        vec![
            RateSnapshot {
                captured: "2026-08-01".to_owned(),
                models: vec![cheap],
            },
            RateSnapshot {
                captured: "2026-08-10".to_owned(),
                models: vec![dear],
            },
        ],
    );
    // Two unhoured days straddling the 08-10 price change: each prices at the
    // snapshot live on its own date — $0.01 + $0.02 = $0.03, not $0.04 at the
    // newest rate for both.
    let row = period_row(
        "m",
        vec![
            unhoured_day("2026-08-05", "m", 10_000, 0),
            unhoured_day("2026-08-12", "m", 10_000, 0),
        ],
    );
    let lines = model_lines(&[row], 60, 5, true, Some(&table), "no model usage yet");
    let text = line_text(&lines[0]);
    assert!(
        text.contains("$0.03"),
        "each day prices at its dated snapshot, got {text:?}"
    );
    assert!(
        !text.trim_end().ends_with('+'),
        "fully priced ⇒ no floor mark, got {text:?}"
    );
}

#[test]
fn unhoured_day_prices_flat_at_default_tier_without_floor_mark() {
    let table = table_of(vec![windowed_model(
        "deepseek-chat",
        (2e-6, 4e-6),
        (1e-6, 2e-6),
    )]);
    // hours: None ⇒ the day's flat split prices at hour 0 (off-peak) on the
    // day's dated rate — the documented un-houred-day approximation. The
    // missing hours are NOT flagged with `+`: that mark keeps its existing
    // meanings only (split incomplete / unpriced with tokens).
    let row = period_row(
        "deepseek-chat",
        vec![unhoured_day("2026-08-15", "deepseek-chat", 10_000, 0)],
    );
    let lines = model_lines(&[row], 60, 5, true, Some(&table), "no model usage yet");
    let text = line_text(&lines[0]);
    assert!(
        text.contains("$0.01"),
        "flat at the hour-0 (off-peak) tier, got {text:?}"
    );
    assert!(
        !text.trim_end().ends_with('+'),
        "no per-row flag for missing hours, got {text:?}"
    );
}

#[test]
fn period_cost_floors_on_unpriced_days() {
    // One priced and one unpriced day for the SAME model: the priced day sums,
    // the unpriced token-bearing day marks the figure as a floor.
    let table = table_of(vec![flat_model("m", 1e-6, 0.0, 0.0, 0.0)]);
    let row = period_row(
        "m",
        vec![
            unhoured_day("2026-08-15", "m", 10_000, 0),
            unhoured_day("2026-08-16", "unknown", 10_000, 0),
        ],
    );
    let lines = model_lines(&[row], 60, 5, true, Some(&table), "no model usage yet");
    let text = line_text(&lines[0]);
    assert!(
        text.contains("$0.010+"),
        "an unpriced token-bearing day floors the row, got {text:?}"
    );
}

#[test]
fn window_cost_sums_rows_and_floors_unpriced_days() {
    let table = table_of(vec![flat_model("m", 1e-6, 0.0, 0.0, 0.0)]);
    let priced = period_row("m", vec![unhoured_day("2026-08-15", "m", 10_000, 0)]);
    let unpriced = period_row(
        "unknown",
        vec![unhoured_day("2026-08-15", "unknown", 10_000, 0)],
    );
    // Only the priced row contributes; the unpriced token-bearing row floors
    // the window figure.
    let (usd, floor) =
        window_cost(Some(&table), &[priced.clone(), unpriced], false).expect("table present");
    assert!((usd - 0.01).abs() < 1e-9, "got {usd}");
    assert!(floor, "unpriced token-bearing rows floor the window");
    // An incomplete split window floors even when every day is priced.
    let (usd, floor) =
        window_cost(Some(&table), std::slice::from_ref(&priced), true).expect("table present");
    assert!((usd - 0.01).abs() < 1e-9);
    assert!(floor, "incomplete splits floor the window");
    // No table → the dash.
    assert!(window_cost(None, &[priced], false).is_none());
}

#[test]
fn today_card_hourly_sum_matches_the_flat_model_totals() {
    // A flat (hour-independent) rate: the card's per-hour walk over
    // `model_hours` must price exactly what the flat `models` totals would —
    // pinning that the buckets sum to the flat split and that the walk covers
    // every model in `models`.
    let table = table_of(vec![flat_model("m", 1e-6, 2e-6, 1e-7, 1.25e-6)]);
    let mut hours = [HourTokens::default(); 24];
    hours[3] = HourTokens {
        input: 500_000,
        output: 250_000,
        cache_read: 100,
        cache_create: 200,
        ..Default::default()
    };
    hours[20] = HourTokens {
        input: 500_000,
        output: 750_000,
        cache_read: 900,
        cache_create: 800,
        ..Default::default()
    };
    let t = DaySummary {
        date: crate::tokens::today_date(),
        model_hours: vec![HourlyModel {
            model: "m".to_owned(),
            hours,
        }],
        models: vec![ModelTokens {
            model: "m".to_owned(),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000,
            cache_create: 1_000,
            shape: Default::default(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (usd, floor) = today_cost(Some(&table), &t).expect("table present");
    assert!(!floor, "the single priced model is not a floor");
    // 1.0 (in) + 2.0 (out) + 0.0001 (read) + 0.00125 (create) = 3.00135 —
    // the same figure pricing the flat totals at hour 0 would produce.
    assert!((usd - 3.00135).abs() < 1e-9, "got {usd}");
}

#[test]
fn today_card_prices_each_model_in_model_hours() {
    let table = table_of(vec![flat_model("m", 1e-6, 0.0, 0.0, 0.0)]);
    // model_hours carries one entry per model in `models`: the priced model
    // sums, the unpriced token-bearing model floors the day figure.
    let mut priced_hours = [HourTokens::default(); 24];
    priced_hours[10] = HourTokens {
        input: 1_000_000,
        ..Default::default()
    };
    let mut unpriced_hours = [HourTokens::default(); 24];
    unpriced_hours[10] = HourTokens {
        input: 500_000,
        ..Default::default()
    };
    let t = DaySummary {
        date: crate::tokens::today_date(),
        model_hours: vec![
            HourlyModel {
                model: "m".to_owned(),
                hours: priced_hours,
            },
            HourlyModel {
                model: "unknown".to_owned(),
                hours: unpriced_hours,
            },
        ],
        models: vec![],
        ..Default::default()
    };
    let (usd, floor) = today_cost(Some(&table), &t).expect("table present");
    assert!((usd - 1.0).abs() < 1e-9, "got {usd}");
    assert!(floor, "the unpriced model's tokens floor the day");
}

#[test]
fn model_detail_total_floors_mixed_priced_days() {
    let table = table_of(vec![flat_model("m", 1e-6, 0.0, 0.0, 0.0)]);
    // The same model priced on one day and unpriced on another: the split
    // carries both days' tokens, the cost prices only the priced day, and the
    // total renders as a `+` floor.
    let row = period_row(
        "m",
        vec![
            unhoured_day("2026-08-15", "m", 10_000, 0),
            unhoured_day("2026-08-16", "unknown", 10_000, 0),
        ],
    );
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&row),
            20_000,
            (Some(&table), false),
            TokenPeriod::Weekly,
            None,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        out.contains("$0.010+"),
        "the total renders as a floor over the priced day, got: {out}"
    );
}

/// The Models detail is a detail pane: its token counts print full grouped
/// integers with thousands separators, never the dashboard's compact `1.2M`.
#[test]
fn model_detail_counts_print_full_grouped_integers() {
    let split = ModelTokens {
        model: "m".into(),
        input: 1_234_567,
        output: 234_567,
        cache_read: 345_678,
        cache_create: 45_678,
        ..Default::default()
    };
    let model = PeriodModel::from_full(&split);
    let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&model),
            0,
            (None, false),
            TokenPeriod::Lifetime,
            None,
        );
    })
    .unwrap();
    let rows: Vec<String> = crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .map(|r| r.trim_end().to_string())
        .collect();
    let row = |label: &str| {
        rows.iter()
            .find(|r| r.starts_with(&format!("│ {label}")))
            .map(|r| {
                r.trim()
                    .trim_start_matches('│')
                    .trim_end_matches('│')
                    .trim()
                    .to_string()
            })
            .unwrap_or_else(|| panic!("{label} row renders: {rows:?}"))
    };
    assert_eq!(row("input"), "input       1,234,567");
    assert_eq!(row("output"), "output        234,567");
    assert_eq!(row("cache read"), "cache read    345,678");
    assert_eq!(row("cache write"), "cache write    45,678");
    assert_eq!(row("total"), "total       1,860,490");
    assert_eq!(row("in+out"), "in+out      1,469,134");
}

#[test]
fn priced_split_model_detail_scroll_reaches_cache_hit_at_eighty_by_fourteen() {
    use crate::tui::app::{TokenView, handle_key};
    use ratatui::crossterm::event::KeyCode;

    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_stats(TokenPeriod::Lifetime);
    app.token_stats.as_mut().unwrap().models = vec![ModelTokens {
        model: "claude-opus-4-8".into(),
        input: 10,
        output: 20,
        cache_read: 30,
        cache_create: 40,
        ..Default::default()
    }];
    app.price_table = Some(table_of(vec![flat_model(
        "claude-opus-4-8",
        0.001,
        0.002,
        0.003,
        0.004,
    )]));
    app.token_view = TokenView::Models;
    let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let before = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        !before.concat().contains("CACHE HIT"),
        "cache-hit tail starts clipped"
    );
    handle_key(&mut app, crate::testutil::key(KeyCode::Enter));
    for _ in 0..30 {
        handle_key(&mut app, crate::testutil::key(KeyCode::Down));
    }
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let after = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        after.concat().contains("CACHE HIT"),
        "the priced model's last section must be reachable"
    );
    assert_eq!(
        app.token_model_cursor, 0,
        "detail scrolling retains the model"
    );
    assert_eq!(
        after[11].chars().skip(24).take(11).collect::<String>(),
        "│ CACHE HIT",
        "the final section is visible at the bottom"
    );
    assert_eq!(
        after[12].chars().last(),
        Some('│'),
        "the pane keeps its right border"
    );
    let (_, detail) = super::master_detail(ratatui::layout::Rect::new(0, 0, 80, 14), 1);
    assert_eq!(
        term.backend()
            .buffer()
            .cell((detail.x + detail.width - 2, 12))
            .unwrap()
            .symbol(),
        "┃",
        "the scrollbar reaches the detail bottom"
    );
    assert_eq!(
        term.backend().buffer().cell((detail.x, 0)).unwrap().fg,
        crate::tui::theme::line_strong_color(),
        "the detail border receives focus"
    );
    assert_eq!(
        term.backend().buffer().cell((0, 0)).unwrap().fg,
        crate::tui::theme::line_color(),
        "the selector border is blurred"
    );
    term.backend_mut().resize(45, 24);
    term.autoresize().unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let resized = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        resized.concat().contains("CACHE HIT"),
        "the narrow stacked detail still shows the last section"
    );
    let (_, narrow_detail) = super::master_detail(ratatui::layout::Rect::new(0, 0, 45, 24), 1);
    assert_eq!(
        term.backend()
            .buffer()
            .cell((
                narrow_detail.x + narrow_detail.width - 2,
                narrow_detail.y + narrow_detail.height - 2
            ))
            .unwrap()
            .symbol(),
        "┃",
        "the resized thumb stays at the bottom"
    );
}

#[test]
fn short_model_detail_has_no_scrollbar() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_stats(TokenPeriod::Lifetime);
    app.token_view = crate::tui::app::TokenView::Models;
    let mut term = Terminal::new(TestBackend::new(80, 40)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let (_, detail) = super::master_detail(ratatui::layout::Rect::new(0, 0, 80, 40), 1);
    let col = detail.x + detail.width - 2;
    assert_eq!(
        app.model_detail_max_scroll.get(),
        0,
        "fitting model has no scroll bound"
    );
    assert_eq!(
        term.backend().buffer().cell((col, 10)).unwrap().symbol(),
        " ",
        "no scrollbar track when content fits"
    );
}

/// The model detail is titled by the selected model's own display name, so it
/// keeps that name's case (the Panel proper-noun exception); with no model
/// selected it falls back to the uppercase structural `MODEL` label. Pinned
/// through the whole Tokens tab, so the `model` the detail is titled with is
/// whatever `draw_models` hands it — the empty list's `None` included.
#[test]
fn the_model_detail_title_keeps_the_selected_models_case() {
    use crate::tui::app::TokenView;

    fn tab_rows(app: &App) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(160, 40)).unwrap();
        term.draw(|f| super::draw(f, f.area(), app)).unwrap();
        crate::testutil::buffer_rows(term.backend().buffer())
    }

    let _home = crate::testutil::HomeSandbox::new();
    // An unrecognized id passes `model_display_name` through untouched, so
    // "keeps the case" must mean exactly these bytes — a forced lowercase
    // would pass an all-lowercase fixture and a forced uppercase a structural
    // label.
    let mut app = app_with_stats(TokenPeriod::Lifetime);
    app.token_stats.as_mut().unwrap().models[0].model = "Llama-4-Maverick".into();
    app.token_view = TokenView::Models;
    let rows = tab_rows(&app);
    let title = rows
        .iter()
        .find(|r| r.contains("╭─ Llama-4-Maverick"))
        .expect("the detail title renders");
    assert!(
        title.contains("╭─ Llama-4-Maverick "),
        "the selected model's identity keeps its case: {title:?}"
    );

    // No model in the list → the structural `MODEL` title stays uppercase.
    let mut bare = app_with_stats(TokenPeriod::Lifetime);
    bare.token_stats.as_mut().unwrap().models.clear();
    bare.token_view = TokenView::Models;
    let rows = tab_rows(&bare);
    assert!(
        rows.iter().any(|r| r.contains("─ MODEL ")),
        "the empty list's structural title is uppercase: {rows:?}"
    );
    assert!(
        rows.concat().contains("no model data"),
        "the empty detail still renders its placeholder"
    );
}

#[test]
fn cost_lens_reads_rates_unavailable_after_a_failed_fetch() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_stats(TokenPeriod::Lifetime);
    app.token_view = crate::tui::app::TokenView::Models;
    // The fetch failed before any table landed: the flag is set, the table
    // absent, so the cost lens must not claim it is still loading.
    app.price_failed = true;
    let out = render_dashboard(&app, 100, 44);
    assert!(out.contains("rates unavailable"), "got: {out}");
}

#[test]
fn cost_lens_reads_rates_loading_before_any_pricing_result() {
    let row = period_row("m", vec![unhoured_day("2026-08-15", "m", 10_000, 0)]);
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&row),
            10_000,
            (None, false),
            TokenPeriod::Weekly,
            None,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(out.contains("rates loading"), "got: {out}");
}

#[test]
fn price_failed_clears_on_loaded() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    // Nothing must spawn a bootstrap thread out of this tick.
    app.bootstrap_started = true;
    app.price_failed = true;
    // The test build drops the pricing worker's sender, so wire a live channel
    // in for the `Loaded` to reach the tick drain.
    let (tx, rx) = std::sync::mpsc::channel();
    app.pricing_events = rx;
    tx.send(crate::pricing::PricingEvent::Loaded(Box::new(table_of(
        vec![],
    ))))
    .unwrap();
    crate::tui::app::on_tick(&mut app);
    assert!(!app.price_failed, "any Loaded clears the failure flag");
    assert!(app.price_table.is_some(), "the table lands");
}

#[test]
fn price_failed_sets_on_failed_without_a_table() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.bootstrap_started = true;
    let (tx, rx) = std::sync::mpsc::channel();
    app.pricing_events = rx;
    tx.send(crate::pricing::PricingEvent::Failed).unwrap();
    crate::tui::app::on_tick(&mut app);
    assert!(
        app.price_failed,
        "a failed fetch with no table sets the flag"
    );
}

#[test]
fn price_failed_keeps_the_last_good_table_on_a_transient_failure() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.bootstrap_started = true;
    app.price_table = Some(table_of(vec![]));
    let (tx, rx) = std::sync::mpsc::channel();
    app.pricing_events = rx;
    tx.send(crate::pricing::PricingEvent::Failed).unwrap();
    crate::tui::app::on_tick(&mut app);
    assert!(
        !app.price_failed,
        "a transient failure mid-session never blanks the cached table's flag"
    );
}

#[test]
fn total_cost_counts_unpriced_with_tokens() {
    let table = table_of(vec![flat_model("m", 1e-6, 2e-6, 0.0, 0.0)]);
    let stats = TokenStats {
        models: vec![
            ModelTokens {
                model: "m".to_owned(),
                input: 1_000_000,
                ..Default::default()
            }, // $1.00, priced
            ModelTokens {
                model: "unknown".to_owned(),
                input: 500_000,
                ..Default::default()
            }, // unpriced, has tokens → floor
            ModelTokens {
                model: "empty-unknown".to_owned(),
                ..Default::default()
            }, // unpriced, no tokens → ignored
        ],
        ..Default::default()
    };
    let (lines, _meta) = total_lines(&stats, &stats.speed_days, 60, 5, false, Some(&table));
    assert!(
        line_text(&lines[1]).contains("$1.00+"),
        "unpriced token-bearing models floor the lifetime figure, got {:?}",
        line_text(&lines[1])
    );
    let (bare, _meta) = total_lines(&stats, &stats.speed_days, 60, 5, false, None);
    assert!(line_text(&bare[1]).contains('—'), "no table → dash");

    // A zero-token unpriced model does NOT floor the figure: the count is
    // token-gated, never rate-gated.
    let zero_only = TokenStats {
        models: vec![
            ModelTokens {
                model: "m".to_owned(),
                input: 1_000_000,
                ..Default::default()
            },
            ModelTokens {
                model: "empty-unknown".to_owned(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (zero_lines, _meta) = total_lines(
        &zero_only,
        &zero_only.speed_days,
        60,
        5,
        false,
        Some(&table),
    );
    let zero_cost = line_text(&zero_lines[1]);
    assert!(
        zero_cost.contains("$1.00") && !zero_cost.contains('+'),
        "zero-token unpriced models do not floor, got {zero_cost:?}"
    );
}

#[test]
fn daily_lens_rows_carry_todays_hourly_buckets() {
    let _home = crate::testutil::HomeSandbox::new();
    // The daily-lens model rows must price today's models at their hourly
    // tiers (the today card's figures), not at the lifetime fallback's flat
    // hour-0 rate: windowed deepseek-chat at hour 10 (peak) reads $0.002
    // through the daily rows, not the $0.001 hour-0 flat price.
    let table = table_of(vec![windowed_model(
        "deepseek-chat",
        (2e-6, 4e-6),
        (1e-6, 2e-6),
    )]);
    let mut hours = [HourTokens::default(); 24];
    hours[10] = HourTokens {
        input: 1000,
        ..Default::default()
    };
    let stats = TokenStats {
        today: Some(DaySummary {
            date: crate::tokens::today_date(),
            model_hours: vec![HourlyModel {
                model: "deepseek-chat".to_owned(),
                hours,
            }],
            models: vec![ModelTokens {
                model: "deepseek-chat".to_owned(),
                input: 1000,
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Tokens;
    app.token_period = TokenPeriod::Daily;
    app.token_stats = Some(stats);
    let rows = crate::tui::app::token_period_models(&app);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].days.len(),
        1,
        "the daily row carries today as a day"
    );
    assert!(
        rows[0].days[0].hours.is_some(),
        "and that day carries its hour buckets"
    );
    let lines = model_lines(&rows, 60, 5, true, Some(&table), "no model usage yet");
    assert!(
        line_text(&lines[0]).contains("$0.002"),
        "the row prices the hourly tier, got {:?}",
        line_text(&lines[0])
    );
}

#[test]
fn model_detail_cost_sums_buckets_across_hourly_days() {
    let table = table_of(vec![windowed_model(
        "deepseek-chat",
        (2e-6, 4e-6),
        (1e-6, 2e-6),
    )]);
    // One hourly day: 1000 input at peak hour 16, 500 output at off-peak hour 0.
    let row = period_row(
        "deepseek-chat",
        vec![hourly_day(
            "2026-08-15",
            "deepseek-chat",
            &[(16, 1000, 0), (0, 0, 500)],
        )],
    );
    let (split, unpriced) = model_detail_cost(&table, &row).expect("priced");
    assert_eq!(unpriced, 0);
    assert!(
        (split.input - 0.002).abs() < 1e-9,
        "peak input rate, got {}",
        split.input
    );
    assert!(
        (split.output - 0.001).abs() < 1e-9,
        "off-peak output rate, got {}",
        split.output
    );
    assert!((split.cache - 0.0).abs() < 1e-9);
    assert!(
        (split.total() - 0.003).abs() < 1e-9,
        "got {}",
        split.total()
    );
}

// ── hour-of-day ticks ─────────────────────────────────────────────────────────

#[test]
fn hour_lines_carry_baseline_ticks_only_when_tall_enough() {
    // The single-cell tick row generated at cell width 1 (w = 30 < 48).
    const TICKS: &str = "0     6     12    18";
    let mut hours = [0u64; 24];
    hours[12] = 10;
    let tall: Vec<String> = hour_lines(&hours, 30, 5).iter().map(line_text).collect();
    assert!(
        tall.iter().any(|t| t.contains(TICKS)),
        "a tall chart carries the 0/6/12/18 tick row"
    );
    // Ticks sit directly above the caption row.
    assert!(tall[tall.len() - 2].contains(TICKS));
    assert!(tall[tall.len() - 1].contains("peak 12:00"));

    let short: Vec<String> = hour_lines(&hours, 30, 2).iter().map(line_text).collect();
    assert!(
        !short.iter().any(|t| t.contains(TICKS)),
        "the 2-row floor drops the ticks, not the chart"
    );
}

#[test]
fn hour_bars_and_ticks_widen_on_wide_cards() {
    let mut hours = [0u64; 24];
    hours[12] = 10;
    // 52 cols → 2-cell hour columns (48 wide, pad 2); ticks track the bars.
    let lines: Vec<String> = hour_lines(&hours, 52, 5).iter().map(line_text).collect();
    assert_eq!(
        lines[0].matches('█').count(),
        2,
        "the peak column is 2 cells wide"
    );
    let ticks = &lines[lines.len() - 2];
    assert_eq!(ticks.find('0'), Some(2), "hour 0 tick at the padded origin");
    assert_eq!(ticks.find("12"), Some(2 + 24), "hour 12 tick under its bar");
}

// ── bucket layout + month ticks ───────────────────────────────────────────────

#[test]
fn bucket_layout_widens_only_when_there_is_room() {
    assert_eq!(
        bucket_layout(116, 116),
        (1, 0),
        "dense daily series stays 1-cell contiguous"
    );
    assert_eq!(
        bucket_layout(25, 110),
        (3, 1),
        "weekly buckets widen with a 1-cell gap"
    );
    assert_eq!(bucket_layout(4, 48), (8, 1), "bar width caps at 8");
    assert_eq!(bucket_layout(0, 40), (1, 0));
}

#[test]
fn trend_bars_widen_with_gaps_and_carry_month_ticks() {
    // 10 days straddling jun → jul, in a 48-wide card: layout (3, 1), pad 4.
    let daily: Vec<DayTokens> = (0..10)
        .map(|i| DayTokens {
            date: if i < 5 {
                format!("2026-06-{:02}", 20 + i)
            } else {
                format!("2026-07-{:02}", i - 4)
            },
            tokens: 1_000_000 * (i as u64 + 1),
        })
        .collect();
    let stats = TokenStats {
        daily,
        ..Default::default()
    };
    let lines: Vec<String> = trend_lines(&stats, &stats.speed_days, 48, 6, TokenPeriod::Lifetime)
        .iter()
        .map(line_text)
        .collect();
    // h = 6 → 4 chart rows + tick row + peak caption.
    assert_eq!(lines.len(), 6);
    let bars = lines[3].trim().split(' ').filter(|s| !s.is_empty()).count();
    assert_eq!(bars, 10, "each bucket is a discrete gapped bar");
    assert_eq!(
        lines[4].find("jun"),
        Some(4),
        "first bucket names its month"
    );
    assert_eq!(
        lines[4].find("jul"),
        Some(4 + 5 * 4),
        "the month change is labeled at its bucket's column"
    );
    assert!(lines[5].contains("peak"));

    // Below the 4-row gate the tick row is dropped, never the chart.
    let short = trend_lines(&stats, &stats.speed_days, 48, 3, TokenPeriod::Lifetime);
    assert!(!line_text(&short[short.len() - 2]).contains("jun"));
}

// ── composition rows ──────────────────────────────────────────────────────────

#[test]
fn composition_pcts_anchor_to_the_card_edge() {
    let lines = comp_rows(10, 20, 30, 40, 60);
    for l in &lines {
        let t = line_text(l);
        assert_eq!(t.chars().count(), 60, "the row spans the full card width");
        assert!(t.ends_with('%'), "pct anchors to the right edge, got {t:?}");
    }
    assert!(line_text(&lines[3]).ends_with(" 40%"));
}

/// P1: a composition share prints at most two nearest-rounded decimals, never
/// a fabricated integer: 7342 of 10000 tokens is 73.42%, not 73%.
#[test]
fn composition_pcts_keep_two_decimals() {
    for w in [28, 45, 80] {
        let lines = comp_rows(7342, 2658, 0, 0, w);
        let input = line_text(&lines[0]);
        let output = line_text(&lines[1]);
        assert!(input.ends_with(" 73.42%"), "input share at {w}: {input:?}");
        assert!(
            output.ends_with(" 26.58%"),
            "output share at {w}: {output:?}"
        );
    }
}

// ── determinate_bar ───────────────────────────────────────────────────────────

#[test]
fn determinate_bar_is_bare_with_a_trailing_label() {
    let t = line_text(&determinate_bar(1, 2, 10, "scanning session logs 1/2"));
    assert!(
        !t.contains('[') && !t.contains(']'),
        "determinate bars are bare — the [ ] frame is the indeterminate tell"
    );
    assert_eq!(t.matches('█').count(), 5, "half done fills half the track");
    assert_eq!(t.matches('░').count(), 5, "the rest stays track");
    assert!(t.ends_with("scanning session logs 1/2"));
}

// ── indeterminate_bar ─────────────────────────────────────────────────────────

#[test]
fn indeterminate_bar_is_bracketed_with_a_bouncing_block() {
    let t = line_text(&indeterminate_bar(0, 12, "reading ~/.claude…"));
    assert!(t.starts_with('['), "opens with the [ frame");
    assert!(t.contains(']'), "closes with the ] frame");
    assert_eq!(t.matches('█').count(), INDET_BLOCK, "a 4-cell block");
    assert!(t.contains("reading ~/.claude…"), "label trails the bar");

    // The block advances with the tick (bounces), so consecutive frames differ.
    let a = line_text(&indeterminate_bar(0, 12, "x"));
    let b = line_text(&indeterminate_bar(1, 12, "x"));
    assert_ne!(a, b, "the block position advances one cell per tick");
}

// ── activity caption ──────────────────────────────────────────────────────────

#[test]
fn activity_caption_reports_the_busiest_bucket_only() {
    let stats = TokenStats {
        activity: vec![
            DayActivity {
                date: "2026-06-01".into(),
                messages: 50,
                sessions: 2,
                tool_calls: 10,
            },
            DayActivity {
                date: "2026-06-02".into(),
                messages: 116_000,
                sessions: 334,
                tool_calls: 41_000,
            },
            DayActivity {
                date: "2026-06-03".into(),
                messages: 80,
                sessions: 900,
                tool_calls: 90_000,
            },
        ],
        ..Default::default()
    };
    let lines = activity_lines(&stats, 48, 3, TokenPeriod::Lifetime);
    let caption = line_text(lines.last().unwrap());
    // The peak-message bucket's OWN three figures (its 334 sess / 41.0K tools),
    // not the other buckets' higher session/tool maxima.
    assert!(
        caption.contains("peak day: 116K msgs   334 sess   41.0K tools"),
        "caption must report one bucket's real figures, got {caption:?}"
    );
}

#[test]
fn activity_caption_names_the_granularity() {
    let stats = populated_stats();
    let wk = activity_lines(&stats, 48, 3, TokenPeriod::Weekly);
    assert!(
        line_text(wk.last().unwrap()).contains("peak wk:"),
        "weekly lens says `peak wk:`"
    );
    let mo = activity_lines(&stats, 48, 3, TokenPeriod::Monthly);
    assert!(
        line_text(mo.last().unwrap()).contains("peak mo:"),
        "monthly lens says `peak mo:`"
    );
}

// ── granularity badges (rendered) ─────────────────────────────────────────────

#[test]
fn trend_and_activity_badges_read_by_week_and_by_month() {
    let _home = crate::testutil::HomeSandbox::new();
    let weekly = render_dashboard(&app_with_stats(TokenPeriod::Weekly), 100, 44);
    assert!(weekly.contains("BY WEEK"), "trend title reads `by week`");
    assert!(weekly.contains("by week"), "activity meta reads `by week`");

    let monthly = render_dashboard(&app_with_stats(TokenPeriod::Monthly), 100, 44);
    assert!(monthly.contains("BY MONTH"), "trend title reads `by month`");
    assert!(
        monthly.contains("by month"),
        "activity meta reads `by month`"
    );
}

// ── pre-first-paint placeholder ───────────────────────────────────────────────

#[test]
fn placeholder_shows_the_full_width_bouncing_bar() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Tokens;
    // token_stats stays None, tokens_failed false → the indeterminate spinner.
    let out = render_dashboard(&app, 100, 10);
    assert!(
        out.contains("parsing stats-cache.json"),
        "the stage-1 loading label renders"
    );
    assert!(out.contains('['), "the bracketed spinner frame renders");
    assert!(out.contains('█'), "the bouncing block renders");
}

// ── shape marker render ───────────────────────────────────────────────────────

/// A model-detail row whose aggregate split is A1/A2-shaped renders the
/// `cache write not reported` marker; a healthy row renders nothing.
#[test]
fn model_detail_marks_cache_write_not_reported() {
    let a1_row = PeriodModel {
        model: "glm-5.3".to_owned(),
        in_out: 500,
        split: ModelTokens {
            model: "glm-5.3".to_owned(),
            input: 500,
            output: 100,
            cache_read: 1_000,
            cache_create: 0,
            shape: crate::tokens::UsageShape::WholePromptInput,
            ..Default::default()
        },
        split_complete: true,
        days: vec![],
    };
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&a1_row),
            2_000,
            (None, false),
            TokenPeriod::Daily,
            None,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        out.contains("cache write not reported"),
        "A1 row carries the marker, got: {out}"
    );

    let a2_row = PeriodModel {
        model: "deepseek-v4-pro".to_owned(),
        in_out: 500,
        split: ModelTokens {
            model: "deepseek-v4-pro".to_owned(),
            input: 500,
            output: 100,
            cache_read: 40_000,
            cache_create: 0,
            shape: crate::tokens::UsageShape::NoCacheWrites,
            ..Default::default()
        },
        split_complete: true,
        days: vec![],
    };
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&a2_row),
            40_600,
            (None, false),
            TokenPeriod::Daily,
            None,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        out.contains("cache write not reported"),
        "A2 row carries the marker, got: {out}"
    );

    let healthy_row = PeriodModel {
        model: "claude-opus-4".to_owned(),
        in_out: 500,
        split: ModelTokens {
            model: "claude-opus-4".to_owned(),
            input: 500,
            output: 100,
            cache_read: 40_000,
            cache_create: 5_000,
            shape: crate::tokens::UsageShape::Healthy,
            ..Default::default()
        },
        split_complete: true,
        days: vec![],
    };
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&healthy_row),
            45_600,
            (None, false),
            TokenPeriod::Daily,
            None,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        !out.contains("cache write not reported"),
        "healthy row renders no marker, got: {out}"
    );
}

/// An incomplete split (stats-cache day) renders no marker: the shape is
/// unknown there, and an unlabelled guess is the silent-correction failure
/// the marker exists to prevent.
#[test]
fn model_detail_marks_nothing_on_incomplete_split() {
    let row = PeriodModel {
        model: "glm-5.3".to_owned(),
        in_out: 500,
        split: ModelTokens::default(),
        split_complete: false,
        days: vec![],
    };
    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| {
        super::draw_model_detail(
            f,
            f.area(),
            Some(&row),
            2_000,
            (None, false),
            TokenPeriod::Daily,
            None,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        !out.contains("cache write not reported"),
        "incomplete split renders no marker, got: {out}"
    );
}

/// A narrow TOTAL card with a free row drops the `cache hit` badge to its own
/// line instead of clipping its tail off the card edge.
#[test]
fn the_total_cache_hit_badge_drops_to_its_own_line_when_narrow() {
    let stats = populated_stats();
    let (lines, _) = total_lines(&stats, &stats.speed_days, 24, 5, true, None);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(
        texts,
        [
            "tokens    170M",
            "  0% cache hit",
            "cost      —",
            "sessions  1,000",
            "msgs      200K"
        ]
    );
}

/// Two models whose splits sum to the lifetime totals, so every Tokens figure
/// derives from one fixture. `two_decimals`: opus holds 73.42% of the tokens
/// and hits its cache 26.45% of the time; the composition reads 73.42% input,
/// a residue-bearing 7% output (7000 / 100000 * 100 = 7.000000000000001) and
/// 19.58% cache read; the lifetime cache hit is 21.05%. Otherwise opus holds
/// 99.999% of the tokens, which rounds up to `100%`.
fn p1_tokens_app(two_decimals: bool) -> App {
    let model = |id: &str, input, output, cache_create, cache_read| ModelTokens {
        model: id.into(),
        input,
        output,
        cache_read,
        cache_create,
        ..Default::default()
    };
    let models = if two_decimals {
        vec![
            model("claude-opus-4-8", 54_000, 0, 0, 19_420),
            model("claude-haiku-4-5", 19_420, 7_000, 1, 159),
        ]
    } else {
        vec![
            model("claude-opus-4-8", 99_999, 0, 0, 0),
            model("claude-haiku-4-5", 0, 1, 0, 0),
        ]
    };
    let mut app = app_with_stats(TokenPeriod::Lifetime);
    let stats = app.token_stats.as_mut().unwrap();
    stats.total_input = models.iter().map(|m| m.input).sum();
    stats.total_output = models.iter().map(|m| m.output).sum();
    stats.total_cache_create = models.iter().map(|m| m.cache_create).sum();
    stats.total_cache_read = models.iter().map(|m| m.cache_read).sum();
    stats.models = models;
    app
}

/// Every row of the Tokens body at `(w, 60)` that prints a percentage, trailing
/// blanks trimmed.
fn pct_rows(app: &App, w: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, 60)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .filter(|r| r.contains('%'))
        .map(|r| r.trim_end().to_string())
        .collect()
}

/// P1 verify matrix, Tokens dashboard: the composition shares and the lifetime
/// cache hit print at most two rounded decimals, shares left-padded to the
/// widest so the `%` signs align, at every width.
#[test]
fn dashboard_readings_keep_two_rounded_decimals_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let want: &[(bool, u16, &[&str])] = &[
        (
            true,
            28,
            &[
                "│   21.05% cache hit       │",
                "│ input       ███░  73.42% │",
                "│ output      ░░░░      7% │",
                "│ cache write ░░░░      0% │",
                "│ cache read  █░░░  19.58% │",
            ],
        ),
        (
            true,
            45,
            &[
                "│ tokens    80.4K          21.05% cache hit │",
                "│ input       ███████████████░░░░░░  73.42% │",
                "│ output      █░░░░░░░░░░░░░░░░░░░░      7% │",
                "│ cache write ░░░░░░░░░░░░░░░░░░░░░      0% │",
                "│ cache read  ████░░░░░░░░░░░░░░░░░  19.58% │",
            ],
        ),
        (
            true,
            80,
            &[
                "│ idle so far today              ││ tokens    80.4K           21.05% cache hit │",
                "│ opus 4.8  ████████████████████████ 54.0K ││ input       █████████░░░  73.42% │",
                "│ haiku 4.5 ████████████░░░░░░░░░░░░ 26.4K ││ output      █░░░░░░░░░░░      7% │",
                "│                                          ││ cache write ░░░░░░░░░░░░      0% │",
                "│                                          ││ cache read  ██░░░░░░░░░░  19.58% │",
            ],
        ),
        (
            false,
            28,
            &[
                "│   0% cache hit           │",
                "│ input       ██████  100% │",
                "│ output      ░░░░░░    0% │",
                "│ cache write ░░░░░░    0% │",
                "│ cache read  ░░░░░░    0% │",
            ],
        ),
        (
            false,
            45,
            &[
                "│ tokens    100K               0% cache hit │",
                "│ input       ███████████████████████  100% │",
                "│ output      ░░░░░░░░░░░░░░░░░░░░░░░    0% │",
                "│ cache write ░░░░░░░░░░░░░░░░░░░░░░░    0% │",
                "│ cache read  ░░░░░░░░░░░░░░░░░░░░░░░    0% │",
            ],
        ),
        (
            false,
            80,
            &[
                "│ idle so far today              ││ tokens    100K                0% cache hit │",
                "│ opus 4.8  ███████████████████████ 100.0K ││ input       ██████████████  100% │",
                "│ haiku 4.5 ░░░░░░░░░░░░░░░░░░░░░░░      1 ││ output      ░░░░░░░░░░░░░░    0% │",
                "│                                          ││ cache write ░░░░░░░░░░░░░░    0% │",
                "│                                          ││ cache read  ░░░░░░░░░░░░░░    0% │",
            ],
        ),
    ];
    for (two_decimals, w, rows) in want {
        let app = p1_tokens_app(*two_decimals);
        assert_eq!(
            pct_rows(&app, *w),
            *rows,
            "dashboard readings (two decimals: {two_decimals}) at {w} cols"
        );
    }
}

/// P1 verify matrix, Tokens model detail: the selected model's share and cache
/// hit print at most two rounded decimals, the pair left-padded to the wider
/// so their `%` signs align, at every width.
#[test]
fn model_detail_readings_keep_two_rounded_decimals_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let want: &[(bool, u16, [&str; 2])] = &[
        (
            true,
            28,
            [
                "│ ███████░░░ 73.42%        │",
                "│ ███░░░░░░░ 26.45%        │",
            ],
        ),
        (
            true,
            45,
            [
                "│ ████████████████████░░░░░░░ 73.42%        │",
                "│ ███████░░░░░░░░░░░░░░░░░░░░ 26.45%        │",
            ],
        ),
        (
            true,
            80,
            [
                "│                      ││ ██████████████████████████░░░░░░░░░░ 73.42%          │",
                "│                      ││ █████████░░░░░░░░░░░░░░░░░░░░░░░░░░░ 26.45%          │",
            ],
        ),
        (
            false,
            28,
            [
                "│ ██████████ 100%          │",
                "│ ░░░░░░░░░░   0%          │",
            ],
        ),
        (
            false,
            45,
            [
                "│ ███████████████████████████ 100%          │",
                "│ ░░░░░░░░░░░░░░░░░░░░░░░░░░░   0%          │",
            ],
        ),
        (
            false,
            80,
            [
                "│                      ││ ████████████████████████████████████ 100%            │",
                "│                      ││ ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░   0%            │",
            ],
        ),
    ];
    for (two_decimals, w, rows) in want {
        let mut app = p1_tokens_app(*two_decimals);
        app.token_view = crate::tui::app::TokenView::Models;
        assert_eq!(
            pct_rows(&app, *w),
            *rows,
            "model detail readings (two decimals: {two_decimals}) at {w} cols"
        );
    }
}

/// P1/R1: with the composition card focused at 28x30, its bar sheds cells
/// (down to one) before any share loses its `%`.
#[test]
fn the_composition_card_keeps_every_share_at_28_by_30() {
    let _home = crate::testutil::HomeSandbox::new();
    for two_decimals in [true, false] {
        let mut app = p1_tokens_app(two_decimals);
        app.token_dash_focus = crate::tui::app::CardKind::Comp;
        let mut term = Terminal::new(TestBackend::new(28, 30)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
        let rows: Vec<String> = crate::testutil::buffer_rows(term.backend().buffer())
            .into_iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        for label in ["input", "output", "cache write", "cache read"] {
            let row = rows
                .iter()
                .find(|r| r.trim_start().starts_with(&format!("│ {label}")))
                .unwrap_or_else(|| panic!("{label} row renders at 28x30: {rows:?}"));
            // The focused card's scrollbar (its own column) trails the row;
            // the share's % must sit whole before the card border.
            let t = row.trim_end();
            let body = t
                .strip_suffix('┊')
                .or_else(|| t.strip_suffix('┃'))
                .unwrap_or(t);
            assert!(
                body.ends_with("% │"),
                "{label} share keeps its % at 28x30 (two decimals: {two_decimals}): {row:?}"
            );
        }
    }
}

// ── tps: the today-card speed row and the total-card avg speed ───────────

/// A fresh snapshot with `total.live.speed` 92.4 and a day sum of 900 tokens
/// over 10 s (90 t/s) across one account and model.
fn tps_tokens_snapshot(age_ms: u64) -> crate::tps::Snapshot {
    let stamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(crate::usage::now_ms() - age_ms).unwrap(),
    )
    .unwrap()
    .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
    .to_string();
    let mut snap = crate::tps::Snapshot {
        version: crate::tps::CACHE_VERSION,
        generated_at: stamp,
        total: crate::tps::Figures {
            live: crate::tps::Figure {
                rate: 67.2,
                speed: Some(92.4),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    snap.days.insert(
        crate::tokens::today_date(),
        std::collections::BTreeMap::from([(
            "uwuclxdy".to_string(),
            std::collections::BTreeMap::from([(
                "sonnet".to_string(),
                crate::tps::DaySum {
                    output: 900,
                    gen_ms: 10_000,
                    ..Default::default()
                },
            )]),
        )]),
    );
    snap
}

/// The today card's speed row reads `S t/s now · S t/s today`, the `now` figure
/// off `total.live.speed` and the `today` figure the merged day sum's average.
#[test]
fn the_today_card_speed_row_reads_now_and_today() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.set_tps(Some(tps_tokens_snapshot(1_000)));

    let line = super::today_speed_line(
        &app,
        Some(crate::tps::DaySum {
            output: 900,
            gen_ms: 10_000,
            ..Default::default()
        }),
    );
    assert_eq!(line_text(&line), "speed     92 t/s now · 90 t/s today");

    // A hydrated app with no producer reads `—` for both figures, never a
    // fabricated 0.
    app.set_tps(None);
    let line = super::today_speed_line(&app, None);
    assert_eq!(line_text(&line), "speed     — now · — today");

    // A fresh app (no cache read) still shows the row, both figures `—`.
    let fresh = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    assert_eq!(
        line_text(&super::today_speed_line(&fresh, None)),
        "speed     — now · — today"
    );
}

/// The total card's `avg speed` row divides the ledger's lifetime speed sums
/// (output ÷ generation seconds) across every finalized day.
#[test]
fn the_total_card_avg_speed_reads_the_ledger_speed_rows() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut stats = populated_stats();
    stats.speed_days.insert(
        "2026-10-14".to_string(),
        crate::tps::DaySum {
            output: 1_800,
            gen_ms: 20_000,
            ..Default::default()
        },
    );
    let (lines, _meta) = total_lines(&stats, &stats.speed_days, 40, 5, false, None);
    let avg = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("avg speed"))
        .expect("a ledger with speed rows shows the avg speed line");
    assert_eq!(avg, "avg speed 90 t/s");

    // The row sits right under `sessions`, above `msgs`.
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let at = |key: &str| texts.iter().position(|t| t.starts_with(key));
    let sessions = at("sessions").expect("the card has a sessions row");
    assert_eq!(
        (at("avg speed"), at("msgs")),
        (Some(sessions + 1), Some(sessions + 2)),
        "{texts:?}"
    );

    // No speed rows, no avg speed line.
    let empty = populated_stats();
    let (empty_lines, _) = total_lines(&empty, &empty.speed_days, 40, 5, false, None);
    assert!(
        !empty_lines
            .iter()
            .map(line_text)
            .any(|t| t.contains("avg speed")),
        "no speed rows, no avg speed line"
    );
}

/// The `cache hit` badge takes its own line only when the card has a row for
/// it: at 5 rows with the avg speed row the badge sheds, at 6 it keeps its
/// line, and with no speed rows it sheds at 4.
#[test]
fn the_total_cache_hit_badge_takes_its_own_line_only_when_the_card_has_a_row_for_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut speed = populated_stats();
    speed.speed_days.insert(
        "2026-10-14".to_string(),
        crate::tps::DaySum {
            output: 1_800,
            gen_ms: 20_000,
            ..Default::default()
        },
    );
    let plain = populated_stats();
    let rows = |stats: &TokenStats, h: usize| -> Vec<String> {
        let (lines, _) = total_lines(stats, &stats.speed_days, 24, h, true, None);
        lines.iter().map(line_text).collect()
    };
    assert_eq!(
        rows(&speed, 5),
        [
            "tokens    170M",
            "cost      —",
            "sessions  1,000",
            "avg speed 90 t/s",
            "msgs      200K"
        ]
    );
    assert_eq!(
        rows(&speed, 6),
        [
            "tokens    170M",
            "  0% cache hit",
            "cost      —",
            "sessions  1,000",
            "avg speed 90 t/s",
            "msgs      200K"
        ]
    );
    assert_eq!(
        rows(&plain, 4),
        [
            "tokens    170M",
            "cost      —",
            "sessions  1,000",
            "msgs      200K"
        ]
    );
}

/// The TOTAL card's 5 inner rows as rendered: the card whose top border names
/// `TOTAL`, cut to its own columns.
fn total_card_rows(rows: &[String]) -> Vec<String> {
    let top = rows
        .iter()
        .position(|r| r.contains("─ TOTAL"))
        .unwrap_or_else(|| panic!("no total card on screen: {rows:#?}"));
    let cells: Vec<char> = rows[top].chars().collect();
    let title_at = rows[top][..rows[top].find("─ TOTAL").unwrap()]
        .chars()
        .count();
    let left = cells[..title_at].iter().rposition(|c| *c == '╭').unwrap();
    let right = left + cells[left..].iter().position(|c| *c == '╮').unwrap();
    rows[top + 1..=top + 5]
        .iter()
        .map(|r| {
            r.chars()
                .skip(left + 1)
                .take(right - left - 1)
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect()
}

/// On screen, at sizes where the badge cannot sit beside the tokens figure
/// (34x60 narrow, 60x40 single column), the card with the avg speed row keeps
/// `msgs` and sheds the badge; at 140x40 the badge sits inline and every row
/// shows.
#[test]
fn the_rendered_total_card_never_clips_msgs_for_the_badge() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = p1_tokens_app(true);
    app.token_stats.as_mut().unwrap().speed_days.insert(
        "2026-10-14".to_string(),
        crate::tps::DaySum {
            output: 1_800,
            gen_ms: 20_000,
            ..Default::default()
        },
    );
    for (w, h, badge) in [(34, 60, false), (60, 40, false), (140, 40, true)] {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
        let card = total_card_rows(&crate::testutil::buffer_rows(term.backend().buffer()));
        assert!(
            card.iter().any(|r| r.starts_with("avg speed 90 t/s")),
            "{w}x{h}: {card:#?}"
        );
        assert!(
            card.iter().any(|r| r.starts_with("msgs")),
            "{w}x{h}: {card:#?}"
        );
        assert_eq!(
            card.iter().any(|r| r.contains("cache hit")),
            badge,
            "{w}x{h}: {card:#?}"
        );
    }
}

// ── tps: the daily trend card's speed strip ──────────────────────────────

/// Two days of tokens, `2026-06-01` carrying a 90 t/s speed row and
/// `2026-06-02` none.
fn strip_stats() -> TokenStats {
    let mut stats = TokenStats {
        daily: vec![
            DayTokens {
                date: "2026-06-01".into(),
                tokens: 100,
            },
            DayTokens {
                date: "2026-06-02".into(),
                tokens: 50,
            },
        ],
        ..Default::default()
    };
    stats.speed_days.insert(
        "2026-06-01".to_string(),
        crate::tps::DaySum {
            output: 900,
            gen_ms: 10_000,
            ..Default::default()
        },
    );
    stats
}

/// The strip is one block per bar, scaled to the shown days' max with the
/// `SPARK` ramp, blank where no producer ran.
#[test]
fn the_speed_strip_scales_blocks_and_blanks_where_absent() {
    assert_eq!(
        line_text(&super::speed_strip(&[Some(90.0), None], 90.0, 5, 2, 1)),
        "██   "
    );
    assert_eq!(
        line_text(&super::speed_strip(
            &[Some(45.0), Some(90.0)],
            90.0,
            5,
            2,
            1
        )),
        "▅▅ ██"
    );
}

/// The trend footer gains `· avg S t/s` once any shown bar has a speed row, and
/// stays bare when none do. The strip row itself appears only on a card tall
/// enough to keep the month ticks under it.
#[test]
fn the_trend_strip_and_footer_avg_appear_only_with_speed_rows() {
    let stats = strip_stats();
    let tall = trend_lines(&stats, &stats.speed_days, 40, 5, TokenPeriod::Daily);
    assert_eq!(tall.len(), 5, "bars + strip + ticks + caption");
    let caption = line_text(tall.last().expect("caption row"));
    assert!(
        caption.contains("· avg 90 t/s"),
        "the footer averages the shown speed rows: {caption:?}"
    );

    let no_speed = TokenStats {
        daily: stats.daily.clone(),
        ..Default::default()
    };
    let bare = trend_lines(&no_speed, &no_speed.speed_days, 40, 5, TokenPeriod::Daily);
    let caption = line_text(bare.last().expect("caption row"));
    assert!(
        !caption.contains("avg"),
        "no suffix without speed rows: {caption:?}"
    );
}

/// On a short card the strip drops before the month ticks: at height 4 the tick
/// row stays and the strip is the row that gives way.
#[test]
fn the_trend_speed_strip_drops_before_the_month_ticks() {
    let stats = strip_stats();
    let tall = trend_lines(&stats, &stats.speed_days, 40, 5, TokenPeriod::Daily);
    let short = trend_lines(&stats, &stats.speed_days, 40, 4, TokenPeriod::Daily);
    assert_eq!(tall.len(), 5);
    assert_eq!(short.len(), 4, "the strip row drops, the ticks stay");
    assert_eq!(
        line_text(&short[2]),
        line_text(&tall[3]),
        "the short card's third line is the tick row the tall card keeps after its strip"
    );
}

// ── tps survivors: dashboard visibility, the `*` account, the weekly fold ──

/// The today and total cards grew to 7 rows so their ruled speed rows reach the
/// screen: pinned through the full dashboard render, not the row builders.
#[test]
fn the_dashboard_shows_the_speed_rows_when_the_cards_grew() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_stats(TokenPeriod::Daily);
    let mut stats = populated_stats();
    stats.speed_days.insert(
        "2026-06-20".to_string(),
        crate::tps::DaySum {
            output: 1_800,
            gen_ms: 20_000,
            ..Default::default()
        },
    );
    app.token_stats = Some(stats);
    app.set_tps(Some(tps_tokens_snapshot(1_000)));

    let out = render_dashboard(&app, 160, 40);
    assert!(
        out.contains("avg speed"),
        "the total card's avg speed row is on screen"
    );
    assert!(
        out.contains("now · "),
        "the today card's speed row is on screen"
    );
}

/// The merged day sums fold every account, the reserved unattributed `*` row
/// included.
#[test]
fn the_merged_day_sums_include_the_unattributed_star_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let mut snap = tps_tokens_snapshot(1_000);
    snap.days
        .get_mut(&crate::tokens::today_date())
        .expect("the fixture carries a day row")
        .insert(
            "*".to_string(),
            std::collections::BTreeMap::from([(
                "sonnet".to_string(),
                crate::tps::DaySum {
                    output: 100,
                    gen_ms: 10_000,
                    ..Default::default()
                },
            )]),
        );
    app.set_tps(Some(snap));

    let merged = super::merged_speed_days(&TokenStats::default(), &app);
    let today = merged
        .get(&crate::tokens::today_date())
        .copied()
        .expect("the cache's day row reaches the merge");
    assert_eq!(
        today,
        crate::tps::DaySum {
            output: 1_000,
            gen_ms: 20_000,
            ..Default::default()
        },
        "(900 + 100) over (10 s + 10 s), the `*` row included"
    );
    assert_eq!(
        line_text(&super::today_speed_line(&app, Some(today))),
        "speed     92 t/s now · 50 t/s today"
    );
}

/// The weekly lens folds every day of a bucket's speed rows before the strip
/// and footer average.
#[test]
fn the_weekly_trend_strip_folds_days_into_their_bucket() {
    let _home = crate::testutil::HomeSandbox::new();
    // 2026-06-01 is a monday, so both days fold into one ISO-week bucket.
    let mut stats = TokenStats {
        daily: vec![
            DayTokens {
                date: "2026-06-01".into(),
                tokens: 100,
            },
            DayTokens {
                date: "2026-06-02".into(),
                tokens: 50,
            },
        ],
        ..Default::default()
    };
    for (day, output) in [("2026-06-01", 900u64), ("2026-06-02", 100)] {
        stats.speed_days.insert(
            day.to_string(),
            crate::tps::DaySum {
                output,
                gen_ms: 10_000,
                ..Default::default()
            },
        );
    }
    let lines = trend_lines(&stats, &stats.speed_days, 40, 5, TokenPeriod::Weekly);
    let caption = line_text(lines.last().expect("caption row"));
    assert!(
        caption.contains("· avg 50 t/s"),
        "the bucket folds both days' sums (900+100 ÷ 20 s), not the first day's 90: {caption:?}"
    );
}

/// A producer cache holding yesterday + today and a ledger holding an older day
/// merge into the Tokens day sums: the TOTAL card and the trend footer average
/// over all three (900+1800+200 ÷ 10+20+20 s = 58 t/s), never the ledger's one
/// day (90 t/s). A stale cache contributes nothing, so only the ledger day
/// survives.
#[test]
fn the_tokens_surfaces_merge_the_cache_days_over_the_ledger() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_stats(TokenPeriod::Daily);

    let mut stats = TokenStats {
        daily: vec![
            DayTokens {
                date: "2026-06-01".into(),
                tokens: 100,
            },
            DayTokens {
                date: "2026-06-02".into(),
                tokens: 50,
            },
            DayTokens {
                date: "2026-06-03".into(),
                tokens: 25,
            },
        ],
        ..Default::default()
    };
    stats.speed_days.insert(
        "2026-06-01".to_string(),
        crate::tps::DaySum {
            output: 900,
            gen_ms: 10_000,
            ..Default::default()
        },
    );
    let mut snap = tps_tokens_snapshot(1_000);
    snap.days.clear();
    snap.days.insert(
        "2026-06-02".to_string(),
        std::collections::BTreeMap::from([(
            "uwuclxdy".to_string(),
            std::collections::BTreeMap::from([(
                "sonnet".to_string(),
                crate::tps::DaySum {
                    output: 1_800,
                    gen_ms: 20_000,
                    ..Default::default()
                },
            )]),
        )]),
    );
    snap.days.insert(
        "2026-06-03".to_string(),
        std::collections::BTreeMap::from([(
            "uwuclxdy".to_string(),
            std::collections::BTreeMap::from([(
                "sonnet".to_string(),
                crate::tps::DaySum {
                    output: 200,
                    gen_ms: 20_000,
                    ..Default::default()
                },
            )]),
        )]),
    );
    app.token_stats = Some(stats);
    app.set_tps(Some(snap));

    let out = render_dashboard(&app, 160, 40);
    assert!(
        out.contains("avg speed 58 t/s"),
        "the TOTAL card averages all three merged days, not the ledger's 90: {out:?}"
    );
    assert!(
        out.contains("· avg 58 t/s"),
        "the trend footer averages all three merged days: {out:?}"
    );

    // A stale cache contributes nothing: only the ledger day survives.
    app.set_tps(Some(tps_tokens_snapshot(31_000)));
    let out = render_dashboard(&app, 160, 40);
    assert!(
        out.contains("avg speed 90 t/s"),
        "a stale cache leaves only the ledger day on the TOTAL card: {out:?}"
    );
    assert!(
        out.contains("· avg 90 t/s"),
        "a stale cache leaves only the ledger day on the trend footer: {out:?}"
    );
}

/// A date in both the ledger and the cache reads the LEDGER's sum when it is
/// the larger: the folded day wins over a smaller cache copy. Guards the
/// keep-larger merge against a cache-wins `insert` regression.
#[test]
fn the_merged_day_sums_let_the_ledger_win_on_a_shared_date() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let mut stats = TokenStats::default();
    stats.speed_days.insert(
        "2026-06-01".to_string(),
        crate::tps::DaySum {
            output: 900,
            gen_ms: 10_000,
            ..Default::default()
        },
    );
    let mut snap = tps_tokens_snapshot(1_000);
    snap.days.clear();
    snap.days.insert(
        "2026-06-01".to_string(),
        std::collections::BTreeMap::from([(
            "uwuclxdy".to_string(),
            std::collections::BTreeMap::from([(
                "sonnet".to_string(),
                crate::tps::DaySum {
                    output: 50,
                    gen_ms: 10_000,
                    ..Default::default()
                },
            )]),
        )]),
    );
    app.set_tps(Some(snap));

    let merged = super::merged_speed_days(&stats, &app);
    assert_eq!(
        merged.get("2026-06-01"),
        Some(&crate::tps::DaySum {
            output: 900,
            gen_ms: 10_000,
            ..Default::default()
        }),
        "the ledger's finalized day wins over the cache's day"
    );
}

/// The today card's body as drawn: the cells between the first card's side
/// borders on every row of a Tokens frame, up to its bottom border, blank rows
/// dropped.
fn today_card_rows(app: &App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
        .into_iter()
        .skip_while(|row| !row.starts_with("╭─ TODAY "))
        .skip(1)
        .take_while(|row| !row.contains('╰'))
        .filter_map(|row| {
            let inner = row.split('│').nth(1)?.trim().to_string();
            (!inner.is_empty()).then_some(inner)
        })
        .collect()
}

/// The today card reads `speed` right under `msgs`, above the in/out split.
#[test]
fn the_today_card_puts_speed_above_the_in_out_split() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = Tab::Tokens;
    app.token_period = TokenPeriod::Daily;
    app.token_stats = Some(TokenStats {
        today: Some(DaySummary {
            date: crate::tokens::today_date(),
            input: 800_000,
            output: 400_000,
            messages: 310,
            ..Default::default()
        }),
        ..Default::default()
    });
    app.set_tps(Some(tps_tokens_snapshot(1_000)));

    assert_eq!(
        today_card_rows(&app, 120, 40),
        [
            "tokens    1.20M",
            "cost      —",
            "msgs      310",
            "speed     92 t/s now · 90 t/s today",
            "800K in                               400K out",
        ]
    );

    // An idle day draws no split: speed stays under the idle line.
    app.token_stats = Some(TokenStats::default());
    assert_eq!(
        today_card_rows(&app, 120, 40),
        ["idle so far today", "speed     92 t/s now · 90 t/s today"]
    );
}

/// A day both the ledger and a fresh cache hold (an early fold, or a day
/// folded when it stopped being today) reads the larger of the two: the
/// cache's whole today (6000 over 60 s) beats the ledger's early part (2000
/// over 40 s), and the today card reads 100 t/s.
#[test]
fn a_day_in_both_the_ledger_and_the_cache_reads_the_larger() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let today = crate::tokens::today_date();
    let cell = |output: u64, gen_ms: u64| crate::tps::DaySum {
        output,
        gen_ms,
        ..Default::default()
    };
    let mut stats = TokenStats::default();
    stats.speed_days.insert(today.clone(), cell(2000, 40_000));
    let mut snap = tps_tokens_snapshot(1_000);
    snap.days.clear();
    snap.days.insert(
        today.clone(),
        std::collections::BTreeMap::from([(
            "kerry".to_string(),
            std::collections::BTreeMap::from([("opus".to_string(), cell(6000, 60_000))]),
        )]),
    );
    app.set_tps(Some(snap));

    let merged = super::merged_speed_days(&stats, &app);
    assert_eq!(
        merged.get(&today),
        Some(&cell(6000, 60_000)),
        "merged today"
    );
    assert_eq!(
        line_text(&super::today_speed_line(&app, merged.get(&today).copied())),
        "speed     92 t/s now · 100 t/s today",
        "the today card"
    );
}

/// A day the ledger holds as kerry 1000/10 s + mommy 500/5 s (a swapped
/// session's day, folded once it left the window) and a fresh cache holds as
/// kerry 1500/15 s alone (the same responses read under one member after a
/// clock step back) merges per model total, the way the fold stores it: the
/// cache's 1500/15 s ties the ledger's and replaces its split, so the day reads
/// 1500/15 s, never kerry 1500 + mommy 500 = 2000/20 s. The ledger reaches the
/// tab through the tokens worker.
#[test]
fn a_day_in_the_ledger_and_the_cache_merges_per_model_total() {
    let sb = crate::testutil::HomeSandbox::new();
    let claude = sb.home().join(".claude");
    let clauth = sb.home().join(".clauth");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::create_dir_all(&clauth).unwrap();
    std::fs::write(claude.join("stats-cache.json"), "{}").unwrap();
    std::fs::write(
        clauth.join("token_speed.json"),
        r#"{"version":2,"days":{"2026-06-01":{
            "kerry":{"opus":{"output":1000,"gen_ms":10000}},
            "mommy":{"opus":{"output":500,"gen_ms":5000}}}}}"#,
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (refresh_tx, refresh_rx) = std::sync::mpsc::channel::<()>();
    crate::tokens::spawn(tx, refresh_rx, claude, Some(clauth));
    let stats = loop {
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(crate::tokens::TokensEvent::Loaded(stats)) => break *stats,
            Ok(crate::tokens::TokensEvent::Failed(_)) => panic!("the tokens worker failed"),
            Ok(_) => {}
            Err(e) => panic!("no Loaded event from the tokens worker: {e}"),
        }
    };
    drop(refresh_tx);
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let mut snap = tps_tokens_snapshot(1_000);
    snap.days.clear();
    snap.days.insert(
        "2026-06-01".to_string(),
        std::collections::BTreeMap::from([(
            "kerry".to_string(),
            std::collections::BTreeMap::from([(
                "opus".to_string(),
                crate::tps::DaySum {
                    output: 1500,
                    gen_ms: 15_000,
                    ..Default::default()
                },
            )]),
        )]),
    );
    app.set_tps(Some(snap));

    let merged = super::merged_speed_days(&stats, &app);
    assert_eq!(
        merged.get("2026-06-01"),
        Some(&crate::tps::DaySum {
            output: 1500,
            gen_ms: 15_000,
            ..Default::default()
        }),
        "the day merged per model total"
    );
}

/// One day's speed cells, each `(account, model, output, gen_ms)`.
fn day_cells(
    cells: &[(&str, &str, u64, u64)],
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, crate::tps::DaySum>> {
    let mut out = std::collections::BTreeMap::new();
    for &(account, model, output, gen_ms) in cells {
        out.entry(account.to_string())
            .or_insert_with(std::collections::BTreeMap::new)
            .insert(
                model.to_string(),
                crate::tps::DaySum {
                    output,
                    gen_ms,
                    ..Default::default()
                },
            );
    }
    out
}

/// The merged 2026-06-01 of a ledger holding `ledger` (whose day total is
/// `ledger_day`, `(output, gen_ms)`) and a fresh cache holding `cache`.
fn merged_shared_day(
    ledger: &[(&str, &str, u64, u64)],
    ledger_day: (u64, u64),
    cache: &[(&str, &str, u64, u64)],
) -> Option<crate::tps::DaySum> {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    let mut stats = TokenStats::default();
    stats
        .speed_cells
        .insert("2026-06-01".to_string(), day_cells(ledger));
    stats.speed_days.insert(
        "2026-06-01".to_string(),
        crate::tps::DaySum {
            output: ledger_day.0,
            gen_ms: ledger_day.1,
            ..Default::default()
        },
    );
    let mut snap = tps_tokens_snapshot(1_000);
    snap.days.clear();
    snap.days.insert("2026-06-01".to_string(), day_cells(cache));
    app.set_tps(Some(snap));
    super::merged_speed_days(&stats, &app)
        .get("2026-06-01")
        .copied()
}

/// A day the ledger holds early (kerry 1000/10 s, folded while the day was
/// still running) and the fresh cache holds grown (kerry 1500/15 s) reads the
/// cache's larger model total.
#[test]
fn a_day_the_cache_grew_past_the_ledger_reads_the_cache_model_total() {
    assert_eq!(
        merged_shared_day(
            &[("kerry", "opus", 1000, 10_000)],
            (1000, 10_000),
            &[("kerry", "opus", 1500, 15_000)],
        ),
        Some(crate::tps::DaySum {
            output: 1500,
            gen_ms: 15_000,
            ..Default::default()
        }),
        "the cache's grown day"
    );
}

/// Each model merges on its own total: the ledger holds kerry opus 1200/12 s
/// and sonnet 500/5 s (1700/17 s), the cache kerry opus 1000/10 s (a lost
/// transcript) and mommy sonnet 600/6 s (a swap). Opus keeps the ledger's
/// 1200/12 s, sonnet takes the cache's 600/6 s: 1800/18 s, never the larger
/// day total 1700/17 s.
#[test]
fn a_two_model_day_merges_each_model_on_its_own_total() {
    assert_eq!(
        merged_shared_day(
            &[
                ("kerry", "opus", 1200, 12_000),
                ("kerry", "sonnet", 500, 5_000),
            ],
            (1700, 17_000),
            &[
                ("kerry", "opus", 1000, 10_000),
                ("mommy", "sonnet", 600, 6_000),
            ],
        ),
        Some(crate::tps::DaySum {
            output: 1800,
            gen_ms: 18_000,
            ..Default::default()
        }),
        "the day merged per model"
    );
}

// ── tps peaks: the today speed row and the total avg-speed row ───────────

/// The text of the card row holding `needle` in a full dashboard render, cut at
/// the card's own `│` borders and trimmed.
fn dashboard_card_row(app: &App, w: u16, h: u16, needle: &str) -> Option<String> {
    dashboard_card_fit(app, w, h, needle).map(|(text, _)| text)
}

/// [`dashboard_card_row`] plus the card's text width: the cells between its
/// `│` borders minus one padding cell on each side.
fn dashboard_card_fit(app: &App, w: u16, h: u16, needle: &str) -> Option<(String, usize)> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    rows.iter().find_map(|row| {
        let at = row.find(needle)?;
        let chars: Vec<char> = row.chars().collect();
        let at = row[..at].chars().count();
        let left = chars[..at].iter().rposition(|c| *c == '│')?;
        let right = at + chars[at..].iter().position(|c| *c == '│')?;
        let text = chars[left + 1..right]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        Some((text, right - left - 1 - 2))
    })
}

/// The longest rung of a whole-part ladder that fits `width` cells; the floor
/// rung when none fits (a row clipped below its floor then fails the compare).
fn widest_fitting<'a>(ladder: &[&'a str], width: usize) -> &'a str {
    ladder
        .iter()
        .rev()
        .find(|rung| rung.chars().count() <= width)
        .copied()
        .unwrap_or(ladder[0])
}

/// The Tokens fixture with speed rows (ledger 1800 over 20 s, today's cache
/// row 900 over 10 s: 90 t/s every way) and a fresh snapshot carrying `peaks`.
fn tokens_app_with_peaks(peaks: Option<serde_json::Value>) -> App {
    let mut app = app_with_stats(TokenPeriod::Daily);
    let mut stats = populated_stats();
    stats.speed_days.insert(
        "2026-06-20".to_string(),
        crate::tps::DaySum {
            output: 1_800,
            gen_ms: 20_000,
            ..Default::default()
        },
    );
    app.token_stats = Some(stats);
    let mut snap = tps_tokens_snapshot(1_000);
    if let Some(peaks) = peaks {
        snap.peaks = serde_json::from_value(peaks).unwrap();
    }
    app.set_tps(Some(snap));
    app
}

fn peaks_fixture() -> serde_json::Value {
    serde_json::json!({
        "today": {
            "speed": {"value": 412.3, "at": "2026-09-22T21:14:00.000+00:00", "account": "kerry", "model": "claude-opus-4-8"},
            "rate": {"value": 1803.4, "at": "2026-09-22T21:02:00.000+00:00"}
        },
        "ever": {
            "speed": {"value": 612.0, "at": "2026-09-20T21:14:00.000+00:00", "account": "kerry", "model": "claude-opus-4-8"},
            "rate": {"value": 3104.2, "at": "2026-09-20T21:02:00.000+00:00"}
        }
    })
}

const DASH_WIDTHS: [u16; 5] = [100, 120, 160, 200, 240];

/// The today card's speed row ends in today's total peak RATE (`peaks.today.rate`),
/// never the peak speed, and adds no row. A card too narrow for it sheds the
/// peak part whole: every width reads either the whole row or the row without
/// its peak, the widest reads the whole row, and once a width shows the peak
/// every wider one does.
#[test]
fn the_today_speed_row_ends_in_todays_peak_rate() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(Some(peaks_fixture()));
    let full = "speed     92 t/s now · 90 t/s today · 1803 tok/s peak";
    let base = "speed     92 t/s now · 90 t/s today";
    let mut shown = false;
    for w in DASH_WIDTHS {
        let row = dashboard_card_row(&app, w, 40, "t/s now")
            .unwrap_or_else(|| panic!("{w}x40: no today speed row"));
        assert!(row == full || row == base, "{w}x40: {row:?}");
        if shown {
            assert_eq!(row, full, "{w}x40: a wider card dropped the peak");
        }
        shown = row == full;
    }
    assert!(shown, "the widest dashboard shows the peak");
}

/// The total card's `avg speed` row ends in the all-time peak SPEED, then the
/// all-time peak RATE (`peaks.ever`), units telling them apart. A narrow card
/// sheds the rate peak first, then the speed peak, each whole.
#[test]
fn the_total_avg_speed_row_ends_in_the_all_time_peaks() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(Some(peaks_fixture()));
    let ladder = [
        "avg speed 90 t/s",
        "avg speed 90 t/s · 612 t/s peak",
        "avg speed 90 t/s · 612 t/s peak · 3104 tok/s peak",
    ];
    // Every width from the narrow layout up: each width reads the longest
    // whole-part rung its card's measured text width holds.
    let mut shed = false;
    for w in 50..=240u16 {
        let (row, width) = dashboard_card_fit(&app, w, 40, "avg speed")
            .unwrap_or_else(|| panic!("{w}x40: no avg speed row"));
        assert_eq!(
            row,
            widest_fitting(&ladder, width),
            "{w}x40 ({width} cells)"
        );
        shed |= row != ladder[2];
    }
    assert!(shed, "some width sheds the rate peak");
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "avg speed").as_deref(),
        Some(ladder[2]),
        "the widest dashboard shows both peaks"
    );

    // A 6-digit speed peak is wide enough that narrow cards shed it too: the
    // rate part goes first, then the speed part.
    let wide = tokens_app_with_peaks(Some(serde_json::json!({
        "ever": {
            "speed": {"value": 123456.0, "at": "2026-09-20T21:14:00.000+00:00", "account": "kerry", "model": "claude-opus-4-8"},
            "rate": {"value": 3104.2, "at": "2026-09-20T21:02:00.000+00:00"}
        }
    })));
    let ladder = [
        "avg speed 90 t/s",
        "avg speed 90 t/s · 123456 t/s peak",
        "avg speed 90 t/s · 123456 t/s peak · 3104 tok/s peak",
    ];
    let mut bare = false;
    for w in 50..=240u16 {
        let (row, width) = dashboard_card_fit(&wide, w, 40, "avg speed")
            .unwrap_or_else(|| panic!("{w}x40: no avg speed row"));
        assert_eq!(
            row,
            widest_fitting(&ladder, width),
            "{w}x40 ({width} cells)"
        );
        bare |= row == ladder[0];
    }
    assert!(bare, "some width sheds the speed peak too");
}

/// A rate peak with no speed peak (every response under the 100-token floor)
/// still shows: each peak part stands on its own.
#[test]
fn the_total_row_shows_a_rate_peak_without_a_speed_peak() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(Some(serde_json::json!({
        "ever": {"rate": {"value": 3104.2, "at": "2026-09-20T21:02:00.000+00:00"}}
    })));
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "avg speed").as_deref(),
        Some("avg speed 90 t/s · 3104 tok/s peak")
    );
}

/// A cache past its freshness window shows no peak anywhere, even when it
/// carries peaks: every peak read goes through the fresh snapshot.
#[test]
fn a_stale_snapshot_shows_no_peak() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = tokens_app_with_peaks(None);
    let mut snap = tps_tokens_snapshot(10 * 60 * 1000);
    snap.peaks = serde_json::from_value(peaks_fixture()).unwrap();
    app.set_tps(Some(snap));
    assert!(app.fresh_tps().is_none(), "a 10-minute-old cache is stale");
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "— now").as_deref(),
        Some("speed     — now · — today")
    );
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "avg speed").as_deref(),
        Some("avg speed 90 t/s")
    );
}

/// The today card's speed row sheds whole parts from the right (the peak,
/// then the `today` figure), never clipping a part mid-way, at every width
/// from the narrow layout up.
#[test]
fn the_today_speed_row_sheds_whole_parts_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(Some(peaks_fixture()));
    let ladder = [
        "speed     92 t/s now",
        "speed     92 t/s now · 90 t/s today",
        "speed     92 t/s now · 90 t/s today · 1803 tok/s peak",
    ];
    for w in 50..=240u16 {
        let (row, width) = dashboard_card_fit(&app, w, 40, "t/s now")
            .unwrap_or_else(|| panic!("{w}x40: no today speed row"));
        assert_eq!(
            row,
            widest_fitting(&ladder, width),
            "{w}x40 ({width} cells)"
        );
    }
}

/// An absent peak adds nothing: a fresh producer with no peaks, and no
/// producer at all, leave both rows exactly as they read before peaks.
#[test]
fn an_absent_peak_leaves_the_speed_rows_unchanged() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(None);
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "t/s now").as_deref(),
        Some("speed     92 t/s now · 90 t/s today")
    );
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "avg speed").as_deref(),
        Some("avg speed 90 t/s")
    );

    let mut app = tokens_app_with_peaks(Some(peaks_fixture()));
    app.set_tps(None);
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "— now").as_deref(),
        Some("speed     — now · — today")
    );
    assert_eq!(
        dashboard_card_row(&app, 240, 40, "avg speed").as_deref(),
        Some("avg speed 90 t/s")
    );
}

/// A later part never shows when an earlier one shed: with a wide speed peak
/// and a narrow rate peak, a card too narrow for the speed peak reads the bare
/// row, never the rate peak alone.
#[test]
fn the_total_rate_peak_never_shows_past_a_shed_speed_peak() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(Some(serde_json::json!({
        "ever": {
            "speed": {"value": 123456.0, "at": "2026-09-20T21:14:00.000+00:00", "account": "kerry", "model": "claude-opus-4-8"},
            "rate": {"value": 5.0, "at": "2026-09-20T21:02:00.000+00:00"}
        }
    })));
    let ladder = [
        "avg speed 90 t/s",
        "avg speed 90 t/s · 123456 t/s peak",
        "avg speed 90 t/s · 123456 t/s peak · 5.0 tok/s peak",
    ];
    // The rows tell a correct shed from a rate-past-a-shed-speed one only on a
    // card of exactly 33 cells: the rate-alone row (33) fits, the speed rung
    // (34) does not. The sweep must reach it.
    let mut telling = false;
    for w in 50..=240u16 {
        let (row, width) = dashboard_card_fit(&app, w, 40, "avg speed")
            .unwrap_or_else(|| panic!("{w}x40: no avg speed row"));
        assert_eq!(
            row,
            widest_fitting(&ladder, width),
            "{w}x40 ({width} cells)"
        );
        telling |= width == 33;
    }
    assert!(telling, "no swept width gave the 33-cell card");
}

/// The today row's peak never shows past a shed `today` part: a wide `today`
/// figure and a narrow peak, at every text width 20..=60, read the longest
/// whole prefix.
#[test]
fn the_today_peak_never_shows_past_a_shed_today_part() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = tokens_app_with_peaks(Some(serde_json::json!({
        "today": {"rate": {"value": 5.0, "at": "2026-09-22T21:02:00.000+00:00"}}
    })));
    let today = crate::tps::DaySum {
        output: 123_000,
        gen_ms: 1_000,
        ..Default::default()
    };
    let ladder = [
        "speed     92 t/s now",
        "speed     92 t/s now · 123000 t/s today",
        "speed     92 t/s now · 123000 t/s today · 5.0 tok/s peak",
    ];
    for w in 20..=60usize {
        let line = super::today_speed_line_within(&app, Some(today), w);
        assert_eq!(line_text(&line), widest_fitting(&ladder, w), "w={w}");
    }
}

/// The typed stats-cache failures map to their lowercase cause lines, driven
/// from a real `load_base` on a sandboxed dir.
#[test]
fn each_stats_cache_failure_maps_to_its_cause_line() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = home.home().join(".claude");
    std::fs::create_dir_all(&dir).unwrap();

    let missing = crate::tokens::load_base(&dir).expect_err("missing file fails");
    assert_eq!(missing.cause_line(), "no such file");

    std::fs::write(dir.join("stats-cache.json"), "{not json").unwrap();
    let invalid = crate::tokens::load_base(&dir).expect_err("invalid json fails");
    assert_eq!(
        invalid.cause_line(),
        "not valid stats json: key must be a string at line 1 column 2"
    );

    std::fs::remove_file(dir.join("stats-cache.json")).unwrap();
    std::fs::create_dir(dir.join("stats-cache.json")).unwrap();
    let unreadable = crate::tokens::load_base(&dir).expect_err("a directory fails");
    // One cause on every OS: Windows refuses a directory with an access error,
    // and the directory still reads as a directory there.
    assert_eq!(unreadable.cause_line(), "read failed: is a directory");
    std::fs::remove_dir(dir.join("stats-cache.json")).unwrap();

    std::fs::write(dir.join("stats-cache.json"), [0xff, 0xfe, 0x00]).unwrap();
    let not_utf8 = crate::tokens::load_base(&dir).expect_err("non-UTF-8 bytes fail");
    assert_eq!(not_utf8.cause_line(), "read failed: invalid data");
    std::fs::remove_file(dir.join("stats-cache.json")).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let cache = dir.join("stats-cache.json");
        std::fs::write(&cache, "{}").unwrap();
        let mut perms = std::fs::metadata(&cache).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&cache, perms).unwrap();
        let denied = crate::tokens::load_base(&dir).expect_err("unreadable file fails");
        assert_eq!(denied.cause_line(), "permission denied");
    }
}

/// The access error Windows returns for a directory reads as a directory on
/// every OS, while the same error on a file stays a permission cause.
#[test]
fn a_permission_error_on_a_directory_names_the_directory() {
    let home = crate::testutil::HomeSandbox::new();
    let dir = home.home().join(".claude");
    std::fs::create_dir_all(dir.join("stats-cache.json")).unwrap();
    let file = dir.join("other.json");
    std::fs::write(&file, "{}").unwrap();
    let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

    assert_eq!(
        crate::tokens::read_error(&denied, &dir.join("stats-cache.json")).cause_line(),
        "read failed: is a directory"
    );
    assert_eq!(
        crate::tokens::read_error(&denied, &file).cause_line(),
        "permission denied"
    );
}

/// An error kind std leaves uncategorized names its raw OS code instead of a
/// word that says nothing.
#[test]
fn an_uncategorized_read_error_names_its_raw_os_code() {
    let home = crate::testutil::HomeSandbox::new();
    let file = home.home().join("other.json");
    std::fs::write(&file, "{}").unwrap();
    // 999 is no errno or Win32 code std maps to a named kind.
    let unnamed = std::io::Error::from_raw_os_error(999);
    assert_eq!(
        crate::tokens::read_error(&unnamed, &file).cause_line(),
        "read failed: os error 999"
    );
}
