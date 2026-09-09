//! Top bar: claude glyph on the left; brand and account count in the text
//! column to the right. Three rows always — [`header_height`] keeps
//! `render::draw`'s layout in step.
//!
//! The active-profile usage gauge sits on row 1 to the right of the account
//! count, separated by a middle dot. The collapse ladder drops the usage bar
//! before the name.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Tab};
use super::super::theme;
use super::format::{bar_string_with_cells, fixed_split};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Account gauge ────────────────────────────────────────────────────────
//
// `name  [███░░░░░] 38%` for the active profile's 5h window. Collapse
// ladder sacrifices the bar before the name (bar shrinks → bar drops →
// name truncates → name drops → hide).

const GAUGE_NAME_GAP: usize = 2;
const GAUGE_BAR_FULL: usize = 10;
const GAUGE_BAR_MIN: usize = 3;
const GAUGE_NAME_MAX: usize = 16;
const GAUGE_NAME_MIN: usize = 3;
const GAUGE_PCT_W: usize = 4;
// ── Account-name pulse ───────────────────────────────────────────────────

const PULSE_SWEEP_MS: u64 = 900;
const PULSE_REST_MS: u64 = 1700;
const PULSE_DEPTH: f32 = 0.4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GaugeFit {
    name_w: usize,
    bar_cells: usize,
    visible: bool,
}

impl GaugeFit {
    const HIDDEN: GaugeFit = GaugeFit {
        name_w: 0,
        bar_cells: 0,
        visible: false,
    };
}

struct ActiveGauge {
    name: String,
    pct: Option<f64>,
}

fn active_gauge(app: &App) -> Option<ActiveGauge> {
    let cfg = app.config();
    let name = cfg.state.active_profile.as_ref()?;
    let profile = cfg.find(name)?;
    let pct = if profile.is_oauth() {
        profile
            .usage
            .as_ref()
            .and_then(|u| u.five_hour.as_ref())
            .map(|w| w.utilization.clamp(0.0, 100.0))
    } else {
        None
    };
    Some(ActiveGauge {
        name: name.to_string(),
        pct,
    })
}

fn gauge_tail_w(has_pct: bool) -> usize {
    if has_pct { GAUGE_PCT_W } else { 0 }
}

fn gauge_total_w(name_w: usize, bar_cells: usize, has_pct: bool) -> usize {
    let mut w = gauge_tail_w(has_pct);
    if bar_cells > 0 {
        w += bar_cells + 2 + 1;
    }
    if name_w > 0 {
        w += name_w + GAUGE_NAME_GAP;
    }
    w
}

/// Collapse ladder: bar shrinks → bar drops → name truncates → name drops → hide.
fn gauge_fit(avail: usize, name_len: usize, has_pct: bool) -> GaugeFit {
    if avail < gauge_tail_w(has_pct) {
        return GaugeFit::HIDDEN;
    }
    let mut name_w = name_len.min(GAUGE_NAME_MAX);
    let mut bar_cells = if has_pct { GAUGE_BAR_FULL } else { 0 };
    let name_floor = GAUGE_NAME_MIN.min(name_len);

    while gauge_total_w(name_w, bar_cells, has_pct) > avail && bar_cells > GAUGE_BAR_MIN {
        bar_cells -= 1;
    }
    if bar_cells > 0 && gauge_total_w(name_w, bar_cells, has_pct) > avail {
        bar_cells = 0;
    }
    while gauge_total_w(name_w, bar_cells, has_pct) > avail && name_w > name_floor {
        name_w -= 1;
    }
    if name_w > 0 && gauge_total_w(name_w, bar_cells, has_pct) > avail {
        name_w = 0;
    }
    if gauge_total_w(name_w, bar_cells, has_pct) > avail {
        return GaugeFit::HIDDEN;
    }
    if !has_pct && name_w == 0 {
        return GaugeFit::HIDDEN;
    }
    GaugeFit {
        name_w,
        bar_cells,
        visible: true,
    }
}

