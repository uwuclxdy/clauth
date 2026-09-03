//! Overview tab: accounts table + fallback flow, inside one content frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use super::super::app::{App, MainItemKind};
use super::super::theme;
use super::chain::reason_marker;
use super::format::{
    NO_DATA, ResetFmt, account_type_label, cue_style, fetch_cue_color, fixed, fixed_split,
    is_past_reset, reset_resume, spinner_frame, spinner_style, window_summary_spans_bracketed,
};
use super::header::pulse_name_spans;
use super::panes::{
    bold_when, draw_scrollbar, empty_state, name_color, section_box, select_line, wrap_words,
};
use super::usage::{eta_left_secs, window_rate_unit};
use crate::fallback::{
    BlockedReason, SwitchAction, blocked_reason, next_target, soonest_resume, threshold_for,
};
use crate::profile::{AppConfig, Profile};
use crate::providers::Provider;
use crate::usage::{
    LABEL_5H, LABEL_7D, ProfileActivity, UsageWindow, humanize_duration, now_epoch_secs, now_ms,
    switch_grade_kick_lifts,
};

/// `XXXs` + 1 trailing space = 5 chars; spinner padded to same width.
const TIMER_SLOT: usize = 5;
/// Rows the accounts table keeps before the chain panel may claim any space —
/// it is the scrollable, interactive list, so it wins the vertical budget.
const ACCOUNTS_MIN: u16 = 7;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // The chain panel's inner width is independent of the vertical split (both
    // panels span the full body width), so build its lines first and size the
    // panel to fit them — no clipped members, no wasted rows.
    let probe = Rect { height: 3, ..area };
    let chain_inner_w = section_box("", false, false).inner(probe).width as usize;
    let chain_lines = fallback_flow_lines(app, chain_inner_w);
    let chain_height = chain_panel_height(chain_lines.len(), area.height);

    let [accounts_area, chain_area] = Layout::vertical([
        Constraint::Min(ACCOUNTS_MIN),
        Constraint::Length(chain_height),
    ])
    .areas(area);

    draw_overview_accounts(frame, accounts_area, app);
    draw_fallback_overview(frame, chain_area, chain_lines);
}

/// Height for the fallback chain panel: sized to its content (`content_rows`
/// plus the 2 border rows), capped so the accounts table keeps [`ACCOUNTS_MIN`]
/// rows whenever `area_height >= ACCOUNTS_MIN + 3`. Below that the 3-row floor
/// (border + one row) wins instead and accounts gives way — a terminal too
/// short for both shrinks the accounts table, not the chain.
fn chain_panel_height(content_rows: usize, area_height: u16) -> u16 {
    let desired = (content_rows as u16).saturating_add(2);
    let max_chain = area_height.saturating_sub(ACCOUNTS_MIN);
    desired.min(max_chain).max(3)
}

fn draw_overview_accounts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Sole interactive content panel on this screen — always focused.
    let focused = true;
    let block = section_box("accounts", focused, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.config().profiles.is_empty() {
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    }

    let [header_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let widths = OverviewWidths::new(list_area.width, app);
    let header = overview_header(&widths, any_deepseek(app));
    frame.render_widget(Paragraph::new(header).style(theme::base()), header_area);

    let items = app.main_items();
    let sel = app.profile_cursor.min(items.len().saturating_sub(1));
    let width = list_area.width;
    let rows: Vec<ListItem<'_>> = items
        .iter()
        .enumerate()
        .map(|(row, item)| match item {
            MainItemKind::Profile(idx) => {
                let selected = row == sel;
                let line = render_overview_row(app, *idx, &widths, selected, focused);
                ListItem::new(select_line(line, selected, focused, width))
            }
        })
        .collect();

    let total = items.len();
    let list = List::new(rows).style(theme::base());
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, list_area, &mut state);

    let viewport = list_area.height as usize;
    draw_scrollbar(frame, list_area, total, state.offset(), viewport);
}

#[derive(Debug, Clone, Copy)]
struct OverviewWidths {
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    /// `LIVE_W` when the live-session column fits, `0` when it is dropped.
    live: usize,
    gap: usize,
    /// Widest amount string (`"1.71"` of `"1.71 USD"`) across DeepSeek rows, so
    /// currencies align in the column. `0` when no DeepSeek balance is cached.
    deepseek_amount_w: usize,
}

/// Width of the live-session column: its `live` header text, which is also the
/// widest cell any real count reaches (`99⇄`). It never widens for a bigger
/// count, because the column is budgeted on width alone and its cost has to be
/// constant for that to hold.
const LIVE_W: usize = 4;

/// Widest the name column ever reaches (the clamp ceiling at wide widths).
const NAME_MAX: usize = 22;
/// Widest the kind column ever reaches.
const KIND_MAX: usize = 16;
/// Widest the 5h column reaches before the wall-clock stamp bonus.
const FIVE_HOUR_MAX: usize = 26;
/// Widest the 7d column reaches before the wall-clock stamp bonus.
const SEVEN_DAY_MAX: usize = 27;
/// `total` at which the name clamp lets a name reach [`NAME_MAX`].
const NAME_WIDE_AT: usize = 86;
/// `total` at which the kind column reaches [`KIND_MAX`].
const KIND_WIDE_AT: usize = 92;
/// `total` at which the 5h column reaches [`FIVE_HOUR_MAX`].
const FIVE_HOUR_WIDE_AT: usize = 81;
/// `total` at which the 7d column reaches [`SEVEN_DAY_MAX`].
const SEVEN_DAY_WIDE_AT: usize = 102;
/// Minimum gap between overview columns.
const GAP_MIN: usize = 2;

