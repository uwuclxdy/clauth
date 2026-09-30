//! Palette and shared style helpers.
//!
//! Catppuccin Mocha is the only palette. Two capability tiers select the color
//! depth: `full` uses 24-bit RGB; `compatible` uses the nearest xterm-256 index.
//! Every color in the TUI comes from this module — raw `Color::Rgb` or raw index
//! values anywhere else are a bug.
//!
//! # Initialization
//!
//! Call [`init`] once before the TUI starts to seed the tier from the CLI flag
//! or config file. The Config tab can later [`set_tier`] live — the holder is an
//! atomic so a re-selection re-renders in the new palette on the next frame
//! without a process restart. Renders read it via the accessor fns below.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};

// ── Tier ──────────────────────────────────────────────────────────────────────

/// Color-depth capability tier. `full` = 24-bit RGB; `compatible` = xterm-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// 24-bit truecolor. Requires `$COLORTERM=truecolor|24bit` or an explicit
    /// CLI / config override.
    Full,
    /// Nearest xterm-256 palette index. Safe on any xterm-compatible terminal.
    Compatible,
}

impl Tier {
    /// Stable atomic encoding. `0` doubles as "uninitialized" so the accessor
    /// can fall back to auto-detect before [`init`] runs.
    fn as_code(self) -> u8 {
        match self {
            Tier::Full => 1,
            Tier::Compatible => 2,
        }
    }

    fn from_code(code: u8) -> Option<Tier> {
        match code {
            1 => Some(Tier::Full),
            2 => Some(Tier::Compatible),
            _ => None,
        }
    }
}

/// Process-global tier as an atomic code (`0` = unset → auto-detect). Seeded by
/// [`init`] and swappable at runtime via [`set_tier`] for the live theme picker.
static TIER: AtomicU8 = AtomicU8::new(0);

/// Detect the tier from `$COLORTERM`:
/// `truecolor` or `24bit` → [`Tier::Full`]; anything else → [`Tier::Compatible`].
pub(crate) fn detect() -> Tier {
    match std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "truecolor" | "24bit" => Tier::Full,
        _ => Tier::Compatible,
    }
}

/// Seed the process tier at startup.
/// Precedence (highest first): explicit override → auto-detect.
pub(crate) fn init(override_tier: Option<Tier>) {
    set_tier(override_tier.unwrap_or_else(detect));
}

/// Swap the active tier at runtime. The next render reads the new value, so the
/// Config tab's theme selector applies immediately.
pub(crate) fn set_tier(tier: Tier) {
    TIER.store(tier.as_code(), Ordering::Relaxed);
}

/// Return the active tier. Falls back to auto-detect if [`init`] was not called.
#[inline]
pub(crate) fn tier() -> Tier {
    Tier::from_code(TIER.load(Ordering::Relaxed)).unwrap_or_else(detect)
}

/// Serializes tests that pin the tier. Every `tests/inline/*.rs` module compiles
/// into the one bin target, so under `cargo test` they run as threads sharing
/// this `TIER`. `testutil::TierSandbox` acquires it as an RAII guard.
#[cfg(test)]
pub(crate) static TIER_TEST_LOCK: crate::lockorder::RankedMutex<
    (),
    crate::lockorder::rank::TierTest,
> = crate::lockorder::RankedMutex::new(());

/// Read the stored pin, `None` for unset. [`tier`] collapses unset into a
/// detected tier, which a restore would then write back as a real pin.
#[cfg(test)]
pub(crate) fn tier_override() -> Option<Tier> {
    Tier::from_code(TIER.load(Ordering::Relaxed))
}

/// Put back a [`tier_override`] reading.
#[cfg(test)]
pub(crate) fn restore_tier(snapshot: Option<Tier>) {
    TIER.store(snapshot.map_or(0, Tier::as_code), Ordering::Relaxed);
}

// ── Palette tables ────────────────────────────────────────────────────────────
//
// Each row: (full: Color::Rgb, compatible: Color::Indexed(xterm-256))
// The xterm-256 index is the nearest match to the RGB value, for 256-color terminals.

#[inline]
fn pick(full: Color, compatible: Color) -> Color {
    match tier() {
        Tier::Full => full,
        Tier::Compatible => compatible,
    }
}

// ── Surfaces ──────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn bg() -> Color {
    pick(Color::Rgb(30, 30, 46), Color::Indexed(235))
}
#[inline]
pub(crate) fn bg_sunken() -> Color {
    pick(Color::Rgb(17, 17, 27), Color::Indexed(233))
}
#[inline]
pub(crate) fn bg_hover() -> Color {
    pick(Color::Rgb(40, 40, 56), Color::Indexed(236))
}

// ── Lines ─────────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn line_color() -> Color {
    pick(Color::Rgb(49, 50, 68), Color::Indexed(238))
}
#[inline]
pub(crate) fn line_strong_color() -> Color {
    pick(Color::Rgb(69, 71, 90), Color::Indexed(240))
}

// ── Text ──────────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn text_color() -> Color {
    pick(Color::Rgb(205, 214, 244), Color::Indexed(189))
}
#[inline]
pub(crate) fn text_dim_color() -> Color {
    pick(Color::Rgb(166, 173, 200), Color::Indexed(145))
}
#[inline]
pub(crate) fn text_faint_color() -> Color {
    pick(Color::Rgb(127, 132, 156), Color::Indexed(102))
}

