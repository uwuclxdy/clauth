//! Usage tab — account picker on the left, the selected account's full usage
//! breakdown on the right: a header (plan, active marker, per-account refresh
//! status / countdown), then every window, reset timers, and extra credits.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, MainItemKind, NoteBuffer, ReadingsSnapshot, SetupSelection, StatusFocus, escape_control,
    pool_auth_label,
};
use super::super::theme;
use super::format::{
    NO_DATA, ResetFmt, activity_verb, duration_label, is_past_reset, relative_age,
    reset_in_secs_at, reset_phrase, spinner_frame, spinner_style, truncate,
};
use super::panes::{
    DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED, DIAG_DISABLED, DIAG_KICK, QueueView,
    cells_window, draw_scrollbar, edit_slot_block, empty_state, follow_window,
    grapheme_cells_before, key_cell, master_detail, pill, place_caret, rail_hint_lines,
    scroll_offset, section_box, section_box_verbatim, wrap_words,
};
use super::prose::{self, cmd, cmd_lit, key_lit};
use crate::format::{account_tier, format_pct};
use crate::profile::Profile;
use crate::providers::{Provider, StatRow, StatRowKind, StatRowRaw};
use crate::usage::{
    ExtraPeriod, FetchStatus, KickBlock, ProfileActivity, QueueSlot, StreakCounts, UsageWindow,
    WindowDollars, humanize_duration, ideal_pace_pct, is_stuck_streak, kick_block_switch_grade,
    now_epoch_secs, now_ms, queue_anchor_cached, selected_next_refresh, switch_grade_kick_lifts,
};

const KEY_W: usize = 8;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;
/// The fix for a third-party profile no leg will ever fetch — the status
/// block's `[ no key ]` hint and the empty body share these exact words.
const KEYLESS_FIX: &str = "no api key set, add one on the setup tab";

/// Config-derived diagnostic flags for the shown profile, gathered under the
/// config guard in [`draw_usage_detail`] so [`status_lines`] stays lock-free.
/// Each maps to a `└` fix hint (see [`diag_fix`]); render-only, no decision
/// consumes them — the same invariant `fallback::blocked_reason` holds.
#[derive(Clone, Copy, Default)]
struct DiagFlags {
    /// AUTH-1 quarantine (`AppConfig::is_auth_broken`).
    auth_broken: bool,
    /// Opted into auto-start — flips the kick-block fix: an auto_start account
    /// self-recovers (7bbeae4 re-tests each poll on a live window), a manual one
    /// never re-tests and must be toggled on.
    auto_start: bool,
    /// 7d window at/over the hard cap (`fallback::weekly_blocked` at
    /// `WEEKLY_HARD_BLOCK_PCT`).
    weekly_hard: bool,
    /// Billing member out of free 5h quota AND over its `max_auto_spend` budget
    /// (`fallback::budget_spent_blocking` — gated on 5h-exhaustion exactly like
    /// `blocked_reason`, so the hint never claims a block the engine skips).
    budget_spent: bool,
    /// Armed to spend with nothing bounding it (`fallback::spend_is_uncapped`) —
    /// the DANGER config warning. Outranks `budget_spent` when both hold.
    spend_uncapped: bool,
}

/// Runtime state gathered once under locks; keeps line builders lock-free.
struct HeaderState {
    activity: ProfileActivity,
    next_refresh_ms: Option<u64>,
    tick: u64,
    /// Consecutive-failure counts for the shown profile (zeroed when absent).
    /// The retry suffix names which retry the countdown leads to, so a deep slot
    /// reads as stuck from the count alone, no judgment label.
    streaks: StreakCounts,
    /// The reading source behind `streaks.rate_limit` — OAuth-first, so the
    /// stuck hint names the endpoint that actually throttled, never the row's
    /// provider.
    rate_limit_source: RateLimitSource,
    /// Live kick-429 block for the shown profile: the messages endpoint is
    /// rejecting the 5h auto-start kick. Orthogonal to `fetch_status` — `/usage`
    /// can stay Fresh straight through the outage — so it earns its own pill.
    kick_block: Option<KickBlock>,
    /// Config-derived diagnostic flags driving the `└` fix hints.
    diag: DiagFlags,
    /// The shown profile's peak-rate state, sampled now off the provider's
    /// own price-store rows. `None` = no store-backed provider or flat rates
    /// — no `pricing` row renders at all.
    peak: Option<crate::pricing::PeakState>,
    /// The shown profile's auto-start queue slot, resolved before the Config
    /// guard (rank order) like the chain card used to; `None` when the queue
    /// toggle is off or the profile holds no slot. The `usage auto-start`
    /// countdown on the `plan` row combines the slot's shared gate estimate
    /// with the profile's own window reset (the LATER wins — see `kick_text`),
    /// and reads the reset alone when this is `None`.
    queue_slot: Option<QueueSlot>,
}

pub(super) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    readings: &ReadingsSnapshot,
    toast_rects: &[Rect],
) {
    let items = app.setup_items().len();
    let (selector, detail) = master_detail(area, items);

    // The shared band-list selector (Setup's shape, no `+ new` row); the
    // selection is held by identity and resolved to a flat index each frame.
    super::config::draw_band_selector(
        frame,
        selector,
        app,
        app.usage_detail_focus == StatusFocus::List,
        app.usage_position(),
        false,
        &app.usage_selector,
    );

    match app.usage_selection() {
        SetupSelection::Item(MainItemKind::Profile(_)) | SetupSelection::New => {
            draw_usage_detail(frame, detail, app, readings, toast_rects);
        }
        SetupSelection::Item(MainItemKind::Codex(_)) => draw_codex_usage_detail(frame, detail, app),
        SetupSelection::Item(MainItemKind::Shunt(..)) => {
            draw_pool_usage_detail(frame, detail, app);
        }
    }
}

/// Which reading source the shown profile's stuck judgment took — the one that
/// actually won, never the row's shape. The stuck hint must name THIS endpoint
/// (a hybrid whose OAuth leg is throttled reads `anthropic`, not its provider).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RateLimitSource {
    OAuth,
    ThirdParty,
}

/// The shown profile's effective consecutive-429 streak for the stuck judgment,
/// plus the reading source it came from: OAuth-first — a member with an OAuth
/// status entry reads the OAuth `rate_limit` axis, else the third-party streak —
/// the same rule `reading_is_actionable` applies. Pure so the fallback is
/// unit-testable.
fn effective_rate_limit(
    name: &crate::profile::ProfileName,
    oauth: &HashMap<String, StreakCounts>,
    third_party: &HashMap<String, u32>,
    oauth_status_has: &std::collections::HashSet<String>,
) -> (u32, RateLimitSource) {
    if oauth_status_has.contains(name.as_str()) {
        (
            oauth.get(name.as_str()).map(|s| s.rate_limit).unwrap_or(0),
            RateLimitSource::OAuth,
        )
    } else {
        (
            third_party.get(name.as_str()).copied().unwrap_or(0),
            RateLimitSource::ThirdParty,
        )
    }
}

fn draw_usage_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    readings: &ReadingsSnapshot,
    toast_rects: &[Rect],
) {
    // Streak snapshot up front: POLL_STREAK (220) ranks below CONFIG
    // (400), so it can't be taken while `cfg` is held below.
    let streaks: HashMap<String, StreakCounts> = app
        .poll_streaks
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default();
    // The third-party consecutive-429 streak + the OAuth status store: the stuck
    // judgment below mirrors the decision predicate's OAuth-first fallback, so a
    // member with no OAuth status entry reads its third-party streak. Both ranks
    // (ThirdPartyStreak 290, UsageStatus 350) sit below CONFIG (400).
    let third_party_streaks: HashMap<String, u32> = app
        .third_party_streaks
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default();
    let oauth_status_has: std::collections::HashSet<String> = app
        .usage_status
        .lock()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    // Same discipline as streaks: KickBlockState (230) ranks below CONFIG (400).
    let kick_blocks: HashMap<String, KickBlock> = app
        .kick_blocks
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default();
    // The queue anchor + the switch-grade lift set, both read before the Config
    // lock (AutoStartQueue 240 and KickBlockState 230 < Config 400). The anchor
    // is the CACHED one: `queue_anchor` would replay per-profile history files,
    // which a render pass must not.
    let queue_anchor = queue_anchor_cached(&app.auto_start_queue);
    let kick_lifts = switch_grade_kick_lifts(&app.kick_blocks);
    // The live key-rejected set, read once before the Config lock (both stores
    // rank below it) — the spend-uncapped check under the guard never re-reads
    // the durable verdict per member.
    let key_rejected = app.key_rejected_names();
    let cfg = app.config();
    let profile = cfg.profiles.get(
        app.profile_cursor
            .read()
            .min(cfg.profiles.len().saturating_sub(1)),
    );

    let title = profile.map(|p| p.name.as_str()).unwrap_or("usage");
    let focused = app.usage_detail_focus == StatusFocus::Detail;
    let block = if profile.is_some() {
        section_box_verbatim(title, focused, false, area.width)
    } else {
        section_box(title, focused, false, area.width)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(profile) = profile else {
        app.usage_detail_max_scroll.set(0);
        app.usage_detail_viewport.set(0);
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    };

    // `config` (via `cfg`) is outer of activity/refresh-timer in lock order.
    let (effective_streak, rate_limit_source) = effective_rate_limit(
        &profile.name,
        &streaks,
        &third_party_streaks,
        &oauth_status_has,
    );
    let header = HeaderState {
        activity: app
            .activity
            .lock()
            .ok()
            .map(|activity| crate::usage::selected_activity(&activity, profile))
            .unwrap_or(ProfileActivity::Idle),
        next_refresh_ms: app
            .next_refresh_per_profile
            .lock()
            .ok()
            .and_then(|m| selected_next_refresh(&m, profile)),
        tick: app.tick_count,
        streaks: {
            let mut s = streaks
                .get(profile.name.as_str())
                .copied()
                .unwrap_or_default();
            s.rate_limit = effective_streak;
            s
        },
        rate_limit_source,
        kick_block: kick_blocks.get(profile.name.as_str()).copied(),
        // Config-dependent predicates, computed under the live config guard so
        // the lock-free line builders below just read booleans. Reuses the
        // fallback engine's own predicates (never a second opinion), the same
        // reason `blocked_reason` reads the walk's.
        diag: {
            let ceiling = profile.max_auto_spend.unwrap_or(0.0);
            DiagFlags {
                auth_broken: cfg.is_auth_broken(&profile.name),
                auto_start: profile.auto_start,
                weekly_hard: crate::fallback::weekly_hard_blocked(readings.usage_of(&profile.name)),
                budget_spent: crate::fallback::budget_spent_blocking(
                    &cfg,
                    profile,
                    readings.usage_of(&profile.name),
                ),
                spend_uncapped: crate::fallback::spend_is_uncapped(
                    &cfg,
                    ceiling,
                    &key_rejected,
                    |n| crate::fallback::is_canceled_from_usage(n, &readings.usage),
                ),
            }
        },
        queue_slot: QueueView::new(&cfg, &kick_lifts, queue_anchor, |n| {
            crate::fallback::is_canceled_from_usage(n, &readings.oauth)
        })
        .slot(&profile.name),
        peak: app.peak_state_for(profile),
    };

    let show_estimates = cfg.state.show_estimates;
    let show_pace = cfg.state.show_pace;
    // Read off the guard already held here: `config` is a plain (non-reentrant)
    // mutex, so a second `app.config()` deeper in the render would self-deadlock.
    let reset_fmt = ResetFmt::from_state(&cfg.state);
    // The note editor docks at the pane's bottom (the contract's multi-line
    // input slot): content above, the auto-grow draft below.
    let slot = app
        .note_editor
        .as_ref()
        .filter(|e| e.profile.as_str() == profile.name.as_str());
    let (content_area, slot_area) = match slot {
        Some(editor) => {
            let slot_h =
                (editor.buf.row_count().min(NOTE_SLOT_MAX_ROWS) as u16 + 2).min(inner.height);
            let content_h = inner.height.saturating_sub(slot_h);
            (
                Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: content_h,
                },
                Some(Rect {
                    x: inner.x,
                    y: inner.y + content_h,
                    width: inner.width,
                    height: slot_h,
                }),
            )
        }
        None => (inner, None),
    };
    let lines = build_usage_lines(
        profile,
        inner.width,
        &header,
        app,
        readings,
        show_estimates,
        show_pace,
        reset_fmt,
    );
    let total = lines.len();
    let viewport = content_area.height as usize;
    let max_scroll = if viewport == 0 {
        0
    } else {
        total.saturating_sub(viewport)
    };
    app.usage_detail_max_scroll.set(max_scroll);
    app.usage_detail_viewport.set(viewport);
    let scroll = app.usage_detail_scroll.min(max_scroll);
    // Paragraph::scroll accepts u16; large saved notes can exceed that row limit.
    let paragraph = if scroll > usize::from(u16::MAX) {
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll)
                .take(viewport)
                .collect::<Vec<_>>(),
        )
    } else {
        Paragraph::new(lines).scroll((scroll as u16, 0))
    };
    frame.render_widget(paragraph.style(theme::base()), content_area);
    draw_scrollbar(frame, content_area, total, scroll, viewport);
    if let Some(editor) = slot {
        draw_note_slot(frame, slot_area.unwrap_or(inner), &editor.buf, toast_rects);
    }
}

