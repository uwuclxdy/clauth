//! Top bar: the mascot on the left; brand, usage gauge and status source in
//! the text column to the right. The art header is the mascot's own rows;
//! when the frame cannot hold the art it falls back to two, carrying the tab
//! bar on the brand row. [`header_height`] keeps `render::draw`'s layout in
//! step.
//!
//! The info column reads top-down from the mascot's own rows: the brand row
//! on the first, the status row on the second, the tabs on the last, and any
//! rows between left blank. The brand row reads `clauth vX.Y.Z` on the left
//! with the herdr tag between them, and the `[ shunt ]  [ daemon ]` service
//! chips on the right edge. The brand row sheds the tag first, then the shunt
//! chip, then the daemon chip; the no-art row also carries the tab bar, which
//! collapses to its overflow form between the tag and the chips. The status
//! row carries the fleet's live-session chip, the tok/s chip, the
//! active-profile usage gauge with its speed suffix and the status indicator:
//! the account counts and the harness filter live on the accounts panel's
//! title row, as meta slots beside the bare `ACCOUNTS` title. The status row
//! places the overdue age last, only in the cells the drawn chips, speed
//! suffix and gauge leave, so it never pushes anything else out; it can
//! outlast a chip whose ceiling charge no longer fits. The tok/s chip sheds
//! first, then the live chip, then the speed suffix; the gauge's ladder drops
//! the usage bar before the name.

use std::rc::Rc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, Health, ReadingsSnapshot, Tab, gateway_health};
use super::super::theme;
use super::format::{
    bar_string_with_cells, fixed_split, past_deadline_age, reading_due_age, relative_age,
};
use super::services::health_color;
use crate::format::format_pct;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BRAND: &str = "clauth";

/// The version suffix that rides the brand: ` v0.18.0`.
fn version_label() -> String {
    format!(" v{VERSION}")
}

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
/// The percent's budget at its widest whole reading, `100%`: holding it
/// constant keeps the ladder still as the reading ticks, and only a longer
/// label (`73.42%`) widens it.
const GAUGE_PCT_W: usize = 4;
/// The widest label the gauge's percent can ever render: `99.99%`, the ceiling
/// `format_pct` reaches at two decimals. Charged in the chip shed checks in
/// place of the live label (through [`gauge_tail_w`], so the formatter owns
/// the number), so a reading update never moves a chip.
const GAUGE_MAX_PCT: f64 = 99.99;
/// The widest the speed suffix can ever render: ` (9999 t/s)`, the suffix's
/// figure going through the same four-cell `format_figure_capped` as the chip.
/// Charged in the chip shed checks so a speed update never moves a
/// chip.
const GAUGE_MAX_SUFFIX_W: usize = 11;
/// The widest the tok/s chip can ever render: `[ 9999 tok/s ]`, the four-cell
/// figure ceiling the chip's whole-`k` form reaches. Charged in the shed checks
/// so a rate update never moves a chip.
const TPS_CHIP_CEILING_W: usize = 14; // "[ " + "9999" + " tok/s ]"
/// The widest the live chip can ever render: `[ 999+999 live ]`, the split
/// form at three digits a part. Charged in the shed checks so a count update
/// never moves a chip.
const LIVE_CHIP_CEILING_W: usize = 16; // "[ " + "999+999" + " live ]"
/// Flush bar brackets: `[` + `]`.
const GAUGE_BAR_CHROME_W: usize = 2;
/// Every gauge gap — name → percent, percent → bar, bare rung — 1 cell.
const GAUGE_GAP: usize = 1;
// ── Account-name pulse ───────────────────────────────────────────────────

const PULSE_SWEEP_MS: u64 = 900;
const PULSE_REST_MS: u64 = 1700;
const PULSE_DEPTH: f32 = 0.4;
const PULSE_CYCLE_MS: u64 = PULSE_SWEEP_MS + PULSE_REST_MS;
/// A phase inside the pulse's rest gap, where it draws the plain name.
const PULSE_REST_PHASE_MS: u64 = PULSE_SWEEP_MS;
/// The identity wave's crest is the provider's own color, not a tint toward it.
const IDENTITY_DEPTH: f32 = 1.0;
/// A row's step into the cycle, as a fraction of it: the golden ratio, so any
/// run of rows spreads its crests across the cycle instead of cascading.
const PULSE_STAGGER_FRACTION: f64 = 0.6180339887498949;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GaugeFit {
    name_w: usize,
    bar_cells: usize,
    /// The cells the overdue age occupies once the drawn row left room for
    /// it. The age is placed after the layout, whole, so a nonzero value
    /// always sits beside the full-width bar.
    age_w: usize,
    visible: bool,
}

