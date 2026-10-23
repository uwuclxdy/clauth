//! Shared formatters and style helpers used across multiple screens.
//! Screen-only helpers stay in their own modules.

use chrono::{DateTime, Datelike, Local, NaiveDateTime, Timelike, Weekday};
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use super::super::theme;
use crate::format::{account_tier, format_pct};
use crate::profile::{AppState, ClockFormat, Profile, ResetDisplay};
use crate::usage::{
    FetchStatus, ProfileActivity, UsageWindow, humanize_duration, iso_to_epoch_secs, now_epoch_secs,
};

pub(super) fn fixed(value: &str, width: usize) -> String {
    let (mut content, pad) = fixed_split(value, width);
    content.push_str(&pad);
    content
}

/// The rendered cell width of `value`: each grapheme's own width summed, the
/// measure `styled_graphemes` and the buffer use (control graphemes dropped).
/// The whole-string [`Span::width`] folds an Arabic lam-alef ligature to one
/// cell while the buffer draws it per grapheme as two, so that measure lets a
/// value overrun its budget.
pub(super) fn grapheme_cells(value: &str) -> usize {
    Span::raw(value)
        .styled_graphemes(Style::default())
        .map(|g| Span::raw(g.symbol).width())
        .sum()
}

/// Keep whole graphemes from `value`, front to back, while they fit `budget`
/// cells. Returns the kept prefix and its cell count.
pub(super) fn prefix_in_cells(value: &str, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in Span::raw(value).styled_graphemes(Style::default()) {
        let w = Span::raw(grapheme.symbol).width();
        if used + w > budget {
            break;
        }
        out.push_str(grapheme.symbol);
        used += w;
    }
    (out, used)
}

/// The graphemes of `value` in reverse order, joined (control graphemes
/// dropped, matching the renderer).
fn reversed_graphemes(value: &str) -> String {
    let mut graphemes: Vec<String> = Span::raw(value)
        .styled_graphemes(Style::default())
        .map(|g| g.symbol.to_owned())
        .collect();
    graphemes.reverse();
    graphemes.into_iter().collect()
}

/// Truncate `value` to `width` display cells with a trailing `…`, splitting
/// between graphemes — never inside one, so `⚠️` (two chars, one two-cell
/// glyph) stays whole. Returns the kept content and the cell count it occupies
/// (≤ `width`). Printable ASCII renders exactly as the old char-budgeting did.
fn truncate_to_cells(value: &str, width: usize) -> (String, usize) {
    let full = grapheme_cells(value);
    if full <= width {
        return (value.to_string(), full);
    }
    if width == 0 {
        return (String::new(), 0);
    }
    // One cell is reserved for `…`; the rest fills with whole graphemes.
    let (mut out, used) = prefix_in_cells(value, width - 1);
    out.push('…');
    (out, used + 1)
}

/// Trailing-ellipsis truncation to `width` display cells, grapheme-safe.
/// `pub(crate)` so `render` can re-export it for the app-level toast title.
pub(crate) fn truncate(value: &str, width: usize) -> String {
    truncate_to_cells(value, width).0
}

/// Truncates with `…` and returns `(content, padding)` separately so callers
/// can style only the content without decorations bleeding past the text.
/// Budgets display cells, so a CJK or emoji value keeps its `…` inside the
/// column instead of overrunning the pane edge.
pub(super) fn fixed_split(value: &str, width: usize) -> (String, String) {
    let (content, cells) = truncate_to_cells(value, width);
    let pad = " ".repeat(width.saturating_sub(cells));
    (content, pad)
}

