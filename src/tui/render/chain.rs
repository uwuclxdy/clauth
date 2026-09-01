//! Fallback tab — master-detail, mirroring the Config layout. Left: the ordered
//! chain (plus a trailing `+ add` row), cursor = `❯`, `#n` chain position, active
//! member name in orange. Right: the selected member's rotation card — labeled
//! key:value rows (`5h usage` gauge with a threshold tick, `rotate at`
//! threshold stepper, `weekly at` per-account weekly-line override, `weekly
//! gate` + `scoped gate` per-account usage-check toggles, `last resort`
//! toggle, `max spend` ceiling, `remove`) — or,
//! on `+ add`, a candidate picker. Order = priority (reorder with ⇧↑↓). The
//! chain-global wrap-off and spend-budget settings live on the Config tab, not
//! here. Editing happens in place: ⏎ on the left drops focus into the right
//! pane, `+` / `-` step the threshold (or ⏎ on it to type a value), space/⏎
//! flips `last resort`, ⏎ types a `max spend` ceiling, ⏎ on remove arms then
//! confirms. No popups.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ChainItemKind, FALLBACK_ROWS, FallbackFocus, FallbackRow, InputState, chain_candidates,
    chain_items, parse_max_spend, parse_threshold, parse_weekly_override,
};
use super::super::theme;
use super::format::{ResetFmt, fixed_split, relative_age, reset_pill, reset_resume};
use super::global_config::default_reminder;
use super::panes::{
    DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED, DIAG_DISABLED, DIAG_KICK, DIAG_STALE,
    DIAG_WEEKLY_SOFT, DIAG_WEEKLY_SPENT, QueueView, bold_when, draw_selector_list, head_cols,
    help_tooltip_lines, highlight_row, invalid_tooltip_lines, key_cell, label_style, master_detail,
    name_color, pill, rail_hint_lines, section_box, section_box_verbatim, select_line, value_caret,
    wrap_words,
};
use crate::fallback::{
    BlockedReason, DEFAULT_THRESHOLD, blocked_reason, health_blocked_reason, soonest_resume,
    spend_is_uncapped, spend_room, threshold_for, uncapped_spend_fix,
};
use crate::profile::AppConfig;
use crate::usage::{humanize_duration, queue_anchor_cached, switch_grade_kick_lifts};

/// Wide enough to read a threshold tick.
const GAUGE_W: usize = 22;
/// Key column width: the longest key (`last resort`, 11), matching the Config
/// tab's `KEY_W` so the two master-detail panes open their value column at the
/// same place. `KEY_GUTTER` is the separator, so an exactly-fitting key never
/// collides with its value.
const KEY_W: usize = 11;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (selector, detail) = master_detail(area, chain_items(app).len());

    let chain_focused = app.fallback_focus == FallbackFocus::Chain;
    draw_chain_selector(frame, selector, app, chain_focused);
    draw_chain_detail(frame, detail, app);
}

fn draw_chain_selector(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let items = chain_items(app);
    // Switch-grade kick blocks the chip flags — read before the Config lock
    // (rank order: KickBlockState 230 < Config 400).
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    let cfg = app.config();
    let sel = app.chain_cursor.min(items.len().saturating_sub(1));
    draw_selector_list(frame, area, "chain", focused, sel, |w| {
        items
            .iter()
            .enumerate()
            .map(|(row, item)| {
                let selected = row == sel;
                let line = match item {
                    ChainItemKind::Member(i) => {
                        let name = cfg
                            .state
                            .fallback_chain
                            .get(*i)
                            .cloned()
                            .unwrap_or_default();
                        // `#n` right-aligned in a fixed 3 cells, so `#1` and
                        // `#10` start their names on the same column. The
                        // trailing gap absorbs the `#`, keeping the rail the
                        // same total width it had as a bare number — nothing
                        // downstream shifts.
                        let ord = format!("#{}", i + 1);
                        let rail = if selected && focused {
                            Span::styled(format!("❯ {ord:>3} "), theme::accent().bold())
                        } else {
                            Span::styled(format!("  {ord:>3} "), theme::faint())
                        };
                        // A member still sits in `fallback_chain` on disk while
                        // disabled (only the walk skips it), so it renders as a
                        // normal row with a dim name; the exclusion itself
                        // arrives through `blocked_reason`'s `Disabled` arm like
                        // any other block. It can never be `is_active`, so dim
                        // always wins over `name_color`.
                        let disabled = cfg.find(&name).is_some_and(|p| p.is_disabled());
                        let ns = if disabled {
                            bold_when(theme::dim(), selected && focused)
                        } else {
                            bold_when(name_color(cfg.is_active(&name)), selected && focused)
                        };
                        let reason = cfg.find(&name).and_then(|p| {
                            blocked_reason(&cfg, p, kick_lifts.get(name.as_str()).copied())
                        });
                        let rail_w = rail.width();
                        let mut spans = vec![rail];
                        match &reason {
                            // The 1-cell marker is right-aligned at the row's last
                            // content column (the scrollbar owns the padding cell
                            // beyond it, so they never collide), and the name is
                            // clamped to whatever that leaves. Unclamped, a long
                            // enough name pushed the marker past the pane and
                            // ratatui dropped it, so a blocked account rendered
                            // identically to a healthy one. Only a row that
                            // actually carries a marker pays the clamp — the name
                            // column's width therefore tracks blocked state.
                            Some(reason) => {
                                let name_w = (w as usize).saturating_sub(rail_w + 2);
                                let (text, pad) = fixed_split(&name, name_w);
                                spans.push(Span::styled(text, ns));
                                spans.push(Span::raw(format!("{pad} ")));
                                spans.push(reason_marker(reason));
                            }
                            None => spans.push(Span::styled(name.to_string(), ns)),
                        }
                        Line::from(spans)
                    }
                    ChainItemKind::Add => {
                        let arrow = if selected && focused {
                            Span::styled("❯ ", theme::accent().bold())
                        } else {
                            Span::raw("  ")
                        };
                        Line::from(vec![
                            arrow,
                            Span::styled(
                                "    + add",
                                bold_when(theme::accent(), selected && focused),
                            ),
                        ])
                    }
                };
                select_line(line, selected, focused, w)
            })
            .collect()
    });
}

