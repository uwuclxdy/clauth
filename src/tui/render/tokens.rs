//! Tokens tab — global Claude Code token usage read from `~/.claude`
//! (`stats-cache.json` + recent transcript top-up; see `crate::tokens`).
//!
//! Two views. The **dashboard** (landing page) is a fixed grid of bordered
//! cards — today, lifetime totals, daily trend, top models, token composition,
//! hour-of-day, and activity — so each metric reads on its own rather than as
//! one long scroll. The **Models** master-detail (reached with `⏎`) drills into
//! a single model. All figures are global across every model/provider Claude
//! Code has run — the on-disk pool is shared across clauth profiles, not
//! per-account.
//!
//! Both views obey the [`TokenPeriod`] lens (`t` / action menu): `lifetime`
//! is the untouched all-time dashboard; `daily`/`weekly`/`monthly` scope the
//! cards to the current calendar bucket and re-bucket the trend rows. Where
//! the data can't follow — pre-cutoff days publish no cache/in-out split —
//! cards fall back to lifetime (badged) and costs render as `+`-marked floors.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, TokenFilter, TokenPeriod, TokenView, token_period_models};
use super::super::theme;
use super::format::{fixed, spinner_frame};
use super::panes::{
    draw_selector_list, master_detail, picker_row, section_box, section_box_loading,
    section_box_verbatim,
};
use crate::pricing::PriceTable;
use crate::tokens::{
    ModelTokens, PeriodModel, TokenStats, bucket_activity, bucket_tokens, current_bucket_bounds,
    effective_cache_basis, is_anthropic, model_display_name, today_date,
};

/// Key column width for label:value rows (`sessions` (8) + a 2-cell gap).
const KEY_W: usize = 10;
/// Dashboard content-width ceiling: past this the percentage-split cards
/// stretch their edge-anchored figures into scattered fragments with a dead
/// middle, so the spare columns split evenly around a centered dashboard.
const DASH_MAX_W: u16 = 120;
/// Wider key column for the spelled-out `cache read`/`cache write` rows
/// (composition card + per-model detail): `cache write` (11) + 1 trailing space,
/// so every label keeps a gap before its bar/value and the columns stay aligned.
const WIDE_KEY_W: usize = 12;
/// Block-glyph ramp for the vertical bar charts, low → high (top partial cell).
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Hour-of-day card outer width: 24 hour buckets + 4 (border + 1-col padding
/// each side), so the fixed-width chart fills the box exactly.
const HOUR_BOX_W: u16 = 28;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match app.token_view {
        TokenView::Dashboard => draw_dashboard(frame, area, app),
        TokenView::Models => draw_models(frame, area, app),
    }
}

// ── shared formatters ──────────────────────────────────────────────────────

/// Compact human count: `2.74B`, `186M`, `33.7M`, `12.3K`, `945`.
fn fmt_count(n: u64) -> String {
    let f = n as f64;
    let (v, suffix) = if f >= 1e12 {
        (f / 1e12, "T")
    } else if f >= 1e9 {
        (f / 1e9, "B")
    } else if f >= 1e6 {
        (f / 1e6, "M")
    } else if f >= 1e3 {
        (f / 1e3, "K")
    } else {
        return n.to_string();
    };
    if v >= 100.0 {
        format!("{v:.0}{suffix}")
    } else if v >= 10.0 {
        format!("{v:.1}{suffix}")
    } else {
        format!("{v:.2}{suffix}")
    }
}

/// Group the integer part of non-negative `n` with `,` thousands separators,
/// keeping `decimals` fractional digits: `(12345.6, 2)` → `12,345.60`.
fn group_thousands(n: f64, decimals: usize) -> String {
    let s = format!("{n:.decimals$}");
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(i, f)| (i, f));
    let digits = int.len();
    let mut out = String::with_capacity(digits + digits / 3 + 1 + frac.len());
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (digits - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// Full USD with 2–3 decimal precision and `,`-grouped thousands: `$12,345.67`,
/// `$340.00`, `$12.50`, `$0.340`, `<$0.001`, `$0`. No K/M/B suffix — the whole
/// figure is shown, two decimals from a dollar up and three below.
fn fmt_money(usd: f64) -> String {
    if usd <= 0.0 {
        return "$0".to_string();
    }
    if usd >= 1.0 {
        format!("${}", group_thousands(usd, 2))
    } else {
        // Sub-dollar: 3 decimals. Format first, then floor — any positive value
        // that rounds to $0.000 (incl. 0.0005 under round-half-to-even) shows
        // `<$0.001` rather than a misleading zero.
        let s = format!("${usd:.3}");
        if s == "$0.000" {
            "<$0.001".to_string()
        } else {
            s
        }
    }
}

/// Cost value style — gives the API-equivalent figures one identity across the
/// tab, distinct from the accent-bold token headline.
fn money_style() -> Style {
    Style::default().fg(theme::accent_2_color())
}

/// A `label  $cost` row summing API-equivalent cost over `models`. Shows `—` when
/// no price table has loaded yet, and a trailing `+` when the figure is a floor
/// rather than a total — some models carry tokens but no matching rate, or the
/// caller knows the token set itself is incomplete (`floor`).
fn cost_line(
    label: &str,
    prices: Option<&PriceTable>,
    models: &[ModelTokens],
    floor: bool,
) -> Line<'static> {
    let value = match prices {
        Some(p) => {
            let (total, unpriced) = p.total_cost(models);
            let mut s = fmt_money(total);
            if unpriced > 0 || floor {
                s.push('+');
            }
            s
        }
        None => "—".to_string(),
    };
    Line::from(vec![key(label), Span::styled(value, money_style())])
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// `2026-01-18[...]` → `jan 18`. Degrades to the raw string when too short.
fn short_date(ymd: &str) -> String {
    if ymd.len() < 10 {
        return ymd.to_string();
    }
    let month: usize = ymd[5..7].parse().unwrap_or(1);
    let day: u32 = ymd[8..10].parse().unwrap_or(0);
    let mon = MONTHS.get(month.saturating_sub(1)).copied().unwrap_or("?");
    format!("{mon} {day}")
}

/// `2026-06[-..]` → `jun`. Degrades to the raw string when too short.
fn month_label(ymd: &str) -> String {
    if ymd.len() < 7 {
        return ymd.to_string();
    }
    let month: usize = ymd[5..7].parse().unwrap_or(1);
    MONTHS
        .get(month.saturating_sub(1))
        .copied()
        .unwrap_or("?")
        .to_string()
}

/// Four-cell bouncing block for the full-width indeterminate spinner.
const INDET_BLOCK: usize = 4;

/// Height-aware vertical block-bar chart: one `cell_w`-wide column per value
/// (`gap` blank cells between columns), `height` rows tall, linear-scaled to
/// the slice max, `fill`-colored. Full `█` blocks stack from the bottom; a
/// bar's top cell is a partial `▁`..`▇` glyph when the value doesn't land on
/// an exact 8th of a row. A nonzero value always keeps the `▁` floor cell, so
/// a real day never renders blank. An all-zero slice renders a flat `▁`
/// baseline. Bars are centered within `width`. Rows are top→bottom.
fn bar_chart(
    vals: &[u64],
    width: usize,
    height: usize,
    fill: Style,
    cell_w: usize,
    gap: usize,
) -> Vec<Line<'static>> {
    bar_chart_scaled(vals, width, height, fill, false, cell_w, gap)
}