impl GaugeFit {
    const HIDDEN: GaugeFit = GaugeFit {
        name_w: 0,
        bar_cells: 0,
        age_w: 0,
        visible: false,
    };
}

struct ActiveGauge {
    name: String,
    pct: Option<f64>,
    /// The reading's own age (`4m ago`), shown only once its refresh is
    /// overdue: the deadline published for this instance, or (none published,
    /// a stuck worker holds the leg) the reading's clock plus the account's
    /// interval. Only a stamp the reading itself carries dates it.
    age: Option<String>,
}

fn active_gauge(app: &App, readings: &ReadingsSnapshot) -> Option<ActiveGauge> {
    let cfg = app.config();
    let name = cfg.state.active_profile.as_ref()?;
    let profile = cfg.find(name)?;
    let pct = if profile.is_oauth() {
        readings
            .usage_of(&profile.name)
            .and_then(|u| u.five_hour.as_ref())
            .map(|w| w.utilization.clamp(0.0, 100.0))
    } else {
        None
    };
    let age = if profile.is_oauth() {
        // The gauge dates its figure, so a reading without a stamp stays
        // undated; a stood-down mirror's past deadline or a stuck worker's
        // un-published one gates the age in, and the age itself is the
        // reading's, not the deadline's. The no-deadline arm also reads the
        // leg's activity: a due account the bootstrap marked Queued (or a
        // fetch in flight) has a fetch coming, so its cadence date is not
        // an overdue verdict yet — the same activity the Overview's spinner
        // arm reads. A leg the scheduler deliberately blanked (a spent-skip,
        // a suppressed credential, a selected leg on no work list) gates it
        // out.
        let (blanked, idle) = {
            let key = crate::usage::FetchLeg::for_profile(profile).key(profile.name.clone());
            app.activity
                .lock()
                .ok()
                .map(|states| {
                    (
                        crate::usage::countdown_blanked(&states, &key),
                        crate::usage::selected_activity(&states, profile)
                            == crate::usage::ProfileActivity::Idle,
                    )
                })
                .unwrap_or((false, false))
        };
        readings.read_clock_of(&profile.name).and_then(|at| {
            let next_refresh = app
                .next_refresh_per_profile
                .lock()
                .ok()
                .and_then(|m| crate::usage::selected_next_refresh(&m, profile));
            let interval = crate::usage::usage_interval_ms(profile, cfg.state.refresh_interval_ms);
            let overdue = match next_refresh {
                Some(next) => past_deadline_age(app.fetch_lease.fetches_here(), next),
                None if !blanked && idle => {
                    reading_due_age(readings.clock_of(&profile.name), interval)
                }
                None => None,
            };
            overdue.map(|_| relative_age(at))
        })
    } else {
        None
    };
    Some(ActiveGauge {
        name: name.to_string(),
        pct,
        age,
    })
}

fn gauge_tail_w(pct: Option<f64>) -> usize {
    pct.map_or(0, |p| format_pct(p).len().max(GAUGE_PCT_W))
}

/// The cells the gauge charges for an overdue age: the ` · ` separator plus
/// the age's own width.
fn gauge_age_w(age: Option<&str>) -> usize {
    age.map_or(0, |a| " · ".chars().count() + a.chars().count())
}

