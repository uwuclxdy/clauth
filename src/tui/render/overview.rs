//! Overview tab: accounts table + fallback flow, inside one content frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, Paragraph};

use super::super::app::{
    App, BandEntry, CodexRow, MainItemKind, ReadingsSnapshot, ShuntPool, escape_control,
};
use super::super::theme::{self, CrestProvider};
use super::chain::reason_marker;
use super::format::{
    NO_DATA, PCT_LABEL_BUDGET, ResetFmt, account_type_label, bar_fits, cue_style, duration_label,
    fetch_cue_color, fixed, fixed_split, grapheme_cells, is_past_reset, overdue_label,
    past_deadline_age, reading_due_age, reset_in_secs, reset_resume, reset_room,
    reset_suffix_worst, spinner_frame, spinner_style, truncate, window_label, window_note_spans,
    window_summary_spans_bracketed,
};
use super::header::{identity_wave_rest_ms, identity_wave_spans};
use super::panes::{
    bold_when, draw_following_list, empty_state, name_color, section_box, section_box_meta,
    select_line, wrap_words,
};
use super::prose;
use super::usage::{eta_left_secs, window_rate_unit};
use crate::fallback::{BlockedReason, SwitchAction, blocked_reason, soonest_resume, threshold_for};
use crate::format::format_pct;
use crate::profile::{AppConfig, Profile, ProfileName};
use crate::providers::Provider;
use crate::usage::{
    FetchStatus, LABEL_5H, LABEL_7D, LABEL_30D, ProfileActivity, UsageWindow, humanize_duration,
    now_epoch_secs, now_ms, selected_next_refresh, switch_grade_kick_lifts, usage_interval_ms,
};
use std::collections::HashSet;

/// `XXXs` + 1 trailing space = 5 chars; spinner padded to same width.
const TIMER_SLOT: usize = 5;
/// The amber mark an unpolled row that draws a cached reading carries in its
/// countdown slot — "these figures are last-known, not current".
const NOT_CURRENT_MARK: &str = "⋯";
/// Rows the accounts table keeps before the chain panel may claim any space —
/// it is the scrollable, interactive list, so it wins the vertical budget.
const ACCOUNTS_MIN: u16 = 7;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, readings: &ReadingsSnapshot) {
    // The chain panel's inner width is independent of the vertical split (both
    // panels span the full body width), so build its lines first and size the
    // panel to fit them, capped; members that do not fit collapse into
    // `truncate_chain`'s `+N more` line.
    let probe = Rect { height: 3, ..area };
    let chain_inner_w = section_box("", false, false, probe.width)
        .inner(probe)
        .width as usize;
    let (full_lines, member_count) = fallback_flow_lines(app, chain_inner_w, readings);
    let chain_height = chain_panel_height(full_lines.len(), area.height);
    // The panel is capped, so its inner height is the budget the member list
    // must fit: when members are hidden, the last row names how many.
    let inner_height = chain_height.saturating_sub(2) as usize;
    let chain_lines = truncate_chain(full_lines, member_count, inner_height);

    let [accounts_area, chain_area] = Layout::vertical([
        Constraint::Min(ACCOUNTS_MIN),
        Constraint::Length(chain_height),
    ])
    .areas(area);

    draw_overview_accounts(frame, accounts_area, app, readings);
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

/// [`fallback_flow_lines`]'s full output cut to `inner_height` content rows.
/// Members first: when every member fits, draw them all plus the captions that
/// still fit, with no `+N more` line. Only when a member is hidden does the
/// last row become `+N more · → fallback tab` (N = hidden members), captions
/// keeping their rows while the members shrink.
fn truncate_chain(
    full: Vec<Line<'static>>,
    member_count: usize,
    inner_height: usize,
) -> Vec<Line<'static>> {
    // The empty-chain prose is not a member list — never truncate it.
    if member_count == 0 || full.len() <= inner_height {
        return full;
    }
    let caption_count = full.len() - member_count;
    if member_count <= inner_height {
        let captions_kept = (inner_height - member_count).min(caption_count);
        let mut out = Vec::with_capacity(member_count + captions_kept);
        out.extend(full[..member_count].iter().cloned());
        out.extend(
            full[member_count..member_count + captions_kept]
                .iter()
                .cloned(),
        );
        return out;
    }
    let room = inner_height.saturating_sub(1);
    let captions_kept = caption_count.min(room);
    let members_drawn = (room - captions_kept).min(member_count);
    let hidden = member_count - members_drawn;
    let mut out = Vec::with_capacity(members_drawn + captions_kept + 1);
    out.extend(full[..members_drawn].iter().cloned());
    out.extend(
        full[member_count..member_count + captions_kept]
            .iter()
            .cloned(),
    );
    out.push(more_members_line(hidden));
    out
}

/// The `+N more · → fallback tab` line: prose faint, the arrow a running-copy
/// key (`ACCENT + bold`) through the shared tokenizer, like every caption.
fn more_members_line(hidden: usize) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(prose::spans(
        &format!("+{hidden} more · {} fallback tab", prose::key("\u{2192}")),
        theme::faint(),
    ));
    Line::from(spans)
}

/// The accounts panel's title-right meta: one count per harness — every
/// harness whatever the filter shows, since the counts describe the rows the
/// table lists, never the view. A roster with no accounts drops out of the
/// words; with all empty there is nothing to count and no slot renders.
fn harness_counts(app: &App) -> String {
    let mut terms: Vec<String> = Vec::new();
    let claude_n = app.config().profiles.len();
    if claude_n > 0 {
        terms.push(format!("{claude_n} claude"));
    }
    let codex_n = app.codex_rows.len();
    if codex_n > 0 {
        terms.push(format!("{codex_n} codex"));
    }
    let shunt_n = app.shunt_pool.row_count();
    if shunt_n > 0 {
        terms.push(format!("{shunt_n} shunt"));
    }
    terms.join(" · ")
}

fn draw_overview_accounts(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    readings: &ReadingsSnapshot,
) {
    // Sole interactive content panel on this screen — always focused.
    let focused = true;
    let block = section_box_meta(
        "accounts",
        app.harness_filter.label_name(),
        &harness_counts(app),
        focused,
        true,
        area.width,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let codex: &[CodexRow] = if app.harness_filter.shows_codex() {
        &app.codex_rows
    } else {
        &[]
    };
    // The shunt section draws even when no rows can: a failed read still names
    // its reason. `Absent` (no adopted gateway) draws nothing.
    let shunt_draws = app.harness_filter.shows_shunt()
        && match &app.shunt_pool {
            ShuntPool::Read(providers) => !providers.is_empty(),
            ShuntPool::NotRead(_) => true,
            ShuntPool::Absent => false,
        };
    if app.config().profiles.is_empty() && codex.is_empty() && !shunt_draws {
        app.overview_selector.offset.set(0);
        app.overview_selector.viewport.set(0);
        app.overview_selector.starts.replace(Vec::new());
        app.overview_selector.block_ends.replace(Vec::new());
        app.overview_selector.total.set(0);
        empty_state(frame, inner, "no accounts yet", "n", "to create one");
        return;
    }

    let [header_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let widths = if list_area.width > OVERVIEW_MAX_W {
        OverviewWidths::new(OVERVIEW_MAX_W, app, readings).filled(usize::from(list_area.width))
    } else {
        OverviewWidths::new(list_area.width, app, readings)
    };
    let header = overview_header(&widths, any_deepseek(app));
    frame.render_widget(Paragraph::new(header).style(theme::base()), header_area);

    let items = app.main_items();
    let sel = app.overview_cursor.min(items.len().saturating_sub(1));
    let width = list_area.width;
    let (reset_fmt, still) = {
        let cfg = app.config();
        (ResetFmt::from_state(&cfg.state), cfg.state.reduce_motion)
    };
    let entries = app.band_entries(app.harness_filter);
    let ranges = App::heading_ranges(&entries);

    // One pass over the shared band walk builds both the rendered rows and the
    // cursor-indexed starts (lead's call 1: the Overview and Setup selectors
    // both render from this one walk, so a band added later shows on both). A
    // heading or blank spacer can never shift a selection off its row.
    let mut rows: Vec<ListItem<'_>> = Vec::with_capacity(items.len() + 8);
    let mut starts: Vec<usize> = Vec::with_capacity(items.len());
    let mut rendered: usize = 0;
    let mut flat: usize = 0;
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            BandEntry::Spacer => {
                rows.push(ListItem::new(Line::from("")));
                rendered += 1;
            }
            BandEntry::Heading(text) => {
                let heading = if ranges
                    .iter()
                    .any(|&(hi, start, end)| hi == i && (start..end).contains(&sel))
                {
                    theme::label_active()
                } else {
                    theme::label()
                };
                rows.push(ListItem::new(Line::from(prose::spans(
                    &format!("  {text}"),
                    heading,
                ))));
                rendered += 1;
            }
            BandEntry::Item(item) => {
                starts.push(rendered);
                let line = match *item {
                    MainItemKind::Profile(_) => {
                        render_overview_row(app, flat, &widths, flat == sel, focused, readings)
                    }
                    MainItemKind::Codex(idx) => render_codex_row(
                        &codex[idx],
                        flat,
                        &widths,
                        flat == sel,
                        wave_phase_ms(still, app.anim_ms(), flat),
                    ),
                    MainItemKind::Shunt(pi, ai) => {
                        let Some((account, auth)) = app.shunt_pool_row(pi, ai) else {
                            rows.push(ListItem::new(Line::from("")));
                            rendered += 1;
                            flat += 1;
                            continue;
                        };
                        let family = auth.clone().family();
                        render_pool_row(app, family, account, &widths, flat == sel, reset_fmt)
                    }
                };
                rows.push(ListItem::new(select_line(
                    line,
                    flat == sel,
                    focused,
                    width,
                )));
                rendered += 1;
                flat += 1;
            }
        }
    }

    let render_sel = starts.get(sel).copied().unwrap_or(0);
    draw_following_list(frame, list_area, rows, render_sel, &app.overview_selector);
    let block_ends: Vec<usize> = starts.iter().map(|s| s + 1).collect();
    app.overview_selector.starts.replace(starts);
    app.overview_selector.block_ends.replace(block_ends);
}