fn draw_chain_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let detail_focused = app.fallback_focus == FallbackFocus::Detail;
    let inner_w = section_box("", detail_focused, false).inner(area).width as usize;
    let items = chain_items(app);
    let selected = items
        .get(app.chain_cursor.min(items.len().saturating_sub(1)))
        .copied();
    // Switch-grade kick blocks and the auto-start queue anchor — both read before
    // the Config lock (rank order: KickBlockState 230, AutoStartQueue 240 < Config
    // 400). The anchor is the CACHED one: `queue_anchor` would fall through to a
    // history replay, which is not something a render loop can do.
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    let queue_anchor = queue_anchor_cached(&app.auto_start_queue);

    // `Add` arm must NOT hold the `config` guard — `add_detail` re-locks it via
    // `chain_candidates`, and the mutex is non-reentrant (deadlock on `+ add` row).
    // `is_name`: member names render in original case; structural titles stay uppercased.
    // `rows_start` = the absolute index where the FALLBACK_ROWS loop begins,
    // REPORTED BY the function that pushes everything above it. The header block
    // above the rows is variable in two directions now (a disabled member stacks
    // a second pill, and every pill drags a wrapped fix line), and a caret on the
    // wrong row is invisible to every text assertion — so this is read out of the
    // buffer rather than tracked in a constant that has to be edited in lockstep.
    let (title, is_name, lines, rows_start): (String, bool, Vec<Line<'static>>, usize) =
        match selected {
            Some(ChainItemKind::Member(i)) => {
                let cfg = app.config();
                let name = cfg.state.fallback_chain.get(i).cloned().unwrap_or_default();
                let kick_lift = kick_lifts.get(name.as_str()).copied();
                let queue = QueueView::new(&cfg, &kick_lifts, queue_anchor).slot(name.as_str());
                let (lines, rows_start) = member_detail(
                    &cfg,
                    &name,
                    MemberCard {
                        focused: detail_focused,
                        row_cursor: app.fallback_detail_cursor,
                        armed_remove: app.fallback_armed_remove,
                        editing: app.fallback_threshold_draft.as_ref(),
                        max_spend_editing: app.fallback_max_spend_draft.as_ref(),
                        weekly_editing: app.fallback_weekly_draft.as_ref(),
                        width: inner_w,
                        kick_lift,
                        sessions: app.live_sessions.member(&name),
                        queue,
                    },
                );
                (name.to_string(), true, lines, rows_start)
            }
            Some(ChainItemKind::Add) => (
                "add to chain".to_string(),
                false,
                add_detail(app, detail_focused, inner_w),
                0,
            ),
            None => ("chain".to_string(), false, empty_detail(), 0),
        };

    let block = if is_name {
        section_box_verbatim(&title, detail_focused, false)
    } else {
        section_box(&title, detail_focused, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // The header block + seven rows + a tooltip outgrow a short pane (a 40x24
    // terminal leaves ~14 inner rows, and a blocked member's pill block eats
    // two more), and the card has no scrollbar — so a FOCUSED card scrolls the
    // cursored row (plus up to a 2-line tooltip) into view instead of clipping
    // `max spend` and `remove` off the bottom. Unfocused keeps the top
    // anchored: while browsing the left chain the identity header is the
    // payload, not the rows.
    let content_h = lines.len();
    let scroll = if detail_focused && matches!(selected, Some(ChainItemKind::Member(_))) {
        let cursor = app.fallback_detail_cursor.min(FALLBACK_ROWS.len() - 1);
        (rows_start + cursor + 3)
            .saturating_sub(inner.height as usize)
            .min(content_h.saturating_sub(inner.height as usize))
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll as u16, 0)),
        inner,
    );

    // Position the native terminal cursor for whichever field is being typed,
    // matching the post-draw cursor path the other edit screens use. This is not
    // decoration: `value_caret` renders the buffer with uniform styling and
    // leaves the caret glyph entirely to the cursor set here, so a field that
    // skips this has no visible caret at all.
    //
    // `rows_start` already accounts for everything `member_detail` pushed above
    // the row loop, and only the row being typed is selected — so no earlier row
    // contributes a tooltip line and a row's index in FALLBACK_ROWS is exactly
    // its offset past `rows_start`.
    let typing = [
        (
            FallbackRow::Threshold,
            app.fallback_threshold_draft.as_ref(),
            0usize,
        ),
        // `+ 1` for the leading `$`, which sits before the buffer.
        (
            FallbackRow::MaxSpend,
            app.fallback_max_spend_draft.as_ref(),
            1usize,
        ),
        (
            FallbackRow::WeeklyAt,
            app.fallback_weekly_draft.as_ref(),
            0usize,
        ),
    ]
    .into_iter()
    .find_map(|(row, draft, unit_cols)| draft.map(|d| (row, d, unit_cols)));

    // The row must actually be ON the pane before its caret is placed. The
    // scroll above chases the CURSORED row, which is the row being typed on
    // every real path — but a degenerate pane (height smaller than the
    // lookahead) can still leave it off, and an unguarded set parks a visible
    // caret on a border or the pane below, since a real terminal clamps the
    // row rather than dropping it. Mirrors the `config.rs` edit-caret guard.
    if detail_focused
        && let Some(ChainItemKind::Member(_)) = selected
        && let Some((row, draft, unit_cols)) = typing
        && let Some(row_idx) = FALLBACK_ROWS.iter().position(|r| *r == row)
        && rows_start + row_idx >= scroll
        && rows_start + row_idx - scroll < inner.height as usize
    {
        // x = "❯ " (2) + key block (KEY_W + KEY_GUTTER cols) + unit + cols before caret.
        let prefix_cols = 2 + KEY_W + KEY_GUTTER + unit_cols + head_cols(draft);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner
            .y
            .saturating_add((rows_start + row_idx - scroll) as u16);
        frame.set_cursor_position((cx, cy));
    }
}