/// Whole-gauge width at a rung. While the bar renders, the percent leads it
/// (`name` pct ` [bar]`, single-spaced); the brackets drop with the bar,
/// leaving the bare `name` pct rung.
fn gauge_total_w(name_w: usize, bar_cells: usize, pct: Option<f64>) -> usize {
    let mut w = gauge_tail_w(pct);
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

/// Collapse ladder: bar shrinks → bar drops → name truncates → name drops →
/// hide.
fn gauge_fit(avail: usize, name_len: usize, pct: Option<f64>) -> GaugeFit {
    if avail < gauge_tail_w(pct) {
        return GaugeFit::HIDDEN;
    }
    let mut name_w = name_len.min(GAUGE_NAME_MAX);
    let mut bar_cells = if pct.is_some() { GAUGE_BAR_FULL } else { 0 };
    let name_floor = GAUGE_NAME_MIN.min(name_len);

    while gauge_total_w(name_w, bar_cells, pct) > avail && bar_cells > GAUGE_BAR_MIN {
        bar_cells -= 1;
    }
    if bar_cells > 0 && gauge_total_w(name_w, bar_cells, pct) > avail {
        bar_cells = 0;
    }
    while gauge_total_w(name_w, bar_cells, pct) > avail && name_w > name_floor {
        name_w -= 1;
    }
    if name_w > 0 && gauge_total_w(name_w, bar_cells, pct) > avail {
        name_w = 0;
    }
    if gauge_total_w(name_w, bar_cells, pct) > avail {
        return GaugeFit::HIDDEN;
    }
    if pct.is_none() && name_w == 0 {
        return GaugeFit::HIDDEN;
    }
    GaugeFit {
        name_w,
        bar_cells,
        age_w: 0,
        visible: true,
    }
}

fn gauge_spans(
    fit: GaugeFit,
    name: &str,
    pct: Option<f64>,
    age: Option<&str>,
    elapsed_ms: u64,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if fit.name_w > 0 {
        let (nt, _pad) = fixed_split(name, fit.name_w);
        let style = Style::default().fg(theme::text_color());
        spans.extend(pulse_name_spans(&nt, style, elapsed_ms));
        spans.push(Span::raw(" "));
    }
    match pct {
        Some(pct) if fit.bar_cells > 0 => {
            let style = Style::default().fg(theme::util_color(pct));
            spans.push(Span::styled(format_pct(pct), style));
            // The reading's age, dim like the brackets: the gauge stays
            // uncolored whatever the fetch state, and only an overdue reading
            // ever rides here.
            if fit.age_w > 0
                && let Some(age) = age
            {
                spans.push(Span::styled(format!(" · {age}"), theme::dim()));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled("[", theme::dim()));
            spans.push(Span::styled(
                bar_string_with_cells(pct, fit.bar_cells),
                style,
            ));
            spans.push(Span::styled("]", theme::dim()));
        }
        Some(pct) => spans.push(Span::styled(
            format_pct(pct),
            Style::default().fg(theme::util_color(pct)),
        )),
        None => {}
    }
    spans
}

pub(super) fn pulse_name_spans(name: &str, style: Style, elapsed_ms: u64) -> Vec<Span<'static>> {
    crest_spans(
        name,
        style,
        theme::accent_2_color(),
        PULSE_DEPTH,
        elapsed_ms,
    )
}

/// The identity wave over one row's provider-owned cell: a `TEXT_DIM` base the
/// provider's crest sweeps, started one golden-ratio step per `position` — the
/// row's position in the list the Overview draws — so a list spreads its crests
/// across the cycle instead of running them as one band, whichever provider
/// each row belongs to, and a row keeps its phase when a neighbour starts or
/// stops cresting.
pub(super) fn identity_wave_spans(
    label: &str,
    provider: theme::CrestProvider,
    position: usize,
    elapsed_ms: u64,
) -> Vec<Span<'static>> {
    crest_spans(
        label,
        theme::dim(),
        theme::crest_color(provider),
        IDENTITY_DEPTH,
        elapsed_ms % PULSE_CYCLE_MS + identity_wave_offset_ms(position),
    )
}

/// A phase at which `position`'s identity wave sits at the start of its rest
/// gap, so [`identity_wave_spans`] draws the plain label there. A row held
/// still under `reduce_motion` samples it.
pub(super) fn identity_wave_rest_ms(position: usize) -> u64 {
    (PULSE_SWEEP_MS + PULSE_CYCLE_MS - identity_wave_offset_ms(position) % PULSE_CYCLE_MS)
        % PULSE_CYCLE_MS
}