#[derive(Debug, Clone, Copy)]
struct OverviewWidths {
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    /// `LIVE_W` when the live-session column fits, `0` when it is dropped
    /// (the fill restores it whole once the slack covers its slot).
    live: usize,
    /// `TPS_W` when the per-account speed column fits, `0` when it is dropped
    /// (the fill restores it whole once the slack covers its slot).
    tps: usize,
    /// The `↺ N` chip slot right behind the last usage column, ahead of `live`
    /// and `t/s` (gap + the widest visible chip text across codex and claude
    /// rows), charged whenever a visible row of either kind carries a banked
    /// reset and the row has room for it; `0` when none does or the chip
    /// sheds whole, so the renderer drops the chip instead of clipping it
    /// mid-glyph. One shared slot every row kind and the header pad: a chip
    /// never clips, sheds the same way on both kinds, and the columns after
    /// it stay aligned.
    chip_w: usize,
    /// Name cells the chip's charge took at the laid-out width (the uncharged
    /// name minus the laid-out one). Past [`OVERVIEW_MAX_W`] the slack restores
    /// them one cell at a time, before anything else.
    name_short: usize,
    /// Cells each usage column's reset suffix needs past the room the
    /// laid-out width leaves it: the widest suffix the current display mode
    /// can draw for the column's window, so a countdown ticking shorter
    /// leaves blank cells after its stamp rather than moving the table. Past
    /// [`OVERVIEW_MAX_W`] the slack restores them one cell at a time, the 5h
    /// column first; the suffix draws clipped until its whole width fits.
    five_restore: usize,
    seven_restore: usize,
    /// The `↺ N` chip slot (gap + text) the laid-out row hid for room. The
    /// chip cannot draw partly, so past [`OVERVIEW_MAX_W`] the split waits
    /// until the slack covers the slot whole: nothing grows until then, so no
    /// bar or gap ever shrinks. `0` when the chip was drawn or no visible
    /// row banks a reset.
    chip_deferred: usize,
    /// The live column's slot (gap + [`LIVE_W`]) the laid-out frame cut for
    /// room. The column cannot draw partly, so past [`OVERVIEW_MAX_W`] the
    /// fill restores it whole once the slack covers the slot — before the t/s
    /// column — or not at all. `0` when the frame drew it.
    live_deferred: usize,
    /// The t/s column's slot (gap + [`TPS_W`]) the laid-out frame cut for
    /// room (in clock and both modes the wall-clock stamps take it). Restored
    /// whole past [`OVERVIEW_MAX_W`], after the live column and before the
    /// chip. `0` when the frame drew it, or when the 7d column it needs
    /// is dropped with it.
    tps_deferred: usize,
    /// Whether each usage column draws the bracketed bar shape at its
    /// laid-out width: some drawn claude or shunt cell must draw one (a
    /// dropped column, a codex-only table's pad cells and a DeepSeek balance
    /// cell draw none), and the 5h column must hold the bar block beside the
    /// widest reading it renders. The bars collectively take a third of the
    /// fill slack, shared only between the columns that draw one.
    five_bar: bool,
    seven_bar: bool,
    gap: usize,
    /// Cells each usage column's bar grows by past [`OVERVIEW_MAX_W`]; a
    /// column whose widest reading leaves no room for the bar takes none and
    /// a lone bar takes the whole third. `0` at and below it.
    five_bar_grow: usize,
    seven_bar_grow: usize,
    /// Cells each [`Gap`] widens by past [`OVERVIEW_MAX_W`], indexed by slot;
    /// all `0` at and below it.
    gap_extra: [usize; Gap::COUNT],
    /// Whether [`Self::filled`] ran: a filled row's reset suffix may render
    /// cut at a grapheme boundary while its room restores.
    fill: bool,
    /// Widest amount string (`"1.71"` of `"1.71 USD"`) across DeepSeek rows, so
    /// currencies align in the column. `0` when no DeepSeek balance is cached.
    deepseek_amount_w: usize,
    claude_labels: LabelWidths,
    codex_labels: LabelWidths,
    pool_labels: LabelWidths,
}

/// The widest reading a band renders in each usage column. Every reading is
/// left-padded to its band's widest so the `%` signs align, and a column
/// widens by the widest band's excess over [`PCT_LABEL_BUDGET`].
#[derive(Debug, Clone, Copy, Default)]
struct LabelWidths {
    five: usize,
    seven: usize,
}

impl LabelWidths {
    fn of(labels: impl Iterator<Item = (Option<String>, Option<String>)>) -> Self {
        labels.fold(Self::default(), |acc, (five, seven)| Self {
            five: acc.five.max(five.map_or(0, |l| grapheme_cells(&l))),
            seven: acc.seven.max(seven.map_or(0, |l| grapheme_cells(&l))),
        })
    }

    fn widest(self, other: Self) -> Self {
        Self {
            five: self.five.max(other.five),
            seven: self.seven.max(other.seven),
        }
    }

    /// Cells each column's tiers grow by to hold a reading wider than `100%`.
    fn excess(self) -> (usize, usize) {
        (
            self.five.saturating_sub(PCT_LABEL_BUDGET),
            self.seven.saturating_sub(PCT_LABEL_BUDGET),
        )
    }
}

/// A codex row's reading. Unclamped, unlike the Claude bar's, so a reading past
/// its cap prints as read.
fn codex_label(window: &UsageWindow) -> String {
    format_pct(window.utilization)
}

/// A pool account's window as the bracketed bar renders it: shunt reports a
/// 0-1 fraction for every family and an epoch-seconds reset, the claude row's
/// bar model reads a 0-100 utilization and an ISO reset stamp. The one
/// conversion both the width band, [`render_pool_row`] and the Usage pool pane
/// use.
pub(super) fn pool_window(utilization: Option<f64>, reset: Option<u64>) -> Option<UsageWindow> {
    utilization.map(|fraction| UsageWindow {
        utilization: fraction * 100.0,
        resets_at: reset.map(|secs| crate::usage::epoch_secs_to_iso(secs as i64)),
    })
}

/// Width of the live-session column: its `live` header text, which is also the
/// widest cell any real count reaches (`99⇄`). It never widens for a bigger
/// count, because the column is budgeted on width alone and its cost has to be
/// constant for that to hold.
const LIVE_W: usize = 4;
/// Width of the per-account speed column: the `t/s` header plus one cell,
/// which a four-digit figure (`9999`) fills; past it the figure abbreviates to
/// whole `k`. Like the live column it is budgeted on width alone and drops
/// whole, never clipping.
const TPS_W: usize = 4;

/// Widest the name column ever reaches (the clamp ceiling at wide widths).
const NAME_MAX: usize = 22;
/// Widest the kind column ever reaches.
const KIND_MAX: usize = 16;
/// Widest the 5h column reaches before the wall-clock stamp bonus.
const FIVE_HOUR_MAX: usize = 26;
/// Narrowest the 5h column ever reaches before a wider reading's excess: the
/// bare `100%` alone, the bar (brackets included) shed rather than clipping the
/// figure.
const FIVE_HOUR_MIN: usize = 4;
/// Narrowest an account name gets: once the 5h column is down to its bare
/// reading, or to make room for the `↺ N` chip. The chain panel's name floor.
const NAME_READING_FLOOR: usize = 6;
/// Widest the 7d column reaches before the wall-clock stamp bonus.
const SEVEN_DAY_MAX: usize = 27;
/// Cells beyond its countdown a wall-clock stamp needs. The worst real `both`
/// product is `6d 23h · 12:05am` (a countdown beside the stamp drops the day
/// qualifier), so a column at its widest tier takes this much for the stamp.
const CLOCK_COLS: usize = 10;
/// Narrowest 7d tier that holds its bar; the tier under it is the bare reading.
const SEVEN_DAY_BAR: usize = 18;
/// The 7d tier holding its reset stamp (spare cell shed): the one the t/s
/// column needs beside it.
const SEVEN_DAY_STAMP: usize = 26;
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
/// The width the accounts table lays its columns out at. Past it the table
/// starts from the exact laid-out frame and only grows: the panel's spare
/// width (the slack) restores the room that frame leaves short — a name the
/// `↺ N` chip shrank, each wall-clock stamp's room up to the widest suffix
/// its display mode draws, a dropped live or t/s column (whole) — then
/// widens the 5h and 7d bars that draw and the column gaps until the table
/// spans the panel ([`OverviewWidths::filled`]).
const OVERVIEW_MAX_W: u16 = 122;

/// The gaps of a table row, each named by the column it opens. The `↺ N`
/// chip's gap is not one: the chip slot keeps its laid-out width.
#[derive(Debug, Clone, Copy)]
enum Gap {
    Kind,
    /// The narrow gap before the timer slot, 4 cells under the others.
    Timer,
    SevenDay,
    Live,
    Tps,
}

impl Gap {
    const COUNT: usize = 5;
}

impl OverviewWidths {
    /// The laid-out width: the panel at or below [`OVERVIEW_MAX_W`] lays the
    /// columns out at the panel's own width; past it the table starts from
    /// this exact frame ([`Self::filled`] then only grows it).
    fn new(width: u16, app: &App, readings: &ReadingsSnapshot) -> Self {
        Self::build(width, app, readings)
    }

    /// Spreads the slack between the laid-out row and a `total`-cell panel,
    /// one cell per extra cell, never shrinking anything: first the name the
    /// chip shrank at the laid-out width regains its cells, then each
    /// usage column's wall-clock stamp regains its room (the 5h column first)
    /// up to the widest suffix the display mode can draw for the column's
    /// window — a fixed target, so a shorter stamp leaves blank cells after
    /// it — and the suffix draws cut at a grapheme boundary until it fits.
    /// A column the laid-out frame dropped cannot draw partly: the live and
    /// t/s columns come back whole, once the slack covers each one's slot
    /// (its gap plus the column), live before t/s, and the split waits for
    /// them — a pending slot withholds everything past the name and the
    /// stamps, so no bar or gap ever shrinks. The deferred chip slot comes
    /// back the same way, after t/s, into its place behind the last usage
    /// column. What is left then splits a third to the
    /// 5h and 7d bars that actually draw (a lone bar takes the whole third,
    /// none the whole slack) and the rest over the gaps, the leftmost gaps
    /// taking a cell each where it does not divide evenly. The row then
    /// spans the panel; kind and the clock room already granted keep their
    /// laid-out widths. A row already wider than the panel (a codex reading
    /// the tiers cannot fit) draws as laid out, clipped at the border.
    fn filled(mut self, total: usize) -> Self {
        self.fill = true;
        let mut slack = total.saturating_sub(self.row_width() + self.chip_w);
        let name = self.name_short.min(slack);
        self.name += name;
        slack -= name;
        let five = self.five_restore.min(slack);
        self.five_hour += five;
        slack -= five;
        let seven = self.seven_restore.min(slack);
        self.seven_day += seven;
        slack -= seven;
        if self.live_deferred > 0 && self.live == 0 {
            if self.live_deferred > slack {
                return self;
            }
            self.live = LIVE_W;
            slack -= self.live_deferred;
        }
        if self.tps_deferred > 0 && self.tps == 0 {
            if self.tps_deferred > slack {
                return self;
            }
            self.tps = TPS_W;
            slack -= self.tps_deferred;
        }
        if self.chip_deferred > 0 && self.chip_w == 0 {
            if self.chip_deferred > slack {
                return self;
            }
            self.chip_w = self.chip_deferred;
            slack -= self.chip_deferred;
        }
        // The bars collectively take a third of the slack (a lone bar the
        // whole of it); shared between two bars the 7d takes the odd cell, so
        // the collective share grows at most one cell per slack cell and the
        // gaps never shrink.
        let bar_share = slack / 3;
        self.five_bar_grow = match (self.five_bar, self.seven_bar) {
            (true, true) => bar_share / 2,
            (true, false) => bar_share,
            _ => 0,
        };
        self.seven_bar_grow = match (self.five_bar, self.seven_bar) {
            (true, true) => bar_share.div_ceil(2),
            (false, true) => bar_share,
            _ => 0,
        };
        self.five_hour += self.five_bar_grow;
        self.seven_day += self.seven_bar_grow;
        let gap_cells = slack - self.five_bar_grow - self.seven_bar_grow;
        let present = [
            (Gap::Kind, true),
            (Gap::Timer, true),
            (Gap::SevenDay, self.seven_day > 0),
            (Gap::Live, self.live > 0),
            (Gap::Tps, self.tps > 0),
        ];
        let slots: Vec<Gap> = present
            .into_iter()
            .filter_map(|(slot, shown)| shown.then_some(slot))
            .collect();
        let share = gap_cells / slots.len();
        let rest = gap_cells % slots.len();
        for (i, slot) in slots.into_iter().enumerate() {
            self.gap_extra[slot as usize] = share + usize::from(i < rest);
        }
        self
    }

    /// The cells of the gap that opens `before`'s column.
    fn gap_before(&self, before: Gap) -> usize {
        let base = match before {
            Gap::Timer => self.gap.saturating_sub(4).max(1),
            Gap::Kind | Gap::SevenDay | Gap::Live | Gap::Tps => self.gap,
        };
        base + self.gap_extra[before as usize]
    }