/// The 5h / 7d(/ 7d fable) window bars both the codex and pool panes render:
/// one trailing reset-countdown + `make_window_stat` per window, with both
/// gates off. The panes differ only in which windows they pass, so the loop is
/// one helper both call.
fn window_stats(
    windows: &[(&str, Option<&UsageWindow>)],
    reset_fmt: ResetFmt,
    now: i64,
) -> Vec<Stat> {
    let mut stats: Vec<Stat> = Vec::new();
    for (label, window) in windows {
        let Some(window) = window else {
            continue;
        };
        let trailing = reset_in_secs_at(window, now)
            .map(|secs| format!("  {}", reset_phrase(secs, reset_fmt)))
            .unwrap_or_default();
        stats.push(make_window_stat(
            label,
            window.utilization,
            window.resets_at.as_deref(),
            now,
            String::new(),
            trailing,
            WindowGates {
                show_estimates: false,
                show_pace: false,
            },
        ));
    }
    stats
}

/// The Usage codex pane: `type`/`plan` (and `login` while quarantined, then
/// `resets` while one is banked)/`status` facts, then the 5h and 7d bars. No
/// notes, no speed/rate and no burn rate — the design's codex rows.
fn draw_codex_usage_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(i) = app.usage_codex_index() else {
        return;
    };
    let Some(row) = app.codex_rows.get(i) else {
        return;
    };
    let focused = app.usage_detail_focus == StatusFocus::Detail;
    let block = section_box_verbatim(row.name.as_ref(), focused, false, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        key_span("type"),
        Span::styled("codex", theme::accent()),
    ]));
    lines.push(Line::from(vec![
        key_span("plan"),
        match row.plan.as_deref() {
            Some(plan) => Span::styled(escape_control(plan), theme::accent()),
            None => Span::styled(NO_DATA.to_string(), theme::faint()),
        },
    ]));
    if crate::codex_auth::read_quarantine(row.name.as_str()).is_some() {
        lines.push(Line::from(
            [
                vec![key_span("login")],
                pill("needs re-login".to_string(), theme::danger().bold()),
            ]
            .concat(),
        ));
    }
    if let Some(resets) = row.resets.filter(|n| *n > 0) {
        lines.push(Line::from(vec![
            key_span("resets"),
            Span::styled(format!("↺ {resets} banked"), theme::accent()),
        ]));
    }
    lines.push(Line::from(vec![
        key_span("status"),
        match row.fetched_at {
            Some(stamp) => Span::styled(format!("◌ read {}", relative_age(stamp)), theme::dim()),
            None => Span::styled("◌ not read yet", theme::dim()),
        },
    ]));
    lines.push(Line::from(""));

    let reset_fmt = ResetFmt::from_state(&app.config().state);
    let now = now_epoch_secs();
    let stats = window_stats(
        &[
            ("5h", row.five_hour.as_ref()),
            ("7d", row.seven_day.as_ref()),
        ],
        reset_fmt,
        now,
    );
    if !stats.is_empty() {
        lines.extend(render_stat_block(&stats, inner.width));
    }
    render_usage_lines(frame, inner, app, lines);
}

/// The Usage pool pane: `type`/`family`/`provider`/`plan`/`status`/`read`
/// facts, then the 5h, 7d and `7d fable` bars from the pool account's own
/// admin row. No notes, no speed/rate and no burn rate — the design's pool
/// rows.
fn draw_pool_usage_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some((pi, ai)) = app.usage_pool_pair() else {
        return;
    };
    let Some((account, auth)) = app.shunt_pool_row(pi, ai) else {
        return;
    };
    let focused = app.usage_detail_focus == StatusFocus::Detail;
    let block = section_box_verbatim(&escape_control(&account.name), focused, false, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        key_span("type"),
        Span::styled("shunt", theme::accent()),
    ]));
    lines.push(Line::from(vec![
        key_span("family"),
        Span::styled(pool_auth_label(auth), theme::accent()),
    ]));
    let providers = app.pool_providers_for(auth, &account.name);
    if !providers.is_empty() {
        let value_width = inner.width.saturating_sub((KEY_W + KEY_GUTTER) as u16) as usize;
        lines.push(Line::from(vec![
            key_span("provider"),
            Span::styled(
                truncate(&providers.join(" · "), value_width),
                theme::accent(),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        key_span("plan"),
        match account.plan.as_deref() {
            Some(plan) => Span::styled(escape_control(plan), theme::accent()),
            None => Span::styled(NO_DATA.to_string(), theme::faint()),
        },
    ]));
    if let Some((label, style, countdown)) = super::config::pool_status_pill(account) {
        let mut spans = vec![key_span("status")];
        spans.extend(pill(label.to_string(), style));
        if let Some(secs) = countdown {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(duration_label(secs), theme::faint()));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        key_span("read"),
        match app.shunt_pool_read_at_ms {
            Some(stamp) => Span::styled(relative_age(stamp), theme::dim()),
            None => Span::styled("not read yet", theme::dim()),
        },
    ]));
    lines.push(Line::from(""));

    let reset_fmt = ResetFmt::from_state(&app.config().state);
    let now = now_epoch_secs();
    let five_h = super::overview::pool_window(account.utilization_5h, account.reset_5h);
    let seven_d = super::overview::pool_window(account.utilization_7d, account.reset_7d);
    let fable = super::overview::pool_window(account.utilization_7d_oi, account.reset_7d_oi);
    let stats = window_stats(
        &[
            ("5h", five_h.as_ref()),
            ("7d", seven_d.as_ref()),
            ("7d fable", fable.as_ref()),
        ],
        reset_fmt,
        now,
    );
    if !stats.is_empty() {
        lines.extend(render_stat_block(&stats, inner.width));
    }
    render_usage_lines(frame, inner, app, lines);
}

/// Render an assembled Usage detail body into `inner`, publishing the scroll
/// bounds the key handler clamps against and drawing the overflow scrollbar.
/// The claude pane has its own slot-aware tail because the note editor docks
/// at its bottom; the codex and pool panes share this one.
fn render_usage_lines(frame: &mut Frame<'_>, inner: Rect, app: &App, lines: Vec<Line<'static>>) {
    let total = lines.len();
    let viewport = inner.height as usize;
    let max_scroll = if viewport == 0 {
        0
    } else {
        total.saturating_sub(viewport)
    };
    app.usage_detail_max_scroll.set(max_scroll);
    app.usage_detail_viewport.set(viewport);
    let scroll = app.usage_detail_scroll.min(max_scroll);
    let paragraph = if scroll > usize::from(u16::MAX) {
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll)
                .take(viewport)
                .collect::<Vec<_>>(),
        )
    } else {
        Paragraph::new(lines).scroll((scroll as u16, 0))
    };
    frame.render_widget(paragraph.style(theme::base()), inner);
    draw_scrollbar(frame, inner, total, scroll, viewport);
}

#[allow(
    clippy::too_many_arguments,
    reason = "one frame's full context: profile, header, app, snapshot"
)]
fn build_usage_lines(
    profile: &Profile,
    inner_w: u16,
    header: &HeaderState,
    app: &App,
    readings: &ReadingsSnapshot,
    show_estimates: bool,
    show_pace: bool,
    reset_fmt: ResetFmt,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(header_lines(profile, header, inner_w, readings));
    lines.push(Line::from(""));
    // The live speed/rate rows sit under the header, ahead of the usage bars:
    // they are the account's current figures, read off a fresh producer, and
    // render from the first frame.
    lines.extend(tps_rows(profile, app, inner_w));
    lines.push(Line::from(""));

    // Notes close the pane (cloudy's placement): below the usage bars and the
    // extra-usage/balance rows, so the account's figures read first. Every
    // branch exit appends it.
    let with_notes = |mut lines: Vec<Line<'static>>| -> Vec<Line<'static>> {
        lines.push(Line::from(""));
        lines.extend(notes_lines(app.note_text.as_deref(), inner_w));
        lines
    };

    // Accounts whose usage figures live in the third-party cache — a recognised
    // provider or a generic api-key endpoint — render via the third-party
    // rows/bars path, the shared cache selector (`usage_cache_is_third_party`,
    // the same predicate the snapshot builder picked each name with);
    // OAuth accounts — including OAuth run against a custom base_url — fall
    // through to their live window bars.
    if profile.usage_cache_is_third_party() {
        // In-memory series only (`app.wallet_cache`) — the same no-disk-read
        // discipline the 5h rate's `history_cache` read keeps one branch up.
        let wallet_rate = app.wallet_rate_for(&profile.name, readings.stats_of(&profile.name));
        lines.extend(build_tp_rows(
            profile,
            inner_w,
            readings,
            show_estimates,
            show_pace,
            reset_fmt,
            wallet_rate.as_ref(),
        ));
        return with_notes(lines);
    }

    let Some(usage) = readings.usage_of(&profile.name) else {
        lines.extend(oauth_empty_lines(
            oauth_empty_msg(profile, readings.status_of(&profile.name)),
            inner_w,
        ));
        return with_notes(lines);
    };

    let mut stats = collect_stats(usage, reset_fmt);
    if stats.is_empty() {
        lines.extend(oauth_empty_lines(
            oauth_empty_msg(profile, readings.status_of(&profile.name)),
            inner_w,
        ));
        return with_notes(lines);
    }

    let history = app
        .history_cache
        .get(profile.name.as_str())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    // 5-hour windows get the recency-weighted recent-burn rate (%/h) from
    // history. 7-day windows instead use a window-anchored average pace (%/d)
    // derived from `resets_at` in `collect_stats` — rotation-proof, where a
    // history slope would jump on every account rotation.
    let mut window_rates: HashMap<String, Option<f64>> = HashMap::new();
    if let Some(u) = readings.usage_of(&profile.name) {
        let five_h: Vec<_> = u
            .windows()
            .into_iter()
            .filter(|(l, _)| !l.starts_with("7d"))
            .collect();
        if !five_h.is_empty() {
            window_rates.extend(crate::usage::compute_burn_rates_from_history(
                history,
                &five_h,
                60 * 60 * 1000, // lookback_ms: last 1h of samples for %/h
                3,              // min_samples before a rate is shown
                10 * 60 * 1000, // gap_cut_ms: cut idle gaps for short-horizon windows
            ));
        }
    }

    // Fill the 5h recent-burn rate; the 7d average pace is already set by
    // collect_stats.
    for stat in &mut stats {
        if !stat.label.starts_with("7d") {
            stat.burn_rate = window_rates.get(&stat.label).and_then(|r| *r);
        }
    }

    if !show_estimates {
        for stat in &mut stats {
            stat.burn_rate = None;
        }
    }
    if !show_pace {
        for stat in &mut stats {
            stat.pace_pct = None;
        }
    }

    lines.extend(render_stat_block(&stats, inner_w));
    with_notes(lines)
}