/// Middle-ellipsis truncation (for paths / URLs / IDs — both ends carry
/// meaning). The head and tail share what the ellipsis does not use; an odd
/// budget spends its extra cell on the head. Budgets display cells, so a CJK
/// path's head and tail stay inside the value column.
pub(super) fn middle_truncate(s: &str, max: usize) -> String {
    if grapheme_cells(s) <= max || max < 3 {
        return truncate(s, max);
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let (front, _) = prefix_in_cells(s, head);
    let back = reversed_graphemes(&prefix_in_cells(&reversed_graphemes(s), tail).0);
    format!("{front}…{back}")
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_format.rs"]
mod tests;

/// Fetch-state cue for a profile's bracketed bars: amber while the row serves
/// last-known numbers (cached / rate-limited both do — matches the amber
/// `rate limited` badge on the usage detail line), red when the fetch failed or
/// the setup token was refused, `None` when live.
pub(super) fn fetch_cue_color(profile: &Profile, status: Option<FetchStatus>) -> Option<Color> {
    if !profile.is_oauth() {
        return None;
    }
    match status {
        Some(FetchStatus::Cached | FetchStatus::RateLimited) => Some(theme::warning_color()),
        Some(FetchStatus::Failed | FetchStatus::AuthExpired) => Some(theme::danger_color()),
        _ => None,
    }
}

/// The cue color when one is live, else the resting style (dim brackets,
/// faint no-data dash).
pub(super) fn cue_style(cue: Option<Color>, resting: Style) -> Style {
    cue.map(|c| Style::default().fg(c)).unwrap_or(resting)
}

/// The house no-data glyph. A caller that has to branch on "is this cell empty"
/// compares against this rather than a second copy of the literal.
pub(super) const NO_DATA: &str = "—";

/// The Overview kind column's tier chip. An account whose plan is not known yet
/// gets [`NO_DATA`]: the old bare "Claude" read as a real plan, and this column
/// is the one place a reader compares tiers side by side. The caller styles the
/// cell, so it is the caller that keeps a no-data dash out of the identity pulse.
pub(super) fn account_type_label(
    profile: &Profile,
    usage: Option<&crate::usage::UsageInfo>,
) -> String {
    if !profile.is_oauth() {
        return "API".to_string();
    }
    account_tier(profile, usage)
        .and_then(|t| t.short_label())
        .unwrap_or_else(|| NO_DATA.to_string())
}

pub(super) fn bar_string_with_cells(pct: f64, cells: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    format!("{}{}", "█".repeat(filled), "░".repeat(cells - filled))
}

/// A duration as its largest whole unit, one unit only (`45s`, `4m`, `1h`,
/// `2d`). The pool row's cooldown slot (4 cells) and the status feed's incident
/// duration share it; unlike [`crate::usage::humanize_duration`] (two units,
/// `1h 0m`), it never spills. The status caller floors sub-minute spans to
/// `1m` at the call site — a sub-minute incident reads `1m`, never `45s`.
pub(super) fn duration_label(secs: u64) -> String {
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days >= 1 {
        format!("{days}d")
    } else if hours >= 1 {
        format!("{hours}h")
    } else if mins >= 1 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// The widest whole reading, `100%`: the label budget every Overview usage tier
/// is sized for. A longer label (`73.42%`) widens its column by the excess.
pub(super) const PCT_LABEL_BUDGET: usize = 4;
const BAR_CELLS: usize = 10;
/// The bar's fill-cell floor: a column too narrow for it sheds the bar whole,
/// brackets included, rather than squeezing the reading.
const BAR_FLOOR: usize = 5;
/// What the full bar block leaves its reset countdown before it renders one:
/// ` (4h 59m)`, the widest 5h countdown.
const RESET_ROOM: usize = 9;

/// Cells a bar-tier column gives its bar once the reading's `label_w` and the
/// bracket-and-gap chrome are paid: the one rail the dash and the reading span
/// both right-align to.
fn rail_cells(width: usize, label_w: usize) -> usize {
    width.saturating_sub(3 + label_w).min(BAR_CELLS)
}

/// A window's reading as the bracketed bar prints it, so a column can size its
/// label width off the same text its rows render.
pub(super) fn window_label(window: &UsageWindow) -> String {
    format_pct(window.utilization.clamp(0.0, 100.0))
}

/// Overview accounts rows only: `[███░░░]` with dim brackets around the bar.
/// Brackets render in `dim`; filled/empty cells keep their semantic util color,
/// unless `stale` is set — a past-reset window's fill + `%` fade to
/// `theme::faint()` instead, since the reading is frozen pre-reset. A param
/// rather than a post-render pass (cf. `overview.rs`'s disabled-row `flatten`
/// closure) so staleness reaches every caller uniformly.
/// `reset_style` colors the trailing ` (reset)` countdown (wide layout only) —
/// pass the drain hue for a live window, `None` to keep it faint.
/// All other bar sites use [`bar_string_with_cells`] directly (no brackets).
/// `label_w` is the widest label the caller's column renders: the reading is
/// left-padded to it so a column's `%` signs align. `prefix_cells` is a
/// left-hand prefix the caller renders OUTSIDE this call (e.g. the overview's
/// `30d ` monthly label): the bar sheds that many cells so the whole cell
/// still fits the column and the `%` stays on the shared rail.
#[allow(clippy::too_many_arguments)]
pub(super) fn window_summary_spans_bracketed(
    window: Option<&UsageWindow>,
    width: usize,
    label_w: usize,
    include_bar: bool,
    reset_style: Option<Style>,
    fmt: ResetFmt,
    stale: bool,
    prefix_cells: usize,
) -> Vec<Span<'static>> {
    bracketed_spans(
        window,
        width,
        label_w,
        include_bar,
        reset_style,
        fmt,
        stale,
        now_epoch_secs(),
        prefix_cells,
    )
}

/// [`window_summary_spans_bracketed`] for a lone cell (the label is its own
/// column) against a caller-supplied clock, so a test can pin the fixture and
/// the render to one instant.
#[cfg(test)]
pub(crate) fn window_summary_spans_bracketed_at(
    window: Option<&UsageWindow>,
    width: usize,
    include_bar: bool,
    reset_style: Option<Style>,
    fmt: ResetFmt,
    stale: bool,
    now: i64,
) -> Vec<Span<'static>> {
    bracketed_spans(
        window,
        width,
        0,
        include_bar,
        reset_style,
        fmt,
        stale,
        now,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn bracketed_spans(
    window: Option<&UsageWindow>,
    width: usize,
    label_w: usize,
    include_bar: bool,
    reset_style: Option<Style>,
    fmt: ResetFmt,
    stale: bool,
    now: i64,
    prefix_cells: usize,
) -> Vec<Span<'static>> {
    // `[` + cells + `]` + ` ` + label: whether the bar's floor fits beside a
    // label `label_w` wide.
    let bar_fits = |label_w: usize| include_bar && width >= BAR_FLOOR + 3 + label_w;
    let Some(window) = window else {
        // The dash sits on the reading rail: right-aligned to the reading
        // span's width, so its right edge meets the readings' at every tier
        // (the bar cells, brackets and gap included).
        let dash = if bar_fits(label_w) {
            format!(
                "{NO_DATA:>right$}",
                right = 3 + rail_cells(width, label_w) + label_w
            )
        } else {
            format!("{NO_DATA:>label_w$}")
        };
        return vec![Span::styled(dash, theme::faint())];
    };
    let bracket = theme::dim();
    let pct = window.utilization.clamp(0.0, 100.0);
    let label = window_label(window);
    let label_w = label_w.max(label.len());
    let fill_style = if stale {
        theme::faint()
    } else {
        Style::default().fg(theme::util_color(pct))
    };
    // The label is the reading, so the reset countdown and then the bar cells
    // give way before it is ever cut.
    let block_w = |cells: usize| cells + 3 + label_w;
    if !bar_fits(label_w) {
        return vec![Span::styled(format!("{label:>label_w$}"), fill_style)];
    }
    let cells = rail_cells(width, label_w).saturating_sub(prefix_cells);
    let mut spans = vec![
        Span::styled("[", bracket),
        Span::styled(bar_string_with_cells(pct, cells), fill_style),
        Span::styled("]", bracket),
        Span::styled(format!(" {label:>label_w$}"), fill_style),
    ];
    let room = width.saturating_sub(block_w(BAR_CELLS));
    if room >= RESET_ROOM
        && let Some(secs) = reset_in_secs_at(window, now)
    {
        let style = reset_style.unwrap_or_else(theme::faint);
        spans.extend(reset_suffix_spans(secs, fmt, room, style));
    }
    spans
}

/// True when the window's stored reset instant has already passed, so its
/// utilization is a stale pre-reset reading awaiting the next fetch. A window
/// with no reset stamp (e.g. extra credits) is never past-reset.
pub(super) fn is_past_reset(window: &UsageWindow) -> bool {
    reset_in_secs(window).is_some_and(|s| s <= 0)
}

/// Seconds until the window resets (may be negative if the stamp is overdue).
pub(super) fn reset_in_secs(window: &UsageWindow) -> Option<i64> {
    reset_in_secs_at(window, now_epoch_secs())
}

/// [`reset_in_secs`] against a caller-supplied clock so a test can pin both
/// reads to one instant instead of racing the wall clock.
pub(crate) fn reset_in_secs_at(window: &UsageWindow, now: i64) -> Option<i64> {
    let resets_at = window.resets_at.as_deref()?;
    let target = iso_to_epoch_secs(resets_at)?;
    Some(target - now)
}

/// The operator's reset-rendering choice, snapshotted so a render pass reads the
/// config once instead of once per window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ResetFmt {
    display: ResetDisplay,
    clock: ClockFormat,
}

impl ResetFmt {
    pub(super) fn from_state(state: &AppState) -> Self {
        Self {
            display: state.reset_display(),
            clock: state.clock_format(),
        }
    }

    pub(super) fn shows_clock(self) -> bool {
        self.display.shows_clock()
    }
}

/// Usage-tab bar line: `resets in 40m` / `resets at 21:20` / `resets in 40m
/// (21:20)`.
pub(super) fn reset_phrase(secs: i64, fmt: ResetFmt) -> String {
    phrase_text(
        &humanize_duration(secs),
        clock_half(secs, fmt, Day::Qualify).as_deref(),
        fmt.display,
    )
}

/// Fallback blocked-pill reset field: `40m` / `until 21:20` / `40m (21:20)`.
pub(super) fn reset_pill(secs: i64, fmt: ResetFmt) -> String {
    pill_text(
        &humanize_duration(secs),
        clock_half(secs, fmt, Day::Qualify).as_deref(),
        fmt.display,
    )
}

/// The all-exhausted caption's tail (`resumes: kerry <tail>`): `in ~4h` /
/// `at 21:20` / `in ~4h (21:20)`. The `~` rides the countdown only — the stamp
/// is a stored `resets_at`, not an estimate.
pub(super) fn reset_resume(secs: i64, fmt: ResetFmt) -> String {
    resume_text(
        &humanize_duration(secs),
        clock_half(secs, fmt, Day::Qualify).as_deref(),
        fmt.display,
    )
}

/// Overview reset column: `40m` / `21:20` / `40m · 21:20`. `both` drops the day
/// qualifier because the countdown sits right beside the stamp and already
/// carries the day — keeping it would cost 4-9 cells in the tightest column on
/// screen and push every 7d reset back to a bare countdown.
pub(super) fn reset_column(secs: i64, fmt: ResetFmt) -> String {
    let day = match fmt.display {
        ResetDisplay::Clock => Day::Qualify,
        _ => Day::TimeOnly,
    };
    column_text(
        &humanize_duration(secs),
        clock_half(secs, fmt, day).as_deref(),
        fmt.display,
    )
}

/// Whether a stamp names the day it falls on, or renders bare because something
/// beside it already answers that.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Day {
    Qualify,
    TimeOnly,
}