/// `bar_chart` on a square-root scale, for outlier-heavy time series: linear
/// let one 982M day flatten months of normal use to sub-cell noise, and the
/// p95 hard-clip that replaced it rendered every above-cap day as an identical
/// full-height wall. sqrt keeps ordering and the peak cluster's shape while
/// quiet days stay readable; heights aren't proportional to values, so the
/// peak caption names the true max.
fn bar_chart_sqrt(
    vals: &[u64],
    width: usize,
    height: usize,
    fill: Style,
    cell_w: usize,
    gap: usize,
) -> Vec<Line<'static>> {
    bar_chart_scaled(vals, width, height, fill, true, cell_w, gap)
}

fn bar_chart_scaled(
    vals: &[u64],
    width: usize,
    height: usize,
    fill: Style,
    sqrt: bool,
    cell_w: usize,
    gap: usize,
) -> Vec<Line<'static>> {
    if height == 0 || vals.is_empty() {
        return Vec::new();
    }
    let max = vals.iter().copied().max().unwrap_or(0);
    // Height of each bar in eighth-cells (0..=height*8). No data → a flat 1/8
    // baseline so an idle window still shows a floor rather than blank space.
    let row_cap = (height * 8) as f64;
    let eighths: Vec<usize> = if max == 0 {
        vec![1; vals.len()]
    } else {
        vals.iter()
            .map(|&v| {
                let ratio = v as f64 / max as f64;
                let scaled = if sqrt { ratio.sqrt() } else { ratio };
                let e = (scaled * row_cap).round() as usize;
                if v > 0 { e.max(1) } else { e }
            })
            .collect()
    };
    let chart_w = vals.len() * cell_w + vals.len().saturating_sub(1) * gap;
    let pad = width.saturating_sub(chart_w) / 2;
    (0..height)
        .map(|row| {
            // Row 0 is the top; count each bar's filled cells up from the bottom.
            let from_bottom = height - row; // 1..=height
            let mut s = String::with_capacity(chart_w * 3);
            for (i, &e) in eighths.iter().enumerate() {
                if i > 0 {
                    s.extend(std::iter::repeat_n(' ', gap));
                }
                let full = e / 8;
                let rem = e % 8;
                let ch = if from_bottom <= full {
                    '█'
                } else if from_bottom == full + 1 && rem > 0 {
                    SPARK[rem - 1]
                } else {
                    ' '
                };
                s.extend(std::iter::repeat_n(ch, cell_w));
            }
            Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(s, fill)])
        })
        .collect()
}

/// Column layout for `n` buckets across `width` cells: `(cell_w, gap)`.
/// Buckets widen to multi-cell bars with a 1-cell gap once every one can hold
/// ≥ 2 cells — discrete, countable bars instead of a thin centered blob —
/// capped at 8 wide so a handful of monthly buckets doesn't render as walls.
/// Falls back to dense 1-cell contiguous columns when the buckets outnumber
/// the space.
fn bucket_layout(n: usize, width: usize) -> (usize, usize) {
    // Widest cell satisfying n*cell + (n-1) gaps <= width; empty series → thin.
    let cell = (width + 1)
        .checked_div(n)
        .map_or(0, |c| c.saturating_sub(1))
        .min(8);
    if cell >= 2 { (cell, 1) } else { (1, 0) }
}

/// Month labels under a dated bucket axis: one at the first visible bucket,
/// then at each bucket opening a new month. A label that would run into the
/// previous one is skipped (dense daily axes keep them ~a month apart anyway).
fn month_ticks(dates: &[&str], width: usize, cell_w: usize, gap: usize) -> Line<'static> {
    let chart_w = dates.len() * cell_w + dates.len().saturating_sub(1) * gap;
    let pad = width.saturating_sub(chart_w) / 2;
    let mut out = String::new();
    let mut prev_month = "";
    for (i, date) in dates.iter().enumerate() {
        if date.len() < 7 || date[5..7] == *prev_month {
            continue;
        }
        prev_month = &date[5..7];
        let col = i * (cell_w + gap);
        if !out.is_empty() && col < out.chars().count() + 1 {
            continue;
        }
        out.push_str(&" ".repeat(col - out.chars().count()));
        out.push_str(&month_label(date));
    }
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(out, theme::faint()),
    ])
}

/// Full-width indeterminate spinner (cloudy-tui): a 4-cell `ACCENT` `█` block
/// bouncing across a `░` `LINE` track inside a `[ ]` `LINE` frame, `label`
/// trailing in `TEXT_DIM`. Position is a triangle wave over `tick` so it rides
/// the app's one 80ms tick clock (no second timer).
fn indeterminate_bar(tick: u64, track: usize, label: &str) -> Line<'static> {
    let max = track.saturating_sub(INDET_BLOCK);
    let pos = if max == 0 {
        0
    } else {
        let period = 2 * max;
        let phase = (tick as usize) % period;
        if phase <= max { phase } else { period - phase }
    };
    let after = track.saturating_sub(pos + INDET_BLOCK);
    Line::from(vec![
        Span::styled("[", theme::line()),
        Span::styled("░".repeat(pos), theme::line()),
        Span::styled("█".repeat(INDET_BLOCK.min(track)), theme::accent()),
        Span::styled("░".repeat(after), theme::line()),
        Span::styled("]", theme::line()),
        Span::styled(format!("  {label}"), theme::dim()),
    ])
}