    fn build(width: u16, app: &App, readings: &ReadingsSnapshot) -> Self {
        let total = width as usize;
        let max_name = app
            .config()
            .profiles
            .iter()
            .map(|p| grapheme_cells(&p.name))
            .max()
            .unwrap_or(8);
        let fmt = ResetFmt::from_state(&app.config().state);
        let shows_clock = fmt.shows_clock();
        let claude_labels = if app.harness_filter.shows_claude() {
            LabelWidths::of(app.config().profiles.iter().map(|p| {
                let (five, seven) = overview_windows(p, readings);
                // A DeepSeek row's 5h cell holds its balance, not a reading.
                let five = five.filter(|_| p.provider != Some(Provider::DeepSeek));
                (
                    five.as_ref().map(window_label),
                    seven.as_ref().map(|l| window_label(&l.window)),
                )
            }))
        } else {
            LabelWidths::default()
        };
        let codex_labels = if app.harness_filter.shows_codex() {
            LabelWidths::of(app.codex_rows.iter().map(|r| {
                (
                    r.five_hour.as_ref().map(codex_label),
                    r.seven_day.as_ref().map(codex_label),
                )
            }))
        } else {
            LabelWidths::default()
        };
        let pool_labels = if app.harness_filter.shows_shunt() {
            match &app.shunt_pool {
                ShuntPool::Read(providers) => {
                    LabelWidths::of(providers.iter().flat_map(|p| p.accounts.iter()).map(|a| {
                        (
                            pool_window(a.utilization_5h, a.reset_5h)
                                .as_ref()
                                .map(window_label),
                            pool_window(a.utilization_7d, a.reset_7d)
                                .as_ref()
                                .map(window_label),
                        )
                    }))
                }
                ShuntPool::NotRead(_) | ShuntPool::Absent => LabelWidths::default(),
            }
        } else {
            LabelWidths::default()
        };
        let labels = claude_labels.widest(codex_labels).widest(pool_labels);
        // The widest `↺ N` chip text across visible codex and claude rows, `0`
        // when none carries a banked reset. Only the text: the gap before it
        // joins the column gaps in the width budget below.
        let codex_chip_text = if app.harness_filter.shows_codex() {
            app.codex_rows
                .iter()
                .filter_map(|r| r.resets)
                .map(|count| grapheme_cells(&format!("↺ {count}")))
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let claude_chip_text = if app.harness_filter.shows_claude() {
            app.config()
                .profiles
                .iter()
                .filter_map(|p| app.claude_resets.get(p.name.as_str()))
                .map(|r| r.usable)
                .filter(|count| *count > 0)
                .map(|count| grapheme_cells(&format!("↺ {count}")))
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let chip_text = codex_chip_text.max(claude_chip_text);
        let (five_excess, seven_excess) = labels.excess();
        let (mut name, kind, mut five_hour, mut seven_day) =
            overview_tiers(max_name, total, labels);
        let live = live_column_width(max_name, total, labels);

        // The speed column exists from the first frame; cells are blank until a
        // fresh producer answers. It drops whole before `live` (it needs both
        // columns to fit) and before the 7d stamp. The wall-clock stamps are
        // granted first, so the speed column takes only the slack they leave,
        // and the countdown cells keep their room.
        let mut five_restore = 0;
        let mut seven_restore = 0;
        if shows_clock {
            // A wall-clock stamp's clock half takes [`CLOCK_COLS`] cells
            // beyond the countdown. Take them ONLY from slack the layout would
            // otherwise spend on gap padding, after the live column and the
            // shrink loop have settled, so at the laid-out width turning the
            // setting on can add a stamp but never costs a countdown, a bar or
            // the live column. Widening the tier itself instead pushed the 7d
            // column down to 5 at 130 columns, deleting its bar. A column that
            // gets only part of the room still degrades cleanly in
            // `reset_suffix`.
            let slack = |five: usize, seven: usize| {
                total.saturating_sub(
                    fixed_overview_width_cols(name, kind, five, seven, live, 0, GAP_MIN)
                        + TIMER_SLOT,
                )
            };
            let five_stamp_col = five_hour == FIVE_HOUR_MAX + five_excess;
            if five_stamp_col {
                let five_grant = CLOCK_COLS.min(slack(five_hour, seven_day));
                five_hour += five_grant;
            }
            let seven_stamp_col = seven_day == SEVEN_DAY_MAX + seven_excess;
            if seven_stamp_col {
                let seven_grant = CLOCK_COLS.min(slack(five_hour, seven_day));
                seven_day += seven_grant;
            }
        }
        let facts = column_facts(app, readings);
        // A usage column whose laid-out width cannot hold the bar block beside
        // the widest reading it renders takes neither a share of the fill's
        // third nor a stamp restore the render would not draw; a row with a
        // narrower reading may still draw its bar at the laid-out length.
        let five_bar = facts.five_bar && bar_fits(five_hour, labels.five);
        if shows_clock {
            // Past the layout width the slack restores each stamp's room one
            // cell at a time, up to the widest suffix the display mode can
            // draw for the column's window — a constant per mode, so a
            // countdown ticking shorter never moves the table — and never
            // into a column the layout dropped or one whose widest reading
            // leaves no room for the bar.
            let (five_worst, seven_worst) = reset_suffix_worst(fmt);
            let restore = |worst: usize, width: usize, label_w: usize, draws: bool| {
                if draws {
                    worst.saturating_sub(reset_room(width, label_w, 0))
                } else {
                    0
                }
            };
            five_restore =
                if five_bar {
                    restore(five_worst, five_hour, claude_labels.five, facts.claude_five).max(
                        restore(five_worst, five_hour, pool_labels.five, facts.pool_five),
                    )
                } else {
                    0
                };
            seven_restore = if seven_day > 0 {
                restore(
                    seven_worst,
                    seven_day,
                    claude_labels.seven,
                    facts.claude_seven,
                )
                .max(restore(
                    seven_worst,
                    seven_day,
                    pool_labels.seven,
                    facts.pool_seven,
                ))
            } else {
                0
            };
        }
        let seven_bar = facts.seven_bar && seven_day >= SEVEN_DAY_BAR + seven_excess;
        let tps = if tps_column_width(max_name, total, labels) > 0 {
            let need =
                fixed_overview_width_cols(name, kind, five_hour, seven_day, live, TPS_W, GAP_MIN)
                    + TIMER_SLOT;
            if need <= total { TPS_W } else { 0 }
        } else {
            0
        };

        let base = fixed_overview_width_cols(name, kind, five_hour, seven_day, live, tps, GAP_MIN);
        let column_count =
            3 + usize::from(seven_day > 0) + usize::from(live > 0) + usize::from(tps > 0);
        let gap_slots = column_count.saturating_sub(1).max(1);
        // `fixed_overview_width` omits the TIMER_SLOT the row always renders;
        // widening gaps from that undercounted figure overflows the row at
        // narrow widths and clips the tail of the 5h column. Widen from the
        // real leftover instead.
        let mut gap =
            (GAP_MIN + total.saturating_sub(base + TIMER_SLOT) / gap_slots).clamp(GAP_MIN, 8);
        let mut chip_w = if chip_text > 0 { gap + chip_text } else { 0 };
        let mut chip_deferred = 0;

        // The name column is the one flexible text column (the only column that
        // truncates with `…`; kind/5h/7d/live/t/s are fixed-width). Once the gap
        // is at its ceiling and the clock bonus has taken what it wants, spare
        // width goes to the name instead of stranding at the row end. The chip
        // slot is charged after this, so the name absorbs its own slack exactly
        // as it would without the chip.
        if gap == 8 {
            name += total.saturating_sub(
                fixed_overview_width_cols(name, kind, five_hour, seven_day, live, tps, gap)
                    + TIMER_SLOT,
            );
        }
        // The chip slot is charged against the flexible widths: the gap first,
        // down to its floor, then the name down to `NAME_READING_FLOOR`. Past
        // that the chip sheds whole, nothing charged: the name and gap revert
        // to the uncharged layout, and the renderer drops the chip rather than
        // letting it peek in. A name the chip shrank is recorded, so the fill
        // restores it before anything else grows.
        let mut name_short = 0;
        if chip_text > 0 {
            let name_uncharged = name;
            let gap_uncharged = gap;
            loop {
                let row =
                    fixed_overview_width_cols(name, kind, five_hour, seven_day, live, tps, gap)
                        + TIMER_SLOT
                        + chip_w;
                if row <= total {
                    break;
                }
                if gap > GAP_MIN {
                    gap -= 1;
                    chip_w = gap + chip_text;
                } else if name > NAME_READING_FLOOR {
                    name -= 1;
                } else {
                    name = name_uncharged;
                    gap = gap_uncharged;
                    chip_w = 0;
                    // Past the layout width the fill may still charge the
                    // slot the row could not fit.
                    chip_deferred = gap + chip_text;
                    break;
                }
            }
            if chip_w > 0 {
                name_short = name_uncharged.saturating_sub(name);
            }
        }
        // The live and t/s columns the laid-out frame cut for room come back
        // whole past the layout width: record their slots (gap + column) so
        // the fill can charge them in order, live first. The t/s column only
        // exists beside the 7d reset stamp (the same tier `tps_column_width`
        // requires), so a frame that dropped the 7d column dropped t/s with
        // it and restores neither.
        let live_deferred = if live == 0 { gap + LIVE_W } else { 0 };
        let tps_deferred = if tps == 0 && seven_day >= SEVEN_DAY_STAMP + seven_excess {
            gap + TPS_W
        } else {
            0
        };

        let deepseek_amount_w = app
            .config()
            .profiles
            .iter()
            .filter(|p| p.provider == Some(Provider::DeepSeek))
            .filter_map(|p| readings.stats_of(&p.name))
            .flat_map(|s| s.rows.iter())
            .filter(|r| crate::providers::is_balance_row(&r.label))
            .filter_map(|r| r.value.rsplit_once(' ').map(|(a, _)| grapheme_cells(a)))
            .max()
            .unwrap_or(0);

        Self {
            name,
            kind,
            five_hour,
            seven_day,
            live,
            tps,
            chip_w,
            name_short,
            five_restore,
            seven_restore,
            chip_deferred,
            live_deferred,
            tps_deferred,
            five_bar,
            seven_bar,
            gap,
            five_bar_grow: 0,
            seven_bar_grow: 0,
            gap_extra: [0; Gap::COUNT],
            fill: false,
            deepseek_amount_w,
            claude_labels,
            codex_labels,
            pool_labels,
        }
    }

    /// The width the table's widest row draws at these columns, before the
    /// `↺ N` chip: the cursor and bell prefix, every column and its gap, and
    /// the timer slot. The widest row the layout can draw is this plus
    /// [`Self::chip_w`], the chip slot every row carries behind its last
    /// usage column.
    fn row_width(&self) -> usize {
        fixed_overview_width_cols(
            self.name,
            self.kind,
            self.five_hour,
            self.seven_day,
            self.live,
            self.tps,
            self.gap,
        ) + TIMER_SLOT
            + self.gap_extra.iter().sum::<usize>()
    }

    /// Whether the 7d column holds its bar: below that tier it is the bare
    /// reading. Judged before the fill's bar share, so the grown bar never
    /// flips it; a stamp restore only widens a column that already holds its
    /// bar.
    fn seven_day_bar(&self) -> bool {
        let (_, seven_excess) = self
            .claude_labels
            .widest(self.codex_labels)
            .widest(self.pool_labels)
            .excess();
        self.seven_day.saturating_sub(self.seven_bar_grow) >= SEVEN_DAY_BAR + seven_excess
    }
}

/// What the rows the panel draws carry in each usage column: whether any
/// claude or pool row draws a reset for the column's window (a codex cell
/// prints its reading alone with no reset suffix, and a DeepSeek balance cell
/// draws no bar either) and whether any row draws the column's bar shape —
/// the bracketed bar or its no-data dash on the same rail. A codex-only
/// table's pad cells draw neither, so a column of only those takes no bar
/// share. The reset booleans gate the fill's stamp restore, whose width is
/// the display mode's own worst suffix rather than the rows' current text;
/// the bar booleans gate the fill's bar share.
#[derive(Default)]
struct ColumnFacts {
    claude_five: bool,
    claude_seven: bool,
    pool_five: bool,
    pool_seven: bool,
    five_bar: bool,
    seven_bar: bool,
}

fn column_facts(app: &App, readings: &ReadingsSnapshot) -> ColumnFacts {
    let mut facts = ColumnFacts::default();
    let mut draws = |window: Option<&UsageWindow>, five_side: bool, pool: bool| {
        if window.and_then(reset_in_secs).is_none() {
            return;
        }
        match (five_side, pool) {
            (true, false) => facts.claude_five = true,
            (false, false) => facts.claude_seven = true,
            (true, true) => facts.pool_five = true,
            (false, true) => facts.pool_seven = true,
        }
    };
    if app.harness_filter.shows_claude() {
        for profile in &app.config().profiles {
            let (five_window, seven_window) = overview_windows(profile, readings);
            // A DeepSeek row's 5h cell holds its balance, not a window; every
            // other row draws the column's bar shape even with no reading (the
            // dash rides the same rail).
            if profile.provider != Some(Provider::DeepSeek) {
                facts.five_bar = true;
                draws(five_window.as_ref(), true, false);
            }
            facts.seven_bar = true;
            draws(seven_window.as_ref().map(|long| &long.window), false, false);
        }
    }
    if app.harness_filter.shows_shunt()
        && let ShuntPool::Read(providers) = &app.shunt_pool
    {
        for account in providers.iter().flat_map(|p| p.accounts.iter()) {
            facts.five_bar = true;
            facts.seven_bar = true;
            draws(
                pool_window(account.utilization_5h, account.reset_5h).as_ref(),
                true,
                true,
            );
            draws(
                pool_window(account.utilization_7d, account.reset_7d).as_ref(),
                false,
                true,
            );
        }
    }
    facts
}

/// The four semantic column widths after the tier ladders and the shrink loop
/// have settled at `total`. The wall-clock stamp bonus is applied by the
/// caller after the live column is decided. Every 5h/7d tier is sized for a
/// `100%` reading and grows by the excess of the column's widest `labels`.
fn overview_tiers(
    max_name: usize,
    total: usize,
    labels: LabelWidths,
) -> (usize, usize, usize, usize) {
    let (five_excess, seven_excess) = labels.excess();
    let five_tier = |t: usize| t + five_excess;
    let seven_tier = |t: usize| t + seven_excess;
    let mut name = max_name.clamp(8, if total >= NAME_WIDE_AT { NAME_MAX } else { 16 });
    let mut kind = if total >= KIND_WIDE_AT {
        KIND_MAX
    } else if total >= 66 {
        12
    } else {
        6
    };
    // 26 = [bar]+pct+reset, 17 = [bar]+pct only.
    let mut five_hour = five_tier(if total >= FIVE_HOUR_WIDE_AT {
        FIVE_HOUR_MAX
    } else if total >= 64 {
        17
    } else {
        12
    });
    // One wider than 5h: `humanize_duration` hits 7 chars (`10h 20m`,
    // `23h 59m`) inside the 7d countdown's 10h–23h band, and the 26-cell
    // tier was sized for the 6-char ceiling. The bare-suffix fit check in
    // `reset_suffix` deliberately does not clamp the countdown itself, so
    // the column has to absorb the extra cell or it leaks into `live`.
    let mut seven_day = if total >= SEVEN_DAY_WIDE_AT {
        seven_tier(SEVEN_DAY_MAX)
    } else if total >= 58 {
        seven_tier(5)
    } else {
        0
    };
    let overflow = |name: usize, kind: usize, five_hour: usize, seven_day: usize| {
        (fixed_overview_width(name, kind, five_hour, seven_day, 0, GAP_MIN) + TIMER_SLOT)
            .saturating_sub(total)
    };
    while overflow(name, kind, five_hour, seven_day) > 0 {
        if seven_day > seven_tier(26) {
            // Shed the 7d reset stamp's spare cell first (bar + `%` stay whole),
            // then the stamp itself, then the column — the transient signal
            // gives way before the persistent one, mirroring the 5h ladder.
            seven_day = seven_tier(26);
        } else if seven_day > seven_tier(SEVEN_DAY_BAR) {
            seven_day = seven_tier(SEVEN_DAY_BAR);
        } else if seven_day > 0 {
            seven_day = 0;
        } else if five_hour > five_tier(17) {
            five_hour = five_tier(17);
        } else if five_hour > five_tier(12) {
            five_hour = five_tier(12);
        } else if kind > 6 {
            kind = 6;
        } else if name > 8 {
            name -= 1;
        } else if five_hour > five_tier(FIVE_HOUR_MIN) {
            // Shed the bracketed bar (keep the `%` figure whole) before the
            // figure itself is ever clipped: `window_summary_spans_bracketed`
            // renders the bare reading alone below the bar's floor.
            five_hour = five_tier(FIVE_HOUR_MIN);
        } else {
            break;
        }
    }
    // Still over at the bare rung (a reading wider than `100%` costs cells
    // there too): the name gives them, but only where that makes the row fit
    // with the name at its floor or wider.
    let over = overflow(name, kind, five_hour, seven_day);
    if over > 0 && name.saturating_sub(over) >= NAME_READING_FLOOR {
        name -= over;
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
/// (`kind` 12→16, `seven_day` 5→27, `five_hour` 17→26), so the raw fit
/// predicate can pass at one width, fail at the next and pass again. Once
/// every ladder has saturated the leftover slack grows 1:1 with width, so the
/// predicate is monotone from there on and only the bounded stretch up to that
/// saturation width needs scanning.
fn live_column_width(max_name: usize, total: usize, labels: LabelWidths) -> usize {
    let fits = |w: usize| {
        let (name, kind, five_hour, seven_day) = overview_tiers(max_name, w, labels);
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
    let (five_excess, seven_excess) = labels.excess();
    let saturation = fixed_overview_width(
        NAME_MAX,
        KIND_MAX,
        FIVE_HOUR_MAX + five_excess,
        SEVEN_DAY_MAX + seven_excess,
        0,
        GAP_MIN,
    ) + TIMER_SLOT;
    let ceiling = saturation.max(largest_ladder_at);
    if total >= ceiling {
        if fits(total) { LIVE_W } else { 0 }
    } else if (total..=ceiling).all(fits) {
        LIVE_W
    } else {
        0
    }
}

/// The per-account speed column width at `total`: [`TPS_W`] only when it fits
/// here and at every wider width, `0` otherwise. Decided like the live column
/// (monotone, gated on width alone) but charged both the live column and the
/// full 7d reset stamp, so it drops whole before the live column and before
/// the 7d stamp sheds its spare cell — the first column to go.
fn tps_column_width(max_name: usize, total: usize, labels: LabelWidths) -> usize {
    // The `+ seven_excess` terms below mirror `live_column_width`'s stamp check
    // and saturation bound for symmetry, but no reachable label width can bind
    // them: `26 + excess` only differs from the plain `26` once
    // `5 + seven_excess >= 26` (a reading past ~24 cells), so no test can red a
    // mutant dropping them.
    let (five_excess, seven_excess) = labels.excess();
    let fits = |w: usize| {
        let (name, kind, five_hour, seven_day) = overview_tiers(max_name, w, labels);
        // `SEVEN_DAY_STAMP` + the label excess = the 7d reset stamp present
        // (its spare cell already shed; `SEVEN_DAY_BAR` is bar + `%` alone).
        // The column outlives the stamp unless it is charged the stamp's
        // width.
        seven_day >= SEVEN_DAY_STAMP + seven_excess
            && fixed_overview_width_cols(name, kind, five_hour, seven_day, LIVE_W, TPS_W, GAP_MIN)
                + TIMER_SLOT
                <= w
    };
    let largest_ladder_at = NAME_WIDE_AT
        .max(KIND_WIDE_AT)
        .max(FIVE_HOUR_WIDE_AT)
        .max(SEVEN_DAY_WIDE_AT);
    let saturation = fixed_overview_width_cols(
        NAME_MAX,
        KIND_MAX,
        FIVE_HOUR_MAX + five_excess,
        SEVEN_DAY_MAX + seven_excess,
        LIVE_W,
        TPS_W,
        GAP_MIN,
    ) + TIMER_SLOT;
    let ceiling = saturation.max(largest_ladder_at);
    if total >= ceiling {
        if fits(total) { TPS_W } else { 0 }
    } else if (total..=ceiling).all(fits) {
        TPS_W
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
    fixed_overview_width_cols(name, kind, five_hour, seven_day, live, 0, gap)
}

fn fixed_overview_width_cols(
    name: usize,
    kind: usize,
    five_hour: usize,
    seven_day: usize,
    live: usize,
    tps: usize,
    gap: usize,
) -> usize {
    let column_count =
        3 + usize::from(seven_day > 0) + usize::from(live > 0) + usize::from(tps > 0);
    // 2 = cursor prefix. Timer slot is in the gap before 5h, not a column.
    // kind→timer gap is 4 chars narrower than standard (min 1).
    let narrow = gap.saturating_sub(4).max(1);
    let standard_gaps = column_count.saturating_sub(2);
    4 + name + kind + five_hour + seven_day + live + tps + standard_gaps * gap + narrow
}

fn overview_header(widths: &OverviewWidths, deepseek: bool) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", theme::label())];
    spans.push(Span::raw("  ")); // bell slot (blank in header)
    spans.push(Span::styled(fixed("account", widths.name), theme::label()));
    spans.push(gap(widths, Gap::Kind));
    spans.push(Span::styled(fixed("type", widths.kind), theme::label()));
    spans.push(gap(widths, Gap::Timer));
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
        spans.push(gap(widths, Gap::SevenDay));
        spans.push(Span::styled(
            fixed(LABEL_7D, widths.seven_day),
            theme::label(),
        ));
    }
    push_chip_slot(&mut spans, widths, None);
    if widths.live > 0 {
        spans.push(gap(widths, Gap::Live));
        spans.push(Span::styled(fixed("live", widths.live), theme::label()));
    }
    if widths.tps > 0 {
        spans.push(gap(widths, Gap::Tps));
        spans.push(Span::styled(fixed("t/s", widths.tps), theme::label()));
    }
    Line::from(spans)
}

/// The `↺ N` banked-reset chip slot, right behind the last usage column drawn
/// and ahead of `live` and `t/s`: the same [`OverviewWidths::chip_w`] cells on
/// every row kind and the header, so the columns after it line up whether a
/// row carries a chip or not, and a reset count never sits under `live` or
/// `t/s`. `count` is the row's banked reset count, `None` for a blank slot;
/// nothing while the slot is not charged.
fn push_chip_slot(spans: &mut Vec<Span<'static>>, widths: &OverviewWidths, count: Option<i64>) {
    if widths.chip_w == 0 {
        return;
    }
    let text_w = widths.chip_w.saturating_sub(widths.gap);
    spans.push(Span::raw(" ".repeat(widths.gap)));
    match count {
        Some(count) => {
            let (chip, pad) = fixed_split(&format!("↺ {count}"), text_w);
            spans.push(Span::styled(chip, theme::dim()));
            spans.push(Span::raw(pad));
        }
        None => spans.push(Span::raw(" ".repeat(text_w))),
    }
}

/// The phase a row's identity wave samples: the animation clock, or under
/// `reduce_motion` the row's rest gap, where the wave draws the plain label.
/// Only the wave's phase stills: the row's spinners turn on the tick count.
fn wave_phase_ms(still: bool, anim_ms: u64, position: usize) -> u64 {
    if still {
        identity_wave_rest_ms(position)
    } else {
        anim_ms
    }
}

/// One codex account, in the claude columns: name, plan, 5h, 7d, and a
/// `↺ N` chip while the account holds a banked usage-limit reset (what
/// `clauth limit-reset` spends). The timer slot stays blank and no live cell
/// is drawn: the Overview cannot refresh or switch a Codex account. The
/// marker cell carries `×` for a quarantined chain and the active account's
/// `⏣` otherwise.
fn render_codex_row(
    row: &CodexRow,
    position: usize,
    widths: &OverviewWidths,
    selected: bool,
    wave_ms: u64,
) -> Line<'static> {
    let name_style = if row.active {
        theme::accent().bold()
    } else if selected {
        theme::base().bold()
    } else {
        theme::base()
    };
    // The two-cell cursor prefix keeps Codex markers under the Claude column.
    let mut spans = vec![
        if selected {
            Span::styled(format!("{} ", theme::CARET), theme::accent().bold())
        } else {
            Span::raw("  ")
        },
        if row.broken {
            Span::styled(theme::dead_credential_glyph(), theme::danger())
        } else if row.active {
            Span::styled(
                theme::active_codex_mark(),
                Style::default().fg(theme::accent_2_color()),
            )
        } else {
            Span::raw(" ")
        },
        Span::raw(" "),
        Span::styled(fixed(row.name.as_str(), widths.name), name_style),
        gap(widths, Gap::Kind),
    ];
    // A quarantined chain holds still: motion reads as live, and it is not.
    match row.plan.as_deref() {
        Some(plan) if !row.broken => {
            let (clamped, pad) = fixed_split(plan, widths.kind);
            spans.extend(identity_wave_spans(
                &clamped,
                CrestProvider::Codex,
                position,
                wave_ms,
            ));
            spans.push(Span::raw(pad));
        }
        Some(plan) => spans.push(Span::styled(fixed(plan, widths.kind), theme::dim())),
        None => spans.push(Span::styled(fixed(NO_DATA, widths.kind), theme::faint())),
    }
    // The usage cells take the claude row's lead-in (narrow gap + a blank
    // timer slot) and its left alignment, and the 7d cell drops with its
    // column, so a codex reading sits under `5h`/`7d` and never under `live`.
    let cell = |window: Option<&UsageWindow>, label_w: usize, w: usize| {
        let (label, style) = match window {
            Some(win) => (codex_label(win), theme::base()),
            None => (NO_DATA.to_string(), theme::faint()),
        };
        let pad = " ".repeat(label_w.saturating_sub(grapheme_cells(&label)));
        Span::styled(fixed(&format!("{pad}{label}"), w), style)
    };
    let labels = widths.codex_labels;
    spans.push(gap(widths, Gap::Timer));
    spans.push(Span::raw(" ".repeat(TIMER_SLOT)));
    spans.push(cell(row.five_hour.as_ref(), labels.five, widths.five_hour));
    if widths.seven_day > 0 {
        spans.push(gap(widths, Gap::SevenDay));
        spans.push(cell(row.seven_day.as_ref(), labels.seven, widths.seven_day));
    }
    push_chip_slot(&mut spans, widths, row.resets);
    if widths.live > 0 {
        spans.push(gap(widths, Gap::Live));
        spans.push(Span::raw(" ".repeat(widths.live)));
    }
    if widths.tps > 0 {
        spans.push(gap(widths, Gap::Tps));
        spans.push(Span::raw(" ".repeat(widths.tps)));
    }
    Line::from(spans)
}

/// One pool account in the claude columns: name, plan in the type column, 5h
/// and 7d as the claude rows' bracketed bars with reset countdowns, and a
/// blank live column. Status rides the existing cues only: the broken-login
/// glyph (DANGER) before a name that needs a re-login, a whole row flattened
/// to dim while paused or disabled, a cooldown's remaining time in the timer
/// slot (WARNING); a remove/refresh in flight puts the braille spinner in the
/// marker slot. No status word anywhere.
fn render_pool_row(
    app: &App,
    family: Option<crate::gateway_admin::PoolFamily>,
    account: &crate::gateway_admin::PoolAccount,
    widths: &OverviewWidths,
    selected: bool,
    reset_fmt: ResetFmt,
) -> Line<'static> {
    let disabled = account.paused || account.disabled;
    let running = app
        .pool_mutation_row
        .as_ref()
        .is_some_and(|(running_family, name)| {
            family == Some(*running_family) && account.name == name.as_str()
        });
    let hue = |s: Style| if disabled { theme::dim() } else { s };
    // A selected paused/disabled row still takes the current-row bold, the
    // claude row's `bold_when` promotion.
    let name_style = bold_when(
        if disabled {
            theme::dim()
        } else {
            theme::base()
        },
        selected,
    );
    let marker = if running {
        Span::styled(spinner_frame(app.tick_count), hue(theme::accent()))
    } else if account.needs_relogin {
        Span::styled(theme::dead_credential_glyph(), hue(theme::danger()))
    } else {
        Span::raw(" ")
    };
    let timer = match account.cooldown_secs_remaining {
        Some(secs) => {
            let label = duration_label(secs);
            Span::styled(format!("{label:>4} "), hue(theme::warning()))
        }
        None => Span::raw(" ".repeat(TIMER_SLOT)),
    };
    let mut spans = vec![
        if selected {
            Span::styled(format!("{} ", theme::CARET), theme::accent().bold())
        } else {
            Span::raw("  ")
        },
        marker,
        Span::raw(" "),
        Span::styled(
            fixed(&escape_control(&account.name), widths.name),
            name_style,
        ),
        gap(widths, Gap::Kind),
        match account.plan.as_deref() {
            Some(plan) => {
                Span::styled(fixed(&escape_control(plan), widths.kind), hue(theme::dim()))
            }
            None => Span::styled(fixed(NO_DATA, widths.kind), hue(theme::faint())),
        },
    ];
    spans.push(gap(widths, Gap::Timer));
    spans.push(timer);
    // Flatten the bar's threshold colors and reset countdown to dim for a
    // paused/disabled row, the same pass the claude row applies.
    let flatten = |mut spans: Vec<Span<'static>>| {
        if disabled {
            for s in &mut spans {
                s.style = theme::dim();
            }
        }
        spans
    };
    let five = pool_window(account.utilization_5h, account.reset_5h);
    let five_spans = flatten(window_summary_spans_bracketed(
        five.as_ref(),
        widths.five_hour,
        widths.pool_labels.five,
        true,
        None,
        reset_fmt,
        five.as_ref().is_some_and(is_past_reset),
        0,
        widths.five_bar_grow,
        widths.fill,
    ));
    let five_len: usize = five_spans.iter().map(|s| grapheme_cells(&s.content)).sum();
    spans.extend(five_spans);
    spans.push(Span::raw(
        " ".repeat(widths.five_hour.saturating_sub(five_len)),
    ));
    if widths.seven_day > 0 {
        spans.push(gap(widths, Gap::SevenDay));
        let seven = pool_window(account.utilization_7d, account.reset_7d);
        let seven_spans = flatten(window_summary_spans_bracketed(
            seven.as_ref(),
            widths.seven_day,
            widths.pool_labels.seven,
            widths.seven_day_bar(),
            None,
            reset_fmt,
            seven.as_ref().is_some_and(is_past_reset),
            0,
            widths.seven_bar_grow,
            widths.fill,
        ));
        let seven_len: usize = seven_spans.iter().map(|s| grapheme_cells(&s.content)).sum();
        spans.extend(seven_spans);
        spans.push(Span::raw(
            " ".repeat(widths.seven_day.saturating_sub(seven_len)),
        ));
    }
    push_chip_slot(&mut spans, widths, None);
    if widths.live > 0 {
        spans.push(gap(widths, Gap::Live));
        spans.push(Span::raw(" ".repeat(widths.live)));
    }
    Line::from(spans)
}

/// Whether the profile's PROVIDER is on peak-rate hours right now, sampled off
/// the price table's store rows (the same schedule the Usage tab's `pricing`
/// row uses). Profiles with no store-backed provider (flat-rate, OAuth,
/// generic endpoints, OpenRouter) answer `false` — the marker column stays as
/// it was.
fn peak_live(app: &App, profile: &Profile) -> bool {
    app.peak_state_for(profile).is_some_and(|s| s.peak)
}

/// The active account's mark: `⥋` while the client-token store holds a token
/// for the profile, else the claude working line — animating while a fresh tps
/// snapshot shows the account spending tokens, resting otherwise. The names
/// come off the [`App::shunt_mark`] cache, which the tick keeps in step with
/// the store file itself, so no render touches the store.
fn active_mark(app: &App, profile: &Profile) -> &'static str {
    if app
        .shunt_mark
        .names
        .iter()
        .any(|held| held == profile.name.as_str())
    {
        return theme::active_shunt_mark();
    }
    let burning = app
        .fresh_tps()
        .and_then(|snap| snap.accounts.get(profile.name.as_str()))
        .is_some_and(|figures| figures.live.rate > 0.0);
    if burning {
        theme::active_claude_frame(app.tps_now_ms(), theme::ghostty_marks())
    } else {
        theme::active_claude_rest()
    }
}