// ── Accents ───────────────────────────────────────────────────────────────────
/// Sapphire primary — the cool accent that carries the UI.
#[inline]
pub(crate) fn accent_color() -> Color {
    pick(Color::Rgb(67, 171, 229), Color::Indexed(75))
}
/// Claude orange — the warm secondary; rule: once per screen max.
#[inline]
pub(crate) fn accent_2_color() -> Color {
    pick(Color::Rgb(217, 119, 87), Color::Indexed(173))
}

// ── Semantic ──────────────────────────────────────────────────────────────────
#[inline]
pub(crate) fn success_color() -> Color {
    pick(Color::Rgb(166, 227, 161), Color::Indexed(151))
}
#[inline]
pub(crate) fn warning_color() -> Color {
    pick(Color::Rgb(249, 226, 175), Color::Indexed(223))
}
#[inline]
pub(crate) fn danger_color() -> Color {
    pick(Color::Rgb(243, 139, 168), Color::Indexed(211))
}
#[inline]
pub(crate) fn info_color() -> Color {
    pick(Color::Rgb(116, 199, 236), Color::Indexed(117))
}

// ── Banner background tints ───────────────────────────────────────────────────
/// DANGER wash blended into BG — banner background for critical conditions.
#[inline]
pub(crate) fn bg_danger_color() -> Color {
    pick(Color::Rgb(75, 35, 44), Color::Indexed(52))
}
/// WARNING wash blended into BG — muted warm-amber background for warning rows.
#[inline]
pub(crate) fn bg_warning_color() -> Color {
    pick(Color::Rgb(74, 60, 33), Color::Indexed(58))
}

/// Per-channel RGB blend of `over` onto `beneath`, weighted by `alpha`
/// (the weight of `over`, clamped to `0.0..=1.0`).
/// Blends only on the full truecolor tier with both colors RGB-resolvable;
/// otherwise returns `over` unchanged.
pub(crate) fn blend_over(beneath: Color, over: Color, alpha: f64) -> Color {
    let (Color::Rgb(br, bg, bb), Color::Rgb(or, og, ob)) = (beneath, over) else {
        return over;
    };
    if tier() != Tier::Full {
        return over;
    }
    let a = alpha.clamp(0.0, 1.0);
    let mix = |o: u8, b: u8| -> u8 { (a * f64::from(o) + (1.0 - a) * f64::from(b)).round() as u8 };
    Color::Rgb(mix(or, br), mix(og, bg), mix(ob, bb))
}

// ── Toggle glyphs (tier-sensitive) ────────────────────────────────────────────

/// Toggle switch in the **on** state.
/// `full`: `─●`  `compatible`: `[on]`
pub(crate) fn toggle_on() -> &'static str {
    match tier() {
        Tier::Full => "─●",
        Tier::Compatible => "[on]",
    }
}

/// Toggle switch in the **off** state.
/// `full`: `○─`  `compatible`: `[off]`
pub(crate) fn toggle_off() -> &'static str {
    match tier() {
        Tier::Full => "○─",
        Tier::Compatible => "[off]",
    }
}

/// Gutter glyph for a row in edit mode — replaces the `❯` selection caret while
/// a text/stepper field is being typed into. Same on both tiers.
pub(crate) fn edit_glyph() -> &'static str {
    "✎"
}

/// Compact blocked-reason marker for a dead login credential — `AuthBroken`
/// and `KeyRejected` deliberately share it (the detail pill and the help-modal
/// legend carry which credential class failed). Same on both tiers.
pub(crate) fn dead_credential_glyph() -> &'static str {
    "×"
}

// ── Style helpers ─────────────────────────────────────────────────────────────

pub(crate) fn base() -> Style {
    Style::default().fg(text_color()).bg(bg())
}

/// Plain body text — foreground only.
pub(crate) fn body() -> Style {
    Style::default().fg(text_color())
}

/// Hairline chrome — tooltip `└ ` leaders and borders at `line_color()`.
pub(crate) fn line() -> Style {
    Style::default().fg(line_color())
}

/// Stronger line color — empty-gauge track and structural fills above `line_color()`.
pub(crate) fn line_strong() -> Style {
    Style::default().fg(line_strong_color())
}

pub(crate) fn dim() -> Style {
    Style::default().fg(text_dim_color())
}

pub(crate) fn faint() -> Style {
    Style::default().fg(text_faint_color())
}

/// Eyebrow label — bold + dim.
pub(crate) fn label() -> Style {
    Style::default()
        .fg(text_dim_color())
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn accent() -> Style {
    Style::default().fg(accent_color())
}

pub(crate) fn warning() -> Style {
    Style::default().fg(warning_color())
}

pub(crate) fn danger() -> Style {
    Style::default().fg(danger_color())
}

/// Background for the selected list row.
pub(crate) fn selected_row() -> Style {
    Style::default().bg(bg_hover())
}

/// Utilization color: dim <60%, warning 60–80%, danger >80%.
pub(crate) fn util_color(pct: f64) -> Color {
    let pct = pct.clamp(0.0, 100.0);
    if pct >= 80.0 {
        danger_color()
    } else if pct >= 60.0 {
        warning_color()
    } else {
        text_dim_color()
    }
}

/// `util_color` as a ready-to-use foreground style.
pub(crate) fn util(pct: f64) -> Style {
    Style::default().fg(util_color(pct))
}

/// Sapphire info accent; spinner color for refresh ops.
pub(crate) fn info() -> Style {
    Style::default().fg(info_color())
}

/// Catppuccin green — success tint; spinner color for auto-start.
pub(crate) fn success() -> Style {
    Style::default().fg(success_color())
}