/// Full-width determinate progress run — bare `█`/`░` per the contract (the
/// `[ ]` frame is the indeterminate variant's tell), label trailing in dim.
fn determinate_bar(done: usize, total: usize, track: usize, label: &str) -> Line<'static> {
    let filled = (done * track).checked_div(total).unwrap_or(0).min(track);
    Line::from(vec![
        Span::styled("█".repeat(filled), theme::accent()),
        Span::styled("░".repeat(track.saturating_sub(filled)), theme::line()),
        Span::styled(format!("  {label}"), theme::dim()),
    ])
}

/// `█`×filled + `░`×rest, `value` scaled against `max`, in `fill`.
fn hbar(value: u64, max: u64, width: usize, fill: Style) -> Vec<Span<'static>> {
    let filled = if max == 0 {
        0
    } else {
        (((value as f64 / max as f64) * width as f64).round() as usize).min(width)
    };
    vec![
        Span::styled("█".repeat(filled), fill),
        Span::styled(
            "░".repeat(width.saturating_sub(filled)),
            theme::line_strong(),
        ),
    ]
}

/// Fixed-width key span in the dim+bold label style — left-justified to `KEY_W`
/// so values/bars in adjacent rows line up regardless of label length.
fn key(label: &str) -> Span<'static> {
    Span::styled(format!("{label:<KEY_W$}"), theme::label())
}

/// Inner content width of a card (`section_box` border + 1-col horizontal padding).
fn inner_w(area: Rect) -> usize {
    (area.width as usize).saturating_sub(4)
}

/// Inner content height of a card (top + bottom border rows).
fn inner_h(area: Rect) -> usize {
    (area.height as usize).saturating_sub(2)
}

/// Last `width`-bounded tail of a chronological slice (the recent days).
fn trail<T>(items: &[T], width: usize) -> &[T] {
    let n = items.len().min(width.max(1));
    &items[items.len().saturating_sub(n)..]
}