/// One golden-ratio step of the cycle per list position.
fn identity_wave_offset_ms(position: usize) -> u64 {
    let fraction = (position as f64 * PULSE_STAGGER_FRACTION).fract();
    (fraction * PULSE_CYCLE_MS as f64).round() as u64
}

fn crest_spans(
    name: &str,
    style: Style,
    crest_color: ratatui::style::Color,
    depth: f32,
    elapsed_ms: u64,
) -> Vec<Span<'static>> {
    use std::f32::consts::{PI, TAU};
    let plain = || vec![Span::styled(name.to_string(), style)];
    if theme::tier() != theme::Tier::Full {
        return plain();
    }
    let t = (elapsed_ms % PULSE_CYCLE_MS) as f32;
    let sweep = PULSE_SWEEP_MS as f32;
    if t >= sweep {
        return plain();
    }
    let progress = t / sweep;
    let envelope = (PI * progress).sin();
    let head = progress * TAU;
    // One span per grapheme, never per char: `⚠️` (U+26A0 + U+FE0F) is one
    // grapheme, and a split leaves the base a cell narrow with the selector
    // drawn as nothing. `styled_graphemes` is the same segmentation the
    // buffer draws from (control graphemes dropped), so each unit keeps its
    // bytes and width across sweep frames.
    let source = Span::raw(name);
    let graphemes: Vec<&str> = source
        .styled_graphemes(Style::default())
        .map(|g| g.symbol)
        .collect();
    let len = graphemes.len().max(1) as f32;
    let base = style.fg.unwrap_or_else(theme::text_color);
    graphemes
        .iter()
        .enumerate()
        .map(|(i, symbol)| {
            let col = (i as f32 / len) * TAU;
            let crest = ((col - head).cos() * 0.5 + 0.5).powi(2);
            let lean = f64::from(crest * envelope * depth);
            let fg = theme::blend_over(base, crest_color, lean);
            Span::styled((*symbol).to_string(), style.fg(fg))
        })
        .collect()
}

// ── Height ───────────────────────────────────────────────────────────────

/// The columns the header gives the art on top of the mascot's own width:
/// one padding column before the info column.
const LOGO_PAD: u16 = 1;
/// Body rows the art header must leave for a usable body, on top of the
/// mascot's own rows.
const ART_BODY_ROWS: u16 = 11;

/// Whether `area` holds the art header: enough rows for the mascot and a
/// usable body, and enough columns for the logo slot, the brand, the version
/// and the daemon chip with the content gap. The mascot reads it too, so it
/// plays only while the art is drawn.
pub(crate) fn art_fits(area: Rect) -> bool {
    let (mascot_cols, mascot_rows) = App::MASCOT_SIZE;
    let brand = BRAND.chars().count();
    let ver = version_label().chars().count();
    let daemon = spans_width(&service_chip("daemon", ratatui::style::Color::Reset));
    area.height >= mascot_rows as u16 + ART_BODY_ROWS
        && usize::from(area.width)
            >= mascot_cols + usize::from(LOGO_PAD) + brand + ver + CONTENT_GAP + daemon
}

/// Header rows `area` gets: the mascot's own with the art, two without it.
pub(super) fn header_height(area: Rect) -> u16 {
    if art_fits(area) {
        App::MASCOT_SIZE.1 as u16
    } else {
        2
    }
}

// ── Draw ─────────────────────────────────────────────────────────────────

