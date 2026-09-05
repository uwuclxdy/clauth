//! Config-tab row geometry. Every blurred row's value starts at the same
//! column (the Config tab is a cloudy-tui tight chip group); cycle options are
//! bare labels on 2-space gaps with the active option bracketed only on focus;
//! an on/off boolean renders as a toggle, not a 2-option cycle.

use super::*;
use ratatui::style::Modifier;

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn row(key: &str, options: &[(&str, bool)], selected: bool) -> String {
    line_text(&cycle_row(Span::raw("  "), key, options, selected))
}

/// Column `key`'s value starts at: the first non-space cell past the key text.
fn value_col(key: &str, rendered: &str) -> usize {
    let after_key = rendered.find(key).expect("row renders its key") + key.chars().count();
    after_key
        + rendered[after_key..]
            .find(|c: char| c != ' ')
            .expect("row renders a value")
}

fn toggles() -> RowState {
    RowState {
        switch_off_when_spent: false,
        burn_aware: false,
        spend_budget: false,
        switch_off_when_budget_spent: true,
        preemptive: false,
        refresh_spent: true,
        auto_start_queue: true,
        any_auto_start: true,
        reset_display: ResetDisplay::Relative,
        clock_format: ClockFormat::H24,
    }
}

fn tunables() -> RowTunables {
    RowTunables {
        refresh_interval_ms: 60_000,
        weekly_pct: 95.0,
        burn_floor_pct: 98.0,
        burn_horizon_ms: 60_000,
        default_divergence: None,
    }
}

#[test]
fn key_cell_is_uniform_width() {
    for key in ["theme", "weekly limit", "on mismatch", "refresh spent"] {
        assert_eq!(
            key_cell(key, KEY_W, KEY_GUTTER).chars().count(),
            KEY_W + KEY_GUTTER,
            "{key} key block must be exactly KEY_W + KEY_GUTTER wide"
        );
    }
}

/// Rot-proof against a new row with a longer key: every blurred row opens its
/// value at the shared column. Reads the real rows, so no key list to sync.
#[test]
fn every_blurred_row_starts_its_value_at_the_shared_column() {
    let value_col = 2 + KEY_W + KEY_GUTTER;
    for r in GLOBAL_CONFIG_ROWS {
        let line = line_text(&detail_row(r, false, toggles(), tunables(), None));
        let before: String = line.chars().take(value_col).collect();
        assert!(
            before.ends_with(&" ".repeat(KEY_GUTTER)),
            "{r:?} key overruns KEY_W or drops the gutter: {before:?}"
        );
        assert_ne!(
            line.chars().nth(value_col),
            Some(' '),
            "{r:?} value column must open on a label/glyph, not a space"
        );
    }
}

// Bug 6 sibling: the selection caret pairs ACCENT + bold in every other card
// (chain.rs, overview.rs, panes.rs) — this card rendered it accent-only.
#[test]
fn selection_caret_is_bold_like_every_other_card() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let line = detail_row(GlobalConfigRow::Theme, true, toggles(), tunables(), None);
    let caret = &line.spans[0];
    assert!(
        caret.style.add_modifier.contains(Modifier::BOLD),
        "selection caret must be bold: {caret:?}"
    );
    assert_eq!(
        caret.style.fg,
        theme::accent().fg,
        "selection caret stays accent"
    );
}

/// The regression the screenshot caught: a key exactly `KEY_W` chars wide (now
/// `extra usage spent`) must not let a `saturating_sub(..).max(1)` pad widen its
/// block by a cell and push the value column right.
#[test]
fn longest_key_aligns_with_shortest() {
    let theme = row("theme", &[("full", true), ("compatible", false)], false);
    let widest = row(
        "extra usage spent",
        &[("stay on active", true), ("switch off all", false)],
        false,
    );
    assert_eq!(value_col("theme", &theme), 2 + KEY_W + KEY_GUTTER);
    assert_eq!(
        value_col("theme", &theme),
        value_col("extra usage spent", &widest),
        "`extra usage spent` (== KEY_W chars) must not push its value column right"
    );
}

