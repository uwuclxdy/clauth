//! Fallback tab — master-detail, mirroring the Config layout. Left: the ordered
//! chain (plus a trailing `+ add` row), cursor = `❯`, `#n` chain position, active
//! member name in orange. Right: the selected member's rotation card — labeled
//! key:value rows (`5h usage` gauge with a threshold tick, `rotate at`
//! threshold stepper, `weekly at` per-account weekly-line override, `weekly
//! gate` + `scoped gate` per-account usage-check toggles, `last resort` +
//! `preferred` toggles, `preferred days` preset cycle + 7-day chip picker,
//! `max spend` ceiling, `remove`) — or,
//! on `+ add`, a candidate picker. Order = priority (reorder with ⇧↑↓). The
//! chain-global wrap-off and spend-budget settings live on the Config tab, not
//! here. Editing happens in place: ⏎ on the left drops focus into the right
//! pane, `+` / `-` step the threshold (or ⏎ on it to type a value), space/⏎
//! flips `last resort`, ⏎ types a `max spend` ceiling, ⏎ on remove arms then
//! confirms. No popups.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use std::collections::HashSet;

use super::super::app::{
    App, CardEdit, ChainItemKind, FALLBACK_ROWS, FallbackFocus, FallbackRow, InputState,
    PREFERRED_DAY_PRESETS, WEEKDAYS_ALL, chain_candidates, chain_items, member_days,
    parse_max_spend, parse_weekly_override, preferred_days_preset,
};
use super::super::theme;
use super::format::{ResetFmt, fixed_split, relative_age, reset_pill, reset_resume};
use super::global_config::default_reminder;
use super::panes::{
    DETAIL_KEY_GUTTER, DETAIL_KEY_W, DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED,
    DIAG_DISABLED, DIAG_KEY_REJECTED, DIAG_KICK, DIAG_NO_USAGE, DIAG_STALE, DIAG_WEEKLY_SOFT,
    DIAG_WEEKLY_SPENT, bold_when, cycle_row_lines, draw_scrolled_lines, draw_selector_list,
    head_cols, help_tooltip_lines, highlight_row, invalid_tooltip_lines, key_cell, label_style,
    master_detail, name_color, pill, rail_hint_lines, section_box, section_box_verbatim,
    select_line, value_caret, wrap_words,
};
use super::prose::{cmd, key};
use crate::fallback::{
    BlockedReason, DEFAULT_THRESHOLD, blocked_reason, health_blocked_reason, parse_threshold,
    soonest_resume, spend_is_uncapped, spend_room, threshold_for, uncapped_spend_fix,
};
use crate::profile::AppConfig;
use crate::usage::{humanize_duration, switch_grade_kick_lifts};
use chrono::Weekday;

/// Wide enough to read a threshold tick.
const GAUGE_W: usize = 22;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (selector, detail) = master_detail(area, chain_items(app).len());
    // The live key-rejected set, read once per frame (never per candidate)
    // before either pane locks the config guard.
    let key_rejected = app.key_rejected_names();

    let chain_focused = app.fallback_focus == FallbackFocus::Chain;
    draw_chain_selector(frame, selector, app, chain_focused, &key_rejected);
    draw_chain_detail(frame, detail, app, &key_rejected);
}

fn draw_chain_selector(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    focused: bool,
    key_rejected: &HashSet<crate::profile::ProfileName>,
) {
    let items = chain_items(app);
    // Switch-grade kick blocks the chip flags — read before the Config lock
    // (rank order: KickBlockState 230 < Config 400).
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    let cfg = app.config();
    let sel = app.chain_cursor.min(items.len().saturating_sub(1));
    draw_selector_list(
        frame,
        area,
        "chain",
        focused,
        sel,
        &app.chain_selector_offset,
        |w| {
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
                                blocked_reason(
                                    &cfg,
                                    p,
                                    kick_lifts.get(name.as_str()).copied(),
                                    key_rejected,
                                )
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
        },
    );
}

