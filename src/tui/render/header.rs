//! Top bar: claude glyph on the left; brand, usage gauge and status source in
//! the text column to the right. Three rows always — [`header_height`] keeps
//! `render::draw`'s layout in step.
//!
//! Row 0 reads `clauth vX.Y.Z` on the left with the herdr tag between them,
//! and the `[ shunt ]  [ daemon ]` service chips on the right edge; it sheds
//! the tag first, then the shunt chip, then the daemon chip. Row 1 carries
//! the fleet's live-session count, the active-profile usage gauge and the
//! status indicator, nothing else: the account counts and the harness filter
//! live on the accounts panel's title row, as meta slots beside the bare
//! `ACCOUNTS` title. Row 1 sheds the live count first; the gauge's ladder
//! drops the usage bar before the name.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Health, Tab, gateway_health, live_count_text};
use super::super::theme;
use super::format::{bar_string_with_cells, fixed_split};
use super::services::health_color;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Account gauge ────────────────────────────────────────────────────────
//
// `name 38% [███░░░░░]` for the active profile's 5h window — cloudy's pick
// (2026-10-07): the percent leads the bar inside the gauge rung, single-spaced
// throughout, a sanctioned house deviation. The bar keeps its sanctioned
// brackets and they drop with it; the collapse ladder sacrifices the bar
// before the name (bar shrinks → bar and brackets drop → name truncates →
// name drops → hide).

const GAUGE_BAR_FULL: usize = 10;
const GAUGE_BAR_MIN: usize = 3;
const GAUGE_NAME_MAX: usize = 16;
const GAUGE_NAME_MIN: usize = 3;
const GAUGE_PCT_W: usize = 4;
/// Flush bar brackets: `[` + `]`.
const GAUGE_BAR_CHROME_W: usize = 2;
/// Every gauge gap — name → percent, percent → bar, bare rung — 1 cell.
const GAUGE_GAP: usize = 1;
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