/// Bare labels: an inactive first option and an active first option open at the
/// same column (no bracket-cell reservation either way).
#[test]
fn active_first_option_aligns_with_inactive_first_option() {
    let theme = row("theme", &[("full", true), ("compatible", false)], false);
    let mismatch = row(
        "on mismatch",
        &[("ask", false), ("overwrite", true), ("new", false)],
        false,
    );
    assert_eq!(
        value_col("theme", &theme),
        value_col("on mismatch", &mismatch),
    );
}

/// Focus wraps the active option in `[]` — the bracket pair is the only width
/// change; labels ahead of the active option hold their columns.
#[test]
fn focus_wraps_the_active_option_in_brackets() {
    let options = [("ask", false), ("overwrite", true), ("new", false)];
    let blurred = row("on mismatch", &options, false);
    let focused = row("on mismatch", &options, true);
    assert!(!blurred.contains('['), "{blurred:?}");
    assert!(focused.contains("[overwrite]"), "{focused:?}");
    assert_eq!(
        focused.chars().count(),
        blurred.chars().count() + 2,
        "the bracket pair is the only width change"
    );
    assert_eq!(
        blurred.find("ask"),
        focused.find("ask"),
        "a label ahead of the active option holds its column"
    );
}

/// Exact bytes: 2-space gaps everywhere, bare labels, active option bracketed
/// only on focus.
#[test]
fn cycle_row_renders_the_contract_shape() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Compatible);
    let options = [("off", true), ("basic", false), ("strict", false)];
    // Key column is `arrow (2) + KEY_W + KEY_GUTTER` wide; derive the pad so the
    // shape assertion tracks KEY_W instead of rebreaking on a width change.
    let pad = " ".repeat(KEY_W + KEY_GUTTER - "verify".len());
    assert_eq!(
        row("verify", &options, false),
        format!("  verify{pad}off  basic  strict"),
    );
    assert_eq!(
        row("verify", &options, true),
        format!("  verify{pad}[off]  basic  strict"),
    );
}

/// The caret math in `draw` assumes the typed buffer starts at the value column.
#[test]
fn edit_line_buffer_starts_at_the_value_column() {
    let input = InputState {
        value: "45".to_string(),
        cursor: 2,
    };
    for rendered in [
        line_text(&weekly_edit_line(Span::raw("  "), &input)),
        line_text(&refresh_edit_line(Span::raw("  "), &input)),
    ] {
        assert_eq!(
            rendered.find("45"),
            Some(2 + KEY_W + KEY_GUTTER),
            "typed buffer must start at the shared value column: {rendered:?}"
        );
    }
}

/// `auto-start queue` is a pure on/off toggle, and BOTH hint strings are
/// pinned whole: this row is the queue's only control, so a reworded hint is a
/// user-facing change nothing else would catch.
#[test]
fn auto_start_queue_renders_as_a_toggle_with_both_hints_pinned() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let on = line_text(&detail_row(
        GlobalConfigRow::AutoStartQueue,
        false,
        toggles(),
        tunables(),
        None,
    ));
    assert!(on.contains("auto-start queue"), "{on}");
    assert!(on.contains(theme::toggle_on()), "on state glyph: {on}");

    let mut off = toggles();
    off.auto_start_queue = false;
    let off_line = line_text(&detail_row(
        GlobalConfigRow::AutoStartQueue,
        false,
        off,
        tunables(),
        None,
    ));
    assert!(
        off_line.contains(theme::toggle_off()),
        "off state glyph: {off_line}"
    );

    assert_eq!(
        row_hint(GlobalConfigRow::AutoStartQueue, toggles(), tunables()).as_deref(),
        Some("space auto-start windows evenly, so one resets every 5h / accounts"),
    );
    let mut off = toggles();
    off.auto_start_queue = false;
    assert_eq!(
        row_hint(GlobalConfigRow::AutoStartQueue, off, tunables()).as_deref(),
        Some("auto-start usage windows as soon as possible"),
    );
}