// The three compositions are pure over an already-resolved clock half, so each
// surface's wording is exact-value testable in any timezone. `at: None` — the
// setting is off, or the instant wouldn't resolve — always falls back to the
// countdown, which is why no arm can drop `rel` on `None`.

fn phrase_text(rel: &str, at: Option<&str>, display: ResetDisplay) -> String {
    match at {
        None => format!("resets in {rel}"),
        Some(at) if display == ResetDisplay::Clock => format!("resets at {at}"),
        Some(at) => format!("resets in {rel} ({at})"),
    }
}

/// The clock-only pill form carries `until` because a bare stamp in a pill that
/// already reads `weekly spent · …` scans as when the block STARTED.
fn pill_text(rel: &str, at: Option<&str>, display: ResetDisplay) -> String {
    match at {
        None => rel.to_string(),
        Some(at) if display == ResetDisplay::Clock => format!("until {at}"),
        Some(at) => format!("{rel} ({at})"),
    }
}

fn resume_text(rel: &str, at: Option<&str>, display: ResetDisplay) -> String {
    match at {
        None => format!("in ~{rel}"),
        Some(at) if display == ResetDisplay::Clock => format!("at {at}"),
        Some(at) => format!("in ~{rel} ({at})"),
    }
}

/// The overview caller wraps this in parens, so the two halves join on `·`
/// rather than nesting a second pair.
fn column_text(rel: &str, at: Option<&str>, display: ResetDisplay) -> String {
    match at {
        None => rel.to_string(),
        Some(at) if display == ResetDisplay::Clock => at.to_string(),
        Some(at) => format!("{rel} · {at}"),
    }
}