/// 1-cell selector marker for a member's worst blocked reason: color bands the
/// severity, the glyph shape names the reason (the detail pill spells it out in
/// full). Absent when the member has headroom.
///
/// `Disabled` and `Canceled` deliberately SHARE `⊖` and split on hue alone
/// (faint vs danger), the one place this app departs from cloudy-tui's
/// shape-names-the-state rule: the two co-occur on nearly every real account
/// (an operator disables a subscription once it's canceled), and the Overview
/// account row picks the canceled arm where this ladder picks the disabled one,
/// so distinct shapes made the same account wear two glyphs on one screen.
pub(super) fn reason_marker(reason: &BlockedReason) -> Span<'static> {
    let (glyph, style) = match reason {
        BlockedReason::Disabled => ("⊖", theme::faint()),
        BlockedReason::Canceled => ("⊖", theme::danger()),
        BlockedReason::AuthBroken => ("×", theme::danger()),
        BlockedReason::WeeklySpent { .. } => ("⊘", theme::danger()),
        BlockedReason::KickRejected { .. } => ("⧗", theme::warning()),
        BlockedReason::BudgetSpent => ("$", theme::warning()),
        BlockedReason::FiveHour { .. } => ("◔", theme::warning()),
        // `⊘` = a weekly window is spent; hue splits scope exactly like `⊖`
        // splits disabled/canceled: danger = the aggregate week (dead for
        // days), warning = one model's week (still serves the rest).
        BlockedReason::ScopedSpent { .. } => ("⊘", theme::warning()),
        BlockedReason::WeeklySoft { .. } => ("~", theme::warning()),
        BlockedReason::Stale => ("⋯", theme::faint()),
    };
    Span::styled(glyph, style)
}

/// Blocked-reason status pill for the detail card: `[ label ]`, label bold in the
/// reason's semantic color (neutral dim for stale), brackets dim — the cloudy-tui
/// status pill. Window resets run through `reset_pill`, so they follow the
/// operator's `reset display` setting; the kick-block lift stays a bare
/// countdown — the limiter relents on its own schedule, so a wall-clock time
/// there would claim a precision the estimate doesn't have.
fn reason_pill_spans(reason: &BlockedReason, fmt: ResetFmt) -> Vec<Span<'static>> {
    // Every pill is `[ label ]` with an optional qualifier trailing as a faint
    // suffix OUTSIDE the brackets (a reset countdown, a lift ETA, "still
    // serving") — never crammed inside with a `·`, so the whole card and the
    // Usage-tab pills read one shape.
    let (label, style, suffix) = match reason {
        BlockedReason::Disabled => (DIAG_DISABLED.to_string(), theme::dim().bold(), None),
        BlockedReason::Canceled => (DIAG_CANCELED.to_string(), theme::danger().bold(), None),
        BlockedReason::AuthBroken => (DIAG_AUTH_BROKEN.to_string(), theme::danger().bold(), None),
        BlockedReason::WeeklySpent { resets_in } => (
            DIAG_WEEKLY_SPENT.to_string(),
            theme::danger().bold(),
            resets_in.as_ref().map(|s| reset_pill(*s, fmt)),
        ),
        BlockedReason::KickRejected { lifts_in } => (
            DIAG_KICK.to_string(),
            theme::warning().bold(),
            Some(humanize_duration(*lifts_in)),
        ),
        BlockedReason::BudgetSpent => {
            (DIAG_BUDGET_SPENT.to_string(), theme::warning().bold(), None)
        }
        BlockedReason::FiveHour { pct, resets_in } => (
            format!("5h {pct:.0}%"),
            theme::warning().bold(),
            resets_in.as_ref().map(|s| reset_pill(*s, fmt)),
        ),
        BlockedReason::ScopedSpent { label, pct } => (
            format!("{label} {pct:.0}%"),
            theme::warning().bold(),
            Some("other models ok".to_string()),
        ),
        BlockedReason::WeeklySoft { pct } => (
            format!("weekly {pct:.0}%"),
            theme::warning().bold(),
            Some("still serving".to_string()),
        ),
        BlockedReason::Stale => (DIAG_STALE.to_string(), theme::dim().bold(), None),
    };
    let mut spans = pill(label, style);
    if let Some(suffix) = suffix {
        spans.push(Span::styled(format!("  {suffix}"), theme::faint()));
    }
    spans
}

/// The `├`/`└` fix line under a blocked-reason pill: what to actually do about
/// it. Deliberately NOT `usage.rs::diag_fix` — that maps a different enum
/// (`UsageDiag` splits kick blocks by `auto_start` and carries states this
/// ladder has no notion of), so bridging the two just to share strings would
/// couple two ladders that are allowed to diverge. Same register: short,
/// lowercase, names the concrete next action.
fn reason_fix(reason: &BlockedReason, name: &crate::profile::ProfileName) -> String {
    match reason {
        BlockedReason::Disabled => "excluded from the walk, enable it on the setup tab".to_string(),
        BlockedReason::Canceled => "this subscription has been canceled".to_string(),
        BlockedReason::AuthBroken => format!("re-login with clauth login {name}"),
        BlockedReason::WeeklySpent { .. } => "weekly limit is spent".to_string(),
        BlockedReason::KickRejected { .. } => "claude code is refusing to start it".to_string(),
        BlockedReason::BudgetSpent => "raise max spend below".to_string(),
        BlockedReason::FiveHour { .. } => "5h quota is spent, it resets on its own".to_string(),
        BlockedReason::ScopedSpent { .. } => {
            "that model's weekly window is spent, other models still serve".to_string()
        }
        BlockedReason::WeeklySoft { .. } => DIAG_WEEKLY_SOFT.to_string(),
        BlockedReason::Stale => "last usage check failed".to_string(),
    }
}