/// Render a list of [`Stat`]s as the shared two-line bar blocks (eyebrow + bar),
/// computing the column widths (`max_trailing`/`max_rate_w`/`max_amount_w`) once
/// so the `%` column lines up across rows. A blank line separates rows. Used by
/// both the OAuth window path and the third-party bars path.
fn render_stat_block(stats: &[Stat], inner_w: u16) -> Vec<Line<'static>> {
    let max_trailing = stats
        .iter()
        .map(|s| s.trailing.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = bar_width_for(inner_w, max_trailing);
    // Right-align % to far content edge so figures stack above the reset text.
    let pct_col = (bar_width + max_trailing).min(inner_w as usize);
    let max_rate_w = stats
        .iter()
        .map(|s| s.rate_section_width())
        .max()
        .unwrap_or(0);
    let max_amount_w = stats
        .iter()
        .map(|s| s.amount.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = Vec::new();
    for (i, stat) in stats.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(stat.render(bar_width, pct_col, max_rate_w, max_amount_w));
    }
    lines
}

struct Stat {
    label: String,
    /// The `%` reading. `None` when the reply carried no `percent` /
    /// `utilization`: the eyebrow renders `—` faint and the bar line draws no
    /// track (figures only), so an unread figure never reads as `0%`.
    pct: Option<f64>,
    color: Style,
    trailing: String,
    /// Absolute `used / total` shown on the eyebrow line immediately before the
    /// `%` (right-aligned in its own column). Empty when the window carries no
    /// absolute amounts. Distinct from `trailing`, which sits on the bar line.
    amount: String,
    burn_rate: Option<f64>,
    rate_unit: &'static str,
    /// Ideal-pace marker position as a percentage (0..=100), or `None` to draw
    /// no marker. Gated by `AppState.show_pace`; computed in [`collect_stats`].
    pace_pct: Option<f64>,
    /// Seconds until this window resets, for the burn-ETA color cue. `None` when
    /// the window carries no reset stamp (e.g. extra credits).
    reset_secs: Option<i64>,
    /// A note-only row: `Some(text)` renders `label  text` faint, with no bar
    /// and no `%`/`$` figures — the billing-off spend block, whose stale cap
    /// must not read as live.
    note: Option<&'static str>,
}

/// Seconds until the window hits 100% at the current burn rate.
/// `rate` may be in %/h or %/d (determined by `rate_unit`).
pub(super) fn eta_left_secs(rate: f64, pct: f64, rate_unit: &str) -> Option<i64> {
    if rate <= 0.0 || pct >= 100.0 {
        return None;
    }
    let rate_per_h = if rate_unit == "d" { rate / 24.0 } else { rate };
    let hours = (100.0 - pct) / rate_per_h;
    let secs = (hours * 3600.0) as i64;
    (secs > 0).then_some(secs)
}

fn eta_left(rate: f64, pct: f64, rate_unit: &str) -> Option<String> {
    eta_left_secs(rate, pct, rate_unit).map(crate::usage::humanize_duration)
}

impl Stat {
    /// Width of the ` · rate` + optional ` · X left` section, for alignment.
    fn rate_section_width(&self) -> usize {
        let Some(rate) = self.burn_rate.filter(|r| *r > 0.0) else {
            return 0;
        };
        let mut w =
            " · ".chars().count() + format!("{:.1} %/{}", rate, self.rate_unit).chars().count();
        if let Some(pct) = self.pct
            && let Some(dur) = eta_left(rate, pct, self.rate_unit)
        {
            w += " · ".chars().count() + dur.chars().count() + " left".chars().count();
        }
        w
    }

    /// Eyebrow (label · rate · `used / total` · %) then bar with trailing reset
    /// suffix. `bar_width` shared across rows; `pct_col` = far content edge for %
    /// alignment. `max_rate_w` / `max_amount_w` keep the rate + amount columns
    /// fixed across all rows so the `%` column never shifts.
    fn render(
        &self,
        bar_width: usize,
        pct_col: usize,
        max_rate_w: usize,
        max_amount_w: usize,
    ) -> Vec<Line<'static>> {
        if let Some(note) = self.note {
            return vec![Line::from(vec![
                Span::styled(self.label.clone(), theme::faint()),
                Span::styled(format!("  {note}"), theme::faint()),
            ])];
        }
        let unread = self.pct.is_none();
        // Natural width — `header_pad` right-aligns the whole block to `pct_col`,
        // so the `%` lands in the same column every row without padding the
        // number. (A fixed `{:>3.0}` width added stray spaces after the amount.)
        let pct_str = self.pct.map_or_else(|| NO_DATA.to_string(), format_pct);

        // The amount sits in its own right-aligned column just left of the %,
        // with a 2-space gap when present.
        let amount_section_w = if max_amount_w > 0 {
            max_amount_w + 2
        } else {
            0
        };
        let header_pad = pct_col
            .saturating_sub(self.label.chars().count())
            .saturating_sub(max_rate_w)
            .saturating_sub(amount_section_w)
            .saturating_sub(pct_str.chars().count());

        let filled = self.pct.map_or(0, |p| {
            (((p / 100.0) * bar_width as f64).round() as usize).min(bar_width)
        });
        let pct_style = if unread {
            theme::faint()
        } else {
            self.color.add_modifier(Modifier::BOLD)
        };
        let marker = self.pace_pct.filter(|_| !unread && bar_width > 0).map(|p| {
            (((p.clamp(0.0, 100.0) / 100.0) * bar_width as f64).round() as usize).min(bar_width - 1)
        });

        let bar_line = if unread {
            // No track: keep the money figures right-aligned where the bar's
            // trailing line would sit, with no `░`/`█`/pace marker.
            let mut v = Vec::new();
            if !self.trailing.is_empty() {
                let pad = pct_col.saturating_sub(self.trailing.chars().count());
                if pad > 0 {
                    v.push(Span::raw(" ".repeat(pad)));
                }
                v.push(Span::styled(self.trailing.clone(), theme::faint()));
            }
            v
        } else {
            let mut v = bar_spans(filled, bar_width, self.color, marker);
            // Right-align trailing text to the same far column as %.
            if !self.trailing.is_empty() {
                let pad = pct_col
                    .saturating_sub(bar_width)
                    .saturating_sub(self.trailing.chars().count());
                if pad > 0 {
                    v.push(Span::raw(" ".repeat(pad)));
                }
                v.push(Span::styled(self.trailing.clone(), theme::faint()));
            }
            v
        };

        let mut label_spans = vec![Span::styled(self.label.clone(), theme::label())];
        if let Some(rate) = self.burn_rate
            && rate > 0.0
        {
            let rate_color = Style::default().fg(theme::util_color(rate.clamp(0.0, 100.0)));
            label_spans.push(Span::styled(" · ", theme::dim()));
            let rate_str = format!("{:.1} %/{}", rate, self.rate_unit);
            label_spans.push(Span::styled(rate_str.clone(), rate_color));

            if let Some(pct) = self.pct
                && let Some(eta_secs) = eta_left_secs(rate, pct, self.rate_unit)
            {
                let dur = crate::usage::humanize_duration(eta_secs);
                // Warn when the window hits 100% before it resets: you run dry
                // before the limit refreshes. Faint when the reset lands first.
                let runs_dry_first = self.reset_secs.is_some_and(|r| eta_secs < r);
                let style = if runs_dry_first {
                    theme::warning()
                } else {
                    theme::faint()
                };
                label_spans.push(Span::styled(" · ", theme::faint()));
                label_spans.push(Span::styled(format!("{dur} left"), style));
            }

            let my_rate_w = self.rate_section_width();
            let extra = max_rate_w.saturating_sub(my_rate_w);
            if extra > 0 {
                label_spans.push(Span::raw(" ".repeat(extra)));
            }
        } else {
            label_spans.push(Span::raw(" ".repeat(max_rate_w)));
        }

        let pct_cells = Span::raw(&pct_str).width();
        let fits =
            Span::raw(&self.label).width() + max_rate_w + amount_section_w + pct_cells <= pct_col;
        let mut out: Vec<Line<'static>> = Vec::new();
        if fits {
            out.push(Line::from({
                let mut spans = label_spans;
                spans.push(Span::raw(" ".repeat(header_pad)));
                if max_amount_w > 0 {
                    // Right-align the amount in its fixed column, then a 2-space gap.
                    let pad = max_amount_w.saturating_sub(self.amount.chars().count());
                    spans.push(Span::raw(" ".repeat(pad)));
                    if !self.amount.is_empty() {
                        spans.push(Span::styled(self.amount.clone(), theme::faint()));
                    }
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(pct_str, pct_style));
                spans
            }));
        } else {
            // The eyebrow no longer holds the `%` figure: it drops to its own
            // line, right-aligned to the far edge, carrying the amount with it
            // so no figure vanishes — the pair never hard-wraps apart.
            out.push(Line::from(label_spans));
            let mut dropped = Vec::new();
            if max_amount_w > 0 {
                let amount_cells = Span::raw(&self.amount).width();
                let right = max_amount_w + 2 + pct_cells;
                dropped.push(Span::raw(" ".repeat(pct_col.saturating_sub(right))));
                dropped.push(Span::raw(
                    " ".repeat(max_amount_w.saturating_sub(amount_cells)),
                ));
                if !self.amount.is_empty() {
                    dropped.push(Span::styled(self.amount.clone(), theme::faint()));
                }
                dropped.push(Span::raw("  "));
            } else {
                dropped.push(Span::raw(" ".repeat(pct_col.saturating_sub(pct_cells))));
            }
            dropped.push(Span::styled(pct_str, pct_style));
            out.push(Line::from(dropped));
        }
        out.push(Line::from(bar_line));
        out
    }
}

/// Classify a window label into its rate unit. OAuth labels carry the unit at
/// the front (`"7d sonnet"`, `"7d opus"`, `"5h"` → `starts_with("7d")`); the
/// third-party bar labels carry it at the end (`"30d"`, `"7d"` → `ends_with('d')`
/// with a leading ascii digit). Anything else is treated as an hour window.
pub(super) fn window_rate_unit(label: &str) -> &'static str {
    if label.starts_with("7d") {
        return "d";
    }
    if label.ends_with('d') && label.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        return "d";
    }
    "h"
}

/// Pace-prediction toggles that always travel together (both originate from the
/// same `AppState.show_*` flags). Grouped so [`make_window_stat`] stays under
/// clippy's argument limit without an ad-hoc `#[allow]`.
#[derive(Clone, Copy)]
struct WindowGates {
    show_estimates: bool,
    show_pace: bool,
}