impl OverviewWidths {
    fn new(width: u16, app: &App) -> Self {
        let total = width as usize;
        let max_name = app
            .config()
            .profiles
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(8);
        let shows_clock = ResetFmt::from_state(&app.config().state).shows_clock();
        let (name, kind, mut five_hour, mut seven_day) = overview_tiers(max_name, total);
        let live = live_column_width(max_name, total);

        if shows_clock {
            const CLOCK_COLS: usize = 10;
            // A wall-clock stamp needs 10 cells beyond the countdown (the worst
            // real product is `6d 23h · 12:05am`, since `reset_column` drops
            // the day qualifier once a countdown carries it). Take them ONLY
            // from slack the layout would otherwise spend on gap padding, after
            // the live column and the shrink loop have settled — so turning the
            // setting on can add a stamp but never cost a countdown, a bar or
            // the live column. Widening the tier itself instead pushed the 7d
            // column down to 5 at 130 columns, deleting its bar. A column that
            // gets only part of the 10 still degrades cleanly in
            // `reset_suffix`.
            let slack = |five: usize, seven: usize| {
                total.saturating_sub(
                    fixed_overview_width(name, kind, five, seven, live, GAP_MIN) + TIMER_SLOT,
                )
            };
            if five_hour == FIVE_HOUR_MAX {
                five_hour += CLOCK_COLS.min(slack(five_hour, seven_day));
            }
            if seven_day == SEVEN_DAY_MAX {
                seven_day += CLOCK_COLS.min(slack(five_hour, seven_day));
            }
        }

        let base = fixed_overview_width(name, kind, five_hour, seven_day, live, GAP_MIN);
        let column_count = 3 + usize::from(seven_day > 0) + usize::from(live > 0);
        let gap_slots = column_count.saturating_sub(1).max(1);
        // `fixed_overview_width` omits the TIMER_SLOT the row always renders;
        // widening gaps from that undercounted figure overflows the row at
        // narrow widths and clips the tail of the 5h column. Widen from the
        // real leftover instead.
        let gap = (GAP_MIN + total.saturating_sub(base + TIMER_SLOT) / gap_slots).clamp(GAP_MIN, 8);

        let deepseek_amount_w = app
            .config()
            .profiles
            .iter()
            .filter(|p| p.provider == Some(Provider::DeepSeek))
            .filter_map(|p| p.third_party_usage.as_ref())
            .flat_map(|s| s.rows.iter())
            .filter(|r| crate::providers::is_balance_row(&r.label))
            .filter_map(|r| r.value.rsplit_once(' ').map(|(a, _)| a.chars().count()))
            .max()
            .unwrap_or(0);

        Self {
            name,
            kind,
            five_hour,
            seven_day,
            live,
            gap,
            deepseek_amount_w,
        }
    }
}

/// The four semantic column widths after the tier ladders and the shrink loop
/// have settled at `total`. The wall-clock stamp bonus is applied by the
/// caller after the live column is decided.
fn overview_tiers(max_name: usize, total: usize) -> (usize, usize, usize, usize) {
    let mut name = max_name.clamp(8, if total >= NAME_WIDE_AT { NAME_MAX } else { 16 });
    let mut kind = if total >= KIND_WIDE_AT {
        KIND_MAX
    } else if total >= 66 {
        12
    } else {
        6
    };
    // 26 = [bar]+pct+reset, 17 = [bar]+pct only.
    let mut five_hour = if total >= FIVE_HOUR_WIDE_AT {
        FIVE_HOUR_MAX
    } else if total >= 64 {
        17
    } else {
        12
    };
    // One wider than 5h: `humanize_duration` hits 7 chars (`10h 20m`,
    // `23h 59m`) inside the 7d countdown's 10h–23h band, and the 26-cell
    // tier was sized for the 6-char ceiling. The bare-suffix fit check in
    // `reset_suffix` deliberately does not clamp the countdown itself, so
    // the column has to absorb the extra cell or it leaks into `live`.
    let mut seven_day = if total >= SEVEN_DAY_WIDE_AT {
        SEVEN_DAY_MAX
    } else if total >= 93 {
        17
    } else if total >= 58 {
        5
    } else {
        0
    };
    while fixed_overview_width(name, kind, five_hour, seven_day, 0, GAP_MIN) > total {
        if seven_day >= 17 {
            seven_day = 5;
        } else if seven_day > 0 {
            seven_day = 0;
        } else if five_hour > 17 {
            five_hour = 17;
        } else if five_hour > 12 {
            five_hour = 12;
        } else if kind > 6 {
            kind = 6;
        } else if name > 8 {
            name -= 1;
        } else {
            break;
        }
    }

    (name, kind, five_hour, seven_day)
}