/// The provider an account's type cell crests for, read off its endpoint or
/// stored login, never its name: a typed endpoint names its provider, a stored
/// login with no endpoint is Anthropic's, anything else has no crest.
fn crest_provider(profile: &Profile) -> Option<CrestProvider> {
    match profile.provider {
        Some(Provider::DeepSeek) => Some(CrestProvider::DeepSeek),
        Some(Provider::Zai) => Some(CrestProvider::Zai),
        Some(Provider::Alibaba) => Some(CrestProvider::Alibaba),
        Some(Provider::OpenRouter) => Some(CrestProvider::OpenRouter),
        Some(Provider::MiniMax) => Some(CrestProvider::MiniMax),
        None => (profile.is_oauth() && profile.credentials.is_some())
            .then_some(CrestProvider::Anthropic),
    }
}

/// The 5h cell of a plan with no 5h limit: the Usage tab's `no limit on this
/// plan`, cut to fit the column.
const NO_FIVE_HOUR_LIMIT: &str = "no limit";

fn render_overview_row(
    app: &App,
    idx: usize,
    widths: &OverviewWidths,
    selected: bool,
    focused: bool,
    readings: &ReadingsSnapshot,
) -> Line<'static> {
    // Read before the config guard, which it ranks below.
    let dead_logins = app
        .third_party_broken
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default();
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return Line::from("");
    };

    let active = cfg.is_active(&profile.name);
    let disabled = profile.is_disabled();
    // Overview rows only: the refresh countdown carries the profile's
    // fetch-state cue (amber = last-known numbers, red = failed) so staleness
    // reads off the timer instead of the bar brackets.
    let cue = fetch_cue_color(profile, readings.status_of(&profile.name));
    let cursor = if selected && focused {
        Span::styled(format!("{} ", theme::CARET), theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    // The not-current mark needs the row's windows up front (and DeepSeek reads
    // its balance instead), so decide "draws a reading" here rather than after
    // the bars are built.
    let (five_window, seven_window) = overview_windows(profile, readings);
    let not_current = !crate::usage::any_fetch_leg_polls(profile)
        && draws_a_reading(
            profile,
            readings,
            five_window.as_ref(),
            seven_window.as_ref(),
            widths,
        );
    // A setup token the usage probe found refused rests: no probe follows, so
    // its slot counts nothing down, though the tick that lands the refusal
    // publishes a countdown before the verdict reaches this map.
    let token_refused = crate::usage::dead_login_is_setup_token(profile)
        && crate::usage::dead_login_fingerprint(profile)
            .is_some_and(|fp| dead_logins.get(profile.name.as_str()) == Some(&fp));
    let inner = TIMER_SLOT - 1;
    let timer_span = if not_current {
        // An unpolled row drawing a cached reading carries the amber `⋯` where a
        // polled row would count down to its next refresh: the figures beside it
        // are last-known, and no fetch leg currently refreshes them (a re-enable,
        // a login or a key puts the profile back on a list). Styled directly
        // rather than through `hue`, so a disabled row keeps this one hue while
        // every other span still flattens to dim.
        Span::styled(
            format!("{:>inner$} ", NOT_CURRENT_MARK, inner = inner),
            theme::warning(),
        )
    } else if disabled {
        // A disabled account is filtered out of the scheduler's polling set
        // (`AppConfig::enabled_profiles`), and `publish_countdowns` republishes
        // the `next_refresh_per_profile` map from the work-list snapshots on
        // every tick, so its entry is dropped rather than left to tick to zero.
        // With no reading to mark stale, blank the slot rather than claim a
        // countdown, keeping its width so no column shifts. The spinner goes too —
        // nothing can be in flight for a profile nothing polls.
        Span::raw(" ".repeat(TIMER_SLOT))
    } else if token_refused {
        Span::raw(" ".repeat(TIMER_SLOT))
    } else if !app.fetch_lease.fetches_here()
        && readings.status_of(&profile.name) == Some(FetchStatus::AuthExpired)
    {
        // A dead credential the holder suppresses blanks its countdown there;
        // this instance's work list still holds the entry and derives a stamp
        // of its own, which must not draw one either — the suppressed account
        // has no next attempt to count down to, so the slot stays blank on a
        // stood-down mirror alike.
        Span::raw(" ".repeat(TIMER_SLOT))
    } else {
        let (activity, blanked) = app
            .activity
            .lock()
            .ok()
            .map(|states| {
                let key = crate::usage::FetchLeg::for_profile(profile).key(profile.name.clone());
                (
                    crate::usage::selected_activity(&states, profile),
                    crate::usage::countdown_blanked(&states, &key),
                )
            })
            .unwrap_or((ProfileActivity::Idle, false));
        let next_refresh = app
            .next_refresh_per_profile
            .lock()
            .ok()
            .and_then(|m| selected_next_refresh(&m, profile));
        let activity =
            crate::usage::display_activity(activity, next_refresh, app.fetch_lease.fetches_here());
        if !matches!(activity, ProfileActivity::Idle) {
            let frame = spinner_frame(app.tick_count);
            let style = spinner_style(activity);
            Span::styled(format!("{frame:>inner$} ", inner = inner), style)
        } else {
            // The reading clock the no-deadline arm derives its due time from:
            // the reading's own stamp (OAuth), else the per-tick cache clock
            // (third-party's mtime, an undatable OAuth body's mtime).
            let reading_clock = readings.clock_of(&profile.name);
            let overdue_age = match next_refresh {
                // A published deadline past on this instance: on a fetching
                // one the spinner above already took it, so only a stood-down
                // mirror reaches here, and its stamp is a cache estimate —
                // nothing is coming, so the slot reads how far past due the
                // reading is instead of a countdown frozen at `0s`.
                Some(next) => past_deadline_age(app.fetch_lease.fetches_here(), next),
                // No deadline published and the scheduler published no
                // deliberate blank: a stuck worker holds the leg, so the
                // reading dates its own refresh and the slot reads the age
                // past `clock + interval`. A deliberately blanked leg (a
                // spent-skip, a suppressed credential, a selected leg on no
                // work list) keeps its blank slot.
                None if !blanked => reading_due_age(
                    reading_clock,
                    usage_interval_ms(profile, cfg.state.refresh_interval_ms),
                ),
                None => None,
            };
            if let Some(age) = overdue_age {
                // Red while the account's last fetch failed or its credential
                // is dead, amber otherwise. Read the status directly: the
                // bars' fetch cue gates on the OAuth shape, which would amber
                // a failed endpoint account's overdue age.
                let style = if matches!(
                    readings.status_of(&profile.name),
                    Some(FetchStatus::Failed | FetchStatus::AuthExpired)
                ) {
                    theme::danger()
                } else {
                    theme::warning()
                };
                Span::styled(
                    format!("{:>inner$} ", overdue_label(age), inner = inner),
                    style,
                )
            } else {
                let secs_str = next_refresh.map(|next_ms| {
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
        }
    };

    // Long-lived-token state from the per-frame-free cache (App::session_tokens).
    // `token_danger` (expired or mis-filled, or refused by the usage probe, the
    // same credential and the same re-mint) drives the `⊘` marker.
    let token_status = app.session_tokens.get(profile.name.as_str());
    let token_danger = token_refused || token_status.is_some_and(|s| s.is_danger(now_ms() as i64));

    let mut spans = vec![cursor];
    // A disabled row flattens every semantic hue to dim — the whole row reads as
    // one inert unit rather than a live row wearing a dim name. The GLYPHS stay:
    // this row never lets state ride on hue alone, so `⊖`/`×`/`⊘`/`!`/the active
    // mark/`▲` still distinguish themselves without the color.
    let hue = |s: Style| if disabled { theme::dim() } else { s };
    // Marker precedence: a running delete (its spinner) > canceled subscription
    // or organization refusal (⊖) > broken login (×) > token danger (⊘) > bell
    // (!) > active (✻ / its working frame / ⏣ / ⥋) > peak hours (▲). A refused
    // service is dead-first (the org refuses every request, matching the
    // Fallback ladder where `OrgRefused` and `Canceled` outrank `AuthBroken`);
    // a dead login makes usage alerts moot until re-login; a dead / mis-filled
    // long-lived token signs sessions out on the next switch, so it outranks a
    // bell. The active mark
    // outranks the peak marker — naming which account a bare `claude`
    // authenticates as beats a schedule the Usage tab's pricing row already names.
    let auth_broken = cfg.is_auth_broken(&profile.name);
    if app.deleting.as_ref() == Some(&profile.name) {
        spans.push(Span::styled(
            spinner_frame(app.tick_count),
            hue(theme::accent()),
        ));
        spans.push(Span::raw(" "));
    } else if crate::fallback::refuses_service_from_info(readings.usage_of(&profile.name)) {
        spans.push(Span::styled("⊖", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if auth_broken {
        spans.push(Span::styled(
            theme::dead_credential_glyph(),
            hue(theme::danger()),
        ));
        spans.push(Span::raw(" "));
    } else if token_danger {
        spans.push(Span::styled("⊘", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if app.bell_fired.is_ringing(profile.name.as_str()) {
        spans.push(Span::styled("!", hue(theme::danger())));
        spans.push(Span::raw(" "));
    } else if active {
        spans.push(Span::styled(
            active_mark(app, profile),
            hue(Style::default().fg(theme::accent_2_color())),
        ));
        spans.push(Span::raw(" "));
    } else if peak_live(app, profile) {
        // Peak-rate marker, non-active rows only: the active mark outranks it,
        // so an active profile on peak hours keeps its mark and the `▲` reads
        // "this other account is on the surcharged rate right now". The
        // pricing row on the Usage tab names the schedule and the flip.
        spans.push(Span::styled("▲", hue(theme::warning())));
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
    spans.push(gap(widths, Gap::Kind));
    let label = account_type_label(profile, readings.usage_of(&profile.name));
    // Read before the tag: a no-data dash is not a tier, and the row must not
    // animate one. A lone glyph color-cycling beside the static faint dashes in
    // the same row reads as live data, which is the opposite of what it means.
    let no_tier = label == super::format::NO_DATA;
    // The identity wave is ambient MOTION, which reads as "this thing is live".
    // A disabled or quarantined account is not, so it renders the same flat dim
    // cell an unrecognized provider gets — dimming the crest would still animate.
    if let Some(provider) = crest_provider(profile)
        && !disabled
        && !auth_broken
        && !no_tier
    {
        let (clamped, pad) = fixed_split(&label, widths.kind);
        let wave_ms = wave_phase_ms(cfg.state.reduce_motion, app.anim_ms(), idx);
        spans.extend(identity_wave_spans(&clamped, provider, idx, wave_ms));
        spans.push(Span::raw(pad));
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
    spans.push(gap(widths, Gap::Timer));
    spans.push(timer_span);
    // Bracketed bars ([███░░░]) for overview account rows only; brackets stay
    // dim — the fetch-state cue lives on the countdown above instead.
    // Usage-page gauges, chain bars, and fallback thresholds stay bracket-less.
    // OAuth windows come from `usage`; an api-key/provider profile carries
    // `usage` only when it was seeded from its provider windows, and where it
    // is absent the 5h/7d windows are synthesized from the matching bars.
    // Drain-color each reset countdown by the window's burn rate — see
    // `drain_rate` for where that rate comes from per window.
    let reset_style = |label, window: Option<&UsageWindow>| {
        let window = window?;
        drain_reset_style(
            drain_rate(app, &profile.name, profile, label, window, readings),
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
    // pad math measures display cells, not styles).
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
            widths.five_hour,
            widths.deepseek_amount_w,
            readings.stats_of(&profile.name),
        ))
    } else if five_window.is_none()
        && readings
            .usage_of(&profile.name)
            .is_some_and(|usage| usage.no_five_hour_limit)
    {
        // A plan with no 5h limit names that shape, as the Usage tab does,
        // rather than the no-data dash an unread account gets.
        flatten(window_note_spans(
            NO_FIVE_HOUR_LIMIT,
            widths.five_hour,
            widths.claude_labels.five,
            true,
        ))
    } else {
        flatten(window_summary_spans_bracketed(
            five_window.as_ref(),
            widths.five_hour,
            widths.claude_labels.five,
            true,
            reset_style(LABEL_5H, five_window.as_ref()),
            reset_fmt,
            five_window.as_ref().is_some_and(is_past_reset),
            0,
            widths.five_bar_grow,
            widths.fill,
        ))
    };
    let five_len: usize = five_spans.iter().map(|s| grapheme_cells(&s.content)).sum();
    let five_pad = widths.five_hour.saturating_sub(five_len);
    spans.extend(five_spans);
    spans.push(Span::raw(" ".repeat(five_pad)));
    if widths.seven_day > 0 {
        spans.push(gap(widths, Gap::SevenDay));
        let seven_monthly = seven_window.as_ref().is_some_and(|l| l.monthly);
        let seven_window = seven_window.as_ref().map(|l| &l.window);
        // The label the drain/pace reads: the monthly window paces per-day over
        // 30d, not 7d.
        let seven_label = if seven_monthly { LABEL_30D } else { LABEL_7D };
        let seven_bar = widths.seven_day_bar();
        // The monthly prefix takes its 4 cells from the row's own bar, so the
        // column never widens for it and the `%` stays on the shared rail. At
        // the bare-reading tier (≤100 cols) there are no bar cells to borrow
        // the prefix from, so the monthly figure renders bare under the `7d`
        // header — a recorded deviation from the labelled-cell display ruling:
        // the owner chose the bare figure over widening the column.
        let mut seven_cell = window_summary_spans_bracketed(
            seven_window,
            widths.seven_day,
            widths.claude_labels.seven,
            seven_bar,
            reset_style(seven_label, seven_window),
            reset_fmt,
            seven_window.is_some_and(is_past_reset),
            if seven_monthly && seven_bar { 4 } else { 0 },
            widths.seven_bar_grow,
            widths.fill,
        );
        if seven_monthly && seven_bar {
            // The header stays `7d`, so the cell names the window itself.
            // Inserted BEFORE the disabled-row flatten so a disabled monthly
            // row dims it with the rest of the cell.
            seven_cell.insert(0, Span::styled(format!("{LABEL_30D} "), theme::faint()));
        }
        let seven_spans = flatten(seven_cell);
        let seven_len: usize = seven_spans.iter().map(|s| grapheme_cells(&s.content)).sum();
        let seven_pad = widths.seven_day.saturating_sub(seven_len);
        spans.extend(seven_spans);
        spans.push(Span::raw(" ".repeat(seven_pad)));
    }
    let resets = app
        .claude_resets
        .get(profile.name.as_str())
        .map(|r| i64::from(r.usable))
        .filter(|n| *n > 0);
    push_chip_slot(&mut spans, widths, resets);
    if widths.live > 0 {
        spans.push(gap(widths, Gap::Live));
        spans.push(live_cell(
            app.live_sessions.member(&profile.name),
            widths.live,
        ));
    }
    if widths.tps > 0 {
        spans.push(gap(widths, Gap::Tps));
        spans.push(tps_cell(app, &profile.name, widths.tps));
    }

    Line::from(spans)
}

/// The row's speed cell: the account's live speed off a fresh producer, blank
/// when absent (never a dash standing in for a figure clauth did not read).
/// Right-aligned to the column, and a figure ≥ 10 000 abbreviates to whole `k`
/// (`12k`) so the column never clips a number into `…`. Already the `TEXT_DIM`
/// tier a disabled row flattens to, so it needs no `hue` pass of its own.
fn tps_cell(app: &App, name: &str, width: usize) -> Span<'static> {
    let speed = app
        .fresh_tps()
        .and_then(|s| s.accounts.get(name))
        .and_then(|f| f.live.speed);
    match speed {
        Some(v) => {
            let text = crate::tps::format_figure_capped(v, width);
            Span::styled(format!("{text:>width$}"), theme::dim())
        }
        None => Span::raw(" ".repeat(width)),
    }
}

/// The row's live-session cell: how many `clauth start` sessions are running as
/// this account, with `⇄` when at least one of them follows the fallback chain.
/// Blank for an account hosting none: a zero count is hidden.
///
/// Distinct from the row's leading active mark (see [`active_mark`]), which
/// names the one profile a bare `claude` authenticates as; an account can
/// carry either, both, or neither.
/// That is why the header reads `live` and not `active` — `active` is the
/// active mark's sense app-wide. Already the `TEXT_DIM` tier a disabled row
/// flattens to, so it needs no `hue` pass of its own.
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
fn deepseek_balances_to_show(stats: &crate::providers::ThirdPartyStats) -> Vec<String> {
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
fn deepseek_balance_cell(
    width: usize,
    amount_w: usize,
    stats: Option<&crate::providers::ThirdPartyStats>,
) -> Vec<Span<'static>> {
    let Some(stats) = stats else {
        return vec![Span::styled(NO_DATA.to_string(), theme::faint())];
    };
    let balances = deepseek_balances_to_show(stats);
    if balances.is_empty() {
        return vec![Span::styled(NO_DATA.to_string(), theme::faint())];
    };
    // Align each "amount CURRENCY": amount left-padded to the widest amount,
    // then one space, then the currency. Multiple balances are comma-joined.
    let inner: String = balances
        .iter()
        .map(|b| match b.rsplit_once(' ') {
            Some((amount, currency)) => format!(
                "{}{amount} {currency}",
                " ".repeat(amount_w.saturating_sub(grapheme_cells(amount)))
            ),
            None => b.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let inner_cells = grapheme_cells(&inner);
    // A fabricated figure is never acceptable (spec: never show a usage figure
    // clauth did not read). The whole balance stays bracketed while it fits;
    // the brackets shed before the figure is cut; only then does it end in `…`,
    // never a closed bracket around a cut figure.
    if inner_cells + 2 <= width {
        let pad = width.saturating_sub(inner_cells + 2);
        return vec![
            Span::styled(format!("[{inner}]"), theme::dim()),
            Span::raw(" ".repeat(pad)),
        ];
    }
    if inner_cells <= width {
        let pad = width.saturating_sub(inner_cells);
        return vec![
            Span::styled(inner, theme::dim()),
            Span::raw(" ".repeat(pad)),
        ];
    }
    vec![Span::styled(truncate(&inner, width), theme::dim())]
}

/// The overview's long-window slot: the window, plus whether it is the monthly
/// one (so the cell prefixes `30d ` — the header stays `7d` — and the reset
/// drain reads the monthly pace).
struct OverviewLong {
    window: UsageWindow,
    monthly: bool,
}

/// The `(5h, long)` windows to show in the overview row. OAuth profiles use
/// their live `UsageInfo`; an api-key/provider profile carries one only when it
/// was seeded from its provider windows, so each missing slot is synthesized
/// from the third-party bar whose label matches (`5h` / `7d`) — the same
/// labels `zai` decodes from its window codes. The second slot is the account's
/// LONG window: the 7d window when present, else the provider-marked monthly
/// quota (an OAuth reading has no monthly, so its 7d-else-first-scoped
/// fallback stays byte-identical). `None` per slot when no source exists
/// (renders `—`).
fn overview_windows(
    profile: &Profile,
    readings: &ReadingsSnapshot,
) -> (Option<UsageWindow>, Option<OverviewLong>) {
    if let Some(usage) = readings.usage_of(&profile.name) {
        let long = if profile.usage_cache_is_third_party() {
            usage.long_window().map(|(label, w)| OverviewLong {
                window: w.clone(),
                monthly: label == LABEL_30D,
            })
        } else {
            usage.weekly_window().map(|w| OverviewLong {
                window: w.clone(),
                monthly: false,
            })
        };
        return (usage.five_hour.clone(), long);
    }
    let Some(bars) = readings.stats_of(&profile.name).map(|s| &s.bars) else {
        return (None, None);
    };
    let window_for = |label: &str| {
        bars.iter().find(|b| b.label == label).map(|b| UsageWindow {
            utilization: b.pct,
            resets_at: b.resets_at.clone(),
        })
    };
    (
        window_for(LABEL_5H),
        window_for(LABEL_7D).map(|window| OverviewLong {
            window,
            monthly: false,
        }),
    )
}

/// Whether this row draws a cached reading: a 5h window, a 7d window in a row
/// whose 7d column is present (`widths.seven_day > 0`), or a non-empty DeepSeek
/// balance. A row drawing only dashes has no figure to mark stale, so it gets no
/// not-current mark. Decided from what the row draws, not from the snapshot
/// alone: a third-party row can carry stats with no bars, a 7d-only window
/// draws nothing where the 7d column was dropped, and a DeepSeek row draws its
/// balance where the window columns would sit.
fn draws_a_reading(
    profile: &Profile,
    readings: &ReadingsSnapshot,
    five_window: Option<&UsageWindow>,
    seven_window: Option<&OverviewLong>,
    widths: &OverviewWidths,
) -> bool {
    if profile.provider == Some(Provider::DeepSeek) {
        readings
            .stats_of(&profile.name)
            .is_some_and(|s| !deepseek_balances_to_show(s).is_empty())
    } else {
        five_window.is_some() || (widths.seven_day > 0 && seven_window.is_some())
    }
}

fn gap(widths: &OverviewWidths, before: Gap) -> Span<'static> {
    Span::raw(" ".repeat(widths.gap_before(before)))
}

fn draw_fallback_overview(frame: &mut Frame<'_>, area: Rect, lines: Vec<Line<'static>>) {
    // Read-only detail pane — focus never descends here from the overview screen.
    let block = section_box("fallback chain", false, false, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // No `Wrap`: an over-wide row truncates rather than folding onto a second
    // line, and the empty-state prose is pre-wrapped into its own lines.
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

const GAUGE_W: usize = 12;

/// The cells a wrapped line may fill inside a pane: the exact budget whenever
/// there is one, so no row wraps past the pane edge (the detail `Paragraph`
/// has no `Wrap`, so an over-wide row is cut with no mark). A zero budget —
/// the chip and its lead alone overrunning the pane, or no pane at all — takes
/// the sibling hint wrappers' 8-cell floor (panes.rs rail and tooltip legs,
/// status.rs, services.rs): nothing there can render, so the floor only bounds
/// the row count `chain_panel_height` reads.
fn wrap_budget(budget: usize) -> usize {
    if budget == 0 { 8 } else { budget }
}

fn fallback_flow_lines(
    app: &App,
    width: usize,
    readings: &ReadingsSnapshot,
) -> (Vec<Line<'static>>, usize) {
    // Switch-grade kick blocks feed the blocked-reason markers; read BEFORE the
    // Config lock (rank order: KickBlockState 230 < Config 400).
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    // The live key-rejected set, read once per frame before the Config lock
    // (both stores rank below it) — never a durable read under the guard.
    let key_rejected = app.key_rejected_names();
    let narrow = super::panes::narrow(width as u16);
    // The projected switch, computed BEFORE the Config lock: the caption's
    // decision reads the stores (which rank below `Config`), so it must not run
    // under the guard below. `projected_switch` reads the config itself.
    let projection = projected_switch(app, &key_rejected);
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
                wrap_budget(width),
            )
            .into_iter()
            .map(|seg| Line::from(Span::styled(seg, theme::dim()))),
        );
        return (lines, 0);
    }

    let chain = &cfg.state.fallback_chain;
    // Narrow: tighter name clamp + a gauge sized from what's left, so a chain
    // row fits a phone line instead of hard-wrapping its trailing figures.
    let last = chain.len() - 1;
    let idx_w = grapheme_cells(&(last + 1).to_string()) + 1;
    // Threshold digits vary across members (`95%` vs `100%`), so left-pad them to
    // the widest so the `%` signs line up.
    // It also makes every row's content the same width, which is what lets the
    // trailer column below sit flush against the content.
    let thr_w = chain
        .iter()
        .filter_map(|n| cfg.find(n))
        .map(|p| grapheme_cells(&format!("{:.0}", threshold_for(p))))
        .max()
        .unwrap_or(3);
    // Readings left-pad to the widest the chain renders, the no-data dash
    // included, so their `%` signs stack the way the thresholds' do.
    let reading_w = chain
        .iter()
        .map(|n| {
            let pct = readings
                .usage_of(n.as_str())
                .and_then(|u| u.five_hour.as_ref())
                .map(|w| w.utilization);
            chain_reading(pct).map_or(grapheme_cells(NO_DATA), |l| grapheme_cells(&l))
        })
        .max()
        .unwrap_or(0);
    let figure_w = 2 + reading_w + 3 + thr_w + 1;
    // The name reaches its full width where the row has the room — a long
    // email-style member name must not gain a `…` with free cells left. It
    // clamps only to what the row leaves after the rail (` ╭ `), the index,
    // the gauge and the inline figure; narrow reserves the gauge floor (4) so
    // the clamp bites only once the gauge + figure need the room.
    let widest_name = chain
        .iter()
        .map(|n| grapheme_cells(n.as_str()))
        .max()
        .unwrap_or(8);
    let name_w = if narrow {
        widest_name.clamp(6, width.saturating_sub(3 + idx_w + 2 + 4 + figure_w).max(6))
    } else {
        widest_name.clamp(
            6,
            width
                .saturating_sub(3 + idx_w + 2 + GAUGE_W + figure_w)
                .max(6),
        )
    };
    // Exact-fit budget against chain_row's spans: ` ╭ `(3) + `N `(digits+1)
    // + name(name_w+2), then the gauge, then the figure `  73.42%`
    // (reading_w+2) + ` / 100%` (thr_w+4). The gauge shrinks first; once even
    // its 4-cell floor leaves no room, the figure drops to its own line instead
    // of hard-wrapping an orphan `63% /`.
    let head = 3 + idx_w + name_w + 2;
    let figure_inline = !narrow || width >= head + 4 + figure_w;
    let gauge_w = if !narrow {
        GAUGE_W
    } else if figure_inline {
        width.saturating_sub(head + figure_w).clamp(4, GAUGE_W)
    } else {
        width.saturating_sub(head).clamp(4, GAUGE_W)
    };

    // Project the active profile's next switch once, up front: a `To(target)`
    // renders inline on the target member's row (right side); `Off` has no
    // single target row, so it stays a caption below. `projection` was computed
    // before the Config lock (the stores rank below it).
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
            let reason = cfg.find(name).and_then(|p| {
                blocked_reason(
                    &cfg,
                    p,
                    kick_lifts.get(name.as_str()).copied(),
                    &key_rejected,
                    readings.usage_of(&p.name),
                    readings.status_of(&p.name),
                    app.installs_session_token(&p.name),
                )
            });
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
                    reading_w,
                    thr_w,
                    figure_inline,
                    reason,
                    switch_eta,
                    pct: readings
                        .usage_of(name.as_str())
                        .and_then(|u| u.five_hour.as_ref())
                        .map(|w| w.utilization),
                },
            )
        })
        .collect();
    let trailer_col = rows.iter().map(ChainRow::base_width).max().unwrap_or(0) + TRAILER_GAP;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for row in rows {
        let ChainRow {
            base,
            figure,
            hint,
            marker,
        } = row;
        lines.push(
            ChainRow {
                base,
                figure: None,
                hint,
                marker,
            }
            .into_line(trailer_col, width),
        );
        if let Some(figure) = figure {
            lines.push(figure);
        }
    }

    // All-spent caption: wrap-off `stop` vs wrap-mode `stay`. The chip and its
    // tail wrap together so a narrow pane never clips the caption's tail.
    let (chip_style, chip_word, tail) = if cfg.state.switch_off_when_spent {
        (theme::danger().bold(), "stop", "when all spent")
    } else {
        (
            theme::dim().bold(),
            "stay",
            "on the active account when all spent",
        )
    };
    let chip = vec![
        Span::styled("[ ", theme::dim()),
        Span::styled(chip_word, chip_style),
        Span::styled(" ]", theme::dim()),
        Span::raw(" "),
    ];
    let chip_w: usize = chip.iter().map(|s| grapheme_cells(&s.content)).sum();
    // Only the first line carries the 2-cell lead and the chip, so it takes the
    // whole tail words that fit `width - chip - 2`; the rest wrap at a
    // continuation row's own budget (`width - 2`), which is wider, so a word
    // the first row cannot hold whole is never split for it. A pane the lead
    // and chip alone overrun wraps both at the floor: the chip row is cut
    // anyway, and a continuation budget under the floor would split every word.
    let words: Vec<&str> = tail.split_whitespace().collect();
    let (first_budget, rest_budget) = match width.checked_sub(chip_w + 2) {
        Some(first) => (first, width - 2),
        None => (wrap_budget(0), wrap_budget(0)),
    };
    let mut head: Vec<&str> = Vec::new();
    let mut head_w = 0usize;
    for &word in &words {
        let joined = head_w + usize::from(!head.is_empty()) + grapheme_cells(word);
        if joined > first_budget {
            break;
        }
        head_w = joined;
        head.push(word);
    }
    let mut first = vec![Span::raw("  ")];
    first.extend(chip);
    if !head.is_empty() {
        first.push(Span::styled(head.join(" "), theme::faint()));
    }
    lines.push(Line::from(first));
    if head.len() < words.len() {
        for seg in wrap_words(&words[head.len()..].join(" "), rest_budget) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(seg, theme::faint()),
            ]));
        }
    }

    // A wallet-bearing active's runway: its funded balance and burn rate, and
    // how long the two hold — the wallet sibling of the projection above. No
    // threshold and no warning hue; the figure and its pace, the operator
    // judges. Gated on the cache selector (a profile edited off a third-party
    // endpoint keeps its never-evicted store entry) and on enabled-ness (the
    // usage tab renders a disabled account terminal, no figures).
    if let Some(active) = cfg.state.active_profile.as_ref().and_then(|n| cfg.find(n))
        && active.usage_cache_is_third_party()
        && !active.is_disabled()
        && let Some(rate) = app.wallet_rate_for(&active.name, readings.stats_of(&active.name))
    {
        let secs = (rate.amount / rate.per_day * 86_400.0) as i64;
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} drains in ~{}",
                    rate.label,
                    crate::usage::humanize_duration(secs)
                ),
                theme::faint(),
            ),
        ]));
    }

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
    if let Some((name, eta)) = soonest_resume(&cfg, &key_rejected, &readings.usage) {
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
    (lines, chain.len())
}