/// Shared core for both Stat-building paths (OAuth usage windows and third-party
/// provider bars). Computes the clamped pct, theme color, rate unit, and the
/// window-anchored burn pace / ideal-pace marker / reset countdown — all derived
/// purely from `pct` + `resets_at`, so the two paths render identically. The
/// caller supplies its own `trailing` (bar-line reset suffix) and `amount`
/// (eyebrow `used / total`); `gates` controls the pace fields.
fn make_window_stat(
    label: &str,
    pct: f64,
    resets_at: Option<&str>,
    now: i64,
    amount: String,
    trailing: String,
    gates: WindowGates,
) -> Stat {
    let pct = pct.clamp(0.0, 100.0);
    let rate_unit = window_rate_unit(label);
    let window = UsageWindow {
        utilization: pct,
        resets_at: resets_at.map(str::to_string),
    };
    let burn_rate = gates
        .show_estimates
        .then(|| {
            crate::usage::window_avg_pace_per_day(label, &window, now).map(|per_day| {
                if rate_unit == "d" {
                    per_day
                } else {
                    per_day / 24.0
                }
            })
        })
        .flatten();
    let pace_pct = gates
        .show_pace
        .then(|| ideal_pace_pct(label, &window, now))
        .flatten();
    let reset_secs = resets_at
        .and_then(crate::usage::iso_to_epoch_secs)
        .map(|r| r - now);
    // Past-reset windows show a frozen pre-reset reading; fade the bar fill +
    // `%` (both driven by `color`, see `Stat::render`) so staleness reads
    // visually until the next fetch lands.
    let color = if is_past_reset(&window) {
        theme::faint()
    } else {
        Style::default().fg(theme::util_color(pct))
    };
    Stat {
        label: label.to_string(),
        pct: Some(pct),
        color,
        trailing,
        amount,
        burn_rate,
        rate_unit,
        pace_pct,
        reset_secs,
        note: None,
    }
}

/// Eyebrow string for a window's absolute dollar figures (`$used / $limit`).
fn fmt_window_dollars(d: &WindowDollars) -> String {
    match (d.used, d.limit) {
        (Some(u), Some(l)) => format!("${u:.2} / ${l:.2}"),
        (Some(u), None) => format!("${u:.2}"),
        (None, Some(l)) => format!("/ ${l:.2}"),
        (None, None) => String::new(),
    }
}

/// One credit figure (`$used` / `$limit`), `—` when the reply carried none —
/// never a fabricated `$0.00` for an unread figure.
fn fmt_credit_amt(sym: &str, v: Option<f64>) -> String {
    v.map_or_else(|| NO_DATA.to_string(), |v| format!("{sym}{v:.2}"))
}

fn collect_stats(usage: &crate::usage::UsageInfo, reset_fmt: ResetFmt) -> Vec<Stat> {
    let now_secs = now_epoch_secs();
    let mut stats: Vec<Stat> = Vec::new();
    for (label, w) in usage.windows() {
        let trailing = reset_in_secs_at(w, now_secs)
            .map(|secs| format!("  {}", reset_phrase(secs, reset_fmt)))
            .unwrap_or_default();
        // Absolute $ figures on the eyebrow when the window carries them (null on
        // every current account; Claude Code itself drops these fields).
        let amount = usage
            .window_dollars
            .iter()
            .find(|d| d.label == label)
            .map(fmt_window_dollars)
            .unwrap_or_default();
        // OAuth paths compute ungated here; the 5h recent-burn rate is filled
        // later from history (overwriting whatever `make_window_stat` set), and
        // the show_estimates / show_pace gates are applied by the caller.
        stats.push(make_window_stat(
            label,
            w.utilization,
            w.resets_at.as_deref(),
            now_secs,
            amount,
            trailing,
            WindowGates {
                show_estimates: true,
                show_pace: true,
            },
        ));
    }
    // `spend` is the newer, correctly-typed view of the same credit cap; while
    // its billing is ON, the legacy `extra_usage` bar would just duplicate it.
    // Fall back to `extra` when spend billing is OFF — the stale-limit case,
    // where the spend row is a billing-off note, not a bar.
    let spend_billing_on = usage.spend.as_ref().is_some_and(|s| s.enabled);
    if let Some(extra) = &usage.extra_usage
        && extra.is_enabled
        && !spend_billing_on
    {
        let pct = extra.utilization.map(|v| v.clamp(0.0, 100.0));
        let sym = match extra.currency.as_deref() {
            Some("USD") | None => "$",
            Some(other) => other,
        };
        // Legacy `extra_usage` reports money as bare minor units (cents); `spend`
        // carries the same figures already scaled to dollars. Divide to match.
        let used = extra.used_credits.map(|v| v / 100.0);
        let limit = extra.monthly_limit.map(|v| v / 100.0);
        stats.push(Stat {
            label: "extra".to_string(),
            pct,
            color: pct.map_or(theme::faint(), |p| {
                Style::default().fg(theme::util_color(p))
            }),
            // Credits used/limit ride the bar's trailing line, where window bars
            // show their reset countdown, so the eyebrow carries just the %.
            trailing: format!(
                "{} / {}",
                fmt_credit_amt(sym, used),
                fmt_credit_amt(sym, limit)
            ),
            amount: String::new(),
            burn_rate: None,
            rate_unit: "h",
            pace_pct: None,
            reset_secs: None,
            note: None,
        });
    }
    // Per-period extra-credit breakdowns (`daily`/`weekly`) — shape
    // unconfirmed, absent on every current account. Keyed on `extra.is_enabled`
    // alone: a daily/weekly breakdown is a figure no other row shows, never a
    // duplicate of the spend bar.
    if let Some(extra) = &usage.extra_usage
        && extra.is_enabled
    {
        for (label, raw) in [
            ("extra usage (24h)", &extra.daily),
            ("extra usage (7d)", &extra.weekly),
        ] {
            let Some(period) = raw.as_ref().and_then(ExtraPeriod::from_value) else {
                continue;
            };
            let pct = period.utilization.map(|v| v.clamp(0.0, 100.0));
            let sym = match period.currency.as_deref().or(extra.currency.as_deref()) {
                Some("USD") | None => "$",
                Some(other) => other,
            };
            let cost = match period.monthly_limit {
                Some(_) => format!(
                    "{} / {}",
                    fmt_credit_amt(sym, period.used_credits),
                    fmt_credit_amt(sym, period.monthly_limit)
                ),
                None => fmt_credit_amt(sym, period.used_credits),
            };
            stats.push(Stat {
                label: label.to_string(),
                pct,
                color: pct.map_or(theme::faint(), |p| {
                    Style::default().fg(theme::util_color(p))
                }),
                trailing: cost,
                amount: String::new(),
                burn_rate: None,
                rate_unit: "h",
                pace_pct: None,
                reset_secs: None,
                note: None,
            });
        }
    }
    if let Some(spend) = &usage.spend {
        if spend.enabled {
            let pct = spend.percent.map(|v| v.clamp(0.0, 100.0));
            let sym = match spend.currency.as_deref() {
                Some("USD") | None => "$",
                Some(other) => other,
            };
            let cost = match spend.limit {
                Some(_) => format!(
                    "{} / {}",
                    fmt_credit_amt(sym, spend.used),
                    fmt_credit_amt(sym, spend.limit)
                ),
                None => fmt_credit_amt(sym, spend.used),
            };
            stats.push(Stat {
                label: "spend".to_string(),
                pct,
                color: pct.map_or(theme::faint(), |p| {
                    Style::default().fg(theme::util_color(p))
                }),
                trailing: cost,
                amount: String::new(),
                burn_rate: None,
                rate_unit: "h",
                pace_pct: None,
                reset_secs: None,
                note: None,
            });
        } else if spend.is_visible() {
            // Billing is off but the wire left a stale cap behind; render only a
            // faint note so the stale figures never read as a live bar.
            stats.push(Stat {
                label: "spend".to_string(),
                pct: None,
                color: theme::faint(),
                trailing: String::new(),
                amount: String::new(),
                burn_rate: None,
                rate_unit: "h",
                pace_pct: None,
                reset_secs: None,
                note: Some("billing off"),
            });
        }
    }
    stats
}

/// As wide as possible while leaving room for the longest trailing suffix.
fn bar_width_for(inner_w: u16, max_trailing: usize) -> usize {
    let avail = (inner_w as usize).saturating_sub(max_trailing);
    if avail >= 10 {
        avail
    } else {
        // Suffix nearly fills the line — keep what's left rather than forcing a
        // 10-cell bar that pushes the suffix off the edge.
        avail.max(1)
    }
}

/// The usage bar: `filled` █ cells in `fill`, the rest ░, with an optional `│`
/// ideal-pace marker at `marker_col`. The marker reads WARNING once the fill has
/// passed it (usage running ahead of an even spread) and faint while the fill is
/// still behind it. Drawn over the bar so a wide fill never hides it — the
/// horizontal twin of `chain::gauge_with_tick`.
fn bar_spans(
    filled: usize,
    bar_width: usize,
    fill: Style,
    marker_col: Option<usize>,
) -> Vec<Span<'static>> {
    let empty = bar_width - filled;
    let Some(m) = marker_col.filter(|&m| m < bar_width) else {
        return vec![
            Span::styled("█".repeat(filled), fill),
            Span::styled("░".repeat(empty), theme::line_strong()),
        ];
    };

    // Emit each run only when non-empty so the marker splits the bar cleanly.
    let run =
        |glyph: &str, n: usize, style: Style| (n > 0).then(|| Span::styled(glyph.repeat(n), style));
    let mut spans = Vec::with_capacity(4);
    if m < filled {
        spans.extend(run("█", m, fill));
        spans.push(Span::styled("│".to_string(), theme::warning()));
        spans.extend(run("█", filled - m - 1, fill));
        spans.extend(run("░", empty, theme::line_strong()));
    } else {
        spans.extend(run("█", filled, fill));
        spans.extend(run("░", m - filled, theme::line_strong()));
        spans.push(Span::styled("│".to_string(), theme::dim()));
        spans.extend(run("░", bar_width - m - 1, theme::line_strong()));
    }
    spans
}

fn header_lines(
    profile: &Profile,
    header: &HeaderState,
    inner_w: u16,
    readings: &ReadingsSnapshot,
) -> Vec<Line<'static>> {
    let plan = readings
        .stats_of(&profile.name)
        .and_then(|s| s.plan.clone())
        // Forked exactly as the BODY below is, so the header cannot name a
        // different account kind than the bars under it. `is_oauth()` is the
        // wrong test here: it keys on `base_url` alone, so a hybrid (OAuth pair
        // plus a custom endpoint) reads "api" directly above the live Anthropic
        // windows its own picked reading fed.
        .or_else(|| {
            if profile.usage_cache_is_third_party() {
                Some("api".to_string())
            } else {
                account_tier(profile, readings.usage_of(&profile.name)).and_then(|t| t.display())
            }
        });
    // No tier known yet takes the house no-data dash: a bare "Claude" here read
    // as a real plan on the one row an operator checks their plan from.
    let plan_w = plan.as_deref().map(|s| s.chars().count()).unwrap_or(1);
    let plan_span = match plan {
        Some(label) => Span::styled(label, theme::body()),
        None => Span::styled("—".to_string(), theme::faint()),
    };
    let plan_key = key_span("plan");
    let left_w = plan_key.width() + plan_w;
    let mut plan_spans = vec![plan_key, plan_span];
    if profile.auto_start {
        plan_spans.extend(kick_spans(
            &kick_text(profile, header, readings),
            left_w,
            inner_w as usize,
        ));
    }
    let mut lines = vec![Line::from(plan_spans)];
    if let Some(peak) = header.peak {
        lines.push(pricing_line(peak));
    }
    lines.extend(status_lines(profile, header, inner_w, readings));
    lines
}