/// Total display columns of a span run.
fn span_w(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// A row with `left` flush to the start and `right` flush to `width`, the gap
/// filled with spaces. cloudy-tui leans on alignment + color to separate facts,
/// not a `·` middot (that's reserved for banner/toast prose).
fn lr(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let gap = width.saturating_sub(span_w(&left) + span_w(&right)).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// A row with `spans` centered within `width`.
fn center(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(span_w(&spans)) / 2;
    let mut out = vec![Span::raw(" ".repeat(pad))];
    out.extend(spans);
    Line::from(out)
}

fn busiest_hour(hours: &[u64; 24]) -> Option<usize> {
    let (hour, &count) = hours.iter().enumerate().max_by_key(|&(_, c)| *c)?;
    (count > 0).then_some(hour)
}

// ── dashboard view ─────────────────────────────────────────────────────────

/// Center `area` within itself at `max_w` columns when it's wider.
fn clamp_width(area: Rect, max_w: u16) -> Rect {
    if area.width <= max_w {
        return area;
    }
    Rect {
        x: area.x + (area.width - max_w) / 2,
        width: max_w,
        ..area
    }
}

/// Card rectangles for the dashboard grid, resolved per layout mode.
struct DashRects {
    first: Rect,
    total: Rect,
    trend: Rect,
    models: Rect,
    comp: Rect,
    hour: Rect,
    activity: Rect,
}

/// Two-column reflow gates: enough width for a card column plus a chart column
/// that still beats the single-column band, enough height for the full card
/// stack (6+6+7+6 rows) with a usable hour chart below it.
const TWO_COL_MIN_W: u16 = 140;
const TWO_COL_MIN_H: u16 = 30;
/// Card (left) column width in the two-column layout.
const CARD_COL_W: u16 = 56;

/// On a big terminal the dashboard reflows to two columns — cards stacked on
/// the left, the trend + activity charts taking the whole right column, so
/// extra width buys visible history and extra height buys chart resolution
/// instead of dead margins. Otherwise the single-column grid, centered in the
/// `DASH_MAX_W` band (past it the percentage-split cards stretch their
/// edge-anchored figures into scattered fragments).
fn dash_rects(area: Rect) -> DashRects {
    // Phone width: every card pair stacks — a 45-col terminal has no room for
    // side-by-side cards (the TODAY card alone needs ~20 content cells). Rows
    // are the abundant resource there; a short terminal clips the trailing
    // charts (hour/activity) rather than crushing every card.
    if super::panes::narrow(area.width) {
        let rows: [Rect; 7] = Layout::vertical([
            Constraint::Length(6), // today / this week / this month
            Constraint::Length(6), // total
            Constraint::Length(4), // trend (compact)
            Constraint::Length(7), // top models
            Constraint::Length(6), // composition
            Constraint::Length(9), // hour of day (24 buckets fit 45 cols)
            Constraint::Min(0),    // activity takes whatever is left
        ])
        .areas(area);
        return DashRects {
            first: rows[0],
            total: rows[1],
            trend: rows[2],
            models: rows[3],
            comp: rows[4],
            hour: rows[5],
            activity: rows[6],
        };
    }

    if area.width >= TWO_COL_MIN_W && area.height >= TWO_COL_MIN_H {
        let cols: [Rect; 2] =
            Layout::horizontal([Constraint::Length(CARD_COL_W), Constraint::Fill(1)]).areas(area);
        let left: [Rect; 5] = Layout::vertical([
            Constraint::Length(6), // today / this week / this month
            Constraint::Length(6), // total
            Constraint::Length(7), // top models
            Constraint::Length(6), // composition
            Constraint::Min(5),    // hour of day (grows)
        ])
        .areas(cols[0]);
        let right: [Rect; 2] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(cols[1]);
        return DashRects {
            first: left[0],
            total: left[1],
            models: left[2],
            comp: left[3],
            hour: left[4],
            trend: right[0],
            activity: right[1],
        };
    }

    let area = clamp_width(area, DASH_MAX_W);
    // Grow the trend card toward a ~10-row cap so its chart breathes, leaving the
    // bottom row (hour + activity) its 4-row floor plus the rest of the height.
    // Falls back to the old 4-row trend on short terminals.
    let trend_h = area.height.saturating_sub(6 + 7 + 4).clamp(4, 10);
    let rows: [Rect; 4] = Layout::vertical([
        Constraint::Length(6), // today/this week/this month · total (incl. cost row)
        Constraint::Length(trend_h), // daily/weekly/monthly trend (growable)
        Constraint::Length(7), // top models · composition
        Constraint::Min(4),    // hour · activity (takes the rest)
    ])
    .areas(area);
    let top = halves(rows[0], 42);
    let mid = halves(rows[2], 55);
    // Hour graph is a fixed 24-bucket chart (one cell/hour). Pin its box width
    // to 24 + 4 (border + 1-col padding each side) so the graph fills it with
    // no gap; activity takes the rest and shows more history on wide terminals.
    let bot: [Rect; 2] =
        Layout::horizontal([Constraint::Length(HOUR_BOX_W), Constraint::Fill(1)]).areas(rows[3]);
    DashRects {
        first: top[0],
        total: top[1],
        trend: rows[1],
        models: mid[0],
        comp: mid[1],
        hour: bot[0],
        activity: bot[1],
    }
}

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(stats) = app.token_stats.as_ref() else {
        let area = clamp_width(area, DASH_MAX_W);
        let block = section_box("tokens", false, true);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let line = if app.tokens_failed {
            Line::from(Span::styled(
                "~/.claude/stats-cache.json unreadable",
                theme::danger(),
            ))
        } else {
            // Pre-first-paint: only the stats-cache parse runs before `Base`
            // seeds the tab, so the label names that stage. Reserve the `[ ]`,
            // its gap, and the label from the track.
            const LABEL: &str = "parsing stats-cache.json…";
            let track = inner_w(area)
                .saturating_sub(2 + 2 + LABEL.chars().count())
                .clamp(INDET_BLOCK, 40);
            indeterminate_bar(app.tick_count, track, LABEL)
        };
        frame.render_widget(Paragraph::new(line).style(theme::base()), inner);
        return;
    };

    let count_cache = app.config().state.count_cache;
    let prices = app.price_table.as_ref();
    let period = app.token_period;
    // While the transcript top-up is in flight, the first card's title carries a
    // braille spinner (one clock — the app's tick frame).
    let card_spin = app.tokens_topping_up.then(|| spinner_frame(app.tick_count));

    let r = dash_rects(area);
    if let Some(bucket) = period.bucket() {
        // Scoped first card: the current calendar bucket, meta = its start day.
        let (from, to) = current_bucket_bounds(&today_date(), bucket);
        // `→` = "from this day"; a `+` here would collide with the cost floor's
        // `$X+` suffix inches below it.
        let meta = format!("{} →", short_date(&from));
        card(
            frame,
            r.first,
            period.badge().unwrap_or("period"),
            Some(&meta),
            true,
            card_spin,
            period_lines(stats, inner_w(r.first), count_cache, prices, &from, &to),
        );
    } else {
        // Today's date → the today card's title-right meta badge.
        let today_meta = stats.today.as_ref().map(|t| short_date(&t.date));
        card(
            frame,
            r.first,
            "today",
            today_meta.as_deref(),
            true,
            card_spin,
            today_lines(stats, inner_w(r.first), count_cache, prices),
        );
    }
    // Total stays the lifetime anchor in every period — the scoped window
    // already owns the first card.
    let (total_body, total_meta) = total_lines(stats, inner_w(r.total), count_cache, prices);
    card(
        frame,
        r.total,
        "total",
        total_meta.as_deref(),
        false,
        None,
        total_body,
    );

    // Freshness badge → the trend card's title-right meta slot. Before the first
    // top-up lands there is no `live thru` date, so a topping-up spinner stands in.
    let trend_meta = if let Some(d) = stats.topped_up_through.as_deref() {
        Some(format!("live thru {}", short_date(d)))
    } else if app.tokens_topping_up {
        // Sweep counts (when the loader has reported any) ride the spinner.
        let count = app
            .tokens_progress
            .map(|(d, t)| format!(" {d}/{t}"))
            .unwrap_or_default();
        Some(format!("{} scanning{count}", spinner_frame(app.tick_count)))
    } else {
        None
    };
    let trend_title = match period {
        TokenPeriod::Weekly => "by week",
        TokenPeriod::Monthly => "by month",
        TokenPeriod::Lifetime | TokenPeriod::Daily => "daily",
    };
    // A fresh install has no daily history to chart while the first sweep runs;
    // the trend interior carries the full-width scanning bar instead of the
    // bare "no daily data" empty state (determinate once counts arrive).
    let trend_body = if stats.daily.is_empty() && app.tokens_topping_up {
        let label = match app.tokens_progress {
            Some((d, t)) => format!("scanning session logs {d}/{t}"),
            None => "scanning session logs".to_string(),
        };
        let track = inner_w(r.trend)
            .saturating_sub(2 + 2 + label.chars().count())
            .clamp(INDET_BLOCK, 40);
        vec![match app.tokens_progress {
            Some((d, t)) => determinate_bar(d, t, track, &label),
            None => indeterminate_bar(app.tick_count, track, &label),
        }]
    } else {
        trend_lines(stats, inner_w(r.trend), inner_h(r.trend), period)
    };
    card(
        frame,
        r.trend,
        trend_title,
        trend_meta.as_deref(),
        false,
        None,
        trend_body,
    );

    // Filter + period lenses both show as the card's title-right meta badge.
    let model_rows = token_period_models(app);
    let models_meta = join_badges(app.token_filter.badge(), Some(period.lens_badge()));
    card(
        frame,
        r.models,
        "top models",
        models_meta.as_deref(),
        false,
        None,
        model_lines(
            &model_rows,
            inner_w(r.models),
            5,
            effective_cache_basis(&model_rows, count_cache),
            prices,
            empty_models_msg(app.token_filter),
        ),
    );
    // Composition can scope honestly only to today (transcript-derived split);
    // weekly/monthly fall back to lifetime, badged as such.
    let (comp_meta, comp) = match period {
        TokenPeriod::Daily => (Some("today"), today_comp_lines(stats, inner_w(r.comp))),
        TokenPeriod::Weekly | TokenPeriod::Monthly => {
            (Some("lifetime"), comp_lines(stats, inner_w(r.comp)))
        }
        TokenPeriod::Lifetime => (Some("lifetime"), comp_lines(stats, inner_w(r.comp))),
    };
    card(frame, r.comp, "composition", comp_meta, false, None, comp);

    // Same fallback shape as composition: per-day hours exist only for today.
    let (hour_meta, hours) = match period {
        TokenPeriod::Daily => (
            Some("today"),
            stats.today.as_ref().map(|t| t.hours).unwrap_or([0; 24]),
        ),
        TokenPeriod::Weekly | TokenPeriod::Monthly | TokenPeriod::Lifetime => {
            (Some("lifetime"), stats.hour_counts)
        }
    };
    card(
        frame,
        r.hour,
        "hour of day",
        hour_meta,
        false,
        None,
        hour_lines(&hours, inner_w(r.hour), inner_h(r.hour)),
    );
    let act_meta = match period {
        TokenPeriod::Weekly => Some("by week"),
        TokenPeriod::Monthly => Some("by month"),
        TokenPeriod::Lifetime | TokenPeriod::Daily => Some("by day"),
    };
    card(
        frame,
        r.activity,
        "activity",
        act_meta,
        false,
        None,
        activity_lines(stats, inner_w(r.activity), inner_h(r.activity), period),
    );
}

/// Compose the filter + period meta badges into one title-right string.
fn join_badges(filter: Option<&'static str>, period: Option<&'static str>) -> Option<String> {
    match (filter, period) {
        (Some(f), Some(p)) => Some(format!("{f}  {p}")),
        (Some(f), None) => Some(f.to_string()),
        (None, Some(p)) => Some(p.to_string()),
        (None, None) => None,
    }
}

/// Empty-state copy for a model list: name the filter when one is narrowing,
/// else the window simply has no usage.
fn empty_models_msg(filter: TokenFilter) -> &'static str {
    if filter == TokenFilter::All {
        "no model usage yet"
    } else {
        "no models match the filter"
    }
}