/// The live-session column width at `total`: [`LIVE_W`] only when the column
/// fits here and at every wider width, `0` otherwise. Gated on width alone,
/// never on whether anything is live, so the table never reflows when a
/// session starts or stops.
///
/// Decided BEFORE the wall-clock stamp bonus, so it may take cells the stamp
/// would otherwise use — but never a name, a bar or a countdown, and
/// `reset_suffix` degrades cleanly on whatever the stamp gets.
///
/// The tier ladders jump by more than the one cell a width increment adds
/// (`kind` 12→16, `seven_day` 5→17→27, `five_hour` 17→26), so the raw fit
/// predicate can pass at one width, fail at the next and pass again. Once
/// every ladder has saturated the leftover slack grows 1:1 with width, so the
/// predicate is monotone from there on and only the bounded stretch up to that
/// saturation width needs scanning.
fn live_column_width(max_name: usize, total: usize) -> usize {
    let fits = |w: usize| {
        let (name, kind, five_hour, seven_day) = overview_tiers(max_name, w);
        fixed_overview_width(name, kind, five_hour, seven_day, LIVE_W, GAP_MIN) + TIMER_SLOT <= w
    };
    // Two quantities bound the scan: the largest ladder threshold (past it no
    // tier can still jump) and the width at which every ladder maximum plus the
    // timer fits (past it the settled tiers stop changing, so slack grows 1:1).
    // Take the larger so neither half is a coincidence the other one covers.
    let largest_ladder_at = NAME_WIDE_AT
        .max(KIND_WIDE_AT)
        .max(FIVE_HOUR_WIDE_AT)
        .max(SEVEN_DAY_WIDE_AT);
    let saturation =
        fixed_overview_width(NAME_MAX, KIND_MAX, FIVE_HOUR_MAX, SEVEN_DAY_MAX, 0, GAP_MIN)
            + TIMER_SLOT;
    let ceiling = saturation.max(largest_ladder_at);
    if total >= ceiling {
        if fits(total) { LIVE_W } else { 0 }
    } else if (total..=ceiling).all(fits) {
        LIVE_W
    } else {
        0
    }
}

fn fixed_overview_width(
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    live: usize,
    gap: usize,
) -> usize {
    let column_count = 3 + usize::from(seven_day > 0) + usize::from(live > 0);
    // 2 = cursor prefix. Timer slot is in the gap before 5h, not a column.
    // kind→timer gap is 4 chars narrower than standard (min 1).
    let narrow = gap.saturating_sub(4).max(1);
    let standard_gaps = column_count.saturating_sub(2);
    4 + name + kind + five_hour + seven_day + live + standard_gaps * gap + narrow
}

fn overview_header(widths: &OverviewWidths, deepseek: bool) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme::label())];
    spans.push(Span::raw("  ")); // bell slot (blank in header)
    spans.push(Span::styled(fixed("account", widths.name), theme::label()));
    spans.push(gap(widths));
    spans.push(Span::styled(fixed("type", widths.kind), theme::label()));
    spans.push(narrow_gap(widths));
    // Blank TIMER_SLOT keeps the label aligned over the bar.
    spans.push(Span::raw(" ".repeat(TIMER_SLOT)));
    // The column doubles as a balance readout for DeepSeek accounts (no 5h
    // window, just a USD total), so the header names both when one is present.
    let five_label = if deepseek { "5h / balance" } else { LABEL_5H };
    spans.push(Span::styled(
        fixed(five_label, widths.five_hour),
        theme::label(),
    ));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        spans.push(Span::styled(
            fixed(LABEL_7D, widths.seven_day),
            theme::label(),
        ));
    }
    if widths.live > 0 {
        spans.push(gap(widths));
        spans.push(Span::styled(fixed("live", widths.live), theme::label()));
    }
    Line::from(spans)
}