/// The member card's live-session block: how many `clauth start` sessions are
/// running as THIS member, plus — only once one of them has actually swapped —
/// when that happened and the caveat that makes the figure honest.
///
/// `live` is the one word every TUI surface counting sessions uses (the
/// Overview column header, the Plugin runtime row, the Setup tab's disable
/// gate), so the count reads the same wherever the operator meets it on screen.
/// The CLI's refusals share the noun but not the phrasing — they answer a
/// different question (why a command was refused), so they word it their own
/// way.
///
/// The follower qualifier (`N, M with fallback`) returned to this card on
/// 2026-07-27 after a one-day removal: `⇄` on the Overview cell still carries
/// the same split for the column view, but this card is where the chain facts
/// live. A glyph alone could not say "1 of these is movable" next to rows
/// that already spell out every other chain fact. Shown only when
/// `following > 0`. A pure-pinned count has no split to name. Both surfaces
/// read the same `MemberSessions::following` field.
///
/// No leading blank: the caller owns the gaps around the block (post-pill
/// blank above, post-live blank below), so where it sits in the card is the
/// caller's decision and stays consistent whether or not 5h data exists.
///
/// Claude Code re-reads its credentials on its NEXT REQUEST, an mtime `stat` on
/// the request path with no watcher behind it, so a session that just swapped
/// keeps authenticating as the old member until it next talks. `current_member`
/// is therefore where clauth PUT the link, not who is being billed this second,
/// and nothing in the registry can observe the pickup — hence a caveat rather
/// than an invented "not yet picked up" state. A session that never swapped has
/// no repointed link and so gets no caveat.
fn live_session_lines(
    sessions: crate::live_sessions::MemberSessions,
    width: usize,
) -> Vec<Line<'static>> {
    if sessions.sessions == 0 {
        return Vec::new();
    }
    let count = if sessions.following > 0 {
        format!(
            "{}, {} with fallback",
            sessions.sessions, sessions.following
        )
    } else {
        sessions.sessions.to_string()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(key_cell("live", KEY_W, KEY_GUTTER), theme::label()),
        Span::styled(count, theme::dim()),
    ])];
    let Some(at) = sessions.last_swap_at else {
        return lines;
    };
    // An AGE, so it reads through `relative_age` (single largest unit, ISO date
    // past 30 days) rather than the two-unit `humanize_duration` the countdowns
    // use — a countdown is a duration, this is a point in the past.
    lines.push(Line::from(vec![
        Span::styled(key_cell("last swap", KEY_W, KEY_GUTTER), theme::label()),
        Span::styled(relative_age(at), theme::dim()),
    ]));
    lines.extend(help_tooltip_lines(
        "picked up on the session's next request",
        width,
    ));
    lines
}