/// `refresh spent` is a pure on/off boolean — a cloudy-tui toggle (`─●` / `○─`),
/// not a 2-option cycle row (`[on]  off`).
#[test]
fn refresh_spent_renders_as_a_toggle_not_a_cycle() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let on = line_text(&detail_row(
        GlobalConfigRow::RefreshSpentAccounts,
        false,
        toggles(),
        tunables(),
        None,
    ));
    assert!(on.contains(theme::toggle_on()), "on state glyph: {on}");
    assert!(
        !on.contains("off"),
        "must not render the cycle off-option: {on}"
    );

    let mut off = toggles();
    off.refresh_spent = false;
    let off_line = line_text(&detail_row(
        GlobalConfigRow::RefreshSpentAccounts,
        false,
        off,
        tunables(),
        None,
    ));
    assert!(
        off_line.contains(theme::toggle_off()),
        "off state glyph: {off_line}"
    );
    assert!(
        !off_line.contains("  on"),
        "must not render the cycle on-option: {off_line}"
    );
}

/// With no account opted into `auto_start` there is nothing to space, so the
/// queue row renders as a cloudy-tui disabled row (whole content faint, knob
/// included) — it must never read as an armed setting. One opted-in account
/// makes it a live toggle again.
#[test]
fn auto_start_queue_dims_when_no_account_opts_in() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut none_opted = toggles();
    none_opted.any_auto_start = false;
    let dimmed = detail_row(
        GlobalConfigRow::AutoStartQueue,
        false,
        none_opted,
        tunables(),
        None,
    );
    assert!(
        dimmed
            .spans
            .iter()
            .all(|s| s.content.trim().is_empty() || s.style.fg == theme::faint().fg),
        "every content span must be faint while inert: {:?}",
        dimmed.spans,
    );

    let live = detail_row(
        GlobalConfigRow::AutoStartQueue,
        false,
        toggles(),
        tunables(),
        None,
    );
    assert!(
        live.spans.iter().any(|s| s.style.fg == theme::accent().fg),
        "an opted-in account brings the on-state knob back to accent: {:?}",
        live.spans,
    );
}

// ── `money spent` dims while inert (spend budget off) ────────────────────────

/// With `spend budget` off nothing spends, so `money spent` decides no halt.
/// It renders as a cloudy-tui disabled row (whole content faint) so it never
/// reads as an armed setting; flip the toggle on and it becomes a live cycle.
#[test]
fn money_spent_dims_when_spend_budget_is_off() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dimmed = detail_row(
        GlobalConfigRow::SwitchOffWhenBudgetSpent,
        false,
        toggles(), // spend_budget: false
        tunables(),
        None,
    );
    assert!(
        dimmed
            .spans
            .iter()
            .all(|s| s.content.trim().is_empty() || s.style.fg == theme::faint().fg),
        "every content span must be faint while inert: {:?}",
        dimmed.spans,
    );

    let mut on = toggles();
    on.spend_budget = true;
    let live = line_text(&detail_row(
        GlobalConfigRow::SwitchOffWhenBudgetSpent,
        true,
        on,
        tunables(),
        None,
    ));
    assert!(
        live.contains('['),
        "spend budget on: live + focused brackets the active option: {live}"
    );
}

#[test]
fn money_spent_hint_states_the_halt_not_inertness() {
    // The dim row already signals inertness, so the hint drops the gate clause
    // and states what the setting does. `toggles()` has switch-off-when-spent on.
    let hint = row_hint(
        GlobalConfigRow::SwitchOffWhenBudgetSpent,
        toggles(),
        RowTunables {
            refresh_interval_ms: 90_000,
            weekly_pct: 98.0,
            ..tunables()
        },
    )
    .expect("the money-spent row carries a behavior hint");
    assert!(!hint.contains("inert"), "gate clause dropped: {hint}");
    assert!(hint.contains("switch everything off"), "{hint}");
}

/// The faint `default: X` reminder rides a value row only while it is off its
/// default — the operator sees the default exactly when they've moved off it.
#[test]
fn a_non_default_value_shows_a_faint_default_reminder() {
    let off = line_text(&detail_row(
        GlobalConfigRow::RefreshInterval,
        false,
        toggles(),
        RowTunables {
            refresh_interval_ms: 30_000,
            weekly_pct: 98.0,
            ..tunables()
        },
        None,
    ));
    assert!(
        off.contains("default: 90s"),
        "off-default carries it: {off}"
    );
    let default = line_text(&detail_row(
        GlobalConfigRow::RefreshInterval,
        false,
        toggles(),
        RowTunables {
            refresh_interval_ms: 90_000,
            weekly_pct: 98.0,
            ..tunables()
        },
        None,
    ));
    assert!(
        !default.contains("default:"),
        "the default value carries no reminder: {default}"
    );
}

