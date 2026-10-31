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

#[cfg(test)]
use std::cell::Cell;
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

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
    /// Stable code. `0` doubles as "uninitialized": the production accessor
    /// falls back to auto-detect, the test accessor to `Full`.
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

/// Tier holder. In production builds a process-global atomic code (`0` = unset
/// → auto-detect), seeded by [`init`] and swappable at runtime via [`set_tier`]
/// for the live theme picker. In test builds a thread-local cell, so a
/// `TierSandbox` on one test thread never changes another thread's tier.
#[cfg(not(test))]
static TIER: AtomicU8 = AtomicU8::new(0);

/// Terminal family holder, paired with [`TIER`]. Claude Code draws a different
/// working-line set when `TERM` is `xterm-ghostty`, so the choice is made once
/// at startup rather than per frame.
#[cfg(not(test))]
static GHOSTTY: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static TIER: Cell<u8> = const { Cell::new(0) };
    static GHOSTTY: Cell<bool> = const { Cell::new(false) };
}

/// The environment the production detection reads.
fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The startup detection read through `env`, the only seam it has to the
/// process environment: the tier comes from `$COLORTERM` (`truecolor` or
/// `24bit` → [`Tier::Full`], anything else → [`Tier::Compatible`]), the
/// ghostty flag from `TERM == "xterm-ghostty"`. `NO_COLOR` is the terminal
/// library's to honour: crossterm's `Colored` display path drops colour while
/// it is set. `TERM=dumb` is no tier: crossterm 0.29 (measured 2026-10-31)
/// reads `TERM` only in `available_color_count`, which the TUI never calls,
/// and in its windows ANSI probe once VT processing fails to enable. The tier
/// ignores both.
pub(crate) fn detect_with(env: impl Fn(&str) -> Option<String>) -> (Tier, bool) {
    let tier = match env("COLORTERM").unwrap_or_default().to_lowercase().as_str() {
        "truecolor" | "24bit" => Tier::Full,
        _ => Tier::Compatible,
    };
    let ghostty = env("TERM").is_some_and(|term| term == "xterm-ghostty");
    (tier, ghostty)
}

/// Detect the tier from the process environment (see [`detect_with`]).
/// Test builds read the tier through the thread-local pin only, so the
/// fallback's caller is `cfg(not(test))` and so is this.
#[cfg(not(test))]
pub(crate) fn detect() -> Tier {
    detect_with(process_env).0
}

/// Seed the process tier and terminal family at startup.
/// Precedence (highest first): explicit override → auto-detect.
pub(crate) fn init(override_tier: Option<Tier>) {
    let (tier, ghostty) = detect_with(process_env);
    set_tier(override_tier.unwrap_or(tier));
    set_ghostty(ghostty);
}