/// Whole-gauge width at a rung. While the bar renders, the percent leads it
/// (`name` pct ` [bar]`, single-spaced); the brackets drop with the bar,
/// leaving the bare `name` pct rung.
fn gauge_total_w(name_w: usize, bar_cells: usize, has_pct: bool) -> usize {
    let mut w = gauge_tail_w(has_pct);
    if bar_cells > 0 {
        w += bar_cells + GAUGE_BAR_CHROME_W + GAUGE_GAP;
        if name_w > 0 {
            w += name_w + GAUGE_GAP;
        }
    } else if name_w > 0 {
        w += name_w + GAUGE_GAP;
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
    let push_name = |spans: &mut Vec<Span<'static>>, gap: &'static str| {
        if fit.name_w > 0 {
            let (nt, _pad) = fixed_split(name, fit.name_w);
            let style = Style::default().fg(theme::text_color());
            spans.extend(pulse_name_spans(&nt, style, elapsed_ms));
            spans.push(Span::raw(gap));
        }
    };
    match pct {
        Some(pct) if fit.bar_cells > 0 => {
            let style = Style::default().fg(theme::util_color(pct));
            push_name(&mut spans, " ");
            spans.push(Span::styled(format!("{pct:.0}%"), style));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("[", theme::dim()));
            spans.push(Span::styled(
                bar_string_with_cells(pct, fit.bar_cells),
                style,
            ));
            spans.push(Span::styled("]", theme::dim()));
        }
        Some(pct) => {
            push_name(&mut spans, " ");
            spans.push(Span::styled(
                format!("{pct:.0}%"),
                Style::default().fg(theme::util_color(pct)),
            ));
        }
        None => push_name(&mut spans, " "),
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

const CONTENT_GAP: usize = 3;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols: [Rect; 2] =
        Layout::horizontal([Constraint::Length(10), Constraint::Min(20)]).areas(area);

    draw_logo(frame, cols[0], app);

    let rows: [Rect; 3] = Layout::vertical([Constraint::Length(1); 3]).areas(cols[1]);

    let info_width = rows[0].width as usize;

    let gauge = if app.tab == Tab::Overview || app.compact {
        None
    } else {
        active_gauge(app)
    };

    // ── Row 0: clauth [herdr tag] vX.Y.Z ....... [ shunt ]  [ daemon ] ─────
    let brand = "clauth";
    let ver = format!(" v{VERSION}");
    let mut row0: Vec<Span<'static>> = vec![
        Span::styled(brand, Style::default().fg(theme::text_color()).bold()),
        Span::styled(ver, theme::dim()),
    ];
    let mut used: usize = row0.iter().map(|s| s.content.chars().count()).sum();
    // Service chips, stuck to the right edge in a fixed order two cells apart.
    // Always present: an absent service dims its chip rather than hiding it.
    // They shed after the tag, the shunt chip before the daemon chip, each only
    // once brand + version + the chips left no longer fit with the content gap
    // the chips keep from what sits to their left.
    let shunt = service_chip("shunt", shunt_chip_color(app));
    let daemon = service_chip("daemon", daemon_chip_color(app));
    let daemon_w = spans_width(&daemon);
    let chips_w = spans_width(&shunt) + CHIP_GAP.len() + daemon_w;
    // `[ herdr ]` context tag, between the brand and the version. It is the
    // first span this row sheds, so it renders only while it still leaves room
    // for both chips — brand and version never clip.
    if app.herdr_mode {
        let tag = "  [ herdr ]";
        if used + tag.chars().count() + chips_w + CONTENT_GAP <= info_width {
            row0.insert(1, Span::styled(tag, theme::dim()));
            used += tag.chars().count();
        }
    }
    let chips: Vec<Span<'static>> = if used + chips_w + CONTENT_GAP <= info_width {
        let mut both = Vec::from(shunt);
        both.push(Span::raw(CHIP_GAP));
        both.extend(daemon);
        both
    } else if used + daemon_w + CONTENT_GAP <= info_width {
        Vec::from(daemon)
    } else {
        Vec::new()
    };
    if !chips.is_empty() {
        let gap = info_width - used - spans_width(&chips);
        row0.push(Span::raw(" ".repeat(gap)));
        row0.extend(chips);
    }
    frame.render_widget(
        Paragraph::new(Line::from(row0)).style(theme::base()),
        rows[0],
    );

    // ── Row 1: [[ N live ] · ][gauge] ...... ● feed ──────────────────────
    // The fleet's live count and the active profile's gauge, left-aligned, and
    // the status indicator, the only thing right-aligned, across an elastic
    // gap. The account counts and the harness filter left this row for the
    // accounts panel's title row, whose meta slots carry them beside the bare
    // `ACCOUNTS` title (the filter name left, the roster counts right). The
    // indicator drops whole, head and feed together, since a dot
    // left to render clips the feed mid-word and a half-spelled source says
    // less than none. Its gate is charged the gauge as rendered, so the gauge's
    // own ladder gives way before the indicator does.
    let row1_width = rows[1].width as usize;
    let feed = "status.claude.ai";
    let status_head = "● ";
    let status_w = status_head.chars().count() + feed.chars().count();
    // Right-aligned columns keep a minimum 3-cell gap from the content to
    // their left (`CONTENT_GAP`, the same gap row 0 keeps): the indicator sheds
    // before it would render closer than that, never at a 1-2 cell squeeze.
    let mut left_spans: Vec<Span<'static>> = Vec::new();
    if let Some(ref g) = gauge {
        let gauge_budget = row1_width.saturating_sub(status_w + CONTENT_GAP);
        let fit = gauge_fit(gauge_budget, g.name.chars().count(), g.pct.is_some());
        if fit.visible {
            left_spans.extend(gauge_spans(fit, &g.name, g.pct, app.anim_ms()));
        }
    }
    // The fleet's live-session count leads the row as a counter chip, zero
    // included: hiding it at zero would move the gauge each time the first
    // session starts or the last one ends. It takes only what the gauge and the
    // indicator leave, so it is the first thing the row sheds, whole with its
    // separator, never costing the gauge a cell.
    let mut prefix = live_chip_spans(app);
    if !left_spans.is_empty() {
        prefix.push(Span::styled(" · ", theme::dim()));
    }
    let prefix_w: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
    let gauge_w: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    if prefix_w + gauge_w + status_w + CONTENT_GAP <= row1_width {
        prefix.append(&mut left_spans);
        left_spans = prefix;
    }
    let left_w: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let mut row1_spans = left_spans;
    if row1_width >= left_w + status_w + CONTENT_GAP {
        let gap = row1_width - left_w - status_w;
        row1_spans.push(Span::raw(" ".repeat(gap)));
        row1_spans.push(Span::styled(
            status_head,
            Style::default().fg(status_dot_color(app)),
        ));
        row1_spans.push(Span::styled(feed, theme::dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(row1_spans)).style(theme::base()),
        rows[1],
    );

    // ── Row 2: tabs ──────────────────────────────────────────────────────
    super::tabs::draw(frame, rows[2], app);
}