/// The blocked-reason pill block: each pill on its own row with its `└` fix
/// line, connected into one `├│└` rail when 2+ stack (cloudy-tui Stacked
/// hints). The first row carries the `status` key so the rail has a column to
/// anchor against; later rows bridge with `│` at col 0 while the rail is open.
///
/// Mirrors `usage.rs::status_lines`'s shape but keys off THIS card's `KEY_W`,
/// so the pill's value column lines up with `5h usage` / `rotate at` beneath
/// it. Both surfaces draw their glyph lines with the shared
/// [`rail_hint_lines`], so the rail itself has exactly one implementation.
fn pill_block(pills: Vec<(Vec<Span<'static>>, String)>, width: usize) -> Vec<Line<'static>> {
    let total = pills.len();
    let mut lines = Vec::with_capacity(total * 2);
    for (i, (content, hint)) in pills.into_iter().enumerate() {
        // Any row past the first implies 2+ pills, and the rail is still open
        // there because this row's own hint hasn't been emitted yet — so a
        // later row always bridges, never blank-pads.
        let key = if i == 0 {
            Span::styled(key_cell("status", KEY_W, KEY_GUTTER), theme::label())
        } else {
            Span::styled(
                format!("│{}", " ".repeat(KEY_W + KEY_GUTTER - 1)),
                theme::line(),
            )
        };
        let mut spans = vec![key];
        spans.extend(content);
        lines.push(Line::from(spans));
        lines.extend(rail_hint_lines(&hint, width, i + 1 < total));
    }
    lines
}

/// The member card's render context — pane focus, cursor, edit drafts, width,
/// the kick lift, and the live-session tally. Grouped so [`member_detail`]
/// stays under clippy's argument limit without an ad-hoc `#[allow]`.
#[derive(Clone, Copy, Default)]
struct MemberCard<'a> {
    focused: bool,
    row_cursor: usize,
    armed_remove: bool,
    editing: Option<&'a InputState>,
    max_spend_editing: Option<&'a InputState>,
    weekly_editing: Option<&'a InputState>,
    width: usize,
    kick_lift: Option<i64>,
    sessions: crate::live_sessions::MemberSessions,
    /// This member's auto-start queue slot (`usage::auto_start_queue`),
    /// resolved by the caller before the Config guard via [`QueueView`];
    /// `None` when it holds none.
    queue: Option<crate::usage::QueueSlot>,
}

/// Live-session count, 5h gauge with threshold tick, headroom figure, and the
/// inline `rotate at` threshold stepper/editor + `last resort` toggle + `remove` rows.
/// Caret only when focused.
fn member_detail(
    cfg: &AppConfig,
    name: &crate::profile::ProfileName,
    card: MemberCard<'_>,
) -> (Vec<Line<'static>>, usize) {
    let MemberCard {
        focused,
        row_cursor,
        armed_remove,
        editing,
        max_spend_editing,
        weekly_editing,
        width,
        kick_lift,
        sessions,
        queue,
    } = card;
    let Some(profile) = cfg.find(name) else {
        return (
            vec![Line::from(Span::styled(
                "account no longer exists · remove it from the chain",
                theme::danger(),
            ))],
            0,
        );
    };

    let threshold = threshold_for(profile);
    let pct = profile
        .usage
        .as_ref()
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization);
    let cursor = row_cursor.min(FALLBACK_ROWS.len() - 1);

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Blocked-reason pills, worst first, above everything else on the card.
    // `Disabled` does NOT hide the member's health: an operator who disabled a
    // broken account still needs to see that it is broken, so the health reason
    // stacks beneath the `[ disabled ]` pill rather than being replaced by it.
    // Both come out of the one ladder (`blocked_reason` delegates to
    // `health_blocked_reason`), so the pills can't disagree with the marker.
    let fmt = ResetFmt::from_state(&cfg.state);
    let mut pills: Vec<(Vec<Span<'static>>, String)> = Vec::new();
    if let Some(reason) = blocked_reason(cfg, profile, kick_lift) {
        pills.push((reason_pill_spans(&reason, fmt), reason_fix(&reason, name)));
        if reason == BlockedReason::Disabled
            && let Some(health) = health_blocked_reason(cfg, profile, kick_lift)
        {
            pills.push((reason_pill_spans(&health, fmt), reason_fix(&health, name)));
        }
    }
    if !pills.is_empty() {
        lines.extend(pill_block(pills, width));
        lines.push(Line::from(""));
    }

    // Live-session block above the 5h gauge, so the chain-movable count reads
    // ahead of the gauge that decides when it moves. The block owns no padding:
    // the gap above is the post-pill blank (when pills exist); the gap below is
    // the trailing blank emitted here when the block is non-empty.
    let live_lines = live_session_lines(sessions, width);
    if !live_lines.is_empty() {
        lines.extend(live_lines);
        lines.push(Line::from(""));
    }

    // Auto-start queue slot, on the card that already spells out every other
    // chain fact about this member. Its own `auto-start` key rather than a
    // pill stacked into the block above: that block is the BLOCKED-reason
    // ladder, and a queue slot is not a reason this member is being routed
    // around. No tooltip line — the pill and the Config row's hint already say
    // it, and this row sits above the fold on every member's card. Same block
    // convention as the live-session one above — the row owns no padding, the
    // trailing blank is emitted here.
    if let Some(slot) = &queue {
        let mut spans = vec![Span::styled(
            key_cell("auto-start", KEY_W, KEY_GUTTER),
            theme::label(),
        )];
        spans.extend(pill(
            format!("queue {}/{}", slot.position, slot.total),
            theme::accent(),
        ));
        // The countdown — the QUEUE's shared estimate, not this member's own
        // turn — sits flush against the card's right edge, shedding its
        // leading words when the card narrows so the digits survive at every
        // operable width. Rendered inline it was the tail that clipped, and
        // `next in 2h 26m` read `next in 2` on a narrow card.
        if let Some(secs) = slot.next_in {
            let used: usize = spans.iter().map(|s| s.width()).sum();
            let eta = humanize_duration(secs);
            let fitting = [format!("next in {eta}"), format!("in {eta}"), eta]
                .into_iter()
                .find(|v| used + 1 + v.chars().count() <= width);
            if let Some(text) = fitting {
                spans.push(Span::raw(" ".repeat(width - used - text.chars().count())));
                spans.push(Span::styled(text, theme::faint()));
            }
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(""));
    }

    // `5h usage` — gauge lives on the kv key row (matching the `rotate at`
    // grammar), headroom figure indented beneath it. Two lines, not three:
    // the standalone eyebrow is folded into the key.
    let mut gauge_spans = vec![Span::styled(
        key_cell("5h usage", KEY_W, KEY_GUTTER),
        theme::label(),
    )];
    gauge_spans.extend(gauge_with_tick(pct, Some(threshold)));
    if let Some(v) = pct {
        gauge_spans.push(Span::styled(format!("  {v:.0}% used"), theme::util(v)));
    } else {
        gauge_spans.push(Span::styled("  no data yet", theme::faint()));
    }
    lines.push(Line::from(gauge_spans));

    // No 5h reading means no headroom figure — an empty continuation line here
    // read as a deliberate gap on exactly the accounts that had nothing to say.
    if let Some(v) = pct {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(KEY_W + KEY_GUTTER)),
            Span::styled(
                format!("{:.0}% until rotate", (threshold - v).max(0.0)),
                theme::faint(),
            ),
        ]));
    }
    lines.push(Line::from(""));

    // Where the FALLBACK_ROWS loop starts, taken from the buffer itself rather
    // than from a hand-maintained constant. `draw_chain_detail` adds `row_idx`
    // to this for the native caret, and a caret on the wrong row is invisible to
    // every text assertion — so the count is READ from what was actually pushed.
    // The old `ROWS_BEFORE = 5` had to be edited in lockstep with the header
    // block and would have silently desynced when the `priority` row went away.
    let rows_start = lines.len();

    for (i, row) in FALLBACK_ROWS.iter().enumerate() {
        let selected = focused && i == cursor;
        let row_editing = match *row {
            FallbackRow::Threshold => editing,
            FallbackRow::MaxSpend => max_spend_editing,
            FallbackRow::WeeklyAt => weekly_editing,
            _ => None,
        };
        let line = detail_row(
            *row,
            selected,
            MemberRow {
                threshold,
                weekly_override: profile.weekly_threshold,
                weekly_default: cfg.state.weekly_switch_threshold_pct(),
                check_weekly: profile.check_weekly,
                check_scoped: profile.check_scoped,
                last_resort: profile.last_resort,
                preferred: profile.preferred,
                max_spend: profile.max_auto_spend.unwrap_or(0.0),
                spend_budget: cfg.state.spend_budget_switching,
                armed_remove,
            },
            row_editing,
        );
        lines.push(if selected {
            highlight_row(line, width)
        } else {
            line
        });
        // `rotate at` shows its help hint while the row is selected; while typing,
        // it swaps to an always-on `0–100 %` range tooltip (faint, DANGER when out
        // of range) — mirroring the Config-tab refresh editor.
        if *row == FallbackRow::Threshold {
            match row_editing {
                Some(input) => lines.extend(threshold_range_tooltip(input, width)),
                None if selected => lines.extend(help_tooltip_lines(
                    "switches to the next account once 5h usage passes this level",
                    width,
                )),
                None => {}
            }
        }
        // `weekly at` mirrors `rotate at`: a range tooltip while typing, else
        // a hint naming what the current value does — including the inert
        // state while the member's weekly gate is off.
        if *row == FallbackRow::WeeklyAt {
            match row_editing {
                Some(input) => lines.extend(weekly_override_range_tooltip(input, width)),
                None if selected => {
                    let hint = if !profile.check_weekly {
                        "weekly gate is off, this line isn't checked for this account"
                    } else if profile.weekly_threshold.is_some() {
                        "switches away from this account once weekly usage passes this level"
                    } else {
                        "switches away from this account at the chain's shared weekly level"
                    };
                    lines.extend(help_tooltip_lines(hint, width));
                }
                None => {}
            }
        }
        // The gate toggles hint the CURRENT state — what the walk does with
        // this account right now — so flipping reads as choosing the other
        // sentence.
        if *row == FallbackRow::CheckWeekly && selected {
            let hint = if profile.check_weekly {
                "weekly usage past the limit takes this account out of rotation"
            } else {
                "weekly usage isn't checked when auto-switching; only the 100% cap blocks"
            };
            lines.extend(help_tooltip_lines(hint, width));
        }
        if *row == FallbackRow::CheckScoped && selected {
            let hint = if profile.check_scoped {
                "a spent per-model week (e.g. 7d fable) takes this account out of rotation"
            } else {
                "per-model weeks aren't checked; stays in rotation for other models"
            };
            lines.extend(help_tooltip_lines(hint, width));
        }
        if *row == FallbackRow::LastResort && selected {
            lines.extend(help_tooltip_lines(
                &last_resort_hint(cfg, name, profile.last_resort),
                width,
            ));
        }
        if *row == FallbackRow::Preferred && selected {
            lines.extend(help_tooltip_lines(
                &preferred_hint(cfg, name, profile.preferred),
                width,
            ));
        }
        // `max spend` mirrors `rotate at`: a range tooltip while typing, else a
        // hint naming the state the current value produces. The hint calls out
        // the OTHER half of the opt-in when it is the one holding spending
        // back — a ceiling with the chain toggle off does nothing, and silently
        // doing nothing is exactly what an operator would misread as armed.
        if *row == FallbackRow::MaxSpend {
            let ceiling = profile.max_auto_spend.unwrap_or(0.0);
            match row_editing {
                Some(input) => lines.extend(max_spend_range_tooltip(input, width)),
                // An uncapped config warns whether or not the row is selected:
                // it is the one state where the ceiling does not bound the bill,
                // so it must not hide until someone arrows onto the field.
                None if spend_is_uncapped(cfg, ceiling) => lines.extend(invalid_tooltip_lines(
                    &format!("nothing stops the spending: {}", uncapped_spend_fix()),
                    width,
                )),
                None if selected => lines.extend(help_tooltip_lines(
                    &max_spend_hint(cfg, name, ceiling),
                    width,
                )),
                None => {}
            }
        }
    }

    // All-exhausted sibling of the Overview projection line: when EVERY chain
    // member is currently maxed, name whichever one resumes first instead of
    // leaving the recovery implicit (issue #10 follow-up). Chain-wide, so it
    // renders under whichever member happens to be selected.
    if let Some((resume_name, eta)) = soonest_resume(cfg) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "resumes: {resume_name} {}",
                reset_resume(eta, ResetFmt::from_state(&cfg.state))
            ),
            theme::faint(),
        )));
    }
    (lines, rows_start)
}