/// The overview column's ` (…)` suffix, fitted to the `room` the bar block
/// leaves in the column. A stamp too wide for it degrades to the bare countdown
/// rather than overflowing the row — the countdown itself is never fit-checked,
/// so the stock relative form renders exactly as it did before the setting
/// existed.
fn reset_suffix(secs: i64, fmt: ResetFmt, room: usize) -> String {
    let bare = format!(" ({})", humanize_duration(secs));
    // Under `relative` this equals `bare`, so the fit check only ever strips a
    // clock — the stock form renders exactly as it did before the setting
    // existed, at every width.
    let full = format!(" ({})", reset_column(secs, fmt));
    if full.chars().count() <= room {
        full
    } else {
        bare
    }
}

/// [`reset_suffix`] as styled spans, keeping the `·` that joins a countdown to
/// its stamp neutral. Only `both` mode produces one; every other shape (bare
/// countdown, clock-only) has no separator and lands as a single span.
fn reset_suffix_spans(secs: i64, fmt: ResetFmt, room: usize, style: Style) -> Vec<Span<'static>> {
    let suffix = reset_suffix(secs, fmt, room);
    match suffix.split_once(" · ") {
        Some((pre, post)) => vec![
            Span::styled(pre.to_string(), style),
            Span::raw(" · "),
            Span::styled(post.to_string(), style),
        ],
        None => vec![Span::styled(suffix, style)],
    }
}