fn render_overview_row(
    app: &App,
    idx: usize,
    widths: &OverviewWidths,
    selected: bool,
    focused: bool,
) -> Line<'static> {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return Line::from("");
    };

    let active = cfg.is_active(&profile.name);
    let disabled = profile.is_disabled();
    // Overview rows only: the refresh countdown carries the profile's
    // fetch-state cue (amber = last-known numbers, red = failed) so staleness
    // reads off the timer instead of the bar brackets.
    let cue = fetch_cue_color(profile);
    let cursor = if selected && focused {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    // A disabled account is filtered out of the scheduler's polling set
    // (`AppConfig::enabled_profiles`), so its `next_refresh_per_profile` entry is
    // frozen at whatever was published before it was disabled: the countdown
    // ticks to zero and then sits there claiming a refresh that will never run.
    // Blank the slot rather than lie, keeping its width so no column shifts. The
    // spinner goes too — nothing can be in flight for a profile nothing polls.
    let timer_span = if disabled {
        Span::raw(" ".repeat(TIMER_SLOT))
    } else {
        let inner = TIMER_SLOT - 1;
        let activity = app
            .activity
            .lock()
            .ok()
            .and_then(|g| g.get(profile.name.as_str()).copied())
            .unwrap_or(ProfileActivity::Idle);
        if !matches!(activity, ProfileActivity::Idle) {
            let frame = spinner_frame(app.tick_count);
            let style = spinner_style(activity);
            Span::styled(format!("{frame:>inner$} ", inner = inner), style)
        } else {
            let secs_str = app
                .next_refresh_per_profile
                .lock()
                .ok()
                .and_then(|m| m.get(profile.name.as_str()).copied())
                .map(|next_ms| {
                    let now = now_ms();
                    let secs = ((next_ms as i64 - now as i64) / 1000).max(0);
                    format!("{secs}s")
                });
            match secs_str {
                Some(s) => Span::styled(
                    format!("{:>inner$} ", s, inner = inner),
                    cue_style(cue, theme::faint()),
                ),
                None => Span::raw(" ".repeat(TIMER_SLOT)),
            }
        }
    };

    // Long-lived-token state from the per-frame-free cache (App::session_tokens).
    // `token_danger` (expired or mis-filled) drives the `⊘` marker.
    let token_status = app.session_tokens.get(profile.name.as_str());
    let token_danger = token_status.is_some_and(|s| s.is_danger(now_ms() as i64));

    let mut spans = vec![cursor];
    // A disabled row flattens every semantic hue to dim — the whole row reads as
    // one inert unit rather than a live row wearing a dim name. The GLYPHS stay:
    // cloudy-tui never lets state ride on hue alone, so `⊖`/`×`/`⊘`/`!`/`●` still
    // distinguish themselves without the color.
    let hue = |s: Style| if disabled { theme::dim() } else { s };
    // Marker precedence: canceled subscription (⊖) > broken login (×) > token
    // danger (⊘) > bell (!) > active (●). Canceled is dead-first (the org 403s
    // every request, matching the Fallback ladder where `Canceled` outranks
    // `AuthBroken`); a dead login makes usage alerts moot until re-login; a dead /
    // mis-filled long-lived token signs sessions out on the next switch, so it
    // outranks a bell.
    if crate::fallback::is_canceled(profile) {
        spans.push(Span::styled("⊖", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if cfg.is_auth_broken(&profile.name) {
        spans.push(Span::styled("×", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if token_danger {
        spans.push(Span::styled("⊘", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if app.bell_fired.contains_key(profile.name.as_str()) {
        spans.push(Span::styled("!", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if active {
        spans.push(Span::styled(
            "●",
            hue(Style::default().fg(theme::accent_2_color())),
        ));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw("  "));
    }
    let (nt, np) = fixed_split(&profile.name, widths.name);
    // A disabled account can never be active (the disable action itself
    // refuses on an active target), so dim always wins over `name_color`.
    let ns = bold_when(
        if disabled {
            theme::dim()
        } else {
            name_color(active)
        },
        selected && focused,
    );
    spans.push(Span::styled(nt, ns));
    spans.push(Span::raw(np));
    spans.push(gap(widths));
    let label = account_type_label(profile);
    // Read before the tag: a no-data dash is not a tier, and the row must not
    // animate one. A lone glyph color-cycling beside the static faint dashes in
    // the same row reads as live data, which is the opposite of what it means.
    let no_tier = label == super::format::NO_DATA;
    // The credentialed identity-wave is ambient MOTION, which reads as "this
    // thing is live". A disabled account is not, so it renders the same flat dim
    // cell an uncredentialed row gets — dimming the pulse's crest would still
    // animate.
    if profile.credentials.is_some() && !disabled && !no_tier {
        let (clamped, pad) = fixed_split(&label, widths.kind);
        let mut pulse = pulse_name_spans(&clamped, theme::dim(), app.anim_ms());
        pulse.push(Span::raw(pad));
        spans.extend(pulse);
    } else {
        // The dash joins the other no-data cells at `faint` only when it is the
        // whole cell. A disabled row still flattens to dim, outranking no-data
        // the way it outranks stale.
        let style = if no_tier && !disabled {
            theme::faint()
        } else {
            theme::dim()
        };
        spans.push(Span::styled(fixed(&label, widths.kind), style));
    }
    spans.push(narrow_gap(widths));
    spans.push(timer_span);
    // Bracketed bars ([███░░░]) for overview account rows only; brackets stay
    // dim — the fetch-state cue lives on the countdown above instead.
    // Usage-page gauges, chain bars, and fallback thresholds stay bracket-less.
    // OAuth windows come from `usage`; api-key/provider profiles have no `usage`,
    // so the 5h/7d windows are synthesized from the matching third-party bars.
    let (five_window, seven_window) = overview_windows(profile);
    // Drain-color each reset countdown by the window's burn rate — see
    // `drain_rate` for where that rate comes from per window.
    let reset_style = |label, window: Option<&UsageWindow>| {
        let window = window?;
        drain_reset_style(
            drain_rate(app, &profile.name, profile, label, window),
            window_rate_unit(label),
            window,
        )
    };
    let reset_fmt = ResetFmt::from_state(&cfg.state);
    // Flatten the bar's threshold colors and drain-colored reset countdown to dim
    // for a disabled row. The NUMBERS stay — they're the last real reading and
    // still informative; it's the semantic hue that lies once the data is frozen.
    // Post-processed here rather than threaded through
    // `window_summary_spans_bracketed`: that helper lives in the shared
    // `format.rs`, so restyling its OUTPUT at this one call site cannot reach a
    // future caller the way a new parameter would. Widths are unaffected (the
    // pad math counts chars, not styles).
    let flatten = |mut spans: Vec<Span<'static>>| {
        if disabled {
            for s in &mut spans {
                s.style = theme::dim();
            }
        }
        spans
    };
    let five_spans = if profile.provider == Some(Provider::DeepSeek) {
        flatten(deepseek_balance_cell(
            profile,
            widths.five_hour,
            widths.deepseek_amount_w,
        ))
    } else {
        flatten(window_summary_spans_bracketed(
            five_window.as_ref(),
            widths.five_hour,
            true,
            reset_style(LABEL_5H, five_window.as_ref()),
            reset_fmt,
            five_window.as_ref().is_some_and(is_past_reset),
        ))
    };
    let five_len: usize = five_spans.iter().map(|s| s.content.chars().count()).sum();
    let five_pad = widths.five_hour.saturating_sub(five_len);
    spans.extend(five_spans);
    spans.push(Span::raw(" ".repeat(five_pad)));
    if widths.seven_day > 0 {
        spans.push(gap(widths));
        let seven_spans = flatten(window_summary_spans_bracketed(
            seven_window.as_ref(),
            widths.seven_day,
            widths.seven_day >= 18,
            reset_style(LABEL_7D, seven_window.as_ref()),
            reset_fmt,
            seven_window.as_ref().is_some_and(is_past_reset),
        ));
        let seven_len: usize = seven_spans.iter().map(|s| s.content.chars().count()).sum();
        let seven_pad = widths.seven_day.saturating_sub(seven_len);
        spans.extend(seven_spans);
        spans.push(Span::raw(" ".repeat(seven_pad)));
    }
    if widths.live > 0 {
        spans.push(gap(widths));
        spans.push(live_cell(
            app.live_sessions.member(&profile.name),
            widths.live,
        ));
    }

    Line::from(spans)
}

/// The row's live-session cell: how many `clauth start` sessions are running as
/// this account, with `⇄` when at least one of them follows the fallback chain.
/// Blank for an account hosting none — cloudy-tui hides a zero count.
///
/// Distinct from the row's leading `●`, which marks the one profile a bare
/// `claude` authenticates as; an account can carry either, both, or neither.
/// That is why the header reads `live` and not `active` — `active` is the `●`
/// sense app-wide. Already the `TEXT_DIM` tier a disabled row flattens to, so it
/// needs no `hue` pass of its own.
fn live_cell(sessions: crate::live_sessions::MemberSessions, width: usize) -> Span<'static> {
    if sessions.sessions == 0 {
        return Span::raw(" ".repeat(width));
    }
    // `⇄` goes at the trailing cell so the count column is shared across rows
    // and the marker lines up at the right edge.
    let marker = if sessions.following > 0 { "⇄" } else { " " };
    let count_w = width.saturating_sub(1);
    Span::styled(
        format!("{}{marker}", fixed(&sessions.sessions.to_string(), count_w)),
        theme::dim(),
    )
}

/// `true` when any profile on the overview is a DeepSeek api-key account.
/// Those report a USD balance instead of a 5h utilization window, so the
/// column header and the per-row cell both swap to carry it.
fn any_deepseek(app: &App) -> bool {
    app.config()
        .profiles
        .iter()
        .any(|p| p.provider == Some(Provider::DeepSeek))
}

/// DeepSeek balance total strings (e.g. `"1.71 USD"`, `"100.00 CNY"`) to show
/// in the overview cell, sorted by numeric amount descending. All funded
/// wallets are included; when none are funded, only the highest one is
/// returned — an account with no funds is still a real account, and a blank
/// cell would read as no-data. Empty when there is no cached snapshot or no
/// balance row. The wallet set is the shared
/// [`crate::providers::funded_wallets`] selection, so this column and the MCP
/// roster's rank drop the same zero-amount wallets.
fn deepseek_balances_to_show(profile: &Profile) -> Vec<String> {
    let Some(stats) = profile.third_party_usage.as_ref() else {
        return Vec::new();
    };
    let mut wallets = crate::providers::funded_wallets(&stats.rows);
    let no_funded = wallets.is_empty();
    if no_funded {
        wallets = crate::providers::balance_wallets(&stats.rows);
    }
    wallets.sort_by(|a, b| b.amount.total_cmp(&a.amount));
    if no_funded {
        wallets.truncate(1);
    }
    wallets.into_iter().map(|w| w.value).collect()
}

/// The 5h-column cell for a DeepSeek profile: its total balance bracketed and
/// dimmed (matching the OAuth bar's `[...]` shape), or [`NO_DATA`] when no
/// cached balance row exists. Multiple balances are comma-joined (e.g.
/// `[1.71 USD, 100.00 CNY]`). `amount_w` is the widest amount string across all
/// DeepSeek rows so currencies line up under each other; the longest amount
/// gets exactly one space before its currency. Width is exact so the column
/// boundary holds.
fn deepseek_balance_cell(profile: &Profile, width: usize, amount_w: usize) -> Vec<Span<'static>> {
    let balances = deepseek_balances_to_show(profile);
    if balances.is_empty() {
        return vec![Span::styled(NO_DATA.to_string(), theme::faint())];
    };
    // Align each "amount CURRENCY": amount left-padded to the widest amount,
    // then one space, then the currency. Multiple balances are comma-joined.
    let inner: String = balances
        .iter()
        .map(|b| match b.rsplit_once(' ') {
            Some((amount, currency)) => format!("{amount:<amount_w$} {currency}"),
            None => b.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    // 2 cells reserved for brackets; truncate inner if it would overflow.
    let inner_w = width.saturating_sub(2);
    let inner: String = inner.chars().take(inner_w).collect();
    let used = inner.chars().count() + 2;
    let pad = width.saturating_sub(used);
    vec![
        Span::styled(format!("[{inner}]"), theme::dim()),
        Span::raw(" ".repeat(pad)),
    ]
}

/// The `(5h, 7d)` windows to show in the overview row. OAuth profiles use their
/// live `UsageInfo`; api-key/provider profiles have no `UsageInfo`, so each slot
/// is synthesized from the third-party bar whose label matches (`5h` / `7d`) —
/// the same labels `zai` decodes from its window codes. `None` per slot when no
/// source exists (renders `—`).
fn overview_windows(profile: &Profile) -> (Option<UsageWindow>, Option<UsageWindow>) {
    if let Some(usage) = profile.usage.as_ref() {
        return (usage.five_hour.clone(), usage.weekly_window().cloned());
    }
    let Some(bars) = profile.third_party_usage.as_ref().map(|s| &s.bars) else {
        return (None, None);
    };
    let window_for = |label: &str| {
        bars.iter().find(|b| b.label == label).map(|b| UsageWindow {
            utilization: b.pct,
            resets_at: b.resets_at.clone(),
        })
    };
    (window_for(LABEL_5H), window_for(LABEL_7D))
}

fn gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap))
}

/// 4 chars less than standard gap; min 1. Used between `type` and timer slot.
fn narrow_gap(widths: &OverviewWidths) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap.saturating_sub(4).max(1)))
}

fn draw_fallback_overview(frame: &mut Frame<'_>, area: Rect, lines: Vec<Line<'static>>) {
    // Read-only detail pane — focus never descends here from the overview screen.
    let block = section_box("fallback chain", false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // No `Wrap`: an over-wide row truncates rather than folding onto a second
    // line, and the empty-state prose is pre-wrapped into its own lines.
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

const GAUGE_W: usize = 12;

fn fallback_flow_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    // Switch-grade kick blocks feed the blocked-reason markers; read BEFORE the
    // Config lock (rank order: KickBlockState 230 < Config 400).
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    let narrow = super::panes::narrow(width as u16);
    let cfg = app.config();
    if cfg.state.fallback_chain.is_empty() {
        let mut lines = vec![Line::from(Span::styled(
            "no fallback chain yet",
            theme::dim(),
        ))];
        lines.extend(
            wrap_words(
                "add accounts on the fallback tab and clauth switches between them \
                 as each runs out.",
                width,
            )
            .into_iter()
            .map(|seg| Line::from(Span::styled(seg, theme::dim()))),
        );
        return lines;
    }

    let chain = &cfg.state.fallback_chain;
    // Narrow: tighter name clamp + a gauge sized from what's left, so a chain
    // row fits a phone line instead of hard-wrapping its trailing figures.
    let name_w = chain
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(8)
        .clamp(6, if narrow { 12 } else { 18 });
    // Threshold digits vary across members (`95%` vs `100%`), so left-pad them to
    // the widest so the `%` signs line up (cloudy-tui numeric-column alignment).
    // It also makes every row's content the same width, which is what lets the
    // trailer column below sit flush against the content.
    let thr_w = chain
        .iter()
        .filter_map(|n| cfg.find(n))
        .map(|p| format!("{:.0}", threshold_for(p)).len())
        .max()
        .unwrap_or(3);
    let last = chain.len() - 1;
    let gauge_w = if narrow {
        // Exact-fit budget against chain_row's spans: ` ╭ `(3) + `N `(digits+1)
        // + name(name_w+2) + figure `  100`(5) + ` / 100%`(7, 3-digit worst).
        // Trailers (hint/marker) only render when the base leaves room, so a
        // narrow line keeps its figures on one row instead of hard-wrapping.
        let idx_w = (last + 1).to_string().chars().count() + 1;
        width
            .saturating_sub(3 + idx_w + name_w + 2 + 5 + 7)
            .clamp(4, GAUGE_W)
    } else {
        GAUGE_W
    };

    // Project the active profile's next switch once, up front: a `To(target)`
    // renders inline on the target member's row (right side); `Off` has no
    // single target row, so it stays a caption below.
    let projection = projected_switch(app, &cfg);
    let switch_to = match &projection {
        Some((SwitchAction::To(target), secs)) => Some((target.clone(), *secs)),
        _ => None,
    };

    // Two passes: build every row's content first so the trailers can share one
    // column just past the widest row, rather than each one flying out to the
    // panel's right edge (which strands the markers far from the data they mark).
    let rows: Vec<ChainRow> = chain
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let reason = cfg
                .find(name)
                .and_then(|p| blocked_reason(&cfg, p, kick_lifts.get(name.as_str()).copied()));
            let switch_eta = switch_to
                .as_ref()
                .filter(|(target, _)| target.as_str() == name.as_str())
                .map(|(_, secs)| *secs);
            chain_row(
                &cfg,
                name,
                ChainRowCtx {
                    index: i,
                    last,
                    name_w,
                    gauge_w,
                    thr_w,
                    reason,
                    switch_eta,
                },
            )
        })
        .collect();
    let trailer_col = rows.iter().map(ChainRow::base_width).max().unwrap_or(0) + TRAILER_GAP;

    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| row.into_line(trailer_col, width))
        .collect();

    // All-spent caption: wrap-off `stop` vs wrap-mode `stay`.
    let caption = if cfg.state.switch_off_when_spent {
        vec![
            Span::raw("  "),
            Span::styled("[ ", theme::dim()),
            Span::styled("stop", theme::danger().bold()),
            Span::styled(" ]", theme::dim()),
            Span::styled(" when all spent", theme::faint()),
        ]
    } else {
        vec![
            Span::raw("  "),
            Span::styled("[ ", theme::dim()),
            Span::styled("stay", theme::dim().bold()),
            Span::styled(" ]", theme::dim()),
            Span::styled(" on the active account when all spent", theme::faint()),
        ]
    };
    lines.push(Line::from(caption));

    // `Off` projection: chain-wide, no target row to sit on — keep it a caption.
    if let Some((SwitchAction::Off, secs)) = &projection {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("signs everything out in ~{}", humanize_duration(*secs)),
                theme::faint(),
            ),
        ]));
    }

    // All-exhausted sibling of the projection: when EVERY chain member is maxed
    // (wrap-off's active-cleared state, or wrap mode's stalled-active
    // equivalent), name whichever one resumes first. Mutually exclusive with the
    // projection — `burn_rate_eta` returns `None` once the active crosses its
    // own threshold, which is a precondition for `soonest_resume` to return.
    if let Some((name, eta)) = soonest_resume(&cfg) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!(
                    "resumes: {name} {}",
                    reset_resume(eta, ResetFmt::from_state(&cfg.state))
                ),
                theme::faint(),
            ),
        ]));
    }
    lines
}