/// Hint under the `last resort` toggle — phrased for the state flipping it
/// would produce: on → describes the standing behavior; off → what turning it
/// on does, naming the member the (exclusive) mark would move away from.
fn last_resort_hint(cfg: &AppConfig, name: &crate::profile::ProfileName, on: bool) -> String {
    if on {
        return "this account keeps working once every other one is spent".to_string();
    }
    match cfg
        .profiles
        .iter()
        .find(|p| p.last_resort && p.name != *name)
    {
        Some(marked) => format!(
            "make this the fallback of last resort instead of '{}'",
            marked.name
        ),
        None => "keep using this account once every other one is spent".to_string(),
    }
}

/// Hint under the `preferred` toggle — twin of [`last_resort_hint`]: on →
/// describes the standing return behavior; off → what turning it on does,
/// naming the member the (exclusive) mark would move away from.
fn preferred_hint(cfg: &AppConfig, name: &crate::profile::ProfileName, on: bool) -> String {
    if on {
        return "work returns to this account once it's free again".to_string();
    }
    match cfg.profiles.iter().find(|p| p.preferred && p.name != *name) {
        Some(marked) => format!("make this the home account instead of '{}'", marked.name),
        None => "return work to this account once it's free again".to_string(),
    }
}

/// Sub-line under the `rotate at` field while typing: the valid range, in DANGER
/// when the current buffer parses out of range (or non-numeric), else faint —
/// the threshold twin of the Config-tab refresh editor's `refresh_range_tooltip`.
fn threshold_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "0-100 %";
    if parse_threshold(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

/// Sub-line under the `weekly at` field while typing: the valid range, DANGER
/// when the buffer parses invalid.
fn weekly_override_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "50-100 %";
    if parse_weekly_override(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

/// Sub-line under the `max spend` field while typing — the ceiling twin of
/// [`threshold_range_tooltip`]. `inf` parses as a float, so the rejection is a
/// money guard, not input hygiene (see `app::parse_max_spend`).
fn max_spend_range_tooltip(input: &InputState, width: usize) -> Vec<Line<'static>> {
    let range = "dollars · 0 turns it off";
    if parse_max_spend(input.trimmed()).is_none() {
        invalid_tooltip_lines(range, width)
    } else {
        help_tooltip_lines(range, width)
    }
}

/// Hint under the `max spend` field, naming whichever half of the opt-in is
/// currently holding spending back and showing the REAL armed room when both are
/// set. Both halves are required, so a ceiling alone reads as armed while doing
/// nothing — that is the reading this line exists to stop. `spend_room` fails
/// closed on money (unknown spend never reads as $0), so each of its refusals
/// gets its own copy instead of one $0-implying fallback.
fn max_spend_hint(cfg: &AppConfig, name: &crate::profile::ProfileName, ceiling: f64) -> String {
    if !cfg.state.spend_budget_switching {
        return "turn on allow extra usage in config before this does anything".to_string();
    }
    if ceiling <= 0.0 {
        return "never spends here; type a ceiling to allow it".to_string();
    }
    let spend = cfg
        .find(name)
        .and_then(|p| p.usage.as_ref())
        .and_then(|u| u.spend.as_ref());
    match spend {
        Some(spend) if !spend.enabled => "this account isn't set up for paid usage".to_string(),
        // A live figure only when spend is known AND some room remains; unknown
        // spend or a spent-out budget both fall back to the ceiling statement,
        // which stays true either way rather than inventing a $0 room.
        Some(spend) => match spend_room(spend, ceiling) {
            Some(room) => format!("${room:.2} left to spend here before it stops"),
            None => format!("spends at most ${ceiling:.2} here once every account is spent"),
        },
        None => format!("spends at most ${ceiling:.2} here once every account is spent"),
    }
}

/// The member values one FALLBACK_ROWS row renders, read off the member's
/// `Profile` and the chain-wide `AppState` in [`member_detail`]. Grouped so
/// [`detail_row`] stays under clippy's argument limit without an ad-hoc
/// `#[allow]`.
#[derive(Clone, Copy)]
struct MemberRow {
    threshold: f64,
    weekly_override: Option<f64>,
    weekly_default: f64,
    check_weekly: bool,
    check_scoped: bool,
    last_resort: bool,
    preferred: bool,
    max_spend: f64,
    spend_budget: bool,
    armed_remove: bool,
}

fn detail_row(
    row: FallbackRow,
    selected: bool,
    values: MemberRow,
    editing: Option<&InputState>,
) -> Line<'static> {
    let MemberRow {
        threshold,
        weekly_override,
        weekly_default,
        check_weekly,
        check_scoped,
        last_resort,
        preferred,
        max_spend,
        spend_budget,
        armed_remove,
    } = values;
    let arrow = if editing.is_some() {
        Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold())
    } else if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    match row {
        FallbackRow::Threshold => {
            let mut spans = vec![
                arrow,
                Span::styled(
                    key_cell("rotate at", KEY_W, KEY_GUTTER),
                    label_style(selected),
                ),
            ];
            match editing {
                Some(input) => {
                    // Invalid typed value renders in DANGER (the gutter `└ invalid input`
                    // tooltip carries the reason); valid keeps body styling.
                    let invalid = parse_threshold(input.trimmed()).is_none();
                    spans.extend(value_caret(input, invalid));
                    let pct_style = if invalid {
                        theme::danger()
                    } else {
                        theme::faint()
                    };
                    // Leading space so the native caret (parked at the buffer end)
                    // sits in a blank cell and `%` renders after it — matching the
                    // refresh editor's ` s` unit.
                    spans.push(Span::styled(" %", pct_style));
                }
                None => {
                    spans.push(Span::styled(format!("{threshold:.0}%"), theme::accent()));
                    if (threshold - DEFAULT_THRESHOLD).abs() > f64::EPSILON {
                        spans.push(Span::styled(
                            format!("   default: {DEFAULT_THRESHOLD:.0}%"),
                            theme::faint(),
                        ));
                    }
                }
            }
            Line::from(spans)
        }
        FallbackRow::WeeklyAt => {
            // Inert while the member's weekly gate is off: the line isn't
            // judged, so render the whole row faint (the key handler no-ops
            // it) — same disabled-row contract as the budget-off ceiling.
            let dimmed = !check_weekly && editing.is_none();
            let arrow = if dimmed && selected {
                Span::styled("❯ ", theme::faint())
            } else {
                arrow
            };
            let key_style = if dimmed {
                theme::faint()
            } else {
                label_style(selected)
            };
            let mut spans = vec![
                arrow,
                Span::styled(key_cell("weekly at", KEY_W, KEY_GUTTER), key_style),
            ];
            match editing {
                Some(input) => {
                    let invalid = parse_weekly_override(input.trimmed()).is_none();
                    spans.extend(value_caret(input, invalid));
                    let pct_style = if invalid {
                        theme::danger()
                    } else {
                        theme::faint()
                    };
                    spans.push(Span::styled(" %", pct_style));
                }
                None => match weekly_override {
                    Some(v) => {
                        let value_style = if dimmed {
                            theme::faint()
                        } else {
                            theme::accent()
                        };
                        spans.push(Span::styled(format!("{v:.0}%"), value_style));
                        // Mirrors `rotate at`: only remind of the default once the
                        // value actually differs from it (else it's noise).
                        if (v - weekly_default).abs() > f64::EPSILON {
                            spans.push(default_reminder(format!("{weekly_default:.0}%")));
                        }
                    }
                    // Unset follows the chain-wide line — show that value, but
                    // faint, so a member-set figure stays visually distinct.
                    None => {
                        spans.push(Span::styled(
                            format!("{weekly_default:.0}%"),
                            theme::faint(),
                        ));
                    }
                },
            }
            Line::from(spans)
        }
        FallbackRow::CheckWeekly
        | FallbackRow::CheckScoped
        | FallbackRow::LastResort
        | FallbackRow::Preferred => {
            let (key, on) = match row {
                FallbackRow::CheckWeekly => ("weekly gate", check_weekly),
                FallbackRow::CheckScoped => ("scoped gate", check_scoped),
                FallbackRow::Preferred => ("preferred", preferred),
                _ => ("last resort", last_resort),
            };
            let (value, style) = if on {
                (theme::toggle_on().to_string(), theme::accent())
            } else {
                (theme::toggle_off().to_string(), theme::faint())
            };
            Line::from(vec![
                arrow,
                Span::styled(key_cell(key, KEY_W, KEY_GUTTER), label_style(selected)),
                Span::styled(value, style),
            ])
        }
        FallbackRow::MaxSpend => {
            // Inert until the chain-wide `spend budget` is on: render the whole row
            // faint (cloudy-tui disabled row) so a ceiling never reads as armed
            // while nothing can spend, and the key handler no-ops it. The
            // `max_spend_hint` names the holding half.
            let dimmed = !spend_budget && editing.is_none();
            let arrow = if dimmed && selected {
                Span::styled("❯ ", theme::faint())
            } else {
                arrow
            };
            let key_style = if dimmed {
                theme::faint()
            } else {
                label_style(selected)
            };
            let mut spans = vec![
                arrow,
                Span::styled(key_cell("max spend", KEY_W, KEY_GUTTER), key_style),
            ];
            match editing {
                Some(input) => {
                    let invalid = parse_max_spend(input.trimmed()).is_none();
                    // `$` leads the field here rather than trailing as a unit —
                    // the caret parks at the buffer end, so a trailing symbol
                    // would sit behind it.
                    spans.push(Span::styled(
                        "$",
                        if invalid {
                            theme::danger()
                        } else {
                            theme::faint()
                        },
                    ));
                    spans.extend(value_caret(input, invalid));
                }
                None if max_spend > 0.0 => {
                    let value_style = if dimmed {
                        theme::faint()
                    } else {
                        theme::accent()
                    };
                    spans.push(Span::styled(format!("${max_spend:.2}"), value_style));
                }
                // $0 is the never-spend default, so it reads as off rather than
                // as a number the operator chose.
                None => spans.push(Span::styled("off", theme::faint())),
            }
            Line::from(spans)
        }
        FallbackRow::Remove => {
            let label = if armed_remove {
                "press again to remove".to_string()
            } else {
                "remove from chain".to_string()
            };
            Line::from(vec![
                arrow,
                Span::styled(key_cell("remove", KEY_W, KEY_GUTTER), label_style(selected)),
                Span::styled(label, theme::danger()),
            ])
        }
    }
}