/// Whether the terminal is the family Claude Code draws its other working-line
/// set for.
pub(crate) fn ghostty_marks() -> bool {
    #[cfg(not(test))]
    {
        GHOSTTY.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    {
        GHOSTTY.with(Cell::get)
    }
}

pub(crate) fn set_ghostty(on: bool) {
    #[cfg(not(test))]
    GHOSTTY.store(on, Ordering::Relaxed);
    #[cfg(test)]
    GHOSTTY.with(|c| c.set(on));
}

/// Swap the active tier at runtime. The next render reads the new value, so the
/// Config tab's theme selector applies immediately.
pub(crate) fn set_tier(tier: Tier) {
    #[cfg(not(test))]
    TIER.store(tier.as_code(), Ordering::Relaxed);
    #[cfg(test)]
    TIER.with(|c| c.set(tier.as_code()));
}

/// Return the active tier. Falls back to auto-detect if [`init`] was not called;
/// in test builds an unset tier is `Full`, independent of the ambient
/// `$COLORTERM`.
#[inline]
pub(crate) fn tier() -> Tier {
    #[cfg(not(test))]
    {
        Tier::from_code(TIER.load(Ordering::Relaxed)).unwrap_or_else(detect)
    }
    #[cfg(test)]
    {
        Tier::from_code(TIER.with(|c| c.get())).unwrap_or(Tier::Full)
    }
}

/// Read the stored pin, `None` for unset. [`tier`] collapses unset into the
/// test default `Full`, which a restore would then write back as a real pin.
#[cfg(test)]
pub(crate) fn tier_override() -> Option<Tier> {
    Tier::from_code(TIER.with(|c| c.get()))
}

/// Put back a [`tier_override`] reading.
#[cfg(test)]
pub(crate) fn restore_tier(snapshot: Option<Tier>) {
    TIER.with(|c| c.set(snapshot.map_or(0, Tier::as_code)));
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
//
// The painted surface colours are the full tier's fills and, on the compatible
// tier, the colour a surface's text rides when it sits on an inverse fill. A
// surface fill paints only on the full tier; the selected-row tint (`bg_hover`)
// and the toast glass (`bg_sunken` blended) paint on both. Otherwise the
// compatible tier leaves a surface unpainted so the terminal's own background
// shows through, and elevation falls to borders and colour alone.
#[inline]
pub(crate) fn bg() -> Color {
    pick(Color::Rgb(30, 30, 46), Color::Indexed(235))
}
#[inline]
pub(crate) fn bg_sunken() -> Color {
    pick(Color::Rgb(17, 17, 27), Color::Indexed(233))
}

/// The base surface fill (`BG`), unpainted on the compatible tier.
#[inline]
pub(crate) fn surface_fill() -> Color {
    pick(Color::Rgb(30, 30, 46), Color::Reset)
}

/// The sunken surface fill (`BG_SUNKEN`), unpainted on the compatible tier.
#[inline]
pub(crate) fn sunken_fill() -> Color {
    pick(Color::Rgb(17, 17, 27), Color::Reset)
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
/// Clawd's `clawd_background` cut-out colour: `rgb(0,0,0)` in the source's dark
/// and light themes alike (its ansi themes use `ansi:black`). The compatible
/// tier uses xterm-256 index 16 (the colour cube's black), never 16-colour
/// `Black`, which a terminal theme can repaint.
#[inline]
pub(crate) fn clawd_cutout_color() -> Color {
    pick(Color::Rgb(0, 0, 0), Color::Indexed(16))
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
/// (the weight of `over`, clamped to `0.0..=1.0`). The full tier blends two
/// `Color::Rgb`s and returns `over` unchanged otherwise — the call shape the
/// identity-wave crest uses. The compatible tier resolves both colors to RGB
/// (an indexed color to the RGB its xterm index stands in for, an unknown or
/// `Reset` one to `BG`), blends, and snaps to the nearest xterm-256 index.
pub(crate) fn blend_over(beneath: Color, over: Color, alpha: f64) -> Color {
    let a = alpha.clamp(0.0, 1.0);
    let mix = |o: u8, b: u8| -> u8 { (a * f64::from(o) + (1.0 - a) * f64::from(b)).round() as u8 };
    match tier() {
        Tier::Full => {
            let (Color::Rgb(br, bg, bb), Color::Rgb(or, og, ob)) = (beneath, over) else {
                return over;
            };
            Color::Rgb(mix(or, br), mix(og, bg), mix(ob, bb))
        }
        Tier::Compatible => {
            let (br, bg, bb) = rgb_of(beneath);
            let (or, og, ob) = rgb_of(over);
            Color::Indexed(nearest_index((mix(or, br), mix(og, bg), mix(ob, bb))))
        }
    }
}

/// The RGB a color resolves to for the compatible-tier blend. An indexed color
/// resolves to the RGB its xterm index stands in for; an unknown or `Reset`
/// one counts as `BG`.
fn rgb_of(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) => indexed_rgb(i),
        _ => (30, 30, 46),
    }
}

/// The RGB column of the theme token whose nearest xterm-256 index is `i` —
/// the same RGB the accessors above pair with that index. An index the theme
/// did not mint resolves from the standard xterm-256 palette instead.
fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        235 => (30, 30, 46),
        16 => (0, 0, 0),
        234 => (24, 24, 37),
        233 => (17, 17, 27),
        236 => (40, 40, 56),
        238 => (49, 50, 68),
        240 => (69, 71, 90),
        189 => (205, 214, 244),
        145 => (166, 173, 200),
        102 => (127, 132, 156),
        75 => (67, 171, 229),
        173 => (217, 119, 87),
        151 => (166, 227, 161),
        223 => (249, 226, 175),
        211 => (243, 139, 168),
        117 => (116, 199, 236),
        52 => (75, 35, 44),
        58 => (74, 60, 33),
        _ => standard_palette_rgb(i),
    }
}