const CONTENT_GAP: usize = 3;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App, readings: &ReadingsSnapshot) {
    let art = art_fits(area);
    let header = Rect {
        height: header_height(area),
        ..area
    };
    let info_area = if art {
        let cols: [Rect; 2] = Layout::horizontal([
            Constraint::Length(App::MASCOT_SIZE.0 as u16 + LOGO_PAD),
            Constraint::Min(20),
        ])
        .areas(header);
        draw_logo(frame, cols[0], app);
        cols[1]
    } else {
        header
    };

    // The info column follows the mascot's own rows: the brand row first, the
    // status row second, the tabs on the last, any rows between left blank.
    let rows: Rc<[Rect]> = if art {
        Layout::vertical([Constraint::Length(1); App::MASCOT_SIZE.1]).split(info_area)
    } else {
        let two: [Rect; 2] = Layout::vertical([Constraint::Length(1); 2]).areas(info_area);
        Rc::from([two[0], two[1], Rect::default()])
    };

    let info_width = rows[0].width as usize;

    let gauge = if app.tab == Tab::Overview || app.compact {
        None
    } else {
        active_gauge(app, readings)
    };

    // ── Row 0: clauth [herdr tag] vX.Y.Z ....... [ shunt ]  [ daemon ] ─────
    let row0 = if art {
        art_row0(app, info_width)
    } else {
        fallback_row0(app, info_width)
    };
    frame.render_widget(
        Paragraph::new(Line::from(row0)).style(theme::base()),
        rows[0],
    );

    // ── Row 1: [[ N live ]  [ R tok/s ] · ][gauge][ (S t/s)] ...... ● feed ──
    // The fleet's live-session count, the total consumption rate, the active
    // profile's gauge with its speed suffix, left-aligned, and the status
    // indicator, the only thing right-aligned, across an elastic gap. The
    // indicator drops whole, head and feed together, since a dot left to
    // render clips the feed mid-word and a half-spelled source says less than
    // none. Its gate is charged the content as rendered, so the content's own
    // ladder gives way before the indicator does. Narrow widths shed the
    // tok/s chip first, then the live chip, then the speed suffix, then the
    // gauge's own ladder, then the indicator; the overdue age draws only in
    // the cells that row leaves.
    let row1_width = rows[1].width as usize;
    let feed = "status.claude.ai";
    let status_head = format!("{} ", theme::DOT_ON);
    let status_w = status_head.chars().count() + feed.chars().count();
    // Right-aligned columns keep a minimum 3-cell gap from the content to
    // their left (`CONTENT_GAP`, the same gap row 0 keeps): the indicator sheds
    // before it would render closer than that, never at a 1-2 cell squeeze.
    let left_budget = row1_width.saturating_sub(status_w + CONTENT_GAP);

    let live = live_chip_spans(app);
    let tps = tps_chip_spans(app);
    let suffix = speed_suffix_spans(app, gauge.as_ref());
    let suffix_w: usize = suffix.iter().map(|s| s.width()).sum();

    // The gauge's full form is the content with the highest priority after the
    // indicator: the suffix sheds before the gauge's ladder, so the gauge keeps
    // its full form while the suffix can still ride it, and only shrinks once
    // the suffix is gone. The age decides nothing here: the row lays out as it
    // would with no age, so the age can never displace the suffix or a rung.
    let g = gauge.as_ref();
    let (name_len, pct, age_w) = g.map_or((0, None, 0), |g| {
        (
            Span::raw(g.name.as_str()).width(),
            g.pct,
            gauge_age_w(g.age.as_deref()),
        )
    });
    let mut gauge_rung = GaugeFit::HIDDEN;
    let mut keep_suffix = false;
    if g.is_some() {
        let full_fit = gauge_fit(usize::MAX, name_len, pct);
        let full_reserved = gauge_total_w(full_fit.name_w, full_fit.bar_cells, pct);
        if full_reserved + suffix_w <= left_budget {
            gauge_rung = full_fit;
            keep_suffix = !suffix.is_empty();
        } else if full_reserved <= left_budget {
            gauge_rung = full_fit;
        } else {
            gauge_rung = gauge_fit(left_budget, name_len, pct);
        }
    }
    // The chip shed checks charge the widest the gauge, its speed suffix and
    // each chip can ever render — the full bar rung with the widest percent
    // label, the ` (9999 t/s)` suffix, and the `[ 9999 tok/s ]` /
    // `[ 999+999 live ]` ceilings — so a chip's presence depends on the
    // terminal width and the active account's name only, never on a reading,
    // speed, rate or count update. The overdue age charges nothing here: it
    // draws only in the cells the drawn chips, the suffix and the gauge leave,
    // so an overdue reading never moves a chip. The gauge, suffix and chips
    // still render at their real widths in whatever room the shed checks
    // leave.
    let (charged_gauge_w, charged_suffix_w) = g.map_or((0, 0), |g| {
        let oauth = pct.is_some();
        let name_w = Span::raw(g.name.as_str()).width().min(GAUGE_NAME_MAX);
        (
            gauge_total_w(
                name_w,
                if oauth { GAUGE_BAR_FULL } else { 0 },
                oauth.then_some(GAUGE_MAX_PCT),
            ),
            GAUGE_MAX_SUFFIX_W,
        )
    });
    let sep_w = if gauge_rung.visible {
        " · ".chars().count()
    } else {
        0
    };
    // The chips take only what the gauge (and the suffix) leave, so the tok/s
    // chip sheds first, whole with its chip gap, then the live chip, whole with
    // its separator — never costing the gauge a cell.
    let gap_w = Span::raw(CHIP_GAP).width();
    let both_chips = !tps.is_empty()
        && LIVE_CHIP_CEILING_W
            + gap_w
            + TPS_CHIP_CEILING_W
            + sep_w
            + charged_gauge_w
            + charged_suffix_w
            <= left_budget;
    let live_chip = LIVE_CHIP_CEILING_W + sep_w + charged_gauge_w + charged_suffix_w <= left_budget;
    // The age takes only the cells the drawn row leaves, whole: the chips'
    // real widths and the kept suffix hold their places first, and the gauge
    // counts its percent at no less than the label's 4-cell budget, so a
    // whole percent changing width never makes the age appear or vanish; a
    // fractional one (`42.37%`) can. No room means no age, never a shed
    // chip, suffix or gauge rung.
    if age_w > 0 && gauge_rung.visible && gauge_rung.bar_cells > 0 {
        let live_w: usize = live.iter().map(|s| s.width()).sum();
        let tps_w: usize = tps.iter().map(|s| s.width()).sum();
        let chips_w = if both_chips {
            live_w + gap_w + tps_w
        } else if live_chip {
            live_w
        } else {
            0
        };
        let row_cells = chips_w
            + if chips_w > 0 { sep_w } else { 0 }
            + gauge_total_w(gauge_rung.name_w, gauge_rung.bar_cells, pct)
            + if keep_suffix { suffix_w } else { 0 };
        if left_budget.saturating_sub(row_cells) >= age_w {
            gauge_rung.age_w = age_w;
        }
    }
    let name_ms = if app.config().state.reduce_motion {
        PULSE_REST_PHASE_MS
    } else {
        app.anim_ms()
    };
    let gauge_spans_vec = match g {
        Some(g) if gauge_rung.visible => {
            gauge_spans(gauge_rung, &g.name, pct, g.age.as_deref(), name_ms)
        }
        _ => Vec::new(),
    };
    let gauge_w: usize = gauge_spans_vec.iter().map(|s| s.width()).sum();

    let mut left: Vec<Span<'static>> = Vec::new();
    if both_chips {
        left.extend(live);
        left.push(Span::raw(CHIP_GAP));
        left.extend(tps);
        if gauge_w > 0 {
            left.push(Span::styled(" · ", theme::dim()));
        }
        left.extend(gauge_spans_vec);
        if keep_suffix {
            left.extend(suffix);
        }
    } else if live_chip {
        left.extend(live);
        if gauge_w > 0 {
            left.push(Span::styled(" · ", theme::dim()));
        }
        left.extend(gauge_spans_vec);
        if keep_suffix {
            left.extend(suffix);
        }
    } else {
        left.extend(gauge_spans_vec);
        if keep_suffix {
            left.extend(suffix);
        }
    }

    let left_w: usize = left.iter().map(|s| s.width()).sum();
    let mut row1_spans = left;
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

    // ── Last row: tabs (the art header only; without the art they ride row 0) ──
    if art {
        super::tabs::draw(frame, rows[rows.len() - 1], app);
    }
}