/// Split a row into two columns, the left taking `left_pct` percent.
fn halves(area: Rect, left_pct: u16) -> [Rect; 2] {
    Layout::horizontal([
        Constraint::Percentage(left_pct),
        Constraint::Percentage(100 - left_pct),
    ])
    .areas(area)
}

/// Draw one bordered card with its lines. `meta` (if any) renders as a
/// right-aligned title badge (the cloudy-tui title-right meta slot); `spinner`
/// (if any) appends a braille loading frame into the title's left inset.
fn card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    meta: Option<&str>,
    first: bool,
    spinner: Option<&str>,
    lines: Vec<Line<'static>>,
) {
    let mut block = match spinner {
        Some(f) => section_box_loading(title, false, first, f),
        None => section_box(title, false, first),
    };
    if let Some(m) = meta {
        block = block.title(
            Line::from(Span::styled(format!(" {m} "), theme::dim())).alignment(Alignment::Right),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn today_lines(
    stats: &TokenStats,
    w: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
) -> Vec<Line<'static>> {
    let Some(t) = stats.today.as_ref() else {
        return vec![Line::from(Span::styled(
            "idle so far today",
            theme::faint(),
        ))];
    };
    let tokens = if count_cache { t.total() } else { t.in_out() };
    vec![
        kv_accent("tokens", fmt_count(tokens)),
        cost_line("cost", prices, &t.models, false),
        Line::from(vec![
            key("msgs"),
            Span::styled(group_thousands(t.messages as f64, 0), theme::body()),
        ]),
        lr(
            vec![Span::styled(
                format!("{} in", fmt_count(t.input)),
                theme::dim(),
            )],
            vec![Span::styled(
                format!("{} out", fmt_count(t.output)),
                theme::dim(),
            )],
            w,
        ),
    ]
}

/// The lifetime TOTAL card: kv rows grouped on the left (the old right-edge
/// `msgs`/date scatter read as fragments on wide cards), lifetime date range
/// as the returned title-right meta.
fn total_lines(
    stats: &TokenStats,
    w: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
) -> (Vec<Line<'static>>, Option<String>) {
    let last = stats
        .topped_up_through
        .as_deref()
        .or(stats.daily.last().map(|d| d.date.as_str()))
        .or(stats.last_computed_date.as_deref());
    let total = if count_cache {
        stats.total_tokens()
    } else {
        stats.total_in_out()
    };
    let lines = vec![
        lr(
            vec![
                key("tokens"),
                Span::styled(
                    fmt_count(total),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ],
            vec![Span::styled(
                format!("{:.0}% cache hit", stats.cache_hit_ratio() * 100.0),
                Style::default().fg(theme::info_color()),
            )],
            w,
        ),
        cost_line("cost", prices, &stats.models, false),
        Line::from(vec![
            key("sessions"),
            Span::styled(
                group_thousands(stats.total_sessions as f64, 0),
                theme::body(),
            ),
        ]),
        Line::from(vec![
            key("msgs"),
            Span::styled(fmt_count(stats.total_messages), theme::body()),
        ]),
    ];
    let meta = match (stats.first_session_date.as_deref(), last) {
        (Some(first), Some(latest)) => {
            Some(format!("{} → {}", short_date(first), short_date(latest)))
        }
        _ => None,
    };
    (lines, meta)
}

/// The this-week / this-month headline card. Tokens and msgs sum the calendar
/// bucket's days; cost sums the priceable (split-bearing) days and renders as
/// a `+` floor when stats-cache days hide part of the window.
fn period_lines(
    stats: &TokenStats,
    w: usize,
    count_cache: bool,
    prices: Option<&PriceTable>,
    from: &str,
    to: &str,
) -> Vec<Line<'static>> {
    let in_range = |date: &str| date >= from && date <= to;
    let rows = crate::tokens::period_models(&stats.daily_models, from, to);
    let complete = rows.iter().all(|m| m.split_complete);
    let splits: Vec<ModelTokens> = rows.iter().map(|m| m.split.clone()).collect();
    let in_out: u64 = stats
        .daily
        .iter()
        .filter(|d| in_range(&d.date))
        .map(|d| d.tokens)
        .sum();
    if in_out == 0 && rows.is_empty() {
        return vec![Line::from(Span::styled("idle so far", theme::faint()))];
    }
    // The cache-counting basis holds only when the whole window carries splits.
    let tokens = if count_cache && complete {
        splits.iter().map(ModelTokens::total).sum()
    } else {
        in_out
    };
    let msgs: u64 = stats
        .activity
        .iter()
        .filter(|a| in_range(&a.date))
        .map(|a| a.messages)
        .sum();
    vec![
        kv_accent("tokens", fmt_count(tokens)),
        cost_line("cost", prices, &splits, !complete),
        Line::from(vec![
            key("msgs"),
            Span::styled(group_thousands(msgs as f64, 0), theme::body()),
        ]),
        lr(
            vec![Span::styled(short_date(from), theme::dim())],
            vec![Span::styled(short_date(to), theme::dim())],
            w,
        ),
    ]
}

/// The trend chart — per-day columns, or calendar buckets under the weekly /
/// monthly lens (peak caption names the bucket). The bars grow to fill `h` rows,
/// caption pinned to the bottom.
fn trend_lines(stats: &TokenStats, w: usize, h: usize, period: TokenPeriod) -> Vec<Line<'static>> {
    let series = match period.bucket() {
        Some(b) => bucket_tokens(&stats.daily, b),
        None => stats.daily.clone(),
    };
    let tail = trail(&series, w);
    let vals: Vec<u64> = tail.iter().map(|d| d.tokens).collect();
    if vals.is_empty() {
        return vec![Line::from(Span::styled("no daily data", theme::faint()))];
    }
    let dates: Vec<&str> = tail.iter().map(|d| d.date.as_str()).collect();
    let (peak_v, peak_d) = series
        .iter()
        .max_by_key(|d| d.tokens)
        .map(|d| (d.tokens, d.date.clone()))
        .unwrap_or((0, String::new()));
    let peak = match period {
        TokenPeriod::Weekly => {
            format!("peak {} wk of {}", fmt_count(peak_v), short_date(&peak_d))
        }
        TokenPeriod::Monthly => format!("peak {} {}", fmt_count(peak_v), month_label(&peak_d)),
        TokenPeriod::Lifetime | TokenPeriod::Daily => {
            format!("peak {} {}", fmt_count(peak_v), short_date(&peak_d))
        }
    };
    // Bars widen to fill the card; a month tick row squeezes in under the axis
    // only when the chart keeps ≥ 2 rows of its own (same gate as hour-of-day).
    let (cell, gap) = bucket_layout(vals.len(), w);
    let ticks = h >= 4;
    let mut lines = bar_chart_sqrt(
        &vals,
        w,
        h.saturating_sub(1 + usize::from(ticks)),
        theme::accent(),
        cell,
        gap,
    );
    if ticks {
        lines.push(month_ticks(&dates, w, cell, gap));
    }
    lines.push(center(vec![Span::styled(peak, theme::faint())], w));
    lines
}

fn model_lines(
    rows: &[PeriodModel],
    w: usize,
    n: usize,
    basis: bool,
    prices: Option<&PriceTable>,
    empty_msg: &'static str,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::from(Span::styled(empty_msg, theme::faint()))];
    }
    let max = rows.first().map(|m| m.metric(basis)).unwrap_or(0);
    let names: Vec<String> = rows
        .iter()
        .take(n)
        .map(|m| model_display_name(&m.model))
        .collect();
    // Token-count strings, right-aligned to a shared column so the counts (and
    // the cost suffix after them) form clean vertical columns across rows.
    let counts: Vec<String> = rows
        .iter()
        .take(n)
        .map(|m| fmt_count(m.metric(basis)))
        .collect();
    let count_w = counts.iter().map(|s| s.chars().count()).max().unwrap_or(3);
    // Per-model API-equivalent cost suffix (empty when unpriced / not loaded),
    // right-aligned to its own shared column; a partial split reads as a floor.
    let costs: Vec<String> = rows
        .iter()
        .take(n)
        .map(|m| match prices {
            // No price table at all → no column; a loaded table with an
            // unpriced model (unknown third-party id) shows the no-value dash.
            None => String::new(),
            Some(p) => p
                .cost(&m.split)
                .map(|c| {
                    let mut s = fmt_money(c);
                    if !m.split_complete {
                        s.push('+');
                    }
                    s
                })
                .unwrap_or_else(|| "—".to_string()),
        })
        .collect();
    let cost_w = costs.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    // Only carry the cost column when the card is wide enough for label + a
    // minimum bar + the count + cost; otherwise drop it (token bars stay legible
    // on narrow terminals rather than clipping). `cost_col` includes its gap.
    let cost_col = if cost_w > 0 { cost_w + 1 } else { 0 };
    let show_cost = cost_col > 0 && w >= 6 + 2 + 8 + count_w + cost_col;
    let cost_col = if show_cost { cost_col } else { 0 };

    // Expand the label column to the longest name when there's room; only
    // truncate (via `fixed`) when the card is too narrow. Reserve the count
    // column + two 1-cell gaps + a minimum 8-cell bar + the cost column.
    let longest = names.iter().map(|s| s.chars().count()).max().unwrap_or(6);
    let max_label = w.saturating_sub(count_w + 2 + 8 + cost_col);
    let label_w = longest.clamp(6, max_label.max(6));
    let bar_w = w
        .saturating_sub(label_w)
        .saturating_sub(count_w + 2 + cost_col)
        .clamp(4, 30);
    rows.iter()
        .take(n)
        .zip(names.iter())
        .zip(counts.iter())
        .zip(costs.iter())
        .map(|(((m, name), count), cost)| {
            let fill = if is_anthropic(&m.model) {
                theme::accent()
            } else {
                theme::dim()
            };
            let val = m.metric(basis);
            let mut spans = vec![
                Span::styled(fixed(name, label_w), theme::body()),
                Span::raw(" "),
            ];
            spans.extend(hbar(val, max, bar_w, fill));
            spans.push(Span::styled(format!(" {count:>count_w$}"), theme::dim()));
            if show_cost {
                // The no-value dash stays faint — orange is the money identity.
                let style = if cost == "—" {
                    theme::faint()
                } else {
                    money_style()
                };
                // Cost column anchored to the card's right edge — the bar cap
                // (30) otherwise leaves the row's tail dangling mid-card.
                let gap = w
                    .saturating_sub(label_w + 1 + bar_w + 1 + count_w + cost_w)
                    .max(1);
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(format!("{cost:>cost_w$}"), style));
            }
            Line::from(spans)
        })
        .collect()
}