/// The VGA/web 16-color set `standard_palette_rgb` maps to indices 0–15. Those
/// indices are terminal-defined, so [`nearest_index`] never targets them; this
/// table only resolves a `Color::Indexed(0..=15)` beneath to a fixed RGB.
const ANSI_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// The six cube levels of the standard xterm-256 palette.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The standard xterm-256 palette value for one index: 16 ANSI colors, a
/// 6×6×6 cube, then a grayscale ramp.
fn standard_palette_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => ANSI_RGB[usize::from(i)],
        16..=231 => {
            let n = i - 16;
            (
                CUBE_LEVELS[usize::from(n / 36)],
                CUBE_LEVELS[usize::from((n / 6) % 6)],
                CUBE_LEVELS[usize::from(n % 6)],
            )
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// The xterm-256 index nearest to `rgb` by squared distance, the lower index
/// winning a tie. Searches 16..=255 only: indices 0..15 are terminal-defined,
/// so they are never a snap target.
fn nearest_index(rgb: (u8, u8, u8)) -> u8 {
    let (r, g, b) = rgb;
    let distance = |(pr, pg, pb): (u8, u8, u8)| {
        let dr = i32::from(r) - i32::from(pr);
        let dg = i32::from(g) - i32::from(pg);
        let db = i32::from(b) - i32::from(pb);
        dr * dr + dg * dg + db * db
    };
    let mut best = 16;
    let mut best_distance = i32::MAX;
    for i in 16..=u8::MAX {
        let d = distance(standard_palette_rgb(i));
        if d < best_distance {
            best_distance = d;
            best = i;
        }
    }
    best
}

// ── Marks (tier-independent) ──────────────────────────────────────────────────

/// Selection caret, drawn only inside the focused pane.
pub(crate) const CARET: &str = "❯";
/// Filled status dot — active / online.
pub(crate) const DOT_ON: &str = "●";
/// Hollow status dot — inactive / disconnected.
pub(crate) const DOT_OFF: &str = "○";
/// Done / captured mark.
pub(crate) const DONE: &str = "✓";
/// Collapsed disclosure arrow.
pub(crate) const DISCLOSURE_SHUT: &str = "▶";
/// Expanded disclosure arrow.
pub(crate) const DISCLOSURE_OPEN: &str = "▼";
/// Heavy vertical: the toast's left bar and the scrollbar's thumb.
pub(crate) const BAR: &str = "┃";
/// Scrollbar track.
pub(crate) const TRACK: &str = "┊";
/// Static waiting mark — `queued` / `idle`.
pub(crate) const WAITING: &str = "◌";

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

// Claude Code's working-line marks, in its default terminals and in the set it
// draws when `TERM` is `xterm-ghostty`.
const CLAUDE_WORKING_FRAMES: [&str; 6] = ["·", "✢", "*", "✶", "✻", "✽"];
const CLAUDE_WORKING_FRAMES_GHOSTTY: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✻"];
const CLAUDE_WORKING_PERIOD_MS: u64 = 2_000;
const CLAUDE_WORKING_SNAP_MS: u64 = 64;

/// The active claude account's mark `elapsed_ms` into its working animation:
/// six frames over a 2 s period, eased in and out by a cosine.
pub(crate) fn active_claude_frame(elapsed_ms: u64, ghostty: bool) -> &'static str {
    let phase =
        elapsed_ms / CLAUDE_WORKING_SNAP_MS * CLAUDE_WORKING_SNAP_MS % CLAUDE_WORKING_PERIOD_MS;
    let turn = std::f64::consts::TAU * phase as f64 / CLAUDE_WORKING_PERIOD_MS as f64;
    let index = ((1.0 - turn.cos()) / 2.0 * 5.0).round() as usize;
    let frames = if ghostty {
        &CLAUDE_WORKING_FRAMES_GHOSTTY
    } else {
        &CLAUDE_WORKING_FRAMES
    };
    frames[index.min(frames.len() - 1)]
}

/// The mark an active claude account rests on while it spends nothing.
pub(crate) fn active_claude_rest() -> &'static str {
    CLAUDE_WORKING_FRAMES[4]
}

/// The active codex account's mark. Static on both tiers.
pub(crate) fn active_codex_mark() -> &'static str {
    "⏣"
}

/// An active claude account while the client-token store holds a token for it.
/// Static: only the claude account's own mark moves.
pub(crate) fn active_shunt_mark() -> &'static str {
    "⥋"
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
    Style::default().fg(text_color()).bg(surface_fill())
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

/// Eyebrow label while focus rests in its section — bold + text.
pub(crate) fn label_active() -> Style {
    body().add_modifier(Modifier::BOLD)
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

#[cfg(test)]
#[path = "../../tests/inline/tui_theme.rs"]
mod tests;