/// The `speed` and `rate` rows: each reads `live · 5m · 1h · today`, one figure
/// per window: `—` faint with no fresh producer, and on a speed window no
/// response touched; a fresh producer's absent account reads a `0` rate. Speed
/// is output tokens ÷ generation seconds; rate is output tokens ÷ wall-clock
/// window length (today = seconds since 00:00 UTC, `0` under a fresh producer
/// with no day row). Windows shed whole from the right when `inner_w` cannot
/// hold them; both rows shed to the same window set (the wider rate row
/// decides).
fn tps_rows(profile: &Profile, app: &App, inner_w: u16) -> Vec<Line<'static>> {
    let name = profile.name.as_str();
    // `fresh` gates every figure: `—` everywhere with no fresh producer. Under
    // a fresh producer `accounts` names only accounts with finished responses,
    // so an absent account still has a real zero RATE (rate is never absent
    // under a fresh producer; `which --json` already says `rate: 0`) while its
    // SPEED stays `—`, since no response touched the window.
    let fresh = app.fresh_tps();
    let figures = fresh.and_then(|s| s.accounts.get(name));
    let today = fresh
        .and_then(|s| s.days.get(&crate::tokens::today_date()))
        .and_then(|accounts| accounts.get(name))
        .map(|models| {
            models
                .values()
                .fold(crate::tps::DaySum::default(), |acc, d| crate::tps::DaySum {
                    output: acc.output + d.output,
                    gen_ms: acc.gen_ms + d.gen_ms,
                })
        });
    let secs_since_midnight = (app.tps_now_ms() % 86_400_000) as f64 / 1000.0;
    let today_speed = today.and_then(crate::tui::app::day_average);
    // A fresh account with no day row has a real zero today-rate, not an
    // absence; only no fresh producer reads `—`.
    let today_rate = fresh.map(|_| {
        if secs_since_midnight > 0.0 {
            today.map_or(0.0, |t| t.output as f64 / secs_since_midnight)
        } else {
            0.0
        }
    });

    let speed_values = [
        ("live", figures.and_then(|f| f.live.speed)),
        ("5m", figures.and_then(|f| f.m5.speed)),
        ("1h", figures.and_then(|f| f.h1.speed)),
        ("today", today_speed),
    ];
    let rate_values = [
        ("live", fresh.map(|_| figures.map_or(0.0, |f| f.live.rate))),
        ("5m", fresh.map(|_| figures.map_or(0.0, |f| f.m5.rate))),
        ("1h", fresh.map(|_| figures.map_or(0.0, |f| f.h1.rate))),
        ("today", today_rate),
    ];
    let keep = row_windows("tok/s", &rate_values, inner_w);
    vec![
        figure_row("speed", "t/s", &speed_values[..keep]),
        figure_row("rate", "tok/s", &rate_values[..keep]),
    ]
}

/// How many windows (the leftmost `live` included) fit `inner_w` for a row
/// whose figures carry `unit`.
fn row_windows(unit: &'static str, values: &[(&'static str, Option<f64>)], inner_w: u16) -> usize {
    let mut used = KEY_W + KEY_GUTTER;
    for (i, (window, value)) in values.iter().enumerate() {
        let figure = value.map_or_else(
            || "—".to_owned(),
            |v| format!("{} {unit}", crate::tps::format_figure(v)),
        );
        let sep_w = if i > 0 { " · ".chars().count() } else { 0 };
        let cell_w = sep_w + Span::raw(*window).width() + 1 + figure.chars().count();
        if i > 0 && used + cell_w > inner_w as usize {
            return i;
        }
        used += cell_w;
    }
    values.len()
}

fn figure_row(
    label: &'static str,
    unit: &'static str,
    values: &[(&'static str, Option<f64>)],
) -> Line<'static> {
    let mut spans = vec![key_span(label)];
    for (i, (window, value)) in values.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(*window, theme::dim()));
        spans.push(Span::raw(" "));
        match value {
            Some(v) => spans.push(Span::styled(
                format!("{} {unit}", crate::tps::format_figure(*v)),
                theme::body(),
            )),
            None => spans.push(Span::styled("—", theme::faint())),
        }
    }
    Line::from(spans)
}

/// The `pricing` header row: the peak-rate state sampled now, named as a pill
/// plus the countdown to the next flip. Peak is a charged state (WARNING);
/// off-peak is the neutral resting state. Into peak the countdown warns
/// (`peak starts in …`); leaving peak it is relief and the pill supplies the
/// subject, so it is a bare `ends in …`. No trailing countdown when no flip
/// lands inside the query horizon. Windows come from the price table's own
/// constraints — the same schedule cost pricing uses, never a second opinion.
fn pricing_line(peak: crate::pricing::PeakState) -> Line<'static> {
    let (label, style) = if peak.peak {
        ("peak rate", theme::warning().bold())
    } else {
        ("off-peak", theme::dim().bold())
    };
    let mut spans = vec![key_span("pricing")];
    spans.extend(pill(label.to_string(), style));
    if let Some((to_peak, secs)) = peak.next_flip {
        let countdown = if to_peak {
            format!("peak starts in {}", humanize_duration(secs))
        } else {
            format!("ends in {}", humanize_duration(secs))
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(countdown, theme::faint()));
    }
    Line::from(spans)
}

/// The `usage auto-start in …` value, shown for ANY account that opted into
/// `auto_start`, queue toggle on or off. The value is THIS account's next
/// kick: with a queue slot, the LATER of the queue's next-opening estimate
/// and the account's own 5h window reset — the kick fires once the queue
/// gate has cleared AND this window has lapsed, so either clock can delay
/// it, and the gate alone would name an instant no kick fires at. Without a
/// slot (toggle off, or the profile is excluded from the queue) it is the
/// account's own reset alone — the lapsed-leg kick fires the moment that
/// reset passes.
fn kick_text(profile: &Profile, header: &HeaderState, readings: &ReadingsSnapshot) -> String {
    let now = now_epoch_secs();
    let own_reset_in = readings
        .usage_of(&profile.name)
        .and_then(|u| u.five_hour.as_ref())
        .and_then(|w| reset_in_secs_at(w, now))
        .filter(|secs| *secs > 0);
    let next_in = match header.queue_slot {
        Some(slot) => match (slot.next_in, own_reset_in) {
            (Some(gate), Some(reset)) => Some(gate.max(reset)),
            (Some(gate), None) => Some(gate),
            (None, reset) => reset,
        },
        None => own_reset_in,
    };
    match next_in {
        Some(secs) => format!("usage auto-start in {}", humanize_duration(secs)),
        None => "usage auto-start due now".to_string(),
    }
}

/// Spans putting `text` flush against the pane's right edge on the `plan` row,
/// keeping the house 3-cell minimum gap from the row's left content. Truncates
/// with `…` when the row can't hold both; drops the kick when not even a
/// countdown hint fits.
fn kick_spans(text: &str, left_w: usize, inner_w: usize) -> Vec<Span<'static>> {
    let avail = inner_w.saturating_sub(left_w);
    if avail < 3 {
        return Vec::new();
    }
    let text = truncate(text, avail - 3);
    if text.chars().count() < 4 {
        return Vec::new();
    }
    let pad = avail - Span::raw(&text).width();
    vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(text, theme::faint()),
    ]
}

/// One row of the `status` block paired with its optional `└`/`├` fix hint.
/// Collected before render so [`render_status_rows`] can see the total hint
/// count up front and connect 2+ into one rail instead of floating each `└`
/// detached.
struct DiagRow {
    /// Row content AFTER the key/rail column — `render_status_rows` decides
    /// that column once every row's hint state is known.
    content: Vec<Span<'static>>,
    hint: Option<String>,
}