/// The active profile's projected next switch (action + eta secs), or `None`
/// when none is imminent. The action is the scheduler walk's answer to "where
/// does the chain land when the active crosses its threshold"
/// ([`App::auto_switch_projection`]); the eta guards the projection the same
/// way the old inline line did: only when the active crosses its threshold
/// BEFORE its 5h window resets (past the reset the window refills and no switch
/// fires). Shared by the inline `To` hint (on the target's row) and the `Off`
/// caption.
fn projected_switch(app: &App, key_rejected: &HashSet<ProfileName>) -> Option<(SwitchAction, i64)> {
    let projection = app.auto_switch_projection(key_rejected);
    let action = projection.action?;
    let usage = projection.active_usage.as_ref()?.five_hour.as_ref()?;
    // In-memory rate (`history_cache`) — no disk read under any guard. A
    // third-party member has no burn history to pair with its provider window,
    // so no rate and no projection.
    let eta_secs = burn_rate_eta(
        projection.active_rate,
        usage.utilization,
        projection.active_threshold,
    )?;
    let reset_secs = super::format::reset_in_secs(usage);
    if reset_secs.is_some_and(|reset| eta_secs >= reset) {
        return None;
    }
    Some((action, eta_secs))
}

/// Cells between the widest chain row's content and the shared trailer column
/// (the contract's 3-cell right-aligned gap).
const TRAILER_GAP: usize = 3;