/// Row 0 with the art beside it: brand, the `[ herdr ]` tag between brand and
/// version, then the service chips on the right edge.
fn art_row0(app: &App, info_width: usize) -> Vec<Span<'static>> {
    let mut row0: Vec<Span<'static>> = vec![
        Span::styled(BRAND, Style::default().fg(theme::text_color()).bold()),
        Span::styled(version_label(), theme::dim()),
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
    // The daemon chip always fits: [`art_fits`] guarantees the columns for it.
    let chips: Vec<Span<'static>> = if used + chips_w + CONTENT_GAP <= info_width {
        let mut both = Vec::from(shunt);
        both.push(Span::raw(CHIP_GAP));
        both.extend(daemon);
        both
    } else {
        Vec::from(daemon)
    };
    let gap = info_width.saturating_sub(used + spans_width(&chips));
    row0.push(Span::raw(" ".repeat(gap)));
    row0.extend(chips);
    row0
}

/// Which service chips the no-art row carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chips {
    None,
    Daemon,
    Both,
}

impl Chips {
    /// `(width, gap before the chips)` for this set. With no chip there is no
    /// gap to keep.
    fn metrics(self, shunt_w: usize, daemon_w: usize) -> (usize, usize) {
        match self {
            Chips::Both => (shunt_w + CHIP_GAP.len() + daemon_w, CONTENT_GAP),
            Chips::Daemon => (daemon_w, CONTENT_GAP),
            Chips::None => (0, 0),
        }
    }
}