/// Value rows fold the live value into their hint, so cycling a row re-explains
/// what it now does with the real number.
#[test]
fn value_rows_interpolate_the_live_value_into_their_hint() {
    let refresh = row_hint(
        GlobalConfigRow::RefreshInterval,
        toggles(),
        RowTunables {
            refresh_interval_ms: 30_000,
            weekly_pct: 98.0,
            ..tunables()
        },
    )
    .expect("refresh row has a hint");
    assert!(refresh.contains("every 30s"), "{refresh}");
    let weekly = row_hint(
        GlobalConfigRow::WeeklyThreshold,
        toggles(),
        RowTunables {
            refresh_interval_ms: 90_000,
            ..tunables()
        },
    )
    .expect("weekly row has a hint");
    assert!(weekly.contains("95%"), "{weekly}");
}

// ── burn floor / horizon dim while inert (burn-aware off) ─────────────────────

/// Both burn-aware tunables gate a projection that never runs under static
/// switch mode, so they render as cloudy-tui disabled rows (whole content faint)
/// while burn-aware is off, and become live cycles once it is on.
#[test]
fn burn_tunables_dim_when_burn_aware_is_off() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for r in [GlobalConfigRow::BurnFloor, GlobalConfigRow::BurnHorizon] {
        let dimmed = detail_row(r, false, toggles(), tunables(), None);
        assert!(
            dimmed
                .spans
                .iter()
                .all(|s| s.content.trim().is_empty() || s.style.fg == theme::faint().fg),
            "{r:?} must render fully faint while burn-aware is off: {:?}",
            dimmed.spans,
        );

        let mut on = toggles();
        on.burn_aware = true;
        let live = line_text(&detail_row(r, true, on, tunables(), None));
        assert!(
            live.contains('['),
            "{r:?} burn-aware on: live + focused brackets the active preset: {live}"
        );
    }
}

// ── preemptive rotation is live on every platform ────────────────────────────

/// The rotation lead is a clock margin against the running `claude`'s own
/// refresh threshold, which is platform-independent — so the `rotation` row is
/// an editable `cycle_row` everywhere, never a dimmed one, and its hint states
/// the behavior rather than a platform caveat.
#[test]
fn rotation_row_is_live_on_every_platform() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let row = detail_row(
        GlobalConfigRow::PreemptiveRotation,
        false,
        toggles(),
        tunables(),
        None,
    );
    assert!(
        row.spans
            .iter()
            .any(|s| !s.content.trim().is_empty() && s.style.fg != theme::faint().fg),
        "an editable row must carry at least one non-faint span: {:?}",
        row.spans,
    );

    let mut on = toggles();
    on.preemptive = true;
    for (state, want) in [(toggles(), "rejects"), (on, "before it expires")] {
        let hint = row_hint(
            GlobalConfigRow::PreemptiveRotation,
            state,
            RowTunables {
                refresh_interval_ms: 90_000,
                weekly_pct: 98.0,
                ..tunables()
            },
        )
        .expect("the rotation row carries a hint");
        assert!(
            hint.contains(want),
            "the hint must state the live behavior, wanted {want:?}, got {hint:?}"
        );
        assert!(
            !hint.contains("macos"),
            "no platform caveat survives on a cross-platform row: {hint}"
        );
    }
}

// ── concern bands + their eyebrow headers ────────────────────────────────────

/// The renderer opens a band the first time it sees a new one, so a band whose
/// rows are split by an unrelated row would print its header twice and read as
/// two sections. Pins the contiguity `GLOBAL_CONFIG_ROWS` relies on.
#[test]
fn config_bands_stay_contiguous() {
    let mut seen: Vec<&str> = Vec::new();
    for row in GLOBAL_CONFIG_ROWS {
        if seen.last() != Some(&row.band()) {
            assert!(
                !seen.contains(&row.band()),
                "band {:?} reopens after another band: {seen:?}",
                row.band(),
            );
            seen.push(row.band());
        }
    }
    assert_eq!(
        seen,
        ["appearance", "scheduler", "auto-switch", "extra usage"],
        "the band order is the display order",
    );
}

