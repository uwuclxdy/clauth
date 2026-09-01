//! Tokens-dashboard render tests: the height-aware bar charts, the full-width
//! loading spinner, the honest activity caption, and the granularity badges.

use super::{
    INDET_BLOCK, activity_lines, bar_chart, bar_chart_sqrt, bucket_layout, comp_rows,
    determinate_bar, hour_lines, indeterminate_bar, model_detail_cost, model_lines, today_cost,
    today_lines, total_lines, trend_lines, window_cost,
};
use crate::pricing::{Constraint, HourTokens, PriceEntry, PriceTable, PricedModel, RateSnapshot};
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
            },
        ],
    )
}

/// A table from `models`, captured today with no history.
fn table_of(models: Vec<PricedModel>) -> PriceTable {
    PriceTable::capture(
        models,
        Vec::new(),
        crate::tokens::today_date(),
        0,
        Vec::new(),
    )
}

/// A table whose newest snapshot is `models` and whose older snapshots are
/// given explicitly (`capture` appends nothing — the last history entry must
/// equal `models`).
fn table_with_history(models: Vec<PricedModel>, history: Vec<RateSnapshot>) -> PriceTable {
    PriceTable::capture(models, Vec::new(), crate::tokens::today_date(), 0, history)
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
        row(6).contains(" TOTAL "),
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
    assert!(out.contains("sessions"), "sessions is a spelled-out kv key");
    assert!(out.contains("1,000"), "session count is comma-grouped");
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
    let lines = today_lines(&stats, 60, false, Some(&table));
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
    let bare = today_lines(&stats, 60, false, None);
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
    let lines = today_lines(&stats_with_today("glm-5.2", hours), 60, false, Some(&table));
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
    };
    hours[20] = HourTokens {
        input: 500_000,
        output: 750_000,
        cache_read: 900,
        cache_create: 800,
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
            Some(&table),
            false,
            TokenPeriod::Weekly,
        );
    })
    .unwrap();
    let out = crate::testutil::buffer_rows(term.backend().buffer()).concat();
    assert!(
        out.contains("$0.010+"),
        "the total renders as a floor over the priced day, got: {out}"
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
            None,
            false,
            TokenPeriod::Weekly,
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
    let (lines, _meta) = total_lines(&stats, 60, false, Some(&table));
    assert!(
        line_text(&lines[1]).contains("$1.00+"),
        "unpriced token-bearing models floor the lifetime figure, got {:?}",
        line_text(&lines[1])
    );
    let (bare, _meta) = total_lines(&stats, 60, false, None);
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
    let (zero_lines, _meta) = total_lines(&zero_only, 60, false, Some(&table));
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
    let lines: Vec<String> = trend_lines(&stats, 48, 6, TokenPeriod::Lifetime)
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
    let short = trend_lines(&stats, 48, 3, TokenPeriod::Lifetime);
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