/// The active profile's projected next switch (action + eta secs), or `None`
/// when none is imminent. Guards the projection the same way the old inline line
/// did: only when the active crosses its threshold BEFORE its 5h window resets
/// (past the reset the window refills and no switch fires). Shared by the inline
/// `To` hint (on the target's row) and the `Off` caption.
fn projected_switch(app: &App, cfg: &AppConfig) -> Option<(SwitchAction, i64)> {
    if cfg.state.fallback_chain.len() <= 1 {
        return None;
    }
    let active_name = cfg.state.active_profile.as_ref()?;
    let profile = cfg.find(active_name)?;
    let usage_info = profile.usage.as_ref()?;
    let usage = usage_info.five_hour.as_ref()?;
    let threshold = threshold_for(profile);
    // In-memory rate (`app.history_cache`) — no disk read while `cfg` is held.
    let active_rate = app.active_burn_rate(active_name, usage_info);
    let eta_secs = burn_rate_eta(active_rate, usage.utilization, threshold)?;
    let reset_secs = super::format::reset_in_secs(usage);
    if reset_secs.is_some_and(|reset| eta_secs >= reset) {
        return None;
    }
    next_target(cfg, active_rate).map(|action| (action, eta_secs))
}

/// Cells between the widest chain row's content and the shared trailer column.
/// Matches the row's own internal 2-space gaps, so a trailer reads as part of
/// its row instead of a right-edge rail.
const TRAILER_GAP: usize = 2;