fn gauge_spans(fit: GaugeFit, name: &str, pct: Option<f64>, elapsed_ms: u64) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if fit.name_w > 0 {
        let (nt, _pad) = fixed_split(name, fit.name_w);
        let style = Style::default().fg(theme::text_color());
        spans.extend(pulse_name_spans(&nt, style, elapsed_ms));
        spans.push(Span::raw("  "));
    }
    match pct {
        Some(pct) if fit.bar_cells > 0 => {
            let style = Style::default().fg(theme::util_color(pct));
            spans.push(Span::styled("[", theme::dim()));
            spans.push(Span::styled(
                bar_string_with_cells(pct, fit.bar_cells),
                style,
            ));
            spans.push(Span::styled("]", theme::dim()));
            spans.push(Span::styled(format!(" {pct:.0}%"), style));
        }
        Some(pct) => {
            spans.push(Span::styled(
                format!("{pct:.0}%"),
                Style::default().fg(theme::util_color(pct)),
            ));
        }
        None => {}
    }
    spans
}

pub(super) fn pulse_name_spans(name: &str, style: Style, elapsed_ms: u64) -> Vec<Span<'static>> {
    use std::f32::consts::{PI, TAU};
    let plain = || vec![Span::styled(name.to_string(), style)];
    if theme::tier() != theme::Tier::Full {
        return plain();
    }
    let t = (elapsed_ms % (PULSE_SWEEP_MS + PULSE_REST_MS)) as f32;
    let sweep = PULSE_SWEEP_MS as f32;
    if t >= sweep {
        return plain();
    }
    let progress = t / sweep;
    let envelope = (PI * progress).sin();
    let head = progress * TAU;
    let len = name.chars().count().max(1) as f32;
    let base = style.fg.unwrap_or_else(theme::text_color);
    name.chars()
        .enumerate()
        .map(|(i, ch)| {
            let col = (i as f32 / len) * TAU;
            let crest = ((col - head).cos() * 0.5 + 0.5).powi(2);
            let lean = f64::from(crest * envelope * PULSE_DEPTH);
            let fg = theme::blend_over(base, theme::accent_2_color(), lean);
            Span::styled(ch.to_string(), style.fg(fg))
        })
        .collect()
}

// ── Height ───────────────────────────────────────────────────────────────

pub(super) fn header_height(_app: &App) -> u16 {
    3
}