fn comp_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    comp_rows(
        stats.total_input,
        stats.total_output,
        stats.total_cache_create,
        stats.total_cache_read,
        w,
    )
}

/// Today's transcript-derived split — the one window besides lifetime whose
/// composition is fully known.
fn today_comp_lines(stats: &TokenStats, w: usize) -> Vec<Line<'static>> {
    match stats.today.as_ref() {
        Some(t) => comp_rows(t.input, t.output, t.cache_create, t.cache_read, w),
        None => vec![Line::from(Span::styled(
            "idle so far today",
            theme::faint(),
        ))],
    }
}

fn comp_rows(
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    w: usize,
) -> Vec<Line<'static>> {
    let grand = input
        .saturating_add(output)
        .saturating_add(cache_write)
        .saturating_add(cache_read);
    let bar_w = w.saturating_sub(WIDE_KEY_W).saturating_sub(6).clamp(4, 28);
    [
        ("input", input, theme::accent()),
        ("output", output, theme::success()),
        ("cache write", cache_write, theme::warning()),
        ("cache read", cache_read, theme::info()),
    ]
    .into_iter()
    .map(|(label, value, fill)| {
        let pct = if grand == 0 {
            0.0
        } else {
            value as f64 / grand as f64 * 100.0
        };
        let mut left = vec![Span::styled(
            format!("{label:<WIDE_KEY_W$}"),
            theme::label(),
        )];
        left.extend(hbar(value, grand, bar_w, fill));
        // Percentage anchored to the card's right edge (not the bar's end), so
        // the column reads as one rail however wide the card gets.
        lr(
            left,
            vec![Span::styled(format!("{pct:>3.0}%"), theme::dim())],
            w,
        )
    })
    .collect()
}