fn draw_chain_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    key_rejected: &HashSet<crate::profile::ProfileName>,
) {
    let detail_focused = app.fallback_focus == FallbackFocus::Detail;
    let inner_w = section_box("", detail_focused, false).inner(area).width as usize;
    let items = chain_items(app);
    let selected = items
        .get(app.chain_cursor.min(items.len().saturating_sub(1)))
        .copied();
    // Switch-grade kick blocks, read before the Config lock (rank order:
    // KickBlockState 230 < Config 400).
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    // The card's open edit, drawn only on the member it is pinned to.
    let edit = match selected {
        Some(ChainItemKind::Member(i)) => {
            let cfg = app.config();
            let name = cfg.state.fallback_chain.get(i);
            app.fallback_edit
                .as_ref()
                .filter(|e| name == Some(&e.member))
                .map(|e| &e.state)
        }
        Some(ChainItemKind::Add) | None => None,
    };
    let typed = |row: FallbackRow| edit.filter(|e| e.row() == row).and_then(CardEdit::input);

    // `Add` arm must NOT hold the `config` guard — `add_detail` re-locks it via
    // `chain_candidates`, and the mutex is non-reentrant (deadlock on `+ add` row).
    // `is_name`: member names render in original case; structural titles stay uppercased.
    // `spans` = where each FALLBACK_ROWS row landed, REPORTED BY the function
    // that pushed it. The header block above the rows is variable in two
    // directions (a disabled member stacks a second pill, and every pill drags a
    // wrapped fix line) and the `preferred days` row wraps, and a caret on the
    // wrong row is invisible to every text assertion — so positions are read
    // out of the buffer rather than tracked in a constant edited in lockstep.
    let mut add_focus = (0, 0);
    let (title, is_name, lines, spans): (String, bool, Vec<Line<'static>>, RowSpans) =
        match selected {
            Some(ChainItemKind::Member(i)) => {
                let cfg = app.config();
                let name = cfg.state.fallback_chain.get(i).cloned().unwrap_or_default();
                let kick_lift = kick_lifts.get(name.as_str()).copied();
                let day_picker = match edit {
                    Some(CardEdit::Days(picker)) => Some(picker.cursor),
                    _ => None,
                };
                let day_stop = app
                    .fallback_day_stop
                    .as_ref()
                    .filter(|stop| stop.member == name)
                    .map(|stop| stop.state.as_slice());
                let (lines, spans) = member_detail(
                    &cfg,
                    &name,
                    MemberCard {
                        focused: detail_focused,
                        row_cursor: app.fallback_detail_cursor,
                        armed_remove: matches!(edit, Some(CardEdit::ArmedRemove)),
                        editing: typed(FallbackRow::Threshold),
                        max_spend_editing: typed(FallbackRow::MaxSpend),
                        weekly_editing: typed(FallbackRow::WeeklyAt),
                        day_picker,
                        day_stop,
                        width: inner_w,
                        kick_lift,
                        sessions: app.live_sessions.member(&name),
                    },
                    key_rejected,
                );
                (name.to_string(), true, lines, spans)
            }
            Some(ChainItemKind::Add) => {
                let (lines, focus) = add_detail(app, detail_focused, inner_w, key_rejected);
                add_focus = focus;
                (
                    "add to chain".to_string(),
                    false,
                    lines,
                    std::array::from_fn(|_| 0..0),
                )
            }
            None => (
                "chain".to_string(),
                false,
                empty_detail(),
                std::array::from_fn(|_| 0..0),
            ),
        };

    let block = if is_name {
        section_box_verbatim(&title, detail_focused, false)
    } else {
        section_box(&title, detail_focused, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = inner.height as usize;
    let focus = match selected {
        // The header block + the rows + a tooltip outgrow a short pane (a
        // 40x24 terminal leaves ~14 inner rows, and a blocked member's pill
        // block eats two more), so a FOCUSED card scrolls the cursored row's
        // block (its own lines and its whole tooltip, however far a narrow
        // pane wraps it) into view instead of clipping `max spend`, `remove`
        // or a hint's tail off the bottom. Unfocused keeps the top anchored:
        // while browsing the left chain the identity header is the payload,
        // not the rows.
        Some(ChainItemKind::Member(_)) if detail_focused => {
            let cursor = app.fallback_detail_cursor.min(FALLBACK_ROWS.len() - 1);
            let block = &spans[cursor];
            // The block's first line caps the scroll, so a row taller than the
            // pane (the day picker on a small terminal) keeps its label. The
            // picker's caret line outranks the label when the two cannot share
            // the pane: a space there saves the day under the caret, which
            // must be on screen.
            let caret_line = match edit {
                Some(CardEdit::Days(picker))
                    if FALLBACK_ROWS[cursor] == FallbackRow::PreferredDays =>
                {
                    day_picker_rows(inner_w)
                        .iter()
                        .position(|r| r.contains(&picker.cursor))
                }
                _ => None,
            }
            .map(|k| block.start + k);
            let start = caret_line.map_or(block.start, |line| {
                block.start.max((line + 1).saturating_sub(height))
            });
            (start, block.end)
        }
        Some(ChainItemKind::Add) => add_focus,
        Some(ChainItemKind::Member(_)) | None => (0, 0),
    };
    let scroll = draw_scrolled_lines(
        frame,
        inner,
        lines,
        focus,
        Some(&app.fallback_detail_offset),
    );

    // Position the native terminal cursor for whichever field is being typed,
    // matching the post-draw cursor path the other edit screens use. This is not
    // decoration: `value_caret` renders the buffer with uniform styling and
    // leaves the caret glyph entirely to the cursor set here, so a field that
    // skips this has no visible caret at all. The typed row's first line comes
    // from `spans`, so everything pushed above it, a wrapped `preferred days`
    // row included, is already counted.
    let typing = edit.and_then(|e| {
        // `+ 1` for the leading `$`, which sits before the buffer.
        let unit_cols = usize::from(e.row() == FallbackRow::MaxSpend);
        e.input().map(|draft| (e.row(), draft, unit_cols))
    });

    // The row must actually be ON the pane before its caret is placed. The
    // scroll above chases the CURSORED row, which is the row being typed on
    // every real path — but a degenerate pane (no rows at all) can still leave
    // it off, and an unguarded set parks a visible
    // caret on a border or the pane below, since a real terminal clamps the
    // row rather than dropping it. Mirrors the `config.rs` edit-caret guard.
    if detail_focused
        && let Some(ChainItemKind::Member(_)) = selected
        && let Some((row, draft, unit_cols)) = typing
        && let Some(row_idx) = FALLBACK_ROWS.iter().position(|r| *r == row)
        && let Some(at) = spans
            .get(row_idx)
            .filter(|span| !span.is_empty())
            .map(|span| span.start)
        && at >= scroll
        && at - scroll < inner.height as usize
    {
        // x = the value column + unit + cols before caret.
        let prefix_cols = VALUE_COL + unit_cols + head_cols(draft);
        let cx = inner.x.saturating_add(prefix_cols as u16);
        let cy = inner.y.saturating_add((at - scroll) as u16);
        frame.set_cursor_position((cx, cy));
    }
}

/// 1-cell selector marker for a member's worst blocked reason: color bands the
/// severity, the glyph shape names the reason (the detail pill spells it out in
/// full). Absent when the member has headroom.
///
/// `Disabled` and `Canceled` deliberately SHARE `⊖` and split on hue alone
/// (faint vs danger), a departure from the shape-names-the-state rule: the two
/// co-occur on nearly every real account (an operator disables a subscription
/// once it's canceled), and the Overview account row picks the canceled arm
/// where this ladder picks the disabled one, so distinct shapes made the same
/// account wear two glyphs on one screen.
///
/// `Stale` and `NoUsage` share `⋯` at one hue, the way `AuthBroken` and
/// `KeyRejected` share `×`: the walk treats them alike (neither is ever fresh,
/// so both wait for the pass that accepts any freshness), and the pill says
/// which.
pub(super) fn reason_marker(reason: &BlockedReason) -> Span<'static> {
    let (glyph, style) = match reason {
        BlockedReason::Disabled => ("⊖", theme::faint()),
        BlockedReason::Canceled => ("⊖", theme::danger()),
        BlockedReason::AuthBroken => (theme::dead_credential_glyph(), theme::danger()),
        BlockedReason::KeyRejected => (theme::dead_credential_glyph(), theme::danger()),
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
        BlockedReason::NoUsage => ("⋯", theme::faint()),
    };
    Span::styled(glyph, style)
}

/// Blocked-reason status pill for the detail card: `[ label ]`, label bold in the
/// reason's semantic color (neutral dim for stale), brackets dim. Window resets
/// run through `reset_pill`, so they follow the operator's `reset display`
/// setting; the kick-block lift stays a bare countdown — the limiter relents on
/// its own schedule, so a wall-clock time there would claim a precision the
/// estimate doesn't have.
fn reason_pill_spans(reason: &BlockedReason, fmt: ResetFmt) -> Vec<Span<'static>> {
    // Every pill is `[ label ]` with an optional qualifier trailing as a faint
    // suffix OUTSIDE the brackets (a reset countdown, a lift ETA, "still
    // serving") — never crammed inside with a `·`, so the whole card and the
    // Usage-tab pills read one shape.
    let (label, style, suffix) = match reason {
        BlockedReason::Disabled => (DIAG_DISABLED.to_string(), theme::dim().bold(), None),
        BlockedReason::Canceled => (DIAG_CANCELED.to_string(), theme::danger().bold(), None),
        BlockedReason::AuthBroken => (DIAG_AUTH_BROKEN.to_string(), theme::danger().bold(), None),
        BlockedReason::KeyRejected => (DIAG_KEY_REJECTED.to_string(), theme::danger().bold(), None),
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
        BlockedReason::NoUsage => (DIAG_NO_USAGE.to_string(), theme::dim().bold(), None),
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
        BlockedReason::AuthBroken => {
            format!("re-login with {}", cmd(&format!("clauth login {name}")))
        }
        BlockedReason::KeyRejected => "re-enter the api key on the setup tab".to_string(),
        BlockedReason::WeeklySpent { .. } => "weekly limit is spent".to_string(),
        BlockedReason::KickRejected { .. } => "claude code is refusing to start it".to_string(),
        BlockedReason::BudgetSpent => "raise max spend below".to_string(),
        BlockedReason::FiveHour { .. } => "5h quota is spent, it resets on its own".to_string(),
        BlockedReason::ScopedSpent { .. } => {
            "that model's weekly window is spent, other models still serve".to_string()
        }
        BlockedReason::WeeklySoft { .. } => DIAG_WEEKLY_SOFT.to_string(),
        BlockedReason::Stale => "last usage check failed".to_string(),
        // The split `clauth login --setup-token` describes: an OAuth login
        // beside the setup token gives `/usage` a pair to poll, while sessions
        // keep running on the setup token.
        BlockedReason::NoUsage => format!(
            "add an oauth login with {} so clauth can read its usage",
            cmd(&format!("clauth login {name}"))
        ),
    }
}

/// The member card's live-session block: how many `clauth start` sessions are
/// running as THIS member, plus — only once one of them has actually swapped —
/// when that happened and the caveat that makes the figure honest.
///
/// `live` is the one word every TUI surface counting sessions uses (the
/// Overview column header and the Setup tab's disable gate), so the count
/// reads the same wherever the operator meets it on screen.
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
        Span::styled(
            key_cell("live", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        Span::styled(count, theme::dim()),
    ])];
    let Some(at) = sessions.last_swap_at else {
        return lines;
    };
    // An AGE, so it reads through `relative_age` (single largest unit, local
    // stamp past 30 days) rather than the two-unit `humanize_duration` the countdowns
    // use — a countdown is a duration, this is a point in the past.
    lines.push(Line::from(vec![
        Span::styled(
            key_cell("last swap", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            theme::label(),
        ),
        Span::styled(relative_age(at), theme::dim()),
    ]));
    lines.extend(help_tooltip_lines(
        "picked up on the session's next request",
        width,
    ));
    lines
}

