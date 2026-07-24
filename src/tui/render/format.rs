//! Shared formatters and style helpers used across multiple screens.
//! Screen-only helpers stay in their own modules.

use chrono::{DateTime, Datelike, Local, NaiveDateTime, Timelike, Weekday};
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use super::super::theme;
use crate::format::endpoint_label;
use crate::profile::{AppState, ClockFormat, Profile, ResetDisplay};
use crate::usage::{
    FetchStatus, ProfileActivity, UsageWindow, humanize_duration, iso_to_epoch_secs, now_epoch_secs,
};

pub(super) fn fixed(value: &str, width: usize) -> String {
    let (mut content, pad) = fixed_split(value, width);
    content.push_str(&pad);
    content
}

/// Truncates with `…` and returns `(content, padding)` separately so callers
/// can style only the content without decorations bleeding past the text.
pub(super) fn fixed_split(value: &str, width: usize) -> (String, String) {
    let mut content = String::with_capacity(width);
    let mut count = 0;
    // Peek instead of a plain `for`: the loop head must not consume the char
    // AFTER the window, or a value exactly one char over `width` reads as
    // exhausted and gets silently cropped with no ellipsis
    // ("Max 20x" at 6 → "Max 20" instead of "Max 2…").
    let mut iter = value.chars().peekable();
    while count < width {
        let Some(ch) = iter.next() else { break };
        content.push(ch);
        count += 1;
    }
    if width > 0 && iter.peek().is_some() {
        content.pop();
        content.push('…');
        count = width;
    }
    let pad = " ".repeat(width - count);
    (content, pad)
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_format.rs"]
mod tests;

/// Fetch-state cue for a profile's bracketed bars: amber while the row serves
/// last-known numbers (cached / rate-limited both do — matches the amber
/// `rate limited` badge on the usage detail line), red when the fetch failed,
/// `None` when live.
pub(super) fn fetch_cue_color(profile: &Profile) -> Option<Color> {
    if !profile.is_oauth() {
        return None;
    }
    match profile.fetch_status {
        Some(FetchStatus::Cached | FetchStatus::RateLimited) => Some(theme::warning_color()),
        Some(FetchStatus::Failed) => Some(theme::danger_color()),
        _ => None,
    }
}

/// The cue color when one is live, else the resting style (dim brackets,
/// faint no-data dash).
pub(super) fn cue_style(cue: Option<Color>, resting: Style) -> Style {
    cue.map(|c| Style::default().fg(c)).unwrap_or(resting)
}

pub(super) fn account_type_label(profile: &Profile) -> String {
    // The harness tag: a codex profile's kind column names its CLI, so the
    // accounts list reads at a glance which rows switch which tool (CDX-1 T8).
    if profile.is_codex() {
        return "Codex".to_string();
    }
    if !profile.is_oauth() {
        return "API".to_string();
    }
    let label = endpoint_label(profile);
    label
        .strip_prefix("Claude ")
        .unwrap_or(label.as_str())
        .to_string()
}

pub(super) fn bar_string_with_cells(pct: f64, cells: usize) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    format!("{}{}", "█".repeat(filled), "░".repeat(cells - filled))
}

/// Cells the bar block occupies at the widest tier: `[` + 10 cells + `]` +
/// ` 100%`. Whatever the reset suffix adds has to fit in the rest of the column,
/// which is padded to a fixed width by the caller.
const BAR_BLOCK_COLS: usize = 17;

/// Overview accounts rows only: `[███░░░]` with dim brackets around the bar.
/// Brackets render in `dim`; filled/empty cells keep their semantic util color,
/// unless `stale` is set — a past-reset window's fill + `%` fade to
/// `theme::faint()` instead, since the reading is frozen pre-reset. A param
/// rather than a post-render pass (cf. `overview.rs`'s disabled-row `flatten`
/// closure) so staleness reaches every caller uniformly.
/// `reset_style` colors the trailing ` (reset)` countdown (wide layout only) —
/// pass the drain hue for a live window, `None` to keep it faint.
/// All other bar sites use [`bar_string_with_cells`] directly (no brackets).
pub(super) fn window_summary_spans_bracketed(
    window: Option<&UsageWindow>,
    width: usize,
    include_bar: bool,
    reset_style: Option<Style>,
    fmt: ResetFmt,
    stale: bool,
) -> Vec<Span<'static>> {
    let Some(window) = window else {
        return vec![Span::styled("—".to_string(), theme::faint())];
    };
    let bracket = theme::dim();
    let pct = window.utilization.clamp(0.0, 100.0);
    let fill_style = if stale {
        theme::faint()
    } else {
        Style::default().fg(theme::util_color(pct))
    };

    if include_bar && width >= 26 {
        // [██████░░░░] XX% (reset)
        let mut spans = vec![
            Span::styled("[", bracket),
            Span::styled(bar_string_with_cells(pct, 10), fill_style),
            Span::styled("]", bracket),
            Span::styled(format!(" {:>3.0}%", pct), fill_style),
        ];
        if let Some(secs) = reset_in_secs(window) {
            let style = reset_style.unwrap_or_else(theme::faint);
            spans.extend(reset_suffix_spans(secs, fmt, width, style));
        }
        spans
    } else if include_bar && width >= 17 {
        // [██████░░░░] XX%
        vec![
            Span::styled("[", bracket),
            Span::styled(bar_string_with_cells(pct, 10), fill_style),
            Span::styled("]", bracket),
            Span::styled(format!(" {:>3.0}%", pct), fill_style),
        ]
    } else if include_bar && width >= 12 {
        // [███░░░] XX%  — bar shrinks to fit
        let bar_cells = width.saturating_sub(7).clamp(3, 7);
        vec![
            Span::styled("[", bracket),
            Span::styled(bar_string_with_cells(pct, bar_cells), fill_style),
            Span::styled("]", bracket),
            Span::styled(format!(" {:>3.0}%", pct), fill_style),
        ]
    } else {
        vec![Span::styled(format!("{pct:>3.0}%"), fill_style)]
    }
}