/// The wall-clock half of a reset stamp, or `None` when the operator didn't ask
/// for one, the reset is already overdue, or the instant can't be resolved.
/// Every surface falls back to the countdown on `None`, so a stamp that would
/// mislead degrades instead of blanking the reset.
///
/// The overdue gate is why no surface can promise `resets at 17:42` at 19:42:
/// `/usage` data outlives its window whenever a fetch fails, and the countdown
/// already reads `now` there.
fn clock_half(secs: i64, fmt: ResetFmt, day: Day) -> Option<String> {
    (fmt.shows_clock() && secs > 0)
        .then(|| local_clock(secs, fmt.clock, day))
        .flatten()
}

/// Wall-clock rendering of an instant `secs` from now in the operator's local
/// zone. Thin by design: the zone lookup lives here, the rendering rules live in
/// the pure [`clock_text`]. Re-reading the clock costs sub-millisecond drift
/// against the `now` the caller derived `secs` from, which can't move a minute.
fn local_clock(secs: i64, fmt: ClockFormat, day: Day) -> Option<String> {
    let now = now_epoch_secs();
    let target = local_naive(now.checked_add(secs)?)?;
    Some(clock_text(target, local_naive(now)?, fmt, day))
}

fn local_naive(epoch: i64) -> Option<NaiveDateTime> {
    Some(
        DateTime::from_timestamp(epoch, 0)?
            .with_timezone(&Local)
            .naive_local(),
    )
}

/// `21:20` on today's local date, `wed 21:20` inside the next six days,
/// `jun 27, 21:20` beyond that. The day qualifier is what makes a 7d window's
/// reset readable — a bare clock three days out doesn't say which day — so only
/// a caller with a countdown beside it passes [`Day::TimeOnly`]. Day 7 takes the
/// date because a weekday that far out collides with today's own name.
fn clock_text(target: NaiveDateTime, now: NaiveDateTime, fmt: ClockFormat, day: Day) -> String {
    let time = match fmt {
        ClockFormat::H24 => format!("{:02}:{:02}", target.hour(), target.minute()),
        ClockFormat::H12 => {
            let (pm, hour) = target.hour12();
            let meridiem = if pm { "pm" } else { "am" };
            format!("{hour}:{:02}{meridiem}", target.minute())
        }
    };
    if day == Day::TimeOnly {
        return time;
    }
    match (target.date() - now.date()).num_days() {
        0 => time,
        1..=6 => format!("{} {time}", weekday_label(target.weekday())),
        _ => format!("{} {}, {time}", month_label(target.month()), target.day()),
    }
}

fn weekday_label(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn month_label(month: u32) -> &'static str {
    MONTHS
        .get((month as usize).saturating_sub(1))
        .copied()
        .unwrap_or("jan")
}

/// Relative age of an epoch-ms timestamp: single largest unit under 30 days
/// (`4m ago`, `2h ago`, `3d ago`, `2w ago`); the local ISO date (`2026-04-12`,
/// no time) at 30 days and beyond. `< 1 minute` reads `just now`. Routes through
/// the shared [`crate::format::spoken_age`] so the CLI reads the same shape.
pub(super) fn relative_age(epoch_ms: u64) -> String {
    let now = crate::usage::now_ms();
    let age_secs = now.saturating_sub(epoch_ms) / 1000;
    crate::format::spoken_age(age_secs, (epoch_ms / 1000) as i64)
        .unwrap_or_else(|| NO_DATA.to_string())
}

use crate::spinner::SPINNER_FRAMES;

pub(super) fn spinner_frame(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick as usize) % SPINNER_FRAMES.len()]
}

/// Distinct tint per activity so states read differently at a glance.
pub(super) fn spinner_style(activity: ProfileActivity) -> Style {
    match activity {
        ProfileActivity::Fetching => theme::accent(),
        ProfileActivity::Refreshing | ProfileActivity::Switching => theme::info(),
        ProfileActivity::Queued | ProfileActivity::Idle => theme::faint(),
    }
}

pub(super) fn activity_verb(activity: ProfileActivity) -> &'static str {
    match activity {
        ProfileActivity::Fetching => "fetching",
        ProfileActivity::Refreshing => "refreshing",
        ProfileActivity::Switching => "switching",
        ProfileActivity::Queued => "queued",
        ProfileActivity::Idle => "idle",
    }
}