/// A chain row before its trailer lands. Split from the assembled `Line` so the
/// panel can measure every row's content and start every trailer at one column.
/// Only ONE row can carry the `↩ ~eta` hint (the single projected-switch
/// target), so at most that row's marker sits further right than its siblings'.
struct ChainRow {
    base: Vec<Span<'static>>,
    hint: Option<Span<'static>>,
    marker: Option<Span<'static>>,
}

impl ChainRow {
    fn base_width(&self) -> usize {
        self.base.iter().map(|s| s.width()).sum()
    }

    /// Pad the content out to `col`, then append whichever trailers fit inside
    /// `width` (the panel's inner width). A projected-switch target carries the
    /// `↩ ~eta` hint; a blocked member carries its 1-cell reason marker. BOTH can
    /// apply to one row: `next_target`'s headroom walk only prefers a fresh
    /// member and falls through to a stale-but-unexhausted one (`is_exhausted`
    /// ignores `fetch_status`), so a `To` target can also be `Stale`. Render both
    /// then (hint, then marker outermost) instead of dropping the imminent-switch
    /// projection. Too narrow for the pair → keep the marker (the persistent
    /// signal) and drop the hint; too narrow for even that → drop both.
    fn into_line(self, col: usize, width: usize) -> Line<'static> {
        let fits = |w: usize| col + w <= width;
        let trailer: Vec<Span<'static>> = match (self.hint, self.marker) {
            (Some(h), Some(m)) if fits(h.width() + 1 + m.width()) => vec![h, Span::raw(" "), m],
            (Some(_), Some(m)) if fits(m.width()) => vec![m],
            (Some(h), None) if fits(h.width()) => vec![h],
            (None, Some(m)) if fits(m.width()) => vec![m],
            _ => Vec::new(),
        };
        let mut spans = self.base;
        if !trailer.is_empty() {
            let used: usize = spans.iter().map(|s| s.width()).sum();
            spans.push(Span::raw(" ".repeat(col.saturating_sub(used))));
            spans.extend(trailer);
        }
        Line::from(spans)
    }
}