/// A chain row before its trailer lands. Split from the assembled `Line` so the
/// panel can measure every row's content and start every trailer at one column.
/// Only ONE row can carry the `↲ ~eta` hint (the single projected-switch
/// target), so at most that row's marker sits further right than its siblings'.
struct ChainRow {
    base: Vec<Span<'static>>,
    /// The `63 / 90%` figure when it drops to its own line (the row was too
    /// narrow to hold it beside the gauge); `None` when it rides the base.
    figure: Option<Line<'static>>,
    hint: Option<Span<'static>>,
    marker: Option<Span<'static>>,
}

impl ChainRow {
    fn base_width(&self) -> usize {
        self.base.iter().map(|s| grapheme_cells(&s.content)).sum()
    }

    /// Pad the content out to `col`, then append whichever trailers fit inside
    /// `width` (the panel's inner width). A projected-switch target carries the
    /// `↲ ~eta` hint; a blocked member carries its 1-cell reason marker. BOTH can
    /// apply to one row: the walk's headroom pass only prefers a fresh
    /// member and falls through to a stale-but-unexhausted one, so a `To`
    /// target can also be `Stale`. Render both
    /// then (hint, then marker outermost) instead of dropping the imminent-switch
    /// projection. Too narrow for the pair → keep the marker (the persistent
    /// signal) and drop the hint; too narrow for even that → drop both.
    fn into_line(self, col: usize, width: usize) -> Line<'static> {
        let fits = |w: usize| col + w <= width;
        let trailer: Vec<Span<'static>> = match (self.hint, self.marker) {
            (Some(h), Some(m))
                if fits(grapheme_cells(&h.content) + 1 + grapheme_cells(&m.content)) =>
            {
                vec![h, Span::raw(" "), m]
            }
            (Some(_), Some(m)) if fits(grapheme_cells(&m.content)) => vec![m],
            (Some(h), None) if fits(grapheme_cells(&h.content)) => vec![h],
            (None, Some(m)) if fits(grapheme_cells(&m.content)) => vec![m],
            _ => Vec::new(),
        };
        let mut spans = self.base;
        if !trailer.is_empty() {
            let used: usize = spans.iter().map(|s| grapheme_cells(&s.content)).sum();
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
    reading_w: usize,
    thr_w: usize,
    /// Whether the figure rides the base line; when false it drops to its own
    /// line so the `%` stays whole instead of hard-wrapping off the edge.
    figure_inline: bool,
    reason: Option<BlockedReason>,
    switch_eta: Option<i64>,
    /// The member's 5h utilization off the per-frame snapshot.
    pct: Option<f64>,
}