fn hour_lines(hours: &[u64; 24], w: usize, h: usize) -> Vec<Line<'static>> {
    // 24-bucket chart grown vertically — hour columns widen up to 3 cells when
    // the card has the room (contiguous: this one is a distribution, not
    // discrete buckets) — busiest hour named below it. Tick labels squeeze in
    // only when the chart keeps ≥ 2 rows of its own.
    let peak = busiest_hour(hours)
        .map(|h| format!("peak {h:02}:00"))
        .unwrap_or_default();
    let cell = (w / 24).clamp(1, 3);
    let ticks = h >= 4;
    let chart_h = h.saturating_sub(1 + usize::from(ticks));
    let mut lines = bar_chart(hours, w, chart_h, theme::accent(), cell, 0);
    if ticks {
        // 0/6/12/18 labels at their hour's column, matching `bar_chart`'s
        // centering so ticks sit under bars at any cell width.
        let pad = w.saturating_sub(24 * cell) / 2;
        let mut row = String::new();
        for hour in [0usize, 6, 12, 18] {
            row.push_str(&" ".repeat(hour * cell - row.chars().count()));
            row.push_str(&hour.to_string());
        }
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled(row, theme::faint()),
        ]));
    }
    lines.push(center(vec![Span::styled(peak, theme::faint())], w));
    lines
}

fn activity_lines(
    stats: &TokenStats,
    w: usize,
    h: usize,
    period: TokenPeriod,
) -> Vec<Line<'static>> {
    let series = match period.bucket() {
        Some(b) => bucket_activity(&stats.activity, b),
        None => stats.activity.clone(),
    };
    let tail = trail(&series, w);
    let msgs: Vec<u64> = tail.iter().map(|a| a.messages).collect();
    if msgs.is_empty() {
        return vec![Line::from(Span::styled("no activity data", theme::faint()))];
    }
    let dates: Vec<&str> = tail.iter().map(|a| a.date.as_str()).collect();
    // Caption reports the single busiest-by-messages bucket's own three real
    // figures — not maxima mixed across buckets plus a lifetime tool sum (which
    // read the same in every lens). Granularity word distinguishes day/wk/mo.
    let gran = match period {
        TokenPeriod::Weekly => "wk",
        TokenPeriod::Monthly => "mo",
        TokenPeriod::Lifetime | TokenPeriod::Daily => "day",
    };
    let caption = series
        .iter()
        .max_by_key(|a| a.messages)
        .map(|a| {
            format!(
                "peak {gran}: {} msgs   {} sess   {} tools",
                fmt_count(a.messages),
                a.sessions,
                fmt_count(a.tool_calls),
            )
        })
        .unwrap_or_default();
    // Same sqrt scale, bar widening, and tick gate as the trend chart —
    // activity has the same outlier shape and the same axis question.
    let (cell, gap) = bucket_layout(msgs.len(), w);
    let ticks = h >= 4;
    let mut lines = bar_chart_sqrt(
        &msgs,
        w,
        h.saturating_sub(1 + usize::from(ticks)),
        theme::accent(),
        cell,
        gap,
    );
    if ticks {
        lines.push(month_ticks(&dates, w, cell, gap));
    }
    lines.push(center(vec![Span::styled(caption, theme::faint())], w));
    lines
}

/// `key:value` line whose value is the accent-bold headline number.
fn kv_accent(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        key(label),
        Span::styled(value, theme::accent().add_modifier(Modifier::BOLD)),
    ])
}

// ── models master-detail view ──────────────────────────────────────────────

fn draw_models(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let grouped = token_period_models(app);
    // The master-detail split lives in `master_detail`: its desktop branch is
    // the upstream `selector_width(area.width) | Min(20)` horizontal split, and
    // it adds the fork's narrow (phone-width) stacked variant. Passing
    // `grouped.len()` lets the narrow selector size to its row count.
    let (selector, detail) = master_detail(area, grouped.len());
    let cols = [selector, detail];
    let sel = app.token_model_cursor.min(grouped.len().saturating_sub(1));

    // The selector title carries the active lenses so the narrowed list reads
    // as such.
    let title = match join_badges(
        app.token_filter.badge(),
        Some(app.token_period.lens_badge()),
    ) {
        Some(badge) => format!("models  {badge}"),
        None => "models".to_string(),
    };
    // A lens can empty the list mid-view (menu on the Models view) —
    // `draw_selector_list`'s shared empty state talks about accounts, so
    // render the lens-specific message instead.
    if grouped.is_empty() {
        let block = section_box(&title, true, true);
        let inner = block.inner(cols[0]);
        frame.render_widget(block, cols[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                empty_models_msg(app.token_filter),
                theme::faint(),
            )))
            .style(theme::base()),
            inner,
        );
        draw_model_detail(
            frame,
            cols[1],
            None,
            0,
            app.price_table.as_ref(),
            app.token_period,
        );
        return;
    }
    draw_selector_list(frame, cols[0], &title, true, sel, |w| {
        grouped
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let style = if is_anthropic(&m.model) {
                    Style::default().fg(theme::text_color())
                } else {
                    theme::dim()
                };
                picker_row(i == sel, true, model_display_name(&m.model), style, w)
            })
            .collect()
    });

    draw_model_detail(
        frame,
        cols[1],
        grouped.get(sel),
        period_grand(app),
        app.price_table.as_ref(),
        app.token_period,
    );
}