/// The geometry and trailer context one fallback-flow row renders under —
/// position in the chain, the width budget, the blocked reason, and the
/// projected switch eta. Grouped so [`chain_row`] stays under clippy's
/// argument limit without an ad-hoc `#[allow]`.
struct ChainRowCtx {
    index: usize,
    last: usize,
    name_w: usize,
    gauge_w: usize,
    thr_w: usize,
    reason: Option<BlockedReason>,
    switch_eta: Option<i64>,
}

fn chain_row(cfg: &AppConfig, name: &crate::profile::ProfileName, ctx: ChainRowCtx) -> ChainRow {
    let ChainRowCtx {
        index,
        last,
        name_w,
        gauge_w,
        thr_w,
        reason,
        switch_eta,
    } = ctx;
    let active = cfg.is_active(name);
    let rail = if index == 0 && last == 0 {
        "╶"
    } else if index == 0 {
        "╭"
    } else if index == last {
        "╰"
    } else {
        "│"
    };
    // Color carries active state — no glyph needed.
    let name_style = if active {
        Style::default().fg(theme::accent_2_color())
    } else {
        theme::dim()
    };
    let name_pad = name_w.saturating_sub(name.chars().count());

    let mut spans = vec![
        Span::styled(format!(" {rail} "), theme::faint()),
        Span::styled(format!("{} ", index + 1), theme::faint()),
        Span::styled(format!("{name}{}  ", " ".repeat(name_pad)), name_style),
    ];

    match cfg.find(name) {
        None => spans.push(Span::styled("missing", theme::danger())),
        Some(profile) => {
            let threshold = threshold_for(profile);
            let pct = profile
                .usage
                .as_ref()
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization);
            spans.extend(gauge_spans(pct, threshold, gauge_w));
            let (figure, figure_style) = match pct {
                Some(v) => (
                    format!("  {v:>3.0}"),
                    Style::default().fg(theme::util_color(v)),
                ),
                None => ("    —".to_string(), theme::faint()),
            };
            spans.push(Span::styled(figure, figure_style));
            spans.push(Span::styled(
                format!(" / {threshold:>thr_w$.0}%"),
                theme::faint(),
            ));
        }
    }

    ChainRow {
        base: spans,
        hint: switch_eta.map(|secs| {
            // `projected_switch` only ever fires off `burn_rate_eta`, so this
            // hint is always an EXHAUSTION projection — a genuine event-driven
            // return (healthy active, preferred just freed) has no eta to show.
            // The `⌂` glyph therefore marks an exhaustion hop that LANDS on the
            // home account, telling it apart from the plain `↩` of a hop onto any
            // other member; it is keyed on the destination, not on the cause.
            let glyph = if cfg.find(name).is_some_and(|p| p.preferred) {
                "⌂"
            } else {
                "↩"
            };
            Span::styled(
                format!("{glyph} ~{}", humanize_duration(secs)),
                theme::faint(),
            )
        }),
        marker: reason.as_ref().map(reason_marker),
    }
}