/// A chain member's 5h reading as its row's figure prints it.
fn chain_reading(pct: Option<f64>) -> Option<String> {
    pct.map(format_pct)
}

fn chain_row(cfg: &AppConfig, name: &crate::profile::ProfileName, ctx: ChainRowCtx) -> ChainRow {
    let ChainRowCtx {
        index,
        last,
        name_w,
        gauge_w,
        reading_w,
        thr_w,
        figure_inline,
        reason,
        switch_eta,
        pct,
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
    let (name_text, name_pad) = fixed_split(name.as_str(), name_w);

    let mut spans = vec![
        Span::styled(format!(" {rail} "), theme::faint()),
        // Right-align the index to the widest index's digits so every row's
        // index fills the same `idx_w` slot the budget reserved — unpadded, a
        // 1-digit row runs one cell narrower than a 2-digit row and shifts
        // every shared column after it.
        Span::styled(
            format!(
                "{:>idx$} ",
                index + 1,
                idx = grapheme_cells(&(last + 1).to_string())
            ),
            theme::faint(),
        ),
        Span::styled(format!("{name_text}{name_pad}  "), name_style),
    ];
    let mut figure_line = None;

    match cfg.find(name) {
        None => spans.push(Span::styled("missing", theme::danger())),
        Some(profile) => {
            let threshold = threshold_for(profile);
            spans.extend(gauge_spans(pct, threshold, gauge_w));
            let (figure, figure_style) = match pct {
                Some(v) => {
                    let label = format_pct(v);
                    let pad = " ".repeat(reading_w.saturating_sub(grapheme_cells(&label)));
                    (
                        format!("  {pad}{label}"),
                        Style::default().fg(theme::util_color(v)),
                    )
                }
                None => (
                    format!(
                        "  {}{NO_DATA}",
                        " ".repeat(reading_w.saturating_sub(grapheme_cells(NO_DATA)))
                    ),
                    theme::faint(),
                ),
            };
            if figure_inline {
                spans.push(Span::styled(figure, figure_style));
                spans.push(Span::styled(
                    format!(" / {threshold:>thr_w$.0}%"),
                    theme::faint(),
                ));
            } else {
                figure_line = Some(Line::from(vec![
                    Span::styled(figure, figure_style),
                    Span::styled(format!(" / {threshold:>thr_w$.0}%"), theme::faint()),
                ]));
            }
        }
    }

    ChainRow {
        base: spans,
        figure: figure_line,
        hint: switch_eta.map(|secs| {
            // `projected_switch` only ever fires off `burn_rate_eta`, so this
            // hint is always an EXHAUSTION projection — a genuine event-driven
            // return (healthy active, preferred just freed) has no eta to show.
            let mark = switch_mark();
            Span::styled(
                format!("{} ~{}", mark.content, humanize_duration(secs)),
                mark.style,
            )
        }),
        marker: reason.as_ref().map(reason_marker),
    }
}

/// The projected-switch mark a target member's chain row leads its eta with.
/// One source for the row and the help modal's glyph legend, so the legend
/// cannot drift from the glyph or the hue the chain renders.
pub(super) fn switch_mark() -> Span<'static> {
    Span::styled("\u{21b2}", theme::faint())
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
/// and a third-party window — bar-synthesized or seeded from the provider's
/// derived usage — has no history to weigh, since no third-party leg ever
/// appends `usage_history.jsonl`.
fn drain_rate(
    app: &App,
    name: &crate::profile::ProfileName,
    profile: &Profile,
    label: &str,
    window: &UsageWindow,
    readings: &ReadingsSnapshot,
) -> Option<f64> {
    if label == LABEL_5H
        && !profile.usage_cache_is_third_party()
        && let Some(usage) = readings.usage_of(&profile.name)
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