/// Share-of-window denominator for the detail card, on the same basis as its
/// numerator per mode: full throughput for lifetime/today, in+out for scoped
/// weeks/months (the only figure every day in range carries).
fn period_grand(app: &App) -> u64 {
    let Some(stats) = app.token_stats.as_ref() else {
        return 0;
    };
    if let Some(bucket) = app.token_period.bucket() {
        let (from, to) = current_bucket_bounds(&today_date(), bucket);
        stats
            .daily
            .iter()
            .filter(|d| d.date >= from && d.date <= to)
            .map(|d| d.tokens)
            .sum()
    } else if app.token_period == TokenPeriod::Daily {
        stats.today.as_ref().map(|t| t.total()).unwrap_or(0)
    } else {
        stats.total_tokens()
    }
}

fn draw_model_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    model: Option<&PeriodModel>,
    grand: u64,
    prices: Option<&PriceTable>,
    period: TokenPeriod,
) {
    let title = model
        .map(|m| model_display_name(&m.model))
        .unwrap_or_else(|| "model".to_string());
    let block = section_box_verbatim(&title, false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(m) = model else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("no model data", theme::faint())))
                .style(theme::base()),
            inner,
        );
        return;
    };

    // This block carries the spelled-out `cache read`/`cache write` rows, so it
    // pads to WIDE_KEY_W (not the shared KEY_W) to keep the value column aligned.
    let kv = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<WIDE_KEY_W$}"), theme::label()),
            Span::styled(value, theme::body()),
        ])
    };
    let s = &m.split;
    let mut lines = if m.split_complete {
        vec![
            kv("input", fmt_count(s.input)),
            kv("output", fmt_count(s.output)),
            kv("cache read", fmt_count(s.cache_read)),
            kv("cache write", fmt_count(s.cache_create)),
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("{:<WIDE_KEY_W$}", "total"), theme::label()),
                Span::styled(
                    fmt_count(s.total()),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ]),
            kv("in+out", fmt_count(s.in_out())),
        ]
    } else {
        // Part of the window predates the transcript cutoff, where only the
        // combined in+out per model exists — no split rows, no cache lens.
        vec![
            Line::from(vec![
                Span::styled(format!("{:<WIDE_KEY_W$}", "tokens"), theme::label()),
                Span::styled(
                    fmt_count(m.in_out),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "in+out only (older days carry no split)",
                theme::faint(),
            )),
        ]
    };

    // API-equivalent cost, split by token bucket (rates differ per bucket).
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "COST · API-EQUIVALENT",
        theme::label(),
    )));
    // Cost values share the `money_style` identity (vs the body-styled
    // token counts above).
    let cost_kv = |label: &str, value: String| {
        Line::from(vec![key(label), Span::styled(value, money_style())])
    };
    match prices {
        None => lines.push(Line::from(Span::styled("rates loading", theme::faint()))),
        Some(p) => match p.rate(&m.model) {
            None => lines.push(Line::from(Span::styled(
                "no rate for this model",
                theme::faint(),
            ))),
            Some(r) if m.split_complete => {
                let c_in = s.input as f64 * r.input;
                let c_out = s.output as f64 * r.output;
                let c_cache =
                    s.cache_read as f64 * r.cache_read + s.cache_create as f64 * r.cache_write;
                lines.push(cost_kv("input", fmt_money(c_in)));
                lines.push(cost_kv("output", fmt_money(c_out)));
                lines.push(cost_kv("cache", fmt_money(c_cache)));
                lines.push(Line::from(vec![
                    key("total"),
                    Span::styled(
                        fmt_money(c_in + c_out + c_cache),
                        money_style().add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            Some(_) => {
                // Floor over the split-bearing days only.
                let floor = p.cost(s).unwrap_or(0.0);
                lines.push(Line::from(vec![
                    key("total"),
                    Span::styled(
                        format!("{}+", fmt_money(floor)),
                        money_style().add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
        },
    }

    let bar_w = (inner.width as usize).saturating_sub(14).clamp(6, 36);

    lines.push(Line::from(""));
    let share_title = match period.badge() {
        Some(b) => format!("SHARE OF {}", b.to_uppercase()),
        None => "SHARE OF ALL TOKENS".to_string(),
    };
    lines.push(Line::from(Span::styled(share_title, theme::label())));
    // Numerator on the denominator's basis (see `period_grand`): in+out under
    // a weekly/monthly bucket, full throughput otherwise.
    let share_val = if period.bucket().is_some() {
        m.in_out
    } else {
        s.total()
    };
    let share = if grand == 0 {
        0.0
    } else {
        share_val as f64 / grand as f64 * 100.0
    };
    let mut share_line = hbar(share_val, grand, bar_w, theme::accent());
    share_line.push(Span::styled(format!(" {share:>4.1}%"), theme::dim()));
    lines.push(Line::from(share_line));

    if m.split_complete {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("CACHE HIT", theme::label())));
        let denom = s.cache_read + s.cache_create + s.input;
        let hit = if denom == 0 {
            0.0
        } else {
            s.cache_read as f64 / denom as f64
        };
        let mut hit_line = hbar(
            (hit * 100.0) as u64,
            100,
            bar_w,
            Style::default().fg(theme::info_color()),
        );
        hit_line.push(Span::styled(
            format!(" {:>3.0}%", hit * 100.0),
            theme::dim(),
        ));
        lines.push(Line::from(hit_line));
    }

    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_tokens.rs"]
mod tests;