/// The `status` block: dead-first diagnostic pills (disabled → canceled →
/// auth-broken → kick → spend),
/// then the fetch state / refresh countdown, each carrying a fix hint that
/// names what's wrong and how to fix it (config-aware; see [`diag_fix`]). Kept
/// multi-line because at full spread one line runs ~78 cells — the detail pane
/// clears that only past a ~116-column terminal, and this `Paragraph` has no
/// wrap, so a single row silently clipped the ceiling off.
///
/// Render-only: reads the fallback engine's own predicates via `header.diag`,
/// never a second opinion, so a hint can't claim a state the engine won't act on.
fn status_lines(
    profile: &Profile,
    header: &HeaderState,
    inner_w: u16,
    readings: &ReadingsSnapshot,
) -> Vec<Line<'static>> {
    if !matches!(header.activity, ProfileActivity::Idle) {
        let frame = spinner_frame(header.tick);
        let verb = activity_verb(header.activity);
        return vec![Line::from(vec![
            key_span("status"),
            Span::styled(format!("{frame} {verb}"), spinner_style(header.activity)),
        ])];
    }

    let now = now_epoch_secs();
    let w = inner_w as usize;
    let mut rows: Vec<DiagRow> = Vec::new();

    // 0. Disabled leads everything, but suppresses only what it makes UNTRUE.
    //    The scheduler's work list (`AppConfig::enabled_profiles`) skips the
    //    account, so the fetch-state and refresh-countdown rungs below would lie
    //    ("up to date" / "refresh in 40s" for a poll that will never run) — those
    //    are gated off at rung 5. Everything between stays accurate: a canceled
    //    subscription, a dead login, a kick block and a spent budget are all just
    //    as true on a disabled account, and hiding them would strand an operator
    //    who re-enables it. So this rung pushes and falls through.
    let disabled = profile.is_disabled();
    if disabled {
        rows.push(DiagRow {
            content: pill(
                DIAG_DISABLED.to_string(),
                theme::dim().add_modifier(Modifier::BOLD),
            ),
            hint: Some(diag_fix(UsageDiag::Disabled, &profile.name)),
        });
    }

    // 1. Canceled subscription DOMINATES (dead-first, above auth-broken): a
    //    canceled account 429s `/usage` forever, so the fetch line would read
    //    "rate limited" while the real reason is the dead subscription itself.
    //    `blocked_reason` ranks it first for the same reason. Sourced from the
    //    profile's cached plan (no config lock needed), so it's checked inline
    //    rather than via `DiagFlags`.
    if readings
        .usage_of(&profile.name)
        .and_then(|u| u.plan.as_ref())
        .is_some_and(|p| p.is_canceled())
    {
        rows.push(DiagRow {
            content: pill(
                DIAG_CANCELED.to_string(),
                theme::danger().add_modifier(Modifier::BOLD),
            ),
            hint: Some(diag_fix(UsageDiag::Canceled, &profile.name)),
        });
        return render_status_rows(rows, w);
    }

    // 2. Auth-broken DOMINATES the rest (dead-first): a revoked login can't
    //    serve at all, so a kick block, spend state, or freshness/refresh line on
    //    it is moot — re-login is the only action, and `blocked_reason` ranks it
    //    first for the same reason. Return once the pill + its re-login hint are
    //    emitted so nothing below paints a reassuring "up to date" (or a phantom
    //    "refresh in Ns") under a dead login.
    if header.diag.auth_broken {
        rows.push(DiagRow {
            content: pill(
                DIAG_AUTH_BROKEN.to_string(),
                theme::danger().add_modifier(Modifier::BOLD),
            ),
            hint: Some(diag_fix(UsageDiag::AuthBroken, &profile.name)),
        });
        return render_status_rows(rows, w);
    }

    // 3. Kick-429 block, additive to whatever the fetch state says: `/usage` can
    //    stay Fresh straight through a messages-limiter outage, so the fetch line
    //    below reads healthy while the 5h window silently never opens. Same
    //    amber→red escalation as the other streak pills; the suffix names the
    //    limiter's advertised ceiling, an upper bound (it has relented early).
    if let Some(block) = header.kick_block {
        // `streak_style` already stamps BOLD, so the pill's own style carries it.
        let mut spans = pill(DIAG_KICK.to_string(), streak_style(block.streak));
        if let Some(until) = block.until {
            let left = until.saturating_sub(now);
            spans.push(Span::styled(
                format!("  {}", crate::usage::humanize_duration(left)),
                theme::faint(),
            ));
        }
        // The flagship divergence: a switch-grade block on an auto_start account
        // self-recovers (re-tested each poll on a live window), a manual one sits
        // until the ceiling; a non-switch-grade burst is low-urgency backoff.
        let diag = if kick_block_switch_grade(&block, now) {
            UsageDiag::KickSwitchGrade {
                auto_start: header.diag.auto_start,
            }
        } else {
            UsageDiag::KickBurst
        };
        rows.push(DiagRow {
            content: spans,
            hint: Some(diag_fix(diag, &profile.name)),
        });
    }

    // 4. Spend: uncapped (DANGER config) outranks a spent budget (WARN); the two
    //    never render together — an uncapped ceiling makes "raise it" meaningless.
    if header.diag.spend_uncapped {
        rows.push(DiagRow {
            content: pill(
                "uncapped".to_string(),
                theme::danger().add_modifier(Modifier::BOLD),
            ),
            hint: Some(diag_fix(UsageDiag::SpendUncapped, &profile.name)),
        });
    } else if header.diag.budget_spent {
        rows.push(DiagRow {
            content: pill(
                DIAG_BUDGET_SPENT.to_string(),
                theme::warning().add_modifier(Modifier::BOLD),
            ),
            hint: Some(diag_fix(UsageDiag::BudgetSpent, &profile.name)),
        });
    }

    // A disabled account is never polled, so the fetch state is frozen at
    // whatever it was and `next_refresh_ms` counts down to a poll that will
    // never run. Both would be false claims, so the block ends here — with
    // nothing else wrong that leaves a single row and a lone `└`.
    if disabled {
        return render_status_rows(rows, w);
    }

    let countdown = header.next_refresh_ms.map(|next| {
        let secs = ((next as i64 - now_ms() as i64) / 1000).max(0);
        format!("{secs}s")
    });

    // 5. The fetch row, always last. Its own fix hint (if any) rides beneath it,
    //    connected into the same rail as the rows above once 2+ hints stack.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut fetch_hint: Option<UsageDiag> = None;
    match readings.status_of(&profile.name) {
        _ if (profile.base_url.is_some() || profile.provider.is_some())
            && !crate::usage::third_party_credentialed(profile)
            && profile
                .credentials
                .as_ref()
                .and_then(|c| c.claude_ai_oauth.as_ref())
                .is_none() =>
        {
            // A profile no leg will ever fetch — both work lists' own
            // membership predicates (no OAuth pair, no third-party
            // credential) — must not claim `up to date`, and a historical
            // fetch verdict is not current truth either: the key is gone, so
            // name that, dead-first over every outcome and dot below. The
            // endpoint shape keeps OAuth's own no-login state out of here:
            // that account has no key to miss. Figures shown are last-known,
            // and the stale cue can coexist with this pill: age is a fact
            // whether or not a fetch is scheduled.
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled(
                    "no key".to_string(),
                    theme::warning().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ]", theme::dim()),
            ]);
            // The hint rides only when figures render — without them the body
            // already names the fix, and one pane must not say it twice.
            if readings.stats_of(&profile.name).is_some() {
                fetch_hint = Some(UsageDiag::NoKey);
            }
        }
        Some(FetchStatus::Failed) => {
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled("failed", theme::danger().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
            ]);
            if let Some(c) = countdown {
                spans.push(Span::styled(format!("  retry in {c}"), theme::faint()));
            }
        }
        Some(FetchStatus::Cached) => {
            // A run of failed token refreshes lands here — we ARE serving
            // last-known numbers, so `Cached` is honest — but "cached" alone
            // names the symptom and not the cause, and nothing else on the row
            // would say the chain has stopped rotating. `auth failing` claims no
            // more than we know: the refresh is not going through, and the
            // endpoint has not confirmed the token is dead (that confirmed path
            // returns above under the `auth broken` pill, so we never reach here).
            let failing = header.streaks.refresh_fail > 0;
            let label = if failing { "auth failing" } else { "cached" };
            let style = if failing {
                streak_style(header.streaks.refresh_fail)
            } else {
                theme::warning().add_modifier(Modifier::BOLD)
            };
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled(label, style),
                Span::styled(" ]", theme::dim()),
            ]);
            if let Some(c) = countdown {
                // The countdown leads to the next REFRESH attempt while failing,
                // not to a plain usage poll, so it reads as a retry ordinal —
                // same shape the throttled row uses.
                let suffix = if failing {
                    format!("  {} retry in {c}", ordinal(header.streaks.refresh_fail))
                } else {
                    format!("  refresh in {c}")
                };
                spans.push(Span::styled(suffix, theme::faint()));
            }
            // Set unconditionally: auth-broken returns at §1 before this arm, so a
            // dead login never reaches here; the hint rides the fetch row below.
            fetch_hint = Some(if failing {
                UsageDiag::RefreshFailing
            } else {
                UsageDiag::Stale
            });
        }
        Some(FetchStatus::RateLimited) => {
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled("rate limited", streak_style(header.streaks.rate_limit)),
                Span::styled(" ]", theme::dim()),
            ]);
            if let Some(c) = countdown {
                // The retry ordinal makes slot depth visible — a high count
                // means the throttle never drained (#40's distrust boundary
                // sits past the 6th) — without a judgment label.
                let suffix = if header.streaks.rate_limit > 0 {
                    format!("  {} retry in {c}", ordinal(header.streaks.rate_limit))
                } else {
                    format!("  retry in {c}")
                };
                spans.push(Span::styled(suffix, theme::faint()));
            }
            // A deep slot the daemon itself distrusts (#40) names the throttle; a
            // shallow one is merely serving old numbers.
            fetch_hint = Some(if is_stuck_streak(header.streaks.rate_limit) {
                UsageDiag::Stuck429 {
                    throttler: throttler_name(profile, header.rate_limit_source),
                }
            } else {
                UsageDiag::Stale
            });
        }
        Some(FetchStatus::AuthExpired) => {
            // No countdown: the profile is session-suppressed, so there is no
            // next attempt to count down to. Only a re-login (or a manual
            // refresh, which retries once) moves this.
            let label = crate::format::auth_expired_label(profile);
            spans.extend([
                Span::styled("[ ", theme::dim()),
                Span::styled(label, theme::danger().add_modifier(Modifier::BOLD)),
                Span::styled(" ]", theme::dim()),
            ]);
            if crate::usage::oauth_without_pair(profile) {
                fetch_hint = Some(UsageDiag::SetupTokenRefused);
            }
        }
        _ => match countdown {
            // A scheduled refresh is work lined up — the `queued` dot (`◌` in
            // ACCENT), not a spinner: nothing is running yet.
            Some(c) => spans.extend([
                Span::styled("◌ ", theme::accent()),
                Span::styled(format!("refresh in {c}"), theme::dim()),
            ]),
            None => {
                // No scheduled refresh means `refresh_spent_accounts` is OFF and
                // this account is spent — skipped until its window resets. Render
                // it as a status pill like the fetch states above so the blank
                // overview timer reads as intent; the binding reset is already on
                // the maxed window's own bar line, so the pill stays bare. A
                // genuinely quiet account with no maxed window falls through to
                // "up to date".
                let resumes = readings
                    .usage_of(&profile.name)
                    .and_then(|u| crate::usage::spent_resume_in_secs(u, now));
                match resumes {
                    Some(_) => {
                        spans.extend([
                            Span::styled("[ ", theme::dim()),
                            Span::styled("spent", theme::warning().add_modifier(Modifier::BOLD)),
                            Span::styled(" ]", theme::dim()),
                        ]);
                        // Only the weekly cap earns the teach — the domain fact
                        // that a live-looking 5h window can't serve while the week
                        // is spent. A 5h-only spend is self-evident from the reset.
                        if header.diag.weekly_hard {
                            let monthly = readings
                                .usage_of(&profile.name)
                                .and_then(|u| u.long_window())
                                .is_some_and(|(label, _)| label == crate::usage::LABEL_30D);
                            fetch_hint = Some(UsageDiag::WeeklyHard { monthly });
                        }
                    }
                    // Nothing pending and nothing maxed — the `idle` dot, which
                    // differs from `queued` above by color alone, so the label
                    // carries the meaning on a monochrome read.
                    None => spans.extend([
                        Span::styled("◌ ", theme::dim()),
                        Span::styled("up to date", theme::dim()),
                    ]),
                }
            }
        },
    }
    // The `stale` cue: cache age past `stale_after_ms`, a fact orthogonal to
    // `fetch_status`. It prepends the fetch row rather than taking a rung of
    // its own. The one dot that would contradict it — `up to date` — is
    // unreachable while it fires (a keyless profile renders `[ no key ]`
    // above, a spent account is exempt, standdown re-seeds by age, and a
    // panicked worker records Failed); a `◌ refresh in Ns` can still follow
    // it after a failed cache write or a lowered interval, and that line
    // reads honestly: old numbers, refresh coming.
    if readings.stale_of(&profile.name) {
        let mut merged = pill(
            "stale".to_string(),
            theme::warning().add_modifier(Modifier::BOLD),
        );
        merged.push(Span::raw(" "));
        merged.extend(spans);
        spans = merged;
    }
    rows.push(DiagRow {
        content: spans,
        hint: fetch_hint.map(|d| diag_fix(d, &profile.name)),
    });

    render_status_rows(rows, w)
}

/// Render collected [`DiagRow`]s: the first carries the `status` key, the rest
/// blank-pad to the value column — unless 2+ rows carry a fix hint, in which
/// case every row between the first and last hint takes the rail's `│` at
/// col 0 instead of blank padding, and each hint renders `├`/`└` + text at
/// col 2. A single hint stays the plain `└` form: nothing to connect.
fn render_status_rows(rows: Vec<DiagRow>, width: usize) -> Vec<Line<'static>> {
    let hint_count = rows.iter().filter(|r| r.hint.is_some()).count();
    let mut lines = Vec::with_capacity(rows.len() * 2);
    let mut seen = 0usize;
    for (i, row) in rows.into_iter().enumerate() {
        let bridging = hint_count >= 2 && seen > 0 && seen < hint_count;
        let key = if i == 0 {
            key_span("status")
        } else if bridging {
            Span::styled(
                format!("│{}", " ".repeat(KEY_W + KEY_GUTTER - 1)),
                theme::line(),
            )
        } else {
            Span::raw(" ".repeat(KEY_W + KEY_GUTTER))
        };
        let mut spans = vec![key];
        spans.extend(row.content);
        lines.push(Line::from(spans));
        if let Some(hint) = &row.hint {
            seen += 1;
            let more_follow = hint_count >= 2 && seen < hint_count;
            lines.extend(rail_hint_lines(hint, width, more_follow));
        }
    }
    lines
}