fn add_detail(app: &App, focused: bool, width: usize) -> Vec<Line<'static>> {
    let candidates = chain_candidates(app);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("add an account to the rotation", theme::dim())),
        Line::from(""),
    ];
    lines.extend(
        wrap_words(
            "when an account runs out, clauth points claude code at the next one.",
            width,
        )
        .into_iter()
        .map(|seg| Line::from(Span::styled(seg, theme::dim()))),
    );
    lines.push(Line::from(""));

    if candidates.is_empty() {
        lines.push(Line::from(Span::styled(
            "every account is already in the chain",
            theme::faint(),
        )));
        return lines;
    }

    if !focused {
        return lines;
    }

    let cursor = app
        .fallback_detail_cursor
        .min(candidates.len().saturating_sub(1));
    for (i, name) in candidates.iter().enumerate() {
        let selected = i == cursor;
        let arrow = if selected {
            Span::styled("❯ ", theme::accent().bold())
        } else {
            Span::raw("  ")
        };
        let ns = bold_when(theme::body(), selected);
        let line = Line::from(vec![arrow, Span::styled(name.clone(), ns)]);
        lines.push(if selected {
            highlight_row(line, width)
        } else {
            line
        });
    }
    lines
}

fn empty_detail() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("chain is empty", theme::dim())),
        Line::from(""),
        Line::from(Span::styled(
            "create an account first, then add it to the chain.",
            theme::dim(),
        )),
    ]
}