/// `[ N live ]`: brackets and unit dim, the number `TEXT` + bold, dim at zero
/// and as the `—` of a tally that could not read. Blank cells after the closing
/// bracket pad it to the widest count the app has read, so a count losing a
/// digit moves nothing right of the chip.
fn live_chip_spans(app: &App) -> Vec<Span<'static>> {
    let total = app.live_sessions.total();
    let number = live_count_text(total);
    let number_style = match total {
        Some(n) if n > 0 => Style::default().fg(theme::text_color()).bold(),
        Some(_) | None => theme::dim(),
    };
    let pad = app.live_count_width.saturating_sub(number.chars().count());
    let mut spans = vec![
        Span::styled("[ ", theme::dim()),
        Span::styled(number, number_style),
        Span::styled(" live ]", theme::dim()),
    ];
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans
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

/// The cells between two service chips.
const CHIP_GAP: &str = "  ";

/// A service chip in the pill grammar: the brackets are chrome, the word
/// carries the health color in bold.
fn service_chip(word: &'static str, color: ratatui::style::Color) -> [Span<'static>; 3] {
    [
        Span::styled("[ ", theme::dim()),
        Span::styled(word, theme::label().fg(color)),
        Span::styled(" ]", theme::dim()),
    ]
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// `[ daemon ]` header-chip color. Mirrors [`status_dot_color`]: green = daemon
/// up + fresh feed, amber = up but its `status.json` is stale (wedging /
/// pre-abort / just booted), faint = no daemon at all (the chip is always on
/// the row; the TUI self-fetches under its own lease).
fn daemon_chip_color(app: &App) -> ratatui::style::Color {
    use crate::daemon::DaemonHealth;
    match app.daemon_health {
        DaemonHealth::Absent => theme::text_faint_color(),
        DaemonHealth::Stale => theme::warning_color(),
        DaemonHealth::Fresh => theme::success_color(),
    }
}

/// `[ shunt ]` header-chip color: the Services row's state buckets, so the
/// chip and the row's dot never disagree — green serving, amber starting,
/// unhealthy or between runs, red refused — except that an idle gateway
/// (absent, disabled, held, or unobserved: no supervisor reads it) is faint
/// like an absent daemon rather than the row's dim.
fn shunt_chip_color(app: &App) -> ratatui::style::Color {
    match gateway_health(app.gateway_state) {
        Health::Idle => theme::text_faint_color(),
        health => health_color(health),
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

// Row-0 pins: the brand/version/tag/chip layout, the chip's pill colors, and
// the shed ladder — tag first, chip second — with both seams pinned on either
// side and the plain launch pinned as that same shape minus the tag.
#[cfg(test)]
#[path = "../../../tests/inline/tui_render_header_herdr.rs"]
mod herdr_mode_tests;
