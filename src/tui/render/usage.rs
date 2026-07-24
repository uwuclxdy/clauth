//! Usage tab — account picker on the left, the selected account's full usage
//! breakdown on the right: a header (plan, active marker, per-account refresh
//! status / countdown), then every window, reset timers, and extra credits.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::App;
use super::super::theme;
use super::format::{
    ResetFmt, activity_verb, is_past_reset, reset_in_secs, reset_phrase, spinner_frame,
    spinner_style,
};
use super::panes::{
    DIAG_AUTH_BROKEN, DIAG_BUDGET_SPENT, DIAG_CANCELED, DIAG_DISABLED, DIAG_KICK, active_pill,
    draw_profile_selector, empty_state, key_cell, master_detail, pill, rail_hint_lines,
    section_box, section_box_verbatim,
};
use crate::format::{endpoint_label, plan_label};
use crate::profile::Profile;
use crate::providers::StatRowKind;
use crate::usage::{
    ExtraPeriod, FetchStatus, KickBlock, ProfileActivity, StreakCounts, UsageWindow, WindowDollars,
    ideal_pace_pct, is_stuck_streak, kick_block_switch_grade, now_epoch_secs, now_ms,
};

const KEY_W: usize = 8;
/// Fixed gap between the padded key and the value column (house standard).
const KEY_GUTTER: usize = 2;

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
    /// Whether this profile holds the live credentials — drives the plan row's
    /// `[ active ]` pill.
    is_active: bool,
    activity: ProfileActivity,
    next_refresh_ms: Option<u64>,
    tick: u64,
    /// The stored login's email (the identity anchor's readable half),
    /// mirroring the Setup tab's `account` row. OAuth-only; `None` until a
    /// login or the /profile fetch seeds it.
    account_email: Option<String>,
    /// Consecutive-failure counts for the shown profile (zeroed when absent).
    /// The retry suffix names which retry the countdown leads to, so a deep slot
    /// reads as stuck from the count alone, no judgment label.
    streaks: StreakCounts,
    /// Live kick-429 block for the shown profile: the messages endpoint is
    /// rejecting the 5h auto-start kick. Orthogonal to `fetch_status` — `/usage`
    /// can stay Fresh straight through the outage — so it earns its own pill.
    kick_block: Option<KickBlock>,
    /// Config-derived diagnostic flags driving the `└` fix hints.
    diag: DiagFlags,
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items = app.config().profiles.len();
    let (selector, detail) = master_detail(area, items);

    draw_profile_selector(frame, selector, app, app.profile_cursor, true);
    draw_usage_detail(frame, detail, app);
}