/// `GAUGE_W`-cell usage bar: fill colored by the usage thresholds (via
/// `util_color`), with a `│` tick at the rotate threshold. Once the fill reaches
/// or passes the tick column, the tick is drawn `│` in `DANGER` over the fill so
/// the "over limit" marker is never occluded.
fn gauge_with_tick(pct: Option<f64>, threshold: Option<f64>) -> Vec<Span<'static>> {
    let value = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let fill = ((value / 100.0) * GAUGE_W as f64).round() as usize;
    let fill = fill.min(GAUGE_W);
    let tick = threshold.map(|t| {
        (((t.clamp(0.0, 100.0) / 100.0) * GAUGE_W as f64).round() as usize).min(GAUGE_W - 1)
    });
    let fill_style = match pct {
        Some(v) => theme::util(v),
        None => theme::faint(),
    };

    let mut spans = vec![];
    for i in 0..GAUGE_W {
        if Some(i) == tick {
            // Below the fill the tick is a neutral marker; once fill reaches it,
            // promote to DANGER so it stays visible over the blocks.
            let style = if i < fill {
                theme::danger()
            } else {
                theme::dim()
            };
            spans.push(Span::styled("│", style));
        } else if i < fill {
            spans.push(Span::styled("█", fill_style));
        } else {
            spans.push(Span::styled("░", theme::line_strong()));
        }
    }
    spans
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_chain.rs"]
mod tests;