/// A detected Usage-tab diagnostic state paired with the config context that
/// shapes its fix. Pure input to [`diag_fix`]; render-only, no decision consumes
/// it (mirrors `fallback::blocked_reason`).
#[derive(Clone)]
enum UsageDiag {
    /// Operator disabled the account: the scheduler doesn't poll it at all.
    Disabled,
    /// Subscription canceled: the org dropped to `claude_free` and `/v1/messages`
    /// 403s, so the account can't serve at all — re-login won't fix it.
    Canceled,
    /// Switch-grade kick block. `auto_start` flips the fix (the flagship
    /// divergence): an auto_start account self-recovers, a manual one won't.
    KickSwitchGrade { auto_start: bool },
    /// Burst (non-switch-grade) kick 429 — pill + backoff only, no chain switch.
    KickBurst,
    /// Deep-slot stuck-429 distrust (#40). `throttler` names the endpoint that
    /// is throttling, keyed on the reading source that won: `anthropic` for an
    /// OAuth streak, a typed provider's lowercase display name or `the endpoint`
    /// for a third-party streak.
    Stuck429 { throttler: String },
    /// AUTH-1 quarantine.
    AuthBroken,
    /// Long window (7d, else the provider-marked monthly quota) at/over the
    /// hard cap; `monthly` names the monthly arm so the teach never calls a
    /// monthly-only block "weekly".
    WeeklyHard { monthly: bool },
    /// Billing member that spent its `max_auto_spend` budget.
    BudgetSpent,
    /// Armed to spend with no cap and no parking spot (DANGER).
    SpendUncapped,
    /// Serving last-known numbers (cached / endpoint-429).
    Stale,
    /// Transient (non-quarantining) refresh failure.
    RefreshFailing,
    /// Endpoint-shaped profile no leg will ever fetch (no OAuth pair, no
    /// third-party credential): the fetch row names the missing key instead
    /// of a dot or a historical verdict.
    NoKey,
    /// The usage probe's setup token was refused: only a new mint revives it.
    SetupTokenRefused,
}

/// The stuck-429 throttler name, keyed on the reading source that won (never
/// the row type): an OAuth streak reads `anthropic`, a third-party streak reads
/// the typed provider's lowercase display name or `the endpoint` for a generic
/// api-key base URL.
fn throttler_name(profile: &Profile, source: RateLimitSource) -> String {
    match source {
        RateLimitSource::OAuth => "anthropic".to_string(),
        RateLimitSource::ThirdParty => match profile.provider {
            Some(p) => p.throttle_hint_name(),
            None => "the endpoint".to_string(),
        },
    }
}

/// The `└` fix text for a diagnostic state: what's wrong and the concrete fix,
/// varying with config. The `KickSwitchGrade` `auto_start` split is the flagship
/// (state, config) → hint divergence — an auto_start account self-recovers on
/// the poll-paced re-test (7bbeae4), a manual one sits until the ceiling.
fn diag_fix(diag: UsageDiag, profile_name: &str) -> String {
    match diag {
        UsageDiag::Disabled => "enable it on the setup tab".to_string(),
        UsageDiag::Canceled => "this subscription has been canceled".to_string(),
        UsageDiag::KickSwitchGrade { auto_start: true } => {
            "clauth is re-testing periodically".to_string()
        }
        UsageDiag::KickSwitchGrade { auto_start: false } => {
            "won't recover with auto-start off, enable it".to_string()
        }
        UsageDiag::KickBurst => "claude code hit a burst limit".to_string(),
        UsageDiag::Stuck429 { throttler } => format!("{throttler} is throttling usage reads"),
        UsageDiag::AuthBroken => {
            format!(
                "re-login with {}",
                cmd(&format!("clauth login {profile_name}"))
            )
        }
        UsageDiag::WeeklyHard { monthly } => {
            if monthly {
                "monthly quota is spent".to_string()
            } else {
                "weekly limit is spent".to_string()
            }
        }
        UsageDiag::BudgetSpent => "raise max spend on the fallback tab".to_string(),
        UsageDiag::SpendUncapped => crate::fallback::uncapped_spend_fix().to_string(),
        UsageDiag::Stale => "last usage check failed".to_string(),
        UsageDiag::RefreshFailing => "login refresh failing, re-login if it persists".to_string(),
        UsageDiag::NoKey => KEYLESS_FIX.to_string(),
        UsageDiag::SetupTokenRefused => format!(
            "mint a new one with {}",
            cmd(&format!("clauth login {profile_name} --setup-token"))
        ),
    }
}

/// Pill style for a consecutive-failure streak. Amber while it may still be a
/// blip — the shown numbers are merely old, which is what `cached` already says
/// — and red once the streak passes the bound the daemon itself stops trusting
/// the reading at ([`ACTIVE_CAP_MAX_STREAK`], the boundary `is_stuck_rate_limited`
/// and `status.json`'s `stale` key on). Red is reserved across this app for "not
/// recovering on its own", the same claim `×` and `failed` make; a wifi blip must
/// not borrow it, or the red that means a dead login stops being read.
fn streak_style(streak: u32) -> Style {
    let base = if is_stuck_streak(streak) {
        theme::danger()
    } else {
        theme::warning()
    };
    base.add_modifier(Modifier::BOLD)
}

/// English ordinal (`1st`, `2nd`, `3rd`, `4th`, `11th`…) for the retry count.
fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Terminal message for an OAuth profile with nothing renderable. "loading"
/// only while a fetch can still land: a profile with neither a pair nor a
/// probed setup token is never scheduled, a disabled one is likewise skipped,
/// and a terminal `Failed` or a refused setup token already tried — mirror
/// `build_tp_rows`, never spin on "loading" forever (issue #2).
fn oauth_empty_msg(profile: &Profile, status: Option<FetchStatus>) -> &'static str {
    let probed = profile.has_long_lived_sidecar && crate::usage::usage_probe_admits(profile);
    if crate::usage::usage_poll_pair(profile).is_none() && !probed {
        // A long-lived session-token sidecar is a login the OAuth leg cannot
        // read usage through: never call it "not logged in", tell the operator
        // to log the same account in or probe it, the login first: the probe
        // spends inference, the login does not. An api key / `[env]` inference
        // token has no usage windows at all — nothing to fix, so no fix line.
        // Only a member holding nothing is "not logged in".
        if profile.has_long_lived_sidecar {
            "can't read usage, log in the same account with + login on the setup tab, or turn on usage probe"
        } else if crate::claude::has_inference_auth(profile) {
            "can't read usage, an api key has no usage windows"
        } else {
            "not logged in, use + login on the setup tab"
        }
    } else if profile.is_disabled()
        || matches!(status, Some(FetchStatus::Failed | FetchStatus::AuthExpired))
    {
        // Disabled: never scheduled, so with no seeded cache no fetch will land.
        "no usage available"
    } else {
        "loading"
    }
}

/// The OAuth empty-body terminal messages, wrapped through the shared greedy
/// wrapper like the third-party arm's (`build_tp_rows`) — stack, don't
/// truncate. The OAuth body keeps its 2-cell indent on every segment, so the
/// wrap width is the interior minus the indent (`tooltip_lines`' lead shape),
/// or indent + segment would overrun the pane edge.
fn oauth_empty_lines(msg: &str, inner_w: u16) -> Vec<Line<'static>> {
    const INDENT_W: usize = 2;
    wrap_words(msg, usize::from(inner_w).saturating_sub(INDENT_W).max(8))
        .into_iter()
        .map(|seg| Line::from(Span::styled(format!("  {seg}"), theme::faint())))
        .collect()
}