fn draw_usage_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Streak snapshot up front: POLL_STREAK (220) ranks below CONFIG
    // (400), so it can't be taken while `cfg` is held below.
    let streaks: HashMap<String, StreakCounts> = app
        .poll_streaks
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default();
    // Same discipline as streaks: KickBlockState (230) ranks below CONFIG (400).
    let kick_blocks: HashMap<String, KickBlock> = app
        .kick_blocks
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default();
    let cfg = app.config();
    let profile = cfg
        .profiles
        .get(app.profile_cursor.min(cfg.profiles.len().saturating_sub(1)));

    let title = profile.map(|p| p.name.as_str()).unwrap_or("usage");
    // Detail pane: read-only, focus never descends into it; second panel on screen.
    // Profile names preserve original case; the "usage" fallback stays uppercased.
    let block = if profile.is_some() {
        section_box_verbatim(title, false, false)
    } else {
        section_box(title, false, false)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(profile) = profile else {
        frame.render_widget(empty_state("no accounts yet", "n", "to create one"), inner);
        return;
    };

    // `config` (via `cfg`) is outer of activity/refresh-timer in lock order.
    let header = HeaderState {
        is_active: cfg.is_active(&profile.name),
        activity: app
            .activity
            .lock()
            .ok()
            .and_then(|g| g.get(profile.name.as_str()).copied())
            .unwrap_or(ProfileActivity::Idle),
        next_refresh_ms: app
            .next_refresh_per_profile
            .lock()
            .ok()
            .and_then(|m| m.get(profile.name.as_str()).copied()),
        tick: app.tick_count,
        // One tiny cached-file read, cursor profile only — the same per-frame
        // page-cache read the Setup tab's `account` row makes.
        account_email: profile
            .is_oauth()
            .then(|| {
                crate::profile_cache::load_profile_cache::<String>(
                    profile.name.as_str(),
                    crate::profile_cache::ACCOUNT_EMAIL_CACHE_FILE,
                )
            })
            .flatten(),
        streaks: streaks
            .get(profile.name.as_str())
            .copied()
            .unwrap_or_default(),
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
                weekly_hard: crate::fallback::weekly_hard_blocked(profile),
                budget_spent: crate::fallback::budget_spent_blocking(&cfg, profile),
                spend_uncapped: crate::fallback::spend_is_uncapped(&cfg, ceiling),
            }
        },
    };

    let show_estimates = cfg.state.show_estimates;
    let show_pace = cfg.state.show_pace;
    // Read off the guard already held here: `config` is a plain (non-reentrant)
    // mutex, so a second `app.config()` deeper in the render would self-deadlock.
    let reset_fmt = ResetFmt::from_state(&cfg.state);
    let lines = build_usage_lines(
        profile,
        inner.width,
        &header,
        app,
        show_estimates,
        show_pace,
        reset_fmt,
    );
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