/// The header is a fixed `TEXT_DIM + bold` eyebrow whatever the cursor does; the
/// underline is the only thing focus moves. Rendering the focused band brighter,
/// or underlining a band with no row focused, is the bug this pins.
#[test]
fn band_header_underlines_only_the_focused_band() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let blurred = band_header("auto-switch", false);
    let focused = band_header("auto-switch", true);
    for line in [&blurred, &focused] {
        assert_eq!(line_text(line), "AUTO-SWITCH", "eyebrows render uppercase");
        assert_eq!(
            line.spans[0].style.fg,
            theme::label().fg,
            "the tier never changes with focus"
        );
        assert!(
            line.spans[0].style.add_modifier.contains(Modifier::BOLD),
            "the eyebrow bold is a fixed label treatment, not a focus cue"
        );
    }
    assert!(
        !blurred.spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED),
        "no underline while focus is elsewhere"
    );
    assert!(
        focused.spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED),
        "the underline is the active-section cue"
    );
}

// ── reset display + clock notation (issue #39) ───────────────────────────────

/// The reset row is a three-way cycle and the focused row brackets whichever
/// value is live, so the operator can see the other two without cycling.
#[test]
fn reset_display_row_shows_all_three_shapes() {
    for (display, active) in [
        (ResetDisplay::Relative, "[relative]"),
        (ResetDisplay::Clock, "[clock]"),
        (ResetDisplay::Both, "[both]"),
    ] {
        let mut rows = toggles();
        rows.reset_display = display;
        let line = line_text(&detail_row(
            GlobalConfigRow::ResetShape,
            true,
            rows,
            tunables(),
            None,
        ));
        assert!(
            line.contains(active),
            "{display:?} must bracket {active}: {line}"
        );
        for label in ["relative", "clock", "both"] {
            assert!(line.contains(label), "{display:?} lists {label}: {line}");
        }
    }
}

/// The notation decides nothing while resets render as a bare countdown, so the
/// row is a cloudy-tui disabled row until a clock shows — and live after.
#[test]
fn clock_row_dims_until_a_reset_renders_a_clock() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dimmed = detail_row(
        GlobalConfigRow::ClockNotation,
        false,
        toggles(),
        tunables(),
        None,
    );
    assert!(
        dimmed
            .spans
            .iter()
            .all(|s| s.content.trim().is_empty() || s.style.fg == theme::faint().fg),
        "clock must render fully faint under relative resets: {:?}",
        dimmed.spans,
    );

    for display in [ResetDisplay::Clock, ResetDisplay::Both] {
        let mut rows = toggles();
        rows.reset_display = display;
        let live = line_text(&detail_row(
            GlobalConfigRow::ClockNotation,
            true,
            rows,
            tunables(),
            None,
        ));
        assert!(
            live.contains("[24h]") && live.contains("12h"),
            "{display:?} makes the clock row live: {live}"
        );
    }
}

/// Both rows re-explain themselves per value — the value alone doesn't say what
/// changes on screen, and the notation hint is where "local timezone" is stated.
#[test]
fn reset_rows_hints_track_their_value() {
    let hint = |rows: RowState, row| row_hint(row, rows, tunables()).expect("row has a hint");
    let mut rows = toggles();
    assert!(hint(rows, GlobalConfigRow::ResetShape).contains("how long"));
    rows.reset_display = ResetDisplay::Clock;
    assert!(hint(rows, GlobalConfigRow::ResetShape).contains("time of day"));
    rows.reset_display = ResetDisplay::Both;
    let both = hint(rows, GlobalConfigRow::ResetShape);
    assert!(
        both.contains("how long") && both.contains("time it resets"),
        "{both}"
    );

    let h24 = hint(rows, GlobalConfigRow::ClockNotation);
    assert!(h24.contains("21:20") && h24.contains("local"), "{h24}");
    rows.clock_format = ClockFormat::H12;
    assert!(hint(rows, GlobalConfigRow::ClockNotation).contains("9:20pm"));
}