/// True when the window's stored reset instant has already passed, so its
/// utilization is a stale pre-reset reading awaiting the next fetch. A window
/// with no reset stamp (e.g. extra credits) is never past-reset.
pub(super) fn is_past_reset(window: &UsageWindow) -> bool {
    reset_in_secs(window).is_some_and(|s| s <= 0)
}

/// Seconds until the window resets (may be negative if the stamp is overdue).
pub(super) fn reset_in_secs(window: &UsageWindow) -> Option<i64> {
    let resets_at = window.resets_at.as_deref()?;
    let target = iso_to_epoch_secs(resets_at)?;
    Some(target - now_epoch_secs())
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

/// The overview column's ` (…)` suffix, fitted to the column. A stamp too wide
/// for what the bar block leaves degrades to the bare countdown rather than
/// overflowing the row — the countdown itself is never fit-checked, so the
/// stock relative form renders exactly as it did before the setting existed.
fn reset_suffix(secs: i64, fmt: ResetFmt, width: usize) -> String {
    let bare = format!(" ({})", humanize_duration(secs));
    // Under `relative` this equals `bare`, so the fit check only ever strips a
    // clock — the stock form renders exactly as it did before the setting
    // existed, at every width.
    let full = format!(" ({})", reset_column(secs, fmt));
    if BAR_BLOCK_COLS + full.chars().count() <= width {
        full
    } else {
        bare
    }
}

/// [`reset_suffix`] as styled spans, keeping the `·` that joins a countdown to
/// its stamp neutral. Only `both` mode produces one; every other shape (bare
/// countdown, clock-only) has no separator and lands as a single span.
fn reset_suffix_spans(secs: i64, fmt: ResetFmt, width: usize, style: Style) -> Vec<Span<'static>> {
    let suffix = reset_suffix(secs, fmt, width);
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

/// Relative age of an epoch-ms timestamp per the cloudy-tui Time-formatting
/// contract: single largest unit under 30 days (`4m ago`, `2h ago`, `3d ago`,
/// `2w ago`); the absolute ISO date (`2026-04-12`) at 30 days and beyond.
/// `< 1 minute` reads `just now`.
pub(super) fn relative_age(epoch_ms: u64) -> String {
    let now = crate::usage::now_ms();
    let age_secs = (now.saturating_sub(epoch_ms) / 1000) as i64;
    let mins = age_secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    let weeks = days / 7;
    if age_secs < 60 {
        "just now".to_string()
    } else if days < 1 {
        if hours < 1 {
            format!("{mins}m ago")
        } else {
            format!("{hours}h ago")
        }
    } else if days < 30 {
        if weeks < 1 {
            format!("{days}d ago")
        } else {
            format!("{weeks}w ago")
        }
    } else {
        let iso = crate::usage::epoch_secs_to_iso((epoch_ms / 1000) as i64);
        iso.split('T').next().unwrap_or(&iso).to_string()
    }
}

/// Lowercase clock label for an epoch-ms timestamp: `jun 5, 18:27`. A comma sits
/// directly after the day, no space before it. With `utc = true` appends ` utc`
/// (used only on the detail `started` row). All times are UTC.
pub(super) fn clock_label(epoch_ms: u64, utc: bool) -> String {
    // `epoch_secs_to_iso` → `YYYY-MM-DDTHH:MM:SS+00:00`.
    let iso = crate::usage::epoch_secs_to_iso((epoch_ms / 1000) as i64);
    let bytes = iso.as_bytes();
    // Defensive: a malformed ISO string degrades to the raw value.
    if bytes.len() < 16 || bytes[10] != b'T' {
        return iso;
    }
    let mon = month_label(iso[5..7].parse().unwrap_or(1));
    // Day without a leading zero.
    let day: u32 = iso[8..10].parse().unwrap_or(0);
    let hm = &iso[11..16];
    let suffix = if utc { " utc" } else { "" };
    format!("{mon} {day}, {hm}{suffix}")
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