fn build_usage_lines(
    profile: &Profile,
    inner_w: u16,
    header: &HeaderState,
    app: &App,
    show_estimates: bool,
    show_pace: bool,
    reset_fmt: ResetFmt,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(header_lines(profile, header, inner_w));
    lines.push(Line::from(""));

    // Api-key/provider accounts (recognised or generic) render via the third-party
    // rows/bars path; OAuth accounts — including OAuth run against a custom
    // base_url — fall through to their live window bars.
    if profile.api_key.is_some() || profile.is_third_party() {
        lines.extend(build_tp_rows(
            profile,
            inner_w,
            show_estimates,
            show_pace,
            reset_fmt,
        ));
        return lines;
    }

    if profile.usage.is_none() {
        lines.push(Line::from(Span::styled(
            format!("  {}", oauth_empty_msg(profile)),
            theme::faint(),
        )));
        return lines;
    }

    let mut stats = collect_stats(profile, reset_fmt);
    if stats.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", oauth_empty_msg(profile)),
            theme::faint(),
        )));
        return lines;
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
    if let Some(u) = profile.usage.as_ref() {
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
    lines
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
    pct: f64,
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
        if let Some(dur) = eta_left(rate, self.pct, self.rate_unit) {
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
        // Natural width — `header_pad` right-aligns the whole block to `pct_col`,
        // so the `%` lands in the same column every row without padding the
        // number. (A fixed `{:>3.0}` width added stray spaces after the amount.)
        let pct_str = format!("{:.0}%", self.pct);

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

        let filled = (((self.pct / 100.0) * bar_width as f64).round() as usize).min(bar_width);
        let marker = self.pace_pct.filter(|_| bar_width > 0).map(|p| {
            (((p.clamp(0.0, 100.0) / 100.0) * bar_width as f64).round() as usize).min(bar_width - 1)
        });

        let mut bar_line = bar_spans(filled, bar_width, self.color, marker);
        // Right-align trailing text to the same far column as %.
        if !self.trailing.is_empty() {
            let pad = pct_col
                .saturating_sub(bar_width)
                .saturating_sub(self.trailing.chars().count());
            if pad > 0 {
                bar_line.push(Span::raw(" ".repeat(pad)));
            }
            bar_line.push(Span::styled(self.trailing.clone(), theme::faint()));
        }

        let mut label_spans = vec![Span::styled(self.label.clone(), theme::label())];
        if let Some(rate) = self.burn_rate
            && rate > 0.0
        {
            let rate_color = Style::default().fg(theme::util_color(rate.clamp(0.0, 100.0)));
            label_spans.push(Span::styled(" · ", theme::dim()));
            let rate_str = format!("{:.1} %/{}", rate, self.rate_unit);
            label_spans.push(Span::styled(rate_str.clone(), rate_color));

            if let Some(eta_secs) = eta_left_secs(rate, self.pct, self.rate_unit) {
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

        vec![
            Line::from({
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
                spans.push(Span::styled(
                    pct_str,
                    self.color.add_modifier(Modifier::BOLD),
                ));
                spans
            }),
            Line::from(bar_line),
        ]
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
            crate::usage::window_avg_pace_per_day(label, &window, now, 3600).map(|per_day| {
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
        pct,
        color,
        trailing,
        amount,
        burn_rate,
        rate_unit,
        pace_pct,
        reset_secs,
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

fn collect_stats(profile: &Profile, reset_fmt: ResetFmt) -> Vec<Stat> {
    let Some(usage) = profile.usage.as_ref() else {
        return Vec::new();
    };
    let now_secs = now_epoch_secs();
    let mut stats: Vec<Stat> = Vec::new();
    for (label, w) in usage.windows() {
        let trailing = reset_in_secs(w)
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
    // `spend` is the newer, correctly-typed view of the same credit cap; when it
    // renders, the legacy `extra_usage` bar would just duplicate it. Fall back to
    // `extra` only for accounts that expose the legacy field but no `spend` block.
    let spend_shown = usage.spend.as_ref().is_some_and(|s| s.is_visible());
    if let Some(extra) = &usage.extra_usage
        && extra.is_enabled
        && !spend_shown
    {
        let pct = extra.utilization.unwrap_or(0.0).clamp(0.0, 100.0);
        let sym = match extra.currency.as_deref() {
            Some("USD") | None => "$",
            Some(other) => other,
        };
        // Legacy `extra_usage` reports money as bare minor units (cents); `spend`
        // carries the same figures already scaled to dollars. Divide to match.
        let used = extra.used_credits.unwrap_or(0.0) / 100.0;
        let limit = extra.monthly_limit.unwrap_or(0.0) / 100.0;
        stats.push(Stat {
            label: "extra".to_string(),
            pct,
            color: Style::default().fg(theme::util_color(pct)),
            // Credits used/limit ride the bar's trailing line, where window bars
            // show their reset countdown, so the eyebrow carries just the %.
            trailing: format!("{sym}{used:.2} / {sym}{limit:.2}"),
            amount: String::new(),
            burn_rate: None,
            rate_unit: "h",
            pace_pct: None,
            reset_secs: None,
        });
    }
    // Per-period extra-credit breakdowns (`daily`/`weekly`) — shape unconfirmed,
    // absent on every current account; rendered only when a value is extractable.
    if let Some(extra) = &usage.extra_usage {
        for (label, raw) in [
            ("extra usage (24h)", &extra.daily),
            ("extra usage (7d)", &extra.weekly),
        ] {
            let Some(period) = raw.as_ref().and_then(ExtraPeriod::from_value) else {
                continue;
            };
            let pct = period.utilization.unwrap_or(0.0).clamp(0.0, 100.0);
            let sym = match period.currency.as_deref().or(extra.currency.as_deref()) {
                Some("USD") | None => "$",
                Some(other) => other,
            };
            let cost = match (period.used_credits, period.monthly_limit) {
                (Some(u), Some(l)) => format!("{sym}{u:.2} / {sym}{l:.2}"),
                (Some(u), None) => format!("{sym}{u:.2}"),
                _ => String::new(),
            };
            stats.push(Stat {
                label: label.to_string(),
                pct,
                color: Style::default().fg(theme::util_color(pct)),
                trailing: cost,
                amount: String::new(),
                burn_rate: None,
                rate_unit: "h",
                pace_pct: None,
                reset_secs: None,
            });
        }
    }
    if let Some(spend) = &usage.spend
        && spend.is_visible()
    {
        let pct = spend.percent.unwrap_or(0.0).clamp(0.0, 100.0);
        let sym = match spend.currency.as_deref() {
            Some("USD") | None => "$",
            Some(other) => other,
        };
        let used = spend.used.unwrap_or(0.0);
        let cost = match spend.limit {
            Some(limit) => format!("{sym}{used:.2} / {sym}{limit:.2}"),
            None => format!("{sym}{used:.2}"),
        };
        stats.push(Stat {
            label: "spend".to_string(),
            pct,
            color: Style::default().fg(theme::util_color(pct)),
            trailing: cost,
            amount: String::new(),
            burn_rate: None,
            rate_unit: "h",
            pace_pct: None,
            reset_secs: None,
        });
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

fn header_lines(profile: &Profile, header: &HeaderState, inner_w: u16) -> Vec<Line<'static>> {
    let plan = profile
        .third_party_usage
        .as_ref()
        .and_then(|s| s.plan.clone())
        .or_else(|| {
            profile
                .usage
                .as_ref()
                .and_then(|u| u.plan.as_ref())
                .map(plan_label)
        })
        // With no fetched plan, show the OAuth token's tier (what the Overview
        // shows) instead of a bare "oauth"; api-key profiles keep "api".
        // `is_oauth` gates out the base-url branch of `endpoint_label`, so it
        // only ever resolves to a tier here, never a raw endpoint url.
        .unwrap_or_else(|| {
            if profile.is_oauth() {
                endpoint_label(profile)
            } else {
                "api".to_string()
            }
        });
    let mut plan_spans = vec![key_span("plan"), Span::styled(plan.clone(), theme::body())];
    if header.is_active {
        // "[ active ]" = 10 chars; left side = key block + plan chars; pad the gap.
        let left_w = KEY_W + KEY_GUTTER + plan.chars().count();
        let indicator_w = "[ active ]".chars().count(); // 10
        let pad = (inner_w as usize)
            .saturating_sub(left_w)
            .saturating_sub(indicator_w);
        plan_spans.push(Span::raw(" ".repeat(pad)));
        plan_spans.extend(active_pill());
    }

    let mut lines = vec![Line::from(plan_spans)];
    // Account row — which login this profile actually holds, so
    // which-account-is-this never needs forensics (Setup tab's sibling).
    if let Some(email) = header.account_email.as_deref() {
        lines.push(Line::from(vec![
            key_span("account"),
            Span::styled(email.to_string(), theme::dim()),
        ]));
    }
    lines.extend(status_lines(profile, header, inner_w));
    lines
}

/// One row of the `status` block paired with its optional `└`/`├` fix hint.
/// Collected before render so [`render_status_rows`] can see the total hint
/// count up front and connect 2+ into one rail instead of floating each `└`
/// detached (cloudy-tui Stacked hints).
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
fn status_lines(profile: &Profile, header: &HeaderState, inner_w: u16) -> Vec<Line<'static>> {
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
    if profile
        .usage
        .as_ref()
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
    match profile.fetch_status {
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
                UsageDiag::Stuck429
            } else {
                UsageDiag::Stale
            });
        }
        _ => match countdown {
            // A scheduled refresh is work lined up — the cloudy-tui `queued`
            // dot (`◌` in ACCENT), not a spinner: nothing is running yet.
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
                let resumes = profile
                    .usage
                    .as_ref()
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
                            fetch_hint = Some(UsageDiag::WeeklyHard);
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
/// col 2. A single hint stays the plain `└` form: nothing to connect (cloudy-tui
/// Stacked hints).
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
#[derive(Clone, Copy)]
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
    /// Deep-slot stuck-429 distrust (#40).
    Stuck429,
    /// AUTH-1 quarantine.
    AuthBroken,
    /// 7d window at/over the hard cap.
    WeeklyHard,
    /// Billing member that spent its `max_auto_spend` budget.
    BudgetSpent,
    /// Armed to spend with no cap and no parking spot (DANGER).
    SpendUncapped,
    /// Serving last-known numbers (cached / endpoint-429).
    Stale,
    /// Transient (non-quarantining) refresh failure.
    RefreshFailing,
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
        UsageDiag::Stuck429 => "anthropic is throttling usage reads".to_string(),
        UsageDiag::AuthBroken => format!("re-login with clauth login {profile_name}"),
        UsageDiag::WeeklyHard => "weekly limit is spent".to_string(),
        UsageDiag::BudgetSpent => "raise max spend on the fallback tab".to_string(),
        UsageDiag::SpendUncapped => crate::fallback::uncapped_spend_fix().to_string(),
        UsageDiag::Stale => "last usage check failed".to_string(),
        UsageDiag::RefreshFailing => "login refresh failing, re-login if it persists".to_string(),
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
/// only while a fetch can still land: a credential-less profile is never
/// scheduled (`collect_tokens` skips it), a disabled one is likewise skipped, and
/// a terminal `Failed` already tried — mirror `build_tp_rows`, never spin on
/// "loading" forever (issue #2).
fn oauth_empty_msg(profile: &Profile) -> &'static str {
    let has_oauth = profile
        .credentials
        .as_ref()
        .is_some_and(|c| c.claude_ai_oauth.is_some());
    if !has_oauth {
        "not logged in, use + login on the setup tab"
    } else if profile.is_disabled() || profile.fetch_status == Some(FetchStatus::Failed) {
        // Disabled: never scheduled, so with no seeded cache no fetch will land.
        "no usage available"
    } else {
        "loading"
    }
}

/// Render provider-agnostic third-party stats. The header (plan + status) was
/// already pushed by the caller; only the stats body goes here.
fn build_tp_rows(
    profile: &Profile,
    inner_w: u16,
    show_estimates: bool,
    show_pace: bool,
    reset_fmt: ResetFmt,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let Some(stats) = profile.third_party_usage.as_ref() else {
        // No data yet. "loading" only while a fetch is pending or in flight; a
        // terminal Failed status means we tried and the provider has nothing to
        // show, and a RateLimited one means the provider throttled us with
        // nothing cached — never spin on "loading" forever (the original z.ai bug).
        // A disabled profile is never scheduled either, so it never loads.
        let msg = if profile.is_disabled() {
            "no usage available"
        } else {
            match profile.fetch_status {
                Some(FetchStatus::Failed) => "no usage available",
                Some(FetchStatus::RateLimited) => "rate limited, retrying",
                _ => "loading",
            }
        };
        lines.push(Line::from(Span::styled(msg, theme::faint())));
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
                lines.push(Line::from(key_value_span(&row.label, &row.value, style)));
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
        lines.push(Line::from(Span::styled(
            "looks wrong? report at github.com/uwuclxdy/clauth/issues",
            theme::faint(),
        )));
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
        (Some(used), Some(total)) => format!("{} / {}", fmt_amount(used), fmt_amount(total)),
        _ => String::new(),
    }
}

fn fmt_amount(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.2}")
    }
}

/// Key column width for third-party stat rows (wider than `KEY_W` to fit
/// labels like "topped up" plus a 1-space gap).
const TP_KEY_W: usize = 10;

fn key_value_span(key: &str, value: &str, value_style: Style) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!("  {}", key_cell(key, TP_KEY_W, KEY_GUTTER)),
        theme::faint(),
    )];
    spans.push(Span::styled(value.to_string(), value_style));
    spans
}

fn key_span(key: &str) -> Span<'static> {
    Span::styled(key_cell(key, KEY_W, KEY_GUTTER), theme::label())
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_usage.rs"]
mod tests;