/// The blocked-reason pill block: each pill on its own row with its `└` fix
/// line, connected into one `├│└` rail when 2+ stack. The first row carries
/// the `status` key so the rail has a column to anchor against; later rows
/// bridge with `│` at col 0 while the rail is open.
///
/// Mirrors `usage.rs::status_lines`'s shape but keys off THIS card's `DETAIL_KEY_W`,
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
            Span::styled(
                key_cell("status", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                theme::label(),
            )
        } else {
            Span::styled(
                format!("│{}", " ".repeat(DETAIL_KEY_W + DETAIL_KEY_GUTTER - 1)),
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
    /// The chip caret while the day picker is open on this member.
    day_picker: Option<usize>,
    /// The custom day list the open card remembers as a cycle stop.
    day_stop: Option<&'a [Weekday]>,
    width: usize,
    kick_lift: Option<i64>,
    sessions: crate::live_sessions::MemberSessions,
}

/// The lines each `FALLBACK_ROWS` row occupies in [`member_detail`]'s output,
/// its own lines plus the tooltip under it, indexed like `FALLBACK_ROWS`: the
/// block the card's scroll keeps on screen, whose first line is the row's own.
type RowSpans = [std::ops::Range<usize>; FALLBACK_ROWS.len()];

/// Live-session count, 5h gauge with threshold tick, headroom figure, and the
/// inline `rotate at` threshold stepper/editor + `last resort` toggle + `remove` rows.
/// Caret only when focused.
fn member_detail(
    cfg: &AppConfig,
    name: &crate::profile::ProfileName,
    card: MemberCard<'_>,
    key_rejected: &HashSet<crate::profile::ProfileName>,
) -> (Vec<Line<'static>>, RowSpans) {
    let MemberCard {
        focused,
        row_cursor,
        armed_remove,
        editing,
        max_spend_editing,
        weekly_editing,
        day_picker,
        day_stop,
        width,
        kick_lift,
        sessions,
    } = card;
    let Some(profile) = cfg.find(name) else {
        return (
            vec![Line::from(Span::styled(
                "account no longer exists · remove it from the chain",
                theme::danger(),
            ))],
            std::array::from_fn(|_| 0..0),
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
    if let Some(reason) = blocked_reason(cfg, profile, kick_lift, key_rejected) {
        pills.push((reason_pill_spans(&reason, fmt), reason_fix(&reason, name)));
        if reason == BlockedReason::Disabled
            && let Some(health) = health_blocked_reason(cfg, profile, kick_lift, key_rejected)
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

    // `5h usage` — gauge lives on the kv key row (matching the `rotate at`
    // grammar), headroom figure indented beneath it. Two lines, not three:
    // the standalone eyebrow is folded into the key. The gauge takes what the
    // key and the figure leave, up to `GAUGE_W`, so on a narrow pane the bar
    // gives and the figure reads whole.
    let (figure, figure_style) = match pct {
        Some(v) => (format!("  {v:.0}% used"), theme::util(v)),
        None => ("  no data yet".to_string(), theme::faint()),
    };
    let gauge_w = width
        .saturating_sub(DETAIL_KEY_W + DETAIL_KEY_GUTTER + figure.chars().count())
        .clamp(4, GAUGE_W);
    let mut gauge_spans = vec![Span::styled(
        key_cell("5h usage", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
        theme::label(),
    )];
    gauge_spans.extend(gauge_with_tick(pct, Some(threshold), gauge_w));
    gauge_spans.push(Span::styled(figure, figure_style));
    lines.push(Line::from(gauge_spans));

    // No 5h reading means no headroom figure — an empty continuation line here
    // read as a deliberate gap on exactly the accounts that had nothing to say.
    if let Some(v) = pct {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(DETAIL_KEY_W + DETAIL_KEY_GUTTER)),
            Span::styled(
                format!("{:.0}% until rotate", (threshold - v).max(0.0)),
                theme::faint(),
            ),
        ]));
    }
    lines.push(Line::from(""));

    // Where each FALLBACK_ROWS row landed, taken from the buffer itself rather
    // than from a hand-maintained count. `draw_chain_detail` places the native
    // caret and chases the cursored row with these, and a caret on the wrong row
    // is invisible to every text assertion — so the positions are READ from what
    // was actually pushed. A header block of variable height and a wrapping
    // `preferred days` row both move the rows beneath them.
    let mut spans: RowSpans = std::array::from_fn(|_| 0..0);

    for (i, row) in FALLBACK_ROWS.iter().enumerate() {
        let selected = focused && i == cursor;
        let row_editing = match *row {
            FallbackRow::Threshold => editing,
            FallbackRow::MaxSpend => max_spend_editing,
            FallbackRow::WeeklyAt => weekly_editing,
            _ => None,
        };
        let picking = day_picker.filter(|_| *row == FallbackRow::PreferredDays);
        let row_lines = detail_row(
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
                preferred_days: &profile.preferred_days,
                day_picker: picking,
                day_stop,
                max_spend: profile.max_auto_spend.unwrap_or(0.0),
                spend_budget: cfg.state.spend_budget_switching,
                armed_remove,
            },
            row_editing,
            width,
        );
        let start = lines.len();
        lines.extend(row_lines.into_iter().map(|line| {
            if selected {
                highlight_row(line, width)
            } else {
                line
            }
        }));
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
                &preferred_hint(cfg, name, profile.preferred, key_rejected),
                width,
            ));
        }
        // At rest, what the list does and that ⏎ picks single days, blocker
        // first like `Disabled`: a list that cannot claim is the one fact worth
        // saying before space starts cycling presets. It says nothing of the
        // days it leaves alone, since another member's list can claim those
        // ahead of `preferred`; the `preferred` hint above covers them. A day
        // another list names too goes to whichever claimant reads clear first
        // (`is_home_on` reads the claimant set), so that arm says so instead of
        // promising this account the day. Descended, the picker's grammar.
        if *row == FallbackRow::PreferredDays && selected {
            let shared = shared_days(cfg, name, &profile.preferred_days, key_rejected);
            let hint = match (
                picking,
                crate::fallback::day_claim_blocker(cfg, name, key_rejected),
            ) {
                (Some(_), _) => format!(
                    "{} walk · {} toggles and saves · {} done · {} leave",
                    key("← →"),
                    key("space"),
                    key("↵ esc"),
                    key("↑ ↓")
                ),
                (None, Some(reason)) => format!(
                    "a day list here would claim nothing: {reason} · {} picks days one by one",
                    key("↵")
                ),
                (None, None) if !shared.is_empty() => format!(
                    "another list also names {}: work returns to whichever account reads clear \
                     first · {} picks days one by one",
                    day_list_label(&shared),
                    key("↵")
                ),
                (None, None) => format!(
                    "work returns to this account on the days set here · {} picks days one by one",
                    key("↵")
                ),
            };
            lines.extend(help_tooltip_lines(&hint, width));
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
                None if spend_is_uncapped(cfg, ceiling, key_rejected) => {
                    lines.extend(invalid_tooltip_lines(
                        &format!("nothing stops the spending: {}", uncapped_spend_fix()),
                        width,
                    ))
                }
                None if selected => lines.extend(help_tooltip_lines(
                    &max_spend_hint(cfg, name, ceiling),
                    width,
                )),
                None => {}
            }
        }
        spans[i] = start..lines.len();
    }

    // All-exhausted sibling of the Overview projection line: when EVERY chain
    // member is currently maxed, name whichever one resumes first instead of
    // leaving the recovery implicit (issue #10 follow-up). Chain-wide, so it
    // renders under whichever member happens to be selected.
    if let Some((resume_name, eta)) = soonest_resume(cfg, key_rejected) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "resumes: {resume_name} {}",
                reset_resume(eta, ResetFmt::from_state(&cfg.state))
            ),
            theme::faint(),
        )));
    }
    (lines, spans)
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
fn preferred_hint(
    cfg: &AppConfig,
    name: &crate::profile::ProfileName,
    on: bool,
    key_rejected: &HashSet<crate::profile::ProfileName>,
) -> String {
    // A day list decides the days it names, for every account — so the hint
    // reads off the lists before the toggle, on both sides of one. A list here
    // does not answer for the days no list claims, so the toggle still speaks
    // there and the hint has to say both halves; with a list elsewhere the
    // toggle holds only on the days that list leaves alone.
    // A list this account could not serve claims nothing, so the branches below
    // answer instead — the same eligibility the `claimed_elsewhere` scan applies
    // to the other side. This card's own `preferred days` row names the reason,
    // so repeating it here would only say the same thing twice.
    if let Some(days) = cfg
        .find(name)
        .map(|p| p.preferred_days.clone())
        .filter(|d| !d.is_empty())
        .filter(|_| crate::fallback::day_claim_blocker(cfg, name, key_rejected).is_none())
    {
        let named = crate::profile::render_preferred_days(&days).join(", ");
        // The toggle's half names the days it really decides: "the rest" would
        // be false the moment another member's list names one of them.
        return if on {
            format!(
                "home on {named} by its preferred days, and on days no list claims by this toggle"
            )
        } else {
            format!("home on {named} by its preferred days")
        };
    }
    // Only a list that could actually serve stands the toggle down, matching
    // the claim scan in `is_home_on`.
    let claimed_elsewhere = cfg.state.fallback_chain.iter().any(|n| {
        n != name
            && !crate::fallback::walk_excluded(cfg, n, key_rejected)
            && cfg.find(n).is_some_and(|p| !p.preferred_days.is_empty())
    });
    if on {
        return if claimed_elsewhere {
            "work returns to this account on the days no day list claims".to_string()
        } else {
            "work returns to this account once it's free again".to_string()
        };
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
struct MemberRow<'a> {
    threshold: f64,
    weekly_override: Option<f64>,
    weekly_default: f64,
    check_weekly: bool,
    check_scoped: bool,
    last_resort: bool,
    preferred: bool,
    preferred_days: &'a [Weekday],
    /// The chip caret while the day picker is open on this member.
    day_picker: Option<usize>,
    /// The custom day list the open card remembers as a cycle stop.
    day_stop: Option<&'a [Weekday]>,
    max_spend: f64,
    spend_budget: bool,
    armed_remove: bool,
}

/// One FALLBACK_ROWS row's own lines, tooltip excluded: one line, except the
/// `preferred days` row, which wraps at rest and while its picker is open.
fn detail_row(
    row: FallbackRow,
    selected: bool,
    values: MemberRow<'_>,
    editing: Option<&InputState>,
    width: usize,
) -> Vec<Line<'static>> {
    let MemberRow {
        threshold,
        weekly_override,
        weekly_default,
        check_weekly,
        check_scoped,
        last_resort,
        preferred,
        preferred_days,
        day_picker,
        day_stop,
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
    let line = match row {
        FallbackRow::Threshold => {
            let mut spans = vec![
                arrow,
                Span::styled(
                    key_cell("rotate at", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
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
                Span::styled(
                    key_cell("weekly at", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                    key_style,
                ),
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
                Span::styled(
                    key_cell(key, DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                    label_style(selected),
                ),
                Span::styled(value, style),
            ])
        }
        FallbackRow::PreferredDays => {
            return match day_picker {
                Some(caret) => day_picker_lines(preferred_days, caret, width),
                None => day_list_lines(arrow, selected, preferred_days, day_stop, width),
            };
        }
        FallbackRow::MaxSpend => {
            // Inert until the chain-wide `spend budget` is on: render the whole row
            // faint (a disabled row) so a ceiling never reads as armed
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
                Span::styled(
                    key_cell("max spend", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                    key_style,
                ),
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
                Span::styled(
                    key_cell("remove", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
                    label_style(selected),
                ),
                Span::styled(label, theme::danger()),
            ])
        }
    };
    vec![line]
}

/// Where a value opens on the card: past the 2-cell gutter and the key column.
const VALUE_COL: usize = 2 + DETAIL_KEY_W + DETAIL_KEY_GUTTER;

/// The days in `days` another chain member's list also claims, in
/// `WEEKDAYS_ALL` order: on those, work returns to whichever claimant reads
/// clear first rather than to `name`.
fn shared_days(
    cfg: &AppConfig,
    name: &crate::profile::ProfileName,
    days: &[Weekday],
    key_rejected: &HashSet<crate::profile::ProfileName>,
) -> Vec<Weekday> {
    WEEKDAYS_ALL
        .into_iter()
        .filter(|day| days.contains(day) && cfg.day_listers(*day, key_rejected).any(|n| n != name))
        .collect()
}

/// A day list as the card names it: the preset rung's own name, or the custom
/// set in its canonical spelling.
fn day_list_label(days: &[Weekday]) -> String {
    match preferred_days_preset(days) {
        Some(i) => PREFERRED_DAY_PRESETS[i].0.to_string(),
        None => crate::profile::render_preferred_days(days).join(", "),
    }
}

/// The `preferred days` row at rest: the house cycle row over
/// [`PREFERRED_DAY_PRESETS`], `never` a live rung like the rest. A custom set
/// trails the run spelled as the file writes it: the row's value while the
/// member holds it, else the stop the open card remembers stepping past, which
/// the cycle comes back to.
fn day_list_lines(
    arrow: Span<'static>,
    selected: bool,
    days: &[Weekday],
    stop: Option<&[Weekday]>,
    width: usize,
) -> Vec<Line<'static>> {
    let held = preferred_days_preset(days);
    let options: Vec<(&str, bool)> = PREFERRED_DAY_PRESETS
        .iter()
        .enumerate()
        .map(|(i, (label, _))| (*label, held == Some(i)))
        .collect();
    let custom = match held {
        None => Some((days, true)),
        Some(_) => stop.map(|stop| (stop, false)),
    }
    .map(|(set, active)| {
        (
            crate::profile::render_preferred_days(set).join(", "),
            active,
        )
    });
    let lead = vec![
        arrow,
        Span::styled(
            key_cell("preferred days", DETAIL_KEY_W, DETAIL_KEY_GUTTER),
            label_style(selected),
        ),
    ];
    cycle_row_lines(
        lead,
        &options,
        custom
            .as_ref()
            .map(|(value, active)| (value.as_str(), *active)),
        None,
        selected,
        width,
    )
}

/// Where the picker's first caret slot sits on every line: past the gutter and
/// the key column, in the key gutter's last cell, so each chip's mark opens on
/// the card's value column.
const PICKER_LEAD: usize = VALUE_COL - 1;
/// Blank cells between one chip and the next chip's caret slot.
const CHIP_GAP: usize = 2;

/// One chip's cells: the caret slot, the `[x]` mark, the day's name.
fn chip_width(name: &str) -> usize {
    1 + 3 + name.chars().count()
}

/// The `WEEKDAYS_ALL` indices each picker line holds at `width`: filled
/// greedily, broken between chips and never inside one, at least one chip a
/// line however narrow the pane.
fn day_picker_rows(width: usize) -> Vec<std::ops::Range<usize>> {
    let names = crate::profile::render_preferred_days(&WEEKDAYS_ALL);
    let mut rows = Vec::new();
    let (mut start, mut used) = (0, PICKER_LEAD);
    for (i, name) in names.iter().enumerate() {
        let chip = chip_width(name);
        if i > start && used + CHIP_GAP + chip > width {
            rows.push(start..i);
            (start, used) = (i, PICKER_LEAD + chip);
        } else {
            used += if i > start { CHIP_GAP + chip } else { chip };
        }
    }
    rows.push(start..names.len());
    rows
}

/// The `preferred days` row descended (a multi-select chip row): `✎`
/// in the gutter, then one `[x]`/`[ ]` chip per weekday, picked as the member's
/// saved `days` say. Every chip reserves a 1-cell caret slot, `❯` on the chip
/// under `caret`, so nothing shifts as it walks; lines past the first indent to
/// [`PICKER_LEAD`] so their marks sit under the first line's.
fn day_picker_lines(days: &[Weekday], caret: usize, width: usize) -> Vec<Line<'static>> {
    let names = crate::profile::render_preferred_days(&WEEKDAYS_ALL);
    day_picker_rows(width)
        .into_iter()
        .enumerate()
        .map(|(line_no, row)| {
            let mut spans = if line_no == 0 {
                vec![
                    Span::styled(format!("{} ", theme::edit_glyph()), theme::accent().bold()),
                    Span::styled(
                        key_cell("preferred days", DETAIL_KEY_W, DETAIL_KEY_GUTTER - 1),
                        label_style(true),
                    ),
                ]
            } else {
                vec![Span::raw(" ".repeat(PICKER_LEAD))]
            };
            for i in row.clone() {
                if i > row.start {
                    spans.push(Span::raw(" ".repeat(CHIP_GAP)));
                }
                let picked = days.contains(&WEEKDAYS_ALL[i]);
                spans.push(if i == caret {
                    Span::styled("❯", theme::accent().bold())
                } else {
                    Span::raw(" ")
                });
                spans.push(Span::styled("[", theme::dim()));
                spans.push(if picked {
                    Span::styled("x", theme::accent())
                } else {
                    Span::raw(" ")
                });
                spans.push(Span::styled("]", theme::dim()));
                spans.push(Span::styled(
                    names[i].clone(),
                    if picked {
                        theme::accent()
                    } else {
                        theme::faint()
                    },
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// The `+ add` pane's lines, and the block the view keeps on screen: the
/// candidate under the cursor with the note it carries.
fn add_detail(
    app: &App,
    focused: bool,
    width: usize,
    key_rejected: &HashSet<crate::profile::ProfileName>,
) -> (Vec<Line<'static>>, (usize, usize)) {
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
        return (lines, (0, 0));
    }

    if !focused {
        return (lines, (0, 0));
    }

    let cursor = app
        .fallback_detail_cursor
        .min(candidates.len().saturating_sub(1));
    let mut focus = (0, 0);
    for (i, name) in candidates.iter().enumerate() {
        let selected = i == cursor;
        if selected {
            focus.0 = lines.len();
        }
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
        // A member taken off the chain keeps its day list, and adding it back
        // re-arms that list, so the pick names it before the add does. Blocker
        // first, like the card's own row: a list that cannot claim promises
        // nothing.
        let member = crate::profile::ProfileName::from(name.as_str());
        if selected
            && let Some(days) = member_days(app, &member)
            && !days.is_empty()
        {
            let label = day_list_label(&days);
            let blocker = crate::fallback::walk_blocker(&app.config(), &member, key_rejected);
            let hint = match blocker {
                Some(reason) => format!("its day list ({label}) would claim nothing: {reason}"),
                None => format!("brings back its preferred days: {label}"),
            };
            lines.extend(help_tooltip_lines(&hint, width));
        }
        if selected {
            focus.1 = lines.len();
        }
    }
    (lines, focus)
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

/// `width`-cell usage bar: fill colored by the usage thresholds (via
/// `util_color`), with a `│` tick at the rotate threshold. Once the fill reaches
/// or passes the tick column, the tick is drawn `│` in `DANGER` over the fill so
/// the "over limit" marker is never occluded.
fn gauge_with_tick(pct: Option<f64>, threshold: Option<f64>, width: usize) -> Vec<Span<'static>> {
    let value = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let fill = ((value / 100.0) * width as f64).round() as usize;
    let fill = fill.min(width);
    let tick = threshold.map(|t| {
        (((t.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize)
            .min(width.saturating_sub(1))
    });
    let fill_style = match pct {
        Some(v) => theme::util(v),
        None => theme::faint(),
    };

    let mut spans = vec![];
    for i in 0..width {
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