/// Whether the no-art row's tab bar renders in full, in its overflow form, or
/// not at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabStrip {
    Full,
    Collapsed,
    None,
}

/// Row 0 without the art: brand and version at column 0, the no-logo
/// separator, the tab bar, then the service chips on the right edge. It sheds
/// the `[ herdr ]` tag, then collapses the tab bar to its overflow form, then
/// sheds the shunt chip, then the daemon chip. Brand and version never shed,
/// and the ladder is monotone: a wider row never shows fewer chips or a
/// narrower tab form. The collapse and the chip seams charge the widest
/// overflow bar over every tab, so neither depends on which tab is active;
/// the bar itself draws at the active tab's own width. Under that reserved
/// width the bar and its separator drop whole.
fn fallback_row0(app: &App, width: usize) -> Vec<Span<'static>> {
    let tag = "  [ herdr ]";
    let sep = "  •  ";
    let shunt = service_chip("shunt", shunt_chip_color(app));
    let daemon = service_chip("daemon", daemon_chip_color(app));
    let shunt_w = spans_width(&shunt);
    let daemon_w = spans_width(&daemon);
    let brand_w = BRAND.chars().count();
    let tag_w = tag.chars().count();
    let sep_w = sep.chars().count();
    let prefix_w = brand_w + version_label().chars().count();
    let strip_w = super::tabs::full_strip_width();
    let collapsed_w = super::tabs::overflow_width_max();

    // Widest rung first; the first that fits wins. The tag rides only while
    // both chips fit beside the full strip.
    let ladder = [
        (true, Chips::Both, TabStrip::Full),
        (false, Chips::Both, TabStrip::Full),
        (false, Chips::Both, TabStrip::Collapsed),
        (false, Chips::Daemon, TabStrip::Collapsed),
        (false, Chips::None, TabStrip::Collapsed),
        (false, Chips::None, TabStrip::None),
    ];
    let chosen = ladder.into_iter().find(|&(tag_on, chips, strip)| {
        if tag_on && !app.herdr_mode {
            return false;
        }
        let head_w = prefix_w + if tag_on { tag_w } else { 0 };
        let (tabs_w, strip_sep_w) = match strip {
            TabStrip::Full => (strip_w, sep_w),
            TabStrip::Collapsed => (collapsed_w, sep_w),
            TabStrip::None => (0, 0),
        };
        let (chips_w, gap) = chips.metrics(shunt_w, daemon_w);
        head_w + strip_sep_w + tabs_w + gap + chips_w <= width
    });
    // Below the last rung the brand and version still hold the row.
    let (tag_on, chips, strip) = chosen.unwrap_or((false, Chips::None, TabStrip::None));

    let head_w = prefix_w + if tag_on { tag_w } else { 0 };
    let (chips_w, gap) = chips.metrics(shunt_w, daemon_w);
    let avail = width.saturating_sub(head_w + sep_w + chips_w + gap);
    let mut row0 = vec![Span::styled(
        BRAND,
        Style::default().fg(theme::text_color()).bold(),
    )];
    if tag_on {
        row0.push(Span::styled(tag, theme::dim()));
    }
    row0.push(Span::styled(version_label(), theme::dim()));
    if strip != TabStrip::None {
        row0.push(Span::styled(sep, theme::dim()));
        let tabs = if strip == TabStrip::Full {
            super::tabs::spans(app, avail)
        } else {
            super::tabs::spans(app, 0)
        };
        row0.extend(tabs);
    }
    if chips != Chips::None {
        let used: usize = row0.iter().map(Span::width).sum();
        row0.push(Span::raw(" ".repeat(width.saturating_sub(used + chips_w))));
        if chips == Chips::Both {
            row0.extend(shunt);
            row0.push(Span::raw(CHIP_GAP));
        }
        row0.extend(daemon);
    }
    row0
}