/// `gauge_w`-cell bar relative to the member's threshold (full = rotate off).
/// `GAUGE_W` on desktop; narrow rows pass what their line has left.
fn gauge_spans(pct: Option<f64>, threshold: f64, gauge_w: usize) -> Vec<Span<'static>> {
    let fill = pct
        .map(|v| {
            let frac = if threshold > 0.0 {
                (v / threshold).clamp(0.0, 1.0)
            } else {
                1.0
            };
            (frac * gauge_w as f64).round() as usize
        })
        .unwrap_or(0)
        .min(gauge_w);
    let fill_color = pct
        .map(theme::util_color)
        .unwrap_or(theme::text_faint_color());

    (0..gauge_w)
        .map(|i| {
            if i < fill {
                Span::styled("▰", Style::default().fg(fill_color))
            } else {
                Span::styled("▱", theme::faint())
            }
        })
        .collect()
}

/// Seconds until `current` crosses `threshold` at the given 5h-window burn
/// `rate` (%/h, from [`App::active_burn_rate`]). Returns `None` when there's no
/// rate yet, the rate is flat/negative, or utilization is already at/above the
/// threshold.
fn burn_rate_eta(rate: Option<f64>, current: f64, threshold: f64) -> Option<i64> {
    if current >= threshold {
        return None;
    }
    let rate = rate?;
    if rate <= 0.0 {
        return None;
    }
    let hours = (threshold - current) / rate;
    if hours <= 0.0 {
        return None;
    }
    Some((hours * 3600.0) as i64)
}

/// Drain color for an overview reset-countdown suffix (wide layout only).
/// `util_color` of the burn `rate` (slow drain dim, fast warning/danger —
/// mirrors the usage page), escalated to a flat WARNING when the window
/// projects to hit 100% BEFORE it resets ("runs dry first" — you top out ahead
/// of the refill). `rate` is in `rate_unit` (`%/h` or `%/d`) — the window's own
/// unit, the same one the usage page hues by, so both surfaces agree.
/// `None` (caller keeps the faint default) when there's no positive rate yet.
fn drain_reset_style(rate: Option<f64>, rate_unit: &str, window: &UsageWindow) -> Option<Style> {
    let rate = rate.filter(|r| *r > 0.0)?;
    let eta = eta_left_secs(rate, window.utilization, rate_unit);
    let reset = super::format::reset_in_secs(window);
    let runs_dry_first = matches!((eta, reset), (Some(e), Some(r)) if e < r);
    Some(if runs_dry_first {
        theme::warning()
    } else {
        Style::default().fg(theme::util_color(rate.clamp(0.0, 100.0)))
    })
}

/// The rate to drain-color `window`'s countdown by, in the window's native unit
/// (see [`drain_reset_style`]).
///
/// An OAuth 5h window uses the recency-weighted recent burn — in-memory
/// `history_cache`, so no disk read happens under the config guard. Every other
/// window falls back to the window's own average pace, which needs no burn
/// history at all: 7d moves too slowly for the recency weighting to say much,
/// and a synthesized third-party window has no history to weigh.
fn drain_rate(
    app: &App,
    name: &crate::profile::ProfileName,
    profile: &Profile,
    label: &str,
    window: &UsageWindow,
) -> Option<f64> {
    if label == LABEL_5H
        && let Some(usage) = profile.usage.as_ref()
    {
        return app.active_burn_rate(name, usage);
    }
    let per_day = crate::usage::window_avg_pace_per_day(label, window, now_epoch_secs())?;
    Some(if window_rate_unit(label) == "d" {
        per_day
    } else {
        per_day / 24.0
    })
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_overview.rs"]
mod tests;