// ── Draw ─────────────────────────────────────────────────────────────────

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols: [Rect; 2] =
        Layout::horizontal([Constraint::Length(10), Constraint::Min(20)]).areas(area);

    draw_logo(frame, cols[0], app);

    let rows: [Rect; 3] = Layout::vertical([Constraint::Length(1); 3]).areas(cols[1]);

    let n = app.config().profiles.len();
    let info_width = rows[0].width as usize;

    let gauge = if app.tab == Tab::Overview || app.compact {
        None
    } else {
        active_gauge(app)
    };

    // ── Row 0: brand [· ● daemon] ......... version ───────────────────────
    let brand = "clauth";
    let ver = format!("v{VERSION}");
    let mut row0: Vec<Span<'static>> = vec![Span::styled(
        brand,
        Style::default().fg(theme::text_color()).bold(),
    )];
    // `● daemon` health dot, mirroring the row-1 `status.claude.ai` dot. Hidden
    // when no daemon runs (the TUI self-fetches under its own lease).
    if let Some(color) = daemon_dot_color(app) {
        row0.push(Span::raw("  "));
        row0.push(Span::styled("● ", Style::default().fg(color)));
        row0.push(Span::styled("daemon", theme::dim()));
    }
    let mut used: usize = row0.iter().map(|s| s.content.chars().count()).sum();
    // `[ herdr ]` context tag, between the brand and the daemon dot. The tag is
    // the one span this row can shed: it renders only while brand + tag +
    // daemon + version all fit, so the version stays right-aligned and nothing
    // new clips at narrow widths (without the tag the row is byte-identical to
    // a non-herdr launch at every width).
    if app.herdr_mode {
        let tag = "  [ herdr ]";
        if used + tag.chars().count() + ver.chars().count() <= info_width {
            row0.insert(1, Span::styled(tag, theme::dim()));
            used += tag.chars().count();
        }
    }
    let gap = info_width.saturating_sub(used + ver.chars().count());
    row0.push(Span::styled(" ".repeat(gap), theme::base()));
    row0.push(Span::styled(ver, theme::dim()));
    frame.render_widget(
        Paragraph::new(Line::from(row0)).style(theme::base()),
        rows[0],
    );

    // ── Row 1: N accounts · [gauge] ... ● status.claude.ai ──────────────
    // The count + gauge are left-aligned together; the status dot is the
    // only thing right-aligned, with an elastic gap in between.
    let row1_width = rows[1].width as usize;
    let prefix = format!("{n} account{}", crate::format::plural(n));
    let feed = "status.claude.ai";
    let status_head = "● ";
    let status_w = status_head.chars().count() + feed.chars().count();
    let reserve: usize = 1;

    let mut left_spans: Vec<Span<'static>> = vec![Span::styled(prefix, theme::faint())];
    if let Some(ref g) = gauge {
        // The ` · ` separator is budgeted here but only rendered when the gauge
        // survives the fit — a hidden gauge must not leave a dangling dot.
        let sep = " · ";
        let gauge_budget = row1_width.saturating_sub(
            left_spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
                + sep.chars().count()
                + status_w
                + reserve,
        );
        let fit = gauge_fit(gauge_budget, g.name.chars().count(), g.pct.is_some());
        if fit.visible {
            left_spans.push(Span::styled(sep.to_string(), theme::faint()));
            left_spans.extend(gauge_spans(fit, &g.name, g.pct, app.anim_ms()));
        }
    }
    let left_w: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let mut row1_spans = left_spans;
    if row1_width >= left_w + status_w + reserve {
        let gap = row1_width - left_w - status_w;
        row1_spans.push(Span::raw(" ".repeat(gap)));
    }
    row1_spans.push(Span::styled(
        status_head,
        Style::default().fg(status_dot_color(app)),
    ));
    row1_spans.push(Span::styled(feed, theme::dim()));

    frame.render_widget(
        Paragraph::new(Line::from(row1_spans)).style(theme::base()),
        rows[1],
    );

    // ── Row 2: tabs ──────────────────────────────────────────────────────
    super::tabs::draw(frame, rows[2], app);
}

fn status_dot_color(app: &App) -> ratatui::style::Color {
    use crate::status::Impact;
    match app.status.worst_active_impact() {
        Impact::Critical | Impact::Major => theme::danger_color(),
        Impact::Minor => theme::warning_color(),
        Impact::Maintenance => theme::text_dim_color(),
        Impact::None | Impact::Other(_) => theme::success_color(),
    }
}

/// `● daemon` header-dot color, or `None` to hide it when no daemon runs.
/// Mirrors [`status_dot_color`]: green = daemon up + fresh feed, amber = up but
/// its `status.json` is stale (wedging / pre-abort / just booted).
fn daemon_dot_color(app: &App) -> Option<ratatui::style::Color> {
    use crate::daemon::DaemonHealth;
    match app.daemon_health {
        DaemonHealth::Absent => None,
        DaemonHealth::Stale => Some(theme::warning_color()),
        DaemonHealth::Fresh => Some(theme::success_color()),
    }
}

fn draw_logo(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let blink = (app.anim_ms() % 6000) < 200;

    let style = Style::default().fg(theme::accent_2_color());

    let logo_top = if blink {
        " ▐█████▌ "
    } else {
        " ▐▛███▜▌ "
    };
    let logo_mid = "▝▜█████▛▘";
    let logo_eyes = "  ▘▘ ▝▝  ";

    let lines = vec![
        Line::from(Span::styled(logo_top, style)).alignment(Alignment::Left),
        Line::from(Span::styled(logo_mid, style)).alignment(Alignment::Left),
        Line::from(Span::styled(logo_eyes, style)).alignment(Alignment::Left),
    ];

    let para = Paragraph::new(lines).style(theme::base());
    frame.render_widget(para, area);
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_header.rs"]
mod gauge_tests;

// Herdr-mode row-0 pins: the `[ herdr ]` tag, its shed order at narrow
// widths, and the byte-identical plain launch.
#[cfg(test)]
#[path = "../../../tests/inline/tui_render_header_herdr.rs"]
mod herdr_mode_tests;