/// `[ N live ]` or its split form `[ N+M live ]`: brackets and unit dim, the
/// own number `TEXT` + bold, the delegates number `ACCENT` + bold, the `+` dim,
/// all dim at zero and as the `—` of a tally that could not read.
fn live_chip_spans(app: &App) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("[ ", theme::dim())];
    spans.extend(live_number_spans(
        app.live_sessions.own(),
        app.live_sessions.delegates(),
    ));
    spans.push(Span::styled(" live ]", theme::dim()));
    spans
}

/// The live chip's number field, as spans: own sessions then delegates joined
/// by a dim `+`. A zero or unread part drops with its `+`; both zero renders
/// one dim `0`, and no part reads renders a dim `—`. Own renders `TEXT` + bold,
/// delegates `ACCENT` + bold, the `+` and a zero dim.
fn live_number_spans(own: Option<usize>, delegates: Option<usize>) -> Vec<Span<'static>> {
    let text = Style::default().fg(theme::text_color()).bold();
    let accent = Style::default().fg(theme::accent_color()).bold();
    let dim = theme::dim();
    let part = |n: usize, nonzero: Style| -> Span<'static> {
        if n == 0 {
            Span::styled("0", dim)
        } else {
            Span::styled(n.to_string(), nonzero)
        }
    };
    match (own, delegates) {
        (None, None) => vec![Span::styled("—", dim)],
        (Some(o), None) => vec![part(o, text)],
        (None, Some(d)) => vec![part(d, accent)],
        (Some(0), Some(0)) => vec![Span::styled("0", dim)],
        (Some(o), Some(0)) => vec![part(o, text)],
        (Some(0), Some(d)) => vec![part(d, accent)],
        (Some(o), Some(d)) => vec![part(o, text), Span::styled("+", dim), part(d, accent)],
    }
}

/// `[ R tok/s ]`: the total consumption rate read off the producer's cache, in
/// the live chip's grammar — brackets and unit dim, the number `TEXT` + bold,
/// dim at zero and as the `—` of a cache that has no fresh producer behind it.
/// It renders from the first frame: an unhydrated launch still shows
/// `[ — tok/s ]`.
fn tps_chip_spans(app: &App) -> Vec<Span<'static>> {
    let number = app.tps_rate_text();
    // Bright only for a fresh non-zero reading: the no-reading `—` and the
    // zero both stay dim, per the Counter chip contract.
    let bright = app.fresh_tps().is_some() && number != "0";
    let number_style = if bright {
        Style::default().fg(theme::text_color()).bold()
    } else {
        theme::dim()
    };
    vec![
        Span::styled("[ ", theme::dim()),
        Span::styled(number, number_style),
        Span::styled(" tok/s ]", theme::dim()),
    ]
}

/// ` (S t/s)`: the active account's live speed, dim like the other secondary
/// figures on the row. Empty when the gauge is absent or the account has no
/// fresh live speed.
fn speed_suffix_spans(app: &App, gauge: Option<&ActiveGauge>) -> Vec<Span<'static>> {
    let Some(gauge) = gauge else {
        return Vec::new();
    };
    let Some(speed) = app
        .fresh_tps()
        .and_then(|s| s.accounts.get(&gauge.name))
        .and_then(|f| f.live.speed)
    else {
        return Vec::new();
    };
    vec![Span::styled(
        format!(" ({} t/s)", crate::tps::format_figure_capped(speed, 4)),
        theme::dim(),
    )]
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
/// The cells between two adjacent header chips: row 0's service chips and row
/// 1's live and tok/s chips.
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
    let lines = app.mascot.draw(app.anim_ms());
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