/// Render provider-agnostic third-party stats. The header (plan + status) was
/// already pushed by the caller; only the stats body goes here.
///
/// `wallet_rate` is the funded wallet's burn figure (in-memory series), which
/// the balance row carries beside its value — the wallet sibling of the window
/// bars' `· rate` eyebrow section.
fn build_tp_rows(
    profile: &Profile,
    inner_w: u16,
    readings: &ReadingsSnapshot,
    show_estimates: bool,
    show_pace: bool,
    reset_fmt: ResetFmt,
    wallet_rate: Option<&crate::usage::WalletRate>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(stats) = readings.stats_of(&profile.name) else {
        // No data yet. "loading" only while a fetch is pending or in flight; a
        // terminal Failed status means we tried and the provider has nothing to
        // show, and a RateLimited one means the provider throttled us with
        // nothing cached — never spin on "loading" forever (the original z.ai bug).
        // A disabled profile is never scheduled either, so it never loads.
        let msg = if profile.is_disabled() {
            "no usage available"
        } else {
            match readings.status_of(&profile.name) {
                Some(FetchStatus::Failed) => "no usage available",
                Some(FetchStatus::RateLimited) => "rate limited, retrying",
                // The ways a provider's usage credential can be unusable read
                // differently to the operator, and the profile itself says
                // which one this is: a lapsed or never-captured console session
                // (Alibaba), or a dead api key (any other provider, whose
                // verdict only a 401 can produce).
                Some(FetchStatus::AuthExpired) if profile.console.is_some() => {
                    concat!("console login expired, run ", cmd_lit!("clauth login"))
                }
                Some(FetchStatus::AuthExpired) if profile.provider != Some(Provider::Alibaba) => {
                    "api key rejected, re-enter it on the setup tab"
                }
                Some(FetchStatus::AuthExpired) => {
                    concat!("console login needed, run ", cmd_lit!("clauth login"))
                }
                // A profile no leg will ever fetch must not claim to be
                // loading — the same rule `oauth_empty_msg` applies. An Alibaba
                // profile is never in here: its quota runs on the console
                // session, so it is scheduled with or without an api key.
                _ if !crate::usage::third_party_credentialed(profile) => KEYLESS_FIX,
                _ => "loading",
            }
        };
        // Stack, don't truncate: the fix copy must survive a narrow pane, so
        // every terminal message routes through the shared greedy wrapper
        // (`panes::wrap_words`) instead of clipping its tail at the pane edge.
        lines.extend(
            prose::wrap(msg, usize::from(inner_w), theme::faint())
                .into_iter()
                .map(Line::from),
        );
        return lines;
    };

    let has_bars = !stats.bars.is_empty();

    // Percentage windows → bars rendered through the same `Stat::render` path as
    // OAuth window bars (near-full-width bar + two-line eyebrow), using each bar's
    // API-provided label in source order and showing absolute `used / total` on
    // the eyebrow just before the %.
    if has_bars {
        let bar_stats = stats_from_bars(&stats.bars, show_estimates, show_pace, reset_fmt);
        lines.extend(render_stat_block(&bar_stats, inner_w));
    }

    // Text rows (e.g. z.ai per-model token totals, DeepSeek balances) render
    // below the bars. A provider can carry both.
    if !stats.rows.is_empty() {
        if has_bars {
            lines.push(Line::from(""));
        }
        // The widest FIGURE value, so every figure (Body, Danger, and the
        // z.ai `total` row's leading count) left-pads to it and the right
        // edges align (the contract's numeric-column rule); a word or no-data
        // glyph starts at the value column unpadded.
        let value_w = stats
            .rows
            .iter()
            .filter(|r| {
                !r.label.is_empty()
                    && matches!(
                        r.kind,
                        StatRowKind::Body | StatRowKind::Danger | StatRowKind::Faint
                    )
            })
            .filter(|r| is_row_figure(r))
            .map(|r| Span::raw(&row_parts(r).0).width())
            .max()
            .unwrap_or(0);
        for row in &stats.rows {
            if row.label.is_empty() {
                let style = match row.kind {
                    StatRowKind::Danger => theme::danger(),
                    _ => theme::faint(),
                };
                lines.push(Line::from(Span::styled(row.value.to_string(), style)));
            } else if row.kind == StatRowKind::Heading {
                lines.push(Line::from(Span::styled(
                    row.label.to_string(),
                    theme::label(),
                )));
            } else {
                let style = match row.kind {
                    StatRowKind::Danger => theme::danger(),
                    StatRowKind::Faint => theme::faint(),
                    _ => theme::body(),
                };
                let (lead, tail) = row_parts(row);
                let pad = if matches!(
                    row.kind,
                    StatRowKind::Body | StatRowKind::Danger | StatRowKind::Faint
                ) && is_row_figure(row)
                {
                    value_w
                } else {
                    0
                };
                let mut spans = key_value_span(&row.label, &lead, style, pad);
                // The `total` row's `(N calls)` suffix trails outside the figure
                // column, after its leading count has padded into it.
                if let Some(tail) = tail {
                    spans.push(Span::styled(tail, style));
                }
                // The rate rides only the funded wallet's own row — matched on
                // (label, currency), since a two-wallet provider lists both
                // under the same label with different currencies.
                if let Some(rate) = wallet_rate.filter(|r| {
                    r.label == row.label
                        && crate::providers::parse_balance(&row.value)
                            .is_some_and(|(currency, _)| currency == r.currency)
                }) {
                    spans.push(Span::styled(" · ", theme::dim()));
                    spans.push(Span::styled(
                        format!("~{:.1} {}/day", rate.per_day, rate.currency),
                        theme::faint(),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    } else if !has_bars {
        let (msg, style) = if stats.is_available {
            ("no stats reported", theme::faint())
        } else {
            ("usage unavailable", theme::danger())
        };
        lines.push(Line::from(Span::styled(msg, style)));
    }

    // Best-effort (unknown-provider) data is mapped heuristically — invite a
    // report so a real integration can be added. Subtle, below everything.
    if stats.best_effort && (has_bars || !stats.rows.is_empty()) {
        lines.push(Line::from(""));
        // Stack, don't truncate: the URL is the only in-app report pointer,
        // so the footer routes through the shared greedy wrapper like the
        // terminal arms above — a mid-URL clip at the 41-cell narrow
        // interior would send the report nowhere.
        lines.extend(
            wrap_words(
                "looks wrong? report at github.com/uwuclxdy/clauth/issues",
                usize::from(inner_w),
            )
            .into_iter()
            .map(|seg| Line::from(Span::styled(seg, theme::faint()))),
        );
    }

    lines
}

/// Build render [`Stat`]s from third-party percentage bars: each bar keeps its
/// API-provided label and source order (no inferred window vocabulary, no
/// reordering), shows absolute `used / total` on the eyebrow before the %, and a
/// reset countdown on the bar line. Rendered through the same `Stat::render` path
/// as OAuth window bars, so the two are visually identical.
///
/// A bar with a `<n>h`/`<n>d` label and a reset stamp also gets the OAuth window
/// predictions: a window-anchored average pace (`<n>d` → %/d, sub-day → %/h), a
/// burn ETA, and the ideal-pace marker — gated by the same `show_estimates` /
/// `show_pace` toggles. Providers never rotate, so the window-average pace is a
/// stable read (no recency-weighted history is kept for third-party accounts).
fn stats_from_bars(
    bars: &[crate::providers::UsageBar],
    show_estimates: bool,
    show_pace: bool,
    reset_fmt: ResetFmt,
) -> Vec<Stat> {
    let now = now_epoch_secs();
    let gates = WindowGates {
        show_estimates,
        show_pace,
    };
    bars.iter()
        .map(|bar| {
            let rem = window_remaining(bar, now);
            make_window_stat(
                &bar.label,
                bar.pct,
                bar.resets_at.as_deref(),
                now,
                bar_amount(bar),
                bar_reset_trailing(rem, reset_fmt),
                gates,
            )
        })
        .collect()
}

/// Seconds until `bar` resets (may be negative if overdue). `None` when the bar
/// carries no reset stamp — its window length is then unknown.
fn window_remaining(bar: &crate::providers::UsageBar, now: i64) -> Option<i64> {
    let reset = crate::usage::iso_to_epoch_secs(bar.resets_at.as_deref()?)?;
    Some(reset - now)
}

/// Bar-line trailing: the reset countdown (`  resets in …`), or empty when the
/// bar carries no future reset. The absolute amount now lives on the eyebrow.
fn bar_reset_trailing(rem: Option<i64>, reset_fmt: ResetFmt) -> String {
    match rem.filter(|&s| s > 0) {
        Some(secs) => format!("  {}", reset_phrase(secs, reset_fmt)),
        None => String::new(),
    }
}

/// Eyebrow amount for a bar: `used / total` when both are present, else empty.
fn bar_amount(bar: &crate::providers::UsageBar) -> String {
    match (bar.used, bar.total) {
        (Some(used), Some(total)) => format!(
            "{} / {}",
            crate::format::format_amount(used),
            crate::format::format_amount(total)
        ),
        _ => String::new(),
    }
}

/// Key column width for third-party stat rows (wider than `KEY_W` to fit the
/// longest label — DeepSeek's `api balance` — plus a 1-space gap).
const TP_KEY_W: usize = 11;

/// A labelled stat row's rendered value split for the figure column: the
/// leading figure (the number the column measures and pads) and, for a `Calls`
/// row, the trailing `  (N calls)` suffix that stays outside the column. The
/// full grouped figure comes from the raw count when the producer carried one
/// (z.ai's token totals), else the row's pre-formatted `value`.
fn row_parts(row: &StatRow) -> (String, Option<String>) {
    match &row.raw {
        Some(StatRowRaw::Count { value }) => {
            (crate::format::group_thousands(*value as f64, 0), None)
        }
        Some(StatRowRaw::Calls { value, calls }) => (
            crate::format::group_thousands(*value as f64, 0),
            Some(format!(
                "  ({} calls)",
                crate::format::group_thousands(*calls as f64, 0)
            )),
        ),
        None => (row.value.clone(), None),
    }
}

/// Whether a labelled row's value reads as a figure — a raw-carrying row's
/// count is always one; a value-only row is one when its text opens with a
/// digit (or `-` + digit) and holds no `·` / `(`.
fn is_row_figure(row: &StatRow) -> bool {
    match &row.raw {
        Some(_) => true,
        None => is_figure(&row.value),
    }
}

/// Whether a stat-row value reads as a figure — one number (optionally signed)
/// with an optional unit/currency word — rather than a word (`valid`) or a
/// no-data glyph (`-`, `—`), or a `·`-joined / parenthesized composite. Figure
/// rows left-pad to the block's widest figure; everything else starts at the
/// value column unpadded. Residuals the predicate cannot separate without a
/// producer to distinguish them: a leading-currency `$5` reads as a word and an
/// ISO date (`2026-10-01`) as a figure; no current producer emits either shape.
fn is_figure(value: &str) -> bool {
    let mut chars = value.chars();
    let digit_led = match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('-') => chars.next().is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    };
    digit_led && !value.contains('·') && !value.contains('(')
}

fn key_value_span(
    key: &str,
    value: &str,
    value_style: Style,
    value_w: usize,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!("  {}", key_cell(key, TP_KEY_W, KEY_GUTTER)),
        theme::faint(),
    )];
    let pad = value_w.saturating_sub(Span::raw(value).width());
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(value.to_string(), value_style));
    spans
}

fn key_span(key: &str) -> Span<'static> {
    Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::label())
}

/// The value column the `notes:` row opens at. `notes:` (6) is shorter than
/// `KEY_W` (8), so the key cell always pads to `KEY_W + KEY_GUTTER`.
const NOTES_VALUE_LEAD: usize = KEY_W + KEY_GUTTER;

/// The `notes:` row: the faint add-hint while the account has none, the full
/// note wrapped to the value column once it does. Copy is cloudy's (lowercase
/// `notes:`, full-note display); styling is contract-clean — accent key, faint
/// hint, no italic (the contract reserves italic for titles).
fn notes_lines(note: Option<&str>, inner_w: u16) -> Vec<Line<'static>> {
    let value_w = (inner_w as usize).saturating_sub(NOTES_VALUE_LEAD).max(8);
    let pad = " ".repeat(NOTES_VALUE_LEAD);
    match note.filter(|t| !t.is_empty()) {
        None => {
            let lead = Span::styled(key_cell("notes:", KEY_W, KEY_GUTTER), theme::accent());
            let hint = prose::wrap(
                concat!("press ", key_lit!("n"), " to add notes"),
                value_w,
                theme::faint(),
            );
            let mut lines = Vec::new();
            for (j, seg) in hint.into_iter().enumerate() {
                if j == 0 {
                    let mut spans = vec![lead.clone()];
                    spans.extend(seg);
                    lines.push(Line::from(spans));
                } else {
                    let mut spans = vec![Span::raw(pad.clone())];
                    spans.extend(seg);
                    lines.push(Line::from(spans));
                }
            }
            lines
        }
        Some(text) => {
            let mut lines = Vec::new();
            for (i, note_line) in text.split('\n').enumerate() {
                for (j, seg) in wrap_words(note_line, value_w).into_iter().enumerate() {
                    let value = Span::styled(seg, theme::body());
                    if i == 0 && j == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(key_cell("notes:", KEY_W, KEY_GUTTER), theme::accent()),
                            value,
                        ]));
                    } else {
                        lines.push(Line::from(vec![Span::raw(pad.clone()), value]));
                    }
                }
            }
            lines
        }
    }
}

/// The note editor docks at the detail pane's bottom with this many draft rows
/// (the contract's auto-grow 1→8) before the draft scrolls inside.
const NOTE_SLOT_MAX_ROWS: usize = 8;

/// The note editor's docked slot: a `╭─ ✎ ───╮` sub-block, LINE_STRONG border,
/// the draft inside, the native cursor at the caret, and the draft scrolling
/// cursor-first once it passes [`NOTE_SLOT_MAX_ROWS`]. The caret is measured
/// in display cells; a line wider than the slot scrolls the draft sideways
/// with the caret ([`follow_window`]), every row through the same window.
fn draw_note_slot(frame: &mut Frame<'_>, area: Rect, buf: &NoteBuffer, toast_rects: &[Rect]) {
    let block = edit_slot_block();
    let slot_inner = block.inner(area);
    frame.render_widget(block, area);
    let (crow, caret_byte) = buf.caret();
    let width = slot_inner.width as usize;
    let (cells, caret) = grapheme_cells_before(buf.line(crow), caret_byte);
    let start = follow_window(&cells, caret, buf.offset.get(), width, 0);
    buf.offset.set(start);
    let ccol = cells
        .iter()
        .take(caret)
        .sum::<usize>()
        .saturating_sub(start);
    let empty = buf.row_count() == 1 && buf.line(0).is_empty();
    let lines: Vec<Line<'static>> = if empty {
        vec![Line::from(Span::styled("add a note…", theme::faint()))]
    } else {
        (0..buf.row_count())
            .map(|i| {
                Line::from(Span::styled(
                    cells_window(buf.line(i), start, width),
                    theme::body(),
                ))
            })
            .collect()
    };
    let viewport = slot_inner.height as usize;
    let total = lines.len();
    let offset = scroll_offset(total, viewport, (crow, crow + 1));
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((offset as u16, 0)),
        slot_inner,
    );
    draw_scrollbar(frame, slot_inner, total, offset, viewport);
    let x = slot_inner.x + (ccol as u16).min(slot_inner.width.saturating_sub(1));
    let vis_row = crow.checked_sub(offset).unwrap_or(usize::MAX);
    if vis_row < slot_inner.height as usize {
        place_caret(
            frame,
            toast_rects,
            Position::new(x, slot_inner.y + vis_row as u16),
        );
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_usage.rs"]
mod tests;
